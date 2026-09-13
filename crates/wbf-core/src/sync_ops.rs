//! 進料：`watch`（等新事件）與 `recent`（跨房間把水位之後的事件拉回快取）。
//!
//! ⚠️ 這兩個都是**串流**：事件一則一則到，不是一次回一包。所以它們**邊做邊發
//! [`crate::CoreEvent`]**，回傳值只是收尾的摘要——🚫 不要等全部收齊再回，
//! 那樣 `watch tail` 永遠不會回來。

use serde::Serialize;

use wbf_sdk::chat::{ChatBackend, Message, Update, WatchControl};
use wbf_sdk::event_json::messages_from_json;
use wbf_sdk::{RecentPlan, Transport};

use crate::error::{CoreError, CoreErrorKind};
use crate::{Core, CoreEvent, Target};

/// `watch` 要等多久、等到什麼為止。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WatchMode {
    /// 一直等，🚫 不會自己回來。
    Tail,
    /// 等這麼多秒就回來。
    Wait { seconds: u64 },
    /// 等到**別人**送的第一則就回來（自己送的不算，CLI 規格 §3.4.2）。
    Once { timeout_seconds: Option<u64> },
}

/// `watch` 收尾的摘要。
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct WatchSummary {
    /// 下次接續用的游標。
    pub since: String,
    /// 這一輪印出去幾則。
    pub seen: usize,
    /// `Once` 時是不是真的等到了別人的訊息（`false` = 逾時）。
    pub stopped_by_message: bool,
}

/// `recent` 收尾的摘要。
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RecentSummary {
    pub pulled: usize,
    pub written: usize,
    pub windows: u32,
    pub batches: u32,
    /// ⚠️ `false` = 撞到 `max_events` 停下來了，比水位更舊的還沒進快取。
    pub caught_up: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cg_seq_before: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cg_seq_after: Option<i64>,
    /// 沒帶 `room_id` 而被跳過的則數。🚫 不猜房間：猜錯會落到查不到的地方
    /// （PR #13 審查 salvia🟢2）。
    pub skipped_without_room: usize,
}

impl Core {
    /// 等這個房間的新事件。**每一則發一個 [`CoreEvent::Message`]**。
    ///
    /// 收尾時把印過的寫穿快取（callback 是同步的，所以收在外面一次寫）。
    pub async fn watch(
        &self,
        room: &str,
        mode: WatchMode,
        since: Option<&str>,
        target: &Target,
    ) -> Result<WatchSummary, CoreError> {
        let account = self.account_or_current(target)?;
        let backend = self
            .synced_backend_of(&account, target.server_backup)
            .await?;
        let me = self.session_of(&account)?.user_id;
        let deadline = match mode {
            WatchMode::Tail => None,
            WatchMode::Wait { seconds } => Some(std::time::Duration::from_secs(seconds)),
            WatchMode::Once { timeout_seconds } => {
                timeout_seconds.map(std::time::Duration::from_secs)
            }
        };
        let once = matches!(mode, WatchMode::Once { .. });
        let mut seen: Vec<Message> = Vec::new();
        let mut on_update = |update: Update| -> WatchControl {
            let Update::NewMessage(message) = &update else {
                return WatchControl::Continue;
            };
            if message.conversation != room {
                return WatchControl::Continue;
            }
            // 自己送的也發出去（呼叫端自己濾），但 `once` 不把自己的算「第一則」。
            let own = message.sender == me;
            self.events.emit(CoreEvent::Message {
                user: me.clone(),
                message: message.clone(),
            });
            seen.push((**message).clone());
            match once && !own {
                true => WatchControl::Stop,
                false => WatchControl::Continue,
            }
        };
        let end = backend.watch(since, deadline, &mut on_update).await?;
        if !seen.is_empty() {
            if let Ok((mut cache, me)) = self.cache_and_me(&account) {
                self.write_through(cache.upsert_messages(&me, &seen));
            }
        }
        Ok(WatchSummary {
            since: end.since,
            seen: seen.len(),
            stopped_by_message: end.stopped_by_callback,
        })
    }

    /// `Event/Recent`：一窗一窗把 `cg_seq` 之後的事件跨房間拉回來寫進 `cache.db`。
    ///
    /// 這是快取的**主要進料口**；`history`／`files`／`watch` 只是順手寫穿。
    ///
    /// ⚠️ 中途斷線或 server 回錯：已寫進快取的**有效**，水位不動（server 的
    /// pack-pipeline §6.4）；下次再跑會從水位重來。
    ///
    /// Args:
    ///     from_scratch: 不帶 `cg_seq`（把快取水位當沒有），server 從最新往回給
    pub async fn recent(
        &self,
        plan: RecentPlan,
        from_scratch: bool,
        transport: Transport,
        client_name: &str,
        target: &Target,
    ) -> Result<RecentSummary, CoreError> {
        let account = self.account_or_current(target)?;
        let (mut cache, me) = self.cache_and_me(&account)?;
        let mut client = self.client_of(&account, transport).await?;
        client.hello(client_name).await?;
        let cg_seq = match from_scratch {
            true => None,
            false => cache.get_cg_seq(&me)?,
        };
        let mut pulled = 0usize;
        let mut written = 0usize;
        let mut batches = 0u32;
        let mut skipped_without_room = 0usize;
        let mut on_batch = |meta: &wbf_sdk::protocol::BatchMeta,
                            raws: Vec<serde_json::Value>|
         -> Result<(), wbf_sdk::SdkError> {
            batches += 1;
            pulled += raws.len();
            // Recent 的事件自帶 `room_id`。🚫 沒帶的不猜、不寫——猜錯會落到 room
            // "unknown"，之後查不到（PR #13 審查 salvia🟢2）。一個 Batch 寫一次 DB。
            let (with_room, without_room): (Vec<_>, Vec<_>) = raws.into_iter().partition(|raw| {
                raw.get("room_id")
                    .and_then(|value| value.as_str())
                    .is_some()
            });
            skipped_without_room += without_room.len();
            let messages = messages_from_json("unknown", &with_room);
            written += cache.upsert_messages(&me, &messages)?;
            self.events.progress(format!(
                "recent: batch {batches}: {} events (window {}, {} left, g_seq {}..{})",
                meta.bc, meta.tc, meta.r, meta.fs, meta.ls
            ));
            Ok(())
        };
        let summary = client.recent_sync(cg_seq, plan, &mut on_batch).await?;
        if skipped_without_room > 0 {
            self.events.progress(format!(
                "recent: skipped {skipped_without_room} event(s) without room_id"
            ));
        }
        if let Some(new_cg_seq) = summary.new_cg_seq {
            cache.set_cg_seq(&me, new_cg_seq)?;
        }
        if !summary.caught_up {
            self.events.progress(format!(
                "recent: stopped at the {} event limit; older events (below g_seq {:?}) are not in the cache yet",
                summary.events, summary.last_ls
            ));
        }
        Ok(RecentSummary {
            pulled,
            written,
            windows: summary.windows,
            batches,
            caught_up: summary.caught_up,
            cg_seq_before: cg_seq,
            cg_seq_after: summary.new_cg_seq.or(cg_seq),
            skipped_without_room,
        })
    }
}

/// `watch` 的模式字串 → [`WatchMode`]。
///
/// 🚫 認不得的模式**報錯**，不落到某個預設：`watch` 會等很久，落錯模式的人要很久才發現。
pub fn watch_mode_from_name(
    mode: &str,
    seconds: Option<u64>,
    timeout_seconds: Option<u64>,
) -> Result<WatchMode, CoreError> {
    match mode {
        "tail" => Ok(WatchMode::Tail),
        "wait" => Ok(WatchMode::Wait {
            seconds: seconds.ok_or_else(|| {
                CoreError::new(CoreErrorKind::Usage, "watch wait needs the seconds")
            })?,
        }),
        "once" => Ok(WatchMode::Once { timeout_seconds }),
        other => Err(CoreError::new(
            CoreErrorKind::Usage,
            format!("watch mode {other}: use tail, wait or once"),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unknown_watch_mode_is_refused_rather_than_defaulted() {
        assert_eq!(
            watch_mode_from_name("tail", None, None).unwrap(),
            WatchMode::Tail
        );
        assert_eq!(
            watch_mode_from_name("wait", Some(5), None).unwrap(),
            WatchMode::Wait { seconds: 5 }
        );
        // `wait` 沒帶秒數是用法錯，🚫 不要自己挑一個數字。
        assert_eq!(
            watch_mode_from_name("wait", None, None).unwrap_err().kind,
            CoreErrorKind::Usage
        );
        assert_eq!(
            watch_mode_from_name("forever", None, None)
                .unwrap_err()
                .kind,
            CoreErrorKind::Usage
        );
    }
}

//! 訂閱線的內容：房間事件的訂閱與推播寫進 `cache.db`（daemon-runtime 第 6／7 階段；server 的語意在 wbfuwunel 的 `/docs/design/wbf-event-push.md`）。
//!
//! 維護者 2026-09-22／23 定的形狀（/docs/design/rooms/room-sync.md §0）：
//!
//! - 池開線走一支通用的 [`Core::init_connection`]：hello 之後看角色。`Rooms` 就送 `Event/Subscribe`、起一個收推播的 task；`Keys` 是金鑰那半（`key_sync.rs`）。
//!   線死了、重開時自然重訂（/docs/design/daemon/link-pool.md §3）。訂閱會話結束（server 送 `Error`）時 socket 可能還活著，池看不出來——
//!   task 收攤時自己把那格關掉，「重開就重訂」在這條路才成立；🚫 關線不是重訂，重開是 daemon 的鉤子（`link_keeper.rs`）在解鎖／登入時做的。
//! - **daemon 只管訂閱當下**：一包來寫一包、commit 之後發 `room.message`。**🚫 不碰水位、不記洞、不補窗**。
//! - 水位（`cg_seq`）只由 UI 叫的 `sync.recent` 動；推播漏掉的（server 的 `gap`、本地丟包、一包解不開、寫失敗）**都不管**：
//!   UI 下次叫 `Recent` 會從它自己決定的起點重拉那一段（冪等），UI 不叫就不補，永遠拿不到也不管。誰記有沒有漏是 UI 層的事。
//!
//! 加密的事件收到時有金鑰就解（密文明文一起存），沒金鑰只存密文（維護者 2026-09-29，room_crypto.rs）；訂閱是純的，`seq` 跳號不管、`Subscribe` 不帶 `cg_seq`。
//! `DeviceChanged` 原樣轉成 `CoreEvent::DeviceChanged` 給 UI，要不要 refresh 是 UI 的事（/docs/design/keys/e2ee-rpc.md §4）。

use std::sync::Arc;
use std::time::Duration;

use wbf_sdk::channel::Channel;
use wbf_sdk::client::{RoomSubscription, WbfClient};
use wbf_sdk::crypto_engine::OlmEngine;
use wbf_sdk::event_json::messages_from_incoming;
use wbf_sdk::protocol::{EventSubscribeReply, PushMeta};
use wbf_sdk::IncomingEvent;

use crate::accounts::AccountDir;
use crate::error::CoreError;
use crate::event::{EventSink, LinkState};
use crate::link_pool::{LinkPool, LinkRole};
use crate::room_crypto;
use crate::server_cache::ServerCache;
use crate::{Core, CoreEvent};

/// 等 `Subscribe` 的 Ack 最久多久。
const ACK_TIMEOUT: Duration = Duration::from_secs(30);
/// 訂閱 task 每次等推播最久多久：到了只是「這段時間沒事」，繼續等（心跳另外在線上跑）。
const PUSH_IDLE_TIMEOUT: Duration = Duration::from_secs(60);
/// 關訂閱線時等 task 收攤最久多久；不肯就 abort。
const STOP_TIMEOUT: Duration = Duration::from_secs(5);

/// 一個帳號的收推播 task。丟掉就 abort（`Core` 丟掉、或線重開換新的一個）。
pub(crate) struct RoomSyncHandle {
    task: tokio::task::JoinHandle<()>,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
}

impl Drop for RoomSyncHandle {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// task 自己握著的東西——🚫 不握 `Core`（task 是 `'static`），也不握線（線在池裡；訂閱是線上的一個會話，guard 放掉之後線照樣能用）。
struct RoomSyncTask {
    me: String,
    cache: Arc<ServerCache>,
    events: EventSink,
    /// 這個帳號的池：只為了收攤時把訂閱那格關掉（模組註解）。
    pool: Arc<LinkPool>,
    /// 解密用；None ＝ 引擎開不起來（講過一聲），密文照存、之後金鑰那半補解。
    engine: Option<Arc<OlmEngine>>,
}

impl Core {
    /// 池開完一條線的通用初始化（維護者 2026-09-22）：看角色決定還要做什麼。
    ///
    /// | 角色 | 做什麼 |
    /// |---|---|
    /// | `Rooms` | `Event/Subscribe`（帳號層、不帶 `cg_seq`）→ Ack 之後起收推播的 task（之前有的話換掉：它的線已經死了）；訂閱結束時 task 關這格線 |
    /// | `Keys` | `Device/Subscribe` → 追平 → 收金鑰的 task（`key_sync.rs`）；訂閱結束時 task 關這格線 |
    /// | 其他 | 不做事 |
    ///
    /// Args:
    ///     role: 這條線的角色, example: LinkRole::Rooms
    ///     client: 已經 hello 過的線
    /// Return:
    ///     Ok(())
    ///     Err(Network)    Ack 沒等到、線死了（池就當這條沒開成）
    ///     Err(Server)     server 拒
    ///     Err(AccountBusy) 正在登出：不替它訂
    pub(crate) async fn init_connection(
        &self,
        account: &AccountDir,
        role: LinkRole,
        client: &mut WbfClient<Channel>,
    ) -> Result<(), CoreError> {
        match role {
            LinkRole::Rooms => self.init_rooms(account, client).await,
            LinkRole::Keys => self.init_keys(account, client).await,
            LinkRole::Misc | LinkRole::Upload | LinkRole::Download => Ok(()),
        }
    }

    /// 房間那條線開好之後：`Event/Subscribe` → 起收推播的 task。
    async fn init_rooms(
        &self,
        account: &AccountDir,
        client: &mut WbfClient<Channel>,
    ) -> Result<(), CoreError> {
        let (cache, me) = self.server_cache_and_me(account)?;
        let pool = self.pool_of_account(account)?;
        let engine = match self.olm_engine_of(account).await {
            Ok(engine) => Some(engine),
            Err(error) => {
                self.events.progress(format!(
                    "room sync: pushed encrypted messages stay encrypted in the cache for {}: {error}",
                    account.label()
                ));
                None
            }
        };
        let subscription = client.room_subscription(None, ACK_TIMEOUT).await?;
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let task = RoomSyncTask {
            me,
            cache,
            events: self.events.clone(),
            pool,
            engine,
        };
        let handle = RoomSyncHandle {
            task: tokio::spawn(task.run(subscription, stopped)),
            stop: Some(stop),
        };
        // 舊的（線死了留下來的）在這裡被 Drop、abort。
        let _previous = self
            .room_syncs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(account.dir.clone(), handle);
        Ok(())
    }

    /// 收掉這個帳號的收推播 task（關線、登出用）。
    ///
    /// Return:
    ///     bool  true ＝ 本來在跑
    pub(crate) async fn stop_room_sync_of(&self, account: &AccountDir) -> bool {
        let handle = self
            .room_syncs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&account.dir);
        let Some(mut handle) = handle else {
            return false;
        };
        if handle.task.is_finished() {
            return false;
        }
        if let Some(stop) = handle.stop.take() {
            let _ = stop.send(());
        }
        let _ = tokio::time::timeout(STOP_TIMEOUT, &mut handle.task).await;
        true
    }

    /// Return:
    ///     bool  true ＝ 這個帳號的收推播 task 還在（只給測試斷言用；生產路徑看 `link.state`）
    #[cfg(test)]
    pub(crate) fn is_room_syncing(&self, account: &AccountDir) -> bool {
        self.room_syncs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&account.dir)
            .is_some_and(|handle| !handle.task.is_finished())
    }
}

impl RoomSyncTask {
    async fn run(
        self,
        mut subscription: RoomSubscription,
        mut stopped: tokio::sync::oneshot::Receiver<()>,
    ) {
        // Ack 之前就推來的：跟之後的一樣處理。
        let early: Vec<_> = subscription.early_pushes.drain(..).collect();
        for (meta, events) in early {
            self.on_push(meta, events).await;
        }
        loop {
            // 本地收件匣滿過＝丟過包：講一聲就好——補不補是 UI 的事（模組註解）。
            if subscription.take_gap() {
                self.events
                    .progress("room sync: the subscription inbox overflowed; some pushes were dropped (sync.recent refetches them)");
            }
            let next = tokio::select! {
                _ = &mut stopped => return,
                next = subscription.next(PUSH_IDLE_TIMEOUT) => next,
            };
            // 訂閱還活著的那幾種都 `continue`；走到下面的只有「訂閱結束了」，帶著原因。
            let why = match next {
                Ok(Some(EventSubscribeReply::Push { meta, events })) => {
                    self.on_push(meta, events).await;
                    continue;
                }
                // 原樣轉給 UI；要不要 refresh 是 UI 的事（/docs/design/keys/e2ee-rpc.md §4）。
                Ok(Some(EventSubscribeReply::DeviceChanged(changed))) => {
                    self.events.emit(CoreEvent::DeviceChanged {
                        user: self.me.clone(),
                        changed_user: changed.user_id,
                        device_version: changed.device_version,
                        rooms: changed.rooms,
                        gap: changed.gap,
                    });
                    continue;
                }
                Ok(Some(EventSubscribeReply::Acknowledged(_)))
                | Ok(Some(EventSubscribeReply::Unsubscribed { .. })) => continue,
                Err(wbf_sdk::SdkError::Timeout(_)) => continue,
                // 一包壞了就是漏一包：講一聲，繼續收。
                Err(wbf_sdk::SdkError::Protocol(why)) => {
                    self.events.progress(format!(
                        "room sync: a push could not be read and is dropped ({why}); sync.recent refetches it"
                    ));
                    continue;
                }
                Ok(None) => "the server ended the subscription".to_string(),
                Err(error) => error.to_string(),
            };
            let reason = format!("the room subscription ended: {why}");
            // 訂閱會話結束了 socket 可能還活著（server 送 Error，例如被另一台裝置接手），池的殞死偵測看不出來：
            // 這裡把那格關掉、池發 closed；下次鉤子（link_keeper.rs）才重開、重訂（🚫 不在這裡重訂）。
            // 線不在池裡（已經被別人關了）就只講一聲。
            if !self.pool.close(LinkRole::Rooms, &reason).await {
                self.events.emit(CoreEvent::Link {
                    user: self.me.clone(),
                    role: LinkRole::Rooms,
                    state: LinkState::Closed,
                    reason: Some(reason),
                });
            }
            return;
        }
    }

    /// 一包（Ack 之前推來的也走這裡）：`gap` 只講一聲；寫進 cache（照房分組；加密的有金鑰就解）、commit 之後發 `room.message`。🚫 不碰水位（模組註解）。
    ///
    /// 存不了的（沒 `room_id`：不猜房間；沒 `event_id`／`sender`：`upsert_events` 不寫）在分組時就擋掉——
    /// 🚫 不進 `by_room`，所以不會替一則不在庫裡的事件發 `room.message`；數出來、講出來（`Note`）。規則只有一份：`storable_identity`。
    async fn on_push(&self, meta: PushMeta, events: Vec<serde_json::Value>) {
        if meta.gap {
            self.events.progress(
                "room sync: the server dropped pushes before this one (sync.recent refetches them)",
            );
        }
        let mut by_room: std::collections::BTreeMap<String, Vec<IncomingEvent>> =
            std::collections::BTreeMap::new();
        let mut unstorable = 0usize;
        for raw in events {
            let room = raw
                .get("room_id")
                .and_then(|value| value.as_str())
                .map(str::to_string);
            let incoming = match &room {
                Some(room) => room_crypto::to_incoming(self.engine.as_deref(), room, raw).await,
                None => IncomingEvent::from_ws_json(raw),
            };
            match room {
                Some(room) if incoming.storable_identity().is_some() => {
                    by_room.entry(room).or_default().push(incoming)
                }
                _ => unstorable += 1,
            }
        }
        // 通知給折好的訊息（自己送的也發，收的人自己濾）；庫裡存原樣。
        let notices: Vec<CoreEvent> = by_room
            .iter()
            .flat_map(|(room, events)| {
                messages_from_incoming(room, events)
                    .into_iter()
                    .map(|message| CoreEvent::Message {
                        user: self.me.clone(),
                        message: Box::new(message),
                    })
            })
            .collect();
        let me = self.me.clone();
        let written = self
            .cache
            .run(move |cache| {
                let mut unstorable = 0usize;
                for (room, events) in &by_room {
                    unstorable += cache.upsert_events_counted(&me, room, events)?.unstorable;
                }
                Ok(unstorable)
            })
            .await;
        match written {
            Ok(unstorable_in_cache) => {
                // commit 之後才發（PR #32 的規矩）。
                for notice in notices {
                    self.events.emit(notice);
                }
                let dropped = unstorable + unstorable_in_cache;
                if dropped > 0 {
                    self.events.progress(format!(
                        "room sync: {dropped} pushed event(s) could not be stored (no room_id, event_id or sender) and are dropped"
                    ));
                }
            }
            // 寫失敗只報不擋（快取壞了的代價是重拉：UI 的 sync.recent）；沒落地的就不通知。
            Err(error) => self
                .events
                .progress(format!("room sync: cache write failed (ignored): {error}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use serde_json::{json, Value};
    use wbf_sdk::{RecentPlan, Transport};
    use wbf_wire::pack::control;
    use wbf_wire::Kind;

    use crate::event::LinkState;
    use crate::link_pool::LinkRole;
    use crate::test_support::*;
    use crate::{Core, CoreEvent, Target};

    /// 模組註解的形狀：`init_connection` 訂了、🚫 沒叫 Recent → 推一包寫進去、`room.message` 在 commit 之後、**水位不動** →
    /// 帶 gap 的包、壞包（`bc` 對不上）、下一包都一樣：寫得了的寫、講一聲、水位還是不動 → UI 叫 `sync.recent`（帶 `since`）才動水位、才補回漏的 →
    /// 關訂閱線只關那一條。
    #[tokio::test]
    async fn the_task_only_writes_pushes_and_never_touches_the_watermark() {
        let dir = scratch("pure");
        let (core, account) = core_with_wbf_account(&dir).await;
        let mut seen = core.subscribe();
        let events: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
        let (subscription_id, fake, pool) = subscribed(&core, &account, &events).await;
        assert!(core.is_room_syncing(&account));
        assert!(
            fake.recent_requests.lock().unwrap().is_empty(),
            "補窗不是 daemon 的事：開線不叫 Recent"
        );

        // 一包正常的推播：寫進去、commit 之後才有 room.message、水位不動。
        events.lock().unwrap().push(text_event(4801));
        fake.outbound
            .send(push(subscription_id, 0, false, &[text_event(4801)]))
            .await
            .unwrap();
        assert_eq!(next_message(&mut seen).await, "$4801");
        assert!(cached_ids(&core, &account)
            .await
            .contains(&"$4801".to_string()));
        assert_eq!(cg_seq_of(&core, &account).await, None, "🚫 推播不碰水位");

        // server 說有洞（4850 掉了）、一包壞掉、再一包正常：都寫得了的寫、水位還是不動。
        events
            .lock()
            .unwrap()
            .extend([text_event(4850), text_event(4900), text_event(4950)]);
        fake.outbound
            .send(push(subscription_id, 1, true, &[text_event(4900)]))
            .await
            .unwrap();
        assert_eq!(next_message(&mut seen).await, "$4900");
        let mut broken = push(subscription_id, 2, false, &[text_event(4925)]);
        broken.meta = br#"{"bc":2,"fs":4925,"ls":4925,"gap":false}"#.to_vec();
        fake.outbound.send(broken).await.unwrap();
        fake.outbound
            .send(push(subscription_id, 3, false, &[text_event(4950)]))
            .await
            .unwrap();
        assert_eq!(next_message(&mut seen).await, "$4950");
        assert_eq!(
            cg_seq_of(&core, &account).await,
            None,
            "🚫 有洞、壞包、之後的包：水位一樣不動"
        );
        let ids = cached_ids(&core, &account).await;
        assert!(!ids.contains(&"$4850".to_string()), "漏的 daemon 不補");
        assert!(!ids.contains(&"$4925".to_string()), "壞包丟掉");

        // UI 補：`sync.recent` 帶自己的起點（它手上 room.message 最後一則的 g_seq 之前也行，這裡從頭）→ 4850 回來、水位到 4950。
        let (misc, _fake_misc) = memory_client_with_hello(events.clone()).await;
        drop(
            pool.acquire(LinkRole::Misc, || async move { Ok(misc) })
                .await
                .unwrap(),
        );
        let summary = core
            .recent(
                RecentPlan {
                    max_events: None,
                    ..RecentPlan::default()
                },
                Some(4801),
                false,
                Transport::WebSocket,
                &Target::default(),
            )
            .await
            .expect("sync.recent");
        assert_eq!(
            (summary.pulled, summary.cg_seq_before, summary.cg_seq_after),
            (3, Some(4801), Some(4950)),
            "UI 給的起點就是起點"
        );
        assert!(cached_ids(&core, &account)
            .await
            .contains(&"$4850".to_string()));
        assert_eq!(
            cg_seq_of(&core, &account).await,
            Some(4950),
            "水位只由 Recent 動"
        );
        // 之後的推播照樣不碰它。
        events.lock().unwrap().push(text_event(5000));
        fake.outbound
            .send(push(subscription_id, 4, false, &[text_event(5000)]))
            .await
            .unwrap();
        assert_eq!(next_message(&mut seen).await, "$5000");
        assert_eq!(cg_seq_of(&core, &account).await, Some(4950));
        // 沒帶 since：從 daemon 存的水位（4950）起。
        let summary = core
            .recent(
                RecentPlan {
                    max_events: None,
                    ..RecentPlan::default()
                },
                None,
                false,
                Transport::WebSocket,
                &Target::default(),
            )
            .await
            .unwrap();
        assert_eq!(
            (summary.cg_seq_before, summary.cg_seq_after),
            (Some(4950), Some(5000))
        );

        assert!(core.is_room_syncing(&account), "UI 叫 Recent 不影響訂閱");
        fake.task.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 存不了的事件（沒 `sender`）：寫得了的照寫、有 `Note`、🚫 不替它發 `room.message`。
    #[tokio::test]
    async fn an_event_that_can_never_be_stored_is_reported_and_not_announced() {
        let dir = scratch("unstorable");
        let (core, account) = core_with_wbf_account(&dir).await;
        let mut seen = core.subscribe();
        let events: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
        let (subscription_id, fake, _pool) = subscribed(&core, &account, &events).await;
        let mut no_sender = text_event(401);
        no_sender.as_object_mut().unwrap().remove("sender");
        fake.outbound
            .send(push(
                subscription_id,
                0,
                false,
                &[no_sender, text_event(400)],
            ))
            .await
            .unwrap();
        assert_eq!(
            next_message(&mut seen).await,
            "$400",
            "沒 sender 的那則不通知"
        );
        let ids = cached_ids(&core, &account).await;
        assert!(ids.contains(&"$400".to_string()));
        assert!(!ids.contains(&"$401".to_string()), "沒 sender 的不寫");
        let reported = std::iter::from_fn(|| seen.try_recv().ok()).any(|event| {
            matches!(event, CoreEvent::Note { text, .. } if text.contains("1 pushed event(s) could not be stored"))
        });
        assert!(reported, "存不了的要講出來");
        fake.task.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 線死了：task 結束、先發一次 `link.state: closed`（不等池下次取用才發現）、`is_room_syncing` 變 false。task 自己🚫 不重連（重開是看線迴圈的事，/docs/design/daemon/link-pool.md §3.1）。
    #[tokio::test]
    async fn a_dead_line_ends_the_task_and_says_so() {
        let dir = scratch("dead");
        let (core, account) = core_with_wbf_account(&dir).await;
        let mut seen = core.subscribe();
        let (mut client, fake) = memory_client_with_hello(Arc::new(Mutex::new(Vec::new()))).await;
        core.init_connection(&account, LinkRole::Rooms, &mut client)
            .await
            .expect("subscribe");
        // 對方收攤。
        fake.task.abort();
        drop(fake.outbound);
        wait_for_async(
            || async { !core.is_room_syncing(&account) },
            "the task ends",
        )
        .await;
        let closed = std::iter::from_fn(|| seen.try_recv().ok()).find_map(|event| match event {
            CoreEvent::Link {
                role: LinkRole::Rooms,
                state: LinkState::Closed,
                reason,
                ..
            } => Some(reason),
            _ => None,
        });
        assert!(
            closed
                .clone()
                .flatten()
                .is_some_and(|reason| reason.contains("subscription ended")),
            "線死了要講：{closed:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// server 收掉訂閱會話、socket 還活著（例如被另一台裝置接手的 `Error`）：task 把那格關掉（池發 `closed` 帶原因）、🚫 不自己重訂；
    /// 下一次開這條線（鉤子的 `ensure_open`，底下是同一支 `acquire`）走 `open` → `init_connection` → 第二個 `Subscribe`、新訂閱收得到（PR #58 審查 rumia #655／cirno #658：
    /// 之前 task 只發事件不關線，池看 socket 還活著就把舊線交回去，永遠不再訂）。
    #[tokio::test]
    async fn an_ended_subscription_closes_the_line_so_the_next_open_subscribes_again() {
        let dir = scratch("resubscribe");
        let (core, account) = core_with_wbf_account(&dir).await;
        let mut seen = core.subscribe();
        let events: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
        let (subscription_id, fake, pool) = subscribed(&core, &account, &events).await;
        assert_eq!(pool.open_count(), 1);

        fake.outbound
            .send(response(
                Kind::Control,
                control::ERROR,
                subscription_id,
                0,
                json!({ "code": "M_UNKNOWN", "message": "subscription superseded by another device" }),
                Vec::new(),
            ))
            .await
            .unwrap();
        wait_for_async(
            || async { !core.is_room_syncing(&account) },
            "the task ends",
        )
        .await;
        assert_eq!(
            pool.open_count(),
            0,
            "訂閱會話死了就把那條線關掉，socket 活著也一樣"
        );
        let closed = std::iter::from_fn(|| seen.try_recv().ok()).find_map(|event| match event {
            CoreEvent::Link {
                role: LinkRole::Rooms,
                state: LinkState::Closed,
                reason,
                ..
            } => Some(reason),
            _ => None,
        });
        assert!(
            closed
                .clone()
                .flatten()
                .is_some_and(|reason| reason.contains("subscription ended")),
            "關線要帶原因：{closed:?}"
        );

        // 下一次開：那格是空的 → `open`（生產路徑的 open_link：hello 之後 init_connection）→ 第二個 Subscribe、新的 task。
        let (mut client, fake_again) = memory_client_with_hello(events.clone()).await;
        let (core_ref, account_ref) = (&core, &account);
        drop(
            pool.acquire(LinkRole::Rooms, || async move {
                core_ref
                    .init_connection(account_ref, LinkRole::Rooms, &mut client)
                    .await?;
                Ok(client)
            })
            .await
            .unwrap(),
        );
        let again = fake_again.subscription_ids.lock().unwrap().last().copied();
        let again = again.expect("重開就重訂：第二個 Subscribe");
        assert!(core.is_room_syncing(&account));
        events.lock().unwrap().push(text_event(9));
        fake_again
            .outbound
            .send(push(again, 0, false, &[text_event(9)]))
            .await
            .unwrap();
        assert_eq!(next_message(&mut seen).await, "$9", "新訂閱收得到");
        fake.task.abort();
        fake_again.task.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Ack 之前就推來的包（server 先登記再回 Ack）跟之後的一樣：帶 `gap` 也要講一聲（PR #58 審查 salvia #656／cirno #658 🟢）。
    #[tokio::test]
    async fn a_gap_in_a_push_before_the_ack_is_reported_too() {
        let dir = scratch("early-gap");
        let (core, account) = core_with_wbf_account(&dir).await;
        let mut seen = core.subscribe();
        let events: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(vec![text_event(7)]));
        let (mut client, fake) = memory_client_with_hello(events).await;
        *fake.early_push.lock().unwrap() = Some((0, true, vec![text_event(7)]));
        core.init_connection(&account, LinkRole::Rooms, &mut client)
            .await
            .expect("subscribe");
        let mut gap_reported = false;
        let first = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                match seen.recv().await.unwrap() {
                    CoreEvent::Note { text, .. } if text.contains("the server dropped pushes") => {
                        gap_reported = true
                    }
                    CoreEvent::Message { message, .. } => return message.id,
                    _ => {}
                }
            }
        })
        .await
        .expect("the early push arrives");
        assert_eq!(first, "$7", "Ack 之前推來的照寫、照通知");
        assert!(gap_reported, "Ack 之前的 gap 也要講");
        fake.task.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 對真的 wbfuwunel：alice 登入、鉤子開五條線（`ensure_links`）；bob（另一個 `Core`、另一個資料目錄）用 `Event/Send` 送一則；alice 的 `room.message` 在時限內到、
    /// cache 有它、水位不動；兩邊登出（task 跟著收掉）。
    ///
    /// `--ignored`；環境變數：`WBF_E2E_SERVER`、`WBF_E2E_USER`（完整 mxid）、`WBF_E2E_PASSWORD_FILE`、`WBF_E2E_USER_B`、`WBF_E2E_PASSWORD_B_FILE`、
    /// `WBF_E2E_ROOM`（兩人都在的明文房）。
    #[tokio::test]
    #[ignore = "needs a running wbfuwunel: WBF_E2E_SERVER, WBF_E2E_USER, WBF_E2E_PASSWORD_FILE, WBF_E2E_USER_B, WBF_E2E_PASSWORD_B_FILE, WBF_E2E_ROOM"]
    async fn a_subscription_receives_what_another_account_sends_over_the_real_server() {
        let env = |name: &str| std::env::var(name).unwrap_or_else(|_| panic!("{name}"));
        let password_of = |file: &str| {
            let text = std::fs::read_to_string(file).unwrap();
            text.strip_suffix('\n').unwrap_or(&text).to_string()
        };
        let server = env("WBF_E2E_SERVER");
        let room = env("WBF_E2E_ROOM");
        let (user_a, password_a) = (
            env("WBF_E2E_USER"),
            password_of(&env("WBF_E2E_PASSWORD_FILE")),
        );
        let (user_b, password_b) = (
            env("WBF_E2E_USER_B"),
            password_of(&env("WBF_E2E_PASSWORD_B_FILE")),
        );
        let (dir_a, dir_b) = (scratch("real-a"), scratch("real-b"));
        let core_a = Core::open(&dir_a);
        core_a.create_vault(None).unwrap();
        core_a
            .log_in(&server, &user_a, &password_a, "room-sync e2e A", true)
            .await
            .expect("alice logs in");
        let core_b = Core::open(&dir_b);
        core_b.create_vault(None).unwrap();
        core_b
            .log_in(&server, &user_b, &password_b, "room-sync e2e B", true)
            .await
            .expect("bob logs in");
        let account_a = core_a.current_account().unwrap();
        let mut seen = core_a.subscribe();

        let ensured = core_a.ensure_links().await;
        assert!(ensured.failed.is_empty(), "五條都開得起來：{ensured:?}");
        assert_eq!(ensured.opened.len(), 5, "五條都是這次開的：{ensured:?}");
        assert!(core_a.is_room_syncing(&account_a));

        // 送文字只看本地記的加不加密（維護者 2026-10-05）：照 UI 的順序先拿那間房。
        core_b
            .conversation(&room, crate::SyncMode::Both, &Target::default())
            .await
            .expect("bob fetches the room");
        let body = format!("room sync e2e {}", std::process::id());
        let event_id = core_b
            .send_text(
                &room,
                &body,
                &crate::SendOptions::default(),
                &Target::default(),
            )
            .await
            .expect("bob sends over Event/Send");
        let arrived = tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                if let CoreEvent::Message { user, message } = seen.recv().await.unwrap() {
                    if message.id == event_id {
                        assert_eq!(user, user_a);
                        return message;
                    }
                }
            }
        })
        .await
        .expect("alice gets bob's message as a push within 20 s");
        assert_eq!(arrived.conversation, room);
        // 讀完就放：登出要關 cache.db，還有人握著它會被拒。
        let cached = {
            let (cache, me) = core_a.server_cache_and_me(&account_a).unwrap();
            let reader = cache.read().await;
            reader
                .list_messages_by_event_ids(&me, &room, std::slice::from_ref(&event_id))
                .unwrap()
        };
        assert_eq!(cached.len(), 1, "commit 之後才發事件，所以此刻庫裡一定有");
        assert_eq!(cg_seq_of(&core_a, &account_a).await, None, "推播不碰水位");

        core_a
            .log_out(&user_a, None, true, true)
            .await
            .expect("alice logs out");
        assert!(!core_a.is_room_syncing(&account_a), "登出收掉 task");
        core_b
            .log_out(&user_b, None, true, true)
            .await
            .expect("bob logs out");
        let _ = std::fs::remove_dir_all(&dir_a);
        let _ = std::fs::remove_dir_all(&dir_b);
    }
}

//! 後台送房間金鑰（/docs/design/keys/e2ee-rpc.md §3.1）：每個帳號一個 task，只做一件事——把排在房間金鑰上的 to-device 送到該拿的裝置。
//!
//! - 誰都🚫 等它（維護者 2026-10-05：送訊息🚫 綁發金鑰）。送出、refresh、1506 之後只「交給它」（[`Core::queue_room_key_share`]）就走。
//! - 走 `Keys` 線、用 `reuse`，🚫 自己開線：線沒開就等下一件事，或等 `init_keys` 開好線時的那一聲（[`Core::wake_room_key_share`]）。
//! - 失敗的房留著，隔一段時間再試（`RETRY_FIRST` 起加倍到 `RETRY_MAX`），講一聲（`Note`），🚫 回給誰（沒有人在等）。
//! - 只在記憶體記「哪些房還有事」；要送的 to-device 存在上游那把 session 上（crypto store）。daemon 重開之後要等那個房下一次送或 refresh（§8）。

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio::time::Instant;
use wbf_sdk::crypto_engine::OlmEngine;

use crate::accounts::AccountDir;
use crate::event::EventSink;
use crate::link_pool::{LinkPool, LinkRole};
use crate::Core;

/// 一個房送失敗之後，第一次隔多久再試；之後每次加倍，到 `RETRY_MAX` 為止（維護者 2026-10-05 照預設：30 秒到 5 分鐘）。
#[cfg(not(test))]
const RETRY_FIRST: Duration = Duration::from_secs(30);
#[cfg(not(test))]
const RETRY_MAX: Duration = Duration::from_secs(300);
/// 測試裡縮短才等得到重試；形狀一樣（起點、加倍、上限）。
#[cfg(test)]
const RETRY_FIRST: Duration = Duration::from_millis(100);
#[cfg(test)]
const RETRY_MAX: Duration = Duration::from_millis(800);

/// 交給後台的一件事。
enum KeyShareWork {
    /// 這個房的房間金鑰要送給這些人（UI 帶來的那份 join 成員，含自己）；同一個房還沒做完又來，成員取最新的。
    Room { room: String, members: Vec<String> },
    /// `Keys` 線剛開好：手上還沒做完的房再跑一輪。
    LineOpened,
}

/// 一個帳號的送金鑰 task。丟掉就 abort（登出、`Core` 丟掉）。
pub(crate) struct KeyShareHandle {
    work: UnboundedSender<KeyShareWork>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for KeyShareHandle {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// task 自己握著的東西——🚫 不握 `Core`、不握線：線在池裡，要用時 `reuse` 那一格。
struct KeyShareTask {
    engine: Arc<OlmEngine>,
    pool: Arc<LinkPool>,
    events: EventSink,
}

impl Core {
    /// 把這個房的房間金鑰交給後台送（/docs/design/keys/e2ee-rpc.md §3.1）；🚫 等它送。
    /// 起不了後台（引擎開不起來、正在登出）只講一聲：金鑰還排在 session 上，下一次送或 refresh 會再交。
    ///
    /// Args:
    ///     room: example: "!r:localhost"
    ///     members: UI 帶來的那份 join 成員（含自己）, example: vec!["@alice:localhost".to_string(), "@bob:localhost".to_string()]
    pub(crate) async fn queue_room_key_share(
        &self,
        account: &AccountDir,
        room: &str,
        members: Vec<String>,
    ) {
        let work = KeyShareWork::Room {
            room: room.to_string(),
            members,
        };
        let Some(work) = self.hand_key_share_work(account, work) else {
            return;
        };
        // 還沒有後台（第一次、或先前收掉了）：起一個。開引擎是 async，鎖外做，起的時候再看一次（同時進來的另一個可能剛起好）。
        let started = async {
            let engine = self.olm_engine_of(account).await?;
            let pool = self.pool_of_account(account)?;
            Ok::<_, crate::CoreError>((engine, pool))
        }
        .await;
        let (engine, pool) = match started {
            Ok(started) => started,
            Err(error) => {
                self.events.progress(format!(
                    "room keys: the key of {room} stays queued in the crypto store; the background sender for {} could not start: {error}",
                    account.label()
                ));
                return;
            }
        };
        let mut shares = self
            .key_shares
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(handle) = shares.get(&account.dir) {
            if !handle.task.is_finished() {
                // 有人剛起好：交給它。它收不下（剛結束）就算了，下一次送會再交。
                let _ = handle.work.send(work);
                return;
            }
        }
        let (sender, receiver) = unbounded_channel();
        // 收的那頭在這裡、還活著：送不出去是到不了的，但🚫 靠它，失敗就是這一件沒交上（下一次送會再交）。
        let _ = sender.send(work);
        let task = KeyShareTask {
            engine,
            pool,
            events: self.events.clone(),
        };
        shares.insert(
            account.dir.clone(),
            KeyShareHandle {
                work: sender,
                task: tokio::spawn(task.run(receiver)),
            },
        );
    }

    /// `Keys` 線開好了（`init_keys` 叫）：手上還沒做完的房再跑一輪。這個帳號還沒有後台就什麼都不做（沒有事要送）。
    pub(crate) fn wake_room_key_share(&self, account: &AccountDir) {
        let _ = self.hand_key_share_work(account, KeyShareWork::LineOpened);
    }

    /// 收掉這個帳號的送金鑰 task（登出用）。還沒送的仍在 crypto store 那把 session 上，但登出會刪 store。
    pub(crate) fn stop_room_key_share_of(&self, account: &AccountDir) {
        let _ = self
            .key_shares
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&account.dir);
    }

    /// 交給這個帳號正在跑的後台。
    ///
    /// Return:
    ///     None                  交上了
    ///     Some(KeyShareWork)    沒有正在跑的後台（原樣還給呼叫端）
    fn hand_key_share_work(
        &self,
        account: &AccountDir,
        work: KeyShareWork,
    ) -> Option<KeyShareWork> {
        let shares = self
            .key_shares
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match shares.get(&account.dir) {
            Some(handle) if !handle.task.is_finished() => match handle.work.send(work) {
                Ok(()) => None,
                Err(unsent) => Some(unsent.0),
            },
            _ => Some(work),
        }
    }

    /// Return:
    ///     bool  true ＝ 這個帳號的送金鑰 task 還在（只給測試斷言用）
    #[cfg(test)]
    pub(crate) fn is_sharing_room_keys(&self, account: &AccountDir) -> bool {
        self.key_shares
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&account.dir)
            .is_some_and(|handle| !handle.task.is_finished())
    }
}

impl KeyShareTask {
    async fn run(self, mut work: UnboundedReceiver<KeyShareWork>) {
        let mut rooms: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let mut retry_in = RETRY_FIRST;
        let mut retry_at: Option<Instant> = None;
        loop {
            // 等下一件事；有房在等重試就最多等到那一刻。收的那頭沒人了（`Core` 丟掉）就結束。
            let next = match retry_at {
                Some(at) => tokio::select! {
                    next = work.recv() => match next {
                        Some(next) => Some(next),
                        None => return,
                    },
                    () = tokio::time::sleep_until(at) => None,
                },
                None => match work.recv().await {
                    Some(next) => Some(next),
                    None => return,
                },
            };
            take_work(&mut rooms, next);
            while let Ok(more) = work.try_recv() {
                take_work(&mut rooms, Some(more));
            }
            if rooms.is_empty() {
                retry_at = None;
                continue;
            }
            // 🚫 開線（/docs/design/keys/e2ee-rpc.md §3.1）：沒開就等下一件事、或開線那一聲。
            let Some(mut line) = self.pool.reuse(LinkRole::Keys).await else {
                retry_at = None;
                continue;
            };
            let mut failed = false;
            let pending: Vec<(String, Vec<String>)> = rooms
                .iter()
                .map(|(room, members)| (room.clone(), members.clone()))
                .collect();
            for (room, members) in pending {
                match self
                    .engine
                    .distribute_room_key(&mut line, &room, &members)
                    .await
                {
                    Ok(_) => {
                        rooms.remove(&room);
                    }
                    Err(error) => {
                        failed = true;
                        self.events.progress(format!(
                            "room keys: sending the key of {room} failed (retried in {}s): {error}",
                            retry_in.as_secs_f32()
                        ));
                    }
                }
            }
            drop(line);
            if failed {
                retry_at = Some(Instant::now() + retry_in);
                retry_in = (retry_in * 2).min(RETRY_MAX);
            } else {
                retry_at = None;
                retry_in = RETRY_FIRST;
            }
        }
    }
}

/// 一件事併進「哪些房還有事」：同一個房只留一件，成員取最新的；開線那一聲與重試到時不加新的房。
fn take_work(rooms: &mut BTreeMap<String, Vec<String>>, work: Option<KeyShareWork>) {
    if let Some(KeyShareWork::Room { room, members }) = work {
        rooms.insert(room, members);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;
    use std::sync::{Arc, Mutex};

    use super::{RETRY_FIRST, RETRY_MAX};
    use crate::accounts::AccountDir;
    use crate::link_pool::LinkRole;
    use crate::room_crypto::{RoomDevices, SendOptions};
    use crate::test_support::*;
    use crate::{Core, Target};

    /// 一條放進池裡、接著假 server 的線。
    async fn line_on_fake_server(core: &Core, account: &AccountDir, role: LinkRole) -> FakeServer {
        let (client, fake) = memory_client_with_hello(Arc::new(Mutex::new(Vec::new()))).await;
        drop(
            core.pool_of_account(account)
                .unwrap()
                .acquire(role, || async move { Ok(client) })
                .await
                .unwrap(),
        );
        fake
    }

    /// `Misc` 線接假 server：`ROOM` 在 server 與本地都是加密房、房間版本號 7、成員只有自己。
    async fn encrypted_room_on_misc(core: &Core, account: &AccountDir) -> FakeServer {
        let misc = line_on_fake_server(core, account, LinkRole::Misc).await;
        misc.room_is_encrypted.store(true, Ordering::SeqCst);
        *misc.current_room_version.lock().unwrap() = Some(7);
        remember_room(core, account, ROOM, true).await;
        misc
    }

    /// UI 手上那份：房間版本號 7、成員只有自己（裝置版本號跟假 server 的 `Members` 同一個）。
    fn send_options(txn_id: &str) -> SendOptions {
        let hash =
            wbf_sdk::device_version::compute_device_keys_hash(ME, &serde_json::json!({})).unwrap();
        SendOptions {
            room_devices: Some(RoomDevices {
                room_version: 7,
                members: [(ME.to_string(), format!("1-{hash}"))].into(),
            }),
            txn_id: Some(txn_id.to_string()),
        }
    }

    /// 送訊息🚫 綁發金鑰（維護者 2026-10-05，/docs/design/keys/e2ee-rpc.md §3）：送的那一路假 server 一個走橋的請求都🚫 收到
    /// （`Members`／`/keys/query`／`/keys/claim`／`sendToDevice` 全沒有），`Event/Send` 送的是密文；金鑰之後由後台走 `Keys` 線送（§3.1）。
    #[tokio::test]
    async fn an_encrypted_send_asks_for_no_keys_and_the_background_sends_them_on_the_keys_line() {
        let dir = scratch("key-share-send");
        let (core, account) = core_with_wbf_account(&dir).await;
        let misc = encrypted_room_on_misc(&core, &account).await;
        let keys = line_on_fake_server(&core, &account, LinkRole::Keys).await;

        core.send_text(ROOM, "hi", &send_options("t-1"), &Target::default())
            .await
            .expect("sent without waiting for any key");
        assert_eq!(
            *misc.bridge_calls.lock().unwrap(),
            Vec::new(),
            "the send path asks the server for no keys"
        );
        let sent = misc.sent_events.lock().unwrap().clone();
        assert_eq!(sent.len(), 1, "{sent:?}");
        assert_eq!(
            (sent[0].1.as_str(), sent[0].2),
            ("m.room.encrypted", Some(7))
        );

        wait_for_async(
            || async { !keys.bridge_calls.lock().unwrap().is_empty() },
            "the background works on the Keys line",
        )
        .await;
        assert!(core.is_sharing_room_keys(&account));
        assert_eq!(
            *misc.bridge_calls.lock().unwrap(),
            Vec::new(),
            "the background does not use the line the command used"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 後台🚫 自己開線：`Keys` 線沒開就等，開好那一聲（`init_keys` 叫的 `wake_room_key_share`）來了才送。
    /// 送失敗的房隔一段時間（`RETRY_FIRST`）再試；送成就停，🚫 一直重送。
    #[tokio::test]
    async fn the_background_waits_for_the_keys_line_and_retries_only_what_failed() {
        let dir = scratch("key-share-retry");
        let (core, account) = core_with_wbf_account(&dir).await;
        let _misc = encrypted_room_on_misc(&core, &account).await;

        core.send_text(ROOM, "hi", &send_options("t-1"), &Target::default())
            .await
            .expect("sent while the Keys line is closed");
        tokio::time::sleep(RETRY_FIRST * 3).await;
        let pool = core.pool_of_account(&account).unwrap();
        assert!(
            pool.reuse(LinkRole::Keys).await.is_none(),
            "the background does not open the Keys line"
        );

        let keys = line_on_fake_server(&core, &account, LinkRole::Keys).await;
        keys.fail_bridged.store(1, Ordering::SeqCst);
        core.wake_room_key_share(&account);
        wait_for_async(
            || async {
                keys.fail_bridged.load(Ordering::SeqCst) == 0
                    && keys.bridge_calls.lock().unwrap().len() >= 2
            },
            "the first attempt failed and a retry followed",
        )
        .await;
        // 重試成功之後就停：等過最長的間隔，走橋的呼叫數不再變。
        tokio::time::sleep(RETRY_MAX).await;
        let settled = keys.bridge_calls.lock().unwrap().len();
        tokio::time::sleep(RETRY_MAX * 2).await;
        assert_eq!(
            keys.bridge_calls.lock().unwrap().len(),
            settled,
            "nothing is sent again once the room's key went out"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

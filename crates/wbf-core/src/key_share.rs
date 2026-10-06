//! 金鑰線的 queue（/docs/design/keys/e2ee-rpc.md §3.1）：每個帳號一個後台 task，把「要上傳到 server 的金鑰」送到、**server 回 Ack 才算數**——
//! 排在房間金鑰上的 to-device（送給該拿的裝置），以及這台裝置自己的金鑰（裝置金鑰、一次性金鑰）上傳失敗的那次。
//!
//! - 誰都🚫 等它（維護者 2026-10-05：送訊息🚫 綁發金鑰）。送出、refresh、1506 之後只「交給它」（[`Core::queue_room_key_share`]）就走。
//! - 走 `Keys` 線、用 `reuse`，🚫 自己開線：線沒開就等，`init_keys` 開好線時叫醒它（[`Core::key_share_inbox`] ＋ [`KeyShareInbox::line_opened`]）。
//! - 失敗的留著，隔一段時間再試（`RETRY_FIRST` 起加倍到 `RETRY_MAX`），講一聲（`Note`），🚫 回給誰（沒有人在等）。
//! - **還有哪些房沒送完存在 `m/ks.sealed`**（維護者 2026-10-06，/docs/design/storage/local-storage.md）：daemon 重開、`Keys` 線一開就照著補。
//!   要送的 to-device 本身存在上游那把 session 上（crypto store），這個檔只記「哪個房、送給哪些成員」，第三把子金鑰封著。
//!   自己的金鑰沒存：上游自己記得還有沒上傳的，每次開線 `init_keys` 都會再上傳一次。

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio::time::Instant;
use wbf_sdk::crypto_engine::OlmEngine;
use wbf_sdk::vault::KeyShareQueueFile;

use crate::accounts::AccountDir;
use crate::error::CoreError;
use crate::event::EventSink;
use crate::link_pool::{LinkPool, LinkRole};
use crate::Core;

/// 金鑰線的 queue 存在帳號目錄的 `m/` 裡（跟 crypto store 同生共死：`m/` 被刪它就一起沒）。
pub(crate) const KEY_SHARE_QUEUE_FILE_NAME: &str = "ks.sealed";

/// 一件事送失敗之後，第一次隔多久再試；之後每次加倍，到 `RETRY_MAX` 為止（維護者 2026-10-05 照預設：30 秒到 5 分鐘）。
#[cfg(not(test))]
const RETRY_FIRST: Duration = Duration::from_secs(30);
#[cfg(not(test))]
const RETRY_MAX: Duration = Duration::from_secs(300);
/// 測試裡縮短才等得到重試；形狀一樣（起點、加倍、上限）。
#[cfg(test)]
const RETRY_FIRST: Duration = Duration::from_millis(100);
#[cfg(test)]
const RETRY_MAX: Duration = Duration::from_millis(800);
/// 收 task 時等它結束最久多久（abort 之後通常下一次 poll 就放手）。
const STOP_TIMEOUT: Duration = Duration::from_secs(5);

/// `m/ks.sealed` 解開之後的樣子（我們自己的格式，/docs/design/storage/local-storage.md）。
#[derive(Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
struct QueuedRoomKeys {
    /// 格式版本；看不懂的版本🚫 猜。
    v: u32,
    /// 房間 id → 要拿這把房間金鑰的成員（UI 帶來的那份 join 成員，含自己）。
    rooms: BTreeMap<String, Vec<String>>,
}

const QUEUED_ROOM_KEYS_VERSION: u32 = 1;

/// 交給後台的一件事。
enum KeyShareWork {
    /// 這個房的房間金鑰要送給這些人；同一個房還沒做完又來，成員取最新的。
    Room { room: String, members: Vec<String> },
    /// 這台裝置自己的金鑰上傳失敗了：之後重試到 server 回 Ack。
    UploadOwnKeys,
    /// `Keys` 線剛開好：手上還沒做完的再跑一輪。
    LineOpened,
}

/// 往這個帳號的後台交事情的那頭（`init_keys` 與收金鑰的 task 各握一份）。後台收掉了就交不上，回 false。
#[derive(Clone)]
pub(crate) struct KeyShareInbox(UnboundedSender<KeyShareWork>);

impl KeyShareInbox {
    /// 這台裝置自己的金鑰上傳失敗，交給後台重試。
    ///
    /// Return:
    ///     bool  true ＝ 交上了；false ＝ 後台已經收掉（下次開線 `init_keys` 會再上傳）
    pub(crate) fn retry_own_key_upload(&self) -> bool {
        self.0.send(KeyShareWork::UploadOwnKeys).is_ok()
    }

    /// `Keys` 線開好了：手上還沒做完的（含上次 daemon 留在 `m/ks.sealed` 的）再跑一輪。
    pub(crate) fn line_opened(&self) {
        let _ = self.0.send(KeyShareWork::LineOpened);
    }
}

/// 一個帳號的送金鑰 task。丟掉就 abort（登出、`Core` 丟掉）。
pub(crate) struct KeyShareHandle {
    inbox: KeyShareInbox,
    task: tokio::task::JoinHandle<()>,
    /// 試過幾次送一個房（只給測試：送成之後再試不會走橋，從假 server 看不出來有沒有停）。
    #[cfg(test)]
    attempts: Arc<std::sync::atomic::AtomicU32>,
}

impl Drop for KeyShareHandle {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// task 自己握著的東西——🚫 不握 `Core`、不握線（線在池裡，要用時 `reuse` 那一格）、🚫 握主金鑰（queue 檔只帶第三把子金鑰）。
struct KeyShareTask {
    engine: Arc<OlmEngine>,
    pool: Arc<LinkPool>,
    events: EventSink,
    queue_file: KeyShareQueueFile,
    #[cfg(test)]
    attempts: Arc<std::sync::atomic::AtomicU32>,
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
        let handed = match self.key_share_inbox(account).await {
            Ok(inbox) => inbox.0.send(work).is_ok(),
            Err(_) => false,
        };
        if !handed {
            self.events.progress(format!(
                "room keys: the key of {room} stays queued in the crypto store; the background sender for {} is not running",
                account.label()
            ));
        }
    }

    /// 這個帳號的後台：在跑就給它的 inbox；沒有就起一個——起的時候讀回 `m/ks.sealed` 上次沒送完的房。
    ///
    /// Return:
    ///     Ok(KeyShareInbox)
    ///     Err(...)    引擎開不起來、正在登出、沒解鎖
    pub(crate) async fn key_share_inbox(
        &self,
        account: &AccountDir,
    ) -> Result<KeyShareInbox, CoreError> {
        if let Some(inbox) = self.running_key_share_inbox(account) {
            return Ok(inbox);
        }
        // 開引擎是 async，鎖外做；起的時候再看一次（同時進來的另一個可能剛起好）。
        let engine = self.olm_engine_of(account).await?;
        let pool = self.pool_of_account(account)?;
        let queue_file = self
            .vault()?
            .key_share_queue_file(&account.matrix_store_dir().join(KEY_SHARE_QUEUE_FILE_NAME));
        let mut shares = self
            .key_shares
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(handle) = shares.get(&account.dir) {
            if !handle.task.is_finished() {
                return Ok(handle.inbox.clone());
            }
        }
        let (sender, receiver) = unbounded_channel();
        let inbox = KeyShareInbox(sender);
        #[cfg(test)]
        let attempts = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let task = KeyShareTask {
            engine,
            pool,
            events: self.events.clone(),
            queue_file,
            #[cfg(test)]
            attempts: attempts.clone(),
        };
        shares.insert(
            account.dir.clone(),
            KeyShareHandle {
                inbox: inbox.clone(),
                task: tokio::spawn(task.run(receiver)),
                #[cfg(test)]
                attempts,
            },
        );
        Ok(inbox)
    }

    /// 收掉這個帳號的送金鑰 task（登出用），**等它真的結束**才回：task 握著引擎（`m/` 的 sqlite），
    /// 只 abort 不等的話它要等下一次被 poll 才放手，而登出接著就要刪 `m/`（Windows 上開著刪不掉；單執行緒的 runtime 上刪檔的重試還會擋住它被 poll）。
    /// 還沒送的仍在 crypto store 與 `m/ks.sealed`，但登出會刪整個 `m/`。
    pub(crate) async fn stop_room_key_share_of(&self, account: &AccountDir) {
        let handle = self
            .key_shares
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&account.dir);
        let Some(mut handle) = handle else {
            return;
        };
        handle.task.abort();
        if tokio::time::timeout(STOP_TIMEOUT, &mut handle.task)
            .await
            .is_err()
        {
            self.events.progress(format!(
                "room keys: the background key sender for {} did not stop within {}s",
                account.label(),
                STOP_TIMEOUT.as_secs()
            ));
        }
    }

    /// Return:
    ///     Some(KeyShareInbox)   這個帳號的後台正在跑
    ///     None                  還沒起、或已經結束
    fn running_key_share_inbox(&self, account: &AccountDir) -> Option<KeyShareInbox> {
        self.key_shares
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&account.dir)
            .filter(|handle| !handle.task.is_finished())
            .map(|handle| handle.inbox.clone())
    }

    /// Return:
    ///     bool  true ＝ 這個帳號的送金鑰 task 還在（只給測試斷言用）
    #[cfg(test)]
    pub(crate) fn is_sharing_room_keys(&self, account: &AccountDir) -> bool {
        self.running_key_share_inbox(account).is_some()
    }

    /// Return:
    ///     u32  這個帳號的後台試過幾次送一個房（只給測試斷言。還沒有後台是 0）
    #[cfg(test)]
    pub(crate) fn room_key_share_attempts(&self, account: &AccountDir) -> u32 {
        self.key_shares
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&account.dir)
            .map_or(0, |handle| {
                handle.attempts.load(std::sync::atomic::Ordering::SeqCst)
            })
    }
}

impl KeyShareTask {
    async fn run(self, mut work: UnboundedReceiver<KeyShareWork>) {
        let mut rooms = self.load_rooms();
        let mut upload_own_keys = false;
        let mut retry_in = RETRY_FIRST;
        let mut retry_at: Option<Instant> = None;
        loop {
            // 等下一件事；有事在等重試就最多等到那一刻。收的那頭沒人了（`Core` 丟掉）就結束。
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
            let mut changed = take_work(&mut rooms, &mut upload_own_keys, next);
            while let Ok(more) = work.try_recv() {
                changed |= take_work(&mut rooms, &mut upload_own_keys, Some(more));
            }
            if changed {
                self.save_rooms(&rooms);
            }
            if rooms.is_empty() && !upload_own_keys {
                retry_at = None;
                continue;
            }
            // 🚫 開線（/docs/design/keys/e2ee-rpc.md §3.1）：沒開就等下一件事、或開線那一聲。
            let Some(mut line) = self.pool.reuse(LinkRole::Keys).await else {
                retry_at = None;
                continue;
            };
            let mut failed = false;
            if upload_own_keys {
                match self.engine.send_outgoing_requests(&mut line).await {
                    Ok(_) => upload_own_keys = false,
                    Err(error) => {
                        failed = true;
                        self.events.progress(format!(
                            "keys: uploading this device's keys failed (retried in {}s): {error}",
                            retry_in.as_secs_f32()
                        ));
                    }
                }
            }
            let pending: Vec<(String, Vec<String>)> = rooms
                .iter()
                .map(|(room, members)| (room.clone(), members.clone()))
                .collect();
            let mut sent_any = false;
            for (room, members) in pending {
                #[cfg(test)]
                self.attempts
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                match self
                    .engine
                    .distribute_room_key(&mut line, &room, &members)
                    .await
                {
                    Ok(_) => {
                        rooms.remove(&room);
                        sent_any = true;
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
            if sent_any {
                self.save_rooms(&rooms);
            }
            if failed {
                retry_at = Some(Instant::now() + retry_in);
                retry_in = (retry_in * 2).min(RETRY_MAX);
            } else {
                retry_at = None;
                retry_in = RETRY_FIRST;
            }
        }
    }

    /// 上次 daemon 沒送完的房（`m/ks.sealed`）。沒有檔就是沒有；檔壞了或看不懂就講一聲、從空的開始——
    /// 🚫 刪它（下次寫的時候才蓋掉），那些房下一次送或 refresh 會再交進來。
    fn load_rooms(&self) -> BTreeMap<String, Vec<String>> {
        let parsed =
            self.queue_file
                .read()
                .and_then(|plaintext| match plaintext {
                    None => Ok(QueuedRoomKeys::default()),
                    Some(plaintext) => serde_json::from_slice::<QueuedRoomKeys>(&plaintext)
                        .map_err(|error| {
                            wbf_sdk::SdkError::Protocol(format!(
                                "the key-share queue is not readable: {error}"
                            ))
                        }),
                });
        match parsed {
            Ok(queue) if queue.v == QUEUED_ROOM_KEYS_VERSION || queue.rooms.is_empty() => {
                queue.rooms
            }
            Ok(queue) => {
                self.events.progress(format!(
                    "room keys: the key-share queue has version {}, this build understands {QUEUED_ROOM_KEYS_VERSION}; starting empty",
                    queue.v
                ));
                BTreeMap::new()
            }
            Err(error) => {
                self.events.progress(format!(
                    "room keys: the key-share queue could not be read ({error}); starting empty"
                ));
                BTreeMap::new()
            }
        }
    }

    /// 寫回 `m/ks.sealed`（空了就刪檔）。寫失敗只講一聲：記憶體裡的照送，只是重開之後不會接著送。
    fn save_rooms(&self, rooms: &BTreeMap<String, Vec<String>>) {
        let saved = if rooms.is_empty() {
            self.queue_file.remove()
        } else {
            serde_json::to_vec(&QueuedRoomKeys {
                v: QUEUED_ROOM_KEYS_VERSION,
                rooms: rooms.clone(),
            })
            .map_err(|error| wbf_sdk::SdkError::Protocol(format!("key-share queue: {error}")))
            .and_then(|plaintext| self.queue_file.write(&plaintext))
        };
        if let Err(error) = saved {
            self.events.progress(format!(
                "room keys: the key-share queue could not be saved ({error}); rooms still queued are lost if the daemon restarts"
            ));
        }
    }
}

/// 一件事併進手上的事：同一個房只留一件、成員取最新的；自己的金鑰要重傳就記一筆；開線那一聲與重試到時不加新的。
///
/// Return:
///     bool  true ＝ 房的清單變了（要寫回 `m/ks.sealed`）
fn take_work(
    rooms: &mut BTreeMap<String, Vec<String>>,
    upload_own_keys: &mut bool,
    work: Option<KeyShareWork>,
) -> bool {
    match work {
        Some(KeyShareWork::Room { room, members }) => {
            rooms.insert(room, members.clone()) != Some(members)
        }
        Some(KeyShareWork::UploadOwnKeys) => {
            *upload_own_keys = true;
            false
        }
        Some(KeyShareWork::LineOpened) | None => false,
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

    /// 後台🚫 自己開線：`Keys` 線沒開就等，開好那一聲（`init_keys` 叫的 `line_opened`）來了才送。
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
        core.key_share_inbox(&account).await.unwrap().line_opened();
        wait_for_async(
            || async {
                keys.fail_bridged.load(Ordering::SeqCst) == 0
                    && keys.bridge_calls.lock().unwrap().len() >= 2
            },
            "the first attempt failed and a retry followed",
        )
        .await;
        // 重試成功之後就停：等過最長的間隔，走橋的呼叫數與「試過幾次」都不再變。
        // 只看走橋的不夠：金鑰送出去之後再試一次什麼都不用送，一直重試也看不出來（變異驗證抓到）。
        tokio::time::sleep(RETRY_MAX).await;
        let settled = keys.bridge_calls.lock().unwrap().len();
        let attempts = core.room_key_share_attempts(&account);
        assert!(
            attempts >= 2,
            "one failed attempt and one retry: {attempts}"
        );
        tokio::time::sleep(RETRY_MAX * 2).await;
        assert_eq!(
            keys.bridge_calls.lock().unwrap().len(),
            settled,
            "nothing is sent again once the room's key went out"
        );
        assert_eq!(
            core.room_key_share_attempts(&account),
            attempts,
            "the room is not tried again once its key went out"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 還沒送完的房存在 `m/ks.sealed`（維護者 2026-10-06）：後台收掉（＝daemon 重開）之後，新的後台一開線就照著補，
    /// 沒有人再交一次；送完就刪檔。檔是封起來的：明文裡的房間 id 🚫 出現在檔裡。
    #[tokio::test]
    async fn rooms_still_queued_survive_a_restart_in_a_sealed_file_and_are_sent_when_the_keys_line_opens(
    ) {
        let dir = scratch("key-share-persist");
        let (core, account) = core_with_wbf_account(&dir).await;
        let _misc = encrypted_room_on_misc(&core, &account).await;
        let queue_path = account
            .matrix_store_dir()
            .join(super::KEY_SHARE_QUEUE_FILE_NAME);

        core.send_text(ROOM, "hi", &send_options("t-1"), &Target::default())
            .await
            .expect("sent while the Keys line is closed");
        wait_for_async(
            || async { queue_path.exists() },
            "the queued room is written to m/ks.sealed",
        )
        .await;
        let sealed = std::fs::read(&queue_path).unwrap();
        assert!(
            !sealed
                .windows(ROOM.len())
                .any(|window| window == ROOM.as_bytes()),
            "the room id is not on disk in plain text"
        );

        core.stop_room_key_share_of(&account).await;
        assert!(
            queue_path.exists(),
            "stopping the background keeps the file"
        );
        let keys = line_on_fake_server(&core, &account, LinkRole::Keys).await;
        core.key_share_inbox(&account).await.unwrap().line_opened();
        wait_for_async(
            || async { core.room_key_share_attempts(&account) >= 1 && !queue_path.exists() },
            "the new background sends the room read back from the file and removes it",
        )
        .await;
        assert!(!keys.bridge_calls.lock().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `m/ks.sealed` 壞了（不是我們封的、被截斷）：講一聲、從空的開始，後台照常活著、照常收新的事。
    #[tokio::test]
    async fn a_queue_file_that_cannot_be_opened_starts_the_background_empty_instead_of_stopping_it()
    {
        let dir = scratch("key-share-corrupt");
        let (core, account) = core_with_wbf_account(&dir).await;
        let _misc = encrypted_room_on_misc(&core, &account).await;
        let queue_path = account
            .matrix_store_dir()
            .join(super::KEY_SHARE_QUEUE_FILE_NAME);
        std::fs::create_dir_all(account.matrix_store_dir()).unwrap();
        std::fs::write(
            &queue_path,
            b"{\"v\":1,\"nonce\":\"AAAA\",\"sealed\":\"AAAA\"}",
        )
        .unwrap();
        let _keys = line_on_fake_server(&core, &account, LinkRole::Keys).await;

        core.key_share_inbox(&account).await.unwrap().line_opened();
        tokio::time::sleep(RETRY_FIRST * 2).await;
        assert!(core.is_sharing_room_keys(&account));
        assert_eq!(
            core.room_key_share_attempts(&account),
            0,
            "nothing read back"
        );

        core.send_text(ROOM, "hi", &send_options("t-1"), &Target::default())
            .await
            .expect("sent");
        wait_for_async(
            || async { core.room_key_share_attempts(&account) >= 1 && !queue_path.exists() },
            "new work is still taken and sent",
        )
        .await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 自己的金鑰上傳 server 沒回 Ack（維護者 2026-10-05：要保證）：交給金鑰線的 queue，隔一段時間再上傳一次，傳成就停。
    #[tokio::test]
    async fn an_own_key_upload_that_failed_is_retried_on_the_keys_line_until_the_server_acknowledges_it(
    ) {
        let dir = scratch("key-share-upload");
        let (core, account) = core_with_wbf_account(&dir).await;
        let keys = line_on_fake_server(&core, &account, LinkRole::Keys).await;
        keys.fail_bridged.store(1, Ordering::SeqCst);
        let uploads = || {
            keys.bridge_calls
                .lock()
                .unwrap()
                .iter()
                .filter(|(kind, subtype)| {
                    *kind == wbf_sdk::protocol::BRIDGE_KEYS_UPLOAD.kind
                        && *subtype == wbf_sdk::protocol::BRIDGE_KEYS_UPLOAD.subtype
                })
                .count()
        };

        assert!(core
            .key_share_inbox(&account)
            .await
            .unwrap()
            .retry_own_key_upload());
        wait_for_async(
            || async { uploads() >= 2 },
            "the failed upload is tried again",
        )
        .await;
        tokio::time::sleep(RETRY_MAX * 2).await;
        assert_eq!(
            uploads(),
            2,
            "uploaded once more after the failure, then stopped"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

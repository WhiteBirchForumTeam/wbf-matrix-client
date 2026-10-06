//! 金鑰線的後台（/docs/design/keys/e2ee-rpc.md §3、§3.1）：每個帳號一個 task，**房間金鑰只在這裡建、換、送**，server 回 Ack 才算數；
//! 這台裝置自己的金鑰（裝置金鑰、一次性金鑰）上傳沒拿到 Ack 也在這裡重試。
//!
//! - **送訊息只用已經分好的金鑰**（維護者 2026-10-06）：送出前問「房 R 的金鑰對房間版本號 V 就緒了沒」（[`Core::is_room_key_ready_within`]），
//!   沒有就交給這裡準備（`Prepare`），最多等 `ROOM_KEY_WAIT`；等不到，那一則就不送。
//! - 送出之後交 `AfterSend`：這把快到期就提早換、先分完，下一則拿到的就是就緒的。refresh、1506 之後交 `Prepare`：對新的 V 先分好。
//! - 走 `Keys` 線、用 `reuse`，🚫 自己開線：線沒開就等，`init_keys` 開好線時叫醒它（[`KeyShareInbox::line_opened`]）。
//! - 失敗的留著，隔一段時間再試（`RETRY_FIRST` 起加倍到 `RETRY_MAX`），講一聲（`Note`）。
//! - 🚫 存檔：待送的那一份在上游 crypto store 的 session 上，而沒拿到 Ack 的金鑰從來沒被拿來加密過，daemon 重開丟了也沒有訊息解不開；
//!   下一次 refresh 或送出會再交進來。自己的金鑰也一樣：上游記得還有沒上傳的，每次開線 `init_keys` 都會再傳。

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio::sync::watch;
use tokio::time::Instant;
use wbf_sdk::crypto_engine::{OlmEngine, RoomKeyState};

use crate::accounts::AccountDir;
use crate::error::CoreError;
use crate::event::EventSink;
use crate::link_pool::{LinkPool, LinkRole};
use crate::Core;

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
/// 送出等房間金鑰就緒最久多久（維護者 2026-10-06：2 秒），等不到那一則就回「金鑰還沒準備好」。
#[cfg(not(test))]
pub(crate) const ROOM_KEY_WAIT: Duration = Duration::from_secs(2);
/// 測試裡縮短，等不到的那條才不用真的等 2 秒。
#[cfg(test)]
pub(crate) const ROOM_KEY_WAIT: Duration = Duration::from_millis(500);

/// 一個房要準備成什麼樣子：對這個房間版本號（UI 帶來的那份成員與裝置）分好金鑰。
#[derive(Clone, Debug, PartialEq, Eq)]
struct RoomKeyWant {
    /// example: 81234
    room_version: u64,
    /// 那一份的 join 成員（含自己）, example: vec!["@alice:localhost".to_string()]
    members: Vec<String>,
    /// 先丟掉現在那把再分（提早換）；丟過一次就清掉，重試🚫 再丟（不然剛分到一半的新金鑰又被丟掉）。
    discard_first: bool,
}

/// 交給後台的一件事。
enum KeyShareWork {
    /// 這個房對這個房間版本號要有分好的金鑰（refresh、1506 之後、送出時發現還沒就緒）。
    Prepare { room: String, want: RoomKeyWant },
    /// 剛送出一則：看這把是不是快到期了，是就提早換。
    AfterSend { room: String, want: RoomKeyWant },
    /// 這台裝置自己的金鑰上傳失敗了：之後重試到 server 回 Ack。
    UploadOwnKeys,
    /// `Keys` 線剛開好：手上還沒做完的再跑一輪。
    LineOpened,
}

/// 哪個房的金鑰對哪個房間版本號分好了（task 寫、送出那一路讀）。每次有房就緒，`changed` 的數字就 +1，等待的人醒來重看。
struct RoomKeyReadiness {
    /// 房間 id → 分好時依據的房間版本號
    ready: Mutex<HashMap<String, u64>>,
    changed: watch::Sender<u64>,
}

impl RoomKeyReadiness {
    fn ready(&self) -> MutexGuard<'_, HashMap<String, u64>> {
        self.ready
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Return:
    ///     bool  true ＝ 這個房的金鑰是對這個房間版本號分好的
    fn is_ready_for(&self, room: &str, room_version: u64) -> bool {
        self.ready().get(room) == Some(&room_version)
    }

    fn set_ready(&self, room: &str, room_version: u64) {
        self.ready().insert(room.to_string(), room_version);
        self.changed.send_modify(|generation| *generation += 1);
    }
}

/// 往這個帳號的後台交事情的那頭（`init_keys` 與收金鑰的 task 各握一份）。
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

    /// `Keys` 線開好了：手上還沒做完的再跑一輪。
    pub(crate) fn line_opened(&self) {
        let _ = self.0.send(KeyShareWork::LineOpened);
    }
}

/// 一個帳號的後台。丟掉就 abort（登出、`Core` 丟掉）。
pub(crate) struct KeyShareHandle {
    inbox: KeyShareInbox,
    readiness: Arc<RoomKeyReadiness>,
    task: tokio::task::JoinHandle<()>,
    /// 試過幾次（分一個房、或重傳一次自己的金鑰；只給測試：做成之後再試不會走橋，從假 server 看不出來有沒有停）。
    #[cfg(test)]
    attempts: Arc<std::sync::atomic::AtomicU32>,
}

impl Drop for KeyShareHandle {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// task 自己握著的東西——🚫 不握 `Core`、不握線（線在池裡，要用時 `reuse` 那一格）。
struct KeyShareTask {
    engine: Arc<OlmEngine>,
    pool: Arc<LinkPool>,
    events: EventSink,
    readiness: Arc<RoomKeyReadiness>,
    #[cfg(test)]
    attempts: Arc<std::sync::atomic::AtomicU32>,
}

impl Core {
    /// 讓後台對這個房間版本號分好這個房的金鑰（refresh、1506 之後叫）；🚫 等它。
    /// 起不了後台（引擎開不起來、正在登出）只講一聲：送出時會再交一次，那時等不到就回「金鑰還沒準備好」。
    ///
    /// Args:
    ///     room: example: "!r:localhost"
    ///     room_version: example: 81234
    ///     members: 那一份的 join 成員（含自己）, example: vec!["@alice:localhost".to_string(), "@bob:localhost".to_string()]
    pub(crate) async fn prepare_room_key(
        &self,
        account: &AccountDir,
        room: &str,
        room_version: u64,
        members: Vec<String>,
    ) {
        let want = RoomKeyWant {
            room_version,
            members,
            discard_first: false,
        };
        self.hand_key_share_work(
            account,
            room,
            KeyShareWork::Prepare {
                room: room.to_string(),
                want,
            },
        )
        .await;
    }

    /// 剛送出一則加密訊息：交給後台看這把是不是快到期了，是就提早換、先分完；🚫 等它。
    pub(crate) async fn room_key_used(
        &self,
        account: &AccountDir,
        room: &str,
        room_version: u64,
        members: Vec<String>,
    ) {
        let want = RoomKeyWant {
            room_version,
            members,
            discard_first: false,
        };
        self.hand_key_share_work(
            account,
            room,
            KeyShareWork::AfterSend {
                room: room.to_string(),
                want,
            },
        )
        .await;
    }

    /// 送出前：這個房的金鑰對這個房間版本號分好了沒；還沒就交給後台準備，最多等 `wait`。
    ///
    /// Args:
    ///     room: example: "!r:localhost"
    ///     room_version: UI 帶來的那份, example: 81234
    ///     members: 同一份的 join 成員（含自己）, example: vec!["@alice:localhost".to_string()]
    ///     wait: example: ROOM_KEY_WAIT
    /// Return:
    ///     Ok(bool)    true ＝ 就緒（這一刻的金鑰排過的 to-device 都拿到 Ack、是對這個版本分的）；false ＝ 等到時間還沒好
    ///     Err(...)    引擎開不起來、正在登出、沒解鎖、crypto store 讀不了
    pub(crate) async fn is_room_key_ready_within(
        &self,
        account: &AccountDir,
        room: &str,
        room_version: u64,
        members: &[String],
        wait: Duration,
    ) -> Result<bool, CoreError> {
        let engine = self.olm_engine_of(account).await?;
        let (inbox, readiness) = self.key_share_parts(account).await?;
        // 先訂閱再看：看完到開始等之間就緒的，也會讓下面的 `changed()` 醒來。
        let mut changed = readiness.changed.subscribe();
        let deadline = Instant::now() + wait;
        let mut handed = false;
        loop {
            if readiness.is_ready_for(room, room_version)
                && matches!(
                    engine.room_key_state(room).await?,
                    RoomKeyState::Ready { .. }
                )
            {
                return Ok(true);
            }
            if !handed {
                handed = true;
                let want = RoomKeyWant {
                    room_version,
                    members: members.to_vec(),
                    discard_first: false,
                };
                let _ = inbox.0.send(KeyShareWork::Prepare {
                    room: room.to_string(),
                    want,
                });
            }
            match tokio::time::timeout_at(deadline, changed.changed()).await {
                Ok(Ok(())) => continue,
                // 時間到、或後台收掉了（它的 sender 丟了）：都是沒等到。
                Ok(Err(_)) | Err(_) => return Ok(false),
            }
        }
    }

    /// 這個帳號的後台的 inbox（`init_keys` 用）；沒在跑就起一個。
    ///
    /// Return:
    ///     Ok(KeyShareInbox)
    ///     Err(...)    引擎開不起來、正在登出、沒解鎖
    pub(crate) async fn key_share_inbox(
        &self,
        account: &AccountDir,
    ) -> Result<KeyShareInbox, CoreError> {
        Ok(self.key_share_parts(account).await?.0)
    }

    /// 收掉這個帳號的後台（登出用），**等它真的結束**才回：task 握著引擎（`m/` 的 sqlite），
    /// 只 abort 不等的話它要等下一次被 poll 才放手，而登出接著就要刪 `m/`（Windows 上開著刪不掉；單執行緒的 runtime 上刪檔的重試還會擋住它被 poll）。
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

    /// 交一件房間金鑰的事給後台；起不了就講一聲。
    async fn hand_key_share_work(&self, account: &AccountDir, room: &str, work: KeyShareWork) {
        let handed = match self.key_share_parts(account).await {
            Ok((inbox, _)) => inbox.0.send(work).is_ok(),
            Err(_) => false,
        };
        if !handed {
            self.events.progress(format!(
                "room keys: the key of {room} cannot be prepared now; the background sender for {} is not running",
                account.label()
            ));
        }
    }

    /// 這個帳號的後台：在跑就給它的 inbox 與就緒表；沒有就起一個。
    ///
    /// Return:
    ///     Ok((KeyShareInbox, Arc<RoomKeyReadiness>))
    ///     Err(...)    引擎開不起來、正在登出、沒解鎖
    async fn key_share_parts(
        &self,
        account: &AccountDir,
    ) -> Result<(KeyShareInbox, Arc<RoomKeyReadiness>), CoreError> {
        if let Some(parts) = self.running_key_share_parts(account) {
            return Ok(parts);
        }
        // 開引擎是 async，鎖外做；起的時候再看一次（同時進來的另一個可能剛起好）。
        let engine = self.olm_engine_of(account).await?;
        let pool = self.pool_of_account(account)?;
        let mut shares = self
            .key_shares
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(handle) = shares.get(&account.dir) {
            if !handle.task.is_finished() {
                return Ok((handle.inbox.clone(), handle.readiness.clone()));
            }
        }
        let (sender, receiver) = unbounded_channel();
        let inbox = KeyShareInbox(sender);
        let readiness = Arc::new(RoomKeyReadiness {
            ready: Mutex::new(HashMap::new()),
            changed: watch::channel(0).0,
        });
        #[cfg(test)]
        let attempts = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let task = KeyShareTask {
            engine,
            pool,
            events: self.events.clone(),
            readiness: readiness.clone(),
            #[cfg(test)]
            attempts: attempts.clone(),
        };
        shares.insert(
            account.dir.clone(),
            KeyShareHandle {
                inbox: inbox.clone(),
                readiness: readiness.clone(),
                task: tokio::spawn(task.run(receiver)),
                #[cfg(test)]
                attempts,
            },
        );
        Ok((inbox, readiness))
    }

    /// Return:
    ///     Some((KeyShareInbox, Arc<RoomKeyReadiness>))   這個帳號的後台正在跑
    ///     None                                           還沒起、或已經結束
    fn running_key_share_parts(
        &self,
        account: &AccountDir,
    ) -> Option<(KeyShareInbox, Arc<RoomKeyReadiness>)> {
        self.key_shares
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&account.dir)
            .filter(|handle| !handle.task.is_finished())
            .map(|handle| (handle.inbox.clone(), handle.readiness.clone()))
    }

    /// Return:
    ///     u32  這個帳號的後台試過幾次（分一個房、或重傳一次自己的金鑰；只給測試斷言。還沒有後台是 0）
    #[cfg(test)]
    pub(crate) fn key_share_attempts(&self, account: &AccountDir) -> u32 {
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
        // 要分的房（同一個房只留一件，要的取最新）；剛送出、要看該不該提早換的房。
        let mut rooms: BTreeMap<String, RoomKeyWant> = BTreeMap::new();
        let mut used: BTreeMap<String, RoomKeyWant> = BTreeMap::new();
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
            take_work(&mut rooms, &mut used, &mut upload_own_keys, next);
            while let Ok(more) = work.try_recv() {
                take_work(&mut rooms, &mut used, &mut upload_own_keys, Some(more));
            }
            // 剛送出的：只讀本機，快到期才交去換（🚫 每則都上網）。
            for (room, want) in std::mem::take(&mut used) {
                self.check_after_send(&mut rooms, room, want).await;
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
                #[cfg(test)]
                self.attempts
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
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
            let pending: Vec<String> = rooms.keys().cloned().collect();
            for room in pending {
                let Some(want) = rooms.get_mut(&room) else {
                    continue;
                };
                #[cfg(test)]
                self.attempts
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                match self.prepare(&mut line, &room, want).await {
                    Ok(()) => {
                        rooms.remove(&room);
                    }
                    Err(error) => {
                        failed = true;
                        self.events.progress(format!(
                            "room keys: preparing the key of {room} failed (retried in {}s): {error}",
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

    /// 剛送出一則之後：這把就緒而且快到期 → 交去提早換；已經不就緒（到期、作廢）→ 交去分一把新的；就緒也沒到期 → 什麼都不做。
    async fn check_after_send(
        &self,
        rooms: &mut BTreeMap<String, RoomKeyWant>,
        room: String,
        mut want: RoomKeyWant,
    ) {
        match self.engine.room_key_state(&room).await {
            Ok(RoomKeyState::Ready {
                due_for_rotation: false,
                ..
            }) => {}
            Ok(RoomKeyState::Ready {
                due_for_rotation: true,
                ..
            }) => {
                want.discard_first = true;
                merge_want(rooms, room, want);
            }
            Ok(RoomKeyState::NotReady { .. }) => merge_want(rooms, room, want),
            Err(error) => self.events.progress(format!(
                "room keys: cannot read the key state of {room} after sending: {error}"
            )),
        }
    }

    /// 分好一個房：要提早換就先丟掉現在那把（只丟一次）→ 建／換、送到每台裝置 → 確認就緒才記下這個房間版本號、叫醒等的人。
    ///
    /// Return:
    ///     Ok(())       就緒了
    ///     Err(...)     任何一步失敗、或送完了還不就緒（留著重試）
    async fn prepare(
        &self,
        line: &mut wbf_sdk::WbfClient<wbf_sdk::Channel>,
        room: &str,
        want: &mut RoomKeyWant,
    ) -> Result<(), wbf_sdk::SdkError> {
        if want.discard_first {
            self.engine.discard_room_key(room).await?;
            want.discard_first = false;
        }
        self.engine
            .distribute_room_key(line, room, &want.members)
            .await?;
        match self.engine.room_key_state(room).await? {
            RoomKeyState::Ready { .. } => {
                self.readiness.set_ready(room, want.room_version);
                Ok(())
            }
            RoomKeyState::NotReady { reason } => Err(wbf_sdk::SdkError::Protocol(format!(
                "the key is still not ready after distributing it: {reason}"
            ))),
        }
    }
}

/// 同一個房只留一件：要的取最新的（房間版本號、成員），「先丟掉」只要有一件要就保留。
fn merge_want(rooms: &mut BTreeMap<String, RoomKeyWant>, room: String, want: RoomKeyWant) {
    let discard_first = want.discard_first
        || rooms
            .get(&room)
            .is_some_and(|previous| previous.discard_first);
    rooms.insert(
        room,
        RoomKeyWant {
            discard_first,
            ..want
        },
    );
}

/// 一件事併進手上的事。開線那一聲與重試到時不加新的。
fn take_work(
    rooms: &mut BTreeMap<String, RoomKeyWant>,
    used: &mut BTreeMap<String, RoomKeyWant>,
    upload_own_keys: &mut bool,
    work: Option<KeyShareWork>,
) {
    match work {
        Some(KeyShareWork::Prepare { room, want }) => merge_want(rooms, room, want),
        Some(KeyShareWork::AfterSend { room, want }) => {
            used.insert(room, want);
        }
        Some(KeyShareWork::UploadOwnKeys) => *upload_own_keys = true,
        Some(KeyShareWork::LineOpened) | None => {}
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;
    use std::sync::{Arc, Mutex};

    use wbf_sdk::crypto_engine::RoomKeyState;

    use super::RETRY_MAX;
    use crate::accounts::AccountDir;
    use crate::error::CoreErrorKind;
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

    /// 這個房現在那把房間金鑰的 session id 與狀態（測試用）。
    async fn room_key_state(core: &Core, account: &AccountDir) -> RoomKeyState {
        core.olm_engine_of(account)
            .await
            .unwrap()
            .room_key_state(ROOM)
            .await
            .unwrap()
    }

    /// 送訊息只用已經分好的金鑰（維護者 2026-10-06，/docs/design/keys/e2ee-rpc.md §3）：送的那一路（`Misc`）一個走橋的請求都🚫 收到，
    /// 金鑰是後台在 `Keys` 線上建、送完才拿來加密；送出去的是密文、帶那個號碼。
    #[tokio::test]
    async fn an_encrypted_send_only_uses_a_key_the_background_prepared_on_the_keys_line() {
        let dir = scratch("key-share-send");
        let (core, account) = core_with_wbf_account(&dir).await;
        let misc = encrypted_room_on_misc(&core, &account).await;
        let keys = line_on_fake_server(&core, &account, LinkRole::Keys).await;

        core.send_text(ROOM, "hi", &send_options("t-1"), &Target::default())
            .await
            .expect("sent once the background prepared the key");
        assert_eq!(
            *misc.bridge_calls.lock().unwrap(),
            Vec::new(),
            "the send path asks the server for no keys"
        );
        assert!(
            !keys.bridge_calls.lock().unwrap().is_empty(),
            "the key was prepared on the Keys line before the send"
        );
        let sent = misc.sent_events.lock().unwrap().clone();
        assert_eq!(sent.len(), 1, "{sent:?}");
        assert_eq!(
            (sent[0].1.as_str(), sent[0].2),
            ("m.room.encrypted", Some(7))
        );
        assert!(matches!(
            room_key_state(&core, &account).await,
            RoomKeyState::Ready {
                message_count: 1,
                ..
            }
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `Keys` 線沒開：後台🚫 自己開線，送出最多等 `ROOM_KEY_WAIT`，等不到就 `RoomKeyNotReady`、訊息沒加密沒送、`data` 帶 `txn_id`。
    /// 線開好那一聲（`init_keys` 叫的 `line_opened`）之後後台分好，同一個 `txn_id` 重送就過。
    #[tokio::test]
    async fn a_send_without_a_prepared_key_waits_then_fails_without_sending_and_goes_through_once_the_key_is_ready(
    ) {
        let dir = scratch("key-share-not-ready");
        let (core, account) = core_with_wbf_account(&dir).await;
        let misc = encrypted_room_on_misc(&core, &account).await;

        let refused = core
            .send_text(ROOM, "hi", &send_options("t-1"), &Target::default())
            .await
            .unwrap_err();
        assert_eq!(refused.kind, CoreErrorKind::RoomKeyNotReady, "{refused:?}");
        assert_eq!(refused.data, Some(serde_json::json!({ "txn_id": "t-1" })));
        assert!(
            misc.sent_events.lock().unwrap().is_empty(),
            "🚫 用沒分好的金鑰加密、🚫 送"
        );
        let pool = core.pool_of_account(&account).unwrap();
        assert!(
            pool.reuse(LinkRole::Keys).await.is_none(),
            "the background does not open the Keys line"
        );

        let _keys = line_on_fake_server(&core, &account, LinkRole::Keys).await;
        core.key_share_inbox(&account).await.unwrap().line_opened();
        wait_for_async(
            || async {
                matches!(
                    room_key_state(&core, &account).await,
                    RoomKeyState::Ready { .. }
                )
            },
            "the background prepares the key once the line is open",
        )
        .await;
        core.send_text(ROOM, "hi", &send_options("t-1"), &Target::default())
            .await
            .expect("the same txn_id goes through once the key is ready");
        assert_eq!(misc.sent_events.lock().unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 準備失敗的房隔一段時間（`RETRY_FIRST`）再試，在送出的等待時間內分好就照送；分好之後就停，🚫 一直重試。
    #[tokio::test]
    async fn a_failed_prepare_is_retried_within_the_wait_and_stops_once_the_key_is_ready() {
        let dir = scratch("key-share-retry");
        let (core, account) = core_with_wbf_account(&dir).await;
        let misc = encrypted_room_on_misc(&core, &account).await;
        let keys = line_on_fake_server(&core, &account, LinkRole::Keys).await;
        keys.fail_bridged.store(1, Ordering::SeqCst);

        core.send_text(ROOM, "hi", &send_options("t-1"), &Target::default())
            .await
            .expect("the retry finished within the wait");
        assert_eq!(
            keys.fail_bridged.load(Ordering::SeqCst),
            0,
            "the first call failed"
        );
        assert_eq!(misc.sent_events.lock().unwrap().len(), 1);
        // 分好之後就停：等過最長的間隔，走橋的呼叫數與「試過幾次」都不再變。
        // 只看走橋的不夠：分好之後再試一次什麼都不用送，一直重試也看不出來（變異驗證抓到）。
        tokio::time::sleep(RETRY_MAX).await;
        let settled = keys.bridge_calls.lock().unwrap().len();
        let attempts = core.key_share_attempts(&account);
        assert!(
            attempts >= 2,
            "one failed attempt and one retry: {attempts}"
        );
        tokio::time::sleep(RETRY_MAX * 2).await;
        assert_eq!(keys.bridge_calls.lock().unwrap().len(), settled);
        assert_eq!(
            core.key_share_attempts(&account),
            attempts,
            "the room is not prepared again once its key is ready"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 快到期的金鑰（剩不到 `PRE_ROTATE_MESSAGES` 則）在送出之後由後台提早換一把、先分完：下一則用的是新的、就緒的那把，
    /// 🚫 等它到期才在送的路上卡住。
    #[tokio::test]
    async fn a_key_close_to_expiry_is_replaced_after_a_send_before_the_next_one() {
        let dir = scratch("key-share-rotate");
        let (core, account) = core_with_wbf_account(&dir).await;
        let misc = encrypted_room_on_misc(&core, &account).await;
        let _keys = line_on_fake_server(&core, &account, LinkRole::Keys).await;

        core.send_text(ROOM, "0", &send_options("t-0"), &Target::default())
            .await
            .unwrap();
        let RoomKeyState::Ready {
            session_id: first, ..
        } = room_key_state(&core, &account).await
        else {
            panic!("ready after the first send");
        };
        // 上游預設一把用 100 則；剩 5 則（第 95 則送完）就該換。
        for position in 1..95 {
            core.send_text(
                ROOM,
                &position.to_string(),
                &send_options(&format!("t-{position}")),
                &Target::default(),
            )
            .await
            .unwrap();
        }
        wait_for_async(
            || async {
                matches!(
                    room_key_state(&core, &account).await,
                    RoomKeyState::Ready { ref session_id, .. } if *session_id != first
                )
            },
            "the background replaced the key after the 95th message",
        )
        .await;
        core.send_text(ROOM, "95", &send_options("t-95"), &Target::default())
            .await
            .unwrap();
        let sent = misc.sent_events.lock().unwrap().clone();
        assert_eq!(sent.len(), 96);
        let session_of = |index: usize| {
            serde_json::from_slice::<serde_json::Value>(&sent[index].4).unwrap()["session_id"]
                .as_str()
                .unwrap()
                .to_string()
        };
        assert_eq!(
            session_of(94),
            first,
            "the 95th message still used the first key"
        );
        assert_ne!(session_of(95), first, "the 96th used the replacement");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 自己的金鑰上傳 server 沒回 Ack（維護者 2026-10-05：要保證）：交給金鑰線的後台，隔一段時間再上傳一次，傳成就停。
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
        // 傳成之後就停。只數上傳不夠：傳成之後再試一次什麼都不用傳、不走橋，一直重試也看不出來（變異驗證抓到），所以也數「試過幾次」。
        tokio::time::sleep(RETRY_MAX).await;
        let attempts = core.key_share_attempts(&account);
        tokio::time::sleep(RETRY_MAX * 2).await;
        assert_eq!(
            uploads(),
            2,
            "uploaded once more after the failure, then stopped"
        );
        assert_eq!(attempts, 2, "one failed upload and one retry");
        assert_eq!(
            core.key_share_attempts(&account),
            attempts,
            "the upload is not tried again once the server acknowledged it"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

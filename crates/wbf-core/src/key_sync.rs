//! 訂閱線的金鑰那半：`Device/Subscribe`、上線追平、推來的匯進 store、任何異常就從佇列頭拉一次（key-sync.md）。
//!
//! 維護者 2026-09-24 定的形狀：
//!
//! - **推來的（`Push`）與主動拉的（`Fetch` 的 `Batch`）封包大同小異，最根本的處理是同一支**：sdk 的 `OlmEngine::import_items`
//!   ——匯進 crypto store、水位與待銷毀清單落地、對 server 銷毀那一包。這裡不解封包、不碰 store，只把 items 交給它。
//! - `Device/Fetch`／`ItemsDestroy` 走訂閱線（server：只有持有這台裝置佇列的連線能銷毀），所以 task 要用線時跟池拿同一格。
//! - 訂閱結束（被另一台裝置接手的 1505、線死了）就停、發 `keys.state: stopped`；🚫 不重訂（to-device-client.md §5.1：兩台會互踢），
//!   🚫 不主動重開線（等 server 支援更多連線再做）。線死了房間那半（`room_sync.rs`）會關那格。
//! - `keys.state` 留著（「有點多餘，但傾向保留——不然 RPC 無從知道」）。
//!
//! 🚨 **佇列頭就是水位**（維護者 2026-09-26，wbfuwunel #87）：server 的佇列沒有洞（每一則存到我們 `ItemsDestroy` 才刪），
//! `Fetch` 🚫 不帶 `cd_seq`、讓 server 從最舊還沒銷毀的起給。所以沒有「client 的游標越過一則沒匯的」這種事：
//! 匯失敗、gap、壞包、收件匣滿、Ack 前推來的，全部沒銷掉的都還在佇列裡，從頭拉一次就回來（重複匯入無害）。
//! 之前那套「不先匯、落後中、水位不越過」是在補自己挖的坑（PR #60 審查 rumia／cirno／salvia 三條 🔴），拿掉了。

use std::sync::Arc;
use std::time::Duration;

use wbf_sdk::channel::Channel;
use wbf_sdk::client::{DeviceSubscription, WbfClient};
use wbf_sdk::crypto_engine::{ImportReport, OlmEngine};
use wbf_sdk::login::SessionBackend;
use wbf_sdk::protocol::SubscribeReply;

use crate::accounts::AccountDir;
use crate::error::{CoreError, CoreErrorKind};
use crate::event::{EventSink, KeysState};
use crate::link_pool::{LinkPool, LinkRole};
use crate::{Core, CoreEvent};

/// 等 `Subscribe` 的 Ack／`Fetch` 的每個 Batch／`ItemsDestroy` 的回覆最久多久。
const REPLY_TIMEOUT: Duration = Duration::from_secs(30);
/// task 每次等推播最久多久：到了只是「這段時間沒事」，繼續等（心跳另外在線上跑）。
const PUSH_IDLE_TIMEOUT: Duration = Duration::from_secs(60);
/// 收 task 時等它收攤最久多久；不肯就 abort。
const STOP_TIMEOUT: Duration = Duration::from_secs(5);

/// 一個帳號的收金鑰 task。丟掉就 abort（`Core` 丟掉、或線重開換新的一個）。
pub(crate) struct KeySyncHandle {
    task: tokio::task::JoinHandle<()>,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
}

impl Drop for KeySyncHandle {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// task 自己握著的東西——🚫 不握 `Core`、不握線：線在池裡，要用時 `reuse` 那一格（`Fetch`／`ItemsDestroy` 要走訂閱線）。
struct KeySyncTask {
    me: String,
    engine: Arc<OlmEngine>,
    events: EventSink,
    pool: Arc<LinkPool>,
}

impl Core {
    /// 這個帳號長活的 crypto 引擎（`m/` 的 OlmMachine）：第一次要用才開，之後共用。
    ///
    /// Return:
    ///     Ok(Arc<OlmEngine>)
    ///     Err(Usage)          不是 wbf 帳號（matrix-sdk 帳號的金鑰在它自己的 Client 裡，🚫 不開第二台狀態機）
    ///     Err(NotLoggedIn)    沒 session
    ///     Err(Io)             `m/` 開不起來
    pub(crate) async fn olm_engine_of(
        &self,
        account: &AccountDir,
    ) -> Result<Arc<OlmEngine>, CoreError> {
        // 鎖持到開完、放進去：兩個同時進來的後到者等第一個開完就共用，🚫 不各開一次同一個 store。
        let mut engines = self.crypto_engines.lock().await;
        if let Some(engine) = engines.get(&account.dir) {
            return Ok(engine.clone());
        }
        let session = self.session_of(account)?;
        if session.backend != Some(SessionBackend::WbfSdk) {
            return Err(CoreError::new(
                CoreErrorKind::Usage,
                format!(
                    "{} is not a wbf account: its keys live in the matrix-sdk client, not here",
                    account.label()
                ),
            ));
        }
        let store_key = self.vault()?.matrix_store_key();
        let engine = Arc::new(
            OlmEngine::open(
                &account.matrix_store_dir(),
                &store_key,
                &session.user_id,
                &session.device_id,
            )
            .await?,
        );
        engines.insert(account.dir.clone(), engine.clone());
        Ok(engine)
    }

    /// 登出用：把長活的引擎丟掉（store 要刪，Windows 上開著刪不掉）。
    pub(crate) async fn forget_crypto_engine(&self, account: &AccountDir) {
        self.crypto_engines.lock().await.remove(&account.dir);
    }

    /// 訂閱線開好之後金鑰那半（`room_sync::init_connection` 叫）：`Device/Subscribe` → 上線追平（先訂再拉，中間到的沒人漏）→ 起收金鑰的 task。
    ///
    /// ⚠️ 這裡回錯就是整條訂閱線沒開成（房間那半也一起）：fail loud，因為「金鑰沒在收」靠 UI 看不出來，而線開不起來看得出來（key-sync.md §1）。
    /// 唯一的例外是「這不是 wbf 帳號」：金鑰不在這裡，講一聲、跳過，房間那半照常。
    ///
    /// Args:
    ///     client: 已經 hello 過、還沒放進池的訂閱線
    /// Return:
    ///     Ok(())
    ///     Err(Server)     `Forbidden`：`device_id` 不是這個 session 的
    ///     Err(Network)    線死了（池就當這條沒開成）
    ///     Err(Protocol)   追平拉了太多窗、或封包壞了
    pub(crate) async fn init_keys(
        &self,
        account: &AccountDir,
        client: &mut WbfClient<Channel>,
    ) -> Result<(), CoreError> {
        let session = self.session_of(account)?;
        // 不是 wbf 帳號：金鑰在 matrix-sdk 的 Client 裡，這裡不訂、講一聲，房間那半照常。
        // 判準跟 `olm_engine_of` 同一條（session 的 backend），🚫 不靠錯誤種類分桶（PR #60 審查 cirno 🟡：`Usage` 桶裡還有真的開不起來）。
        if session.backend != Some(SessionBackend::WbfSdk) {
            self.events.progress(format!(
                "keys: not subscribing for {}: its keys live in the matrix-sdk client, not here",
                account.label()
            ));
            return Ok(());
        }
        let engine = self.olm_engine_of(account).await?;
        let pool = self.pool_of_account(account)?;
        let me = session.user_id.clone();
        let mut subscription = client
            .device_subscription(&session.device_id, REPLY_TIMEOUT)
            .await?;
        // Ack 之前推來的（early pushes）不用單獨匯：它們還沒銷，還在佇列裡，下面的追平會連同更舊的一起拉回來。
        subscription.early_pushes.clear();
        // 上線追平（to-device-client.md §7）：從佇列最舊還沒銷毀的起一窗一窗拉到 more=false；空窗也走一次（補送上次沒銷成的）。
        // 這裡失敗＝整條訂閱線沒開成；沒進 store 的還在佇列裡，下次開線的追平會拉回（key-sync.md §1、PR #60 審查 rumia 🟡）。
        let reports = engine.pull_to_device(client, REPLY_TIMEOUT).await?;
        emit_caught_up(&self.events, &me, &reports);
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let task = KeySyncTask {
            me,
            engine,
            events: self.events.clone(),
            pool,
        };
        let handle = KeySyncHandle {
            task: tokio::spawn(task.run(subscription, stopped)),
            stop: Some(stop),
        };
        // 舊的（線死了留下來的）在這裡被 Drop、abort。
        let _previous = self
            .key_syncs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(account.dir.clone(), handle);
        Ok(())
    }

    /// 收掉這個帳號的收金鑰 task（關訂閱線、登出用）。
    ///
    /// Return:
    ///     bool  true ＝ 本來在跑
    pub(crate) async fn stop_key_sync_of(&self, account: &AccountDir) -> bool {
        let handle = self
            .key_syncs
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

    /// 說出口的退出（wbf-to-device.md §4）：下線前對訂閱線送 `Device/Unsubscribe`。線沒開就沒事；失敗只講一聲（token 之後也撤了）。
    pub(crate) async fn unsubscribe_keys_of(&self, account: &AccountDir) {
        let pool = self
            .link_pools
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&account.dir)
            .cloned();
        let Some(pool) = pool else {
            return;
        };
        let Some(mut line) = pool.reuse(LinkRole::Subscriptions).await else {
            return;
        };
        if let Err(error) = line.device_unsubscribe().await {
            self.events.progress(format!(
                "keys: Device/Unsubscribe failed (ignored; the token is revoked next): {error}"
            ));
        }
    }

    /// Return:
    ///     bool  true ＝ 這個帳號的收金鑰 task 還在（只給測試斷言用；生產路徑看 `keys.state`）
    #[cfg(test)]
    pub(crate) fn is_key_syncing(&self, account: &AccountDir) -> bool {
        self.key_syncs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&account.dir)
            .is_some_and(|handle| !handle.task.is_finished())
    }
}

impl KeySyncTask {
    async fn run(
        self,
        mut subscription: DeviceSubscription,
        mut stopped: tokio::sync::oneshot::Receiver<()>,
    ) {
        // 要拉一次：有東西可能還留在佇列裡沒進 store（gap、壞包、收件匣滿、匯失敗）。拉失敗就留著這個旗，
        // 下一個事件或下一次閒置逸時（60 秒）再拉——🚫 不在原地狂試。佇列頭就是水位，所以中間照樣匯推來的包也不會越過什麼。
        let mut needs_pull = false;
        loop {
            // 本地收件匣滿過＝丟過推播：東西還在 server 佇列裡（沒銷毀前不會掉）。
            if subscription.take_gap() {
                needs_pull = true;
            }
            let next = tokio::select! {
                _ = &mut stopped => return,
                next = subscription.next(PUSH_IDLE_TIMEOUT) => next,
            };
            // 訂閱還活著的那幾種回 `None`；「訂閱結束了」回原因。
            let ended = match next {
                Ok(Some(SubscribeReply::Push { meta, items })) => {
                    // gap：這包之前有沒推到的，從佇列頭拉一次（這包也在裡面）。匯不進去（或銷不掉）也是。
                    if meta.gap || !self.import(items).await {
                        needs_pull = true;
                    }
                    None
                }
                // OTK 存量：用途是補上傳金鑰，那是 E2EE 的 RPC 面那支；這裡只講一聲。
                // 但它跟 `Push` 共用這條訂閱的 gap 旗（server 給一次就清掉）：gap 真就是有推送沒到，一樣拉。
                Ok(Some(SubscribeReply::CryptoState(state))) => {
                    if state.gap {
                        needs_pull = true;
                    }
                    self.events.progress(format!(
                        "keys: one-time key stock is now {:?} (uploading keys is not wired yet)",
                        state.otk_counts
                    ));
                    None
                }
                Ok(Some(SubscribeReply::Acknowledged)) | Err(wbf_sdk::SdkError::Timeout(_)) => None,
                // 一包壞了：它還在 server 佇列裡，拉一次就回來。
                Err(wbf_sdk::SdkError::Protocol(why)) => {
                    self.events.progress(format!(
                        "keys: a push could not be read ({why}); pulling the queue instead"
                    ));
                    needs_pull = true;
                    None
                }
                // server 對這段會話送了 Error（被另一台裝置接手的 1505 就是這樣來的）：會話到此為止。
                Err(error @ wbf_sdk::SdkError::Server { .. }) => {
                    Some(format!("the server ended the key subscription: {error}"))
                }
                Ok(None) => Some("the server ended the key subscription".to_string()),
                Err(error) => Some(error.to_string()),
            };
            let Some(why) = ended else {
                if needs_pull {
                    needs_pull = !self.pull().await;
                }
                continue;
            };
            // 🚫 不重訂（to-device-client.md §5.1：對面也會被踢，兩台互踢到天荒地老）；🚫 不關線（房間訂閱還在同一條線上；線真死了房間那半會關）。
            self.events.emit(CoreEvent::Keys {
                user: self.me.clone(),
                state: KeysState::Stopped,
                imported: None,
                room_keys: None,
                reason: Some(why),
            });
            return;
        }
    }

    /// 推來的一包：跟 `Fetch` 的一窗走同一支（匯入 → 落地 → 銷毀那一包）。
    ///
    /// Return:
    ///     bool  true ＝ 這包完整走完；false ＝ 線不在、或匯入／銷毀回錯（呼叫端要拉一次：沒銷掉的還在 server 佇列裡，
    ///           但**不會再推一次**，只有從佇列頭拉才拿得回來）
    async fn import(&self, items: Vec<(u64, serde_json::Value)>) -> bool {
        let Some(mut line) = self.pool.reuse(LinkRole::Subscriptions).await else {
            self.events.progress(format!(
                "keys: the subscriptions line is gone; {} pushed item(s) stay queued on the server until the line is reopened",
                items.len()
            ));
            return false;
        };
        match self
            .engine
            .import_items(&mut line, items, REPLY_TIMEOUT)
            .await
        {
            Ok(report) => {
                emit_caught_up(&self.events, &self.me, &[report]);
                true
            }
            // 匯入或銷毀失敗：已落地的照樣有效（import_items 的順序鎖死）；沒銷掉的還在佇列裡，呼叫端從佇列頭拉回。
            Err(error) => {
                self.events.progress(format!(
                    "keys: importing a pushed batch failed; pulling the queue instead: {error}"
                ));
                false
            }
        }
    }

    /// 從佇列頭拉到追平（gap、本地丟包、壞包、匯失敗都走這裡）。
    ///
    /// Return:
    ///     bool  true ＝ 追平了；false ＝ 線不在或拉失敗（呼叫端留著「要拉」，下一個事件或閒置逾時再拉）
    async fn pull(&self) -> bool {
        let Some(mut line) = self.pool.reuse(LinkRole::Subscriptions).await else {
            self.events.progress(
                "keys: the subscriptions line is gone; the queue is pulled when the line is reopened",
            );
            return false;
        };
        match self.engine.pull_to_device(&mut line, REPLY_TIMEOUT).await {
            Ok(reports) => {
                emit_caught_up(&self.events, &self.me, &reports);
                true
            }
            Err(error) => {
                self.events.progress(format!(
                    "keys: pulling the queue failed (retried on the next event or within a minute): {error}"
                ));
                false
            }
        }
    }
}

/// 一輪（一包、或追平的幾窗）匯完：`keys.state: caught_up`，帶匯了幾則、幾把新房間金鑰。
fn emit_caught_up(events: &EventSink, me: &str, reports: &[ImportReport]) {
    events.emit(CoreEvent::Keys {
        user: me.to_string(),
        state: KeysState::CaughtUp,
        imported: Some(reports.iter().map(|report| report.imported).sum()),
        room_keys: Some(reports.iter().map(|report| report.room_keys.len()).sum()),
        reason: None,
    });
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use serde_json::{json, Value};
    use wbf_wire::pack::{control, device};
    use wbf_wire::Kind;

    use wbf_sdk::Transport;

    use crate::accounts::AccountDir;
    use crate::backend_choice::MethodHome;
    use crate::event::KeysState;
    use crate::link_pool::LinkRole;
    use crate::test_support::*;
    use crate::{Core, CoreEvent, Target};

    fn cd_seq_of(account: &AccountDir) -> Option<u64> {
        wbf_sdk::to_device_state::ToDeviceState::load(&account.matrix_store_dir())
            .unwrap()
            .cd_seq
    }

    /// 下一則 `keys.state`：(state, imported, room_keys, reason)。
    async fn next_keys_state(
        seen: &mut tokio::sync::broadcast::Receiver<CoreEvent>,
    ) -> (KeysState, Option<usize>, Option<usize>, Option<String>) {
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                if let CoreEvent::Keys {
                    state,
                    imported,
                    room_keys,
                    reason,
                    ..
                } = seen.recv().await.unwrap()
                {
                    return (state, imported, room_keys, reason);
                }
            }
        })
        .await
        .expect("a keys.state arrives")
    }

    /// 開訂閱線之前佇列裡已經有東西（離線期間到的）：`init_connection` 訂了金鑰、追平（Fetch → 匯入 → 銷毀）、發 `caught_up`；
    /// 之後推來一包走同一支（銷毀那一包）；帶 `gap` 的包從佇列頭拉一次，漏的那則一起回來；關訂閱線 task 收掉。
    #[tokio::test]
    async fn the_line_subscribes_keys_catches_up_and_imports_pushes_through_one_path() {
        let dir = scratch("keys");
        let (core, account) = core_with_wbf_account(&dir).await;
        let mut seen = core.subscribe();
        let events: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
        let (mut client, fake) = memory_client_with_hello(events).await;
        fake.to_device
            .lock()
            .unwrap()
            .extend([to_device_item(1), to_device_item(2)]);
        core.init_connection(&account, LinkRole::Subscriptions, &mut client)
            .await
            .expect("subscribe");
        let pool = core.pool_of_account(&account).unwrap();
        drop(
            pool.acquire(LinkRole::Subscriptions, || async move { Ok(client) })
                .await
                .unwrap(),
        );
        let device_subscription_id = fake
            .device_subscription_ids
            .lock()
            .unwrap()
            .last()
            .copied()
            .expect("server got a Device/Subscribe");
        assert_eq!(
            *fake.destroyed.lock().unwrap(),
            vec![1, 2],
            "上線追平：拉到的兩則匯完就銷毀"
        );
        assert_eq!(cd_seq_of(&account), Some(2));
        assert!(core.is_key_syncing(&account));
        assert_eq!(
            next_keys_state(&mut seen).await,
            (KeysState::CaughtUp, Some(2), Some(0), None)
        );

        // 推來一包：跟拉的一樣——匯入、落地、銷毀那一包。
        fake.to_device.lock().unwrap().push(to_device_item(3));
        fake.outbound
            .send(device_push(
                device_subscription_id,
                0,
                false,
                &[to_device_item(3)],
            ))
            .await
            .unwrap();
        assert_eq!(
            next_keys_state(&mut seen).await,
            (KeysState::CaughtUp, Some(1), Some(0), None)
        );
        assert_eq!(*fake.destroyed.lock().unwrap(), vec![1, 2, 3]);
        assert_eq!(cd_seq_of(&account), Some(3));

        // server 丟過一包（4 還在佇列裡沒推到）、推 5 帶 gap：從水位 3 拉一次，4 與 5 一起回來。
        fake.to_device
            .lock()
            .unwrap()
            .extend([to_device_item(4), to_device_item(5)]);
        fake.outbound
            .send(device_push(
                device_subscription_id,
                1,
                true,
                &[to_device_item(5)],
            ))
            .await
            .unwrap();
        assert_eq!(
            next_keys_state(&mut seen).await,
            (KeysState::CaughtUp, Some(2), Some(0), None),
            "gap 那包不先匯：拉回來的是漏的 4 加上這包的 5"
        );
        assert_eq!(*fake.destroyed.lock().unwrap(), vec![1, 2, 3, 4, 5]);
        assert_eq!(cd_seq_of(&account), Some(5));

        assert!(core.close_subscriptions(&Target::default()).await.unwrap());
        assert!(!core.is_key_syncing(&account));
        fake.task.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 被另一台裝置接手（server 對金鑰訂閱送 Error）：task 停、發 `keys.state: stopped` 帶原因；🚫 不重訂、🚫 不關線——
    /// 房間訂閱還在同一條線上活著。
    #[tokio::test]
    async fn a_taken_over_key_subscription_stops_and_says_so_but_keeps_the_line() {
        let dir = scratch("keys-superseded");
        let (core, account) = core_with_wbf_account(&dir).await;
        let mut seen = core.subscribe();
        let events: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
        let (_room_subscription_id, fake, pool) = subscribed(&core, &account, &events).await;
        let device_subscription_id = fake
            .device_subscription_ids
            .lock()
            .unwrap()
            .last()
            .copied()
            .expect("server got a Device/Subscribe");
        assert!(core.is_key_syncing(&account));

        fake.outbound
            .send(response(
                Kind::Control,
                control::ERROR,
                device_subscription_id,
                0,
                json!({ "code": "Superseded", "message": "another device took over" }),
                Vec::new(),
            ))
            .await
            .unwrap();
        wait_for_async(
            || async { !core.is_key_syncing(&account) },
            "the key task ends",
        )
        .await;
        let stopped = std::iter::from_fn(|| seen.try_recv().ok()).find_map(|event| match event {
            CoreEvent::Keys {
                state: KeysState::Stopped,
                reason,
                ..
            } => Some(reason),
            _ => None,
        });
        assert!(
            stopped
                .clone()
                .flatten()
                .is_some_and(|reason| reason.contains("ended the key subscription")),
            "停了要講原因：{stopped:?}"
        );
        assert!(core.is_room_syncing(&account), "房間那半照收");
        assert_eq!(pool.open_count(), 1, "線不關：被接手不是斷線");
        assert_eq!(
            fake.device_subscription_ids.lock().unwrap().len(),
            1,
            "🚫 不重訂"
        );
        fake.task.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 追平從佇列頭拉，🚫 不看 `td.json` 的記錄：就算記錄說「處理過到 99」（還有 Ack 前推來一包 3），佇列裡沒銷的 1、2、3 全部回來。
    /// 帶游標的話 `Fetch{99}` 拉到空、三則永遠問不到（PR #60 審查 🔴1、wbfuwunel #87）。
    #[tokio::test]
    async fn an_early_push_does_not_let_the_catch_up_skip_older_queued_keys() {
        let dir = scratch("keys-early");
        let (core, account) = core_with_wbf_account(&dir).await;
        wbf_sdk::to_device_state::ToDeviceState {
            cd_seq: Some(99),
            to_destroy: Vec::new(),
        }
        .save(&account.matrix_store_dir())
        .unwrap();
        let events: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
        let (mut client, fake) = memory_client_with_hello(events).await;
        fake.to_device.lock().unwrap().extend([
            to_device_item(1),
            to_device_item(2),
            to_device_item(3),
        ]);
        *fake.device_early_push.lock().unwrap() = Some((0, false, vec![to_device_item(3)]));
        core.init_connection(&account, LinkRole::Subscriptions, &mut client)
            .await
            .expect("subscribe");
        assert_eq!(
            *fake.destroyed.lock().unwrap(),
            vec![1, 2, 3],
            "追平從佇列頭拉，三則一起回來"
        );
        assert_eq!(
            cd_seq_of(&account),
            Some(99),
            "記錄只升不降，但它不影響拉什麼"
        );
        assert_eq!(
            *fake.fetch_requests.lock().unwrap(),
            vec![None],
            "追平不帶游標"
        );
        fake.task.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `CryptoState` 跟 `Push` 共用這條訂閱的 gap 旗：gap 真就是有推送沒到，要從佇列頭拉（🔴3）。
    #[tokio::test]
    async fn a_crypto_state_with_gap_pulls_the_queue() {
        let dir = scratch("keys-crypto-state-gap");
        let (core, account) = core_with_wbf_account(&dir).await;
        let events: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
        let (_room_subscription_id, fake, _pool) = subscribed(&core, &account, &events).await;
        let device_subscription_id = fake
            .device_subscription_ids
            .lock()
            .unwrap()
            .last()
            .copied()
            .expect("server got a Device/Subscribe");
        fake.to_device.lock().unwrap().push(to_device_item(6));
        fake.outbound
            .send(response(
                Kind::Device,
                device::CRYPTO_STATE,
                device_subscription_id,
                1,
                json!({ "otk_counts": { "signed_curve25519": 49 }, "unused_fallback_key_types": [], "gap": true }),
                Vec::new(),
            ))
            .await
            .unwrap();
        wait_for_async(
            || async { fake.destroyed.lock().unwrap().contains(&6) },
            "the gap on a CryptoState makes the task pull the queue",
        )
        .await;
        assert_eq!(cd_seq_of(&account), Some(6));
        fake.task.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 佇列頭就是水位（維護者 2026-09-26，wbfuwunel #87）：後到的推播匯成功，🚫 不會讓更早還在佇列裡的那則變得拉不到——
    /// 之前帶 `cd_seq` 的 `Fetch` 會（游標一過就問不到）。拉失敗的留著「要拉」，下一個事件再拉；而且每一次 `Fetch` 都不帶游標。
    #[tokio::test]
    async fn a_later_push_never_makes_an_earlier_queued_key_unreachable_and_failed_pulls_are_retried(
    ) {
        let dir = scratch("keys-head-is-watermark");
        let (core, account) = core_with_wbf_account(&dir).await;
        let events: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
        let (_room_subscription_id, fake, _pool) = subscribed(&core, &account, &events).await;
        let device_subscription_id = fake
            .device_subscription_ids
            .lock()
            .unwrap()
            .last()
            .copied()
            .expect("server got a Device/Subscribe");
        assert_eq!(fake.fetch_requests.lock().unwrap().len(), 1, "上線追平那次");

        // 佇列裡有 1、2，但只有 2 推到了（1 的推播沒到、也沒有 gap）：2 照樣匯、銷毀；1 還在佇列裡。
        fake.to_device
            .lock()
            .unwrap()
            .extend([to_device_item(1), to_device_item(2)]);
        fake.outbound
            .send(device_push(
                device_subscription_id,
                0,
                false,
                &[to_device_item(2)],
            ))
            .await
            .unwrap();
        wait_for_async(
            || async { fake.destroyed.lock().unwrap().contains(&2) },
            "the pushed item is imported and destroyed",
        )
        .await;
        assert_eq!(cd_seq_of(&account), Some(2), "紀錄上處理過的最新是 2");

        // 有訊號說要拉（CryptoState 帶 gap），但那次 Fetch 失敗：1 還沒回來，「要拉」留著。
        fake.fail_next_fetch
            .store(true, std::sync::atomic::Ordering::SeqCst);
        fake.outbound
            .send(response(
                Kind::Device,
                device::CRYPTO_STATE,
                device_subscription_id,
                1,
                json!({ "otk_counts": { "signed_curve25519": 49 }, "unused_fallback_key_types": [], "gap": true }),
                Vec::new(),
            ))
            .await
            .unwrap();
        wait_for_async(
            || async { fake.fetch_requests.lock().unwrap().len() == 2 },
            "the gap triggers a Fetch (which is made to fail)",
        )
        .await;
        assert!(!fake.destroyed.lock().unwrap().contains(&1));

        // 下一個事件（推 3）：3 照樣匯，然後重拉——不帶游標，所以 1 回來了（帶 cd_seq=3 的話永遠問不到）。
        fake.to_device.lock().unwrap().push(to_device_item(3));
        fake.outbound
            .send(device_push(
                device_subscription_id,
                2,
                false,
                &[to_device_item(3)],
            ))
            .await
            .unwrap();
        wait_for_async(
            || async { fake.destroyed.lock().unwrap().contains(&1) },
            "the retried pull brings back the earlier item",
        )
        .await;
        let destroyed = fake.destroyed.lock().unwrap().clone();
        assert!(
            [1, 2, 3].iter().all(|count| destroyed.contains(count)),
            "{destroyed:?}"
        );
        assert!(fake.to_device.lock().unwrap().is_empty(), "佇列清空");
        assert!(
            fake.fetch_requests
                .lock()
                .unwrap()
                .iter()
                .all(Option::is_none),
            "🚫 Fetch 從不帶 cd_seq：{:?}",
            fake.fetch_requests.lock().unwrap()
        );
        fake.task.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 對真的 wbfuwunel（同一個帳號兩台裝置）：B 是 core 那台（登入、上傳裝置金鑰、`open_subscriptions`）；A 是 sdk 層的另一台裝置，
    /// 查到 B 之後把一個房的房間金鑰用 to-device 分給 B → B 的收金鑰 task 收到 `Push`、匯進 crypto store → `keys.state: caught_up` 帶 `room_keys ≥ 1`。
    /// 最後 B 登出（走退訂那條）。
    ///
    /// `--ignored`；環境變數：`WBF_E2E_SERVER`、`WBF_E2E_USER`（完整 mxid）、`WBF_E2E_PASSWORD_FILE`、`WBF_E2E_ROOM`。
    #[tokio::test]
    #[ignore = "needs a running wbfuwunel: WBF_E2E_SERVER, WBF_E2E_USER, WBF_E2E_PASSWORD_FILE, WBF_E2E_ROOM"]
    async fn a_key_subscription_receives_a_room_key_shared_by_another_device_over_the_real_server()
    {
        let env = |name: &str| std::env::var(name).unwrap_or_else(|_| panic!("{name}"));
        let server = env("WBF_E2E_SERVER");
        let user = env("WBF_E2E_USER");
        let room = env("WBF_E2E_ROOM");
        let password = std::fs::read_to_string(env("WBF_E2E_PASSWORD_FILE"))
            .unwrap()
            .trim_end_matches(['\r', '\n'])
            .to_string();

        // B：core 那台。登入、上傳裝置金鑰（A 才查得到它）、開訂閱線（房間＋金鑰、追平）。
        let dir_b = scratch("real-keys-b");
        let core_b = Core::open(&dir_b);
        core_b.create_vault(None).unwrap();
        core_b
            .log_in(&server, &user, &password, "key-sync e2e B", true)
            .await
            .expect("B logs in");
        let account_b = core_b.current_account().unwrap();
        {
            let engine_b = core_b.olm_engine_of(&account_b).await.unwrap();
            let mut line = core_b
                .client_of(
                    &account_b,
                    Transport::WebSocket,
                    MethodHome::WbfSdkOnly,
                    LinkRole::Misc,
                )
                .await
                .unwrap();
            engine_b
                .send_outgoing_requests(&mut line)
                .await
                .expect("B uploads its device keys");
        }
        let mut seen = core_b.subscribe();
        core_b
            .open_subscriptions(&Target::default())
            .await
            .expect("B subscribes over the real server");
        assert!(core_b.is_key_syncing(&account_b));
        let (state, _, _, _) = next_keys_state(&mut seen).await;
        assert_eq!(state, KeysState::CaughtUp, "上線追平那則");

        // A：sdk 層的另一台裝置：上傳金鑰、查自己這個帳號的裝置（含 B）、把房間金鑰分給他們。
        let session_a =
            wbf_sdk::login::login_with_password(&server, &user, &password, "key-sync e2e A")
                .await
                .expect("A logs in");
        let channel = wbf_sdk::Channel::connect(
            &session_a.server,
            &session_a.access_token,
            Transport::WebSocket,
        )
        .await
        .expect("A connects");
        let mut ws_a = wbf_sdk::WbfClient::new(channel);
        ws_a.hello("key-sync e2e A", &[]).await.expect("A hello");
        let store_a = scratch("real-keys-a-store");
        let engine_a = wbf_sdk::crypto_engine::OlmEngine::open(
            &store_a,
            &wbf_sdk::vault::Key32([9u8; 32]),
            &session_a.user_id,
            &session_a.device_id,
        )
        .await
        .expect("A opens its crypto store");
        engine_a
            .send_outgoing_requests(&mut ws_a)
            .await
            .expect("A uploads keys");
        engine_a
            .track_users(std::slice::from_ref(&user))
            .await
            .unwrap();
        engine_a
            .mark_users_changed(std::slice::from_ref(&user))
            .await
            .unwrap();
        engine_a
            .send_outgoing_requests(&mut ws_a)
            .await
            .expect("A queries keys");
        let shared = engine_a
            .share_room_key(
                &mut ws_a,
                &room,
                std::slice::from_ref(&user),
                wbf_sdk::crypto_engine::room_key_share_settings(),
            )
            .await
            .expect("A shares the room key");
        assert!(shared >= 1, "A 至少要送給 B 一則 to-device");

        // B 的 task 收到 Push、匯進 store：caught_up 帶至少一把新的房間金鑰。
        let mut room_keys_seen = 0usize;
        while room_keys_seen == 0 {
            let (state, imported, room_keys, reason) = next_keys_state(&mut seen).await;
            assert_eq!(state, KeysState::CaughtUp, "{reason:?}");
            assert!(imported.unwrap_or(0) >= 1);
            room_keys_seen += room_keys.unwrap_or(0);
        }

        core_b
            .log_out(&user, None, true, true)
            .await
            .expect("B logs out (unsubscribes first)");
        wbf_sdk::login::logout(&session_a)
            .await
            .expect("A logs out");
        let _ = std::fs::remove_dir_all(&dir_b);
        let _ = std::fs::remove_dir_all(&store_a);
    }
}

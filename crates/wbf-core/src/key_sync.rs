//! 訂閱線的金鑰那半：`Device/Subscribe`、上線追平、推來一包就匯（key-sync.md）。
//!
//! 維護者 2026-09-24 定的形狀：
//!
//! - **推來的（`Push`）與主動拉的（`Fetch` 的 `Batch`）封包大同小異，最根本的處理是同一支**：sdk 的 `OlmEngine::import_items`
//!   ——匯進 crypto store、水位與待銷毀清單落地、對 server 銷毀那一包。這裡不解封包、不碰 store，只把 items 交給它。
//! - `Device/Fetch`／`ItemsDestroy` 走訂閱線（server：只有持有這台裝置佇列的連線能銷毀），所以 task 要用線時跟池拿同一格。
//! - 訂閱結束（被另一台裝置接手的 1505、線死了）就停、發 `keys.state: stopped`；🚫 不重訂（to-device-client.md §5.1：兩台會互踢），
//!   🚫 不主動重開線（等 server 支援更多連線再做）。線死了房間那半（`room_sync.rs`）會關那格。
//! - `keys.state` 留著（「有點多餘，但傾向保留——不然 RPC 無從知道」）。

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
        let opened = self
            .crypto_engines
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&account.dir)
            .cloned();
        if let Some(engine) = opened {
            return Ok(engine);
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
        // 兩個同時進來的：先放進去的贏，後到的那把丟掉（同一個 store，兩把狀態機各持一份會打架）。
        let engine = self
            .crypto_engines
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .entry(account.dir.clone())
            .or_insert(engine)
            .clone();
        Ok(engine)
    }

    /// 登出用：把長活的引擎丟掉（store 要刪，Windows 上開著刪不掉）。
    pub(crate) fn forget_crypto_engine(&self, account: &AccountDir) {
        self.crypto_engines
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&account.dir);
    }

    /// 訂閱線開好之後金鑰那半（`room_sync::init_connection` 叫）：`Device/Subscribe` → 上線追平（先訂再拉，中間到的沒人漏）→ 起收金鑰的 task。
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
        let engine = self.olm_engine_of(account).await?;
        let pool = self.pool_of_account(account)?;
        let me = session.user_id.clone();
        let mut subscription = client
            .device_subscription(&session.device_id, REPLY_TIMEOUT)
            .await?;
        // Ack 之前就推來的：跟之後的一樣處理（同一支）。
        let early: Vec<_> = subscription.early_pushes.drain(..).collect();
        let mut reports = Vec::new();
        for (_meta, items) in early {
            reports.push(engine.import_items(client, items, REPLY_TIMEOUT).await?);
        }
        // 上線追平（to-device-client.md §7）：離線期間漏的一窗一窗拉到 more=false；空窗也走一次（補送上次沒銷成的）。
        reports.extend(engine.pull_to_device(client, REPLY_TIMEOUT).await?);
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
        loop {
            // 本地收件匣滿過＝丟過推播：東西還在 server 佇列裡（沒銷毀前不會掉），拉一次補回。
            if subscription.take_gap() {
                self.pull("the subscription inbox overflowed").await;
            }
            let next = tokio::select! {
                _ = &mut stopped => return,
                next = subscription.next(PUSH_IDLE_TIMEOUT) => next,
            };
            // 訂閱還活著的那幾種都 `continue`；走到下面的只有「訂閱結束了」，帶著原因。
            let why = match next {
                Ok(Some(SubscribeReply::Push { meta, items })) => {
                    // gap：這包之前有被丟掉的（count 比它小、還在佇列裡）。🚫 不能先匯這包——水位一過它們就拉不到了；
                    // 從水位起拉一次，這包連同漏的一起回來（重複拿到無害：匯入與銷毀冪等）。
                    if meta.gap {
                        self.pull("the server dropped pushes before this one").await;
                    } else {
                        self.import(items).await;
                    }
                    continue;
                }
                // OTK 存量：用途是補上傳金鑰，那是 E2EE 的 RPC 面那支；這裡只講一聲。
                Ok(Some(SubscribeReply::CryptoState(state))) => {
                    self.events.progress(format!(
                        "keys: one-time key stock is now {:?} (uploading keys is not wired yet)",
                        state.otk_counts
                    ));
                    continue;
                }
                Ok(Some(SubscribeReply::Acknowledged)) => continue,
                Err(wbf_sdk::SdkError::Timeout(_)) => continue,
                // 一包壞了：它還在 server 佇列裡，拉一次就回來。
                Err(wbf_sdk::SdkError::Protocol(why)) => {
                    self.events.progress(format!(
                        "keys: a push could not be read ({why}); pulling the queue instead"
                    ));
                    self.pull("a push could not be read").await;
                    continue;
                }
                // server 對這段會話送了 Error（被另一台裝置接手的 1505 就是這樣來的）：會話到此為止。
                Err(error @ wbf_sdk::SdkError::Server { .. }) => {
                    format!("the server ended the key subscription: {error}")
                }
                Ok(None) => "the server ended the key subscription".to_string(),
                Err(error) => error.to_string(),
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

    /// 推來的一包：跟 `Fetch` 的一窗走同一支（匯入 → 落地 → 銷毀那一包）。線不在就講一聲：東西還在 server 佇列裡，下次開線的追平會拉回。
    async fn import(&self, items: Vec<(u64, serde_json::Value)>) {
        let Some(mut line) = self.pool.reuse(LinkRole::Subscriptions).await else {
            self.events.progress(format!(
                "keys: the subscriptions line is gone; {} pushed item(s) stay queued on the server until the line is reopened",
                items.len()
            ));
            return;
        };
        match self
            .engine
            .import_items(&mut line, items, REPLY_TIMEOUT)
            .await
        {
            Ok(report) => emit_caught_up(&self.events, &self.me, &[report]),
            // 匯入或銷毀失敗：沒落地的還在 server 佇列裡，下次拉再來；已落地的照樣有效（import_items 的順序鎖死）。
            Err(error) => self.events.progress(format!(
                "keys: importing a pushed batch failed (ignored; it stays queued on the server): {error}"
            )),
        }
    }

    /// 從水位起拉到追平（gap、本地丟包、壞包都走這裡）。
    async fn pull(&self, why: &str) {
        let Some(mut line) = self.pool.reuse(LinkRole::Subscriptions).await else {
            self.events.progress(format!(
                "keys: {why}, but the subscriptions line is gone; the queue is pulled when the line is reopened"
            ));
            return;
        };
        match self.engine.pull_to_device(&mut line, REPLY_TIMEOUT).await {
            Ok(reports) => emit_caught_up(&self.events, &self.me, &reports),
            Err(error) => self.events.progress(format!(
                "keys: {why}; pulling the queue failed (ignored; retried on the next push or reopen): {error}"
            )),
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
    use wbf_wire::pack::control;
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
    /// 之後推來一包走同一支（銷毀那一包、水位跟著走）；帶 `gap` 的包不先匯、從水位起拉一次，漏的那則一起回來；關訂閱線 task 收掉。
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

//! 裝置驗證的 RPC 面與「誰被信任」（/docs/design/keys/cross-signing-and-verification.md §5、§6）。
//! 要不要接受、表情對不對，一律由 UI 問使用者（/docs/design/overview/architecture-v2.md §3）；daemon 只照命令做、把變化推出去。
//!
//! 每個流程一個觀察者：每 [`WATCH_EVERY`] 看一次狀態，有變就發 `verify.state`，結束（完成、取消、狀態機裡沒了）或過了 [`WATCH_AT_MOST`] 就停。
//! 同一個帳號的觀察者共用一個停止開關：登出、關機叫停並等它們結束（/docs/design/daemon/task-supervision.md §2.1），不再握著引擎。
//! 用看的而**不**接上游的 `changes()` 串流：請求轉成比對時要換一條串流接，看狀態的寫法一支函數就說完了，代價只是最多慢 250 毫秒。
//! 只有 wbf 帳號（一般 Matrix 帳號沒有常駐的同步迴圈，收不到對方的驗證訊息，§0）。

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use wbf_sdk::cross_signing::{self, UserTrust};
use wbf_sdk::crypto_engine::OlmEngine;
use wbf_sdk::task_stop::{StopSignal, StopTimedOut, StoppableTask};
use wbf_sdk::verification::{self, VerifyStage, VerifyState, WatchedFlow};
use wbf_sdk::Transport;

use crate::backend_choice::MethodHome;
use crate::error::CoreError;
use crate::event::EventSink;
use crate::link_pool::{LinkPool, LinkRole};
use crate::shutdown::BackgroundGate;
use crate::{Core, CoreEvent, Target};

/// 觀察者多久看一次。
const WATCH_EVERY: Duration = Duration::from_millis(250);
/// 一個流程最多看多久：上游自己 10 分鐘逾時，多留一點讓「逾時取消」那一則推得出去。
const WATCH_AT_MOST: Duration = Duration::from_secs(15 * 60);
/// 驗證完成後隔多久再重查一次雙方的金鑰：對方的簽章可能比「完成」晚一步才上傳。
const REQUERY_AFTER_DONE: Duration = Duration::from_secs(3);

/// 正在看的流程：(這個帳號, 對方, flow id)。同一個流程只起一個觀察者（同一則請求可能被匯兩次：銷毀沒回來時佇列會再給）。
/// 另外記每個帳號的觀察者 task，叫停的人等得到它們。觀察者從收金鑰的 task 裡起（不是從 `Core`），所以自己握一份閘門。
#[derive(Debug)]
pub(crate) struct VerifyWatchers {
    gate: Arc<BackgroundGate>,
    flows: Mutex<HashSet<(String, String, String)>>,
    running: Mutex<HashMap<String, Vec<StoppableTask>>>,
}

impl VerifyWatchers {
    pub(crate) fn new(gate: Arc<BackgroundGate>) -> VerifyWatchers {
        VerifyWatchers {
            gate,
            flows: Mutex::default(),
            running: Mutex::default(),
        }
    }

    fn flows(&self) -> std::sync::MutexGuard<'_, HashSet<(String, String, String)>> {
        self.flows
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn running(&self) -> std::sync::MutexGuard<'_, HashMap<String, Vec<StoppableTask>>> {
        self.running
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// 走閘門起一個觀察者、記在這個帳號下；已經結束的順手清掉（PR #87 審查 oliver 🟡：原本直接 `tokio::spawn`，關機清空之後還起得來）。
    ///
    /// Return:
    ///     Ok(())               起了
    ///     Err(ShuttingDown)    關機開始了，沒起
    fn spawn<F, Fut>(&self, me: &str, work: F) -> Result<(), CoreError>
    where
        F: FnOnce(StopSignal) -> Fut,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        let task = self.gate.spawn("a verification watcher", work)?;
        let mut running = self.running();
        let tasks = running.entry(me.to_string()).or_default();
        tasks.retain(|task| !task.is_finished());
        tasks.push(task);
        Ok(())
    }

    /// 叫停這個帳號的觀察者、等它們自己結束（登出用）。
    ///
    /// Return:
    ///     Ok(())            都停了（含本來就沒有）
    ///     Err(StopTimedOut) 等不到；沒有 abort
    pub(crate) async fn stop_of(&self, me: &str) -> Result<(), StopTimedOut> {
        let tasks = self.running().remove(me);
        match tasks {
            Some(tasks) => stop_and_join(tasks).await,
            None => Ok(()),
        }
    }

    /// 叫停每個帳號的觀察者、等它們結束（關機用）。
    pub(crate) async fn stop_all(&self) -> Result<(), StopTimedOut> {
        let tasks: Vec<StoppableTask> = self
            .running()
            .drain()
            .flat_map(|(_, tasks)| tasks)
            .collect();
        stop_and_join(tasks).await
    }

    /// Return:
    ///     bool  true ＝ 新加的；false ＝ 已經在看
    fn insert(&self, me: &str, other_user: &str, flow_id: &str) -> bool {
        self.flows()
            .insert((me.to_string(), other_user.to_string(), flow_id.to_string()))
    }

    fn remove(&self, me: &str, other_user: &str, flow_id: &str) {
        self.flows()
            .remove(&(me.to_string(), other_user.to_string(), flow_id.to_string()));
    }

    /// Return:
    ///     Vec<(String, String)>  這個帳號正在看的 (對方, flow id)
    fn list_of(&self, me: &str) -> Vec<(String, String)> {
        self.flows()
            .iter()
            .filter(|(owner, _, _)| owner == me)
            .map(|(_, other, flow)| (other.clone(), flow.clone()))
            .collect()
    }
}

/// 叫停一串觀察者，等全部結束（加起來最多 `STOP_TIMEOUT`：先全部叫停，再一個一個等剩下的時間）。
async fn stop_and_join(tasks: Vec<StoppableTask>) -> Result<(), StopTimedOut> {
    for task in &tasks {
        task.request_stop();
    }
    let deadline = tokio::time::Instant::now() + wbf_sdk::task_stop::STOP_TIMEOUT;
    let mut outcome = Ok(());
    for task in tasks {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        if task.stop(left).await.is_err() {
            outcome = Err(StopTimedOut);
        }
    }
    outcome
}

/// 觀察者 task 握著的清單位置：drop 時拿掉。
struct WatcherSlot {
    watchers: Arc<VerifyWatchers>,
    me: String,
    other_user: String,
    flow_id: String,
}

impl Drop for WatcherSlot {
    fn drop(&mut self) {
        self.watchers
            .remove(&self.me, &self.other_user, &self.flow_id);
    }
}

/// 一個觀察者需要的東西（**不**握 `Core`）。
#[derive(Clone)]
pub(crate) struct VerifyWatch {
    pub(crate) watchers: Arc<VerifyWatchers>,
    pub(crate) engine: Arc<OlmEngine>,
    pub(crate) pool: Arc<LinkPool>,
    pub(crate) events: EventSink,
    pub(crate) me: String,
}

impl VerifyWatch {
    /// 開始看一個流程（已經在看就什麼都不做）。
    pub(crate) fn start(&self, other_user: &str, flow_id: &str) {
        if !self.watchers.insert(&self.me, other_user, flow_id) {
            return;
        }
        let watch = self.clone();
        let (other, flow) = (other_user.to_string(), flow_id.to_string());
        let started = self.watchers.spawn(&self.me, |stop| async move {
            // task 不論怎麼結束都把這個流程從清單拿掉，之後才看得到同一個 flow id。
            let _leave_the_list = WatcherSlot {
                watchers: watch.watchers.clone(),
                me: watch.me.clone(),
                other_user: other.clone(),
                flow_id: flow.clone(),
            };
            watch.run(&other, &flow, stop).await;
        });
        if started.is_err() {
            // 關機開始了、沒起：清單那一項由這裡拿掉（task 沒起，就沒有它的 `WatcherSlot` 來拿）。
            self.watchers.remove(&self.me, other_user, flow_id);
        }
    }

    /// 安全點是「兩次看之間」：發完推播、重查完才看開關（/docs/design/daemon/task-supervision.md §2.1）。
    async fn run(&self, other_user: &str, flow_id: &str, mut stop: StopSignal) {
        // 一開始就握住上游的物件：完成、取消的流程會被上游清出快取，每次重查就看不到最後那一步（`WatchedFlow`）。
        let Ok(Some(mut flow)) = WatchedFlow::find(&self.engine, other_user, flow_id) else {
            // 上游沒收這個請求（太舊、查不到對方那台的金鑰…）：講一聲，沒東西可看。
            self.events.progress(format!(
                "verification: {flow_id} with {other_user} is not in the crypto state machine; nothing to show"
            ));
            return;
        };
        let deadline = tokio::time::Instant::now() + WATCH_AT_MOST;
        let mut last: Option<VerifyState> = None;
        while tokio::time::Instant::now() < deadline {
            let now = flow.state(&self.engine);
            if last.as_ref() != Some(&now) {
                self.events.emit(CoreEvent::Verify {
                    user: self.me.clone(),
                    state: Box::new(now.clone()),
                });
                if now.state == VerifyStage::Done {
                    self.requery_after_done(other_user, &mut stop).await;
                }
            }
            if matches!(now.state, VerifyStage::Done | VerifyStage::Cancelled) {
                return;
            }
            last = Some(now);
            tokio::select! {
                _ = stop.requested() => return,
                _ = tokio::time::sleep(WATCH_EVERY) => {}
            }
        }
    }

    /// 驗證完：重查雙方的金鑰（剛上傳的簽章才回得到 store 裡、`devices.list` 看得到），並送掉狀態機排的請求
    /// （自我驗證完要跟對方要的秘密）。隔一下再查一次：對方的簽章可能晚一步才上傳。
    /// 叫停了就不再等第二次（登出、關機）。
    async fn requery_after_done(&self, other_user: &str, stop: &mut StopSignal) {
        let users = [self.me.clone(), other_user.to_string()];
        for wait in [Duration::ZERO, REQUERY_AFTER_DONE] {
            tokio::select! {
                _ = stop.requested() => return,
                _ = tokio::time::sleep(wait) => {}
            }
            let Some(mut line) = self.pool.reuse(LinkRole::Keys).await else {
                self.events.progress(
                    "verification: the keys line is gone; the verified keys are re-read when it reopens",
                );
                return;
            };
            let refreshed = async {
                self.engine.mark_users_changed(&users).await?;
                self.engine.send_outgoing_requests(&mut line).await
            };
            if let Err(error) = refreshed.await {
                self.events.progress(format!(
                    "verification: re-reading the verified keys failed: {error}"
                ));
            }
        }
    }
}

impl Core {
    /// 開一個驗證請求：自己（省略 device_id ＝ 送給自己所有裝置）或別人的某一台。
    ///
    /// Args:
    ///     user_id: example: "@bob:localhost"
    ///     device_id: example: Some("ABCDEFGHIJ")
    /// Return:
    ///     Ok(VerifyState)   剛開的流程（`requested`）
    ///     Err(Usage)        不是 wbf 帳號、沒有交叉簽章身分、裝置不認得
    ///     Err(Server)／Err(Network)  送不出去
    pub async fn verify_request(
        &self,
        user_id: &str,
        device_id: Option<&str>,
        target: &Target,
    ) -> Result<VerifyState, CoreError> {
        let (watch, mut client) = self.verify_parts(target).await?;
        let flow_id =
            verification::request_verification(&watch.engine, &mut client, user_id, device_id)
                .await?;
        drop(client);
        let mut flow = held_flow(&watch, user_id, &flow_id)?;
        watch.start(user_id, &flow_id);
        Ok(flow.state(&watch.engine))
    }

    /// 接受對方的請求、或對方開始的比對。
    pub async fn verify_accept(
        &self,
        user_id: &str,
        flow_id: &str,
        target: &Target,
    ) -> Result<VerifyState, CoreError> {
        let (watch, mut client) = self.verify_parts(target).await?;
        let mut flow = held_flow(&watch, user_id, flow_id)?;
        verification::accept(&watch.engine, &mut client, user_id, flow_id).await?;
        drop(client);
        watch.start(user_id, flow_id);
        Ok(flow.state(&watch.engine))
    }

    /// 從雙方都接受了的請求開始比對。
    pub async fn verify_start_sas(
        &self,
        user_id: &str,
        flow_id: &str,
        target: &Target,
    ) -> Result<VerifyState, CoreError> {
        let (watch, mut client) = self.verify_parts(target).await?;
        let mut flow = held_flow(&watch, user_id, flow_id)?;
        verification::start_sas(&watch.engine, &mut client, user_id, flow_id).await?;
        drop(client);
        Ok(flow.state(&watch.engine))
    }

    /// 使用者說表情一樣。
    pub async fn verify_confirm(
        &self,
        user_id: &str,
        flow_id: &str,
        target: &Target,
    ) -> Result<VerifyState, CoreError> {
        let (watch, mut client) = self.verify_parts(target).await?;
        let mut flow = held_flow(&watch, user_id, flow_id)?;
        verification::confirm(&watch.engine, &mut client, user_id, flow_id).await?;
        drop(client);
        Ok(flow.state(&watch.engine))
    }

    /// 取消。
    pub async fn verify_cancel(
        &self,
        user_id: &str,
        flow_id: &str,
        target: &Target,
    ) -> Result<VerifyState, CoreError> {
        let (watch, mut client) = self.verify_parts(target).await?;
        let mut flow = held_flow(&watch, user_id, flow_id)?;
        verification::cancel(&watch.engine, &mut client, user_id, flow_id).await?;
        drop(client);
        Ok(flow.state(&watch.engine))
    }

    /// 這個帳號還在進行的驗證（daemon 重開就沒了）。
    ///
    /// Return:
    ///     Ok(Vec<VerifyState>)  照 flow id 排序
    ///     Err(Usage)            不是 wbf 帳號
    pub async fn verify_list(&self, target: &Target) -> Result<Vec<VerifyState>, CoreError> {
        let account = self.account_or_current(target)?;
        let me = self.session_of(&account)?.user_id;
        let engine = self.olm_engine_of(&account).await?;
        let mut states: Vec<VerifyState> = self
            .verify_watchers
            .list_of(&me)
            .iter()
            .filter_map(|(other, flow)| {
                verification::find_state(&engine, other, flow)
                    .ok()
                    .flatten()
            })
            .filter(|state| !matches!(state.state, VerifyStage::Done | VerifyStage::Cancelled))
            .collect();
        states.sort_by(|left, right| left.flow_id.cmp(&right.flow_id));
        Ok(states)
    }

    /// 這個人的身分與每台裝置被不被信任。先重查那個人的金鑰（走金鑰線），讀的才是 server 現在的簽章。
    ///
    /// Args:
    ///     user_id: example: "@bob:localhost"
    /// Return:
    ///     Ok(UserTrust)
    ///     Err(Usage)        不是 wbf 帳號、user_id 不合法
    ///     Err(Server)／Err(Network)  重查失敗
    pub async fn list_devices(
        &self,
        user_id: &str,
        target: &Target,
    ) -> Result<UserTrust, CoreError> {
        let (watch, mut client) = self.verify_parts(target).await?;
        let users = [user_id.to_string()];
        watch.engine.track_users(&users).await?;
        watch.engine.mark_users_changed(&users).await?;
        watch.engine.send_outgoing_requests(&mut client).await?;
        drop(client);
        Ok(cross_signing::list_devices(&watch.engine, user_id).await?)
    }

    /// 收到別人要跟這台驗證的請求時（金鑰線匯完一批）：開始看那個流程。
    pub(crate) fn verify_watch_of(
        &self,
        engine: Arc<OlmEngine>,
        pool: Arc<LinkPool>,
        me: &str,
    ) -> VerifyWatch {
        VerifyWatch {
            watchers: self.verify_watchers.clone(),
            engine,
            pool,
            events: self.events.clone(),
            me: me.to_string(),
        }
    }

    /// 驗證的每一步要的：這個帳號的引擎、觀察者、金鑰線。
    async fn verify_parts(
        &self,
        target: &Target,
    ) -> Result<(VerifyWatch, crate::PooledClient), CoreError> {
        let account = self.account_or_current(target)?;
        let me = self.session_of(&account)?.user_id;
        let engine = self.olm_engine_of(&account).await?;
        let pool = self.pool_of_account(&account)?;
        let client = self
            .client_of(
                &account,
                Transport::WebSocket,
                MethodHome::WbfSdkOnly,
                LinkRole::Keys,
            )
            .await?;
        Ok((self.verify_watch_of(engine, pool, &me), client))
    }
}

/// 動手之前先握住這個流程（`WatchedFlow`）：做完那一步上游可能就把它清出快取，回的狀態要從握著的物件讀。
///
/// Return:
///     Ok(WatchedFlow)
///     Err(Usage)   狀態機裡沒有這個流程（從沒有過、已經結束、或 daemon 重開了）
fn held_flow(watch: &VerifyWatch, user_id: &str, flow_id: &str) -> Result<WatchedFlow, CoreError> {
    WatchedFlow::find(&watch.engine, user_id, flow_id)?.ok_or_else(|| {
        CoreError::new(
            crate::CoreErrorKind::Usage,
            format!("there is no verification {flow_id} with {user_id}"),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::VerifyWatch;
    use crate::error::CoreErrorKind;
    use crate::test_support::{core_with_wbf_account, scratch};

    /// 閘門關了之後，驗證觀察者與金鑰後台都起不來（PR #87 審查 oliver 🟡）。這裡直接叫 `VerifyWatch::start`——
    /// 收金鑰的 task 在追平時呼叫的就是它，走的是同一條拒絕路（PR #88 審查 oliver 🟢：第一版的名字說成「從在跑的工作裡觸發」，比它實際測的多）。
    /// 觀察者沒起，「正在看」的清單也不留一筆死項（不然同一個流程之後再也起不來）。
    #[tokio::test]
    async fn once_the_gate_is_closed_a_verification_watcher_and_a_key_sender_are_refused() {
        let dir = scratch("verify-gate-closed");
        let (core, account) = core_with_wbf_account(&dir).await;
        let watch = VerifyWatch {
            watchers: core.verify_watchers.clone(),
            engine: core.olm_engine_of(&account).await.unwrap(),
            pool: core.pool_of_account(&account).unwrap(),
            events: core.events.clone(),
            me: core.session_of(&account).unwrap().user_id,
        };
        core.background.close();

        watch.start("@someone:localhost", "flow-1");
        assert!(
            core.verify_watchers.list_of(&watch.me).is_empty(),
            "no dead entry left"
        );
        assert!(
            core.verify_watchers.running().is_empty(),
            "no watcher started"
        );

        let share = core.key_share_inbox(&account).await.map(|_| ());
        assert_eq!(
            share.map_err(|error| error.kind),
            Err(CoreErrorKind::ShuttingDown)
        );
        assert!(core.key_shares.lock().unwrap().is_empty());
        core.shutdown().await;
        let _ = std::fs::remove_dir_all(&dir);
    }
}

//! 背景 task 怎麼起、怎麼停（/docs/design/daemon/task-supervision.md §2.1、§2.3、§2.4）：一律送開關、等它自己結束，**不** abort。
//!
//! - **起**只有一個入口：[`BackgroundGate::spawn`]。關機開始之後它一律拒絕，所以「關機中不起任何新工作」靠的是**一個閘門**，
//!   不是每個起工作的地方各自記得去看旗子（維護者 2026-11-18；PR #87 審查連續三輪各抓到一個沒看旗子的地方）。
//!   crate 根的 clippy 設定禁止直接呼叫 `tokio::spawn`／`StoppableTask::spawn`（`crates/wbf-core/clippy.toml`），繞路編譯不過。
//! - 閘門登記它起的每一個 task；[`Core::shutdown`] 最後叫停並等**全部**——不管它有沒有被放進哪一張表，所以漏不掉。
//! - 換線時被換下來的舊 task 交給「停止中」的清單（[`Core::retire_task`]）：開線的人握著那格的寫鎖，舊 task 收攤時也要那把鎖，當場等會互相等。
//!   登出（[`Core::stop_background_of`]）與關機（[`Core::shutdown`]）等清單裡的。
//! - 等不到（[`wbf_sdk::task_stop::STOP_TIMEOUT`]）就講一聲、往下走：那是 bug 的訊號，task 照樣會在它的下一個安全點停。

use std::future::Future;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use tokio_util::task::TaskTracker;
use wbf_sdk::task_stop::{StopSignal, StopSwitch, StopTimedOut, StoppableTask, STOP_TIMEOUT};

use crate::accounts::AccountDir;
use crate::error::{CoreError, CoreErrorKind};
use crate::Core;

/// 所有背景 task 的唯一入口：關機開始（[`BackgroundGate::close`]）之後一律不起。
/// 起的時候登記它的開關與它的結束（`TaskTracker`），[`BackgroundGate::stop_all`] 叫停並等全部。
#[derive(Debug, Default)]
pub(crate) struct BackgroundGate {
    state: Mutex<GateState>,
    tracker: TaskTracker,
}

#[derive(Debug, Default)]
struct GateState {
    closed: bool,
    switches: Vec<StopSwitch>,
}

impl BackgroundGate {
    fn state(&self) -> std::sync::MutexGuard<'_, GateState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// 起一個背景 task。關機開始之後拒絕：判斷與登記在同一把鎖裡，跟 [`BackgroundGate::close`] 不會交錯。
    /// task 自己起的子 task（例：下載處理端的發送端）不走這裡，由父 task 結束前等完。
    ///
    /// Args:
    ///     what: 給人看的名字, example: "room sync"
    ///     work: 收開關、回 task 本體, example: |stop| task.run(stop)
    /// Return:
    ///     Ok(StoppableTask)    起了，呼叫端照常握著它（放進自己的表、登出時叫停）
    ///     Err(ShuttingDown)    關機開始了，沒起
    pub(crate) fn spawn<F, Fut>(&self, what: &str, work: F) -> Result<StoppableTask, CoreError>
    where
        F: FnOnce(StopSignal) -> Fut,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let mut state = self.state();
        if state.closed {
            return Err(shutting_down(what));
        }
        state.switches.retain(StopSwitch::is_listened_to);
        let tracker = self.tracker.clone();
        // 這裡就是那個唯一入口（模組註解）。
        #[allow(clippy::disallowed_methods)]
        let task = StoppableTask::spawn(|stop| tracker.track_future(work(stop)));
        state.switches.push(task.switch());
        Ok(task)
    }

    /// Return:
    ///     bool  true ＝ 關機開始了
    pub(crate) fn is_closed(&self) -> bool {
        self.state().closed
    }

    /// 關機的第一步：之後 [`BackgroundGate::spawn`] 一律拒絕。只會從開變關。
    pub(crate) fn close(&self) {
        self.state().closed = true;
    }

    /// 叫停閘門起過、還沒結束的每一個 task，等它們全部結束（最多 `timeout`）。
    ///
    /// Return:
    ///     Ok(())            全部結束了
    ///     Err(StopTimedOut) 等不到；沒有 abort
    pub(crate) async fn stop_all(&self, timeout: Duration) -> Result<(), StopTimedOut> {
        let switches = std::mem::take(&mut self.state().switches);
        for switch in &switches {
            switch.request_stop();
        }
        self.tracker.close();
        tokio::time::timeout(timeout, self.tracker.wait())
            .await
            .map_err(|_| StopTimedOut)
    }
}

fn shutting_down(what: &str) -> CoreError {
    CoreError::new(
        CoreErrorKind::ShuttingDown,
        format!("the daemon is shutting down; {what} is refused"),
    )
}

/// 被換下來、已經叫停、還沒確認結束的 task。
pub(crate) struct RetiringTask {
    account_dir: PathBuf,
    what: &'static str,
    task: StoppableTask,
}

impl Core {
    /// 關機開始之後，「建新的資源」一律拒絕：新的池、新線、新的 `cache.db`（PR #87 審查 rumia 🟡2）。已經存在的照常給：收尾中的 task 還要用。
    /// 「起新的工作」不在這裡，在 [`BackgroundGate::spawn`]；兩個看的是同一個開關（閘門關了沒）。
    ///
    /// Args:
    ///     what: 要建的東西，給人看的, example: "a new link"
    /// Return:
    ///     Ok(())               還沒關機
    ///     Err(ShuttingDown)    關機開始了
    pub(crate) fn check_not_shutting_down(&self, what: &str) -> Result<(), CoreError> {
        if self.background.is_closed() {
            return Err(shutting_down(what));
        }
        Ok(())
    }

    /// 叫停一個被換下來的 task、放進停止中的清單（不在這裡等，模組註解）。已經結束的順手清掉。
    ///
    /// Args:
    ///     what: 給人看的名字, example: "room sync"
    pub(crate) fn retire_task(
        &self,
        account: &AccountDir,
        what: &'static str,
        task: StoppableTask,
    ) {
        task.request_stop();
        let mut retiring = self
            .retiring
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        retiring.retain(|retired| !retired.task.is_finished());
        retiring.push(RetiringTask {
            account_dir: account.dir.clone(),
            what,
            task,
        });
    }

    /// 叫停、等它自己結束；等不到就講一聲（**不** abort）。
    ///
    /// Args:
    ///     what: 給人看的名字, example: "key sync"
    pub(crate) async fn stop_and_wait(
        &self,
        account: &AccountDir,
        what: &str,
        task: StoppableTask,
    ) {
        if task.stop(STOP_TIMEOUT).await.is_err() {
            self.events.progress(format!(
                "{what} of {} did not stop within {}s; it stops at its next safe point",
                account.label(),
                STOP_TIMEOUT.as_secs()
            ));
        }
    }

    /// 這個帳號停止中的那些：等它們結束。
    pub(crate) async fn stop_retired_of(&self, account: &AccountDir) {
        let mine: Vec<RetiringTask> = {
            let mut retiring = self
                .retiring
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let (mine, others) = std::mem::take(&mut *retiring)
                .into_iter()
                .partition(|retired| retired.account_dir == account.dir);
            *retiring = others;
            mine
        };
        for retired in mine {
            self.stop_and_wait(account, retired.what, retired.task)
                .await;
        }
    }

    /// 這個帳號的背景 task：叫停、等它們結束（登出用；**不**登出、**不**關線、**不**刪東西）。下載處理端由之後的 `close_links` 收。
    /// 順序：先收跟上游的兩條（收金鑰的那條會起驗證觀察者，所以先停它），再收金鑰後台、驗證觀察者、停止中的。
    pub(crate) async fn stop_background_of(&self, account: &AccountDir) {
        self.stop_room_sync_of(account).await;
        self.stop_key_sync_of(account).await;
        self.stop_room_key_share_of(account).await;
        if let Ok(session) = self.session_of(account) {
            if self
                .verify_watchers
                .stop_of(&session.user_id)
                .await
                .is_err()
            {
                self.events.progress(format!(
                    "verification watchers of {} did not stop within {}s",
                    account.label(),
                    STOP_TIMEOUT.as_secs()
                ));
            }
        }
        self.stop_retired_of(account).await;
    }

    /// 關機（/docs/design/daemon/task-supervision.md §2.3 第 4、5 步）：每個背景 task 叫停、等它們結束，每個池關線，每個 `cache.db` 寫完佇列再關。
    /// **不**登出、**不**刪任何東西：重開 daemon 之後照常接著用。之後這個 `Core` 只剩讀的用途（呼叫端接著就結束程序）。
    pub async fn shutdown(&self) {
        // 第一步就關閘門：之後不起任何新的背景 task（[`BackgroundGate::spawn`]）、不建新的池、線、`cache.db`（[`Core::check_not_shutting_down`]）。
        self.background.close();
        let tasks: Vec<(PathBuf, &'static str, StoppableTask)> = {
            let mut tasks = Vec::new();
            for (dir, task) in self
                .room_syncs
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .drain()
            {
                tasks.push((dir, "room sync", task));
            }
            for (dir, task) in self
                .key_syncs
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .drain()
            {
                tasks.push((dir, "key sync", task));
            }
            for (dir, share) in self
                .key_shares
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .drain()
            {
                tasks.push((dir, "the background key sender", share.into_task()));
            }
            for retired in self
                .retiring
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .drain(..)
            {
                tasks.push((retired.account_dir, retired.what, retired.task));
            }
            tasks
        };
        for (dir, what, task) in tasks {
            if task.stop(STOP_TIMEOUT).await.is_err() {
                self.events.progress(format!(
                    "{what} of {} did not stop within {}s",
                    dir.display(),
                    STOP_TIMEOUT.as_secs()
                ));
            }
        }
        if self.verify_watchers.stop_all().await.is_err() {
            self.events.progress(format!(
                "verification watchers did not stop within {}s",
                STOP_TIMEOUT.as_secs()
            ));
        }
        self.stop_all_downloads().await;
        // 最後掃一次：閘門起過、還沒結束的全部叫停並等——上面哪張表沒收到的（例：閘門關之前起、清空之後才插進表的）也在這裡。
        // 排在關池、關 `cache.db` 之前：它們手上那件還要用線與庫。
        if self.background.stop_all(STOP_TIMEOUT).await.is_err() {
            self.events.progress(format!(
                "some background tasks did not stop within {}s; they stop at their next safe point",
                STOP_TIMEOUT.as_secs()
            ));
        }
        let pools: Vec<_> = self
            .link_pools
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .drain()
            .map(|(_, pool)| pool)
            .collect();
        for pool in pools {
            // 每一格有上限：還在連線的開線握著寫鎖時，關機不能被它卡住（`LinkPool::close_all_within`）。
            pool.close_all_within("the daemon is shutting down", STOP_TIMEOUT)
                .await;
        }
        self.close_all_server_caches();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    use wbf_sdk::login::SessionBackend;

    use super::BackgroundGate;
    use crate::error::CoreErrorKind;
    use crate::link_pool::LinkRole;
    use crate::test_support::{core_with_account_on, core_with_wbf_account, scratch};

    /// 開線卡在連線裡（TCP 收下、升級永遠不回）的時候關機：關機照樣在上限內回來，**不**被握著那格寫鎖的開線卡住（PR #87 審查 oliver／salvia 🔴；形狀照 oliver 的探針）。
    /// 上限是「關池每一格 5 秒」，加上其他步驟，給 15 秒；連線本身的逾時是 60 秒，所以這裡回來靠的是關池的上限，不是連線逾時。
    #[tokio::test]
    async fn shutdown_returns_while_a_line_is_stuck_connecting() {
        let silent = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let server = format!("http://127.0.0.1:{}", silent.local_addr().unwrap().port());
        let dir = scratch("shutdown-stuck-connect");
        let (core, account) = core_with_account_on(&dir, &server, SessionBackend::WbfSdk).await;
        let pool = core.pool_of_account(&account).unwrap();
        let opening = pool.ensure_open(LinkRole::Rooms, || {
            core.open_link(&account, LinkRole::Rooms)
        });
        let shutting_down = async {
            // 讓開線先拿到寫鎖、卡進連線。
            tokio::time::sleep(Duration::from_millis(300)).await;
            tokio::time::timeout(Duration::from_secs(15), core.shutdown()).await
        };
        tokio::select! {
            opened = opening => panic!("the open cannot finish against a silent server: {:?}", opened.map(|_| ())),
            finished = shutting_down => assert!(finished.is_ok(), "shutdown did not return within 15s"),
        }
        drop(silent);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 閘門：關上之後一律不起；關上之前起的，**不管有沒有放進哪張表**，`stop_all` 都叫停並等到它結束（維護者 2026-11-18：一個閘門，不靠每個呼叫端記得收）。
    #[tokio::test]
    async fn the_gate_refuses_after_closing_and_stops_everything_it_started() {
        let gate = BackgroundGate::default();
        let ended = Arc::new(AtomicBool::new(false));
        let marked = ended.clone();
        // 起了、沒放進任何表——就是「閘門關之前起、清空之後才要插進表」的那種。handle 留著，證明叫停它的是 stop_all，不是丟掉 handle。
        let orphan = gate
            .spawn("an orphan", |mut stop| async move {
                stop.requested().await;
                marked.store(true, Ordering::SeqCst);
            })
            .unwrap();
        gate.close();
        assert!(gate.is_closed());
        let too_late = gate.spawn("too late", |_| async {}).map(|_| ());
        assert_eq!(
            too_late.map_err(|error| error.kind),
            Err(CoreErrorKind::ShuttingDown)
        );
        assert!(!ended.load(Ordering::SeqCst), "nothing stopped it yet");

        assert_eq!(gate.stop_all(Duration::from_secs(2)).await, Ok(()));
        assert!(
            ended.load(Ordering::SeqCst),
            "stop_all stopped the task no table held"
        );
        drop(orphan);
    }

    /// `Core::shutdown` 真的會掃：閘門起過、沒放進任何表的 task，關機回來時已經結束（不靠呼叫端記得收）。
    #[tokio::test]
    async fn core_shutdown_stops_a_task_that_no_table_holds() {
        let dir = scratch("shutdown-sweeps");
        let (core, _account) = core_with_wbf_account(&dir).await;
        let ended = Arc::new(AtomicBool::new(false));
        let marked = ended.clone();
        let orphan = core
            .background
            .spawn("an orphan", |mut stop| async move {
                stop.requested().await;
                marked.store(true, Ordering::SeqCst);
            })
            .unwrap();
        core.shutdown().await;
        assert!(
            ended.load(Ordering::SeqCst),
            "the shutdown sweep stopped it"
        );
        drop(orphan);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 關機之後「建新的」一律 `ShuttingDown`：池、`cache.db` 都**不**被重新建出來（PR #87 審查 rumia 🟡2）。
    /// 關機之前同一個帳號建得出來（不然這條測的只是「本來就建不出來」）。
    #[tokio::test]
    async fn after_shutdown_no_new_pool_or_cache_is_opened() {
        let dir = scratch("shutdown-no-new");
        let (core, account) = core_with_wbf_account(&dir).await;
        let server = core.session_of(&account).unwrap().server;
        assert!(
            core.pool_of_account(&account).is_ok(),
            "opens before shutdown"
        );
        assert!(
            core.server_cache_of(&account, &server).is_ok(),
            "opens before shutdown"
        );

        core.shutdown().await;
        assert!(core.link_pools.lock().unwrap().is_empty());
        assert!(core.server_caches.lock().unwrap().is_empty());
        let pool = core.pool_of_account(&account).map(|_| ());
        assert_eq!(
            pool.map_err(|error| error.kind),
            Err(CoreErrorKind::ShuttingDown)
        );
        let cache = core.server_cache_of(&account, &server).map(|_| ());
        assert_eq!(
            cache.map_err(|error| error.kind),
            Err(CoreErrorKind::ShuttingDown)
        );
        assert!(
            core.link_pools.lock().unwrap().is_empty(),
            "nothing was re-created"
        );
        assert!(
            core.server_caches.lock().unwrap().is_empty(),
            "nothing was re-created"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

//! 背景 task 怎麼停（/docs/design/daemon/task-supervision.md §2.1、§2.3）：一律送開關、等它自己結束，**不** abort。
//!
//! - 換線時被換下來的舊 task 交給「停止中」的清單（[`Core::retire_task`]）：開線的人握著那格的寫鎖，舊 task 收攤時也要那把鎖，當場等會互相等。
//!   登出（[`Core::stop_background_of`]）與關機（[`Core::shutdown`]）等清單裡的。
//! - 等不到（[`wbf_sdk::task_stop::STOP_TIMEOUT`]）就講一聲、往下走：那是 bug 的訊號，task 照樣會在它的下一個安全點停。

use std::path::PathBuf;
use std::sync::atomic::Ordering;

use wbf_sdk::task_stop::{StoppableTask, STOP_TIMEOUT};

use crate::accounts::AccountDir;
use crate::error::{CoreError, CoreErrorKind};
use crate::Core;

/// 被換下來、已經叫停、還沒確認結束的 task。
pub(crate) struct RetiringTask {
    account_dir: PathBuf,
    what: &'static str,
    task: StoppableTask,
}

impl Core {
    /// 關機開始之後，「建新的」一律拒絕：新的池、新線、新的 `cache.db`（PR #87 審查 rumia 🟡2：
    /// 等不到 5 秒就往下走的那一輪看線，原本會在 [`Core::shutdown`] 收掉之後把池、線、`cache.db` 重新建出來）。
    /// 已經存在的照常給：收尾中的 task 還要用它們把手上那件做完。
    ///
    /// Args:
    ///     what: 要建的東西，給人看的, example: "a new link"
    /// Return:
    ///     Ok(())               還沒關機
    ///     Err(ShuttingDown)    關機開始了
    pub(crate) fn check_not_shutting_down(&self, what: &str) -> Result<(), CoreError> {
        if self.shutting_down.load(Ordering::SeqCst) {
            return Err(CoreError::new(
                CoreErrorKind::ShuttingDown,
                format!("the daemon is shutting down; {what} is not opened"),
            ));
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
        // 第一步就立旗子：之後還在跑的那一輪看線建不出新的池、線、`cache.db`（[`Core::check_not_shutting_down`]）。
        self.shutting_down.store(true, Ordering::SeqCst);
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
        let pools: Vec<_> = self
            .link_pools
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .drain()
            .map(|(_, pool)| pool)
            .collect();
        for pool in pools {
            pool.close_all("the daemon is shutting down").await;
        }
        self.close_all_server_caches();
    }
}

#[cfg(test)]
mod tests {
    use crate::error::CoreErrorKind;
    use crate::test_support::{core_with_wbf_account, scratch};

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

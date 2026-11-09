//! 背景 task 怎麼停（/docs/design/daemon/task-supervision.md §2.1、§2.3）：一律送開關、等它自己結束，**不** abort。
//!
//! - 換線時被換下來的舊 task 交給「停止中」的清單（[`Core::retire_task`]）：開線的人握著那格的寫鎖，舊 task 收攤時也要那把鎖，當場等會互相等。
//!   登出（[`Core::stop_background_of`]）與關機（[`Core::shutdown`]）等清單裡的。
//! - 等不到（[`wbf_sdk::task_stop::STOP_TIMEOUT`]）就講一聲、往下走：那是 bug 的訊號，task 照樣會在它的下一個安全點停。

use std::path::PathBuf;

use wbf_sdk::task_stop::{StoppableTask, STOP_TIMEOUT};

use crate::accounts::AccountDir;
use crate::Core;

/// 被換下來、已經叫停、還沒確認結束的 task。
pub(crate) struct RetiringTask {
    account_dir: PathBuf,
    what: &'static str,
    task: StoppableTask,
}

impl Core {
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

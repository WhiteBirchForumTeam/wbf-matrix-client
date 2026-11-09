//! 正在跑的 RPC 請求與背景工作：登記、`cancel`、關機時等它們做完（/docs/design/daemon/task-supervision.md §2.2、§2.3）。
//!
//! - 每個請求一個取消開關（合作式：長工作在安全點看它，`wbf_core::job`）。`cancel { id }` 只找得到**這條連線**發的。
//! - 關機：先等請求做完（最多 [`REQUEST_GRACE`]），還沒完的送取消、再等它們回 `105`；背景工作（開線的一次性工作）也等。
//! - 禁止 abort：等不到就講一聲、往下一步走（task 照樣在它的下一個安全點停）。

use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use tokio_util::task::TaskTracker;
use wbf_sdk::task_stop::{StopSignal, StopSwitch, STOP_TIMEOUT};

/// 關機時等正在跑的請求做完多久，之後才送取消（維護者 2026-11-10 同意 10 秒，/docs/design/daemon/task-supervision.md §4 第 3 點）。
pub const REQUEST_GRACE: Duration = Duration::from_secs(10);

/// 一個請求的鍵：(這條連線的號, 請求的 `id`)。
type RequestKey = (u64, u64);

#[derive(Default)]
pub struct InFlight {
    next_connection: AtomicU64,
    switches: Mutex<HashMap<RequestKey, StopSwitch>>,
    requests: TaskTracker,
    background: TaskTracker,
    connections: TaskTracker,
}

impl InFlight {
    fn switches(&self) -> std::sync::MutexGuard<'_, HashMap<RequestKey, StopSwitch>> {
        self.switches
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// 一條新連線的號（只在這個程序裡唯一）。
    pub fn next_connection_number(&self) -> u64 {
        self.next_connection.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// 起一個請求的 task：有 `id` 的登記取消開關；關機時等得到它。
    ///
    /// Args:
    ///     connection: [`InFlight::next_connection_number`] 給的, example: 3
    ///     id: 請求的 `id`；None ＝ 不要回應的請求，取消不到, example: Some(7)
    ///     work: 收取消開關、回 task 本體
    pub fn spawn_request<F, Fut>(&self, connection: u64, id: Option<u64>, work: F)
    where
        F: FnOnce(StopSignal) -> Fut,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let (switch, signal) = StopSwitch::new();
        if let Some(id) = id {
            self.switches().insert((connection, id), switch.clone());
        }
        // 開關的另一頭跟著 task 走：連線關了、登記表裡那份被拿掉，也**不**算取消（前端關掉連線，它發起的長工作繼續跑，/docs/design/daemon/daemon-runtime.md §9）。
        let work = work(signal);
        self.requests.spawn(async move {
            let _keep_the_switch = switch;
            work.await;
        });
    }

    /// 請求做完了：拿掉它的取消開關（之後 `cancel` 它是 `was_running: false`）。
    pub fn finish_request(&self, connection: u64, id: u64) {
        self.switches().remove(&(connection, id));
    }

    /// `cancel { id }`（/docs/design/rpc-specs/rpc-spec.md §3.9）。
    ///
    /// Return:
    ///     bool  true ＝ 這條連線上這個 `id` 還在跑、送了取消；false ＝ 已經回完或不認得
    pub fn cancel_request(&self, connection: u64, id: u64) -> bool {
        match self.switches().get(&(connection, id)) {
            Some(switch) => {
                switch.request_stop();
                true
            }
            None => false,
        }
    }

    /// 起一個背景的一次性工作（`ensure_links`）：關機時等它做完。
    pub fn spawn_background<Fut>(&self, work: Fut)
    where
        Fut: Future<Output = ()> + Send + 'static,
    {
        self.background.spawn(work);
    }

    /// 關機的第 2 步：等請求做完，最多 [`REQUEST_GRACE`]；還沒完的送取消，再等 [`STOP_TIMEOUT`]。
    ///
    /// Return:
    ///     bool  true ＝ 全部做完了；false ＝ 取消之後還有等不到的（已經講過）
    pub async fn finish_requests(&self) -> bool {
        self.requests.close();
        if tokio::time::timeout(REQUEST_GRACE, self.requests.wait())
            .await
            .is_ok()
        {
            return true;
        }
        for switch in self.switches().values() {
            switch.request_stop();
        }
        let finished = tokio::time::timeout(STOP_TIMEOUT, self.requests.wait())
            .await
            .is_ok();
        if !finished {
            eprintln!(
                "{} request(s) did not finish {}s after being cancelled; they stop at their next safe point",
                self.requests.len(),
                STOP_TIMEOUT.as_secs()
            );
        }
        finished
    }

    /// 起一條 RPC 連線的 task：關機時等它把自己的請求回完、送出關閉通知。
    pub fn spawn_connection<Fut>(&self, work: Fut)
    where
        Fut: Future<Output = ()> + Send + 'static,
    {
        self.connections.spawn(work);
    }

    /// 關機：等每條連線送完關閉通知，最多 [`STOP_TIMEOUT`]（它們等的是自己的請求，先經過 [`InFlight::finish_requests`]）。
    pub async fn finish_connections(&self) -> bool {
        self.connections.close();
        let finished = tokio::time::timeout(STOP_TIMEOUT, self.connections.wait())
            .await
            .is_ok();
        if !finished {
            eprintln!(
                "{} RPC connection(s) did not close within {}s",
                self.connections.len(),
                STOP_TIMEOUT.as_secs()
            );
        }
        finished
    }

    /// 關機：等背景的一次性工作做完，最多 [`STOP_TIMEOUT`]。
    pub async fn finish_background(&self) -> bool {
        self.background.close();
        let finished = tokio::time::timeout(STOP_TIMEOUT, self.background.wait())
            .await
            .is_ok();
        if !finished {
            eprintln!(
                "{} background job(s) did not finish within {}s",
                self.background.len(),
                STOP_TIMEOUT.as_secs()
            );
        }
        finished
    }

    /// 現在還在跑的請求數（測試用）。
    #[cfg(test)]
    pub fn running_requests(&self) -> usize {
        self.requests.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;

    /// `cancel` 只找得到自己這條連線的；做完之後找不到。
    #[tokio::test]
    async fn a_request_is_cancelled_only_from_its_own_connection() {
        let in_flight = InFlight::default();
        let (mine, other) = (
            in_flight.next_connection_number(),
            in_flight.next_connection_number(),
        );
        let saw_cancel = Arc::new(AtomicBool::new(false));
        let seen = saw_cancel.clone();
        in_flight.spawn_request(mine, Some(7), |mut cancel| async move {
            cancel.requested().await;
            seen.store(true, Ordering::SeqCst);
        });
        assert!(!in_flight.cancel_request(other, 7), "another connection");
        assert!(in_flight.cancel_request(mine, 7));
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(saw_cancel.load(Ordering::SeqCst));
        in_flight.finish_request(mine, 7);
        assert!(!in_flight.cancel_request(mine, 7), "already done");
    }

    /// 登記表裡那份被拿掉（連線關了）**不**算取消：請求照樣跑完。
    #[tokio::test]
    async fn dropping_the_registered_switch_does_not_cancel_the_request() {
        let in_flight = InFlight::default();
        let connection = in_flight.next_connection_number();
        let cancelled = Arc::new(AtomicBool::new(false));
        let marked = cancelled.clone();
        in_flight.spawn_request(connection, Some(1), |cancel| async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            marked.store(cancel.is_requested(), Ordering::SeqCst);
        });
        in_flight.finish_request(connection, 1);
        assert!(in_flight.finish_requests().await);
        assert!(!cancelled.load(Ordering::SeqCst));
    }

    /// 關機：做得完的等它做完，不送取消。
    #[tokio::test]
    async fn shutting_down_waits_for_requests_that_finish_in_time() {
        let in_flight = InFlight::default();
        let connection = in_flight.next_connection_number();
        let finished = Arc::new(AtomicBool::new(false));
        let marked = finished.clone();
        in_flight.spawn_request(connection, Some(1), |cancel| async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            marked.store(!cancel.is_requested(), Ordering::SeqCst);
        });
        assert!(in_flight.finish_requests().await);
        assert!(finished.load(Ordering::SeqCst), "finished, not cancelled");
        assert_eq!(in_flight.running_requests(), 0);
    }
}

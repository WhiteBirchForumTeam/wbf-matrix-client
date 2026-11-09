//! 長活 task 的終止接口（/docs/design/daemon/task-supervision.md §0、§2.1）：送開關、等它自己 `return`。
//!
//! task 只在**安全點**看開關（等下一包、下一件事的那一個 `await`），手上那一件做完才停，所以停下來時沒有寫到一半的東西。
//! 丟掉 [`StoppableTask`] 只送開關、**不** abort：abort 讓 task 停在任何一個 `await` 上，那不是正常結束（維護者 2026-11-09）。

use std::future::Future;
use std::time::Duration;

use tokio::sync::watch;
use tokio::task::JoinHandle;

/// 叫停一個背景 task 之後最多等多久（維護者 2026-11-10 同意 5 秒，/docs/design/daemon/task-supervision.md §4 第 3 點）。
pub const STOP_TIMEOUT: Duration = Duration::from_secs(5);

/// task 手上那一份：叫停了沒。
#[derive(Clone, Debug)]
pub struct StopSignal {
    receiver: watch::Receiver<bool>,
    /// 只有 [`StopSignal::never`] 有：自己握著送出端，所以它永遠不會變成「另一頭不在了」。
    _keep_open: Option<std::sync::Arc<watch::Sender<bool>>>,
}

impl StopSignal {
    /// 等到有人叫停；開關的另一頭不在了也算（沒有人能再叫它停，留著就是孤兒）。只放在安全點的 `select!` 裡。
    pub async fn requested(&mut self) {
        let _ = self.receiver.wait_for(|stop| *stop).await;
    }

    /// Return:
    ///     bool  true ＝ 已經叫停（迴圈在兩件事之間看它）
    pub fn is_requested(&self) -> bool {
        *self.receiver.borrow() || self.receiver.has_changed().is_err()
    }

    /// 永遠不會被叫停的開關：給不需要停的呼叫點（例：測試、一次性工作）。
    pub fn never() -> StopSignal {
        let (sender, receiver) = watch::channel(false);
        // 送出端跟著這份走：丟掉的話 `requested` 會當成「叫停了」。
        StopSignal {
            receiver,
            _keep_open: Some(std::sync::Arc::new(sender)),
        }
    }
}

/// 送開關的那一頭；可以複製給很多個 task（一個開關停一群）。
#[derive(Clone, Debug)]
pub struct StopSwitch {
    sender: watch::Sender<bool>,
}

impl StopSwitch {
    /// Return:
    ///     (StopSwitch, StopSignal)  一對；`signal` 可以 clone 給很多個 task
    pub fn new() -> (StopSwitch, StopSignal) {
        let (sender, receiver) = watch::channel(false);
        (
            StopSwitch { sender },
            StopSignal {
                receiver,
                _keep_open: None,
            },
        )
    }

    pub fn request_stop(&self) {
        self.sender.send_replace(true);
    }

    /// 再多拿一份給新的 task。
    pub fn signal(&self) -> StopSignal {
        StopSignal {
            receiver: self.sender.subscribe(),
            _keep_open: None,
        }
    }
}

/// 等不到 task 自己結束：這是 bug 的訊號，呼叫端講一聲、往下走（**不**再 abort，task 照樣會在它的下一個安全點停）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StopTimedOut;

/// 一個有終止接口的 task：持有者握著它，`stop().await` ＝ 叫停＋等它回來。
#[derive(Debug)]
pub struct StoppableTask {
    switch: StopSwitch,
    task: Option<JoinHandle<()>>,
}

impl StoppableTask {
    /// 起一個 task，把它的開關交給它。⚠️ 要在 tokio runtime 裡叫。
    ///
    /// Args:
    ///     work: 收開關、回 task 本體, example: |stop| task.run(stop)
    pub fn spawn<F, Fut>(work: F) -> StoppableTask
    where
        F: FnOnce(StopSignal) -> Fut,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let (switch, signal) = StopSwitch::new();
        StoppableTask {
            task: Some(tokio::spawn(work(signal))),
            switch,
        }
    }

    pub fn request_stop(&self) {
        self.switch.request_stop();
    }

    /// Return:
    ///     bool  true ＝ task 已經結束（自己收攤了、或還沒起就沒了）
    pub fn is_finished(&self) -> bool {
        self.task.as_ref().is_none_or(JoinHandle::is_finished)
    }

    /// 叫停、等它自己 `return`。
    ///
    /// Args:
    ///     timeout: 最多等多久, example: Duration::from_secs(5)
    /// Return:
    ///     Ok(())            停了（含本來就停了）
    ///     Err(StopTimedOut) 等不到；task **沒有**被 abort，會在它的下一個安全點停
    pub async fn stop(mut self, timeout: Duration) -> Result<(), StopTimedOut> {
        self.switch.request_stop();
        let Some(task) = self.task.take() else {
            return Ok(());
        };
        match tokio::time::timeout(timeout, task).await {
            Ok(_) => Ok(()),
            Err(_) => Err(StopTimedOut),
        }
    }
}

impl Drop for StoppableTask {
    /// 只送開關：沒先 `stop()` 就丟掉是呼叫端的 bug，但 task 照樣在安全點收攤；禁止 abort。
    fn drop(&mut self) {
        self.switch.request_stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;

    /// 叫停之後 task 把手上那一件做完才回來：「做完」與「回來」都看得到。
    #[tokio::test]
    async fn stop_waits_for_the_task_to_finish_what_it_is_doing() {
        let finished_items = Arc::new(AtomicU32::new(0));
        let counted = finished_items.clone();
        let task = StoppableTask::spawn(|mut stop| async move {
            loop {
                tokio::select! {
                    _ = stop.requested() => return,
                    _ = tokio::time::sleep(Duration::from_millis(5)) => {
                        // 一件事：中間有 await，叫停也不能停在這裡
                        tokio::time::sleep(Duration::from_millis(30)).await;
                        counted.fetch_add(1, Ordering::SeqCst);
                    }
                }
            }
        });
        tokio::time::sleep(Duration::from_millis(15)).await;
        let started = finished_items.load(Ordering::SeqCst);
        assert_eq!(task.stop(Duration::from_secs(2)).await, Ok(()));
        assert!(
            finished_items.load(Ordering::SeqCst) > started,
            "the item in hand finished before the task returned"
        );
    }

    /// 等不到就回 `StopTimedOut`，task 沒被 abort：它之後照樣自己結束。
    #[tokio::test]
    async fn a_stop_that_times_out_leaves_the_task_to_finish_on_its_own() {
        let ended = Arc::new(AtomicU32::new(0));
        let marked = ended.clone();
        let task = StoppableTask::spawn(|mut stop| async move {
            stop.requested().await;
            tokio::time::sleep(Duration::from_millis(100)).await;
            marked.fetch_add(1, Ordering::SeqCst);
        });
        assert_eq!(
            task.stop(Duration::from_millis(10)).await,
            Err(StopTimedOut)
        );
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(ended.load(Ordering::SeqCst), 1, "not aborted");
    }

    /// 丟掉只送開關：task 收到、自己收攤（不是被 abort 掉在半路）。
    #[tokio::test]
    async fn dropping_the_holder_asks_the_task_to_stop_instead_of_aborting_it() {
        let ended = Arc::new(AtomicU32::new(0));
        let marked = ended.clone();
        let task = StoppableTask::spawn(|mut stop| async move {
            stop.requested().await;
            tokio::time::sleep(Duration::from_millis(20)).await;
            marked.fetch_add(1, Ordering::SeqCst);
        });
        drop(task);
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(ended.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_never_signal_is_never_requested_and_a_switch_reaches_every_copy() {
        let never = StopSignal::never();
        assert!(!never.is_requested());
        let (switch, first) = StopSwitch::new();
        let second = switch.signal();
        let third = first.clone();
        switch.request_stop();
        assert!(first.is_requested() && second.is_requested() && third.is_requested());
    }
}

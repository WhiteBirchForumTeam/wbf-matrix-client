//! 「現在跑的是哪一個工作？」—— 讓事件說得出它屬於誰（/docs/design/rpc-specs/rpc-spec.md §4 的 `progress.id`）。
//!
//! ## 問題
//!
//! core 的事件是**廣播**的（`event.rs`）：所有訂閱者都收到同一串。可是 daemon 同時可能有三個
//! 長工作在跑（一個上傳、一個 `sync.recent`、一個下載），前端要知道**每一則進度是哪一個請求的**
//! —— 而 core 完全不知道「請求」這種東西。
//!
//! ## 為什麼不是「多一個參數」
//!
//! 最直覺的做法是每個長工作的方法多收一個 `job: u64`。**不**做，因為那要改十幾個公開簽名、
//! 每一層都得原樣往下傳，而**中間任何一層忘了傳，事件就默默變成無主的** —— 那種漏法不會有人發現。
//!
//! ## 做法：`tokio` 的 task-local
//!
//! 呼叫端把整個工作包在 [`run_as_job`] 裡；`EventSink` 發事件時自己去問 [`current`]。
//! ⭐ **一個地方標、一個地方讀**，中間的十幾層什麼都不用知道。
//!
//! ```text
//! daemon:  run_as_job(請求的 id, core.upload_file(...)).await
//! core:    ... events.progress_of(3, Some(10), "chunk 3/10")   ← job 自動是那個 id
//! ```
//!
//! ⚠️ **限制**：task-local 不會跟著 `tokio::spawn` 進到新的 task。core 與 sdk 目前的長工作都在
//! 呼叫者的 task 上跑（進度回呼是同步的），所以沒問題；但哪天有人在 core 裡 `spawn` 一個
//! 會發進度的 task，那些事件就會變成無主的（`job: None`）——那時要嘛把 id 明確傳進去，
//! 要嘛在那個 task 外面再包一層 `run_as_job`。
//!
//! 📎 `None` 不是錯誤：CLI 直接叫 core（沒有請求 id）就是 `None`，daemon 的背景工作也是。
//!
//! ## 同一個做法帶取消開關（/docs/design/daemon/task-supervision.md §2.2）
//!
//! `cancel { id }` 要讓那個請求停下來，而且要**自己停**（在安全點收攤），不是被 drop 掉——drop 跟 abort 一樣停在任何一個 `await` 上。
//! 所以開關跟 `job` 一起標在 task-local：長工作在送完一塊、寫完一批之後問 [`check_cancelled`]，是的話回 `Cancelled`。
//! 沒有開關（CLI、背景工作）就永遠沒被取消。

use std::future::Future;

use wbf_sdk::task_stop::StopSignal;

use crate::error::{CoreError, CoreErrorKind};

tokio::task_local! {
    /// 只有 [`run_as_job`] 設得了它，所以「現在是哪個工作」永遠只有一個答案。
    static JOB: u64;
    /// 同上：這個工作的取消開關。
    static CANCEL: StopSignal;
}

/// 把一個工作標上 id 與取消開關跑。裡面（不管多深）發出來的事件都會帶這個 `job`，問 [`check_cancelled`] 的都看這個開關。
///
/// Args:
///     id: 呼叫端自己的編號，daemon 用的是 RPC 請求的 `id`, example: 7
///     cancel: 這個請求的取消開關；不能取消的給 `StopSignal::never()`
///     work: 要跑的 future
/// Return:
///     work 的回傳值，原樣。
pub async fn run_as_job<F: Future>(id: u64, cancel: StopSignal, work: F) -> F::Output {
    CANCEL.scope(cancel, JOB.scope(id, work)).await
}

/// 長工作在安全點問一次。
///
/// Return:
///     Ok(())           繼續（含不在任何可取消的工作裡）
///     Err(Cancelled)   這個請求被取消了：收攤、把這個錯原樣往上回
pub fn check_cancelled() -> Result<(), CoreError> {
    let cancelled = CANCEL.try_with(StopSignal::is_requested).unwrap_or(false);
    if cancelled {
        return Err(CoreError::new(CoreErrorKind::Cancelled, "cancelled"));
    }
    Ok(())
}

/// 讀的那一頭就是取消的安全點：上傳讀下一塊、匯出讀下一段之前（/docs/design/daemon/task-supervision.md §2.2）。
/// 取消之後下一次 `read` 回錯，呼叫端走它本來的錯誤路收攤（上傳的狀態檔留著可續、匯出的半成品刪掉），再用 [`check_cancelled`] 把錯換成 `Cancelled`。
/// 開關在建它的那一刻從 task-local 拿：之後搬進 `spawn_blocking` 也看得到。
pub(crate) struct ReadUntilCancelled<R> {
    inner: R,
    cancel: Option<StopSignal>,
}

impl<R> ReadUntilCancelled<R> {
    /// 在請求自己的 task 上建（不在可取消的工作裡就永遠讀得下去）。
    pub(crate) fn new(inner: R) -> ReadUntilCancelled<R> {
        ReadUntilCancelled {
            inner,
            cancel: CANCEL.try_with(StopSignal::clone).ok(),
        }
    }
}

impl<R: std::io::Read> std::io::Read for ReadUntilCancelled<R> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        if self.cancel.as_ref().is_some_and(StopSignal::is_requested) {
            return Err(std::io::Error::other("cancelled"));
        }
        self.inner.read(buffer)
    }
}

impl<R: std::io::Seek> std::io::Seek for ReadUntilCancelled<R> {
    fn seek(&mut self, position: std::io::SeekFrom) -> std::io::Result<u64> {
        self.inner.seek(position)
    }
}

/// Return:
///     Some(u64)  現在在某個 [`run_as_job`] 裡面
///     None       不在（CLI 直接叫、或背景工作）
pub fn current() -> Option<u64> {
    JOB.try_with(|id| *id).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_job_id_reaches_code_that_never_heard_of_it() {
        // 「深處」的函數：它不知道 job 是什麼，只是發事件的那一層會去問。
        async fn deep_inside() -> Option<u64> {
            tokio::task::yield_now().await;
            current()
        }
        assert_eq!(current(), None, "外面沒有工作");
        assert_eq!(
            run_as_job(7, StopSignal::never(), deep_inside()).await,
            Some(7)
        );
        assert_eq!(current(), None, "出來就沒了");
    }

    /// 兩個工作同時跑，各自看到自己的 id（**不**會互相汙染）。
    #[tokio::test]
    async fn two_jobs_side_by_side_do_not_mix() {
        let (one, other) = tokio::join!(
            run_as_job(1, StopSignal::never(), async {
                tokio::task::yield_now().await;
                current()
            }),
            run_as_job(2, StopSignal::never(), async {
                tokio::task::yield_now().await;
                current()
            })
        );
        assert_eq!((one, other), (Some(1), Some(2)));
    }

    /// ⚠️ 釘住那個限制：`spawn` 出去的 task **收不到**這個標記。
    /// 它會紅的那天，表示有人改了 tokio 的語意，或改了我們的假設 —— 兩種都該停下來看。
    #[tokio::test]
    async fn a_spawned_task_does_not_inherit_the_job() {
        let inside = run_as_job(9, StopSignal::never(), async {
            tokio::spawn(async { current() }).await.unwrap()
        })
        .await;
        assert_eq!(inside, None, "spawn 出去就不在那個工作裡了");
    }

    /// 取消開關跟著工作走：叫停之前 `check_cancelled` 是 Ok、之後是 `Cancelled`；不在工作裡的永遠不是。
    #[tokio::test]
    async fn the_cancel_switch_reaches_code_that_never_heard_of_it() {
        use wbf_sdk::task_stop::StopSwitch;
        assert!(check_cancelled().is_ok(), "外面沒有工作");
        let (switch, signal) = StopSwitch::new();
        let outcome = run_as_job(3, signal, async {
            let before = check_cancelled().is_ok();
            switch.request_stop();
            tokio::task::yield_now().await;
            (before, check_cancelled().map_err(|error| error.kind))
        })
        .await;
        assert_eq!(outcome, (true, Err(CoreErrorKind::Cancelled)));
    }

    /// 上傳、匯出讀檔的那一頭：取消之前讀得到、之後讀不下去；開關是建它時拿的（搬進 blocking 執行緒也一樣）；不在可取消的工作裡永遠讀得下去。
    #[tokio::test]
    async fn a_reader_stops_reading_once_the_job_is_cancelled() {
        use std::io::Read;
        use wbf_sdk::task_stop::StopSwitch;
        let (switch, signal) = StopSwitch::new();
        let mut reader = run_as_job(4, signal, async {
            ReadUntilCancelled::new(std::io::Cursor::new(vec![1u8; 8]))
        })
        .await;
        let mut buffer = [0u8; 4];
        assert_eq!(reader.read(&mut buffer).unwrap(), 4);
        switch.request_stop();
        let after = tokio::task::spawn_blocking(move || reader.read(&mut buffer).is_err())
            .await
            .unwrap();
        assert!(after, "cancelled: the next read fails");
        let mut outside = ReadUntilCancelled::new(std::io::Cursor::new(vec![1u8; 4]));
        assert_eq!(outside.read(&mut [0u8; 4]).unwrap(), 4);
    }
}

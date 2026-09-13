//! 「現在跑的是哪一個工作？」—— 讓事件說得出它屬於誰（rpc-spec §4 的 `progress.id`）。
//!
//! ## 問題
//!
//! core 的事件是**廣播**的（`event.rs`）：所有訂閱者都收到同一串。可是 daemon 同時可能有三個
//! 長工作在跑（一個上傳、一個 `sync.recent`、一個下載），前端要知道**每一則進度是哪一個請求的**
//! —— 而 core 完全不知道「請求」這種東西。
//!
//! ## 為什麼不是「多一個參數」
//!
//! 最直覺的做法是每個長工作的方法多收一個 `job: u64`。🚫 不做，因為那要改十幾個公開簽名、
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
//! 呼叫者的 task 上跑（進度回呼是同步的），所以沒問題；🚫 但哪天有人在 core 裡 `spawn` 一個
//! 會發進度的 task，那些事件就會變成無主的（`job: None`）——那時要嘛把 id 明確傳進去，
//! 要嘛在那個 task 外面再包一層 `run_as_job`。
//!
//! 📎 `None` 不是錯誤：CLI 直接叫 core（沒有請求 id）就是 `None`，daemon 的背景工作也是。

use std::future::Future;

tokio::task_local! {
    /// 只有 [`run_as_job`] 設得了它，所以「現在是哪個工作」永遠只有一個答案。
    static JOB: u64;
}

/// 把一個工作標上 id 跑。裡面（不管多深）發出來的事件都會帶這個 `job`。
///
/// Args:
///     id: 呼叫端自己的編號，daemon 用的是 RPC 請求的 `id`, example: 7
///     work: 要跑的 future
/// Return:
///     work 的回傳值，原樣。
pub async fn run_as_job<F: Future>(id: u64, work: F) -> F::Output {
    JOB.scope(id, work).await
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
        assert_eq!(run_as_job(7, deep_inside()).await, Some(7));
        assert_eq!(current(), None, "出來就沒了");
    }

    /// 兩個工作同時跑，各自看到自己的 id（🚫 不會互相汙染）。
    #[tokio::test]
    async fn two_jobs_side_by_side_do_not_mix() {
        let (one, other) = tokio::join!(
            run_as_job(1, async {
                tokio::task::yield_now().await;
                current()
            }),
            run_as_job(2, async {
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
        let inside = run_as_job(9, async {
            tokio::spawn(async { current() }).await.unwrap()
        })
        .await;
        assert_eq!(inside, None, "spawn 出去就不在那個工作裡了");
    }
}

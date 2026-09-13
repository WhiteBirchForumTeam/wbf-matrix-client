//! core 往外講話的唯一管道（architecture-v2 §7：**事件用 channel，不用回呼引用**）。
//!
//! 為什麼不是回呼：回呼綁著呼叫端的生命週期，而 core 之後可能被 RPC 包住（呼叫端在
//! 另一個程序）或被 uniffi 包住（呼叫端在 JVM 裡）。跨那兩條邊界都沒有「借一個 closure
//! 給你」這種東西，channel 有。
//!
//! 🚫 core **不印任何東西**。`eprintln!` 對 rpc-cli 是對的，對 daemon 是把訊息丟進虛空
//! （沒有人在看那個 stderr），對 Android 更是。誰要顯示、顯示成什麼樣，是前端的事（§3）。

use tokio::sync::broadcast;

/// 廣播佇列的深度。慢的訂閱者會收到 `RecvError::Lagged` 而不是把 core 拖住——
/// ⚠️ 跟 server 那邊推送的取捨一樣：**事件掉了可以補**（重新查一次狀態），
/// 讓 core 卡在一個讀得慢的前端上不行。
const EVENT_QUEUE: usize = 256;

/// core 發生的事。
///
/// ⚠️ 每個 variant 的欄位都要是**可序列化的簡單型別**：它們會變成 RPC 的推播訊息
/// （§4.6 的「沒有 `id` 的請求」）。🚫 不要在這裡放 handle、路徑以外的 `PathBuf`、
/// 或任何帶秘密的東西。
///
/// 📎 有 `Serialize`／`Deserialize`：daemon 那層要把它原樣送過 RPC，而**現在**補比
/// 之後補便宜（PR #24 審查 rumia🟡2／salvia🟡2）。
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum CoreEvent {
    /// 一句給人看的話。⚠️ 措辭是**給人看的**，🚫 前端不要拿它做邏輯判斷
    /// （要判斷就看 [`CoreEvent::Progress`] 的數字，或 method 的回傳值）。
    Note {
        /// 哪一個工作發的（[`crate::job::run_as_job`] 標的）。`None` ＝ 不在任何工作裡。
        job: Option<u64>,
        text: String,
    },
    /// 長工作的進度。⭐ 有數字，前端可以畫進度條，🚫 不必去 parse 一句話。
    Progress {
        job: Option<u64>,
        /// 已經完成多少（單位由那個工作決定：塊、byte、事件…；`note` 講得出是什麼）。
        done: u64,
        /// 總共多少。**串流的時候不知道，就是 `None`**，🚫 不要填 0 假裝知道。
        total: Option<u64>,
        /// 給人看的一句話，例如 "chunk 3/10"。
        text: String,
    },
    /// 收到一則訊息（`watch`、或 daemon 的上游會話，architecture-v2 §6.1）。
    /// 串流的東西走事件，🚫 不等收齊再一次回 ——`watch tail` 永遠不會「收齊」。
    Message {
        /// **哪個帳號的**。⚠️ 事件是每個帳號一組的（architecture-v2 §6.1），
        /// 所以每個帳號相關的事件都要說得出是誰的，🚫 不能讓前端猜。
        user: String,
        message: Box<wbf_sdk::chat::Message>,
    },
    /// 這個帳號跟它的 homeserver 之間的狀態變了（architecture-v2 §6.1）。
    SyncState {
        user: String,
        state: SyncState,
        /// 房間事件的水位；不知道就 `None`。
        cg_seq: Option<i64>,
    },
}

/// 一個帳號跟它的 homeserver 之間現在是什麼狀態（rpc-spec §4 的 `sync.state`）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncState {
    /// 連上了，但還在追歷史（`Recent` 還沒拉完）。
    CatchingUp,
    /// 追平了：現在收到的都是新的。
    CaughtUp,
    /// 斷了。⚠️ 這是**那一個帳號**的連線斷了，🚫 不代表 daemon 有問題。
    Disconnected,
}

/// core 內部拿來發事件的那一端。
///
/// 🚫 **crate 內部限定**：`progress` 收 `impl Into<String>`，而 §7 明文說公開介面上
/// 不要有 `impl Trait`。前端要聽事件走 [`Core::subscribe`]，拿到的是 receiver
/// ——那個形狀跨得過 RPC 與 uniffi（PR #24 審查 rumia🟡1／salvia🟡1）。
///
/// 📎 `broadcast` 而不是 `mpsc`：允許多條連線各自訂閱（§4.7「允許多條連線，每條都平等」），
/// 而且**沒有訂閱者時發送是零成本的**——rpc-cli 在 `--quiet` 下就是這種情況。
pub(crate) struct EventSink {
    sender: broadcast::Sender<CoreEvent>,
}

impl EventSink {
    pub(crate) fn new() -> EventSink {
        EventSink {
            sender: broadcast::channel(EVENT_QUEUE).0,
        }
    }

    /// 訂閱之後的事件。⚠️ 訂閱**之前**發生的收不到——這跟 server 的推送同一條規矩：
    /// 推送是「不用輪詢」，不是「保證看得到全部」。
    pub(crate) fn subscribe(&self) -> broadcast::Receiver<CoreEvent> {
        self.sender.subscribe()
    }

    /// 發一個事件。沒有訂閱者就是 no-op，🚫 不當成錯誤。
    pub(crate) fn emit(&self, event: CoreEvent) {
        let _ = self.sender.send(event);
    }

    /// 發一句給人看的話。core 裡面最常發的就是這個。
    ///
    /// ⚠️ `job` 是**自動**填的（[`crate::job::current`]）：呼叫端🚫 不用管，也不該管 ——
    /// 它是「現在這個 async 工作是誰」的答案，而那個答案只有跑它的人（daemon）知道。
    pub(crate) fn progress(&self, message: impl Into<String>) {
        self.emit(CoreEvent::Note {
            job: crate::job::current(),
            text: message.into(),
        });
    }

    /// 發一個有數字的進度。
    ///
    /// Args:
    ///     done: 已完成, example: 3
    ///     total: 總數；不知道就 None, example: Some(10)
    ///     text: 給人看的一句話, example: "chunk 3/10"
    pub(crate) fn progress_of(&self, done: u64, total: Option<u64>, text: impl Into<String>) {
        self.emit(CoreEvent::Progress {
            job: crate::job::current(),
            done,
            total,
            text: text.into(),
        });
    }
}

impl Default for EventSink {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_subscriber_gets_what_is_emitted_after_it_subscribed() {
        let sink = EventSink::new();
        // 訂閱之前的收不到。
        sink.progress("before");
        let mut rx = sink.subscribe();
        sink.progress("after");
        assert_eq!(
            rx.recv().await.unwrap(),
            CoreEvent::Note {
                job: None,
                text: "after".into()
            }
        );
    }

    #[tokio::test]
    async fn emitting_with_nobody_listening_is_not_an_error() {
        // rpc-cli 在 --quiet 下就是這樣：core 不該因為沒人聽就失敗。
        let sink = EventSink::new();
        sink.progress("into the void");
        let mut rx = sink.subscribe();
        sink.progress("now someone is here");
        assert_eq!(
            rx.recv().await.unwrap(),
            CoreEvent::Note {
                job: None,
                text: "now someone is here".into()
            }
        );
    }
}

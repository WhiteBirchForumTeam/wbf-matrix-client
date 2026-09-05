//! 聊天模型（`docs/design/chat-model.md` §2）與 `ChatBackend` trait。
//!
//! 這裡只有資料與契約，沒有 Matrix：`matrix_sdk::Room`、ruma 的型別不出現在這個檔（plan-v1 §7.2）。
//! 第一個實作在 `backend/matrix_sdk.rs`；之後自己的 WS 協定是同一個 trait 的另一個實作。
//!
//! 與 chat-model.md 的差異（第 3 步先做的縮小版，文件那邊同步標了）：
//! - id 用 `String`，不另外包 newtype；對外仍是不透明字串。
//! - `SystemEvent` 先用 `event_type` 加一行文字，不逐種列 enum。
//! - `watch` 是 callback 而不是 `Stream`：sync 迴圈在 backend 手上，CLI 的 tail／wait／once 用回傳值控制。

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::chunk_block::ChunkedBlock;
use crate::error::SdkError;

/// 高層的分類，從 room 的事實推出來（chat-model §2.1、§3.1、§3.2）；底層永遠是 room。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConversationKind {
    Direct,
    Group,
    Channel,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Conversation {
    pub id: String,
    pub kind: ConversationKind,
    pub name: Option<String>,
    pub topic: Option<String>,
    /// false 是例外，UI 要標；送檔案前要警告（約定 §5.1）。
    pub encrypted: bool,
    pub member_count: u64,
    /// Matrix 的 power level 照給（chat-model §2.4）。
    pub my_power_level: i64,
    /// 從 power levels 算好的結論，UI 直接用。
    pub can_send_message: bool,
    /// `kind == Direct` 才有。
    pub direct_peer: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attachment {
    pub mxc: String,
    /// 約定 §5 的區塊，含 key；就是 manifest 的 `block`。
    pub block: ChunkedBlock,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MessageKind {
    Text {
        body: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        formatted_html: Option<String>,
    },
    /// 我們的分塊檔（`msgtype: org.wbftw.wbfuwunel.file`）。
    File {
        attachment: Attachment,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        caption: Option<String>,
    },
    /// redaction 之後的骨架。
    Deleted {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    /// 誰加入、改名、改權限…；`line` 是給人看的一行。
    System { event_type: String, line: String },
    /// 認不得的事件：照印 type，不丟（chat-model §3.4）。解不開的加密事件也走這裡，`Message.decrypted` 是 `Some(false)`。
    Unsupported {
        event_type: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        body: Option<String>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reaction {
    pub key: String,
    pub by: Vec<String>,
}

/// 一則訊息，也是 CLI 規格 §3.4.1 印的形狀。順序不在這裡：照 backend 交出來的順序（chat-model §4.3）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    pub id: String,
    pub conversation: String,
    pub sender: String,
    /// `origin_server_ts`，毫秒；只當顯示用的時間。
    pub sent_at: u64,
    #[serde(flatten)]
    pub kind: MessageKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<String>,
    /// Some = 被改過，`kind` 已是最新版。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edited_by: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reactions: Vec<Reaction>,
    /// None = 本來就不是加密事件；Some(false) 帶 `undecryptable_reason`（chat-model §2.3）。
    pub decrypted: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub undecryptable_reason: Option<String>,
    /// server 發的 room 內連續序號（chat-model §4.3）；非 fork server 的 room 是 None，呼叫者顯式判斷。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub r_seq: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub g_seq: Option<i64>,
}

/// `history` 的一頁：`next` 是 None 表示到頭了（CLI 規格 §3.4.1）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Page {
    pub events: Vec<Message>,
    pub next: Option<String>,
}

/// watch 流的一則（chat-model §4.2 的縮小版：第 3 步只有新訊息與房間層的變化）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "update", rename_all = "snake_case")]
pub enum Update {
    /// Box：`Message` 幾百 byte，其他變體只有一個 id（clippy large_enum_variant）。
    NewMessage(Box<Message>),
    ConversationJoined {
        id: String,
    },
    ConversationLeft {
        id: String,
    },
}

/// callback 回的：繼續等，還是停。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WatchControl {
    Continue,
    Stop,
}

/// `watch` 結束後給呼叫者的：下次 `--since` 用的 token（CLI 規格 §3.4.2）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WatchEnd {
    pub since: String,
    /// 是 callback 說停的（true），還是 deadline 到了（false）。
    pub stopped_by_callback: bool,
}

/// 聊天的動作（chat-model §2.6 第 3 步的子集）。上傳不在這裡：那是 `WbfClient` 的事，與 backend 無關。
#[allow(async_fn_in_trait)]
pub trait ChatBackend {
    async fn conversations(&self) -> Result<Vec<Conversation>, SdkError>;

    async fn conversation(&self, id: &str) -> Result<Conversation, SdkError>;

    /// 歷史，從最新往回；`before` 接上一頁的 `next`。過濾在呼叫者端（CLI 規格 §3.4.1）。
    async fn history(&self, id: &str, before: Option<&str>, limit: u32) -> Result<Page, SdkError>;

    /// Return:
    ///     Ok(String)   event_id
    async fn send_text(&self, id: &str, body: &str) -> Result<String, SdkError>;

    /// 送約定 §5 的事件。⚠️ 附件宣告（約定 §5.2）目前沒有路可帶：matrix-sdk 不能在送訊息的請求加 header，
    /// server 的 `Event/Send` 也還是提案；實作要在回傳前把這件事講清楚（見 `backend/matrix_sdk.rs`）。
    async fn send_file(
        &self,
        id: &str,
        attachment: &Attachment,
        caption: Option<&str>,
    ) -> Result<String, SdkError>;

    /// 從 `since` 起等新事件，每一則叫一次 `on_update`；callback 回 `Stop`、或 `deadline` 到了就結束。
    /// `since` 是 None 就從「現在」起：實作先對齊位置、不吐舊事件。
    async fn watch(
        &self,
        since: Option<&str>,
        deadline: Option<Duration>,
        on_update: &mut dyn FnMut(Update) -> WatchControl,
    ) -> Result<WatchEnd, SdkError>;
}

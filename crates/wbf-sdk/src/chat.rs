//! 聊天模型（`/docs/design/rooms/chat-model.md` §2）與 `ChatBackend` trait。
//!
//! 這裡只有資料與契約，沒有 Matrix：`matrix_sdk::Room`、ruma 的型別不出現在這個檔（/docs/design/overview/architecture-v2.md §8）。
//! 第一個實作在 `backend/matrix_sdk.rs`；之後自己的 WS 協定是同一個 trait 的另一個實作。
//!
//! 與 /docs/design/rooms/chat-model.md §2 模型的差異（清單在同一份文件 §6）：
//! - id 用 `String`，不另外包 newtype；對外仍是不透明字串。
//! - `SystemEvent` 先用 `event_type` 加一行文字，不逐種列 enum。
//! - `watch` 是 callback 而不是 `Stream`：sync 迴圈在 backend 手上，CLI 的 tail／wait／once 用回傳值控制。

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::chunk_block::ChunkedBlock;
use crate::error::SdkError;
use crate::incoming::{EventPage, IncomingEvent};

/// 高層的分類，從 room 的事實推出來（/docs/design/rooms/chat-model.md §2.1、§3.1、§3.2）；底層永遠是 room。
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
    /// false 是例外，UI 要標；送檔案前要警告（/docs/design/media/wbf-client-convention-for-chunk.md §5.1）。
    pub encrypted: bool,
    pub member_count: u64,
    /// Matrix 的 power level 照給（/docs/design/rooms/chat-model.md §2.4）。
    pub my_power_level: i64,
    /// 從 power levels 算好的結論，UI 直接用。
    pub can_send_message: bool,
    /// `kind == Direct` 才有。
    pub direct_peer: Option<String>,
}

/// 房間列表的一列（`room.list`，/docs/design/rooms/chat-model.md §2.1）：本地知道多少給多少，不知道的是 `null`。
/// 列表只問 server「加入了哪些房」；每一間的樣子是 UI 對看得到的房間叫 `room.get` 拿回來、寫進本地的（維護者 2026-10-05）。
/// 欄位跟 [`Conversation`] 同名；`refreshed_at` 是 `null` ＝ 這一間還沒拿過（`name` 的 `null` 這時是「不知道」，🚫 是「沒名字」）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoomListEntry {
    pub id: String,
    pub kind: Option<ConversationKind>,
    pub name: Option<String>,
    pub topic: Option<String>,
    pub encrypted: Option<bool>,
    pub member_count: Option<u64>,
    pub my_power_level: Option<i64>,
    pub can_send_message: Option<bool>,
    pub direct_peer: Option<String>,
    /// 這一間的樣子是什麼時候拿的（Unix 毫秒）。
    pub refreshed_at: Option<u64>,
}

impl RoomListEntry {
    /// Args:
    ///     room_id: example: "!abc:localhost"
    /// Return:
    ///     RoomListEntry  只有 id，其他都是 null（加入了、還沒拿過）
    pub fn unknown(room_id: &str) -> RoomListEntry {
        RoomListEntry {
            id: room_id.to_string(),
            kind: None,
            name: None,
            topic: None,
            encrypted: None,
            member_count: None,
            my_power_level: None,
            can_send_message: None,
            direct_peer: None,
            refreshed_at: None,
        }
    }

    /// Args:
    ///     conversation: 拿回來的那一間
    ///     refreshed_at: 什麼時候拿的；沒存進本地的（`sync=server`）給 None, example: Some(1_700_000_000_000)
    pub fn from_conversation(
        conversation: Conversation,
        refreshed_at: Option<u64>,
    ) -> RoomListEntry {
        RoomListEntry {
            id: conversation.id,
            kind: Some(conversation.kind),
            name: conversation.name,
            topic: conversation.topic,
            encrypted: Some(conversation.encrypted),
            member_count: Some(conversation.member_count),
            my_power_level: Some(conversation.my_power_level),
            can_send_message: Some(conversation.can_send_message),
            direct_peer: conversation.direct_peer,
            refreshed_at,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attachment {
    pub mxc: String,
    /// /docs/design/media/wbf-client-convention-for-chunk.md §5 的區塊，含 key；就是 manifest 的 `block`。
    pub block: ChunkedBlock,
}

/// 標準 Matrix 的附件（/docs/design/rpc-specs/data-plane.md §7.1 的 `kind` 2、3）。從事件內容來，🚫 是分塊。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MatrixAttachment {
    /// example: "m.image"
    pub msgtype: String,
    /// example: "mxc://matrix.org/AbCdEf"
    pub mxc: String,
    /// `MatrixEncrypted`（有 `file`）或 `MatrixPlain`（只有 `url`）
    pub kind: crate::media_kind::MediaKind,
    /// `filename`，沒有就用 `body`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// `info.mimetype`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mimetype: Option<String>,
    /// `info.size`（明文的大小；事件沒給就是不知道）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    /// `kind` 2：事件裡的 `EncryptedFile` 原樣（`v`、`key`、`iv`、`hashes`、`url`），**含金鑰**；`kind` 3 沒有
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<serde_json::Value>,
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
    /// 標準 Matrix 的附件（`m.file`／`m.image`／`m.video`／`m.audio`，帶 `url` 或 `file`，/docs/design/media/media-download.md §12）。
    /// 別的 Matrix client（Element…）送的都是這種。
    MatrixFile {
        attachment: MatrixAttachment,
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
    /// 解不開的加密事件：跟 `Deleted` 一樣是明確的記號，UI 直接渲染（維護者 2026-09-14）。
    /// `Message.decrypted` 是 `Some(false)`，原因在 `undecryptable_reason`。
    Undecryptable,
    /// 這則被 edit 過，但目前的那個 edit 這個帳號**還沒同步到**：本地手上的版本過時了，🚫 原文與 edit 內容都不給，
    /// 同步之後就會是新版本（維護者 2026-09-14）。跟 `Deleted`、`Undecryptable` 一樣是 UI 直接渲染的記號。
    Outdated,
    /// 認不得的事件：照印 type，不丟（/docs/design/rooms/chat-model.md §3.4）。
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

/// 一則訊息，也是 /docs/design/rpc-specs/wbf-cli-spec.md §3.4.1 印的形狀。順序不在這裡：照 backend 交出來的順序（/docs/design/rooms/chat-model.md §4.3）。
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
    /// None = 本來就不是加密事件；Some(false) 帶 `undecryptable_reason`（/docs/design/rooms/chat-model.md §2.3）。
    pub decrypted: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub undecryptable_reason: Option<String>,
    /// server 發的 room 內連續序號（/docs/design/rooms/chat-model.md §4.3）；非 fork server 的 room 是 None，呼叫者顯式判斷。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub r_seq: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub g_seq: Option<i64>,
}

/// watch 流的一則（/docs/design/rooms/chat-model.md §4.2 的子集：只有新訊息與房間層的變化）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "update", rename_all = "snake_case")]
pub enum Update {
    /// 一個房間新到的事件，**原樣、照上游順序**（關係事件也在裡面）：寫庫的人要原樣（/docs/design/messages/edits-and-redactions.md），
    /// 要顯示的人自己折（`event_json::messages_from_incoming`）。
    NewEvents {
        conversation: String,
        events: Vec<IncomingEvent>,
    },
    /// 房間層的變化只有這一個；「新加入的房間」用 `conversations()` 看，watch 還不推。
    ConversationLeft { id: String },
}

/// callback 回的：繼續等，還是停。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WatchControl {
    Continue,
    Stop,
}

/// `watch` 結束後給呼叫者的：下次 `--since` 用的 token（/docs/design/rpc-specs/wbf-cli-spec.md §3.4.2）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WatchEnd {
    pub since: String,
    /// 是 callback 說停的（true），還是 deadline 到了（false）。
    pub stopped_by_callback: bool,
}

/// 聊天的動作（/docs/design/rooms/chat-model.md §2.6 的子集）。上傳不在這裡：那是 `WbfClient` 的事，與 backend 無關。
#[allow(async_fn_in_trait)]
pub trait ChatBackend {
    async fn conversations(&self) -> Result<Vec<Conversation>, SdkError>;

    async fn conversation(&self, id: &str) -> Result<Conversation, SdkError>;

    /// 歷史，從最新往回；`before` 接上一頁的 `next`。過濾在呼叫者端（/docs/design/rpc-specs/wbf-cli-spec.md §3.4.1）。
    ///
    /// 🚨 `before` 與 `next` 都是 **`event_id`**（這一頁最舊那則），🚫 不是 server 的翻頁 token ——
    /// UI 不分 server 是誰，一律拿手上最舊那則往回問（/docs/design/rooms/chat-model.md §4.3、/docs/design/rpc-specs/rpc-spec.md §3.3）。
    /// 回的是**原樣**的事件（/docs/design/messages/edits-and-redactions.md：原始事件存庫、顯示另外折）。
    async fn history(
        &self,
        id: &str,
        before: Option<&str>,
        limit: u32,
    ) -> Result<EventPage, SdkError>;

    /// Return:
    ///     Ok(String)   event_id
    async fn send_text(&self, id: &str, body: &str) -> Result<String, SdkError>;

    /// 送 /docs/design/media/wbf-client-convention-for-chunk.md §5 的事件。⚠️ 附件宣告（/docs/design/media/wbf-client-convention-for-chunk.md §5.2）在這條路帶不出去：matrix-sdk 不能在送訊息的請求加 header；
    /// 實作要在回傳前把這件事講清楚（見 `backend/matrix_sdk.rs`）。wbf 帳號不走這個 trait，它走 `Event/Send`、有宣告。
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
        on_update: &mut (dyn FnMut(Update) -> WatchControl + Send),
    ) -> Result<WatchEnd, SdkError>;
}

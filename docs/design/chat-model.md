# 聊天模型與房間設計：站在 Matrix 的高度，用 Telegram 的形狀

> 狀態：草案，2026-09-05，等維護者訂正。
> 前提：[plan-v1.md](plan-v1.md) §7.2 —— 上游 `matrix-sdk` 是可拆的零件。這份文件定的是**我們的模型**；
> 「現在怎麼接到 Matrix」只是第一個 backend 的接法，之後換成自己的協定時，模型不動、只換接法。
> Telegram 的部分是憑印象寫的（維護者明說接受）；Matrix 的部分照規格與 `vendor/matrix-rust-sdk` 的實作。
> 🚨 標 **[審]** 的地方會 breaking Matrix 兼容或 Matrix 做不到，要維護者定案（plan-v1 §7.2）。

## 0. 一句話

使用者看到的是 Telegram 的形狀：**對話**（私訊、群組、頻道）、**訊息**（文字、檔案、系統）、回覆、編輯、刪除、已讀、置頂。
底下是 Matrix 的骨架：對話是 room、訊息是 event、身份是 mxid 加裝置、加密是 Megolm。
**模型只講我們的名詞**；Matrix 的名詞只出現在 §3 的對照表與 adapter 裡。

## 1. 兩邊的世界觀，先對齊名詞

| 概念 | Matrix | Telegram（印象） | 我們叫它 |
|---|---|---|---|
| 一個聊天 | room：一個有狀態機的事件 DAG，DM 也是 room | chat：private／group／supergroup／channel 四種 | **Conversation**，帶 `kind` |
| 誰 | `@user:server`（mxid），一人多裝置，每裝置一組金鑰 | user id 加 @username，帳號綁手機 | **Peer**（人）＋ **Device**（裝置，E2EE 用） |
| 一則 | event：有 `type`、`content`、`sender`、`origin_server_ts`，id 是雜湊 | message：per-chat 遞增的整數 id | **Message**，id 用 Matrix 的 event_id，順序另算（§4.3） |
| 訊息內容 | `m.room.message` 加 `msgtype`（text／file／image…），其他 type 是 state 或系統 | 訊息帶 media 欄位 | **MessageKind**（§2.3） |
| 回覆 | `m.relates_to.m.in_reply_to` | reply_to_message_id | `reply_to: Option<MessageId>` |
| 編輯 | 新事件帶 `m.relates_to.rel_type = m.replace`，原事件不變 | 原訊息被改、標 edited | `edited: Option<Edit>`，adapter 把 replace 折進原訊息 |
| 刪除 | redaction：事件內容被清空，留骨架 | delete for me／for everyone | `Deleted`；for-me 是 **[審]**（§5） |
| 反應 | `m.annotation` 加 emoji key | reactions | `reactions: Vec<Reaction>` |
| 已讀 | 讀取收據 `m.read`（別人看得到）加 `m.fully_read`（只有自己） | 雙勾勾，per-chat 已讀到哪 | **ReadMarker**（自己）＋ **ReadReceipt**（別人） |
| 正在輸入 | `m.typing`（EDU，不進歷史） | typing 狀態 | `Typing`，只在 watch 流裡出現，不落地 |
| 誰能做什麼 | power levels：每個 event type 一個門檻 | admin／member／restricted | **Role** 三級（§2.4），adapter 對到 power level |
| 頻道 | 沒有原生概念；用 power levels 讓只有管理員能發 | channel：單向廣播、訂閱者看不到彼此 | `ConversationKind::Channel`，兼容的做法見 §3.2 |
| 置頂 | `m.room.pinned_events` state | pinned messages | `pinned: Vec<MessageId>` |
| 資料夾／封存 | `m.tag`（`m.favourite`、`m.lowpriority`）加 client 自己的 | folders、archive | **Tag**，第一版只做 favourite／archived |
| 歷史 | `/messages` 分頁，token 往回翻 | 一路往上捲 | `history(before)`（CLI 規格 §3.4.1 的 `read`） |
| 即時 | `/sync` 長輪詢，回全部房間的增量 | 推送 | **watch 流**（§4.2） |
| 加密 | Olm（裝置對裝置）＋ Megolm（房間金鑰），跨裝置簽章驗證 | 一般聊天 server 端可讀；secret chat 端到端、綁單一裝置 | **一律 E2EE 是預設**，非 E2EE 是例外要警告（約定 §5.1） |
| 檔案 | `m.file` 加 `file` 欄（AES-CTR）；整檔上傳 | 檔案 2 GB，串流播放 | 我們的分塊檔（約定規格書），`msgtype = org.wbftw.wbfuwunel.file` |
| 聯邦 | 有：room 可以跨 server | 沒有：單一平台 | 保留，能兼容盡量兼容（plan-v1 §7.2） |

Telegram 沒有而 Matrix 有、我們**要留**的：多裝置各自金鑰、裝置驗證、聯邦、房間 state（可查歷史誰改了名字）。
Matrix 沒有而 Telegram 有的，全部在 §5 列成 **[審]**。

## 2. 我們的模型（`wbf-sdk` 的 pub 型別）

只放資料，不放 Matrix 的東西。下面是 Rust 的形狀，欄位名就是 JSON 欄位名（CLI 印的就是它）。

### 2.1 對話

```rust
pub struct ConversationId(String);        // 現在 = room_id，之後可以是任何東西；對外是不透明字串

pub enum ConversationKind {
    Direct,       // 兩個人；一個對象只有一個 Direct（§3.1 怎麼保證）
    Group,        // 多人，大家都能發
    Channel,      // 單向：只有 Role::Admin 以上能發，其他人只能看
}

pub struct Conversation {
    pub id: ConversationId,
    pub kind: ConversationKind,
    pub name: Option<String>,             // Direct 沒有 name，顯示對方
    pub topic: Option<String>,
    pub avatar: Option<Attachment>,       // 之後
    pub encrypted: bool,                  // false 是例外，UI 要標
    pub member_count: u32,
    pub my_role: Role,
    pub last_message: Option<MessageSummary>,
    pub unread: Unread,                   // §2.5
    pub pinned: Vec<MessageId>,
    pub tags: Vec<Tag>,
    pub direct_peer: Option<PeerId>,      // kind == Direct 才有
}
```

### 2.2 人與裝置

```rust
pub struct PeerId(String);                // 現在 = mxid
pub struct Peer { pub id: PeerId, pub display_name: Option<String>, pub avatar: Option<Attachment> }
pub struct Member { pub peer: Peer, pub role: Role, pub joined_at: Option<Timestamp> }

pub struct DeviceId(String);
pub enum DeviceTrust { Verified, Unverified, Blocked }   // E2EE 的事，UI 顯示鎖頭用
```

### 2.3 訊息

```rust
pub struct MessageId(String);             // 現在 = event_id

pub struct Message {
    pub id: MessageId,
    pub conversation: ConversationId,
    pub sender: PeerId,
    pub sent_at: Timestamp,               // server 收到的時間（§4.3 講為什麼不能拿來排序）
    pub kind: MessageKind,
    pub reply_to: Option<MessageId>,
    pub edited: Option<Edit>,             // Some = 被改過，內容已是最新版
    pub reactions: Vec<Reaction>,
    pub decrypted: Option<bool>,          // None = 本來就不是加密事件；Some(false) 帶 undecryptable_reason
    pub undecryptable_reason: Option<String>,
    pub mentions_me: bool,
}

pub enum MessageKind {
    Text { body: String, formatted: Option<Formatted> },
    File { attachment: Attachment, caption: Option<String> },     // 我們的分塊檔；圖片影片也是 File，用 mimetype 分
    Deleted { by: PeerId, reason: Option<String> },
    System(SystemEvent),                  // 誰加入、改名、改權限…；UI 印成一行灰字
    Unsupported { event_type: String },   // 認不得的事件：照印 type，不丟
}

pub struct Attachment {
    pub mxc: String,
    pub name: Option<String>, pub mimetype: Option<String>, pub size: Option<u64>,
    pub block: ChunkedBlock,              // 約定 §5 的區塊，含 key；就是 manifest 的 block
}

pub struct Edit { pub at: Timestamp, pub by: PeerId }
pub struct Reaction { pub key: String, pub by: Vec<PeerId> }
pub struct Formatted { pub html: String }          // 第一版只收不產：我們送純文字，收到別人的 HTML 就帶著
pub enum SystemEvent { Joined(PeerId), Left(PeerId), Invited { who: PeerId, by: PeerId }, Kicked{..}, Banned{..},
                       NameChanged{..}, TopicChanged{..}, RoleChanged{..}, EncryptionEnabled, Pinned{..}, Other(String) }
```

### 2.4 角色：三級，不是 power level 的數字

```rust
pub enum Role { Owner, Admin, Member }   // Channel 的訂閱者也是 Member，只是不能發
```

Matrix 的 power level 是 0–100 的數字加每個事件類型一個門檻，太細。我們對外只有三級，adapter 負責對應（§3.3）。
要更細的（例如「可以邀請但不能踢」）**[審]**：是做進 Role 還是不做。

### 2.5 未讀

```rust
pub struct Unread { pub count: u32, pub mentions: u32, pub marker: Option<MessageId> }
```

`count` 是 marker 之後、不是自己發的、非 System 的訊息數；我們自己算，不信 server 的通知計數（各 server 算法不一）。

### 2.6 動作（`Backend` trait 的一半）

```rust
pub trait Backend {
    async fn conversations(&self) -> Result<Vec<Conversation>>;
    async fn conversation(&self, id: &ConversationId) -> Result<Conversation>;
    async fn members(&self, id: &ConversationId) -> Result<Vec<Member>>;
    async fn history(&self, id: &ConversationId, before: Option<&Cursor>, limit: u32) -> Result<Page<Message>>;
    async fn send_text(&self, id: &ConversationId, body: &str, reply_to: Option<&MessageId>) -> Result<MessageId>;
    async fn send_file(&self, id: &ConversationId, attachment: Attachment, caption: Option<&str>) -> Result<MessageId>;
    async fn edit(&self, msg: &MessageId, body: &str) -> Result<MessageId>;
    async fn delete(&self, msg: &MessageId, reason: Option<&str>) -> Result<()>;
    async fn react(&self, msg: &MessageId, key: &str) -> Result<()>;
    async fn mark_read(&self, id: &ConversationId, upto: &MessageId) -> Result<()>;
    async fn pin(&self, msg: &MessageId, pinned: bool) -> Result<()>;
    async fn create(&self, kind: ConversationKind, name: Option<&str>, invite: &[PeerId]) -> Result<ConversationId>;
    async fn invite / leave / kick / ban / set_role ...
    fn watch(&self) -> impl Stream<Item = Update>;      // §4.2
}
```

`send_file` 收的是 `Attachment`：上傳是 `wbf-sdk` 現有的 `WbfClient` 做的，**與 Backend 無關**。這條線畫在這裡是刻意的：
之後換自己的協定，上傳那半邊已經是我們的了。

## 3. 現在怎麼接到 Matrix（`backend/matrix_sdk.rs`）

### 3.1 Direct 的判定與唯一性

Matrix 沒有「DM」型別，只有慣例：建房時 `is_direct: true`，雙方的帳號資料 `m.direct` 記「這個人 ↔ 這些房」。
問題是一個人可以有好幾個 DM room（兩邊各建一個、或舊的沒退）。我們定：

- `kind == Direct` 的條件：`m.direct` 裡有它，**且**成員（含邀請中）剛好兩個。不滿足就當 Group，不猜。
- 同一個 peer 有多個 Direct：取最近有訊息的那個當「這個人的對話」，其他照列但 `direct_peer` 一樣填；UI 可以合併顯示。
  **不**自動退出多的那些（那是使用者的資料）。
- `create(Direct, invite=[p])`：先找既有的，有就回它，沒有才建（`is_direct: true`，加寫 `m.direct`）。這是 Telegram 的「一個人一個聊天」。

### 3.2 Channel：兼容的做法

Matrix 的做法是 power levels：`events_default: 50`（發訊息要 50）、預設成員 0。這樣其他 Matrix client 進來也是「看得到、發不出」，兼容。
我們再加一個 state 事件 `org.wbftw.wbfuwunel.conversation_kind` 內容 `{ "kind": "channel" }`，讓我們自己的 client 不用從 power level 反推。
沒有這個 state 的房，adapter 用 power level 猜：`events_default ≥ 50` 且我不是 admin → Channel；猜的結果標 `kind_inferred: true`。

Telegram 頻道「訂閱者看不到彼此」：Matrix 成員列表對所有成員可見，**做不到** → **[審]**：接受不同，還是 server 端 extension。

### 3.3 Role 對 power level

| Role | power level |
|---|---|
| Owner | 100（建房者） |
| Admin | 50–99 |
| Member | < 50 |

`set_role` 只寫 100／50／0 三個值。收到別的 client 設的 73，對外仍是 Admin。

### 3.4 訊息事件的對應

| 我們 | Matrix 事件 |
|---|---|
| `Text` | `m.room.message`，`msgtype: m.text`，`body`；有 `formatted_body` 就進 `Formatted` |
| `File` | `m.room.message`，`msgtype: org.wbftw.wbfuwunel.file`，區塊照約定 §5。**別人的 `m.file`／`m.image`（標準附件，AES-CTR）：第一版當 `Unsupported`，印 type 與 `body`**，下載標準附件是之後的事 |
| `reply_to` | `m.relates_to.m.in_reply_to.event_id`；`body` 不再塞引文（新規格已廢引文），`m.mentions` 照填 |
| `edited` | 收：`m.replace` 事件折進原訊息（adapter 做聚合）；送：`edit()` 發 `m.replace` |
| `Deleted` | 收：redacted 事件；送：`delete()` 發 redaction。**內容被清空是 server 行為，我們不能保留原文**（本地也不存，§7.1） |
| `reactions` | `m.reaction` 事件，`m.annotation`；adapter 聚合成 `key → Vec<PeerId>` |
| `System` | `m.room.member`、`m.room.name`、`m.room.topic`、`m.room.power_levels`、`m.room.encryption`、`m.room.pinned_events`… |
| `Unsupported` | 其他所有 type。**不丟**，這是 fail-safe：至少讓人看到「這裡有東西」 |

### 3.5 已讀

- `mark_read(upto)`：同時送 `m.read` 收據（別人看到雙勾勾）與 `m.fully_read`（自己的 marker）。
  Telegram 的已讀是給對方看的，Matrix 兩者分開；我們合成一個動作。要不要有「只更新自己、不讓對方知道」的模式 **[審]**（Matrix 有 `m.read.private`）。
- `Unread` 自己算：從 `marker` 之後數。

### 3.6 加密

- 建 Group／Direct 時預設 `m.room.encryption`（Megolm）。Channel 也加密：Telegram 的頻道是公開的不加密，我們的頻道是「E2EE 的單向群組」，這是本質差異，保留。
  公開頻道（任何人可加入、歷史公開）要不要不加密 **[審]**。
- 裝置驗證、cross-signing、金鑰備份：第一版只做「解得開就解、解不開標 `decrypted: false`」，驗證流程是第 4 步以後的事。
- 這一層是 `RoomCrypto` trait 的實作包 `OlmMachine`（plan-v1 §7.2）；adapter 呼叫的是 trait。

### 3.7 上游依賴清單（第 3 步 PR 要列的）

`matrix-sdk`（`Client`、`Room`、sync 迴圈、send queue、`m.direct` 處理）、`matrix-sdk-crypto`（透過 `RoomCrypto`）、`ruma`（事件型別，只在 adapter 內）。
**不用** `matrix-sdk-ui`（它的 `Timeline` 是給 UI 綁定用的，聚合邏輯我們自己做，理由 plan-v1 §7.2）。

## 4. 流：歷史與即時

### 4.1 歷史

`history(before, limit)` 回一頁加 `next: Option<Cursor>`，`None` 是到頭。Cursor 對外不透明（現在是 `/messages` 的 token）。
過濾（type、sender）在 client 端做（CLI 規格 §3.4.1 的理由）。

### 4.2 watch 流

```rust
pub enum Update {
    NewMessage(Message),
    MessageEdited { id: MessageId, message: Message },
    MessageDeleted { id: MessageId, conversation: ConversationId },
    ReactionsChanged { id: MessageId, reactions: Vec<Reaction> },
    ReadReceipt { conversation: ConversationId, peer: PeerId, upto: MessageId },
    Typing { conversation: ConversationId, peers: Vec<PeerId> },
    ConversationChanged(Conversation),     // 名稱、成員數、置頂、角色…任何一個變了就整份給
    ConversationJoined(Conversation), ConversationLeft(ConversationId),
    Invited { conversation: ConversationId, by: PeerId },
    DecryptionCaughtUp { conversation: ConversationId, ids: Vec<MessageId> },   // 之前解不開、現在拿到金鑰了
}
```

現在的實作：matrix-sdk 的 sync 迴圈 → adapter 把每個增量翻成 `Update`。CLI 的 `watch tail|wait|once` 就是消費這個流、只留一個 conversation 的（CLI 規格 §3.4.2）。
之後換自己的協定：server 推 pack，adapter 翻成同一個 `Update`，CLI／UI 不動。

### 4.3 順序：為什麼不能用時間戳排序

`sent_at` 是**發送者的 server**蓋的時間，聯邦下兩台 server 時鐘不同，排序會亂。Matrix 的真順序是 DAG 的拓樸序，`/sync` 與 `/messages` 回來的順序就是它。
我們定：**訊息的順序 = backend 交出來的順序**，`Message` 不帶序號；UI 與快取（之後）用「backend 給的順序」加 `sent_at` 當顯示用的時間。
Telegram 的遞增 message id 我們沒有 → 「跳到第 N 則」做不到，只有「跳到某個 id」與「翻到某個時間」。**[審]** 要不要在自己的協定裡加 per-conversation 序號。

## 5. Telegram 有、Matrix 沒有：全部要審

| 功能 | Matrix 能不能 | 建議 | |
|---|---|---|---|
| Delete for me（只在自己這邊消失） | 不能：redaction 是全體。本地不存（plan-v1 §7.1）所以現在連藏都藏不住 | 等本地快取有了再做成「本地隱藏」，不動 server | **[審]** |
| 自毀訊息（timer） | 沒有（MSC 有草案沒定案）。client 端計時刪除擋不住不配合的 client | 做成 extension：事件帶 `org.wbftw.wbfuwunel.expires_at`，我們的 client 到時本地隱藏並發 redaction；其他 client 看到永久訊息 | **[審]**：接受「只對我們的 client 有效」嗎 |
| 轉發（帶原作者） | 沒有轉發語意；只能複製內容 | 事件加 `org.wbftw.wbfuwunel.forwarded_from { peer, message }`，其他 client 看到普通訊息 | **[審]** |
| 頻道訂閱者互不可見 | 不能 | 接受不同，或 server extension 限制 `/members` | **[審]** |
| 公開頻道不加密、歷史公開可搜 | 可以：`history_visibility: world_readable` 加不開加密 | 頻道分「私密（加密）」「公開（不加密、world_readable）」兩種 | **[審]** |
| @username 搜人 | 有 user directory（`/user_directory/search`），但只搜同 server 與共同房間 | 用它，接受範圍限制 | 兼容 |
| 訊息序號、跳到第 N 則 | 沒有 | 之後自己的協定再加 | **[審]** |
| 已讀不通知對方 | 有 `m.read.private` | 提供開關 | 兼容 |
| 排程訊息、草稿同步 | 沒有 | 草稿只放記憶體（§7.1）；排程不做 | 不做 |
| 語音訊息、貼圖、投票 | 有 MSC（voice `m.audio` 加 `org.matrix.msc3245.voice`；投票 MSC3381） | 都是 `File` 或之後的 `MessageKind`；第 3 步不做 | 之後 |
| 超大群（十萬人） | Matrix 房間可以，但成員同步要 lazy load | adapter 一律 lazy load members | 兼容 |

## 6. 第 3 步的範圍（不是全部一次做）

1. `Backend` trait 與 `matrix_sdk` adapter；`conversations`、`conversation`、`history`、`send_text`、`send_file`、`watch`。
2. CLI：`rooms`（印 `Conversation`）、`send --text`、`send --file`（非加密對話警告並確認，約定 §5.1）、`watch`、`read`、`files`（CLI 規格 §3.4）。
3. `Message` 的聚合：edit 折進去、reaction 聚合、redaction 變 `Deleted`；`Unsupported` 不丟。
4. 加密：解得開就解，解不開標原因；`RoomCrypto` trait 立起來，實作包 `OlmMachine`。
5. **不做**：建房、邀請、角色、置頂、已讀送出、裝置驗證、標準附件下載。這些是第 3 步之後一個一個加。

## 7. 要維護者訂正的

1. §1 的名詞：Conversation／Peer／Message／Role 這幾個叫法可以嗎？
2. §2.4 Role 三級夠不夠。
3. §3.2 Channel 的兼容做法，與「訂閱者互不可見」做不到。
4. §3.6 頻道要不要分私密（加密）與公開（不加密）。
5. §4.3 沒有遞增序號，接不接受。
6. §5 每一列的 **[審]**。
7. §6 的範圍。

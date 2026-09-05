# 聊天模型與房間設計：站在 Matrix 的高度，用 Telegram 的形狀

> 狀態：2026-09-05 維護者訂正過一輪（標「維護者定」的是定案，標「再議」的還開著）。
> 前提：[plan-v1.md](plan-v1.md) §7.2 —— 上游 `matrix-sdk` 是可拆的零件。這份文件定的是**我們的模型**；
> 「現在怎麼接到 Matrix」只是第一個 backend 的接法，之後換成自己的協定時，模型不動、只換接法。
> Telegram 的部分是憑印象寫的（維護者明說接受）；Matrix 的部分照規格與 `vendor/matrix-rust-sdk` 的實作。
> 🚨 標 **[審]** 的地方會 breaking Matrix 兼容或 Matrix 做不到，要維護者定案（plan-v1 §7.2）。

## 0. 一句話

使用者看到的是 Telegram 的形狀：**對話**（私訊、群組、頻道）、**訊息**（文字、檔案、系統）、回覆、編輯、刪除、已讀、置頂。
底下是 Matrix 的骨架，**而且底層統一就是 ROOM**（維護者 2026-09-05 定）：DM 是兩個人的 room、group 是多人的 room、channel 是只有 owner 能講話的 room。
這是 Matrix 的標準模型，打掉它會讓兼容難辦；「這是什麼類型」在**高層**（UI）區分，底層不加任何東西。
**模型只講我們的名詞**（Conversation／Peer／Message，維護者同意）；Matrix 的名詞只出現在 §3 的對照表與 adapter 裡。

## 1. 兩邊的世界觀，先對齊名詞

| 概念 | Matrix | Telegram（印象） | 我們叫它 |
|---|---|---|---|
| 一個聊天 | room：一個有狀態機的事件 DAG，DM 也是 room | chat：private／group／supergroup／channel 四種 | **Conversation**，帶 `kind` |
| 誰 | `@user:server`（mxid），一人多裝置，每裝置一組金鑰 | user id 加 @username，帳號綁手機 | **Peer**（人）＋ **Device**（裝置，E2EE 用） |
| 一則 | event：有 `type`、`content`、`sender`、`origin_server_ts`，id 是雜湊 | message：per-chat 遞增的整數 id | **Message**，id 用 Matrix 的 event_id，順序另算（§4.3） |
| 訊息內容 | `m.room.message` 加 `msgtype`（text／file／image…），其他 type 是 state 或系統 | 訊息帶 media 欄位 | **MessageKind**（§2.3） |
| 回覆 | `m.relates_to.m.in_reply_to` | reply_to_message_id | `reply_to: Option<MessageId>` |
| 編輯 | 新事件帶 `m.relates_to.rel_type = m.replace`，原事件不變 | 原訊息被改、標 edited | `edited: Option<Edit>`，adapter 把 replace 折進原訊息 |
| 刪除 | redaction：事件內容被清空，留骨架 | delete for me／for everyone | `Deleted`（for everyone）；for-me 是本地標記，不動 server（§5，維護者定） |
| 反應 | `m.annotation` 加 emoji key | reactions | `reactions: Vec<Reaction>` |
| 已讀 | 讀取收據 `m.read`（別人看得到）加 `m.fully_read`（只有自己） | 雙勾勾，per-chat 已讀到哪 | **維持 Matrix 做法**，兩種 read：本地 offset（自己看的，進本地庫）與給遠端看的 read（要不要送是 UI 設定）（§3.5，維護者定） |
| 正在輸入 | `m.typing`（EDU，不進歷史） | typing 狀態 | `Typing`，只在 watch 流裡出現，不落地 |
| 誰能做什麼 | power levels：每個 event type 一個門檻 | admin／member／restricted | **維持 power level 模式**，不自己搞一套（§2.4，維護者定） |
| 頻道 | 沒有原生概念；用 power levels 讓只有管理員能發 | channel：單向廣播 | 底層 **do nothing**：UI 建「channel」時把 room 包裝成大家都沒權限發言、只有 owner 能發（§3.2，維護者定） |
| 置頂 | `m.room.pinned_events` state | pinned messages | `pinned: Vec<MessageId>` |
| 資料夾／封存 | `m.tag`（`m.favourite`、`m.lowpriority`）加 client 自己的 | folders、archive | **Tag**，第一版只做 favourite／archived |
| 歷史 | `/messages` 分頁，token 往回翻 | 一路往上捲 | `history(before)`（CLI 規格 §3.4.1 的 `read`） |
| 即時 | `/sync` 長輪詢，回全部房間的增量 | 推送 | **watch 流**（§4.2） |
| 加密 | Olm（裝置對裝置）＋ Megolm（房間金鑰），跨裝置簽章驗證 | 一般聊天 server 端可讀；secret chat 端到端、綁單一裝置 | **一律 E2EE 是預設**，非 E2EE 是例外要警告（約定 §5.1） |
| 檔案 | `m.file` 加 `file` 欄（AES-CTR）；整檔上傳 | 檔案 2 GB，串流播放 | 我們的分塊檔（約定規格書），`msgtype = org.wbftw.wbfuwunel.file` |
| 聯邦 | 有：room 可以跨 server | 沒有：單一平台 | 保留，能兼容盡量兼容（plan-v1 §7.2） |

Telegram 沒有而 Matrix 有、我們**要留**的：多裝置各自金鑰、裝置驗證、聯邦、房間 state（可查歷史誰改了名字）。
Matrix 沒有而 Telegram 有的，全部在 §5，維護者逐列定過。

## 2. 我們的模型（`wbf-sdk` 的 pub 型別）

只放資料，不放 Matrix 的東西。下面是 Rust 的形狀，欄位名就是 JSON 欄位名（CLI 印的就是它）。

### 2.1 對話

```rust
pub struct ConversationId(String);        // 現在 = room_id，之後可以是任何東西；對外是不透明字串

/// 高層的分類，**從 room 的事實推出來**（成員數、`m.direct`、power levels），不是 room 上多存的欄位；
/// 底層永遠是一致的 ROOM（維護者 2026-09-05 定）。
pub enum ConversationKind {
    Direct,       // 兩個人的 room（§3.1 怎麼判）
    Group,        // 多人的 room，大家都能發
    Channel,      // 只有 owner 能發言的 room（§3.2）
}

pub struct Conversation {
    pub id: ConversationId,
    pub kind: ConversationKind,
    pub name: Option<String>,             // Direct 沒有 name，顯示對方
    pub topic: Option<String>,
    pub avatar: Option<Attachment>,       // 之後
    pub encrypted: bool,                  // false 是例外，UI 要標
    pub member_count: u32,
    pub my_power_level: i64,              // Matrix 的數字照給（§2.4）
    pub can_send_message: bool,           // 從 power levels 算好的結論，UI 直接用
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
pub struct Member { pub peer: Peer, pub power_level: i64, pub joined_at: Option<Timestamp> }

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
                       NameChanged{..}, TopicChanged{..}, PowerLevelChanged{..}, EncryptionEnabled, Pinned{..}, Other(String) }
```

### 2.4 權限：維持 Matrix 的 power level，不自己搞一套（維護者定）

`Conversation.my_power_level`、`Member.power_level` 就是 Matrix 的數字；「能不能做 X」由 adapter 拿 power levels 的門檻算好，
以 `can_send_message` 這種 bool 給出來，UI 不自己比數字。沒有 Role enum：Owner／Admin／Member 只是 UI 顯示用的詞（100／≥ 50／其他），不進模型。

### 2.5 未讀

```rust
pub struct Unread { pub count: u32, pub mentions: u32, pub local_offset: Option<MessageId> }
```

`local_offset` 是自己看到哪，只在本地（之後進 [local-cache-db.md](local-cache-db.md) 的 `read_positions`；現在沒有本地庫，只活在記憶體）。
`count` 是 `local_offset` 之後、不是自己發的、非 System 的訊息數；我們自己算，不信 server 的通知計數（各 server 算法不一）。

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
    async fn send_read_receipt(&self, id: &ConversationId, upto: &MessageId, visible_to_others: bool) -> Result<()>;  // §3.5
    async fn pin(&self, msg: &MessageId, pinned: bool) -> Result<()>;
    async fn create(&self, options: CreateOptions) -> Result<ConversationId>;   // name、invite、encrypted（§3.6）、channel 包裝（§3.2）
    async fn invite / leave / kick / ban / set_power_level ...
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

### 3.2 Channel：底層 do nothing，用原生邏輯達到功能（維護者定）

不自己蓋 channel、不加任何 state 事件。UI 建「channel」時只是把 room 包裝成：`events_default` 設到只有 owner 達得到（100）、其他成員預設 0。
這樣其他 Matrix client 進來也是「看得到、發不出」，完全兼容。
`ConversationKind::Channel` 是 adapter 從 power levels **推**出來的：發訊息的門檻高到只有 owner 達得到 → Channel。標準就是這樣，沒有「猜」。

Telegram 頻道「訂閱者看不到彼此」：Matrix 做不到，**先不考慮**（維護者定）。頻道的其他功能（訂閱、簽名、統計…）是草案，與 fork server 連動，這裡不規劃。

### 3.3 power level：照抄（維護者定）

數字直接給（`my_power_level`、`Member.power_level`），`set_power_level` 寫什麼就是什麼。adapter 只多做一件事：把「門檻 vs 我的數字」算成 `can_send_message` 這種 bool。
UI 要顯示 Owner／Admin／Member 自己對（100／≥ 50／其他），不進模型。

### 3.4 訊息事件的對應

| 我們 | Matrix 事件 |
|---|---|
| `Text` | `m.room.message`，`msgtype: m.text`，`body`；有 `formatted_body` 就進 `Formatted` |
| `File` | `m.room.message`，`msgtype: org.wbftw.wbfuwunel.file`，區塊照約定 §5。**送出時同一個請求要宣告 `attachments`**（約定 §5.2：`Event/Send` 的 meta，或過渡期 HTTP 的 `X-Wbf-Attachments` header），不然 server 過保護期把媒體清掉。**別人的 `m.file`／`m.image`（標準附件，AES-CTR）：第一版當 `Unsupported`，印 type 與 `body`**，下載標準附件是之後的事 |
| `reply_to` | `m.relates_to.m.in_reply_to.event_id`；`body` 不再塞引文（新規格已廢引文），`m.mentions` 照填 |
| `edited` | 收：`m.replace` 事件折進原訊息（adapter 做聚合）；送：`edit()` 發 `m.replace` |
| `Deleted` | 收：redacted 事件；送：`delete()` 發 redaction。**內容被清空是 server 行為，我們不能保留原文**（本地也不存，§7.1） |
| `reactions` | `m.reaction` 事件，`m.annotation`；adapter 聚合成 `key → Vec<PeerId>` |
| `System` | `m.room.member`、`m.room.name`、`m.room.topic`、`m.room.power_levels`、`m.room.encryption`、`m.room.pinned_events`… |
| `Unsupported` | 其他所有 type。**不丟**，這是 fail-safe：至少讓人看到「這裡有東西」 |

### 3.5 已讀

維持 Matrix 做法，兩種 read 分開（維護者定）：

| | 放哪 | 誰看得到 | 怎麼送 |
|---|---|---|---|
| **本地 offset** | 本地（之後進 local-cache-db 的 `read_positions`；現在只在記憶體） | 只有自己這台裝置 | 不送。UI 捲到哪就寫哪 |
| **給遠端看的 read** | server（`m.read` 收據） | 房裡每個人（雙勾勾） | `send_read_receipt(visible_to_others)`：true 送 `m.read`，false 送 `m.read.private`（只同步自己的裝置，對方看不到）。**要不要送、送哪種，是 UI 的 feature 設定**，不是 SDK 的政策 |

`m.fully_read`：不用，本地 offset 取代它（它本來就只是「自己讀到哪」的 server 端副本）。`Unread` 從本地 offset 算。

### 3.6 加密

- **開 room 時自由選擇要不要加密**，由 UI（也就是 owner）決定；底層不帶立場（`CreateOptions.encrypted`）（維護者定）。
  不論是不是 E2EE，都照 Matrix 實作：加密就 `m.room.encryption`（Megolm）；公開可搜就 `history_visibility: world_readable` 加 `join_rule: public`，這兩件事互不影響。
  UI 的預設值（例如 Direct／Group 預設加密）是 UI 的事；非加密 room 送檔案前的警告照約定 §5.1。
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
維護者 2026-09-05 定、server 端 2026-09-06 做完（wbfuwunel #20／#22，規格 `room-seq-and-recent.md`）：**每個 room 一個連續序號**，第一個事件是 1。
這是 Telegram 的做法：順序由 server 說了算，所以我們的使用者之間一致。兩個數，都在事件的 `unsigned`：

| key | 意思 | client 拿來做什麼 |
|---|---|---|
| `org.wbftw.wbfuwunel.r_seq` | **room 內**連續：本站到達順序，新事件 1、2、3…，state 事件也算；只有聯邦 `/backfill` 補回的歷史拿 0、−1、−2；redact 不改號；發出去的正號永不重排 | 排序、未讀數相減、判快取的洞、「跳到第 N 則」 |
| `org.wbftw.wbfuwunel.g_seq` | **本站全域**序號（就是 server 的 PduCount），跨 room 可比大小，對單一 room 不連號 | 只當**水位線**：「我的快取讀到哪」，餵給 `Event/Recent` 的 `cg_seq` |

| | 怎麼做 |
|---|---|
| client 端 | `Message.r_seq: Option<i64>`、`Message.g_seq: Option<i64>`（`protocol::event_seqs`）。`unsigned` 是 server 加的、不進雜湊、送聯邦時被剥掉，其他 client 無感 |
| offset | 一對 `(event_id, r_seq)`：`event_id` 是可攜的權威（聯邦、換 server 都認得），`r_seq` 是本地算術用；比較用 `r_seq` |
| `sent_at` | 只當顯示用的時間 |
| 全域更新 | pack `Event/Recent`（kind `0x14`／subtype `0x01`）：client 帶快取裡最新的 `g_seq` 當 `cg_seq`，server 從最新往舊回到碰到它為止（最多 `limit`，預設 10000），這樣**只拿快取缺的**；差距超過 `limit` 就 `complete=false`、帶同一個 `cg_seq` 加 `before=next` 補洞到 `complete`。`latest_g_seq` 存下來當下次的 `cg_seq`。事件新到舊、自帶 `room_id`。這就是「初開 app 掛載一萬則」的實作，而且之後每次開都只拿差異 |
| 還沒有的 | server 端「`r_seq` → 事件」的反查（跳到第 N 則直接問）：server 列為候選。現在 client 用 `/messages` 二分逼近，或先只提供「跳到快取裡有的第 N 則」 |

**退化（維護者接受）**：非 fork 的 server 上的 room 沒有 `r_seq`。client 必須顯式判斷它在不在，不靠巧合：

| 功能 | 有 `r_seq` | 沒有 `r_seq` |
|---|---|---|
| 順序 | 照 `r_seq` | 照 backend 交出來的順序 |
| 跳到某則（reply、搜尋結果）帶上下文 | `/context/{event_id}` | 同左，這是 Matrix 標準，都能 |
| 跳到第 N 則 | 能 | **不能**，UI 不提供 |
| 依日期跳 | `/timestamp_to_event` 再 `/context` | 同左，標準 |
| 快取判洞 | 看 `r_seq` 有沒有斷 | 只快取最新的一段連續視窗，跳過去的段不快取（或存 token 判洞，之後再說） |
| 未讀數 | `r_seq` 相減 | 從 offset 往後數，只數快取裡有的 |
| 全域更新 | `Event/Recent` 帶 `cg_seq` | 沒有：逐房 `/sync`／`/messages` |

這張表的「沒有 `r_seq`」那一欄就是聯邦兼容的代價，不補。`Hello.features` 有 `seq`、`recent` 才表示 server 支援（feature 旗標是短名，與 `unsigned` 裡的 key 是兩層）。

## 5. Telegram 有、Matrix 沒有：全部要審

維護者 2026-09-05 逐列定過：

| 功能 | Matrix 能不能 | 定案 | |
|---|---|---|---|
| Delete for me（只在自己這邊消失） | 不能：redaction 是全體 | **本地標記**：清本地快取並記「這則已清」，之後從 server 拿到也忽略；重新安裝 app 會恢復。不動 server。進 local-cache-db 的 `hidden_messages` | 維護者定 |
| 自毀訊息（timer） | 沒有 | **草案**，未來與 fork server 連動，這裡不規劃 | 維護者定 |
| 轉發（帶原作者） | 沒有轉發語意 | 事件加 `org.wbftw.wbfuwunel.forwarded_from { peer, message }`，其他 client 看到普通訊息 | 維護者定（同意） |
| 頻道訂閱者互不可見 | 不能 | 先不考慮 | 維護者定 |
| 頻道功能（訂閱、簽名、統計…） | 部分 | 草案，與 fork server 連動 | 維護者定 |
| 公開頻道不加密、歷史公開可搜 | 可以 | 由 owner 在開 room 時決定；加密與公開可搜是兩個獨立選項，都照 Matrix 實作（§3.6） | 維護者定 |
| @username 搜人 | user directory，只搜同 server 與共同房間 | 用它 | 兼容 |
| 訊息序號、跳到第 N 則 | 沒有 | per-room 連續 `seq`，由 fork server 發、放 `unsigned`；非 fork server 的 room 沒有就不提供跳第 N 則（§4.3） | 維護者定 |
| 已讀不通知對方 | 有 `m.read.private` | UI 設定（§3.5） | 兼容 |
| 排程訊息、草稿同步 | 沒有 | 不做；草稿只做本地版 | 維護者定 |
| 語音訊息、貼圖、投票 | 有 MSC | 維持 Matrix 模式，優化再議 | 維護者定 |
| 超大群（十萬人） | 可以 | 不做 | 維護者定 |

## 6. 第 3 步的範圍（不是全部一次做）

> 2026-09-06 第一版做了（`wbf-sdk/src/chat.rs`、`backend/matrix_sdk.rs`、CLI `rooms`／`send`／`watch`／`read`／`files`）。與本文的差異，程式碼檔頭也寫了：
> - id 用 `String`，不包 newtype；`SystemEvent` 先是 `event_type` 加一行文字；`Unread`、`Tag`、`DeviceTrust` 還沒。
> - `watch` 是 callback（`FnMut(Update) -> Continue|Stop` 加 deadline）不是 `Stream`：sync 迴圈在 backend 手上。
> - **`RoomCrypto` trait 這一版沒有**：加密完全在 matrix-sdk 的 `Room::send`／`TimelineEvent` 裡，我們沒碰 `OlmMachine`，沒東西可包；
>   接管送訊息（附件宣告需要）那一版才會出現。空的 trait 是儀式，不先立。
> - **附件宣告（約定 §5.2）帶不出去**：matrix-sdk 的 `Room::send` 不能加 header、server 的 `Event/Send` 還是提案；CLI 送檔案時印警告。
>   要帶就得自己 Megolm 加密再走 `Event/Send`，那需要 submodule 露出 `Room::encrypt` 這類的入口（小 patch，但是 fork）或直接拿 `OlmMachine`。等 server 定案再定。
> - 聚合（edit／reaction／redaction 折進目標）只在同一頁內；目標不在頁裡的關係事件照原樣留著。

1. `Backend` trait 與 `matrix_sdk` adapter；`conversations`、`conversation`、`history`、`send_text`、`send_file`、`watch`。
2. CLI：`rooms`（印 `Conversation`）、`send --text`、`send --file`（非加密對話警告並確認，約定 §5.1）、`watch`、`read`、`files`（CLI 規格 §3.4）。
3. `Message` 的聚合：edit 折進去、reaction 聚合、redaction 變 `Deleted`；`Unsupported` 不丟。
4. 加密：解得開就解，解不開標原因；`RoomCrypto` trait 立起來，實作包 `OlmMachine`。
5. **不做**：建房、邀請、角色、置頂、已讀送出、裝置驗證、標準附件下載。這些是第 3 步之後一個一個加。

## 7. 還開著的（再議）

1. ~~時序與序號~~ 定了：§4.3。
2. ~~「跨房間全域最近 N 則」~~ server 做完了（wbfuwunel #22）：`Event/Recent`，帶 `cg_seq` 只拿快取缺的（§4.3）。client 端 `WbfClient::recent` 已接，對著 server 的向量檔有測試。
3. ~~§6 的範圍~~ 維護者 2026-09-06 同意。
4. ~~開 issue~~ [wbfuwunel #20](http://ai.zooy.cc:30008/amaid/wbfuwunel/issues/20) 已關，#22 合併：名字定為 `r_seq`／`g_seq`（與 pack 標頭的 `seq` 分開）。
5. **送事件要宣告附件**（約定 §5.2，server 提案 `media-attachments.md`）：server 端還沒實作，定案後要回來核對約定 §5.2 每一條。第 3 步的 `send --file` 從第一版就要帶。

已定案的都寫在各節，標「維護者定」。

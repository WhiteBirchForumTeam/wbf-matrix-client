# 訊息動作：回覆、編輯、表情、收回（維護者 2026-11-04 定）

> 這份講**送**的那半：UI 叫哪支 RPC、daemon 送出什麼 content、哪些在本地先擋。
> **收**的那半（edit 怎麼折回目標、redact 怎麼打勾、reaction 怎麼聚合）早就做好了，權威在 /docs/design/messages/edits-and-redactions.md §4–§6，這份不重複。
> 狀態：已實作（2026-11-04）。RPC 的權威表在 /docs/design/rpc-specs/rpc-spec.md §3.3，這份留理由與 content 的逐字形狀。
> 程式碼：content 怎麼組在 `crates/wbf-sdk/src/message_content.rs`（純函式），送出與本地的擋在 `crates/wbf-core/src/message_actions.rs`。

## 0. 一句話

UI 叫一支 RPC，daemon 照 Matrix 的格式包一則事件送出去、回 server 給的 `event_id`。daemon 送完**不**寫本地：結果跟別人送的一樣從 Rooms 線推回來，照收的規則折進目標。

## 1. 已定的（維護者 2026-11-04）

1. **範圍是四件事，照 Telegram 的切法**（/docs/design/overview/architecture-v2.md §8）：

   | | Telegram | 我們（RPC） | 上游 Matrix |
   |---|---|---|---|
   | 回覆 | 每個送出方法都帶 `reply_parameters` | `room.send_text`／`room.send_file`／`room.send_attachment` 多一個 `reply_to?` | content 加 `m.relates_to.m.in_reply_to` |
   | 編輯 | `editMessageText` | 新的 `room.edit` | `m.replace` ＋ `m.new_content` |
   | 表情 | `setMessageReaction` | 新的 `room.react` | `m.reaction`（`m.annotation`） |
   | 收回 | `deleteMessage` | 新的 `room.redact`：收回訊息、收回自己的表情都是它 | redaction |

2. **收回自己的表情：`Reaction` 多一欄 `mine`**（甲）——讀的那個帳號自己按的那則反應的 `event_id`，UI 拿它叫 `room.redact`。
   **不**另開 `room.unreact`：收回表情在 Matrix 上就是收回一則事件，那則的 id 已經在 `mine` 裡，daemon 不必替 UI 去找（§5）。
3. **回覆填 `m.mentions`**：送出的 RPC 多一個 `mentions?`，原樣帶上去；有 `reply_to` 時 daemon 再把被回覆的人加進去（§3.2）。
4. **編輯先只做文字**：目標必須是 `m.text`。改附件說明（Telegram 的 `editMessageCaption`）之後再補。
5. **同一個表情：UI 切換，daemon 照寫**（維護者 2026-11-04 原話：「UI 點第一下是新加表情，點第二下已存在的應該收回該表情，但 daemon 對送出／收回應該照寫」）：
   - **切換是 UI 的事**：這個 key 沒有 `mine` → 叫 `room.react`；有 `mine` → 拿它叫 `room.redact`。
   - `room.react` 只會「加」，`room.redact` 只會「收」。daemon：禁止把 `room.react` 改成收回。
   - daemon 唯一的「不再送」是冪等：本地看得到這個帳號已經按過同一個 key，就回那則的 `event_id`，不再送一則（§4.3）。效果跟同一個 `txn_id` 重送一樣。

## 2. RPC

所有的都另帶 `user?`、`server?`，跟其他 RPC 一樣。

| method | 參數 | 回應 |
|---|---|---|
| `room.send_text` | 現有的參數 ＋ `reply_to?`、`mentions?` | `{ event_id }`（不變） |
| `room.send_file` | 現有的參數 ＋ `reply_to?`、`mentions?` | 不變 |
| `room.send_attachment` | 現有的參數 ＋ `reply_to?`、`mentions?` | 不變 |
| `room.edit` | `{ room, event_id, body, mentions?, room_devices?, txn_id? }` | `{ event_id }`：**edit 那則自己的** id |
| `room.react` | `{ room, event_id, key, room_devices?, txn_id? }` | `{ event_id }`：那則反應的 id（冪等時是本地已有的那則） |
| `room.redact` | `{ room, event_id, reason?, txn_id? }` | `{ event_id }`：**收回動作本身**那則的 id（被收回的那則是參數裡的 `event_id`） |

- `event_id` 一律是**目標**（被回覆、被改、被按、被收回的那則）。
- `mentions` 是 mxid 的陣列，例 `["@bob:localhost"]`。`@room`（`m.mentions.room`）不在這支。
- `room_devices`、`txn_id`、1401／1402 跟 `room.send_text` 同一套（/docs/design/keys/e2ee-rpc.md §3）：加密房必帶 `room_devices`；`txn_id` 沒帶 daemon 產一個；daemon NEVER 自動重送。
- `room.redact` **沒有** `room_devices`：redaction 在 Matrix 上一律明文（§4.4）。

例（本機 wbfuwunel 實跑，`crates/wbf-daemon/tests/real_server.rs` 的 `message_actions_reply_react_edit_and_redact_between_two_accounts`，明文房；房間與被回覆那則的 id 換成 `<…>`）：

```jsonc
→ { "method": "room.send_text", "params": { "user": "@alice:localhost", "room": "<room>", "body": "好", "reply_to": "<bob 問的那則>" } }
← {"code":0,"id":80,"msg":"ok","result":{"event_id":"$7NQBXztO69txToJnpIW1_z6ZWYZxww7qTJBFcAueuqE"}}

→ { "method": "room.react",  "params": { "user": "@alice:localhost", "room": "<room>", "event_id": "<bob 問的那則>", "key": "👍" } }
← {"code":0,"id":82,"msg":"ok","result":{"event_id":"$mQ-ljzklaEH6IWE5j_n402-4QqZuFpAkn8L4OMvgdrw"}}

→ { "method": "room.edit",   "params": { "user": "@alice:localhost", "room": "<room>", "event_id": "$7NQBXztO69txToJnpIW1_z6ZWYZxww7qTJBFcAueuqE", "body": "好，吃拉麵" } }
← {"code":0,"id":86,"msg":"ok","result":{"event_id":"$cZStIU2YScVG5egplQ0rdzbcG-YjCUQdEtUvYxTPKgo"}}

→ { "method": "room.redact", "params": { "user": "@alice:localhost", "room": "<room>", "event_id": "$mQ-ljzklaEH6IWE5j_n402-4QqZuFpAkn8L4OMvgdrw" } }   // 收回自己的 👍（`mine`）
← {"code":0,"id":88,"msg":"ok","result":{"event_id":"$WipZvvwOHlSrVYVd-H7-J4DUVDSdpc4LCdIneMFUg_U"}}

→ { "method": "room.redact", "params": { "user": "@bob:localhost", "room": "<room>", "event_id": "$7NQBXztO69txToJnpIW1_z6ZWYZxww7qTJBFcAueuqE" } }   // bob 收回 alice 的
← {"code":1400,"data":{"code":"Forbidden","code_id":1302,"errcode":"M_FORBIDDEN","message":"M_FORBIDDEN: User cannot redact this event.","status":403},"id":91,"msg":"server Forbidden (1302): M_FORBIDDEN: User cannot redact this event.","result":null}
```

加密房的每一支（`room.redact` 以外）再帶 `room_devices`（`room.refresh_devices` 回的那份），回應形狀一樣。

錯誤：

| 情況 | 回 |
|---|---|
| `room.edit` 的目標不合 §4.2 那張表 | **1100**，`msg` 寫哪一條 |
| 加密房沒帶 `room_devices`、本地不知道這間加不加密 | **1100**（同 `room.send_text`） |
| 號碼過期／金鑰沒就緒 | **1401**／**1402**（同 `room.send_text`） |
| server 拒了 `room.redact`（例：收回別人的訊息而權限不夠，403） | **1400**，`data` 是 server 的拒絕原樣（同房間動作，/docs/design/rpc-specs/rpc-spec.md §5.3） |

## 3. 送出什麼 content（逐字形狀，測試要逐字斷言）

### 3.1 回覆

在原本的 content 上加 `m.relates_to`。禁止在 `body` 塞引文：規格已廢 fallback 引文（/docs/design/rooms/chat-model.md §3.4）。

```jsonc
{ "msgtype": "m.text", "body": "好",
  "m.relates_to": { "m.in_reply_to": { "event_id": "$abc" } },
  "m.mentions": { "user_ids": ["@alice:localhost"] } }
```

附件（`room.send_file`、`room.send_attachment`）同樣加在那則 `m.room.message` 的 content 上。

### 3.2 `m.mentions`

| `reply_to` | `mentions` | 送出的 `m.mentions` |
|---|---|---|
| 沒有 | 沒有 | **整欄不帶**（跟現在一樣：對方照 body 文字找 @ 名字） |
| 沒有 | 有 | `{ "user_ids": <mentions> }` |
| 有 | 有或沒有都一樣 | `{ "user_ids": <mentions> ∪ {被回覆的人} }` |

- **被回覆的人從本地查**：目標在這個帳號的本地（`(room, event_id)` 找得到、而且這個帳號同步過）就取它的 `sender`。
  查不到就不加，也**不**報錯：回覆照送，只是對方可能收不到通知。
- 被回覆的人是自己 → 不加（規格：`m.mentions` 不放自己）。
- `user_ids` 去重，照「`mentions` 的順序、被回覆的人放最後」排，測試好斷言。
- ⚠️ 為什麼有回覆時一定要帶：只要送了 `m.mentions`，對方的 client 就**不再**從 body 文字裡找 @ 名字。
  所以 body 裡 @ 到的其他人要由 UI 放進 `mentions`。daemon：禁止從 body 解析 @ 名字。

### 3.3 編輯（`room.edit`）

```jsonc
// 沒帶 mentions（`crates/wbf-sdk/src/message_content.rs` 的 `an_edit_without_mentions_notifies_nobody` 逐字釘住）
{ "msgtype": "m.text", "body": "* 改過的字",
  "m.new_content": { "msgtype": "m.text", "body": "改過的字", "m.mentions": {} },
  "m.mentions": {},
  "m.relates_to": { "rel_type": "m.replace", "event_id": "$abc" } }
```

- 外層 `body` 是給不認得 edit 的 client 看的 fallback，照慣例前面加 `* `。
- `m.new_content` 是**完整的新內容**（全量，/docs/design/messages/edits-and-redactions.md §5），裡面**不**放 `m.relates_to`：
  回覆關係留在原訊息上，收的那半本來就這樣讀（同一份 §5「`m.relates_to` 留自己原本的」）。
- 帶了 `mentions`：`m.new_content.m.mentions = { user_ids: <mentions> }`（新版本的完整名單）；外層 `m.mentions` 只放**這次新加的**，
  也就是 `mentions` 減掉目標目前那一版的 `m.mentions.user_ids`（規格：改字只通知新提到的人）。
- 沒帶 `mentions`：兩層都帶 `"m.mentions": {}`。改字就**不**通知任何人，對方也**不會**照 body 文字重新比對。

### 3.4 表情（`room.react`）

```jsonc
// event type: m.reaction
{ "m.relates_to": { "rel_type": "m.annotation", "event_id": "$abc", "key": "👍" } }
```

`key` 原樣送：Matrix 的 key 是任意字串，daemon **不**檢查它是不是 emoji。空字串 → 1100。

### 3.5 收回（`room.redact`）

`PUT /_matrix/client/v3/rooms/{room_id}/redact/{event_id}/{txn_id}`，body `{}` 或 `{ "reason": "打錯字" }`。

## 4. 怎麼送

### 4.1 加密跟 `room.send_text` 同一套

- 加不加密看本地的 `rooms.encrypted`（/docs/design/rpc-specs/rpc-spec.md 的 `room.send_text` 那列）；不知道 → 1100。
- 加密房：只用後台已經分好的房間金鑰，等就緒最多 2 秒，否則 1402；被 server 擋（號碼過期）→ daemon 重拿房間狀態、回 1401（/docs/design/keys/e2ee-rpc.md §3）。
- **關係留在明文那層**：上游加密時把 `m.relates_to` 拷到明文那層（vendor 的 `matrix-sdk-crypto/src/olm/group_sessions/outbound.rs:594`），server 與別的 client 都折得回目標。
- **reaction 的 event type 是 `m.reaction`**。wbf 帳號送訊息的路（`crates/wbf-core/src/wbf_rooms.rs` 的 `wbf_send_message`）現在把 type 寫死成 `m.room.message`，要改成由呼叫者給。

### 4.2 `room.edit` 先在本地擋（fail closed）

目標必須**同時**滿足下面每一條，否則回 1100、不送：

| 條件 | 為什麼 |
|---|---|
| 在這個帳號的本地（`(room, event_id)` 找得到、這個帳號同步過） | 下面幾條都要讀它；找不到就驗不了 |
| `sender` 是這個帳號自己 | 🚨 規格：sender 不同的 edit 無效 |
| 本身是一般訊息（`class` 是 `msg`），不是另一個 edit | 規格：edit 只能指向原始訊息 |
| 還在（`is_redacted = 0`） | 已收回的目標，收的那半會丟掉它的 edit（/docs/design/messages/edits-and-redactions.md §5「寫入一個 edit 時」） |
| 是 `m.room.message`、`msgtype` 是 `m.text` | 規格：edit 跟目標同一個 type；文字以外的這支不做（§1 第 4 條） |
| 目標是加密的（`decrypted` 有值）→ 這次也要加密送 | 🚨 規格：加密訊息只能用加密的 edit 改。wbf 帳號照本地的加密標記送，所以比一次；收到加密事件本來就會把標記往上升，這條是防線。一般 Matrix 帳號由 matrix-sdk 照房間加密 |

⭐ **為什麼 daemon 擋、不交給 server**：違反這幾條的 edit server 照收，但**每個 client 都會丟掉它**（/docs/design/messages/edits-and-redactions.md §5 最後那張表）。
送出看起來成功，其實沒人看得到。這跟 `room.send_text`「本地不知道加不加密就報錯」是同一類：擋的是「送出去一定錯」的請求，**不是**替 UI 做選擇。

📎 本地看到的可能比 server 舊（例：目標剛被別的裝置收回、還沒推到）。那種會照送，收的那半照規則丟掉，跟「送出之後才被收回」是同一件事。

### 4.3 `room.react` 的冪等

送之前查本地：這個帳號對這則、這個 `key` 已經有一則**還在、這個帳號看得到**的反應 → 直接回它的 `event_id`，不送。
- 本地還沒有（例：剛送、還沒推回來）→ 照送。萬一 server 上因此有兩則，聚合照人去重（§5），UI 看到的仍是一個。
- 只查本地，**不**問 server。

### 4.4 `room.redact` 不加密、走端點表

Matrix 的 redaction 在加密房也是明文，所以**不**帶 `room_devices`，也**不會**回 1401／1402。
走 `crates/wbf-sdk/src/matrix_endpoint.rs` 的那張表（多一列 `REDACT`，橋 `0x14 0x24`，wbfuwunel 的 /docs/bridge-specs/0x14-event.md）：
wbf 帳號走橋、一般 Matrix 帳號走同一支 HTTP 端點，跟房間動作同一條接縫（/docs/design/rooms/room-actions.md）。
daemon **不**檢查目標是誰的、在不在本地：收回別人的訊息要權限，由 server 照 power level 擋（1400）。

### 4.5 各帳號走哪條線

| | wbf 帳號 | 一般 Matrix 帳號 |
|---|---|---|
| 回覆、編輯、表情 | **WS** `Event/Send`（明文房明文；加密房 Megolm、帶 `room_version`） | matrix-sdk 的 `Room::send_raw`（加密房它自己 Megolm）：文字、編輯、表情、附件都是，content 跟 wbf 那條同一份（`message_content.rs` 組的） |
| 收回 | 橋 `0x14 0x24` | HTTP `PUT …/redact/…` |

📎 原生 `Event/Send` 收 `m.reaction`：2026-11-04 在 e2e 裡對本機 wbfuwunel 實跑過，明文房與加密房都收（加密房的 server 上是 `m.room.encrypted`，`m.relates_to` 在明文那層）。

### 4.6 送完不寫本地

跟 `room.send_text` 一樣：結果從 Rooms 線推回來，照 /docs/design/messages/edits-and-redactions.md §4 折進目標。UI 要「按下去就變」的話是 UI 自己先畫，等推播來再對齊。

## 5. `Reaction.mine`：收的那半要改的

```jsonc
// room.history 回的一則訊息的 reactions（`crates/wbf-core/src/message_actions.rs` 的 `local_history_counts_a_person_once_and_marks_mine` 印出來的）：
// @a 讀，她自己按過兩次 👍（$first、$again）、bob 按過一次
"reactions": [ { "key": "👍", "by": ["@a:localhost", "@bob:localhost"], "mine": "$first" } ]
// bob 讀同一則：沒有 mine（e2e 實跑）
"reactions": [ { "key": "👍", "by": ["@alice:localhost"] } ]
```

- `mine` ＝**讀的那個帳號**（RPC 的 `user`）自己按的那則反應的 `event_id`；沒按過就整欄不帶。
- 跟 `by` 同一條可見性規則（/docs/design/messages/edits-and-redactions.md §8「讀取」）：還在、這個帳號同步過、沒 hide。
- **`by` 照人去重**（順手修現在的 bug）：同一個人對同一個 key 有兩則（別的 client 送重了、或 §4.3 那個窗口），現在 `crates/wbf-sdk/src/cache.rs` 的 `list_reactions` 會把他算兩次。
  去重之後 `mine` 取**最早**的那則：讀本地是寫進來的順序（`ORDER BY x.id` 的第一則）；不寫庫的那條照 `origin_server_ts`（一頁可能是新到舊）。📎 兩則都在時，收回一則之後另一則還在、`mine` 換成它，UI 再點一次就收回它，照切換的規則一樣走得完。
- 不寫庫的路（`sync=server`、watch 與推播的通知，`event_json::messages_from_incoming`）也算 `mine`，所以它收讀的帳號。
  這條路的規則跟寫庫那條一樣（審查 #81 補齊）：
  1. **收回先套**：同一頁裡收回比 edit 早，還是收回贏，訊息**不**會被 edit 改回文字；被收回的表情與 edit 折掉、不算數，`mine` **不**指著已收回的那則。
  2. **edit 要過 `check_replacement` 才套**（/docs/design/messages/edits-and-redactions.md §5）：別人送的、用明文改加密訊息的、改了 type 的都折掉、不套。
  3. 其餘關係照 `origin_server_ts` 舊到新套：同一頁裡有兩個 edit 時是**最新**的贏（以前照頁的順序套，新到舊的頁會讓最舊的贏）。

## 6. 不在這支

- 討論串（`m.thread`）、轉傳、改附件說明、`@room`。
- 已讀（/docs/design/messages/read-receipts.md）。
- server 的關聯查詢 `0x14 0x26`–`0x29`：關係事件會跟著推播進來，現在用不到。

## 7. 怎麼驗

| 層 | 測試 | 驗什麼 |
|---|---|---|
| sdk（純函式） | `crates/wbf-sdk/src/message_content.rs` 的 tests | §3 的每個例子逐字；§3.2 那張表一列一條；編輯只通知新加的人 |
| sdk（不寫庫的聚合） | `crates/wbf-sdk/tests/chat_mapping.rs` 的 `the_same_person_pressing_twice_counts_once_and_mine_is_the_earliest` | 新到舊的一頁：`by` 照人去重、`mine` 是最早的、別人讀沒有 `mine` |
| sdk（不寫庫的聚合） | 同一個檔的 `a_redaction_older_than_an_edit_on_the_same_page_still_wins`、`an_edit_from_someone_else_is_folded_but_not_applied`、`a_plaintext_edit_cannot_replace_an_encrypted_message`、`a_reaction_or_edit_withdrawn_on_the_same_page_does_not_count` | §5 那三條規則：收回贏（新到舊、舊到新兩種頁）、別人的 edit 不套、明文 edit 改不了加密訊息、同一頁收回的表情與 edit 不算數 |
| core（假 server） | `crates/wbf-core/src/message_actions.rs` 的 tests | 回覆送出的 content 逐字；編輯逐字；§4.2 每一條拒絕且什麼都不送；表情冪等（別人的、收回過的不算）；收回走橋 `0x14 0x24`；本地讀的去重與 `mine` |
| daemon（真 server） | `crates/wbf-daemon/tests/real_server.rs` 的 `message_actions_reply_react_edit_and_redact_between_two_accounts` | alice 回覆、按表情、編輯、收回表情、收回訊息，明文房與加密房各一次；bob 從 `room.history` 看到折完的結果；server 上的原樣關係在明文那層；bob 收回 alice 的是 1400 |

📎 一般 Matrix 帳號那條（matrix-sdk 的 `send_raw`、HTTP 的 `REDACT`）對本機 wbfuwunel 走不到（登入一定走 wbf），只有編得過；`REDACT` 的 HTTP 路徑跟橋同一張表。

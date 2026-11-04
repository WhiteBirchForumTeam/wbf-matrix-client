# 訊息動作：回覆、編輯、表情、收回（維護者 2026-11-04 定）

> 這份講**送**的那半：UI 叫哪支 RPC、daemon 送出什麼 content、哪些在本地先擋。
> **收**的那半（edit 怎麼折回目標、redact 怎麼打勾、reaction 怎麼聚合）早就做好了，權威在 /docs/design/messages/edits-and-redactions.md §4–§6，這份不重複。
> 狀態：形狀已定、**還沒實作**（等維護者說開工）。實作之後 RPC 的權威表搬進 /docs/design/rpc-specs/rpc-spec.md §3.3，這份留理由與 content 的逐字形狀。

## 0. 一句話

UI 叫一支 RPC，daemon 照 Matrix 的格式包一則事件送出去、回 server 給的 `event_id`；送完🚫 寫本地，結果跟別人送的一樣從 Rooms 線推回來、照收的規則折進目標。

## 1. 已定的（維護者 2026-11-04）

1. **範圍是四件事，照 Telegram 的切法**（/docs/design/overview/architecture-v2.md §8）：

   | | Telegram | 我們（RPC） | 上游 Matrix |
   |---|---|---|---|
   | 回覆 | 每個送出方法都帶 `reply_parameters` | `room.send_text`／`room.send_file`／`room.send_attachment` 多一個 `reply_to?` | content 加 `m.relates_to.m.in_reply_to` |
   | 編輯 | `editMessageText` | 新的 `room.edit` | `m.replace` ＋ `m.new_content` |
   | 表情 | `setMessageReaction` | 新的 `room.react` | `m.reaction`（`m.annotation`） |
   | 收回 | `deleteMessage` | 新的 `room.redact`：收回訊息、收回自己的表情都是它 | redaction |

2. **收回自己的表情：`Reaction` 多一欄 `mine`**（甲）——讀的那個帳號自己按的那則反應的 `event_id`，UI 拿它叫 `room.redact`。
   🚫 另開 `room.unreact`：收回表情在 Matrix 上就是收回一則事件，daemon 🚫 替 UI 找（§5）。
3. **回覆填 `m.mentions`**：送出的 RPC 多一個 `mentions?`，原樣帶上去；有 `reply_to` 時 daemon 再把被回覆的人加進去（§3.2）。
4. **編輯先只做文字**：目標必須是 `m.text`。改附件說明（Telegram 的 `editMessageCaption`）之後再補。
5. **同一個表情：UI 切換，daemon 照寫**（維護者 2026-11-04 原話：「UI 點第一下是新加表情，點第二下已存在的應該收回該表情，但 daemon 對送出／收回應該照寫」）：
   - **切換是 UI 的事**：這個 key 沒有 `mine` → 叫 `room.react`；有 `mine` → 拿它叫 `room.redact`。
   - **daemon 🚫 把 `room.react` 變成收回**。`room.react` 只會「加」，`room.redact` 只會「收」。
   - daemon 唯一的「不再送」是冪等：本地看得到這個帳號已經按過同一個 key，就回那則的 `event_id`、🚫 再送一則（§4.3）。效果跟同一個 `txn_id` 重送一樣。

## 2. RPC

所有的都另帶 `user?`、`server?`，跟其他 RPC 一樣。JSON 是**示意**（還沒實作，跑不出來）。

| method | 參數 | 回應 |
|---|---|---|
| `room.send_text` | 現有的參數 ＋ `reply_to?`、`mentions?` | `{ event_id }`（不變） |
| `room.send_file` | 現有的參數 ＋ `reply_to?`、`mentions?` | 不變 |
| `room.send_attachment` | 現有的參數 ＋ `reply_to?`、`mentions?` | 不變 |
| `room.edit` | `{ room, event_id, body, mentions?, room_devices?, txn_id? }` | `{ event_id }`：**edit 那則自己的** id |
| `room.react` | `{ room, event_id, key, room_devices?, txn_id? }` | `{ event_id }`：那則反應的 id（冪等時是本地已有的那則） |
| `room.redact` | `{ room, event_id, reason?, txn_id? }` | `{ event_id }`：**收回動作本身**那則的 id（不是被收回的那則） |

- `event_id` 一律是**目標**（被回覆、被改、被按、被收回的那則）。
- `mentions` 是 mxid 的陣列，例 `["@bob:localhost"]`。`@room`（`m.mentions.room`）不在這支。
- `room_devices`、`txn_id`、1401／1402 跟 `room.send_text` 同一套（/docs/design/keys/e2ee-rpc.md §3）：加密房必帶 `room_devices`；`txn_id` 沒帶 daemon 產一個；🚫 自動重送。
- `room.redact` **沒有 `room_devices`**：redaction 在 Matrix 上一律明文（§4.4）。

```jsonc
→ { "method": "room.send_text", "params": { "room": "!r:localhost", "body": "好", "reply_to": "$abc" } }
← { "event_id": "$reply1" }

→ { "method": "room.edit",   "params": { "room": "!r:localhost", "event_id": "$abc", "body": "改過的字",
                                        "room_devices": { … room.refresh_devices 回的那份 … } } }
← { "event_id": "$edit1" }

→ { "method": "room.react",  "params": { "room": "!r:localhost", "event_id": "$abc", "key": "👍" } }
← { "event_id": "$react1" }

→ { "method": "room.redact", "params": { "room": "!r:localhost", "event_id": "$react1" } }   // 收回自己的 👍
← { "event_id": "$redaction1" }
```

錯誤：

| 情況 | 回 |
|---|---|
| `room.edit` 的目標不合 §4.2 那張表 | **1100**，`msg` 寫哪一條 |
| 加密房沒帶 `room_devices`、本地不知道這間加不加密 | **1100**（同 `room.send_text`） |
| 號碼過期／金鑰沒就緒 | **1401**／**1402**（同 `room.send_text`） |
| server 拒了 `room.redact`（例：收回別人的訊息而權限不夠，403） | **1400**，`data` 是 server 的拒絕原樣（同房間動作，/docs/design/rpc-specs/rpc-spec.md §5.3） |

## 3. 送出什麼 content（逐字形狀，測試要逐字斷言）

### 3.1 回覆

在原本的 content 上加 `m.relates_to`；🚫 在 `body` 塞引文（規格已廢 fallback 引文，/docs/design/rooms/chat-model.md §3.4）。

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
| 有 | 不管有沒有 | `{ "user_ids": <mentions> ∪ {被回覆的人} }` |

- **被回覆的人從本地查**：目標在這個帳號的本地（`(room, event_id)` 找得到、而且這個帳號同步過）就取它的 `sender`；**查不到就不加、🚫 報錯**——回覆照送，只是對方可能不會被通知。
- 被回覆的人是自己 → 不加（規格：🚫 提到自己）。
- `user_ids` 去重，照「`mentions` 的順序、被回覆的人放最後」排，測試好斷言。
- ⚠️ 為什麼有回覆時一定要帶：只要送了 `m.mentions`，對方的 client 就🚫 再從 body 文字裡找 @ 名字。所以 body 裡 @ 到的其他人要由 UI 放進 `mentions`，daemon 🚫 parse body。

### 3.3 編輯（`room.edit`）

```jsonc
{ "msgtype": "m.text", "body": "* 改過的字",
  "m.new_content": { "msgtype": "m.text", "body": "改過的字" },
  "m.relates_to": { "rel_type": "m.replace", "event_id": "$abc" } }
```

- 外層 `body` 是給不認得 edit 的 client 看的 fallback，照慣例前面加 `* `。
- `m.new_content` 是**完整的新內容**（全量，/docs/design/messages/edits-and-redactions.md §5）。🚫 放 `m.relates_to`：回覆關係留在原訊息上，收的那半本來就是這樣讀（同一份 §5「`m.relates_to` 留自己原本的」）。
- 帶了 `mentions`：`m.new_content.m.mentions = { user_ids: <mentions> }`（新版本的完整名單）；外層 `m.mentions` 只放**這次新加的**——`mentions` 減掉目標目前那一版的 `m.mentions.user_ids`（規格：改字🚫 重新通知已經被提過的人）。
  沒帶 `mentions`：兩層都帶 `"m.mentions": {}`（改字🚫 通知任何人，也🚫 讓對方照 body 文字重新比對）。

### 3.4 表情（`room.react`）

```jsonc
// event type: m.reaction
{ "m.relates_to": { "rel_type": "m.annotation", "event_id": "$abc", "key": "👍" } }
```

`key` 原樣送，daemon 🚫 檢查是不是 emoji（Matrix 的 key 是任意字串）；空字串 → 1100。

### 3.5 收回（`room.redact`）

`PUT /_matrix/client/v3/rooms/{room_id}/redact/{event_id}/{txn_id}`，body `{}` 或 `{ "reason": "打錯字" }`。

## 4. 怎麼送

### 4.1 加密跟 `room.send_text` 同一套

- 加不加密看本地的 `rooms.encrypted`（/docs/design/rpc-specs/rpc-spec.md 的 `room.send_text` 那列）；不知道 → 1100。
- 加密房：只用後台已經分好的房間金鑰，等就緒最多 2 秒，否則 1402；被 server 擋（號碼過期）→ daemon 重拿房間狀態、回 1401（/docs/design/keys/e2ee-rpc.md §3）。
- **關係不會被包進密文**：上游加密時把 `m.relates_to` 拷到明文那層（vendor 的 `matrix-sdk-crypto/src/olm/group_sessions/outbound.rs:594`），server 與別的 client 都折得回目標。
- **edit 與 reaction 的 event type 不是 `m.room.message`**：reaction 是 `m.reaction`。wbf 帳號送訊息的路（`crates/wbf-core/src/wbf_rooms.rs` 的 `wbf_send_message`）現在把 type 寫死成 `m.room.message`，要改成由呼叫者給。

### 4.2 `room.edit` 先在本地擋（fail closed）

目標必須**同時**滿足下面每一條，否則 1100、🚫 送：

| 條件 | 為什麼 |
|---|---|
| 在這個帳號的本地（`(room, event_id)` 找得到、這個帳號同步過） | 下面幾條都要讀它；找不到就驗不了 |
| `sender` 是這個帳號自己 | 🚨 規格：sender 不同的 edit 無效 |
| 本身不是 edit（`class` 是 `msg`） | 規格：🚫 edit 一個 edit |
| 沒被收回（`is_redacted = 0`） | 收的那半會丟掉（/docs/design/messages/edits-and-redactions.md §5「寫入一個 edit 時」） |
| 是 `m.room.message`、`msgtype` 是 `m.text` | 規格：edit 🚫 改 type；文字以外的這支不做（§1 第 4 條） |
| 目標是加密的（`decrypted` 不是 NULL）→ 這次也要加密送 | 🚨 規格：明文 edit 蓋不掉加密訊息 |

⭐ **為什麼 daemon 擋、不交給 server**：這幾條違反了 server 照收，但**每個 client 都會丟掉那個 edit**（/docs/design/messages/edits-and-redactions.md §5 最後那張表）——送出看起來成功、其實沒人看得到。
跟 `room.send_text`「本地不知道加不加密就報錯」是同一類：擋的是「送出去一定錯」，🚫 替 UI 做選擇。

📎 本地看到的可能比 server 舊（例：目標剛被別的裝置收回、還沒推到）。那種會照送，收的那半照規則丟掉——跟「送出之後才被收回」是同一件事。

### 4.3 `room.react` 的冪等

送之前查本地：這個帳號對這則、這個 `key` 已經有一則**沒被收回、這個帳號看得到**的反應 → 直接回它的 `event_id`、🚫 送。
- 本地還沒有（例：剛送、還沒推回來）→ 照送。萬一 server 上因此有兩則，聚合會照人去重（§5），UI 看到的仍是一個。
- 只查本地、🚫 問 server。

### 4.4 `room.redact` 不加密、走端點表

Matrix 的 redaction 在加密房也是明文，所以🚫 `room_devices`、🚫 1401／1402。
走 `crates/wbf-sdk/src/matrix_endpoint.rs` 的那張表（多一列 `REDACT`，橋 `0x14 0x24`，wbfuwunel 的 /docs/bridge-specs/0x14-event.md）：
wbf 帳號走橋、一般 Matrix 帳號走同一支 HTTP 端點——跟房間動作同一條接縫（/docs/design/rooms/room-actions.md）。
daemon 🚫 檢查目標是誰的、在不在本地：收回別人的訊息要權限，由 server 照 power level 擋（1400）。

### 4.5 各帳號走哪條線

| | wbf 帳號 | 一般 Matrix 帳號 |
|---|---|---|
| 回覆、編輯、表情 | **WS** `Event/Send`（明文房明文；加密房 Megolm、帶 `room_version`） | matrix-sdk：文字與表情 `Room::send_raw`（加密房它自己 Megolm）；附件 `AttachmentConfig` 的 `reply`／`mentions` |
| 收回 | 橋 `0x14 0x24` | HTTP `PUT …/redact/…` |

📎 「原生 `Event/Send` 收 `m.reaction` 型別」只讀過 server 文件、沒實跑，實作時在 e2e 裡驗。

### 4.6 送完🚫 寫本地

跟 `room.send_text` 一樣：結果從 Rooms 線推回來，照 /docs/design/messages/edits-and-redactions.md §4 折進目標。UI 要「按下去就變」的話是 UI 自己先畫，等推播來再對齊。

## 5. `Reaction.mine`：收的那半要改的

```jsonc
// room.history 回的一則訊息（示意）：alice 讀，她自己按過 👍
"reactions": [ { "key": "👍", "by": ["@alice:localhost", "@bob:localhost"], "mine": "$react1" },
               { "key": "🎉", "by": ["@bob:localhost"] } ]
```

- `mine` ＝**讀的那個帳號**（RPC 的 `user`）自己按的那則反應的 `event_id`；沒按過就整欄不帶。
- 跟 `by` 同一條可見性規則（/docs/design/messages/edits-and-redactions.md §8「讀取」）：沒被收回、這個帳號同步過、沒 hide。
- **`by` 照人去重**（順手修現在的 bug）：同一個人對同一個 key 有兩則（別的 client 送重了、或 §4.3 那個窗口），現在 `crates/wbf-sdk/src/cache.rs` 的 `list_reactions` 會把他算兩次。
  去重之後 `mine` 取**最早**的那則（`ORDER BY x.id` 的第一則）。📎 兩則都在時，收回一則之後另一則還在、`mine` 換成它，UI 再點一次就收回它——照切換的規則一樣走得完。
- 不寫庫的路（`sync=server`、watch 的通知，`event_json::messages_from_incoming`）也要算 `mine`，所以它要知道讀的帳號是誰。

## 6. 不在這支

- 討論串（`m.thread`）、轉傳、改附件說明、`@room`。
- 已讀（/docs/design/messages/read-receipts.md）。
- server 的關聯查詢 `0x14 0x26`–`0x29`：關係事件會跟著推播進來，現在用不到。

## 7. 怎麼驗（實作時）

- **單元**：每支 RPC 用假 server 驗送出的 content **逐字**（§3 的每個例子各一條）；`room.edit` 的 §4.2 每一條拒絕各一條；`m.mentions` 那張表每一列一條；`room.react` 冪等一條；`list_reactions` 去重與 `mine` 各一條。
- **真 server e2e**：alice 回覆、編輯、按表情、收回表情、收回訊息，明文房、加密房各走一次；bob 用 `room.history` 看到折完的結果——
  `reply_to` 有值、`edited_by` 有值、`reactions` 先有後沒有、最後那則是 `deleted`；alice 自己讀時 `mine` 有值、收回後沒有。

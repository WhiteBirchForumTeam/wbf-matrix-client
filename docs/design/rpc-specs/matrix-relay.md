# 其餘的 Matrix 端點：推播規則、搜尋、檢舉、在線狀態、關聯、過濾器……（批 3／批 4 全接）

> 這份回答：server 批 3、批 4 開上通道、房間動作那支沒接的 30 支端點，每支對到哪個 RPC、params 怎麼翻、result 是什麼。
> 本 repo issue #50（批 3）、#55（批 4）；號碼的權威是 wbfuwunel 的 /docs/bridge-specs/index.md §2。
> 房間動作那 40 多支在 /docs/design/rooms/room-actions.md §2，這裡**不**重抄；兩份共用同一張端點表與同一組中轉函式。

## 0. 一句話

UI 叫一個 RPC，daemon 照 Matrix 的名字把 params 填進端點的變數與 body，送出去，把 server 回的 body 原樣交回。
wbf 帳號走橋，一般 Matrix 帳號照同一個 HTTP 端點送（`crates/wbf-sdk/src/matrix_endpoint.rs`，一張表兩條路）。本地**不**存任何東西，所以**沒有** `sync` 參數。

## 1. 規則（沿用 /docs/design/rooms/room-actions.md §1 第 14、15 條）

1. **daemon 是中轉站**（維護者 2026-10-07）：參數 1:1 照 Matrix 的名字與格式，結果是 server 回的 body 原樣。daemon 只擋型別（`102`），值合不合理交 server 擋。
2. **server 開了就全接**（同上，第 15 條）：「現在 UI 還用不到」**不是**少接的理由。
3. **`user`／`server` 已經是「哪個帳號」**（/docs/design/rpc-specs/rpc-spec.md §2）。所以：
   - 路徑上要「自己的 mxid」的（過濾器、設自己的在線狀態），daemon 填目前帳號的 mxid，UI **不**給。
   - 被問到、被檢舉的那個人叫 `user_id`（Matrix 的名字）。
4. **server 拒了是 `1400`**，`data` 是 server 給的 Matrix 錯誤（`status`、`errcode`，/docs/design/rpc-specs/rpc-spec.md §5.3）。
   例外：下表標「沒有是 `null`」的那幾支，404 回 `result: null`，**不是**錯誤（跟 `room.get_state` 同一個規矩）。
5. **body 是 Matrix 物件的那幾支，params 除了 daemon 自己的欄位，剩下的原樣當 body**（`push.set_rule`、`push.set_pusher`；跟 `room.create` 同一個做法）。
   daemon 自己的欄位是 `user`、`server`，以及那一支表上寫的路徑變數與 query 變數。

## 2. RPC

每支都帶 `user?`、`server?`。「示意」是形狀，**不是**跑出來的值；對本機 wbfuwunel 跑出來的回應在 §2.8。

### 2.1 推播規則、pusher、通知（`0x18 Push`，批 4）

| method | params | result | 橋 |
|---|---|---|---|
| `push.get_rules` | `{}` | `{ global: { override, content, room, sender, underride } }`（缺的那類整個不出現，見 §3 第 5 條） | `0x18 0x20` |
| `push.get_global_rules` | `{}` | 上面的 `global` 那一份 | `0x18 0x21` |
| `push.get_rule` | `{ kind, rule_id }` | 那一條規則；**沒有是 `null`** | `0x18 0x22` |
| `push.set_rule` | `{ kind, rule_id, before?, after?, …規則本身 }`（`actions`、`conditions`、`pattern` 原樣進 body） | `{}` | `0x18 0x23` |
| `push.delete_rule` | `{ kind, rule_id }` | `{}` | `0x18 0x24` |
| `push.get_rule_enabled`／`push.set_rule_enabled` | `{ kind, rule_id }`／`{ kind, rule_id, enabled: bool }` | `{ enabled }`／`{}` | `0x18 0x25`／`0x26` |
| `push.get_rule_actions`／`push.set_rule_actions` | `{ kind, rule_id }`／`{ kind, rule_id, actions: [...] }` | `{ actions }`／`{}` | `0x18 0x27`／`0x28` |
| `push.get_pushers` | `{}` | `{ pushers }` | `0x18 0x29` |
| `push.set_pusher` | `{ pushkey, app_id, kind, … }`（整份原樣進 body；`kind: null` 是**移除**） | `{}` | `0x18 0x2A` |
| `push.notifications` | `{ from?, limit?, only? }` | `{ notifications, next_token? }` | `0x18 0x2B` |

- `kind` 是**規則種類**（`override`／`content`／`room`／`sender`／`underride`），**不是** pack 的 kind；`scope` **不是**變數，路徑固定 `global`（wbfuwunel 的 /docs/bridge-specs/index.md §2 `0x18`）。
- 靜音一個房間（示意）：`push.set_rule_actions { kind: "room", rule_id: "!r:localhost", actions: [] }`，或 `push.set_rule_enabled { …, enabled: false }`。

### 2.2 搜尋與使用者目錄（`0x1A Search`，批 4）

| method | params | result | 橋 |
|---|---|---|---|
| `search.events` | `{ search_categories, next_batch? }` | server 的 body | `0x1A 0x20` |
| `search.users` | `{ search_term, limit? }` | `{ results, limited }` | `0x1A 0x21` |

- `search_categories` 原樣進 body（示意：`{ room_events: { search_term: "週末", filter: { rooms: ["!r:localhost"] } } }`）。
- 翻頁：`next_batch` 在**回覆是 body 欄位**（`search_categories.room_events.next_batch`），在**請求是 query 變數**——從回覆拿出來，放進下一次的 `next_batch`。

### 2.3 關聯、討論串、跳到某天（`0x14 Event`，批 3、批 4）

| method | params | result | 橋 |
|---|---|---|---|
| `room.relations` | `{ room, event_id, rel_type?, event_type?, from?, to?, dir?, limit?, recurse? }` | `{ chunk, next_batch?, prev_batch?, recursion_depth? }` | 沒給 `rel_type` → `0x14 0x26`；給了 → `0x27`；再給 `event_type` → `0x28`。只給 `event_type` 沒給 `rel_type` 是 `102` |
| `room.threads` | `{ room, from?, include?, limit? }` | `{ chunk, next_batch? }` | `0x14 0x29` |
| `room.timestamp_to_event` | `{ room, ts, dir }`（`ts` 毫秒、`dir` 是 `"f"`／`"b"`，兩個都必填，跟 Matrix 一樣） | `{ event_id, origin_server_ts }` | `0x14 0x2A` |

### 2.4 在線狀態（`0x15 Receipt`，批 3）

| method | params | result | 橋 |
|---|---|---|---|
| `presence.get` | `{ user_id }` | `{ presence, last_active_ago?, status_msg?, currently_active? }` | `0x15 0x23` |
| `presence.set` | `{ presence, status_msg? }`（路徑的 `user_id` 是自己，daemon 填） | `{}` | `0x15 0x24` |

### 2.5 過濾器（`0x12 Sync`，批 3）

| method | params | result | 橋 |
|---|---|---|---|
| `filter.create` | `{ filter }`（過濾器本身，原樣當 body） | `{ filter_id }` | `0x12 0x21` |
| `filter.get` | `{ filter_id }` | 過濾器本身；**沒有是 `null`** | `0x12 0x20` |

路徑的 `user_id` 一律是自己（daemon 填）：拿別人的 `user_id` 配自己的 token，server 回 404 不是 403（本 repo issue #50 坑 1），UI 會讀成「沒有這個過濾器」。

### 2.6 檢舉（`0x1D Report`，批 3）

| method | params | result | 橋 |
|---|---|---|---|
| `report.event` | `{ room, event_id, reason?, score? }` | `{}` | `0x1D 0x20` |
| `report.room` | `{ room, reason? }` | `{}` | `0x1D 0x21` |
| `report.user` | `{ user_id, reason? }` | `{}` | `0x1D 0x22` |

`reason` 規格上檢舉房間、使用者是必填，daemon **不**擋（規則 1），沒給的話由 server 回 400（`M_BAD_JSON`，§2.8）。
📎 這台 server 把檢舉轉成管理房間裡的一則 `@room` 訊息；管理員帳號的 `push.notifications` 會看到它。

### 2.7 server 的能力、媒體設定、連結預覽、TURN（`0x1C`、`0x19`、`0x1B`）

| method | params | result | 橋 |
|---|---|---|---|
| `server.capabilities` | `{}` | `{ capabilities }` | `0x1C 0x20`（批 3） |
| `media.config` | `{}` | `{ "m.upload.size"? }` | `0x19 0x20`（批 4） |
| `media.preview_url` | `{ url, ts? }` | OpenGraph 的欄位（`og:title`、`og:image`…） | `0x19 0x21`（批 4） |
| `voip.turn_server` | `{}` | `{ username, password, uris, ttl }`；**這台沒有 TURN 是 `null`** | `0x1B 0x20`（批 4） |

- `media.preview_url` 的 403（`1400`）是「這台不給預覽」：整個關掉，或這個網址不准（例：內網位址，§2.8），**不是**連結壞了。`ts` 這台 server 收下後忽略。
- `voip.turn_server` 的 404 是 MSC4166 的「沒設定 TURN」，讀成「通話只能直連」，所以回 `null`。憑證有 `ttl`（秒），過期要重拿。

### 2.8 跑出來的回應（2026-11-08，本機 wbfuwunel）

result 就是 server 的 body（走 HTTP 與走橋一樣，§7）。房間 id、事件 id 換成 `<…>`。

| RPC | 回應 |
|---|---|
| `media.config` | `{"m.upload.size": 25165824}` |
| `push.get_rule { kind: "room", rule_id: "<房間>" }`（剛用 `actions: []` 寫完） | `{"actions": [], "default": false, "enabled": true, "rule_id": "<房間>"}` |
| 同上，`push.delete_rule` 之後 | `null`（server 是 404 `M_NOT_FOUND`） |
| `push.get_rule_enabled` | `{"enabled": true}` |
| `push.get_global_rules` 的鍵（全新帳號） | 只有 `override`、`underride`（§3 第 5 條） |
| `filter.create { filter: { room: { timeline: { limit: 7 } } } }` | `{"filter_id": "gaXD"}` |
| `presence.set { presence: "online", status_msg: "lunch" }` → `presence.get` | `{"status_msg": "lunch", "currently_active": true, "presence": "online"}` |
| `room.timestamp_to_event { ts: <現在＋60 秒>, dir: "b" }` | `{"event_id": "<…>", "origin_server_ts": 1794082243786}` |
| `search.users { search_term: "bob", limit: 5 }` | `{"results": [{"user_id": "@bob:localhost", "display_name": "bob 🍃"}], "limited": false}` |
| `voip.turn_server`（這台沒設 TURN） | `null`（server 是 404） |
| `media.preview_url { url: "http://127.0.0.1:9/" }` | `1400`，`data` 是 403 `M_FORBIDDEN`（「URL is not allowed to be previewed」） |
| `report.user { user_id: "@bob:localhost" }`（沒給 `reason`） | `1400`，`data` 是 400 `M_BAD_JSON` |
| `server.capabilities` | `{"capabilities": {"m.change_password": {"enabled": true}, "m.room_versions": {"default": "11", "available": {…}}, "m.set_displayname": …, …}}` |

## 3. 寫的時候容易錯的（本 repo issue #50、#55 列的，跟這支有關的那幾條）

1. 推播規則的 `kind` 打錯是 **404**，跟「規則不存在」分不出來（`push.get_rule` 兩種都回 `null`）。
2. `SetPusher` 的 `kind: null` 是移除；`pushkey`＋`app_id` 是身分，同一組再送是覆蓋。daemon 原樣送，**不**把 `null` 拿掉。
3. `SearchEvents` 的 `next_batch` 一個名字兩個位置（§2.2）。
4. **搜尋在加密房搜不到內容**：server 看到的是密文。UI 要講清楚，別讓使用者以為「搜不到＝沒有」。
5. 讀規則時，一條規則都沒有的那類鍵**整個不出現**，不是空陣列：全新帳號只看得到 `override` 與 `underride`。
6. 連結預覽關掉是 403；TURN 沒設定是 404（§2.7）。
7. 房間摘要攤平、`MutualRooms` 的 `user_id` 在 query：房間動作那支已經照做（/docs/design/rooms/room-actions.md §2.1）。

## 4. 要維護者決定的（照預設做了）

1. **加密房的事件原樣回，daemon 不解**：`room.relations`、`room.threads`、`push.notifications` 回的事件在加密房是 `m.room.encrypted`。
   預設照規則 1 原樣回；UI 手上沒有金鑰，所以在加密房這三支現在只看得到「有幾則、誰送的、什麼關聯」（`m.relates_to` 在密文外面）。
   要解的話，做法是比照 `room.history` 用同一套補解（/docs/design/keys/e2ee-rpc.md §6），另開一支。
2. **`push.get_rule`、`filter.get`、`voip.turn_server` 的 404 回 `null`**，其他照 `1400` 回錯（規則 4）。
3. **RPC 的名字**：`push.*`、`search.*`、`presence.*`、`filter.*`、`report.*`、`voip.*` 是新的名詞；關聯三支併成一支 `room.relations`（看給了哪些參數選端點，跟 `room.public_rooms` 同一個做法）。

## 5. 不在這支（server 也開了、client 還沒接的；另開一支或等決定）

| 端點 | 為什麼不在這裡 |
|---|---|
| `0x14 0x20` GetEvent、`0x25` Context | 「跳到訊息」那條，等 wbfuwunel #64（/docs/handover.md §6） |
| `0x15 0x21` ReadMarkers | 維護者 2026-11-05：`m.fully_read` 這版不做（/docs/design/messages/read-receipts.md §8） |
| `0x15 0x20` Typing | 送得出去，但 server 不推別人的「輸入中」，收不到就沒有用（/docs/design/messages/read-receipts.md §8 同一個原因） |
| `0x11 0x21`–`0x24` profile、`0x16 0x20`–`0x22` 裝置清單與改名 | 批 1 的，不在 #50、#55；接法跟這支一樣，下一支 |
| `0x11 0x2C` ChangePassword、`0x2D` Deactivate、`0x16 0x23`–`0x24` 刪裝置、`0x10` 註冊 | 要 UIAA（兩輪、帶密碼），形狀要另外定 |
| `0x17` 其餘的備份端點（刪版本、逐房讀寫） | 刪版本不可逆（/docs/design/keys/room-key-backup.md §9）；逐房下載是備份超過 2 MiB 那條的解法，另開 |

## 6. 程式碼在哪

| 層 | 檔案 | 做什麼 |
|---|---|---|
| wire | `crates/wbf-wire/src/pack.rs` | `Kind::Sync`（`0x12`）、`Push`（`0x18`）、`Media`（`0x19`）、`Search`（`0x1A`）、`Voip`（`0x1B`）、`Misc`（`0x1C`）、`Report`（`0x1D`） |
| sdk | `crates/wbf-sdk/src/matrix_endpoint.rs` | 30 支端點的列（橋的號碼＋HTTP 的 method、路徑、query 變數） |
| daemon | `crates/wbf-daemon/src/handle/matrix_relay.rs` | 每支 RPC 的參數翻譯；中轉函式跟房間動作共用（`handle/room_actions.rs` 的 `pass_through`、`find_through`） |
| core | 不改 | `Core::call_matrix_endpoint`、`find_via_matrix_endpoint`（`crates/wbf-core/src/room_actions.rs`）就是兩條路的分岔點 |

## 7. 怎麼驗

- **單元**：每支端點請求的前 4 bytes、路徑模板、query 變數，照 wbfuwunel 的 /docs/bridge-specs/index.md §2 逐列抄成字面值比（`crates/wbf-sdk/tests/unit.rs`）；`/pushrules/` 結尾的斜線、關聯的 v1 路徑填成的 URL（`matrix_endpoint.rs`）；`room.relations` 選哪一支端點。
  ⚠️ query 變數少一個（例 `SetPushRule` 的 `after`）時，帶它的請求被 `check_variables` 拒成 `1100`，「走到 core」的參數表照樣綠，所以 query 要另外逐字比。
- **daemon 的兩張參數表**（`crates/wbf-daemon/src/handle/mod.rs`）：28 支都走到 core；型別不對、缺必填、`event_type` 沒有 `rel_type` 是 `102`。
- **真 server（本機 wbfuwunel）**：
  - sdk 層：每支端點走 HTTP 與走橋回的一樣（`crates/wbf-sdk/tests/e2e_local_server.rs`）。
  - daemon 層：從 RPC 走一輪，確認 params 翻得對（`crates/wbf-daemon/tests/real_server.rs`）。`push.set_rule` 帶 `after` 之後讀回 `room` 那類的順序：
    daemon 漏轉 `after` 時 server 照樣 200（新規則排到最前面），只有順序看得出來。

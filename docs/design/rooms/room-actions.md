# 房間動作：建房、成員、權限、置頂（維護者 2026-10-07 逐條定）

> §1 是定了的規則，§2–§5 是每支 RPC 與本地寫什麼（權威；/docs/design/rpc-specs/rpc-spec.md §3.3.1 只列 method 名），§6 是還要維護者定的，§7 是程式碼在哪。
> 範圍：/docs/handover.md §7 第 2 項的「房間動作」那一半。訊息動作（回覆、編輯、收回、表情）與已讀排在後面幾支（維護者 2026-10-07）。

## 0. 一句話

UI 叫一個 RPC，daemon 做一個房間動作，回 server 給的結果。會改到「誰在不在這間房」的動作帶 `sync`：`both`（預設）收到 ACK 之後把本地的房間列一起改掉，`server` 只打遠端。

## 1. 已定的（維護者 2026-10-07）

1. **這支只做房間動作**：建房、加入、退出、忘記、邀請、踢人、封鎖、解除封鎖、改名、改主題、改權限、置頂。
2. **建 Direct 一律建新房**：🚫 先找既有的 Direct（daemon 🚫 替 UI 串動作）。要不要沿用舊房由 UI 從房間列表判斷（/docs/design/rooms/chat-model.md §3.1）。
3. **`is_direct: true` 時 `room.create` 順便寫 `m.direct`**：建完房，daemon 讀帳號資料的 `m.direct`、把「每個 `invite` 的人 → 這間房」加進去、寫回。Matrix 規格要 client 做這件事；不寫的話，別的 client 不會把它當成一對一。
   這一步失敗🚫 回錯（房已經建好了），推一則 `note` 請 UI 叫 `room.set_direct`。
4. **channel 與權限全交給 UI**：daemon 🚫 認得「channel」。UI 自己「建房＋改權限」兩步；daemon 只負責**權限怎麼寫**（照 Matrix 的 `m.room.power_levels` 格式），
   要設多少、限制到哪、哪些人是什麼等級全是 UI 的事，daemon 🚫 檢查值（server 照 auth rules 擋，§4）。
5. **建房時就指定加不加密**：Matrix 的 `createRoom` 支援（`initial_state` 帶 `m.room.encryption`，規格的標準寫法；wbfuwunel 的 /docs/bridge-specs/0x13-room.md `0x20` 也寫明「加密房間放在 `initial_state` 裡」），
   所以 `room.create` 開放 `encrypted` 給 UI，而且**必填**（/docs/design/rooms/chat-model.md §3.6「底層不帶立場」，預設值是 UI 的事）。
   另外有一支 **`room.enable_encryption`** 把既有的明文房改成加密（維護者 2026-10-07：「這是 feature，不是精簡的理由」，§2、§3.2）。
6. **狀態的讀寫：通用的一組＋窄的方便版都給**（維護者 2026-10-07 第二次定，取代同日的「🚫 通用 `set_state`」）：`room.get_state`／`room.set_state` 照 Matrix 的狀態事件原樣讀寫；
   改名、改主題、頭像、別名、歷史可見性、加入規則、訪客存取、加密、權限、置頂另各有一支窄的（底下都是 `set_state`；權限與置頂由 daemon 讀出目前那一項、改給了的部分、再寫回）。
7. **`sync` 只收 `both`、`server`，預設 `both`**：`both` 收到 server 的 ACK 才寫本地；`server` 🚫 碰本地。`local` 不合法（動作一定要打到 server）。
8. **本地記每個帳號在房裡的身分**：`membership` 是 `join`（在裡面）／`invite`（被邀請）／`leave`（主動離開，或被踢）／`ban`（被封鎖）。退出🚫 刪列，只把它改成 `leave`（§3）。
9. **`room.forget` 才刪列**：刪掉這個帳號那一列；這間房在本機只剩這個帳號的話，連本地的聊天紀錄一起清掉（§3.1）。池檔🚫 當場刪，留給掃描。
10. **`joined` 換成 `membership`**（🚫 並存）：schema v10。
11. **動作只做動作**：`room.create` 之後 daemon 🚫 自己補拿房間資料；UI 要就叫 `room.get`，daemon 那時才去拿、`sync=both` 存回本地。
12. **成功以 server 的 ACK 為準**：本地寫不進去照樣回成功，另推一則 `note`。
13. **`room.get` 拿到什麼就只多不少地給 UI**：置頂清單、權限的整份內容……都在它本來就拿的房間狀態裡，放進 `Conversation` 給出去（§5.1，整份狀態、成員事件以外全拿），🚫 讓 UI 另外再問。
14. **daemon 只是中轉站，一切跟上游一樣**（維護者 2026-10-07）：參數 1:1 照 Matrix 的名字與格式（`room.create` 的 12 個參數全開），結果是 server 回的 body 原樣。
    daemon 自己加的只有兩件：`encrypted`（必填，轉成 `initial_state` 的 `m.room.encryption`）、`is_direct: true` 時順便寫 `m.direct`（第 3 條）。
    草案自己發明的 `public`／`direct` 拿掉，改用 Matrix 的 `preset`、`visibility`、`is_direct`。
15. **房間相關的橋全接**：server 的 WS 幾乎全開，沒理由不用（§2 的清單）。太細、先不加的列在 §2.4。
16. **收邀請等 server 做完再補**（§3.3）：已開 wbfuwunel #111（維護者要求優先）；這支🚫 接邀請的推送與補拿（維護者 2026-10-07 開工時定：「invite 的推播語意先不做，等 server 完成後補」）。
    `membership` 的 `invite` 這支只由 `room.invite` 邀本機帳號時寫（§3）。

## 2. RPC

每支都帶 `user?`、`server?`（/docs/design/rpc-specs/rpc-spec.md §2）。**參數照 Matrix 的名字與格式，結果是 server 回的 body 原樣**（§1 第 14 條）；server 回的錯照原樣往上帶：`1400`，`data` 是 server 的 Matrix 錯誤（例：權限不夠是 `status` 403、`errcode` `M_FORBIDDEN`，/docs/design/rpc-specs/rpc-spec.md §5.3）。
⚠️ `user`／`server` 已經是「哪個帳號」，所以**被動到的那個人叫 `user_id`**（Matrix body 本來就是這個名字）、公開目錄問哪一台叫 `directory_server`（Matrix 的 query 是 `server`，對不上是不得已）。
**一張端點表、兩條路**（`crates/wbf-sdk/src/matrix_endpoint.rs`）：wbf 帳號走 wbfuwunel 的橋（wbfuwunel 的 /docs/bridge-specs/0x13-room.md、0x14-event.md、0x11-account.md，下表只寫 kind 與編號）；
一般 Matrix 帳號照同一個 HTTP 端點送（同一份變數、同一份 body、同一個「變數合不合橋的規則」檢查）。🚫 各端點去找 matrix-sdk 有沒有現成方法：那會是兩份實作、遲早漂。
matrix-sdk 的 store 下一次 sync 就看得到這些動作的結果（房間的讀都先 sync 一次）。
「批 3／批 4」是 server 後來才開的端點，接之前先讀本 repo issue #55 列的十個坑（/docs/handover.md §6）。

### 2.1 成員與房間本身（Room kind）

| method | params（除了 `sync?`、`user?`、`server?`） | result | wbf 帳號 |
|---|---|---|---|
| `room.create` | `{ encrypted: bool（必填）, name?, topic?, invite?, invite_3pid?, room_version?, room_alias_name?, visibility?, preset?, is_direct?, initial_state?, creation_content?, power_level_content_override? }` | `{ room_id }` | Room `0x20`；`is_direct` 時再走 Account `0x25`／`0x26`（讀、寫 `m.direct`） |
| `room.join` | `{ room: "!id" \| "#別名", via?, reason? }` | `{ room_id }` | Room `0x21` |
| `room.knock` | `{ room: "!id" \| "#別名", via?, reason? }` | `{ room_id }` | Room `0x2E`（批 3） |
| `room.leave` | `{ room, reason? }` | `{}` | Room `0x22` |
| `room.forget` | `{ room }` | `{ history_cleared: bool }`（§3.1；daemon 加的欄位） | Room `0x23`（server 要先 leave，不然 400） |
| `room.invite` | `{ room, user_id, reason? }` | `{}` | Room `0x24`（被邀的人在 body） |
| `room.kick`／`room.ban`／`room.unban` | `{ room, user_id, reason? }` | `{}` | Room `0x25`／`0x26`／`0x27` |
| `room.upgrade` | `{ room, new_version }` | `{ replacement_room }` | Room `0x2D`（批 3） |
| `room.members` | `{ room, membership?, not_membership? }` | server 的 body（`chunk` 加 wbfuwunel 的 `org.wbftw.room_version`） | Room `0x29` |
| `room.joined_members` | `{ room }` | `{ joined: { mxid: { display_name, avatar_url } } }` | Room `0x2F`（批 3） |
| `room.summary` | `{ room: "!id" \| "#別名", via? }` | server 的 body | Room `0x32`（批 3）。**還沒加入也能問**：被邀請時拿房名的另一條路 |
| `room.hierarchy` | `{ room, from?, limit?, max_depth?, suggested_only? }` | server 的 body | Room `0x33`（批 3，space） |
| `room.mutual_rooms` | `{ user_id, from? }` | server 的 body | Room `0x34`（批 3） |
| `room.public_rooms` | `{ directory_server?, limit?, since?, filter?, include_all_networks?, third_party_instance_id?, room_types? }`（給了 `filter`／`include_all_networks`／`third_party_instance_id`／`room_types` 任一個就走 Filtered，`limit`／`since` 一起放進 body） | server 的 body | Room `0x35`／`0x36`（批 4） |
| `room.get_visibility`／`room.set_visibility` | `{ room }`／`{ room, visibility }` | server 的 body | Room `0x30`／`0x31`（批 3，公開目錄） |
| `room.resolve_alias` | `{ alias }` | `{ room_id, servers }` | Room `0x2A` |
| `room.set_alias`／`room.delete_alias` | `{ alias, room }`／`{ alias }` | `{}` | Room `0x2B`／`0x2C` |
| `room.aliases` | `{ room }` | `{ aliases }` | Room `0x37`（批 4） |

`room.create`：除了 `encrypted`，每個參數都**原樣**進 CreateRoom 的 body（名字、格式、預設值都是 Matrix 的，daemon 🚫 補預設）。`encrypted: true` 時 daemon 在 `initial_state` 加
`{ "type": "m.room.encryption", "state_key": "", "content": { "algorithm": "m.megolm.v1.aes-sha2" } }`；UI 自己在 `initial_state` 放了 `m.room.encryption` 而 `encrypted` 給 `false`（兩邊說的相反）→ 用法錯（`1100`，`crates/wbf-sdk/src/room_state_edit.rs` 的 `to_create_room_body`）；沒帶 `encrypted` 是參數錯（`102`）。
UI 放的那則 `m.room.encryption` 要是 server 會當成加密的那種（`state_key` 是空的或省略——`initial_state` 省略時就是空字串，Matrix 規格——、content 有非空的 `algorithm`，跟寫完狀態升 `rooms.encrypted` 是同一句：`crates/wbf-sdk/src/room_state.rs` 的 `is_encryption_event`），不是就 `1100`：🚫 當成「已經加密」而不補——那會建出明文房、本地卻記成加密（PR #76 審查）。
📎 想建「公開可搜」的房（/docs/design/rooms/chat-model.md §3.6）：`preset: "public_chat"`＋`initial_state` 帶 `m.room.history_visibility: world_readable`，要列進目錄再加 `visibility: "public"`——全是 UI 組。

### 2.2 房間狀態（Event kind 的 `0x21`–`0x23`）

| method | params | result | 底下 |
|---|---|---|---|
| `room.get_state` | `{ room, event_type, state_key?: "" }` | 那一項的 content；沒有這一項 → `null` | Event `0x22` |
| `room.set_state` | `{ room, event_type, state_key?: "", content }` | `{ event_id }` | Event `0x23` |
| `room.set_name` | `{ room, name }` | `{ event_id }` | `set_state`（`m.room.name`） |
| `room.set_topic` | `{ room, topic }` | `{ event_id }` | `set_state`（`m.room.topic`） |
| `room.set_avatar` | `{ room, url }`（mxc） | `{ event_id }` | `set_state`（`m.room.avatar`） |
| `room.set_canonical_alias` | `{ room, alias?, alt_aliases? }`（都沒給就寫 `{}`：清掉正式別名，Matrix 合法） | `{ event_id }` | `set_state`（`m.room.canonical_alias`） |
| `room.set_history_visibility` | `{ room, history_visibility: "world_readable" \| "shared" \| "invited" \| "joined" }` | `{ event_id }` | `set_state`（`m.room.history_visibility`） |
| `room.set_join_rule` | `{ room, join_rule: "public" \| "invite" \| "knock" \| "restricted" \| "knock_restricted" \| "private", allow? }` | `{ event_id }` | `set_state`（`m.room.join_rules`） |
| `room.set_guest_access` | `{ room, guest_access: "can_join" \| "forbidden" }` | `{ event_id }` | `set_state`（`m.room.guest_access`） |
| `room.enable_encryption` | `{ room }` | `{ event_id }` | `set_state`（`m.room.encryption`，§3.2） |
| `room.set_power_levels` | `{ room, users?, users_default?, events_default?, state_default?, events?, invite?, kick?, ban?, redact?, notifications? }` | `{ event_id }` | 讀 `m.room.power_levels` → 合併（§4.2）→ `set_state` |
| `room.pin` | `{ room, event_id, pinned: bool }` | `{ event_id? }`（已經是那個狀態就🚫 寫、沒有 `event_id`） | 讀 `m.room.pinned_events` → 加或拿掉 → `set_state`（§5） |

窄的那幾支的值 daemon 🚫 檢查合不合理（server 照 auth rules 擋）；列舉值寫在表裡是給 UI 看的 Matrix 規格，daemon 只擋「型別不對」（例：`history_visibility` 不是字串）。

### 2.3 房間層的帳號資料與標籤（Account kind）

| method | params | result | 底下 |
|---|---|---|---|
| `room.get_tags` | `{ room }` | `{ tags: { tag: { order? } } }` | Account `0x29` |
| `room.set_tag`／`room.delete_tag` | `{ room, tag, order? }`／`{ room, tag }` | `{}` | Account `0x2A`／`0x2B`（`m.favourite`、`m.lowpriority`、`u.自訂`） |
| `room.get_account_data`／`room.set_account_data` | `{ room, event_type }`／`{ room, event_type, content }` | content（沒有是 `null`）／`{}` | Account `0x27`／`0x28` |
| `account.get_data`／`account.set_data` | `{ event_type }`／`{ event_type, content }` | content（沒有是 `null`）／`{}` | Account `0x25`／`0x26`（帳號層，例 `m.direct`） |
| `room.set_direct` | `{ room, user_id, direct: bool }` | `{}`（已經是那個狀態就🚫 寫） | 讀 `m.direct` → 加或拿掉「`user_id` → `room`」（那個人的清單空了就把他拿掉）→ 整份寫回（§2.5） |

### 2.4 太細、先不加的（維護者看過再說）

- **訊息層的**（Event `0x20` GetEvent、`0x24` Redact、`0x25` Context、`0x26`–`0x29` Relations／Threads、`0x2A` TimestampToEvent）：排在「訊息動作」那一支（§0 的範圍）；Context 是「跳到訊息」，等 wbfuwunel #64。
- **已讀、輸入中**（Receipt kind）：排在「已讀」那一支。
- **帳號本身**（profile、改密碼、停用帳號）：不是房間的事。
- `m.room.server_acl`（擋哪些 server 的事件）、`m.room.tombstone`（手動寫墓碑；升級由 `room.upgrade` 自己寫）：聯邦還沒開，現在用不到；要的話 `room.set_state` 寫得出來。

### 2.5 `is_direct` 之後能不能改（維護者問過，2026-10-07）

`is_direct` **不是房間的屬性**，Matrix 裡它分兩處：
- 邀請那一則 `m.room.member` 的 content 上的 `is_direct: true`（建房時的 `invite` 會帶上）——那是一則已經發出去的事件，改不了，但它只是「邀請時的提示」。
- **每個人自己的帳號資料 `m.direct`**（`{ "@bob:x": ["!r1", "!r2"] }`）：誰把這間房當一對一，是各人自己記的，自己隨時能改。

房間本身就是普通的房：有邀請權限的人隨時可以再邀第三個人，server 🚫 擋。我們的模型照 /docs/design/rooms/chat-model.md §3.1 判斷：「`m.direct` 裡有它**且**已加入的成員剛好兩個」才是 `Direct`，所以第三個人加入之後 `room.get` 自動變成 `Group`。
別的 client（例 Element）多半只看 `m.direct`，所以人變多之後要讓它們也不當一對一，就是把這間房從自己的 `m.direct` 拿掉：`room.set_direct { direct: false }`。對方的 `m.direct` 是對方的，我們改不了。

## 3. `sync=both` 時本地寫什麼

`cache.db` 的 `room_list` 是「一個帳號一列」——主鍵是（帳號, 房間），`membership` 記這個帳號在這間房的身分：`join`／`invite`／`knock`（敲門等回應，`room.knock` 才有）／`leave`／`ban`（§1 第 8、10 條，schema v10，/docs/design/storage/local-cache-db.md §5）。
退出的🚫 刪列、標 `leave`；刪列只有 `room.forget`（§3.1）。
例：A 建房、B 在裡面、C 不在 → A、B 各一列（`conversation_json` 是各自上次 `room.get` 看到的樣子），C 沒有列。
「在房裡」＝ `membership = join`；`room.list` 預設只列 `join` 的，另收 `membership?: ["join", "invite", …]` 要哪幾種自己挑，每列帶 `membership`。

**帶 `sync` 的是會改本地的那幾支**：成員動作（§2.1 的 create、join、knock、leave、forget、invite、kick、ban、unban、upgrade）與狀態的寫入（§2.2 的 `set_state` 與它所有的窄版）。
純讀的（members、summary、hierarchy、public_rooms、get_state、tags、account data…）本地沒存、🚫 帶 `sync`，一律問 server。標籤與帳號資料本地也沒存，寫入🚫 帶 `sync`。

| method | `both`：ACK 之後寫 |
|---|---|
| `room.create` | 建房者那一列 `membership = join`、`conversation_json` 空著（`refreshed_at` 空 ＝ 還沒 `room.get` 過）；`rooms.encrypted` 寫建房時給的值（`room.send_text` 只信本地的 `rooms.encrypted`，建完馬上能送靠的是這一筆）。`conversation_json` 留空：它是一整份 `Conversation`，只填幾欄等於說謊（`member_count`、`can_send_message`…）；UI 要就叫 `room.get`（§1 第 11 條） |
| `room.join` | 自己那一列 `membership = join`（列不在就加；`conversation_json` 不動） |
| `room.knock` | 自己那一列 `membership = knock`（列不在就加） |
| `room.leave` | 自己那一列 `membership = leave`，🚫 刪、`conversation_json` 留著 |
| `room.forget` | §3.1 |
| `room.invite` | 被邀請的人**是這台機器登入的帳號**（同一台 server；用它的 localpart 算出目錄、解開 session、mxid 逐字比對——`@bob:別台` 會算到同一個目錄，對不上就不是，fail closed；🚫 看 `cache.db` 的 `users`：那裡連事件的 sender 都有）→ 它那一列 `membership = invite`（列不在就加）。不是本機帳號就🚫 寫 |
| `room.kick` | 被踢的人是本機帳號 → 它那一列 `membership = leave`（Matrix 裡被踢之後就是 `leave`；列不在就加） |
| `room.ban` | 被封鎖的人是本機帳號 → 它那一列 `membership = ban`（列不在就加） |
| `room.unban` | 被解除的人是本機帳號、它那一列是 `ban` → 改成 `leave`（Matrix 裡解除封鎖之後是 `leave`，要再邀請或自己加入才回來） |
| `room.upgrade` | 新房（`replacement_room`）加一列 `membership = join`、`conversation_json` 空著；舊房那一列🚫 動（還在舊房裡，舊房多了墓碑，下次 `room.get` 看得到） |
| `room.set_state` 與它所有的窄版 | 自己那一列有拿過狀態（`conversation_json` 帶 `state`）就把那一項換成這次寫的，再用整份 `state` 重算型別欄位（`name`、`topic`、`encrypted`、`my_power_level`、`can_send_message`、`kind`；成員那幾欄 `member_count`、`direct_peer` 照舊；跟 `room.get` 同一套規則，`crates/wbf-sdk/src/room_state.rs` 的 `conversation_with_state_event`）。沒拿過狀態（還沒 `room.get` 過、或一般 Matrix 帳號只在 `room.list` 上）就🚫 寫。那份 `state` 是列表留下來的（`room_list.is_state_carried_over`：一般 Matrix 帳號的 `room.list` 帶來新的型別欄位、狀態則是上次 `room.get` 的）也🚫 寫：拿舊狀態重算會把新的名字、權限改回舊的。重算不出來（例：存著的狀態沒有 `m.room.create`）也🚫 寫那一列、照樣回成功；下一次 `room.get` 整份蓋上。其他帳號那一列是它們自己看到的樣子，下次它們 `room.get` 才更新。⚠️ 名字：沒有名字也沒有別名的房，名字是用成員湊的（成員不在 `state` 裡）——原本就是湊的照舊；原本是名字或別名、這次把它拿掉了，就變 `null`，下次 `room.get` 才湊得回來（🚫 留著已經不成立的舊名字） |
| 寫的是 `m.room.encryption`（`room.enable_encryption`，或 `room.set_state` 直接寫它） | 上一列之外，`rooms.encrypted = 1`，不管有沒有 `conversation_json`、上一列寫沒寫（重算失敗🚫 把它一起回滾）（§3.2；這一欄是房間的事實，所有帳號共用、只升不降） |

`server`：一律只打遠端、🚫 碰本地。
**寫本地失敗**（DB 壞了）：成功以 server 的 ACK 為準，照樣回成功，另推一則 `note`（例：`room.leave: !r:localhost was left on the server, but the local room list could not be updated (…); call room.list sync=both`）。🚫 回錯：回錯 UI 會以為動作沒做而重做一次（維護者 2026-10-07）。

### 3.1 `room.forget`：刪列，只剩自己時連聊天紀錄一起清

1. 先打 server 的 Forget（`sync=server` 到這裡就停）。
2. `both`：刪這個帳號那一列 `room_list`、它的已讀位置（`read_positions`）、它在這間房的事件可見性（`events_synced_log` 裡這間房的事件）。
3. 看這間房在本機**還有沒有別的帳號**：別的帳號還有 `room_list` 列、**或**還看得到這間房的任何一則事件（`events_synced_log`）。
   - 有 → 到此為止：房間與事件留給它們（回 `history_cleared: false`）。
   - 沒有 → 刪 `rooms` 那一列：`events`、`room_list`、`read_positions` 都是 ON DELETE CASCADE，本地這間房的聊天紀錄一次清光；事件與媒體的連結（`event_media`）跟著事件沒了，
     這間房的事件本來指著、而現在已經沒有任何事件指著的 `media` 列一起刪（只刪這些；沒綁事件的列——例如這台機器剛傳、事件還沒回來的——🚫 動）。
     **池檔🚫 當場刪**（維護者 2026-10-07）：列沒了之後沒人指著它，下次掃描（`media::sweep`，/docs/design/media/media-pool.md §5）收。回 `history_cleared: true`。
   - 📎 媒體跟訊息的綁定：`event_media`（事件 ↔ `media` 列，ON DELETE CASCADE）。同一個 mxc 可以被好幾間房的事件指著（轉發），所以只刪「清完之後一個連結都不剩」的列。
   - 判斷「還有沒有別人」兩個都看（維護者的條件是「同一個房間 id 的列 ≤ 1」）：只看 `room_list` 的話，別的帳號同步過這間房的事件、卻還沒有列（例：`sync.recent` 拉到、還沒 `room.list`），它的紀錄會被一起清掉；不確定就不清（fail closed）。
4. 本地那半一個 transaction 做完：中途失敗就本地整個沒動；因為 server 已經 ACK，照 §3 回成功（`history_cleared: false`）、另推一則 `note`。
   之後再叫一次 `sync=both` 會重打一次 Forget——server 對已經忘記的房回什麼還沒對真 server 試過（e2e 只走了第一次）。

### 3.2 `room.enable_encryption`：把明文房改成加密

- 寫 `m.room.encryption`（`{ "algorithm": "m.megolm.v1.aes-sha2" }`）。Matrix 的加密是**單向**的：開了就關不掉，🚫 有「關加密」的 RPC。
- 改它要的等級是 `events["m.room.encryption"]`（wbfuwunel 建房預設 100）。不夠 → server 回 403。
- `both`（預設）：ACK 之後本地 `rooms.encrypted = 1`（`rooms.encrypted` 本來就只升不降，/docs/design/storage/local-cache-db.md §5）。之後 `room.send_text` 就走加密那條。
- ⚠️ **`server` 很危險，只給除錯用**（維護者 2026-10-07）：server 上已經加密、本地還記著明文，`room.send_text` 信本地的標記，會把訊息**明文**送進加密房，直到下一次 `room.get sync=both` 把標記升上去。
  所以 rpc-spec 要把這個值標成「除錯用」。
- 已經加密的房再叫一次：daemon 🚫 先問，照送；server 收了就是多一則一樣的狀態事件，本地本來就是 1。

### 3.3 收邀請（issue #77；草案，等維護者點頭）

server 那半是 wbfuwunel PR #113（issue #111），權威是它的 /docs/design/events/invites-on-the-wire.md（下面簡稱「server §n」）。
維護者 2026-10-14 定的：**跟房間線共用**、**開線只訂閱、補拿是 UI 叫的 RPC**（§3.3.2）、**daemon 加一個「是不是本站說的」欄位**（§3.3.4）、**一般 Matrix 帳號走甲**（§3.3.5）。

**現況**：wbf 帳號收不到邀請——訂閱線只推已加入的房、`JoinedRooms` 只列已加入的、還沒加入時讀房間狀態是 403。Matrix 也沒有「列出我被邀請的房」的端點，邀請只從 `/sync` 來，所以 server 造了自己的一條流 `0x05 Invite`。

#### 3.3.1 線上的形狀（server §5，摘要）

| subtype | 方向 | 要點 |
|---|---|---|
| `0x04 Subscribe` | client → server | 回 `{ latest_count }`；之後的 `Push`／`Gone` 抄這個 `id` |
| `0x01 Fetch` | client → server | `{ ci_seq?, batch? }`，回一串 `0x02 Batch`；**只走 WS** |
| `0x02 Batch` | server → client | `{ tc, bc, os?, ns?, counts, r }`，data 是 `bc` 間房，每間 `{ room_id, inviter?, is_direct, reason?, state }`；**`r == 0` 才是補完**；`bc == 0` 時🚫 帶 `os`／`ns` |
| `0x06 Push` | server → client | `{ room_id, inviter?, is_direct, reason?, is, sc, gap }`，data 是 `sc` 則縮減狀態；`sc == 0` 只會是太大被降級（server §5.3） |
| `0x07 Gone` | server → client | `{ room_id, membership, is, gap }`；`membership` 是 `leave`／`ban`／`join`／`knock` |
| `0x05 Unsubscribe` | client → server | `{}` |

🚨 **server 定了每次連線走一輪完整快照、🚫 留跨連線的水位**（server §3.3）：被收回的邀請**只能靠「不在快照裡」發現**，帶舊水位回來會永遠留著已經被收回的邀請。`ci_seq` 只是同一輪之內的續傳游標。

#### 3.3.2 誰做什麼（跟訊息那條線同一套，維護者 2026-10-14）

> 維護者 2026-10-14：「開線時，只訂閱邀請就好了。補拿一輪應該做成 rpc 指令，讓 ui 去叫。」

| | daemon | UI |
|---|---|---|
| 開線（房間線）時 `Invite/Subscribe` | ✅ 只訂閱，收推播寫本地 | — |
| 補拿一輪 `Fetch` 到 `r == 0` | RPC 叫了才跑（§3.3.4 的 `room.list`） | 起來的當下、看到 `link.state` 線又通了、看到推播帶 `gap: true` 時叫 |
| 存水位 | 🚫（server §3.3） | 🚫（每輪都是完整快照，不需要） |
| 本地的邀請清單 | `room_list` 那一列：`membership = invite`、`invite_json`（§3.3.3） | 讀 `room.list { membership: ["invite"] }` |
| 「房名、頭像是誰說的」 | 算好 `is_from_this_server` 給出去（§3.3.4） | 決定怎麼顯示 |
| 接受／拒絕 | 照舊：`room.join`／`room.leave` | 叫哪一支 |

跟訊息那條線一樣（/docs/design/rooms/room-sync.md §0：daemon 只有訂閱新事件，🚫 主動叫 `Recent`，什麼時候補由 UI 觸發）。
不一樣的只有「補到哪裡停」：訊息補一段就停、往回多遠是 UI 的選擇；邀請一輪就是**到 `r == 0` 為止**，daemon 在一支 RPC 裡跑完整輪才回（server 說補到一半的不是快照，§3.3.3），UI 🚫 自己接續傳游標。

#### 3.3.3 寫本地的規則（server §8 的六條，逐條對應）

一輪 `Fetch` 是 UI 叫的那支 RPC 在跑（走 Misc 線，跟 `Recent` 一樣一問一答、回應是一串 `Batch`；`Push`／`Gone` 照舊從房間線來）。
這支 RPC 跑的期間，daemon 在記憶體裡記「這一輪開始之後被 `Push`／`Gone` 碰過的房 → 它的 `is`」，RPC 回了就丟（🚫 落地）。`is` 跟 `counts` 是同一個號碼空間（server §3.1），所以兩條線的先後不要緊：

| 收到 | `room_list` 那一列（這個帳號的） |
|---|---|
| `Batch` 的一間 | 那間這一輪被碰過、而且碰的 `is` ≥ 這筆的 count → 🚫 寫（推播比較新，server §8 ②'）；否則 `membership = invite`（列不在就加）、`invite_json` 換成這筆 |
| `r == 0`（補完） | 本地是 `invite`、這一輪沒出現、這一輪也沒被碰過的 → `membership = leave`、`invite_json = NULL`（離線期間被收回的；為什麼結束看不出來，標 `leave`，跟 `room.list` 處理退出的房同一套）。⚠️ **補到一半斷了就🚫 刪**：那不是快照（server §8 ②） |
| `Push` | 按 `room_id` 取代（server §8 ③）：`membership = invite`、`invite_json` 換成這筆。`sc == 0` 時那份 `state` 留著本地原本的（🚫 拿「太大沒送」蓋掉「之前拿到的」，跟 `room.get` 的 `state` 同一條） |
| `Gone` | `membership` 改成它說的（`knock` 照寫）、`invite_json = NULL` |
| `bc == 0` 的批次 | 🚫 推進續傳游標（沒有 `ns`） |

`invite_json` 是 `room_list` 的新欄（**schema v11**），內容是那筆原樣：`{ inviter?, is_direct, reason?, state: [...] }`。`conversation_json` 🚫 動（被邀請的房沒有「它看到的樣子」）。

#### 3.3.4 給 UI 的

- `room.list { membership: ["invite"] }`：每一列多帶 `invite`：
  ```jsonc
  { "id": "!r:localhost", "membership": "invite", /* 其他欄位照 RoomListEntry，不知道的是 null */
    "invite": { "inviter": "@alice:localhost", "is_direct": false, "reason": "來吃飯",
                "state": [ /* 縮減狀態原樣：m.room.create、m.room.name、…、被邀請者自己的 m.room.member */ ],
                "is_from_this_server": true } }
  ```
  - `sync=local`：讀本地。`sync=server`：跑一輪 `Fetch`、回那份快照、🚫 寫庫。`sync=both`：跑一輪、照 §3.3.3 寫、再從本地讀。
  - ⭐ **這就是「補拿一輪」的 RPC**：🚫 另開一支 method。`membership` 裡有 `invite`、`sync` 不是 `local` 才跑 `Fetch`；只要 `join` 的照舊只問 `JoinedRooms`，兩個都要就兩個都問。
  - 一輪在這支 RPC 裡跑到 `r == 0` 才回；中途斷了回錯、🚫 刪任何一列（已經寫進去的那幾間留著：那些是真的邀請），UI 再叫一次就是新的一輪。
- 推播 `room.membership`：server 的每一則 `Push`／`Gone` 推一則，`gap` 原樣帶著；一輪 `Fetch` 寫完有變的列也推（不帶 `gap`）：
  ```jsonc
  { "user": "@bob:localhost", "room": "!r:localhost", "membership": "invite", "invite": { /* 同上 */ }, "gap": false }
  { "user": "@bob:localhost", "room": "!r:localhost", "membership": "leave", "gap": false }
  ```
  - `gap: true` 是 server 說「這條流在這之前掉過東西」（server §8 ④）：daemon 🚫 自己重跑，原樣轉給 UI，要不要叫 `room.list` 補一輪是 UI 的事。
    📌 訊息那條線的 `gap` 是丟掉不轉的（UI 不需要知道洞，維護者 2026-09-22）；邀請這條要轉，因為它是「該補一輪了」唯一的訊號。
- 🚨 **`is_from_this_server`**（daemon 加的，維護者 2026-10-14）：`inviter` 的網域跟這個帳號的網域**逐字相同**才是 `true`；沒有 `inviter`、網域解不出來，一律 `false`（fail closed）。
  `false` 時 `state` 裡的房名、頭像、topic 都是**對方 server 說的**：縮減狀態沒有簽章，就算有也驗不動（要先加入才驗得了，而要不要加入正是這則邀請在問的，server §5.2.3）。UI 🚫 該把它當成已驗證的事實顯示。只有 `inviter` 本身有協定綁定。
- `room.get` 對被邀請的房照舊會被 server 拒（還沒加入）；UI 顯示「某某邀你加入某某房」靠的是 `invite`。

#### 3.3.5 一般 Matrix 帳號（甲，維護者 2026-10-14）

一般 Matrix 帳號**沒有常駐的同步迴圈**：daemon 只在要用時跑一次 `sync_once`（逾時 0，`crates/wbf-sdk/src/backend/matrix_sdk.rs`），所以它們連訊息都沒有即時推播。這支🚫 開迴圈（那是另一件事：會變成一般 Matrix 帳號訊息即時推播的地基）。

- matrix-sdk 的 `/sync` 本來就帶 `rooms.invite`（被邀請的房與縮減狀態），sync 完 store 裡就是**完整的**被邀請清單（跟 wbf 一輪 `Fetch` 補完同一個意思）。
- `room.list { membership: ["invite"], sync }` 走跟 wbf 一樣的語意：`server` 回 store 那份、🚫 寫庫；`both` 照 §3.3.3 寫——清單裡有的寫 `invite`，本地是 `invite`、清單裡沒有的照 store 說的改（store 知道它現在是 `join`／`leave`／`ban`），有變的推 `room.membership`（`gap` 一律 `false`）。
- 🚫 即時：UI 叫了才有（UI 起來的當下、定時、或使用者下拉時叫）。
- `is_from_this_server` 同一套算法。

## 4. 權限：`room.set_power_levels`

### 4.1 `m.room.power_levels` 能指定哪些

權限是房間的一項狀態事件，content 長這樣（欄位都可以省，省了用規格的預設）：

| 欄位 | 意思 | 欄位不在時（事件在） | 整個事件不在時 |
|---|---|---|---|
| `users` | 每個人的等級：`{ "@alice:x": 100, "@bob:x": 50 }` | 空 | 建房者 100、其他人 0 |
| `users_default` | `users` 裡沒列到的人的等級 | 0 | 0 |
| `events_default` | **發一般訊息（非狀態事件）**要多少，`events` 沒列到的訊息型別用它 | 0 | 0 |
| `state_default` | **改房間狀態**要多少，`events` 沒列到的狀態型別用它 | 50 | 0 |
| `events` | 個別事件型別的門檻：`{ "m.room.name": 50, "m.room.power_levels": 100 }` | 空 | 空 |
| `invite` | 邀請別人要多少 | 0 | 0 |
| `kick` | 踢人要多少 | 50 | 50 |
| `ban` | 封鎖（與解除封鎖）要多少 | 50 | 50 |
| `redact` | 收回**別人的**訊息要多少（收回自己的不用） | 50 | 50 |
| `notifications` | `{ "room": 50 }`：發 `@room`（叫所有人）要多少 | `room` 50 | `room` 50 |

本機 wbfuwunel 建房時預設寫進去的（wbfuwunel 的 `src/api/client/room/create.rs` 的 `default_power_levels_content`）：`users` 只有建房者 100；
`events` 裡 `m.room.power_levels`、`m.room.server_acl`、`m.room.encryption`、`m.room.history_visibility` 是 100，`m.room.tombstone` 是 100（v12 起 150），投票回覆 0；
`public_chat` 另外把 `invite` 拉到 50、通話事件 50。其他欄位用上面的預設（`ban`、`kick`、`redact`、`state_default` 50，`invite`、`events_default`、`users_default` 0）。

**改的規則**（Matrix 的 auth rules，**server 擋**，daemon 🚫 自己判斷、🚫 限制值，§1 第 4 條）：
- 要改 `m.room.power_levels` 本身，自己的等級要 ≥ `events["m.room.power_levels"]`（上面是 100）。
- 改任何一欄的門檻：舊值與新值都不能高過自己的等級。
- 改某個人的等級：那個人原本的等級要**低於**自己（同級的不能動），新值也不能高過自己；降自己可以。
- room v12 起，建房者（與 `additional_creators`）是「無限」、🚫 寫在 `users` 裡（`crates/wbf-sdk/src/room_state.rs` 的 `my_power_level` 已經照這個算）。

**我們的模型怎麼讀它**（/docs/design/rooms/chat-model.md §2.4、§3.2、§3.3）：`my_power_level` 照給；`can_send_message` ＝ 我的等級 ≥ `events["m.room.message"]`（沒列就 `events_default`）；
發言門檻高到只有擁有者達得到（100）→ `kind` 是 `Channel`。所以 UI 建 channel ＝ `room.create` 之後 `room.set_power_levels { events_default: 100 }`。

### 4.2 `room.set_power_levels` 的合併規則

參數的欄位跟 `m.room.power_levels` 的 content **同名、同格式**（Matrix 相容），daemon 讀出目前那一份，照下面合併，整份寫回：

| 給了什麼 | 怎麼合 |
|---|---|
| 數字欄（`users_default`、`events_default`、`state_default`、`invite`、`kick`、`ban`、`redact`） | 給了就換成這個值；沒給就照舊 |
| `users`、`events`、`notifications`（對照表） | **逐個 key 合併**：給的 key 覆蓋、沒給的 key 照舊；某個 key 給 `null` ＝ 把它從表裡拿掉（回到預設） |
| 都沒給 | 用法錯（`1100`），🚫 寫一份一樣的回去 |
| 房間沒有 `m.room.power_levels` | 用法錯（`1100`）：🚫 從空的開始合——省掉的欄位會落到「事件在」的預設，建房者會丟掉 100；要寫就用 `room.set_state` 寫整份 |

例（本機 wbfuwunel 實跑，`crates/wbf-daemon/tests/real_server.rs` 的 `room_actions_walk_the_server_and_the_local_rows_of_two_accounts`：alice 把發言門檻拉到 100，這間房在 `room.get` 就變成 `channel`）：

```jsonc
→ { "method": "room.set_power_levels", "params": { "user": "@alice:localhost", "room": "!jUZRmPusRwM8e4b0QV:localhost", "events_default": 100 }, "id": 80 }
← { "code": 0, "id": 80, "msg": "ok", "result": { "event_id": "$4krhd_Y2vOHL6-96Xg7nkSboemHFwwmapLtmCGuiswo" } }
```

值是什麼、合不合理 daemon 🚫 看（例：負數、比自己高）——server 會照 auth rules 擋，擋了回 `1400`、`data` 是 403 `M_FORBIDDEN`。daemon 只擋「格式不是 Matrix 能接受的」：數字欄不是整數、對照表不是物件或值不是整數／`null`、`users` 的 key 不是 mxid、表上沒有的欄位（用法錯 `1100`，`crates/wbf-sdk/src/room_state_edit.rs` 的 `merge_power_levels`）。
兩邊同時改，後寫的會蓋掉先寫的那幾項（Matrix 沒有 compare-and-set；matrix-sdk 的 `update_power_levels` 也是這樣）。

## 5. 置頂：`m.room.pinned_events`

content 是 `{ "pinned": ["$event_id", …] }`（照順序）。`room.pin { pinned: true }`：不在清單就加到最後；`false`：在就拿掉。已經是那個狀態就🚫 寫（回沒有 `event_id`）。
改它要的等級是 `events["m.room.pinned_events"]`，沒列就 `state_default`（50）。置頂清單本身由 `room.get` 給（§5.1），🚫 另開一支讀的。

### 5.1 `room.get` 只多不少（維護者 2026-10-07）

`room.get` 本來就拿整份房間狀態（wbf 帳號是 `GetState`），然後只算出幾個欄位（`name`、`topic`、`encrypted`、`my_power_level`…）給出去，置頂清單、權限的整份內容都丟掉了。
這支改成「拿到什麼就給什麼」：`Conversation` 的型別欄位照舊，另外多帶房間狀態本身：`state`，成員事件（`m.room.member`）以外的每一項狀態事件原樣（server 給什麼帶什麼）。這樣 UI 要改權限時手上就有目前的整份內容、要顯示置頂也不必再問。
`sync=both` 時整份存進自己那一列的 `conversation_json`，`room.list` 讀本地時一樣帶著（`RoomListEntry` 跟著加）。

## 6. 還要維護者定的

目前沒有（§3.3 收邀請的形狀等維護者看過、說開工）。

### 6.1 已經定的（原本在這裡的問題）

- **分房間金鑰算不算被邀請的人**（維護者 2026-10-07 在 server 那邊定，權威是 wbfuwunel 的 /docs/design/keys/invite-key-scope.md）：
  - 房間的歷史可見性是 `joined` → 只算已加入的（現狀）；`shared`／`invited`／`world_readable` → 已加入的＋被邀請的（跟 matrix-sdk 的 `share_room_key` 一樣）。可見性改了不特別處理：拿舊版本號送出會吃 `1506`。
  - server 還沒實作（2026-10-14 看 wbfuwunel main：`is_membership_counted` 仍只算 `Join`／`Leave`／`Ban`）。
  - client：金鑰是 client 照 server 的 `Members` 發的，server 改完收件人自然多了被邀請者。client 要跟著改的是 `crates/wbf-sdk/src/crypto_engine.rs` 的 `room_key_share_settings`（可見性寫死成 `shared`，改成照房間的）與 `crates/wbf-core/src/key_share.rs` 文件裡的「join 成員」；server 合併後另開一支。
- `room.get` 帶整份狀態（A，全拿，成員事件以外）。
- `joined` 換成 `membership`，🚫 並存（量過：只在 `crates/wbf-sdk/src/cache.rs` 的 5 句 SQL 與 1 條測試）。
- `room.enable_encryption` 照樣做（§3.2）。
- `room.create` 之後 daemon 🚫 補拿房間資料；`conversation_json` 空著，UI 叫 `room.get` 時才拿、存回。
- `room.forget` 清紀錄時池檔🚫 當場刪，留給掃描（§3.1）。
- 本地寫不進去：回成功（以 server 的 ACK 為準）＋推 `note`（§3）。
- 置頂清單由 `room.get` 給（§5.1）。
- 參數、結果全部跟上游一樣，daemon 只是中轉站（§1 第 14 條）；通用的 `get_state`／`set_state`＋窄的方便版都給；房間相關的橋全接（§2）。
- 收邀請：開 server issue 優先處理（wbfuwunel #111）；推播語意等 server 做完再補（§3.3）。

## 7. 程式碼在哪

| 層 | 檔案 | 做什麼 |
|---|---|---|
| sdk | `crates/wbf-sdk/src/matrix_endpoint.rs` | 一張端點表（橋的號碼＋HTTP 的 method、路徑模板、query 變數）；`check_variables`（橋的規則，兩條路都先過：wbfuwunel 的 /docs/bridge-specs/index.md §1.3——缺、型別、`.`／`..`、值裡有 `\t`／`\n`／`\r`、表上沒有的）、`encode_body`、`call_over_http`（填完路徑再比對段數，跟橋的 `fill_path` 同一個不變式） |
| sdk | `crates/wbf-sdk/src/client.rs` | `WbfClient::call_matrix_endpoint`：同一張表的走橋那條 |
| sdk | `crates/wbf-sdk/src/room_state_edit.rs` | §2.1 的 createRoom body、§4.2 的權限合併、§5 的置頂、§2.5 的 `m.direct`（純函數） |
| sdk | `crates/wbf-sdk/src/room_state.rs`、`chat.rs` | `Conversation`／`RoomListEntry` 帶 `state` 與 `membership`；`conversation_with_state_event`（§3 的 `set_state` 那一列） |
| sdk | `crates/wbf-sdk/src/cache.rs` | schema v10 的 `membership`；`record_membership`、`record_written_state`、`record_room_encryption`、`forget_room`（§3.1）；`list_room_entries` 的身分篩選 |
| core | `crates/wbf-core/src/room_actions.rs` | 每個動作一個入口；`call_endpoint_raw` 是兩條路唯一的分岔點；`WriteSync`；寫不進去推 `note`；server 拒絕的 Matrix 錯誤放進 `data` |
| core | `crates/wbf-core/src/rooms_ops.rs` | `room.list` 的身分篩選；一般 Matrix 帳號的 `room.get` 多問一次 `GetState` |
| daemon | `crates/wbf-daemon/src/handle/room_actions.rs` | 參數解析（型別不對 `102`、`sync` 只收 `both`／`server`） |
| 測試 | `crates/wbf-sdk/src/{matrix_endpoint,room_state,room_state_edit,cache}.rs` 的單元測試；`crates/wbf-core/src/room_actions.rs`（假 server：`test_support.rs` 的 `room_action_reply`）；`crates/wbf-daemon/src/handle/mod.rs` 的兩張參數表；真 server：`crates/wbf-sdk/tests/e2e_local_server.rs` 的 `the_matrix_endpoint_table_answers_the_same_over_http_and_over_the_bridge`、`crates/wbf-daemon/tests/real_server.rs` 的 `room_actions_walk_the_server_and_the_local_rows_of_two_accounts` | |
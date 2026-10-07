# 房間動作：建房、成員、權限、置頂（草案，維護者 2026-10-07 起逐條定）

> 狀態：**草案，還沒動程式碼**。§1 是已經定的，§6 是還要維護者定的，§7 是這支 PR 要改的檔案清單。
> 範圍：/docs/handover.md §7 第 2 項的「房間動作」那一半。訊息動作（回覆、編輯、收回、表情）與已讀排在後面幾支（維護者 2026-10-07）。

## 0. 一句話

UI 叫一個 RPC，daemon 做一個房間動作，回 server 給的結果。會改到「誰在不在這間房」的動作帶 `sync`：`both`（預設）收到 ACK 之後把本地的房間列一起改掉，`server` 只打遠端。

## 1. 已定的（維護者 2026-10-07）

1. **這支只做房間動作**：建房、加入、退出、忘記、邀請、踢人、封鎖、解除封鎖、改名、改主題、改權限、置頂。
2. **建 Direct 一律建新房**：🚫 先找既有的 Direct（daemon 🚫 替 UI 串動作）。要不要沿用舊房由 UI 從房間列表判斷。
   /docs/design/rooms/chat-model.md §3.1「`create(Direct)` 先找既有的」那一句照這條改。
3. **`direct: true` 時 `room.create` 順便寫 `m.direct`**：建完房，daemon 讀帳號資料的 `m.direct`、把「對方 → 這間房」加進去、寫回。Matrix 規格要 client 做這件事；不寫的話，別的 client 不會把它當成一對一。
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

每支都帶 `user?`、`server?`（/docs/design/rpc-specs/rpc-spec.md §2）。**參數照 Matrix 的名字與格式，結果是 server 回的 body 原樣**（§1 第 14 條）；server 回的錯照原樣往上帶（例：權限不夠是 403 `M_FORBIDDEN`，core 的 `Server` 錯）。
wbf 帳號走 wbfuwunel 的橋（/docs/bridge-specs/0x13-room.md、0x14-event.md、0x11-account.md，下表只寫 kind 與編號）；一般 Matrix 帳號走 matrix-sdk（有現成方法用現成的，沒有就照同一個 HTTP 端點送原始請求）。
「批 3／批 4」是 server 後來才開的端點，接之前先讀本 repo issue #55 列的十個坑（/docs/handover.md §6）。

### 2.1 成員與房間本身（Room kind）

| method | params（除了 `sync?`、`user?`、`server?`） | result | wbf 帳號 |
|---|---|---|---|
| `room.create` | `{ encrypted: bool（必填）, name?, topic?, invite?, invite_3pid?, room_version?, room_alias_name?, visibility?, preset?, is_direct?, initial_state?, creation_content?, power_level_content_override? }` | `{ room_id }` | Room `0x20`；`is_direct` 時再走 Account `0x25`／`0x26`（讀、寫 `m.direct`） |
| `room.join` | `{ room: "!id" \| "#別名", via?, reason? }` | `{ room_id }` | Room `0x21` |
| `room.knock` | `{ room: "!id" \| "#別名", via?, reason? }` | `{ room_id }` | Room `0x2E`（批 3） |
| `room.leave` | `{ room, reason? }` | `{}` | Room `0x22` |
| `room.forget` | `{ room }` | `{ history_cleared: bool }`（§3.1；daemon 加的欄位） | Room `0x23`（server 要先 leave，不然 400） |
| `room.invite` | `{ room, user, reason? }` | `{}` | Room `0x24` |
| `room.kick`／`room.ban`／`room.unban` | `{ room, user, reason? }` | `{}` | Room `0x25`／`0x26`／`0x27` |
| `room.upgrade` | `{ room, new_version }` | `{ replacement_room }` | Room `0x2D`（批 3） |
| `room.members` | `{ room, membership?, not_membership? }` | server 的 body（`chunk` 加 wbfuwunel 的 `org.wbftw.room_version`） | Room `0x29` |
| `room.joined_members` | `{ room }` | `{ joined: { mxid: { display_name, avatar_url } } }` | Room `0x2F`（批 3） |
| `room.summary` | `{ room: "!id" \| "#別名", via? }` | server 的 body | Room `0x32`（批 3）。**還沒加入也能問**：被邀請時拿房名的另一條路 |
| `room.hierarchy` | `{ room, from?, limit?, max_depth?, suggested_only? }` | server 的 body | Room `0x33`（批 3，space） |
| `room.mutual_rooms` | `{ user, from? }` | server 的 body | Room `0x34`（批 3） |
| `room.public_rooms` | `{ server?, limit?, since?, filter?, include_all_networks?, third_party_instance_id? }`（給了 `filter` 就走 Filtered） | server 的 body | Room `0x35`／`0x36`（批 4） |
| `room.get_visibility`／`room.set_visibility` | `{ room }`／`{ room, visibility }` | server 的 body | Room `0x30`／`0x31`（批 3，公開目錄） |
| `room.resolve_alias` | `{ alias }` | `{ room_id, servers }` | Room `0x2A` |
| `room.set_alias`／`room.delete_alias` | `{ alias, room }`／`{ alias }` | `{}` | Room `0x2B`／`0x2C` |
| `room.aliases` | `{ room }` | `{ aliases }` | Room `0x37`（批 4） |

`room.create`：除了 `encrypted`，每個參數都**原樣**進 CreateRoom 的 body（名字、格式、預設值都是 Matrix 的，daemon 🚫 補預設）。`encrypted: true` 時 daemon 在 `initial_state` 加
`{ "type": "m.room.encryption", "state_key": "", "content": { "algorithm": "m.megolm.v1.aes-sha2" } }`；UI 自己在 `initial_state` 放了 `m.room.encryption` 而 `encrypted` 給 `false`（兩邊說的相反）→ 參數錯（`102`）。
📎 想建「公開可搜」的房（/docs/design/rooms/chat-model.md §3.6）：`preset: "public_chat"`＋`initial_state` 帶 `m.room.history_visibility: world_readable`，要列進目錄再加 `visibility: "public"`——全是 UI 組。

### 2.2 房間狀態（Event kind 的 `0x21`–`0x23`）

| method | params | result | 底下 |
|---|---|---|---|
| `room.get_state` | `{ room, event_type, state_key?: "" }` | 那一項的 content；沒有這一項 → `null` | Event `0x22` |
| `room.set_state` | `{ room, event_type, state_key?: "", content }` | `{ event_id }` | Event `0x23` |
| `room.set_name` | `{ room, name }` | `{ event_id }` | `set_state`（`m.room.name`） |
| `room.set_topic` | `{ room, topic }` | `{ event_id }` | `set_state`（`m.room.topic`） |
| `room.set_avatar` | `{ room, url }`（mxc） | `{ event_id }` | `set_state`（`m.room.avatar`） |
| `room.set_canonical_alias` | `{ room, alias?, alt_aliases? }` | `{ event_id }` | `set_state`（`m.room.canonical_alias`） |
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
| `room.set_direct` | `{ room, user, direct: bool }` | `{}` | 讀 `m.direct` → 加或拿掉「`user` → `room`」→ 寫回（§2.5） |

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

**現況**：`cache.db` 的 `room_list` 已經是「一個帳號一列」——主鍵是（帳號, 房間），`joined` 0／1 記這個帳號在不在裡面，退出的🚫 刪列、標 0（v8，維護者 2026-10-05，/docs/design/storage/local-cache-db.md §5）。
例：A 建房、B 在裡面、C 不在 → A、B 各一列（`conversation_json` 是各自上次 `room.get` 看到的樣子），C 沒有列。

**這支改成**：`joined` 換成 `membership`（`join`／`invite`／`knock`／`leave`／`ban`，§1 第 8、10 條；`knock` 是敲門等回應，`room.knock` 才有），schema v10（升級表＝重建，跟 v9 一樣🚫 寫遷移）。
「在房裡」＝ `membership = join`；`room.list` 預設只列 `join` 的（跟現在只列 `joined = 1` 一樣），另收 `membership?: ["join", "invite", …]` 要哪幾種自己挑。

**帶 `sync` 的是會改本地的那幾支**：成員動作（§2.1 的 create、join、knock、leave、forget、invite、kick、ban、unban、upgrade）與狀態的寫入（§2.2 的 `set_state` 與它所有的窄版）。
純讀的（members、summary、hierarchy、public_rooms、get_state、tags、account data…）本地沒存、🚫 帶 `sync`，一律問 server。標籤與帳號資料本地也沒存，寫入🚫 帶 `sync`。

| method | `both`：ACK 之後寫 |
|---|---|
| `room.create` | 建房者那一列 `membership = join`、`conversation_json` 空著（`refreshed_at` 空 ＝ 還沒 `room.get` 過）；`rooms.encrypted` 寫建房時給的值（`room.send_text` 只信本地的 `rooms.encrypted`，建完馬上能送靠的是這一筆）。`conversation_json` 留空：它是一整份 `Conversation`，只填幾欄等於說謊（`member_count`、`can_send_message`…）；UI 要就叫 `room.get`（§1 第 11 條） |
| `room.join` | 自己那一列 `membership = join`（列不在就加；`conversation_json` 不動） |
| `room.knock` | 自己那一列 `membership = knock`（列不在就加） |
| `room.leave` | 自己那一列 `membership = leave`，🚫 刪、`conversation_json` 留著 |
| `room.forget` | §3.1 |
| `room.invite` | 被邀請的人**是這台機器登入的帳號**（同 server 的 `cache.db` 有它這個 user）→ 它那一列 `membership = invite`（列不在就加）。不是本機帳號就🚫 寫 |
| `room.kick` | 被踢的人是本機帳號 → 它那一列 `membership = leave`（Matrix 裡被踢之後就是 `leave`） |
| `room.ban` | 被封鎖的人是本機帳號 → 它那一列 `membership = ban`（列不在就加） |
| `room.unban` | 被解除的人是本機帳號、它那一列是 `ban` → 改成 `leave`（Matrix 裡解除封鎖之後是 `leave`，要再邀請或自己加入才回來） |
| `room.upgrade` | 新房（`replacement_room`）加一列 `membership = join`、`conversation_json` 空著；舊房那一列🚫 動（還在舊房裡，舊房多了墓碑，下次 `room.get` 看得到） |
| `room.set_state` 與它所有的窄版 | 自己那一列有 `conversation_json` 就把它的 `state`（§5.1）裡那一項換成這次寫的，再用整份 `state` 重算型別欄位（`name`、`topic`、`encrypted`、`my_power_level`、`can_send_message`、`kind`；成員那幾欄 `member_count`、`direct_peer` 照舊）。沒有 `conversation_json`（還沒 `room.get` 過）就🚫 寫。其他帳號那一列是它們自己看到的樣子，下次它們 `room.get` 才更新 |
| 寫的是 `m.room.encryption`（`room.enable_encryption`，或 `room.set_state` 直接寫它） | 上一列之外，`rooms.encrypted = 1`，不管有沒有 `conversation_json`（§3.2；這一欄是房間的事實，所有帳號共用、只升不降） |

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
   之後再叫一次 `sync=both` 會重打一次 Forget——server 對已經忘記的房回什麼，實作時確認。

### 3.2 `room.enable_encryption`：把明文房改成加密

- 寫 `m.room.encryption`（`{ "algorithm": "m.megolm.v1.aes-sha2" }`）。Matrix 的加密是**單向**的：開了就關不掉，🚫 有「關加密」的 RPC。
- 改它要的等級是 `events["m.room.encryption"]`（wbfuwunel 建房預設 100）。不夠 → server 回 403。
- `both`（預設）：ACK 之後本地 `rooms.encrypted = 1`（`rooms.encrypted` 本來就只升不降，/docs/design/storage/local-cache-db.md §5）。之後 `room.send_text` 就走加密那條。
- ⚠️ **`server` 很危險，只給除錯用**（維護者 2026-10-07）：server 上已經加密、本地還記著明文，`room.send_text` 信本地的標記，會把訊息**明文**送進加密房，直到下一次 `room.get sync=both` 把標記升上去。
  所以 rpc-spec 要把這個值標成「除錯用」。
- 已經加密的房再叫一次：daemon 🚫 先問，照送；server 收了就是多一則一樣的狀態事件，本地本來就是 1。

### 3.3 收邀請（等 wbfuwunel #111，這支🚫 做）

> 維護者 2026-10-07 開工時定：「invite 的推播語意先不做，等 server 完成後補」。下面是 #111 做完之後那一支的形狀，這支只做到 `membership` 收得下 `invite`。

**現況**：wbf 帳號收不到邀請。訂閱線只推已加入的房（wbfuwunel 的 /docs/design/events/event-push.md §1）、`JoinedRooms` 只列已加入的、還沒加入時讀房間狀態是 403。
標準 Matrix 是 `/sync` 的 `rooms.invite`，每間附一份縮減過的狀態（`invite_state`：建房事件、房名、頭像、加入規則、別名、加不加密、邀請的那則成員事件）。
server 其實存著這份（wbfuwunel 的 `src/service/rooms/state_cache/update.rs` 的 `mark_as_invited`），只是沒路送給 wbf client——#111 請 server 優先補。

**#111 做完之後 client 這樣接**（server 定了不一樣，照 server 的對齊）：

| 收到 | 本地（這個帳號那一列） | 推給 UI |
|---|---|---|
| 推送 `invite { room_id, inviter, is_direct, reason?, state }` | `membership = invite`（列不在就加）；那份縮減狀態存進新的一欄 `invite_json`（`{ inviter, is_direct, reason?, state }` 原樣），`conversation_json` 🚫 動 | 新推播 `room.membership { user, room, membership: "invite", invite: {…} }` |
| 推送 `invite_gone { room_id, membership }` | `membership` 改成它說的（`leave`／`ban`／`join`），`invite_json` 清掉 | `room.membership { user, room, membership }` |
| 補拿（重連、`room.list sync=both` 時一起問）`{ invites: […] }` | 跟推送一樣寫；本地是 `invite`、補拿的清單裡卻沒有的 → 改成 `leave`（跟 `room.list` 處理退出的房同一套） | 有變的才推 |

- `room.list { membership: ["invite"] }` 列被邀請的房時，每一列帶 `invite`（`invite_json` 的內容）；`room.get` 對被邀請的房照舊會被 server 拒（還沒加入），所以 UI 顯示「某某邀你加入某某房」靠的是 `invite`。
- 還沒等到 #111 的替代：`room.summary`（§2.1，批 3，還沒加入也能問房名）。
- 一般 Matrix 帳號：matrix-sdk 的 sync 本來就收邀請與縮減狀態（`Client::invited_rooms`），同一套寫法，不用等 server。
- `invite_json` 是 `room_list` 的新欄，跟著那一支加（這支的 v10 🚫 加：沒有人寫它）。

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
| 都沒給 | 參數錯（`102`），🚫 寫一份一樣的回去 |

例：UI 把 bob 升成管理員、同時把發言門檻拉到 100（channel）：

```jsonc
// 示意（實作後換成實跑的輸出）
→ { "method": "room.set_power_levels", "params": { "room": "!r:localhost", "users": { "@bob:localhost": 50 }, "events_default": 100 }, "id": 3 }
← { "code": 0, "msg": "ok", "id": 3, "result": { "event_id": "$pl…" } }
```

值是什麼、合不合理 daemon 🚫 看（例：負數、比自己高）——server 會照 auth rules 擋，擋了回 403 `M_FORBIDDEN`。daemon 只擋「格式不是 Matrix 能接受的」：數字欄不是整數、`users` 的 key 不是 mxid（參數錯 `102`）。
兩邊同時改，後寫的會蓋掉先寫的那幾項（Matrix 沒有 compare-and-set；matrix-sdk 的 `update_power_levels` 也是這樣）。

## 5. 置頂：`m.room.pinned_events`

content 是 `{ "pinned": ["$event_id", …] }`（照順序）。`room.pin { pinned: true }`：不在清單就加到最後；`false`：在就拿掉。已經是那個狀態就🚫 寫（回沒有 `event_id`）。
改它要的等級是 `events["m.room.pinned_events"]`，沒列就 `state_default`（50）。置頂清單本身由 `room.get` 給（§5.1），🚫 另開一支讀的。

### 5.1 `room.get` 只多不少（維護者 2026-10-07）

`room.get` 本來就拿整份房間狀態（wbf 帳號是 `GetState`），然後只算出幾個欄位（`name`、`topic`、`encrypted`、`my_power_level`…）給出去，置頂清單、權限的整份內容都丟掉了。
這支改成「拿到什麼就給什麼」：`Conversation` 的型別欄位照舊，另外多帶房間狀態本身：`state`，成員事件（`m.room.member`）以外的每一項狀態事件原樣（server 給什麼帶什麼）。這樣 UI 要改權限時手上就有目前的整份內容、要顯示置頂也不必再問。
`sync=both` 時整份存進自己那一列的 `conversation_json`，`room.list` 讀本地時一樣帶著（`RoomListEntry` 跟著加）。

## 6. 還要維護者定的

1. **分房間金鑰要不要算進被邀請的人**（維護者 2026-10-07 點出的設計缺口，**不在這支的範圍**，要另開一支 E2EE 的 PR＋server issue）：
   - matrix-sdk：房間的歷史可見性是 `joined` 才只分給已加入的，其他三種連被邀請的人也分（vendor 的 `matrix-sdk-base/src/client.rs` `share_room_key`）。
   - 我們：不管可見性一律只分給已加入的；告訴加密元件的設定也寫死成預設的 `shared`（`crates/wbf-sdk/src/crypto_engine.rs` 的 `room_key_share_settings`）。
     結果：可見性是 `shared`／`invited` 的房，被邀請期間別人送的密文，那個人加入後解不開——跟房間的設定說的不一樣。
   - 要改得 server 一起改：wbfuwunel 的 /docs/design/keys/room-device-version.md 第 110、114、360 行定了「邀請不算進房間版本號（無權看的人不必管）」，那是在「被邀請者看不到」的前提下定的；
     可見性不是 `joined` 時這前提不成立，房間版本號與 `Members` 的裝置版本號都要算進被邀請者，不然被邀請者換了裝置，「送出前版本號要對」那道檢查擋不到。已在 #111 的末尾先提一句。
   - 要定的：**照可見性分給被邀請者**（client＋server 都改）還是維持現狀。

## 6.1 已經定的（原本在這裡的問題，維護者 2026-10-07）

- `room.get` 帶整份狀態（A，全拿，成員事件以外）。
- `joined` 換成 `membership`，🚫 並存（量過：只在 `crates/wbf-sdk/src/cache.rs` 的 5 句 SQL 與 1 條測試）。
- `room.enable_encryption` 照樣做（§3.2）。
- `room.create` 之後 daemon 🚫 補拿房間資料；`conversation_json` 空著，UI 叫 `room.get` 時才拿、存回。
- `room.forget` 清紀錄時池檔🚫 當場刪，留給掃描（§3.1）。
- 本地寫不進去：回成功（以 server 的 ACK 為準）＋推 `note`（§3）。
- 置頂清單由 `room.get` 給（§5.1）。
- 參數、結果全部跟上游一樣，daemon 只是中轉站（§1 第 14 條）；通用的 `get_state`／`set_state`＋窄的方便版都給；房間相關的橋全接（§2）。
- 收邀請：開 server issue 優先處理（wbfuwunel #111），client 先照「server 會推」接（§3.3）。

## 7. 這支 PR 要改的檔案

| 層 | 檔案 | 改什麼 |
|---|---|---|
| sdk | `crates/wbf-sdk/src/protocol.rs` | 新的 `BridgedEndpoint`：Room `0x20`–`0x27`、`0x2A`–`0x37`（`0x28` JoinedRooms、`0x29` Members 已有）、Event `0x23` SetStateEvent、Account `0x26`–`0x2B`；與它們的 meta 變數型別 |
| sdk | `crates/wbf-sdk/src/client.rs` | `WbfClient`：每個橋一支薄的（照 Matrix 的 body 原樣送、原樣收），不在 sdk 加規則 |
| sdk | `crates/wbf-sdk/src/backend/matrix_sdk.rs` | 一般 Matrix 帳號的同一組（有現成方法用現成的，沒有就送原始請求） |
| sdk | `crates/wbf-sdk/src/cache.rs` | schema v10：`room_list.joined` 換成 `membership`；本地寫法（§3）；`forget_room_of`（§3.1，一個 transaction）；`room.list` 的 `membership` 篩選 |
| sdk | `crates/wbf-sdk/src/chat.rs`、`room_state.rs` | `Conversation`／`RoomListEntry` 多帶 `state`（§5.1）；從 `state` 重算型別欄位（§3 的 `set_state` 那一列） |
| sdk | `crates/wbf-sdk/src/power_levels.rs`（新，或放進 `room_state.rs`） | §4.2 的合併規則（純函數，單元測試） |
| core | `crates/wbf-core/src/room_actions.rs`（新） | 每個動作一個入口：分帳號種類、組 body（`encrypted` → `initial_state`）、讀改寫（權限、置頂、`m.direct`）、`sync=both` 寫本地、寫不進去推 `note` |
| core | `crates/wbf-core/src/room_sync.rs`、`wbf_rooms.rs` | `room.list` 讀寫 `membership`；收邀請的推送與補拿等 #111（§3.3）🚫 這支 |
| core | `crates/wbf-core/src/lib.rs`、`event.rs` | 掛模組、匯出型別 |
| daemon | `crates/wbf-daemon/src/handle/rooms.rs`、`handle/mod.rs`、`push.rs` | 參數解析（`sync` 只收 `both`／`server`）、dispatch、參數錯的測試 |
| 測試 | core 的假 wbf server（`crates/wbf-core/src/test_support.rs` 的 `bridged_reply`）答新的橋；core 單元測試；daemon 真 server 測試加「建房 → 邀請 → 對方加入 → 踢 → 本地列」、「forget 清紀錄」、「明文房升級加密之後送的字是密文」、「改權限 → `room.get` 的 `state` 與 `can_send_message`」 | |
| 文件 | /docs/design/rpc-specs/rpc-spec.md §3.3、§4、§10；/docs/design/rooms/chat-model.md §2.1、§2.6、§3.1、§3.2、§6；/docs/design/storage/local-cache-db.md §5（v10）；/docs/handover.md §1、§6、§7；這一份從草案改成定案 | |

**順手一起修**（#75 審查的小項，維護者 2026-10-07：併進下一支）：
- `crates/wbf-core/src/media_ops.rs` 的 `del_local_media`：「正在寫的檔」快照與刪除之間的空檔，快照挪進刪除那一步（cirno 🟢1）。
- `crates/wbf-core/src/download_queue.rs` 的 `close_everything`：收攤時「準備中」的 job 也推 `failed`（cirno 🟢2）。
- `crates/wbf-core/src/matrix_download.rs` 的 `ensure_matrix_download`：`open_complete` 叫兩次合成一次（rumia）。
- `crates/wbf-sdk/src/event_json.rs`：fallback 字串 `"file"` 收成常數（rumia）。

# E2EE 的 RPC 面：狀態放 UI、金鑰由 daemon 自動、1506 之後 daemon 補完再一起回

> 維護者 2026-09-29 定的形狀（原話在 §0）。實作：
> - sdk：`crypto_engine.rs` 的 `encrypt_and_send`（只在本機備好房間金鑰、加密、送；🚫 上網分金鑰）、`distribute_room_key`（後台把金鑰送出去）、`to_incoming`（收到時有金鑰就解），
>   `cache.rs` 的 `list_undecrypted_ciphertexts`（找還沒解的密文）。
> - core：`room_crypto.rs`（refresh、加密送出、1506 之後自動重拿、補解）、`key_share.rs`（後台送房間金鑰，§3.1）、`key_sync.rs`（上傳金鑰、補一次性金鑰、金鑰到了補解）、
>   `room_sync.rs`（推來的有金鑰就解、`DeviceChanged` 轉給 UI）、`sync_ops.rs`（`Recent` 拉完補解）、`link_pool.rs`（宣告 feature）。
> - daemon：`room.refresh_devices`、`room.send_text` 多兩個參數、錯誤回應的 `data`、`devices.changed` 推播。
>
> server 的語意在 wbfuwunel 的 `/docs/design/wbf-room-device-version.md`（房間版本號、1506、`DeviceChanged`）與 `wbf-e2ee.md`（`CryptoState`）。
> UI 與 daemon 誰呼叫什麼，以這份為準（/docs/design/keys/e2ee-walkthrough.md §16.6 只留原則）。

## 0. 規矩（維護者原話，2026-09-29）

> UI 寫 token 叫起 daemon，UI 透過加密管道告知 daemon 解 vault，解開後，得到帳號等一切聊天紀錄、金鑰、session，開始訂閱，當下的 server，
> 讓 user "online" 但不完全 sync。UI 啟動，UI 可以主動叫 Recent 要全局事件更新最近的狀態，點進房間後再次要求刷新該房間聊天紀錄、包括房間成員版本號。
> 所以 UI 理論上應該要知道版本號，UI 送出新訊息自帶房間版本號，房間版本號異動時，會被 server 阻擋。
> 此時 daemon 發現之前送舊的版本號被擋時應該自動要求同步裝置，重新拿最新房間狀態，狀態回來後，同時回傳 UI。
> 而不是等 UI 主動……能少一步，daemon 可以自動一點。
> ……我明確說房間版本號存在 UI，UI 能主動發現版本號變異後，UI 再發送 RPC 要求更新裝置或是房間版本號。終端客戶是 UI，而不是 daemon。

> 金鑰的分發可以高度自動化，由 daemon 控制。

> 收到的訊息什麼時候解密？如果已有金鑰，解密的同時也存密文、明文，如果沒金鑰，不解密直接存。
> 在金鑰訂閱側，收到金鑰，嘗試找是那些訊息？那些訊息解密了嗎？已經解了，不管，沒解，立刻解密並保存。
> 舊訊息補解開之後，要不要通知 UI？那當然要。走一樣的流程，訊息推送 RPC 鉤子。

> 「上傳自己的一次性金鑰、不夠就補」這件事給 daemon 沒問題……而且盡量補多一點。

> （2026-10-05）為什麼發訊息一定要綁定發金鑰？生成 session 時就該處理金鑰，後台需要生成金鑰時就散播，而不是累積到 send message。完全沒必要耦合。
> 後台怎樣不管，後台有沒有送達金鑰也不管，在途也不管，已經生成了，那就用生成的 session 去加密，然後去送，散播金鑰的事，後台自己處理。

> 1506 補 data 同意。

📎 對話裡釐清過、寫下來免得下一個人再問一次：
- **沒有「房主分發金鑰」**：每個人在每個房各有自己的一把房間金鑰，只用來加密自己送出的訊息，也只由自己的客戶端分出去。
  房主（有權限的人）只能在 `m.room.encryption` 設換金鑰的期限（`rotation_period_ms`／`rotation_period_msgs`），設完不必在線。
- **對方不在線也分得出去**：分金鑰只用 server 上已經有的東西（對方上傳過的公鑰與一次性金鑰），分出去的 to-device 由 server 排隊保管到對方上線。
- **一次性金鑰的數量**：上游（vodozemac）固定讓 server 上保持 50 把，再加一把 fallback key（用完 50 把時頂上）。調高要改 fork 裡的常數、上游明講有風險
  （server 的剩餘數量可能比「用掉那把的訊息」先到，補得太勤會把還沒用上的私鑰擠掉），所以維持 50；重點是補得及時——`CryptoState` 一到就補。

## 1. 狀態放哪

| 狀態 | 誰存、存在哪 | 用途 |
|---|---|---|
| 自己裝置的身分金鑰、一次性金鑰私鑰（最多 5000 把）＋ fallback key | daemon，crypto store（`m/`） | 代表這台裝置；別人跟它建一對一通道 |
| 一對一加密通道（Olm session） | daemon，crypto store | 傳房間金鑰的管道 |
| **自己的房間金鑰**（每房一把 outbound Megolm session）＋「已經送過哪些裝置」 | daemon，crypto store | 加密自己送出的；決定還要補給誰 |
| **別人的房間金鑰**（inbound Megolm session） | daemon，crypto store | 解別人的 |
| 別人的裝置公鑰清單、追蹤中的人 | daemon，crypto store | 分金鑰時送給誰、用哪把公鑰 |
| 訊息（密文，解得開的連明文） | daemon，`cache.db` | UI 讀歷史；補解時找還沒解的 |
| **房間版本號、每個成員的裝置版本號**（`RoomDevices`） | **UI** | 送出時帶回來：號碼給 server 擋、成員給 daemon 分金鑰、也當「上一份」讓 1506 之後只重查變了的人 |
| 全局同步的起點（`g_seq`） | UI | 下次 `sync.recent` 從哪接（/docs/design/rooms/room-sync.md） |
| 還沒送成功的訊息與它的 `txn_id` | UI | 重送用同一個 `txn_id`，server 冪等 |

- **金鑰一律不離開 daemon**；分金鑰、換金鑰、補一次性金鑰全由 daemon 自動做。
- **daemon 不存 UI 那幾樣**：每房的 `RoomDevices` 快照 🚫 不存，每次由 UI 帶進來，或在回應裡交回給 UI。
- ⚠️ 「裝置清單」有兩樣東西：**裝置版本號**（每個成員一個雜湊，只拿來判斷「他的裝置變了沒」）在 UI；**裝置公鑰清單**（加密要用）在 daemon 的 crypto store。

## 2. 線上宣告與 refresh

- `Misc` 與 `Rooms` 兩條線的 `Hello.features` 宣告 `org.wbftw.device_versions`（`link_pool::features_of`）：
  - `Misc`：加密訊息從這條 `Event/Send`。宣告了，server 對**加密**事件一律要 `room_version`（漏帶是 `InvalidRequest`），明文不受影響。
  - `Rooms`：宣告了 server 才推 `Event/DeviceChanged`。
  - ⚠️ server 把宣告記在**連線**上、下一個 `Hello` 覆蓋（沒帶就收回）。所以只有開線的人 hello，共用 `Misc` 上的命令（`server.ping`、`room.history`、`sync.recent`）
    🚫 再 hello，帶的是開線那次的結果（/docs/design/daemon/link-pool.md §5、2026-10-05）。
- `room.refresh_devices { room, previous?, user?, server? }` → `{ room_version, members, shared }`：
  daemon 拿這一刻的成員清單與版本號（橋 `Members`）→ 跟 `previous` 比出誰的裝置版本號變了 → 只重查那些人（`/keys/query`）→ 雜湊對一次（不對再查一次，還不對就拒絕，fail closed）
  → 把這個房交給後台送金鑰（§3.1），🚫 等它送完。`previous` 不帶就每個人都查（只是查得多，送金鑰照樣只送缺的）。
  查裝置、對雜湊留在這個命令裡：那是 UI 要的答案（新的 `RoomDevices`），🚫 是散播金鑰。
  ⚠️ 回應的 `shared` 改成「這輪交給後台送的 to-device 數」（在本機排好的那幾個），🚫 代表已經送到。
- UI 什麼時候叫：點進房間、收到 `devices.changed`、或自己覺得版本號可能舊了。**daemon 🚫 不自己叫**（除了 §3 的 1506）。

## 3. 送出

`room.send_text { room, body, room_devices?, txn_id?, user?, server? }`：

| 房間 | 做什麼 |
|---|---|
| 明文 | 跟以前一樣 `Event/Send` 明文，不看 `room_devices` |
| 加密、沒帶 `room_devices` | 拒絕（`usage` 1100），🚫 不送明文 |
| 加密、帶了 | sdk `encrypt_and_send`：**在本機備好房間金鑰**（見下）→ 加密 → 帶 `room_devices.room_version` 送 → 把這個房交給後台送金鑰（§3.1） |

**送訊息🚫 綁發金鑰**（維護者 2026-10-05，§0）：送的路上🚫 任何金鑰的網路動作（`Members`／`/keys/query`／`/keys/claim`／`sendToDevice`），
🚫 等金鑰送到、🚫 管在途。先後順序不是問題：Matrix 🚫 要求金鑰先於訊息到，收的一方先留著解不開的、金鑰到了再補解（我們自己是 §6）。

「在本機備好房間金鑰」是上游 `OlmMachine::share_room_key`，它**只碰本機的 crypto store、不上網**：
1. 這個房還沒有自己的房間金鑰 → 當場建一把；
2. 該換了（則數或時間到期、跟 UI 帶來的成員比有人離開或裝置被移除、可見性或演算法改了）→ 當場換一把新的；
3. 給「本機已知、還沒拿到」的裝置排好要送的 to-device（存在那把 session 上，🚫 在這裡送）。還沒有 Olm 通道的裝置上游排一則「暫不提供（`m.no_olm`）」，
   但仍記成**還沒分到**：後台建好通道後再分一次，它就拿得到（上游 `ShareInfo::Withheld → ShareState::NotShared`）。

這一步也是防 panic 的：上游的加密在「沒有 outbound session」與「session 過期」時是 `expect`／`assert` 不是回錯，
加密只要 session 在、沒過期，🚫 管金鑰送到沒。先備好就把兩條都排除；備好到加密之間剛好跨過期限的那一瞬間，用 `catch_unwind` 接住轉成錯。

代價（維護者選的取捨）：在這個房第一次送、或剛換金鑰時，對方可能先收到解不開的訊息，金鑰等後台送到才解得開；
後台在 daemon 關掉前沒送完的，要等這個房下一次送或 refresh 才補（§8）。

**被 1506 擋**（帶的號碼過期）：daemon 自動跑一次 §2 的 refresh（`previous` 就是 UI 帶來的那份，只重查變了的人），然後回錯：

```json
{ "code": 1401, "msg": "…send again with the room_devices in data and the same txn_id",
  "result": null, "id": 12,
  "data": { "room_version": 9, "members": { "@bob:localhost": "4-0a1b2c3d4e", … }, "shared": 1, "txn_id": "wbf-…" } }
```

- `1401 room_devices_changed` 是 RPC 的號碼（/docs/design/rpc-specs/rpc-spec.md §5.2，server 家族 1400 裡拆出來的；server 那邊叫 1506，訊息裡照帶）。
- `data` 就是新的 `RoomDevices` ＋ `shared` ＋ 這則的 `txn_id`（UI 沒給的話是 daemon 產的）：UI 存下它、用同一個 `txn_id` 重送就過。
- 重拿也失敗：`data` 只有 `{ txn_id, current_room_version }`，`msg` 說明，UI 自己叫 `room.refresh_devices`。
- 🚫 **daemon 不自動重送**：使用者可能已經撤回或改了，重送的政策在 UI。

加密房的**檔案**走資料平面：UI 先 `media.create` ＋ `PUT` 傳完，再 `room.send_attachment` 帶 PUT 回的 manifest 與同一份 `room_devices`，1506 的處理跟文字一樣（/docs/design/rpc-specs/data-plane.md §5）。路徑版的 `room.send_file` 在加密房仍拒（§8）。

### 3.1 後台送房間金鑰（`key_share.rs`）

每個帳號一個後台 task，只做一件事：**把排好的房間金鑰送到該拿的裝置**。誰都🚫 等它。

- **交給它的時機**：加密送出之後（§3）、`room.refresh_devices` 與 1506 之後的 refresh 對完雜湊之後（§2）。
  交的是「哪個房、哪些成員」（成員是 UI 帶來的那份）；同一個房還沒做完又交一次，就併成一件、成員取最新的。
- **它做的事**，一個房一輪：
  1. 追蹤這些人，該查的 `/keys/query`；
  2. 缺 Olm 通道的裝置 `/keys/claim` 一次性金鑰、建通道；
  3. 上游 `share_room_key`：會把剛建好通道的裝置（之前排成 `m.no_olm` 的）補進來，回這把 session 上**所有還沒送出的** to-device；
  4. 一個一個 `sendToDevice`，成功的交回上游（`mark_request_as_sent`），上游就記得那台已經有了。
- **走哪條線**：`Keys`（金鑰的線，`key_sync.rs` 也在上面上傳自己的金鑰）。用 `reuse`，🚫 自己開線：線沒開就等下次有人交、或線開好時再做（線開好時把手上還沒做完的房再跑一輪）。
- **失敗**：那個房留著，隔一段時間（30 秒起、加倍到 5 分鐘）再試；發 `Note` 講一聲，🚫 回給誰（沒有人在等）。
  裝置雜湊的比對（fail closed）只在 refresh 做（§2），跟以前送出時一樣；後台🚫 另外判斷要不要發。
- **不存狀態**：要送的 to-device 本來就存在上游那把 session 上（crypto store），後台只在記憶體記「哪些房還有事」。
  daemon 重開之後那張表是空的，要等那個房下一次送或 refresh 再交給它（§8）。

## 4. `devices.changed` 推播

`Rooms` 線上 server 推來的 `Event/DeviceChanged` 原樣轉成 `devices.changed { user, changed_user, device_version, rooms: { room: room_version }, gap }`。
daemon 自己 🚫 不動作；UI 決定要不要對開著的房叫 `room.refresh_devices`（`gap: true` ＝ 前面有推送被丟，該把開著的房都 refresh 一次）。

## 5. 自己的金鑰：上傳與補一次性金鑰（`key_sync.rs`）

- 開 `Keys` 線時（`init_keys`）：先把訂閱時 server 跟著推的那個 `CryptoState`（sdk 收在 `DeviceSubscription::crypto_state`）交給狀態機，
  再 `send_outgoing_requests` 一次——裝置金鑰、一次性金鑰、fallback key 一起上傳。別人查得到這台，就是從這一步開始。
  ⚠️ 初始那份一定要先交：沒交的話 fallback key 要等到下一個 `CryptoState` 才會補（測試釘住，§9）。
- 之後每個 `CryptoState`：存量交給狀態機，它要補就補（上游一律補到 50 把；fallback key 到期才換），走 `Keys` 那條線上傳。
- 上傳失敗只講一聲（`Note`）：收金鑰照常，下一個 `CryptoState` 再試；一次性金鑰領光了還有 fallback key 撐著。

## 6. 解密

- **收到時**（`to_incoming`）：有金鑰就解，密文與明文一起存（`raw_event` 是密文、`content_json` 是明文、`decrypted = 1`）；沒金鑰只存密文、標成沒解。
  三個入口：房間推播（`room_sync.rs`）、`room.history` 往上游拉的那頁（`rooms_ops.rs`）、`sync.recent`（`sync_ops.rs`）。
  - `sync.recent` 的收批回呼是**同步**的、不能等解密：拉的時候記下這輪有哪些密文，整輪拉完、回給 UI 之前一起解（`decrypt_stored(… EventIds …)`）。
    🚫 不發 `room.message`（`Recent` 本來就不推，UI 拉完自己讀）。
- **金鑰到了**（`key_sync.rs` 每匯進一批，`ImportReport.room_keys` 是這批帶來的新房間金鑰）：對每一把，去 cache 找「這個房、用這把 session 加密、還沒解」的
  （`list_undecrypted_ciphertexts(BySession)`，`json_extract(raw_event, '$.content.session_id')`），解開、補存明文，
  **commit 之後照收訊息那條路發 `room.message`**（同一個 `event_id`，UI 當更新）。已經解了的不動、不再發。
- 引擎開不起來（crypto store 壞了之類）：講一聲，密文照存；之後引擎好了、金鑰那半會補解。

## 7. 程式碼接縫

| 問題 | 在哪回答 | 只在那裡 |
|---|---|---|
| 加密前房間金鑰備好了沒、要不要換 | sdk `encrypt_and_send` → 上游 `share_room_key`（只碰本機） | ✅ 呼叫端拿不到「沒備好就加密」的路（上游會 panic） |
| 房間金鑰送給誰、什麼時候送 | core `key_share.rs` → sdk `distribute_room_key` | 送出與 refresh 只「交給它」，🚫 自己送 |
| 一則 WS 收到的事件要怎麼寫進 cache | `room_crypto::to_incoming`（→ sdk `OlmEngine::to_incoming`） | 三個入口都叫它 |
| cache 裡還沒解的要怎麼補解、要不要通知 | `room_crypto::decrypt_stored` | 金鑰到了（`announce: true`）與 `Recent` 拉完（`false`）共用 |
| 被 1506 擋之後做什麼 | `Core::wbf_send_encrypted` | 自動 refresh、組 `data` |
| 這個房加密了沒（送**文字**） | `Core::find_local_room_encryption`（只看本地 `rooms.encrypted`：拿房間時寫、收到加密的證據時往上升，維護者 2026-10-05） | 本地不知道就是錯（先拿房間），🚫 當成沒加密。⚠️ 代價：本地記著「沒加密」、房間卻在這台沒在聽的時候（離線、訂閱線沒開）開了加密，就照送明文，直到下次拿房間或收到加密的證據——維護者選的取捨（daemon 只管 RPC 來的命令、🚫 每次上網問） |
| 這個房加密了沒（送**附件**） | `Core::wbf_is_room_encrypted`（問這一刻的 `m.room.encryption`，🚫 用快取：/docs/design/rpc-specs/data-plane.md §4.1） | 問不到就是錯，🚫 不當成沒加密 |
| 走橋的每支端點是哪個 kind／subtype | sdk `protocol.rs` 的 `BRIDGE_*` 常數（對 wbfuwunel 的 `/docs/bridge-specs/`） | core 的假 server（`test_support::bridged_reply`）也吃這些常數，🚫 不手寫 hex |

## 8. 不在這支（已知的缺口）

- **路徑版送檔進加密房**：`room.send_file` 在加密房拒絕（資料平面那條可以，/docs/design/rpc-specs/data-plane.md）。
- **後台沒送完就關了 daemon**：要送的 to-device 還在 crypto store 的那把 session 上，但後台「哪些房還有事」只在記憶體，
  重開之後要等那個房下一次送或 refresh 才補。要不要在 `Keys` 線開好時掃一次所有加密房（要每房的成員，得上網拿）待維護者決定。
- **`devices.changed` 🚫 觸發後台**：誰的裝置變了仍是 UI 決定要不要 refresh（§4，2026-09-29 定的）；refresh 之後才交給後台。
- **新裝置讀不到舊訊息**：送出當下不存在的裝置沒分到金鑰。wbf 帳號的金鑰備份（server 端 backup）與「向自己其他裝置要金鑰」都還沒接。
- **房間自己設的換金鑰期限**：`room_key_share_settings` 用上游預設（一週／100 則），🚫 還沒讀 `m.room.encryption` 的 `rotation_period_*`。
- **交叉簽章**：分享策略仍是 `AllDevices`（`IdentityBasedStrategy` 要先 bootstrap，/docs/design/keys/e2ee-walkthrough.md §16）。
- **補解寫失敗的那批不會自動重試**：金鑰到了、解開了，但 cache 寫失敗——錯誤會講出來（帶則數），那幾則仍是密文；
  那把金鑰已經匯入，之後不會再觸發補解（只有同一把金鑰再來才會）。要不要加一個觸發點（例如 `room.history` 讀到未解的就試一次）待維護者決定。

## 9. 測試

- sdk `tests/pipeline.rs`（會答橋的假 server）：沒 refresh 過就直接送——**送之前假 server 🚫 收到任何金鑰的請求**、不 panic、送的是密文帶 UI 的號碼、自己解得開（`to_incoming` 走 Decrypted、密文照帶）；
  `distribute_room_key` 之後才有 `/keys/query`／`/keys/claim`／`sendToDevice`；
  `to_incoming` 明文原樣、解不開的照存帶原因；refresh 與 1506 各一條。
- sdk `cache.rs`：`list_undecrypted_ciphertexts` 只回這個讀者同步過、還沒解、那把 session（或那幾則）的，舊→新；解開之後就不再出現。
- core `room_crypto.rs`（core 的假 server 多答橋的 `Members`／`GetStateEvent`／`KeysUpload`／`KeysQuery`／`KeysClaim`／`SendToDevice` 與 `Event/Send`，並學 server 的 1506）：
  - 加密房沒帶 `room_devices` 拒、🚫 不送明文；refresh 回 UI 要存的；送出去是密文、帶那個號碼；
  - **送的那一路假 server 🚫 收到 `Members`／`KeysQuery`／`KeysClaim`／`SendToDevice`**（`Event/Send` 之前一個都沒有）；後台之後把金鑰送出去；
    `SendToDevice` 失敗一次，後台隔一段時間重送、送成就不再送；線沒開時後台🚫 開線；號碼過期 → `RoomDevicesChanged`、`data` 帶新狀態與同一個 `txn_id`、訊息沒送；帶新狀態重送就過；
  - 明文房照舊；`room_devices` 形狀不對是 `Usage`；
  - 補解：cache 裡那把 session 的密文解開、補存、發 `room.message`（同 `event_id`、明文），第二次什麼都不做；
  - 房間線推來的密文存成解開的、`room.message` 帶明文；`DeviceChanged` 原樣轉成 `CoreEvent::DeviceChanged`；
  - `sync.recent` 拉到的密文在回給 UI 之前解開、🚫 不發 `room.message`；
  - `to_incoming` 有引擎就解、沒引擎原樣。
- core `key_sync.rs`：開線上傳一次（裝置金鑰＋一次性金鑰＋fallback key）；存量滿的 `CryptoState` 不上傳、剩 10 把的上傳一次。
- daemon：錯誤的 `data` 原樣進回應、沒有就不在；`devices.changed` 推播的形狀；`room.refresh_devices` 在方法表上、參數錯是 102。
- 真 server（`--ignored`，對本機 wbfuwunel 跑）：
  - core `an_encrypted_conversation_survives_a_new_device_over_the_real_server`：alice、bob 各一個 `Core` 登入、鉤子開五條線 → alice refresh、帶 `RoomDevices` 送 → bob 的 `room.message` 是解開的；bob 登第二台裝置 → alice 帶舊的送 → `RoomDevicesChanged`、`data` 是新狀態、訊息沒送 → 帶新狀態同一個 `txn_id` 重送 → bob 的新舊兩台都解得開。
  - daemon `real_server`：加密房沒帶 `room_devices` 是 1100；`room.refresh_devices` 回的整份當 `room_devices` 帶回去送，成功。
  - 另外還有 sdk `e2e_crypto_engine`（issue #45 的驗收）、`e2e_local_server`、core 房間／金鑰兩條、daemon 兩條。
  - ⚠️ client 把房間版本號當**不透明的值**、只比相不相等：wbfuwunel 的 `docs/room-version-prev` 把它從「只增不減的位置」改成「成員集合的雜湊」，兩種定義 client 都照常運作。sdk 的驗收因此斷言「不相等」，🚫 不斷言「變大」。

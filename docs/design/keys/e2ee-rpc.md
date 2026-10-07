# E2EE 的 RPC 面：狀態放 UI、金鑰由 daemon 自動、1506 之後 daemon 補完再一起回

> 維護者 2026-09-29 定的形狀（原話在 §0）。實作：
> - sdk：`crypto_engine.rs` 的 `encrypt_and_send`（只用已經分好的金鑰加密、送；🚫 建、換、分金鑰）、`room_key_state`（就緒了沒）、`distribute_room_key`（後台建／換金鑰並送出去）、`to_incoming`（收到時有金鑰就解），
>   `cache.rs` 的 `list_undecrypted_ciphertexts`（找還沒解的密文）。
> - core：`room_crypto.rs`（refresh、加密送出、1506 之後自動重拿、補解）、`key_share.rs`（金鑰線的後台：建、換、送房間金鑰、記就緒、重傳自己的金鑰，§3.1）、`key_sync.rs`（上傳金鑰、補一次性金鑰、金鑰到了補解）、
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

> （2026-10-06，取代上一條的「在途也不管」）message 送出時，從不自己產生金鑰，金鑰總是後台產生，送出訊息後立刻調用檢查金鑰函數，發現金鑰該換就立刻換一把，
> 這樣下一則訊息處理時，拿到的金鑰總是已經生成並送出過的，如果拿不到，就 blocking sleep 最多 2 秒，然後回訊息發送失敗，沒有金鑰。
> （實作是等後台的通知、🚫 真的 sleep；「就緒」以 UI 帶來的房間版本號為準，§3。）

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
- `room.refresh_devices { room, previous?, user?, server? }` → `{ room_version, members }`：
  daemon 在 `Keys` 線上（金鑰的網路動作都在那條，§3.1；線沒開就開，這是 UI 下的命令）拿這一刻的成員清單與版本號（橋 `Members`）→ 跟 `previous` 比出誰的裝置版本號變了 → 只重查那些人（`/keys/query`）→ 雜湊對一次（不對再查一次，還不對就拒絕，fail closed）
  → 交給後台對這一刻的狀態（`room_version` 與成員）先分好房間金鑰（§3.1），🚫 等它分完。`previous` 不帶就每個人都查。
  refresh 本身🚫 碰房間金鑰：它回的是 UI 要的答案（新的 `RoomDevices`）；建通道、建／換金鑰、送出去都是後台的事。
  例：bob 剛登一台新裝置 → 回 `{ "room_version": 9, "members": { "@alice:localhost": "1-…", "@bob:localhost": "4-0a1b2c3d4e" } }`，後台同時開始對 9 分金鑰。
  📎 回應原本有一個 `shared`（排了幾個 to-device），2026-10-06 拿掉：金鑰不在 refresh 裡排，這個數字沒有意義了。
- UI 什麼時候叫：點進房間、收到 `devices.changed`、或自己覺得版本號可能舊了。**daemon 🚫 不自己叫**（除了 §3 的 1506）。
  點進房就叫的好處：後台在使用者按送出前就分好了，第一則不用等（§3）。

## 3. 送出

`room.send_text { room, body, room_devices?, txn_id?, user?, server? }`：

| 房間 | 做什麼 |
|---|---|
| 明文 | 跟以前一樣 `Event/Send` 明文，不看 `room_devices` |
| 加密、沒帶 `room_devices` | 拒絕（`usage` 1100），🚫 不送明文 |
| 加密、帶了 | 等「這個房的金鑰對 `room_devices.room_version` 就緒」→ 加密 → 帶號碼在 `Misc` 上送 → 交給後台看要不要提早換 |

**送訊息只用已經分好的金鑰**（維護者 2026-10-06，§0）：

1. **就緒**＝這個房現在那把房間金鑰：存在、沒到期、沒作廢、排過的 to-device **全部拿到 server 的 Ack**，而且是對 UI 帶來的那個 `room_version` 分的。
   所以每一則加密訊息送出之前，它的金鑰對送出當下房裡**建得起 Olm 通道的每一台裝置**都已經在 server 上了；訊息在，金鑰就在。
   ⚠️ 例外：對方裝置一次性金鑰被領光、又從沒上傳過 fallback key（不支援 fallback 的舊 client、又長期離線）時，server 的 `/keys/claim` 整個略過它、通道建不起來，
   上游只能排一則 `m.no_olm` 拒絕通知；這則通知一樣會拿到 Ack（上游對同一台只送一次，之後連待送都沒有），所以這個房照樣算就緒、訊息照送。
   那台在補到通道之前送的那幾則解不開（上游把它記成「沒分到」，下一次分金鑰會再 claim、補給它，但只從當下的位置開始）。
   這是對方 client 的事：我們的 daemon 一定上傳 fallback key，而 server 發出 fallback key 只標記已用、照樣重發（wbfuwunel 的 `take_fallback_key`），所以我們自己的裝置離線再久都建得起通道。跟 §8「新裝置讀不到舊訊息」同一類。
2. **還沒就緒**（這個房第一次送、號碼變了、到期了、daemon 剛重開）→ 交給後台準備，最多等 **2 秒**（`ROOM_KEY_WAIT`，等的是後台的通知、🚫 輪詢）。
   等到就照送；等不到回 **`1402 room_key_not_ready`**，訊息**沒加密、沒送**，`data` 是 `{ "txn_id": "…" }`。後台照樣繼續分，UI 用同一個 `txn_id` 重送就好。
3. **送出之後**交給後台看這把是不是快到期了（上游預設一把用 100 則或一週；剩 5 則以內或 1 小時以內就算快到期）：是就**提早換一把、先分完**，下一則拿到的就是就緒的新金鑰，🚫 等它真的到期才在送的路上卡住。
4. **送的路上🚫 建、🚫 換、🚫 送金鑰**，也🚫 任何金鑰的網路動作（`Members`／`/keys/query`／`/keys/claim`／`sendToDevice`）；`Misc` 上只有 `Event/Send`。

📎 為什麼這樣（維護者 2026-10-05 → 10-06 的演變，留下來免得再繞一次）：
- 10-05 定的是「送訊息🚫 綁發金鑰」：先加密送出，金鑰之後由後台送。但上游換金鑰時會**丟掉舊那把還沒送出的 to-device**（`group_sessions/mod.rs` 的 `share_room_key`：換了之後只回新那把的待送），
  而我們已經用舊那把加密送出了訊息——那幾則就永遠解不開。不論怎麼持久化「還沒送完的」，到期換金鑰這一條都擋不住。
- matrix-sdk 的做法是每則都同步分完才加密、失敗就丟掉金鑰（`room/mod.rs` 的 `preshare_room_key`）：保證對，但每則都綁。
- 10-06 定的是兩者之間：**金鑰的事全在後台，送出只拿已經分好的那把**。被丟掉的待送永遠是還沒拿來加密過的，保證成立；
  平常（金鑰就緒）送出完全不等，只有金鑰事件那一則要等後台一趟。也因此**不需要任何持久化**（之前的 `m/ks.sealed` 拿掉了）：沒拿到 Ack 的金鑰從來沒被拿來加密過，daemon 重開丟了它也沒有訊息解不開。

上游的加密在「沒有 outbound session」與「session 過期」時是 `expect`／`assert` 不是回錯：就緒檢查把這兩條排除了；
檢查完到加密之間剛好跨過期限（或被後台換掉）的那一瞬間，用 `catch_unwind` 接住、當成沒就緒：sdk 回 `RoomKeyNotReady`，跟第 2 點同一條路（交給後台、回 1402）。

**被 1506 擋**（帶的號碼過期）：daemon 自動跑一次 §2 的 refresh（`previous` 就是 UI 帶來的那份，只重查變了的人；後台對新的號碼開始分），然後回錯：

```json
{ "code": 1401, "msg": "…send again with the room_devices in data and the same txn_id",
  "result": null, "id": 12,
  "data": { "room_version": 9, "members": { "@bob:localhost": "4-0a1b2c3d4e", … }, "txn_id": "wbf-…" } }
```

- `1401 room_devices_changed` 是 RPC 的號碼（/docs/design/rpc-specs/rpc-spec.md §5.2，server 家族 1400 裡拆出來的；server 那邊叫 1506，訊息裡照帶）。
- `data` 就是新的 `RoomDevices` ＋ 這則的 `txn_id`（UI 沒給的話是 daemon 產的）：UI 存下它、用同一個 `txn_id` 重送就過——重送時會等後台對新的號碼分好（第 2 點）。
- 重拿也失敗：`data` 只有 `{ txn_id, current_room_version }`，`msg` 說明，UI 自己叫 `room.refresh_devices`。
- 🚫 **daemon 不自動重送**：使用者可能已經撤回或改了，重送的政策在 UI。1402 也一樣。

加密房的**檔案**走資料平面：UI 先 `media.create` ＋ `PUT` 傳完，再 `room.send_attachment` 帶 PUT 回的 manifest 與同一份 `room_devices`，1506、1402 的處理跟文字一樣（/docs/design/rpc-specs/data-plane.md §5）。
路徑版的 `room.send_file`（2026-10-07 起）一樣帶 `room_devices`：這一刻房間加不加密問 server、`cipher` 跟房間對不上或加密房沒帶 `room_devices` 都在**上傳之前**拒；
傳完拿真的區塊再對一次（續傳用的是狀態檔裡那份加密）、走同一支 `wbf_send_message`。被 1506、1402 擋時檔案已經在 server 上：錯誤的 `data` 多帶 `manifest`（含金鑰，跟成功時的回應一樣敏感），UI 用它改走 `room.send_attachment` 重送、🚫 重傳檔案。
CLI 自己就是前端：`send` 在加密房（wbf 帳號）同一個命令裡先 `refresh_room_devices` 再帶著送；被 1401 擋就用 `data` 與同一個 `txn_id` 再送一次（只一次）——
文字重送 `send_text`，檔案拿 `data.manifest` 走 `send_attachment`。

### 3.1 金鑰線的後台（`key_share.rs`）

每個帳號一個後台 task。**房間金鑰只在這裡建、換、送**，server 回 Ack 才算數（維護者 2026-10-05：金鑰建立、刷新、更新都要上傳成功，這點要保證）。

- **交給它的事**：
  - `Prepare(房, room_version, 成員)`：這個房對這個號碼要有分好的金鑰。refresh 之後（§2）、1506 之後、送出時發現還沒就緒（§3 第 2 點）交。
    同一個房還沒做完又交一次，就併成一件、號碼與成員取最新的。
  - `AfterSend(房, …)`：剛送出一則。只讀本機看這把是不是快到期；是就「先丟掉、再分一把」，🚫 每則都上網。
  - 自己的金鑰：開 `Keys` 線時上傳裝置金鑰、補一次性金鑰（§5）沒拿到 Ack 的，交給它重傳。
- **分一個房做的事**（sdk `distribute_room_key`）：
  0. 問這個房的 `m.room.encryption`（只問這一項）拿換金鑰的期限 `rotation_period_ms`／`rotation_period_msgs`（2026-10-07）：沒設（或不是正整數）的那一項用上游預設（一週／100 則），
     問不到就這輪不分、留著重試（🚫 當成預設：房主設的可能比較短）。現在那把是照別的期限建的（房主改過）就先丟掉、照新的建——上游只在到期／作廢時換，
     所以房主改了期限，下一次 refresh（UI 進房）就照新的。上游自己把時間夾在一小時以上、則數夾在 1–10 000；「快到期」的門檻用同樣的夾法，
     再縮到期限的四分之一（預設仍是剩 5 則／1 小時；例：8 則的房剩 2 則就換），🚫 每送一則就換一把。
  1. 要提早換就先丟掉現在那把（`discard_room_key`，只丟一次，重試🚫 再丟）；
  2. 追蹤這些人，該查的 `/keys/query`；
  3. 缺 Olm 通道的裝置 `/keys/claim` 一次性金鑰、建通道——**一定在排之前**：上游的房間金鑰是在排的那一刻、照 session 當下的位置匯出的，
     沒通道的裝置只能排成 `m.no_olm`，之後再補只拿得到之後的位置（真 server 實測：`unknown message index, first known index 1`）；
  4. 上游 `share_room_key`：沒有、到期、作廢、有人離開就建新的；回這把 session 上所有還沒送出的 to-device；
  5. 一個一個 `sendToDevice`，成功的交回上游（`mark_request_as_sent`），上游就記得那台已經有了；
  6. 再看一次就緒（`room_key_state`）：是就記下「這個房對這個號碼就緒」、叫醒在等的送出。
- **走哪條線**：`Keys`。金鑰的網路動作都在這條：後台分金鑰、refresh（§2）、自己的金鑰上傳（§5）。
  後台用 `reuse`，🚫 自己開線：線沒開就等。vault 解鎖、線開好時（`init_keys`）就起這個 task 並叫醒它。
  ⚠️ 同一台狀態機的「送出所有待送請求」（sdk `send_outgoing_requests`）一次只跑一個（2026-10-07）：開線時的上傳自己的金鑰、refresh、後台分金鑰會同時進來，
  沒有序列化時它們各自拿同一批 `/keys/query` 去送、互相讓對方的回應過期，繞滿上限就失敗（CLI 新裝置第一個命令實測 35 個查詢）。
- **失敗**：留著，隔一段時間（30 秒起、加倍到 5 分鐘）再試；發 `Note` 講一聲。在等的送出等到時間就回 1402，不跟著等重試。
  裝置雜湊的比對（fail closed）只在 refresh 做（§2）；後台🚫 另外判斷要不要發。
- **🚫 存檔**：就緒表只在記憶體；待送的 to-device 在上游 crypto store 的 session 上。重開之後下一次 refresh 或送出會再交進來（§8 第 3 點）。

## 4. `devices.changed` 推播

`Rooms` 線上 server 推來的 `Event/DeviceChanged` 原樣轉成 `devices.changed { user, changed_user, device_version, rooms: { room: room_version }, gap }`。
daemon 自己 🚫 不動作；UI 決定要不要對開著的房叫 `room.refresh_devices`（`gap: true` ＝ 前面有推送被丟，該把開著的房都 refresh 一次）。

## 5. 自己的金鑰：上傳與補一次性金鑰（`key_sync.rs`）

- 開 `Keys` 線時（`init_keys`）：先把訂閱時 server 跟著推的那個 `CryptoState`（sdk 收在 `DeviceSubscription::crypto_state`）交給狀態機，
  再 `send_outgoing_requests` 一次——裝置金鑰、一次性金鑰、fallback key 一起上傳。別人查得到這台，就是從這一步開始。
  ⚠️ 初始那份一定要先交：沒交的話 fallback key 要等到下一個 `CryptoState` 才會補（測試釘住，§9）。
- 之後每個 `CryptoState`：存量交給狀態機，它要補就補（上游一律補到 50 把；fallback key 到期才換），走 `Keys` 那條線上傳。
- 上傳失敗（沒拿到 Ack）：交給金鑰線的後台（§3.1）退避重試到 server 回 Ack，並發 `Note`；收金鑰照常。一次性金鑰領光了還有 fallback key 撐著。

## 6. 解密

- **收到時**（`to_incoming`）：有金鑰就解，密文與明文一起存（`raw_event` 是密文、`content_json` 是明文、`decrypted = 1`）；沒金鑰只存密文、標成沒解。
  三個入口：房間推播（`room_sync.rs`）、`room.history` 往上游拉的那頁（`rooms_ops.rs`）、`sync.recent`（`sync_ops.rs`）。
  - `sync.recent` 的收批回呼是**同步**的、不能等解密：拉的時候記下這輪有哪些密文，整輪拉完、回給 UI 之前一起解（`decrypt_stored(… EventIds …)`）。
    🚫 不發 `room.message`（`Recent` 本來就不推，UI 拉完自己讀）。
- **金鑰到了**（`key_sync.rs` 每匯進一批，`ImportReport.room_keys` 是這批帶來的新房間金鑰）：對每一把，去 cache 找「這個房、用這把 session 加密、還沒解」的
  （`list_undecrypted_ciphertexts(BySession)`，`json_extract(raw_event, '$.content.session_id')`），解開、補存明文，
  **commit 之後照收訊息那條路發 `room.message`**（同一個 `event_id`，UI 當更新）。已經解了的不動、不再發。
- **`room.history` 讀到還沒解開的**（2026-10-07，`room_crypto.rs` 的 `retry_undecrypted_in_page`）：`sync` 是 `local`／`both` 時，這一頁裡還是密文的那幾則
  拿去再解一次（同一支 `decrypt_stored(… EventIds …)`），解開的補存、換掉頁裡那幾則，🚫 發 `room.message`（UI 正在讀這一頁）。
  補的是「金鑰到了、解開了、但當時寫 cache 失敗」那一批：那把金鑰已經匯入，不會再有「金鑰到了」叫它們。再試失敗只發 `Note`、照原樣回頁（🚫 讓讀歷史失敗）。
  `sync: server` 🚫 寫庫，那條拿到的就是剛解的；`sync.recent` 本來就會把這一輪拉到的密文（含原本就在 cache 裡的）再解一次。
- 引擎開不起來（crypto store 壞了之類）：講一聲，密文照存；之後引擎好了、金鑰那半會補解。

## 7. 程式碼接縫

| 問題 | 在哪回答 | 只在那裡 |
|---|---|---|
| 這把房間金鑰能不能拿來加密 | sdk `OlmEngine::room_key_state`（只讀本機：有、沒到期、沒作廢、排過的 to-device 全拿到 Ack） | ✅ `encrypt_and_send` 先問它，不是 `Ready` 就不加密；呼叫端拿不到「沒分好就加密」的路（上游會 panic） |
| 這個房的金鑰對這個房間版本號就緒了沒 | core `key_share.rs` 的就緒表（房 → 分好時的房間版本號）＋上面那一項 | 送出只問 `is_room_key_ready_within`，🚫 自己比 |
| 房間金鑰什麼時候建、換、送給誰；自己的金鑰沒上傳成功怎麼辦 | core `key_share.rs`（金鑰線的後台）→ sdk `distribute_room_key`／`discard_room_key`／`send_outgoing_requests` | 房間金鑰**只在這裡**建、換、送；refresh、送出、`init_keys` 只「交給它」 |
| 一則 WS 收到的事件要怎麼寫進 cache | `room_crypto::to_incoming`（→ sdk `OlmEngine::to_incoming`） | 三個入口都叫它 |
| cache 裡還沒解的要怎麼補解、要不要通知 | `room_crypto::decrypt_stored` | 金鑰到了（`announce: true`）與 `Recent` 拉完（`false`）共用 |
| 被 1506 擋之後做什麼 | `Core::wbf_send_encrypted` | 自動 refresh、組 `data` |
| 這個房加密了沒（送**文字**） | `Core::find_local_room_encryption`（只看本地 `rooms.encrypted`：拿房間時寫、收到加密的證據時往上升，維護者 2026-10-05） | 本地不知道就是錯（先拿房間），🚫 當成沒加密。⚠️ 代價：本地記著「沒加密」、房間卻在這台沒在聽的時候（離線、訂閱線沒開）開了加密，就照送明文，直到下次拿房間或收到加密的證據——維護者選的取捨（daemon 只管 RPC 來的命令、🚫 每次上網問） |
| 這個房加密了沒（送**附件**） | `Core::wbf_is_room_encrypted`（問這一刻的 `m.room.encryption`，🚫 用快取：/docs/design/rpc-specs/data-plane.md §4.1） | 問不到就是錯，🚫 不當成沒加密 |
| 走橋的每支端點是哪個 kind／subtype | sdk `protocol.rs` 的 `BRIDGE_*` 常數（對 wbfuwunel 的 `/docs/bridge-specs/`） | core 的假 server（`test_support::bridged_reply`）也吃這些常數，🚫 不手寫 hex |

## 8. 不在這支（已知的缺口）

- **`devices.changed` 🚫 觸發後台**：誰的裝置變了仍是 UI 決定要不要 refresh（§4，2026-09-29 定的）；refresh 之後才交給後台。
- **daemon 重開之後的第一則要等**：就緒表只在記憶體裡，重開後每個房的第一則都要等後台對那個房間版本號跑一輪（金鑰都還在的話只是查一次、什麼都不用送）。UI 點進房就 refresh 的話，這一輪在使用者按送出前就做完了。
- **上游升級會清掉自己的房間金鑰**：上游的 migration 清過兩次 `outbound_group_session`（格式變了就整張清、讓它換金鑰）。對我們無害：送出前會發現「這個房沒有金鑰」、等後台分一把新的；被清掉的那把沒送到的 to-device 從來沒被拿來加密過。
- **新裝置讀不到舊訊息**：送出當下不存在的裝置沒分到金鑰。wbf 帳號的金鑰備份（server 端 backup）與「向自己其他裝置要金鑰」都還沒接。
- **建不起 Olm 通道的裝置也讀不到**：對方沒有 fallback key、一次性金鑰又被領光時，那台只收到 `m.no_olm`、房間照樣算就緒（§3 第 1 點）；它之後補到通道，下一次分金鑰才補給它（只從當下的位置）。🚫 每送一則就重 claim（matrix-sdk 那樣）：只有不支援 fallback key 的舊 client 會遇到。
- **交叉簽章**：分享策略仍是 `AllDevices`（`IdentityBasedStrategy` 要先 bootstrap，/docs/design/keys/e2ee-walkthrough.md §16）。
- **補解寫失敗的那批要等有人讀到才再試**：金鑰到了、解開了，但 cache 寫失敗——錯誤會講出來（帶則數），那幾則仍是密文；
  `room.history` 讀到那一頁、或 `sync.recent` 再拉到它們時才再解（§6，2026-10-07 照 /docs/handover.md §7 的預設做）。🚫 背景自己掃整個 cache。

## 9. 測試

- sdk `tests/pipeline.rs`（會答橋的假 server）：
  - `sending_before_the_key_is_distributed_sends_nothing_and_goes_through_after_it_is`：還沒分好就送 → `RoomKeyNotReady`，假 server **一個請求都沒收到**（🚫 建金鑰、🚫 加密、🚫 送）、不 panic；
    `distribute_room_key` 之後 `room_key_state` 是 `Ready`，再送就過，送的是密文帶 UI 的號碼、自己解得開（`to_incoming` 走 Decrypted、密文照帶）；
  - refresh 雜湊對不上重查一次仍不對就拒；refresh → 分好 → 送，號碼過期是 `RoomDevicesChanged`（結果，不是錯）；
  - `to_incoming` 明文原樣、解不開的照存帶原因。
- sdk `cache.rs`：`list_undecrypted_ciphertexts` 只回這個讀者同步過、還沒解、那把 session（或那幾則）的，舊→新；解開之後就不再出現。
- core `room_crypto.rs`（core 的假 server 多答橋的 `Members`／`GetStateEvent`／`KeysUpload`／`KeysQuery`／`KeysClaim`／`SendToDevice` 與 `Event/Send`，並學 server 的 1506）：
  - 加密房沒帶 `room_devices` 拒、🚫 不送明文；refresh 走 `Keys` 線、🚫 碰 `Misc`，回 UI 要存的；送出去是密文、帶那個號碼；
  - 號碼過期 → `RoomDevicesChanged`、`data` 帶新狀態與同一個 `txn_id`、訊息沒送；帶新狀態重送就過（送出時等後台對新的號碼分好）；
  - 明文房照舊；`room_devices` 形狀不對是 `Usage`；
  - 補解：cache 裡那把 session 的密文解開、補存、發 `room.message`（同 `event_id`、明文），第二次什麼都不做；
  - 房間線推來的密文存成解開的、`room.message` 帶明文；`DeviceChanged` 原樣轉成 `CoreEvent::DeviceChanged`；
  - `sync.recent` 拉到的密文在回給 UI 之前解開、🚫 不發 `room.message`；
  - `to_incoming` 有引擎就解、沒引擎原樣。
- core `key_share.rs`（一帳號兩條線各接一個假 server；測試裡重試間隔縮成 100–800 毫秒、`ROOM_KEY_WAIT` 縮成 500 毫秒）：
  - `an_encrypted_send_only_uses_a_key_the_background_prepared_on_the_keys_line`：送的那一路（`Misc`）一個走橋的請求都沒收到；金鑰在送之前就由後台在 `Keys` 線上分好；
  - `a_send_without_a_prepared_key_waits_then_fails_without_sending_and_goes_through_once_the_key_is_ready`：`Keys` 線沒開 → 後台🚫 開線、等過 `ROOM_KEY_WAIT` 回 `RoomKeyNotReady`、`data` 帶 `txn_id`、什麼都沒送；開線之後分好，同一個 `txn_id` 重送就過；
  - `a_failed_prepare_is_retried_within_the_wait_and_stops_once_the_key_is_ready`：第一個走橋的請求失敗 → 隔 `RETRY_FIRST` 重試 → 在等待時間內分好、照送 → 之後🚫 再試（數「試過幾次」才看得出來）；
  - `a_key_close_to_expiry_is_replaced_after_a_send_before_the_next_one`：第 95 則送完，後台提早換一把、先分完；第 95 則用舊的，第 96 則用新的；
  - `an_own_key_upload_that_failed_is_retried_on_the_keys_line_until_the_server_acknowledges_it`：自己的金鑰上傳失敗 → 隔一段時間再傳一次 → 傳成就停。
- core `key_sync.rs`：開線上傳一次（裝置金鑰＋一次性金鑰＋fallback key）；存量滿的 `CryptoState` 不上傳、剩 10 把的上傳一次。
- core `error.rs`：`RoomKeyNotReady` 是 1402（逐字對 /docs/design/rpc-specs/rpc-spec.md §5.2）。
- daemon：錯誤的 `data` 原樣進回應、沒有就不在；`devices.changed` 推播的形狀；`room.refresh_devices` 在方法表上、參數錯是 102。
- 真 server（`--ignored`，對本機 wbfuwunel 跑）：
  - core `an_encrypted_conversation_survives_a_new_device_over_the_real_server`：alice、bob 各一個 `Core` 登入、鉤子開五條線 → alice refresh、帶 `RoomDevices` 送 → bob 的 `room.message` 是解開的；bob 登第二台裝置 → alice 帶舊的送 → `RoomDevicesChanged`、`data` 是新狀態、訊息沒送 → 帶新狀態同一個 `txn_id` 重送 → bob 的新舊兩台都解得開。
  - daemon `real_server`：加密房沒帶 `room_devices` 是 1100；`room.refresh_devices` 回的整份當 `room_devices` 帶回去送，成功。
  - sdk `e2e_crypto_engine` 的 issue #45 驗收照「先 `distribute_room_key`、再送訊息」的順序跑；
  - 另外還有 sdk `e2e_crypto_engine` 其他幾條、`e2e_local_server`、core 房間／金鑰兩條、daemon 兩條。
  - ⚠️ client 把房間版本號當**不透明的值**、只比相不相等：wbfuwunel 的 `docs/room-version-prev` 把它從「只增不減的位置」改成「成員集合的雜湊」，兩種定義 client 都照常運作。sdk 的驗收因此斷言「不相等」，🚫 不斷言「變大」。

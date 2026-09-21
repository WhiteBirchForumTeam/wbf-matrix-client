# 帳號的會話：探活、登入、登出、誰用 Client

> 維護者 2026-09-21 定的規矩，全文照抄在 §0。這份文件是那段話落到程式的形狀；連線池本身在 `link-pool.md`，E2EE 的金鑰在 `e2ee-walkthrough.md` §13。
> 兩支 PR：**A**（PR #54，已合：探活不帶 token、以 server 為鍵；四條線；登出封鎖）、**B**（wbf 帳號不建 matrix-sdk 的 Client：登入登出走標準 HTTP、`m/` 由 OlmEngine 開、房間與送訊息走 WS、備份暫擋）。

## 0. 規矩（維護者原話，2026-09-21）

> 1. 先探活，探明 server 版本。
> 2. 是標準 matrix，走 matrix sdk 可以用她的 client，基本全部走原生。
> 3. 是 wbf server，走 wbf sdk，不用 client，基本全部走 ws。
>
> 登入登出維持 HTTP 慣例——改用標準原生 HTTP post，自己寫兩個 function 包裹登入登出。
> WebSocket 的登入只走 token，也就是 http 拿回的 token 用於 websocket。
>
> 登入成功後拿到 token，在 ws 開連線；如果 ws 遇到未登入則自動關連線，由 server 端實作，client 不用處理什麼。
> 登出：先暫時 block 連線池，發登出 HTTP，確定登出成功，主動關閉連線池（遠端已經先關閉那就關閉，無所謂，二步關閉，不要二次跳錯），然後清除本地資料庫，走原子清除。
> 登出失敗時 no op，連線照常，解除登出封鎖，連線池可以繼續放新封包。
> 未登入不允許啟動 ws 池：先登入，然後探測版本，是 wbf，啟動 wbf sdk backend（連線池開啟，每個連線打一次登入 token，確保連線真的登入）。
>
> 金鑰：最終完全排除 matrix sdk client，只使用她的演算法加密解密；金鑰存放是她自己的 db（crypto store），我們不存一份。訂閱房間訊息與訂閱 key 暫時共用同一條連線。

## 1. 探活：不帶 token 的 WS Hello，以 server 為鍵

- server 允許不帶 Bearer 的升級：未登入的連線只接受 `Hello`／`Ping`，30 秒沒登入自己關（wire-format §6.3.3）。所以探活**不需要帳號**：
  `WsChannel::connect_anonymous(server)` → `hello` → 看 `protocol` 認不認得 → 丟掉那條線。
- 探測結果以 **server URL** 為鍵（`Core::backends`）。之前以帳號目錄為鍵，是因為拿那個帳號的 token 去探（PR #33 審查 rumia：A 的 token 壞了不能拖累 B）；不帶 token 之後這個理由沒了，
  而「這台 server 講不講 wbf」本來就是 server 的事實。探不到（連不上、Hello 不回）**只算這一次**、不寫進去，下次重探。
- 探活在登入**之前**就做得到（§3 的順序需要）。什麼時候作廢：daemon 重開；之後監督者重連時重探。

## 2. 兩種 backend，一刀切在探活

| 探到 | backend | 登入登出 | 房間、訊息、媒體、金鑰 |
|---|---|---|---|
| 一般 Matrix | `MatrixBackend`（matrix-sdk 的 Client） | Client 的 login／logout | 全部原生（/sync、Room::send、她的 OlmMachine、她的備份） |
| wbf | wbf-sdk | **標準 HTTP** `/login`、`/logout`（`wbf_sdk::login`，自己包的兩支，不經 Client） | 全部 WS：連線池四條線、`Event/Recent`／`Send`、橋、`Device/*`；金鑰是 `OlmEngine` 開的 `m/`（**唯一主人**） |

誰記得這個帳號走哪一邊：`Session::backend`（`matrix_sdk_client`／`wbf_sdk`，封在 `session.sealed` 裡）。登入時定、之後不再探：wbf 帳號的 `get_backend_kind` 直接回 `WbfSdk`（server 暫時不通時把它探成「一般 Matrix」會讓池那條路回「接錯線」而不是 `Network`）。
`None`（舊版封的、`--token` 接的）照舊：探活＋`store_dir`。消費端用 `Core::is_wbf_account` 問；`backend_of`（開 Client 的唯一入口）對 wbf 帳號一律拒（`no_matrix_client_error`）。

🚫 wbf 帳號**不建 Client**。之前 3a／3b 的 `OlmEngine` 與 Client 各開一次 `m/` 的 crypto store、各一台 OlmMachine，那是「我們自己存了一份」的變形；Client 拿掉之後只剩她的 crypto 層與她的 DB，我們不存金鑰（`m/td.json` 只有水位與待銷毀清單）。
📎 crypto 層自己就有備份（`BackupMachine`）、secret storage（`SecretStorageKey`）、驗證的機器；Client 只是 HTTP 與 /sync 的膠水。缺的是那幾支 HTTP 端點——走橋。

## 3. 登入（wbf）

```
探活(server) → 一般 Matrix？→ MatrixBackend::login（不變）
             → wbf？      → HTTP /login（自己包）→ token → session.sealed → OlmEngine::open(m/) → 連線池可用（每條線開時 Bearer 升級 + hello）
```

- 登入前**不准**起 WS 池：池要 session（`pool_of_account` 先 `session_of`）。
- 拿回 `user_id`（權威拼法：目錄改名那段照舊）、`device_id`、`access_token`。沒要 refresh_token，token 沒有期限。
- 順序：登入 → 目錄改成權威拼法 → **建 `m/`** → 封 session。`m/` 建不起來就把剛拿到的 token 撤掉（best effort）再回錯：🚫 不留「登入了、但沒有金鑰庫」的帳號。
- `m/` 由 `OlmEngine::open` 建（同一把 `matrix_store_key`）。裝置金鑰的上傳（`send_outgoing_requests`）跟 E2EE 那支一起接；在那之前這台裝置在 server 上沒有裝置金鑰，別人加密不到它——這是刻意的過渡，不是漏。
- 「每個連線打一次登入 token，確保連線真的登入」＝ 池開線的 `hello`：Bearer 升級過了不算，Hello 回來才算。

## 4. 登出（兩種 backend 同一套順序）

| 步 | 做什麼 | 失敗時 |
|---|---|---|
| 0 | 兩關閘門（歷史救得回來？）——不變 | 拒絕，什麼都不動 |
| 1 | **封池**：這個帳號標成「登出中」，`pool_of_account` 從此拒絕（`AccountBusy`），正在跑的命令不受影響 | — |
| 2 | HTTP `/logout`（wbf：自己包的那支；一般 Matrix：Client 的）| **no-op**：解封，池照常收新封包，錯原樣回 |
| 3 | 成了 → `close_all`：等每一格的鎖、取出、關掉；遠端已經先關的就只是丟掉，🚫 不二次跳錯 | — |
| 4 | 清本地：`session.sealed`、`m/`、快照、current、最後一個帳號時的 cache.db——照原本的原子清除 | 照原本 |
| 5 | 解封（session 已經沒了，之後 `session_of` 自己會回 NotLoggedIn；解封是為了重登入） | — |

- 🚫 池裡不存「登出了沒」：真相是 server 的 token 表與本地的 `session.sealed`；「登出中」是**帳號**的狀態，記在 `Core`（跟生命週期鎖同一層），不是池的。
- WS 過期或被撤由 server 每個 message 重驗、關 1008；client 不特別處理，池下次取用看到 `is_closed` 就重開（開不起來就是 hello 被拒 → 錯原樣回）。

## 5. 四條線（暫時）

`Misc`、`Upload`、`Download`、`Subscriptions`（房間事件與金鑰事件共用）。server 每台裝置預設 4 條 WS（`wbf_ws_max_connections_per_device`），先不動 server；將來要分開就是多一個角色（link-pool.md §1）。

## 6. wbf 帳號暫時做不到的（PR B 落地後、E2EE 與備份搬家之前）

「wbf 帳號」那欄就是現在的行為（core 的 `wbf_rooms.rs`；sdk 的 `room_state.rs` 把狀態組成 `Conversation`，規則跟 Client 那邊的 `describe` 同一套）。

| 功能 | 一般 Matrix（走 Client） | wbf 帳號（不建 Client） |
|---|---|---|
| `room.list`／`room.get` 的 `server`／`both` | Client 的 /sync | 橋 `JoinedRooms`（0x13/0x28）＋`m.direct`（`GetAccountData` 0x11/0x25）＋每房 `GetState`（0x14/0x21）組 `Conversation`，`both` 寫進 `room_list`；`local` 不變。⚠️ N 間房是 N＋2 次往返；狀態超過 2 MiB 的房 server 回 `TooLarge`，整個呼叫失敗（講出來比少列一間好） |
| `room.send_text` | `Room::send`（含加密） | `Event/Send` 明文（`txn_id` 隨機：server 去重鍵在帳號、不分裝置，wbfuwunel #78）；**加密房拒絕**（1100，問的是這一刻的 `GetState`、🚫 不用快取），E2EE 那支接 `encrypt_and_send` |
| `room.send_file` 的送事件半段 | `Room::send`（`attachment_declared: false`） | `Event/Send` 帶 `attachments`（約定 §5.2 的宣告終於成立，`attachment_declared: true`）；加密房在**上傳之前**就拒。兩邊的 content 同一份（`event_json::file_message_content`） |
| `room.history` 錨點不在本地 | `/context` | 拒絕（1100），等 wbfuwunel #64 的 `before_event_id`；`sync=both` 先把錨點寫進快取就翻得下去 |
| `watch`（CLI） | /sync 的迴圈 | 拒絕（1100）：daemon 的新訊息走訂閱＋推播（第 6 階段） |
| `backup.*`、`recovery.*` | Client 的備份與 SSSS | **拒絕、回明確的錯**（1100，`backend_of` 擋；🚫 不靜默失效）；搬到 crypto 層＋橋（`BackupMachine`、`SecretStorageKey`、橋的 `/room_keys`、account data）排在 E2EE 的 RPC 面之後 |
| 登出閘門的「server 那份救得回來嗎」 | Client 問 backup status | 問不到 → 只認本機的 recovery key 或 `accept_history_loss`（fail closed，1021） |
| 裝置金鑰上傳（`/keys/upload`） | Client 登入就傳 | **還沒傳**：`m/` 建了、身分金鑰生了，上傳跟 E2EE 那支一起接（§3）。在那之前別人加密不到這台裝置 |

## 7. 測試

- A：探活對真 server（不帶 token，daemon 的 `real_server` 流程）；探活失敗不記（沒人在聽的位址）；一台 server 一格；四條線的角色表；
  登出封鎖（封了 `pool_of_account` 拒、guard 丟掉就解封；HTTP 失敗 no-op；**HTTP 成功之後解封、重登入開得了池**——本機起一個回 200 的迷你 HTTP 當 `/logout`）。
  ⚠️ 沒有假的 wbf server：「接得上但版本不認得」那格沒測；`connect_anonymous` 走真的 tungstenite，記憶體對接驅動不了它。
- B：登入（探測結果直接記進註冊表、`/login` 由回 200 的迷你 HTTP 扮）→ session 記 `wbf_sdk`、`m/` 只有 crypto store、忘掉探測也還是 wbf；
  路由（session 指向沒人聽的位址）：`room.list`／`get`／`send_text` 到開線才失敗（`Network`，🚫 不是「log in again」），`backup.status`／`watch`／錮點不在本地的 `history` 是 `Usage`、登出閘門是 `HistoryWouldBeLost`；
  `room_state.rs`：Group／Direct（要 m.direct 且兩人）／Channel（門檻 100）、v12 建房者無限、字串型 power level、沒有 algorithm 的 encryption 不算加密、名字的後備順序；`file_message_content` 的形狀；
  真 server（daemon `real_server`）：`room.list sync=both` 走橋、`room.send_text` 走 `Event/Send`、`backup.status` 1100、`sync=server` 第二頁 1100。
  ⚠️ 沒測的：`send_file` 走 `Event/Send`（沒有 daemon 的 e2e）、加密房被拒（要一間加密房）、一般 Matrix 那條路的登入（沒有一台不講 wbf 的 server；它的程式沒動）。

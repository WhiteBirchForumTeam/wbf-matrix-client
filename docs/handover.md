# 交接：現在在哪、怎麼跑、下一步

> 給下一個接手的人（人或 agent）。2026-09-06 寫、2026-09-21 更新（上一版 2026-09-14），每次交接更新。設計理由不在這裡，在 `docs/design/`；這裡只講**現況、怎麼跑、坑、下一步**。

## 1. 現況一句話

第 1 步（`wbf-wire` codec）、第 2 步（`wbf-sdk` 密碼層／通道／上傳下載、`apps/wbf-cli`）做完；
第 3 步（接 matrix-sdk 做房間）第一版做完：`rooms`、`send --text|--file`、`watch`、`read`、`files` 對本機 wbfuwunel 全走過。
本地資料庫三步都合併了：vault 與金鑰（#11）、`cache.db` 多帳號混存（#13）、媒體儲存池（#14）。`Event/Recent` 跟上 server 的拉窗＋`Batch` 串流（#16）。
PR #19 做完資料目錄的兩層路徑加密、`account` 一族與房間金鑰備份；PR #20 定了架構 v2 的形狀。
**架構 v2 的第一塊落地了**：命令的「做什麼」全部搬進 `crates/wbf-core`（#24），
`apps/wbf-cli` 只剩「解析參數 → 叫一個 core 方法 → 印 JSON」。
**架構 v2 的 daemon 也落地了**：`crates/wbf-daemon` 有加密的 RPC、資料目錄獨佔、token 生命週期，
全部有 core 對應的 method 都接上了（#30、#31）。daemon 執行期前三階段（#32：事件帶 `user`／`job`、
`cache.db` 單一寫入者、`sync=local|server|both`）與 backend 接縫（#33：`transport` 就是選 backend、探測決定用哪一套）也合了。
PR #1–#34 全部合併。

**2026-09-15 → 09-21 這一段：E2EE 全走 WS 做到「可以送、可以收、被擋了知道怎麼修」。** 房間歷史走 wbf（#35–#39）、account destroy 什麼都不留（#40）、
跟上 server 的橋（#42）、E2EE 走讀文件（#44）；然後 issue #45（server 的房間版本號／F1–F4 做完後 client 要對齊的四件事）分四支全合：
#46 協議層（向量、1506、`room_version`、裝置版本號雜湊）、#47 橋的通用入口（`call_bridge`、Members、發 to-device）、
#48 `OlmMachine` 引擎與 to-device 的「拉」（Fetch／匯入／落地／銷毀鎖成一步、Subscribe／Unsubscribe）、#49 送（`refresh_room_devices`、
`encrypt_and_send` 帶 `room_version`、1506 是結果不是錯、`decrypt_room_event`）。**#45 的驗收對真 server 走通**：Bob 登新裝置 → Alice 帶舊號碼送被 1506 擋 →
refresh 只比出 Bob、金鑰補到新裝置 → 同 txn_id 重送接受 → 兩台都解得開。分工定案（e2ee-walkthrough §16.6）：**訊息是 UI 的，金鑰是 daemon 的**。

**第 4 階段的 SDK 那半（2026-09-21）**：`WsChannel` 底下換成 `link::WsLink`——讀取 task ＋ 送出 task ＋ 會話表（`design/ws-receive-dispatch.md`）。每個收到的 pack 依 id 交給在等的會話，
推播進訂閱的 handle（`WbfClient::device_subscription`），`Superseded` 進訂閱當終點，順序亂掉不出事；「丟推播」那段拿掉了。每個收到的 pack 都經過一個鉤子（`ReceivedHook`），之後 daemon 的 RPC 面在鉤子裡決定要不要送 UI。

**連線生命週期（2026-09-21，`design/link-pool.md`）**：一個帳號四條線（misc／upload／download／subscriptions；房間與金鑰的訂閱暫時共用一條，server 每台裝置預設 4 條）的連線池在 core，要用才開、斷了下次要用再開、登出全關、沒有背景監督者；
每條線開關發 `link.state`、收到的每個 pack 發 `pack.received`（只有標頭）。daemon 接上了 `subscribe`／`unsubscribe`、每條 RPC 連線一個推播 task（訂了才推；`progress`／`note` 發那個請求的連線不用訂）、`desync`。
每條線自己一個**心跳**（照 WireGuard：24 秒一次、最近 20 秒有通訊就跳過、沒 Pong 就當死），閘著的線不會被 server 的 300 秒 idle 收掉。

**帳號的會話（2026-09-21，`design/account-session.md`，維護者定的規矩）**：探活不帶 token（未登入的 WS Hello）、以 server 為鍵；登入登出只走標準 HTTP，WS 只用 token；
登出是「封池 → HTTP 登出（只有成與不成）→ 成了關池（等在跑的做完）→ 清本地 → 解封；不成就解封、no-op」。**下一支（B）**：wbf 帳號不建 matrix-sdk 的 Client，`m/` 由 OlmEngine 開、房間與送訊息走 WS、備份暫擋。

**還沒有：UI、E2EE 接進 daemon／CLI 的產品路徑（沒有任何一條路宣告 feature；接在 keys 那條線上）、`Event/Subscribe` 的 codec 與 rooms 那條線的內容、監督者（背景重連、退避）、交叉簽章、cancel、資料平面 HTTP、單發命令列。**

## 2. 讀哪些文件、什麼順序

| 順序 | 檔 | 講什麼 |
|---|---|---|
| 1 | [`README.md`](../README.md) | 佈局、狀態表、怎麼跑測試、貢獻規則 |
| 1.5 | [`design/architecture-v2.md`](design/architecture-v2.md) | **daemon／RPC／四個前端的分層**（維護者 2026-09-09 定的方向）。要動介面之前先看這份 |
| 1.55 | [`design/rpc-spec.md`](design/rpc-spec.md) | **草案**——前端 ↔ daemon 的逐條訊息：method 表、`params`／`result`、code 表（`CoreErrorKind` 的號碼在這）、推播、資料平面的 HTTP。2026-09-12 第一版；§10 每個 method 的現況（判準：走我們自己的 WS 才算做完） |
| 1.57 | [`design/daemon-runtime.md`](design/daemon-runtime.md) | daemon 跑起來之後：多帳號怎麼落到 `cache.db`（§2 **一個 server 一個寫入者**，#32 做了）、UI 的每個動作走本地讀還是上游拉（§3 `sync` 參數；§3.5 **backend 與 transport**，#33）、事件扇出與 `user` 規則（§5）、通知為什麼不在 rpc-spec（§7）、`job` 與 `cancel`。**§11 九階段**：1–3 ✅，4–9 還沒 |
| 1.58 | [`design/e2ee-walkthrough.md`](design/e2ee-walkthrough.md) | **E2EE 從建房到退出每一步發生什麼**（§1–§11）、只把 `OlmMachine` 當狀態機要自己寫哪十件（§13）、**#45 四支的落點與實跑踩到的事（§16.1–§16.4）、整套版本號驅動的分發邏輯對照 server 設計（§16.5）、UI 與 daemon 的分界（§16.6）**。要碰 `crypto_engine` 之前先看這份 |
| 1.6 | [`design/to-device-client.md`](design/to-device-client.md) | **client 端怎麼接 `0x16 Device`**（to-device：金鑰、驗證、SSSS）。⚠️ 線上格式的權威在 wbfuwunel 的 `wbf-wire-format.md` §3.2 與 `wbf-to-device.md`，這份只寫我們最容易寫錯的地方與待辦 |
| 2 | [`design/plan-v1.md`](design/plan-v1.md) | 範圍、順序、進度；**§7.1**（本地不存）與 **§7.2**（耦合方向：上游 SDK 是可拆的零件）是所有程式的前提 |
| 3 | [`design/wbf-client-convention-for-chunk.md`](design/wbf-client-convention-for-chunk.md) | client 之間的約定：每塊怎麼加密、事件區塊、seek；**§5.2 送事件要宣告附件**（等 server 定案） |
| 4 | [`design/chat-model.md`](design/chat-model.md) | 聊天模型（Conversation／Message）、怎麼接 Matrix、Telegram 有 Matrix 沒有的逐列定案、`r_seq`／`g_seq`、§6 第 3 步範圍與差異 |
| 5 | [`design/wbf-cli-spec.md`](design/wbf-cli-spec.md) | CLI 每個命令、exit code、manifest、狀態檔、session 檔、驗收腳本 |
| 6 | [`design/local-cache-db.md`](design/local-cache-db.md) | 本地資料庫：主金鑰與五把子金鑰（§4）、房間金鑰備份（§10）、佈局（§5.6）、`cache.db` 的 schema 與多帳號混存規則（§6）、媒體儲存池與它的檔案格式（§8、§8.8）。檔頭的進度表三段都 ✅ |

server 端的權威在 wbfuwunel repo：`docs/design/chunked-upload-spec.md`（線上規格）、`room-seq-and-recent.md`、`media-attachments.md`（提案）、`wbf-vectors.json`（整份複製到本 repo，不手改）。

## 3. 程式碼在哪

```
crates/wbf-wire/         pack、EncryptedFileInfo、CRC-32C；純函數。tests/vectors.rs 對 docs/design/wbf-vectors.json
crates/wbf-sdk/src/
  cipher.rs chunk_block.rs chunk_crypto.rs   密碼層（約定 §2–§4、§7）；tests/client_vectors.rs 產生並比對 wbf-client-vectors.json
  protocol.rs channel.rs client.rs upload.rs download.rs manifest.rs login.rs error.rs   通道與上傳／下載（線上規格）
  chat.rs                聊天模型與 ChatBackend trait，沒有 Matrix 型別
  vault.rs               local.key、六把子金鑰、session.sealed、封 recovery key（local-cache-db §4）；沒有 SQLite、沒有 matrix-sdk
  account_dir.rs         資料目錄名的確定性加密（`<b58 nonce>_<b58 密文>`，local-cache-db §11）；沒有 IO
  room_keys.rs           本地金鑰快照放哪、用什麼 passphrase、權限（local-cache-db §10.4）；不碰 matrix-sdk
  cache.rs               cache.db（feature `cache`，SQLCipher；local-cache-db §6）：users／rooms／events／events_synced_log／room_list／sync_state／read_positions／media／event_media
  media_pool.rs          媒體儲存池的落地格式（64 KiB 段各自 AEAD、暫定段、續傳、BLAKE3 檔名）；沒有 SQL、沒有網路
  media.rs               fetch／collect_garbage／sweep：下載管線、池、cache.db 三者唯一的交會點（feature `cache`）
  event_json.rs          原始 Matrix 事件 JSON → Message；matrix backend 與 recent 共用，不掛 feature
  backend/matrix_sdk.rs  `use matrix_sdk` 的地方之一（feature `matrix`，預設關）；store 吃 vault 的第二把子金鑰
  crypto_engine.rs       另一個碰上游的地方（feature `matrix`）：`OlmEngine` —— 同一個 sqlite crypto store（`m/`）上的 `OlmMachine` 只當狀態機用。
                         `send_outgoing_requests`（KeysUpload／Query／Claim／Signatures／發 to-device 全走橋）、`refresh_room_devices`（一支例行程序：
                         成員清單 → diff → 只重查變的人 → 雜湊對一次、不對再查、還不對就拒發 → `share_room_key`）、`encrypt_and_send(&RoomRefresh, …)`（🚨 只收 RoomRefresh：
                         上游沒 outbound session 是 panic）、`decrypt_room_event`、`import_window`／`pull_to_device`（匯入 → 落地 → 銷毀鎖死）。
                         分享策略 `room_key_share_settings()` 明確選 AllDevices，交叉簽章做好後換 IdentityBased 只改那裡
  device_version.rs      裝置版本號（`序號-雜湊`）與房間版本號：成員清單怎麼讀（沒號碼是錯不是 0）、`diff_from`（誰要重查、誰離開）、
                         `compute_device_keys_hash`（照 server §3.4 重算，黃金向量 810b7c3be4；🚨 user_id 用 get 逐層查、不拼 JSON Pointer）。沒網路
  to_device_state.rs     to-device 的 `cd_seq` 與待銷毀清單 → `m/td.json`（原子寫；壞檔是錯不是從頭；Ack 不清清單，只清 ItemsDestroyed 回來的）
  protocol.rs            還有：橋（`BridgedEndpoint`、`bridge_request`／`expect_bridge_reply`，號碼只抄用得到的）、`0x16 Device` 的原生 pack（Fetch／Batch／Subscribe／
                         ItemsDestroy／ItemsDestroyed／CryptoState）
  channel.rs             `PackChannel`（request／request_stream／`subscribe`，後者預設回 Usage）、`Channel` enum、`HttpChannel`；`WsChannel` 是 `WsLink` 的薄殼（`connect_with_hook` 帶鉤子）
  transport.rs           bytes 進出：`FrameSource`／`FrameSink`，tungstenite 一組、`memory_pair` 一組（測試餵亂序用）。不知道什麽是 pack
  sessions.rs            **會話表**（ws-receive-dispatch.md §2–§4）：`SessionKey::{Session(id), Reply{id,seq}}` → `PackSink`；四條分派規則（活會話擁有它的 id → 精確 (id,seq) → Create 例外 → 無主計數）；
                         三種 sink（單發／串流 256 滿了失敗／訂閱 64 滿了丟標 gap）；`ReceivedHook` 每個 pack 都經過（含無主），這層只呼叫不判斷。純資料結構，單元測試在同檔
  link.rs                `WsLink`：讀取 task（收→decode→鎖內 classify→鎖外叫鉤子→鎖內 dispatch；連續 8 個壞 frame 就關）＋送出 task（有界佇列 16，單一 task 寫 sink 保序）＋表；
                         `request`／`request_with_policy(AckPolicy)`（預設不重送）／`open_stream`／`subscribe`；關線只有一條路 `shut_down`（讀取 task 結束、送出 task 寫失敗、`close()` 三個入口都走它：
                         closed → fail_all → 兩個 task 都 abort）；逾時分 `Timeout`（連線活著）與 `Network`（連線沒了）；handle 的 Drop 只拿自己那一代（世代號）。🚫 不重連
                         **心跳**（`Heartbeat`，維護者 2026-09-21 照 WireGuard）：每條線自己一個 task，24 秒一次、最近 20 秒有送或收就跳過、Ping 沒 Pong 10 秒就 shut_down；
                         請求號從 u32::MAX 往下；`Heartbeat::OFF`／`start_with_heartbeat` 給測試
crates/wbf-core/src/     **命令的本體全在這裡**（#24）。公開面只有可序列化的 DTO 與 `CoreError`
  lib.rs                 `Core`（解鎖一次的 vault、多帳號入口）、`Target`（user／server／server_backup，＝RPC 的 params 形狀）
  error.rs               `CoreError { kind, message }`、`CoreErrorKind`、`rpc_code()`（rpc-spec §5.2 的號碼）
  conf.rs                wbf.conf 的解析與自動生成（CLI 規格 §10）；從 apps/wbf-cli 搬進來，daemon 與 CLI 共用一份
  event.rs               `CoreEvent`（`Note`／`Progress` 帶 `job`，`Message`／`SyncState` 帶 `user`）與 broadcast channel。🚫 core 不印任何東西
                         2026-09-21 多兩個：`Link`（線開關，帶 `LinkRole`／`LinkState`）、`Received`（線收到 pack，只有標頭）
  link_pool.rs           **連線池**（link-pool.md）：`LinkRole` 五條線、`LinkPool`（要用才開、死了下次重開、`close_all`）、`PooledClient`（一條線一次一個命令）、
                         `Core::client_of(…, role)` 是唯一閘門、`open_link`（session → Bearer 升級 → hello）、`close_links`（登出叫）、`received_hook`（pack → `CoreEvent::Received`）、
                         `open_link_count`（`daemon.info` 的 `links`）。單元測試用記憶體對接的假 opener
  job.rs                 「現在跑的是哪個請求」：tokio task-local，讓事件說得出屬於誰。⚠️ 不跟著 `tokio::spawn`（有測試釘住）
  server_cache.rs        `cache.db` 的**單一寫入者**：一個 server dir 一條 OS 執行緒＋無上限 queue；`post`（commit 之後才發事件）／
                         `run`（等它落地）＋一條重用的讀連線。⚠️ 媒體那幾條是刻意的例外（daemon-runtime §2.3.1）。
                         🚨 刪 cache.db 之前要 `Core::close_server_cache`（等 queue 寫完、執行緒結束），不然 Windows 刪不掉、Linux 寫進已刪的檔
  backend_choice.rs      `transport` → backend：`ws`＝wbf-sdk、`http`＝matrix-sdk。探測 `get_backend_kind`（key 是**帳號**、
                         只記 server 回答過的）、規則 `get_backend_for`、閘門 `client_of`、`MethodHome` 暫時清單
  handles.rs             Session／Cache／Backend／池的取得，全 `pub(crate)`：🚫 `access_token` 一個欄位都不離開這個 crate。
                         `server_cache_of` 的註冊表鎖握滿「查、開、放」整段（#32：放掉會 `database is locked`）
  accounts.rs recovery.rs  資料目錄佈局（`DataDirMap`）、`r/` 的 recovery key；都是 crate 內部
  *_ops.rs               命令本體：login／account／session（logout/destroy）／rooms／upload／media／backup／sync／misc
                         ⚠️ 公開介面不能假設同程序（architecture-v2 §7）：`&self`、可序列化的型別、事件走 channel、
                         🚫 不問終端、🚫 沒有生命週期／trait object／`impl Trait`。**加新方法一樣要過這條**
crates/wbf-daemon/src/   **RPC 那一面**（rpc-spec）。控制平面的基底與全部有 core 對應的 method：
  pack.rs                ver‖type‖data 的編解碼與 XChaCha20-Poly1305（token 導兩把鑰）；純函數
  message.rs             Request／Response、請求層 code（1xx）、協議層 CloseReason（9xxx）
  protocol.rs            hello 的兩關：client 名字前綴、protocol 交集
  connection.rs          一條連線的狀態機；⚠️ **出去的包該不該加密只在這裡判**（EncryptionPolicy 是全局）
  handle/                method → core。mod.rs 是分派與共同欄位（Target／transport）；local／accounts／rooms／media／backup 一模組一族。
                         ⚠️ dispatch 每個分支 Box::pin（E0275）；fresh 資料目錄的起手式是 vault.create，🚫 account.add 不偷建 vault
  lock.rs                資料目錄的獨佔：寫排他／讀共享（std 的 File::try_lock）＋ `WriteAccess`
                         全局能力（起手 false，要寫才拿；`call()` 是唯一檢查點）
  settings.rs            從 wbf.conf 讀 SERVER_BACKUP／LOCAL_ROOM_KEYS／TRANSPORT（解析在 wbf_core::conf，跟 CLI 共用）
  server.rs              loopback WS listener；一連線一 Connection 一 writer task；請求各自 spawn
                         2026-09-21 多：每連線一個推播 task（`forward_pushes`：core 的 broadcast → 訂閱過濾 → `seal_push`；`Lagged` → `desync`）、`subscribe`／`unsubscribe` 在這層回（不進 handle）、
                         請求包在 `job::run_as_job(id)` 裡（`progress`／`note` 對得回發它的連線）
  push.rs                `Subscriptions`（每連線一份：名字集合＋選填 `user`）、`push_of`（`CoreEvent` → 推播名與 params，照 rpc-spec §4）、`desync`、`parse_subscription_params`
  token.rs               token 檔的三遍覆蓋抹除（隨機 → 0xFF → 0x00 → 刪）與權限檢查；⚠️ daemon 預設不動 token，誰起的誰動
  main.rs                `-s` 常駐（先拿寫權、讀 token 與 conf、寫 daemon.json）。沒有 `-s` ＝單發，⚠️ **還沒實作**（會報錯講清楚）；
                         兩個都帶也報錯。資料平面還沒有
  tests/loopback.rs      真的起 listener、用 tokio-tungstenite 原生 client 走 hello／token 錯／text frame／shutdown
  tests/process.rs       真的把 daemon binary 跑起來：ready 的兩個管道、殘留的 daemon.json 被蓋掉、
                         token 檔 daemon 不動、shutdown 之後程序結束並收走 daemon.json
  tests/real_server.rs   `--ignored`：對真 wbfuwunel 走 vault.create（passphrase）→account.add→whoami→ping→
                         room.list→sync.recent→backup.status→**停掉 daemon 再起**→unlock→whoami→account.del
apps/wbf-cli/src/        瘦的前端：main.rs（參數、`CoreErrorKind` → exit code）、unlock.rs（passphrase 來源：檔案或終端，🚫 沒有 ticket 了）、
                         commands.rs／rooms.rs／recent.rs（叫 core、印 JSON）；conf 的解析已搬到 wbf_core::conf
                         ⚠️ 目錄名還叫 `wbf-cli`：改成 rpc-cli 留到它真的變成 RPC 前端那支 PR
scripts/acceptance.sh    CLI 規格 §8 的驗收，對本機 wbfuwunel 跑
vendor/matrix-rust-sdk   上游 submodule，path dependency；只在 backend/matrix_sdk.rs 出現
```

## 4. 怎麼跑

```bash
# Windows：先 export PATH="/c/Strawberry/perl/bin:$PATH"，不然 openssl-sys（SQLCipher 用）編不起來（local-cache-db §3）
cargo test --workspace                       # 不含 matrix feature，快；含 cache feature 的測試要 --features cache
cargo test -p wbf-sdk --features matrix      # 事件轉換、aggregate、錯密碼分類（起迷你 403 server）
cargo clippy -p wbf-sdk -p wbf-core -p wbf-cli --features wbf-sdk/matrix --all-targets -- -D warnings
cargo fmt -p wbf-wire -p wbf-sdk -p wbf-core -p wbf-cli  # 🚫 不要 --all：會格式化 submodule
```

對真 server（本機 wbfuwunel，Windows）：

1. 把 server 的 exe **複製**到別處再跑（維護者會重編 `target/`，直接跑會鎖檔）。最新 code 在 `target/e2e/`，`target/release/` 可能是舊的。
2. config 最小集：`server_name = "localhost"`、`port = 6167`、`allow_registration = true`、`registration_token = "<自訂>"`、`database_path`、`log = "warn"`。啟動要十幾秒，輪詢 `/_matrix/client/versions` 到 200。
3. 註冊測試帳號：`POST /_matrix/client/v3/register` 帶 `auth.type = m.login.registration_token`。
4b. **E2EE 引擎的驗收**（#48／#49，要 feature `matrix`、兩個帳號、一定 `--test-threads=1`）：
    `WBF_E2E_SERVER=... WBF_E2E_USER=alice WBF_E2E_PASSWORD_FILE=... WBF_E2E_USER_B=bob WBF_E2E_PASSWORD_B_FILE=... cargo test -p wbf-sdk --features matrix --test e2e_crypto_engine -- --ignored --test-threads=1`。
    三條：同帳號兩台裝置房間金鑰只靠 WS 從 A 到 B；#45 驗收（Bob 登新裝置 → 1506 → refresh → 重送 → 兩台解得開）；
    第 4 階段的（B 長活訂閱 → A 分房間金鑰 → `CryptoState` 與 `Push` 都進 B 的 handle → 再 `Fetch` 一窗、沒有任何 pack 無主）。並行跑會互相分到對方的房間金鑰。
    不用 server 的那半在 `tests/dispatch.rs`（記憶體對接餵亂序）與 `sessions.rs` 的單元測試。
    橋的那條在 `e2e_local_server`（`bridge_members_and_send_to_device_against_real_server`）。
4. `WBF_E2E_SERVER=... WBF_E2E_USER=... WBF_E2E_PASSWORD_FILE=... cargo test -p wbf-sdk --test e2e_local_server -- --ignored`（第 2 步的驗收）；`WBF_PASSWORD_FILE=... scripts/acceptance.sh`（CLI 的驗收，200 MiB 約 80 秒；`WBF_ACCEPT_SIZE_MIB=16` 快跑）。
5. 第 3 步的手動流程：`login` → 用 token `createRoom`（`initial_state` 帶 `m.room.encryption`）→ `rooms` → `send --text` → `read` → `send --file` → `files --save` → `download --manifest`。token 現在在 `session.sealed` 裡讀不到，`createRoom` 那步的 token 用 curl 另外登入一次拿（驗收腳本就是這樣做）。
8. 快取的手動流程（一個帳號）：`recent`（第一次 `cg_seq_before` 是 null）→ `read <room> --from-cache` → `recent` 再跑一次（`pulled` 應該是 0）。
10. 媒體池：`upload --sha256 --manifest m.json` → `download --manifest m.json -o a`（`source: server`，池裡出現 `media/<hh>/<hash>`）→ 再 `download` 一次（`source: cache`）→ `media-stats` → 大檔用 `--transport http` 下到一半殺掉 → `media-stats` 看到 `incomplete_files: 1` → 再 `download` 的第一行進度從上次快照的塊數開始、sha 對 → `media-gc --quota-mib 1 --protect-days 0` 清光。2026-09-08 跑過一次全對。
9b. 金鑰備份與閘門的手動流程（2026-09-09 跑過）：`login alice` → `key-backup status`（`server_backup_exists` 應為 true，證明 `auto_enable_backups` 生效）→ `logout`（**應該被閘門擋，exit 1**）→ `key-backup recovery`（產生並封進 `r/`）→ `recovery list` → `logout`（這次過）→ 確認 `r/` **還在** → 重 `login` → `key-backup restore` → `key-backup save`／`import` → `account destroy`（`recovery list` 應該空掉）。
   ⚠️ 閘門的 principal 迴歸：`account switch @alice`（有 recovery key）後 `account del @bob:localhost`（沒有）**必須被擋** —— 擋不住就是又用了 current 的狀態（PR #19 審查 rumia／salvia 🔴1）。
   ⚠️ data dir 用短路徑（例如 `C:/Users/<you>/AppData/Local/Temp/wt`）：加密過的目錄名很長，scratchpad 那種深路徑在 Windows 會撞 MAX_PATH（餘裕算法見 local-cache-db §11.4.1）。
9. 多帳號混存（兩個帳號 alice、bob，同一個 `--data-dir`）。（PR #19 起是 `account` 一族：`account status`／`switch`／`del`／`destroy`；下面的舊命令名要照著換）：alice `login` → 建只有 alice 的房 A 與邀 bob 的房 C，各送幾則 → `recent` → bob `login`（alice 不 logout）→ `accounts` 兩個、current 是 bob → `rooms` 只有 C → `read A --from-cache` 0 則、`read C --from-cache` 0 則（bob 還沒親自拿過）→ `recent` → `read C --from-cache` 有了，而且 alice 解過的那幾則是明文 → `--account alice read A --from-cache` 仍有 → alice `logout`（`cache.db` 還在）→ bob `logout`（`cache.db` 被刪）。`forget-account @alice:localhost --yes` 後 alice 的 `--from-cache` 全空、bob 的不受影響。2026-09-07 跑過一次全對。
7. vault 的手動流程：`login --passphrase-file pw`（建 `passphrase` 模式的 `local.key`）→ `rooms --passphrase-file pw`（過）→ `rooms`（**問 passphrase**；非互動就 exit 1）→ `remove-passphrase --passphrase-file pw` → `rooms`（不問）。⚠️ 2026-09-13 起**每個命令都要 passphrase**：`unlock.ticket` 與 `lock` 都沒了（CLI 規格 §7.1）。
6. 測完 `taskkill //F //IM <複本名>.exe`。

## 5. 坑（都踩過）

- **wbfuwunel 的 `id` 第一個 byte 是型別**（wire-format §2.2，2026-09-12 BREAKING）：`Event/Recent`
  這種 client 自己鑄會話號的包要用 `wbf_wire::pack::id::compose(id::SESSION, n)`，填裸的 `n` server 回
  `InvalidRequest: this kind takes a conversation the client named in its id, and this one carries none`。
  server 鑄的（上傳 id、`g_seq`）回來已經組好，原樣抄回去就對。
  來源是 wbfuwunel PR #47（2026-09-13 合併，`e36b136cc`）；本專案 issue #29 第 1 項是同一件事。
  📎 向量檔已經照合併後的 main 重抄（`recent_*`／`batch_*`／`subscribe_rooms`／`error_unsupported` 九個包的 id
  從裸值變成 `0x01` 開頭的組合值，`ack_draft_open` 的 meta 錨改成 `g_seq` 4711）。
  ⚠️ codec 不驗語意 —— 裸的 `10` 跟組好的 `0x01…0a` 對它一樣好，所以那段期間向量整片綠。
  現在有 `every_vector_id_carries_a_type_byte_we_know` 釘住「非零的 id 一定帶得出型別」，重抄到沒組型別的向量會當場紅。
- **`cargo test --workspace` 綠不代表 SDK 對得上 server**：黃金向量是整份複製的，server 加了 kind（`0x02 Stream`、
  `0x16 Device`）我們的 `Kind` 表沒有，`vectors.rs` 才會紅；漏抄向量就什麼都不會紅。每次 server 那邊改 wire 就重抄一次。
- 🚨 **我們只在 Windows 上跑測試，而有些檢查在 Windows 上是 no-op**：`token::is_private`
  在非 Unix 一律回 `true`（靠目錄 ACL）。所以「測試自己用 `std::fs::write` 寫 token 檔」
  在這裡全綠，到 Unix 上卻會被 daemon fail closed 擋掉、每個 process 測試都死在啟動
  （PR #31 審查 cirno🔴 抓到）。⭐ 寫測試用的私密檔一律走 `wbf_sdk::vault::write_private`，
  🚫 不要 `std::fs::write` 之後再 chmod。📎 同一類的還有檔案鎖與權限位元 —— 平台差異的地方，
  **綠燈只代表這個平台綠**。
- **Windows 上剛關掉的 SQLite store 還會被握著幾百毫秒**：登出刪 `m/` 會撞 `os error 32`
  （`AccountDir::delete_matrix_store` 因此重試 10 × 100 ms）。⚠️ 它是**間歇的** —— 2026-09-13 對真
  server 跑 daemon e2e 第一次紅、第二次就過。📎 重試完仍失敗就回錯，🚫 不吞：那時多半是別的程序
  開著同一個 store。
- **core 的長工作 future 要 `Send`**：daemon 把每個請求 `tokio::spawn`，wbf-sdk 的回呼型別一律是
  `&mut (dyn FnMut(…) + Send)`。新加回呼型別漏了 `+ Send`，錯會在 daemon 的 `dispatch` 那一行爆，不在 sdk。
  📎 同一行還會撞 E0275（matrix-sdk 的 future 太深、推 `Send` 爆遞迴上限）：`dispatch` 每個分支 `Box::pin` 就是為了這個。

- 🚨 **server 的 WS `Event/Send` 把 txn_id 去重鍵在帳號不分裝置**（wbfuwunel #78，`wbf/send.rs` 傳 `sender_device: None`）：同帳號另一台裝置、甚至另一個房重用 txn_id，
  會拿到上次那則的 event_id（HTTP `GET /event` 還會回 200）。e2e 因此曾誤判「帶 room_version 的加密訊息進不了 Recent」——追了一個小時。測試的 txn_id 一律帶每輪唯一後綴。
- **`ItemsDestroy` 只有持有這台裝置佇列的連線能做**（server 回 `Forbidden`）：順序是 `Subscribe` → `Fetch` → 匯入 → `ItemsDestroy`，🚫 不能只 Fetch 不 Subscribe。
  訂了之後 server 隨時推東西進來（別人 claim 你一把 OTK 就推 CryptoState），而通道現在只有「送一個等一個」——這就是第 4 階段要解的事。
- **已追蹤的人只靠 `update_tracked_users` 不會再查**：要「這個人變了、重查」用 `OlmEngine::mark_users_changed`（走 `device_lists.changed` 同一個入口）。
- **上游 `encrypt` 在房間沒有 outbound session 時是 panic 不是回錯**（`expect("Session wasn't created nor shared")`）：所以 `encrypt_and_send` 只收 `RoomRefresh`，型別上逼你先 refresh。
- **`ItemsDestroyed` 只抄 `id`、`seq` 是 0**（`Ack` 才抄命令的 seq）；向量裡命令的 seq 剛好也是 0，靠向量看不出來。
- ruma 組請求對要 token 的端點一定要給 token：引擎給占位字串、只取 body，真的 `Authorization` 由橋在 server 那端填。
- **D 槽會滿**：連結器 `1201`／`1180`／`1318` 先 `df -h /d`；`cargo clean -p wbf-sdk -p wbf-core -p wbf-daemon -p wbf-cli -p wbf-wire`（2026-09-21 清出 25.7G），🚫 不清 deps。
- `cargo fmt --all` 會格式化 `vendor/matrix-rust-sdk`（path dependency）。用 `-p`。commit 前看 `git -C vendor/matrix-rust-sdk status` 是空的。
- 帶 `--features matrix` 的第一次編譯很久（matrix-sdk 全家）；放背景。
- Windows 的 autocrlf 會把向量 JSON 換成 CRLF，`client_vectors.rs` 比對前有 normalize；`*.sh` 靠 `.gitattributes` 保持 LF。
- `main` 有分支保護，文件也走 PR。feature 分支推 `origin` 之後也推鏡像 `wbftw`；PR 合併後把 `origin/main` 同步到 `wbftw/main`（維護者 2026-09-15 定）。
- matrix-sdk 的錯誤不能 parse Display 字串抓 errcode（永遠抓不到），用 `client_api_error_kind()`。有回歸測試。
- wbfuwunel 對 `Create` 的回應標頭 `id` 是新上傳 id，不是線上規格說的抄回 0；SDK 兩種都收，只對 `Create` 放寬。
- `watch` 對齊「現在」要一次 sync，debug build 啟動超過一秒；測時序要留餘裕。
- Windows 主執行緒棧只有 1 MB，debug build 的 `send` 會爆棧；`main.rs` 把 runtime 跑在 64 MiB 棧的執行緒上。新增大的 async 路徑如果又爆，先懷疑這個。
- `logout` 一定要連 `m/`（matrix-sdk 的 crypto store）一起刪：Matrix logout 讓裝置失效，留著的 crypto store 會擋下一次 `login`（"account in the store doesn't match"）。`cache.db` 反過來要留（維護者定），只有這個 server 最後一個帳號登出才刪。
- 快取讀寫都要帶「我是誰」（mxid）：`Context::cache()` 回 `(Cache, me)`；漏帶就變成別人的視角。SDK 端沒有預設值可以偷懶。
- 編 SQLCipher（`cache` feature，wbf-cli 預設帶）在 Windows 要 Strawberry Perl 在 PATH 前面，不然 openssl-sys 的 build script 掛在 `Configure`。

## 6. 已知的洞（不是忘了，是等別人）

| 洞 | 卡在哪 | 影響 |
|---|---|---|
| **E2EE 房送檔案沒宣告附件**（約定 §5.2） | server 的 `Event/Send` 是提案；matrix-sdk 的 `Room::send` 不能加 header | server 端媒體計數 0，過保護期（≥ 7 天）被清。CLI 送檔會印警告 |
| ~~`RoomCrypto` trait 還沒有~~ | ✅ 引擎是 `crypto_engine::OlmEngine`（#48／#49），沒抽 trait（只有一個實作，抽了是儀式） | CLI 送訊息還走 matrix-sdk；引擎還沒接進 daemon |
| ~~推播被通道丟在地上~~ | ✅ 第 4 階段 SDK 那半做了（`ws-receive-dispatch.md`）：會話表依 id 交付、`device_subscription` 長活收、`Superseded` 進訂閱當終點、鉤子給 RPC 面 | daemon 那半（推播封裝、`desync`、推來就叫 refresh）還沒接（§7 第 2 項） |
| **E2EE 沒有產品路徑** | §7 第 2 項 | 沒有任何一條路在 Hello 宣告 `org.wbftw.device_versions`；引擎只有 e2e 在用 |
| 交叉簽章沒 bootstrap | §7 第 3 項 | 分享策略只能 `AllDevices`；server 建議的 `IdentityBased` 現在等於發給零台 |
| PR #43（走橋 GetEvent 當歷史錨點）擱置 | 等 wbfuwunel #64（Recent 收 `before_event_id`）合併後重做 | 跳到訊息還是兩個來回 |
| ~~房間金鑰沒有任何備份~~ | ✅ PR #19 做了：server 端標準 backup、本地全量快照、`logout` 的兩關閘門、`r/` 獨立保管 | — |
| `Session/*`（WS 上的 Login／Refresh／Logout）只加了 wire 常數 | client 登入仍走 HTTP `/login` 加 matrix-sdk | 沒影響；要把登入搬到 WS 時再做 |
| 斷線後 `recent` 不自動續 | 命令 exit、下次從水位重來；server 不記狀態、寫入冪等 | 多拉一輪；UI 那版做自動從最後的 `ls` 續 |
| **沒有假的 wbf server 可以在 core 層測「成功」路徑** | 還沒做 | 探測成功、`watch`、`log_in` 的探測接點都只有 `--ignored` 的真 server 測試走得到。#32／#33 的審查每一輪都碰到這個缺口 |

## 7. 下一步（維護者 2026-09-06 同意的順序，2026-09-10 更新）

做完的（都合併了）：資料目錄兩層路徑加密＋`account` 一族＋房間金鑰備份（#19，設計在 #18）、架構 v2 的形狀（#20）、vault 與金鑰（#11）、`cache.db` 多帳號混存（#13；rusqlite 0.40 與 matrix-sdk 合得來，代價是 Windows 要 Strawberry Perl，local-cache-db §3）、媒體儲存池（#14）、`recent` 改成拉窗＋`Event/Batch` 串流與三層分工 `RecentPlan { max_events, window, batch }`（#16，issue #15）。

還沒做的：

📍 **2026-09-21 的順序**（維護者定；下面 09-14 與 0–6 是更早的清單，保留當歷史）：

1. ✅ **第 4 階段：收包依種類分派**（維護者 2026-09-21 定的形狀；SDK 那半 2026-09-21 做完，設計在 `design/ws-receive-dispatch.md`；daemon 那半接在鉤子後面、歸第 2 項）。「送一個等一個」沒錯，錯的是「下一個收到的就是我的回覆」：
   - `WsChannel` 多一個讀取 task：收、解碼、**依 kind／subtype 分派**。回覆類（Ack／Error／橋的回覆／Batch／ItemsDestroyed）依會話 id 表交給在等的請求；
     推播類（Device／Push、CryptoState、Event／Push、DeviceChanged、Superseded）依訂閱 id 交給那個訂閱的處理器。順序亂掉不會出事。
   - 會話 id 表：無序類（id 0）以 seq 對；有序或指定 id 的（Recent、Fetch、ItemsDestroy、Subscribe）以 id 登記一個會話項，seq 只在會話內驗遞增；
     訂閱是長活的會話項，到 Unsubscribe 或 Superseded 才收。
   - 現在 `receive_pack` 那段「推播型且 id 不符就丟」與 `dropped_pushes` **整段拿掉**；`PackChannel` 的 request／request_stream 介面可以不動，底下換成從會話表取。
   - 測試要把「傳輸」跟「分派」拆開，用記憶體 duplex 餵亂序的 pack；現有的同步假 server 不夠用。
   - 要小心：WS 關掉時所有在等的會話都要收到錯、每個會話自己的逾時、訂閱句柄的緩衝要有界（滿了當一次 gap）、Superseded 是 Control/Error 帶訂閱 id 要分到訂閱不是請求。
   - 做完之後 e2ee-walkthrough §16.5 第 8、10 列、to-device-client §8 第 4、5 列才能勾。✅ 勾了（通道那半）。
   - 維護者加的一條：**每個收到的 pack 都經過一個鉤子**（`ReceivedHook`），之後 daemon 的 RPC 面在鉤子裡決定要不要送 UI；link 只呼叫不判斷。
     還有：送與收分開（兩個 task）；會話項是 trait（`PackSink`）好接特規 spec；重送靠 id 表（`AckPolicy`，預設關、冪等的呼叫點自己開）。
     四條線（architecture-v2 §6.1.1）不進這層：一條 `WsLink` 一張表，daemon 開四條就是四個實例。
1b. ✅ **連線生命週期**（維護者 2026-09-21 定：五條線各司其職、連線池按種類挑線、預設不起訂閱、斷了下次要用再開；`design/link-pool.md`）：
   core 的 `link_pool.rs`（`LinkPool`／`LinkRole`／`client_of(…, role)`／`open_link`／`close_links`）、`CoreEvent::Link`／`Received`；daemon 的 `push.rs`（訂閱集合、事件→推播）與 `server.rs` 的推播 task。
   兩條訂閱線（rooms、keys）這支只保證開得起來、關得掉、有人收；內容在第 2、3 項。
2. **E2EE 接進 daemon**（e2ee-walkthrough §16.6 的 RPC 面；走 `LinkRole::Keys` 那條線，`features_of(Keys)` 宣告 `org.wbftw.device_versions`）：`refresh_room_devices` RPC（UI 點進房間叫）、send RPC 走 `encrypt_and_send`、
   被 1506 擋時 daemon 自動 refresh 再把錯原樣回 UI 並發「這個房版本到 V、可以送了」的狀態訊息（daemon 🚫 不自動重送）、每房 `RoomRefresh` 的落地（記憶體還是 cache.db 待定）、
   上線 `device_subscribe` → `pull_to_device`、下線 `device_unsubscribe`。這是第一條宣告 `org.wbftw.device_versions` 的產品路徑。
3. 交叉簽章 bootstrap（`SigningKeysUpload` 已在橋上）→ 分享策略換 `IdentityBasedStrategy`（只改 `room_key_share_settings`）。
4. 假的 wbf server 測試工具（老缺口；假 server 已經會橋、Device、Send，差的是在 core 層驅動）。
5. PR #43 等 wbfuwunel #64 合併後重做；`Batch.more` 那項（server 未合分支）。

📍 **2026-09-14 的建議順序**：

1. ✅ **房間歷史走 wbf**（2026-09-14）：`Event/Recent{rooms}`、翻頁一律 `event_id`、matrix 走 `/context`、
   一般 Matrix 房本地不答。順帶修掉「最後一個帳號登出時 cache.db 還被註冊表握著」（真 server 測試抓到的）。
2. **假的 wbf server 測試工具** —— 這支靠 `--ignored` 的真 server 測試才驗到兩條上游路線，缺口還在。
3. **「沒洞就不問上游」**：記 server 保證過的範圍（按帳號、用 `g_seq`），🚫 不從本地 `r_seq` 連不連號推。等 server 推送再確認。
4. **階段 4 訂閱／推播** —— 先跟維護者討論。

0. ✅ **房間金鑰備份與周邊**（維護者 2026-09-09 提；2026-09-10 全部做完）：設計定案在 local-cache-db §10 與 CLI 規格 §3.1／§3.6／§10。
   ✅ **PR #19 合併了大半**：兩層路徑加密、`account` 一族、CLI 輸出英文、金鑰備份（server 端 backup、本地全量快照、`key-backup` 六個子命令（status／upload／save／import／restore／recovery）、`logout` 的兩關閘門、`r/` 獨立保管與 `recovery list`／`show`）。
   ✅ **2026-09-10 收掉了剩下兩塊**：`wbf.conf`（CLI 規格 §10：解析、旗標 > 環境 > conf > 預設、自動生成、`SERVER_BACKUP`／`LOCAL_ROOM_KEYS` 兩個開關）、passphrase 吃原始 bytes（local-cache-db §12）。**這一項到此關掉。**
   ⚠️ PR #19 是 **breaking 的**：舊的 data dir（明文目錄名）一律**砍掉重來，🚫 不寫遷移**。本機測試環境要重新 `login`。
   ✅ **2026-09-09 對真 server 跑過全流**（步驟見 §4 的 9b），含 principal 修正的迴歸（current=alice 刪 bob 時看的是 bob 的狀態）。
   ✅ **Windows MAX_PATH 已解**：nonce 縮到 12 byte、目錄名縮成 `s`／`a`／`m`／`k`／`r`，最長路徑 230 → 184、餘裕 20 → 76；路徑太長時的錯誤訊息也不再誤導成「it was made with another key file」。前後對照與教訓在 local-cache-db §11.4.1。
1. chat-model §6 剩的房間功能：`room`、建房、邀請、改權限、置頂、已讀送出、裝置驗證、標準附件下載。穿插。
2. 附件宣告：等 server 定案（`media-attachments.md` 仍是提案）。期間寫設計：用 `matrix-sdk-crypto` 的 `OlmMachine` 自己 Megolm 加密、走 `Event/Send` pack（這也是 `RoomCrypto` trait 出現的地方）。✅ **2026-09-10 定了**：走 `OlmMachine::encrypt_room_event_raw`。⚠️ `Room` 上沒有 `encrypt`，所以那從來不是二選一；fork 已建、`base_client()` 改 `pub` 那一行也推上去了（architecture-v2 §8.3，#23）。
3. ⚠️ **架構 v2 的落地**（`design/architecture-v2.md`，2026-09-09 定形狀、2026-09-10 定名字）。
   ✅ **`wbf-core` 做完了**（#24）：命令本體全搬進去，公開面只剩可序列化的 DTO 與 `CoreError`。
   ✅ **兩個「未定的決策」也不擋了**：fork 已建並成為 submodule（#23）；server 的 to-device
   已由 wbfuwunel PR #43 實作（`0x16 Device`，權威在那邊的 `wbf-to-device.md`）。
   還沒做的三塊，**建議順序**：
   1. ✅ **`rpc-spec.md`**（2026-09-12 第一版）：method 表、code 表（`CoreErrorKind` 配號在 §5.2）、推播、資料平面。
      §8 列了 daemon PR 要順手補進 core 的四樣（建檔與送事件拆開、`PoolReader` 接 Range、
      結構化事件、配號）。📎 本來還有第五樣 `Core::lock()`，2026-09-13 連 `vault.lock` 一起取消了
      （rpc-spec §3.1：daemon 沒有「鎖上」這個 feature，真正的 lock 是 `daemon.shutdown`）。
   2. 🔁 **`crates/wbf-daemon`**：第一塊（#30）pack、訊息、hello、連線狀態機、WS listener、`-s`；
      第二塊（#31）全部有 core 對應的 method 與 conf 搬進 daemon，外加寫權能力、token 五步、`vault.create`。
      **執行期（daemon-runtime §11）**：階段 1–3 ✅（#32）、backend 接縫 ✅（#33）。
      還沒：**階段 4 訂閱／推播**（⚠️ 維護者要先討論；wbfuwunel #51 也改了 `Subscribe` 補窗的語意）、
      5 `cancel`、6 sdk 的 `0x04/05/06` codec、7 上游會話、8 監督者、9 已讀三層；
      資料平面 HTTP（media.open／create、send_attachment）、單發命令列。
      ⏳ 懸著等維護者：`media.db` 拆檔（維護者說「等要做的時候再討論，這點我有一些想說清楚」）。
      📎 起手式是 `vault.create`（fresh 資料目錄）：🚫 `account.add` 不替前端建 vault（rpc-spec §3.1）。
      原定義：core ＋ RPC 服務 ＋ 資料平面 ＋ **自己的命令列**（`daemon <命令>` 單發＝測試性質、常駐中再叫獨佔命令跳錯、
      `daemon -s` 常駐；arg 先轉成 RPC 訊息再進 handle，architecture-v2 §0.2）。
   3. **`apps/wbf-cli` → rpc-cli**：參數解析搬進 daemon，殼縮成「封裝 RPC 訊息丟本地 WS」的測試工具。
      改名跟著「真的走 RPC」那支走，🚫 不單獨開一支改名 PR。
   📎 附件訊息的完整流程（建檔拿 URL → 發訊息 ＆ PUT bytes 並行 → 進度）與閘門鏈在 architecture-v2 §4.9。
   ⚠️ 還有一個 client 端的欠債：**接 `0x16 Device`**（server 那邊 2026-09-12 全做完了，
   我們一個字都沒寫）。五件事、為什麼要排在 daemon 之後，在 `design/to-device-client.md`。
4. UI 框架比較文件。UI 的同步流程已經有 SDK 介面可接：開一個 task 跑 `recent_sync`，callback 把每個 Batch 丟 channel 給寫 DB 的 task（chat-model §4.3）；媒體用 `media::fetch` 加 `PoolReader`。
5. 串流／seek 對著媒體池讀（local-cache-db §8.6）：`seek` 命令現在仍直接打 server。
6. ⚠️ UI 落地前要確認「進房逐房翻頁」真的存在：`recent` 被 `max_events` 停下時，`[last_ls, 舊水位)` 那段是永久洞，只有逐房 `/messages` 會補（PR #16 審查記錄）。

## 8. 規矩（維護者定，全域 CLAUDE.md 也有）

- 一律開分支送 PR，merge commit，不 rebase、不 squash、不 amend、不 force push。
- 每個 PR 描述要列「新增了對上游的哪些依賴」（plan-v1 §7.2）。
- 會 breaking Matrix 兼容的設計先寫給維護者，不自己選。
- 本地不落地任何聊天內容（plan-v1 §7.1），直到 local-cache-db 那一版。
- 審查者 cirno／rumia／salvia 每個 PR 都會來；逐條回應，能改就改，不改講理由。

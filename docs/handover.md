# 交接：現在在哪、怎麼跑、下一步

> 給下一個接手的人（人或 agent）。每次交接更新（最近一次 2026-10-04）。設計理由不在這裡，在 `/docs/design/`（索引 `/docs/design/index.md`）；
> 這裡只講**現況、怎麼跑、坑、下一步**。每一支 PR 做了什麼看 git 歷史與 Forgejo 上的 PR，這裡不重述。

## 1. 現況

PR #1–#69 合併（main `199a88a`，2026-10-04）；#43（走橋的 GetEvent）是 2026-09-16 關掉、沒合——「跳到訊息」等 wbfuwunel #64 合了再開一支。已經能用的，照層次：

- **線上協議與媒體**：`wbf-wire` 的 codec 對著 server 的黃金向量；`wbf-sdk` 的分塊上傳／下載／seek／續傳／串流、每塊 AEAD（`/docs/design/media/wbf-client-convention-for-chunk.md`）。
- **本地資料**：vault 與子金鑰、資料目錄兩層路徑加密（`/docs/design/storage/vault-and-keys.md`）；`cache.db` 一個 server 一份、多帳號混存、單一寫入者（`/docs/design/storage/local-cache-db.md`、`/docs/design/daemon/daemon-runtime.md` §2）；
  媒體池（`/docs/design/media/media-pool.md`）；房間金鑰的本地備份（`/docs/design/keys/room-key-backup.md`，只對一般 Matrix 帳號）。
- **daemon**：加密的 RPC、資料目錄獨佔、token 生命週期（`/docs/design/rpc-specs/local-interface.md`）；方法與推播照 `/docs/design/rpc-specs/rpc-spec.md`；
  `sync=local|server|both`、`transport` 選 backend（`/docs/design/daemon/daemon-runtime.md` §3）。
- **帳號**：探活不帶 token；wbf 帳號不建 matrix-sdk 的 Client，登入登出走標準 HTTP、`m/` 只有 crypto store（`/docs/design/daemon/account-session.md`）。
- **連線**：一個帳號五條線（Misc、Upload、Download、Rooms、Keys），解鎖／登入後 daemon 全開、每 15 秒看一次、死了重開、各自心跳（`/docs/design/daemon/link-pool.md`）；
  很多命令同時用一條線、各等自己的回條、號由線發、逾時看整條線（`/docs/design/daemon/link-requests.md`）；
  收包依會話表交付、順序亂掉不出事（`/docs/design/daemon/ws-receive-dispatch.md`）。
- **房間**：wbf 帳號的房間列表只問加入了哪些（`JoinedRooms`）、每一間的樣子是 UI 對看得到的房間叫 `room.get`（`/docs/design/rooms/chat-model.md` §2.1）、送訊息與送檔走 `Event/Send` 並宣告附件；訂閱線收推播寫快取、🚫 不碰水位，水位只由 UI 叫的 `sync.recent` 推（`/docs/design/rooms/room-sync.md`）。
  訊息的 edit／redact 照 `/docs/design/messages/edits-and-redactions.md` 存。
- **E2EE（wbf 帳號）**：金鑰線追平與匯入、佇列頭就是水位（`/docs/design/keys/key-sync.md`）；狀態放 UI、金鑰由 daemon 自動、1506 之後 daemon 補完再回 1401（`/docs/design/keys/e2ee-rpc.md`）。
  加密房的**文字**收發對真 server 驗過（bob 登新裝置、舊版本號被擋、重送後新舊裝置都解得開）。
- **資料平面**（`/docs/design/rpc-specs/data-plane.md`）：上傳是 UI 發動的兩步（`media.create` → `PUT /upload` 拿 manifest → `room.send_attachment`）；
  讀是 `media.open` → `GET /media`（Range 就是 seek）。下載是每帳號一個處理端、所有檔一起跑（每檔一塊在途）、`Download` 線送收分開（`/docs/design/daemon/link-requests.md`）、池格式 v2、seek 暫存檔，
  進度是推播 `media.download`（`/docs/design/media/media-download.md`）。兩邊都對真 server 驗過。

**還沒有**：UI；路徑版送檔進加密房（資料平面那條可以）；一般 Matrix 帳號的傳統上傳與下載；wbf 帳號的金鑰備份與向自己裝置要金鑰（新裝置讀不到舊訊息）；房間自設的換金鑰期限；交叉簽章；
已讀（`/docs/design/messages/read-receipts.md` 是草案）；RPC 的 `cancel`（下載有自己的 `media.cancel`）；下載的暫停；daemon 的單發命令列；監督者的 task panic 收攤與重探 backend。

⏳ **等維護者**：補解寫失敗那批要不要加重試的觸發點、CLI 要不要能送加密房（`/docs/design/keys/e2ee-rpc.md` §8；§7 第 1 項先照預設做）。

## 2. 讀哪些文件、什麼順序

設計文件依類別分資料夾，全部列在 [`/docs/design/index.md`](design/index.md)。第一次接手照這個順序讀：

1. [`/README.md`](../README.md)：佈局、狀態表、怎麼跑測試、貢獻規則。
2. `/docs/design/overview/architecture-v2.md`：daemon／RPC／前端的分層，§8 耦合方向（上游 SDK 是可拆的零件）是所有程式的前提。
3. `/docs/design/rpc-specs/rpc-spec.md`：前端 ↔ daemon 的每一條 method、錯誤碼、推播。
4. `/docs/design/daemon/daemon-runtime.md` 與同資料夾的 `/docs/design/daemon/link-pool.md`、`/docs/design/daemon/account-session.md`：daemon 跑起來之後誰擁有什麼。
5. 要碰哪一塊再讀那個資料夾：房間 `rooms/`、訊息 `messages/`、E2EE `keys/`（`/docs/design/keys/e2ee-rpc.md` 最權威）、本地資料 `storage/`、媒體 `media/`。

server 端的權威在 wbfuwunel repo：`/docs/design/chunked-upload-spec.md`（分塊上傳的線上格式）、`room-seq-and-recent.md`、`media-attachments.md`、`wbf-vectors.json`（整份複製到本 repo 的 `/docs/design/wire/`，不手改）。

## 3. 程式碼在哪

```
crates/wbf-wire/         pack、EncryptedFileInfo、CRC-32C；純函數。tests/vectors.rs 對 /docs/design/wire/wbf-vectors.json
crates/wbf-sdk/src/
  cipher.rs chunk_block.rs chunk_crypto.rs   密碼層（/docs/design/media/wbf-client-convention-for-chunk.md §2–§4、§7）；tests/client_vectors.rs 產生並比對 wbf-client-vectors.json
  protocol.rs channel.rs client.rs upload.rs download.rs manifest.rs login.rs error.rs   通道與上傳／下載（線上規格）
  chat.rs                聊天模型與 ChatBackend trait，沒有 Matrix 型別
  vault.rs               local.key、六把子金鑰、session.sealed、封 recovery key（/docs/design/storage/vault-and-keys.md §1）；沒有 SQLite、沒有 matrix-sdk
  account_dir.rs         資料目錄名的確定性加密（`<b58 nonce>_<b58 密文>`，/docs/design/storage/vault-and-keys.md §2）；沒有 IO
  room_keys.rs           本地金鑰快照放哪、用什麼 passphrase、權限（/docs/design/keys/room-key-backup.md §4）；不碰 matrix-sdk
  cache.rs               cache.db（feature `cache`，SQLCipher；/docs/design/storage/local-cache-db.md §5）：users／rooms／events／events_synced_log／room_list／sync_state／read_positions／media／event_media
  media_pool.rs          媒體儲存池的落地格式（池格式 v2：64 KiB 段都寫滿、長度在密文裡、續傳從檔案本身、owner 認暫存名、BLAKE3 檔名；/docs/design/media/media-download.md §4.1）；沒有 SQL、沒有網路
  seek_store.rs          seek 暫存檔（固定大小的格、O(1) 位置表、重開重建；/docs/design/media/media-download.md §4.2）
  media.rs               `MediaDownload`（一個檔在主檔、暫存檔與網路之間怎麼拿塊）／collect_garbage／sweep：下載管線、池、cache.db 三者唯一的交會點（feature `cache`）
  event_json.rs          原始 Matrix 事件 JSON → Message；matrix backend 與 recent 共用，不掛 feature
  backend/matrix_sdk.rs  `use matrix_sdk` 的地方之一（feature `matrix`，預設關）；store 吃 vault 的第二把子金鑰
  crypto_engine.rs       另一個碰上游的地方（feature `matrix`）：`OlmEngine` —— 同一個 sqlite crypto store（`m/`）上的 `OlmMachine` 只當狀態機用。
                         `send_outgoing_requests`（KeysUpload／Query／Claim／Signatures／發 to-device 全走橋）、`refresh_room_devices`（一支例行程序：
                         成員清單 → diff → 只重查變的人 → 雜湊對一次、不對再查、還不對就拒發 → `share_room_key`）、`encrypt_and_send(&RoomRefresh, …)`（🚨 只收 RoomRefresh：
                         上游沒 outbound session 是 panic）、`decrypt_room_event`、`import_items`／`pull_to_device`（匯入 → 落地 → 銷毀鎖死；推來的一包與 Fetch 的一窗同一支）。
                         分享策略 `room_key_share_settings()` 明確選 AllDevices，交叉簽章做好後換 IdentityBased 只改那裡
  device_version.rs      裝置版本號（`序號-雜湊`）與房間版本號：成員清單怎麼讀（沒號碼是錯不是 0）、`diff_from`（誰要重查、誰離開）、
                         `compute_device_keys_hash`（照 server §3.4 重算，黃金向量 810b7c3be4；🚨 user_id 用 get 逐層查、不拼 JSON Pointer）。沒網路
  to_device_state.rs     to-device 的 `cd_seq` 與待銷毀清單 → `m/td.json`（原子寫；壞檔是錯不是從頭；Ack 不清清單，只清 ItemsDestroyed 回來的）
  protocol.rs            還有：橋（`BridgedEndpoint`、`bridge_request`／`expect_bridge_reply`，號碼只抄用得到的）、`0x16 Device` 的原生 pack（Fetch／Batch／Subscribe／
                         ItemsDestroy／ItemsDestroyed／CryptoState）
  channel.rs             `PackChannel`（request／request_stream／`subscribe`，後者預設回 Usage）、`Channel` enum、`HttpChannel`；`WsChannel` 是 `WsLink` 的薄殼（`connect_with_hook` 帶鉤子）
  transport.rs           bytes 進出：`FrameSource`／`FrameSink`，tungstenite 一組、`memory_pair` 一組（測試餵亂序用）。不知道什麽是 pack
  sessions.rs            **會話表**（/docs/design/daemon/ws-receive-dispatch.md §2–§4）：`SessionKey::{Session(id), Reply{id,seq}}` → `PackSink`；四條分派規則（活會話擁有它的 id → 精確 (id,seq) → Create 例外 → 無主計數）；
                         三種 sink（單發／串流 256 滿了失敗／訂閱 64 滿了丟標 gap）；`ReceivedHook` 每個 pack 都經過（含無主），這層只呼叫不判斷。純資料結構，單元測試在同檔
  link.rs                `WsLink`：讀取 task（收→decode→鎖內 classify→鎖外叫鉤子→鎖內 dispatch；連續 8 個壞 frame 就關）＋送出 task（有界佇列 16，單一 task 寫 sink 保序）＋表；
                         `request`／`request_with_policy(AckPolicy)`（預設不重送）／`request_until_silent`（整條線沉默才逾時，`Pong` 不算）／`open_stream`／`subscribe`；
                         `next_seq`／`next_session_number`：**號由線發**（2³¹ 起往上，同一條線上的 client、`RequestLine`、心跳共用，/docs/design/daemon/link-requests.md §7）；關線只有一條路 `shut_down`（讀取 task 結束、送出 task 寫失敗、`close()` 三個入口都走它：
                         closed → fail_all → 兩個 task 都 abort）；逾時分 `Timeout`（連線活著）與 `Network`（連線沒了）；handle 的 Drop 只拿自己那一代（世代號）。🚫 不重連
                         **心跳**（`Heartbeat`，維護者 2026-09-21 照 WireGuard）：每條線自己一個 task，每 24 秒一定送一個 Ping（10-02 起🚫 因為最近有通訊就跳過：server 的 idle 是 60 秒（wbfuwunel #103，2026-10-02）、只看 client 送的）、送出 Ping 之後 10 秒什麼都沒收到才 shut_down（Pong 排在別的回覆後面不算死）；
                         請求號也是線發的；`Heartbeat::OFF`／`start_with_heartbeat` 給測試
crates/wbf-core/src/     **命令的本體全在這裡**（#24）。公開面只有可序列化的 DTO 與 `CoreError`
  lib.rs                 `Core`（解鎖一次的 vault、多帳號入口）、`Target`（user／server／server_backup，＝RPC 的 params 形狀）
  error.rs               `CoreError { kind, message, data }`（`data` 選填：例 1401 帶 daemon 自動重拿的房間狀態，/docs/design/rpc-specs/rpc-spec.md §5.3）、`CoreErrorKind`、`rpc_code()`（/docs/design/rpc-specs/rpc-spec.md §5.2 的號碼）
  conf.rs                wbf.conf 的解析與自動生成（/docs/design/rpc-specs/wbf-cli-spec.md §10）；從 apps/wbf-cli 搬進來，daemon 與 CLI 共用一份
  event.rs               `CoreEvent`（`Note`／`Progress` 帶 `job`，`Message`／`SyncState` 帶 `user`）與 broadcast channel。🚫 core 不印任何東西
                         2026-09-21 多兩個：`Link`（線開關，帶 `LinkRole`／`LinkState`）、`Received`（線收到 pack，只有標頭）
  room_sync.rs           **房間那條線的內容**（/docs/design/rooms/room-sync.md）：`init_connection`（池開線的通用初始化：`Rooms` 就訂、起收推播的 task；`Keys` 交給 key_sync）；不碰水位（只有 `sync.recent` 動它）；一帳號一 task，登出收
  room_crypto.rs         **房間的加解密**（/docs/design/keys/e2ee-rpc.md）：`refresh_room_devices`、加密送出與 1506 之後自動重拿（`wbf_send_encrypted`）、收到時解（`to_incoming`）、補解（`decrypt_stored`）；`RoomDevices`／`SendOptions` 是 DTO
  link_keeper.rs         **「該開的線都開著嗎」的鉤子**（/docs/design/daemon/link-pool.md §3.1）：`Core::ensure_links`，daemon 解鎖／登入後與背景迴圈每一輪叫
  wbf_rooms.rs           **wbf 帳號的房間**（/docs/design/daemon/account-session.md §6）：列表只問橋的 `JoinedRooms`、單一房間 `GetState` 與 `m.direct` 一起送、`Event/Send` 送文字（加不加密只看本地 `rooms.encrypted`、不知道就報錯；明文房明文、加密房交給 room_crypto.rs 加密；加密房的檔案拒）。`is_wbf_account` 在 handles.rs
  link_pool.rs           **連線池**（/docs/design/daemon/link-pool.md）：`LinkRole` 五條線（misc／upload／download／rooms／keys）、`logging_out_guard`（登出封池，丟掉就解封）、`LinkPool`（要用才開、死了下次重開、`close_all`）、`PooledClient`（同一條線上的一個 client；一格是讀寫鎖，很多命令同時用、開／關獨佔）、
                         `Core::client_of(…, role)` 是唯一閘門、`open_link`（session → Bearer 升級 → hello）、`close_links`（登出叫）、`received_hook`（pack → `CoreEvent::Received`）、
                         `open_link_count`（`daemon.info` 的 `links`）。單元測試用記憶體對接的假 opener
  job.rs                 「現在跑的是哪個請求」：tokio task-local，讓事件說得出屬於誰。⚠️ 不跟著 `tokio::spawn`（有測試釘住）
  server_cache.rs        `cache.db` 的**單一寫入者**：一個 server dir 一條 OS 執行緒＋無上限 queue；`post`（commit 之後才發事件）／
                         `run`（等它落地）＋一條重用的讀連線。媒體也走它（/docs/design/daemon/daemon-runtime.md §2.3.1）；繞過它的 `cache_of` 只在測試建置。
                         🚨 刪 cache.db 之前要 `Core::close_server_cache`（等 queue 寫完、執行緒結束），不然 Windows 刪不掉、Linux 寫進已刪的檔
  backend_choice.rs      `transport` → backend：`ws`＝wbf-sdk、`http`＝matrix-sdk。探測 `get_backend_kind`（key 是**帳號**、
                         只記 server 回答過的）、規則 `get_backend_for`、閘門 `client_of`、`MethodHome` 暫時清單
  handles.rs             Session／Cache／Backend／池的取得，全 `pub(crate)`：🚫 `access_token` 一個欄位都不離開這個 crate。
                         `server_cache_of` 的註冊表鎖握滿「查、開、放」整段（#32：放掉會 `database is locked`）
  accounts.rs recovery.rs  資料目錄佈局（`DataDirMap`）、`r/` 的 recovery key；都是 crate 內部
  download_queue.rs      **每帳號的下載處理端**（/docs/design/media/media-download.md §5、§6）：收件 queue（job 處理一次就消耗）、`downloading` 表（取消旗標＋進度）、
                         在途表（同一個塊請求只送一次）、所有檔一起跑、每檔一塊在途、seek 的請求插最前面、
                         同 server 同 mxc 只有一個寫入者（`MediaClaims`）、推播節流。登出時 `close_links` 一併收
  link_requests.rs       一條線的發送 queue ＋ 發送端（/docs/design/daemon/link-requests.md）：送出🚫 等回覆、回覆連同動作交回擁有者；現在只有 `Download` 用
  media_stream.rs        `GET /media` 的來源：本機原檔 → 完整池檔 →（下載處理端）主檔已封的段 → 暫存檔 → 現拉；URL 不帶帳號，照已登入的帳號找
  *_ops.rs               命令本體：login／account／session（logout/destroy）／rooms／upload／attachment／media（排隊、另存、stats、gc）／backup／sync／misc
                         ⚠️ 公開介面不能假設同程序（/docs/design/overview/architecture-v2.md §6）：`&self`、可序列化的型別、事件走 channel、
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
  push.rs                `Subscriptions`（每連線一份：名字集合＋選填 `user`）、`push_of`（`CoreEvent` → 推播名與 params，照 /docs/design/rpc-specs/rpc-spec.md §4）、`desync`、`parse_subscription_params`
  token.rs               token 檔的三遍覆蓋抹除（隨機 → 0xFF → 0x00 → 刪）與權限檢查；⚠️ daemon 預設不動 token，誰起的誰動
  main.rs                `-s` 常駐（先拿寫權、讀 token 與 conf、寫 daemon.json）。沒有 `-s` ＝單發，⚠️ **還沒實作**（會報錯講清楚）；
                         兩個都帶也報錯。控制平面與資料平面一起開、一起停
  data_plane.rs          資料平面：URL 與 meta（共享 token 加密；URL 只帶「用途 ‖ mxc」、上傳狀態在 `Wbf-Upload-Meta` header）與 hyper 的 HTTP listener（`PUT /upload/mxc/…`、`GET`／`HEAD /media/mxc/…` 的 Range 與串流 body、Host 檢查）
  tests/loopback.rs      真的起 listener、用 tokio-tungstenite 原生 client 走 hello／token 錯／text frame／shutdown
  tests/process.rs       真的把 daemon binary 跑起來：ready 的兩個管道、殘留的 daemon.json 被蓋掉、
                         token 檔 daemon 不動、shutdown 之後程序結束並收走 daemon.json
  tests/real_server.rs   `--ignored`：對真 wbfuwunel 走 vault.create（passphrase）→account.add→whoami→ping→
                         room.list→sync.recent→backup.status→**停掉 daemon 再起**→unlock→whoami→account.del
apps/wbf-cli/src/        瘦的前端：main.rs（參數、`CoreErrorKind` → exit code）、unlock.rs（passphrase 來源：檔案或終端，🚫 沒有 ticket 了）、
                         commands.rs／rooms.rs／recent.rs（叫 core、印 JSON）；conf 的解析已搬到 wbf_core::conf
                         ⚠️ 目錄名還叫 `wbf-cli`：改成 rpc-cli 留到它真的變成 RPC 前端那支 PR
scripts/acceptance.sh    /docs/design/rpc-specs/wbf-cli-spec.md §8 的驗收，對本機 wbfuwunel 跑
vendor/matrix-rust-sdk   上游 submodule，path dependency；只在 backend/matrix_sdk.rs 出現
```

## 4. 怎麼跑

```bash
# Windows：先 export PATH="/c/Strawberry/perl/bin:$PATH"，不然 openssl-sys（SQLCipher 用）編不起來（/docs/design/storage/local-cache-db.md §3）
cargo test --workspace                       # 不含 matrix feature，快；含 cache feature 的測試要 --features cache
cargo test -p wbf-sdk --features matrix      # 事件轉換、aggregate、錯密碼分類（起迷你 403 server）
cargo clippy --workspace --all-targets --features wbf-sdk/matrix -- -D warnings   # 含 §8 那套「不准 panic」的 lint（crate 根的 deny）
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
4c. **E2EE 的 RPC 面整條**（#62，/docs/design/keys/e2ee-rpc.md §9）：alice 建加密房、邀 bob、bob 加入，然後
    `WBF_E2E_SERVER=... WBF_E2E_USER=@alice:localhost WBF_E2E_PASSWORD_FILE=... WBF_E2E_USER_B=@bob:localhost WBF_E2E_PASSWORD_B_FILE=... WBF_E2E_ENCRYPTED_ROOM='!…' cargo test --workspace --lib -- --ignored --test-threads=1 an_encrypted_conversation`
    （alice 送、bob 解；bob 登第二台 → 1506 → daemon 回新狀態 → 重送、新舊兩台都解得開，約 3 秒）。daemon 的 `real_server` 帶 `WBF_E2E_ENCRYPTED_ROOM` 會多驗 `room.refresh_devices`＋帶 `room_devices` 送。
4. `WBF_E2E_SERVER=... WBF_E2E_USER=... WBF_E2E_PASSWORD_FILE=... cargo test -p wbf-sdk --test e2e_local_server -- --ignored`（第 2 步的驗收）；`WBF_PASSWORD_FILE=... scripts/acceptance.sh`（CLI 的驗收，200 MiB 約 80 秒；`WBF_ACCEPT_SIZE_MIB=16` 快跑）。
5. 第 3 步的手動流程：`login` → 用 token `createRoom`（`initial_state` 帶 `m.room.encryption`）→ `rooms` → `send --text` → `read` → `send --file` → `files --save` → `download --manifest`。token 現在在 `session.sealed` 裡讀不到，`createRoom` 那步的 token 用 curl 另外登入一次拿（驗收腳本就是這樣做）。
8. 快取的手動流程（一個帳號）：`recent`（第一次 `cg_seq_before` 是 null）→ `read <room> --from-cache` → `recent` 再跑一次（`pulled` 應該是 0）。
10. 媒體池：`upload --sha256 --manifest m.json` → `download --manifest m.json -o a`（`source: server`，池裡出現 `media/<hh>/<hash>`）→ 再 `download` 一次（`source: cache`）→ `media-stats` → 大檔用 `--transport http` 下到一半殺掉 → `media-stats` 看到 `incomplete_files: 1` → 再 `download` 的第一行進度從上次快照的塊數開始、sha 對 → `media-gc --quota-mib 1 --protect-days 0` 清光。2026-09-08 跑過一次全對。
9b. 金鑰備份與閘門的手動流程（2026-09-09 跑過）：`login alice` → `key-backup status`（`server_backup_exists` 應為 true，證明 `auto_enable_backups` 生效）→ `logout`（**應該被閘門擋，exit 1**）→ `key-backup recovery`（產生並封進 `r/`）→ `recovery list` → `logout`（這次過）→ 確認 `r/` **還在** → 重 `login` → `key-backup restore` → `key-backup save`／`import` → `account destroy`（`recovery list` 應該空掉）。
   ⚠️ 閘門的 principal 迴歸：`account switch @alice`（有 recovery key）後 `account del @bob:localhost`（沒有）**必須被擋** —— 擋不住就是又用了 current 的狀態（PR #19 審查 rumia／salvia 🔴1）。
   ⚠️ data dir 用短路徑（例如 `C:/Users/<you>/AppData/Local/Temp/wt`）：加密過的目錄名很長，scratchpad 那種深路徑在 Windows 會撞 MAX_PATH（餘裕算法見 /docs/design/storage/vault-and-keys.md §2.4.1）。
9. 多帳號混存（兩個帳號 alice、bob，同一個 `--data-dir`）。（PR #19 起是 `account` 一族：`account status`／`switch`／`del`／`destroy`；下面的舊命令名要照著換）：alice `login` → 建只有 alice 的房 A 與邀 bob 的房 C，各送幾則 → `recent` → bob `login`（alice 不 logout）→ `accounts` 兩個、current 是 bob → `rooms` 只有 C → `read A --from-cache` 0 則、`read C --from-cache` 0 則（bob 還沒親自拿過）→ `recent` → `read C --from-cache` 有了，而且 alice 解過的那幾則是明文 → `--account alice read A --from-cache` 仍有 → alice `logout`（`cache.db` 還在）→ bob `logout`（`cache.db` 被刪）。`forget-account @alice:localhost --yes` 後 alice 的 `--from-cache` 全空、bob 的不受影響。2026-09-07 跑過一次全對。
7. vault 的手動流程：`login --passphrase-file pw`（建 `passphrase` 模式的 `local.key`）→ `rooms --passphrase-file pw`（過）→ `rooms`（**問 passphrase**；非互動就 exit 1）→ `remove-passphrase --passphrase-file pw` → `rooms`（不問）。⚠️ 2026-09-13 起**每個命令都要 passphrase**：`unlock.ticket` 與 `lock` 都沒了（/docs/design/rpc-specs/wbf-cli-spec.md §7.1）。
6. 測完 `taskkill //F //IM <複本名>.exe`。

## 5. 坑（都踩過）

- **wbfuwunel 的 `id` 第一個 byte 是型別**（wbf-wire-format.md §2.2，2026-09-12 BREAKING）：`Event/Recent`
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

- ✅ ~~server 的 WS `Event/Send` 把 txn_id 去重鍵在帳號不分裝置~~（wbfuwunel #78，2026-09-26 在 PR #88 修了：現在按 `(user, device, txn)` 算，跟 HTTP 一樣）。
  當時同帳號另一台裝置重用 txn_id 會拿到上次那則的 event_id，e2e 因此誤判過一次。client 的 `new_txn_id` 照舊是 128 位元隨機——修了之後也無害，不改。
- **`ItemsDestroy` 只有持有這台裝置佇列的連線能做**（server 回 `Forbidden`）：順序是 `Subscribe` → `Fetch` → 匯入 → `ItemsDestroy`，🚫 不能只 Fetch 不 Subscribe。
  訂了之後 server 隨時推東西進來（別人 claim 你一把 OTK 就推 CryptoState），而通道現在只有「送一個等一個」——這就是第 4 階段要解的事。
- **已追蹤的人只靠 `update_tracked_users` 不會再查**：要「這個人變了、重查」用 `OlmEngine::mark_users_changed`（走 `device_lists.changed` 同一個入口）。
- **上游 `encrypt` 在房間沒有 outbound session、或 session 過期時是 panic 不是回錯**（`expect("Session wasn't created nor shared")`、`assert!(!session.expired())`）：
  #62 起 sdk 的 `encrypt_and_send` 自己先 `share_room_key`（沒有就建、過期就換），加密那一步再用 `catch_unwind` 接住跨過期限那一瞬間（實測接得住，回 `Protocol`）。🚫 不要繞過它直接叫上游的 `encrypt_room_event_raw`。
- **`ItemsDestroyed` 只抄 `id`、`seq` 是 0**（`Ack` 才抄命令的 seq）；向量裡命令的 seq 剛好也是 0，靠向量看不出來。
- ruma 組請求對要 token 的端點一定要給 token：引擎給占位字串、只取 body，真的 `Authorization` 由橋在 server 那端填。
- **D 槽會滿**：連結器 `1201`／`1180`／`1318`、`os error 112`（磁碟空間不足）、`invalid metadata` 先 `df -h /d`。🚫 不整個 `cargo clean`（重編 matrix-sdk 一輪十幾分鐘）。
  先清「現在的建置用不到的舊產物」：等這一輪該編的都編完，對 `cargo test --workspace --no-run`、`cargo clippy --workspace --all-targets --features wbf-sdk/matrix`、
  `cargo test -p wbf-sdk --features matrix --no-run` 各跑一次 `--message-format=json`（都是新鮮度檢查、不重編），收 `compiler-artifact` 的檔名，`target/debug/deps` 裡不在清單上的就是舊的
  （2026-09-29 這樣清出 2.47G，全是舊版 matrix-sdk 的變體）。還不夠才 `cargo clean -p wbf-sdk -p wbf-core -p wbf-daemon -p wbf-cli -p wbf-wire`（2026-09-21 清出 25.7G），🚫 不清 deps。
- `cargo fmt --all` 會格式化 `vendor/matrix-rust-sdk`（path dependency）。用 `-p`。commit 前看 `git -C vendor/matrix-rust-sdk status` 是空的。
- 帶 `--features matrix` 的第一次編譯很久（matrix-sdk 全家）；放背景。
- Windows 的 autocrlf 會把向量 JSON 換成 CRLF，`client_vectors.rs` 比對前有 normalize；`*.sh` 靠 `.gitattributes` 保持 LF。
- `main` 有分支保護，文件也走 PR。feature 分支推 `origin` 之後也推鏡像 `wbftw`；PR 合併後把 `origin/main` 同步到 `wbftw/main`（維護者 2026-09-15 定）。
- matrix-sdk 的錯誤不能 parse Display 字串抓 errcode（永遠抓不到），用 `client_api_error_kind()`。有回歸測試。
- wbfuwunel 對 `Create` 的回應標頭 `id` 是新上傳 id，不是線上規格說的抄回 0；SDK 兩種都收，只對 `Create` 放寬。
- `watch` 對齊「現在」要一次 sync，debug build 啟動超過一秒；測時序要留餘裕。
- **同一條線再 `hello` 會蓋掉 feature 宣告**（server 把宣告記在連線上、下一個 Hello 覆蓋，沒帶就收回）：只有開線的人 hello，池發出去的 client 帶它的結果（`WbfClient::with_hello`）；🚫 在池裡的線上重 hello（2026-10-05 拿掉 `ping`／`room.history`／`sync.recent` 三處）。
- **一個命令🚫 握著一個 `PooledClient` 再要同一條線的第二個**：一格是讀寫鎖，中間有人等寫鎖（登出、重開）就互等。要同時送就各拿各的（/docs/design/daemon/link-pool.md §5）。
- **測試裡開訂閱線要照正式路徑的順序**：`init_connection` 放在 `pool.acquire` 的開線閉包裡跑（`test_support::subscribed_keys`）。先 init 再放進池的話，task 一起來就 `reuse` 會看到空格，第一個 `CryptoState` 的補上傳落空（#62 的測試抓到過）。
- **daemon 的真 server 測試要等線開好**：`account.add`／`vault.unlock` 之後五條線在背景開（/docs/design/daemon/link-pool.md §3.1）；要驗推播，先 `wait_for_links(5)` 再送——訂閱不補訂閱之前的訊息。
- **`target/e2e/tuwunel.exe` 不一定是 server main 編的**：先在 wbfuwunel 看 `git branch --show-current`。2026-09-30 那顆是未合併的 `docs/room-version-prev`（房間版本號改成成員集合的雜湊、不遞增）——client 只比房間版本號相不相等，測試也只能這樣斷言。
- **`cargo check` 會另外產一整套產物**（跟 test／clippy 不共用），D 槽緊的時候別用；型別檢查用 `cargo clippy --workspace --all-targets --features wbf-sdk/matrix`（反正要跑）。repo 的 `.cargo/config.toml` 已把 `build.jobs` 限 2。
- Windows 主執行緒棧只有 1 MB，debug build 的 `send` 會爆棧；`main.rs` 把 runtime 跑在 64 MiB 棧的執行緒上。新增大的 async 路徑如果又爆，先懷疑這個。
- `logout` 一定要連 `m/`（matrix-sdk 的 crypto store）一起刪：Matrix logout 讓裝置失效，留著的 crypto store 會擋下一次 `login`（"account in the store doesn't match"）。`cache.db` 反過來要留（維護者定），只有這個 server 最後一個帳號登出才刪。
- 快取讀寫都要帶「我是誰」（mxid）：`Context::cache()` 回 `(Cache, me)`；漏帶就變成別人的視角。SDK 端沒有預設值可以偷懶。
- 編 SQLCipher（`cache` feature，wbf-cli 預設帶）在 Windows 要 Strawberry Perl 在 PATH 前面，不然 openssl-sys 的 build script 掛在 `Configure`。

## 6. 已知的洞（不是忘了，是等別人）

| 洞 | 卡在哪 | 影響 |
|---|---|---|
| **E2EE 還缺的**（`/docs/design/keys/e2ee-rpc.md` §8） | 排在 §7 第 1 項 | 加密房送檔（路徑版）仍拒，資料平面那條可以；新裝置讀不到舊訊息（wbf 帳號的金鑰備份、向自己裝置要金鑰都沒接）；房間自設的換金鑰期限沒讀（一律一週／100 則）；補解寫失敗那批不自動重試；CLI 給不了 `room_devices`（加密房送不了） |
| 交叉簽章沒 bootstrap | client 還沒接；server 的橋都有了（wbfuwunel 的 /docs/bridge-specs/0x17-keys.md `0x24`／`0x25`，驗證訊息走 to-device） | 分享策略只能 `AllDevices`；server 建議的 `IdentityBasedStrategy` 現在等於發給零台 |
| 一般 Matrix 帳號送檔案沒宣告附件（`/docs/design/media/wbf-client-convention-for-chunk.md` §5.2） | matrix-sdk 的 `Room::send` 不能加 header | server 端媒體計數 0，過保護期（≥ 7 天）被清，CLI 送檔會印警告。wbf 帳號走 `Event/Send`，沒有這個洞 |
| 跳到訊息的錨點（PR #43 走橋 GetEvent，2026-09-16 關掉、沒合） | 等 wbfuwunel #64（`Recent` 收 `before_event_id`），合了再開一支 | 跳到訊息還是兩個來回 |
| server 批 3、批 4 的新 kind 沒接（0x12／0x15／0x1C／0x1D、0x18 Push／0x19 Media／0x1A Search／0x1B Voip，本 repo issue #55） | 用到才加；沒有破壞性改動、向量檔沒變 | 推播規則、搜尋、目錄、TURN 都還沒有。做的時候先讀 #55 列的十個坑 |
| 斷線後 `recent` 不自動續 | 命令 exit，下次從水位重來；server 不記狀態、寫入冪等 | 多拉一輪 |
| core 層測「成功路徑」的假 wbf server 還不全 | core 的 `test_support` 會答訂閱、Device、橋的 Members／GetStateEvent／Keys*／SendToDevice、`Event/Send` | 探測成功、`watch`、`log_in` 的探測接點只有 `--ignored` 的真 server 測試走得到 |

📎 `Session/*`（WS 上的 Login／Refresh／Logout）只有 wire 常數：不是洞，是決定——登入登出維持標準 HTTP（`/docs/design/daemon/account-session.md` §0）。

## 7. 下一步（維護者 2026-09-30 定的切法：少而大的 PR）

1. **送訊息不綁發金鑰**（維護者 2026-10-05）：加密房送訊息只拿本機現有的 session 加密、送 `Event/Send`；沒有或到期就本機當場建新的。
   散播金鑰是後台自己的事（房間變加密、session 到期、成員或裝置變了、送失敗），送訊息🚫 等、🚫 管送到沒有。
   先改 `/docs/design/keys/e2ee-rpc.md` 給維護者看再寫。半路知道的事實：規格沒有「金鑰先到」；`matrix-sdk-crypto` 加密只要本機有 session、沒過期（沒有會 panic）。
   （其他四條線送收分開 2026-10-05 做完：`/docs/design/daemon/link-requests.md` §2.1。）
2. **官方 Matrix 的傳統上傳與下載、E2EE 收尾**：`/_matrix/media`（`/docs/design/rpc-specs/data-plane.md` §7；下載那半接在同一組 `media.*` 上）；讀 `m.room.encryption` 的換金鑰期限、補解寫失敗的重試觸發點（`room.history`／`sync.recent` 讀到未解的就再試）、CLI 能送加密房（CLI 自己就是前端：同一個命令裡先 refresh 再送）。
3. **訊息功能**：已讀三層（`/docs/design/messages/read-receipts.md`）；`/docs/design/rooms/chat-model.md` §6 剩的房間功能（建房、邀請、改權限、置頂、裝置驗證）。
4. **daemon 穩健性**：task panic 收攤、重連時重探 backend、`cancel`、進度節流（`/docs/design/daemon/daemon-runtime.md` §10）；
   `apps/wbf-cli` 不再越過 daemon 寫資料目錄（維護者 2026-09-30：前端只能發 RPC，`/docs/design/overview/architecture-v2.md` §0.2）——過渡的「先拿 `daemon.lock`、拿不到就拒絕」已做，剩改走 RPC。

之後（還沒排）：wbf 帳號的金鑰備份與交叉簽章、裝置驗證（純 client：server 的橋都有了，wbfuwunel 的 /docs/bridge-specs/0x17-keys.md `0x24`–`0x25`、`0x30`–`0x3D`，secret storage 走 /docs/bridge-specs/0x11-account.md 的 account data）；「跳到訊息」等 wbfuwunel #64（#43 已關）；server 批 3／4 的功能（#55）；
下載的暫停（只停主檔、🚫 不停 seek，`/docs/design/media/media-download.md` §5.4）、daemon 的單發命令列、`apps/wbf-cli` 改成走 RPC（那時 `download --no-cache` 的直寫路一併收掉）；
UI 框架比較。
⚠️ UI 落地前要確認「進房逐房翻頁」真的存在：`recent` 被 `max_events` 停下時，`[last_ls, 舊水位)` 那段是永久洞，只有逐房 `/messages` 會補。
⏳ 懸著等維護者：`media.db` 拆檔（維護者：「等要做的時候再討論」）。

## 8. 規矩（維護者定，全域 CLAUDE.md 也有）

- 一律開分支送 PR，merge commit，不 rebase、不 squash、不 amend、不 force push。
- 每個 PR 描述要列「新增了對上游的哪些依賴」（/docs/design/overview/architecture-v2.md §8）。
- 會 breaking Matrix 兼容的設計先寫給維護者，不自己選。
- 🔒 **資料目錄綁定 daemon**（維護者 2026-09-30）：前端（UI、rpc-cli）只能發 RPC 請 daemon 改，🚫 不越過 daemon 直接寫；不拿 `daemon.lock` 就寫是非法侵占。
- 📑 **引用文件寫從 repo 根目錄算的完整路徑加章節**（維護者 2026-09-30）：`/docs/design/keys/e2ee-rpc.md §3`；只有指同一份文件自己的章節才只寫 `§3`。搬家或改名時 grep 完整路徑就找得到每一處。
- 📜 **設計文件只講現在的約定與理由**（維護者 2026-09-30）：被推翻的版本、進度、「第 N 步」「PR #x 合併了」不寫進文件，歷史在 git；改規則的那支 PR 同時掃掉舊說法。
- 審查者 cirno／rumia／salvia 每個 PR 都會來；逐條回應，能改就改，不改講理由。
- 🧭 **狀態放 UI、動作能自動就自動**（維護者 2026-09-29，/docs/design/keys/e2ee-rpc.md §0）：「誰記住什麼」預設是 UI（房間版本號、同步起點、`txn_id`），daemon 不存快照；「被擋之後該補的」daemon 順手做完再一起回（1506 → 自動 refresh、新狀態放錯誤的 `data`），但🚫 不替 UI 做決定（不自動重送、`DeviceChanged` 只轉不叫 refresh）。兩題分開答：狀態放哪、動作能不能順手做完。
- 🔌 **訂閱與連線總是由 daemon 搞定**（維護者 2026-09-29，/docs/design/daemon/link-pool.md §3.1）：解鎖／登入後五條線全開、背景每 15 秒看一次；UI 收不收推播用 `subscribe`，🚫 沒有開關上游訂閱的 RPC。
- 🧯 **正式碼不用會讓整支程式收掉的方法**（維護者 2026-09-23）：`unwrap()`、`expect()`、`panic!`、`unreachable!`、`todo!`、`[i]` 直接索引／切片。
  每個失敗要有去處（`?` 往上丟、給安全值、`get`／`split_at_checked`、mutex 用 `unwrap_or_else(|p| p.into_inner())`）；「這裡不可能失敗」不是理由，「CLI 炸了就炸了」也不是。
  唯一的例外是「沒有它就沒有這支程式」的層級（runtime、主執行緒起不來），那也是講清楚、給 exit code，🚫 不 panic。
  **閘門在每個 crate 根**：`#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unreachable, clippy::todo, clippy::unimplemented, clippy::indexing_slicing, clippy::string_slice))]`——
  非測試建置有一個就編不過；測試建置（`#[cfg(test)]` 模組、`tests/*.rs`）放行，測試就是要看到它炸。
  這支 PR 把當時的 141 處全部給了去處（含 CLI）；server 端同一條規則在 wbfuwunel #83。

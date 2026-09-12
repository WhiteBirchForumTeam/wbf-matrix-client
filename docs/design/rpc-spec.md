# RPC 規格：前端 ↔ daemon 的每一則訊息

> 2026-09-12 第一版。形狀（加密、framing、`code`／`msg`、有 `id` 要回）在
> [`architecture-v2.md`](architecture-v2.md) §4，**這裡不重複**；這份只定**逐條**：
> method 清單、每個的 `params`／`result`、code 表、推播清單、資料平面的 HTTP 介面。
> 它是 `crates/wbf-daemon` 與 rpc-cli 的前提（handover §7 第 3 項），也是
> `wbf-core::CoreErrorKind` 配號的權威（PR #24 刻意留空等這份）。
>
> 🚫 **定了就不改的東西只有兩樣：`code` 的號碼、`method` 的名字。** 其餘（欄位可以加、
> 推播可以加、新的 method 可以加）都是相容的變動，不動 `protocol` 版號。

## 0. 一句話

```
前端 ──{ method, params, id }──> daemon ──{ code, msg, result, id }──> 前端
daemon ──{ method, params }（沒有 id）──> 前端        推播：progress、room.message、…
```

一條連線、任意多個未完成的 `id`、每個 `id` 一個回應；長工作的中途狀態用推播帶著那個 `id` 回來。

## 1. 連線的生命週期

1. 前端連上 `ws://127.0.0.1:<rpc port>`（port 在 `<data dir>/daemon.json`，§4.3）。
2. **第一則必須是 `hello`**。它之前送任何別的 method → daemon 回 `103` 然後**關連線**。
3. 解不開的 frame（token 不對）、超過 1 MiB 的 frame → **不回、直接關**（§4.4）。
4. 之後任意順序、任意並行。前端關掉連線＝它所有訂閱作廢、所有未完成的長工作**繼續跑**
   （上傳到一半不會因為 Desktop 關掉而中斷；要停要明講 `cancel`）。

### 1.1 `hello`

```jsonc
{ "method": "hello", "params": { "protocol": 1, "client": "rpc-cli 0.1.0" }, "id": 0 }
{ "code": 0, "msg": "ok", "id": 0, "result": {
    "protocol": 1,
    "daemon": "wbf-matrix-client-daemon 0.1.0",
    "data_dir": "C:/Users/me/AppData/Roaming/wbf-matrix-client",
    "unlocked": false,
    "key_mode": "passphrase"          // "plain" | "passphrase" | null（還沒有 local.key）
} }
```

- `protocol` 不相等 → `104`，關連線。第一版是 `1`。
- `client` 只進 log，給人看的。
- `hello` 與 `vault.*`、`daemon.*` 是**未解鎖時也接受**的全部（§4.5）；其他一律 `1001`。

## 2. 共同的 params 欄位

| 欄位 | 型別 | 意思 |
|---|---|---|
| `user` | string? | 對哪個帳號動作，mxid 或 localpart。**沒給 = `current`**。＝ `wbf-core::Target.user` |
| `server` | string? | 同名 localpart 在多個 server 時消歧。＝ `Target.server` |
| `transport` | `"ws"` \| `"http"` | 跟 wbfuwunel 講話走哪條。**預設 `ws`**。只有標了「有 `transport`」的 method 認得它 |

- ⚠️ **`server_backup` 不在 RPC 上**：那是 `wbf.conf` 的開關（CLI 規格 §10），**由 daemon 讀 conf 填進 `Target`**。
  前端不該替使用者決定要不要備份，而 daemon 就是 conf 的主人（它是 client 本體，architecture-v2 §0.2）。
- 認不得的欄位**忽略**（前端可以比 daemon 新）；缺必填欄位 → `102`。
- 路徑欄位（`path`、`out`）是 **daemon 那台機器的路徑**。1:1 同機所以通常就是前端的路徑，
  但 Android 沒有路徑可給——那些走資料平面（§6）。

## 3. method 清單

`名詞.動詞`（§4.6）。「core」欄是它包的 `wbf-core` 方法，沒有的標 ⚠️。

### 3.1 daemon 與 vault（未解鎖也接受）

| method | params | result | core |
|---|---|---|---|
| `hello` | §1.1 | §1.1 | — |
| `daemon.info` | — | `{ version, data_dir, unlocked, key_mode, rpc_port, data_port, uptime_seconds, connections }` | `key_mode`、`is_unlocked` |
| `daemon.shutdown` | — | `{ ok: true }`；回完之後才關 | — ⚠️ 生命週期整體還沒定（architecture-v2 §8 第 4 點），這條只是「有人能把它關掉」的最低限度 |
| `vault.unlock` | `{ passphrase_base64?: string }`。`plain` 模式不帶；`passphrase` 模式帶**原始 bytes** 的 base64（local-cache-db §12） | `{ ok: true, key_mode }` | `unlock` |
| `vault.lock` | — | `{ ok: true }` | ⚠️ core 沒有——`Core` 是解鎖一次就活著；daemon 這邊 lock ＝ 丟掉 `Core` 重開一個。**沒有 ticket 可刪**（§1 那個妥協消失了） |
| `vault.set_passphrase` | `{ passphrase_base64: string }` | `{ ok: true, key_mode: "passphrase" }` | `set_passphrase(Some)` |
| `vault.remove_passphrase` | — | `{ ok: true, key_mode: "plain" }` | `set_passphrase(None)` |

⚠️ passphrase 用 base64 而不是字串：它是任意 bytes（可以是一個 mp3）。🚫 不提供 `passphrase_file`
——那是「daemon 替前端讀檔」，web 前端根本給不出檔案路徑，而 rpc-cli 自己讀了再送不多一行。

### 3.2 帳號

| method | params | result | core |
|---|---|---|---|
| `account.add` | `{ server: string, user: string, password: string, device_name?: string }`。`device_name` 預設 `"wbf-matrix-client"` | `LoginResult`：`{ user_id, device_id, server, switched_from? }` | `log_in`。沒 `local.key` 就先 `create_vault(None)`（plain）——要 passphrase 模式先 `vault.set_passphrase` |
| `account.list` | — | `AccountStatus`：`{ accounts: [{ user_id?, server, localpart, logged_in, current }], undecryptable_hint? }` | `account_status` |
| `account.switch` | `{ user, server? }` | `SwitchResult`：`{ current, switched_from?, logged_in }` | `switch_current` |
| `account.whoami` | `{ user?, server? }` | `{ user_id, device_id, server }` | `whoami` |
| `account.del` | `{ user, server?, accept_history_loss?: bool }` | `{ user }` | `log_out`。閘門擋下 → `1021` |
| `account.destroy` | `{ user, server?, accept_history_loss?: bool }` | `DestroyResult`：`{ user, events_removed, media_removed, pool_files_removed, recovery_key_destroyed }` | `destroy_account` |

⚠️ `password` 在 RPC 上是明文字串——它在加密的 frame 裡，而且**只在這一則**。daemon 🚫 不留、不進 log、不進任何推播。
🚫 沒有「確認」這種互動：`account.destroy` 沒有 `--yes`，前端要問就自己問（daemon 不代前端做決定，§3）。

### 3.3 房間

| method | params | result | core |
|---|---|---|---|
| `room.list` | `{ user?, server? }` | `[Conversation]`（chat-model §2.1） | `list_conversations` |
| `room.get` | `{ room, user?, server? }` | `Conversation` | `conversation` |
| `room.send_text` | `{ room, body, user?, server? }` | `{ event_id }` | `send_text` |
| `room.send_file` | `{ room, path, caption?, cipher?, chunk_size?, name?, mimetype?, sha256?, transport?, user?, server? }`。**路徑版**：daemon 自己讀檔、上傳、送事件，一則回應。給有路徑的前端（rpc-cli、Desktop 拖檔） | `{ event_id, mxc, attachment_declared }` | `send_file`。長工作：推 `progress` |
| `room.send_attachment` | `{ room, upload_id, caption?, user?, server? }`。**資料平面版**的後半：`media.create` 之後、bytes 還在 PUT 的時候就能送（architecture-v2 §4.9 第 4 步） | `{ event_id, mxc, attachment_declared }` | ⚠️ core 沒有——現在 `send_file` 是「傳完再送」一條龍。要拆成「建檔→（送事件 ∥ 傳 bytes）」 |
| `room.history` | `HistoryQuery` 加 `user?`／`server?`：`{ room, limit, before?, source: "server"\|"cache", types?, sender? }` | `MessagePage`：`{ events: [Message], next? }` | `history` |
| `room.files` | `{ room, limit, before?, source, user?, server? }` | `FilePage`：`{ files: [{ event_id, sender, ts, manifest }], next? }` | `files(save_to: None)`。⚠️ CLI 的 `--save` 是前端的事：拿到 manifest 自己寫檔 |

🚫 **沒有 `room.watch`**。CLI 的 `watch tail|wait|once` 是「一個命令一個程序」的產物；daemon 常駐，
新訊息走**訂閱＋推播**（§4）。rpc-cli 要模擬 `watch once --timeout` 就是「訂閱、等第一則、退訂」。

### 3.4 同步

| method | params | result | core |
|---|---|---|---|
| `sync.recent` | `{ max_events?: 10000, window?: 320, batch?: 10, from_scratch?: bool, user?, server? }`（三層的意思在 CLI 規格 §3.5） | `RecentSummary`：`{ pulled, written, windows, batches, caught_up, cg_seq_before?, cg_seq_after?, skipped_without_room }` | `recent`。長工作：推 `progress` |

📎 daemon 之後 `recent` 應該是**它自己排程跑**的（連上 server 就補洞），這條 method 是「現在就跑一輪」。
排程怎麼訂還沒定，跟 architecture-v2 §6 的四條連線一起做。

### 3.5 上傳（不進房間的裸上傳；有 `transport`）

| method | params | result | core |
|---|---|---|---|
| `upload.file` | `{ path, cipher?, chunk_size?, name?, mimetype?, sha256?, transport?, user?, server? }` | `Manifest`（CLI 規格 §5） | `upload_file`。長工作 |
| `upload.status` | `{ upload_id, transport?, user?, server? }` | `UploadStatusReport` | `upload_status` |
| `upload.abort` | `{ upload_id, state_file?: path, transport?, user?, server? }` | `{ ok: true }` | `abort_upload` |

`upload --stream`（stdin）在 daemon 模型下**就是資料平面的 PUT**（§6.2），沒有對應的 method。

### 3.6 媒體（有 `transport`）

| method | params | result | core |
|---|---|---|---|
| `media.info` | `{ mxc, manifest?, transport?, user?, server? }` | `MediaInfo` | `media_info` |
| `media.open` | `{ manifest, user?, server? }` 或 `{ event_id, room, user?, server? }`（daemon 從快取找 manifest） | `{ url, mimetype?, size, expires_in }`。`url` 是資料平面的 capability URL（§6.1） | ⚠️ core 缺「給一個 reader」的形狀：現在 `download_to` 直接寫檔、`seek_read` 一次回整段 bytes。daemon 要的是 `PoolReader`（local-cache-db §8.6）接到 HTTP Range 上 |
| `media.create` | `{ room?, name, size?, mimetype?, cipher?, chunk_size?, sha256?, user?, server? }` | `{ upload_id, mxc, url, expires_in }`。`url` 是資料平面的 PUT URL（§6.2）。`mxc` 在這一步就有（server 的 `Create` 就配好 id）——所以 `room.send_attachment` 不必等傳完 | ⚠️ core 缺（同 `room.send_attachment`） |
| `media.save_to` | `{ manifest, out: path, no_cache?: bool, transport?, user?, server? }` | `DownloadResult` 或（`no_cache`）`DirectDownloadResult` | `download_to`／`download_direct`。長工作。**明文落地是使用者要的**（§4.8） |
| `media.stats` | `{ user?, server? }` | `MediaStats` | `media_stats` |
| `media.gc` | `{ quota_mib?: 2048, protect_days?: 7, user?, server? }` | `MediaGcReport` | `collect_media_garbage` |

`seek` 沒有 method：`media.open` 的 URL 上發 `Range` 就是 seek（同一個語意，約定 §7）。

### 3.7 房間金鑰備份

| method | params | result | core |
|---|---|---|---|
| `backup.status` | `{ user?, server? }` | `BackupStatusReport` 加 conf 的兩個開關 `server_backup_setting`／`local_room_keys_setting`（daemon 從 conf 填） | `backup_status` |
| `backup.upload` | `{ user?, server? }` | `UploadResult` | `upload_room_keys(also_save_snapshot = conf 的 LOCAL_ROOM_KEYS)` |
| `backup.save` | `{ user?, server? }` | `{ bytes }` | `save_room_key_snapshot` |
| `backup.import` | `{ user?, server? }` | `ImportResult` | `import_room_key_snapshot` |
| `backup.restore` | `{ user?, server? }` | `RecoveryStateReport` | `restore_from_recovery_key`。沒保管 → `1020` |
| `backup.create_recovery_key` | `{ user?, server? }` | `{ recovery_key }` ⚠️ 秘密，只回這一次 | `create_recovery_key` |
| `recovery.list` | — | `{ users: [mxid] }` | `list_recovery_key_users` |
| `recovery.show` | `{ user }` | `{ user, recovery_key }` ⚠️ 秘密 | `find_recovery_key`。沒有 → `1020` |

### 3.8 server

| method | params | result | core |
|---|---|---|---|
| `server.ping` | `{ transport?, user?, server? }` | `ServerHello` | `ping(client_name = daemon 的名字與版本)` |

### 3.9 訂閱、取消

| method | params | result |
|---|---|---|
| `subscribe` | `{ events: [string], user?, server? }`。`events` 是 §4 的推播名，`"*"` 全訂。`user` 給了就只收那個帳號的 | `{ subscribed: [string] }` |
| `unsubscribe` | `{ events: [string] }` | `{ subscribed: [string] }`（剩下的） |
| `cancel` | `{ id: number }`——**要取消的那個請求的 `id`** | `{ ok: true, was_running: bool }`。被取消的請求自己收到 `105` |

- 訂閱是**每條連線一份**，連線關了就沒了。
- `cancel` 只對長工作有意義（`room.send_file`、`upload.file`、`media.save_to`、`sync.recent`）；
  對已經回完的 `id` → `was_running: false`，不是錯誤。

## 4. 推播（沒有 `id` 的請求，daemon → 前端）

| method | params | 什麼時候 |
|---|---|---|
| `progress` | `{ id: number, done: number, total?: number, note?: string }`。`id` 是**哪個請求**的進度 | 長工作跑的時候。`total` 不知道就不帶（串流）。`note` 給人看，🚫 不做邏輯 |
| `room.message` | `{ user, room, message: Message }`（chat-model §2.2，含 `decrypted`／`undecryptable_reason`） | 這個帳號收到一則新訊息（sync 或 `Event/Push` 進來、解完密、寫進快取**之後**） |
| `sync.state` | `{ user, state: "connected"\|"disconnected"\|"catching_up"\|"caught_up", cg_seq? }` | 跟 server 的連線狀態變了 |
| `vault.state` | `{ unlocked: bool }` | 另一條連線解鎖或鎖上了——多條連線各自平等（§4.7），所以要互相通知 |

- 推播**要先 `subscribe`**（§4.6）。`progress` 例外：**發出長工作的那條連線自動收到自己請求的 `progress`**，不必訂——不然每個前端都要多寫一步。
- 推播是「不用輪詢」，🚫 不是「保證看得到全部」：慢的訂閱者會掉事件（`wbf-core::event::EVENT_QUEUE`），掉了就重查狀態。
- ⚠️ core 現在的 `CoreEvent::Progress` 是一句字串，`room.message` 對得上 `CoreEvent::Message`；
  `sync.state` 與結構化的 `progress` 是 **core 要補的 variant**（daemon PR 順手做，🚫 不在 daemon 裡 parse 那句字串）。

## 5. code 表（定了就不改）

`code` 是整數。`0` 成功。**`1–999` 是 RPC 層的**（daemon 自己擋下、沒碰 core）；**`1000–1999` 一對一對到 `CoreErrorKind`**；
之後有新層（例如 uniffi 綁定）從 `2000` 起。

### 5.1 RPC 層

| code | 名字 | 什麼時候 |
|---|---|---|
| 100 | `bad_request` | 解出來不是 JSON 物件、沒有 `method`、`id` 不是整數 |
| 101 | `unknown_method` | 沒這個 method |
| 102 | `invalid_params` | 缺必填、型別不對、base64 解不開、路徑不是絕對路徑 |
| 103 | `hello_required` | 第一則不是 `hello`。**回完關連線** |
| 104 | `protocol_mismatch` | `hello.protocol` 不對。**回完關連線** |
| 105 | `cancelled` | 這個請求被 `cancel` 掉了 |
| 106 | `busy` | 同一個帳號已經有一個同種的長工作在跑（例如兩個 `sync.recent`）。🚫 不排隊，讓前端決定 |
| 107 | `daemon_shutting_down` | `daemon.shutdown` 之後進來的任何請求 |

### 5.2 core 層 ＝ `CoreErrorKind` 的號碼

| code | `CoreErrorKind` | rpc-cli 的 exit code（CLI 規格 §4） |
|---|---|---|
| 1001 | `locked` | 1 |
| 1002 | `no_key_file` | 1 |
| 1003 | `need_passphrase` | 1 |
| 1004 | `unexpected_passphrase` | 1 |
| 1005 | `wrong_passphrase` | 1 |
| 1010 | `no_such_account` | 1 |
| 1011 | `ambiguous_account` | 1 |
| 1012 | `not_logged_in` | 1 |
| 1020 | `no_recovery_key_here` | 1 |
| 1021 | `history_would_be_lost` | 1 |
| 1100 | `usage` | 1 |
| 1200 | `io` | 1 |
| 1300 | `network` | 4 |
| 1400 | `server` | 2 |
| 1500 | `integrity` | 3 |
| 1600 | `timeout` | 5 |

- 號碼**留了縫**（1006–1009、1013–1019……）：同一族拆新 variant 就填進去，🚫 不重排。
- `usage`（1100）是過渡桶子（`error.rs` 自己標的）：每次前端需要分辨就拆一個新號碼出去，🚫 讓前端 parse `msg`。
- `msg` 就是 `CoreError.message`，給人看。**`kind` 的名字不另外放進回應**——`code` 就是它，一個欄位夠了（§4.6）。
- RPC 層錯誤（1xx）的 exit code 一律 **1**（用法錯），除了 `105` 是 **130**（跟 Ctrl-C 一樣的慣例）。

## 6. 資料平面（HTTP，`http://127.0.0.1:<data port>`）

架構在 §4.8；這裡只定路徑與狀態碼。**沒有全域 token**，每個 URL 自己就是 capability。

### 6.1 讀：`GET /media/<token>`

| | |
|---|---|
| 來源 | `media.open` 的 `url` |
| 支援 | `Range: bytes=a-b`（單一 range）；沒 `Range` 就整檔 |
| 回 | `200`（整檔）／`206 Partial Content`（有 Range）；`Content-Type` 是 manifest 的 mimetype，沒有就 `application/octet-stream`；`Accept-Ranges: bytes`；`Content-Length` |
| `416` | Range 超出檔尾 |
| `404` | 認不得或過期的 token。🚫 不分辨兩者 |
| `503` | 未解鎖 |
| `502` | 從 server 拉塊失敗（快取沒有、server 又拿不到）。⚠️ 半途失敗時 HTTP 已經回 200 了，只能斷連線——這跟 `seek` 的「stdout 已印出去的不收回」是同一件事 |

daemon 邊解密邊吐（媒體池 64 KiB 段各自 AEAD），🚫 不整檔進記憶體。同一個 token 可以重複 GET（播放器 seek）。

### 6.2 寫：`PUT /upload/<token>`

| | |
|---|---|
| 來源 | `media.create` 的 `url` |
| body | 明文 bytes，**一條連線送到底**。`Content-Length` 有就用（＝固定大小上傳），沒有（chunked transfer）就是串流上傳（`0/0` 哨兵那套，約定 §2） |
| 回 | `200` 加 JSON body ＝ `Manifest`（CLI 規格 §5）。**回應在 `Seal` 完成之後才到**，所以 PUT 的回應就是「傳完了」 |
| `4xx`／`5xx` | JSON body `{ code, msg }`，code 用 §5 的表 |
| `404`／`503` | 同 §6.1 |
| `409` | 這個 token 已經有一個 PUT 在進行 |

- 進度走 RPC 的 `progress`，`id` 是 `media.create` 那一則的 `id`——所以前端要留著那個 `id`。
- 中途斷線：daemon 保留狀態檔（CLI 規格 §6），同一個 token **重新 PUT 可以續傳**，daemon 從 `Status` 問到收了幾塊、回 `100 Continue` 之前先跳過那些 bytes。⚠️ 續傳的細節（怎麼告訴前端從第幾 byte 送）第一版不做，先整個重送。
- token TTL 與撤銷（architecture-v2 §8 第 5 點）**第一版**：TTL 1 小時、`account.del`／`vault.lock` 時全部作廢。

## 7. 一次完整的例子：Desktop 送一個 2 GB 的影片進 E2EE 房

```jsonc
→ { "method": "media.create", "params": { "room": "!r:localhost", "name": "v.mkv", "size": 2147483648, "mimetype": "video/x-matroska" }, "id": 12 }
← { "code": 0, "msg": "ok", "id": 12, "result": { "upload_id": 77, "mxc": "mxc://localhost/abc", "url": "http://127.0.0.1:51235/upload/7c1b…", "expires_in": 3600 } }

   前端同時做兩件事：
   (a) PUT http://127.0.0.1:51235/upload/7c1b…   ← bytes 開始流
   (b)
→ { "method": "room.send_attachment", "params": { "room": "!r:localhost", "upload_id": 77, "caption": "看這個" }, "id": 13 }
← { "code": 0, "msg": "ok", "id": 13, "result": { "event_id": "$e1", "mxc": "mxc://localhost/abc", "attachment_declared": true } }

← { "method": "progress", "params": { "id": 12, "done": 536870912, "total": 2147483648 } }
← { "method": "progress", "params": { "id": 12, "done": 1073741824, "total": 2147483648 } }
   …
   (a) 的 HTTP 回應到了：200，body 是 manifest → 傳完
```

(b) 失敗（`1400`）不影響 (a)；(a) 失敗（PUT 回 5xx）之後訊息已經在房間裡指著一個傳不完的檔——
這是 architecture-v2 §4.9 說的「兩件事」，前端要決定是重傳還是撤回訊息。

## 8. 跟 core 的差距（daemon PR 要順手補的）

| 缺什麼 | 給誰用 |
|---|---|
| `Core::lock()`（或 daemon 丟掉重開） | `vault.lock` |
| 「建檔 → 拿 reader 邊收邊傳」的拆分：`send_file` 現在是一條龍 | `media.create`、`room.send_attachment`、`PUT /upload` |
| `PoolReader` 接到 HTTP Range：`download_to` 只會寫檔 | `GET /media` |
| `CoreEvent` 補 `SyncState` 與結構化的 `Progress { id, done, total }` | §4 |
| `CoreErrorKind` 配上 §5.2 的號碼（`impl From<CoreErrorKind> for u32` 之類，一個地方） | 所有錯誤回應 |

## 9. 明確不做的

- 🚫 沒有批次請求（JSON-RPC 的陣列形式）：一條連線本來就能並行。
- 🚫 沒有「哪個前端是主」：§4.7。
- 🚫 沒有壓縮：frame 上限 1 MiB，大的走資料平面。
- 🚫 daemon 不問終端、不彈視窗、不讀 passphrase 檔：全部從 RPC 進來（§4.5）。
- 🚫 `msg` 不當邏輯用、🚫 `code` 不重排、🚫 `method` 不改名——改名等於新 method 加舊的廢棄，廢棄的回 `101` 前先活一個版本。

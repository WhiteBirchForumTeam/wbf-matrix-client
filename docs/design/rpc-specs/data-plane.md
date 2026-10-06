# 資料平面：媒體的 bytes 怎麼進出 daemon

```
http://127.0.0.1:<data port>
  PUT /upload/mxc/e-<B58 nonce>_<B58 密文>   UI → daemon：明文 bytes，daemon 邊收邊做 chunk 加密邊傳到 homeserver，傳完回 manifest
                                            ＋ header Wbf-Upload-Meta: e-<B58 nonce>_<B58 密文>（上傳狀態）
  PUT /upload/mxc/c-<B58 明文>               同上，明文模式（daemon.set_encryption 關掉時才收）
  GET /media/mxc/e-<B58 nonce>_<B58 密文>    daemon → UI：明文 bytes，從本機原檔、媒體池或現拉，支援 Range（§8）；`HEAD` 一樣、沒有 body
```

🚨 **媒體本身的 bytes 只走這裡**（維護者 2026-09-30 再確認）。RPC 只傳媒體**訊息**的 JSON：
`media.create`／`media.open` 拿 URL、`room.send_attachment` 送事件。🚫 bytes 不進 RPC。

為什麼是另一個 port、為什麼是 HTTP、為什麼不能給檔案路徑，在 /docs/design/rpc-specs/local-interface.md §1、§8；這份只定**怎麼用**。
method 的總表在 /docs/design/rpc-specs/rpc-spec.md §3.6、§3.3；這份是資料平面那幾支的權威，兩邊對不上時以這份為準。

## 0. 一句話

送一個附件是**兩步，都由 UI 發動**（維護者 2026-09-30 定）：

1. **HTTP**：`media.create` 拿一個 PUT 的 URL → `PUT` bytes → 回應是 manifest（有 `mxc` 與含金鑰的區塊）。
2. **RPC**：`room.send_attachment` 帶那份 manifest → daemon 組事件、送進房間。

🚫 daemon **不替 UI 發訊息**，也🚫 **不記上傳**：上傳的狀態在 PUT 的 header 裡（§2），傳完的東西在 UI 手上（manifest）。daemon 是連線的載體。

```
UI                                   daemon                               homeserver
 │ RPC media.create {room,name,size}   │                                      │
 │────────────────────────────────────>│  Upload/Create（描述已加密）         │
 │                                     │─────────────────────────────────────>│
 │ {upload_id, mxc, url, headers}      │<─────────────────────────────────────│
 │<────────────────────────────────────│                                      │
 │                                     │                                      │
 │ HTTP PUT /upload/mxc/e-… (bytes…)   │  Upload/Chunk × N（每塊各自加密）    │
 │   Wbf-Upload-Meta: e-…              │                                      │
 │────────────────────────────────────>│─────────────────────────────────────>│
 │                                     │  Upload/Seal（最終描述）             │
 │ 200 manifest                        │─────────────────────────────────────>│
 │<────────────────────────────────────│                                      │
 │                                     │                                      │
 │ RPC room.send_attachment            │  Event/Send（加密房是密文）          │
 │ {room, manifest, room_devices}      │  meta.attachments = [mxc]  ← §6      │
 │────────────────────────────────────>│─────────────────────────────────────>│
 │ {event_id}                          │<─────────────────────────────────────│
 │<────────────────────────────────────│                                      │
```

⚠️ **一定要傳完才能送**：server 只認 `Seal` 過的媒體，宣告一個還在傳的 mxc 會被**整則拒送**（`attachment … is not media this server has`，
2026-09-30 對真 server 驗到的）。所以順序是固定的，🚫 不能並行。
⚠️ 上傳與送訊息是**兩件事**：各自失敗、各自重試。上傳失敗就沒有訊息；送訊息失敗，檔案在 server 上、manifest 在 UI 手上，重送就好。

## 1. 誰做什麼

| 誰 | 做什麼 | 程式 |
|---|---|---|
| UI | 叫 `media.create`、PUT bytes、看 PUT 送出去多少當進度、拿 manifest 叫 `room.send_attachment` | — |
| daemon 的 handle | 建檔之後把上傳狀態封進 URL | `crates/wbf-daemon/src/handle/media.rs`、`rooms.rs` |
| daemon 的資料平面 | 開 URL 與 meta、Host 檢查、狀態碼、把 body 交給 core | `crates/wbf-daemon/src/data_plane.rs` |
| core | 建檔（決定加不加密）、收 bytes 切塊加密上傳、送附件事件（加密房先 Megolm） | `crates/wbf-core/src/attachment_ops.rs` |
| sdk | `Upload/*` 的每一個請求、`FileCipher`、事件 content | `crates/wbf-sdk/src/upload.rs`、`chunk_crypto.rs`、`event_json.rs` |

core 不知道 HTTP：bytes 從一個 `AsyncRead` 進來。建好的上傳（`UploadState`，**含檔案金鑰**）由呼叫端帶著、每一步交回來；
core 每一步都**自己再核對**「這個上傳是不是這個帳號的」、「這份 manifest 是不是這台 server 的」，🚫 不靠 daemon 記得查。

## 2. URL 與 meta：都用共享 token 加密（維護者 2026-09-30 定）

**URL 只帶「用途 ‖ mxc」**，上傳要的其他東西（含檔案金鑰）放在 PUT 的 header（維護者 2026-09-30）：

```
URL   /upload/mxc/e-<B58(nonce)>_<B58( XChaCha20-Poly1305(k_data, nonce, aad = "wbf-data url v1", 用途(1) ‖ mxc) )>
meta  Wbf-Upload-Meta: e-<B58(nonce)>_<B58( XChaCha20-Poly1305(k_data, nonce, aad = "wbf-data meta v1 " ‖ mxc, UploadState JSON) )>
k_data = BLAKE3 derive_key("wbf-matrix-client data plane v1", 共享 token)
```

實際長相（2 GB 影片，`media.create` 回的）：

```
用途 ‖ mxc            0x01 ‖ mxc://localhost/000000000000004d                        → URL 那段約 100 個字元
UploadState JSON      {"server":"http://127.0.0.1:6167","user_id":"@alice:localhost","upload_id":77,
                       "mxc":"mxc://localhost/000000000000004d","chunk_max_bytes":69632,
                       "block":{"v":1,"cipher":"chacha20-poly1305","key":"<32 byte>","nonce_base":"<8 byte>",
                       "chunk_size":65536,"file_size":2147483648,"name":"v.mkv","mimetype":"video/x-matroska"}}
                                                                                     → header 約 520 個字元
```

- **長相跟目錄名一樣**（維護者 2026-10-01）：`e-<B58(nonce)>_<B58(密文)>`，同 /docs/design/storage/vault-and-keys.md §2.2。
  前綴用連字號、nonce 與密文之間用底線——Base58 的字母表兩個都沒有，所以前綴說模式、底線說分隔，各管一件事，
  解析也不必靠「前 24 byte 是 nonce」這種長度常數。🚫 不用方括號（`[e-]`）：`[` `]` 在 URL 路徑裡要 percent-encode。
- **為什麼 URL 用加密、不用 hash**：下載的 URL 要交給播放器，而播放器🚫 不能加 header——URL 自己要開得回 mxc。上傳下載同一套。
- **meta 綁住它的 mxc**（AAD 接 mxc）：A 檔的 meta 配 B 檔的 URL 解不開；明文模式沒有 AAD，所以解開之後兩種都再對一次 mxc。
- **header 名字** `Wbf-Upload-Meta`：用連字號（有些代理會丟掉名字帶底線的 header）、🚫 不加 `X-`（RFC 6648）。
  `media.create` 直接回 `headers: { "Wbf-Upload-Meta": "…" }`，UI 照抄。
- **共享 token** 就是 UI 產生、交給 daemon 的那份 `daemon.token`（/docs/design/rpc-specs/local-interface.md §3），RPC 的兩把鑰也從它導出；
  資料平面的這把用另一個 context，跟 RPC 的分開。
- **daemon 解得開就是它發的**：🚫 沒有 token 表、🚫 沒有 TTL。上傳能活多久、要不要拒，是 server 的事（維護者 2026-09-30：
  「daemon 應該是更純的連線載體」）。
- **可以重用**：同一組 URL 與 meta 可以一直 PUT（續傳，§4.3）；nonce 每次隨機，所以同一個上傳鑄兩次會得到兩組不同但都有效的值。
- **URL 看不出是哪個檔**：mxc 在密文裡。URL 會進播放器與 HTTP 元件的 log，那些 log 只看得到一串 base58；檔案金鑰只在 header。
- **失效的時候**：daemon 重開（UI 每次重新產生 token，舊的就解不開了）、帳號登出（core 那邊沒有 session，照樣拒）。
- **用途**寫在 URL 的密文裡：下載的 URL（§8）拿來上傳會被拒。
- 認不得的 URL 或 meta（別的 token 發的、被改過一個字、用途不對、meta 不是這個 URL 的、形狀不對）一律 **404**，🚫 不分辨原因。沒帶 meta 是 **400**（講清楚要帶什麼）。
- **明文模式**：`daemon.set_encryption { enforced: false }`（除錯用，/docs/design/rpc-specs/rpc-spec.md §3.1）時，URL 是 `c-` ＋ B58(用途 ‖ mxc)、
  meta 是 `c-` ＋ B58(JSON)，都不加密。加密模式下拿 `c-` 來一律 404（fail closed）；加密的 `e-` 兩個模式都收。
  ⚠️ `c-` **沒有任何認證**：本機任何程序都能自己組出一組合法的 URL＋meta。「daemon 解得開就是它發的」只對 `e-` 成立；
  `c-` 擋得住的只剩 core 的「這個上傳是不是這個帳號的」核對——所以它只給除錯，🚫 不給正式環境。
  讀的那一面一樣：`c-` 模式下 `GET /media/mxc/c-…` 也沒有認證，知道 port 與 mxc 的本機程序（含瀏覽器裡的網頁）都讀得到解密後的媒體。
- **Host 檢查**：Host 標頭不是 `127.0.0.1`、`localhost`、`[::1]`（帶不帶 port 都可以）一律 **403**。擋的是 DNS rebinding：
  網頁把自己的網域指到 127.0.0.1 之後，瀏覽器送的 Host 是那個網域。
- **未解鎖一律 503**：東西在，只是現在打不開（/docs/design/rpc-specs/local-interface.md §5）。

📎 唯一的表是「正在收的上傳」：同一個上傳同時只收一條 PUT（兩條交錯送，串流的塊會亂），只活在那條連線的期間。

📎 daemon 沒開資料平面（單發命令、沒有 `-s`）時，`media.create` 回 **100**、🚫 不去 server 建檔：建了也沒有地方收 bytes。
`daemon.info`、`daemon.json`、ready 那行的 `data_port` 就是這個 port（啟動參數 `--data-port`，0 ＝ 隨機）。

## 3. 路徑

| 方法 | 路徑 | 狀況 |
|---|---|---|
| `PUT` | `/upload/mxc/<URL key>`，帶 `Wbf-Upload-Meta` | §4 |
| 其他方法 | `/upload/mxc/<URL key>` | **405**，`Allow: PUT` |
| `GET`、`HEAD` | `/media/mxc/<URL key>` | §8 |
| 其他方法 | `/media/mxc/<URL key>` | **405**，`Allow: GET, HEAD` |
| 任何 | 其他路徑 | **404** |
| 任何 | Host 不是 loopback | **403**（§2） |

錯誤的 body 是 JSON `{ "code": <int>, "msg": "<給人看的>" }`，`code` 跟 RPC 同一張表（/docs/design/rpc-specs/rpc-spec.md §5）。403、404、405 沒有 body。

## 4. 上傳

### 4.1 `media.create`

```jsonc
→ { "method": "media.create", "params": { "room": "!r:localhost", "name": "v.mkv", "size": 2147483648, "mimetype": "video/x-matroska" }, "id": 12 }
← { "code": 0, "msg": "ok", "id": 12, "result": {
      "upload_id": 77, "mxc": "mxc://localhost/000000000000004d",
      "url": "http://127.0.0.1:51235/upload/mxc/e-Hq3TbQ…_4kVn9s…",
      "headers": { "Wbf-Upload-Meta": "e-Hq3TbQ…_8Pz2Lw…" } } }
```

| params | 必要？ | example | 說明 |
|---|---|---|---|
| `name` | 必要 | `"v.mkv"` | 給對方看的檔名，進描述與事件區塊 |
| `room` | 選填 | `"!r:localhost"` | 要送去哪個房。**給了就照房間決定加不加密**（下面那張表）；沒給是裸上傳，照 `cipher` |
| `size` | 選填 | `2147483648` | 明文總長。有 ＝ **固定大小**（可以續傳）；沒有 ＝ **串流**（§4.4） |
| `mimetype` | 選填 | `"video/x-matroska"` | |
| `cipher` | 選填 | `"aes-256-gcm"` | 沒給用這台機器的預設；給了要跟房間對得上 |
| `chunk_size` | 選填 | `65536` | 沒給：固定大小照檔案大小挑，串流用行動網路那一檔（小塊，斷了少重送） |
| `source_uri` | 選填 | `"file:///home/me/v.mkv"` | 原檔在這台機器的位置，**URI**（§8.1）。意義由 UI 定，daemon 🚫 不驗；傳完記進 `media` 列，之後讀這個檔優先讀原檔 |
| `user`／`server` | 選填 | | 哪個帳號（/docs/design/rpc-specs/rpc-spec.md §2） |

**加不加密由房間決定**（/docs/design/media/wbf-client-convention-for-chunk.md §5.1），在建檔時就擋：

| 房間 | `cipher` 沒給 | 給了加密的 | 給了 `none` |
|---|---|---|---|
| 有 E2EE | 預設的加密 | 照給的 | **1100**：檔案會以明文存在 server 上 |
| 沒 E2EE | `none` | **1100**：金鑰會公開在事件裡 | `none` |

- 「房間加不加密」問的是**這一刻**的 `m.room.encryption`，🚫 不用快取（過期的「沒加密」會把金鑰公開出去）。
- `size: 0` 是 1100：協議沒有零塊的上傳。
- 一般 Matrix 帳號走傳統上傳（§7.2）：要 `size`、受 `m.upload.size` 限制、`cipher` 只認沒給與 `none`。（實作之前仍回 1100。）
- `upload_id` 給 `upload.status`／`upload.abort` 用；UI 送訊息用的是 PUT 回的 manifest（§5）。
- `source_uri` 跟上傳狀態一起封在 `Wbf-Upload-Meta` 裡（§2），PUT 傳完（`Seal` 成功、而且 server 沒截斷）才寫進 `media` 列；
  寫不進去只發一則提醒、PUT 照樣回 200（檔已經在 server 上了）。

### 4.2 `PUT /upload/mxc/<URL key>`

| | |
|---|---|
| header | `Wbf-Upload-Meta`：`media.create` 回的 `headers` 照抄。沒帶 **400** |
| body | 明文 bytes，**一條連線送到底**。`Content-Length` 或 chunked transfer 都可以 |
| `Content-Length` | 有帶的話，固定大小的上傳必須**剛好等於** `size`，不然 **400**、🚫 不讀 body |
| 回 | **200**，body 是 manifest（/docs/design/rpc-specs/wbf-cli-spec.md §5）。**回應在 `Seal` 之後才到**，所以 PUT 的回應就是「傳完了」 |
| 進度 | 就是這個 PUT 送出去多少。🚫 不走 RPC |

⚠️ manifest **含檔案金鑰**：UI 要存就自己用私有權限存。

錯誤：

| 狀態碼 | 什麼時候 | body 的 `code` |
|---|---|---|
| **400** | 沒帶 `Wbf-Upload-Meta`、body 比 `size` 短或長、`Content-Length` 對不上或不是數字、串流的 body 是空的、串流已經傳過又 PUT、上傳不是這個帳號的、找不到帳號 | 102（header、`Content-Length`）／1100 |
| **403** | Host 不是 loopback | — |
| **404** | 不是這個 daemon 發的 URL 或 meta；meta 不是這個 URL 的；帳號已經登出 | —／1012 |
| **409** | 這個上傳已經有一條 PUT 在收 | 106 |
| **502** | homeserver 拒絕、連不上、逾時；包括「這個上傳 server 那邊已經沒有了」（例如傳完之後又 PUT） | 1400／1300／1600 |
| **503** | vault 還沒解鎖 | 1001 |
| **500** | 帶了 `Content-Length` 卻在中途斷線（hyper 先回錯，core 收到的是 IO 錯）、其他本機 IO | 1200 |

### 4.3 同一個 URL 再 PUT 一次

| 上一次 | 這一次 |
|---|---|
| 還在收 | **409** |
| 固定大小、斷了 | **續傳**：UI 用同一組 URL 與 header 再送一次**整個** body；daemon 先問 server 收到第幾塊，收過的塊照讀（要算整檔 SHA-256）但🚫 不再送 |
| 串流、斷了 | **400**：server 不能續傳串流（新的 body 對不上已經在 server 的塊），重新 `media.create` |
| 傳完了 | **502**：server 已經把這個上傳收掉。UI 手上的 manifest 就是結果，🚫 不必再問 |

📎 還沒做：讓 UI「從第幾 byte 開始送」（`Content-Range` 或回報已收的位置）。現在是 UI 整個重送、daemon 跳過 server 已經有的塊——
多讀一遍本機的 bytes，但網路上只送缺的。

### 4.4 固定大小與串流

| | 固定大小（有 `size`） | 串流（沒有 `size`） |
|---|---|---|
| server 端 | `Create` 帶塊數 | `0/0` 哨兵（/docs/design/media/wbf-client-convention-for-chunk.md §6） |
| body 長度 | 必須剛好 `size` | 讀到 EOF 為止 |
| 續傳 | 可以（§4.3） | 不行 |
| manifest | `file_size` 就是 `size` | `file_size` 是實際讀到的 |

兩種都是 PUT 回 200 之後才送訊息（§0）；事件區塊用 manifest 那份，`file_size` 與 `sha256` 都有。

### 4.5 流量控制：本機快、上游慢的時候

UI 到 daemon 是本機（可能 100 MiB/s），daemon 到 homeserver 可能只有 20 KiB/s。daemon **不緩衝**，靠背壓：

```
UI ──TCP──> 核心 socket 緩衝 ──> hyper 讀取緩衝 ──> daemon 讀一塊 ──> 加密 ──> WS 送出 ──> 等 Ack ──> 才讀下一塊
```

daemon 在上一塊的 Ack 回來之前不讀 body；hyper 只在被要求時才從 socket 讀；核心緩衝滿了，TCP 視窗降到 0，UI 的 `write` 就卡住。
所以 UI 被自然壓到上游的速度，**每個上傳的記憶體是定值**：核心緩衝（幾 MB 以內）＋ hyper 緩衝（約 400 KiB）＋ 手上一塊（≤ 1 MiB）。

代價：PUT 會開到上游傳完為止（2 GB 在 20 KiB/s 要約 29 小時），UI 的 HTTP client 要把逾時關掉、UI 也不能先走。
「UI 丟完就走、daemon 背景慢慢送」要的是一個落地的上傳池（多一份磁碟、要管配額與清理、進度得另外回報），現在🚫 不做。

每送一塊才去連線池借一次 `Upload` 線（/docs/design/daemon/link-pool.md），🚫 不是整個檔握著它：整個檔握著線的話，
第二個檔的 `media.create` 就得等第一個傳完。同時有兩個檔在傳時兩者的塊交錯送，server 那邊每個上傳各自照序號收。

## 5. `room.send_attachment`

```jsonc
→ { "method": "room.send_attachment", "params": { "room": "!r:localhost", "caption": "看這個",
    "manifest": { "server": "http://127.0.0.1:6167", "mxc": "mxc://localhost/000000000000004d", "block": { … } },
    "room_devices": { "room_version": 81234, "members": { "@bob:localhost": "3-810b7c3be4" } } }, "id": 13 }
← { "code": 0, "msg": "ok", "id": 13, "result": { "event_id": "$e1", "mxc": "mxc://localhost/000000000000004d", "attachment_declared": true } }
```

| params | 必要？ | 說明 |
|---|---|---|
| `room` | 必要 | |
| `manifest` | 必要 | PUT 回的那份，原樣帶回來（`upload.file` 的也可以） |
| `caption` | 選填 | 進事件的 `body` 與 `caption` |
| `room_devices` | 加密房必要 | 跟 `room.send_text` 同一份（/docs/design/keys/e2ee-rpc.md §3） |
| `txn_id` | 選填 | 重送用同一個 |
| `user`／`server` | 選填 | |

- core 核對 manifest 是**這個帳號那台 server** 的（不是就 1100）；「上傳者是不是 sender」由 server 驗（不是就整則拒，§6）。
- 問這一刻的房間加不加密，**區塊跟房間對不上就拒（1100）**：加密房配 `none` 的檔（server 讀得到）、明文房配加密的檔（金鑰會公開）。
  建檔到送出之間房間可能變了，所以建檔與送出兩頭都擋。
- 明文房：`Event/Send` 送明文事件。加密房：只用後台已經分好的房間金鑰、Megolm 加密、帶 `room_version` 送（/docs/design/keys/e2ee-rpc.md §3）。
  事件 content 是 /docs/design/media/wbf-client-convention-for-chunk.md §5 的形狀，**檔案金鑰在區塊裡、區塊在密文裡**——這正是 Matrix 把附件金鑰放事件裡的做法。
- 被 1506 擋：回 **1401**，`data` 是 daemon 自動重拿的房間狀態（跟 `room.send_text` 一樣）。
  UI 用新的 `room_devices`、**同一份 manifest、同一個 `txn_id`** 重送；檔案🚫 不必重傳。
- 房間金鑰在 2 秒內沒準備好：回 **1402**，訊息沒送，`data` 帶 `txn_id`；同一份 manifest 與 `txn_id` 重送，檔案🚫 必重傳（/docs/design/keys/e2ee-rpc.md §3）。
- 同一份 manifest 可以送進好幾個房（轉傳）：每則各自宣告一次，server 的計數各自 +1（§6）。

## 6. ⚠️ 附件一定要宣告，不然媒體留不住

> 依據：wbfuwunel 的 /docs/design/media/chunked-upload-spec.md §12、wbfuwunel 的 /docs/design/media/media-attachments.md（維護者 2026-09-06 定方向）。

server 在加密房讀不到訊息內容，不知道哪則訊息用了哪個 mxc。**沒有任何訊息宣告的新媒體，計數是 0；過了保護期
（server 的 `media_unreferenced_grace_seconds`，至少 7 天）後台掃描會把它刪掉**——訊息還在，檔案沒了。

所以 `room.send_attachment` 送事件的**同一個請求**帶宣告：

| 入口 | 怎麼帶 |
|---|---|
| wbf pack `Event/Send` | meta `{ "room_id", "type", "txn_id", "attachments": ["mxc://…"], "room_version"? }`，data 是事件 content（加密房就是 `m.room.encrypted` 的 content） |

- **明文房也一律宣告**（server 自己讀得到 `url`，宣告了也無妨），少一個分支。
- 🚫 不分兩個請求：中間掛掉就留下一則指著會消失的媒體的訊息。
- server 驗每個 mxc：本站的、找得到（**已經 `Seal`**）、**上傳者就是 sender**、沒墓碑；任一個不過**整則拒送**。
- 結果的 `attachment_declared` 永遠是 `true`（這條路只有 wbf 帳號）。它存在是為了跟路徑版 `room.send_file` 同一個形狀——
  那邊一般 Matrix 帳號是 `false`（/docs/design/rpc-specs/rpc-spec.md §3.3）。
- ⚠️ 上傳完到送出之間，媒體靠保護期撐著；保護期是 server 的設定，client 🚫 不假設它多長。UI 拿到 manifest 就盡快送。

## 7. 傳統格式：一般 Matrix 的 homeserver 上傳、任何帳號下載標準附件（維護者 2026-09-30、10-06 定；還沒做）

homeserver 是官方 Matrix（不講 wbf）時，**用傳統方式上傳**，不是 wbf 的分塊（維護者 2026-09-30）。
下載是另一件事：**wbf 帳號也會收到傳統格式的附件**（同一台 server 上用 Element 的人、別台 server 同步過來的），所以傳統下載兩種帳號都要有，
看的是**檔案的格式**（事件內容），🚫 不是帳號的種類（/docs/design/media/media-download.md §12）。

UI 看到的形狀不變：`media.create` → `PUT` → `room.send_attachment`；讀一律 `media.open` → `GET /media`。

### 7.1 檔案有三種（`media.kind`）

| `kind` | 程式裡的名字 | 事件 `content` 裡有 | 加密 | 完整性 |
|---|---|---|---|---|
| 1 | `WbfChunked` | `org.wbftw.wbfuwunel.chunked`（/docs/design/media/wbf-client-convention-for-chunk.md §5） | 每塊各自 AEAD | 每塊收到就驗得了 |
| 2 | `MatrixEncrypted` | `file`（Matrix 的 `EncryptedFile`，`v: "v2"`） | 整檔一條 AES-256-CTR | 只有整檔讀完、比密文的 SHA-256（`file.hashes.sha256`）才知道 |
| 3 | `MatrixPlain` | 只有 `url` | 沒有 | 沒有可比的（只能靠 TLS 信 server） |

- 判斷順序照上表由上往下；三個都沒有就不是檔，不建 `media` 列。
- 程式裡是 enum、DB 存整數（維護者 2026-10-06）：讀到不認得的整數就回錯、🚫 猜成哪一種（DB 的 CHECK 擋著，不該發生）。
- `0` 🚫 是任何一種：漏寫或預設成 0 會被 CHECK 擋下來，而不是悄悄變成某一種。

### 7.2 上傳（只有一般 Matrix 帳號）

wbf 帳號一律走分塊（§4），🚫 走這條。

| 步驟 | 明文房 | 加密房 |
|---|---|---|
| `media.create` | 先問 `GET /_matrix/client/v1/media/config` 的 `m.upload.size`，`size` 超過就 **1100**、🚫 讀任何 bytes；再 `POST /_matrix/media/v1/create` 預先拿 mxc | 同左 |
| `PUT` | PUT 進來的 body 一段一段直接串流成 `PUT /_matrix/media/v3/upload/{server}/{media_id}` 的 body | 同左，中間邊收邊 AES-256-CTR 加密、邊算密文的 SHA-256 |
| PUT 回的 manifest | `{ server, mxc, kind: 3, name, mimetype, size }` | `{ server, mxc, kind: 2, name, mimetype, size, file: EncryptedFile }`（含 `key`、`iv`、`hashes.sha256`） |
| `room.send_attachment` | 標準的 `m.file`／`m.image`／`m.video`／`m.audio`，`url` 就是 mxc；`info.size`、`info.mimetype` 照 manifest | 同左，但 `url` 換成 `file`；Megolm 加密送出（matrix-sdk 的 `Room::send`） |
| 附件宣告（§6） | 沒有這個機制：媒體留多久照那台 server 自己的設定；`attachment_declared` 是 `false` | 同左 |

- **只收固定大小**：`/upload` 一個請求送完整個檔，要先講 `Content-Length`。`media.create` 沒給 `size` 就 **1100**。CTR 不改長度，密文長度就是 `size`。
- **加不加密照 §4.1 那張表**（問這一刻的房間）；`cipher` 只認「沒給」與 `none`，給了 wbf 的演算法名字是 1100（這條路只有 AES-256-CTR v2）；`chunk_size` 不適用，給了也忽略。
- **🚫 整檔讀進記憶體**（維護者 2026-10-06）：AES-CTR 是串流加密，第 i 個 byte 的密文只看明文第 i 個 byte 與它的位置；SHA-256 也能一段一段餵，
  而 hash 要到 `room.send_attachment` 才用得到，那時已經算完。所以 daemon 手上只有一小段，背壓同 §4.5（上游送不出去就不讀 PUT 的 body）。
- **加密用上游的串流零件**：`matrix-sdk-crypto` 的 `AttachmentEncryptor`（包住一個 `Read`、讀完 `finish()` 拿 `key`／`iv`／`hashes`）。🚫 自己刻 AES-CTR。
  它吃同步的 `Read`：在 `spawn_blocking` 裡跑，PUT 的 body 經一個有界 channel 餵它，加密後的段經另一個有界 channel 交給 HTTP 的串流 body；兩個 channel 都只放幾段，背壓不斷。
  🚫 用 matrix-sdk 的 `Media::upload`／`upload_encrypted_file`：它們先 `read_to_end` 整檔進記憶體。
- **金鑰與 IV 每次 PUT 現產**，🚫 在 `media.create` 產、🚫 放進 `Wbf-Upload-Meta`：同一組 key／IV 加密兩份不同的 body（UI 重送時改了檔）會洩漏兩份明文的 XOR。
  金鑰只在 PUT 回的 manifest 裡（跟 §4.2 一樣，UI 要存就自己用私有權限存）。
- **body 比 `size` 短或長**：中斷上游的請求（🚫 送出一個長度不對的檔），回 **400**。
- **🚫 續傳**：傳統 `/upload` 沒有「收到第幾塊」。斷了 UI 用同一組 URL 再 PUT 一次**整個** body，daemon 用新的 key／IV 重送到同一個 mxc；
  server 說那個 mxc 已經有內容（`M_CANNOT_OVERWRITE_MEDIA`）或預先拿的 mxc 過期了 → **502**，重新 `media.create`。
- 這條路的檔案用 Matrix 標準格式，別的 Matrix client 看得懂；wbf 的分塊檔只有 wbf client 看得懂（/docs/design/media/wbf-client-convention-for-chunk.md §5）。
- ⚠️ server 那邊收 `/upload` 是整檔進記憶體、可能沒有並行上限（wbfuwunel #110，server 怎麼做維護者還沒決定）；client 這邊🚫 為它做任何事，只照 `m.upload.size`。

### 7.3 下載（兩種帳號都要）

`kind` 2、3 的檔：`GET /_matrix/client/v1/media/download/{server}/{media_id}`（帶這個帳號的 access token；舊 server 回 `M_UNRECOGNIZED` 才退到 `/_matrix/media/v3/download/…`），
一個請求從頭讀到尾，邊收邊（`kind` 2）解密、邊照池格式 v2 寫進主檔。細節在 /docs/design/media/media-download.md §12；這裡只講 UI 看得到的：

- **`GET /media` 可以邊下載邊讀、驗證中也能讀**（維護者 2026-10-06）：已經寫進主檔的段照常交出去，跟 `kind` 1 一樣。
  ⚠️ `kind` 2 在整檔 hash 比對之前，交出去的位元組**還沒驗過**：AES-CTR 沒有防竄改，server 翻一個密文 bit，明文同一個 bit 就翻了、解密照樣成功。
  所以 `kind` 2 沒驗過、驗不過時狀態碼是 **412**、body 照給（§8.2 的約定）。
- **下載完自動驗**：`kind` 2 比密文的 SHA-256 與事件的 `file.hashes.sha256`，推播 `media.download` 先報 `verifying`、再報結果
  （`verified`：1 正確、2 不正確，/docs/design/media/media-download.md §12.3）。**驗不過🚫 刪檔**：資料留著、標 2，讀的時候是 412。
  `kind` 3 沒有 hash：大小對得上事件的 `info.size`（有給的話）就完成，`verified = 0`。
- **🚫 seek**：一個 HTTP 從頭讀到尾，沒有「先拉第 n 塊」。GET 要的位置還沒寫到就停著等主檔寫到那裡（同 §8「上游慢就停著等」）。
- **🚫 續傳**：daemon 重開或取消之後再要，從頭重下（它受 server 的上限、是快取）。要續傳得有 HTTP Range ＋ 從任意位置開始的 CTR，之後真的需要再做。
- `media.export_to` 跟 GET 同一個約定（/docs/design/media/media-download.md §7.3）。

## 8. 讀：`GET /media/mxc/<URL key>`

下載🚫 不需要 header：播放器只吃 URL。URL 跟上傳同一套（§2），用途是 `0x02`，**不帶帳號**（維護者 2026-10-01：「只要匹配 mxc 就能看」）：

```
/media/mxc/e-<B58(nonce)>_<B58( XChaCha20-Poly1305(k_data, nonce, "wbf-data url v1", 0x02 ‖ mxc) )>
```

這跟媒體池原本的設計一致（/docs/design/media/media-pool.md §2）：池跟 `cache.db` 同層、同 server 的帳號共用、不分帳號、不要可見性——
「拿得到 mxc 的人 server 就給他檔；可見性在事件那層擋過」。

**下載怎麼做的權威是 /docs/design/media/media-download.md**（維護者 2026-10-01 定）：每帳號一個下載處理端，要下載的檔一起跑、每個檔一塊在途，各自順序寫進池裡的**主檔**；
播放器 seek 到還沒拉到的地方，那幾塊的請求插到 `Download` 線發送 queue 的最前面，拉到的順序 append 進 **seek 暫存檔**，用 O(1) 的位置表記位置；
主檔追到時從暫存檔搬、不走網路；主檔完成就刪暫存檔。**每個檔都只被順序寫**。這裡只列 GET 由上往下找的來源（細節在 /docs/design/media/media-download.md §7.2）：

| 先後 | 來源 | 要不要檔案金鑰 |
|---|---|---|
| 1 | 本機原檔（§8.1） | 🚫 不要 |
| 2 | 池裡完整的主檔（`wbf_sdk::media::open_complete` → `PoolReader`，明文位置的 `Read + Seek`） | 🚫 不要（池金鑰） |
| 3 | 主檔已寫的段（下載中） | 🚫 不要（池金鑰） |
| 4 | seek 暫存檔（位置表裡有） | 🚫 不要（池金鑰） |
| 5 | 現拉（seek）：`Download` 線插隊 `Read`，解開後存進暫存檔再吐 | 要，從事件拿（`event_media` → `events.content_json` 的區塊） |

- URL 可以重用，播放器 seek 沒問題；daemon 邊讀邊吐，🚫 不整檔進記憶體。
- mxc 屬於哪一份 `cache.db`（一台 server 一份）：照本機已登入的帳號一個一個找，mxc 的 server_name 跟帳號網域一樣的先找。現拉的金鑰只從那個帳號看得到的事件拿，🚫 不跨帳號借。

| | |
|---|---|
| 支援 | `Range` 單一一段：`bytes=a-b`、`bytes=a-`、`bytes=-n`（最後 n byte），終點超過檔尾就截到檔尾；沒帶、寫壞了、不只一段 → 整檔（RFC 9110 §14.2：認不得的 Range 可以不理）。`HEAD` 回一樣的標頭、沒有 body |
| 回 | `200`（整檔）／`206 Partial Content`（有 Range，帶 `Content-Range`）；`Content-Type` 是 `media` 列的 mimetype（沒有就用區塊的），都沒有就 `application/octet-stream`；`Accept-Ranges: bytes`；`Content-Length`；`X-Content-Type-Options: nosniff` 與 `Content-Security-Policy: sandbox`（型別是寄件者填的，被瀏覽器當頁面打開時🚫 跑腳本） |
| `412` | `kind` 2（傳統加密）的檔還沒驗、或驗不過：**body 照給**，跟 `200`／`206` 一樣（§8.2 的約定）。每個回應都帶 `Wbf-Media-Kind`、`Wbf-Media-Verified` |
| `416` | Range 的起點在檔尾或之後（帶 `Content-Range: bytes */<大小>`） |
| `404` | 不是這個 daemon 發的 URL、用途不對（上傳的 URL）、或本機沒有任何帳號有這個 mxc 的紀錄 |
| `503` | 未解鎖 |
| `502` | 有紀錄，但沒有完整的檔、也沒有帳號拿得到金鑰去拉；或看得到的描述都跟本地那一列對不上（`Integrity`，/docs/design/media/media-download.md §7.2） |

- body 開始吐了才拉不到（線斷了、一塊壞了）：狀態碼已經送出去了，所以是讓 body 出錯、連線斷掉，播放器知道沒收完（🚫 不假裝結束）。
- 播放中的進度就是這個 GET 收到多少 bytes；背景下載的進度是推播 `media.download`（/docs/design/media/media-download.md §5.5）。

### 8.1 路由：本機原檔優先（維護者 2026-10-01 定）

讀一個 mxc 時，**先看 `media` 列有沒有 `source_uri`**——這台機器上傳它時 UI 給的原檔位置：

```
media 列在，而且 source_uri 不是空的
  → 照下面的規則解成本機路徑 → 是一般檔、大小剛好等於 media.file_size → 直接讀原檔
  → 任何一步不成立 → 當成「本機沒有原檔」，走上面那張表（池）
```

- **為什麼優先**：從池拿要先確認要的那一段下載了沒，很複雜；從原檔拿什麼都不用管——🚫 不觸發下載、🚫 不碰池、🚫 不動 DB（不 touch `last_used_at`、不發任何事件）。
- **server 截斷過就不記**：`Seal` 時 server 說截斷了（含續傳之前那一輪截斷的，從 `Status` 讀），`source_uri` 不寫進列；
  就算漏了，列的 `file_size` 跟原檔對不上，讀的時候大小比對也會自然退回池。
- **大小相同就信**：這是猜，不是驗證（🚫 不算 hash，大檔太貴）。原檔被改過但大小沒變，讀到的就是改過的內容——接受，原檔是使用者自己的。
- **`source_uri` 一律是 URI**，意義由 UI 決定，daemon 只有猜的權力。解的規則（`wbf_sdk::local_source`）：

| 寫法 | daemon 怎麼解 |
|---|---|
| `file:///home/me/v.mkv` | `/home/me/v.mkv` |
| `file:///C:/Users/me/v.mkv` | Windows：`C:/Users/me/v.mkv`（去掉磁碟機代號前那個 `/`） |
| `file:///Users/me/v.mkv`（在 Windows 上） | 路徑以 `/` 開頭、沒有磁碟機：**目前磁碟的根** |
| `file://localhost/home/me/v.mkv` | 同第一列（host 是 `localhost` 等於空） |
| `%20`、`%E5%BD%B1` 這類 | 照 RFC 3986 百分比解碼，結果要是 UTF-8 |
| 裸路徑 `/home/me/v.mkv`、`C:\…`、相對路徑 | **非法**：當成本機沒有原檔 |
| `file://server/share/…`（別的 host）、`file:/x`、百分比編碼壞、含 NUL | 同上 |
| `content://…`（Android） | 同上；之後要支援得靠 NDK 那邊解，到時候再說 |

- **約定**：UI 一律給 `file://` 開頭的 URI，🚫 不要直接給 `/絕對路徑`。daemon 🚫 不擋非法的值（`media.create` 照收、照記），只是讀的時候不走本機。
- 只有**這台機器傳的**檔才有 `source_uri`；別人傳來的、或 UI 沒給的，一律走池。

### 8.2 資料照給，狀態碼說它可不可信（維護者 2026-10-06 定的約定）

**約定**：還沒驗、或驗不過的檔，daemon 🚫 擋、🚫 刪，**資料照給，用狀態碼（RPC 是錯誤碼）告訴前端它可不可信**。
前端看狀態碼決定要不要用；堅決要拿就照拿——資料都在 body，該警告使用者的由 UI 警告。

| 檔（`media.kind`，§7.1） | `media.verified` | `GET`／`HEAD` 的狀態碼 | body |
|---|---|---|---|
| 1 `WbfChunked` | 不看 | `200`／`206` | 照常 |
| 2 `MatrixEncrypted` | 1 驗了、正確 | `200`／`206` | 照常 |
| 2 `MatrixEncrypted` | 0 還沒驗（下載中、驗證中）或 2 驗了、不正確 | **`412 Precondition Failed`** | **照常**：跟 `200`／`206` 會給的一模一樣（有 Range 就帶 `Content-Range`、只給那一段） |
| 3 `MatrixPlain` | 不看（沒有 hash 可比） | `200`／`206` | 照常 |
| 任何一種，從本機原檔讀（§8.1） | 不看 | `200`／`206` | 照常（這台機器自己傳的） |

- **為什麼只有 `kind` 2**：`kind` 1 每塊下載時各自 AEAD 驗過（不需要整檔 hash）；`kind` 3 沒有發送者給的 hash，驗不了也就沒有「沒驗過」可說；
  只有 `kind` 2 是「有 hash 可比、而 AES-CTR 本身擋不住竄改」（§7.3）。
- **每個回應都帶兩個標頭**，讓前端知道 412 是哪一種：`Wbf-Media-Kind: 1|2|3`、`Wbf-Media-Verified: 0|1|2`（`HEAD` 也有，前端可以先問再決定要不要拿）。
- ⚠️ 412 對一般播放器就是錯誤：`<video src=…>` 這類直接吃 URL 的 🚫 會播還沒驗過的 `kind` 2。這是刻意的安全預設——
  要在驗完之前就播，UI 得自己用 HTTP client 拿、看到 412 仍然收 body。
- 412 是「條件不成立」：這裡的條件是「這個檔驗過、而且正確」，而前端🚫 必帶任何條件標頭——條件是這個約定本身。
- 驗證本身與它的進度推播在 /docs/design/media/media-download.md §12.3；`media.export_to` 用同一個約定（/docs/design/media/media-download.md §7.3）。

## 9. 本機這一段的取捨：現在是明文（維護者 2026-09-30 定）

UI ↔ daemon 的 **bytes 是明文**，保護靠「URL 是 daemon 用共享 token 加密發的」。老實寫清楚這條線擋得住誰、擋不住誰：

| 本機的誰 | 擋得住嗎 |
|---|---|
| 同一台機器的**別的 OS 帳號**、瀏覽器裡的網頁（對 localhost 發請求、DNS rebinding） | ✅ 猜不到 URL（§2）；Host 不對就 403 |
| 能監聽 loopback 的人（管理員／root） | ❌ 看得到 bytes。維護者定：被監聽封包就沒轍 |
| 同一個 OS 帳號底下的惡意程式 | ❌ 讀得到 UI 的記憶體與使用者原本那個檔 |

⭐ **伏筆：以後的「機密模式」**（維護者 2026-09-30）。一個隱私通訊軟體，可能會要連本機、連記憶體也保護：
金鑰在記憶體裡也是加密的（鑑識撈記憶體也找不到）、UI 一上鎖就解不開、關機之後 vault 直接鎖死，
而媒體播放改成 UI 拿 bytes 自己 decode 去 render（例如整合 ffplay），🚫 不再把 URL 交給外部播放器。那時這裡要堵的是：

- **body 加密**：`e-` 的 URL 之外，body 本身也用從 token 導出、每條連線不同的鑰加密（以最省 CPU 的 AEAD 分幀，有 AES-NI 用 AES-GCM、沒有用 ChaCha20-Poly1305），
  回應的 manifest 也加密。明文模式（`c-`）才送明文。
- **下載只給自己的 UI**：body 加密之後外部播放器就吃不了，播放要走 UI 自己的 decode。這是 UI 的範圍，daemon 只負責把 bytes 加密吐出去。
- **manifest 不落在 UI 的明文記憶體太久**：它含檔案金鑰。
- **daemon 這邊**：vault 的主金鑰、檔案金鑰在記憶體裡也要包起來、上鎖就丟（現在 `Core` 解鎖一次活到程序結束，/docs/design/rpc-specs/local-interface.md §5）。

這一節🚫 不是現在要做的事，是提醒：哪天要做機密模式，上面每一條都要回來堵。

## 10. 明確不做的

- 🚫 daemon 替 UI 發訊息：上傳完就停在「回 manifest」，發不發是 UI 的事（§0）。
- 🚫 token 表、🚫 TTL、🚫 一次性 URL：URL 自己帶著狀態、可以重用，要不要拒是 server 的事（§2）。
- 🚫 把 bytes 放進 RPC（base64 也不要）：小東西的例外只有 /docs/design/rpc-specs/local-interface.md §8 說的縮圖那種，而且不是上傳。
- 🚫 給 UI 檔案路徑讀媒體池：池是加密的，給路徑不是給密文就是明文落地（/docs/design/rpc-specs/local-interface.md §8）。
- 🚫 落地的上傳池（§4.5）。

## 11. 現況

| 項目 | 狀態 | 測試 |
|---|---|---|
| `media.create`、`PUT /upload/mxc/…`、`room.send_attachment`（wbf 帳號，明文房與加密房，固定大小與串流） | ✅ | core `attachment_ops::tests`、daemon `data_plane::tests` 與 `tests/data_plane.rs`、真 server `tests/real_server.rs` 的 `an_attachment_goes_over_the_data_plane_into_plain_and_encrypted_rooms` |
| URL 與 meta（`e-`／`c-`、別的 token 發的拒、被改過的拒、用途不對的拒、meta 配不上 URL 的拒、沒帶 meta 的 400）、Host 檢查 | ✅ | daemon `data_plane::tests`、`tests/data_plane.rs` |
| 續傳（固定大小再 PUT） | ✅（假 server） | core `a_sized_body_must_match_and_a_second_put_resumes` |
| 一般 Matrix 帳號的傳統上傳（§7.2） | ❌（2026-10-06 定了形狀：串流 AES-CTR、只收固定大小、先問 `m.upload.size`） | |
| `media.kind`／`verified`（cache.db v9）、下載完自動驗、GET 的 412、匯出的 1501（§8.2、/docs/design/media/media-download.md §12.3） | ✅ | sdk `media_kind::tests`、`cache::tests::media_kind_and_verification_are_recorded_and_checked`；core `download_queue::tests::a_traditional_encrypted_file_reads_but_is_trusted_only_once_it_matched`；daemon `data_plane::tests`（`status_for_trust`）、真 server `tests/real_server.rs` |
| 傳統格式附件的下載，兩種帳號（§7.3、/docs/design/media/media-download.md §12）：HTTP 串流、`AttachmentDecryptor` 邊收邊解、進同一個池、邊下載邊讀、🚫 seek、🚫 續傳；同一個 mxc 兩份描述照 §12.1 | ✅ | 真 server `tests/real_server.rs` 的 `a_standard_matrix_attachment_downloads_over_http_and_reads_over_the_data_plane`（`kind` 2 對的與被改過一個 bit 的、`kind` 3）；sdk `tests/matrix_media.rs`、`tests/chat_mapping.rs` 的 `standard_attachments_are_recognised_and_a_broken_encrypted_one_is_not_taken_as_plain`、`media::tests::two_matrix_descriptions_are_the_same_file_only_with_the_same_kind_hash_and_size`；core `matrix_download::tests` |
| `media.open`、`GET`／`HEAD /media`（Range、416、用途不對的 URL 不收）、下載處理端、seek 暫存檔（§8，/docs/design/media/media-download.md） | ✅ | daemon `data_plane::tests` 與 `tests/data_plane.rs`、core `download_queue::tests`、真 server `tests/real_server.rs` 的 `an_attachment_goes_over_the_data_plane_into_plain_and_encrypted_rooms`；清單在 /docs/design/media/media-download.md §10 |
| `source_uri`：`media.create` 收、封進 meta、傳完記進 `media` 列；URI 解析與大小比對；讀的時候優先讀原檔（§8.1） | ✅ | core `the_local_source_is_remembered_once_the_upload_is_sealed`、sdk `local_source::tests` |
| UI 指定從第幾 byte 續傳 | ❌ | |
| 機密模式（§9） | ❌ 伏筆 | |

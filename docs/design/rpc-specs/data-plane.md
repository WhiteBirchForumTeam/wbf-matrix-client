# 資料平面：媒體的 bytes 怎麼進出 daemon

```
http://127.0.0.1:<data port>
  PUT /upload/mxc/e_<base58>   UI → daemon：明文 bytes，daemon 邊收邊做 chunk 加密邊傳到 homeserver，傳完回 manifest
                               ＋ header Wbf-Upload-Meta: e_<base64url>（上傳狀態）
  PUT /upload/mxc/c_<base58>   同上，明文模式（daemon.set_encryption 關掉時才收）
  GET /media/mxc/e_<base58>    daemon → UI：邊拉邊解密邊吐，支援 Range（還沒做，§8）
```

🚨 **媒體本身的 bytes 只走這裡**（維護者 2026-09-30 再確認）。RPC 只傳媒體**訊息**的 JSON：
`media.create` 拿 URL、`room.send_attachment` 送事件。🚫 bytes 不進 RPC，進度也不走 RPC。

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
 │ HTTP PUT /upload/mxc/e_… (bytes…)   │  Upload/Chunk × N（每塊各自加密）    │
 │   Wbf-Upload-Meta: e_…              │                                      │
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
URL   /upload/mxc/e_<base58( nonce(24) ‖ XChaCha20-Poly1305(k_data, aad = "wbf-data url v1", 用途(1) ‖ mxc) )>
meta  Wbf-Upload-Meta: e_<base64url( nonce(24) ‖ XChaCha20-Poly1305(k_data, aad = "wbf-data meta v1 " ‖ mxc, UploadState JSON) )>
k_data = BLAKE3 derive_key("wbf-matrix-client data plane v1", 共享 token)
```

實際長相（2 GB 影片，`media.create` 回的）：

```
用途 ‖ mxc            0x01 ‖ mxc://localhost/000000000000004d                        → URL 那段約 100 個字元
UploadState JSON      {"server":"http://127.0.0.1:6167","user_id":"@alice:localhost","upload_id":77,
                       "mxc":"mxc://localhost/000000000000004d","chunk_max_bytes":69632,
                       "block":{"v":1,"cipher":"chacha20-poly1305","key":"<32 byte>","nonce_base":"<8 byte>",
                       "chunk_size":65536,"file_size":2147483648,"name":"v.mkv","mimetype":"video/x-matroska"}}
                                                                                     → header 約 500 個字元
```

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
- **明文模式**：`daemon.set_encryption { enforced: false }`（除錯用，/docs/design/rpc-specs/rpc-spec.md §3.1）時，URL 是 `c_` ＋ base58(用途 ‖ mxc)、
  meta 是 `c_` ＋ base64url(JSON)，都不加密。加密模式下拿 `c_` 來一律 404（fail closed）；加密的 `e_` 兩個模式都收。
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
| `GET` | `/media/mxc/<URL key>` | 還沒做（§8），現在是 404 |
| 任何 | 其他路徑 | **404** |
| 任何 | Host 不是 loopback | **403**（§2） |

錯誤的 body 是 JSON `{ "code": <int>, "msg": "<給人看的>" }`，`code` 跟 RPC 同一張表（/docs/design/rpc-specs/rpc-spec.md §5）。403、404、405 沒有 body。

## 4. 上傳

### 4.1 `media.create`

```jsonc
→ { "method": "media.create", "params": { "room": "!r:localhost", "name": "v.mkv", "size": 2147483648, "mimetype": "video/x-matroska" }, "id": 12 }
← { "code": 0, "msg": "ok", "id": 12, "result": {
      "upload_id": 77, "mxc": "mxc://localhost/000000000000004d",
      "url": "http://127.0.0.1:51235/upload/mxc/e_3mJr7AoUXx2Wqd…",
      "headers": { "Wbf-Upload-Meta": "e_Qk3vT0…" } } }
```

| params | 必要？ | example | 說明 |
|---|---|---|---|
| `name` | 必要 | `"v.mkv"` | 給對方看的檔名，進描述與事件區塊 |
| `room` | 選填 | `"!r:localhost"` | 要送去哪個房。**給了就照房間決定加不加密**（下面那張表）；沒給是裸上傳，照 `cipher` |
| `size` | 選填 | `2147483648` | 明文總長。有 ＝ **固定大小**（可以續傳）；沒有 ＝ **串流**（§4.4） |
| `mimetype` | 選填 | `"video/x-matroska"` | |
| `cipher` | 選填 | `"aes-256-gcm"` | 沒給用這台機器的預設；給了要跟房間對得上 |
| `chunk_size` | 選填 | `65536` | 沒給：固定大小照檔案大小挑，串流用行動網路那一檔（小塊，斷了少重送） |
| `user`／`server` | 選填 | | 哪個帳號（/docs/design/rpc-specs/rpc-spec.md §2） |

**加不加密由房間決定**（/docs/design/media/wbf-client-convention-for-chunk.md §5.1），在建檔時就擋：

| 房間 | `cipher` 沒給 | 給了加密的 | 給了 `none` |
|---|---|---|---|
| 有 E2EE | 預設的加密 | 照給的 | **1100**：檔案會以明文存在 server 上 |
| 沒 E2EE | `none` | **1100**：金鑰會公開在事件裡 | `none` |

- 「房間加不加密」問的是**這一刻**的 `m.room.encryption`，🚫 不用快取（過期的「沒加密」會把金鑰公開出去）。
- `size: 0` 是 1100：協議沒有零塊的上傳。
- 一般 Matrix 帳號現在是 1100（§7）。
- `upload_id` 給 `upload.status`／`upload.abort` 用；UI 送訊息用的是 PUT 回的 manifest（§5）。

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
| **500** | 其他（本機 IO 等） | |

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
- 明文房：`Event/Send` 送明文事件。加密房：先分房間金鑰、Megolm 加密、帶 `room_version` 送（/docs/design/keys/e2ee-rpc.md §3）。
  事件 content 是 /docs/design/media/wbf-client-convention-for-chunk.md §5 的形狀，**檔案金鑰在區塊裡、區塊在密文裡**——這正是 Matrix 把附件金鑰放事件裡的做法。
- 被 1506 擋：回 **1401**，`data` 是 daemon 自動重拿的房間狀態（跟 `room.send_text` 一樣）。
  UI 用新的 `room_devices`、**同一份 manifest、同一個 `txn_id`** 重送；檔案🚫 不必重傳。
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

## 7. 一般 Matrix 的 homeserver：傳統上傳（還沒做）

維護者 2026-09-30 定：homeserver 是官方 Matrix（不講 wbf）時，**用傳統方式上傳**，不是 wbf 的分塊。現在 `media.create` 對一般 Matrix 帳號回 1100。

預定的形狀（下一支 PR；UI 看到的 `media.create` → `PUT` → `room.send_attachment` 不變）：

| 步驟 | 明文房 | 加密房 |
|---|---|---|
| `media.create` | `POST /_matrix/media/v1/create` 預先拿 mxc | 同左，另外產一組 AES-256-CTR 的鑰與 IV |
| `PUT` | 串流轉送到 `PUT /_matrix/media/v3/upload/{server}/{media_id}`（要 `Content-Length`，所以只收固定大小） | 邊收邊 AES-CTR 加密、邊算密文的 SHA-256，一樣串流轉送 |
| `room.send_attachment` | 標準的 `m.file`／`m.image`…，`url` 就是 mxc | `m.file` 的 `file` 欄位（`EncryptedFile`：`key`、`iv`、`hashes.sha256`、`v: "v2"`），Megolm 加密送出 |
| 附件宣告 | 沒有這個機制：媒體留多久照那台 server 自己的設定 | 同左 |

- 🚫 不能整檔讀進記憶體：`/upload` 一個請求送完整個檔，所以是「PUT 進來的 body 直接串流成 `/upload` 的 body」，daemon 手上只有一小段。
- 這條路的檔案用的是 Matrix 標準格式，別的 Matrix client 看得懂；wbf 的分塊檔只有 wbf client 看得懂（/docs/design/media/wbf-client-convention-for-chunk.md §5）。

## 8. 讀：`GET /media/mxc/<URL key>`（還沒做；預定的設計）

下載🚫 不需要 header：播放器只吃 URL。daemon 拿 URL 開出來的 mxc **查自己的資料庫**就有其他一切。

**URL**：跟上傳同一套（§2），用途是 `0x02`，明文多帶**是哪個帳號**：

```
/media/mxc/e_<base58( nonce ‖ XChaCha20-Poly1305(k_data, "wbf-data url v1", 0x02 ‖ mxid ‖ 0x00 ‖ mxc) )>
```

為什麼要帶帳號：`cache.db` 是**一台 server 一份、多個帳號共用**（/docs/design/storage/local-cache-db.md），哪個帳號看得到哪則事件記在
`events_synced_log`。不帶帳號的話，daemon 在 GET 時就無從判斷「這個 URL 是替誰開的」，只能信任發 URL 那一刻的判斷——帳號登出、
事件被藏起來之後 URL 照樣能用。帶了就能在**每一次 GET** 重新檢查（A5：不是正面認得就拒）。

**`media.open { mxc }` 與每一次 GET 都做的事**：

| 步 | 查什麼 | 在哪 |
|---|---|---|
| 1 | 這個 mxc 有沒有 `media` 列 | `media.mxc` |
| 2 | 引用它的事件 | `event_media` → `events` |
| 3 | 那些事件裡，**這個帳號看得到**的（有 `events_synced_log` 列、沒 `hidden`） | `events_synced_log` |
| 4 | 從看得到的那則事件拿區塊（含檔案金鑰）；加密房用解密後的 `content_json` | `events.content_json` |
| 5 | 有完整的快取就讀媒體池（64 KiB 段各自 AEAD，Range 直接 seek）；沒有就逐塊向 server 拉、解、切 | `media.pool_file`／`Download/Read` |

- 任一步找不到 → `media.open` 回 1100、GET 回 404。🚫 不會因為「mxc 對得上」就給：檔案金鑰只從這個帳號看得到的事件拿。
- `media` 表🚫 存金鑰（現在也沒有）：金鑰只在事件裡，事件的可見性就是金鑰的可見性。

| | |
|---|---|
| 支援 | `Range: bytes=a-b`（單一 range）；沒 `Range` 就整檔 |
| 回 | `200`（整檔）／`206 Partial Content`（有 Range）；`Content-Type` 是區塊的 mimetype，沒有就 `application/octet-stream`；`Accept-Ranges: bytes`；`Content-Length` |
| `416` | Range 超出檔尾 |
| `404` | 不是這個 daemon 發的 URL、或這個帳號（已經）看不到那個檔 |
| `503` | 未解鎖 |
| `502` | 從 server 拉塊失敗。⚠️ 半途失敗時 HTTP 已經回 200 了，只能斷連線 |

- URL 可以重用，播放器 seek 沒問題；daemon 邊解密邊吐，🚫 不整檔進記憶體。
- 🚨 **上游慢下來的時候：停止送 bytes，但連線開著**（維護者 2026-09-13 定）。🚫 不回空回應（UI 會以為傳完了）、🚫 不斷線（UI 會以為失敗了）；
  拿不到才斷，還在拿就等。🚫 逾時不要設得比 homeserver 的慢速還短。
- 下載進度就是這個 GET 收到多少 bytes，🚫 不走 RPC。

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

- **body 加密**：`e_` 的 URL 之外，body 本身也用從 token 導出、每條連線不同的鑰加密（以最省 CPU 的 AEAD 分幀，有 AES-NI 用 AES-GCM、沒有用 ChaCha20-Poly1305），
  回應的 manifest 也加密。明文模式（`c_`）才送明文。
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
| URL 與 meta（`e_`／`c_`、別的 token 發的拒、被改過的拒、用途不對的拒、meta 配不上 URL 的拒、沒帶 meta 的 400）、Host 檢查 | ✅ | daemon `data_plane::tests`、`tests/data_plane.rs` |
| 續傳（固定大小再 PUT） | ✅（假 server） | core `a_sized_body_must_match_and_a_second_put_resumes` |
| 一般 Matrix 帳號的傳統上傳（§7） | ❌ 下一支 | |
| `media.open`、`GET /media`（§8） | ❌ | |
| UI 指定從第幾 byte 續傳 | ❌ | |
| 機密模式（§9） | ❌ 伏筆 | |

# 資料平面：媒體的 bytes 怎麼進出 daemon

```
http://127.0.0.1:<data port>
  PUT /upload/<token>    前端 → daemon：明文 bytes，daemon 邊收邊加密邊傳到 homeserver，傳完回 manifest
  GET /media/<token>     daemon → 前端：邊拉邊解密邊吐，支援 Range（還沒做，§8）
```

🚨 **媒體本身的 bytes 只走這裡**（維護者 2026-09-30 再確認）。RPC 只傳媒體**訊息**的 JSON：
`media.create` 拿 URL、`room.send_attachment` 送事件。🚫 bytes 不進 RPC，進度也不走 RPC。

為什麼是另一個 port、為什麼是 HTTP、為什麼不能給檔案路徑，在 /docs/design/rpc-specs/local-interface.md §1、§8；這份只定**怎麼用**。
method 的總表在 /docs/design/rpc-specs/rpc-spec.md §3.6、§3.3；這份是資料平面那幾支的權威，兩邊對不上時以這份為準。

## 0. 一句話

送一個附件是**兩步，都由 UI 發動**（維護者 2026-09-30 定）：

1. **HTTP**：`media.create` 拿一張 PUT 的 URL → `PUT` bytes → 回應是 manifest（裡面有 `mxc`）。
2. **RPC**：`room.send_attachment` 帶那個 `mxc` → daemon 組事件、送進房間。

🚫 daemon **不替 UI 發訊息**：上傳就是上傳，發不發、發到哪、什麼時候發，都是 UI 拿到 manifest 之後的決定。

```
UI                                daemon                                homeserver
 │ RPC media.create {room,name,size}│                                       │
 │─────────────────────────────────>│  Upload/Create（描述已加密）          │
 │                                  │──────────────────────────────────────>│
 │ {upload_id, mxc, url}            │<──────────────────────────────────────│
 │<─────────────────────────────────│                                       │
 │                                  │                                       │
 │ HTTP PUT /upload/<token> (bytes…)│  Upload/Chunk × N（每塊各自加密）     │
 │─────────────────────────────────>│──────────────────────────────────────>│
 │                                  │  Upload/Seal（最終描述）              │
 │ 200 manifest                     │──────────────────────────────────────>│
 │<─────────────────────────────────│                                       │
 │                                  │                                       │
 │ RPC room.send_attachment         │  Event/Send（加密房是密文）           │
 │ {room, mxc, room_devices}        │  meta.attachments = [mxc]  ← §6       │
 │─────────────────────────────────>│──────────────────────────────────────>│
 │ {event_id}                       │<──────────────────────────────────────│
 │<─────────────────────────────────│                                       │
```

⚠️ **一定要傳完才能送**：server 只認 `Seal` 過的媒體，宣告一個還在傳的 mxc 會被**整則拒送**（`attachment … is not media this server has`，
2026-09-30 對真 server 驗到的）。所以順序是固定的，🚫 不能並行。
⚠️ 上傳與送訊息是**兩件事**：各自失敗、各自重試。上傳失敗就沒有訊息；送訊息失敗，檔案在 server 上、manifest 在 UI 手上，重送就好。

## 1. 誰做什麼

| 誰 | 做什麼 | 程式 |
|---|---|---|
| UI | 叫 `media.create`、PUT bytes、看 PUT 送出去多少當進度、拿 manifest 的 `mxc` 叫 `room.send_attachment` | — |
| daemon 的 handle | 替上傳鑄 token、照帳號與 mxc 找傳完的上傳 | `crates/wbf-daemon/src/handle/media.rs`、`rooms.rs` |
| daemon 的資料平面 | capability 表、HTTP listener、狀態碼、把 body 交給 core | `crates/wbf-daemon/src/data_plane.rs` |
| core | 建檔（決定加不加密）、收 bytes 切塊加密上傳、送附件事件（加密房先 Megolm） | `crates/wbf-core/src/attachment_ops.rs` |
| sdk | `Upload/*` 的每一個請求、`FileCipher`、事件 content | `crates/wbf-sdk/src/upload.rs`、`chunk_crypto.rs`、`event_json.rs` |

core 不知道 HTTP：bytes 從一個 `AsyncRead` 進來。建好的上傳（`UploadState`，**含檔案金鑰**）與傳完的 manifest 由 daemon 記在記憶體裡、
每一步交回 core；core 每一步都**自己再核對**「這個上傳是不是這個帳號的」、「這份 manifest 是不是這個上傳的」，🚫 不靠 daemon 記得查。

## 2. 認證：每個資源一張 capability URL

```
http://127.0.0.1:<data port>/upload/<token>
```

- **token**：32 byte OS 亂數的小寫 hex（64 字元）。形狀不對的直接 404，🚫 不去查表。
- **綁單一資源**：一張 token 就是一個上傳。同一個上傳可以 PUT 多次（續傳、或傳完再問一次 manifest），但同時只能一個（§4.3）。
- **放在 path，不是 header**：有些媒體元件只吃 URL、不讓設 header（/docs/design/rpc-specs/local-interface.md §8）。
- **TTL 1 小時**（`media.create` 回 `expires_in: 3600`）。過期只擋**新的**請求；進行中的 PUT 不被過期打斷。
  ⚠️ `room.send_attachment` 也要在這一小時內叫：過期之後 daemon 不記得那個上傳，就組不出事件（§5）。
- **撤銷**：`account.del`／`account.destroy` 成功後，那個帳號的 token 全部作廢。帳號的 session 不在了，core 那邊本來也會拒。
- **只在記憶體**：🚫 不落地。daemon 重開，token 與上傳紀錄全部失效（檔案本身還在 server 上，但要 `media.create` 重傳一次）。
- 認不得的 token 與過期的 token 一律 **404**，🚫 不分辨兩者（那會變成探測工具）。
- **未解鎖一律 503**（不管 token 認不認得）：東西在，只是現在打不開（/docs/design/rpc-specs/local-interface.md §5）。

📎 daemon 沒開資料平面（單發命令、沒有 `-s`）時，`media.create` 回 **100**、🚫 不去 server 建檔：建了也沒有地方收 bytes。
`daemon.info`、`daemon.json`、ready 那行的 `data_port` 就是這個 port（啟動參數 `--data-port`，0 ＝ 隨機）。

## 3. 路徑

| 方法 | 路徑 | 狀況 |
|---|---|---|
| `PUT` | `/upload/<token>` | §4 |
| 其他方法 | `/upload/<token>` | **405**，`Allow: PUT` |
| `GET` | `/media/<token>` | 還沒做（§8），現在是 404 |
| 任何 | 其他路徑 | **404** |

錯誤的 body 是 JSON `{ "code": <int>, "msg": "<給人看的>" }`，`code` 跟 RPC 同一張表（/docs/design/rpc-specs/rpc-spec.md §5）。404 與 405 沒有 body。

## 4. 上傳

### 4.1 `media.create`

```jsonc
→ { "method": "media.create", "params": { "room": "!r:localhost", "name": "v.mkv", "size": 2147483648, "mimetype": "video/x-matroska" }, "id": 12 }
← { "code": 0, "msg": "ok", "id": 12, "result": {
      "upload_id": 77, "mxc": "mxc://localhost/000000000000004d",
      "url": "http://127.0.0.1:51235/upload/7c1b…", "expires_in": 3600 } }
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
- `mxc` 與 `upload_id` 是 server 的 `Create` 配的。`upload_id` 給 `upload.status`／`upload.abort` 用；**送訊息認的是 `mxc`**（§5）。

### 4.2 `PUT /upload/<token>`

| | |
|---|---|
| body | 明文 bytes，**一條連線送到底**。`Content-Length` 或 chunked transfer 都可以 |
| `Content-Length` | 有帶的話，固定大小的上傳必須**剛好等於** `size`，不然 **400**、🚫 不讀 body |
| 回 | **200**，body 是 manifest（/docs/design/rpc-specs/wbf-cli-spec.md §5）。**回應在 `Seal` 之後才到**，所以 PUT 的回應就是「傳完了」 |
| 進度 | 就是這個 PUT 送出去多少。🚫 不走 RPC |

⚠️ manifest **含檔案金鑰**：UI 要存就自己用私有權限存。送訊息🚫 不必把它交回來，帶 `mxc` 就好（§5）。

錯誤：

| 狀態碼 | 什麼時候 | body 的 `code` |
|---|---|---|
| **400** | body 比 `size` 短或長、`Content-Length` 對不上或不是數字、串流的 body 是空的、上傳不是這個帳號的 | 102（`Content-Length`）／1100 |
| **404** | token 認不得或過期；帳號已經登出（token 形同撤銷） | —／1012 |
| **409** | 這張 token 已經有一個 PUT 在收；或它是串流、上一次 PUT 斷了（§4.3） | 106 |
| **502** | homeserver 拒絕、連不上、逾時 | 1400／1300／1600 |
| **503** | vault 還沒解鎖 | 1001 |
| **500** | 其他（本機 IO 等） | |

⚠️ body 讀到一半斷了（UI 關掉連線），回應送不回去；daemon 那邊把這張 token 放回「沒在收」，下一個 PUT 🚫 不會拿到 409。

### 4.3 同一張 token 再 PUT 一次

| 上一次 | 這一次 |
|---|---|
| 還在收 | **409** |
| 固定大小、斷了 | **續傳**：UI 再送一次**整個** body；daemon 先問 server 收到第幾塊，收過的塊照讀（要算整檔 SHA-256）但🚫 不再送 |
| 串流、斷了 | **409**：server 不能續傳串流，這張 token 作廢，重新 `media.create` |
| 傳完了 | **200**，回同一份 manifest，🚫 不讀 body、不重傳（冪等：UI 沒收到回應時可以再問一次） |

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

### 4.5 一塊借一次上傳線

每送一塊才去連線池借一次 `Upload` 線（/docs/design/daemon/link-pool.md），🚫 不是整個檔握著它。
一條線一次一個命令；整個檔握著線的話，第二個檔的 `media.create` 就得等第一個傳完。
代價是同時有兩個檔在傳時，兩者的塊交錯送——server 那邊每個上傳各自照序號收，不受影響。

## 5. `room.send_attachment`

```jsonc
→ { "method": "room.send_attachment", "params": { "room": "!r:localhost", "mxc": "mxc://localhost/000000000000004d", "caption": "看這個",
    "room_devices": { "room_version": 81234, "members": { "@bob:localhost": "3-810b7c3be4" } } }, "id": 13 }
← { "code": 0, "msg": "ok", "id": 13, "result": { "event_id": "$e1", "mxc": "mxc://localhost/000000000000004d", "attachment_declared": true } }
```

| params | 必要？ | 說明 |
|---|---|---|
| `room` | 必要 | |
| `mxc` | 必要 | PUT 回的 manifest 裡那個 |
| `caption` | 選填 | 進事件的 `body` 與 `caption` |
| `room_devices` | 加密房必要 | 跟 `room.send_text` 同一份（/docs/design/keys/e2ee-rpc.md §3） |
| `txn_id` | 選填 | 重送用同一個 |
| `user`／`server` | 選填 | |

- daemon 照**這個帳號**（它的 server 與 mxid）與 `mxc` 找自己封存的那份上傳，拿它的區塊（含金鑰）組事件。
  🚫 金鑰不從 UI 收回來：UI 只講「哪一個」，daemon 用自己手上那份——UI 帶錯或被改過的區塊進不了事件。
  - 還沒傳完（PUT 沒回 200）→ **1100**，訊息說「PUT 回來之後再送」。
  - 找不到（過期、daemon 重開過、別的帳號的、從沒在這裡建過）→ **1100**。
  - 一定帶帳號找：兩台 server 的 `server_name` 一樣時 mxc 也會撞。
- core 再核對一次上傳是這個帳號的、manifest 是這個上傳的，然後問這一刻的房間加不加密，**區塊跟房間對不上就拒（1100）**：
  加密房配 `none` 的上傳（server 讀得到檔）、明文房配加密的上傳（金鑰會公開）。建檔到送出之間房間可能變了，所以兩頭都擋。
- 明文房：`Event/Send` 送明文事件。加密房：先分房間金鑰、Megolm 加密、帶 `room_version` 送（/docs/design/keys/e2ee-rpc.md §3）。
  事件 content 是 /docs/design/media/wbf-client-convention-for-chunk.md §5 的形狀，**檔案金鑰在區塊裡、區塊在密文裡**——這正是 Matrix 把附件金鑰放事件裡的做法。
- 被 1506 擋：回 **1401**，`data` 是 daemon 自動重拿的房間狀態（跟 `room.send_text` 一樣）。
  UI 用新的 `room_devices`、**同一個 `mxc`、同一個 `txn_id`** 重送；檔案🚫 不必重傳。
- 同一個 `mxc` 可以送進好幾個房（轉傳）：每則各自宣告一次，server 的計數各自 +1（§6）。

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
- ⚠️ 上傳完到送出之間，媒體靠保護期撐著；保護期是 server 的設定，client 🚫 不假設它多長。UI 拿到 manifest 就盡快送，
  🚫 不要先上傳一堆、晚點再慢慢寫訊息（何況 daemon 只記得一小時，§2）。

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

## 8. 讀：`GET /media/<token>`（還沒做）

`media.open` 給一張讀的 token，播放器／圖片元件直接拿 URL 發 `Range`。規格照舊（原本寫在 /docs/design/rpc-specs/rpc-spec.md §6.1，搬來這裡）：

| | |
|---|---|
| 來源 | `media.open` 的 `url` |
| 支援 | `Range: bytes=a-b`（單一 range）；沒 `Range` 就整檔 |
| 回 | `200`（整檔）／`206 Partial Content`（有 Range）；`Content-Type` 是 manifest 的 mimetype，沒有就 `application/octet-stream`；`Accept-Ranges: bytes`；`Content-Length` |
| `416` | Range 超出檔尾 |
| `404` | 認不得或過期的 token |
| `503` | 未解鎖 |
| `502` | 從 server 拉塊失敗。⚠️ 半途失敗時 HTTP 已經回 200 了，只能斷連線 |

- daemon 邊解密邊吐（快取裡有完整檔就讀媒體池的 64 KiB 段，沒有就逐塊向 server 拉），🚫 不整檔進記憶體。同一個 token 可以重複 GET（播放器 seek）。
- 🚨 **上游慢下來的時候：停止送 bytes，但連線開著**（維護者 2026-09-13 定）。🚫 不回空回應（UI 會以為傳完了）、🚫 不斷線（UI 會以為失敗了）；
  拿不到才斷，還在拿就等。🚫 逾時不要設得比 homeserver 的慢速還短。
- 下載進度就是這個 GET 收到多少 bytes，🚫 不走 RPC。

## 9. 明確不做的

- 🚫 daemon 替 UI 發訊息：上傳完就停在「回 manifest」，發不發是 UI 的事（§0）。
- 🚫 全域 token、🚫 header 認證：一個資源一張 URL（§2）。
- 🚫 把 bytes 放進 RPC（base64 也不要）：小東西的例外只有 /docs/design/rpc-specs/local-interface.md §8 說的縮圖那種，而且不是上傳。
- 🚫 給 UI 檔案路徑讀媒體池：池是加密的，給路徑不是給密文就是明文落地（/docs/design/rpc-specs/local-interface.md §8）。
- 🚫 `room.send_attachment` 從 UI 收區塊或金鑰：只收 `mxc`，daemon 用自己封存的那份（§5）。
- 🚫 token 落地、🚫 跨 daemon 重開保留上傳：重開就重傳（§2）。

## 10. 現況

| 項目 | 狀態 | 測試 |
|---|---|---|
| `media.create`、`PUT /upload`、`room.send_attachment`（wbf 帳號，明文房與加密房，固定大小與串流） | ✅ | core `attachment_ops::tests`、daemon `data_plane::tests` 與 `tests/data_plane.rs`、真 server `tests/real_server.rs` 的 `an_attachment_goes_over_the_data_plane_into_plain_and_encrypted_rooms` |
| 續傳（固定大小再 PUT） | ✅ | core `a_sized_body_must_match_and_a_second_put_resumes` |
| 一般 Matrix 帳號的傳統上傳（§7） | ❌ 下一支 | |
| `media.open`、`GET /media`（§8） | ❌ | |
| UI 指定從第幾 byte 續傳 | ❌ | |

# WBF client 約定規格書：分塊／串流媒體，client 之間怎麼讀

> 狀態：草案，2026-09-04，等維護者同意。
>
> server 端的規格書（wbfuwunel `docs/design/chunked-upload-spec.md`，以下稱「線上規格」）只講 byte 怎麼排、
> 打哪個端點、server 怎麼回。它刻意不講的東西 —— 每塊怎麼加密、`Create` 的 data 裝什麼、串流怎麼收尾、
> 房間事件怎麼放、seek 怎麼算 —— 由本文定。**server 不讀本文定義的任何內容**；兩個 client 只要都照本文，
> 就能互相解開。
>
> 本文與線上規格有出入時，線上規格管 byte 與 server 行為，本文管 client 之間的語意；兩邊都不該碰對方的範圍。
> 本文改了要升 `v`（§8）。

## 0. 一句話

明文切成固定大小的塊，一塊一個 pack 送。**加密與否由房間決定**（維護者 2026-09-04 定）：

| 房間 | 模式 | 塊的 data | `cipher` |
|---|---|---|---|
| 有 E2EE | 加密 | 每塊各自 AEAD 加密，每檔一把隨機金鑰，金鑰放事件區塊、由房間的 E2EE 保護 | `chacha20-poly1305` 或 `aes-256-gcm`，發送端每檔選一個（§3） |
| 沒 E2EE | 明文 | 就是明文塊，什麼都不加。client 發送前要警告並取得確認（§5.1） | `none` |

兩種模式的線上流程、塊切法、seek、串流完全一樣，差別只在塊的 data 有沒有加密、事件區塊有沒有 `key`。
server 上那份描述是事件區塊（不含 `key`）的副本；加密模式下它也是加密的。

## 1. 名詞

| 名詞 | 意思 |
|---|---|
| 明文塊 `pt_i` | 原檔第 `i` 塊，`i` 從 0 起；長度 = `chunk_size`，最後一塊 = `file_size − i × chunk_size` |
| 密文塊 `ct_i` | `pt_i` 加密後，長度 = `len(pt_i) + 16`；就是線上規格 `Chunk` 的 data |
| 描述 | 檔名、MIME、大小等；明文是 §4 的 JSON，加密後是線上規格 `Create`／`Seal` 的 data，`Info` 原樣還回 |
| 區塊 | 房間事件 `content` 裡 `org.wbftw.wbfuwunel.chunked` 那個物件（§5） |

命名是 `org.wbftw.wbfuwunel.<名字>`：`org.wbftw` 是組織（Matrix 的反向網域慣例，維護者 2026-09-04 定；`zooy.cc` 只是私人 server，
不拿來當名義），`wbfuwunel` 是專案，最後一段才是名字，與 `org.matrix.msc1767.text` 同款。

## 2. 每檔的參數

| 參數 | 怎麼來 | 放哪 |
|---|---|---|
| `key` | 加密模式：每檔一把，32 byte，CSPRNG。明文模式：**沒有** | **只放房間事件的區塊**。不進描述、不進任何 server 看得到的地方。🚫 永遠不出現在沒 E2EE 的房間事件裡 |
| `nonce_base` | 加密模式：每檔一個，8 byte，CSPRNG。明文模式：沒有 | 區塊與描述都有 |
| `chunk_size` | 依**明文大小**選，見下表；必須在線上規格允許的範圍（預設 4 KiB 到 16 MiB） | 區塊與描述都有；**必須等於** `Create` 的 `EncryptedFileInfo.chunk_size`（`Create` 送 0 讓 server 挑預設時，Ack 回的 `chunk_size` 才是真值，事件與描述要寫那個） |
| `file_size` | 明文總長 | 區塊一定有；描述在知道之後才有（串流上傳要到 `Seal`，§4、§6）。固定大小上傳時**必須等於** `EncryptedFileInfo.file_size` |

`chunk_size` 的選法（維護者 2026-09-04 定）：

| 明文大小 | `chunk_size` |
|---|---|
| < 50 MiB | 64 KiB |
| ≥ 50 MiB | 1 MiB |
| 串流（大小未知），行動網路或判斷不出 | 64 KiB |
| 串流（大小未知），Wi-Fi 或有線 | 1 MiB |

串流看的是線路不是大小（維護者 2026-09-04）：邊生成邊傳，塊小才不會為了湊滿一塊等太久、丟一塊也少重送；
Wi-Fi 或有線才值得用大塊。判斷不出線路類型就當行動網路，取小的那個。

只看大小、不看類型：`chunk_size` 與 `file_size` 都是 server 看得到的明文，依類型選就等於把類型寫在明文欄位上；
依大小選則沒有多洩任何東西，`file_size` 本來就在那裡。下載端不假設這張表：塊大小一律以區塊的 `chunk_size` 為準，
舊版或別的 client 用了不同的值也解得開。

同一把 `key` 加同一個 `nonce_base` 只能用在**一個**上傳。重傳同一個檔要重新產生兩者。
理由：AEAD 的 nonce 重用是致命的，而「同一個檔傳兩次」是最容易踩到的路。

## 3. 塊的 data

明文模式（`cipher: none`）：`data_i = pt_i`，沒有標籤、沒有 nonce，`Read` 回來的 `len` 必須剛好等於預期明文長度。以下是加密模式。

- 演算法二選一，發送端每檔決定，寫在 `cipher`（維護者 2026-09-04 定：讓 SDK 與 UI 能選）。兩個的金鑰、nonce、標籤長度一樣，nonce 與 AAD 的構造共用，下載端兩個都要支援：

| `cipher` | 演算法 | 什麼時候選 |
|---|---|---|
| `chacha20-poly1305` | ChaCha20-Poly1305，IETF 版（RFC 8439） | 沒有硬體 AES 的平台（多數手機的純軟體路徑）。線上規格 §7 的建議值 |
| `aes-256-gcm` | AES-256-GCM（NIST SP 800-38D） | 有硬體 AES（x86 AES-NI、ARMv8 Crypto Extensions）時比 ChaCha 快，且常見於硬體安全晶片 |

  SDK 的預設：偵測到硬體 AES 就 `aes-256-gcm`，否則 `chacha20-poly1305`；UI 可以覆蓋成固定一種。
  兩者都是 12 byte nonce、16 byte 標籤、32 byte 金鑰。
- `nonce_i = nonce_base ‖ u32_be(i)`，12 byte。
- `aad_i = "wbf-chunk-v1"`（ASCII，12 byte，不含結尾 0）。用途是把塊密文與描述密文（§4）的網域分開；塊索引已經在 nonce 裡，不重複放。
- `ct_i = AEAD(key, nonce_i, aad_i, pt_i)`，`AEAD` 是 `cipher` 指定的那個；標籤附在密文後（crate `chacha20poly1305` 與 `aes-gcm` 的預設）。

塊索引 `i` 的上限是 `0xFFFF_FFFD`：`0xFFFF_FFFF` 與 `0xFFFF_FFFE` 保留給描述（`Create` 與 `Seal` 各一個，§4）。線上規格的 `chunk_count` 是 u32，
實際上單檔上限（預設 10 GiB）遠早於此。

### 3.1 下載端必須做的檢查（全部 fail closed，任一不過就整個檔視為壞的，不顯示部分內容）

1. 區塊的 `v` 認得（§8），`cipher` 是 `chacha20-poly1305`、`aes-256-gcm` 或 `none`。加密模式必須有 `key` 與 `nonce_base`，明文模式必須沒有 `key`。
2. `chunk_size`、`file_size` 與 `Info` 回的 `chunk_size`、`chunk_count` 一致：`chunk_count == ceil(file_size / chunk_size)`。
   不一致代表事件與 server 上的東西對不上，拒絕。
3. 每塊 `Read` 回來的 `len` 必須等於預期長度：加密模式 `預期明文長度 + 16`，明文模式 `預期明文長度`（預期明文長度：非最後一塊 = `chunk_size`；最後一塊 = `file_size − i × chunk_size`）。
   線上規格允許密文長度不固定，是為了讓 server 不用懂加密；本文把它定死，所以 client 端一樣要驗。
4. 加密模式：AEAD 標籤驗證失敗 → 拒絕。
5. 全檔下載完（不是 seek 部分讀取）且描述有 `sha256` 時，整檔明文 SHA-256 要對得上。

## 4. 描述：`Create`／`Seal` 的 data

**兩個都必帶、都不空**（維護者 2026-09-04 定）。它讓「有 `key` 但沒有房間事件」的人也能從 `Info` 重建參數，
這是分享連結（mxc 加 `key`，不進房間就能看）的基礎；data 送空就沒有分享。正常的房間下載路徑只用 §5 的區塊，不讀它。

內容是 JSON（UTF-8，不限鍵序）。加密模式用檔案金鑰加密後才放進 data（本節尾）；明文模式**直接放 JSON**，不加密，因為沒有金鑰：

```json
{ "v": 1, "name": "video.mkv", "mimetype": "video/x-matroska",
  "file_size": 132056, "chunk_size": 65536, "nonce_base": "AAECAwQFBgc=", "sha256": "<hex, 選用>" }
```

| key | 必要？ | example | 備註 |
|---|---|---|---|
| `v` | 必要 | `1` | 本文版本（§8）。不認得就拒絕 |
| `chunk_size` | 必要 | `65536` | 明文塊大小，byte。`Create` 就定死，串流也知道。必須等於 `Create` Ack 回的 `chunk_size` |
| `cipher` | 必要 | `"chacha20-poly1305"` | `"chacha20-poly1305"`、`"aes-256-gcm"`、`"none"` 三選一。與事件區塊一致 |
| `nonce_base` | 加密模式必要，明文模式不放 | `"AAECAwQFBgc="` | 8 byte，base64（RFC 4648 標準字母表、帶 `=`）。每檔一個 |
| `file_size` | 選用 | `132056` | 明文總長，byte。**缺 = 還不知道**（串流上傳 `Create` 時），`Seal` 那份必須有。🚫 不要寫 0 當佔位，0 會被讀成空檔 |
| `name` | 選用 | `"video.mkv"` | 缺 = 不知道，顯示成 `unknown`；不要用空字串當佔位 |
| `mimetype` | 選用 | `"video/x-matroska"` | 同上 |
| `sha256` | 選用 | `"9f86d0…"` | 整檔明文 SHA-256，十六進位小寫。缺 = 沒算或還不知道。有就要驗（§3.1 第 5 條） |

- 欄位與 §5 房間事件的區塊一模一樣，**只少 `key`**。兩份不一致時以事件為準；下載端可以拿描述交叉核對，不一致就拒絕。
- 塊數不寫：從 `file_size` 與 `chunk_size` 算得出來，server 的 `Info` 也會回。
- `Seal` 帶的描述是最終版，整份覆蓋 `Create` 那份（線上規格 §3.4）。`Seal` 是約定：固定大小與串流都一樣要帶，一條規則。
  沒 `Seal` 的上傳不會留下半成品：server 在 `media_upload_ttl` 內沒收到新塊就整個清掉（線上規格 §3.5），所以描述不會因為少一次 `Seal` 而漂移。
- 不認得的 key 忽略（與 Matrix 事件同一規則），本文新增選用欄位不用升 `v`。

加密模式的描述加密：同一把 `key`、`aad = "wbf-desc-v1"`，nonce 用保留的塊索引，`Create` 與 `Seal` 各一個，因為兩份內容不同，不能共用 nonce：

| 哪一份 | nonce |
|---|---|
| `Create` 的 data | `nonce_base ‖ 0xFF_FF_FF_FF` |
| `Seal` 的 data | `nonce_base ‖ 0xFF_FF_FF_FE` |

## 5. 房間事件

用 `m.room.message` 加**自訂 msgtype**，讓不認識的 client 顯示 `body` 的一行文字，而不是去下載一個解不開的檔。
不用 `m.file`：規格的 `file` 欄位語意是 AES-256-CTR 加 SHA-256（`v: "v2"`、`iv`、`hashes` 必填），
放 ChaCha20 的參數進去是在說謊。也不用自訂事件型別：那樣舊 client 整則隱形，使用者不知道有東西。

```json
{
  "type": "m.room.message",
  "content": {
    "msgtype": "org.wbftw.wbfuwunel.file",
    "body": "video.mkv（WBF 分塊檔，需要 WBF client 才能開）",
    "url": "mxc://example.org/1122334455667788",
    "org.wbftw.wbfuwunel.chunked": {
      "v": 1,
      "cipher": "chacha20-poly1305",
      "key": "<base64, 32 bytes>",
      "nonce_base": "<base64, 8 bytes>",
      "chunk_size": 65536,
      "file_size": 132056,
      "name": "video.mkv",
      "mimetype": "video/x-matroska",
      "sha256": "<hex, 選用>"
    }
  }
}
```

`content` 的欄位：

| key | 必要？ | example | 備註 |
|---|---|---|---|
| `msgtype` | 必要 | `"org.wbftw.wbfuwunel.file"` | 舊 client 不認得就顯示 `body` |
| `body` | 必要 | `"video.mkv（WBF 分塊檔，需要 WBF client 才能開）"` | 給舊 client 看的一行字。不當權威，檔名以區塊的 `name` 為準 |
| `url` | 必要 | `"mxc://example.org/1122334455667788"` | `Create` Ack 回的 `mxc`（media id = 上傳 id 的 16 位小寫 hex） |
| `org.wbftw.wbfuwunel.chunked` | 必要 | 見下表 | 區塊。缺就當解不開的檔 |

區塊 `org.wbftw.wbfuwunel.chunked` 的欄位：§4 描述的每一個 key（同樣的必要／選用規則，但 `file_size` 在事件裡**必要**，事件在 `Seal` 之後才送，一定知道），再加：

| key | 必要？ | example | 備註 |
|---|---|---|---|
| `cipher` | 必要 | `"chacha20-poly1305"` | `"chacha20-poly1305"`、`"aes-256-gcm"`、`"none"` 三選一。其他值拒絕 |
| `key` | 加密模式必要，明文模式**必須沒有** | `"<base64, 32 bytes>"` | 每檔一把。**只在這裡**，不進描述、不進 server 看得到的地方 |

- 認得 `msgtype` 但 `org.wbftw.wbfuwunel.chunked` 缺、`v` 不認得、`cipher` 不認得、或 §3.1 第 1 條不過 → 當成解不開的檔，顯示 `body`，不下載。

### 5.1 模式由房間決定（維護者 2026-09-04 定：不擋，但要警告）

| 房間 | 發送端 | 接收端 |
|---|---|---|
| 有 E2EE | 用加密模式，`cipher` 依 §3 的表選。`key` 靠 Megolm 保護，這正是 Matrix 把附件金鑰放事件裡的做法 | 照 §3 解 |
| 沒 E2EE | **先警告、取得確認**才送，用明文模式。警告要講清楚：這個房間沒有加密，檔案會以明文存在 server 上、房間裡每個人與 server 都看得到 | 照明文模式讀 |

- 🚫 發送端**永遠不**在沒 E2EE 的房間送 `cipher: chacha20-poly1305`：事件是明文，`key` 就公開了，加密白做。這是 client 的 bug，不是使用者的選擇。
- 接收端在沒 E2EE 的房間收到帶 `key` 的區塊，仍然解得開，但要標示「金鑰已公開」，不要裝作它是加密的。
- 縮圖：沒有（密文做不出來），事件不帶 `info.thumbnail_*`。
- `m.video`／`m.audio` 的 `info.duration` 這類明文元資料，v1 不放；要放就放進區塊，之後升 `v`。

## 6. 串流上傳（大小未知）

1. `Create`：`EncryptedFileInfo { file_size: 0, chunk_size: N, chunk_count: 0 }`（線上規格的串流哨兵），
   data = §4 的描述加密，**不含 `file_size`、不含 `sha256`**（不知道就不寫，§4）。此時描述只有 `v`、`chunk_size`、`nonce_base` 與知道的 `name`／`mimetype`。
2. 每讀滿 `chunk_size` 明文就送一塊，索引遞增；最後一塊（可以不滿）帶 `IS_LAST`。加密方式與 §3 完全相同，沒有特例。
3. `Seal` 帶重新加密的完整描述（§4，一律必帶），`file_size` 是真值、`sha256` 有算就放。server 拿它整份覆蓋 `Create` 那份。
4. 房間事件在 `Seal` 之後才送，區塊寫最終的 `file_size`。

下載端看不出一個檔是不是串流傳的，也不需要：`Info` 的 `file_size` 在串流上傳會是 `null`（server 不知道），
此時 §3.1 第 2 條用區塊的 `file_size` 與 `Info` 的 `chunk_count`、`chunk_size` 核對。
`Info` 還回的描述如果缺 `file_size`，代表上傳者沒照第 3 步做；區塊有 `file_size` 就照區塊，描述只是副本。

## 7. Seek（`play --at pos`）

```
i   = pos / chunk_size            （整數除法）
off = pos − i × chunk_size
Read(mxc, chunk=i) → ct_i → 解密 → pt_i[off..]
```

只需要那一塊。這是整個設計的核心驗收：大於 1 GiB 的檔中途 seek 不必下載前面。

## 8. 版本

- 區塊與描述都有 `v`，目前 `1`。
- 不認得的 `v` 一律拒絕（顯示 `body`），不要猜。
- 本文任何會讓舊 client 解錯的改動（演算法、nonce 構造、AAD、欄位語意）都要升 `v`；只加選用欄位不用。

## 9. 測試向量（第 2 步產生）

本文的可執行版本：`docs/design/wbf-client-vectors.json`，由 `wbf-sdk` 的實作產生，內容：固定 `key`、`nonce_base`、一個小檔的
每塊 `ct_i`、描述密文、對應的房間事件區塊。`wbf-sdk` 每次測試對著它跑，任何其他語言的 client 也能拿去驗。
現在還沒有；做 wbf-sdk 時一起產生，並在本節填入路徑。

## 10. 明確不做的

- 邊上傳邊看（server 還沒有推送）。
- 多把金鑰／金鑰輪替：一檔一把，換就重傳。
- 與 Matrix 標準附件（`m.file` 加 `file`）相容：線上規格 §10 說只有單塊可能相容，v1 不做。

## 11. 要維護者決定的

1. ~~命名空間~~ 定了：`org.wbftw`（維護者 2026-09-04）。
2. ~~E2EE 硬擋~~ 定了：不擋，沒 E2EE 的房間用明文模式，發送前警告並確認（§5.1，維護者 2026-09-04）。
3. ~~`chunk_size`~~ 定了：依大小，50 MiB 以下 64 KiB、以上 1 MiB；串流依線路，行動網路 64 KiB、Wi-Fi 1 MiB（§2，維護者 2026-09-04）。
4. ~~`cipher`~~ 定了：`chacha20-poly1305` 與 `aes-256-gcm` 二選一，SDK 依硬體預設、UI 可覆蓋（§3，維護者 2026-09-04）。

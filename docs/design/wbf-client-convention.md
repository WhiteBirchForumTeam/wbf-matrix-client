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

每個檔一把隨機金鑰；明文切成固定大小的塊，每塊用 ChaCha20-Poly1305 各自加密，nonce 由檔案的 `nonce_base` 加塊索引組成；
金鑰與參數放在房間事件裡我們自己命名空間的區塊，由房間的 E2EE 保護；server 上那份加密描述是同一組參數（不含金鑰）的副本。

## 1. 名詞

| 名詞 | 意思 |
|---|---|
| 明文塊 `pt_i` | 原檔第 `i` 塊，`i` 從 0 起；長度 = `chunk_size`，最後一塊 = `file_size − i × chunk_size` |
| 密文塊 `ct_i` | `pt_i` 加密後，長度 = `len(pt_i) + 16`；就是線上規格 `Chunk` 的 data |
| 描述 | 檔名、MIME、大小等；明文是 §4 的 JSON，加密後是線上規格 `Create`／`Seal` 的 data，`Info` 原樣還回 |
| 區塊 | 房間事件 `content` 裡 `org.wbftw.chunked` 那個物件（§5） |

命名空間 `org.wbftw` 照 Matrix 規格的反向網域慣例，對應組織 wbftw（維護者 2026-09-04 定：這個 repo 是組織的，
`zooy.cc` 只是私人 server，不拿來當名義）。

## 2. 每檔的參數

| 參數 | 怎麼來 | 放哪 |
|---|---|---|
| `key` | 每檔一把，32 byte，CSPRNG | **只放房間事件的區塊**。不進描述、不進任何 server 看得到的地方 |
| `nonce_base` | 每檔一個，8 byte，CSPRNG | 區塊與描述都有 |
| `chunk_size` | 上傳者定，必須在線上規格允許的範圍（預設 4 KiB 到 16 MiB）；建議 64 KiB（一般）或 1 MiB（影片） | 區塊與描述都有；**必須等於** `Create` 的 `EncryptedFileInfo.chunk_size`（`Create` 送 0 讓 server 挑預設時，Ack 回的 `chunk_size` 才是真值，事件與描述要寫那個） |
| `file_size` | 明文總長 | 區塊一定有；描述在知道之後才有（串流上傳要到 `Seal`，§4、§6）。固定大小上傳時**必須等於** `EncryptedFileInfo.file_size` |

同一把 `key` 加同一個 `nonce_base` 只能用在**一個**上傳。重傳同一個檔要重新產生兩者。
理由：AEAD 的 nonce 重用是致命的，而「同一個檔傳兩次」是最容易踩到的路。

## 3. 塊加密

- 演算法：**ChaCha20-Poly1305**（IETF 版，RFC 8439：12 byte nonce、16 byte 標籤）。線上規格 §7 的建議，這裡定死。
- `nonce_i = nonce_base ‖ u32_be(i)`，12 byte。
- `aad_i = "wbf-chunk-v1"`（ASCII，12 byte，不含結尾 0）。用途是把塊密文與描述密文（§4）的網域分開；塊索引已經在 nonce 裡，不重複放。
- `ct_i = ChaCha20Poly1305(key, nonce_i, aad_i, pt_i)`，標籤附在密文後（crate `chacha20poly1305` 的預設）。

塊索引 `i` 的上限是 `0xFFFF_FFFD`：`0xFFFF_FFFF` 與 `0xFFFF_FFFE` 保留給描述（`Create` 與 `Seal` 各一個，§4）。線上規格的 `chunk_count` 是 u32，
實際上單檔上限（預設 10 GiB）遠早於此。

### 3.1 解密端必須做的檢查（全部 fail closed，任一不過就整個檔視為壞的，不顯示部分內容）

1. 區塊的 `v` 認得（§8）。
2. `chunk_size`、`file_size` 與 `Info` 回的 `chunk_size`、`chunk_count` 一致：`chunk_count == ceil(file_size / chunk_size)`。
   不一致代表事件與 server 上的東西對不上，拒絕。
3. 每塊 `Read` 回來的 `len` 必須等於 `預期明文長度 + 16`（預期明文長度：非最後一塊 = `chunk_size`；最後一塊 = `file_size − i × chunk_size`）。
   線上規格允許密文長度不固定，是為了讓 server 不用懂加密；本文把它定死，所以 client 端一樣要驗。
4. AEAD 標籤驗證失敗 → 拒絕。
5. 全檔下載完（不是 seek 部分讀取）且描述有 `sha256` 時，整檔明文 SHA-256 要對得上。

## 4. 描述：`Create`／`Seal` 的 data

明文是 JSON（UTF-8，不限鍵序）：

```json
{ "v": 1, "name": "video.mkv", "mimetype": "video/x-matroska",
  "file_size": 132056, "chunk_size": 65536, "nonce_base": "AAECAwQFBgc=", "sha256": "<hex, 選用>" }
```

- 欄位與 §5 的區塊一模一樣，**只少 `key`**。兩份不一致時以事件為準；下載端可以拿描述交叉核對，不一致就拒絕（§3.1 第 2 條同一精神）。
- `file_size` 與 `sha256` **可缺**，缺 = 還不知道（串流上傳在 `Create` 時就是這樣，§6）。**不要寫 0 當佔位**：0 會被讀成「空檔」。
  `Seal` 帶的描述才是最終版，會整份覆蓋 `Create` 那份（線上規格 §3.4）；上傳者在 `Seal` 時知道多少就寫多少。
- `chunk_size` 一定有：它是 `Create` 就定死的參數，串流也一樣。塊數不寫在描述裡，它從 `file_size` 算得出來，server 的 `Info` 也會回。
- `name`、`mimetype` 可缺；缺的顯示成 `unknown`，不要用空字串當佔位。
- `sha256` 選用；有就是整檔明文的十六進位小寫。
- base64 一律 RFC 4648 標準字母表、**帶** `=` padding。

加密：同一把 `key`，`nonce_desc = nonce_base ‖ 0xFF_FF_FF_FF`，`aad = "wbf-desc-v1"`。密文放 `Create` 的 data；`Seal` 再帶一次就用同一個 nonce 與 AAD 重新加密（內容不同、nonce 相同，對 AEAD 是 nonce 重用）—— 所以**`Seal` 那份改用 `0xFF_FF_FF_FE`**：`Create` 用 `…FF_FF_FF_FF`，`Seal` 用 `…FF_FF_FF_FE`，塊索引上限因此是 `0xFFFF_FFFD`。

為什麼描述還要存一份在 server：串流上傳（§6）在 `Create` 時不知道 `file_size` 與 `sha256`，`Seal` 才補得齊；
而且拿到金鑰的人即使房間事件被撤回，仍能從 `Info` 重建參數。**它是副本，不是權威。**

## 5. 房間事件

用 `m.room.message` 加**自訂 msgtype**，讓不認識的 client 顯示 `body` 的一行文字，而不是去下載一個解不開的檔。
不用 `m.file`：規格的 `file` 欄位語意是 AES-256-CTR 加 SHA-256（`v: "v2"`、`iv`、`hashes` 必填），
放 ChaCha20 的參數進去是在說謊。也不用自訂事件型別：那樣舊 client 整則隱形，使用者不知道有東西。

```json
{
  "type": "m.room.message",
  "content": {
    "msgtype": "org.wbftw.file",
    "body": "video.mkv（WBF 分塊檔，需要 WBF client 才能開）",
    "url": "mxc://example.org/1122334455667788",
    "org.wbftw.chunked": {
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

- `url` 是 `Create` Ack 回的 `mxc`（media id = 上傳 id 的 16 位小寫 hex）。
- `body` 給舊 client 看；內容不當權威（檔名以區塊的 `name` 為準）。
- **只在 E2EE 房間送**。`key` 靠 Megolm 保護，這正是 Matrix 把附件金鑰放事件裡的做法；非加密房間裡送這個事件等於把金鑰公開，client 必須拒送。
- 認得 `msgtype` 但 `org.wbftw.chunked` 缺、`v` 不認得、`cipher` 不是 `chacha20-poly1305` → 當成解不開的檔，顯示 `body`，不下載。
- 縮圖：沒有（密文做不出來），事件不帶 `info.thumbnail_*`。
- `m.video`／`m.audio` 的 `info.duration` 這類明文元資料，v1 不放；要放就放進區塊，之後升 `v`。

## 6. 串流上傳（大小未知）

1. `Create`：`EncryptedFileInfo { file_size: 0, chunk_size: N, chunk_count: 0 }`（線上規格的串流哨兵），
   data = §4 的描述加密，**不含 `file_size`、不含 `sha256`**（不知道就不寫，§4）。此時描述只有 `v`、`chunk_size`、`nonce_base` 與知道的 `name`／`mimetype`。
2. 每讀滿 `chunk_size` 明文就送一塊，索引遞增；最後一塊（可以不滿）帶 `IS_LAST`。加密方式與 §3 完全相同，沒有特例。
3. **`Seal` 必須帶 data**：重新加密的完整描述，`file_size` 是真值、`sha256` 有算就放。server 拿它整份覆蓋 `Create` 那份。
   串流上傳 `Seal` 不帶 data 是 client 的 bug：server 上會留一份沒有大小的描述。固定大小上傳的 `Seal` 帶不帶都可以。
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
2. 「只在 E2EE 房間送」硬擋（§5）可以嗎。
3. `chunk_size` 的預設值：一般檔 64 KiB、影片 1 MiB（§2），還是全部一種。

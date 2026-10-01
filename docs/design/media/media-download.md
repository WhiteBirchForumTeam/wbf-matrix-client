# 媒體下載：佇列、主檔、seek 暫存檔（設計，維護者 2026-10-01 定）

> **狀態：設計，程式還沒做。** 這份是下一支 PR 的規格。daemon ↔ homeserver 的順序下載**現在已經有**（`wbf_sdk::media::fetch`，
> `media.save_to` 用的就是它，真 server 驗過）；這份把它改成「佇列 ＋ 主檔 ＋ seek 暫存檔」，並接上資料平面的讀（`media.open`、`GET /media`）。
>
> 相關：媒體池（本地加密的儲存）在 /docs/design/media/media-pool.md；資料平面的 URL、Host 檢查、狀態碼在 /docs/design/rpc-specs/data-plane.md；
> 塊怎麼加密、下載端要做哪些檢查在 /docs/design/media/wbf-client-convention-for-chunk.md §3；server 的 `Download/*` 在 wbfuwunel 的 /docs/design/media/chunked-upload-spec.md §4。

## 0. 一句話

**一律順序寫。** 每個帳號一條下載佇列，一次只順序拉一個檔，寫進池裡的**主檔**；播放器 seek 到還沒拉到的地方時，
在同一條 `Download` 線上**插隊**拉那幾塊，順序 append 進旁邊的 **seek 暫存檔**，用一張 O(1) 的位置表記「第幾塊放在暫存檔的第幾格」。
主檔順序拉到那一塊時先查表，有就從暫存檔搬、不走網路。主檔完成就把暫存檔與位置表一起刪掉。最差狀況是同時佔兩份空間。

為什麼不用「一開始建出完整大小的檔、塊到哪寫哪」（BitTorrent 的做法）：那等於隨機寫——HDD 要來回尋道，Windows 不是稀疏檔時還會先補零
（同一個檔寫兩遍），而這兩件事維護者都不要（2026-10-01）。這份的做法讓**每一個檔都只被順序 append**。

## 1. 名詞

| 名詞 | 意思 |
|---|---|
| **塊（chunk）** | server 上的傳輸單位，大小是事件區塊的 `chunk_size`（4 KiB～16 MiB，常見 64 KiB 或 1 MiB）。每塊各自用**那個檔的金鑰** AEAD 加密 |
| **段（segment）** | 媒體池的儲存單位，固定 64 KiB 明文。每段各自用**池金鑰** AEAD 加密。跟塊大小無關，也不必對齊 |
| **主檔** | 池裡這個 mxc 的檔，從第 0 段開始順序 append。完成前叫 `m<media.id>`、在 `pending/`；完成後改名成明文的 BLAKE3 |
| **seek 暫存檔** | 這個 mxc 被 seek 拉到的塊，照到達順序 append。一筆一個塊、每筆固定大小。主檔完成就刪 |
| **位置表** | 塊號 → 它在 seek 暫存檔的第幾格。記憶體裡的陣列，O(1) |
| **mxc 的狀態** | 一個 mxc 在 daemon 裡的東西：manifest、驗過的參數、主檔寫到哪、暫存檔與位置表、正在拉哪幾塊。同一個 mxc 只有一份；job 與 seek 都用它 |
| **job** | 「把這個 mxc 順序拉完」這件事：`{ mxc, next, total, … }`（§5.2）。一塊一步 |
| **downloading 表** | 每帳號一張 `HashMap<mxc, Arc<Downloading>>`：正在拉的檔，裡面有取消旗標與進度；開始拉時放進去、拉完或取消時自己拿掉（§5.2） |
| **下載佇列** | 每個帳號一條，排著 job，**一次一個** |
| **下載 worker** | 每個帳號一個 task，只有它用那個帳號的 `Download` 線拉檔；有 seek 與 job 兩個收件匣，seek 先（§5.1） |
| **`Download` 線** | 連線池裡每個帳號五條 WS 線之一（/docs/design/daemon/link-pool.md），專跑 `Download/*`（kind `0x04`） |

## 2. 全景

```
              homeserver
                 │  WS「Download」線（每帳號一條）：Info、Read{mxc, chunk}
                 ▼
   ┌───────── daemon ───────────────────────────────────────────────────────────┐
   │  下載佇列（每帳號一條，一次一個檔）        seek（GET 要的塊還沒有）            │
   │      │ 每塊借一次線                           │ 插隊借線（優先）              │
   │      ▼                                        ▼                              │
   │  Read 塊 i → 驗長度、AEAD 解開（檔案金鑰）→ 明文在記憶體（一塊）              │
   │      │                                        │                              │
   │      ▼ 順序                                   ▼ 順序                         │
   │  主檔 pending/m<id>  ◀── 先查位置表 ──  seek 暫存檔 pending/m<id>.seek        │
   │  （池格式 v2、池金鑰）    有就搬、不走網路   （固定大小的格、池金鑰）          │
   │      │ 完成：BLAKE3 → 改名 <hh>/<hash>、cache.db 記完成、刪暫存檔               │
   │      ▼                                                                       │
   │  GET /media/mxc/e-…（資料平面）：原檔 → 完整主檔 → 主檔已寫的段 → 暫存檔 → 現拉 │
   └──────────────────────────────────────────────────────────────────────────────┘
                 │  HTTP（明文 bytes，Range）
                 ▼
              UI／播放器
```

## 3. 從 homeserver 到記憶體

### 3.1 走哪條線

**一律 `Download` 線**（WS，kind `0x04`）。🚫 不走 `Misc`：`Misc` 送訊息、拉訊息窗、查房間狀態，一塊可能 16 MiB，塞在那條上會卡住送訊息。

連線池裡一條線一次只跑一個命令（/docs/design/daemon/link-pool.md §5）。下載與 seek 都是**每塊借一次線**，借到就送一個 `Read`、等到它的回應就還，
不是整個檔握著線（現在的 `media::fetch` 是整個檔握著，要改）。這樣 seek 才插得進來（§6.2）。

### 3.2 開始一個檔：`Info`，一個 mxc 只做一次

```
Download/Info { mxc } → Ack { file_size, chunk_size, chunk_count, ... }，data = Create 時那份加密描述
```

照 /docs/design/media/wbf-client-convention-for-chunk.md §3.1 第 1、2 條與 §4（現有的 `WbfClient::verify_target`）：

- 區塊認得（`v`、`cipher`、加密模式有 `key`／`nonce_base`）；
- `chunk_size`、`chunk_count` 與 `Info` 一致，`chunk_count == ceil(file_size / chunk_size)`；
- 描述用檔案金鑰解得開，內容跟區塊對得上。

任一條不過 → 這個 mxc 整個當壞檔（fail closed），佇列移除它、GET 回 502，🚫 不吐任何部分內容。結果（`VerifiedTarget`：檔案金鑰、`file_size`、`chunk_count`）
存在這個 mxc 的狀態裡，之後每一塊（job 與 seek 都是）都用它，🚫 不重問。

**金鑰從哪來**：manifest（`media.open` 帶的，或 daemon 從 `cache.db` 找：`event_media` → 引用這個 mxc 的事件 → `content_json` 裡的區塊）。
找不到任何區塊 → 沒有金鑰 → `media.open` 回 1100。

### 3.3 一塊：`Read`

```
Download/Read { mxc, chunk: i } → Ack { chunk: i, len, ... }，data = 第 i 塊密文
```

1. **長度**：密文長度必須剛好是「預期明文長度 ＋ 16」（明文模式是預期明文長度）。預期明文長度：非最後一塊 ＝ `chunk_size`；最後一塊 ＝ `file_size − i × chunk_size`。
2. **AEAD**：`nonce = nonce_base ‖ u32_be(i)`、`aad = "wbf-chunk-v1"`，用檔案金鑰解。標籤不對 → 這一塊壞了。
3. 現有的 `read_and_open_chunk` 就是這兩步。

**一塊壞了**：重拉一次（傳輸錯）；還是壞 → 整個檔當壞檔，佇列移除、正在讀它的 GET 斷線。🚫 不跳過、🚫 不吐。

### 3.4 記憶體

- 一次只有**一塊**明文在記憶體（≤ `chunk_size`），解開 → 交給下一步寫 → 釋放。明文 buffer 用 `Zeroizing`，丟掉時清零。
- 佇列與 seek 是兩條路，所以最多同時兩塊；同一塊被好幾個 GET 同時要只拉一次（§6.3）。
- 🚫 不在記憶體裡累積「先收著之後再寫」：寫不進去就讓下一個 `Read` 等，這就是背壓（同 /docs/design/rpc-specs/data-plane.md §4.5 的上傳）。

## 4. 從記憶體落地

### 4.1 主檔：池格式 v2（每段寫滿、長度另記）

維護者 2026-10-01：**每一段都寫正好的尺寸**——1 byte 的檔也寫滿 64 KiB；真實長度記在段裡。好處是檔案大小永遠可以驗算：
不是「檔頭 ＋ 段大小的整數倍」就一定壞了（寫到一半），直接截掉最後那段不完整的。

⚠️ 這跟現在的池格式 v1（/docs/design/media/media-pool.md §8）不一樣：**v1 沒有每段的長度欄位**，長度從檔案總長推，最後一段可以短；
而且有「暫定段」（每 1.5 秒把湊不滿的尾巴先封起來、之後截掉重寫）。v2 拿掉這兩件事：

```
偏移   長度   內容
0      4      magic "WBFP"
4      1      version = 2
5      3      保留，全 0
8      4      segment_size（u32 LE）＝ 65536
12     16     nonce_base，這個檔隨機
28     4      保留，全 0
32     …      段 0、段 1、…

R = 4 + segment_size + 16                                    每段在磁碟上的固定大小（65556）
第 i 段的位置 = 32 + i × R
第 i 段 = XChaCha20-Poly1305(
            key   = 池金鑰（第四把子金鑰 "wbf-matrix-client media store v1"）
            nonce = nonce_base(16) ‖ u64_le(i)
            aad   = "wbf-media-pool v2" ‖ nonce_base ‖ u64_le(i)
            明文  = u32_le(len) ‖ data(len) ‖ 0 × (segment_size − len) )
規則：除了最後一段 len == segment_size；最後一段 1 ≤ len ≤ segment_size；沒有零段的檔（協議沒有零塊的上傳）
```

- **長度在密文裡**：被 AEAD 保護，改一個 bit 就解不開。
- **檔長驗算**：`(檔長 − 32) % R ≠ 0` → 最後一段寫到一半，截到 `32 + ⌊(檔長 − 32) / R⌋ × R`。
- **明文總長** ＝ `(段數 − 1) × segment_size ＋ 最後一段的 len`，跟 `media.file_size` 交叉核對。
- **每段只寫一次**：nonce 由段號決定，一個段號只封一份明文。續傳時重寫同一段，明文一定一樣（就是檔案內容），所以密文也一樣、沒有 nonce 重用。
- **代價**：每段多 4 byte 長度欄（＋16 byte 標籤，v1 就有）；最後一段補滿，1 byte 的檔在磁碟上佔 65,588 byte。配額算的是 `bytes_on_disk`，照實算。

**寫入（主檔）**：

1. 明文丟進段大小的緩衝；**湊滿一段才封、才 append**（塊大小不必是 64 KiB 的倍數，塊的明文可以跨段）。
2. 同時順序餵 BLAKE3（池檔的檔名）與 SHA-256（區塊有 `sha256` 時，§3.1 第 5 條的整檔核對）。
3. 最後一塊寫完：封最後一段（補滿、記真實長度）、fsync、得出 BLAKE3。
4. **進度**：**檔案本身就是進度**——完整的段數就是寫到哪。每 1.5 秒 fsync 一次（限制斷電時最多丟多少），並把段數寫回 `cache.db`（`media.chunks_written` 改記**段數**，給 `media.info`、`media.queue` 顯示用；🚫 不當續傳的依據）。

**續傳（daemon 重開、或被取消後重新排進佇列）**：

1. 開主檔，驗檔頭（magic、version、segment_size）。**v1 一律當壞檔、刪掉重下**（維護者 2026-10-01：它是快取），完成檔與暫存檔都一樣（§9）。
2. 檔長驗算，不是整數倍就截掉最後那段不完整的。
3. **從第 0 段起逐段解開**：餵回 BLAKE3／SHA-256，碰到第一個解不開的段（斷電時檔案長度先落地、內容沒落地）就截到它前面。
   全部重讀一遍本來就省不了（BLAKE3 要從頭算），所以順便把每一段都驗過，🚫 不靠 fsync 的時序猜哪幾段可信。
4. 寫到第 `k` 段 → 明文位置 `p = k × segment_size` → 從**涵蓋 `p` 的那一塊** `⌊p / chunk_size⌋` 接著拉，那塊在 `p` 之前的部分丟掉。

**收尾**：明文 BLAKE3 hex 就是檔名 → `adopt`：已經有同 hash 的檔就刪掉自己（去重）、沒有就改名進 `media/<hh>/<hash>` →
`cache.db` 的 `media_finish`（`pool_file`、`complete = 1`、`bytes_on_disk`）→ 刪 seek 暫存檔（§4.2）。

### 4.2 seek 暫存檔：固定大小的格

**一格放一個塊**，每格大小固定（由這個檔的 `chunk_size` 決定），照到達順序 append：

```
偏移   長度   內容
0      4      magic "WBFS"
4      1      version = 1
5      3      保留，全 0
8      4      chunk_size（u32 LE）＝ 事件區塊的 chunk_size
12     4      chunk_count（u32 LE）
16     16     nonce_base，這個檔隨機
32     …      格 0、格 1、…

C = 4 + 4 + chunk_size + 16                                  每格在磁碟上的固定大小
第 s 格的位置 = 32 + s × C
第 s 格 = u32_le(塊號 i)                                       ← 明文：重開時不必解密就能重建位置表
          ‖ XChaCha20-Poly1305(
              key   = 池金鑰
              nonce = nonce_base(16) ‖ u64_le(s)
              aad   = "wbf-media-seek v1" ‖ nonce_base ‖ u64_le(s) ‖ u32_le(i) ‖ u32_le(chunk_size)
              明文  = u32_le(len) ‖ 第 i 塊的明文(len) ‖ 0 × (chunk_size − len) )
規則：len == chunk_size，除非 i 是最後一塊（那時 len = file_size − i × chunk_size）
```

- **塊號放在明文**（4 byte），但**綁在 AAD 裡**：改了它就解不開。洩漏的只有「這個檔哪幾塊被 seek 過」，可接受（池本來就不防檔案大小與數量，/docs/design/media/media-pool.md §8）。
- **最後一塊也寫滿一格**，真實長度記在 `len`（維護者 2026-10-01：寫正好的尺寸，真實長度看長度欄）。
- 🚫 不存解不開的東西：存進來的明文是 §3.3 驗過的。
- 每格**只寫一次**：nonce 由格號決定，格號只增不減。

**位置表（O(1)）**：

```
slots: Vec<u32>，長度 = chunk_count，初值 0
寫入第 i 塊到第 s 格之後：slots[i] = s + 1         （0 ＝ 沒有）
查第 i 塊：slots[i] == 0 → 沒有；否則在第 slots[i] − 1 格，位置 32 + (slots[i] − 1) × C
```

輸入塊號、O(1) 拿到它在暫存檔的位置（維護者 2026-10-01）。記憶體是 `4 byte × chunk_count`：10 GiB、64 KiB 塊是 640 KiB；
規格允許的最小塊（4 KiB）配 10 GiB 是 10 MiB——那是最壞情況，常見的 64 KiB／1 MiB 遠小於此。

**寫入順序（每一格）**：寫完整的一格 → fsync → **然後**才更新 `slots`。所以位置表永遠只指向已經落地的格。

**daemon 重開：保留暫存檔、重建位置表**（維護者 2026-10-01）：

1. 開檔，驗檔頭；`chunk_size`、`chunk_count` 要跟這次的 manifest 一樣，不一樣就整個刪掉（不是同一個檔）。
2. 檔長驗算：`(檔長 − 32) % C ≠ 0` → 最後一格寫到一半，截到 `32 + ⌊(檔長 − 32) / C⌋ × C`。**被 force 關掉也不會壞檔，只是最後一筆沒寫進去。**
3. 逐格只讀前 4 byte（塊號）重建 `slots`：塊號 ≥ `chunk_count` → 從這格起截掉（檔壞了）；同一塊號出現兩次 → 留第一個。
4. 內容的 AEAD **讀的時候才驗**：某一格解不開 → 把那一格從 `slots` 拿掉、當成沒有、重拉（重拉的塊 append 到新的一格，舊格留著不用，主檔完成時一起刪）。

### 4.3 命名與目錄

```
<data dir>/s/<B58 nonce>_<B58 密文>/                         這台 server（跟 cache.db 同層，同 server 的帳號共用）
    cache.db
    media/
        pending/
            m<media.id>                                       主檔，下載中（池格式 v2）
            m<media.id>.seek                                  seek 暫存檔（§4.2）
        <hash 前 2 hex>/<hash>                                完成的主檔（明文 BLAKE3，64 個小寫 hex）
```

- 檔名只有 `media.id` 與 hash：原檔名、mimetype、mxc 只在 `cache.db`（/docs/design/media/media-pool.md §2）。
- 啟動時的掃描（`sweep`）多一條：`pending/` 裡沒有對應 `media` 列、或 `media.complete = 1` 的 `.seek` → 刪。

### 4.4 `cache.db` 寫哪些

| 時機 | 寫什麼 |
|---|---|
| job 建立 | `media_begin`：沒有這個 mxc 的列就建（`complete = 0`）。已經 `complete = 1` 而且池檔在 → 不下載 |
| 每 1.5 秒（主檔） | `chunks_written` ＝ 完整段數（只給顯示用） |
| 主檔完成 | `media_finish`：`pool_file`、`complete = 1`、`file_size`、`bytes_on_disk`；區塊沒帶 sha256 時 `hash = blake3:…` |
| seek 暫存檔 | 🚫 不進 DB：暫存檔自己就是記錄（§4.2 的重建） |

⚠️ `chunks_written` 從「塊數」改記「段數」：欄位名字已經不對了。實作時改名（`segments_written`），`cache.db` 的 schema 版本跟著加一（舊庫整個重建，/docs/design/storage/local-cache-db.md §1）。

## 5. 下載佇列：一塊一步的 job（維護者 2026-10-01 定）

### 5.1 誰在跑

**每個帳號一個下載 worker**（一個 task），只有它用這個帳號的 `Download` 線拉檔。它有兩個收件匣：

| 收件匣 | 放什麼 | 先後 |
|---|---|---|
| seek 請求 | 「第 `i` 塊，拉到交給我」（§6） | **先** |
| 下載步驟 | 佇列頭那個 job 的下一步 | 沒有 seek 請求時才處理 |

worker 每次只做**一塊**：做完一塊回頭看收件匣，所以 seek 最多等一塊的傳輸時間（§6.2）。

### 5.2 job 與 downloading 表

```
DownloadJob {                                       排在佇列裡的東西
    mxc,            要下載哪個檔                      example: "mxc://localhost/000000000000004d"
    next,           下一塊要拉第幾塊（位置）           example: 123
    total,          總塊數；不知道是 0                 example: 456
    manifest,       區塊（檔案金鑰），驗過的參數（§3.2）
}

downloading: HashMap<mxc, Arc<Downloading>>         每個帳號一張（維護者 2026-10-01）
Downloading {                                       一個正在下載的檔，所有塊共用這一份
    cancelled: AtomicBool,                          取消旗標
    done:      AtomicU32,                           已落地的塊數（= next）
    total:     AtomicU32,                           總塊數（0 = 還不知道）
}
```

- **什麼時候放進去**：worker 第一次開始拉這個 job（§5.4 第 0 步之前）。還在佇列裡排著的 job **不在表裡**。
- **什麼時候拿掉**：最後一包落地、或處理一包時發現旗標是 true——**由 worker 自己拿掉自己**，🚫 別人不替它拿。
- **每個塊的請求都帶著同一份 `Arc<Downloading>`**：取消只要把 `cancelled` 設成 true，之後收到的封包就只處理當下那一包（落地），🚫 不再往下拉。
- `done`／`total` 也在這裡：`media.queue` 與進度推播（§5.5）直接讀它，🚫 不必去問 worker。

### 5.3 建一個 job：先看 DB 與檔案

收到要整檔的請求（`media.download`、`media.open`、`media.save_to`，§7.1），要哪個 mxc：

| 本地狀況 | 做什麼 |
|---|---|
| `media` 列 `complete = 1`、池檔在 | 🚫 不建 job：已經有了 |
| 有本機原檔（/docs/design/rpc-specs/data-plane.md §8.1） | 🚫 不建 job（`media.save_to` 例外：它要的是池或原檔裡的 bytes，直接從原檔複製） |
| 這個 mxc 已經在 `downloading` 表或佇列裡 | 🚫 不重複建 |
| 有 pending 主檔 | **從檔案確定進度**：驗檔長、逐段解（§4.1 續傳）→ 已寫 `k` 段 → `next = ⌊k × segment_size / chunk_size⌋`；DB 的 `segments_written` 只當提示，🚫 不當依據 |
| 都沒有 | `next = 0` |
| `total` | 區塊在手（有 `file_size`、`chunk_size`）就算 `ceil(file_size / chunk_size)`；不知道就填 **0**，第一步問 `Info` 再補 |

然後 job 排到佇列尾。daemon 重開時佇列不保存（維護者 2026-10-01）：之後再有人要這個檔，照上表從 DB 與檔案重建進度、接著拉。

### 5.4 一步：拉一塊、落地、觸發下一步

```
worker 取佇列頭的 job：
  第一次 → downloading.insert(mxc, Arc<Downloading>{ cancelled: false, done: next, total })
  0. total == 0 → Info（§3.2）驗過、填 total
  1. 封一個塊請求：{ chunk: next, 落地後 next + 1, total, 那一份 Arc<Downloading> }
  2. 拿第 next 塊：
       位置表有（§4.2）→ 從 seek 暫存檔讀那一格、用池金鑰解開   ← 不走網路
       沒有             → Download/Read { mxc, chunk: next }，驗長度、用檔案金鑰解開（§3.3）
  3. 明文餵進主檔（湊滿一段才封、才 append，§4.1）→ 這一塊落地結束 → done = next + 1
  4. 觸發落地後續（看塊請求帶來的那一份）：
       cancelled == true      → 停：主檔與暫存檔留著；downloading.remove(mxc)       ← 取消在這裡生效
       next + 1 == total      → 收尾（§4.1）、刪暫存檔、推最後一則進度；downloading.remove(mxc)；拿下一個 job
       否則                    → next += 1，回到 worker（先看有沒有 seek 請求）
```

- **取消**只在「一塊落地之後」生效：正在拉的那一塊會拉完、寫完，所以主檔永遠停在整塊（整段）的邊界上。
- **壞檔**（§3.2、§3.3）→ job 移除、`downloading.remove(mxc)`、`media` 列 reset、推一則失敗。
- **網路斷**：job 停在 `next`（還在表裡），線重開（/docs/design/daemon/link-pool.md §3.1）後從同一塊接著拉。
- **沒人看的時候照樣拉完**（Downloading 就是拉完）。**暫停**之後才加，加的時候只停這個收件匣，🚫 不停 seek。

**`media.cancel { mxc }`**：

| 這個 mxc 在哪 | 做什麼 |
|---|---|
| `downloading` 表裡（正在拉） | `cancelled = true`。worker 處理完手上那一包就停、自己從表裡拿掉 |
| 佇列裡（還沒開始） | 從佇列拿掉（它不在表裡） |
| 都不在 | 回 `cancelled: false` |

### 5.5 進度：daemon 負責（維護者 2026-10-01）

背景下載沒有 HTTP 連線可以看，所以進度由 daemon 管、由 daemon 報：

- **狀態在 `downloading` 表裡**（`done`／`total`，§5.2）；DB 每 1.5 秒寫一次段數（顯示用，§4.4）。
- **推播 `media.download`**（要先 `subscribe`，/docs/design/rpc-specs/rpc-spec.md §4）：
  `{ mxc, state: "queued"|"downloading"|"complete"|"cancelled"|"failed", done: next, total, user }`。
  每個 job **最多每秒一則**，加上每次狀態改變一則（排進、開始、完成、取消、失敗）——🚫 不是每塊一則（那會把 `room.message` 擠掉，/docs/design/daemon/daemon-runtime.md §5.4）。
- **`media.queue`** 隨時可以問現在的樣子。
- 播放中的進度照舊：就是那個 GET 收到多少 bytes。

`media.save_to`（另存新檔）也排隊（維護者 2026-10-01）：建 job、等它完成、再從池讀出來寫到使用者指定的位置；`no_cache` 版也排隊。

## 6. Seek

### 6.1 什麼時候算 seek

GET 要的那一段（§7.2），有任何一塊**不在**「本機原檔、完整主檔、主檔已寫的段、seek 暫存檔」之中 → 那幾塊要**現拉**，這就是 seek。
seek 🚫 不算下載：不建 job、不進佇列、不影響佇列的順序（維護者 2026-10-01）。

### 6.2 在哪條線、怎麼插隊

**同一條 `Download` 線，seek 優先**（維護者 2026-10-01）：GET 把「第 `i` 塊」丟進那個帳號 worker 的 seek 收件匣（§5.1），
worker 做完手上這一塊就先處理它。

- 佇列一次只做一塊，所以 seek 最多等**一塊**的傳輸時間。
- seek 跟佇列可能是不同的檔：線是帳號的，不是檔的。
- seek 之後播放器通常順著往下讀，所以 seek 那邊也是一塊接一塊地要；這段時間佇列被讓到一邊，等播放器讀夠了（或主檔追上了）再回來。

### 6.3 一塊被 seek 拉到之後

1. §3.3：驗長度、AEAD 解開 → 明文在記憶體。
2. **append 進 seek 暫存檔**的下一格 → fsync → `slots[i] = s + 1`（§4.2）。
3. 交給要它的 GET。

同一塊被好幾個 GET 同時要（播放器常同時開好幾條 Range）：worker 記「正在拉的塊」，第二個請求等第一個拉完、讀暫存檔，🚫 不重拉。

### 6.4 主檔追到 seek 拉過的塊

就是 §5.4 第 2 步：位置表有就從暫存檔搬、**不走網路**；沒有（或那一格解不開）照常向 server 拉。

所以主檔永遠是「從第 0 段起連續」，不會有洞；暫存檔只是讓主檔追上來時快一點。最差狀況：同一塊在暫存檔與主檔各一份，**同時佔兩份空間**，主檔完成就刪掉暫存檔。暫存檔先不設上限（維護者 2026-10-01）。

## 7. 交給使用者：RPC 與 HTTP

### 7.1 RPC

| method | params | result | 說明 |
|---|---|---|---|
| `media.download` | `{ mxc } \| { room, event_id } \| { manifest }`，`user?`、`server?` | `{ state, done, total }` | 建 job（§5.3）。已經完整或有原檔就直接回 `complete`／`local_source` |
| `media.open` | 同上 | `{ url, mimetype?, size, state }` | `url` 是 `/media/mxc/e-…`（/docs/design/rpc-specs/data-plane.md §8，不帶帳號）。`state`：`local_source`、`complete`、`downloading`、`queued`。不完整也沒原檔 → 順便建 job |
| `media.queue` | `{ user?, server? }` | `{ items: [{ mxc, name?, state, done, total }] }` | 佇列現在的樣子，第一個是正在拉的 |
| `media.cancel` | `{ mxc, user?, server? }` | `{ cancelled: bool }` | 正在拉 → 設 `downloading[mxc].cancelled`，處理完手上那一包就停；排著 → 從佇列拿掉（§5.4） |
| 推播 `media.download` | — | `{ mxc, state, done, total, user }` | §5.5 |

`media.save_to` 的參數不變，行為改成排隊（§5.5 最後）。

### 7.2 GET 的路由

`GET /media/mxc/e-…` 開出 mxc（URL 的格式、Host 檢查、狀態碼在 /docs/design/rpc-specs/data-plane.md §2、§8），然後把要的 Range **一段一段**分給來源，由上往下：

| 先後 | 來源 | 條件 |
|---|---|---|
| 1 | **本機原檔** | `media.source_uri` 解得出來、一般檔、大小對得上（/docs/design/rpc-specs/data-plane.md §8.1）。整個 Range 直接讀它，🚫 不碰池、不觸發下載 |
| 2 | **完整的主檔** | `media.complete = 1`：`PoolReader` seek 到起點往下讀 |
| 3 | **主檔已寫的段** | 這一段在 `已寫段數 × segment_size` 之前：讀 pending 主檔（只讀完整的段） |
| 4 | **seek 暫存檔** | `slots[i] ≠ 0`：讀那一格 |
| 5 | **現拉（seek）** | §6 |

- 一個 GET 可能橫跨好幾種來源（前面在主檔、後面要現拉）：照塊號一塊一塊決定，邊讀邊吐。
- 吐出去的是**明文**；記憶體裡同時最多一塊（或一段）。HTTP 回應用串流 body，背壓同上傳：播放器讀得慢，daemon 就晚一點讀下一塊。
- 上游慢就停著等、連線不斷；拿不到才斷（/docs/design/rpc-specs/data-plane.md §8）。

## 8. 斷在哪裡、會留下什麼

| 斷點 | 留下的 | 下次 |
|---|---|---|
| 主檔寫到一半（段沒寫完） | 檔長不是整數倍 | 截掉那一段，從涵蓋它的塊接著拉 |
| 主檔的長度落地、內容沒落地 | 檔長是整數倍，但最後幾段解不開 | 逐段解，截到第一個解不開的段 |
| 暫存檔寫到一半 | 檔長不是整數倍 | 截掉最後一格；那一塊之後再拉 |
| 暫存檔某一格內容沒落地 | 那一格解不開 | 讀到時拿掉、重拉 |
| 主檔完成、改名前 | pending 主檔完整 | 續傳時整檔驗完 → 直接收尾 |
| 改名後、DB 記完成前 | 池檔在、列說沒完成 | 下次 `media_begin` 發現同 hash 的池檔 → 去重、記完成 |
| 收尾後、刪暫存檔前 | 多一個 `.seek` | 啟動掃描刪（§4.3） |

**每一筆寫入都是「整段／整格 ＋ fsync ＋ 才改記憶體或 DB」**，所以被 force 關掉最多丟最後一筆，🚫 不會留下讀得到的壞資料。

## 9. 跟現在的差異

| 現在 | 這份之後 |
|---|---|
| 池格式 v1：沒有長度欄、最後一段可以短、有暫定段（每 1.5 秒截掉重寫） | v2：每段寫滿、長度在密文裡、沒有暫定段（§4.1） |
| `media::fetch` 整個檔握著 `Download` 線 | 每塊借一次線（§3.1），seek 插隊 |
| 下載是 `media.save_to` 叫的時候當場拉 | 每帳號一條佇列，一次一個檔（§5） |
| 續傳信 DB 的 `chunks_written` | 檔案本身就是進度（§4.1） |
| 不做 seek（/docs/design/media/media-pool.md §6） | seek 暫存檔 ＋ 位置表（§4.2、§6） |
| 沒有讀的 HTTP | `media.open`、`GET /media`（§7） |

/docs/design/media/media-pool.md 的「沒有 bitmap、檔要嘛完整要嘛是連續的前綴」**不變**：主檔還是連續前綴；seek 暫存檔是旁邊多出來、用完就丟的東西。

## 10. 要測的

- 池格式 v2：寫讀、1 byte 的檔、剛好整段、跨段；檔長不是整數倍 → 截；最後一段解不開 → 截；翻一個 bit／錯金鑰／段搬位置都拒；長度欄超過 segment_size 拒。
- seek 暫存檔：寫讀、最後一塊補滿；重開時截掉半格、重建位置表、同塊號重複留第一個、塊號超出拒、chunk_size 對不上整個刪；格的 AEAD 綁塊號（改塊號就解不開）。
- 位置表：O(1) 查、`slots[i] = s + 1` 在 fsync 之後。
- 佇列：一次一個、先進先出、壞檔移除、重複排不重複（佇列裡或 `downloading` 表裡都算）。
- 取消：排著的從佇列拿掉、不在表裡；正在拉的設旗標後**只再落地手上那一包**（假 server 計 `Read` 次數）、自己從表裡拿掉；完成也從表裡拿掉；取消後再排一次從斷點接。
- seek：seek 時佇列讓線（最多等一塊）、同一塊兩個 GET 只拉一次、主檔追上時從暫存檔搬（不走網路，假 server 計數）、主檔完成刪暫存檔。
- GET 路由：五種來源各一、一個 Range 橫跨主檔與現拉、Range 超出檔尾 416。
- 斷點表（§8）每一列一條。
- 真 server：上傳一個檔 → `media.open` → 從中間 Range（seek）→ 拿到的 bytes 對 → 等佇列拉完 → 整檔對、暫存檔不見了。

## 11. 維護者 2026-10-01 定的

1. **v1 的池檔一律當壞檔重下**（它是快取）：啟動掃描遇到 v1 的完成檔與暫存檔都刪，`media` 列 reset。
2. **佇列不跨 daemon 重開保存**：進度照 DB 或直接讀檔案大小確定（§5.3），🚫 不另存佇列。
3. **`media.save_to` 也排隊**（§5.5）。
4. **背景下載的進度由 daemon 負責**（§5.5）：job 記著、推播 `media.download`（每個 job 最多每秒一則＋狀態改變）、`media.queue` 可以問。
5. **佇列是一塊一步的 job**（§5.4）：「拉第 `next` 塊 → 落地 → 觸發後續（`next + 1`）」，取消插在落地之後。
6. **取消旗標放在 `downloading` 表**（§5.2）：key 是 mxc，所有塊共用同一份；設成 true 就只處理當下那一包；拉完或取消時自己從表裡拿掉。

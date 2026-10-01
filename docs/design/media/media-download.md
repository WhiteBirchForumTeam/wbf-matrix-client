# 媒體下載：佇列、主檔、seek 暫存檔（維護者 2026-10-01 定）

> 程式在哪：sdk 的 `media_pool.rs`（池格式 v2，§4.1）、`seek_store.rs`（§4.2）、`media.rs` 的 `MediaDownload`（一個檔在主檔、暫存檔與網路之間怎麼拿塊）；
> core 的 `download_queue.rs`（每帳號的 worker，§5、§6）、`media_ops.rs`（RPC 面，§7.1）、`media_stream.rs`（GET 的來源，§7.2）；daemon 的 `data_plane.rs`、`handle/media.rs`。
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
🚫 不是整個檔握著線。這樣 seek 才插得進來（§6.2）。

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

這是池格式 v2（為什麼長這樣在 /docs/design/media/media-pool.md §8）。跟 v1 的差別：v1 沒有每段的長度欄位、長度從檔案總長推、最後一段可以短，
而且有「暫定段」（每 1.5 秒把湊不滿的尾巴先封起來、之後截掉重寫）；v2 拿掉這兩件事：

```
偏移   長度   內容
0      4      magic "WBFP"
4      1      version = 2
5      3      保留，全 0
8      4      segment_size（u32 LE）＝ 65536
12     16     nonce_base，這個檔隨機
28     4      owner（u32 LE）＝ mxc 的 BLAKE3 前 4 byte（下面那條）
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
- **`owner`：主檔是誰的**。暫存名是 `m<media.id>`，而 `cache.db` 重建（換 schema、解不開）之後 id 從 1 重新編號：舊的 pending 檔還在的話，
  新的 mxc 可能拿到同一個名字。續傳時 `owner` 跟這次的 mxc 對不上就整個重下，🚫 不把別的檔的段接進來、🚫 不拿同一個段號（nonce）封別的明文。
  只有 4 byte：它防的是意外（本機的檔），🚫 不是安全邊界（池金鑰才是）。完成檔照內容去重、可以被幾個 mxc 共用，所以 mxc 🚫 不進段的 AAD。

**寫入（主檔）**：

1. 明文丟進段大小的緩衝；**湊滿一段才封、才 append**（塊大小不必是 64 KiB 的倍數，塊的明文可以跨段）。
2. 同時順序餵 BLAKE3（池檔的檔名）與 SHA-256（區塊有 `sha256` 時，§3.1 第 5 條的整檔核對）。
3. 最後一塊寫完：封最後一段（補滿、記真實長度）、fsync、得出 BLAKE3；區塊帶 `sha256` 就跟整檔的 SHA-256 比，對不上是壞檔。
4. **進度**：**檔案本身就是進度**——完整的段數就是寫到哪。每 1.5 秒 fsync 一次（限制斷電時最多丟多少），並把段數寫回 `cache.db` 的 `media.segments_written`（給 `media.info` 顯示用；🚫 不當續傳的依據）。

**續傳（daemon 重開、或被取消後重新排進佇列）**：

1. 開主檔，驗檔頭（magic、version、segment_size、owner）。**v1 一律當壞檔、重下**（維護者 2026-10-01：它是快取），完成檔與暫存檔都一樣（§9）。
2. 檔長驗算，不是整數倍就截掉最後那段不完整的。
3. **從第 0 段起逐段解開**：餵回 BLAKE3／SHA-256，碰到第一個解不開的段（斷電時檔案長度先落地、內容沒落地）就截到它前面。
   全部重讀一遍本來就省不了（BLAKE3 要從頭算），所以順便把每一段都驗過，🚫 不靠 fsync 的時序猜哪幾段可信。
4. 寫到第 `k` 段 → 明文位置 `p = k × segment_size` → 從**涵蓋 `p` 的那一塊** `⌊p / chunk_size⌋` 接著拉，那塊在 `p` 之前的部分丟掉。
5. 最後一段是短的（收尾封過、還沒改名就斷了）：那一段解回記憶體，整個檔已經收齊，直接收尾；🚫 不准再接著寫（同一個段號會封到不同明文）。

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
              aad   = "wbf-media-seek v1" ‖ nonce_base ‖ u64_le(s) ‖ u32_le(i) ‖ u32_le(chunk_size) ‖ mxc
              明文  = u32_le(len) ‖ 第 i 塊的明文(len) ‖ 0 × (chunk_size − len) )
規則：len == chunk_size，除非 i 是最後一塊（那時 len = file_size − i × chunk_size）
```

- **塊號放在明文**（4 byte），但**綁在 AAD 裡**：改了它就解不開。洩漏的只有「這個檔哪幾塊被 seek 過」，可接受（池本來就不防檔案大小與數量，/docs/design/media/media-pool.md §8）。
- **最後一塊也寫滿一格**，真實長度記在 `len`（維護者 2026-10-01：寫正好的尺寸，真實長度看長度欄）。
- 🚫 不存解不開的東西：存進來的明文是 §3.3 驗過的。
- 每格**只寫一次**：nonce 由格號決定，格號只增不減。
- **mxc 在 AAD 裡**：跟主檔的 `owner` 同一個理由（§4.1），暫存名換了主人，舊格一律解不開。檔頭剛好 32 byte、沒有空位放標記，所以綁在每一格；暫存檔只屬於一個 mxc，綁了沒有代價。

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

1. 開檔，驗檔頭；`chunk_size`、`chunk_count` 要跟這次的 manifest 一樣，不一樣就整個重建（不是同一個檔）。有格的話試解第 0 格，解不開（別的 mxc 的）也整個重建。
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
- 掃描（`media::sweep`：這個程序裡第一次起這台 server 的 worker 時、`media.gc` 時）對 `pending/` 的規則：沒有對應的 `media` 列、列已經 `complete = 1`、
  主檔不是池格式 v2、或超過保護期沒動過（看檔案的修改時間）→ 刪；**worker 正開著的暫存名🚫 不碰**（正在下載的檔不能從底下抽掉）。
  `.seek` 跟它的主檔同一個主人。完成檔是 v1（打不開）→ 列 reset、沒人指著的檔刪。

### 4.4 `cache.db` 寫哪些

| 時機 | 寫什麼 |
|---|---|
| job 建立 | `media_begin`：沒有這個 mxc 的列就建（`complete = 0`）。已經 `complete = 1` 而且池檔在 → 不下載 |
| 每 1.5 秒（主檔） | `segments_written` ＝ 完整段數（只給顯示用） |
| 主檔完成 | `media_finish`：`pool_file`、`complete = 1`、`file_size`、`bytes_on_disk`；區塊沒帶 sha256 時 `hash = blake3:…` |
| seek 暫存檔 | 🚫 不進 DB：暫存檔自己就是記錄（§4.2 的重建） |

欄位記的是**段數**（池格式 v2 的 64 KiB 段），🚫 不是塊數：塊大小跟段大小無關。DB 一律經那台 server 的唯一寫入者（`ServerCache`，/docs/design/daemon/daemon-runtime.md §2）。

## 5. 下載佇列：一塊一步的 job（維護者 2026-10-01 定）

### 5.1 誰在跑

**每個帳號一個下載 worker**（一個 task），只有它用這個帳號的 `Download` 線拉檔。它有兩個收件匣：

| 收件匣 | 放什麼 | 先後 |
|---|---|---|
| seek 請求 | 「第 `i` 塊，拉到交給我」（§6） | **先** |
| 下載步驟 | 佇列頭那個 job 的下一步 | 沒有 seek 請求時才處理 |

worker 每次只做**一塊**：做完一塊回頭看收件匣，所以 seek 最多等一塊的傳輸時間（§6.2）。反過來也成立：佇列那一塊卡在網路上時，seek 也等它。

- **線只借開著的**（`LinkPool::reuse`）：開線是入口（`media.download`／`media.open`／`media.save_to` 先確定 `Download` 線開著）與看線迴圈（/docs/design/daemon/link-pool.md §3.1）的事。
  線沒開或斷了，worker 每秒再試一次，進度停在原地，🚫 不在 worker 裡開線（那要整個 `Core`）。
- **第一次要用才起**，登出、換 session 時收（`Core::close_links` 一併收）：被收的 worker 開著的檔 fsync 留著，下次從檔案接著拉；排著的 job 不保存（§11 第 2 條），等它的人收到錯誤。
- **主檔與暫存檔只有一個寫入者**：worker 是唯一寫它們、也是唯一讀「還沒完成的主檔」與暫存檔的人（GET 要這兩種也經過 worker，§7.2），🚫 不另開把手跟它搶。
- **同一台 server 的帳號共用池**（`m<media.id>` 是 server 層級的名字），所以同一個 mxc 同時只准一個帳號的 worker 寫：開檔前先在 `Core` 的認領表登記（server dir ＋ mxc → 帳號）。
  別的帳號正在寫它，這個 job 就等（每秒看一次；對方寫完，DB 會說完成）；seek 交給認領的那個 worker。worker 被收時它的認領一定放掉（`Worker` 的 `Drop`，🚫 不靠 abort 剛好停在哪一行）。
  收尾時先寫 DB 記完成、**才**放掉認領：反過來的話，別的帳號會在 DB 記完成之前接手、把剛進池的檔重下一次。

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
| `media.download` | `{ mxc } \| { room, event_id } \| { manifest }`（剛好給一種，給了不只一種是參數錯，🚫 不猜哪個優先），`user?`、`server?` | `{ mxc, state, done, total }` | 建 job（§5.3）。已經完整或有原檔就直接回 `complete`／`local_source` |
| `media.open` | 同上 | `{ url, mxc, mimetype?, size, state }` | `url` 是 `/media/mxc/e-…`（/docs/design/rpc-specs/data-plane.md §8，不帶帳號）。`state`：`local_source`、`complete`、`downloading`、`queued`。不完整也沒原檔 → 順便建 job |
| `media.queue` | `{ user?, server? }` | `{ items: [{ mxc, name?, state, done, total }] }` | 佇列現在的樣子，第一個是正在拉的 |
| `media.cancel` | `{ mxc, user?, server? }` | `{ cancelled: bool }` | 正在拉 → 設 `downloading[mxc].cancelled`，處理完手上那一包就停；排著 → 從佇列拿掉（§5.4） |
| `media.save_to` | 同 `media.download` 的三種說法，加 `out`、`no_cache?` | `{ out, bytes, source, hash? }`；`source` 是 `local_source`、`cache`、`server` | 排隊、等它完成、從池（或本機原檔）複製到 `out`（§5.5 最後）。`no_cache`：這次下載的複製完就從池拿掉（別的 mxc 還指著、或有人正在讀，就只清這一列），池裡本來就有的不動 |
| 推播 `media.download` | — | `{ mxc, state, done, total, user, reason? }` | §5.5。`reason` 只在 `failed` 帶 |

- 金鑰只從**這個帳號看得到的事件**裡找（§3.2）：同一份 `cache.db` 裡有同 server 別的帳號的事件，金鑰🚫 不跨帳號借。找不到是 `1100`，訊息叫你帶 manifest 或 `room` ＋ `event_id`。
- 一般 Matrix 帳號（走 matrix-sdk 的）的媒體沒有 `Download` 線，這四支加 `media.save_to` 都回 `1100`；傳統下載（`/_matrix/media`）跟傳統上傳一起做（/docs/design/rpc-specs/data-plane.md §7）。
- 下載一律走 WS 的 `Download` 線：`transport` 參數對這幾支沒有意義。CLI 的 `download --no-cache` 不走佇列（直接逐塊寫到檔案，不碰池），等 CLI 改走 RPC 時一併收掉。

### 7.2 GET 的路由

`GET /media/mxc/e-…` 開出 mxc（URL 的格式、Host 檢查、狀態碼在 /docs/design/rpc-specs/data-plane.md §2、§8），然後把要的 Range **一段一段**分給來源，由上往下：

| 先後 | 來源 | 條件 |
|---|---|---|
| 1 | **本機原檔** | `media.source_uri` 解得出來、一般檔、大小對得上（/docs/design/rpc-specs/data-plane.md §8.1）。整個 Range 直接讀它，🚫 不碰池、不觸發下載 |
| 2 | **完整的主檔** | `media.complete = 1`：`PoolReader` seek 到起點往下讀 |
| 3 | **主檔已寫的段** | 這一塊在 `已寫段數 × segment_size` 之前：讀 pending 主檔（只讀完整的段） |
| 4 | **seek 暫存檔** | `slots[i] ≠ 0`：讀那一格 |
| 5 | **現拉（seek）** | §6 |

- 1、2 在 GET 這邊直接讀；3～5 一塊一塊交給那個帳號的 worker（§5.1：還沒完成的主檔與暫存檔只有它碰）。worker 收到時檔剛好完成了，就從完整的池檔讀。
- **URL 不帶帳號**：照本機已登入的帳號一個一個找有這個 mxc 紀錄的 `cache.db`，mxc 的 server_name 跟帳號網域一樣的先找。
  1、2 不要金鑰；要現拉就要那個帳號看得到帶金鑰的事件。有紀錄但沒有完整的檔、也沒有帳號拿得到金鑰 → 502；完全沒有紀錄 → 404。
- `HEAD` 回一樣的標頭、沒有 body。Range 只認單一一段（`bytes=a-b`、`bytes=a-`、`bytes=-n`），終點超過檔尾就截到檔尾；寫壞的、不只一段的就當沒帶、回整檔（RFC 9110 §14.2）；起點在檔尾或之後是 416。
- 一個 GET 可能橫跨好幾種來源（前面在主檔、後面要現拉）：照塊號一塊一塊決定，邊讀邊吐。
- 吐出去的是**明文**；記憶體裡同時最多一塊（或一段）。HTTP 回應用串流 body，背壓同上傳：播放器讀得慢，daemon 就晚一點讀下一塊。
- 上游慢就停著等、連線不斷；拿不到才斷（/docs/design/rpc-specs/data-plane.md §8）：body 已經開始吐就沒辦法改狀態碼，所以是讓 body 出錯、連線斷掉，播放器知道沒收完（🚫 不假裝結束）。

## 8. 斷在哪裡、會留下什麼

| 斷點 | 留下的 | 下次 |
|---|---|---|
| 主檔寫到一半（段沒寫完） | 檔長不是整數倍 | 截掉那一段，從涵蓋它的塊接著拉 |
| 主檔的長度落地、內容沒落地 | 檔長是整數倍，但最後幾段解不開 | 逐段解，截到第一個解不開的段 |
| 暫存檔寫到一半 | 檔長不是整數倍 | 截掉最後一格；那一塊之後再拉 |
| 暫存檔某一格內容沒落地 | 那一格解不開 | 讀到時拿掉、重拉 |
| 主檔完成、改名前 | pending 主檔完整 | 續傳時整檔驗完 → 直接收尾 |
| 改名後、DB 記完成前 | 池檔在、列說沒完成 | 下次重下一次，收尾時 `adopt` 發現同 hash 的池檔 → 去重、記完成（在那之前掃描先跑到的話，沒人指著的池檔被刪，結果一樣） |
| 收尾後、刪暫存檔前 | 多一個 `.seek` | 掃描刪（列已經完成，§4.3） |

**每一筆寫入都是「整段／整格 ＋ fsync ＋ 才改記憶體或 DB」**，所以被 force 關掉最多丟最後一筆，🚫 不會留下讀得到的壞資料。

## 9. 跟池格式 v1 那一版的差異

| v1 | v2（現在） |
|---|---|
| 池格式 v1：沒有長度欄、最後一段可以短、有暫定段（每 1.5 秒截掉重寫） | v2：每段寫滿、長度在密文裡、沒有暫定段（§4.1） |
| `media::fetch` 整個檔握著 `Download` 線 | 每塊借一次線（§3.1），seek 插隊 |
| 下載是 `media.save_to` 叫的時候當場拉 | 每帳號一條佇列，一次一個檔（§5） |
| 續傳信 DB 記的塊數 | 檔案本身就是進度（§4.1）；DB 的 `segments_written` 只給顯示 |
| 媒體繞過唯一寫入者、自己開 `cache.db` | 一律經 `ServerCache`（/docs/design/daemon/daemon-runtime.md §2） |
| 不做 seek（/docs/design/media/media-pool.md §6） | seek 暫存檔 ＋ 位置表（§4.2、§6） |
| 沒有讀的 HTTP | `media.open`、`GET /media`（§7） |

/docs/design/media/media-pool.md 的「沒有 bitmap、檔要嘛完整要嘛是連續的前綴」**不變**：主檔還是連續前綴；seek 暫存檔是旁邊多出來、用完就丟的東西。

## 10. 測試在哪

| 測什麼 | 在哪 |
|---|---|
| 池格式 v2：1 byte、剛好整段、跨段；半段截掉、第一個解不開的段截掉；翻 bit／錯金鑰／段搬位置／檔長不是整數筆都拒；長度欄超過 segment_size 拒；收尾過、沒改名的檔續回整個、不准再寫；`owner` 對不上或 v1 不續；寫的中途讀回已封的段 | sdk `media_pool::tests` |
| seek 暫存檔：照到達順序 append、最後一塊寫滿一格；重開截半格、重建位置表、重複留第一個、塊號超出就截；改塊號／翻 bit／別的 mxc／chunk_size 不同都不收；位置表太大不給 | sdk `seek_store::tests` |
| `MediaDownload` 對假 server：下載進池、去重、從完整的段續傳（塊跟段不對齊）、seek 拉過的塊主檔不再上網、區塊對不上就刪掉暫存檔；配額清理；掃描（不碰正在下載的、刪 v1、刪過期的） | sdk `tests/media_cache.rs` |
| 佇列：一塊一個 `Read`、重複排不重複、取消只再落地手上那一包且再排接得上、排著的取消不碰網路且等的人收到錯、seek 插隊而且同一塊只上網一次、`save_to`（含 `no_cache`）、別的帳號在寫就等、worker 被收會放掉認領 | core `download_queue::tests` |
| URL：讀與上傳的 URL 不能互換；Range 解析 | daemon `data_plane::tests` |
| HTTP：未解鎖 503、別的 daemon 發的／用途不對／本機沒紀錄 404、方法不對 405 | daemon `tests/data_plane.rs` |
| 推播 `media.download` 的欄位 | daemon `push::tests` |
| 真 server：池清掉 → `media.open { room, event_id }` → 從中間 Range（seek，206）→ 等佇列拉完 → 整檔對、有完成的推播、暫存檔不見了 | daemon `tests/real_server.rs` 的 `an_attachment_goes_over_the_data_plane_into_plain_and_encrypted_rooms` |

## 11. 維護者 2026-10-01 定的

1. **v1 的池檔一律當壞檔重下**（它是快取）：啟動掃描遇到 v1 的完成檔與暫存檔都刪，`media` 列 reset。
2. **佇列不跨 daemon 重開保存**：進度照 DB 或直接讀檔案大小確定（§5.3），🚫 不另存佇列。
3. **`media.save_to` 也排隊**（§5.5）。
4. **背景下載的進度由 daemon 負責**（§5.5）：job 記著、推播 `media.download`（每個 job 最多每秒一則＋狀態改變）、`media.queue` 可以問。
5. **佇列是一塊一步的 job**（§5.4）：「拉第 `next` 塊 → 落地 → 觸發後續（`next + 1`）」，取消插在落地之後。
6. **取消旗標放在 `downloading` 表**（§5.2）：key 是 mxc，所有塊共用同一份；設成 true 就只處理當下那一包；拉完或取消時自己從表裡拿掉。

# 媒體下載：所有檔一起跑、主檔、seek 暫存檔（維護者 2026-10-01、10-02 定）

> 程式在哪：sdk 的 `media_pool.rs`（池格式 v2，§4.1）、`seek_store.rs`（§4.2）、`media.rs` 的 `MediaDownload`（一個檔在主檔、暫存檔與網路之間怎麼拿塊）；
> core 的 `download_queue.rs`（每帳號的下載處理端，§5、§6；線的請求規則在 /docs/design/daemon/link-requests.md）、`media_ops.rs`（RPC 面，§7.1）、`media_stream.rs`（GET 的來源，§7.2）；daemon 的 `data_plane.rs`、`handle/media.rs`。
>
> 傳統格式的附件（Matrix 標準的 `file`／`url`，不是分塊）怎麼進同一個池在 §12。
>
> 相關：媒體池（本地加密的儲存）在 /docs/design/media/media-pool.md；資料平面的 URL、Host 檢查、狀態碼在 /docs/design/rpc-specs/data-plane.md；
> 塊怎麼加密、下載端要做哪些檢查在 /docs/design/media/wbf-client-convention-for-chunk.md §3；server 的 `Download/*` 在 wbfuwunel 的 /docs/design/media/chunked-upload-spec.md §4。

## 0. 一句話

**一律順序寫。** 每個帳號一個下載處理端：要下載的檔**一起跑**，每個檔同時只有一塊在途，各自順序寫進池裡的**主檔**；
「拿到第 n 塊之後要第 n+1 塊」封在塊請求裡，回覆到了才送下一塊（/docs/design/daemon/link-requests.md）。
播放器 seek 到還沒拉到的地方時，那幾塊的請求**插到發送 queue 最前面**，拉到的塊順序 append 進旁邊的 **seek 暫存檔**，
用一張 O(1) 的位置表記「第幾塊放在暫存檔的第幾格」。主檔順序拉到那一塊時先查表，有就從暫存檔搬、不走網路。主檔完成就把暫存檔與位置表一起刪掉。最差狀況是同時佔兩份空間。

為什麼不用「一開始建出完整大小的檔、塊到哪寫哪」（BitTorrent 的做法）：那等於隨機寫——HDD 要來回尋道，Windows 不是稀疏檔時還會先補零
（同一個檔寫兩遍），而這兩件事維護者都不要（2026-10-01）。這份的做法讓**每一個檔都只被順序 append**。

## 1. 名詞

| 名詞 | 意思 |
|---|---|
| **塊（chunk）** | server 上的傳輸單位，大小是事件區塊的 `chunk_size`（server 收 4 KiB～2 MiB；我們用 64 KiB（預設）或 1 MiB（`chunk_size_large`））。每塊各自用**那個檔的金鑰** AEAD 加密 |
| **段（segment）** | 媒體池的儲存單位，固定 64 KiB 明文。每段各自用**池金鑰** AEAD 加密。跟塊大小無關，也不必對齊 |
| **主檔** | 池裡這個 mxc 的檔，從第 0 段開始順序 append。完成前叫 `m<media.id>`、在 `pending/`；完成後改名成明文的 BLAKE3 |
| **seek 暫存檔** | 這個 mxc 被 seek 拉到的塊，照到達順序 append。一筆一個塊、每筆固定大小。主檔完成就刪 |
| **位置表** | 塊號 → 它在 seek 暫存檔的第幾格。記憶體裡的陣列，O(1) |
| **mxc 的狀態** | 一個 mxc 在 daemon 裡的東西：manifest、驗過的參數、主檔寫到哪、暫存檔與位置表、哪幾塊在途。同一個 mxc 只有一份；主檔的塊與 seek 都用它 |
| **job** | 收件 queue 裡的一件事：「下載這個檔」（`{ mxc, manifest }`）或「seek 要這一塊」。**處理一次就消耗掉**（§5.1） |
| **downloading 表** | 每帳號一張 `HashMap<mxc, Arc<Downloading>>`：正在拉的檔，裡面有取消旗標與進度；處理 job 時放進去、拉完或取消時自己拿掉（§5.2） |
| **塊請求** | 發送 queue 裡的一個 `Read`（或第一次的 `Info`）＋ 回覆到了要做的**動作**（§5.2、/docs/design/daemon/link-requests.md §3） |
| **下載處理端** | 每個帳號一個 task：處理 job、執行回覆的動作。主檔與 seek 暫存檔只有它寫（§5.1） |
| **`Download` 線** | 連線池裡每個帳號五條 WS 線之一（/docs/design/daemon/link-pool.md），專跑 `Download/*`（kind `0x04`）；送收分開（/docs/design/daemon/link-requests.md） |

## 2. 全景

```
              homeserver
                 │  WS「Download」線（每帳號一條）：Info、Read{mxc, chunk}
                 ▼
   ┌───────── daemon ───────────────────────────────────────────────────────────┐
   │  收件 queue：job（下載 A、下載 B、下載 C、seek B5…），一個處理一次                │
   │      │ 處理 job：建 downloading 項、開主檔、塞第一個塊請求                        │
   │      ▼                                                                       │
   │  發送 queue：A0 B0 C0 → A1 B1 C1 …（每檔一塊在途）；seek 的塊請求插最前面          │
   │      │                                                                       │
   │      ▼ 回覆到了：執行它的動作                                                  │
   │  主檔的塊：驗長度、AEAD 解開 → 落地 → 看取消旗標 → 塞下一塊                       │
   │  seek 的塊：驗、解開 → append 進暫存檔 → 交給 GET                                │
   │      │                                                                       │
   │      ▼ 順序                                                                   │
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

**一律 `Download` 線**（WS，kind `0x04`）。🚫 不走 `Misc`：`Misc` 送訊息、拉訊息窗、查房間狀態，一塊可能 1 MiB，塞在那條上會卡住送訊息。

塊請求交給這條線的發送 queue，回覆到了執行它的動作（/docs/design/daemon/link-requests.md）：🚫 沒有人握著線等回覆，
所以很多個檔可以同時有一塊在途，seek 的請求也插得進發送 queue 的最前面（§6.2）。

### 3.2 開始一個檔：`Info`，一個 mxc 只做一次

```
Download/Info { mxc } → Ack { file_size, chunk_size, chunk_count, ... }，data = Create 時那份加密描述
```

照 /docs/design/media/wbf-client-convention-for-chunk.md §3.1 第 1、2 條與 §4（現有的 `WbfClient::verify_target`）：

- 區塊認得（`v`、`cipher`、加密模式有 `key`／`nonce_base`）；
- `chunk_size`、`chunk_count` 與 `Info` 一致，`chunk_count == ceil(file_size / chunk_size)`；
- 描述用檔案金鑰解得開，內容跟區塊對得上。

任一條不過 → 這個 mxc 整個當壞檔（fail closed；只是 seek 的處理端沒有檔可刪，只回錯、丟掉那份描述，§6.1），從 downloading 表移除、GET 斷線，🚫 不吐任何部分內容。結果（`VerifiedTarget`：檔案金鑰、`file_size`、`chunk_count`）
存在這個 mxc 的狀態裡，之後每一塊（主檔與 seek 都是）都用它，🚫 不重問。

**金鑰從哪來**：manifest（`media.open` 帶的，或 daemon 從 `cache.db` 找：`event_media` → 引用這個 mxc 的事件 → `content_json` 裡的區塊）。
找不到任何區塊 → 沒有金鑰 → `media.open` 回 1100。

### 3.3 一塊：`Read`

```
Download/Read { mxc, chunk: i } → Ack { chunk: i, len, ... }，data = 第 i 塊密文
```

1. **長度**：密文長度必須剛好是「預期明文長度 ＋ 16」（明文模式是預期明文長度）。預期明文長度：非最後一塊 ＝ `chunk_size`；最後一塊 ＝ `file_size − i × chunk_size`。
2. **AEAD**：`nonce = nonce_base ‖ u32_be(i)`、`aad = "wbf-chunk-v1"`，用檔案金鑰解。標籤不對 → 這一塊壞了。
3. 現有的 `read_and_open_chunk` 就是這兩步。

**一塊壞了**：重拉一次（傳輸錯）；還是壞 → 整個檔當壞檔（只是 seek 的同 §3.2），從 downloading 表移除、正在讀它的 GET 斷線。🚫 不跳過、🚫 不吐。

### 3.4 記憶體

- **一個檔**一次只有**一塊**明文在記憶體（≤ `chunk_size`），解開 → 交給下一步寫 → 釋放。明文 buffer 用 `Zeroizing`，丟掉時清零。
- 整個帳號同時在記憶體裡的塊數 ≤ 在跑的檔數 ＋ 在途的 seek 塊數；同一塊被好幾個 GET 同時要只拉一次（§6.3）。每個在跑的檔另外有一個主檔的段緩衝（64 KiB）。
- 🚫 不在記憶體裡累積「先收著之後再寫」：下一塊的請求要等這一塊落地的動作才送出去，寫不進去就沒有下一塊，這就是背壓（同 /docs/design/rpc-specs/data-plane.md §4.5 的上傳）。

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
3. 最後一塊寫完：封最後一段（補滿、記真實長度）、fsync、得出 BLAKE3；區塊帶 `sha256` 就跟整檔的 SHA-256 比，結果記進 `verified`（1 對上、2 對不上，§12.3），🚫 當壞檔（維護者 2026-10-06：每塊都過了 AEAD，要不要用由前端決定）。
4. **進度**：**檔案本身就是進度**——完整的段數就是寫到哪。每 1.5 秒 fsync 一次（限制斷電時最多丟多少），並把段數寫回 `cache.db` 的 `media.segments_written`（給 `media.info` 顯示用；🚫 不當續傳的依據）。

**續傳（daemon 重開、或被取消後再要一次）**：

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
- 每格**只寫一次**：nonce 由格號決定，格號只增不減。**格號先佔走才寫**：寫失敗（例：磁碟滿）、重開時寫到一半或塊號壞掉的格，號碼都跳過、🚫 再給別的塊——
  同一把池金鑰、同一個 nonce 🚫 封第二份明文（2026-10-02，PR #69 審查）。所以重開🚫 截檔：殘留的半格留在檔裡，位置表永遠不指到它。
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
2. 檔長驗算：`(檔長 − 32) % C ≠ 0` → 最後一格寫到一半：**🚫 截**，半格留著、它的格號算用過；下一格的號碼是 `⌈(檔長 − 32) / C⌉`（2026-10-02，PR #69 審查：截掉再寫會讓同一個 nonce 封兩份明文）。**被 force 關掉也不會壞檔，只是最後一筆沒寫進去。**
3. 逐格只讀前 4 byte（塊號）重建 `slots`：塊號 ≥ `chunk_count` → **跳過**這格（號碼照樣算用過）；同一塊號出現兩次 → 留第一個。
4. 內容的 AEAD **讀的時候才驗**：某一格解不開 → 把那一格從 `slots` 拿掉、當成沒有、重拉（重拉的塊 append 到新的一格，舊格留著不用，主檔完成時一起刪）。

### 4.3 命名與目錄

```
<data dir>/s/<B58 nonce>_<B58 密文>/    這台 server（跟 cache.db 同層，同 server 的帳號共用）
├── cache.db
└── media/
    ├── pending/
    │   ├── m<media.id>                  主檔，下載中（池格式 v2）；例 m7
    │   └── m<media.id>.seek             seek 暫存檔（§4.2）；例 m7.seek
    └── <hash 前 2 hex>/
        └── <hash>                       完成的主檔（明文 BLAKE3，64 個小寫 hex）
                                         例 e0/e050d82e36326f3fdf063dd38b034f2b43be659eba3d6488396db907d66e60d5
```

- 檔名只有 `media.id` 與 hash：原檔名、mimetype、mxc 只在 `cache.db`（/docs/design/media/media-pool.md §2）。整個資料目錄的樹與一個完整檔的實際檔頭在 `/docs/design/storage/local-storage.md` §3、§5.2。
- 掃描（`media::sweep`：這個程序裡第一次起這台 server 的下載處理端時、`media.gc` 時）對 `pending/` 的規則：沒有對應的 `media` 列、列已經 `complete = 1`、
  主檔不是池格式 v2、或超過保護期沒動過（看檔案的修改時間）→ 刪；**處理端正開著的暫存名🚫 不碰**（正在下載的檔不能從底下抽掉）。
  `.seek` 跟它的主檔同一個主人。完成檔是 v1（打不開）→ 列 reset、沒人指著的檔刪。

### 4.4 `cache.db` 寫哪些

| 時機 | 寫什麼 |
|---|---|
| job 建立 | `media_begin`：沒有這個 mxc 的列就建（`complete = 0`）。已經 `complete = 1` 而且池檔在 → 不下載 |
| 每 1.5 秒（主檔） | `segments_written` ＝ 完整段數（只給顯示用） |
| 主檔完成 | `media_finish`：`pool_file`、`complete = 1`、`verified`（同一筆，§12.3）、`file_size`、`bytes_on_disk`；區塊沒帶 sha256 時 `hash = blake3:…` |
| seek 暫存檔 | 🚫 不進 DB：暫存檔自己就是記錄（§4.2 的重建） |

欄位記的是**段數**（池格式 v2 的 64 KiB 段），🚫 不是塊數：塊大小跟段大小無關。DB 一律經那台 server 的唯一寫入者（`ServerCache`，/docs/design/daemon/daemon-runtime.md §2）。

## 5. 下載：所有檔一起跑，每個檔一塊在途（維護者 2026-10-01、10-02 定）

### 5.1 誰在跑

`Download` 線照 /docs/design/daemon/link-requests.md：送收分開、動作封在請求裡。每個帳號一個**下載處理端**（一個 task），有兩條 queue：

| queue | 放什麼 | 誰消耗 |
|---|---|---|
| **收件 queue** | job：「下載這個檔」（`media.download`／`media.open`／`media.export_to`，§5.3）、「seek 要這一塊」（GET，§6） | 處理端：一個 job **處理一次就消耗掉** |
| **發送 queue**（線的） | 塊請求：`Read`（或第一次的 `Info`）＋ 回覆到了要做的動作（§5.2） | 線的發送端：取出就送，🚫 不等回覆；seek 的塊請求放**最前面** |

- **處理一個「下載這個檔」的 job 很快**：建 downloading 項、開主檔、塞第一個塊請求，就換下一個 job。所以 jobA、jobB、jobC 進來就是三個檔一起跑，
  發送 queue 變成 `A0 B0 C0 → A1 B1 C1 → …`（維護者 2026-10-02：「處理全部」）。每個檔同時只有**一塊**在途：第 n 塊落地的動作才塞第 n+1 塊。
- 🚫 不設同時幾個檔的上限（維護者 2026-10-02）。代價說清楚：頻寬由在跑的檔平分，排了 500 個檔，每個都要很久才完成；每個在跑的檔一個主檔把手加一個 64 KiB 的段緩衝。
  `Download` 是獨立的一條 WS，檔再多也只佔這條線，🚫 影響送訊息、訂閱；每個檔同時只有一塊在途、server 一次回一塊，所以各檔的塊是交替回來的（A1 B1 C1…），🚫 互相阻塞。
  逾時看的是整條線（/docs/design/daemon/link-requests.md §4）：有塊請求在等、而 60 秒內一個回應都沒收到才算，所以檔再多、線在交替回塊就🚫 逾時。
- **主檔與暫存檔只有一個寫入者**：落地都在處理端執行的動作裡做，處理端是一個 task；GET 要「還沒完成的主檔」與暫存檔也交給處理端讀（§7.2），🚫 不另開把手跟它搶。
- **線斷了**：在途的塊請求的動作收到 `Network`，同一個請求排回發送 queue 最前面；線重開（/docs/design/daemon/link-pool.md §3.1）之後接著送，進度停在原地
  （/docs/design/daemon/link-requests.md §4）。
- **第一次要用才起**，登出、換 session 時收（`Core::close_links` 一併收）：開著的主檔 fsync 留著，下次從檔案接著拉；收件與發送 queue 不保存（§11 第 2 條），等它的人收到錯誤。
- **同一台 server 的帳號共用池**（`m<media.id>` 是 server 層級的名字），所以同一個 mxc 同時只准一個處理端寫：開檔前先在 `Core` 的認領表登記（server dir ＋ mxc → 帳號＋處理端編號）。
  - **認領認的是處理端，🚫 只認帳號**：同一個帳號重登之後，舊處理端可能還被串流中的 GET 留著；只認帳號的話新舊兩個都能寫同一個檔、舊的收掉時也會放掉新處理端的認領。帳號只給 GET 找「誰在寫」用。
  - **認領就是「誰在下載」**（維護者 2026-10-04）：只有 job 認領。認領到的處理端是這個檔唯一碰主檔與 `m<id>.seek` 的，別的處理端🚫 碰這兩個檔。
  - **別的處理端也要下載同一個檔**：🚫 再下載一次、🚫 排著等。job 轉給正在下載的那個處理端（它的收件 queue 記在認領裡），回的是它的狀態（通常是 `downloading`）；
    要等它完成的（`media.export_to`）掛到它身上。交 job 的時候就看；交進來之後對方才開始的，認領不到時一樣轉過去。那個處理端剛好在收（認領還沒放）就回錯、請他再試；
    轉過去之後它才收，它收攤時把收件 queue 裡沒處理的 job、在跑的、等的人一起回錯、推一則失敗，🚫 悄悄消失。
  - **只是 seek**（這個處理端沒在下載這個檔）：🚫 認領、🚫 開檔、🚫 寫任何東西，拉到就交給 GET（§6.1）。所以播放永遠不擋下載。
  - job 停了（取消、失敗）、GET 也不再等它的塊，檔就關掉、放掉認領（檔留著，下次接著拉）。
  - 處理端被收時它的認領一定放掉（`Drop`，🚫 不靠 abort 剛好停在哪一行），包括認領了、還在等 DB 建列、還沒開好的那幾個；收尾時先寫 DB 記完成、**才**放掉認領——反過來的話，別的帳號會在 DB 記完成之前接手、把剛進池的檔重下一次。
  - **收就是停**：登出、換 session 時處理端當場 abort，🚫 只從表裡拿掉等最後一個人放手——串流中的 GET 握著它的話，它的線已經關了，GET 要的塊永遠等不到，開著的檔也不在掃描的保護名單上。GET 當場拿到錯、斷線。

### 5.2 job、downloading 表、塊請求的動作

```
DownloadJob { mxc, manifest }                       收件 queue 裡的東西：處理一次就消耗掉

downloading: HashMap<mxc, Arc<Downloading>>         每個帳號一張（維護者 2026-10-01）
Downloading {                                       一個正在下載的檔，它所有的塊請求共用這一份
    cancelled: AtomicBool,                          取消旗標
    done:      AtomicU32,                           已落地的塊數（= next）
    total:     AtomicU32,                           總塊數（0 = 還不知道）
}

塊請求（/docs/design/daemon/link-requests.md §3，資料不是閉包；請求本身就是動作）
    Info  { mxc }                                   Download/Info 回來：驗區塊 → 主檔走「下一塊」、掛著的 seek 送它們的 Read
    Chunk { mxc, index }                            Download/Read 回來：解開 → 主檔在等就落地、走「下一塊」；只有 seek 在等就存進暫存檔 → 交給 GET

在途表：塊請求 → 等它的人（Main ＝ 主檔、Seek ＝ 在等的 GET），同一個請求只送一次（§6.3）
```

- **什麼時候放進表**：處理「下載這個檔」的 job 時（維護者 2026-10-02：解析 job 的時候就建表）。
- **什麼時候拿掉**：最後一塊落地、或動作發現旗標是 true——**由處理端自己拿掉**，🚫 別人不替它拿。
- 動作只帶「是誰的、第幾塊」：處理端照 mxc 從表裡拿同一份 `Downloading`，設成 `cancelled` 之後收到的那一塊就只落地、🚫 不塞下一塊。
- `done`／`total` 也在這裡：`media.queue` 與進度推播（§5.5）直接讀它。

### 5.3 處理一個「下載這個檔」的 job：先看 DB 與檔案

**描述（金鑰）從哪來**（維護者 2026-10-07）：本地的 `media` 列說了算，它只在兩個地方生出來——事件寫進 `cache.db` 時（第一則引用這個 mxc 的事件，`INSERT OR IGNORE`），
與「從訊息點下載」時由那則事件建（列不在的話，例如 `media.delete_local` 刪過，§7.4）。列一旦在，🚫 有任何路徑換掉它的描述。

| 請求怎麼說 | 用哪份描述 |
|---|---|
| `{ mxc }` | **照列挑**：沒有列 → `1100`（叫 UI 從訊息點）；有列 → 這個帳號看得到、而且跟列說的是同一個檔的事件裡**第一則**的金鑰（`media::find_key_matching_record`）。都對不上（只看得到寫錯或偽造的）→ `1100` |
| `{ room, event_id, mxc? }`（從訊息點下載） | **由這則事件確定列**：列不在就照它建、補上這則跟列的連結（`Cache::media_link_event`）；用這則的金鑰。給了 `mxc` 就要是這則的附件，否則 `1100`。列在而描述不合 → 下表第一列 |
| `{ manifest }` | manifest 帶的；列不在由處理端照它建（§4.4） |

| 本地狀況 | 做什麼 |
|---|---|
| 列說的跟這次的描述對不上（格式、大小、塊大小；兩邊都帶 sha256 時 sha256），**不管下載完了沒、有沒有人正在下載** | **回 `Integrity`（1500）、🚫 下載、列與檔都🚫 動**。先記下的那份說了算：先到的是寫錯或偽造的事件，真的那則也會被拒——UI 拿到這個錯就是警告。要換成這次的，先 `media.delete_local`（§7.4）再從訊息下載（維護者 2026-10-07，取代 10-02 照 PR #14 的「沒下載完就丟掉重來」與 10-03 的「完整的才不刪」） |
| `media` 列 `complete = 1`、池檔在 | 🚫 不下載：已經有了 |
| 有本機原檔（/docs/design/rpc-specs/data-plane.md §8.1） | 🚫 不下載（`media.export_to` 例外，§7.3） |
| 這個 mxc 已經在 `downloading` 表、正在準備或在收件 queue 裡 | 🚫 不重複 |
| 別的處理端正在下載它 | 轉給那個處理端、回它的狀態（§5.1） |
| 有 pending 主檔 | **從檔案確定進度**：驗檔長、逐段解（§4.1 續傳）→ 已寫 `k` 段 → `next = ⌊k × segment_size / chunk_size⌋`；DB 的 `segments_written` 只當提示，🚫 不當依據 |
| 都沒有 | `next = 0` |
| `total` | 區塊在手（有 `file_size`、`chunk_size`）就算 `ceil(file_size / chunk_size)`；不知道就填 **0**，`Info` 回來再補 |

然後建 downloading 項、開主檔：還沒驗過就送 `Info`（主檔掛在上面），驗過了就走「下一塊」（§5.4）。
從收件 queue 拿出來到進 downloading 表之間（建列、認領、開檔要等 DB）算「準備中」：取消、重複排入、`media.queue` 都認得它。
📎 對比的只是記下來的字，🚫 算 hash：串流（`media.open`、GET）照用，每段讀出來時池的 AEAD 本來就驗過；整檔的 hash 留給匯出（§7.3）。
daemon 重開時 queue 不保存（維護者 2026-10-01）：之後再有人要這個檔，照上表從 DB 與檔案重建進度、接著拉。

### 5.4 動作：落地、看旗標、塞下一塊

```
「下一塊」（第 next 塊）：
    位置表有（§4.2）→ 從 seek 暫存檔讀那一格、用池金鑰解開、落地 → 再走「下一塊」      ← 不走網路
    在途表有（別人正在要這一塊，§6.3）→ 掛上去，那個回覆落地時一起處理
    都沒有           → 塞 Chunk { mxc, next }（排在發送 queue 尾巴），主檔掛在上面

Chunk { mxc, next } 回來、主檔在等：
    驗長度、用檔案金鑰解開（§3.3）→ 明文餵進主檔（湊滿一段才封、才 append，§4.1）→ done = next + 1
    cancelled == true      → 停：主檔與暫存檔留著；downloading.remove(mxc)            ← 取消在這裡生效
    next + 1 == total      → 收尾（§4.1）、刪暫存檔、推最後一則進度；downloading.remove(mxc)
    否則                    → next += 1，走「下一塊」
```

- **取消**在「一塊落地之後」生效：在途的那一塊會收完、寫完，所以主檔永遠停在整塊（整段）的邊界上。主檔的請求還排著沒送（例：線斷了）就當場拿掉、當場停。
- **壞檔**（§3.2、§3.3）或 server **正面拒絕**（`NotFound` 1501、`Forbidden` 1302）→ `downloading.remove(mxc)`、`media` 列 reset、主檔與暫存檔刪掉、推一則失敗。
- **其他的 server 錯**（token 過期 1301、限流 1401、server 內部錯 1901、不認得的碼）→ 這個檔停下、推一則失敗，**主檔與暫存檔留著**，再要一次從斷點接著拉。
  `Corrupt`（1002，server 解不開我們送的）跟形狀不對的回覆一樣重送一次。只有 GET 在等的請求失敗只回 GET，🚫 連坐停掉主檔（除非是上面那兩個拒絕）。
- **線斷、逾時**：照 /docs/design/daemon/link-requests.md §4，同一個請求排回最前面，進度停在 `next`。掛在上面的 GET 🚫 等線，直接收到錯（播放器會再要）；
  線整段 60 秒沒回應就是逾時（/docs/design/daemon/link-requests.md §4），同一個請求連續逾時 3 次就停下這個檔、推一則失敗，檔案留著（下次接著拉）。
  重送幾次記在處理端的在途表那一筆，**逾時與壞回覆各算各的**（逾時過一次，壞塊照樣還有一次重拉），重排、插隊、取消主檔時都沿用，🚫 歸零。
  GET 🚫 自己限時：處理端在線斷、逾時、驗不過時都會回它錯；播放器先放棄（斷線）就不等了。
- **沒人看的時候照樣拉完**。**暫停**之後才加：加的時候是「走『下一塊』時先看暫停旗標」，只停主檔，🚫 不停 seek。

**`media.cancel { mxc }`**：

| 這個 mxc 在哪 | 做什麼 |
|---|---|
| `downloading` 表裡（正在拉） | `cancelled = true`。在途的那一塊落地之後就停；請求還排著沒送就當場停。處理端自己從表裡拿掉 |
| 正在準備（還沒進表） | 記下來，處理端開始它之前看到就停 |
| 收件 queue 裡（還沒開始） | 拿掉（它不在表裡） |
| 都不在 | 回 `cancelled: false` |

### 5.5 進度：daemon 負責（維護者 2026-10-01）

背景下載沒有 HTTP 連線可以看，所以進度由 daemon 管、由 daemon 報：

- **狀態在 `downloading` 表裡**（`done`／`total`，§5.2）；DB 每 1.5 秒寫一次段數（顯示用，§4.4）。
- **推播 `media.download`**（要先 `subscribe`，/docs/design/rpc-specs/rpc-spec.md §4）：
  `{ mxc, state: "queued"|"downloading"|"verifying"|"complete"|"cancelled"|"failed", done: next, total, user, verified?, reason? }`（`verifying` 與 `complete` 帶的 `verified` 在 §12.3）。`queued` 是收到了、還沒處理。
  每個檔**最多每秒一則**，加上每次狀態改變一則（收到、開始、完成、取消、失敗）——🚫 不是每塊一則（那會把 `room.message` 擠掉，/docs/design/daemon/daemon-runtime.md §5.4）。
- **`media.queue`** 隨時可以問現在的樣子：在跑的（`downloading`）與還沒開始的（`queued`）。
- 播放中的進度照舊：就是那個 GET 收到多少 bytes。

`media.export_to`（匯出）也排隊（維護者 2026-10-01）：交一個 job、等它完成，再照 `media.kind`／`verified` 寫到使用者指定的位置（從池匯出🚫 再算 hash）；`no_cache` 版也排隊（§7.3）。

## 6. Seek

### 6.1 什麼時候算 seek

GET 要的那一段（§7.2），有任何一塊**不在**「本機原檔、完整主檔、主檔已寫的段、seek 暫存檔」之中 → 那幾塊要**現拉**，這就是 seek。
seek 🚫 不算下載：不建 downloading 項、不影響哪些檔在跑（維護者 2026-10-01）。

處理端的身分決定 seek 怎麼做（維護者 2026-10-04）：

| 這個處理端對這個檔 | seek 怎麼做 |
|---|---|
| 在下載它（認領了） | 主檔已封的段、`m<id>.seek` 有就給；現拉的存進 `m<id>.seek`，主檔追到時搬（§6.3、§6.4） |
| 已經完整 | 讀池檔 |
| 只是 seek（別人在下載，或沒人在下載） | 現拉、解開就交給 GET，🚫 認領、🚫 開檔、🚫 寫任何東西。只在記憶體記著驗過的 `Info`（最多 16 個檔），下一塊🚫 再問 |

- GET 先照 `find_writer_of` 找正在下載的那個帳號的處理端（它有主檔可以讀）；串流開始後對方才開始下載的，這條 GET 照舊是「只是 seek」，下一個 Range 才轉過去。
- 只是 seek 的代價：拉過的塊下載者用不到。正常播放走 `media.open`，它本來就會順便建下載，播放者就是下載者。
- 只是 seek 的 `Info` 或塊驗不過：錯的是 GET 帶來的描述（寫錯或偽造的事件），回錯、丟掉記著的那份，沒有檔可刪。

### 6.2 一個 seek 是一個 job，請求插到最前面

GET 把「明文第 `p` 個 byte」當成一個 job 交給處理端（一個事件一個 job，維護者 2026-10-02）。GET 🚫 自己算塊號（維護者 2026-10-04：HTTP Range 本來就是 byte）：
是哪一塊照處理端手上那個檔驗過的切法算（在下載的：`Info` 核過的；完整的：列上記的；只是 seek 的：GET 帶來的描述，`Info` 核過跟 server 一樣才拉），
回涵蓋 `p` 的那一塊與它的起點，GET 照起點切。處理它：

```
本地有（主檔已封的段、seek 暫存檔）→ 直接交給 GET
在途表有（主檔正好在拉這一塊，或別的 GET 要過）→ 掛上去，那個回覆到了一起交（§6.3）；還排著沒送的，一起提到最前面
都沒有 → 塞 Chunk { mxc, i } 到發送 queue 的**最前面**，這個 GET 掛在上面（區塊還沒驗過就先掛在 Info 上，Info 回來才送）
```

- 發送 queue 平常幾乎是空的（每個在跑的檔最多一個請求在排），插到最前面就是下一個送出去的，🚫 不等在途的那幾塊回來。
- ⚠️ 它省下的是「等前一個回覆回來才送」的那一個來回，🚫 不是前一個回覆的傳輸時間：server 對一條連線是照到達順序一次處理一個、回應排在同一條 TCP 上
  （/docs/design/daemon/link-requests.md §9）。所以 seek 的回覆還是排在已經在途的那幾塊後面。
- seek 跟在跑的檔可能是不同的檔：線是帳號的，不是檔的。播放器 seek 之後通常順著往下讀，所以 seek 那邊也是一塊接一塊地要。

### 6.3 一塊被 seek 拉到之後、同一塊被要兩次

`Chunk { mxc, i }` 回來、只有 GET 在等：

1. §3.3：驗長度、AEAD 解開 → 明文在記憶體。
2. **append 進 seek 暫存檔**的下一格 → fsync → `slots[i] = s + 1`（§4.2）。
3. 交給所有掛在這一塊上的人（GET；如果主檔剛好也走到這一塊，它也掛在上面，照 §5.4 落地、走下一塊）。

處理端記「哪個 `(mxc, 塊號)` 正在途」（/docs/design/daemon/link-requests.md §6）。播放器常同時開好幾條 Range，同一塊被要第二次時**掛上去、🚫 不重拉**。

### 6.4 主檔追到 seek 拉過的塊

就是 §5.4 的「下一塊」：位置表有就從暫存檔搬、**不走網路**；沒有（或那一格解不開）照常向 server 拉。

所以主檔永遠是「從第 0 段起連續」，不會有洞；暫存檔只是讓主檔追上來時快一點。最差狀況：同一塊在暫存檔與主檔各一份，**同時佔兩份空間**，主檔完成就刪掉暫存檔。暫存檔先不設上限（維護者 2026-10-01）。

## 7. 交給使用者：RPC 與 HTTP

### 7.1 RPC

| method | params | result | 說明 |
|---|---|---|---|
| `media.download` | `{ mxc } \| { room, event_id, mxc? } \| { manifest }`（剛好給一種，給了不只一種是參數錯，🚫 不猜哪個優先；從訊息點下載的那種可以多帶 `mxc`，要是那則的附件），`user?`、`server?` | `{ mxc, state, done, total, kind?, verified? }` | 建 job（§5.3；標準附件見 §12）。已經完整或有原檔就直接回 `complete`／`local_source`；`kind`、`verified` 是 `media` 列的（§12.3）。描述跟本地那一列不合 → 1500（§5.3） |
| `media.open` | 同上 | `{ url, mxc, mimetype?, size?, state, kind?, verified? }` | `url` 是 `/media/mxc/e-…`（/docs/design/rpc-specs/data-plane.md §8，不帶帳號）。`state`：`local_source`、`complete`、`downloading`、`verifying`、`queued`。`size` 只在事件沒給 `info.size` 的傳統檔不在（§12.2）。不完整也沒原檔 → 順便建 job |
| `media.queue` | `{ user?, server? }` | `{ items: [{ mxc, name?, state, done, total }] }` | 現在的樣子：在跑的（`downloading`）與還沒開始的（`queued`） |
| `media.cancel` | `{ mxc, user?, server? }` | `{ cancelled: bool }` | 正在拉 → 設 `downloading[mxc].cancelled`，在途的那一塊落地就停；還沒開始 → 拿掉（§5.4） |
| `media.delete_local` | `{ mxc, user?, server? }` | `{ mxc, removed: bool, cancelled: bool }` | 清掉這個 mxc 在本地的一切（池檔、半成品、seek 暫存檔、`media` 列），先取消這台 server 上所有正在下載它的（§7.4） |
| `media.export_to` | 同 `media.download` 的三種說法，加 `to`（URI；已經存在就覆蓋）、`no_cache?` | `{ to, bytes, source, kind, verified, hash? }`；`source` 是 `local_source`、`cache`、`server` | 匯出（§7.3）：沒有就排、等它完成才寫到 `to`；從池匯出🚫 再算 hash、只核大小，傳統加密的檔沒驗過或驗不過照匯、回 1501（`data` 是這份結果）；從本機原檔匯出一律整檔比 sha256。`to` 現在只收 `file://`。`no_cache`：這次為了匯出才下載的，匯出完就從池拿掉（別的 mxc 還指著、或有人正在讀，就只清這一列），池裡本來就有的不動 |
| 推播 `media.download` | — | `{ mxc, state, done, total, user, verified?, reason? }`；`state` 多一個 `verifying` | §5.5、§12.3。`complete` 一律帶 `verified`（0／1／2）；`reason` 只在 `failed` 帶 |

- 金鑰只從**這個帳號看得到的事件**裡找（§3.2）：同一份 `cache.db` 裡有同 server 別的帳號的事件，金鑰🚫 不跨帳號借。只給 `mxc` 時照本地那一列挑（§5.3）；找不到（沒有列、或看得到的都跟列對不上）是 `1100`，訊息叫你帶 manifest 或 `room` ＋ `event_id`。
- 一般 Matrix 帳號（走 matrix-sdk 的）沒有 `Download` 線：它只看得到標準 Matrix 附件（`kind` 2、3），走 §12 的 HTTP 下載；給它 `manifest` 回 `1100`。
- 分塊檔的下載一律走 WS 的 `Download` 線、標準附件一律走 HTTP（§12）：`transport` 參數對這幾支沒有意義。CLI 的 `download --no-cache` 不走下載處理端（直接逐塊寫到檔案，不碰池），等 CLI 改走 RPC 時一併收掉。

### 7.2 GET 的路由

`GET /media/mxc/e-…` 開出 mxc（URL 的格式、Host 檢查、狀態碼在 /docs/design/rpc-specs/data-plane.md §2、§8），然後把要的 Range **一段一段**分給來源，由上往下：

| 先後 | 來源 | 條件 |
|---|---|---|
| 1 | **本機原檔** | `media.source_uri` 解得出來、一般檔、大小對得上（/docs/design/rpc-specs/data-plane.md §8.1）。整個 Range 直接讀它，🚫 不碰池、不觸發下載 |
| 2 | **完整的主檔** | `media.complete = 1`：`PoolReader` seek 到起點往下讀 |
| 3 | **主檔已寫的段** | 這一塊在 `已寫段數 × segment_size` 之前：讀 pending 主檔（只讀完整的段） |
| 4 | **seek 暫存檔** | `slots[i] ≠ 0`：讀那一格 |
| 5 | **現拉（seek）** | §6 |

- 1、2 在 GET 這邊直接讀；3～5 照 byte 位置一塊一塊當成 seek 的 job 交給下載處理端（§5.1：還沒完成的主檔與暫存檔只有下載者碰；§6.1、§6.2）。處理時檔剛好完成了，就從完整的池檔讀。
- **URL 不帶帳號**：照本機已登入的帳號一個一個找有這個 mxc 紀錄的 `cache.db`，mxc 的 server_name 跟帳號網域一樣的先找。
  1、2 不要金鑰；要現拉就要那個帳號看得到帶金鑰的事件。有紀錄但沒有完整的檔、也沒有帳號拿得到金鑰 → 502；完全沒有紀錄 → 404。
- **現拉要用的描述（金鑰）照只給 `mxc` 的規則挑**（§5.3：這個帳號看得到、跟本地那一列說的是同一個檔的第一則）：一個帳號看到的都對不上（寫錯或偽造的事件）就換下一個帳號；
  都對不上 → 502（跟「沒帳號拿得到金鑰」同一個錯）。切片🚫 用這份描述（§6.2），GET 自己再核一次拿回來的那一塊真的涵蓋它要的位置，對不上就斷線。
- `HEAD` 回一樣的標頭、沒有 body。Range 只認單一一段（`bytes=a-b`、`bytes=a-`、`bytes=-n`），終點超過檔尾就截到檔尾；寫壞的、不只一段的就當沒帶、回整檔（RFC 9110 §14.2）；起點在檔尾或之後是 416。
- 一個 GET 可能橫跨好幾種來源（前面在主檔、後面要現拉）：照塊號一塊一塊決定，邊讀邊吐。
- 吐出去的是**明文**；記憶體裡同時最多一塊（或一段）。HTTP 回應用串流 body，背壓同上傳：播放器讀得慢，daemon 就晚一點讀下一塊。
- 上游慢就停著等、連線不斷；拿不到才斷（/docs/design/rpc-specs/data-plane.md §8）：body 已經開始吐就沒辦法改狀態碼，所以是讓 body 出錯、連線斷掉，播放器知道沒收完（🚫 不假裝結束）。

### 7.3 匯出：`media.export_to`（維護者 2026-10-02）

使用者明說要把明文放到自己選的位置（不是「存檔」：池本來就存著，只是在加密區裡）。**明文落地是使用者要的**（/docs/design/rpc-specs/local-interface.md §8）。

- **`to` 是 URI**，跟 `media.create` 的 `source_uri` 同一套解法（/docs/design/rpc-specs/data-plane.md §8.1）。現在只收 `file://`（例 `file:///tmp/a.mp4`）；
  `http://127.0.0.1:…/from_ui/mxc/…`（daemon 用 HTTP PUT 丟給 UI）之後才做，現在回參數錯。
- **來源**：本機原檔能驗（區塊帶 sha256）就從它匯出；不能驗、或驗不過（原檔上傳後被改過），就從池匯出——池裡沒有就排進下載、等它完成。
  原檔在池外、沒有任何 AEAD 保護，所以**從原檔匯出一律整檔比 sha256**（照舊）。
- **從池匯出🚫 再算 hash，照 `media.kind`／`verified` 決定回什麼**（維護者 2026-10-06，§12；跟 GET 同一個約定，/docs/design/rpc-specs/data-plane.md §8.2）：

  | `kind` | `verified` | 從池匯出時 | 回 |
  |---|---|---|---|
  | 1 `WbfChunked` | 不看 | 照匯，只核大小 | 成功 |
  | 2 `MatrixEncrypted` | 1 驗了、正確 | 照匯，只核大小 | 成功 |
  | 2 `MatrixEncrypted` | 0 或 2 | **照匯**（資料照給） | **1501 `unverified`**，`data` 是成功時會給的那份結果（`to`、`bytes`、`source`、`kind`、`verified`）：檔已經在 `to`，要不要留由 UI 決定、跟使用者講 |
  | 3 `MatrixPlain` | 不看（沒有 hash） | 照匯，只核大小 | 成功 |

  - 為什麼🚫 再算：`kind` 1 每塊下載時各自 AEAD 驗過、池裡每段讀出時又過池的 AEAD，整檔 hash 是多算一次；`kind` 2 下載完已經自動驗過、結果記在 `verified`（§12.3），驗過就🚫 再驗。
  - 匯出本來就等下載完成（還沒完成就排、等），所以到匯出那一刻 `kind` 2 的 `verified` 已經是 1 或 2；0 只會出現在「完成了卻沒驗」這種不該有的列，照 0 處理（1501）。
  - 成功的結果也帶 `kind`、`verified`，前端不必另外問。

- **暫存檔再改名照舊**：先寫一個**這次自己獨占建立**的暫存檔（`<to>.partial.<pid>-<n>`，名字被占了就換一個）、fsync，大小對上才改名成 `to`。
  🚫 開、🚫 刪別人的檔（旁邊本來就有的 `<to>.partial` 之類），失敗只刪自己建的那一個；兩個匯出到同一個 `to` 互不踩。
  這一步防的是「寫到一半」，不是竄改：池讀出來的每一段都過了池的 AEAD。
- **`to` 已經存在就覆蓋**（維護者 2026-10-03：要不要覆蓋是 UI 先問使用者，daemon 🚫 再擋）。
  對不上 → 刪掉暫存、`to` 🚫 被動過、回 `Integrity`。
- 跟串流分開（維護者 2026-10-02）：`media.open`／GET 是串流，🚫 算整檔 hash，每段讀出來時池的 AEAD 已經驗過。
  📎 10-02 原本定「匯出要整檔驗（BLAKE3 ＋ 有就 SHA-256）」；10-06 改成上面那張表：`kind` 1 的整檔 hash 是重複的，`kind` 2 收尾時驗過、記在 `verified`。
- 🔜 之後的形狀：還沒下載完就先交 job，下載完成時**自動匯出**、RPC 不必等著；現在是等它完成才回。

### 7.4 清除本地：`media.delete_local`（維護者 2026-10-07）

清掉一個 mxc 在本地的一切：池檔、pending 主檔、`m<id>.seek`、`media` 列（`event_media` 的連結 ON DELETE CASCADE 一起沒）。
用途：§5.3 的 1500（先到的是寫錯或偽造的事件，真的那則被拒）之後換成真的，或使用者就是要把這個檔從本機拿掉。

```jsonc
// 示意（形狀照 /docs/design/rpc-specs/rpc-spec.md §1）：正在下載時清掉
→ { "method": "media.delete_local", "params": { "mxc": "mxc://localhost/000000000000004d" }, "id": 7 }
← { "code": 0, "msg": "ok", "id": 7, "result": { "mxc": "mxc://localhost/000000000000004d", "removed": true, "cancelled": true } }
```

1. **先取消，同一台 server 上所有正在下載它的**（維護者 10-07 補充）：傳統檔的 task（§12，登記表以 server 為鍵）與**每個帳號**已經起了的分塊處理端都交 `cancel`，
   🚫 第一個成功就停（同 server 別的帳號可能也在拉它），🚫 為了取消新起一個處理端。`cancelled` 是「有沒有任何一個被取消」。
2. **等寫入者放手**，最多 5 秒：分塊的取消會把半成品留著續傳（§5.4），在途的那一塊落地、處理端關掉檔之前🚫 刪。等不到（例如 GET 還在讀下載中的檔）→ `1100`、什麼都🚫 刪。
3. **刪**（`media::del_local_copy`）：先刪檔再刪列（同配額清理，中途失敗時列還在、掃描收得掉）。池檔還有別的 mxc 指著、或有人正開著讀，就🚫 刪檔，列刪了之後沒人指著，掃描會收。
   本地本來就沒有這一列 → `removed: false`、照樣成功。

之後：只給 `mxc` 的下載回 `1100`（沒有列，§5.3）；從訊息點下載（`{ room, event_id, mxc? }`）由那則事件重建列、從頭下載。
正在下載的傳統 task 收尾時再問一次列（§12.1）：列被刪了、或刪了又由別則事件重建，它的結果🚫 記上去、🚫 去清別人的列。

## 8. 斷在哪裡、會留下什麼

| 斷點 | 留下的 | 下次 |
|---|---|---|
| 主檔寫到一半（段沒寫完） | 檔長不是整數倍 | 截掉那一段，從涵蓋它的塊接著拉 |
| 主檔的長度落地、內容沒落地 | 檔長是整數倍，但最後幾段解不開 | 逐段解，截到第一個解不開的段 |
| 暫存檔寫到一半 | 檔長不是整數倍 | 半格留著、它的格號跳過（§4.2：🚫 同一個 nonce 封兩份明文）；那一塊之後再拉 |
| 暫存檔某一格內容沒落地 | 那一格解不開 | 讀到時拿掉、重拉 |
| 主檔完成、改名前 | pending 主檔完整 | 續傳時整檔驗完 → 直接收尾 |
| 改名後、DB 記完成前 | 池檔在、列說沒完成 | 下次重下一次，收尾時 `adopt` 發現同 hash 的池檔 → 去重、記完成（在那之前掃描先跑到的話，沒人指著的池檔被刪，結果一樣） |
| 收尾後、刪暫存檔前 | 多一個 `.seek` | 掃描刪（列已經完成，§4.3） |
| 匯出寫到一半（程序被殺、斷電） | `to` 旁邊多一個 `<to>.partial.<pid>-<n>`；`to` 沒動 | 留著，🚫 自動清：那是使用者選的目錄，daemon 🚫 掃別人的目錄。名字唯一，下次匯出不會撞到它 |

**每一筆寫入都是「整段／整格 ＋ fsync ＋ 才改記憶體或 DB」**，所以被 force 關掉最多丟最後一筆，🚫 不會留下讀得到的壞資料。

## 9. 跟池格式 v1 那一版的差異

| v1 | v2（現在） |
|---|---|
| 池格式 v1：沒有長度欄、最後一段可以短、有暫定段（每 1.5 秒截掉重寫） | v2：每段寫滿、長度在密文裡、沒有暫定段（§4.1） |
| `media::fetch` 整個檔握著 `Download` 線 | 塊請求送收分開、動作封在請求裡（§3.1），seek 的請求插到發送 queue 最前面 |
| 下載是 `media.save_to`（現在的 `media.export_to`）叫的時候當場拉 | 每帳號一個處理端，所有檔一起跑、每檔一塊在途（§5） |
| 續傳信 DB 記的塊數 | 檔案本身就是進度（§4.1）；DB 的 `segments_written` 只給顯示 |
| 媒體繞過唯一寫入者、自己開 `cache.db` | 一律經 `ServerCache`（/docs/design/daemon/daemon-runtime.md §2） |
| 不做 seek（/docs/design/media/media-pool.md §6） | seek 暫存檔 ＋ 位置表（§4.2、§6） |
| 沒有讀的 HTTP | `media.open`、`GET /media`（§7） |

/docs/design/media/media-pool.md 的「沒有 bitmap、檔要嘛完整要嘛是連續的前綴」**不變**：主檔還是連續前綴；seek 暫存檔是旁邊多出來、用完就丟的東西。

## 10. 測試在哪

| 測什麼 | 在哪 |
|---|---|
| 池格式 v2：1 byte、剛好整段、跨段；半段截掉、第一個解不開的段截掉；翻 bit／錯金鑰／段搬位置／檔長不是整數筆都拒；長度欄超過 segment_size 拒；收尾過、沒改名的檔續回整個、不准再寫；`owner` 對不上或 v1 不續；寫的中途讀回已封的段 | sdk `media_pool::tests` |
| seek 暫存檔：照到達順序 append、最後一塊寫滿一格；寫失敗燒掉那個格號；重開🚫 截檔，半格與塊號超出的格跳過、號碼不再用，重建位置表、重複留第一個；改塊號／翻 bit／別的 mxc／chunk_size 不同都不收；位置表太大不給 | sdk `seek_store::tests` |
| `MediaDownload` 對假 server：下載進池、去重、從完整的段續傳（塊跟段不對齊）、seek 拉過的塊主檔不再上網、區塊對不上就刪掉暫存檔；配額清理；掃描（不碰正在下載的、刪 v1、刪過期的）；清除本地（暫存檔還有人在寫就整個拒、放手後半成品與列都刪） | sdk `tests/media_cache.rs` |
| 匯出的暫存檔：旁邊本來就有的 `<to>.partial` 不論成敗都原封不動、成功時覆蓋既有的 `to`、兩個匯出到同一個 `to` 互不踩（失敗的只刪自己的）、不留暫存檔 | sdk `media::tests` |
| 線的發送 queue：送出🚫 等回覆、回覆倒著回也找得回自己的動作、插到最前面的先送、還沒送的拿得回來、線斷了在途的都回 `Network` | core `link_requests::tests` |
| 下載處理端：一塊一個 `Read`、三個檔一起跑時發送 queue 是 `A B C、A B C`、每檔同時一塊在途、重複要不重複、取消只再落地在途那一塊且再要接得上、seek 的請求在發送 queue 最前面而且同一塊只上網一次（含「主檔正在拉的那一塊」）、線斷了在途的請求重排、進度停在原地、線回來接著拉、線斷時進來的 seek 線回來先送、線斷時取消當場停、一塊壞了重拉一次／連兩次就當壞檔不留檔、準備中的 job 也取消得掉、`export_to`（含 `no_cache`、從池匯出🚫 再算 hash（列改指到別的池檔也照給）、推播依序 `verifying` → `complete` 帶 `verified`、快取命中回那一列的 `verified`、傳統加密檔驗了正確才可信（資料照讀）、原檔改過就改從池匯出）、描述對不上一律回錯（完整的快取與沒下載完的半成品、列都🚫 動，清掉之後才換得掉）、清除本地會取消正在跑的下載、等處理端關掉檔才刪、server 內部錯留著半成品而 NotFound 刪掉、逾時過一次壞塊照樣重拉一次、別的處理端在下載就把 job 與等的人轉過去（回 `downloading`、自己不上網；轉過去之後它才收，等的人當場拿到錯）、處理端被收會放掉認領、認領認處理端不認帳號（舊處理端放不掉新的、收了的不再收 job）、收掉的處理端讓等塊的 GET 當場拿到錯、只是 seek 🚫 認領、🚫 寫任何檔、下一塊🚫 再問 `Info`、別的帳號下載不等它、seek 照檔驗過的切法算塊號（描述寫錯塊大小照樣給對的 bytes）、只是 seek 的描述驗不過🚫 刪主檔、GET 只用跟本地那一列對得上的描述 | core `download_queue::tests` |
| URL：讀與上傳的 URL 不能互換；Range 解析 | daemon `data_plane::tests` |
| HTTP：未解鎖 503、別的 daemon 發的／用途不對／本機沒紀錄 404、方法不對 405 | daemon `tests/data_plane.rs` |
| 推播 `media.download` 的欄位 | daemon `push::tests` |
| `media` 列：事件只從這個帳號看得到的裡列、刪列連結跟著沒、從看得到的事件重建（看不到的🚫）、重連不重複 | sdk `cache::tests`（`a_deleted_media_row_is_rebuilt_from_the_event_it_is_downloaded_from`） |
| `media.download` 的參數：`{ mxc, room }` 缺 `event_id`、`media.delete_local` 沒 `mxc` 是參數錯 | daemon `handle::tests` |
| 真 server：池清掉 → `media.open { room, event_id }` → 從中間 Range（seek，206）→ 等它拉完 → 整檔對、有完成的推播、暫存檔不見了 | daemon `tests/real_server.rs` 的 `an_attachment_goes_over_the_data_plane_into_plain_and_encrypted_rooms` |

## 11. 維護者定的

2026-10-01：

1. **v1 的池檔一律當壞檔重下**（它是快取）：啟動掃描遇到 v1 的完成檔與暫存檔都刪，`media` 列 reset。
2. **queue 不跨 daemon 重開保存**：進度照 DB 或直接讀檔案大小確定（§5.3），🚫 不另存 queue。
3. **匯出也排隊**（§5.5；10-02 起叫 `media.export_to`，§7.3）。
4. **背景下載的進度由 daemon 負責**（§5.5）：job 記著、推播 `media.download`（每個 job 最多每秒一則＋狀態改變）、`media.queue` 可以問。
5. **一塊一步**（§5.4）：「拉第 `next` 塊 → 落地 → 觸發後續（`next + 1`）」，取消插在落地之後。
6. **取消旗標放在 `downloading` 表**（§5.2）：key 是 mxc，所有塊共用同一份；設成 true 就只處理當下那一包；拉完或取消時自己從表裡拿掉。

2026-10-02：

7. **送收分開、動作封在請求裡**：「拿到第 n 塊之後要 n+1」是塊請求帶著的動作，回覆到了才執行；五條線都照這個做法（/docs/design/daemon/link-requests.md）。
   預設無序、無狀態，🚫 不靠 WebSocket 的順序。
8. **job 處理一次就消耗掉，所有檔一起跑**（§5.1）：jobA、jobB、jobC 都處理掉，發送 queue 變成 `A B C、A B C`；每個檔同時一塊在途。🚫 不設同時幾個檔的上限。
9. **seek 是一個事件一個 job**（§6.2）：它的塊請求放到發送 queue 最前面。
10. **解析 job 的時候建 downloading 項**（§5.2）。
11. **逾時看整條線**：有請求在等、而線整段沒有回應才算（/docs/design/daemon/link-requests.md §4）；心跳每 24 秒一定送（/docs/design/daemon/ws-receive-dispatch.md §5.1）。
12. **快取命中要跟這次的描述對得上**（§5.3）：大小、塊大小（兩邊都有時 sha256）對不上就丟掉重來，照 PR #14 的規則。（10-07 改成一律回錯、🚫 丟，第 23 條。）
13. **`media.save_to` 改名 `media.export_to`、`to` 收 URI**（§7.3）：現在只做 `file://`；串流不算整檔 hash。（10-02 原本定「匯出要整檔驗過」，10-06 改成照 `kind`／`verified`、從池匯出🚫 再算，§7.3、§12.3。）

2026-10-03：

14. **`to` 已經存在就覆蓋**（§7.3）：要不要覆蓋是 UI 先問使用者，daemon 🚫 再擋。
15. **已經完整下載、驗過的快取🚫 因為一則描述對不上而刪**（§5.3）：錯的是那則描述，回錯給請求者。（10-07 起沒完整的也一樣，第 23 條。）

2026-10-04：

16. **GET 只交 byte 位置**（§6.2）：HTTP Range 本來就是 byte。是哪一塊照處理端手上那個檔驗過的切法算，🚫 照 GET 帶來的描述。
17. **一個 mxc 只有一個處理端碰**（§5.1、§6.1）：主檔與 `m<id>.seek` 都歸正在下載的那個處理端。別的處理端要下載 → 🚫 再下載，轉過去、回 `downloading`；
    只是 seek → 拉了就交出去，🚫 寫任何檔。認領記的是處理端（連線），🚫 只記帳號（重登前後的兩個處理端是兩個）。

2026-10-06：

18. **傳統格式的附件也進同一個池**（§12）：`kind` 2、3 照池格式 v2 寫主檔、收尾改名進池；完成之後跟 `kind` 1 分不出來。
19. **`GET` 可以邊下載邊讀、驗證中也能讀，不論哪一種**（§12.2）：資料照給，**狀態碼說它可不可信**——`kind` 1 不檢查、一律 2xx；
    `kind` 2 沒驗過或驗不過是 **412**、body 照給；前端看狀態碼決定，堅決要拿就照拿（/docs/design/rpc-specs/data-plane.md §8.2）。
20. **`media` 表加 `kind` 與 `verified`，都用整數**（/docs/design/storage/local-cache-db.md §5）：程式裡是 enum，DB 存整數。
    `verified`：0 還沒驗、不知道；1 驗了、正確；2 驗了、不正確。驗過就🚫 再驗。
21. **下載完自動驗證**（§12.3）：推播 `media.download` 先報 `verifying`，驗完報 `complete` 帶 `verified`（成功 1、失敗 2）。驗不過🚫 刪檔。
22. **從池匯出🚫 再算 hash，跟 GET 同一個約定**（§7.3）：`kind` 1 不檢查；`kind` 2 沒驗過或驗不過照匯、回 1501 `unverified`。

2026-10-07（PR #75 審查 cirno 🟡1 之後）：

23. **本地的 `media` 列說了算，描述不合一律回錯**（§5.3、§12.1，分塊與傳統兩條管線都是）：只給 `mxc` 的下載照列挑跟它一致的事件的金鑰，沒有列就是空的、回錯；
    從訊息點下載（帶 `room`、`event_id`，可以再帶 `mxc`）由那則事件確定列；列已經在而描述不合 → RPC 回錯（UI 拿到的警告）、🚫 下載。
    取代第 12 條的「沒下載完就丟掉重來」：沒有任何路徑會換掉列的描述。
24. **新增 `media.delete_local`**（§7.4）：清掉本地資料與 `media` 列，而且**先取消當下所有正在下載同一個 mxc 的**。

## 12. 傳統格式的附件（`kind` 2、3）（維護者 2026-10-06）

wbf 帳號與一般 Matrix 帳號都會收到標準 Matrix 附件（`m.file`／`m.image`… 帶 `file` 或 `url`），它們不是分塊的。
哪一種看**事件內容**（/docs/design/rpc-specs/data-plane.md §7.1），🚫 看帳號：同一個 wbf 帳號的房裡可以同時有 `kind` 1 與 `kind` 2 的檔。

### 12.1 從 homeserver 到池

```
GET /_matrix/client/v1/media/download/{server}/{media_id}（帶這個帳號的 access token；一個請求從頭到尾）
   │ 密文一段一段進來
   ├─ kind 2：餵 SHA-256（算密文，最後跟 file.hashes.sha256 比）→ AES-256-CTR 解開
   ├─ kind 3：原樣
   ▼ 明文
   湊滿 64 KiB 一段 → 池金鑰封起來 → append 到 pending/m<id>（池格式 v2，§4.1，跟 kind 1 同一個格式、同一支寫入）
   同時餵 BLAKE3（池檔名）
   ▼ 讀完 → 自動驗證（§12.3）：推播 state = verifying
   kind 2：密文 SHA-256 對上   → 收尾（adopt、media_finish：complete = 1、verified = 1）→ 推播 complete、verified = 1
           對不上             → 一樣收尾、🚫 刪檔（complete = 1、verified = 2）→ 推播 complete、verified = 2
   kind 3：大小對得上事件的 info.size（有給的話）→ 收尾（complete = 1、verified = 0），🚫 verifying（沒有 hash 可比）
```

- **🚫 走 `Download` 線**：那條是 wbf 的 `Download/*`。傳統下載是一個普通的 HTTPS 請求，每個檔一個。
  **每個檔一個 task**（`crates/wbf-core/src/matrix_download.rs`），🚫 進分塊的下載處理端（§5.1）：那邊的單位是「線上的一塊」，這邊是「一條 HTTP 從頭讀到尾」，
  共用的只有池、`cache.db` 的列、取消（`media.cancel`）與推播 `media.download`。這個 task 是那個主檔唯一的寫入者，還在下載時 GET 要的明文也問它（§12.2）。
  同一個 mxc 第二次要（`media.download`／`open`／`export_to`）就掛到同一個 task 上，🚫 再起一個。所有檔一起跑照舊。
- **解密用上游的 `AttachmentDecryptor`**（`matrix-sdk-crypto`），🚫 自己刻 AES-CTR。它在讀到結尾時才比 hash、對不上回錯——正好是這裡要的時機。
  它吃同步的 `Read`：餵它的是一個記憶體佇列，HTTP 的下一段進來才放進佇列、佇列有東西才叫它解（佇列空著叫＝它當成讀到結尾、拿去比 hash），
  讀完才在佇列空著時叫最後一次。解一段只是幾十 KiB 的 XOR，🚫 `spawn_blocking`。實作在 `crates/wbf-sdk/src/matrix_media.rs`。
- **端點**：先 `GET /_matrix/client/v1/media/download/…`（要驗證的那條），server 回 404 `M_UNRECOGNIZED`（舊 server 沒有）才退到 `/_matrix/media/v3/download/…`。
  mxc 只收規格允許的字元（server_name＝主機名＋埠、media_id＝`[A-Za-z0-9_-]`）：它要拼進 URL 的路徑，🚫 讓 `/`、`..`、`?` 混進去。
- **逾時**看整條線多久沒回應（60 秒，/docs/design/daemon/link-requests.md §4 同一個數），🚫 整個檔限時。
- **進度** `done`／`total` 的單位是池的 64 KiB 段（傳統檔沒有塊）：`total` 從事件的 `info.size` 算，沒給就用列上記的大小，都沒有是 0（前端只能顯示「下載中」）；推播與 `media.download` 的回應用同一個數（PR #75 審查 salvia 2）。
- **背壓**：池寫得慢就晚一點讀 HTTP 的下一段；記憶體裡同時只有幾段。🚫 整檔讀進記憶體（🚫 用 matrix-sdk 的 `get_media_content`，它回 `Vec<u8>`）。
- **金鑰從哪來**：跟 §3.2、§5.3 一樣——只給 `mxc` 照本地那一列挑引用它、跟列一致的事件，從訊息點下載就是那則；讀 `content_json` 的 `file`（`key`、`iv`、`hashes.sha256`、`v`）。
  `v` 不是 `"v2"`、缺任何一個欄位 → 這個 mxc 當壞的（fail closed），🚫 試著解。
- **同一個 mxc、兩份描述**（§5.3 的同一條規則，維護者 2026-10-07）：列記的 `media.hash`（`kind` 2 是事件的 `file.hashes.sha256`，/docs/design/storage/local-cache-db.md §5）、
  格式、大小（兩邊都有時）跟這次事件的比，檔名、型別🚫 比（轉發改名還是同一個檔）。對不上 → **回 `Integrity`、🚫 下載、列與檔🚫 動**，不管下載完了沒、有沒有人在下載。
  先到的偽造事件會擋住後來的真事件，UI 拿到的 1500 就是警告；要換成真的先 `media.delete_local`（§7.4）再從真的那則下載。
  📎 10-06 的第一版是「還沒完整、沒人在下載就換成這次的描述重下」，PR #75 審查（cirno 🟡1）指出格式不同時那條永遠到不了，維護者 10-07 改成上面這樣。
  - 正在下載的 task 照哪份描述在寫記在登記表上：另一份描述來要，🚫 掛上去（拿到的會是別把金鑰解的資料）、回 `Integrity`——比列之後消費端自己再問一次。
  - task **開頭**再問一次列（PR #75 審查 salvia 1）：起 task 前讀的列可能已經舊了（上一個 task 剛收尾），完整就直接收尾、🚫 重下；列不在或換了就🚫 下載。
  - 收尾前 task 再問一次列：下載途中列被刪了（`media.delete_local`）、或刪了又由別則事件重建，這次的結果🚫 記上去，當下載失敗、半成品丟掉，🚫 去清別人的列。
  實作：`crates/wbf-sdk/src/media.rs` 的 `is_same_matrix_file`／`is_same_matrix_description`／`find_key_matching_record`。

### 12.2 GET：邊下載邊讀、驗證中也能讀，狀態碼說可不可信

- 路由照 §7.2，只有兩個差別：**沒有 seek 暫存檔、沒有現拉**（一個 HTTP 從頭讀到尾，拉不了第 n 塊）。GET 要的位置還沒寫到，就停著等主檔寫到那裡。
  事件沒給 `info.size`：不知道總長就給不了 `Content-Length` 與 Range，這個 GET 等整檔下載完才從池給。
- **資料照給，狀態碼說它可不可信**（維護者 2026-10-06 的約定，/docs/design/rpc-specs/data-plane.md §8.2）：
  `kind` 2 的 `verified` 是 0（下載中、驗證中）或 2（驗不過）→ **412**、body 照給；是 1 → 200／206。`kind` 1、3 一律 200／206。
  從驗證中變成驗完的那一刻，已經在吐的 GET 🚫 中斷：狀態碼在開始時就送了，前端想知道結果看推播（§12.3）或再問一次（`HEAD`）。
- `kind` 2 在整檔 hash 比對之前交出去的位元組**還沒驗過**（AES-CTR 沒有防竄改，/docs/design/rpc-specs/data-plane.md §7.3）：這就是 412 的意思，要不要用由前端決定。

### 12.3 驗證：下載完自動跑，`verified` 記結果、驗過就🚫 再驗（維護者 2026-10-06）

`media.verified`（/docs/design/storage/local-cache-db.md §5）是整檔跟發送者給的 hash 比對的結果，程式裡是 enum、DB 存整數：

| 值 | 程式裡的名字 | 意思 |
|---|---|---|
| 0 | `Unknown` | 還沒驗、不知道（下載中、驗證中，或根本沒有 hash 可比） |
| 1 | `Matched` | 驗了，正確 |
| 2 | `Mismatched` | 驗了，不正確 |

**誰、什麼時候驗**：下載讀完那一刻**自動觸發**，🚫 等誰來叫。hash 是下載途中一段一段餵的，所以「驗證」就是收尾時把算好的值拿去比、寫結果：

| `kind` | 比什麼 | 結果 |
|---|---|---|
| 1 `WbfChunked` | 區塊有帶 `sha256` 就比整檔明文的 SHA-256（§4.1 寫入第 3 步本來就在算） | 1 或 2；區塊沒帶 → 🚫 驗，留 0 |
| 2 `MatrixEncrypted` | 密文的 SHA-256 跟事件的 `file.hashes.sha256` | 1 或 2 |
| 3 `MatrixPlain` | 沒有 hash | 🚫 驗，留 0 |

- **驗不過🚫 刪檔、🚫 當成下載失敗**：照樣收尾（`complete = 1`），記 `verified = 2`。資料留著，讀的時候用狀態碼講（GET 的 412、匯出的 1501，/docs/design/rpc-specs/data-plane.md §8.2）。
  📎 這改掉了 §4.1 原本「`kind` 1 區塊 sha256 對不上是壞檔」：要不要用由前端決定，daemon 只把事實記下來。
- **`complete` 與 `verified` 同一筆寫入**（`media_finish`）：🚫 有「完成了卻還沒驗」的中間狀態落在 DB 裡；驗證中當掉，下次照 §12.4 從頭重下。
- **驗過（1 或 2）就🚫 再驗**：匯出、GET、快取命中都只看這一欄，🚫 重算。要重驗只有一條路：把這個檔從池拿掉、重下。
- `adopt` 去重（同一個明文被兩個 mxc 指著）：`verified` 記在 `media` 列（每個 mxc 一列），🚫 記在池檔。另一個 mxc 指到同一個池檔，它自己的列照它自己的收尾寫。

**進度推播**（`media.download`，§5.5）：要驗的檔（`kind` 2、帶 `sha256` 的 `kind` 1）在讀完時多一則、收尾時一則：

```jsonc
{ "user": "@alice:localhost", "mxc": "mxc://…", "state": "verifying", "done": 812, "total": 812 }       // 讀完了、正在比 hash
{ "user": "@alice:localhost", "mxc": "mxc://…", "state": "complete", "done": 812, "total": 812, "verified": 1 }   // 驗證成功
{ "user": "@alice:localhost", "mxc": "mxc://…", "state": "complete", "done": 812, "total": 812, "verified": 2 }   // 驗證失敗（資料仍在）
```

- `state` 多一個值 `verifying`；`complete` 一律帶 `verified`（0／1／2），🚫 只在要驗的檔帶——前端不必分辨「沒帶」與「0」。
- `verifying` 很短（hash 是邊下載邊算的，收尾只是比一次），但照樣發：前端可以顯示「驗證中」，也讓「下載完」與「可以信」這兩件事在推播上分開。
- 已經完成的檔再被要求下載（快取命中）：回 `complete` 時照樣帶這一列的 `verified`，🚫 再驗。

### 12.4 🚫 續傳、🚫 seek（第一版）

- daemon 重開、取消後再要：`kind` 2、3 的 pending 主檔**直接刪掉、從頭重下**（§4.1 的續傳只給 `kind` 1）。檔受 server 的上限（通常幾十 MB），而且是快取。
- 要續傳得有兩樣：server 支援 HTTP Range（規格沒保證），以及「從任意位置開始的 CTR」與重算前面那段密文的 SHA-256（拿池裡的明文用同一組 key／IV 再加密一次就是原本的密文）。
  上游的 `AttachmentDecryptor` 不支援從中間開始，要自己用 `ctr` 的 seek——之後真的需要再做。

### 12.5 測試要釘的

- 假 server，`kind` 2 對的檔：推播依序 `downloading` → `verifying` → `complete` 帶 `verified: 1`；列 `complete = 1`、`verified = 1`；完成後 GET 是 200／206。
- 假 server，`kind` 2 翻一個密文 bit：下載中 GET 是 **412、body 照給**（跟 200 會給的 bytes 一樣）；收尾🚫 刪檔、推播 `complete` 帶 `verified: 2`、列 `verified = 2`；之後 GET 仍是 412、body 照給。
- 給了資料的 GET／HEAD 回應（200／206／412）都帶 `Wbf-Media-Kind`、`Wbf-Media-Verified`（416、404、5xx 🚫 帶）；有 Range 時 412 一樣帶 `Content-Range`、只給那一段。
- `kind` 1：下載中、完成後都是 200／206，不論 `verified`；區塊 sha256 對不上 → `verified = 2`、🚫 當壞檔、GET 照樣 2xx。
- `kind` 3：🚫 `verifying`、完成時 `verified = 0`、GET 2xx；`info.size` 對不上 → `Integrity`。
- 驗過的檔🚫 再驗：快取命中、匯出、GET 都🚫 再餵任何 hash 器。
- 同一個 mxc 兩份描述（§12.1）：偽造的先到 → 從真的那則要 1500、🚫 起 task、列🚫 動；給的 `mxc` 不是那則的 → 1100；`media.delete_local` 之後從真的那則下載、`verified = 1`；
  之後拿偽造那份來要 → 1500；只給 `mxc` 時用的是跟列一致的那則（偽造那則比較早寫進來也🚫 挑它）；一份正在下載時另一份來要 → 1500、正在下載的照常完成。
- 清除：正在下載的被取消（`cancelled: true`）、半成品與列都沒了；只給 `mxc` 下載回 1100、從訊息點下載由那則重建、從頭下載完成。
- task 開頭看到列已經完整（salvia 1 的窗口）→ 直接收尾、推播只有一則 `complete`、🚫 上網。
- 取消：推播 `cancelled`、半成品刪掉、列 `complete = 0`；再要一次從頭下載、完成。
- 從池匯出：`kind` 1、3 照匯、成功；`kind` 2 `verified = 1` 成功、`verified = 2` **照匯**（`to` 有檔）而且回 1501、`data` 是那份結果。
- 跟 `AttachmentEncryptor` 互通：我們上傳的（/docs/design/rpc-specs/data-plane.md §7.2）用上游的 `AttachmentDecryptor` 解得開，反過來也是。
- 真 server：用 matrix-sdk 的 Client（等同 Element 的格式）在加密房送一個附件，wbf 帳號 `media.open` → GET 整檔對。

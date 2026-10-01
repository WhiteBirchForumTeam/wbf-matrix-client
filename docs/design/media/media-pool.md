# 媒體儲存池：整檔明文放進一個加密的池，不進 DB（維護者 2026-09-07 定）

> 為什麼不是「每檔一把金鑰、每塊各自 AEAD、bitmap 記哪塊在」：那是 **server 端**的分片設計（mxc 唯一、每檔各自 key、分片存、給下載與 seek 用），**本地快取跟它無關**（維護者 2026-09-07）。

一句話：本地有**一個加密的儲存池**，裡面是**一個個完整的明文檔**；下載時解一塊就順序 append 進去，下載完 DB 記一列。池用**一把金鑰**。

## 1 池是什麼

- 對上層（下載管線、UI）看起來像掛載了一塊區域：開檔、順序寫、讀、刪，檔就是完整明文，**檔內不分片**。
- 落地時整個池是加密的。池是 `wbf-sdk` 裡的一層虛擬層，不用 FUSE／WinFsp（Windows 要裝東西、Android 沒有）。
- 池只用**一把金鑰**：第四把子金鑰 `"wbf-matrix-client media store v1"`（/docs/design/storage/vault-and-keys.md §1）。沒有每檔金鑰。
- 為了「順序 append」與「從中間讀」，池內部落地會分段加密（gocryptfs、Cryptomator、age 都是這樣）。**那是池的實作細節，對上層完全隱藏**；對上層只有 `PoolWriter`（`Write`，順序 append）與 `PoolReader`（`Read + Seek`，明文位置）。落地格式與段大小在 §8。

存的是**解密後的明文**（維護者：「每塊解密後就放這裡」），不存 server 密文：server 密文的金鑰是每則訊息各自的（事件區塊裡的 `key`），本地要留一堆房間金鑰才讀得回來；明文進池，本地只有一套金鑰制度。

## 2 磁碟上

```
s/<b58>_<b58>/media/<hash 前 2 hex>/<hash>     hash = 明文的 BLAKE3（32 byte），64 個小寫 hex 字元；原檔名、mimetype、mxc 只在 cache.db
```

- **檔名是明文的 hash**（維護者 2026-09-07 定）：同內容不同 mxc 只存一份；`media.pool_file` 指過來，`media_by_pool_file` 索引回答「這個檔被幾個 mxc 指著」，清檔前要問。下載中還算不出 hash，先用 `media.id` 當暫存名，完成算完 hash 再 rename、寫回 `pool_file`。
- 目錄用前 2 hex 扇出（一個目錄不會塞幾萬個檔）。磁碟上能看到的只有「幾個檔、各多大」。
- 池跟 `cache.db` 同層：同一個 server 的所有帳號共用，不分帳號、不要可見性（拿得到 mxc 的人 server 就給他檔；可見性在事件那層擋過）。

## 3 下載時怎麼寫（只考慮 download 模式）

只做**順序整檔下載**：`download` 從第 0 塊拿到最後一塊，每解出一塊就 append 進池裡那個檔。串流與 seek 先不管（§6）。

```
開始   ：cache.db 的 media 列（file 事件進快取時就建了，complete = 0）；池裡開一個新檔
每一塊 ：解密 → 驗 → append 進檔 → 記憶體裡的 chunks_written += 1
每 1–2 秒：把記憶體的 chunks_written 寫回 cache.db（一次 UPDATE），不每塊寫 DB
結束   ：檔 fsync → cache.db 把 complete = 1、chunks_written = 總塊數、bytes_on_disk 寫齊
```

- **進度在記憶體，DB 是每 1–2 秒的快照**（維護者定）。這樣 DB 不會被每塊一次的寫入打爆，而中斷最多重下一兩秒的量。
- **中斷續傳**：下次開始前查到 `complete = 0` 的列，把池裡那個檔**截到 `chunks_written × chunk_size`**（最後一次快照之後 append 的塊可能只寫了一半，不信任它），從第 `chunks_written` 塊續。這跟 `wbf-sdk` 的上傳狀態檔是同一種思路：狀態說到哪就從哪開始，不猜。
- **寫入順序**：先 append 檔、再更新 DB；反過來會出現「DB 說有、檔案沒有」。讀的時候 `complete = 1` 才當成有快取。
- 一塊驗證失敗：這次下載中止、檔截回上次快照，下次續。與下載端現有的規則一致（完整性每塊各自驗）。

**實作（`wbf-sdk::media`）**：
- `fetch(client, manifest, cache, pool)`：`media_begin` 建或取列 → 完整且檔在就 `CacheHit`（touch）→ 不然決定續傳點（上次快照的 `chunks_written`，而且 `chunk_size` 要一樣）→ `pool.resume_pending` 或 `create_pending` → 逐塊 `read_and_open_chunk` 寫進 `PoolWriter`，每 1.5 秒（`PROGRESS_FLUSH`）`sync` 加 `media_progress` → 完成 `finish()` 拿 BLAKE3 → `adopt`（同 hash 去重）→ `media_finish`。中止時 DB 停在上次快照、檔留著。
- **暫定段**：進度快照時記憶體裡湊不滿 64 KiB 的那段也要落地，不然快照指到的資料不在磁碟上、續不了。`PoolWriter::sync()` 把它先封成一個短段寫在檔尾，下次湊滿再把檔截回該段起點重封（每 1.5 秒重寫最多 64 KiB，可忽略）。續傳時 `resume_pending(trusted_len)` 解到 trusted_len 為止、把最後那個不完整段的明文放回記憶體、檔截到該段起點，BLAKE3 從頭重算。
- 暫存檔名是 `m<media.id>`，在 `media/pending/`。

## 4 DB 的指針

表在 /docs/design/storage/local-cache-db.md §5（`media` 與 `event_media`）。沒有 bitmap：檔要嘛完整、要嘛是一個「寫到第 N 塊」的半成品，沒有中間有洞的狀態。
摧毀帳號的鏈（/docs/design/storage/local-cache-db.md §5 `forget_account`）走到 `media` 這一層時回傳沒人指的 `pool_file`，池刪檔。

## 5 配額與清理（維護者 2026-09-05 定）

兩個數字，都是 UI 設定、可調：

| | 預設 | 意思 |
|---|---|---|
| 配額 | **2 GiB** | **best effort，不是 hard limit**：超過就試著刪，刪不了就算了，永遠不因為配額拒絕下載 |
| 保護期 | **7 天** | `last_used_at` 在 7 天內的檔**不自動刪**，不管超過配額多少 |

規則：

- 只算 `bytes_on_disk` 的加總。超過配額時，候選只有**保護期外**的檔，照 `last_used_at` 由舊到新刪整個檔，刪到不超過或候選用完為止。
- 候選用完還是超過（7 天內瘋狂下載）：**不刪、不擋**，UI 顯示「媒體快取超過配額」並提供**手動清理**（清全部、或清到某個日期以前）。
- 單一檔就超過配額（一個 2 GiB 的分塊檔）：照樣下、照樣存；下一個檔進來時，它若已出保護期就是第一個被刪的，若還在保護期就留著。多個小檔超過配額、刪最舊的（7 天外），是正常情形。
- 整檔進整檔出。先刪檔再把列 reset 成「還沒下載」（列本身留著：事件還指著它），中途死掉留下的「說完整但檔不在」的列在下次啟動掃一次 reset；`complete = 0` 且沒有下載在跑的半成品也在啟動時掃，超過保護期就清。
- 有把手打開的檔不刪；讀一次就更新 `last_used_at`，所以正在看的東西自然在保護期內。
  「有沒有把手開著」記在程序層級（`media_pool` 的 `OPEN_READERS`：`PoolReader` 開檔時計數、drop 時扣回）；`collect_garbage` 跳過它們（報告的 `files_in_use`）、列也不動，`sweep` 也不收。
  只看這個程序就夠：資料目錄綁定 daemon（/docs/design/overview/architecture-v2.md §0.2）。
- 事件快取不受這個配額（/docs/design/storage/local-cache-db.md §1）。

**實作（`wbf-sdk::media`）**：`collect_garbage(cache, pool, quota, protect, now)` 照上面的規則，先刪檔再 `media_reset` 列；`media_references` 大於 1（同 hash 去重過）的池檔不刪檔只清列。`sweep(cache, pool, protect, now)` 啟動掃：DB 說完整但檔不在 → reset；半成品超過保護期 → 刪暫存檔加 reset；`pending/` 裡沒有列認領的 → 刪；`media/<hh>/` 裡沒有任何列指著的完成檔（`forget_account` 之後、DB 重建之後留下的）→ 刪。CLI：`media-gc [--quota-mib] [--protect-days]` 先 sweep 再 gc、`media-stats`（/docs/design/rpc-specs/wbf-cli-spec.md §3.5）。UI 之後要的「手動清理」就是 quota 0 或直接刪 `media/`。

## 6 先不做的

- **部分快取**（只存看過的那幾塊）：不做。檔要嘛完整、要嘛是續傳中的半成品（從第 0 段起連續）。
- **seek**：已經設計好（維護者 2026-10-01，/docs/design/media/media-download.md）：主檔照舊只順序寫；seek 拉到的塊另外順序 append 進
  旁邊的 seek 暫存檔、用位置表記位置，主檔追上時從那裡搬，完成就刪。上面那條「沒有部分快取」因此不變。

## 7 與下載管線的接法

下載管線是「`Read` 一塊 → 驗長度 → 解密 → 交出去」；走快取時開始前問 `media` 有沒有 `complete = 1` 的列，有就直接從池裡讀整檔；沒有就照 §3 邊下邊 append。
邊界仍在 `wbf-sdk`：CLI 與 UI 只看到一個檔的把手（`PoolReader`），不知道底下是池還是 server。

做法：`download.rs` 的 `read_and_open_chunk` 是 crate 內可見，`media::fetch` 用它逐塊拿；`WbfClient::download`（直接寫到 `Write`）留著給 `--no-cache` 與 `--token` 模式。三個模組的關係：`cache` 不知道池、`media_pool` 不知道 DB、下載管線不知道兩者，只有 `media.rs` 同時碰三者。

## 8 池的檔案格式（權威；`wbf-sdk::media_pool` 就是照這裡寫的，改這裡要換版本號）

> ⚠️ 這是 **v1**（現在的程式）。**v2 已經設計好**（維護者 2026-10-01，/docs/design/media/media-download.md §4.1）：每段都寫滿、真實長度記在密文裡的 4 byte、
> 沒有暫定段，所以檔長永遠可以驗算。v2 實作時取代這一節。

**原理一句話**：對上層是一個完整的明文檔（`Write` 順序寫、`Read + Seek` 用明文位置讀）；落地時切成**固定 64 KiB 的明文段**各自 AEAD，因為段固定，任何明文位置都能用算術換成密文位置，不需要索引表。「整檔加密」講的是使用者看到的單位，不是密文不分段——gocryptfs（4 KiB）、Cryptomator（32 KiB）、age（64 KiB）都這樣。

**為什麼不能整檔一個 AEAD**：認證標籤要等最後一個 byte 才算得出來，邊下載邊寫沒有可以停的點、中斷後前半段驗不了、播影片跳到中間要從頭解到那裡。分段的代價是每段 16 byte（0.024%）加隨機讀多解最多 64 KiB。

**逐 byte**（所有整數 little-endian）：

```
偏移   長度   內容
0      4      magic "WBFP"
4      1      version = 1
5      3      保留，全 0
8      4      segment_size（u32）＝每段明文長度；目前寫 65536。從檔頭讀，不寫死：以後換數字舊檔照自己的檔頭解
12     16     nonce_base，這個檔隨機（CSPRNG）
28     4      保留，全 0
32     …      段 0、段 1、…，緊接著放

第 i 段（i 從 0 起，u64）：
  位置   = 32 + i × (segment_size + 16)
  內容   = XChaCha20-Poly1305(
             key   = 第四把子金鑰 "wbf-matrix-client media store v1"（32 byte，全池共用，/docs/design/storage/vault-and-keys.md §1）
             nonce = nonce_base(16) ‖ u64_le(i)                     → 24 byte
             aad   = "wbf-media-pool v1" ‖ nonce_base(16) ‖ u64_le(i)
             明文  = 第 i 段明文 )
  長度   = 明文長度 + 16（Poly1305 標籤；標籤是 MAC，裡面沒有欄位、不存長度）
  規則   = 除了最後一段，明文長度一律等於 segment_size；最後一段是剩餘長度（可以短，可以剛好整段）
```

**推導**（沒有 per-segment 長度欄、沒有索引、沒有檔尾）：

```
密文總長 L；body = L − 32；S = segment_size + 16
完整段數 = body / S；餘數 r = body % S
明文總長 = 完整段數 × segment_size + (r == 0 ? 0 : r − 16)      // r 非 0 時必須 > 16，否則檔壞
明文位置 p → 段號 i = p / segment_size，段內偏移 = p % segment_size
```

**寫入（`PoolWriter`）**：收明文進 segment_size 的緩衝，湊滿封一段 append；同時餵 BLAKE3。`finish()` 封最後的短段、fsync、回明文 BLAKE3 hex（就是檔名）。
**暫定段**：進度快照前 `sync()` 把緩衝裡湊不滿的那段先封成短段寫在檔尾並 fsync，讓快照指到的每個 byte 都在磁碟上；下次要封正式的第 i 段時先把檔截回第 i 段的起點再寫。所以**檔中間永遠不會有短段**，短段只可能在檔尾。
**續傳（`resume_pending(trusted_len)`）**：完整段數 = trusted_len / segment_size 逐段解開餵 BLAKE3；尾巴 = trusted_len % segment_size 從下一段解出來放回緩衝；檔截到完整段之後；接著寫。trusted_len 來自 DB 的 `chunks_written × chunk_size`，之後的資料一律不信。
**讀取（`PoolReader`）**：`seek` 只改明文位置；`read` 算出段號、讀那一段解開、快取在記憶體，同段連續讀不重解。任何一段標籤不對回 `Io` 錯，上層當「檔壞了、重拉」（/docs/design/storage/local-cache-db.md §1）。

**跟 server 的 chunk 無關**：server 的 `chunk_size` 是傳輸單位、每檔可不同（事件區塊裡）；池的 `segment_size` 是儲存單位、寫在每個檔頭。下載時一個 chunk 的明文丟進 `PoolWriter`，它照自己的 64 KiB 切，兩邊不必對齊；續傳點落在段中間也照上面的尾巴規則處理。

**檔名與目錄**：完成檔是 `media/<hash 前 2 hex>/<hash>`（hash = 明文 BLAKE3（32 byte），64 個小寫 hex 字元），下載中是 `media/pending/m<media.id>`。**檔案本身不帶任何 metadata**：原檔名、mimetype、校驗碼、mxc、大小都只在 `cache.db` 的 `media` 列（`name`／`mimetype`／`hash` 來自事件區塊，上傳者填的；`hash` 的形式是 `<algo>:<hex>`，區塊沒帶 sha256 時下載完用我們算的 BLAKE3 補成 `blake3:…`；同內容去重成一個池檔時，每個 mxc 各自保留自己的 name／mimetype／hash）。磁碟上能看到的只有幾個檔、各多大。

**安全性質**：段號進 nonce 與 AAD → 段搬位置、跨檔拼接都解不開；nonce_base 每檔隨機 → 同內容兩次寫入密文不同（去重靠 hash，不靠密文）；一把金鑰配隨機 nonce_base 加段號，nonce 不重複；金鑰不進錯誤訊息。**暫定段用自己的 nonce**（段號最高位設 1）：同段號的暫定段與之後的正式段是兩個 nonce，每個 nonce 只封一次——AEAD 同 (key, nonce) 封兩份不同明文會漏 Poly1305 金鑰，這條路被堵死；段號因此只用 63 位。`resume_pending` 讀暫存檔尾巴時先用暫定 nonce、解不開再用正式的（尾巴可能是暫定段，也可能是快照點落在中間的正式整段）；完成檔裡永遠沒有暫定段。**不防**：能讀 `local.key` 的人（同 /docs/design/storage/vault-and-keys.md §1 的威脅模型）、檔案大小與數量。

**快取命中的核對**：`fetch` 命中前比對池檔開得起來、明文長度等於區塊 `file_size`、區塊帶 sha256 時要等於 `media.hash`；任一不符 `media_reset` 重下。CLI 的 `sha256_verified` 只在這次真的下載才 true，命中報 false 並附 `hash`。

**測試**（`media_pool.rs` 的單元測試）：跨段寫讀與 seek、去重、續傳在段中／段界／可信長度超過檔長要拒、翻一個 byte／錯金鑰／第 0 段搬到第 1 段都拒、空檔。


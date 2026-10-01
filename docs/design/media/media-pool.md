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

## 3 下載時怎麼寫

**一律順序寫**：主檔從第 0 塊拿到最後一塊，每解出一塊就 append 進池裡那個檔。誰排隊、seek 怎麼插隊、進度怎麼報，權威是 /docs/design/media/media-download.md；這裡只講池這一層。

```
開始   ：cache.db 的 media 列（file 事件進快取時就建了，complete = 0）；池裡開一個新檔（或接著寫舊的）
每一塊 ：解密 → 驗 → 餵進 PoolWriter（湊滿一段才封、才 append）
每 1.5 秒：fsync，把完整段數寫回 cache.db 的 segments_written（只給顯示）
結束   ：封最後一段 → fsync → 改名進 media/<hh>/<hash> → cache.db 把 complete = 1、bytes_on_disk 寫齊
```

- **檔案本身就是進度**：續傳時從第 0 段起逐段解開，完整的段就是寫到哪；DB 的段數🚫 不當依據（/docs/design/media/media-download.md §4.1）。
- **寫入順序**：先落檔、再更新 DB；反過來會出現「DB 說有、檔案沒有」。讀的時候 `complete = 1` 而且池檔打得開、長度對才當成有快取。
- 一塊驗證失敗：重拉一次，還是不行就是壞檔（主檔與暫存檔都刪、列 reset）。
- 暫存檔名是 `m<media.id>`，在 `media/pending/`；seek 暫存檔是旁邊的 `m<media.id>.seek`。

## 4 DB 的指針

表在 /docs/design/storage/local-cache-db.md §5（`media` 與 `event_media`）。沒有 bitmap：檔要嘛完整、要嘛是一個「寫到第 N 段」的半成品，沒有中間有洞的狀態（seek 拉到的塊在旁邊的暫存檔，不在主檔）。
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

**實作（`wbf-sdk::media`）**：`collect_garbage(cache, pool, quota, protect, now)` 照上面的規則，先刪檔再 `media_reset` 列；`media_references` 大於 1（同 hash 去重過）的池檔不刪檔只清列。`sweep(cache, pool, protect, now, in_use)` 掃孤兒：DB 說完整但檔打不開（不在、池格式 v1）→ reset；`pending/` 的規則在 /docs/design/media/media-download.md §4.3（沒列認領、列已完成、v1、過期 → 刪，`in_use` 裡的（下載 worker 正開著的）🚫 不碰）；`media/<hh>/` 裡沒有任何列指著的完成檔（`forget_account` 之後、DB 重建之後、v1 被 reset 之後留下的）→ 刪。daemon 在這個程序第一次起某台 server 的下載 worker 時掃一次。CLI：`media-gc [--quota-mib] [--protect-days]` 先 sweep 再 gc、`media-stats`（/docs/design/rpc-specs/wbf-cli-spec.md §3.5）。UI 之後要的「手動清理」就是 quota 0 或直接刪 `media/`。

## 6 先不做的

- **部分快取**（只存看過的那幾塊）：不做。檔要嘛完整、要嘛是續傳中的半成品（從第 0 段起連續）。
- **seek 不是部分快取**（維護者 2026-10-01，/docs/design/media/media-download.md）：主檔照舊只順序寫；seek 拉到的塊另外順序 append 進
  旁邊的 seek 暫存檔、用位置表記位置，主檔追上時從那裡搬，完成就刪。上面那條「沒有部分快取」因此不變。

## 7 與下載管線的接法

下載管線是「`Read` 一塊 → 驗長度 → 解密 → 交出去」；一個檔在主檔、seek 暫存檔與網路之間怎麼拿塊是 `wbf-sdk::media::MediaDownload`，
誰、何時、做到哪是 core 的下載 worker（/docs/design/media/media-download.md §5）。讀的人只看到明文（`PoolReader` 或 GET 的 body），不知道底下是池還是 server。

三個模組的關係：`cache` 不知道池、`media_pool`／`seek_store` 不知道 DB、下載管線不知道兩者，只有 `media.rs` 同時碰三者。
`WbfClient::download`（直接寫到 `Write`）留著給 CLI 的 `--no-cache` 與 `--token` 模式。

## 8 池的檔案格式

**逐 byte 的版面是池格式 v2，權威在 /docs/design/media/media-download.md §4.1**（`wbf-sdk::media_pool` 照那裡寫；改版面要換版本號）。這一節講它為什麼長那樣。

**原理一句話**：對上層是一個完整的明文檔（`Write` 順序寫、`Read + Seek` 用明文位置讀）；落地時切成**固定 64 KiB 的明文段**各自 AEAD，因為段固定，任何明文位置都能用算術換成密文位置，不需要索引表。「整檔加密」講的是使用者看到的單位，不是密文不分段——gocryptfs（4 KiB）、Cryptomator（32 KiB）、age（64 KiB）都這樣。

**為什麼不能整檔一個 AEAD**：認證標籤要等最後一個 byte 才算得出來，邊下載邊寫沒有可以停的點、中斷後前半段驗不了、播影片跳到中間要從頭解到那裡。

**為什麼每段都寫滿、長度記在密文裡**（v2，維護者 2026-10-01）：每段在磁碟上一樣大，檔長就能驗算——不是「檔頭 ＋ 整數筆」就一定是最後一段寫到一半，直接截掉；
被 force 關掉最多丟最後一筆，不會留下讀得到的壞資料。真實長度在密文裡，改一個 bit 就解不開。代價是每段多 4 byte、最後一段補滿（1 byte 的檔也佔一整段）。
v1 讓最後一段可以短、靠檔長推長度，進度快照還要把湊不滿的尾巴先封成「暫定段」、之後截掉重封——v2 把這兩件事都拿掉了；v1 的檔一律當壞檔重下（它是快取）。

**跟 server 的 chunk 無關**：server 的 `chunk_size` 是傳輸單位、每檔可不同（事件區塊裡）；池的 `segment_size` 是儲存單位、寫在每個檔頭。下載時一個 chunk 的明文丟進 `PoolWriter`，它照自己的 64 KiB 切，兩邊不必對齊；續傳點落在塊中間時，涵蓋它的那一塊在續傳點之前的部分丟掉。

**檔名與目錄**：完成檔是 `media/<hash 前 2 hex>/<hash>`（hash = 明文 BLAKE3（32 byte），64 個小寫 hex 字元），下載中是 `media/pending/m<media.id>`，seek 暫存檔是 `media/pending/m<media.id>.seek`（版面在 /docs/design/media/media-download.md §4.2）。**檔案本身不帶任何 metadata**：原檔名、mimetype、校驗碼、mxc、大小都只在 `cache.db` 的 `media` 列（`name`／`mimetype`／`hash` 來自事件區塊，上傳者填的；`hash` 的形式是 `<algo>:<hex>`，區塊沒帶 sha256 時下載完用我們算的 BLAKE3 補成 `blake3:…`；同內容去重成一個池檔時，每個 mxc 各自保留自己的 name／mimetype／hash）。下載中的主檔檔頭多一個 4 byte 的 `owner`（mxc 的 BLAKE3 前 4 byte）：只用來認「這個暫存名還是不是同一個檔的」，🚫 不是 metadata、也不是安全邊界。磁碟上能看到的只有幾個檔、各多大。

**安全性質**：段號進 nonce 與 AAD → 段搬位置、跨檔拼接都解不開；nonce_base 每檔隨機 → 同內容兩次寫入密文不同（去重靠 hash，不靠密文）；一把金鑰配隨機 nonce_base 加段號，nonce 不重複；**每個段號只封一份明文**（續傳時重封的那段明文一定一樣；暫存名換了主人就整個重下，不續）；金鑰不進錯誤訊息。**不防**：能讀 `local.key` 的人（同 /docs/design/storage/vault-and-keys.md §1 的威脅模型）、檔案大小與數量。

**快取命中的核對**（`media::open_complete`）：列說 `complete = 1`、池檔開得起來（池格式 v2、檔長是整數筆、最後一段解得開）、明文長度等於列的 `file_size`；任一不符就當沒有，重下。區塊帶 sha256 時，下載收尾就跟整檔的 SHA-256 比過，對不上不會進池。

**測試**：sdk `media_pool::tests`（格式本身）與 `tests/media_cache.rs`（跟 `cache.db` 一起），清單在 /docs/design/media/media-download.md §10。


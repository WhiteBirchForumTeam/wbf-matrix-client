# 本地資料庫設計：加密的暫存快取

> 狀態：草案，2026-09-05。維護者同意 §1–§4、§6 的提案；§5（與 matrix-sdk store 的關係）維護者要求寫細再議。
> 前提在 [plan-v1.md](plan-v1.md) §7.1：**現在不做**，先接通 API；這份是之後動手時的依據。

## 0. 一句話

本地有兩個 SQLite 檔：matrix-sdk 自己的 store（它非存不可的東西）與我們的快取（聊天紀錄、房間、事件區塊）。
兩個都加密，金鑰都從同一把 32 byte 主金鑰導出；主金鑰第一版明文放本地（自解密），加 local password 後被密碼包住，
啟動時要解開，UI 與 CLI 走同一個函數。**快取不是權威**：可以整個刪掉重建，衝突以 server 為準。

## 1. 定位：快取，不是權威

| 因為它是快取，所以 | 具體做法 |
|---|---|
| 可以整個丟掉 | schema 版本不對、server 或 user 換了、解不開：刪檔重建，不寫遷移 |
| 不需要衝突解決 | 同一個 event_id 再寫一次就覆蓋；server 說的算 |
| 事件快取只長不刪 | **500 是同步視窗，不是上限**（維護者 2026-09-05 訂正）：進房時把最新 500 則同步進快取；往舊滑超過就再 load 更舊的存進去；下次重讀那一段從快取拿，不重拉。事件小，不設上限 |
| 媒體快取有配額，但是 best effort | **2 GiB**（維護者 2026-09-05 定），不是 hard limit；另有 **7 天保護期**，期內用過的檔不自動刪（§8.4） |
| 壞掉的代價只是重拉 | 任何讀到壞資料的地方都 fail closed：當成沒有快取，回 server 拿 |

## 2. 存什麼、不存什麼

| 存 | 說明 |
|---|---|
| 房間列表與 metadata | room_id、名稱、是否 E2EE、成員數、最後活動時間 |
| 時間線事件，**解密後的明文** | 含 `org.wbftw.wbfuwunel.chunked` 區塊（裡面有媒體金鑰）。這是整個 DB 非加密不可的理由 |
| 每房的翻頁 token、閱讀位置 | `read`／`watch` 接著翻用 |
| 已知的 manifest | 就是事件區塊加 mxc，給 `files`／`download` 用；不另存一份，從事件查 |

| 不存 | 理由 |
|---|---|
| 密碼 | 永遠不存（CLI 規格 §9） |
| 媒體內容 | **不進 DB**，寫成檔案放加密的檔案空間（§8）；DB 只放指針（`media_files`） |
| session 與 token | **不進 DB**（維護者 2026-09-05 定）：另外用同一把主金鑰導出的第三把子金鑰鎖一次，見 §4 與 §5.6 的 `session.sealed` |

## 3. 加密：SQLCipher，整檔頁級

`rusqlite` 的 `bundled-sqlcipher` feature（實作時確認 feature 名與 matrix-sdk 釘的 rusqlite 0.40 相容；
兩邊 feature 會統一，SDK 的 store 也會連到 SQLCipher 版的 sqlite，但它不下 `PRAGMA key`，行為就是普通 SQLite）。

- 用 **raw key** 開：`PRAGMA key = "x'<64 hex>'"`，跳過 SQLCipher 自己的 PBKDF2，金鑰導出由我們統一做（§4）。
- 不選「自己在 SQLite 上做每列 AEAD」：查詢變難、索引做不了、表名與列數等 metadata 漏在外面。
- Android：`bundled-sqlcipher` 是原始碼編譯，NDK 能編（實作時驗）。

## 4. 金鑰：一把主金鑰，兩種鎖法，型別化

```
local.key（0600）
  ├─ Plain            : { "v": 1, "mode": "plain", "master": "<base64 32 byte>" }
  └─ PasswordWrapped  : { "v": 1, "mode": "password",
                          "kdf": { "name": "argon2id", "m_kib": 65536, "t": 3, "p": 1, "salt": "<base64 16>" },
                          "nonce": "<base64 24>", "wrapped": "<base64 48>" }   // XChaCha20-Poly1305(KEK, master)
```

- **主金鑰** 32 byte，CSPRNG，一台機器一把。
- **導出**：三把 32 byte 子金鑰，`BLAKE3 derive_key(context, master)`，context 是固定字串
  `"wbf-matrix-client cache sqlcipher v1"`、`"wbf-matrix-client matrix-sdk store v1"`、`"wbf-matrix-client session v1"`，加第四把 `"wbf-matrix-client media store v1"`（§8）。
  第三把用 XChaCha20-Poly1305 把 session 檔（server、user_id、device_id、access_token）整份封成 `session.sealed`：
  session 與 token **不進 DB**，但跟 DB 同一把鎖（維護者 2026-09-05 定）。子金鑰不落地，每次開啟導一次。
  換 context 字串就是換金鑰，所以 context 帶版本。
- **local password**：Argon2id 從密碼導 KEK，KEK 用 XChaCha20-Poly1305 包住主金鑰。改密碼只重包 48 byte，DB 不動。
  參數寫在檔裡，之後調高不用遷移。
- **模式是型別，不是空字串**：`enum KeyFile { Plain { master }, PasswordWrapped { kdf, nonce, wrapped } }`，
  讀檔時 `mode` 不認得就拒絕。「沒設密碼」是 `Plain`，不是「密碼等於空字串」——後者會讓空密碼靜默通過。
- **一個入口**：`Vault::open(dir, Unlock::NoPassword | Unlock::Password(secret))`。`Plain` 配 `NoPassword`、
  `PasswordWrapped` 配 `Password`，配錯就 `Err`，UI 與 CLI 都只能走這裡。CLI 的密碼來源同 `login`：`--password-file` 或終端不回顯。
- **解鎖後金鑰放哪**（維護者 2026-09-05：作法由我定，照一般開發工具的做法）：

  | | 做法 |
  |---|---|
  | UI | 解鎖一次，主金鑰只在記憶體；UI runtime 與 wbf-sdk 是同一個程序，關掉就沒了 |
  | CLI（只在開發與 debug 用） | 仿 `sudo`：解鎖成功後寫一張 **unlock ticket**（`<data dir>/unlock.ticket`，0600，內容是主金鑰加 `expires_at`），有效期預設 15 分鐘、`--unlock-ttl <秒>` 可調；期內的命令不再問密碼。`lock` 命令刪掉它。過期的 ticket 讀到就刪，Unix 上模式不是 0600 就拒用 |

  CLI 密碼的來源與 `login` 同一套：`--local-password-file <檔>` 或終端不回顯；不接受命令列明文與環境變數。優先順序：檔案參數 → 有效的 ticket → 問終端。
  ticket 是明文主金鑰落地，安全性等於 `Plain` 模式那 15 分鐘；維護者明說接受（CLI 不是產品面）。這一項不進 UI。

- **威脅模型**（老實寫）：

| 防 | 不防 |
|---|---|
| 把 DB 檔拷走的人（沒有 `local.key` 解不開） | 能登入這台機器、讀得到 `local.key` 的人（`Plain` 模式） |
| 加 local password 後：連 `local.key` 一起拷走也解不開（要猜密碼，Argon2id 拖慢） | 跑著的程序記憶體裡的主金鑰；鍵盤側錄 |

## 5. 與 matrix-sdk 的 store 怎麼相處（細節）

### 5.1 它有什麼

`vendor/matrix-rust-sdk` 的 `matrix-sdk-sqlite`（2026-09-03 的上游）有**四個** store，各自一個 SQLite 檔，由 feature 開關：

| store | 表（migrations 裡的） | 裝什麼 | 我們要不要 |
|---|---|---|---|
| crypto | `device identity session inbound_group_session outbound_group_session secrets key_requests olm_hash tracked_user room_settings …` | 裝置金鑰、Olm／Megolm session、房間金鑰、金鑰備份狀態 | **必要**。沒有它每次啟動是新裝置，E2EE 訊息解不開、要重新驗證 |
| state | `room_info member profile state_event receipt global_account_data room_account_data send_queue_events kv …` | sync 狀態：房間列表、成員、房間 state、收據、送出佇列 | **必要**。沒有它每個命令都 initial sync，慢而且 sync token 沒地方放 |
| event_cache | `linked_chunks event_chunks gap_chunks events threads media …` | SDK 自己的時間線快取（linked chunk 結構），給它的 `Timeline` 用 | **不開**：這正是「聊天紀錄」，§7.1 說不存。不給它 sqlite 版就落在記憶體版，程序結束就沒了 |
| media | `media kv lease_locks` | 媒體內容快取 | **不開**：§2 說第一版不快取媒體 |

所以「SDK 的 store 只放它非存不可的」在程式上就是：只建 crypto 與 state 兩個 sqlite store 交給 `Client` builder，
event cache 與 media 用 SDK 的記憶體實作。**這是 §7.1 的「非存不可」的精確定義。**

### 5.2 它怎麼加密

不是 SQLCipher。`matrix-sdk-store-encryption` 的 `StoreCipher`：

- 每個 store 有一把隨機的 `StoreCipher`（含一把加密 key、一把 MAC key），**加密後存在該 store 的 `kv` 表 `cipher` 列**。
- 值：`XChaCha20-Poly1305` 每值加密。索引鍵（room_id、event_id 這類）：`BLAKE3 keyed hash`，所以查得到但看不出原文。
- 開啟時解開 `StoreCipher` 的兩種方式：`open(passphrase)` 走 PBKDF2-HMAC-SHA256 **200,000 輪**；`open_with_key(&[u8; 32])` 直接用 32 byte 包住。
- **檔案本身不加密**：表名、列數、每列大小、SQLite 頁面都看得到，看不到的是值與鍵的原文。

### 5.3 兩者怎麼接

```
local.key ──master──┬─ BLAKE3 derive_key("…cache sqlcipher v1")   ──► SQLCipher raw key ──► cache.db（整檔加密）
                    └─ BLAKE3 derive_key("…matrix-sdk store v1") ──► open_with_key(...) ──► crypto.db、state.db（每值加密）
```

- SDK 那邊用 `open_with_key`，**不用 passphrase**：PBKDF2 20 萬輪每次啟動要花時間，而且我們的密碼 KDF 已經在 §4 做過一次；
  兩個 store 傳同一把子金鑰即可（各自的 `StoreCipher` 仍是隨機的，子金鑰只是包住它們）。
- 加 local password 後，SDK 的 store 也一起被鎖住：主金鑰解不開就導不出子金鑰，`open_with_key` 就失敗。**不需要動 SDK。**
- 兩個世界的邊界只有一條線：`Vault::open` 回兩把子金鑰。SDK 不知道 SQLCipher，快取不知道 `StoreCipher`。

### 5.4 為什麼不把快取塞進 SDK 的 event_cache（方案 B）

- 它的 schema（linked chunk）是為 SDK 的 `Timeline` 設計的，`files`（依 msgtype 找事件）、`read --type`／`--sender`、配額清理這些是我們的查詢，
  對著它做要嘛繞它的 API、要嘛直接讀它的表（等於綁死上游內部結構，上游改 migration 我們就壞）。
- 它的加密是每值加密、檔案結構外露；我們的快取要整檔加密（§3）。
- 它存不存、存多少由 SDK 決定；§1 的配額與「整個丟掉」政策要自己掌控。
- 代價：多一個檔、多一份「事件」的複本（SDK 記憶體裡一份、我們 DB 一份）。可接受：記憶體那份程序結束就沒了。

### 5.5 為什麼不讓 SDK 也用 SQLCipher（方案 C）

要改 `matrix-sdk-sqlite` 的開檔路徑下 `PRAGMA key`，等於 fork submodule；而 5.3 已經讓密碼一把鎖住兩邊，收益只剩「SDK 的檔案結構也藏起來」。不值得。

### 5.6 各檔在磁碟上

```
<data dir>/wbf/<server host>/<user localpart>/
  local.key      主金鑰（§4）
  cache.db       我們的快取（SQLCipher）：事件、房間、指針
  media/         媒體檔案空間，檔級加密（§8）；檔名是隨機 id，不洩原名
  matrix/        SDK 的 store 目錄（crypto.db、state.db；SDK 自己命名）
  session.sealed 第三把子金鑰封住的 session（取代 CLI 規格 §7 的明文 session.json；那一版同步改規格）
```

`<data dir>`：Windows `%APPDATA%`、macOS `~/Library/Application Support`、Linux `$XDG_DATA_HOME`（沒設就 `~/.local/share`）。
一個 server 加一個 user 一套；換帳號不會撞。

## 6. 快取的 schema（草案）

```sql
CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
-- schema_version、server、user_id、device_id、created_at；任一不符 → 整個重建（§1）

CREATE TABLE rooms (
  room_id TEXT PRIMARY KEY, name TEXT, encrypted INTEGER NOT NULL, member_count INTEGER,
  last_activity_ts INTEGER, next_back_token TEXT, refreshed_at INTEGER NOT NULL);

CREATE TABLE events (
  room_id TEXT NOT NULL, event_id TEXT NOT NULL,
  seq INTEGER,                       -- server 發的 per-room 連續序號（chat-model §4.3）；非 fork server 的 room 是 NULL
  origin_server_ts INTEGER NOT NULL,
  sender TEXT NOT NULL, type TEXT NOT NULL, msgtype TEXT,
  decrypted INTEGER,                 -- 1／0／NULL，同 CLI 規格 §3.4.1 的事件形狀
  undecryptable_reason TEXT,
  content_json TEXT NOT NULL,        -- 解密後的 content
  chunked_block_json TEXT,           -- msgtype 是 org.wbftw.wbfuwunel.file 時抽出來，給 files 查
  mxc TEXT,
  PRIMARY KEY (room_id, event_id));
CREATE UNIQUE INDEX events_by_seq ON events (room_id, seq) WHERE seq IS NOT NULL;   -- 排序、判洞、跳第 N 則
CREATE INDEX events_by_time ON events (room_id, origin_server_ts);                    -- 顯示與日期跳轉用
CREATE INDEX events_files ON events (room_id, msgtype) WHERE chunked_block_json IS NOT NULL;

-- 本地 offset：自己讀到哪，只在這台裝置（chat-model §3.5，維護者定）；給遠端看的 read 在 server，不在這裡。
CREATE TABLE read_positions (room_id TEXT PRIMARY KEY, event_id TEXT NOT NULL, seq INTEGER, ts INTEGER NOT NULL);
-- offset 是一對 (event_id, seq)：event_id 是權威，seq 給算術用；seq 是 NULL 就只能靠 event_id 對齊。

-- Delete for me（chat-model §5，維護者定）：本地清掉並記下來，之後從 server 拿到同一則也忽略。
-- 不動 server；重新安裝（DB 不在了）就恢復。這張表不受配額清理。
CREATE TABLE hidden_messages (room_id TEXT NOT NULL, event_id TEXT NOT NULL, hidden_at INTEGER NOT NULL,
  PRIMARY KEY (room_id, event_id));
```

- 寫入 `events` 前先查 `hidden_messages`，有就不寫；讀出來給 UI 前也再濾一次（消費端自己問，不靠寫入端記得）。

- 解不開的加密事件也存（`decrypted = 0` 帶原因），之後拿到金鑰重解時覆蓋；不存等於每次都要重拉。
- 配額（§1）以 `seq` 為序刪最舊（沒有 `seq` 的 room 用 `origin_server_ts`）；`rooms` 不受配額。
- **洞**：有 `seq` 的 room，「快取裡有哪些」就是 `seq` 的集合，缺的就是洞，不存 token。沒有 `seq` 的 room 只快取最新一段連續視窗（chat-model §4.3 的退化表）。

## 8. 媒體檔案空間：檔級加密，不進 DB（維護者 2026-09-05 定方向）

媒體不用資料庫讀，寫成檔案；DB 只放事件與指針。檔案空間要加密，方向對齊 [gocryptfs](https://github.com/rfjakob/gocryptfs)：
**每個檔案自己一把金鑰、內容切固定大小的塊各自 AEAD、檔名不洩原名**。但不用 gocryptfs 本身：它是 FUSE 掛載，Windows 要 WinFsp、Android 沒有；
我們把同一套做法做在程式裡，UI 拿到的是一個 `Read + Seek` 的把手，不是掛載點。

### 8.1 存什麼

**解密後的明文塊**（維護者：「每塊解密後就放這裡」），再用本地金鑰加密落地。不存 server 上的密文，理由：
- server 密文的金鑰是每則訊息各自的（事件區塊裡的 `key`），本地要留一堆房間金鑰才讀得回來；存明文塊再用本地金鑰包，本地只有一套金鑰制度。
- 塊的邊界照約定規格書的 `chunk_size`，下載端解一塊就能存一塊，seek 讀回來也是一塊一塊，**部分快取是自然的**（看了影片中間一段，就只有那幾塊在）。

### 8.2 檔案格式

```
media/<id 前 2 hex>/<id>          id = 128 bit CSPRNG 的 32 位小寫 hex；原檔名只在 DB
  header（固定長度）
    magic "WBFM"、version 1
    file_id（16 byte，同檔名；進 AAD，防換名／換檔）
    chunk_size（u32）、file_size（u64）
    wrapped_file_key：XChaCha20-Poly1305(第四把子金鑰, nonce 24 byte, file_key 32 byte) → 24 + 48 byte
    nonce_base（8 byte，每檔隨機）
  body：第 i 塊在固定偏移 header_len + i × (chunk_size + 16)
    block_i = AEAD(file_key, nonce_base ‖ u32_be(i), aad = "wbf-media-store-v1" ‖ file_id, 明文塊 i)
```

- 塊的加密就是 `wbf-sdk` 現有的 `FileCipher`（`chunk_crypto`）：同一個 nonce 構造、同一種 AEAD，只換 AAD 與金鑰來源。實作時把 AAD 從常數改成參數，一個小改動。
- 塊固定偏移，所以**沒下載的塊就是空洞**（檔案 sparse 或先不寫），哪些塊在由 DB 的 bitmap 說；讀到不在的塊 → 不是壞，是沒快取，回 server 拿。
- 標籤驗證失敗 → 這一塊當壞的丟掉重拉，其他塊不受影響（與下載端的規則一致：完整性是每塊各自驗）。
- 檔名是隨機 id、目錄用前 2 hex 扇出（一個目錄不會塞幾萬個檔）；大小、mimetype、原名、對應哪個 mxc，全部只在 `cache.db`（它整檔加密）。磁碟上能看到的只有「幾個檔、各多大」。

### 8.3 DB 的指針

```sql
CREATE TABLE media_files (
  file_id TEXT PRIMARY KEY,          -- 32 hex，就是檔名
  mxc TEXT NOT NULL UNIQUE,          -- 對回事件（events.mxc）
  name TEXT, mimetype TEXT,
  file_size INTEGER NOT NULL, chunk_size INTEGER NOT NULL,
  blocks_present BLOB NOT NULL,      -- bitmap，第 i 位 = 第 i 塊在不在
  complete INTEGER NOT NULL,         -- 全部塊都在
  bytes_on_disk INTEGER NOT NULL,    -- 配額用
  created_at INTEGER NOT NULL, last_used_at INTEGER NOT NULL);
CREATE INDEX media_files_lru ON media_files (last_used_at);
```

- 一個 mxc 一個檔；同一個媒體被轉發到別的房間仍是同一個 mxc、同一個檔。
- 寫入順序：先寫塊、fsync、再更新 bitmap；反過來會出現「DB 說有、檔案沒有」。讀的時候**兩邊都問**：bitmap 說在、標籤也要過。

### 8.4 配額與清理（維護者 2026-09-05 定）

兩個數字，都是 UI 設定、可調：

| | 預設 | 意思 |
|---|---|---|
| 配額 | **2 GiB** | **best effort，不是 hard limit**：超過就試著刪，刪不了就算了，永遠不因為配額拒絕下載 |
| 保護期 | **7 天** | `last_used_at` 在 7 天內的檔**不自動刪**，不管超過配額多少 |

規則：

- 只算 `bytes_on_disk` 的加總。超過配額時，候選只有**保護期外**的檔，照 `last_used_at` 由舊到新刪整個檔，刪到不超過或候選用完為止。
- 候選用完還是超過（7 天內瘋狂下載）：**不刪、不擋**，UI 顯示「媒體快取超過配額」並提供**手動清理**（清全部、或清到某個日期以前）。
- 單一檔就超過配額（一個 2 GiB 的分塊檔）：照樣下、照樣存；下一個檔進來時，它若已出保護期就是第一個被刪的，若還在保護期就留著。多個小檔超過配額、砂最舊的（7 天外），是正常情形。
- 不做塊級淘汰，整檔進整檔出。先刪檔再刪列，中途死掉留下的孤兒列在下次啟動掃一次清掉。
- 正在播放的檔不刪（有把手打開就跳過）；讀一次就更新 `last_used_at`，所以正在看的東西自然在保護期內。
- 事件快取不受這個配額（§1）。

### 8.5 與下載管線的接法

`download`／`seek` 現在是「`Read` 一塊 → 驗長度 → 解密 → 交出去」；加快取就是在「交出去」之前多一步「寫進 media store 第 i 塊」，在「`Read`」之前多一步「media store 有第 i 塊就直接用」。
邊界仍在 `wbf-sdk`：CLI 與 UI 只看到 `Read + Seek` 的把手，不知道底下是快取還是 server。

## 9. 還開著的

1. ~~session 與 token 要不要搬進 DB~~ 定了：不進 DB，`session.sealed`（§4）。~~CLI 每個命令輸密碼的體感~~ 定了：仿 sudo 的 unlock ticket（§4）。
2. ~~媒體內容快取另議~~ 定了方向：§8。
3. ~~媒體配額的數字~~ 定了：2 GiB best effort、保護期 7 天（§8.4）。事件快取不設上限；同步視窗 500 則／房、初開全域 10000 則是預設值，可調。

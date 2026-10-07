# 本地資料庫設計：加密的暫存快取

> 這份講 `cache.db`（與 matrix-sdk 的 store 怎麼分工）。主金鑰、子金鑰、路徑加密、passphrase 在 /docs/design/storage/vault-and-keys.md；媒體內容在 /docs/design/media/media-pool.md。

## 0. 一句話

本地有兩種 SQLite：matrix-sdk 的 store（帳號目錄的 `m/`，它非存不可的東西）與我們的快取 `cache.db`（聊天紀錄、房間、事件區塊）。
兩種都加密，金鑰都從同一把 32 byte 主金鑰導出；主金鑰預設明文放本地（`Plain`），設了 passphrase 就被它包住，
啟動時要解開，UI 與 CLI 走同一個函數（/docs/design/storage/vault-and-keys.md §1）。**快取不是權威**：可以整個刪掉重建，衝突以 server 為準。

## 1. 定位：快取，不是權威

| 因為它是快取，所以 | 具體做法 |
|---|---|
| 可以整個丟掉 | 沒有可用的庫（沒檔、解不開、schema 版本不對）：建或刪檔重建，不寫遷移。一個 server 一份、多帳號混存（§5），user 換了不丟。<br>既有而且解得開的庫**照它 `meta` 記的 server 開**，🚫 不拿呼叫端的字串逐字比對（同一台 server 的拼法會不同：`http://localhost:6167`、`localhost:6167`）；記的 host 跟要開的不是同一台就**拒絕（`Usage`：`the cache in … belongs to …, not …; it was left untouched`）、庫不動**（`Core::find_recorded_cache_identity`） |
| 不需要衝突解決 | 同一個 event_id 再寫一次就覆蓋；server 說的算 |
| 事件快取只長不刪 | **500 是同步視窗，不是上限**（維護者 2026-09-05 訂正）：進房時把最新 500 則同步進快取；往舊滑超過就再 load 更舊的存進去；下次重讀那一段從快取拿，不重拉。事件小，不設上限。500 則／房與初開全域 10000 則是預設值，可調 |
| 媒體快取有配額，但是 best effort | **2 GiB**（維護者 2026-09-05 定），不是 hard limit；另有 **7 天保護期**，期內用過的檔不自動刪（/docs/design/media/media-pool.md §5） |
| 壞掉的代價只是重拉 | 任何讀到壞資料的地方都 fail closed：當成沒有快取，回 server 拿 |

## 2. 存什麼、不存什麼

| 存 | 說明 |
|---|---|
| 房間列表與 metadata | room_id、名稱、是否 E2EE、成員數、最後活動時間 |
| 時間線事件，**解密後的明文** | 含 `org.wbftw.wbfuwunel.chunked` 區塊（裡面有媒體金鑰）。這是整個 DB 非加密不可的理由 |
| 每房的閱讀位置 | `read`／`watch` 接著看用。📎 翻頁 token 🚫 不存：往回翻一律拿 `event_id` 當錨（/docs/design/rpc-specs/rpc-spec.md §3.3） |
| 已知的 manifest | 就是事件區塊加 mxc，給 `files`／`download` 用；不另存一份，從事件查 |

| 不存 | 理由 |
|---|---|
| 密碼 | 永遠不存（/docs/design/rpc-specs/wbf-cli-spec.md §9） |
| 媒體內容 | **不進 DB**，整檔明文放進加密的儲存池（/docs/design/media/media-pool.md）；DB 只放指針（`media`、`event_media`） |
| session 與 token | **不進 DB**（維護者 2026-09-05 定）：另外用同一把主金鑰導出的第三把子金鑰鎖一次，見 /docs/design/storage/vault-and-keys.md §1 與 §4.6 的 `session.sealed` |

## 3. 加密：SQLCipher，整檔頁級

`rusqlite` 0.40 開 `bundled-sqlcipher-vendored-openssl`。feature 統一後整個 workspace 的 `libsqlite3-sys` 都是 SQLCipher 版：
matrix-sdk 的 store 也連到它，但不下 `PRAGMA key`，行為就是普通 SQLite。

- 用 **raw key** 開：`PRAGMA key = "x'<64 hex>'"`，跳過 SQLCipher 自己的 PBKDF2，金鑰導出由我們統一做（/docs/design/storage/vault-and-keys.md §1）。
- 不選「自己在 SQLite 上做每列 AEAD」：查詢變難、索引做不了、表名與列數等 metadata 漏在外面。
- **建置需求（2026-09-07 實測）**：SQLCipher 的 AES 走 OpenSSL：

  | 平台 | 需要什麼 |
  |---|---|
  | Windows MSVC | 編 vendored OpenSSL 要 **Strawberry Perl**（git-bash 的 msys perl 跑 `Configure` 會失敗）。cargo 前把 `C:\Strawberry\perl\bin` 排到 PATH 前面。第一次編要好幾分鐘 |
  | Linux | 系統 perl 就夠 |
  | macOS | SQLCipher 走 CommonCrypto，不編 OpenSSL |
  | Android | 還沒驗：`bundled-sqlcipher` 是原始碼編譯，NDK 應該編得動 |

- **fail closed**：`Cache::open` 下完 `PRAGMA key` 會問 `PRAGMA cipher_version`，空的（這個 build 沒有 SQLCipher，`PRAGMA key` 是 no-op）就拒絕開，不會靜默寫出明文快取。

## 4. 與 matrix-sdk 的 store 怎麼相處（細節）

### 4.1 它有什麼

`vendor/matrix-rust-sdk` 的 `matrix-sdk-sqlite` 有**四個** store，各自一個 SQLite 檔（`matrix-sdk-<名字>.sqlite3`）：

| store | 表（migrations 裡的） | 裝什麼 | 我們要不要 |
|---|---|---|---|
| crypto | `device identity session inbound_group_session outbound_group_session secrets key_requests olm_hash tracked_user room_settings …` | 裝置金鑰、Olm／Megolm session、房間金鑰、金鑰備份狀態 | **必要**。沒有它每次啟動是新裝置，E2EE 訊息解不開、要重新驗證 |
| state | `room_info member profile state_event receipt global_account_data room_account_data send_queue_events kv …` | sync 狀態：房間列表、成員、房間 state、收據、送出佇列 | 只有 matrix-sdk `Client` 那條要：沒有它每個命令都 initial sync |
| event_cache | `linked_chunks event_chunks gap_chunks events threads media …` | SDK 自己的時間線快取（linked chunk 結構），給它的 `Timeline` 用 | **不用**：聊天紀錄的權威是 `cache.db`（§1） |
| media | `media kv lease_locks` | 媒體內容快取 | **不用**：媒體在 /docs/design/media/media-pool.md 的池 |

`m/` 裡有什麼看帳號走哪條路（/docs/design/daemon/account-session.md §2）：

- **wbf 帳號**：只有 crypto store，由 `OlmEngine::open` 建（`crypto_engine.rs`），加上 `td.json`（/docs/design/keys/to-device-client.md §2）。不建 `Client`。
- **matrix-sdk `Client` 那條**（一般 Matrix、舊 session）：`build_client` 自己組 `StoreConfig`，**只把 state 與 crypto 開成 sqlite 檔**、放在 `m/`、用同一把子金鑰；
  event_cache 與 media 用上游預設的記憶體版（維護者 2026-09-30：終極目標是只依賴 crypto，聊天紀錄與媒體用自己的）。
  🚫 不用 `sqlite_store_with_config_and_cache_path`：它四個都開成檔。我們用到的（`sync_once`、`room.messages`、`event_with_context`、`TimelineEvent` 的解密）都不靠那兩個落地。
  真 server 測試 `the_matrix_sdk_client_keeps_only_state_and_crypto_on_disk` 守著「只有兩個檔」。

### 4.2 它怎麼加密

不是 SQLCipher。`matrix-sdk-store-encryption` 的 `StoreCipher`：

- 每個 store 有一把隨機的 `StoreCipher`（含一把加密 key、一把 MAC key），**加密後存在該 store 的 `kv` 表 `cipher` 列**。
- 值：`XChaCha20-Poly1305` 每值加密。索引鍵（room_id、event_id 這類）：`BLAKE3 keyed hash`，所以查得到但看不出原文。
- 開啟時解開 `StoreCipher` 的兩種方式：`open(passphrase)` 走 PBKDF2-HMAC-SHA256 **200,000 輪**；`open_with_key(&[u8; 32])` 直接用 32 byte 包住。
- **檔案本身不加密**：表名、列數、每列大小、SQLite 頁面都看得到，看不到的是值與鍵的原文。

### 4.3 兩者怎麼接

```
local.key ──master──┬─ BLAKE3 derive_key("…cache sqlcipher v1")   ──► SQLCipher raw key ──► cache.db（整檔加密）
                    └─ BLAKE3 derive_key("…matrix-sdk store v1") ──► open_with_key(...) ──► m/ 裡的 store（每值加密）
```

- SDK 那邊用 `open_with_key`，**不用 passphrase**：PBKDF2 20 萬輪每次啟動要花時間，而且我們的密碼 KDF 已經在 /docs/design/storage/vault-and-keys.md §1 做過一次；
  每個 store 傳同一把子金鑰即可（各自的 `StoreCipher` 仍是隨機的，子金鑰只是包住它們）。
- 加 passphrase 後，SDK 的 store 也一起被鎖住：主金鑰解不開就導不出子金鑰，`open_with_key` 就失敗。**不需要動 SDK。**
- 兩個世界的邊界只有一條線：`Vault` 導出的兩把子金鑰（`cache_key()`、`matrix_store_key()`）。SDK 不知道 SQLCipher，快取不知道 `StoreCipher`。

### 4.4 為什麼不把快取塞進 SDK 的 event_cache（方案 B）

- 它的 schema（linked chunk）是為 SDK 的 `Timeline` 設計的，`files`（依 msgtype 找事件）、`read --type`／`--sender`、配額清理這些是我們的查詢，
  對著它做要嘛繞它的 API、要嘛直接讀它的表（等於綁死上游內部結構，上游改 migration 我們就壞）。
- 它的加密是每值加密、檔案結構外露；我們的快取要整檔加密（§3）。
- 它存不存、存多少由 SDK 決定；§1 的配額與「整個丟掉」政策要自己掌控。
- 所以 matrix-sdk `Client` 那條的 event_cache 只放記憶體（§4.1），🚫 不落地第二份事件；權威是 `cache.db`。

### 4.5 為什麼不讓 SDK 也用 SQLCipher（方案 C）

要改 `matrix-sdk-sqlite` 的開檔路徑下 `PRAGMA key`，等於 fork submodule；而 §4.3 已經讓密碼一把鎖住兩邊，收益只剩「SDK 的檔案結構也藏起來」。不值得。

### 4.6 各檔在磁碟上：一台機器一把鑰、一個 server 一份快取、一個帳號一套 session（維護者 2026-09-07 定）

每個檔的位置、內容、加密、誰寫誰刪：**`/docs/design/storage/local-storage.md` §3 的目錄樹**（只畫在那裡，這裡🚫 再畫一份）。和這份有關的是：
`cache.db` 與媒體池在 server 層（`s/<加密的 host>/`），`session.sealed`、`m/`、`k/` 在帳號層（`s/…/a/<加密的 localpart>/`），`local.key` 與 `r/` 在最上層。

為什麼這樣分層（維護者 2026-09-07 定）：
- **`local.key` 在頂層**：主金鑰的定位是「這台機器」（/docs/design/storage/vault-and-keys.md §1 第一條），passphrase 也是一台機器一個；一帳號一把會變成每個帳號各自問 passphrase。
- **`cache.db`（與媒體池）在 server 層，多帳號共用**：維護者要的是混存——user1 看得到 room1／2／3、user2 看得到 room1／2／4，不論誰登入都同步進同一個 DB，事件只存一份，可見性逐則記（§5）。共用範圍是同一個 server：`r_seq`／`g_seq` 是 fork server 發的，不同 homeserver 上序號不同。
- **兩層目錄名都是加密的**（/docs/design/storage/vault-and-keys.md §2，維護者 2026-09-09 定）：`s/` 與 `a/` 底下都只看得到 `<b58>_<b58>`，要知道是哪家、是誰得用第六把子金鑰解。真正的 URL 與 mxid 仍然在 `session.sealed`。

## 5. 快取的 schema（v9，就是 `wbf-sdk::cache` 建的）

混存與整數主鍵是維護者 2026-09-07 定的；`events` 的欄位照 /docs/design/messages/edits-and-redactions.md（2026-09-14）。換 schema 就升版號、舊檔整個重建（§1）。
📎 v8 → v9（`media` 加 `kind`、`verified`）一樣整個重建：升級後第一次開要重拉房間列表與訊息；`media` 列沒了，池裡沒人指著的檔會被掃描清掉（/docs/design/media/media-download.md §4.3、§8），要看就重下。

三條原則：

1. **多帳號混存，事件一份**。誰看得到哪一則由 `events_synced_log` 逐則記：server 經 `/messages`、`/sync`、`Recent` 任一條路給過這個 user 的才有列；讀取一律 JOIN 它，沒有列就看不到（fail closed）。🚫 不用「每人一個 r_seq 下界」：user2 在 200 加入、350 離開、500 回來，(350, 500) 他看不到；`history_visibility` 中途改過也會切洞——下界會 fail open。
2. **實體表 `INTEGER PRIMARY KEY`（rowid，64-bit）加識別碼的 UNIQUE 索引；純關聯表用兩個整數外鍵的複合主鍵、`WITHOUT ROWID`**。字串識別碼（mxid、room_id、event_id）在整個 DB 裡各只出現一次，其餘全走整數外鍵（維護者：event id 很長，能用指針就用外鍵）。整數 id 不出 `cache.rs`，對外 API 仍收字串。
3. **`PRAGMA foreign_keys = ON` 每條連線都下、下完再驗一次**：CASCADE 全靠它，SQLite 預設是關的。

```sql
CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
-- schema_version、server（URL，第一個開這份庫的帳號寫下的拼法）、created_at。之後照它開、host 不同就拒絕（§1）。

-- 看過的任何 mxid（本機帳號、事件的 sender 都在這）。哪些是本機帳號由資料目錄的 a/ 決定，不在表裡標。
CREATE TABLE users (id INTEGER PRIMARY KEY, mxid TEXT NOT NULL UNIQUE, first_seen_at INTEGER NOT NULL);
-- encrypted：這個房間開了 E2EE 沒有（維護者 2026-09-14 提）。
--   NULL ＝ 不知道（只因為收到事件才建出來的列）、0 ＝ 明文、1 ＝ 加密。
--   🚫 不是 NOT NULL DEFAULT 0：預設成 0 等於預設「明文」，那是最危險的預設。
--   🚨 只准 0 → 1，不准 1 → 0：Matrix 房間一開加密就關不掉，所以一份過期的「沒加密」（別的帳號舊的、有 bug 的）
--   不准把它蓋回明文 —— 蓋回去的下一步是送檔用 `cipher: none`，把區塊金鑰公開出去（/docs/design/media/wbf-client-convention-for-chunk.md §5.1）。
--   ⭐ 放 rooms 不放 room_list：加不加密是房間的性質、對每個帳號都一樣；room_list 是「這個帳號看到的樣子」。
--   讀的時候（list_room_entries、find_conversation）這一欄說 1 就蓋掉 conversation_json 裡的 encrypted：房間的事實贏過帳號的舊印象。
--   寫的地方兩個：拿一間房的樣子（room.get → upsert_conversations；房間列表只記 id、🚫 寫這一欄），以及收到加密的證據（upsert_events 看到 m.room.encryption 或任何加密事件就設 1，2026-10-05）。
--   送文字只看這一欄（find_room_encrypted；NULL 或沒有這列就報錯、請 UI 先拿房間，維護者 2026-10-05）；送附件照樣問 server 這一刻的狀態。
--   ⚠️ 0 也可能是過期的：房間在這台沒在聽的時候開了加密，這一欄要到下次拿房間或收到加密的證據才升上去，中間送的文字是明文（/docs/design/keys/e2ee-rpc.md §7）。
--   m.room.encryption 要帶非空的 algorithm 才算（room_state::is_encryption_content，跟拿房間同一個判斷）。
CREATE TABLE rooms (id INTEGER PRIMARY KEY, room_id TEXT NOT NULL UNIQUE, first_seen_at INTEGER NOT NULL,
  encrypted INTEGER CHECK (encrypted IN (0, 1)));

-- 事件一份，不記誰的。收到的原樣存 raw_event、要顯示的存 content_json（/docs/design/messages/edits-and-redactions.md）。
CREATE TABLE events (
  id INTEGER PRIMARY KEY,
  room INTEGER NOT NULL REFERENCES rooms(id) ON DELETE CASCADE,
  event_id TEXT NOT NULL,
  sender INTEGER NOT NULL REFERENCES users(id),
  r_seq INTEGER, g_seq INTEGER, origin_server_ts INTEGER NOT NULL,
  decrypted INTEGER CHECK (decrypted IN (0, 1)),   -- 1／0／NULL（NULL = 本來就不是加密事件）
  raw_event TEXT,                       -- 收到的原樣（密文就是密文）；第一次寫入後永遠不動。matrix-sdk 解開的拿不到密文 → NULL
  event_type TEXT,                      -- 明文的 type；還沒解開的是 NULL（/docs/design/messages/edits-and-redactions.md §8）
  content_json TEXT,                    -- 這一則自己的明文 content JSON（edit 是 m.new_content）；NULL = 還沒處理；寫進去就不改（/docs/design/messages/edits-and-redactions.md §2）
  is_processed INTEGER NOT NULL DEFAULT 0 CHECK (is_processed IN (0, 1)),
  is_redacted INTEGER NOT NULL DEFAULT 0 CHECK (is_redacted IN (0, 1)),
  class TEXT NOT NULL DEFAULT 'general' CHECK (class IN ('general', 'msg', 'edit', 'redact', 'reaction')),
  ref_event_id TEXT,                    -- edit／redact／reaction：指向的目標；msg：目前要顯示的 edit（/docs/design/messages/edits-and-redactions.md §3 的刻意例外：可能還不在，沒辦法外鍵）
  modified_timestamp INTEGER NOT NULL DEFAULT 0); -- 寫入時＝0；被 edit 換版或被 redact 時換成那個事件的 server 時間（/docs/design/messages/edits-and-redactions.md §5）
CREATE UNIQUE INDEX events_by_event_id ON events (room, event_id);
CREATE UNIQUE INDEX events_by_seq ON events (room, r_seq) WHERE r_seq IS NOT NULL;   -- 排序、判洞、跳第 N 則
CREATE INDEX events_by_time ON events (room, origin_server_ts);
CREATE INDEX events_by_ref ON events (room, ref_event_id) WHERE ref_event_id IS NOT NULL;   -- 「誰參照這則」

-- 同步紀錄：哪個 user 真的從 server 拿到過哪一則，至少一次。hidden = Delete for me（/docs/design/rooms/chat-model.md §5）：
-- 再同步只更新 last_synced_at，不動 hidden；讀取濾掉 hidden = 1。
CREATE TABLE events_synced_log (
  event INTEGER NOT NULL REFERENCES events(id) ON DELETE CASCADE,
  user INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  first_synced_at INTEGER NOT NULL, last_synced_at INTEGER NOT NULL,
  hidden INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (event, user)) WITHOUT ROWID;
CREATE INDEX events_synced_log_by_user ON events_synced_log (user, event);

-- 房間列表，一人一列。conversation_json 是這個 user 看到的 Conversation（power level、can_send 都是 per user）；
-- 名稱、加密與否、人數都在 JSON 裡，不另開欄。
-- 兩個寫入點（維護者 2026-10-05，/docs/design/rooms/chat-model.md §2.1）：
--   room.list（record_joined_rooms）：只拿「加入了哪些」。新的加一列（conversation_json／refreshed_at 是 NULL ＝ 還沒拿過）；
--     已經有的🚫 覆寫它的樣子；名單裡沒有的標 joined = 0、🚫 刪列（回來了再標回 1）。
--   room.get（upsert_conversations）：一間的樣子整份蓋上去、記 refreshed_at；🚫 動 joined（加入了沒只由 room.list 寫，對退出的房叫 room.get 不會標回加入；新列預設 1）。
-- room.list 讀 joined = 1 的，本地知道多少給多少（RoomListEntry，不知道的是 null）；room.get sync=local 退出的也給（最後看到的樣子）。
CREATE TABLE room_list (
  room INTEGER NOT NULL REFERENCES rooms(id) ON DELETE CASCADE,
  user INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  conversation_json TEXT, refreshed_at INTEGER,
  joined INTEGER NOT NULL DEFAULT 1 CHECK (joined IN (0, 1)),
  PRIMARY KEY (user, room)) WITHOUT ROWID;

-- 每個帳號的 Recent 水位線（cg_seq 是 per user 的：server 依 user 的可見範圍算）。
-- 🚫 to-device 的水位（cd_seq）不進這張表，也不進這個 db：它說的是「crypto store 收到哪」，
--    所以它寫在 m/ 裡面，跟 store 同生共死（維護者 2026-09-12：你同步到哪就寫到哪）。
--    這個 db 是可以被重建的（§5.1 的 Rebuilt），兩者的失效模式不一樣。理由在 /docs/design/keys/to-device-client.md §2.1。
CREATE TABLE sync_state (
  user INTEGER PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
  cg_seq INTEGER NOT NULL, updated_at INTEGER NOT NULL) WITHOUT ROWID;

-- 本地 offset（/docs/design/rooms/chat-model.md §3.5）：一人一房一列，指事件列。
CREATE TABLE read_positions (
  room INTEGER NOT NULL REFERENCES rooms(id) ON DELETE CASCADE,
  user INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  event INTEGER NOT NULL REFERENCES events(id) ON DELETE CASCADE,
  ts INTEGER NOT NULL,
  PRIMARY KEY (user, room)) WITHOUT ROWID;

-- 媒體：mxc → 池裡的檔（/docs/design/media/media-pool.md）。不分帳號、不要可見性（維護者 2026-09-07 定）：拿得到 mxc 的人 server 就給他檔，可見性在事件那層已經擋過。
-- pool_file 是明文的 BLAKE3 hex：同內容不同 mxc 只存一份；下載中先用暫存名，完成算完 hash 再 rename。
-- v9（維護者 2026-10-06）：加 kind 與 verified；chunk_size、file_size 只有 kind 1 一定有（/docs/design/rpc-specs/data-plane.md §7.1、/docs/design/media/media-download.md §12）。
CREATE TABLE media (
  id INTEGER PRIMARY KEY,
  mxc TEXT NOT NULL UNIQUE,
  pool_file TEXT,                       -- NULL = 還在下載
  name TEXT, mimetype TEXT,
  -- 這個檔是哪種格式，建列時照事件內容判斷：1 = wbf 分塊（org.wbftw.wbfuwunel.chunked）、2 = Matrix 加密（file，EncryptedFile v2）、3 = Matrix 明文（只有 url）。
  --   程式裡是 enum MediaKind { WbfChunked = 1, MatrixEncrypted = 2, MatrixPlain = 3 }，DB 存整數（維護者 2026-10-06）。
  --   🚫 0、🚫 DEFAULT：漏寫就被 NOT NULL 擋下，🚫 悄悄變成某一種。讀到不認得的數字就回錯（CHECK 擋著，不該發生）。
  kind INTEGER NOT NULL CHECK (kind IN (1, 2, 3)),
  -- 整檔跟發送者給的 hash 比對的結果（維護者 2026-10-06）：0 = 還沒驗、不知道；1 = 驗了、正確；2 = 驗了、不正確。
  --   程式裡是 enum Verification { Unknown = 0, Matched = 1, Mismatched = 2 }。驗過（1 或 2）就🚫 再驗。
  --   下載完自動驗，結果跟 complete = 1 在同一筆寫入裡（/docs/design/media/media-download.md §12.3）；沒有 hash 可比的（kind 3、區塊沒帶 sha256 的 kind 1）永遠 0。
  --   驗不過🚫 刪檔：資料留著、標 2，讀的時候用狀態碼講（傳統加密的 GET 是 412，/docs/design/rpc-specs/data-plane.md §8.2）。
  verified INTEGER NOT NULL DEFAULT 0 CHECK (verified IN (0, 1, 2)),
  -- 校驗碼 "<algo>:<值>"：
  --   kind 1：事件區塊有 sha256 就是 "sha256:…"（上傳者算的明文）；沒帶就下載完填 "blake3:…"（我們算的，同 pool_file）
  --   kind 2："matrix-sha256:<事件裡的 unpadded base64>"——算的是**密文**，🚫 跟 kind 1 的 "sha256:" 混用（同一個字串前綴說的是兩件事會被拿來互比）
  --   kind 3：下載完填 "blake3:…"
  hash TEXT,
  file_size INTEGER,                    -- 明文總長：kind 1 從事件區塊來（一定有）；kind 2、3 從 info.size（選填），沒給就下載完才填
  chunk_size INTEGER,                   -- 事件區塊的塊大小；只有 kind 1 有
  segments_written INTEGER NOT NULL,    -- 池主檔已落地的完整段數（64 KiB 一段，每 1.5 秒寫一次）：只給顯示，🚫 不當續傳依據（/docs/design/media/media-download.md §4.1）
  complete INTEGER NOT NULL,            -- 1 = 整檔都在
  bytes_on_disk INTEGER NOT NULL,       -- 配額用
  created_at INTEGER NOT NULL,          -- 下載（建立）時間
  last_used_at INTEGER NOT NULL,        -- 最後一次看過
  source_uri TEXT,                      -- 這台機器上傳它時 UI 給的原檔位置（URI）；讀的時候原檔在、大小對得上就讀它（/docs/design/rpc-specs/data-plane.md §8.1）
  -- kind 1 的兩個大小一定有（v8 的 NOT NULL 搬到這裡）；驗過（1、2）一定已經 complete。
  CHECK (kind <> 1 OR (file_size IS NOT NULL AND chunk_size IS NOT NULL)),
  CHECK (verified = 0 OR complete = 1));
CREATE INDEX media_lru ON media (last_used_at);
CREATE INDEX media_by_pool_file ON media (pool_file) WHERE pool_file IS NOT NULL;   -- 同一個檔被幾個 mxc 指著，清檔前要問

-- 事件 → 媒體。file 事件寫入時一併建；一則事件日後可能多個附件，所以獨立一張表。
CREATE TABLE event_media (
  event INTEGER NOT NULL REFERENCES events(id) ON DELETE CASCADE,
  media INTEGER NOT NULL REFERENCES media(id) ON DELETE CASCADE,
  PRIMARY KEY (event, media)) WITHOUT ROWID;
CREATE INDEX event_media_by_media ON event_media (media);
```

規則：

- **寫入**（`upsert_events(user_id, room_id, &[IncomingEvent])`，一個房間、一個 transaction）：mxid／room_id 換成整數 id（`INSERT OR IGNORE` 再 `SELECT id`）→ 事件照 /docs/design/messages/edits-and-redactions.md 寫入與處理（`raw_event` 只寫一次、`content_json` 不被密文蓋掉、file 內容建 `media`（已有就不動）與 `event_media`）→ `events_synced_log(event, user)` upsert（新列 `hidden = 0`，已有只更新 `last_synced_at`）。
- **讀取**（`history`／`files`）：`events JOIN events_synced_log JOIN users(reader) JOIN users(sender) JOIN rooms WHERE reader.mxid = ? AND room_id = ? AND hidden = 0 AND class IN ('msg', 'general')`，`r_seq DESC`，沒有 `r_seq` 退到 `origin_server_ts`（/docs/design/rooms/chat-model.md §4.3 的退化表）。每列從 `content_json` 組回 `Message`（/docs/design/messages/edits-and-redactions.md §8）；`files` 另加 `json_extract(content_json, '$.msgtype')` 是 /docs/design/media/wbf-client-convention-for-chunk.md §5 的檔、而且沒被 redact。
- **忘掉一個帳號**（`forget_account(user_id)`，UI 的「摧毀本帳號的本機紀錄」）：🚫 不是 `DELETE FROM users`（他可能是別人事件的 sender，會把事件 CASCADE 掉）。一個 transaction：刪他的 `events_synced_log`／`room_list`／`sync_state`／`read_positions` → `DELETE FROM events WHERE id NOT IN (SELECT event FROM events_synced_log)`（CASCADE 帶走 `event_media`）→ 沒事件指的 `media` 列（先記下 `pool_file`）→ 沒事件也沒清單的 `rooms`。回傳孤兒 `pool_file` 清單，**只含已經沒有別的 `media` 列指著的**（同 hash 去重過的檔可能還被別的 mxc 用）；呼叫者拿去刪池裡的檔，DB 先、檔案後。
  **預設不叫它；`account del`（即 `logout`）不叫它，只有 `account destroy` 叫。** 這個 server 已經沒有登入中的帳號時，登出直接刪 `cache.db`（維護者：「除非所有帳號被登出」）。
- **`destroy` 的順序**（維護者 2026-09-15）：閘門 → 忘掉鏈 → 池檔 → 裝置層（登出）→ recovery key → 刪目錄。
  - 忘掉鏈**趁 `cache.db` 還在**時跑，走這台 server 的唯一寫入者 `ServerCache`，🚫 不另開 `cache_of` 連線；`cache.db` 不在就不開，🚫 不為了忘掉而建一個空的。
  - 🚨 **登入、登出、destroy 全程握著 `<data dir>/account.lock`**（仿 `daemon.lock`）：OS 層的排他鎖，拿不到就回 `account_busy`（1013），🚫 不排隊。檔案不刪、不寫內容；整個資料目錄一把，鎖檔名🚫 不帶 server 或帳號（否則在外面留痕跡）。所以 destroy 期間不會有登入冒出新目錄，反之亦然。
  - 🚨 **回 Err ⇒ 帳號目錄還在**：它一不在，重跑 destroy 就找不到這個帳號。所以同 server 還有別的帳號就只刪它；它是最後一個時：
    先 `close_server_cache`（關不掉就停，什麼都沒刪）→ 放下 `s/<b58>/to_be_deleted.lock` → 刪 `s/<b58>/` 裡 `a/` 以外的東西（`cache.db`、媒體池）→ **再看一次** `a/` 是不是只剩它（有別的就只刪它、不收 server 目錄；有 `account.lock` 在，這一步是防線，🚫 不是同步）→ 刪帳號目錄 → 收尾。
  - **收尾**：server 目錄先**改名、名字最前面加 🗑️**（`s/🗑️<b58>_<b58>`），再收 `to_be_deleted.lock`、刪那個目錄。改名之後原本的路徑就不在了，之後登入這台 server 建的是新目錄；沒刪完的 `🗑️…` 掃描時一律跳過、也不觸發「local.key 換過了」的提示；改名目標已經存在就停下不覆蓋。這一步 🚫 **不回 Err**：帳號已經不在，回 Err 等於叫人重跑卻找不到它；只發一則 progress 說明。
  - `to_be_deleted.lock` 是**磁碟上的標記**、🚫 不是 OS 鎖：程序當掉也還在。它在的時候登入這台 server 一律回 `server_pending_removal`（1014），🚫 core 不自己收拾，訊息說出要手動刪的目錄。
- 解不開的加密事件也存（`decrypted = 0`、`class = general`、密文在 `raw_event`），之後明文到了才處理（/docs/design/messages/edits-and-redactions.md §4）；不存等於每次都要重拉。`recent` 拿到的原始 `m.room.encrypted` 就是這樣存的。
- **威脅模型的邊界（維護者 2026-09-07 定）**：混存的前提是**同一台機器上的多個帳號屬於同一個人**（它們本來就共用一把 `local.key`）。帳號 A 解開的明文，帳號 B 只要 server 也給過他那則（有 synced_log 列），就讀得到明文，即使 B 的裝置沒有 Megolm 金鑰——這是刻意的（快、不重複存），🚫 不是給不同人共用一台機器的設計。要那種隔離，用不同的 `--data-dir`（不同的 `local.key`）。
- **快取綁帳號的 home server**：`Cache` 的身份是 `session.sealed` 裡的 server，不吃 `--server` 覆蓋；`--server` 臨時指到別家時事件仍寫進原 server 的 `cache.db`。
- **洞**：有 `r_seq` 的 room，「快取裡有哪些」就是 `r_seq` 的集合，缺的就是洞，不存 token。沒有 `r_seq` 的 room 只快取最新一段連續視窗。
- **開 app 的同步**（UI 的順序，維護者定）：先刷房間列表（`room_list`，只有加入了哪些；看得到的房間 UI 再一間一間 `room.get`）→ `Event/Recent` 帶這個帳號的 `cg_seq` 一窗一窗拉（每個 `Batch` 寫一次 DB：事件進 `events` 加 `events_synced_log`），Batch 說 `more: true` 就帶 `before = 最後的 ls` 再一窗；追平（`more: false`，或空窗）才把**第一窗第一個 Batch 的 `fs`** 寫回 `sync_state`。🚫 不看 `tc < limit`：位元組上限滿的窗也是 `tc < limit`，中途斷線不推水位（wbfuwunel 的 pack-pipeline.md §6.4）→ 點進房間才刷該房歷史（`/messages`）。

### 5.1 實作備註

- `Cache::open(dir, key, identity)` 回 `OpenOutcome`（Reused／Created／Rebuilt），呼叫者印出來，重建不是靜默的。錯金鑰在第一次讀頁就是「file is not a database」，走重建；「這個 build 沒有 SQLCipher」是另一種錯，往上丟（§3 的 fail closed）。
- `read_positions.event` 指事件列：那則還沒進快取就拒絕標已讀（先同步再標）。
- `hide_message` 只對這個帳號的 synced_log 列動手，沒同步過的東西沒有可藏的（回 false）。
- `media` 列在 file 事件進來時 `INSERT OR IGNORE` 建（已有就不動），`kind` 照事件內容判斷（/docs/design/rpc-specs/data-plane.md §7.1；v9 起標準的 `m.file`／`m.image`… 也建）；下載進度與完成由 `media_begin`／`media_progress`／`media_finish` 寫（/docs/design/media/media-pool.md §3）。

## 6. 還開著的

目前沒有。


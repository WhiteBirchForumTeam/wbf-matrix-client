# 本地資料庫設計：加密的暫存快取

> 狀態：2026-09-05 草案，維護者同意 §1–§4、§6 的提案；§5（與 matrix-sdk store 的關係）維護者要求寫細再議。
> **2026-09-06 起動手**（維護者定：第 3 步第一版之後下一隻專注這裡）。做到哪：
>
> | 段 | 狀態 |
> |---|---|
> | §4 主金鑰、兩種鎖法、三把子金鑰、`session.sealed`、CLI 的 unlock ticket | ✅ 第一個 PR：`wbf-sdk::vault`（`Vault::create`／`open`／`read_mode`／`set_unlock`、`seal_session`／`unseal_session`）、CLI 的 `unlock.rs`。實作與這裡的差異見 §4.1 |
> | §5.3 matrix-sdk store 用第二把子金鑰 | ✅ 同一個 PR：`SqliteStoreConfig::key`，不走 PBKDF2 |
> | §3、§6 `cache.db`（SQLCipher） | ✅ 第二個 PR：`wbf-sdk::cache`（feature `cache`）、CLI 的 `recent`／`--from-cache`／寫穿、多帳號混存（當時叫 `accounts`／`forget-account`；命令名 2026-09-09 改成 `account` 一族，CLI 規格 §3.1，實作還沒跟上）。§6 的 schema 就是實作的（v2）；建置需求見 §3 |
> | §10 房間金鑰備份（server 一份、本地一份） | 📝 2026-09-09 設計定案，還沒實作。conf 檔（CLI 規格 §10）是它的前提，先做 conf 再做這個 |
> | §8 媒體儲存池 | ✅ 第三個 PR：`wbf-sdk::media_pool`（池的落地格式）、`wbf-sdk::media`（fetch／gc／sweep 的接法）、CLI `download` 走快取、`media-stats`／`media-gc`。格式與續傳細節見 §8.1、§8.3 的「實作」段 |

## 0. 一句話

本地有兩個 SQLite 檔：matrix-sdk 自己的 store（它非存不可的東西）與我們的快取（聊天紀錄、房間、事件區塊）。
兩個都加密，金鑰都從同一把 32 byte 主金鑰導出；主金鑰第一版明文放本地（自解密），加 passphrase 後被密碼包住，
啟動時要解開，UI 與 CLI 走同一個函數。**快取不是權威**：可以整個刪掉重建，衝突以 server 為準。

## 1. 定位：快取，不是權威

| 因為它是快取，所以 | 具體做法 |
|---|---|
| 可以整個丟掉 | schema 版本不對、server 換了、解不開：刪檔重建，不寫遷移。一個 server 一份、多帳號混存（§6），user 換了不丟 |
| 不需要衝突解決 | 同一個 event_id 再寫一次就覆蓋；server 說的算 |
| 事件快取只長不刪 | **500 是同步視窗，不是上限**（維護者 2026-09-05 訂正）：進房時把最新 500 則同步進快取；往舊滑超過就再 load 更舊的存進去；下次重讀那一段從快取拿，不重拉。事件小，不設上限 |
| 媒體快取有配額，但是 best effort | **2 GiB**（維護者 2026-09-05 定），不是 hard limit；另有 **7 天保護期**，期內用過的檔不自動刪（§8.5） |
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
| 媒體內容 | **不進 DB**，整檔明文放進加密的儲存池（§8）；DB 只放指針（`media`、`event_media`） |
| session 與 token | **不進 DB**（維護者 2026-09-05 定）：另外用同一把主金鑰導出的第三把子金鑰鎖一次，見 §4 與 §5.6 的 `session.sealed` |

## 3. 加密：SQLCipher，整檔頁級

`rusqlite` 的 `bundled-sqlcipher` feature（實作時確認 feature 名與 matrix-sdk 釘的 rusqlite 0.40 相容；
兩邊 feature 會統一，SDK 的 store 也會連到 SQLCipher 版的 sqlite，但它不下 `PRAGMA key`，行為就是普通 SQLite）。

- 用 **raw key** 開：`PRAGMA key = "x'<64 hex>'"`，跳過 SQLCipher 自己的 PBKDF2，金鑰導出由我們統一做（§4）。
- 不選「自己在 SQLite 上做每列 AEAD」：查詢變難、索引做不了、表名與列數等 metadata 漏在外面。
- Android：`bundled-sqlcipher` 是原始碼編譯，NDK 能編（實作時驗）。
- **建置需求（2026-09-07 實測）**：`rusqlite` 開 `bundled-sqlcipher-vendored-openssl`，feature 統一後整個 workspace 的 `libsqlite3-sys` 都是 SQLCipher 版（matrix-sdk 的 store 也連到它，但不下 `PRAGMA key`，行為是普通 SQLite）。SQLCipher 的 AES 走 OpenSSL：

  | 平台 | 需要什麼 |
  |---|---|
  | Windows MSVC | 編 vendored OpenSSL 要 **Strawberry Perl**（git-bash 的 msys perl 跑 `Configure` 會失敗）。cargo 前把 `C:\Strawberry\perl\bin` 排到 PATH 前面。第一次編要好幾分鐘 |
  | Linux | 系統 perl 就夠 |
  | macOS | SQLCipher 走 CommonCrypto，不編 OpenSSL |
  | Android | 還沒驗 |

- **fail closed**：`Cache::open` 下完 `PRAGMA key` 會問 `PRAGMA cipher_version`，空的（這個 build 沒有 SQLCipher，`PRAGMA key` 是 no-op）就拒絕開，不會靜默寫出明文快取。

## 4. 金鑰：一把主金鑰，兩種鎖法，型別化

> 用字（維護者 2026-09-07 定）：**passphrase** 是解 `local.key` 的那句話；**password** 一律指 Matrix 帳號密碼。早先草稿寫的「local password」就是 passphrase，已全部改掉，避免跟帳號密碼混。

```
local.key（0600）
  ├─ Plain            : { "v": 1, "mode": "plain", "master": "<base64 32 byte>" }
  └─ PassphraseWrapped  : { "v": 1, "mode": "passphrase",
                          "kdf": { "name": "argon2id", "m_kib": 65536, "t": 3, "p": 1, "salt": "<base64 16>" },
                          "nonce": "<base64 24>", "wrapped": "<base64 48>" }   // XChaCha20-Poly1305(KEK, master)
```

- **主金鑰** 32 byte，CSPRNG，一台機器一把。
- **導出**：三把 32 byte 子金鑰，`BLAKE3 derive_key(context, master)`，context 是固定字串
  `"wbf-matrix-client cache sqlcipher v1"`、`"wbf-matrix-client matrix-sdk store v1"`、`"wbf-matrix-client session v1"`，加第四把 `"wbf-matrix-client media store v1"`（§8 的池，就這一把）、
  第五把 `"wbf-matrix-client room key backup v1"`（§10.4 的本地房間金鑰池，加密與檔名 keyed hash 都用它）。
  第三把用 XChaCha20-Poly1305 把 session 檔（server、user_id、device_id、access_token）整份封成 `session.sealed`：
  session 與 token **不進 DB**，但跟 DB 同一把鎖（維護者 2026-09-05 定）。子金鑰不落地，每次開啟導一次。
  換 context 字串就是換金鑰，所以 context 帶版本。
- **passphrase**：Argon2id 從 passphrase 導 KEK，KEK 用 XChaCha20-Poly1305 包住主金鑰。改 passphrase 只重包 48 byte，DB 不動。
  參數寫在檔裡，之後調高不用遷移。
- **模式是型別，不是空字串**：`enum KeyFile { Plain { master }, PassphraseWrapped { kdf, nonce, wrapped } }`，
  讀檔時 `mode` 不認得就拒絕。「沒設 passphrase」是 `Plain`，不是「passphrase 等於空字串」——後者會讓空 passphrase 靜默通過。
- **一個入口**：`Vault::open(dir, Unlock::NoPassphrase | Unlock::Passphrase(secret))`。`Plain` 配 `NoPassphrase`、
  `PassphraseWrapped` 配 `Passphrase`，配錯就 `Err`，UI 與 CLI 都只能走這裡。CLI 的 passphrase 來源同 `login` 的 password：檔案參數或終端不回顯。
- **解鎖後金鑰放哪**（維護者 2026-09-05：作法由我定，照一般開發工具的做法）：

  | | 做法 |
  |---|---|
  | UI | 解鎖一次，主金鑰只在記憶體；UI runtime 與 wbf-sdk 是同一個程序，關掉就沒了 |
  | CLI（只在開發與 debug 用） | 仿 `sudo`：解鎖成功後寫一張 **unlock ticket**（`<data dir>/unlock.ticket`，0600，內容是主金鑰加 `expires_at`），有效期預設 15 分鐘、`--unlock-ttl <秒>` 可調；期內的命令不再問 passphrase。`lock` 命令刪掉它。過期的 ticket 讀到就刪，Unix 上模式不是 0600 就拒用 |

  CLI passphrase 的來源與 `login` 的 password 同一套：`--passphrase-file <檔>` 或終端不回顯；不接受命令列明文與環境變數。優先順序：檔案參數 → 有效的 ticket → 問終端。
  ticket 是明文主金鑰落地，安全性等於 `Plain` 模式那 15 分鐘；維護者明說接受（CLI 不是產品面）。這一項不進 UI。

### 4.1 實作與上面的差異（第一個 PR，2026-09-06）

- `local.key` 的 `passphrase` 模式在 JSON 裡 `mode` 值是 `"passphrase"`（上面的 `PassphraseWrapped` 是型別名，程式裡也叫 `KeyFile::Passphrase`）。
- 包主金鑰與封 session 都帶固定的 AEAD 附加資料（`wbf-matrix-client local.key v1`、`wbf-matrix-client session.sealed v1`）：把 A 檔的密文搬到 B 檔解不開。
- 多一個 `Vault::read_mode(dir)`：只看鎖法不解。CLI 用它決定要不要問 passphrase，🚫 不靠 `open` 失敗的錯誤字串判斷（那是 parse Display 的老毛病，matrix-sdk 那次踩過）。
- `Vault::set_unlock(&Unlock)` 一個函數涵蓋設 passphrase、改 passphrase、拿掉 passphrase：只重寫 `local.key`，主金鑰不變，所以 `session.sealed` 與 SDK store 不動。空字串 passphrase 在這裡被拒。
- `Vault::from_master(dir, master, mode)` 給 CLI 的 ticket 用；它不驗證主金鑰是不是這個目錄的，信任等於 `Plain`。
- 第四把子金鑰 `media store v1` 已經導出來（`media_store_key`），還沒有人用；先把 context 字串一次定完。
- 寫 `local.key`／`session.sealed`／ticket 都先寫暫存檔再 rename（`vault::write_private`）：寫到一半斷電不留半個檔。
- 既有的 store 用別把金鑰開會失敗：訊息叫人刪 `matrix/` 重新 `login`，不遷移（§1 的政策）。
  ⚠️ 這裡原本寫的理由是「store 只是裝置狀態」——**那句話對 `crypto.db` 是錯的**，它裝著解開全部歷史的房間金鑰。
  政策本身維護者 2026-09-09 決定不改，但它站得住的前提是 §10 的本地金鑰池（不跟著被刪、`key-backup import` 讀得回來）。

- **威脅模型**（老實寫）：

| 防 | 不防 |
|---|---|
| 把 DB 檔拷走的人（沒有 `local.key` 解不開） | 能登入這台機器、讀得到 `local.key` 的人（`Plain` 模式） |
| 加 passphrase 後：連 `local.key` 一起拷走也解不開（要猜 passphrase，Argon2id 拖慢） | 跑著的程序記憶體裡的主金鑰；鍵盤側錄 |

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
- 加 passphrase 後，SDK 的 store 也一起被鎖住：主金鑰解不開就導不出子金鑰，`open_with_key` 就失敗。**不需要動 SDK。**
- 兩個世界的邊界只有一條線：`Vault::open` 回兩把子金鑰。SDK 不知道 SQLCipher，快取不知道 `StoreCipher`。

### 5.4 為什麼不把快取塞進 SDK 的 event_cache（方案 B）

- 它的 schema（linked chunk）是為 SDK 的 `Timeline` 設計的，`files`（依 msgtype 找事件）、`read --type`／`--sender`、配額清理這些是我們的查詢，
  對著它做要嘛繞它的 API、要嘛直接讀它的表（等於綁死上游內部結構，上游改 migration 我們就壞）。
- 它的加密是每值加密、檔案結構外露；我們的快取要整檔加密（§3）。
- 它存不存、存多少由 SDK 決定；§1 的配額與「整個丟掉」政策要自己掌控。
- 代價：多一個檔、多一份「事件」的複本（SDK 記憶體裡一份、我們 DB 一份）。可接受：記憶體那份程序結束就沒了。

### 5.5 為什麼不讓 SDK 也用 SQLCipher（方案 C）

要改 `matrix-sdk-sqlite` 的開檔路徑下 `PRAGMA key`，等於 fork submodule；而 5.3 已經讓密碼一把鎖住兩邊，收益只剩「SDK 的檔案結構也藏起來」。不值得。

### 5.6 各檔在磁碟上（2026-09-07 修訂：一台機器一把鑰、一個 server 一份快取、一個帳號一套 session）

```
<data dir>/
  local.key                      主金鑰（§4），一台機器一把，所有帳號共用
  unlock.ticket                  CLI 的 unlock ticket（§4）
  current                        CLI 的目前帳號
  servers/<server host>/
    cache.db                     這個 server 上所有帳號共用（§6）
    media/                       媒體儲存池（§8），跟 cache.db 同層、同範圍
    accounts/<localpart>/
      session.sealed             第三把子金鑰封住的 session
      matrix/                    SDK 的 store（crypto.db、state.db），綁 device；logout 刪
      room-keys/                 本地房間金鑰備份（§10.4），一房一檔；第五把子金鑰；`account del`／`destroy` 連它一起刪（§10.7 的閘門）
```

`<data dir>`：Windows `%APPDATA%`、macOS `~/Library/Application Support`、Linux `$XDG_DATA_HOME`（沒設就 `~/.local/share`）。

與 2026-09-05 草稿的差異（維護者 2026-09-07 定）：
- 草稿寫 `<data dir>/wbf/<server host>/<user localpart>/` 一帳號一套、`local.key` 在帳號底下。改成 **`local.key` 在頂層**：主金鑰的定位是「這台機器」（§4 第一條），passphrase 也是一台機器一個；一帳號一把會變成每個帳號各自問 passphrase。
- **`cache.db`（與媒體池）在 server 層，多帳號共用**：維護者要的是混存——user1 看得到 room1／2／3、user2 看得到 room1／2／4，不論誰登入都同步進同一個 DB，事件只存一份，可見性逐則記（§6）。共用範圍是同一個 server：`r_seq`／`g_seq` 是 fork server 發的，不同 homeserver 上序號不同。
- 帳號目錄名做過檔名安全化，只是定位；真正的 URL 與 mxid 在 `session.sealed`。

## 6. 快取的 schema（v2，2026-09-07 維護者定；就是 `wbf-sdk::cache` 建的）

三條原則：

1. **多帳號混存，事件一份**。誰看得到哪一則由 `events_synced_log` 逐則記：server 經 `/messages`、`/sync`、`Recent` 任一條路給過這個 user 的才有列；讀取一律 JOIN 它，沒有列就看不到（fail closed）。🚫 不用「每人一個 r_seq 下界」：user2 在 200 加入、350 離開、500 回來，(350, 500) 他看不到；`history_visibility` 中途改過也會切洞——下界會 fail open。
2. **實體表 `INTEGER PRIMARY KEY`（rowid，64-bit）加識別碼的 UNIQUE 索引；純關聯表用兩個整數外鍵的複合主鍵、`WITHOUT ROWID`**。字串識別碼（mxid、room_id、event_id）在整個 DB 裡各只出現一次，其餘全走整數外鍵（維護者：event id 很長，能用指針就用外鍵）。整數 id 不出 `cache.rs`，對外 API 仍收字串。
3. **`PRAGMA foreign_keys = ON` 每條連線都下、下完再驗一次**：CASCADE 全靠它，SQLite 預設是關的。

```sql
CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
-- schema_version、server（URL）、created_at。身份只有 server：不符就重建（§1）。

-- 看過的任何 mxid（本機帳號、事件的 sender 都在這）。哪些是本機帳號由 CLI 的 accounts/ 目錄決定，不在表裡標。
CREATE TABLE users (id INTEGER PRIMARY KEY, mxid TEXT NOT NULL UNIQUE, first_seen_at INTEGER NOT NULL);
CREATE TABLE rooms (id INTEGER PRIMARY KEY, room_id TEXT NOT NULL UNIQUE, first_seen_at INTEGER NOT NULL);

-- 事件一份，解密後的明文放這裡，不記誰的。
-- message_json 是 Message 去掉 id／conversation／sender／sent_at／r_seq／g_seq／decrypted 之後的剩餘（kind、reply_to、edited_by、
-- reactions、undecryptable_reason），讀出時從欄位組回完整的 Message（split_message／join_message），同一份資料不存兩次。
CREATE TABLE events (
  id INTEGER PRIMARY KEY,
  room INTEGER NOT NULL REFERENCES rooms(id) ON DELETE CASCADE,
  event_id TEXT NOT NULL,
  sender INTEGER NOT NULL REFERENCES users(id),
  r_seq INTEGER, g_seq INTEGER, origin_server_ts INTEGER NOT NULL,
  kind INTEGER NOT NULL,                -- 0 text、1 file、2 deleted、3 system、4 unsupported
  decrypted INTEGER,                    -- 1／0／NULL（NULL = 本來就不是加密事件）
  message_json TEXT NOT NULL);
CREATE UNIQUE INDEX events_by_event_id ON events (room, event_id);
CREATE UNIQUE INDEX events_by_seq ON events (room, r_seq) WHERE r_seq IS NOT NULL;   -- 排序、判洞、跳第 N 則
CREATE INDEX events_by_time ON events (room, origin_server_ts);
CREATE INDEX events_files ON events (room) WHERE kind = 1;

-- 同步紀錄：哪個 user 真的從 server 拿到過哪一則，至少一次。hidden = Delete for me（chat-model §5）：
-- 再同步只更新 last_synced_at，不動 hidden；讀取濾掉 hidden = 1。
CREATE TABLE events_synced_log (
  event INTEGER NOT NULL REFERENCES events(id) ON DELETE CASCADE,
  user INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  first_synced_at INTEGER NOT NULL, last_synced_at INTEGER NOT NULL,
  hidden INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (event, user)) WITHOUT ROWID;
CREATE INDEX events_synced_log_by_user ON events_synced_log (user, event);

-- 房間清單，一人一列。conversation_json 是這個 user 看到的 Conversation（power level、can_send 都是 per user）；
-- 名稱、加密與否、人數都在 JSON 裡，不另開欄。
CREATE TABLE room_list (
  room INTEGER NOT NULL REFERENCES rooms(id) ON DELETE CASCADE,
  user INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  conversation_json TEXT NOT NULL, refreshed_at INTEGER NOT NULL,
  PRIMARY KEY (user, room)) WITHOUT ROWID;

-- 每個帳號的 Recent 水位線（cg_seq 是 per user 的：server 依 user 的可見範圍算）。
CREATE TABLE sync_state (
  user INTEGER PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
  cg_seq INTEGER NOT NULL, updated_at INTEGER NOT NULL) WITHOUT ROWID;

-- 本地 offset（chat-model §3.5）：一人一房一列，指事件列。
CREATE TABLE read_positions (
  room INTEGER NOT NULL REFERENCES rooms(id) ON DELETE CASCADE,
  user INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  event INTEGER NOT NULL REFERENCES events(id) ON DELETE CASCADE,
  ts INTEGER NOT NULL,
  PRIMARY KEY (user, room)) WITHOUT ROWID;

-- 媒體：mxc → 池裡的檔（§8）。不分帳號、不要可見性（維護者 2026-09-07 定）：拿得到 mxc 的人 server 就給他檔，可見性在事件那層已經擋過。
-- pool_file 是明文的 BLAKE3 hex：同內容不同 mxc 只存一份；下載中先用暫存名，完成算完 hash 再 rename。
CREATE TABLE media (
  id INTEGER PRIMARY KEY,
  mxc TEXT NOT NULL UNIQUE,
  pool_file TEXT,                       -- NULL = 還在下載
  name TEXT, mimetype TEXT,
  hash TEXT,                            -- 明文校驗碼 "<algo>:<hex>"：事件區塊有 sha256 就是 "sha256:…"（上傳者算的）；沒帶就下載完填 "blake3:…"（我們算的，同 pool_file）
  file_size INTEGER NOT NULL,           -- 明文總長，從事件區塊來
  chunk_size INTEGER NOT NULL,          -- 下載時的塊大小，續傳截檔用
  chunks_written INTEGER NOT NULL,      -- 最後一次快照時已 append 的塊數（§8.3，每 1–2 秒 flush）
  complete INTEGER NOT NULL,            -- 1 = 整檔都在
  bytes_on_disk INTEGER NOT NULL,       -- 配額用
  created_at INTEGER NOT NULL,          -- 下載（建立）時間
  last_used_at INTEGER NOT NULL);       -- 最後一次看過
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

- **寫入**（`upsert_messages(user_id, &[Message])`，一個 transaction）：mxid／room_id 換成整數 id（`INSERT OR IGNORE` 再 `SELECT id`）→ 事件 upsert（`ON CONFLICT(room, event_id)`；**唯一不覆蓋的情況是密文不蓋明文**：快取裡 `decrypted = 1`、來的 `decrypted = 0` 就跳過）→ file 事件建 `media`（已有就不動）與 `event_media` → `events_synced_log(event, user)` upsert（新列 `hidden = 0`，已有只更新 `last_synced_at`）。
- **讀取**（`history`／`files`）：`events JOIN events_synced_log JOIN users(reader) JOIN users(sender) JOIN rooms WHERE reader.mxid = ? AND room_id = ? AND hidden = 0`，`r_seq DESC`，沒有 `r_seq` 退到 `origin_server_ts`（chat-model §4.3 的退化表）。
- **忘掉一個帳號**（`forget_account(user_id)`，UI 的「摧毀本帳號的本機紀錄」）：🚫 不是 `DELETE FROM users`（他可能是別人事件的 sender，會把事件 CASCADE 掉）。一個 transaction：刪他的 `events_synced_log`／`room_list`／`sync_state`／`read_positions` → `DELETE FROM events WHERE id NOT IN (SELECT event FROM events_synced_log)`（CASCADE 帶走 `event_media`）→ 沒事件指的 `media` 列（先記下 `pool_file`）→ 沒事件也沒清單的 `rooms`。回傳孤兒 `pool_file` 清單，**只含已經沒有別的 `media` 列指著的**（同 hash 去重過的檔可能還被別的 mxc 用）；呼叫者拿去刪池裡的檔，DB 先、檔案後。**預設不叫它；`account del`（即 `logout`）不叫它，只有 `account destroy` 叫**；這個 server 最後一個帳號登出時 CLI 直接刪 `cache.db`（維護者：「除非所有帳號被登出」）。
- 解不開的加密事件也存（`decrypted = 0` 帶原因），之後拿到金鑰重解時覆蓋；不存等於每次都要重拉。`recent` 拿到的原始 `m.room.encrypted` 是 `decrypted = 0`、原因 `NotDecryptedHere`。
- **威脅模型的邊界（PR #13 審查 rumia 🟡1，維護者 2026-09-07 定）**：混存的前提是**同一台機器上的多個帳號屬於同一個人**（它們本來就共用一把 `local.key`）。帳號 A 解開的明文，帳號 B 只要 server 也給過他那則（有 synced_log 列），就讀得到明文，即使 B 的裝置沒有 Megolm 金鑰——這是刻意的（快、不重複存），🚫 不是給不同人共用一台機器的設計。要那種隔離，用不同的 `--data-dir`（不同的 `local.key`）。
- **快取綁帳號的 home server**：`Cache` 的身份是 `session.sealed` 裡的 server，不吃 `--server` 覆蓋；`--server` 臨時指到別家時事件仍寫進原 server 的 `cache.db`（PR #13 審查 salvia 🟢3、cirno）。
- **洞**：有 `r_seq` 的 room，「快取裡有哪些」就是 `r_seq` 的集合，缺的就是洞，不存 token。沒有 `r_seq` 的 room 只快取最新一段連續視窗。
- **開 app 的同步**（UI 的順序，維護者定）：先刷房間清單（`room_list`）→ `Event/Recent` 帶這個帳號的 `cg_seq` 一窗一窗拉（每個 `Batch` 寫一次 DB：事件進 `events` 加 `events_synced_log`），`tc == limit` 就帶 `before = 最後的 ls` 再一窗；追平（`tc < limit`）才把**第一窗第一個 Batch 的 `fs`** 寫回 `sync_state`，中途斷線不推水位（server 的 pack-pipeline §6.4）→ 點進房間才刷該房歷史（`/messages`）。

### 6.1 實作備註

- `Cache::open(dir, key, identity)` 回 `OpenOutcome`（Reused／Created／Rebuilt），呼叫者印出來，重建不是靜默的。錯金鑰在第一次讀頁就是「file is not a database」，走重建；「這個 build 沒有 SQLCipher」是另一種錯，往上丟（§3 的 fail closed）。
- `read_positions.event` 指事件列：那則還沒進快取就拒絕標已讀（先同步再標）。
- `hide_message` 只對這個帳號的 synced_log 列動手，沒同步過的東西沒有可藏的（回 false）。
- `media` 這一版只建與寫指針（`find_media`／`touch_media`、file 事件進來時的 `INSERT OR IGNORE`）；池與下載管線是 §8 的 PR。

## 8. 媒體儲存池：整檔明文放進一個加密的池，不進 DB（維護者 2026-09-07 定）

> 2026-09-05 的版本寫成「每個檔一把金鑰、內容切固定塊各自 AEAD、bitmap 記哪塊在」。那是把 **server 端**的分片設計套到本地：server 那邊確實是 mxc 唯一、每檔各自 key、分片存、給下載與 seek 用；**本地快取跟它無關**。維護者 2026-09-07 糾正，改成下面這樣。

一句話：本地有**一個加密的儲存池**，裡面是**一個個完整的明文檔**；下載時解一塊就順序 append 進去，下載完 DB 記一列。池用**一把金鑰**。

### 8.1 池是什麼

- 對上層（下載管線、UI）看起來像掛載了一塊區域：開檔、順序寫、讀、刪，檔就是完整明文，**檔內不分片**。
- 落地時整個池是加密的。池是 `wbf-sdk` 裡的一層虛擬層，不用 FUSE／WinFsp（Windows 要裝東西、Android 沒有）。
- 池只用**一把金鑰**：第四把子金鑰 `"wbf-matrix-client media store v1"`（§4）。沒有每檔金鑰。
- 為了「順序 append」與「從中間讀」，池內部落地會分段加密（gocryptfs、Cryptomator、age 都是這樣）。**那是池的實作細節，對上層完全隱藏**；設計文件不規定段大小，實作時定、寫在程式的 docstring。

**實作（`wbf-sdk::media_pool`，2026-09-08）**：檔頭 32 byte（magic `WBFP`、版本、段大小、每檔隨機的 16 byte nonce_base）；之後每段明文 64 KiB（只有最後一段可短）各自 XChaCha20-Poly1305，nonce = nonce_base ‖ 段號、AAD = `"wbf-media-pool v1"` ‖ nonce_base ‖ 段號，密文段 = 明文 + 16 byte 標籤、固定偏移所以能隨機讀。對上層是 `PoolWriter`（`Write`，順序 append）與 `PoolReader`（`Read + Seek`，明文位置）。段號進 nonce 與 AAD：把第 3 段搬到第 5 段解不開；nonce_base 每檔隨機：同內容的兩份暫存檔密文不同，去重靠 hash 不靠密文。

存的是**解密後的明文**（維護者：「每塊解密後就放這裡」），不存 server 密文：server 密文的金鑰是每則訊息各自的（事件區塊裡的 `key`），本地要留一堆房間金鑰才讀得回來；明文進池，本地只有一套金鑰制度。

### 8.2 磁碟上

```
servers/<server host>/media/<hash 前 2 hex>/<hash>     hash = 明文的 BLAKE3，32 位小寫 hex；原檔名、mimetype、mxc 只在 cache.db
```

- **檔名是明文的 hash**（維護者 2026-09-07 定，取代草稿的隨機 id）：同內容不同 mxc 只存一份；`media.pool_file` 指過來，`media_by_pool_file` 索引回答「這個檔被幾個 mxc 指著」，清檔前要問。下載中還算不出 hash，先用 `media.id` 當暫存名，完成算完 hash 再 rename、寫回 `pool_file`。
- 目錄用前 2 hex 扇出（一個目錄不會塞幾萬個檔）。磁碟上能看到的只有「幾個檔、各多大」。
- 池跟 `cache.db` 同層：同一個 server 的所有帳號共用，不分帳號、不要可見性（拿得到 mxc 的人 server 就給他檔；可見性在事件那層擋過）。

### 8.3 下載時怎麼寫（只考慮 download 模式）

第一版只做**順序整檔下載**：`download` 從第 0 塊拿到最後一塊，每解出一塊就 append 進池裡那個檔。串流與 seek 先不管（§8.6）。

```
開始   ：cache.db 的 media 列（file 事件進快取時就建了，complete = 0）；池裡開一個新檔
每一塊 ：解密 → 驗 → append 進檔 → 記憶體裡的 chunks_written += 1
每 1–2 秒：把記憶體的 chunks_written 寫回 cache.db（一次 UPDATE），不每塊寫 DB
結束   ：檔 fsync → cache.db 把 complete = 1、chunks_written = 總塊數、bytes_on_disk 寫齊
```

- **進度在記憶體，DB 是每 1–2 秒的快照**（維護者定）。這樣 DB 不會被每塊一次的寫入打爆，而中斷最多重下一兩秒的量。
- **中斷續傳**：下次開始前查到 `complete = 0` 的列，把池裡那個檔**截到 `chunks_written × chunk_size`**（最後一次快照之後 append 的塊可能只寫了一半，不信任它），從第 `chunks_written` 塊續。這跟 `wbf-sdk` 現有的上傳狀態檔是同一種思路：狀態說到哪就從哪開始，不猜。
- **寫入順序**：先 append 檔、再更新 DB；反過來會出現「DB 說有、檔案沒有」。讀的時候 `complete = 1` 才當成有快取。
- 一塊驗證失敗：這次下載中止、檔截回上次快照，下次續。與下載端現有的規則一致（完整性每塊各自驗）。

**實作（`wbf-sdk::media`，2026-09-08）**：
- `fetch(client, manifest, cache, pool)`：`media_begin` 建或取列 → 完整且檔在就 `CacheHit`（touch）→ 不然決定續傳點（上次快照的 `chunks_written`，而且 `chunk_size` 要一樣）→ `pool.resume_pending` 或 `create_pending` → 逐塊 `read_and_open_chunk` 寫進 `PoolWriter`，每 1.5 秒 `sync` 加 `media_progress` → 完成 `finish()` 拿 BLAKE3 → `adopt`（同 hash 去重）→ `media_finish`。中止時 DB 停在上次快照、檔留著。
- **暫定段**：進度快照時記憶體裡湊不滿 64 KiB 的那段也要落地，不然快照指到的資料不在磁碟上、續不了。`PoolWriter::sync()` 把它先封成一個短段寫在檔尾，下次湊滿再把檔截回該段起點重封（每 1.5 秒重寫最多 64 KiB，可忽略）。續傳時 `resume_pending(trusted_len)` 解到 trusted_len 為止、把最後那個不完整段的明文放回記憶體、檔截到該段起點，BLAKE3 從頭重算。
- 暫存檔名是 `m<media.id>`，在 `media/pending/`。
- 實跑（2026-09-08，本機 wbfuwunel，40 MiB、64 KiB 塊、HTTP 通道）：在第 204 塊殺掉，續傳從第 172 塊（上次快照）開始，sha256 對。

### 8.4 DB 的指針

表在 §6（`media` 與 `event_media`）。沒有 bitmap：檔要嘛完整、要嘛是一個「寫到第 N 塊」的半成品，沒有中間有洞的狀態。
摧毀帳號的鏈（§6 `forget_account`）走到 `media` 這一層時回傳沒人指的 `pool_file`，池刪檔。

### 8.5 配額與清理（維護者 2026-09-05 定，沒變）

兩個數字，都是 UI 設定、可調：

| | 預設 | 意思 |
|---|---|---|
| 配額 | **2 GiB** | **best effort，不是 hard limit**：超過就試著刪，刪不了就算了，永遠不因為配額拒絕下載 |
| 保護期 | **7 天** | `last_used_at` 在 7 天內的檔**不自動刪**，不管超過配額多少 |

規則：

- 只算 `bytes_on_disk` 的加總。超過配額時，候選只有**保護期外**的檔，照 `last_used_at` 由舊到新刪整個檔，刪到不超過或候選用完為止。
- 候選用完還是超過（7 天內瘋狂下載）：**不刪、不擋**，UI 顯示「媒體快取超過配額」並提供**手動清理**（清全部、或清到某個日期以前）。
- 單一檔就超過配額（一個 2 GiB 的分塊檔）：照樣下、照樣存；下一個檔進來時，它若已出保護期就是第一個被刪的，若還在保護期就留著。多個小檔超過配額、刪最舊的（7 天外），是正常情形。
- 整檔進整檔出。先刪檔再刪列，中途死掉留下的孤兒列在下次啟動掃一次清掉；`complete = 0` 且沒有下載在跑的半成品也在啟動時掃，超過保護期就清。
- 有把手打開的檔不刪；讀一次就更新 `last_used_at`，所以正在看的東西自然在保護期內。
- 事件快取不受這個配額（§1）。

**實作（`wbf-sdk::media`）**：`collect_garbage(cache, pool, quota, protect, now)` 照上面的規則，先刪檔再 `media_reset` 列；`media_references` 大於 1（同 hash 去重過）的池檔不刪檔只清列。`sweep(cache, pool, protect, now)` 啟動掃：DB 說完整但檔不在 → reset；半成品超過保護期 → 刪暫存檔加 reset；`pending/` 裡沒有列認領的 → 刪。CLI：`media-gc [--quota-mib] [--protect-days]` 先 sweep 再 gc、`media-stats`（CLI 規格 §3.5）。UI 之後要的「手動清理」就是 quota 0 或直接刪 `media/`。

### 8.6 先不做的

- **串流與 seek 對著池讀**：之後 UI 要播中間一段，是對著池裡的完整檔 `Read + Seek`，不是對著分片；沒下載完的檔就先下載完（或只走 server 的 seek，不進池）。哪一種等 UI 那版再定。
- **部分快取**（只存看過的那幾塊）：不做。檔要嘛完整、要嘛是續傳中的半成品。

### 8.7 與下載管線的接法

`download` 現在是「`Read` 一塊 → 驗長度 → 解密 → 交出去」；加快取就是：開始前問 `media` 有沒有 `complete = 1` 的列，有就直接從池裡讀整檔；沒有就照 §8.3 邊下邊 append。
邊界仍在 `wbf-sdk`：CLI 與 UI 只看到一個檔的把手（`PoolReader`），不知道底下是池還是 server。

做法（2026-09-08）：`download.rs` 的 `read_and_open_chunk` 開成 crate 內可見，`media::fetch` 用它逐塊拿；`WbfClient::download`（直接寫到 `Write`）留著給 `--no-cache` 與 `--token` 模式。三個模組的關係：`cache` 不知道池、`media_pool` 不知道 DB、下載管線不知道兩者，只有 `media.rs` 同時碰三者。

### 8.8 池的檔案格式（權威；`wbf-sdk::media_pool` 就是照這裡寫的，改這裡要換版本號）

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
             key   = 第四把子金鑰 "wbf-matrix-client media store v1"（32 byte，全池共用，§4）
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
**讀取（`PoolReader`）**：`seek` 只改明文位置；`read` 算出段號、讀那一段解開、快取在記憶體，同段連續讀不重解。任何一段標籤不對回 `Io` 錯，上層當「檔壞了、重拉」（§1）。

**跟 server 的 chunk 無關**：server 的 `chunk_size` 是傳輸單位、每檔可不同（事件區塊裡）；池的 `segment_size` 是儲存單位、寫在每個檔頭。下載時一個 chunk 的明文丟進 `PoolWriter`，它照自己的 64 KiB 切，兩邊不必對齊；續傳點落在段中間也照上面的尾巴規則處理。

**檔名與目錄**：完成檔是 `media/<hash 前 2 hex>/<hash>`（hash = 明文 BLAKE3，32 位小寫 hex），下載中是 `media/pending/m<media.id>`。**檔案本身不帶任何 metadata**：原檔名、mimetype、校驗碼、mxc、大小都只在 `cache.db` 的 `media` 列（`name`／`mimetype`／`hash` 來自事件區塊，上傳者填的；`hash` 的形式是 `<algo>:<hex>`，區塊沒帶 sha256 時下載完用我們算的 BLAKE3 補成 `blake3:…`；同內容去重成一個池檔時，每個 mxc 各自保留自己的 name／mimetype／hash）。磁碟上能看到的只有幾個檔、各多大。

**安全性質**：段號進 nonce 與 AAD → 段搬位置、跨檔拼接都解不開；nonce_base 每檔隨機 → 同內容兩次寫入密文不同（去重靠 hash，不靠密文）；一把金鑰配隨機 nonce_base 加段號，nonce 不重複；金鑰不進錯誤訊息。**暫定段用自己的 nonce**（段號最高位設 1，PR #14 審查 rumia 🟡3）：同段號的暫定段與之後的正式段是兩個 nonce，每個 nonce 只封一次——AEAD 同 (key, nonce) 封兩份不同明文會漏 Poly1305 金鑰，這條路被堵死；段號因此只用 63 位。`resume_pending` 讀暫存檔尾巴時先用暫定 nonce、解不開再用正式的（尾巴可能是暫定段，也可能是快照點落在中間的正式整段）；完成檔裡永遠沒有暫定段。**不防**：能讀 `local.key` 的人（同 §4 的威脅模型）、檔案大小與數量。

**快取命中的核對**（PR #14 審查 rumia／salvia 🟡）：`fetch` 命中前比對池檔開得起來、明文長度等於區塊 `file_size`、區塊帶 sha256 時要等於 `media.hash`；任一不符 `media_reset` 重下。CLI 的 `sha256_verified` 只在這次真的下載才 true，命中報 false 並附 `hash`。

**測試**（`media_pool.rs` 的單元測試）：跨段寫讀與 seek、去重、續傳在段中／段界／可信長度超過檔長要拒、翻一個 byte／錯金鑰／第 0 段搬到第 1 段都拒、空檔。

## 9. 還開著的

1. ~~session 與 token 要不要搬進 DB~~ 定了：不進 DB，`session.sealed`（§4）。~~CLI 每個命令輸密碼的體感~~ 定了：仿 sudo 的 unlock ticket（§4）。
2. ~~媒體內容快取另議~~ 定了：§8（2026-09-07 重寫成儲存池）。
3. ~~媒體配額的數字~~ 定了：2 GiB best effort、保護期 7 天（§8.5）。事件快取不設上限；同步視窗 500 則／房、初開全域 10000 則是預設值，可調。
4. ~~房間金鑰只在 `crypto.db`，刪了就沒~~ 定了：§10（維護者 2026-09-09），server 一份標準 backup、本地一份加密金鑰池。

## 10. 房間金鑰的備份：server 一份、本地一份（維護者 2026-09-09 定）

### 10.1 為什麼要有這一章

在這一章之前，房間金鑰（Megolm inbound session）**只活在一個地方**：`accounts/<localpart>/matrix/crypto.db`。
整個 repo 沒有任何一行碰 `/room_keys`、backup、recovery。這代表：

- 換一台機器、重灌、`logout`，**歷史訊息永久解不開**。事件本身還在 server 上，但沒有鑰匙。
- §4.1 與 §1 那條「store 開不了就刪掉 `matrix/` 重新 `login`，store 只是裝置狀態」**把這件事寫成了正常操作**。
  「只是裝置狀態」對 `state.db` 成立，對 `crypto.db` 不成立 —— 它裡面是解開全部歷史的唯一鑰匙。
- 光有 recovery key 沒有用。recovery key 只是解開 SSSS 拿到 backup 的解密金鑰；**如果沒有人把房間金鑰上傳上去，備份是空的**。
  有意義的是房間金鑰本身（維護者 2026-09-09 在別的專案踩過同一個坑）。

唯一的緩衝是 `cache.db` 存的是**解密後的明文**（§2），所以本機歷史不會馬上消失。但它是快取（§1：可以整個丟掉），而且新裝置拿不到。
**快取不是備份。** 這一章講的是備份。

### 10.2 兩份備份，分工不同

| | server 端（標準 Matrix key backup） | 本地端（我們自己的加密金鑰池） |
|---|---|---|
| 存哪 | homeserver 的 `/room_keys`（`m.megolm_backup.v1.curve25519-aes-sha2`） | `accounts/<localpart>/room-keys/`（§10.4） |
| 防什麼 | 這台機器整個沒了（有 recovery key 之後才真的做得到，見 §10.3） | 意外：`crypto.db` 壞掉、`matrix/` 被刪掉重 `login`、server 端資料沒了 |
| 加密 | backup 的 curve25519 公鑰加密，私鑰在 crypto store（設了 recovery key 之後才進 SSSS） | 第五把子金鑰（`local.key` 導出，§4） |
| 寫入時機 | 上游的背景 task，靠 sync 觸發；**CLI 靠 `key-backup upload` 追平**（§10.6） | **同步寫**，命令結束前落地（§10.5） |
| 開關 | 預設開，可以在 conf 關掉（`SERVER_BACKUP=off`，CLI 規格 §10） | 預設開，可以關（`LOCAL_ROOM_KEYS=off`） |
| 生命週期 | 跟帳號走，`logout` 不動它 | **跟這台機器上的這個帳號走：`logout` 連它一起刪**（維護者 2026-09-09，§10.7） |
| 互通性 | 有：Element 之類的 client 用同一份 | 沒有：只有這個 client 讀得懂 |

兩份都是 best effort 的**副本**，權威永遠是 crypto store。任何一份讀壞就當作沒有（fail closed），不要拿壞掉的金鑰去覆蓋 store。

### 10.3 server 端：標準 Matrix key backup

fork server（wbfuwunel）已經有完整實作（`src/api/client/backup/`、`src/service/key_backups/`），不需要 server 改任何東西。

- `ClientBuilder` 上開 `EncryptionSettings { auto_enable_backups: true, auto_enable_cross_signing: true, backup_download_strategy: AfterDecryptionFailure }`。
  `auto_enable_backups` 的意思是：`login` 之後如果 server 上沒有 backup version 就建一個，並開始上傳。
- **`login` 就開始 backup，recovery key 延後**（維護者 2026-09-09 定）。要知道這個組合的實際含意：

  > `auto_enable_backups` 建 version 時，backup 的**私鑰存在本地 crypto store**，沒有進 SSSS。
  > 所以在使用者顯式產生 recovery key 之前，server 上那份備份**換一台機器也解不開** ——
  > 它防的是「本機 crypto.db 壞掉」，不是「換裝置」。

  這句話要出現在警告裡（警告的原文在 CLI 規格 §3.6），不能只說「你還沒設 recovery key」。
- **recovery key 不自動印**（維護者定）。顯式入口是 `key-backup recovery`（CLI 規格 §3.6）：走上游的 `recovery().enable()`，
  把 recovery key 印**一次**並說明拿不回來（只能 reset）。🚫 不寫進任何檔、不進 conf、不進 log。
- 關掉（`SERVER_BACKUP=off`）就是 `auto_enable_backups: false` 且不跑上傳；**已經在 server 上的 version 不動、不刪**
  （刪 server 端備份是不可逆的，要顯式命令，見 §10.8）。

### 10.4 本地端：一房一檔的加密金鑰池

放在**帳號層**（維護者 2026-09-09 定）：

```
accounts/<localpart>/room-keys/
  <room 識別>.keys        一房一檔，順序 append
```

- **第五把子金鑰**：`BLAKE3 derive_key("wbf-matrix-client room key backup v1", master)`（§4 的表加一列）。
- **`<room 識別>` 是 keyed hash，不是 room_id**：`BLAKE3 keyed_hash(第五把子金鑰, room_id)` 取前 32 hex。
  兩個理由：room_id 含 `!` 與 `:`，Windows 檔名不合法；而且目錄名不該洩漏這個帳號在哪些房（沒有金鑰就看不出是哪一間）。
- **檔案格式**（`WBFRK1`）：32 byte 檔頭 + 一串長度前綴的密文記錄。

  ```
  檔頭  magic "WBFRK1\0\0"(8) | version u16 = 1 | reserved(6) | file_id(16 隨機)
  記錄  u32 明文長度 | u24 序號 | XChaCha20-Poly1305 密文 + tag
        nonce = file_id 前 8 byte || 序號(u64 little endian)      序號從 0 起，每筆 +1
        aad   = 檔頭全部 32 byte                                  搬到別的檔就解不開
  ```

  明文是一筆金鑰的 JSON，欄位就是上游 `ExportedRoomKey` 的（`algorithm`、`room_id`、`sender_key`、`session_id`、
  `session_key`、`sender_claimed_keys`、`forwarding_curve25519_key_chain`）。**不自己發明欄位**：這樣 import 直接餵回上游。
- **只 append，不改寫、不刪單筆**（檔案內容層面；整個目錄什麼時候被清掉見 §10.7）。同一個 `session_id` 可以出現多次（後來拿到 index 更小、更完整的那把）；
  import 時全部餵給上游，由它比 `first_known_index` 決定留哪把。**我們不做取捨**，少一個會出錯的判斷。
- **壞掉怎麼辦**：某一筆解不開就停在那裡，把**前面成功的那些**照樣 import，並印一行說明第幾筆之後被截斷。
  順序 append 的檔案壞掉多半是尾巴（寫到一半斷電），前面的仍然有效；🚫 不因為尾巴壞掉就丟掉整個檔。

### 10.5 什麼時候寫

**同步寫，不靠背景 task。** 每個會拿到房間金鑰的命令（`read`、`recent`、`watch`、`sync`、`rooms`）在結束前：

1. 從 crypto store 匯出金鑰（上游 `encryption().export_room_keys()`，predicate 限定這次碰過的房間）。
2. 跟該房 `.keys` 檔已有的 `(session_id, first_known_index)` 集合比對 —— 開檔時掃一遍建這個集合（一房幾百到幾千筆，每筆約 200 byte，掃得動）。
3. 只 append 新的那幾筆，`fsync` 後才算數。

為什麼不用上游的背景 task：`BackupUploadingTask` 的 `Drop` 直接 `abort()`，CLI 命令 exit 時它可能一筆都還沒送出去。
本地這份是「不能漏」的那一份，所以走同步；server 那份允許落後（§10.6）。

⚠️ 效能：`export_room_keys` 會解密全部 session。房間多、金鑰多的時候這一步會變慢。
第一版接受（正確優先），量大了再換成訂閱上游的 room key stream 只寫增量 —— 換的時候格式不用動。

### 10.6 server 那份怎麼追平：`key-backup upload`

維護者 2026-09-09 定：**不在每個命令結束前等上傳**（那會讓每個命令慢），改成一個獨立命令手動跑。

- `key-backup upload` 走上游的 `backups().wait_for_steady_state()`，印上傳進度與結果，完成才 exit。
- 代價老實寫：**server 那份會長期落後**，落後多少由使用者跑不跑這個命令決定。
  這個代價可以接受，是因為本地那份是同步寫的 —— 最壞情況是「server 落後」，不是「金鑰消失」。
- `key-backup status` 印：本地池有幾把、crypto store 有幾把、server 上的 version 與 count、是否落後、有沒有 recovery key。
  🚫 不印任何金鑰內容。

### 10.7 誰刪 `room-keys/`：意外留門，離開就清乾淨

分界是**這次是意外還是有意的**（維護者 2026-09-09 定）：

| 情形 | `matrix/` | `room-keys/` | 怎麼把歷史找回來 |
|---|---|---|---|
| **意外**：store 壞掉、金鑰對不上，照 §4.1 的指示手動刪 `matrix/` 重新 `login` | 被刪 | **留著** | 重 `login` 後 `key-backup import` 餵回新的 crypto store |
| **有意**：`logout`／`account del <user>`（同一件事，CLI 規格 §3.1） | 被刪（Matrix logout 讓裝置失效，留著會擋下一次 `login`） | **一起刪** | 靠 server 那份加 recovery key（所以有閘門，見下） |
| **有意**：`account destroy <user>` | 被刪（它包含 `del`） | **一起刪** | 同上。它多做的是資料層：這個帳號在 `cache.db` 裡**獨有**的紀錄（別人也持有的不動） |

為什麼 `logout`（即 `account del`）連著刪（維護者 2026-09-09）：它在心智上是「我離開這台機器」，
留一個能解開全部歷史的檔案在磁碟上是驚嚇，而且跟「crypto store 一定會被刪」不一致。

#### `logout` 的閘門：不是正面認得救得回來，就不准走

⚠️ 直接刪有一個連鎖：在還沒有 recovery key 的預設狀態下，**server backup 的私鑰是存在 crypto store 裡的**（§10.3），
而 `logout` 正要刪掉 crypto store。所以「crypto store 沒了 ＋ room-keys 也刪了 ＋ server 那份解不開」＝ 歷史三份全滅。

所以 `logout` 前面加一道閘門，寫成**正面認得**的形式：

```
准走（照常 logout，room-keys/ 一起刪）  ⟸  server backup 開著 ＆ recovery().state() == Enabled
其他任何狀態                            ⟹  exit 1，要 --accept-history-loss 才走
```

「其他任何狀態」包含：沒有 recovery key、`SERVER_BACKUP=off`（使用者自己關掉的，那本地這份就是唯一一份）、
`RecoveryState` 是 `Unknown`／`Incomplete`、問不到 server。🚫 不寫成「沒有 recovery key 才擋」——
那樣新增一種狀態就默默放行；要壞就壞在「多擋一次」那一邊。

擋下來的時候印的訊息要直接給下一步（原文在 CLI 規格 §3.6）：先跑 `key-backup recovery` 產生 recovery key，
server 那份就變成換裝置也解得開的備份，再 `logout` 就沒有損失。

📎 副作用（好的）：這讓「recovery key 延後」不會被無限期延後 —— **延到第一次 `logout` 為止**。
本地池的定位因此也清楚了：它是**線上那份還沒真的可攜之前的中繼**，不是永久保險。

### 10.8 明確不做的 / 還開著的

- 🚫 不自己發明備份格式上傳到 fork server（走 pack 通道）：標準路徑已經可用，自訂等於放棄互通又要 server 改。
- 🚫 `key-backup` 不做「刪掉 server 上的 backup version」：不可逆，而且會讓其他裝置的備份一起失效。要刪去別的 client 刪。
- 還開著：本地金鑰池要不要配額或壓縮（append-only 會一直長）。一筆約 200 byte，一萬把也才 2 MB，第一版不管。
- 還開著：UI 那版怎麼呈現 recovery key（CLI 只印一次就算了，UI 要有「我存好了」的確認流程）。

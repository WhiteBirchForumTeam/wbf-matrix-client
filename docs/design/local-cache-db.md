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
| 有配額 | 每房最多 500 則、總量 200 MiB（維護者同意的傾向值），超過從最舊的刪 |
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
| 媒體內容 | 第一版不快取，manifest 夠用；要快取是另一份設計（密文可以直接落地，但配額與清理另議） |
| session 與 token | 第一版留在 session 檔（CLI 規格 §7）；§7 有討論要不要搬進來 |

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
- **導出**：兩個 store 各自一把 32 byte 子金鑰，`BLAKE3 derive_key(context, master)`，context 是固定字串
  `"wbf-matrix-client cache sqlcipher v1"` 與 `"wbf-matrix-client matrix-sdk store v1"`。子金鑰不落地，每次開啟導一次。
  換 context 字串就是換金鑰，所以 context 帶版本。
- **local password**：Argon2id 從密碼導 KEK，KEK 用 XChaCha20-Poly1305 包住主金鑰。改密碼只重包 48 byte，DB 不動。
  參數寫在檔裡，之後調高不用遷移。
- **模式是型別，不是空字串**：`enum KeyFile { Plain { master }, PasswordWrapped { kdf, nonce, wrapped } }`，
  讀檔時 `mode` 不認得就拒絕。「沒設密碼」是 `Plain`，不是「密碼等於空字串」——後者會讓空密碼靜默通過。
- **一個入口**：`Vault::open(dir, Unlock::NoPassword | Unlock::Password(secret))`。`Plain` 配 `NoPassword`、
  `PasswordWrapped` 配 `Password`，配錯就 `Err`，UI 與 CLI 都只能走這裡。CLI 的密碼來源同 `login`：`--password-file` 或終端不回顯。
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
  cache.db       我們的快取（SQLCipher）
  matrix/        SDK 的 store 目錄（crypto.db、state.db；SDK 自己命名）
  session.json   第一版仍在 CLI 規格 §7 的位置；見 §7
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
  room_id TEXT NOT NULL, event_id TEXT NOT NULL, origin_server_ts INTEGER NOT NULL,
  sender TEXT NOT NULL, type TEXT NOT NULL, msgtype TEXT,
  decrypted INTEGER,                 -- 1／0／NULL，同 CLI 規格 §3.4.1 的事件形狀
  undecryptable_reason TEXT,
  content_json TEXT NOT NULL,        -- 解密後的 content
  chunked_block_json TEXT,           -- msgtype 是 org.wbftw.wbfuwunel.file 時抽出來，給 files 查
  mxc TEXT,
  PRIMARY KEY (room_id, event_id));
CREATE INDEX events_by_time ON events (room_id, origin_server_ts);
CREATE INDEX events_files ON events (room_id, msgtype) WHERE chunked_block_json IS NOT NULL;

CREATE TABLE read_positions (room_id TEXT PRIMARY KEY, event_id TEXT NOT NULL, ts INTEGER NOT NULL);
```

- 解不開的加密事件也存（`decrypted = 0` 帶原因），之後拿到金鑰重解時覆蓋；不存等於每次都要重拉。
- 配額（§1）以 `origin_server_ts` 為序刪最舊；`rooms` 不受配額。

## 7. 還開著的

1. session 與 token 要不要搬進 `cache.db`（或 `local.key` 旁一個被同一把鎖住的檔）：好處是 local password 一把鎖住所有機密；
   壞處是 CLI 每個命令都要先開 vault（`Plain` 模式很快，`PasswordWrapped` 模式每個命令都要輸密碼，除非有 agent 之類的東西）。
   建議：第一版不搬；加 local password 那一版一起決定，因為那時才有「每個命令都要密碼」的體感。
2. 媒體內容快取：另一份設計。
3. 配額的數字：500 則／房、200 MiB 總量，用了再調。

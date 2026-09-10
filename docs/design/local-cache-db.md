# 本地資料庫設計：加密的暫存快取

> 狀態：2026-09-05 草案，維護者同意 §1–§4、§6 的提案；§5（與 matrix-sdk store 的關係）維護者要求寫細再議。
> **2026-09-06 起動手**（維護者定：第 3 步第一版之後下一隻專注這裡）。做到哪：
>
> | 段 | 狀態 |
> |---|---|
> | §4 主金鑰、兩種鎖法、三把子金鑰、`session.sealed`、CLI 的 unlock ticket | ✅ 第一個 PR：`wbf-sdk::vault`（`Vault::create`／`open`／`read_mode`／`set_unlock`、`seal_session`／`unseal_session`）、CLI 的 `unlock.rs`。實作與這裡的差異見 §4.1 |
> | §5.3 matrix-sdk store 用第二把子金鑰 | ✅ 同一個 PR：`SqliteStoreConfig::key`，不走 PBKDF2 |
> | §3、§6 `cache.db`（SQLCipher） | ✅ 第二個 PR：`wbf-sdk::cache`（feature `cache`）、CLI 的 `recent`／`--from-cache`／寫穿、多帳號混存（當時叫 `accounts`／`forget-account`；命令名 2026-09-09 改成 `account` 一族，CLI 規格 §3.1，實作還沒跟上）。§6 的 schema 就是實作的（v2）；建置需求見 §3 |
> | §10 房間金鑰備份（server 一份、本地一份、recovery key 獨立保管） | ✅ 2026-09-09 做了：`EncryptionSettings`、`key-backup status`／`upload`／`save`／`import`／`recovery`、`logout` 的兩關閘門、`room_keys` 模組、`recovery/` 資料夾與 `recovery list`／`show`。§10.4 的本地格式實作時改成全量快照（原因寫在那一節）；conf 的開關還沒做，目前寫死是開的 |
> | §11 路徑兩層都加密、§12 passphrase 是任意 bytes | ✅ §11 2026-09-09 做了（`account_dir`）；§12 還沒。跟第一個實作 PR（conf 加 `account` 一族）一起做。兩者都 breaking，而維護者 2026-09-09 明說不寫遷移（server 從未上線、client 從未被使用）：舊 data dir 直接刪 |
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
  第五把 `"wbf-matrix-client room key backup v1"`（§10.4 的本地房間金鑰池，加密與檔名 keyed hash 都用它）、
  第六把 `"wbf-matrix-client account directory v1"`（§11 的目錄名加密，`servers/` 與 `accounts/` 兩層共用這一把，靠 aad 分）。
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
  r/<b58>_<b58>                  recovery key（§10.8）：檔名是 `recovery-key@mxid` 加密後的樣子。
                                 🚫 logout 不碰它——那是它不放在帳號目錄底下的全部理由
  s/<b58>_<b58>/                 **server host 加密後的名字**（§11.2）：外面看不出這台機器連過哪家
    cache.db                     這個 server 上所有帳號共用（§6）
    media/                       媒體儲存池（§8），跟 cache.db 同層、同範圍
    a/
      <b58>_<b58>/               帳號目錄：**localpart 加密後的名字**（同 §11.2）
        session.sealed           第三把子金鑰封住的 session
        m/                       SDK 的 store（crypto.db、state.db），綁 device；logout 刪
        k/snapshot               本地房間金鑰備份（§10.4）；`account del`／`destroy` 連它一起刪

⚠️ 中間那幾段（`r`／`s`／`a`／`m`／`k`）只有一個字母，理由是 Windows 的 MAX_PATH（§11.4.1）——
兩段加密名字就吃掉 106 字元。
```

`<data dir>`：Windows `%APPDATA%`、macOS `~/Library/Application Support`、Linux `$XDG_DATA_HOME`（沒設就 `~/.local/share`）。

與 2026-09-05 草稿的差異（維護者 2026-09-07 定）：
- 草稿寫 `<data dir>/wbf/<server host>/<user localpart>/` 一帳號一套、`local.key` 在帳號底下。改成 **`local.key` 在頂層**：主金鑰的定位是「這台機器」（§4 第一條），passphrase 也是一台機器一個；一帳號一把會變成每個帳號各自問 passphrase。
- **`cache.db`（與媒體池）在 server 層，多帳號共用**：維護者要的是混存——user1 看得到 room1／2／3、user2 看得到 room1／2／4，不論誰登入都同步進同一個 DB，事件只存一份，可見性逐則記（§6）。共用範圍是同一個 server：`r_seq`／`g_seq` 是 fork server 發的，不同 homeserver 上序號不同。
- **兩層目錄名 2026-09-09 起都是加密的**（§11）：`servers/` 與 `accounts/` 底下都只看得到 `<b58>_<b58>`，要知道是哪家、是誰得用第六把子金鑰解。真正的 URL 與 mxid 仍然在 `session.sealed`。

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
s/<b58>_<b58>/media/<hash 前 2 hex>/<hash>     hash = 明文的 BLAKE3，32 位小寫 hex；原檔名、mimetype、mxc 只在 cache.db
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

在這一章之前，房間金鑰（Megolm inbound session）**只活在一個地方**：帳號目錄底下的 `m/crypto.db`（那時還叫 `matrix/`）。
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
| 存哪 | homeserver 的 `/room_keys`（`m.megolm_backup.v1.curve25519-aes-sha2`） | 帳號目錄的 `k/`（§10.4） |
| 防什麼 | 這台機器整個沒了（有 recovery key 之後才真的做得到，見 §10.3） | 意外：`crypto.db` 壞掉、`matrix/` 被刪掉重 `login`、server 端資料沒了 |
| 加密 | backup 的 curve25519 公鑰加密，私鑰在 crypto store（設了 recovery key 之後才進 SSSS） | 第五把子金鑰（`local.key` 導出，§4） |
| 寫入時機 | 上游的背景 task，靠 sync 觸發；**CLI 靠 `key-backup upload` 追平**（§10.6） | **命令觸發**：`key-backup save`，`upload` 時順手一起（§10.5） |
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
  把 recovery key 印**一次**並說明拿不回來（只能 reset）。同一個命令會把它封進 `<data dir>/r/`（§10.8）——
  🚫 但仍然不進 conf、不進 log，而且那份保管**不算「使用者擁有」**（同一台機器，一起被拿走就一起沒了）。
- 關掉（`SERVER_BACKUP=off`）就是 `auto_enable_backups: false` 且不跑上傳；**已經在 server 上的 version 不動、不刪**
  （刪 server 端備份是不可逆的，要顯式命令，見 §10.9）。

### 10.4 本地端：一個全量快照檔（2026-09-09 實作時改的）

放在**帳號層**（維護者 2026-09-09 定）：

```
a/<b58>_<b58>/k/
  snapshot        上游 export_room_keys 倒出來的全量加密快照
  snapshot.tmp    寫入時的暫存檔，寫完 rename 成 snapshot
```

> ⚠️ **這一節 2026-09-09 實作時改過**。原本定的是「一房一檔、逐把 append 的 `WBFRK1` 格式，
> 每個命令結束前比對後只寫新的那幾筆」。做不到，因為**上游只給全量匯出**：
> `Encryption::export_room_keys(path, passphrase, predicate)` 直接把金鑰寫成一個加密檔，
> 而拿逐把金鑰要 `Client::olm_machine()`，那是 `pub(crate)`（只有 `olm_machine_for_testing()` 露出來，
> 名字就是契約，🚫 不碰）。
>
> 改成全量快照之後**反而簡單得多**，而且原本那套 append 格式的理由都消失了：
>
> | 原本要解決的 | 全量快照為什麼不需要 |
> |---|---|
> | 去重（同一把 session 存很多次） | 每次覆蓋整份，本來就沒有重複 |
> | 檔案愈積愈長 | 快照大小 ＝ 金鑰總量，不隨備份次數長 |
> | 自訂的 `WBFRK1` 格式與它的 nonce／序號 | 不用了，格式是上游的 |
> | 尾巴壞掉要截斷 | 先寫 `.tmp` 再 rename，要嘛舊的完整、要嘛新的完整 |
>
> 代價只有一個：拿不到「只寫增量」，每次要跑一輪 PBKDF2 500,000（約半秒）。所以它是**命令觸發**的（§10.5）。

- **passphrase 是 vault 第五把子金鑰的 base64**（`BLAKE3 derive_key("wbf-matrix-client room key backup v1", master)`），
  不是使用者打的字——所以「passphrase 太弱被暴力破」在這裡不存在。
- **🚫 不再包一層我們自己的 AEAD**：上游的匯出格式本身就是加密的，而它的 passphrase 已經是 vault 保護的，
  多包一層不增加任何安全性，只多一個要維護的格式（全域 CLAUDE.md A2）。
- **金鑰不進我們的記憶體**：上游直接寫檔、直接讀檔，我們只給路徑與 passphrase。
- 一房一檔也做得到（`predicate` 可以按房過濾），但那會變成「房間數 × 半秒」。**維護者當初說的
  「以 server & 房間為識別碼」在這裡沒有照做**，理由就是這個；要改回一房一檔的話只是把 predicate
  換成迴圈，格式不用動。

### 10.5 什麼時候寫

**命令觸發，不是每個命令都做**——每次一輪 PBKDF2 500,000（約半秒），掛在 `read` 這種命令上太貴。

| 時機 | 做什麼 |
|---|---|
| `key-backup save` | 手動存一份 |
| `key-backup upload` | 推完 server 那份**順手也存本地這份**：兩份備份的用途不同（§10.2），但沒有理由讓使用者記得跑兩個命令 |
| `logout` 的閘門擋下來時 | 訊息告訴他有 `key-backup save` 這條路（也老實說 logout 會連它一起刪） |

⚠️ 所以本地這份**不是「一定不會漏」**：兩次 `save` 之間拿到的金鑰，只在 crypto store 與（有跑 upload 的話）
server 那份裡。原本的設計把它寫成「同步寫、不能漏」，那是建立在「拿得到逐把金鑰」的假設上，而那個假設是錯的。
真正的保險仍然是 §10.3 的 server 端 backup 加 recovery key。

### 10.6 server 那份怎麼追平：`key-backup upload`

維護者 2026-09-09 定：**不在每個命令結束前等上傳**（那會讓每個命令慢），改成一個獨立命令手動跑。

- `key-backup upload` 走上游的 `backups().wait_for_steady_state()`，印上傳進度與結果，完成才 exit。
- 代價老實寫：**server 那份會長期落後**，落後多少由使用者跑不跑這個命令決定。
  ⚠️ 🚫 這個代價**不是**靠本地那份補起來的——本地那份同樣是命令觸發的（§10.5），兩者會一起落後。
  真正的保險是 §10.3 的 server 端 backup 加 recovery key。
- `key-backup status` 印這幾個欄位（🚫 不印任何金鑰內容）：
  `server_backup_exists`、`uploading_locally`、`recovery_enabled`、`recovery_state`、
  `local_snapshot`、`local_snapshot_bytes`、`local_snapshot_saved_at`。
  ⚠️ 逐把的計數印不出來：上游只給整包匯出，沒有「crypto store 裡有幾把」這種問法（§10.4）。

### 10.7 誰刪 `k/`：意外留門，離開就清乾淨

分界是**這次是意外還是有意的**（維護者 2026-09-09 定）：

| 情形 | `m/` | `k/` | 怎麼把歷史找回來 |
|---|---|---|---|
| **意外**：store 壞掉、金鑰對不上，照 §4.1 的指示手動刪 `matrix/` 重新 `login` | 被刪 | **留著** | 重 `login` 後 `key-backup import` 把快照餵回新的 crypto store |
| **有意**：`logout`／`account del <user>`（同一件事，CLI 規格 §3.1） | 被刪（Matrix logout 讓裝置失效，留著會擋下一次 `login`） | **一起刪** | 靠 server 那份加 recovery key（所以有閘門，見下） |
| **有意**：`account destroy <user>` | 被刪（它包含 `del`） | **一起刪** | 同上。它多做的是資料層：這個帳號在 `cache.db` 裡**獨有**的紀錄（別人也持有的不動） |

為什麼 `logout`（即 `account del`）連著刪（維護者 2026-09-09）：它在心智上是「我離開這台機器」，
留一個能解開全部歷史的檔案在磁碟上是驚嚇，而且跟「crypto store 一定會被刪」不一致。

#### `logout` 的閘門：不是正面認得救得回來，就不准走

⚠️ 直接刪有一個連鎖：在還沒有 recovery key 的預設狀態下，**server backup 的私鑰是存在 crypto store 裡的**（§10.3），
而 `logout` 正要刪掉 crypto store。所以「crypto store 沒了 ＋ room-keys 也刪了 ＋ server 那份解不開」＝ 歷史三份全滅。

所以 `logout` 前面加一道閘門，寫成**正面認得**的形式：

```
准走（照常 logout，k/ 一起刪）  ⟸  ① server 上有 backup ＆ recovery().state() == Enabled
                                          ＆ ② 這台機器保管著這個帳號的 recovery key（§10.8）
其他任何狀態                            ⟹  exit 1，要 --accept-history-loss 才走
```

⚠️ **兩關都要過，而且第二關才是真的**（維護者 2026-09-09）：`RecoveryState::Enabled` 的上游定義是
「secret storage is set up and we have all the secrets locally」——它說得出 SSSS 設好了，
**說不出那串 recovery key 在誰手上**。跑過 `key-backup recovery`、印出來、沒抄就關掉終端的人，
第一關照樣過。第二關看的是 `<data dir>/r/` 有沒有封著這個帳號的 key，
而那個目錄 `logout` 不碰——所以刪完 `m/` 與 `k/` 之後它還在，歷史真的救得回來。

🚫 **不問使用者手打 recovery key**（維護者 2026-09-09 定）：既然我們自己就保管著，問他等於刁難。

「其他任何狀態」包含：沒有 recovery key、`SERVER_BACKUP=off`（使用者自己關掉的，那本地這份就是唯一一份）、
`RecoveryState` 是 `Unknown`／`Incomplete`、問不到 server。🚫 不寫成「沒有 recovery key 才擋」——
那樣新增一種狀態就默默放行；要壞就壞在「多擋一次」那一邊。

擋下來的時候印的訊息要直接給下一步（原文在 CLI 規格 §3.6）：先跑 `key-backup recovery` 產生 recovery key，
server 那份就變成換裝置也解得開的備份，再 `logout` 就沒有損失。

📎 副作用（好的）：這讓「recovery key 延後」不會被無限期延後 —— **延到第一次 `logout` 為止**。
本地池的定位因此也清楚了：它是**線上那份還沒真的可攜之前的中繼**，不是永久保險。

### 10.8 recovery key 存哪：獨立的資料夾，`logout` 不碰（維護者 2026-09-09 定）

```
<data dir>/r/<b58>_<b58>            檔名是 `recovery-key@bob:matrix.org` 加密後的樣子
```

**為什麼不放在帳號目錄底下**：`logout`／`account del` 要把帳號目錄整個清乾淨
（`session.sealed`、`m/`、`k/`），而 recovery key 正好是**清完之後唯一回得去的路**。
放在一起就會一起被刪，那等於 server 上的備份也沒了——閘門（§10.7）就變成在檢查一個馬上要被自己刪掉的東西。

| 誰 | 對 recovery key 做什麼 |
|---|---|
| `key-backup recovery` | 產生、印出來一次、**封進這裡** |
| `logout`／`account del` | **不碰**——這是它跟帳號目錄分開放的全部理由 |
| `account destroy` | **一起摧毀**（那個命令的語意就是「什麼都不留」）。⚠️ 之後 server 上那份備份永遠解不開 |
| `recovery list` | 列出這台機器保管著誰的（只解**檔名**，🚫 不解內容） |
| `recovery show <user>` | 印出某一個（會印秘密，跟 `key-backup recovery` 一樣） |
| `key-backup restore` | 拿它**恢復這台裝置**——重新 `login` 之後必跑，見下 |

- **檔名跟其他兩層一樣加密**（`DirScope::Recovery`，第六把子金鑰）：外面看不出這台機器保管著誰的 key。
- **內容用第三把子金鑰封**（跟 `session.sealed` 同一把，AAD 不同所以密文換不過去）。
- 目錄 0700。
- ⚠️ **重新 `login` 之後要跑 `key-backup restore`**（2026-09-09 對真 server 驗證時發現）：
  `logout` 之後再 `login` 是**新裝置**，它的 crypto store 沒有 SSSS 的 secrets，
  `RecoveryState` 會是 `Incomplete`、server 上那份備份解不開。保管著 recovery key 不會自動生效，
  要有人拿它去 `recovery().recover()`。
- ⚠️ 老實說它的邊界：這是**方便性的保管**，不是「使用者擁有」的證明——它跟 crypto store 在同一台
  機器上，整台被拿走就一起沒了。真正換裝置時仍然要使用者手上有那串字，所以 `key-backup recovery`
  印出來時還是會叫他寫下來。

### 10.9 明確不做的 / 還開著的

- 🚫 不自己發明備份格式上傳到 fork server（走 pack 通道）：標準路徑已經可用，自訂等於放棄互通又要 server 改。
- 🚫 `key-backup` 不做「刪掉 server 上的 backup version」：不可逆，而且會讓其他裝置的備份一起失效。要刪去別的 client 刪。
- 還開著：本地金鑰池要不要配額或壓縮（append-only 會一直長）。一筆約 200 byte，一萬把也才 2 MB，第一版不管。
- 還開著：UI 那版怎麼呈現 recovery key（CLI 只印一次就算了，UI 要有「我存好了」的確認流程）。

## 11. 路徑兩層都加密：外面連「哪家 server、誰的帳號」都看不到（維護者 2026-09-09 定）

### 11.1 為什麼

`servers/<server host>/accounts/<localpart>/` **兩層都是明文**。DB 加密了、session 封起來了、媒體進了加密池，
結果路徑把「這台機器上有 alice 跟 bob，都在 matrix.org」直接寫在檔案總管裡。
維護者 2026-09-09 定：**server host 與 localpart 都要加密**，外部只看得到 `base58/base58`。

⚠️ 連帶的兩個地方，不改就等於沒加密：

| 地方 | 現在 | 要變成 |
|---|---|---|
| `current`（CLI 記「預設用誰」） | 一行 `<server host>/<localpart>` **明文** | 兩層都是加密後的目錄名；要知道那是誰得解密 |
| `account status` 列帳號 | 掃目錄就有，**不開 vault、不問 passphrase** | 必須解鎖才列得出來（§11.6）。這是這個決定的代價，寫在這裡不藏 |

### 11.2 名字的樣子：`<base58 nonce>_<base58 密文>`

Base58 的字母表**沒有底線**，所以 `_` 可以當分隔符，兩段各自編碼，解析不必靠「前 24 byte 是 nonce」這種長度常數
（維護者 2026-09-09 出的主意）。

**第六把子金鑰**一把就夠（`BLAKE3 derive_key("wbf-matrix-client account directory v1", master)`，§4）：
兩層用不同的 aad 與不同的 nonce context 分開，🚫 不需要第七把。

```
第一層  s/<B58(nonce_s)>_<B58(ct_s)>/
  nonce_s = BLAKE3 keyed_hash(key, "wbf server-dir-nonce v1" ‖ host) 前 12 byte
  ct_s    = ChaCha20-Poly1305(key, nonce_s, host,      aad = "wbf-matrix-client server dir v1")

第二層  a/<B58(nonce_a)>_<B58(ct_a)>/
  nonce_a = BLAKE3 keyed_hash(key, "wbf account-dir-nonce v1" ‖ host ‖ 0x00 ‖ localpart) 前 12 byte
  ct_a    = ChaCha20-Poly1305(key, nonce_a, localpart, aad = "wbf-matrix-client account dir v1" ‖ host)
```

⚠️ **nonce 是 12 byte、演算法是 ChaCha20-Poly1305 而不是 XChaCha20**（維護者 2026-09-09 定，
起因是實跑撞到 Windows 的 MAX_PATH）：

| | 24 byte nonce（XChaCha） | 12 byte nonce |
|---|---|---|
| nonce 那段 base58 | 33 字元 | **17 字元** |
| 兩層合計省 | — | **32 字元** |

12 byte 夠不夠：nonce 是 `BLAKE3 keyed_hash(key, …‖明文)` 的前 12 byte，碰撞要兩個**不同明文**的
hash 前 96 bit 相同——生日界是 2^48 個明文，而這裡的明文是「這台機器的 server host 與 localpart」，
數量是個位數。🚫 這個推導**只在明文數量極少時成立**，別把同一套搬去命名數以萬計的東西。

📎 中間那幾段目錄名也縮到一個字母（`s`／`a`／`m`／`k`／`r`），再省 18 字元。可讀性本來就沒有——
它們夾在兩段密文之間。

- **nonce 由明文確定性導出，而且照樣寫進名字裡**。兩件事都要，理由不同：
  - 寫進去：解密時要先有 nonce，而 nonce 是從還沒解出來的明文導出的 —— 不寫就永遠解不開。
  - 確定性：`login` 能**直接算出**兩層路徑去定位，不必先掃描；同一個帳號永遠是同一個目錄，重登不會長出第二個。
- 🚫 **不可以用固定 nonce**。同一把金鑰配同一個 nonce 加密不同的明文，XChaCha20 是 stream cipher，
  兩份密文 XOR 就洩漏明文 XOR。nonce 從明文導出正好保證「不同明文 → 不同 nonce」。
- **第二層的 aad 與 nonce 都綁明文 host**：帳號目錄從一個 server 目錄搬到另一個底下就解不開（fail closed），
  而且同一個 localpart 在兩個 server 上目錄名不同。第一層的 aad 是固定字串（它上面沒有東西可綁）。
- nonce context 之間加 `0x00` 分隔：`host="a" localpart="bc"` 與 `host="ab" localpart="c"` 不會導出同一個 nonce。

### 11.3 加密之前先正規化，否則同一個 server 會長出兩個目錄

加密是逐 byte 的：`matrix.org` 與 `MATRIX.ORG` 進去就是兩個不同的目錄。
原本 `server_key()` 做的檔名安全化現在改當**正規化**，在加密之前跑：

- **host**：取 URL 的 host，小寫；非預設 port 才帶上（`localhost:6167`、`matrix.org`）。
  🚫 不再過濾 `[A-Za-z0-9._-]` —— 那是為了當檔名才做的，現在檔名是 Base58，過濾只會讓不同的 host 撞在一起。
- **localpart**：以 **server 回的 `user_id`** 為權威（現有的 `login` 已經這樣做：目錄名對不上就搬過去）。
  🚫 不拿使用者打的 `--user` 直接加密。

### 11.4 Windows 的大小寫陷阱與長度

⚠️ Base58 **區分大小寫**，Windows 的檔名**不區分**。所以「兩個名字只差大小寫」在 Windows 上是同一個目錄。
密文有 40 byte 以上的熵，實際碰不到，但 🚫 不靠「不可能碰撞」寫程式（全域 CLAUDE.md A5）：

- **建目錄前先檢查**：目標名字已經存在時，把它解密出來比對 —— 是同一個 host／localpart 才用，不是就報錯，
  🚫 不覆蓋、🚫 不加後綴自己找一個空位。
- 每一段名字上限 **200 字元**（Windows 單一路徑元件是 255）。Base58 大約是 byte 數的 1.37 倍，
  nonce 那段固定 17 字元，所以密文那段大約 130 byte 以上才會踩到 —— Matrix 的 localpart 上限是 255 byte，
  踩得到，要有這個檢查。超過就報錯，🚫 不截斷（截斷等於不可逆）。

### 11.5 讀回來：掃兩層，建記憶體裡的對照

維護者要的流程：**起始時掃一次雙層結構、嘗試解密，解失敗的不加入清單，成功的就把 Base58 映射成明文帶進路徑。**

```
s/ 底下每個目錄名
  → 沒有 `_`、任一段 Base58 解碼失敗、AEAD 解不開  → 跳過，不加入清單
  → 解得開                                        → host，再往下掃它的 a/
       a/ 底下每個目錄名
         → 解不開（aad 綁的是這一層的 host）        → 跳過
         → 解得開                                  → localpart，組回 @localpart:<server_name>
```

`r/`（recovery key，§10.8）跟著一起掃：檔名的明文是 `recovery-key@mxid`，同一套規則。

- 對照表**只在記憶體裡**，一個命令的生命週期。🚫 不落地成明文索引檔 —— 那等於把剛加密的東西再寫一次明文。
- **解不開的不猜、不刪、不報錯**：可能是另一把 `local.key` 建的（換過 data dir），也可能是舊版留下的。
  fail closed 是「當它不存在」。整個 `s/` 都解不開時印一行提示（§11.7）。

#### 11.5.1 兩條路：算得出來的，與只能比對的（維護者 2026-09-10 定）

| 手上有什麼 | 走哪條 |
|---|---|
| **確定就是這個明文**（剛 `login`、`current` 解出來的） | §11.2 的確定性加密，直接算，不碰磁碟 |
| **使用者打進來的字串**（`account del @BOB:matrix.org`） | 當場掃一次建 map，再從 map 比對 |

第二條路**不能用算的**：`@BOB:matrix.org` 加密出來的名字跟 `@bob:matrix.org` 完全不同，
而「大小寫不敏感」沒有算式 —— 只能拿現場有什麼來比。所以：

```
account destroy @BOB:matrix.org
  ① 刷新：掃 s/*/a/* 與 r/，解密每一段名字        ← 當場做，不吃上一次的結果
  ② 建 map：明文 → 磁碟上那個（加密的）名字
  ③ 比對：先精確，再大小寫不敏感
       ⚠️ 大小寫那一輪對到兩個以上 → 當作沒有（fail closed）
  ④ 帳號目錄與 recovery key **都從這同一份 map 來**
```

⚠️ **④ 是重點**：掃兩次就有兩個不同時刻的答案，而這個命令要用它們決定刪哪個目錄。

⚠️ map 是**快照，不是快取**：🚫 不存成長命的全域狀態 —— 存起來的那一份不會知道
中間有東西被刪掉。會刪檔的命令一律當場刷新。

📎 這條規則是實作 §10.8 時補的：`account destroy` 原本拿使用者打的那串去算 recovery key
的檔名，但封存時用的是 server 的權威 mxid，大小寫差一個字就刪不到 —— 而那個命令的語意
是「什麼都不留」（PR #19 審查）。

### 11.6 代價：`account status` 現在要解鎖

原本 `account status`（舊名 `accounts`）明說「掃目錄，不開 vault、不問 passphrase」——
兩層都加密之後做不到了：不解密就不知道有哪些 server、哪些帳號。

- `passphrase` 模式下 `account status` 會問 passphrase（或吃 unlock ticket）。
- 🚫 不做「列出 Base58 但不解密」的半套輸出：那對使用者沒有意義，只會讓人以為壞了。
- 本來就要開帳號目錄的命令沒有變差（它們早就要解鎖才讀得到 `session.sealed`）。

### 11.7 舊的 data dir：砍掉重來，不寫遷移

維護者 2026-09-09：**server 從未上線、client 從未被使用，breaking 就 breaking，當前環境直接砍掉沒問題。**
所以這裡 🚫 不寫遷移、🚫 不留 `layout` 之類的版本標記檔 —— 少一個檔、少一段只跑一次的程式。

- 明文佈局的舊目錄在 §11.5 的掃描裡本來就解不開，會被跳過（fail closed），不會被誤認成別人的帳號。
- `servers/` 底下有東西但**一個都解不開**時，印一行提示就好：

  ```
  warning: no account directory here could be decrypted with this local.key; if this data dir was made
           by an older build, delete it and run `login` again
  ```

- 📎 順帶：`servers/` 兩層都改了之後，CLI 規格 §7 那條「PR #11 的單一目錄佈局要報錯」也失去意義了
  （那個佈局同樣解不開、同樣被跳過）。實作時可以一併拿掉那段檢查。

#### 11.4.1 ⚠️ 真正咬人的不是單段長度，是**整條路徑**（2026-09-09 實測）

Windows 的 `MAX_PATH` 是 **260**，而加密把兩段目錄名從 19 字元（`localhost_6167` ＋ `alice`）
撐到 106。實測 `matrix-sdk-event-cache.sqlite3` 的完整路徑：

| | 加密名字合計 | 最長路徑（data dir 39 字元） |
|---|---|---|
| 24 byte nonce ＋ 長目錄名 | 138 | **230**（餘裕 20，data dir 稍深就爆） |
| **12 byte nonce ＋ `s`／`a`／`m`** | **106** | **184**（餘裕 76） |

⚠️ 第一次驗證時用 130 字元的 data dir **直接失敗**，而且錯誤訊息把它誤報成
「it was made with another key file」——害人去刪一個其實沒問題的目錄。
現在 `build_client` 會先看路徑長度再決定怎麼報（`backend/matrix_sdk.rs`）。

📎 這條的教訓寫下來：**目錄名加密的成本不在 CPU，在路徑預算**。之後要再加一層加密目錄之前，
先算一次最深的那條路徑。

### 11.8 還開著

- 目錄的 mtime、檔案大小、帳號數量仍會洩漏活躍程度：外面數得出這台機器上有幾個 server、幾個帳號，
  只是不知道是誰、在哪家。這是檔案系統層面的事，這一版不處理。

## 12. passphrase 是任意 bytes，不是字串（維護者 2026-09-09 定）

> 維護者的原話：passphrase 可以是任何字元、純二進位文檔，**不要擅自翻譯成純 ASCII**；
> 可以是 UTF-8 的中文，可以是一個 mp3，可以是任何東西。最常見的用法才是一串字。

### 12.1 現在是什麼樣（要改）

`read_password_file` 用 `std::fs::read_to_string`（**要求整檔是合法 UTF-8**）再 `strip_suffix('\n')`
（**吃掉結尾的換行**，`\r\n` 也吃）。所以現在：mp3 直接讀失敗，而結尾多一個 byte 的檔會導出不同的 KEK。

### 12.2 要變成什麼樣

- `Unlock::Passphrase` 的內容是 `Zeroizing<Vec<u8>>`，不是 `String`。`derive_kek` 吃 `&[u8]`。
- `--passphrase-file`：**整檔原始 bytes**，🚫 不去尾換行、🚫 不驗 UTF-8、🚫 不 trim 空白。
  ⚠️ 這代表 `echo hunter2 > pw` 產生的檔（結尾有 `\n`）跟 `printf hunter2 > pw` 是**兩個不同的 passphrase**。
  這是刻意的：檔案就是檔案，我們不替使用者猜哪個 byte 不算數。
- 終端輸入：讀到的那一行的 UTF-8 bytes（不含結尾換行）。終端只打得出字，這是它的天然子集。
- 空的判斷改成 **`bytes.is_empty()`**（「沒設 passphrase」仍然是 `Plain` 模式，不是空 passphrase，§4）。

### 12.3 沒有相容包袱：`v: 1` 直接就是新的定義

改讀法會改變 KEK，舊的 `local.key` 會突然解不開。維護者 2026-09-09：
**server 從未上線、client 從未被使用，breaking 就 breaking，當前環境直接砍掉沒問題。**

- 所以 🚫 **不寫兩套讀法**、🚫 不升版本號、🚫 不留 v1 相容分支：`v: 1` 的定義就改成「整檔原始 bytes」。
  一個問題一份實作，少一個永遠不會有人再讀第二次的分支。
- 現有的 `local.key` 解不開時，錯誤訊息就照現在的：叫人刪掉 data dir 重新 `login`（§11.7 同一個處置）。
- 📎 這條的前提是「還沒有使用者」。**之後有了就不再適用** —— 那時候要改 KDF 的輸入就得升版本號、留讀舊檔的路徑。

### 12.4 `--password-file` **不跟著改**

⚠️ passphrase 與 password 是兩種東西（§4 開頭的用字），這裡是它們處理方式分岔的地方：

| | 給誰 | 怎麼讀 |
|---|---|---|
| **passphrase**（`--passphrase-file`） | 只餵給本機的 Argon2id，永遠不出這台機器 | **原始 bytes**，一個都不動（§12.2） |
| **password**（`--password-file`） | 送給 homeserver 的 `/login`，Matrix 規定它是 JSON 字串 | 維持現狀：當 UTF-8 讀，去掉結尾一個換行 |

🚫 不要「統一」這兩個 —— password 是 mp3 的話根本送不出去（JSON 塞不進任意 bytes），
而 passphrase 去尾換行會讓「檔案內容」與「實際用的東西」對不上。
它們長得像，但一個是本機的鑰匙、一個是要上線的憑證。

# 本地的金鑰：主金鑰與子金鑰、路徑加密、passphrase

> 這份講本機所有加密的根：`local.key` 裡的主金鑰與它導出的子金鑰（§1）、資料目錄裡 server 與帳號名字的加密（§2）、passphrase 怎麼讀（§3）。
> 各檔放在哪見 /docs/design/storage/local-cache-db.md §4.6；`wbf-sdk::vault` 與 `wbf-sdk::account_dir` 就是照這裡寫的。

## 1. 金鑰：一把主金鑰，兩種鎖法，型別化

> 用字（維護者 2026-09-07 定）：**passphrase** 是解 `local.key` 的那句話；**password** 一律指 Matrix 帳號密碼。兩個字不混用。

```
local.key（0600）
  ├─ Plain      : { "v": 1, "mode": "plain", "master": "<base64 32 byte>" }
  └─ Passphrase : { "v": 1, "mode": "passphrase",
                    "kdf": { "name": "argon2id", "m_kib": 65536, "t": 3, "p": 1, "salt": "<base64 16>" },
                    "nonce": "<base64 24>", "wrapped": "<base64 48>" }
                  // wrapped = XChaCha20-Poly1305(KEK, master, aad = "wbf-matrix-client local.key v1")
```

- **主金鑰** 32 byte，CSPRNG，一台機器一把。
- **導出**：子金鑰都是 32 byte，`BLAKE3 derive_key(context, master)`，context 是固定字串。子金鑰不落地，每次要用時導一次。
  換 context 字串就是換金鑰，所以 context 帶版本。

  | # | context | 用途 |
  |---|---|---|
  | 1 | `wbf-matrix-client cache sqlcipher v1` | `cache.db` 的 SQLCipher raw key（/docs/design/storage/local-cache-db.md §3） |
  | 2 | `wbf-matrix-client matrix-sdk store v1` | matrix-sdk store 的 `open_with_key`（/docs/design/storage/local-cache-db.md §4.3） |
  | 3 | `wbf-matrix-client session v1` | XChaCha20-Poly1305 封 `session.sealed`（server、user_id、device_id、access_token）與 `r/` 裡的 recovery key，兩者靠 aad 分 |
  | 4 | `wbf-matrix-client media store v1` | 媒體池（/docs/design/media/media-pool.md §8） |
  | 5 | `wbf-matrix-client room key backup v1` | 本地房間金鑰快照 `k/snapshot` 的 passphrase（base64 後餵給上游的匯出，/docs/design/keys/room-key-backup.md §4） |
  | 6 | `wbf-matrix-client account directory v1` | 目錄名加密（§2），`s/`、`a/`、`r/` 共用這一把，靠 aad 分 |

  session 與 token **不進 DB**，但跟 DB 同一把鎖（維護者 2026-09-05 定）。
- **passphrase**：Argon2id 從 passphrase 導 KEK，KEK 用 XChaCha20-Poly1305 包住主金鑰。改 passphrase 只重包 48 byte，DB 不動。
  參數寫在檔裡，之後調高不用遷移。
- **模式是型別，不是空字串**：`enum KeyFile { Plain { master }, Passphrase { kdf, nonce, wrapped } }`（`mode` 是 serde 的 tag），
  讀檔時 `mode` 不認得就拒絕。「沒設 passphrase」是 `Plain`，不是「passphrase 等於空字串」——後者會讓空 passphrase 靜默通過。
- **一個入口**：`Vault::open(dir, Unlock::NoPassphrase | Unlock::Passphrase(bytes))`。`Plain` 配 `NoPassphrase`、
  `Passphrase` 配 `Passphrase`，配錯就 `Err`，UI 與 CLI 都只能走這裡。CLI 的 passphrase 來源同 `login` 的 password：檔案參數或終端不回顯。
- **解鎖後金鑰放哪**（維護者 2026-09-05 交給實作定，照一般開發工具的做法）：

  | | 做法 |
  |---|---|
  | daemon | 解鎖一次（前端經 RPC 給 passphrase），主金鑰只在 daemon 的記憶體；daemon 結束就沒了 |
  | CLI（只在開發與 debug 用） | **每次都重新解鎖**，主金鑰只在那個程序的記憶體裡、命令結束就沒了 |

  CLI passphrase 的來源與 `login` 的 password 同一套：`--passphrase-file <檔>` 或終端不回顯；不接受命令列明文與環境變數。優先順序：檔案參數 → 問終端。

  🚫 **不把解鎖狀態寫到磁碟**（維護者 2026-09-13）：不做仿 `sudo` 的 ticket（明文主金鑰落地一段時間，省掉每個命令再問 passphrase）。
  ⭐ 理由是那個痛點沒有了：daemon 常駐、解鎖一次（/docs/design/overview/architecture-v2.md §1），而單發命令在 daemon
  起著的時候本來就不准碰資料庫（/docs/design/overview/architecture-v2.md §0.2），所以 CLI 只剩 debug／test 用、一次一個 ——
  省那幾次打字換不到「明文主金鑰落地」。

### 1.1 實作細節

- 包主金鑰、封 session、封 recovery key 各帶固定的 AEAD 附加資料（`wbf-matrix-client local.key v1`、`wbf-matrix-client session.sealed v1`、`wbf-matrix-client recovery.sealed v1`）：把 A 檔的密文搬到 B 檔解不開。
- `Vault::read_mode(dir)`：只看鎖法不解。CLI 用它決定要不要問 passphrase，🚫 不靠 `open` 失敗的錯誤字串判斷（那是 parse Display 的老毛病，matrix-sdk 那次踩過）。
- `Vault::set_unlock(&Unlock)` 一個函數涵蓋設 passphrase、改 passphrase、拿掉 passphrase：只重寫 `local.key`，主金鑰不變，所以 `session.sealed` 與 SDK store 不動。空的 passphrase 在這裡被拒。
- `Vault::from_master(dir, master, mode)` 只給 `set_passphrase` 重包 `local.key` 用（同一個目錄、同一把主金鑰）；⚠️ 它不驗證那把金鑰是不是這個目錄的，所以🚫 除此之外不要拿它做別的事。
- 寫 `local.key`／`session.sealed` 都先寫暫存檔再 rename（`vault::write_private`）：寫到一半斷電不留半個檔。
- 既有的 matrix-sdk store 用別把金鑰開會失敗：訊息叫人刪那個 store 目錄（`m/`）重新 `login`，不遷移（/docs/design/storage/local-cache-db.md §1 的政策，維護者 2026-09-09 決定不改）。
  ⚠️ 這條站得住的前提是 /docs/design/keys/room-key-backup.md 的本地快照：`m/` 裡的 crypto store 裝著解開全部歷史的房間金鑰，手動刪 `m/` 時 `k/snapshot` 留著、`key-backup import` 讀得回來。

- **威脅模型**（老實寫）：

| 防 | 不防 |
|---|---|
| 把 DB 檔拷走的人（沒有 `local.key` 解不開） | 能登入這台機器、讀得到 `local.key` 的人（`Plain` 模式） |
| 加 passphrase 後：連 `local.key` 一起拷走也解不開（要猜 passphrase，Argon2id 拖慢） | 跑著的程序記憶體裡的主金鑰；鍵盤側錄 |

## 2. 路徑兩層都加密：外面連「哪家 server、誰的帳號」都看不到（維護者 2026-09-09 定）

### 2.1 為什麼

DB 加密了、session 封起來了、媒體進了加密池；目錄名要是明文（`s/<server host>/a/<localpart>/`），
路徑還是把「這台機器上有 alice 跟 bob，都在 matrix.org」直接寫在檔案總管裡。
所以 **server host 與 localpart 都加密**，外部只看得到 `base58_base58`。

⚠️ 連帶的兩個地方，不跟著做就等於沒加密：

| 地方 | 怎麼做 |
|---|---|
| `current`（CLI 記「預設用誰」） | 存兩層加密後的目錄名；要知道那是誰得解密 |
| `account status` 列帳號 | 必須解鎖才列得出來（§2.6）。這是這個決定的代價，寫在這裡不藏 |

### 2.2 名字的樣子：`<base58 nonce>_<base58 密文>`

Base58 的字母表**沒有底線**，所以 `_` 可以當分隔符，兩段各自編碼，解析不必靠「前 24 byte 是 nonce」這種長度常數
（維護者 2026-09-09 出的主意）。

**第六把子金鑰**一把就夠（`BLAKE3 derive_key("wbf-matrix-client account directory v1", master)`，§1）：
兩層（加上 `r/` 的 recovery key 檔名）用不同的 aad 與不同的 nonce context 分開，🚫 不需要第七把。

```
第一層  s/<B58(nonce_s)>_<B58(ct_s)>/
  nonce_s = BLAKE3 keyed_hash(key, "wbf server-dir-nonce v1" ‖ 0x00 ‖ host) 前 12 byte
  ct_s    = ChaCha20-Poly1305(key, nonce_s, host,      aad = "wbf-matrix-client server dir v1")

第二層  a/<B58(nonce_a)>_<B58(ct_a)>/
  nonce_a = BLAKE3 keyed_hash(key, "wbf account-dir-nonce v1" ‖ 0x00 ‖ host ‖ 0x00 ‖ localpart) 前 12 byte
  ct_a    = ChaCha20-Poly1305(key, nonce_a, localpart, aad = "wbf-matrix-client account dir v1" ‖ host)

recovery key  r/<B58(nonce_r)>_<B58(ct_r)>          明文是 `recovery-key@mxid`（/docs/design/keys/room-key-backup.md §8）
  nonce_r = BLAKE3 keyed_hash(key, "wbf recovery-name-nonce v1" ‖ 0x00 ‖ 明文) 前 12 byte
  ct_r    = ChaCha20-Poly1305(key, nonce_r, 明文,      aad = "wbf-matrix-client recovery name v1")
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
- 🚫 **不可以用固定 nonce**。同一把金鑰配同一個 nonce 加密不同的明文，ChaCha20 是 stream cipher，
  兩份密文 XOR 就洩漏明文 XOR。nonce 從明文導出正好保證「不同明文 → 不同 nonce」。
- **第二層的 aad 與 nonce 都綁明文 host**：帳號目錄從一個 server 目錄搬到另一個底下就解不開（fail closed），
  而且同一個 localpart 在兩個 server 上目錄名不同。第一層的 aad 是固定字串（它上面沒有東西可綁）。
- nonce 輸入的各段之間加 `0x00` 分隔：`host="a" localpart="bc"` 與 `host="ab" localpart="c"` 不會導出同一個 nonce。

### 2.3 加密之前先正規化，否則同一個 server 會長出兩個目錄

加密是逐 byte 的：`matrix.org` 與 `MATRIX.ORG` 進去就是兩個不同的目錄。
所以加密之前先**正規化**（`accounts::server_host_of`）：

- **host**：取 URL 的 host，小寫；非預設 port 才帶上（`localhost:6167`、`matrix.org`）。
  🚫 不過濾字元（例如只留 `[A-Za-z0-9._-]`）—— 檔名是 Base58，不需要檔名安全化，過濾只會讓不同的 host 撞在一起。
- **localpart**：以 **server 回的 `user_id`** 為權威（`login` 時目錄名對不上就搬過去）。
  🚫 不拿使用者打的 `--user` 直接加密。

### 2.4 Windows 的大小寫陷阱與長度

⚠️ Base58 **區分大小寫**，Windows 的檔名**不區分**。所以「兩個名字只差大小寫」在 Windows 上是同一個目錄。
密文有 40 byte 以上的熵，實際碰不到，但 🚫 不靠「不可能碰撞」寫程式（全域 CLAUDE.md A5）：

- **建目錄前先檢查**：目標名字已經存在時，把它解密出來比對 —— 是同一個 host／localpart 才用，不是就報錯，
  🚫 不覆蓋、🚫 不加後綴自己找一個空位。
  `AccountDir::locate` 只算路徑、不碰磁碟；登入時接著叫 `AccountDir::verify_names_on_disk`：上層目錄裡只差大小寫的名字逐一解密比對，對不上就 `Usage`。
- 每一段名字上限 **200 字元**（Windows 單一路徑元件是 255）。Base58 大約是 byte 數的 1.37 倍，
  nonce 那段固定 17 字元，所以密文那段大約 130 byte 以上才會踩到 —— Matrix 的 localpart 上限是 255 byte，
  踩得到，要有這個檢查。超過就報錯，🚫 不截斷（截斷等於不可逆）。

#### 2.4.1 ⚠️ 真正咬人的不是單段長度，是**整條路徑**（2026-09-09 實測）

Windows 的 `MAX_PATH` 是 **260**，而加密把兩段目錄名從 19 字元（`localhost_6167` ＋ `alice`）
撐到 106。當時實測 `matrix-sdk-event-cache.sqlite3` 的完整路徑（那時 `m/` 裡最長的檔名；現在 `m/` 只開 state 與 crypto，最長是 `matrix-sdk-crypto.sqlite3-wal`，29 字元，比下表短）：

| | 加密名字合計 | 最長路徑（data dir 39 字元） |
|---|---|---|
| 24 byte nonce ＋ 長目錄名 | 138 | **230**（餘裕 20，data dir 稍深就爆） |
| **12 byte nonce ＋ `s`／`a`／`m`** | **106** | **184**（餘裕 76） |

⚠️ 路徑太長時 sqlite 也只回「開不了」，容易被誤報成「it was made with another key file」——害人去刪一個其實沒問題的目錄。
所以 `build_client`（`backend/matrix_sdk.rs`）先看路徑長度再決定怎麼報。

📎 **目錄名加密的成本不在 CPU，在路徑預算**。之後要再加一層加密目錄之前，先算一次最深的那條路徑。

### 2.5 讀回來：掃兩層，建記憶體裡的對照

維護者要的流程：**起始時掃一次雙層結構、嘗試解密，解失敗的不加入清單，成功的就把 Base58 映射成明文帶進路徑。**

```
s/ 底下每個目錄名
  → 沒有 `_`、任一段 Base58 解碼失敗、AEAD 解不開  → 跳過，不加入清單
  → 解得開                                        → host，再往下掃它的 a/
       a/ 底下每個目錄名
         → 解不開（aad 綁的是這一層的 host）        → 跳過
         → 解得開                                  → localpart，組回 @localpart:<server_name>
```

`r/`（recovery key，/docs/design/keys/room-key-backup.md §8）跟著一起掃：檔名的明文是 `recovery-key@mxid`，同一套規則。

- 對照表**只在記憶體裡**，一個命令的生命週期。🚫 不落地成明文索引檔 —— 那等於把剛加密的東西再寫一次明文。
- **解不開的不猜、不刪、不報錯**：可能是另一把 `local.key` 建的（換過 data dir），也可能是舊版留下的。
  fail closed 是「當它不存在」。整個 `s/` 都解不開時印一行提示（§2.7）。

#### 2.5.1 兩條路：算得出來的，與只能比對的（維護者 2026-09-10 定）

| 手上有什麼 | 走哪條 |
|---|---|
| **確定就是這個明文**（剛 `login`、`current` 解出來的） | §2.2 的確定性加密，直接算，不碰磁碟 |
| **使用者打進來的字串**（`account del @BOB:matrix.org`） | 當場掃一次建 map，再從 map 比對 |

第二條路**不能用算的**：`@BOB:matrix.org` 加密出來的名字跟 `@bob:matrix.org` 完全不同，
而「大小寫不敏感」沒有算式 —— 只能拿現場有什麼來比。所以：

```
account destroy @BOB:matrix.org
  ① 刷新：掃 s/*/a/* 與 r/，解密每一段名字        ← 當場做，不吃上一次的結果
  ② 建 map：明文 → 磁碟上那個（加密的）名字
  ③ 比對：先精確，再大小寫不敏感
       ⚠️ 大小寫那一輪對到兩個以上 → `Err`，訊息列出候選
          🚫 不靜默當作沒有：`destroy` 說的是「什麼都不留」，它得知道自己沒留乾淨
  ④ 帳號目錄與 recovery key **都從這同一份 map 來**
```

⚠️ **④ 是重點**：掃兩次就有兩個不同時刻的答案，而這個命令要用它們決定刪哪個目錄。

⚠️ map 是**快照，不是快取**：🚫 不存成長命的全域狀態 —— 存起來的那一份不會知道
中間有東西被刪掉。會刪檔的命令一律當場刷新。

📎 例：recovery key 的檔名是用 server 的權威 mxid 封的（/docs/design/keys/room-key-backup.md §8）。拿使用者打的那串去算，
大小寫差一個字就刪不到 —— 而 `destroy` 的語意是「什麼都不留」。

### 2.6 代價：`account status` 要解鎖

兩層都加密，不解密就不知道有哪些 server、哪些帳號。所以列帳號也要開 vault。

- `passphrase` 模式下 `account status` 會問 passphrase。
- 🚫 不做「列出 Base58 但不解密」的半套輸出：那對使用者沒有意義，只會讓人以為壞了。
- 本來就要開帳號目錄的命令沒有變差（它們早就要解鎖才讀得到 `session.sealed`）。

### 2.7 舊的 data dir：砍掉重來，不寫遷移

維護者 2026-09-09：**server 從未上線、client 從未被使用，breaking 就 breaking，當前環境直接砍掉沒問題。**
所以這裡 🚫 不寫遷移、🚫 不留 `layout` 之類的版本標記檔 —— 少一個檔、少一段只跑一次的程式。

- 明文佈局的舊目錄在 §2.5 的掃描裡本來就解不開，會被跳過（fail closed），不會被誤認成別人的帳號。
- `s/` 底下有東西但**一個都解不開**時（等著被刪的 `🗑️…` 不算，/docs/design/storage/local-cache-db.md §5），印一行提示就好：

  ```
  warning: no directory in <data dir>/s could be decrypted with this local.key; if this data dir was made
           by an older build, delete it and run `login` again
  ```

### 2.8 還開著

- 目錄的 mtime、檔案大小、帳號數量仍會洩漏活躍程度：外面數得出這台機器上有幾個 server、幾個帳號，
  只是不知道是誰、在哪家。這是檔案系統層面的事，不處理。

## 3. passphrase 是任意 bytes，不是字串（維護者 2026-09-09 定）

> 維護者的原話：passphrase 可以是任何字元、純二進位文檔，**不要擅自翻譯成純 ASCII**；
> 可以是 UTF-8 的中文，可以是一個 mp3，可以是任何東西。最常見的用法才是一串字。

### 3.1 兩支讀法，不共用

CLI 的 `--passphrase-file` 與 `--password-file` 各有一支：`read_passphrase_file`（原始 bytes）與 `read_password_file`（UTF-8、去尾換行）。
共用一支的話，要嘛 mp3 讀不進來（要求合法 UTF-8），要嘛結尾多一個換行就被吃掉、導出不同的 KEK。分岔的理由在 §3.4。

### 3.2 passphrase 怎麼讀

- `Unlock::Passphrase` 的內容是 `Zeroizing<Vec<u8>>`，不是 `String`。`derive_kek` 吃 `&[u8]`。
- `--passphrase-file`：**整檔原始 bytes**，🚫 不去尾換行、🚫 不驗 UTF-8、🚫 不 trim 空白。
  ⚠️ 這代表 `echo hunter2 > pw` 產生的檔（結尾有 `\n`）跟 `printf hunter2 > pw` 是**兩個不同的 passphrase**。
  這是刻意的：檔案就是檔案，我們不替使用者猜哪個 byte 不算數。
- 終端輸入：讀到的那一行的 UTF-8 bytes（不含結尾換行）。終端只打得出字，這是它的天然子集。
- RPC：`passphrase_base64`，解開的 bytes 原樣用（/docs/design/rpc-specs/rpc-spec.md §3.1）。
- 空的判斷是 **`bytes.is_empty()`**（「沒設 passphrase」是 `Plain` 模式，不是空 passphrase，§1）。

### 3.3 沒有相容包袱：`v: 1` 就是這個定義

`v: 1` 的定義就是「整檔原始 bytes」。維護者 2026-09-09：
**server 從未上線、client 從未被使用，breaking 就 breaking，當前環境直接砍掉沒問題。**

- 所以 🚫 **不寫兩套讀法**、🚫 不升版本號、🚫 不留相容分支。一個問題一份實作，少一個永遠不會有人再讀第二次的分支。
- 舊讀法產生的 `local.key` 在這裡就是 `wrong passphrase`；處置同 §2.7：刪掉 data dir 重新 `login`。
- 📎 這條的前提是「還沒有使用者」。**之後有了就不再適用** —— 那時候要改 KDF 的輸入就得升版本號、留讀舊檔的路徑。

### 3.4 `--password-file` **不跟著改**

⚠️ passphrase 與 password 是兩種東西（§1 開頭的用字），這裡是它們處理方式分岔的地方：

| | 給誰 | 怎麼讀 |
|---|---|---|
| **passphrase**（`--passphrase-file`） | 只餵給本機的 Argon2id，永遠不出這台機器 | **原始 bytes**，一個都不動（§3.2） |
| **password**（`--password-file`） | 送給 homeserver 的 `/login`，Matrix 規定它是 JSON 字串 | 當 UTF-8 讀，去掉結尾一個換行（`\n` 或 `\r\n`） |

🚫 不要「統一」這兩個 —— password 是 mp3 的話根本送不出去（JSON 塞不進任意 bytes），
而 passphrase 去尾換行會讓「檔案內容」與「實際用的東西」對不上。
它們長得像，但一個是本機的鑰匙、一個是要上線的憑證。

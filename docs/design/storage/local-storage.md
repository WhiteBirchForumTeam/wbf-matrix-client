# 本地儲存：資料目錄裡每一個檔

這份是**清單**：資料目錄（與少數寫在外面的檔）裡有哪些檔、每個檔裝什麼、什麼格式、誰的格式、用哪把鑰加密、怎麼寫、誰刪。
「為什麼這樣設計」留在各自的文件，這裡只指過去：

| 想知道 | 去讀 |
|---|---|
| 主金鑰、子金鑰怎麼導、passphrase、目錄名怎麼加密 | `/docs/design/storage/vault-and-keys.md` §1、§2 |
| `cache.db` 存什麼、schema、為什麼 SQLCipher、多帳號混存 | `/docs/design/storage/local-cache-db.md` §3、§5 |
| `m/` 的 matrix-sdk store 與 StoreCipher | `/docs/design/storage/local-cache-db.md` §4 |
| 媒體池的檔案格式、清理 | `/docs/design/media/media-pool.md`、`/docs/design/media/media-download.md` §4 |
| `k/snapshot`、`r/` 的 recovery key、登出的閘門 | `/docs/design/keys/room-key-backup.md` §4、§7、§8 |
| `m/td.json`（to-device 的游標） | `/docs/design/keys/to-device-client.md` §2 |
| `daemon.token`、`daemon.json` | `/docs/design/rpc-specs/local-interface.md` §3 |
| `wbf.conf` | `/docs/design/rpc-specs/wbf-cli-spec.md` §10 |

⚠️ **目錄樹只畫在這裡（§3）**。其他文件需要的時候指過來，🚫 各自再畫一份——三份樹已經漂移過一次（`r/` 被畫進帳號目錄、`m/` 寫成兩個 store、`local.key` 寫成「32 byte」）。

📎 **範例的來源**：除了標「示意」的，都是 2026-10-06 對本機 wbfuwunel 實際跑出來的（`wbf-cli login` → `send` → `upload --manifest` → `download` → `set-passphrase`，
再起一次 daemon），host 是 `127.0.0.1:6167`、帳號是 `@alice:localhost`。那個資料目錄用完就刪了；金鑰、token 這類值仍然換成 `<…>`。

## 1. 資料目錄在哪

| 誰 | 位置 |
|---|---|
| daemon | 必帶 `--data-dir` 或 `WBF_DATA_DIR`，**沒有預設** |
| wbf-cli | `--data-dir` → `WBF_DATA_DIR` → 預設：Windows `%APPDATA%\wbf-cli`、macOS `~/Library/Application Support/wbf-cli`、其他 `$XDG_DATA_HOME/wbf-cli`（沒設就 `~/.local/share/wbf-cli`） |

例：Windows 上沒給 `--data-dir` 的 CLI → `C:\Users\Weil\AppData\Roaming\wbf-cli\`。

## 2. 共通的規則

### 2.1 敏感的東西都加密

**敏感＝落地之後被拿走就會洩漏金鑰、token、或「這台機器連過誰、跟誰說話」**（維護者 2026-10-06）。判準：

- 金鑰、token、訊息內容、檔案內容 → 一定加密。
- **mxid、房間 id、server host** → 也加密。目錄名特地加密就是為了藏它們（`/docs/design/storage/vault-and-keys.md` §2），檔案內容🚫 把它們用明文再漏一次。
- 只有數字、沒有身分的狀態（`m/td.json` 的游標）→ 可以明文。

明文落地的只有 §9 列的那幾個，每一個都寫了為什麼可以。

### 2.2 子金鑰 → 檔

主金鑰在 `local.key` 裡，子金鑰都從它導（`/docs/design/storage/vault-and-keys.md` §1 是權威）。哪把鑰鎖哪些檔：

| # | 子金鑰（context 字串去掉前綴 `wbf-matrix-client ` 與後綴 ` v1`） | 鎖的檔 |
|---|---|---|
| 1 | `cache sqlcipher` | `s/…/cache.db`（整檔 SQLCipher） |
| 2 | `matrix-sdk store` | `s/…/a/…/m/*.sqlite3`（上游 StoreCipher 的外層） |
| 3 | `session` | `session.sealed`、`r/*`，**靠 aad 分**（§2.3） |
| 4 | `media store` | `s/…/media/` 底下每個池檔與 `.seek` 檔 |
| 5 | `room key backup` | `k/snapshot`（base64 後當上游匯出的 passphrase） |
| 6 | `account directory` | `s/`、`a/`、`r/` 底下每個名字 |

### 2.3 封檔（sealed file）的格式

`session.sealed`、`r/*` 都是同一個殼（`vault.rs` 的 `seal_file`／`open_sealed_file`），pretty JSON：

```json
{
  "v": 1,
  "nonce": "<base64 的 24 byte>",
  "sealed": "<base64 的 密文＋16 byte tag>"
}
```

例（實際的 `session.sealed`，307 byte）：

```json
{
  "v": 1,
  "nonce": "mYHlVbTJxEYeAIhpnGNvl/U5zQRct/3Z",
  "sealed": "qxU9eKzk5qyovebdj+8hiaI6egltR8pSvDicgcyMDZdeRG084q41ryty3nsOg4oZwtrJj3ff7hTTlBHkhno1BQ32kwrrsA2dArxkTpXVT9ne6DhbyFO/K5JeImiN5wclwVkahLMbfhvOx6pNKvzk7YQy0tG+tXjE8SGQ3821RJZIuk/0lZgCERwsvuyl8oq8T27kzJ/09L7I255IocrNwubrtz7s/CAQu9E17nox"
}
```

- XChaCha20-Poly1305，第三把子金鑰，nonce 每次寫都重新隨機（同一份內容寫兩次，`nonce` 與 `sealed` 都不一樣）。
- **每種檔帶自己的 aad**，同一把鑰封的密文搬到別種檔解不開：

  | 檔 | aad |
  |---|---|
  | `session.sealed` | `wbf-matrix-client session.sealed v1` |
  | `r/*` | `wbf-matrix-client recovery.sealed v1` |
  | （`local.key` 的 passphrase 模式包主金鑰） | `wbf-matrix-client local.key v1` |

  例：把 `session.sealed` 複製成 `r/` 裡的一個檔，讀 recovery key 會得到「cannot open …; it was sealed with another key file」，🚫 讀成一串 recovery key。
- 讀的時候：`v` 不認得、nonce 不是 24 byte、解不開、aad 不對 → 都是 `Err`，🚫 panic、🚫 猜。

### 2.4 怎麼寫：`write_private`

`local.key`、`current`、`daemon.json`、`wbf.conf`、所有封檔、上傳的狀態檔與 manifest 都走 `vault::write_private`：

1. `create_dir_all(上一層)`。
2. 寫同目錄的暫存檔，名字是「原副檔名後面接 `.<pid>.tmp`」（Unix 0600），`sync_all`。
   例（pid 30368）：`local.key` → `local.key.30368.tmp`；`session.sealed` → `session.sealed.30368.tmp`；沒有副檔名的 `current` → `current..30368.tmp`。
3. `rename` 蓋掉目標。**🚫 先刪目標**：刪了之後、rename 之前斷電，`local.key` 就沒了，整個 data dir 跟著解不開。
   `std::fs::rename` 在 Windows 也直接蓋（MoveFileExW ＋ MOVEFILE_REPLACE_EXISTING）。rename 失敗就刪掉暫存檔。

- 沒有 fsync 資料夾：斷電可能看到舊檔或新檔，不會是半個檔。
- Windows 不設 ACL，靠使用者目錄本來的權限。

### 2.5 目錄名

`s/`、`a/`、`r/` 底下的每個名字都是 `<base58 nonce>_<base58 密文>`，第六把子金鑰；格式與每一層的 aad 在 `/docs/design/storage/vault-and-keys.md` §2.2。

| 層 | 明文 | 實際的名字 |
|---|---|---|
| `s/` | `127.0.0.1:6167`（正規化過的 host） | `qEo6fzbP5sYDLQTa_kAMTdNaqhioNtFdiJeWrCLJpij1DK2F3p9iLfVGx4` |
| `a/` | `alice`（localpart） | `X5zKj34KRZ7dGWS9_GRFDsqmFjUfSA4RWoEwYv9EB7WHpR` |
| `r/` | `recovery-key@alice:localhost` | 示意：同樣的形狀，明文比較長所以底線後面約 60 個字元 |

- 底線前面固定 16 個字元左右（12 byte 的 nonce）；後面的長度跟著明文走（明文 ＋ 16 byte tag）。所以 host 越長名字越長，上限 200 字元，超過就報錯、🚫 截斷。
- 同一台機器、同一個 host 永遠算出同一個名字（nonce 由主金鑰與明文導出），所以下次登入找得到；換一個 `local.key` 就整個對不上。
- 中間的段名只有一個字母（`s`、`a`、`m`、`k`、`r`）是因為 Windows 的 MAX_PATH（`/docs/design/storage/vault-and-keys.md` §2.4.1）：上面兩段就吃掉 106 字元。

## 3. 目錄樹

```
<data dir>/
├── local.key                              主金鑰（§4.1）
├── current                                CLI 的目前帳號（§4.2）
├── account.lock                           登入／登出／destroy 的 OS 鎖（§4.3）
├── daemon.lock                            daemon 獨佔的 OS 鎖（§4.4）
├── daemon.json                            daemon 起好之後的 port（§4.5）
├── daemon.token                           前端寫、daemon 讀的 256 byte（§4.6；位置可用 --token-file 換）
├── wbf.conf                               設定檔（§4.7）
├── r/                                     recovery key（§4.8；目前只有一般 Matrix 帳號會有）
│   └── <加密的 "recovery-key"+mxid>
└── s/
    ├── <加密的 server host>/              一個 server
    │   ├── cache.db                       這個 server 上所有帳號共用的快取（§5.1）
    │   ├── cache.db-wal                   SQLite 的 WAL（開著時才有）
    │   ├── cache.db-shm                   SQLite 的共享記憶體（開著時才有）
    │   ├── to_be_deleted.lock             destroy 最後一個帳號時的標記（§5.3）
    │   ├── media/                         媒體池（§5.2）
    │   │   ├── <hash 前 2 個 hex>/
    │   │   │   └── <blake3 hex 64 字元>   完整的檔
    │   │   └── pending/
    │   │       ├── m<media.id>            下載中的主檔
    │   │       └── m<media.id>.seek       下載中的 seek 暫存檔
    │   └── a/
    │       └── <加密的 localpart>/        一個帳號（＝這台機器上的一台裝置）
    │           ├── session.sealed         登入的 session（§6.1）
    │           ├── m/                     這台裝置的 crypto 狀態（§6.2）
    │           │   ├── matrix-sdk-crypto.sqlite3
    │           │   ├── matrix-sdk-crypto.sqlite3-wal   （開著時才有）
    │           │   ├── matrix-sdk-crypto.sqlite3-shm   （開著時才有）
    │           │   ├── matrix-sdk-state.sqlite3        只有一般 Matrix 帳號
    │           │   └── td.json            to-device 的游標（§6.2.1）
    │           └── k/
    │               └── snapshot           本地房間金鑰快照（§6.4；目前只有一般 Matrix 帳號會有）
    └── 🗑️<加密的 server host>/            destroy 時改名、刪到一半的 server 目錄（§5.4）
```

實際的例子（`wbf-cli login` 之後再上傳、下載一個 3000 byte 的檔，CLI 結束之後；名字見 §2.5）：

```
wbfdemo/
├── account.lock
├── current
├── daemon.lock
├── local.key
├── wbf.conf
└── s/
    └── qEo6fzbP5sYDLQTa_kAMTdNaqhioNtFdiJeWrCLJpij1DK2F3p9iLfVGx4/
        ├── cache.db
        ├── media/
        │   ├── e0/
        │   │   └── e050d82e36326f3fdf063dd38b034f2b43be659eba3d6488396db907d66e60d5
        │   └── pending/
        └── a/
            └── X5zKj34KRZ7dGWS9_GRFDsqmFjUfSA4RWoEwYv9EB7WHpR/
                ├── m/
                │   └── matrix-sdk-crypto.sqlite3
                └── session.sealed
```

- `-wal`／`-shm` 不在：CLI 結束時關乾淨了。daemon 開著的時候看得到。
- `td.json` 不在：CLI 沒開 `Keys` 線。daemon 收過 to-device 就有。
- `daemon.json`、`daemon.token` 不在：還沒起過 daemon（起過的見 §4.5、§4.6）。
- `*.<pid>.tmp`（§2.4）、`td.json.tmp`、`k/snapshot.tmp` 是寫到一半的暫存檔，正常結束時不會留著。

## 4. 資料目錄的根

### 4.1 `local.key`

| | |
|---|---|
| 格式 | 我們的，pretty JSON（`mode` 決定其他欄位） |
| 加密 | passphrase 模式：Argon2id 導 KEK、XChaCha20-Poly1305 包主金鑰。plain 模式：**主金鑰明文**（§9） |
| 誰寫 | `Vault::create`（建 vault，已存在就拒）、`Vault::set_unlock`（設／改／拿掉 passphrase，主金鑰不變） |
| 誰刪 | 沒有人。程式只重寫它 |

plain 模式（`login` 時沒給 passphrase，91 byte）：

```json
{
  "mode": "plain",
  "v": 1,
  "master": "<base64 的 32 byte 主金鑰，44 個字元>"
}
```

passphrase 模式（`set-passphrase` 之後，287 byte；`salt`、`nonce`、`wrapped` 沒有 passphrase 解不開，照原樣貼）：

```json
{
  "mode": "passphrase",
  "v": 1,
  "kdf": {
    "name": "argon2id",
    "m_kib": 65536,
    "t": 3,
    "p": 1,
    "salt": "PzKsvvoGNLLIFizA6KSiuQ=="
  },
  "nonce": "TNFziCCmzZ5yWLXmPPxAUDBcn2jmKBBa",
  "wrapped": "zi9roWQBnUCVTmi5nPysYQPrMpwoqMgcAG4g7ch4vy4FO9xTcp41cU2ZGqQtS8qT"
}
```

`wrapped` 是 48 byte：32 byte 主金鑰 ＋ 16 byte tag。設、改、拿掉 passphrase 都只重寫這個檔，主金鑰不變，其他檔都不動。

### 4.2 `current`

| | |
|---|---|
| 內容 | 一行 `<server 目錄名>/<帳號目錄名>`，兩段都是加密過的名字，沒有換行 |
| 加密 | 不另外加密；名字本身已經是密文 |
| 誰寫 | `login` 成功、切換帳號 |
| 誰刪 | 登出的帳號就是它指的那個時 |

例（105 byte）：

```
qEo6fzbP5sYDLQTa_kAMTdNaqhioNtFdiJeWrCLJpij1DK2F3p9iLfVGx4/X5zKj34KRZ7dGWS9_GRFDsqmFjUfSA4RWoEwYv9EB7WHpR
```

### 4.3 `account.lock`、4.4 `daemon.lock`

空檔（0 byte），只拿來掛 OS 的檔案鎖（程序結束核心就放手，沒有殘留鎖要清），**不寫內容、🚫 刪**（刪掉會跟「另一個程序正要開它」對撞）。

- `account.lock`：登入、登出、destroy 全程握著獨佔鎖；`link_keeper` 只探不握（`/docs/design/storage/local-cache-db.md` §5）。
- `daemon.lock`：daemon 與單發的 CLI 都握獨佔鎖，同時只有一個碰資料目錄；唯讀的工具才握共享鎖（`/docs/design/overview/architecture-v2.md` §0.2）。
  例：daemon 開著的時候跑 `wbf-cli rooms`，CLI 拿不到鎖、直接報錯，🚫 排隊。

### 4.5 `daemon.json`

| | |
|---|---|
| 內容 | `rpc_port`、`data_port`、`pid`，以及 `instance`（每次啟動新產的 UUID） |
| 格式 | 我們的，一行 JSON |
| 加密 | 沒有（沒有秘密，§9） |
| 誰寫 | daemon 兩個 port 都綁好之後；啟動時先刪掉上次留下的 |
| 誰刪 | daemon 正常結束時。被強制殺掉會留著，下次啟動時刪 |

例（96 byte）：

```json
{"data_port":3931,"instance":"b06d5e31-a8d8-457d-b9af-a6452e386c55","pid":30368,"rpc_port":3930}
```

📎 stdout 那一行 ready 訊號多一個 `"ready":true`，檔裡沒有。

### 4.6 `daemon.token`

| | |
|---|---|
| 內容 | 256 byte 亂數（原始 byte，不是文字）：RPC 與資料平面的金鑰材料 |
| 誰寫 | **前端**（不在這個 repo），0600；Unix 上 group／other 有任何一個位元 daemon 就拒絕啟動 |
| 誰刪 | 前端，用 `wbf_daemon::token::shred`（亂數、0xFF、0x00 各蓋一次再刪）。daemon 只讀一次、🚫 刪；ready 之後 stderr 會提醒「now shred …」 |

例（`xxd` 的前兩行；這是測試用的、用完就丟的那個）：

```
00000000: fc3f 620a 5386 727f 4ba7 d430 c40b c71d  .?b.S.r.K..0....
00000010: 342d e57d fc44 ab86 4d3a 6577 0363 ec8f  4-.}.D..M:ew.c..
```

這個檔不加密：它就是那把鑰，加密它得再有一把鑰放在旁邊（`/docs/design/rpc-specs/local-interface.md` §3）。

### 4.7 `wbf.conf`

`[section]` 與 `KEY=value   ; 來源` 的文字檔。🚫 存秘密（`conf.rs` 的 `REFUSED_KEYS` 拒收）。
只在指定了 `--data-dir`／`WBF_DATA_DIR`、檔還不在、命令成功時由 CLI 生一份；之後程式只讀不寫（`/docs/design/rpc-specs/wbf-cli-spec.md` §10）。

例（`login` 自動生成的那份，550 byte）：

```ini
; wbf.conf —— wbf-cli 自動生成的一份起手式（/docs/design/rpc-specs/wbf-cli-spec.md §10.3）
; 每個值後面註明它這次是哪來的。改這個檔不影響已經登入的帳號。
; 🚫 這裡不放秘密：token、password、passphrase 一律不從這裡讀。
; 已經存在的 wbf.conf 永遠不會被改寫——要重生成就先自己刪掉。

[general]
SERVER=http://127.0.0.1:6167   ; flag or env
TRANSPORT=ws   ; built-in default

[backup]
SERVER_BACKUP=on   ; built-in default
LOCAL_ROOM_KEYS=on   ; built-in default
```

### 4.8 `r/<名字>`：recovery key

| | |
|---|---|
| 名字 | `"recovery-key" + mxid` 加密後的樣子（第六把子金鑰，recovery 那一層的 aad）。例：`@alice:localhost` 的明文是 `recovery-key@alice:localhost` |
| 內容 | recovery key 字串，示意：`EsT1 abcd efgh …`（上游產的，48 個 base58 字元、每 4 個一組） |
| 格式 | 封檔（§2.3），aad `wbf-matrix-client recovery.sealed v1` |
| 權限 | 檔 0600、`r/` 0700 |
| 誰寫 | `key-backup recovery`（`Core::create_recovery_key`） |
| 誰刪 | 只有 destroy。**🚫 logout 碰它**——它不放在帳號目錄底下就是為了這個（`/docs/design/keys/room-key-backup.md` §8） |

⚠️ 目前只有一般 Matrix 帳號會有：wbf 帳號跑 `key-backup recovery` 會得到「wbf accounts do not build the matrix-sdk client」（金鑰備份還要 matrix-sdk 的 Client，`/docs/design/daemon/account-session.md` §6）。

## 5. server 目錄：`s/<加密的 host>/`

名字是正規化過的 server host（小寫、去掉 scheme 與預設 port）加密後的樣子。
例：`http://127.0.0.1:6167` → `127.0.0.1:6167`；`https://Matrix.Example.org:443/` → `matrix.example.org`。

### 5.1 `cache.db`

| | |
|---|---|
| 內容 | 我們的 schema（v9：`media` 多了 `kind`、`verified`）：`meta`、`users`、`rooms`、`events`、`events_synced_log`、`room_list`、`sync_state`、`read_positions`、`media`、`event_media`。每張表在 `/docs/design/storage/local-cache-db.md` §5 |
| 格式 | SQLite（WAL），開著的時候旁邊有 `-wal`、`-shm` |
| 加密 | SQLCipher 整檔，第一把子金鑰當 raw key；開檔時 `cipher_version` 是空的（沒連到 SQLCipher）就拒絕 |
| 權限 | SQLite 預設（umask），🚫 0600——內容整檔加密，權限不是防線 |
| 誰寫 | daemon 的單一寫入者（`/docs/design/daemon/daemon-runtime.md` §2） |
| 誰刪 | 這個 server 最後一個登入中的帳號登出時整檔刪；destroy 最後一個帳號時跟著目錄走；版本號不對、server 不對、解不開時整個重建（它是快取） |

怎麼看出它整檔加密（`xxd` 前 32 byte）——連 SQLite 的檔頭都看不到：

```
00000000: 6a88 8140 e912 15e7 d6ec daa6 1328 333c  j..@.........(3<
00000010: 0556 8b64 137d 0307 f030 281d 278c 34fa  .V.d.}...0(.'.4.
```

對照 `m/` 裡上游的 sqlite（§6.2）：檔頭照樣是 `SQLite format 3`，只有值是加密的。

### 5.2 `media/`：媒體池

格式的權威在 `/docs/design/media/media-download.md` §4，這裡只列檔：

| 檔 | 例 | 內容 | 加密 |
|---|---|---|---|
| `<hh>/<blake3 hex>` | `e0/e050d82e36326f3fdf063dd38b034f2b43be659eba3d6488396db907d66e60d5` | 完整的檔；名字是**明文**的 BLAKE3，資料夾是它的前 2 個 hex；同內容只存一份 | 每段 XChaCha20-Poly1305，第四把子金鑰，32 byte 檔頭 ＋ 固定長度的段 |
| `pending/m<media.id>` | `pending/m7`（示意：`cache.db` 的 `media` 那一列的 id 是 7） | 下載中的主檔，格式同上；檔本身就是進度，🚫 暫存檔 | 同上 |
| `pending/m<media.id>.seek` | `pending/m7.seek`（示意） | 播放跳著讀時的暫存塊 | 同一把鑰，自己的 aad 前綴 |

例：那個 3000 byte 的檔在池裡是 65588 byte＝32 byte 檔頭 ＋ 1 段（4 byte 長度 ＋ 65536 byte 資料與補零 ＋ 16 byte tag）。檔頭：

```
00000000: 5742 4650 0200 0000 0000 0100 b029 2733  WBFP.........)'3
00000010: 1ea5 222a e192 ec14 daa9 4ffe 619c 703d  .."*......O.a.p=
```

| byte | 值 | 是什麼 |
|---|---|---|
| 0–3 | `57 42 46 50` | `WBFP` |
| 4 | `02` | 格式版本 2 |
| 5–7 | `00 00 00` | 保留 |
| 8–11 | `00 00 01 00` | 段長 65536（u32 little-endian） |
| 12–27 | `b0 29 27 33 …` | nonce_base（16 byte 亂數） |
| 28–31 | | owner：mxc 的 BLAKE3 前 4 byte |

- 檔名、mimetype、mxc **🚫 在池檔裡**，只在 `cache.db` 的 `media` 那一列（整檔加密）。
- 權限 umask：內容每段都加密。
- 誰刪：配額清理（預設 2 GiB）、`sweep`（沒人認領、或超過保護期 7 天的 pending）、改了描述、destroy。
  ⚠️ 最後一個帳號**登出**時 `cache.db` 刪了、`media/` 沒刪：池檔要等之後第一次 `sweep` 發現沒有任何一列指著它才清（§10）。

### 5.3 `to_be_deleted.lock`

空檔，是**標記**不是 OS 鎖：destroy 這個 server 的最後一個帳號時先寫它，存在的期間 `login` 到這個 server 一律拒（`ServerPendingRemoval`）。刪完就拿掉。

### 5.4 `🗑️<加密的 host>/`

destroy 最後一個帳號時，整個 server 目錄先改名再刪。例：`s/qEo6fzbP5sYDLQTa_kAMT…Gx4/` → `s/🗑️qEo6fzbP5sYDLQTa_kAMT…Gx4/`。
刪到一半當掉會留著；掃描會跳過它，要手動刪。

## 6. 帳號目錄：`s/…/a/<加密的 localpart>/`

名字是 localpart 加密後的樣子（aad 帶 host，所以兩個 server 上的 `alice` 名字不一樣）。登入時先照使用者打的名字算，成功之後改名成 server 回的 `user_id` 算出來的那個
（例：打 `alice` 或 `@alice:localhost` 都會落在同一個目錄）。
**登出🚫 刪帳號目錄**：留一個空目錄，`account status` 才看得到「這個帳號登出了」。destroy 才刪。

### 6.1 `session.sealed`

| | |
|---|---|
| 格式 | 封檔（§2.3），aad `wbf-matrix-client session.sealed v1`；磁碟上的樣子見 §2.3 的例子 |
| 誰寫 | 登入成功（`finish_login_locally`） |
| 誰刪 | 登出時 server 那邊登出成功之後 |

解開之後（`wbf_sdk::login::Session`；這次登入的值，token 換掉）：

```json
{
  "server": "http://127.0.0.1:6167",
  "user_id": "@alice:localhost",
  "device_id": "cNmyjBoUHr",
  "access_token": "<access token>",
  "backend": "wbf_sdk"
}
```

- `backend`：`"wbf_sdk"`（wbf server，沒有 matrix-sdk 的 Client）或 `"matrix_sdk_client"`（一般 Matrix）。
- `store_dir`：只有 `matrix_sdk_client` 那條有，值是登入時 `m/` 的絕對路徑，例 `"C:/…/a/X5zK…HpR/m"`。wbf 帳號沒有這個欄位（不是空字串）。

### 6.2 `m/`：這台裝置的 crypto 狀態

**一個帳號在這台機器上就是一台裝置，`m/` 就是那台裝置**（裝置的身分金鑰、跟別人的 Olm 通道、收過的房間金鑰全在這裡）。
登出刪整個 `m/`：裝置在 server 上也登出了，留著沒有用，而且留著就是留著金鑰。

最終目標是只用 `matrix-sdk-crypto`（`/docs/design/overview/architecture-v2.md` §8），所以 `m/` 裡**除了上游 crypto store 本身，都是我們自己的格式**；
`matrix-sdk-state.sqlite3` 是一般 Matrix 帳號走 matrix-sdk `Client` 那條的 fallback 才有。

| 檔 | 誰的格式 | 內容 | 加密 | 誰寫 |
|---|---|---|---|---|
| `matrix-sdk-crypto.sqlite3`（＋ `-wal`、`-shm`） | 上游 `matrix-sdk-sqlite` | 見下 | 上游 StoreCipher：每個值 XChaCha20-Poly1305、鍵做雜湊；StoreCipher 自己被第二把子金鑰包住，存在 `kv` 表的 `cipher` 那一列。**檔案結構外露**（表名、列數），🚫 SQLCipher | wbf 帳號：`OlmEngine`；一般 Matrix 帳號：matrix-sdk `Client` |
| `matrix-sdk-state.sqlite3`（＋ `-wal`、`-shm`） | 上游 | 一般 Matrix 帳號的房間狀態（Client 那條的 state store）。**wbf 帳號沒有這個檔** | 同上 | 只有 matrix-sdk `Client` |
| `td.json` | 我們的 | §6.2.1 | 🚫 加密（只有數字，§9） | `OlmEngine::import_items` |

例：剛登入的 wbf 帳號，`m/` 裡只有一個 184320 byte 的 `matrix-sdk-crypto.sqlite3`。它的檔頭是明文的 SQLite 檔頭（對照 §5.1 的 `cache.db`）：

```
00000000: 5351 4c69 7465 2066 6f72 6d61 7420 3300  SQLite format 3.
```

上游 crypto store 裡的表（`vendor/matrix-rust-sdk/crates/matrix-sdk-sqlite/migrations/crypto_store/`）：

| 表 | 存什麼 |
|---|---|
| `kv` | 這台裝置的帳號本身（身分金鑰、一次性金鑰的 pickle）、StoreCipher、其他零散的狀態 |
| `session` | 跟別人每台裝置的 Olm 通道 |
| `inbound_group_session` | 收到的房間金鑰（解訊息用） |
| `outbound_group_session` | 自己在每個房的房間金鑰，**含排好、還沒送出去的 to-device**（後台全部送到、拿到 Ack 之前，這把金鑰🚫 拿來加密，/docs/design/keys/e2ee-rpc.md §3） |
| `device`、`identity`、`tracked_user` | 別人的裝置金鑰、交叉簽章身分、追蹤哪些人的裝置清單 |
| `olm_hash`、`key_requests`、`direct_withheld_info`、`secrets_inbox`、`room_settings`、`lease_locks`、`received_room_key_bundle`、`room_key_backups_fully_downloaded`、`rooms_pending_key_bundle` | 去重、金鑰請求、withheld、秘密、房間設定、跨 process 鎖、歷史金鑰包 |

#### 6.2.1 `m/td.json`：to-device 的游標

示意（一行 JSON，`wbf_sdk::to_device_state::ToDeviceState`）：

```json
{"cd_seq":812,"to_destroy":[810,811]}
```

- `cd_seq`：最後匯入 crypto store 的那則在 server 佇列裡的號；還沒處理過任何一則時這個欄位**不在**（不是 0）。
- `to_destroy`：已經落地、還沒請 server 銷毀的號（`/docs/design/keys/to-device-client.md` §2.1）。上面那個例子是「810、811 已經匯進來了，下次連上要先叫 server 刪掉它們」；銷完就是 `{"cd_seq":812,"to_destroy":[]}`。
- 寫法：`td.json.tmp` 再 rename，🚫 fsync、🚫 0600（只有數字）。
- 跟 crypto store 同生共死：`m/` 沒了它也沒了，重新從佇列頭拉。

#### 6.2.2 金鑰線的後台🚫 存檔

`key_share.rs`（建、換、送房間金鑰）只有記憶體裡的「哪個房對哪個房間版本號就緒」，🚫 寫任何檔。
理由在 `/docs/design/keys/e2ee-rpc.md` §3：送訊息只用排過的 to-device **全部拿到 Ack** 的金鑰，沒拿到 Ack 的那份從來沒被拿來加密過，daemon 重開丟了它也沒有訊息解不開；
待送的 to-device 本身在上游 `outbound_group_session` 那一列上，重開之後下一次 refresh 或送出會再交給後台。

📎 2026-10-06 加過一個 `m/ks.sealed`（記「哪些房還沒送完」）、同一天改了設計拿掉，沒有發布過。看到這個檔就是那天的開發版留下的，可以刪。

### 6.3 誰刪 `m/`

- 登出（先收金鑰線的 task、丟掉引擎，再整個 `remove_dir_all`，Windows 上重試 10 次）。
- 登入時沒有 session 卻有 `m/`（上次清到一半）：先刪再建。
- 登入失敗的回滾。

### 6.4 `k/snapshot`：本地房間金鑰快照

| | |
|---|---|
| 內容 | 上游 `export_room_keys` 的全量匯出（Element 的 `MEGOLM SESSION DATA` 格式） |
| 格式 | 上游的（PBKDF2 50 萬輪＋AES），passphrase 是第五把子金鑰的 base64；我們🚫 再包一層 |
| 寫法 | `k/` 0700 → 匯出到 `snapshot.tmp` → 0600 → rename |
| 誰寫 | `key-backup save`、上傳到 server 備份之後（`LOCAL_ROOM_KEYS` 開著時） |
| 誰刪 | 登出（`/docs/design/keys/room-key-backup.md` §7 的閘門過了之後）、destroy |

示意（文字檔，中間是一大段 base64）：

```
-----BEGIN MEGOLM SESSION DATA-----
AXr0b0Wq2rT…（base64，每行固定長度）
-----END MEGOLM SESSION DATA-----
```

⚠️ 跟 `r/` 一樣，目前只有一般 Matrix 帳號會有：wbf 帳號的 `key-backup save` 也要 matrix-sdk 的 Client。

## 7. 寫在資料目錄外面的檔

路徑都是使用者給的。

| 檔 | 例 | 內容 | 加密 | 誰刪 |
|---|---|---|---|---|
| `<檔>.wbf-upload.json` | `photo.bin.wbf-upload.json`（在 `photo.bin` 旁邊） | 續傳的狀態，**含檔案的鑰**（見下） | 🚫（§9、§10） | Seal 之後、`abort_upload` |
| manifest | `--manifest photo.manifest.json`、`files --save out/` 的 `out/<event_id>.json` | `{server, mxc, block}`，**含檔案的鑰**（見下） | 🚫（使用者要的輸出） | 程式🚫 刪 |
| 匯出的檔 | `photo.out`，寫的途中是 `photo.out.partial.30368-1` | 解密後的檔；先寫 partial、驗大小與雜湊、再 rename | 🚫（使用者要的就是明文） | 失敗時刪自己的 partial；當掉留下的不清（`/docs/design/media/media-download.md` §8） |
| `download --no-cache` 的輸出 | | 解密後的檔，直接寫、🚫 暫存檔 | 🚫 | 失敗時刪 |

manifest 的例子（`wbf-cli upload photo.bin --manifest photo.manifest.json` 寫的；`block` 逐字就是事件裡的 `org.wbftw.wbfuwunel.chunked`）：

```json
{
  "server": "http://127.0.0.1:6167",
  "mxc": "mxc://localhost/a96c92ef6fa51d",
  "block": {
    "v": 1,
    "cipher": "aes-256-gcm",
    "key": "<base64 的 32 byte 檔案金鑰>",
    "nonce_base": "FPX4bQM0Fyc=",
    "chunk_size": 65536,
    "file_size": 3000,
    "name": "photo.bin"
  }
}
```

上傳的狀態檔是同一個 `block` 再加上傳本身的進度（`wbf_sdk::manifest::UploadState`），示意：

```json
{
  "server": "http://127.0.0.1:6167",
  "user_id": "@alice:localhost",
  "upload_id": 17,
  "mxc": "mxc://localhost/a96c92ef6fa51d",
  "chunk_max_bytes": 1048576,
  "block": { "v": 1, "cipher": "aes-256-gcm", "key": "<base64>", "nonce_base": "…", "chunk_size": 65536, "file_size": 3000, "name": "photo.bin" }
}
```

`cipher` 有三種：`"aes-256-gcm"`、`"chacha20-poly1305"`、`"none"`（明文房的檔）；`"none"` 的時候 `key` 與 `nonce_base` 兩個欄位**不在**（不是空字串）。

沒有 log 檔：所有診斷都到 stderr。

## 8. 誰刪什麼

| | 登出（`logout`／`account del`） | destroy |
|---|---|---|
| `session.sealed` | 刪（server 那邊登出成功之後） | 刪 |
| `m/`（含 `td.json`） | 刪 | 刪 |
| `k/` | 刪（閘門過了之後） | 刪 |
| 帳號目錄本身 | **留**（空的） | 刪 |
| `r/<這個帳號>` | **留** | 刪 |
| `current` | 指著這個帳號就刪 | 同左 |
| `cache.db` | 這個 server 最後一個登入中的帳號才刪 | 刪這個帳號的列；最後一個帳號連目錄一起刪 |
| `media/` | **留**（等 `sweep`） | 刪這個帳號獨有的；最後一個帳號連目錄一起刪 |

例：同一個 server 上登入了 alice 與 bob，alice 登出 → alice 的 `session.sealed`、`m/`、`k/` 沒了，帳號目錄剩空殼；`cache.db` 與 `media/` 還在（bob 還在用）。
接著 bob 也登出 → `cache.db` 也刪了，`media/` 還在，等下一次 `sweep`。

## 9. 明文落地的東西，以及為什麼可以

| 檔 | 明文的是什麼 | 為什麼可以 |
|---|---|---|
| `local.key`（plain 模式） | 主金鑰 | 使用者選了不設 passphrase；要保護就設（`/docs/design/storage/vault-and-keys.md` §1） |
| `daemon.token` | 本地介面的金鑰材料 | 它就是那把鑰；前端負責 0600 與粉碎 |
| `daemon.json` | port、pid | 沒有秘密 |
| `wbf.conf` | 設定 | 拒收秘密 |
| `m/td.json` | 兩種序號 | 只有數字，沒有身分 |
| 池檔的檔名 | 內容的 BLAKE3 | 得先有那個檔才算得出來 |
| `m/*.sqlite3` 的結構 | 表名、列數 | 值都加密了；整檔加密要 fork 上游（`/docs/design/storage/local-cache-db.md` §4.5） |
| `<檔>.wbf-upload.json`、manifest | 檔案的鑰 | 見 §10 |

## 10. 已知的缺口

- **最後一個帳號登出，`media/` 留著**：`cache.db` 刪了，池檔要等之後的 `sweep` 才清。在那之前檔還在（加密的，鑰在主金鑰裡）。
- **`<檔>.wbf-upload.json` 放在被上傳的檔旁邊、明文含檔案的鑰**：該不該搬進資料目錄、要不要封起來，是 `/docs/design/overview/architecture-v2.md` §7 第 9 項還沒定的。
- **權限**：`cache.db`、池檔、`m/*.sqlite3`、`td.json`、三個鎖檔都是 umask，不是 0600。內容要嘛整檔加密、要嘛每值加密、要嘛沒有秘密；權限不是它們的防線。
- **wbf 帳號沒有 `k/` 與 `r/`**：金鑰備份與 recovery key 還要 matrix-sdk 的 Client（§4.8、§6.4）。

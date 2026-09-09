# wbf-cli 規格：命令、參數、輸出、狀態檔

> 狀態：草案，2026-09-04，等維護者同意。對應 plan-v1 §6 第 2 步（不接 matrix-sdk）與第 3 步（接）。
> 本文定的是**介面**；線上行為照 wbfuwunel 的線上規格，加解密照
> [wbf-client-convention-for-chunk.md](wbf-client-convention-for-chunk.md)（以下稱「約定」）。

## 0. 一句話

`wbf-cli` 是 `wbf-sdk` 的第一個使用者，也是對真 server 驗收的工具。每個命令做一件事，結果印 JSON 到 stdout，進度與錯誤印到 stderr，
exit code 說明失敗類別；所以它能被腳本串起來，驗收腳本就是這樣寫的。

## 1. 權限從哪來

所有 `/_wbf/v1/*` 請求都要 `Authorization: Bearer <access_token>`。token 是 Matrix 登入發的，拿它**不需要 matrix-sdk**：
一個 `POST /_matrix/client/v3/login`（`m.login.password`）就有。所以 `login` 從第 2 步就存在，用純 HTTP 做。

| 步驟 | `login` 做什麼 | 拿不到什麼 |
|---|---|---|
| 第 2 步 | HTTP 登入、拿 `access_token`、`user_id`、`device_id`，存進 session 檔 | 沒有裝置金鑰、沒有 E2EE。能上傳下載，不能進加密房間送事件 |
| 第 3 步 | 換成 matrix-sdk 的登入，同一個 session 檔多存 SDK 的 store 位置 | —— |

也接受外面給的 token（`--token` 或環境變數），給 Element 那邊登入過的人與腳本用。

## 2. 全域參數

| 參數 | 環境變數 | 說明 |
|---|---|---|
| `--server <url>` | `WBF_SERVER` | homeserver 的 base URL，example: `http://localhost:6167`。沒給就用 session 檔的 |
| `--token <access_token>` | `WBF_ACCESS_TOKEN` | 直接給 token，跳過 session 檔。🚫 不印、不寫進任何輸出 |
| `--data-dir <dir>` | `WBF_DATA_DIR` | 資料目錄（`local.key`、`wbf.conf`、`current`、`servers/<b58>_<b58>/cache.db`、`…/accounts/<b58>_<b58>/`），預設見 §7 |
| `--account <mxid 或 localpart>` | `WBF_ACCOUNT` | 用哪個帳號，**就這一次**（要改預設用 `account switch`，§3.1）；沒給就是 `current`。同名 localpart 在多個 server 都有時要配 `--server`（§7） |
| `--passphrase-file <path>` | `WBF_PASSPHRASE_FILE` | 整檔的**原始 bytes** 就是 passphrase（解 `local.key` 用）。🚫 不去尾換行、🚫 不驗 UTF-8：可以是中文、可以是一個 mp3（local-cache-db.md §12）。沒給就看 unlock ticket，再沒有就從終端讀（不回顯）。🚫 沒有 `--passphrase <pw>`、🚫 不接受環境變數給 passphrase 本身 |
| `--unlock-ttl <秒>` | | 密碼解鎖成功後 unlock ticket 的有效期，預設 900；0 就不寫 ticket（§7.1） |
| `--config <path>` | `WBF_CONFIG` | conf 檔的位置（§10）。沒給就看 `<data dir>/wbf.conf`；明指了卻找不到就報錯，🚫 不默默 fallback |
| `--json` | | stdout 只印 JSON（預設就是；留這個旗標是為了之後加人類可讀模式時介面不變） |
| `--quiet` | | stderr 不印進度 |
| `--transport ws\|http` | | 預設 `ws`；`http` 走 `POST /_wbf/v1/pack` 一請求一包，給除錯與 server 端測試用 |

## 3. 命令

### 3.1 帳號：`account` 一族（維護者 2026-09-09 定）

**多帳號是前提，不是附加功能**（維護者 2026-09-09）：CLI 與 UI 都要能同時登入多個帳號，甚至同時跑起來。
所以帳號目錄一帳號一套（§7），`matrix/`（裝置狀態）也一帳號一套 —— 這正是拿帳號當 `matrix/` 分界的理由。
`current` 只回答一個問題：**沒帶 `--account` 時用誰**。

| 命令 | 做什麼 | stdout |
|---|---|---|
| `login --user <mxid> [--password-file <path>] [--device-name <name>]`<br>`account add …`（同一件事的另一個名字） | 登入、把 session 封進這個帳號目錄的 `session.sealed`，**登入成功自動切成 `current`** 並印一行 switch 提示（§3.1.1）。資料目錄裡沒有 `local.key` 就建一把：給了 `--passphrase-file` 就是 `passphrase` 模式，否則 `plain`。多個帳號可以同時登入著。`--password-file` 整檔就是密碼（去掉結尾一個換行）；沒給就從終端讀（不回顯）。🚫 沒有 `--password <pw>`、🚫 不接受環境變數給密碼：兩者都會留在 shell 歷史與 `ps` 輸出裡 | `{ "user_id", "device_id", "server", "switched_from" }` |
| `account status` | 列本機所有帳號：**掃雙層**（`servers/` 再 `accounts/`）逐一解密目錄名（local-cache-db.md §11.5）。⚠️ **要解鎖**（`passphrase` 模式會問或吃 ticket），因為目錄名是加密的；這跟 2026-09-09 之前的「不開 vault」不一樣。解不開的目錄跳過並警告，🚫 不猜不刪。哪個是 `current`、各自登入了沒。**`user_id` 是完整 mxid**，拿來就能直接餵給 `account switch`／`del`／`destroy`。⚠️ **登出的帳號是 `null`**：目錄名只解得出 localpart 與 host，組不出可靠的 mxid，🚫 不自己拼一個 | `[{ "user_id", "server", "localpart", "logged_in", "current" }…]` |
| `account switch <user>` | 只改 `current`，不連 server。印 switch 提示（§3.1.1）。指到沒登入的帳號會警告但照切（下一個要連線的命令才會失敗） | `{ "ok": true, "current", "switched_from" }` |
| `logout [--accept-history-loss]`<br>`account del <user> [--accept-history-loss]` | **裝置層**：`POST /_matrix/client/v3/logout` 讓 token 失效，刪這個帳號的 `session.sealed`、`matrix/`、**`room-keys/`**（維護者 2026-09-09：離開這台機器就清乾淨，local-cache-db §10.7）與 unlock ticket；`current` 指到它就清掉。**`cache.db` 裡的紀錄留著**（之後再登入還在），`local.key` 也留著。例外：這個 server 最後一個帳號登出時，`cache.db` 一起刪（沒有主人了）。`logout` 就是 `account del <current 帳號>`。`matrix/` 不能留：Matrix 的 logout 讓裝置失效，下次 `login` 是新裝置，舊 crypto store 會擋登入（2026-09-07 實跑踩到，PR #11 那版寫錯了）。**閘門兩關**（local-cache-db §10.7）：① server 上有 backup ＆ `recovery().state() == Enabled`；② 這台機器保管著這個帳號的 recovery key（§3.6.1）。⚠️ 第 ① 關只說得出「SSSS 設好了」，說不出那串字在誰手上——第 ② 關才是真的。任一關不過就 exit 1，要 `--accept-history-loss` 才走。🚫 不問使用者手打 recovery key（我們自己就保管著） | `{ "ok": true, "user" }` |
| `account destroy <user> [--yes] [--accept-history-loss]` | **裝置層加資料層**：先做 `account del <user>` 那一整套，再跑忘掉鏈（§3.5）把這個帳號在 `cache.db` 裡**獨有**的東西清掉 —— 只有他同步過的事件、只有那些事件指的媒體、沒人再認領的池檔、沒事件也沒清單的房間。**別的帳號也持有的一律不動**（維護者 2026-09-09 的原話：扣除別人帳號的持有）。沒 `--yes` 就終端確認，提示要講明會刪掉什麼 | `{ "ok", "user", "events_removed", "media_removed", "pool_files_removed" }` |
| `whoami` | `GET /_matrix/client/v3/account/whoami` | `{ "user_id", "device_id" }` |
| `lock` | 刪 unlock ticket；下一個命令會再問 passphrase | `{ "ok": true, "had_ticket": bool }` |
| `set-passphrase [--new-passphrase-file <path>]` | 給 `local.key` 設或改 passphrase（沒給檔就從終端讀兩次）。只重包主金鑰，`session.sealed` 與 `matrix/` 不動；舊 ticket 作廢 | `{ "ok": true, "mode": "passphrase" }` |
| `remove-passphrase` | 拿掉 passphrase，`local.key` 回到 `plain` | `{ "ok": true, "mode": "plain" }` |

⚠️ 舊名 `accounts` 與 `forget-account` **移除**（維護者 2026-09-09）：碰到就報錯並指向新名字，不留別名。

🔑 **`<user>` 一律是完整 mxid**（維護者 2026-09-09）：`@bob:matrix.org`，🚫 不省略 server name。
`account switch`／`del`／`destroy` 的位置參數都一樣 —— 這些命令會登出、會刪檔，變更的對象不該靠猜。
只給 localpart 就報錯並列出本機的帳號（🚫 不推測、不拿唯一一個頂替）：

```
error: expected a full Matrix ID like @bob:matrix.org, got "bob"
       accounts on this machine: @alice:matrix.org, @bob:matrix.org, @bob:localhost
```

全域的 `--account`（§2）仍然收 localpart 簡寫：它只決定這一次命令用誰，猜錯了最多是讀錯人的快取，而且歧義時本來就要配 `--server`（§7）。

#### 3.1.1 切換帳號的提示訊息

切換帳號時（`login`／`account add` 成功之後，以及 `account switch`）印到 stderr（英文，見 §4）：

```
switched to @alice:localhost on http://localhost:6167 (was @bob:localhost)
```

本來就沒有 `current`（第一次登入）時：

```
switched to @alice:localhost on http://localhost:6167 (no previous account)
```

`account switch` 指到一個沒登入的帳號：

```
warning: @bob:localhost is not logged in; commands that need the server will fail until you run `login --user @bob:localhost`
switched to @bob:localhost on http://localhost:6167 (was @alice:localhost)
```

#### 3.1.2 一輪多帳號長什麼樣

```bash
# 兩個帳號共用一個資料目錄（一把 local.key、一份 cache.db）
wbf-cli --data-dir ~/.wbf login --user @alice:matrix.org --password-file pw-alice
# stderr: switched to @alice:matrix.org on https://matrix.org (no previous account)

wbf-cli --data-dir ~/.wbf login --user @bob:matrix.org --password-file pw-bob
# stderr: switched to @bob:matrix.org on https://matrix.org (was @alice:matrix.org)

wbf-cli --data-dir ~/.wbf account status
# [{"user_id":"@alice:matrix.org","server":"https://matrix.org","localpart":"alice","logged_in":true,"current":false},
#  {"user_id":"@bob:matrix.org","server":"https://matrix.org","localpart":"bob","logged_in":true,"current":true}]

# 換預設帳號：完整 mxid，不是 "alice"
wbf-cli --data-dir ~/.wbf account switch @alice:matrix.org
# stderr: switched to @alice:matrix.org on https://matrix.org (was @bob:matrix.org)
# stdout: {"ok":true,"current":"@alice:matrix.org","switched_from":"@bob:matrix.org"}

# 只有這一條用 bob，current 不動
wbf-cli --data-dir ~/.wbf --account @bob:matrix.org rooms

# 登出 bob：session、matrix/、room-keys/ 沒了，cache.db 裡的紀錄還在
wbf-cli --data-dir ~/.wbf account del @bob:matrix.org

# 連 bob 在本機的紀錄一起清掉（alice 也看得到的那些不動）
wbf-cli --data-dir ~/.wbf account destroy @bob:matrix.org --yes
```

同一個 localpart 在兩個 server 上是**兩個帳號**，完整 mxid 才分得開：

```bash
wbf-cli --data-dir ~/.wbf account switch @bob:matrix.org   # 這個
wbf-cli --data-dir ~/.wbf account switch @bob:localhost    # 跟這個不是同一個人
```

### 3.2 上傳

| 命令 | 做什麼 |
|---|---|
| `upload <file> [--cipher chacha20-poly1305\|aes-256-gcm\|none] [--chunk-size <bytes>] [--manifest <out.json>] [--sha256]` | 固定大小上傳：Create → 逐塊 → Seal。印 manifest（§5） |
| `upload --stream [--cipher …] [--link mobile\|wifi] [--chunk-size <bytes>] [--name <n>] [--mimetype <m>] [--manifest <out.json>]` | 從 stdin 讀、`0/0` 哨兵、最後一塊 `IS_LAST`、Seal 帶最終描述。`--link` 決定串流的 `chunk_size`（約定 §2），預設 `mobile` |
| `status <upload_id>` | 印 `Status` 的 Ack |
| `abort <upload_id> [--file <path>]` | 送 `Abort`；給 `--file` 就順便刪它旁邊的狀態檔（狀態檔跟著檔案放，只有 id 找不到它） |

- `--cipher` 預設：偵測到硬體 AES 用 `aes-256-gcm`，否則 `chacha20-poly1305`（約定 §3）。`none` 是明文模式；第 2 步沒有房間，所以要不要警告是第 3 步 `send` 的事，這裡直接照做。
- `--chunk-size` 沒給就照約定 §2 的表選；給了就照給的（要在 server 允許的範圍，不然 Create 會被拒）。
- `--sha256`：上傳時順便算整檔明文雜湊寫進描述與 manifest。串流模式一律算（反正要讀過一遍）。
- **續傳**：`upload` 開始前在檔案旁寫狀態檔（§6）。同一個 `upload <file>` 再跑一次，看到狀態檔就先 `Status`，從 `received` 接著送，用**同一把** key 與 nonce_base（同一個上傳，不是重傳）。Seal 成功後刪狀態檔。
  狀態檔的 server 與 user 跟現在的不一樣 → 拒絕，不會拿 A server 的上傳去打 B server。

### 3.3 下載

| 命令 | 做什麼 |
|---|---|
| `info <mxc> [--manifest <m.json>]` | 印 `Info` 的 Ack（server 知道的欄位）。有 manifest 就順便解描述印出來，並做約定 §3.1 第 2 條的核對 |
| `download --manifest <m.json> [-o <out>] [--no-cache]` | `Info` → 逐塊 `Read` → 解密 → 寫檔。全部檢查照約定 §3.1，任一不過刪掉半成品、exit 3。沒給 `-o` 用描述的 `name`，沒有就 `download.bin`。**登入中預設走媒體快取**（§3.5）：池裡有完整檔（長度與校驗碼都對）就不連 server（stdout `source: cache`、`sha256_verified: false`、`hash` 是快取記的校驗碼）；沒有就邊下邊進池、可續傳，再從池複製到 `-o`。快取路徑上一塊驗不過仍 exit 3，但**池裡的半成品留著給下次續**（local-cache-db.md §8.3），`-o` 不會產生。`--no-cache` 或 `--token` 模式直接寫檔不進池 |
| `seek --manifest <m.json> --at <pos> [--len <n>]` | 只 `Read` 含 `pos` 的那一塊（`--len` 跨塊就多讀），解密後把 `pos` 起的明文寫到 stdout。這是驗收「不必下載前面」的命令 |

下載的參數都從 manifest 來，不提供 `--key` 這種零散參數：金鑰不該出現在命令列與 shell 歷史裡。

- manifest 的 `server` 與 session 的不同 → exit 1，不拿 A server 的 manifest 去打 B server（與上傳狀態檔的規則一致）。
- `download` 沒給 `-o` 時用描述的 `name`；它是對方寫的，帶路徑分隔符或是 `.`／`..` 就要求明給 `-o`（exit 1），不寫到意料外的位置。
- `--token` 模式會先打一次 `whoami` 填真的 user_id，狀態檔的 server／user 核對才有意義。

#### 3.3.1 `seek` 的語意：位置是明文位置，讀的單位是塊

`--at` 與 `--len` 都是**明文**的 byte 位置與長度，跟 chunk 邊界無關；對齊到塊是 `seek` 自己的事。以 `chunk_size` 64 KiB 為例：

| 命令 | 要的明文範圍 | 實際 `Read` 的塊 | 對塊做的事 |
|---|---|---|---|
| `seek --at 70K` | 70K 起，**到那一塊結尾**（70K–128K，共 58K） | 第 1 塊（64K–128K） | 開頭跳過 6K |
| `seek --at 70K --len 70K` | 70K–140K | 第 1 塊、第 2 塊（128K–192K） | 第 1 塊開頭跳過 6K，第 2 塊只留前 12K |
| `seek --at 64K --len 64K` | 64K–128K | 第 1 塊 | 剛好一塊，不裁 |

- **不帶 `--len`**：讀含 `pos` 的那一塊，印 `pos` 起到該塊結尾。所以長度**不是**固定一個 `chunk_size`，`pos` 在塊中間就比一塊短。要固定長度就給 `--len`。
- **`--len` 跨幾塊就讀幾塊**，第一塊裁頭、最後一塊裁尾，中間的整塊照印。每塊都照約定 §3.1 各自解密驗證，任一塊壞掉就 exit 3，stdout 已經印出去的不收回（呼叫者看 exit code 決定要不要丟掉）。
- **`--len` 超過檔尾**：印到檔尾為止，exit 0，摘要裡 `truncated: true`。這不是錯誤，`tail` 類的用法本來就會這樣要。
- **`--at` 不小於明文總長**：exit 1（用法錯），什麼都不印。
- 明文總長從 manifest 的描述來；`Info` 回的塊數與最後一塊長度要跟它對得上（約定 §3.1 第 2 條），對不上 exit 3。

**返回**：stdout 是明文 bytes，沒有別的。摘要印在 stderr 最後一行，一個 JSON，`--quiet` 也印（它是結果，不是進度）：

```json
{ "at": 71680, "len": 71680, "bytes": 71680, "chunks_read": [1, 2], "truncated": false }
```

`chunks_read` 就是驗收「不必下載前面」的證據：`--at 150000000` 時它必須只有一個元素。

### 3.4 房間（第 3 步，2026-09-06 做了第一版）

從第 3 步起 `login` 走 matrix-sdk（拿到有裝置金鑰的 session，E2EE 房間才解得開），store 放資料目錄的 `matrix/`（§7）。
這些命令都先做一次增量 sync（timeout 0）再動作，所以看到的是現況。

| 命令 | 做什麼 |
|---|---|
| `rooms` | 列出加入的房間：chat-model §2.1 的 `Conversation` 陣列（`id`、`kind`、`name`、`topic`、`encrypted`、`member_count`、`my_power_level`、`can_send_message`、`direct_peer`） |
| `send <room_id> --text <msg>` | 送文字；印 `{ "event_id" }` |
| `send <room_id> --file <file> [--caption <c>] [--cipher …] [--chunk-size …] [--sha256] [--manifest <out>] [--yes]` | upload（含續傳）後把約定 §5 的事件送進房間；印 `{ "event_id", "mxc" }`。房間沒 E2EE：**送之前印警告並要求確認**（約定 §5.1）、強制 `cipher: none`（給別的 `--cipher` 就 exit 1：加密區塊的 key 會公開）；`--yes` 跳過確認給腳本用。⚠️ 附件宣告（約定 §5.2）這一版帶不出去，stderr 會印警告 |
| `watch <room_id> tail \| wait <秒> \| once [--since <token>]` | 從 `/sync` 等**新**事件（現在起），來一個立刻印一個，一行一個 JSON。三種模式見 §3.4.2。認得 `org.wbftw.wbfuwunel.file` 就把區塊解出來當 manifest 印 |
| `ping` | `Hello` 加 `Ping`，印 server 回的 features 與上限。除錯用，第 2 步就有 |

#### 3.4.1 讀房間

`watch` 只看得到現在起的新事件。讀歷史、找檔案、看房間本身，是另外三個問題，各一個命令（`read`、`files` 第 3 步做了；`room` 還沒，`rooms` 的輸出已經有它大部分的欄位）：

| 命令 | 做什麼 | stdout |
|---|---|---|
| `room <room_id>`（還沒） | 房間本身：`GET .../rooms/{id}/state` 挑出來的欄位 | `{ "room_id", "name", "topic", "encrypted": bool, "member_count", "joined_members": [mxid…] }` |
| `read <room_id> [--limit <n>] [--before <token>] [--type <名>…] [--sender <mxid>]` | 歷史：`GET .../rooms/{id}/messages?dir=b`，從最新往回。`--limit` 預設 50；`--before` 接上一頁印的 `next`，再往前翻。`--type`／`--sender` 是 client 端過濾，翻頁的 token 不受影響。`--type` 對的是模型的 `kind`（`text`、`file`、`deleted`、`system`、`unsupported`）或原始 event type | `{ "events": [事件…], "next": token \| null }`，`next` 是 null 表示到頭了 |
| `files <room_id> [--limit <n>] [--before <token>] [--save <dir>]` | `read` 只留 `org.wbftw.wbfuwunel.file`，把區塊解成 manifest（§5）印出來；`--save` 一個事件存一個 `<event_id>.json`，之後直接 `download --manifest` | `{ "files": [{ "event_id", "sender", "ts", "manifest" }…], "next" }` |

事件的統一形狀（`read`、`watch`、`files` 都用）就是 chat-model §2.3 的 `Message` 序列化：

| 欄位 | 說明 |
|---|---|
| `id`、`conversation`、`sender`、`sent_at` | event_id、room_id、mxid、`origin_server_ts`（毫秒，只當顯示用） |
| `kind` 加它的欄位 | `text`（`body`、`formatted_html`）、`file`（`attachment` = `{ mxc, block }`、`caption`）、`deleted`（`reason`）、`system`（`event_type`、`line`）、`unsupported`（`event_type`、`body`）。認不得的事件不丟 |
| `reply_to`、`edited_by`、`reactions` | 同一頁內的關係事件折進目標（chat-model §3.4） |
| `decrypted` | `true`／`false`／`null`。`null` 表示本來就不是加密事件 |
| `undecryptable_reason` | `decrypted` 是 `false` 才有，matrix-sdk 給的原因（example: `MissingMegolmSession`） |
| `r_seq`、`g_seq` | server 發的序號（chat-model §4.3）；非 fork server 的房間沒有 |

規則：

- **解不開不是錯誤。** 加密房間裡拿不到 key 的事件照印，`decrypted: false` 帶原因，exit 仍是 0。用 exit 3 只會讓一整頁因為一則舊訊息全掛。
- **翻頁 token 不落地。** `next` 只印在 stdout，不寫狀態檔；要接著翻是呼叫者的事。session 檔的 SDK store 另有它自己的 sync 位置，跟這個無關。
- **`--type`、`--sender` 在 client 端濾**：Matrix 的 `filter` 參數各 server 支援程度不一，而且只是省流量，結果一樣。過濾後一頁可能是空的但 `next` 不是 null，呼叫者要照 `next` 判斷有沒有到頭，不是照 `events` 長度。
- **`files` 不驗完整性**：它只解區塊、印 manifest，不碰 `Info`。核對是 `info`／`download` 的事（約定 §3.1）。
- 不加 `search`：server 端全文搜尋對加密房間無效，要做也是 client 端掃 `read` 的輸出，那是腳本一行 `jq` 的事。

#### 3.4.2 `watch`：等新事件，有就立刻印

同一個命令，差別只在**什麼時候結束**：

| 模式 | 什麼時候結束 | 給誰用 |
|---|---|---|
| `tail` | 不結束，到 Ctrl-C 或斷線用完續傳次數（exit 4） | 人看、或管線接到另一個程式後面 |
| `wait <秒>` | 時間到就結束，這段時間內進來的都印過了；一則都沒有也是 exit 0 | 腳本：「送完之後等 5 秒看對方回什麼」 |
| `once` | 印到**第一則**（別人的）就結束；`--timeout <秒>` 到了還沒有 exit 5。不帶 `--timeout` 就等到有為止，沒有上限 | 腳本：「阻塞到有回應為止」 |

規則：

- **印是即時的，不是結束才印。** 三種模式都是事件一到就寫一行 JSON 進 stdout 並 flush，所以 `wait 5` 接管線不會等到第 5 秒才一次吐出來。這是 §4「一個 JSON 物件」的第二個例外（第一個是 `seek`）：`watch` 印 **JSON Lines**。
- **結束時 stderr 印一行 `since`**，下次用 `--since` 接著等，中間進來的不漏。這也是 `wait` 連續呼叫的接法；`since` 自己不落地，理由同 §3.4.1 的 `next`。
- **只印這個房間的。** `/sync` 回來的是全部房間，其他房間的丟掉；要看全部就開幾個 `watch`。
- **自己送的也印**（`sender` 是自己），腳本自己濾。`once` 不把自己的算進「第一則」，不然 `send` 完接著 `once` 永遠等到的是自己。
- **第 2 步的 session 在加密房間照樣能用**，只是每則都 `decrypted: false`；`once` 還是算有回應。

### 3.5 本地快取（local-cache-db.md §6，2026-09-07）

`cache.db` 在 `servers/<host>/`（§7），**同一個 server 上的所有帳號共用一份**，SQLCipher 整檔加密，金鑰是 vault 的第一把子金鑰。**快取不是權威**：server 不符、schema 版本不對、解不開，開檔時直接刪掉重建，stderr 說一聲。

多帳號混存怎麼不漏（維護者 2026-09-07 定，細節在 local-cache-db.md §6）：

- 事件只存一份；**誰看得到哪一則逐則記**（`events_synced_log`）：server 經 `/messages`、`/sync`、`Recent` 任一條路給過這個帳號的才算。沒有列就看不到，fail closed。不用「每人一個 r_seq 下界」——離開再加入、`history_visibility` 改過都會切洞，下界會 fail open。
- 所以 user1 與 user2 都在 room1：各自 `recent` 或 `read` 過的事件各自看得到；一則兩人都拿過只存一份。**一人解過的明文另一人也讀得到明文**（都是同一台機器上同一個人的帳號，維護者接受）：bob 的裝置沒有 Megolm 金鑰，`read --from-cache` 仍看到 alice 解過的內容。
- 房間清單、`cg_seq` 水位線、已讀位置、Delete-for-me 都是一人一份。

| 命令 | 做什麼 | stdout |
|---|---|---|
| `recent [--limit <n>] [--window <n>] [--batch <n>] [--from-scratch]` | `Event/Recent`（只走 WS；`--transport http` 會拿到 `Unsupported`）。三層（維護者 2026-09-08 定）：`--limit` 是**這一輪總共要幾則**（預設 10000，0 = 拉到追平），底層拆成一次 `Recent` 一窗 `--window` 則（預設 320、server 上限 500 先 clamp），server 每 `--batch` 則回一個 Batch（預設 10、上限 100）；要 1000 就是 320、320、320、40 四窗。每個 Batch 寫一次快取；一窗 `tc == 要的` 就帶 `before = 最後的 ls` 再一窗，`tc < 要的` 是追平（`caught_up`）；湊滿 `--limit` 也停（stderr 說更舊的還沒進快取）。水位一律是第一窗第一個 Batch 的 `fs`（比它新的全拿到了），中途斷線或 server 回錯就 exit、已寫的有效、水位不動。等待：第一窗每個 Batch 之間 60 秒、之後 10 秒。`--from-scratch` 不帶 `cg_seq`。server 要有 `recent` feature | `{ "pulled", "written", "windows", "batches", "caught_up", "cg_seq_before", "cg_seq_after" }` |
| `read … --from-cache`、`files … --from-cache` | 不連 server，從快取讀這個帳號同步過的。排序照 `r_seq`（沒有 `r_seq` 的房間退到時間）。`--before` 這時是 **r_seq 的數字**（上一頁印的 `next`），不是 server 的翻頁 token；沒有 `r_seq` 的房間 `next` 是 null、翻不了頁 | 與不帶時同形 |
| `account destroy <user> [--yes]` | 忘掉鏈（裝置層那一半在 §3.1）：刪這個帳號的同步紀錄／房間清單／水位線／已讀 → 沒人同步過的事件 → 沒事件指的媒體 → 沒事件也沒清單的房間。順手刪掉已經沒人用的池檔（DB 先、檔案後；刪不掉只說一聲，`media-gc` 的 sweep 會再收）。**這個帳號的 `room-keys/` 也一起刪**（它就是「摧毀本機紀錄」，跟 `logout` 一致，local-cache-db §10.7）；沒 `--yes` 的確認提示要把這件事講出來 | 見 §3.1 |
| `media-stats` | 媒體池的狀態：池目錄、`bytes_on_disk` 加總、完整檔數、半成品數、`pending/` 裡的檔數、最久沒用的時間 | `{ "pool_dir", "bytes_on_disk", "complete_files", "incomplete_files", "pending_on_disk", "oldest_last_used_at" }` |
| `media-gc [--quota-mib <n>] [--protect-days <d>]` | 先掃孤兒（DB 說有檔不在 → 當沒有；沒列認領的暫存檔 → 刪；過保護期的半成品 → 刪；`media/<hh>/` 裡沒任何列指著的完成檔 → 刪），再照 local-cache-db.md §8.5 清到配額以下：只刪保護期外的、最久沒用的先、同 hash 被多個 mxc 指著的檔不刪。預設 2048 MiB、7 天。保護期內全滿了不刪不擋，stderr 提示 | `{ "bytes_before", "bytes_after", "files_removed", "still_over_quota", "swept_missing_files", "swept_pending", "swept_orphan_files" }` |

寫穿：`rooms` 把房間列表、`read`／`files`／`watch` 把印過的事件順手寫進快取（帶著這個帳號的 mxid）。**寫穿失敗只在 stderr 說一聲，命令照樣成功**（快取壞了的代價是重拉）。`--token` 模式沒有 vault 也沒有帳號目錄，沒有快取：`--from-cache` 與 `recent` 會 exit 1。

### 3.6 房間金鑰備份（local-cache-db.md §10，2026-09-09 定，還沒實作）

房間金鑰有兩份備份：server 端的標準 Matrix key backup，與本地 `accounts/<localpart>/room-keys/` 的加密金鑰池。
兩個開關都在 conf 的 `[backup]`（§10），預設都是 `on`。

| 命令 | 做什麼 | stdout |
|---|---|---|
| `key-backup status` | server 上有沒有 backup、本機有沒有在上傳、有沒有 recovery key（只認 `RecoveryState::Enabled`）、本地快照存在嗎／多大／什麼時候存的。⚠️ 「幾把金鑰」印不出來：上游的匯出是不透明的全量檔（local-cache-db §10.4） | `{ "server_backup_exists", "uploading_locally", "has_recovery_key", "recovery_state", "local_snapshot", "local_snapshot_bytes", "local_snapshot_saved_at" }` |
| `key-backup upload` | 把 store 裡的金鑰推上 server，走上游的 `wait_for_steady_state()`，**傳完才 exit**（每個命令都等會太慢，所以獨立成一個命令，維護者 2026-09-09 定）。順手也跑一次 `save`（local-cache-db §10.5） | `{ "ok": true, "server_backup_exists", "has_recovery_key", "local_snapshot_bytes" }` |
| `key-backup save` | 把 crypto store 裡的**全部**房間金鑰倒進 `room-keys/snapshot`（全量覆蓋，先寫 `.tmp` 再 rename）。一輪 PBKDF2 500k 約半秒，所以是命令觸發的（local-cache-db §10.5） | `{ "ok": true, "bytes" }` |
| `key-backup import` | 把 `room-keys/snapshot` 餵回 crypto store（重新 `login`、或刪過 `matrix/` 之後用） | `{ "ok": true, "imported", "total" }` |
| `key-backup restore` | 用 `<data dir>/recovery/` 保管的那把 key 恢復**這台裝置**（解 SSSS、拿回 backup 的解密金鑰）。⚠️ **重新 `login` 之後一定要跑**：新裝置的 crypto store 沒有 SSSS 的 secrets，`RecoveryState` 會是 `Incomplete`，server 上那份備份解不開（2026-09-09 對真 server 驗證時發現的缺口） | `{ "ok": true, "recovery_enabled", "recovery_state" }` |
| `key-backup recovery` | 產生 recovery key（上游 `recovery().enable()`），**印一次**。⚠️ 印完拿不回來，只能 reset。🚫 不寫進任何檔、不進 conf、不進 log | `{ "recovery_key": "…" }`（唯一會印秘密的命令，而且只印這一次） |

#### 3.6.1 `recovery`：這台機器保管著誰的 recovery key

`key-backup recovery` 產生的那串 key 會封進 `<data dir>/recovery/`（local-cache-db §10.8），
**`logout` 不碰那個目錄**——它是清完帳號目錄之後唯一回得去 server 備份的路。

| 命令 | 做什麼 | stdout |
|---|---|---|
| `recovery list` | 列出保管著誰的（只解**檔名**，🚫 不解內容、不印金鑰） | `{ "users": ["@alice:localhost", …] }` |
| `recovery show <mxid>` | 印出某一個。⚠️ 會把秘密印到 stdout | `{ "user", "recovery_key" }` |

⚠️ `account destroy` **會連目標帳號的 recovery key 一起摧毀**（維護者 2026-09-09），
之後 server 上那份備份就永遠解不開了；`logout`／`account del` 🚫 不做這件事。

**警告什麼時候印**（stderr，維護者 2026-09-09：recovery key 延後但要有警告）：

- `login` 成功之後，如果 server backup 開著但還沒有 recovery key：

  ```
  warning: room keys are being uploaded to the server, but the key that decrypts that backup
           currently exists only on this machine. Another machine (or this one after a reinstall)
           will not be able to read your history, and `logout` will refuse to run until you fix this.
           Run `wbf-cli key-backup recovery` once to create a recovery key and keep it somewhere safe;
           the server-side backup then works on any device.
           Until then the only copy that can restore your history is local:
           <data dir>/servers/<host>/accounts/<localpart>/room-keys/
  ```

- `SERVER_BACKUP=off` 時，任何會拿到房間金鑰的命令印一次：

  ```
  warning: server-side room key backup is off ([backup] SERVER_BACKUP=off in wbf.conf).
           Your room keys stay on this machine only:
           <data dir>/servers/<host>/accounts/<localpart>/room-keys/
  ```

- `LOCAL_ROOM_KEYS=off` 且 `SERVER_BACKUP=off`：兩份都關，改印

  ```
  warning: both room key backups are disabled ([backup] in wbf.conf). If the crypto store is
           deleted or breaks, your history becomes unreadable — there is no copy anywhere.
  ```

- `logout` 的閘門（local-cache-db §10.7）擋下來時，exit 1 並印：

  ```
  error: this would delete the room keys on this machine (matrix/ and room-keys/), and the
         server-side backup cannot be decrypted yet — its key lives in the crypto store that is
         about to be deleted. Logging out like this makes @alice:localhost's history unreadable.
         Run `wbf-cli key-backup recovery` first to create a recovery key (printed once, keep it
         safe), then log out.
         If you do not want that history, pass --accept-history-loss.
  ```

  閘門寫成**正面認得**的形式：只有「server backup 開著 ＆ `recovery().state() == Enabled`」才放行，
  其他任何狀態（含 `SERVER_BACKUP=off`、`Unknown`／`Incomplete`、問不到 server）一律擋下來。

🚫 這些警告不印金鑰、不印 recovery key、不印 token。

## 4. 輸出與 exit code

🌐 **CLI 的輸出一律英文**（維護者 2026-09-09 定）：stdout 的 JSON、stderr 的進度、警告、錯誤、確認提示，全部英文。
設計文件與 commit 訊息是中文，面向使用者的字不是。這份規格裡引用的訊息都是原文，改了要兩邊一起改。

- stdout：**一個 JSON 物件**，命令成功才印。兩個例外：`seek` 印明文 bytes；`watch` 印 JSON Lines，一事件一行、即時 flush（§3.4.2）。
- stderr：進度（`chunk 17/2031 …`）、警告、錯誤訊息。
- exit code：

| code | 意思 |
|---|---|
| 0 | 成功 |
| 1 | 用法錯：參數、找不到檔、`local.key`／`session.sealed` 壞掉或解不開、passphrase 錯 |
| 2 | server 回 `Error` pack 或 HTTP 非 2xx；stderr 印 `code` 與 `message` |
| 3 | **完整性失敗**：CRC、AEAD 標籤、長度、sha256、事件與 `Info` 對不上。半成品已刪 |
| 4 | 網路：連不上、斷線且續傳次數用完 |
| 5 | 等逾時：`watch once --timeout` 到了還沒有事件 |

## 5. Manifest：`upload` 印的、`download` 吃的

就是約定 §5 事件區塊的 JSON，外加 `mxc` 與 `server`：

```json
{ "server": "http://localhost:6167", "mxc": "mxc://localhost/1122334455667788",
  "block": { "v": 1, "cipher": "chacha20-poly1305", "key": "…", "nonce_base": "…",
             "chunk_size": 65536, "file_size": 132056, "name": "video.mkv", "mimetype": "video/x-matroska", "sha256": "…" } }
```

- `block` 逐字就是第 3 步 `send --file` 放進事件 `org.wbftw.wbfuwunel.chunked` 的東西；第 2 步沒有房間，manifest 就是「事件」的替身。
- 含 `key`，所以 manifest 檔要當機密：寫檔時權限只給自己（Unix `0600`；Windows 放在使用者目錄）。`--manifest` 沒給就印到 stdout。

## 6. 上傳狀態檔（續傳用）

`<file>.wbf-upload.json`，放在被上傳的檔旁邊；串流模式沒有（stdin 沒得重來）。

```json
{ "server": "…", "user_id": "@a:localhost", "upload_id": 1234605616436508552, "mxc": "…", "chunk_max_bytes": 69632,
  "block": { "v": 1, "cipher": "…", "key": "…", "nonce_base": "…", "chunk_size": 65536, "file_size": 132056,
             "name": "video.mkv", "mimetype": "video/x-matroska" } }
```

- `block` 與 manifest 的 `block` 同一個形狀（SDK 的 `ChunkedBlock`），只是還沒有 `sha256`；Seal 時填上就變成 manifest 的 `block`。
  塊數不存：從 `file_size` 與 `chunk_size` 算得出來。這是 SDK 的 `UploadState`，`Create` 一回來就寫檔。
- 含 `key`，同 manifest 的保護。Seal 成功或 `abort` 後刪。
- 續傳時 SDK 對 server 的 `Status` 有兩種修正：狀態檔比 server 舊（重送已收的塊）server 回冪等 Ack、跳到 `received`；
  比 server 新（從沒收到的塊開始）server 回 `OutOfOrder`、跳回 `expected_seq`。所以 CLI 只要「從 `Status.received` 送」，不必自己算。

## 7. 資料目錄：一台機器一把鑰、一個 server 一份快取、一個帳號一套 session

> 用字（維護者 2026-09-07 定）：**passphrase** 是解 `local.key` 的那句話，只存在這台機器；**password** 一律指 Matrix 帳號密碼，只有 `login` 用一次。旗標、命令、錯誤訊息、文件都照這個分。

| 平台 | 位置 |
|---|---|
| Windows | `%APPDATA%\wbf-cli\` |
| Linux | `$XDG_DATA_HOME/wbf-cli/`（沒設就 `~/.local/share/wbf-cli/`） |
| macOS | `~/Library/Application Support/wbf-cli/` |

```
<data dir>/
  local.key                      32 byte 主金鑰，一台機器一把（local-cache-db.md §4）；所有帳號共用
  wbf.conf                       設定檔（§10）；指定了 --data-dir 而這裡還沒有時自動生成一份
  unlock.ticket                  只有 passphrase 模式會有（§7.1）
  recovery/<b58>_<b58>           recovery key（local-cache-db.md §10.8）；🚫 logout 不碰它
  current                        目前帳號：一行 "<加密的 server 目錄名>/<加密的帳號目錄名>"；沒有這個檔 = 沒登入過。🚫 兩層都不寫明文（寫了等於把剛加密的名字再漏一次）
  servers/<b58>_<b58>/           **server host 加密後的名字**（local-cache-db.md §11.2）：外面看不出這台機器連過哪家
    cache.db                     這個 server 上所有帳號共用的快取（§3.5；local-cache-db.md §6）
    accounts/
      <b58>_<b58>/               帳號目錄：**localpart 加密後的名字**（同 §11.2），第六把子金鑰
        session.sealed           { "server", "user_id", "device_id", "access_token", "store_dir" } 用第三把子金鑰封住
        matrix/                  matrix-sdk 的 crypto 與 state store，綁 device；StoreCipher 用第二把子金鑰包住；logout 刪
        room-keys/               本地房間金鑰備份（local-cache-db.md §10.4），一房一檔；第五把子金鑰；`account del`／`destroy` 連它一起刪（§10.7 的閘門）
    media/                       媒體儲存池（local-cache-db.md §8）：<hash 前 2 hex>/<hash> 是完整檔、pending/m<id> 是下載中；第四把子金鑰
```

- **兩層目錄名都是加密的**（local-cache-db.md §11）：`<base58 nonce>_<base58 密文>`，底線分隔（Base58 字母表沒有 `_`）。上層是正規化過的 server host（小寫、非預設 port 才帶），下層是 localpart。要知道是哪家、是誰得解密，所以連 `account status` 都要先解鎖（§11.6）。真正的 server URL 與 mxid 仍然在 `session.sealed` 裡，不從目錄名反推。
- 哪個帳號：`--account` → `current`。`login` 寫 `current`；`logout` 的帳號是 `current` 就清掉。
- `local.key` 為什麼在頂層不在帳號底下：主金鑰的定位是「這台機器」（local-cache-db.md §4），passphrase 也是一台機器一個；一帳號一把會變成每個帳號各自問 passphrase，沒有理由。
- `cache.db` 為什麼在 server 層：`r_seq`／`g_seq` 是 fork server 發的，同一個 room 在不同 homeserver 上序號不同；共用範圍就是同一個 server 的帳號（§3.5）。
- **明文目錄名的舊佈局（含 PR #11 那版）一律不遷移**：維護者 2026-09-09 定——server 從未上線、client 從未被使用，breaking 就 breaking。舊目錄在 §11.5 的掃描裡本來就解不開、會被跳過；一個都解不開時印一行提示叫人刪掉 data dir 重新 `login`（local-cache-db.md §11.7）。

🚫 任何命令的輸出、log、錯誤訊息都不印 `access_token`、主金鑰、子金鑰、passphrase、password。

### 7.1 unlock ticket（`passphrase` 模式的 CLI 專用）

`local.key` 是 `passphrase` 模式時，每個命令都要 passphrase。CLI 仿 `sudo`：passphrase（檔案或終端）解鎖成功後把**明文主金鑰**加 `expires_at` 寫到 `unlock.ticket`（Unix 0600），有效期 `--unlock-ttl`（預設 900 秒）；期內的命令直接用它。

- 來源順序：`--passphrase-file` → 有效的 ticket → 問終端。`plain` 模式不看 ticket。
- 過期、壞掉的 ticket 讀到就刪；Unix 上 group／other 有任何位元就不認（印一行提示，要人 `lock`）。
- `lock` 刪它；`logout`、`set-passphrase`、`remove-passphrase` 也順便刪。
- ⚠️ ticket 存在的那幾分鐘安全性等於 `plain` 模式。維護者 2026-09-05 明說接受：CLI 是開發與除錯工具，不是產品面。**UI 沒有這個東西**，UI 解鎖一次主金鑰只在記憶體。

## 8. 驗收腳本（第 2 步交付的一部分）

`scripts/acceptance.sh`（Windows 跑 Git Bash），對著本機 wbfuwunel，照 plan-v1 §4 的表：

1. `login`，`ping` 看 features 有 `upload`、`download`。
2. 產生 200 MiB 隨機檔 → `upload` → 標準 `GET /_matrix/client/v1/media/download/…` 拿整份 → 長度必須是 `file_size + 塊數 × 16`（明文模式不加）。
   逐 byte 的密文比對在 `crates/wbf-sdk/tests/e2e_local_server.rs`（CLI 沒有印密文的命令，也不該有）。
3. `download` → 與原檔 `cmp`。
4. `seek --at 150000000 --len 4096` → 與 `dd` 從原檔切的同一段 `cmp`；stderr 摘要的 `chunks_read` 只有一個元素。
5. 上傳到一半 `kill` → 再跑同一條 `upload` → 看到 `resume from chunk N` → 結果同第 2、3 條。
6. `cat 原檔 | upload --stream` → 同第 3 條。
7. 三種 `--cipher` 各跑一次第 2、3 條。

實作：`scripts/acceptance.sh`，`WBF_PASSWORD_FILE=<檔> scripts/acceptance.sh`；`WBF_ACCEPT_SIZE_MIB` 可以縮小檔案快速跑。
第 5 步的「殺掉」走 HTTP 通道（一塊一個請求，慢到來得及殺），殺完先 `status` 確認 `finished` 是 false 才算數。
2026-09-05 對本機 wbfuwunel 跑 200 MiB 全過，79 秒。

## 9. 明確不做的

- 沒有互動模式、沒有進度條以外的 UI。
- 不存密碼，只存 token。
- 第 3 步還沒做的：`room`、建房、邀請、改權限、置頂、已讀送出、裝置驗證、標準附件下載（chat-model §6）。
- 🚫 `key-backup` 不做「刪掉 server 上的 backup version」（不可逆，而且會連帶其他裝置，local-cache-db.md §10.8）。
- 🚫 conf 不改寫已經存在的檔、不寫秘密（§10.3、§10.5）。
- 🚫 不把目錄名的對照表落地成明文索引（local-cache-db.md §11.5）：那等於把剛加密的東西再寫一次明文。
- 🚫 不把 `--password-file` 也改成原始 bytes（local-cache-db.md §12.4）：password 要送給 server，它本來就是字串。

## 10. conf 檔：不用每次指定環境變數（維護者 2026-09-09 要求）

### 10.1 在哪、長怎樣

檔名 `wbf.conf`，放在**資料目錄裡**（`<data dir>/wbf.conf`）。找的順序：

1. `--config <path>` 明指的那個（找不到就報錯，🚫 不默默 fallback —— 明指了還讀到別的比讀不到更糟）。
2. `<data dir>/wbf.conf`（`<data dir>` 由 `--data-dir` → `WBF_DATA_DIR` → §7 的平台預設決定）。
3. 都沒有就全用預設值。

```ini
; wbf.conf —— 分號或井號開頭是註解
# 值後面的 # / ; 前面有空白才算註解；要保留就用雙引號包住整個值

[general]
SERVER=http://localhost:6167
ACCOUNT=@alice:localhost
TRANSPORT=ws
UNLOCK_TTL=900

[backup]
SERVER_BACKUP=on          ; 標準 Matrix key backup（local-cache-db.md §10.3）
LOCAL_ROOM_KEYS=on        ; 本地加密金鑰池（同 §10.4）

[media]
QUOTA_MIB=2048
PROTECT_DAYS=7

[recent]
MAX_EVENTS=10000
WINDOW=500
```

- **區段**：`[名稱]` 一行一個，之後的鍵都屬於它。鍵名在區段內唯一；同一個鍵出現兩次以後面的為準（並印一行警告）。
- **鍵名就是環境變數去掉 `WBF_` 前綴**（`SERVER` ↔ `WBF_SERVER`）。這樣兩邊不會漂移，也不必另外背一套名字。
- **註解**：`;` 或 `#` 起，到行尾。⚠️ 只有**行首**或**前面是空白**時才起註解 —— `PASSWORD_FILE=/tmp/a#b` 裡的 `#` 是值的一部分。
  要讓值以 `#` 開頭或含前後空白，用雙引號包住整個值（`KEY="  # 這是值  "`）。
- 值前後的空白去掉；空值（`KEY=`）**當作沒寫**，不是空字串（全域 CLAUDE.md：佔位值不用空字串）。

### 10.2 優先序：旗標 > 環境變數 > conf > 內建預設

維護者 2026-09-09 定：**環境變數優先於 conf**。命令列旗標仍然最高（它是「就這一次」的意思）。
每個值各自比一次，不是整份取代：conf 給了 `SERVER`、環境給了 `WBF_ACCOUNT`，兩個都生效。

### 10.3 自動生成

維護者 2026-09-09 要求：**指定了資料目錄時，conf 自動生成在同一個目錄下，把當前的值寫進去。**

- 觸發條件（三個都要成立）：`--data-dir` 或 `WBF_DATA_DIR` 有給、那個目錄下**還沒有** `wbf.conf`、這次命令**成功**結束。
- 寫進去的是**這次實際生效的值**（旗標／環境／預設合併之後的），每個值後面附一行註解說它是從哪來的。
- 已經存在的 `wbf.conf` **永遠不改寫**（連補鍵都不做）：那是使用者的檔，不是我們的狀態檔。要重生成先自己刪掉。
- 🚫 **不寫任何秘密**：`ACCESS_TOKEN` 不寫（conf 也不支援這個鍵，見 §10.5），`PASSWORD_FILE`／`PASSPHRASE_FILE`
  這種「秘密在哪」的路徑自動生成時也不寫 —— 要就手動加，讓它是使用者的決定。

### 10.4 認不得的東西怎麼辦：不是正面認得就落到安全值

- **認不得的區段或鍵**：印一行警告到 stderr，忽略它，命令照跑。（conf 要往前相容：舊版 CLI 讀到新版寫的鍵不該整個掛掉。）
- **認得的鍵、認不得的值**：⚠️ 開關型的鍵（`SERVER_BACKUP`、`LOCAL_ROOM_KEYS`）**只正面認得 `on` 與 `off`**（不分大小寫）；
  其他任何值（`true`、`1`、`yes`、拼錯的 `of`）一律警告並落到**安全值**，也就是 `on`。
  判斷一律寫成「**正面認得 `off` 才關**」，🚫 不寫成「不等於 `on` 就關」—— 壞掉的時候要壞在「備份還開著」那一邊，不是「以為開著、其實沒開」。
- **數值型的鍵**（`UNLOCK_TTL`、`QUOTA_MIB`…）：parse 不出來就警告並用內建預設，不用半套的值。
- conf 檔本身壞掉（不是 UTF-8、區段語法錯）：**報錯 exit**，不當作沒有這個檔。設定檔讀一半比讀不到危險。

### 10.5 conf 不放什麼

| 不放 | 理由 |
|---|---|
| `ACCESS_TOKEN` | token 是秘密，🚫 不落地在明文檔裡。要臨時指定用 `--token`／`WBF_ACCESS_TOKEN`（§2） |
| passphrase、password 本身 | 同上，而且 §2 已經定了秘密只能從檔案或終端來 |
| `PASSWORD_FILE`／`PASSPHRASE_FILE` 的自動生成 | 可以手寫（它們是路徑不是秘密，維護者要的正是「不用每次指定」），但自動生成不替使用者決定秘密放哪 |
| 帳號狀態（`current`、session） | 那是狀態不是設定，各有各的檔（§7）。設定檔可以刪掉不影響登入。⚠️ `current` 的**內容一樣要加密**（§7：兩層都寫加密後的目錄名）—— 它不在 conf 裡，但不能因此漏了 |

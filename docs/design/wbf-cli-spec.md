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
| `--session <path>` | `WBF_SESSION` | session 檔位置，預設見 §7 |
| `--json` | | stdout 只印 JSON（預設就是；留這個旗標是為了之後加人類可讀模式時介面不變） |
| `--quiet` | | stderr 不印進度 |
| `--transport ws\|http` | | 預設 `ws`；`http` 走 `POST /_wbf/v1/pack` 一請求一包，給除錯與 server 端測試用 |

## 3. 命令

### 3.1 帳號

| 命令 | 做什麼 | stdout |
|---|---|---|
| `login --user <mxid> [--password-file <path>] [--device-name <name>]` | 登入、寫 session 檔。`--password-file` 整檔就是密碼（去掉結尾一個換行）；沒給就從終端讀（不回顯）。🚫 沒有 `--password <pw>`、🚫 不接受環境變數給密碼：兩者都會留在 shell 歷史與 `ps` 輸出裡 | `{ "user_id", "device_id", "server" }` |
| `logout` | `POST /_matrix/client/v3/logout` 讓 token 失效，刪 session 檔 | `{ "ok": true }` |
| `whoami` | `GET /_matrix/client/v3/account/whoami` | `{ "user_id", "device_id" }` |

### 3.2 上傳

| 命令 | 做什麼 |
|---|---|
| `upload <file> [--cipher chacha20-poly1305\|aes-256-gcm\|none] [--chunk-size <bytes>] [--manifest <out.json>] [--sha256]` | 固定大小上傳：Create → 逐塊 → Seal。印 manifest（§5） |
| `upload --stream [--cipher …] [--link mobile\|wifi] [--chunk-size <bytes>] [--name <n>] [--mimetype <m>] [--manifest <out.json>]` | 從 stdin 讀、`0/0` 哨兵、最後一塊 `IS_LAST`、Seal 帶最終描述。`--link` 決定串流的 `chunk_size`（約定 §2），預設 `mobile` |
| `status <upload_id>` | 印 `Status` 的 Ack |
| `abort <upload_id>` | 送 `Abort`，刪狀態檔 |

- `--cipher` 預設：偵測到硬體 AES 用 `aes-256-gcm`，否則 `chacha20-poly1305`（約定 §3）。`none` 是明文模式；第 2 步沒有房間，所以要不要警告是第 3 步 `send` 的事，這裡直接照做。
- `--chunk-size` 沒給就照約定 §2 的表選；給了就照給的（要在 server 允許的範圍，不然 Create 會被拒）。
- `--sha256`：上傳時順便算整檔明文雜湊寫進描述與 manifest。串流模式一律算（反正要讀過一遍）。
- **續傳**：`upload` 開始前在檔案旁寫狀態檔（§6）。同一個 `upload <file>` 再跑一次，看到狀態檔就先 `Status`，從 `received` 接著送，用**同一把** key 與 nonce_base（同一個上傳，不是重傳）。Seal 成功後刪狀態檔。
  狀態檔的 server 與 user 跟現在的不一樣 → 拒絕，不會拿 A server 的上傳去打 B server。

### 3.3 下載

| 命令 | 做什麼 |
|---|---|
| `info <mxc> [--manifest <m.json>]` | 印 `Info` 的 Ack（server 知道的欄位）。有 manifest 就順便解描述印出來，並做約定 §3.1 第 2 條的核對 |
| `download --manifest <m.json> [-o <out>]` | `Info` → 逐塊 `Read` → 解密 → 寫檔。全部檢查照約定 §3.1，任一不過刪掉半成品、exit 3。沒給 `-o` 用描述的 `name`，沒有就 `download.bin` |
| `play --manifest <m.json> --at <pos> [--len <n>]` | seek：只 `Read` 含 `pos` 的那一塊（`--len` 跨塊就多讀），解密後把 `pos` 起的明文寫到 stdout。這是驗收「不必下載前面」的命令 |

下載的參數都從 manifest 來，不提供 `--key` 這種零散參數：金鑰不該出現在命令列與 shell 歷史裡。

### 3.4 房間（第 3 步）

| 命令 | 做什麼 |
|---|---|
| `rooms` | 列出加入的房間：`[{ "room_id", "name", "encrypted": bool }]` |
| `send <room_id> --text <msg>` | 送文字 |
| `send <room_id> --file <file> [upload 的參數]` | upload 後把約定 §5 的事件送進房間。房間沒 E2EE 就走明文模式，**送之前印警告並要求確認**（約定 §5.1）；`--yes` 跳過確認給腳本用 |
| `watch <room_id> tail \| wait <秒> \| once [--since <token>]` | 從 `/sync` 等**新**事件（現在起），來一個立刻印一個，一行一個 JSON。三種模式見 §3.4.2。認得 `org.wbftw.wbfuwunel.file` 就把區塊解出來當 manifest 印 |
| `ping` | `Hello` 加 `Ping`，印 server 回的 features 與上限。除錯用，第 2 步就有 |

#### 3.4.1 讀房間（規劃，第 3 步之後）

`watch` 只看得到現在起的新事件。讀歷史、找檔案、看房間本身，是另外三個問題，各一個命令：

| 命令 | 做什麼 | stdout |
|---|---|---|
| `room <room_id>` | 房間本身：`GET .../rooms/{id}/state` 挑出來的欄位 | `{ "room_id", "name", "topic", "encrypted": bool, "member_count", "joined_members": [mxid…] }` |
| `read <room_id> [--limit <n>] [--before <token>] [--type <event_type>…] [--sender <mxid>]` | 歷史：`GET .../rooms/{id}/messages?dir=b`，從最新往回。`--limit` 預設 50；`--before` 接上一頁印的 `next`，再往前翻。`--type`／`--sender` 是 client 端過濾，翻頁的 token 不受影響 | `{ "events": [事件…], "next": token \| null }`，`next` 是 null 表示到頭了 |
| `files <room_id> [--limit <n>] [--before <token>] [--save <dir>]` | `read` 只留 `org.wbftw.wbfuwunel.file`，把區塊解成 manifest（§5）印出來；`--save` 一個事件存一個 `<event_id>.json`，之後直接 `download --manifest` | `{ "files": [{ "event_id", "sender", "ts", "manifest" }…], "next" }` |

事件的統一形狀（`read`、`watch`、`files` 都用）：

| 欄位 | 說明 |
|---|---|
| `event_id`、`sender`、`ts`、`type` | 照 Matrix 原樣 |
| `content` | 解密後的內容。明文房間就是原樣 |
| `decrypted` | `true`／`false`／`null`。`null` 表示本來就不是加密事件 |
| `undecryptable_reason` | `decrypted` 是 `false` 才有，example: `no_session_key`、`no_e2ee_store`（第 2 步的 session 沒有裝置金鑰，見 §1） |

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
| `once` | 印到**第一則**就結束；`--timeout <秒>` 到了還沒有 exit 5 | 腳本：「阻塞到有回應為止」 |

規則：

- **印是即時的，不是結束才印。** 三種模式都是事件一到就寫一行 JSON 進 stdout 並 flush，所以 `wait 5` 接管線不會等到第 5 秒才一次吐出來。這是 §4「一個 JSON 物件」的第二個例外（第一個是 `play`）：`watch` 印 **JSON Lines**。
- **結束時 stderr 印一行 `since`**，下次用 `--since` 接著等，中間進來的不漏。這也是 `wait` 連續呼叫的接法；`since` 自己不落地，理由同 §3.4.1 的 `next`。
- **只印這個房間的。** `/sync` 回來的是全部房間，其他房間的丟掉；要看全部就開幾個 `watch`。
- **自己送的也印**（`sender` 是自己），腳本自己濾。`once` 不把自己的算進「第一則」，不然 `send` 完接著 `once` 永遠等到的是自己。
- **第 2 步的 session 在加密房間照樣能用**，只是每則都 `decrypted: false`；`once` 還是算有回應。

## 4. 輸出與 exit code

- stdout：**一個 JSON 物件**，命令成功才印。兩個例外：`play` 印明文 bytes；`watch` 印 JSON Lines，一事件一行、即時 flush（§3.4.2）。
- stderr：進度（`chunk 17/2031 …`）、警告、錯誤訊息。
- exit code：

| code | 意思 |
|---|---|
| 0 | 成功 |
| 1 | 用法錯：參數、找不到檔、session 檔壞掉 |
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
{ "server": "…", "user_id": "@a:localhost", "upload_id": 1234605616436508552, "mxc": "…",
  "cipher": "…", "key": "…", "nonce_base": "…", "chunk_size": 65536, "file_size": 132056, "chunk_count": 3 }
```

含 `key`，同 manifest 的保護。Seal 成功或 `abort` 後刪。

## 7. Session 檔

| 平台 | 位置 |
|---|---|
| Windows | `%APPDATA%\wbf-cli\session.json` |
| Linux | `$XDG_CONFIG_HOME/wbf-cli/session.json`（沒設就 `~/.config/wbf-cli/`） |
| macOS | `~/Library/Application Support/wbf-cli/session.json` |

內容 `{ "server", "user_id", "device_id", "access_token" }`，第 3 步加 SDK store 的路徑。權限只給自己。
🚫 任何命令的輸出、log、錯誤訊息都不印 `access_token`。

## 8. 驗收腳本（第 2 步交付的一部分）

`scripts/acceptance.sh`（Windows 跑 Git Bash），對著本機 wbfuwunel，照 plan-v1 §4 的表：

1. `login`，`ping` 看 features 有 `upload`、`download`。
2. 產生 200 MiB 隨機檔 → `upload` → 標準 `GET /_matrix/client/v1/media/download/…` 拿整份 → 與本地逐塊密文串接後 `cmp`。
3. `download` → 與原檔 `cmp`。
4. `play --at 150000000 --len 4096` → 與 `dd` 從原檔切的同一段 `cmp`；stderr 要顯示只讀了一塊。
5. 上傳到一半 `kill` → 再跑同一條 `upload` → 看到 `resume from chunk N` → 結果同第 2、3 條。
6. `cat 原檔 | upload --stream` → 同第 3 條。
7. 三種 `--cipher` 各跑一次第 2、3 條。

## 9. 明確不做的

- 沒有互動模式、沒有進度條以外的 UI。
- 不存密碼，只存 token。
- 第 2 步不碰房間；`send`／`recv`／`rooms` 第 3 步才有。

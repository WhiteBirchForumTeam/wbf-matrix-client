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
| `download --manifest <m.json> [-o <out>]` | `Info` → 逐塊 `Read` → 解密 → 寫檔。全部檢查照約定 §3.1，任一不過刪掉半成品、exit 3。沒給 `-o` 用描述的 `name`，沒有就 `download.bin` |
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

從第 3 步起 `login` 走 matrix-sdk（拿到有裝置金鑰的 session，E2EE 房間才解得開），store 放 session 檔旁邊的 `matrix/`（§7）。
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
| `once` | 印到**第一則**就結束；`--timeout <秒>` 到了還沒有 exit 5 | 腳本：「阻塞到有回應為止」 |

規則：

- **印是即時的，不是結束才印。** 三種模式都是事件一到就寫一行 JSON 進 stdout 並 flush，所以 `wait 5` 接管線不會等到第 5 秒才一次吐出來。這是 §4「一個 JSON 物件」的第二個例外（第一個是 `seek`）：`watch` 印 **JSON Lines**。
- **結束時 stderr 印一行 `since`**，下次用 `--since` 接著等，中間進來的不漏。這也是 `wait` 連續呼叫的接法；`since` 自己不落地，理由同 §3.4.1 的 `next`。
- **只印這個房間的。** `/sync` 回來的是全部房間，其他房間的丟掉；要看全部就開幾個 `watch`。
- **自己送的也印**（`sender` 是自己），腳本自己濾。`once` 不把自己的算進「第一則」，不然 `send` 完接著 `once` 永遠等到的是自己。
- **第 2 步的 session 在加密房間照樣能用**，只是每則都 `decrypted: false`；`once` 還是算有回應。

## 4. 輸出與 exit code

- stdout：**一個 JSON 物件**，命令成功才印。兩個例外：`seek` 印明文 bytes；`watch` 印 JSON Lines，一事件一行、即時 flush（§3.4.2）。
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
{ "server": "…", "user_id": "@a:localhost", "upload_id": 1234605616436508552, "mxc": "…", "chunk_max_bytes": 69632,
  "block": { "v": 1, "cipher": "…", "key": "…", "nonce_base": "…", "chunk_size": 65536, "file_size": 132056,
             "name": "video.mkv", "mimetype": "video/x-matroska" } }
```

- `block` 與 manifest 的 `block` 同一個形狀（SDK 的 `ChunkedBlock`），只是還沒有 `sha256`；Seal 時填上就變成 manifest 的 `block`。
  塊數不存：從 `file_size` 與 `chunk_size` 算得出來。這是 SDK 的 `UploadState`，`Create` 一回來就寫檔。
- 含 `key`，同 manifest 的保護。Seal 成功或 `abort` 後刪。
- 續傳時 SDK 對 server 的 `Status` 有兩種修正：狀態檔比 server 舊（重送已收的塊）server 回冪等 Ack、跳到 `received`；
  比 server 新（從沒收到的塊開始）server 回 `OutOfOrder`、跳回 `expected_seq`。所以 CLI 只要「從 `Status.received` 送」，不必自己算。

## 7. Session 檔

| 平台 | 位置 |
|---|---|
| Windows | `%APPDATA%\wbf-cli\session.json` |
| Linux | `$XDG_CONFIG_HOME/wbf-cli/session.json`（沒設就 `~/.config/wbf-cli/`） |
| macOS | `~/Library/Application Support/wbf-cli/session.json` |

內容 `{ "server", "user_id", "device_id", "access_token", "store_dir" }`；`store_dir` 是第 3 步加的 matrix-sdk store 目錄（session 檔旁邊的 `matrix/`，裝 crypto 與 state 兩個 sqlite，絕對不當快取，plan-v1 §7.1）。
⚠️ 這一版 store 沒有 passphrase；主金鑰的 vault 是 local-cache-db.md 那一版的事。權限只給自己。
🚫 任何命令的輸出、log、錯誤訊息都不印 `access_token`。

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

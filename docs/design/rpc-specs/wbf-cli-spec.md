# rpc-cli 規格：命令、參數、輸出、檔案格式

> 📎 **檔名沿用舊 `wbf-cli` 的**：別的文件與程式碼註解用 `/docs/design/rpc-specs/wbf-cli-spec.md §N` 指這裡，節號也照舊（§3.3.1、§3.4.1、§5、§6、§10…）。
> 舊 CLI 在 A-2 拿掉（`apps/wbf-cli` 直接叫 `Core`、繞過 daemon）；為什麼換、怎麼決定的在 /docs/design/rpc-specs/rpc-cli.md。
>
> 本文只定 **rpc-cli 自己那一層**：命令、參數、輸出、exit code。每個 method 的參數與回傳照 /docs/design/rpc-specs/rpc-spec.md；
> 線上行為照 wbfuwunel 的線上規格；加解密照 /docs/design/media/wbf-client-convention-for-chunk.md（以下稱「約定」）。

## 0. 一句話

`wbf-matrix-rpc-cli` 是 daemon 的第一個正式外部 client：**每次執行起一支自己的 daemon、丟 RPC、印結果、關掉 daemon**。
命令列只是 RPC 訊息的薄封裝（/docs/design/overview/architecture-v2.md §0.2）：**不**重試、**不**改輸出的形狀、**不**替人串動作。
結果是 JSON，印到 stdout；訊息與錯誤印到 stderr；exit code 說明失敗類別（§4）。所以它能被腳本串起來，驗收腳本就是這樣寫的（§8）。

一次執行做這幾件事（`apps/wbf-matrix-rpc-cli/src/session.rs`）：

```text
1. 先問人：密碼（account add）、確認（account destroy）—— 問完才起 daemon，免得 daemon 開著等人打字
2. 產生 256 byte 的 token，放進子程序的環境變數 WBF_DAEMON_TOKEN，spawn `wbf-matrix-client-daemon -s --data-dir <D>`，讀它 stdout 的 ready 那一行
3. 連 RPC 的 port、hello（client 名稱 `wbf-matrix-rpc-cli <版本>`）
4. 解鎖（§7.1）
5. 命令
6. daemon.shutdown → 等 daemon 自己結束（最多 60 秒）
```

📎 用 `-q` 時看不到 daemon 自己的 stderr；沒用時 daemon 那幾行（`listening on …`、自動生成 conf 的提示）跟 rpc-cli 的混在同一個 stderr。

## 1. 權限從哪來

**rpc-cli 對 daemon**：用它自己產生、交給它自己起的那支 daemon 的 token（/docs/design/rpc-specs/local-interface.md §3）。token 只活在兩個程序的記憶體裡，**不**落地。

所以 **別的前端（例：Desktop）起的 daemon 正在用這個資料目錄時，rpc-cli 用不了**：它連不上那支（沒有 token），也起不了第二支（資料目錄的排他鎖，/docs/design/overview/architecture-v2.md §0.2）。
這時 exit 1，訊息講清楚（實跑，`-q`）：

```text
the daemon exited before saying ready (exit code: 1)
(if another frontend's daemon is serving D:/Weil/Desktop/AI/wbf-walk-a, use that frontend or stop it first: rpc-cli can only talk to a daemon it starts itself, /docs/design/rpc-specs/rpc-cli.md §1)
```

**daemon 對 homeserver**：Matrix 登入發的 `access_token`，由 `account add`（RPC 的 `account.add`）取得、封進帳號目錄的 `session.sealed`（/docs/design/daemon/account-session.md §1）。

| server | `account.add` 怎麼登入 | 裝置金鑰 |
|---|---|---|
| wbfuwunel | 標準 HTTP `POST /_matrix/client/v3/login`（`m.login.password`），**不**經 matrix-sdk | `m/` 由我們的 `OlmEngine` 開（/docs/design/daemon/account-session.md §3） |
| 一般 Matrix | matrix-sdk 的登入 | matrix-sdk 的 store 在 `m/` |

📎 舊 CLI 的 `--token`（直接給 `access_token`、不碰 vault 與帳號目錄）**拿掉了**：daemon 模型下沒有「不碰帳號目錄」這條路（/docs/design/rpc-specs/rpc-cli.md §2.3）。

## 2. 全域參數

| 參數 | 環境變數 | 說明 |
|---|---|---|
| `--data-dir <dir>` | `WBF_DATA_DIR` | daemon 的資料目錄（必填）。裡面每個檔是什麼在 §7 |
| `--passphrase-file <path>` | `WBF_PASSPHRASE_FILE` | 整檔的**原始 bytes** 就是 passphrase（解 `local.key` 用）。**不**去尾換行、**不**驗 UTF-8：可以是中文、可以是一個 mp3（/docs/design/storage/vault-and-keys.md §3）。沒給而 vault 要 passphrase 時從終端讀（不回顯）。**沒有** `--passphrase <pw>`、**不**接受環境變數給 passphrase 本身 |
| `--daemon <path>` | `WBF_DAEMON` | daemon 的執行檔；沒給用 rpc-cli 同一個目錄的 `wbf-matrix-client-daemon`（Windows 加 `.exe`） |
| `-q`／`--quiet` | | daemon 自己的 stderr 丟掉 |

舊 CLI 的全域參數去哪了：

| 舊的 | 現在 |
|---|---|
| `--server`、`--account` | 每則 RPC 的 `server`／`user` 參數（/docs/design/rpc-specs/rpc-spec.md §2）；便利命令有 `--user`／`--server` |
| `--transport ws\|http` | 每則 RPC 的 `transport` 參數；省略就是 conf 的 `TRANSPORT`（§10） |
| `--config` | daemon 讀 `<data dir>/wbf.conf`；rpc-cli 不傳 `--config`（要指別的檔就自己起 daemon） |
| `--token` | 拿掉（§1） |
| `--json` | 拿掉：stdout 本來就只有 JSON |

## 3. 命令

| 命令 | 做什麼 | stdout |
|---|---|---|
| `call <method> [params JSON]` | 送一則 RPC | 它的 `result`（一行 JSON） |
| `batch` | 從 stdin 讀 JSON Lines（每行 `{"method": …, "params": …}`），一則一則送、**共用一支 daemon**；遇到第一個錯就停 | 每則回應**原樣**一行（含 `id`、`code`、`msg`） |
| `account add` | 讀密碼、登入（§3.1） | `account.add` 的 `result` |
| `account destroy <mxid>` | 問確認、摧毀（§3.1） | `account.destroy` 的 `result` |
| `put <file\|->` | 傳檔到資料平面（§3.2） | manifest（§5） |
| `get` | 從資料平面讀檔（§3.3） | 檔案內容（沒給 `-o` 時） |
| `watch [events…]` | 訂閱推播、一則一行印出來（§3.4.2） | JSON Lines |

**其他全部用 `call`**。`call` 與 `batch` 的 `params` 就是 /docs/design/rpc-specs/rpc-spec.md 那一欄，原樣送、原樣印；`id` 由 rpc-cli 發。

```bash
wbf-matrix-rpc-cli --data-dir D call daemon.info
# {"cache_queue":0,"connections":1,"data_dir":"D:\\Weil\\Desktop\\AI\\wbf-smoke-a2","data_port":8605,"encryption_enforced":true,"instance":"8952a68f-ee91-4e05-a1a3-66d4ebee4f98","key_mode":null,"links":0,"local_room_keys_setting":"on","pid":45372,"protocols":[2],"read_receipts_setting":"private","rpc_port":8604,"server_backup_setting":"on","unlocked":false,"uptime_seconds":0,"version":"wbf-matrix-client-daemon 0.1.0"}

printf '%s\n' '{"method":"vault.create"}' '{"method":"account.list"}' '{"method":"nope.nope"}' '{"method":"daemon.info"}' \
  | wbf-matrix-rpc-cli --data-dir D -q batch
# {"code":0,"id":2,"msg":"ok","result":{"key_mode":"plain","ok":true}}
# {"code":0,"id":3,"msg":"ok","result":{"accounts":[]}}
# {"code":101,"id":4,"msg":"unknown method \"nope.nope\"","result":null}
# stderr: error 101: unknown method "nope.nope"      exit 1；第四行沒送
```

- ⭐ **一串命令用 `batch`**：每次執行都要起 daemon、解鎖、在背景開線（每個 wbf 帳號五條，/docs/design/daemon/link-pool.md §3.1）。
  連續跑很多次 rpc-cli，server 可能用 429 拒絕開線（2026-11-19 對本機 wbfuwunel 實跑撞到：`websocket connect …: HTTP error: 429 Too Many Requests`，exit 4）。
- `batch` 跳過空行；第一行開頭的 UTF-8 BOM 去掉（Windows PowerShell 5.1 的管線會加）。

舊 CLI 的命令對到哪：

| 舊的 | 現在 |
|---|---|
| `login`／`account add` | `account add`（§3.1） |
| `account status`、`account switch`、`account del`／`logout`、`whoami` | `call account.list`、`call account.switch`、`call account.del`、`call account.whoami` |
| `account destroy` | `account destroy`（§3.1） |
| `set-passphrase`、`remove-passphrase` | `call vault.set_passphrase`、`call vault.remove_passphrase`（新 passphrase 用 base64 放在 params，/docs/design/rpc-specs/rpc-spec.md §3.1） |
| `upload <file>` | `put <file>`；或 `call upload.file '{"path":"<絕對路徑>"}'`（daemon 自己讀檔、有續傳狀態檔，§6） |
| `upload --stream` | `put -` |
| `status`、`abort` | `call upload.status`、`call upload.abort` |
| `info` | `call media.info` |
| `download` | `get`；或 `call media.export_to`（daemon 寫檔、完成時驗證） |
| `download --no-cache` | `call media.export_to '{"no_cache":true,…}'` |
| `seek` | `get --range`（§3.3.1） |
| `rooms`、`read`、`files` | `call room.list`、`call room.history`、`call room.files`（`files --save` 拿掉：存檔是前端的事） |
| `send --text`、`send --file` | `call room.send_text`；`put` ＋ `call room.send_attachment`，或 `call room.send_file`（§3.4） |
| `watch` | `watch room.message`（§3.4.2） |
| `recent` | `call sync.recent`（§3.5） |
| `media-stats`、`media-gc` | `call media.stats`、`call media.gc` |
| `key-backup …`、`recovery …` | `call backup.*`、`call recovery.*`（§3.6） |
| `ping` | `call server.ping` |

📎 舊 CLI 會把一些輸出改形（例：`account switch` 印 `{ok,current,switched_from}`）；現在一律印 RPC 原本的 `result`。

### 3.1 帳號（維護者 2026-09-09 定）

**多帳號是前提，不是附加功能**：一支 daemon 同時服務資料目錄裡所有登入著的帳號。帳號目錄一帳號一套（§7），`m/`（裝置狀態）也一帳號一套。
`current` 只回答一個問題：**RPC 沒帶 `user` 時用誰**。

| 命令 | 做什麼 |
|---|---|
| `account add --server <url> --user <localpart 或 mxid> [--password-file <path>] [--device-name <name>]` | 讀密碼 → 還沒有 `local.key` 就先 `vault.create`（給了 `--passphrase-file` 是 `passphrase` 模式，否則 `plain`）→ `account.add`。`--password-file`（或 `WBF_PASSWORD_FILE`）整檔就是密碼，去掉結尾一個換行（`\r\n` 也算一個）；沒給就從終端讀（不回顯），stdin 不是終端就拒絕。**沒有** `--password <pw>`、**不**接受環境變數給密碼本身：兩者都會留在 shell 歷史與 `ps` 輸出裡 |
| `account destroy <mxid> [--server <url>] [--yes] [--accept-history-loss]` | **不可逆**：沒帶 `--yes` 就要使用者把 mxid 再打一次；stdin 不是終端就要 `--yes`（禁止默默當成同意）。然後 `account.destroy`。做了什麼在 /docs/design/rpc-specs/rpc-spec.md 的 `account.destroy` 與本文 §3.5 |

```bash
wbf-matrix-rpc-cli --data-dir D -q account add --server http://127.0.0.1:6167 --user alice --password-file alice-pw.txt --device-name rpc-cli-walk
# {"device_id":"r2vpn5sMAn","server":"http://127.0.0.1:6167","user_id":"@alice:localhost"}

wbf-matrix-rpc-cli --data-dir D -q account destroy @bob:localhost < /dev/null
# stderr: account destroy cannot be undone; stdin is not a terminal, so pass --yes      exit 1
```

🔑 **會登出、會刪檔的 method，`user` 一律是完整 mxid**（維護者 2026-09-09）：`account.switch`／`account.del`／`account.destroy` 收 `@bob:matrix.org`，**不**收 `bob`。
變更的對象不該靠猜：只給 localpart 就報錯並列出本機的帳號（**不**推測、禁止拿唯一一個頂替；`crates/wbf-core/src/account_ops.rs` 的 `find_account_by_full_mxid`）。
其他 method 的 `user` 仍然收 localpart 簡寫：它只決定這一次用誰，歧義時要配 `server`（§7）。

#### 3.1.1 切換帳號的提示

`account.add` 成功會自動切成 `current`，`account.switch` 也是；換掉的是誰在 `result` 的 `switched_from`。
core 另外發一則 `note`（`switched to @alice:localhost on http://localhost:6167 (was @bob:localhost)`，第一次是 `(no previous account)`），
要看就 `watch note`；rpc-cli 不另外印。

#### 3.1.2 一輪多帳號長什麼樣（示意）

```bash
# 兩個帳號共用一個資料目錄（一把 local.key、一份 cache.db）
wbf-matrix-rpc-cli --data-dir ~/.wbf account add --server https://matrix.org --user alice --password-file pw-alice
wbf-matrix-rpc-cli --data-dir ~/.wbf account add --server https://matrix.org --user bob --password-file pw-bob

wbf-matrix-rpc-cli --data-dir ~/.wbf call account.list
# 換預設帳號：完整 mxid，不是 "alice"
wbf-matrix-rpc-cli --data-dir ~/.wbf call account.switch '{"user":"@alice:matrix.org"}'
# 只有這一則用 bob，current 不動
wbf-matrix-rpc-cli --data-dir ~/.wbf call room.list '{"user":"@bob:matrix.org","sync":"both"}'
# 登出 bob：session、m/（crypto store）、k/（金鑰快照）沒了，cache.db 裡的紀錄還在
wbf-matrix-rpc-cli --data-dir ~/.wbf call account.del '{"user":"@bob:matrix.org"}'
# 連 bob 在本機的紀錄一起清掉（alice 也看得到的那些不動）
wbf-matrix-rpc-cli --data-dir ~/.wbf account destroy @bob:matrix.org --yes
```

同一個 localpart 在兩個 server 上是**兩個帳號**，完整 mxid 才分得開（`@bob:matrix.org` 跟 `@bob:localhost` 不是同一個人）。

### 3.2 上傳

`put <file|-> [--name <n>] [--mimetype <m>] [--room <room_id>] [--cipher chacha20-poly1305|aes-256-gcm|none] [--chunk-size <bytes>] [--user …] [--server …]`

做的就是 UI 的兩步的第一步（/docs/design/rpc-specs/data-plane.md）：`media.create` → `PUT` 到它給的 URL → 印 manifest（§5）。發訊息是第二步、另外叫：

```bash
wbf-matrix-rpc-cli --data-dir D -q put orig.bin --room '!mm4wSdeMnTkkVqycfJ:localhost' > manifest.json
# manifest.json: {"block":{"chunk_size":65536,"cipher":"none","file_size":300000,"name":"orig.bin","sha256":"c4f8c750…","v":1},"mxc":"mxc://localhost/88612388bc1d73","server":"http://127.0.0.1:6167"}
printf '%s\n' "{\"method\":\"room.send_attachment\",\"params\":{\"room\":\"!mm4wSdeMnTkkVqycfJ:localhost\",\"manifest\":$(cat manifest.json)}}" \
  | wbf-matrix-rpc-cli --data-dir D -q batch
# {"code":0,"id":3,"msg":"ok","result":{"attachment_declared":true,"event_id":"$wa5KJv77yhi95W1x2lHFZTf3aJFgVwNzdKuaJrjAfsg","mxc":"mxc://localhost/88612388bc1d73"}}
```

- `--name` 沒給用檔名；`put -`（讀 stdin）一定要給 `--name`。給檔案時帶 `size`（`Content-Length`），stdin 是串流（chunked）。
- `--room` 給了就**照房間決定加不加密**（上例是明文房，`cipher` 是 `none`；約定 /docs/design/media/wbf-client-convention-for-chunk.md §5.1），沒給是裸上傳、照 `--cipher`。
  `--cipher` 預設：偵測到硬體 AES 用 `aes-256-gcm`，否則 `chacha20-poly1305`（約定 §3）。
- `--chunk-size` 沒給就照約定 §2 的表選；給了就照給的（要在 server 允許的範圍，不然 `Create` 會被拒）。
- `put` **不**續傳（資料平面的 PUT 沒有狀態檔）。要續傳用 `call upload.file '{"path":"<絕對路徑>"}'`：daemon 自己讀那個檔、在旁邊寫狀態檔（§6），同一條再叫一次就從 server 說收到的地方接著送。
  `call upload.status`、`call upload.abort` 對的就是它。
- 資料平面回的不是 200 → exit 2，stderr 印狀態碼與 daemon 回的那則錯誤。

### 3.3 下載

`get (--mxc <mxc> | --manifest <m.json> | --room <room_id> --event-id <event_id>) [-o <out>] [--range bytes=<a>-<b>] [--user …] [--server …]`

`media.open`（三種指法同 /docs/design/rpc-specs/rpc-spec.md §3.6）→ `GET` 它給的 URL → 寫到 `-o`（沒給或 `-` 是 stdout）。stderr 最後一行是狀態碼與 `wbf-media-verified`：

```bash
wbf-matrix-rpc-cli --data-dir D -q get --manifest manifest.json -o got.bin
# stderr: mxc://localhost/88612388bc1d73 cannot be streamed before it is downloaded; waiting for the download
# stderr: status 200, verified 1
wbf-matrix-rpc-cli --data-dir D -q get --manifest manifest.json --range bytes=70000-70999 -o range.bin
# stderr: status 206, verified 1
```

- **還沒下載完、這支 daemon 又現拉不了時，等它下載完再讀**：GET 回 502（沒有帳號看得到帶金鑰的事件，/docs/design/rpc-specs/data-plane.md §8）而 `media.open` 說它在排或在拉時，
  `get` 等 `media.download` 推到 `complete`，再 GET 一次（從池讀就不需要事件）。推到 `failed`／`cancelled` 就 exit 2。等多久沒有上限，Ctrl+C 停。
  ⚠️ 這在 rpc-cli 很常見：每次執行是一支剛起的 daemon，訂閱線還沒把事件帶進快取。UI 的 daemon 常駐，多半直接就讀得到。
- `--mxc` 要本地有那個 mxc 的紀錄（看過帶它的事件、或先前用 manifest 開過）；只上傳、沒送進任何房的檔用 `--manifest`。
- **412**：資料照寫，但它沒驗過或驗不過（傳統加密附件，/docs/design/rpc-specs/data-plane.md §8.2）→ exit 3。
- 不是資料的回應（404、502…）**一個 byte 都不寫進輸出**，stderr 印 daemon 回的那則錯誤，exit 2。
- 下載的參數都從 manifest 或事件來，**沒有** `--key` 這種零散參數：金鑰不該出現在命令列與 shell 歷史裡。

#### 3.3.1 位置是明文位置，讀的單位是塊（SDK 的 `seek_read`）

rpc-cli **沒有** `seek` 命令了：`get --range` 就是 seek（同一個語意，/docs/design/rpc-specs/rpc-spec.md §3.6），差別是範圍照 HTTP `Range`（`bytes=<首>-<尾>`，含尾、超過檔尾截到檔尾、整段在檔外是 416）。
下面是 SDK `wbf_sdk::download` 的 `seek_read` 的語意，資料平面的讀塊照同一套算（約定 §7）：

`at` 與 `len` 都是**明文**的 byte 位置與長度，跟 chunk 邊界無關；對齊到塊是 seek 自己的事。以 `chunk_size` 64 KiB 為例：

| 要的 | 要的明文範圍 | 實際 `Read` 的塊 | 對塊做的事 |
|---|---|---|---|
| `at` 70K | 70K 起，**到那一塊結尾**（70K–128K，共 58K） | 第 1 塊（64K–128K） | 開頭跳過 6K |
| `at` 70K、`len` 70K | 70K–140K | 第 1 塊、第 2 塊（128K–192K） | 第 1 塊開頭跳過 6K，第 2 塊只留前 12K |
| `at` 64K、`len` 64K | 64K–128K | 第 1 塊 | 剛好一塊，不裁 |

- **不帶 `len`**：讀含 `at` 的那一塊，給 `at` 起到該塊結尾。
- **`len` 跨幾塊就讀幾塊**，第一塊裁頭、最後一塊裁尾，中間的整塊照給。每塊都照約定 §3.1 各自解密驗證，任一塊壞掉就是完整性錯。
- **`len` 超過檔尾**：給到檔尾為止，`truncated: true`。這不是錯誤。
- **`at` 不小於明文總長**：用法錯。
- 結果的 `chunks_read` 列出讀了哪幾塊：`at` 150000000 時只有一個元素，那就是「不必下載前面」的證據。

### 3.4 房間

全部用 `call room.*`（/docs/design/rpc-specs/rpc-spec.md §3.2–§3.5）。rpc-cli 跟 UI 走的是同一套 method，所以 UI 要做的順序，用 rpc-cli 時也要自己照做：

- **送文字只看本地記的加不加密**（/docs/design/keys/e2ee-rpc.md §7）：這間房本地還不知道時是 1100，先 `call room.get '{"room":…,"sync":"both"}'` 一次（記在 `cache.db`，之後每次執行都還在）。
- **加密房**：`call room.refresh_devices` 拿到 `room_devices`，帶著它 `call room.send_text`；被 1401 擋就用錯誤 `data` 裡的新狀態再送一次（同一個 `txn_id`，/docs/design/keys/e2ee-rpc.md §3）。rpc-cli **不**替你重送。

```bash
printf '%s\n' '{"method":"room.get","params":{"room":"!mm4wSdeMnTkkVqycfJ:localhost","sync":"both"}}' | wbf-matrix-rpc-cli --data-dir D -q batch
wbf-matrix-rpc-cli --data-dir D -q call room.send_text '{"room":"!mm4wSdeMnTkkVqycfJ:localhost","body":"hello from rpc-cli"}'
# {"event_id":"$lP0O-3QN257vKAfU86tgi4hTCnLMg5tb4rXFcYfJVJ0"}

DEVICES=$(wbf-matrix-rpc-cli --data-dir D -q call room.refresh_devices '{"room":"!wvQRbpkcdLe0ooYAKO:localhost"}')
# {"members":{"@alice:localhost":"5-a4ab25b7e5","@bob:localhost":"6-41c732f4ba"},"room_version":17803949296791669694}
wbf-matrix-rpc-cli --data-dir D -q call room.send_text "{\"room\":\"!wvQRbpkcdLe0ooYAKO:localhost\",\"body\":\"secret\",\"room_devices\":$DEVICES}"
# {"event_id":"$12PdcD68XUCNHao4iNiFRoEAAF1o-989QCYeERA0csQ"}
```

⚠️ **一次執行讀加密房，別台剛分來的房間金鑰可能還沒到**：金鑰走背景開的 `Keys` 線（/docs/design/daemon/link-pool.md §3），而這支 daemon 才剛起。
上例之後 bob 馬上 `call room.history`，那一則是 `"kind":"undecryptable","undecryptable_reason":"NotDecryptedHere"`；同一串命令放進 `batch`、或先 `watch keys.state` 等它追上再讀。

#### 3.4.1 事件長什麼樣

`room.history`、`room.files`、`room.message` 推播裡的事件，就是 /docs/design/rooms/chat-model.md §2.3 的 `Message` 序列化（`crates/wbf-sdk/src/chat.rs`）：

| 欄位 | 說明 |
|---|---|
| `id`、`conversation`、`sender`、`sent_at` | event_id、room_id、mxid、`origin_server_ts`（毫秒，只當顯示用） |
| `kind` 加它的欄位 | `text`（`body`、`formatted_html`）、`file`（`attachment` = `{ mxc, block }`、`caption`）、`matrix_file`（標準 Matrix 附件：`attachment` = `{ msgtype, mxc, kind, name?, mimetype?, size?, file? }`、`caption`，/docs/design/rooms/chat-model.md §2.3）、`deleted`（`reason`）、`undecryptable`（沒有欄位；`decrypted: false`、`undecryptable_reason` 在訊息上）、`outdated`（沒有欄位；本地快取裡目前的 edit 這個帳號還沒同步到）、`system`（`event_type`、`line`）、`unsupported`（`event_type`、`body`）。認不得的事件不丟 |
| `reply_to`、`edited_by`、`reactions` | 同一頁內的關係事件折進目標（/docs/design/rooms/chat-model.md §3.4） |
| `decrypted` | `true`／`false`／`null`。`null` 表示本來就不是加密事件 |
| `undecryptable_reason` | `decrypted` 是 `false` 才有（example: `MissingMegolmSession`、`NotDecryptedHere`） |
| `r_seq`、`g_seq` | server 發的序號（/docs/design/rooms/chat-model.md §4.3）；非 fork server 的房間沒有 |

```json
{"body":"hello from rpc-cli","conversation":"!mm4wSdeMnTkkVqycfJ:localhost","decrypted":null,"g_seq":495,"id":"$lP0O-3QN257vKAfU86tgi4hTCnLMg5tb4rXFcYfJVJ0","kind":"text","r_seq":12,"sender":"@alice:localhost","sent_at":1795052781312}
```

規則：

- **解不開不是錯誤。** 加密房間裡拿不到 key 的事件照給，`kind: "undecryptable"`、`decrypted: false` 帶原因，`code` 仍是 0。整頁因為一則舊訊息全掛沒有道理。
- **`next` 是 `event_id`（這一頁最舊那則），不落地。** 要接著翻是呼叫者的事。**不**是 server 的翻頁 token（/docs/design/rpc-specs/rpc-spec.md §3.3）。
- **`types`、`senders` 過濾在 core 那一層做**（`crates/wbf-core/src/rooms_ops.rs`）：Matrix 的 `filter` 參數各 server 支援程度不一、server 也不知道我們的 `kind` 名字。過濾後一頁可能是空的但 `next` 不是 null，要照 `next` 判斷有沒有到頭，不是照 `events` 長度。
- **`room.files` 不驗完整性**：它只解區塊、給 manifest，不碰 `Info`。核對是下載的事（約定 §3.1）。
- 不加 `search` 的 client 端版本：server 端全文搜尋走 `call search.events`；加密房要搜就掃 `room.history` 的輸出，那是腳本一行 `jq` 的事。

#### 3.4.2 `watch`：訂閱推播，來一則印一則

`watch [events…] [--user <mxid>] [--count <n>] [--timeout <秒>]`

`subscribe { events, user }`（/docs/design/rpc-specs/rpc-spec.md §3.9）→ 每則推播原樣印一行（JSON Lines、即時 flush）。`events` 不給是全部（`*`）。

| 給了什麼 | 什麼時候結束 | exit |
|---|---|---|
| 都沒給 | Ctrl+C | 0 |
| `--count N` | 收滿 N 則 | 0 |
| `--timeout S` | 時間到（＝看 S 秒） | 0 |
| `--count N --timeout S` | 收滿就 0；時間到還沒收滿 | 5 |

```bash
wbf-matrix-rpc-cli --data-dir D2 -q watch room.message --count 1 --timeout 90
# {"method":"room.message","params":{"message":{"body":"hello from rpc-cli","conversation":"!mm4wSdeMnTkkVqycfJ:localhost","decrypted":null,"g_seq":495,"id":"$lP0O-3QN257vKAfU86tgi4hTCnLMg5tb4rXFcYfJVJ0","kind":"text","r_seq":12,"sender":"@alice:localhost","sent_at":1795052781312},"room":"!mm4wSdeMnTkkVqycfJ:localhost","user":"@bob:localhost"}}
```

- `--user` 要**完整 mxid**：daemon 逐字比對推播的 `user`（localpart 會把全部濾掉）。
- **只看得到訂閱之後的**：訂閱不補先前的訊息；要歷史用 `room.history`。⚠️ 剛起的 daemon 要等背景開線（`daemon.info` 的 `links`）才收得到房間推播，所以 `watch` 起來之後隔幾秒才送的那則才保證收得到。
- **自己送的也印**（`sender` 是自己），腳本自己濾。
- **Ctrl+C**：終端裡 daemon 跟 rpc-cli 在同一個 console，兩個都收到；rpc-cli 照樣 `daemon.shutdown`、等 daemon 結束，關機請求送不出去（daemon 已經在關）不算錯（`session.rs` 的 `finish`）。

### 3.5 本地快取（/docs/design/storage/local-cache-db.md §5）

`cache.db` 在 `s/<b58>_<b58>/`（§7），**同一個 server 上的所有帳號共用一份**，SQLCipher 整檔加密，金鑰是 vault 的第一把子金鑰。**快取不是權威**：schema 版本不對、解不開，開檔時直接刪掉重建。
既有的庫照它自己記的 server 開；記的 host 跟這次的不是同一台就拒絕、庫不動（/docs/design/storage/local-cache-db.md §1）。

多帳號混存怎麼不漏（維護者 2026-09-07 定，細節在 /docs/design/storage/local-cache-db.md §5）：

- 事件只存一份；**誰看得到哪一則逐則記**（`events_synced_log`）：server 經 `/messages`、`/sync`、`Recent` 任一條路給過這個帳號的才算。沒有列就看不到，fail closed。不用「每人一個 r_seq 下界」——離開再加入、`history_visibility` 改過都會切洞，下界會 fail open。
- 所以 user1 與 user2 都在 room1：各自拿過的事件各自看得到；一則兩人都拿過只存一份。**一人解過的明文另一人也讀得到明文**（都是同一台機器上同一個人的帳號，維護者接受）。
- 房間清單、`cg_seq` 水位線、已讀位置、Delete-for-me 都是一人一份。

**`sync.recent` 的三層**（維護者 2026-09-08 定；參數在 /docs/design/rpc-specs/rpc-spec.md 的 `sync.recent`）：

- `max_events` 是**這一輪總共要幾則**（預設 10000，`0` ＝ 拉到追平）；底層拆成一次 `Recent` 一窗 `window` 則（預設 320、server 上限 500 先 clamp）；server 每 `batch` 則回一個 Batch（預設 10、上限 100）。要 1000 就是 320、320、320、40 四窗。
- 每個 Batch 寫一次快取；一窗 `tc == 要的` 就帶 `before = 最後的 ls` 再一窗，`tc < 要的` 是追平（`caught_up`）；湊滿 `max_events` 也停。
- 水位一律是第一窗第一個 Batch 的 `fs`（比它新的全拿到了）；中途斷線或 server 回錯就停、已寫的有效、水位不動。`from_scratch` 不帶 `cg_seq`。只走 WS、server 要有 `recent` feature。

**`account destroy` 的忘掉鏈**（裝置層那一半在 /docs/design/rpc-specs/rpc-spec.md 的 `account.del`）：刪這個帳號的同步紀錄／房間清單／水位線／已讀 → 沒人同步過的事件 → 沒事件指的媒體 → 沒事件也沒清單的房間。
順手刪掉已經沒人用的池檔（DB 先、檔案後；刪不掉只說一聲，`media.gc` 的 sweep 會再收）。**別的帳號也持有的一律不動**（維護者 2026-09-09：扣除別人帳號的持有）。
**這個帳號的 `k/` 與保管著的 recovery key 也一起刪**（/docs/design/keys/room-key-backup.md §7）；最後刪帳號目錄，那台 server 一個帳號都不剩就整個 server 目錄一起刪（維護者 2026-09-15）。

媒體池：`call media.stats`、`call media.gc '{"quota_mib":2048,"protect_days":7}'`（清法照 /docs/design/media/media-pool.md §5）。

### 3.6 房間金鑰備份（/docs/design/keys/room-key-backup.md，維護者 2026-09-09 定）

`call backup.status`／`upload`／`save`／`import`／`restore`／`create_recovery_key`、`call recovery.list`／`show`（/docs/design/rpc-specs/rpc-spec.md 的 `backup.*`、`recovery.*`）。
兩個開關都在 conf 的 `[backup]`（§10），預設都是 `on`。**開關怎麼生效**（`crates/wbf-daemon/src/handle/backup.rs`）：

| method | `SERVER_BACKUP=off` | `LOCAL_ROOM_KEYS=off` |
|---|---|---|
| `backup.upload` | **拒絕**（1100，訊息說是哪個鍵） | 照傳，只跳過順手做的 `save`（`local_snapshot_bytes` 是 `null`） |
| `backup.save` | 照做 | **拒絕**（1100，訊息說是哪個鍵）——**不**靜默跳過：命令是他打的 |
| `backup.status` | 照答，另外回 `server_backup_setting`／`local_room_keys_setting`：前面那些是**上游現在的狀態**、後兩個是**這台機器的設定叫它做什麼**，對不上時要看得出來 | 同左 |

- `backup.create_recovery_key` 產生 recovery key（上游 `recovery().enable()`），**回一次**，同時封進 `<data dir>/r/`（/docs/design/keys/room-key-backup.md §8）。⚠️ 回完 server 那邊拿不回來，只能 reset。它與 `recovery.show` 是唯二會回秘密的 method。
- `account.del`／`account.destroy` 的**閘門**（/docs/design/keys/room-key-backup.md §7）寫成**正面認得**的形式：只有「server backup 開著 ＆ `recovery().state() == Enabled`」**而且**這台保管著這個帳號的 recovery key 才放行；
  其他任何狀態（含 `SERVER_BACKUP=off`、`Unknown`／`Incomplete`、問不到 server、wbf 帳號問不出第一關）一律擋下來（`HistoryWouldBeLost`），要 `accept_history_loss: true` 才走。擋下來的訊息（core 的原文，`crates/wbf-core/src/session_ops.rs` 的 `refusal`）：

  ```text
  this would delete @alice:localhost's room keys on this machine (m/ and k/), and
         <這一次為什麼擋>.
  ```

  core 的訊息只說**條件**、**不**提命令名字；下一步（先 `backup.create_recovery_key`，或決定不要歷史）由前端講（/docs/design/keys/room-key-backup.md §7）。
- 📎 舊 CLI 在登入後、`SERVER_BACKUP=off` 時印的那幾段警告**跟著舊 CLI 拿掉了**：daemon 不印給人看的東西。要提醒就看 `backup.status`（`recovery_enabled` 是 false 而 `server_backup_exists` 是 true ＝ 備份的鑰匙只在這台），那是 UI 的事。

## 4. 輸出與 exit code

🌐 **輸出一律英文**（維護者 2026-09-09 定）：stdout 的 JSON、stderr 的訊息、錯誤、確認提示，全部英文。設計文件與 commit 訊息是中文，面向使用者的字不是。

- stdout：成功才印。`call` 與便利命令是**一行 JSON**；`batch` 與 `watch` 是 JSON Lines；`get` 沒給 `-o` 時是檔案內容。
- stderr：RPC 回錯時是 `error <code>: <msg>`，有 `data` 再一行 `data: <JSON>`；其他是一句英文。
- exit code（RPC 的 `code` 怎麼對到這一欄：/docs/design/rpc-specs/rpc-spec.md §5.2；實作 `apps/wbf-matrix-rpc-cli/src/session.rs` 的 `exit_code_of`）：

| code | 意思 | 從哪來 |
|---|---|---|
| 0 | 成功 | |
| 1 | 用法錯 | 參數、找不到檔、params 不是 JSON、確認沒過、daemon 起不來（資料目錄被佔著）；RPC 的所有 1xx（105 除外）、9001、1001–1299（含 passphrase 錯、帳號不在）、**認不得的碼**（禁止當成成功） |
| 2 | server 或資料平面拒絕 | RPC 1400、1401；資料平面回了不是資料的狀態碼（例 502）；下載推到 `failed`／`cancelled` |
| 3 | **完整性失敗** | RPC 1500、1501；`get` 拿到 412（資料寫了、但沒驗過或驗不過） |
| 4 | 網路 | RPC 1300、9xxx（9001 除外）；跟 daemon 的連線斷了、資料平面連不上 |
| 5 | 等逾時 | RPC 1402（房間金鑰在等待時間內沒準備好，訊息沒送、重送就好）、1600；`watch --count --timeout` 時間到還沒收滿 |
| 130 | 工作被取消 | RPC 105（跟 Ctrl+C 同一個慣例） |

📎 clap 自己的用法錯（少了必填參數、認不得的旗標）也是 1，**不是** clap 預設的 2（那一號在這裡是「server 拒絕」）；`--help`／`--version` 是 0。

⚠️ **SDK 認 wbf 的錯誤碼只看 `code_id`，不看 `code` 名字**（wbfuwunel 的 `/docs/design/wbf-wire-format.md` §3.4；issue #29 第 2 項）。
`code` 那個字串同時裝著 Matrix 的 `errcode`（`M_FORBIDDEN`）與我們自己合成的（HTTP 401 的 `Unauthorized`），
所以只給人看。📎 在程式裡是 `SdkError::wbf_code()` → `WbfErrorCode`（`wbf-sdk/src/error_code.rs`，server 那張表的投影）。
🚨 **不認得的碼**（包括 `0`、表上沒有的號）一律當「失敗了、不知道能不能重試」：**不**重試，原樣往上報 ——
例如上傳只在認得的 `Corrupt`（1002）重送一次、認得的 `OutOfOrder`（1503）跳回 `expected_seq`，其他一律往上報。

📎 **從 Matrix 錯誤來的 `Error`（原生 kind 與走橋的都是）多帶 Matrix 的欄位**（wbfuwunel #56，wbf-wire-format.md §3.4 表下）：
`status`（一定有）、`errcode`、`retry_after_ms`、`soft_logout`（只會是 `true` 或不出現）。SDK 的讀法是
`SdkError::matrix_status()`／`matrix_errcode()`／`retry_after_ms()`／`is_soft_logout()`：**欄位不出現就是沒有值**
（server 2026-09-15 起不再寫 `"soft_logout": false`、`"retry_after_ms": null`），形狀不對一律當沒有；
`is_soft_logout` 只認 JSON 的 `true`（錯判成可以 refresh 的代價比錯判成要重新登入大）。
它們是**附加的原因**（例：`M_USER_LOCKED` 被鎖、`M_UNKNOWN_TOKEN` ＋ soft logout 可以 refresh、沒有 soft logout 要重新登入），
**不**取代上面「認碼只看 `code_id`」。

📎 flags bit4 `IS_BRIDGED`（`0x10`）是定義過的位元（走橋的 Matrix 端點呼叫，server 的回覆也帶它）；bit5–bit7 仍然保留，
收到就是 `ReservedFlags`。

## 5. Manifest：`put` 印的、`get --manifest` 吃的

就是約定 §5 事件區塊的 JSON，外加 `mxc` 與 `server`（`crates/wbf-sdk/src/manifest.rs`）：

```json
{"block":{"chunk_size":65536,"cipher":"aes-256-gcm","file_size":300000,"key":"<base64 的檔案金鑰>","name":"stdin.bin","nonce_base":"<base64>","sha256":"c4f8c7506409c26d554b38c0f5da0d7d3afeb00f6cb253966a5fddf95003ac22","v":1},"mxc":"mxc://localhost/2ab68f6ab89375","server":"http://127.0.0.1:6167"}
```

- `block` 逐字就是 `room.send_attachment` 放進事件區塊 `org.wbftw.wbfuwunel.chunked` 的東西；沒送進房間時，manifest 就是「事件」的替身。
- 明文模式（`cipher` 是 `none`）沒有 `key`、`nonce_base`。
- 含 `key`，所以 manifest 要當機密：rpc-cli 印到 stdout，存不存檔、存在哪是呼叫者的事。SDK 寫檔時權限只給自己（`wbf_sdk::vault::write_private`：Unix `0600`；Windows 靠使用者目錄的 ACL）。

## 6. 上傳狀態檔（續傳用）

`<file>.wbf-upload.json`，放在被上傳的檔旁邊：`call upload.file` 與 `call room.send_file`（路徑版，daemon 自己讀檔）才有；資料平面的 `put` 沒有（PUT 一次傳完，沒有「同一條再叫一次」）。

```json
{ "server": "…", "user_id": "@a:localhost", "upload_id": 1234605616436508552, "mxc": "…", "chunk_max_bytes": 69632,
  "block": { "v": 1, "cipher": "…", "key": "…", "nonce_base": "…", "chunk_size": 65536, "file_size": 132056,
             "name": "video.mkv", "mimetype": "video/x-matroska" } }
```

- `block` 與 manifest 的 `block` 同一個形狀（SDK 的 `ChunkedBlock`），只是還沒有 `sha256`；Seal 時填上就變成 manifest 的 `block`。
  塊數不存：從 `file_size` 與 `chunk_size` 算得出來。這是 SDK 的 `UploadState`，`Create` 一回來就寫檔。
- 含 `key`，同 manifest 的保護。Seal 成功或 `upload.abort` 後刪。
- 狀態檔的 server 與 user 跟現在的不一樣 → 拒絕，不拿 A server 的上傳去打 B server。
- 續傳時 SDK 對 server 的 `Status` 有兩種修正：狀態檔比 server 舊（重送已收的塊）server 回冪等 Ack、跳到 `received`；
  比 server 新（從沒收到的塊開始）server 回 `OutOfOrder`、跳回 `expected_seq`。所以呼叫端只要「從 `Status.received` 送」，不必自己算。
- 📎 狀態檔放在被上傳的檔旁邊是一命令一程序時定的；daemon 模型下要不要搬是 /docs/design/overview/architecture-v2.md §7 第 9 點的問題。

## 7. 資料目錄：一台機器一把鑰、一個 server 一份快取、一個帳號一套 session

> 用字（維護者 2026-09-07 定）：**passphrase** 是解 `local.key` 的那句話，只存在這台機器；**password** 一律指 Matrix 帳號密碼，只有 `account add` 用一次。旗標、命令、錯誤訊息、文件都照這個分。

資料目錄裡每個檔（`local.key`、`current`、`wbf.conf`、`daemon.json`、`daemon.lock`、`r/`、`s/<server>/cache.db`、`media/`、`a/<帳號>/session.sealed`、`m/`、`k/`）的
內容、格式、加密、誰寫誰刪：**/docs/design/storage/local-storage.md**（目錄樹在 §3，只畫在那裡）。預設位置也在那裡（§1）。

- **兩層目錄名都是加密的**（/docs/design/storage/vault-and-keys.md §2）：`<base58 nonce>_<base58 密文>`，底線分隔（Base58 字母表沒有 `_`）。
  ⚠️ 中間那幾段（`r`／`s`／`a`／`m`／`k`）只有一個字母：兩段加密名字就吃掉 106 字元，
  而 Windows 的 `MAX_PATH` 是 260（/docs/design/storage/vault-and-keys.md §2.4.1，2026-09-09 實測撞到）。要知道是哪家、是誰得解密，所以連 `account.list` 都要先解鎖（/docs/design/storage/vault-and-keys.md §2.6）。
- `local.key` 為什麼在頂層不在帳號底下：主金鑰的定位是「這台機器」（/docs/design/storage/vault-and-keys.md §1），passphrase 也是一台機器一個。
- `cache.db` 為什麼在 server 層：`r_seq`／`g_seq` 是 fork server 發的，同一個 room 在不同 homeserver 上序號不同；共用範圍就是同一個 server 的帳號（§3.5）。
- **明文目錄名的舊佈局一律不遷移**（維護者 2026-09-09）：一個都解不開時提示刪掉 data dir 重新加帳號（/docs/design/storage/vault-and-keys.md §2.7）。
- **token 不在資料目錄裡**：`daemon.json` 只有 port、pid、`instance`（/docs/design/rpc-specs/local-interface.md §3）。

任何輸出、log、錯誤訊息都**不**印 `access_token`、主金鑰、子金鑰、passphrase、password、daemon 的 token。

### 7.1 解鎖：每次執行都要；**沒有** unlock ticket（維護者 2026-09-13）

每次執行是一支新的 daemon，所以 `local.key` 是 `passphrase` 模式時**每次都要 passphrase**：`--passphrase-file` → 問終端（不回顯）；stdin 不是終端就拒絕。`plain` 模式不問。
還沒有 `local.key` 時不解（要它的 method 自己會回 1002）；`account add` 會先建。

**不**為了省打字讓明文主金鑰落地（沒有 `unlock.ticket`、`lock` 命令、`--unlock-ttl`、conf 的 `UNLOCK_TTL`）。

- ⭐ 理由：常駐的 daemon 解鎖一次就夠（/docs/design/overview/architecture-v2.md §1），rpc-cli 是 debug／test 用、一次一支，省下的那幾次打字**換不到「明文主金鑰落地」**。
- 📎 **UI 的 lock 不是這個東西**：UI 有自己的 lock／unlock，但它鎖的是 UI 那一層，**不**動 daemon（/docs/design/rpc-specs/rpc-spec.md §3.1）。真正要讓金鑰離開記憶體就是 `daemon.shutdown`。

## 8. 驗收腳本

`scripts/acceptance.sh`（Windows 跑 Git Bash），對著本機 wbfuwunel 用 rpc-cli 驗上傳、下載、Range、續傳、串流：

1. `account add`，`call server.ping` 看 features 有 `upload`、`download`。
2. 產生 200 MiB 隨機檔 → `call upload.file` → 標準 `GET /_matrix/client/v1/media/download/…` 拿整份 → 長度必須是 `file_size + 塊數 × 16`（明文模式不加）。
   逐 byte 的密文比對在 `crates/wbf-sdk/tests/e2e_local_server.rs`（rpc-cli 沒有印密文的命令，也不該有）。
3. `get --manifest` → 與原檔 `cmp`。
4. `get --range bytes=150000000-150004095` → 與 `dd` 從原檔切的同一段 `cmp`。
5. `call upload.file` 上傳到一半，把 rpc-cli **與它起的 daemon**（pid 在 `<data dir>/daemon.json`）一起殺掉 → 狀態檔（§6）還在、`call upload.status` 的 `finished` 是 false → 再叫同一條 `upload.file` → 從斷的地方接著傳 → 結果同第 3 條。
   ⚠️ 只殺 rpc-cli 不夠：Windows 上子程序不會跟著死，daemon 會把上傳做完（而且佔著資料目錄）。`upload.*` 只走 WS（`transport: http` 是 1100），所以檔要夠大才來得及殺。
6. `cat 原檔 | put - --name …` → 同第 3 條。
7. 三種 `--cipher` 各跑一次第 2、3 條。

跑法：`WBF_PASSWORD_FILE=<檔> scripts/acceptance.sh`；`WBF_ACCEPT_SIZE_MIB` 可以縮小檔案快速跑（續傳那條檔太小會在殺之前就傳完，腳本會講）。

## 9. 明確不做的

- 沒有互動模式（REPL）、沒有進度條：一串命令用 `batch`。
- rpc-cli 自己的重試、輸出改形、串動作（例：加密房 1401 的重送）：它是薄封裝（/docs/design/rpc-specs/rpc-cli.md §8）。
- 附掛到別的前端起的 daemon（/docs/design/rpc-specs/rpc-cli.md §1 的丙）。
- 不存密碼，只存 token（`session.sealed`）。
- `backup` **不**做「刪掉 server 上的 backup version」（不可逆，而且會連帶其他裝置，/docs/design/keys/room-key-backup.md §9）。
- conf **不**改寫已經存在的檔、不寫秘密（§10.3、§10.5）。
- **不**把目錄名的對照表落地成明文索引（/docs/design/storage/vault-and-keys.md §2.5）：那等於把剛加密的東西再寫一次明文。
- **不**把 `--password-file` 也改成原始 bytes（/docs/design/storage/vault-and-keys.md §3.4）：password 要送給 server，它本來就是字串。

## 10. conf 檔：不用每次指定環境變數（維護者 2026-09-09 要求）

conf 是 **daemon** 讀的（`crates/wbf-daemon/src/settings.rs`）；rpc-cli 自己不讀。

### 10.1 在哪、長怎樣

檔名 `wbf.conf`，放在**資料目錄裡**（`<data dir>/wbf.conf`）。daemon 找的順序：

1. `--config <path>`（或 `WBF_CONFIG`）明指的那個（找不到就報錯，**不**默默 fallback —— 明指了還讀到別的比讀不到更糟）。
2. `<data dir>/wbf.conf`。
3. 都沒有就全用預設值。

daemon 第一次在一個資料目錄起來時自動生成的那份（§10.3，實跑）：

```ini
; wbf.conf —— daemon 第一次在這個資料目錄起來時自動生成的一份起手式（/docs/design/rpc-specs/rpc-cli.md §4）
; 每個值後面註明它這次是哪來的。改這個檔不影響已經登入的帳號。
; 秘密不放這裡：token、password、passphrase 一律不從這裡讀。
; 已經存在的 wbf.conf 永遠不會被改寫——要重生成就先自己刪掉。

[general]
TRANSPORT=ws   ; built-in default

[read]
READ_RECEIPTS=private   ; built-in default

[backup]
SERVER_BACKUP=on   ; built-in default
LOCAL_ROOM_KEYS=on   ; built-in default
```

| 鍵 | 區段 | 值 | 意思 |
|---|---|---|---|
| `TRANSPORT` | `[general]` | `ws`（預設）／`http` | RPC 沒帶 `transport` 時用哪條（/docs/design/rpc-specs/rpc-spec.md §2）。改了要 daemon 重開才生效 |
| `READ_RECEIPTS` | `[read]` | `private`（預設）／`public` | 已讀回執送上游時公不公開（/docs/design/messages/read-receipts.md §5） |
| `SERVER_BACKUP` | `[backup]` | `on`（預設）／`off` | 標準 Matrix key backup（/docs/design/keys/room-key-backup.md §3）；怎麼生效在 §3.6 |
| `LOCAL_ROOM_KEYS` | `[backup]` | `on`（預設）／`off` | 本地加密金鑰快照 `k/`；同上 |

- **區段**：`[名稱]` 一行一個，之後的鍵都屬於它。鍵名在區段內唯一；同一個鍵出現兩次以後面的為準（並印一行警告）。
- **鍵名就是環境變數去掉 `WBF_` 前綴**（之後真的給某個鍵加環境變數時照這條，§10.2）。
- **註解**：`;` 或 `#` 起，到行尾。⚠️ 只有**行首**或**前面是空白**時才起註解 —— `PASSWORD_FILE=/tmp/a#b` 裡的 `#` 是值的一部分。
  要讓值以 `#` 開頭或含前後空白，用雙引號包住整個值（`KEY="  # 這是值  "`）。
- 值前後的空白去掉；空值（`KEY=`）**當作沒寫**，不是空字串（全域 CLAUDE.md：佔位值不用空字串）。
- `call daemon.reload_conf` 重讀：值變了、換上去就生效的鍵當場換；要重開才載入的（`TRANSPORT`）列在 `needs_restart`。

### 10.2 優先序：旗標 > 環境變數 > conf > 內建預設

維護者 2026-09-09 定：**環境變數優先於 conf**。命令列旗標仍然最高（它是「就這一次」的意思）。每個值各自比一次，不是整份取代。

daemon 認得的環境變數只有三個：`WBF_DAEMON_TOKEN`（token，/docs/design/rpc-specs/local-interface.md §3）、`WBF_DATA_DIR`（`--data-dir`）、`WBF_CONFIG`（`--config`）。
📎 這三個**都不是 conf 的鍵**，所以今天 daemon 上**沒有任何一個值同時來自環境與 conf**，這條規則在那邊還沒有實際的比較對象。
**沒有** `WBF_SERVER`／`WBF_ACCOUNT` 那一類：daemon 同時服務所有帳號，「對誰動作」是每則 RPC 的 `params`（/docs/design/rpc-specs/rpc-spec.md §2），不是啟動時的環境。

⚠️ **之後要給某個 conf 鍵加環境變數，就加在 clap 的 `env = "WBF_…"` 上**（旗標 > 環境的順序 clap 免費給），禁止在 `Settings::load` 裡自己讀 `std::env` —— 那會繞過旗標、也會讓同一條優先序有兩個實作。

### 10.3 自動生成

維護者 2026-09-09 要求：**資料目錄裡自動生成一份 conf，把當前的值寫進去。** A-2 起由 daemon 做（以前是舊 CLI，/docs/design/rpc-specs/rpc-cli.md §4）：

- 觸發條件：daemon `-s` 起來、**拿到資料目錄的寫權之後**、沒用 `--config` 指別的檔、`<data dir>` 下**還沒有** `wbf.conf`。
  daemon 起不來（token 不對、資料目錄被佔著、沒帶 `-s`）就不寫。
- 寫進去的是**這次實際生效的值**，每個值後面附一行註解說它是從哪來的；stderr 說一聲寫了哪個檔。寫不成只說一聲、照樣跑（沒有它就是內建預設）。
- 已經存在的 `wbf.conf` **永遠不改寫**（連補鍵都不做）：那是使用者的檔，不是我們的狀態檔。要重生成先自己刪掉。
- **不**寫任何秘密，也不寫「秘密在哪」的路徑（§10.5）。

### 10.4 認不得的東西怎麼辦：不是正面認得就落到安全值

- **認不得的區段或鍵**：印一行警告到 stderr，忽略它，照跑。（conf 要往前相容：舊版讀到新版寫的鍵不該整個掛掉；舊 CLI 寫過的 `SERVER`、`ACCOUNT`、`[media]`、`[recent]` 也落在這裡。）
- ⚠️ **`READ_RECEIPTS` 的安全值是 `private`**：只正面認得 `public` 才公開，其餘一律 private
  （拼錯、空的、認不得的值都是）。⭐ 壞在「別人看不到你已讀」，**不**壞在「你被看見了」——
  後者不可回收（/docs/design/messages/read-receipts.md §5）。
- **認得的鍵、認不得的值**：⚠️ 開關型的鍵（`SERVER_BACKUP`、`LOCAL_ROOM_KEYS`）**只正面認得 `on` 與 `off`**（不分大小寫）；
  其他任何值（`true`、`1`、`yes`、拼錯的 `of`）一律警告並落到**安全值**，也就是 `on`。
  判斷一律寫成「**正面認得 `off` 才關**」，禁止寫成「不等於 `on` 就關」—— 壞掉的時候要壞在「備份還開著」那一邊，不是「以為開著、其實沒開」。
- **數值型的鍵**（之後加的）：parse 不出來就警告並用內建預設，不用半套的值。
- conf 檔本身壞掉（不是 UTF-8、區段語法錯）：**報錯、daemon 不啟動**，不當作沒有這個檔。設定檔讀一半比讀不到危險。

### 10.5 conf 不放什麼

| 不放 | 理由 |
|---|---|
| 任何 token（daemon 的、Matrix 的 `access_token`） | 秘密**不**落地在明文檔裡 |
| passphrase、password 本身 | 同上，而且 §2 已經定了秘密只能從檔案或終端來 |
| `PASSWORD_FILE`／`PASSPHRASE_FILE` | 那是 rpc-cli 的參數（環境變數 `WBF_PASSWORD_FILE`／`WBF_PASSPHRASE_FILE`），daemon 不讀 conf 裡的它們；自動生成也不替使用者決定秘密放哪 |
| 帳號狀態（`current`、session） | 那是狀態不是設定，各有各的檔（§7）。設定檔可以刪掉不影響登入。⚠️ `current` 的**內容一樣要加密**（§7：兩層都寫加密後的目錄名）—— 它不在 conf 裡，但不能因此漏了 |

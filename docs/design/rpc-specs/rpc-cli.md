# rpc-cli：命令列改走 RPC（草稿，等維護者點頭）

> 這份回答：`apps/wbf-cli` 怎麼從「直接叫 `Core`」變成 rpc-cli（從另一個程序丟 RPC 給 daemon）、它跟 daemon 怎麼拿到同一把 token、命令面留下什麼。
> 對應 /docs/handover.md §7 第 3 點的後半、/docs/design/overview/architecture-v2.md §0.2／§4／§6／§6.1、/docs/design/daemon/task-supervision.md §3 的 A-2；本 repo issue #26 的 CLI 那半也在這裡。

## 0. 為什麼要做

- **現在的 CLI 繞過 daemon**：`apps/wbf-cli`（2359 行）解析參數之後直接叫 `Core`，碰資料目錄之前先拿 `daemon.lock` 的排他鎖（`apps/wbf-cli/src/commands.rs` 的 `lock_data_dir`）。
  所以 daemon 跑著時它整支被拒，連唯讀的命令也一樣。
- **維護者 2026-09-30**：「不拿 `daemon.lock` 卻寫資料是非法侵占。」前端只能發 RPC 請 daemon 改（/docs/design/overview/architecture-v2.md §0.2）。
- **定位早就定了**（同一份 §0.2）：rpc-cli 是「從另一個程序丟 RPC 進來，測整條路有沒有通」，命令列只是 RPC 訊息的薄封裝，**不**長自己的邏輯，「它可以很小」。
- ⭐ **順帶的收穫**：它會是第一個**正式的**外部 RPC client。現在所有走 WS 的測試都是測試碼裡各自拼的 client（`crates/wbf-daemon/tests/` 的 `loopback.rs`、`process.rs`、`real_server.rs` 各一份）。
  rpc-cli 走得通的路 UI 就走得通；它走得彆扭的地方，就是 UI 會痛的地方。

## 1. 擋路的事：rpc-cli 拿不到別人那支 daemon 的 token

/docs/design/rpc-specs/local-interface.md §3：token 由**叫起 daemon 的那個前端**產生，交給 daemon 之後就抹掉，此後只活在兩個程序的記憶體裡；`daemon.json` 只有 port、pid、`instance`，**沒有** token。

所以：

| 情況 | 結果 |
|---|---|
| 沒有 daemon 在跑 | rpc-cli 自己起一支（它就是那個「叫起 daemon 的前端」），token 它自己有 |
| 別的前端（例：Desktop）起的 daemon 在跑 | rpc-cli **連不上**（沒 token），也**起不了**第二支（排他鎖，/docs/design/overview/architecture-v2.md §0.2） |

/docs/design/overview/architecture-v2.md §4 寫的「連不到就自己 spawn 一個（使用者無感）」只解了第一列。

| 選項 | 做法 | 評估 |
|---|---|---|
| **甲**（建議） | **一次執行＝一支 daemon**：產生 token → spawn `daemon -s --token-file` → 等 ready → 抹掉 token 檔 → `hello` → `vault.unlock` → 命令 → `daemon.shutdown` → 等它結束。第二列照現在一樣拒絕，訊息講清楚「別的前端起的 daemon 在跑，請用那個前端或先關掉它」 | 完全照 /docs/design/rpc-specs/local-interface.md §3 的五步，不新增任何信任面。代價見下 |
| 乙 | rpc-cli 起一支常駐的、把 token 存檔給之後的 rpc-cli 用 | 推翻 /docs/design/rpc-specs/local-interface.md §3 第 5 步（token 不落地）。不建議 |
| 丙 | daemon 加「第二個前端附掛」的機制（另一把 token、或 OS 層的認證） | 這是 /docs/design/overview/architecture-v2.md §7 第 4 點（daemon 的生命週期、多個前端誰說了算），那裡寫明「等 Desktop 的實際使用模式出來再定」。不在這支做 |

**甲的代價**：每次執行都要起 daemon、解鎖（有 passphrase 時要跑金鑰衍生）；解鎖之後 daemon 會在背景開線（每個 wbf 帳號五條，/docs/design/daemon/link-pool.md §3.1），關機時最多等它 5 秒。

- 批次模式（§2.1 的 `--stdin`）讓一串命令共用一次起動，腳本與驗收主要走這條。
- 📎 考慮過「rpc-cli 起的 daemon 不在背景開線」的旗標，**這支不做**：收金鑰的 `Keys` 線與收房間推播的 `Rooms` 線沒有命令會用，只有背景開線會開（/docs/design/daemon/link-pool.md §3 的「訂閱線」），
  關掉它們，別台裝置分來的房間金鑰就收不到，讀加密房會看到解不開的訊息。先量實際的起動時間，嫌慢再談。

## 2. 命令面：一個通用的 `call`，加上只有前端做得到的幾件事

### 2.1 核心：`call`

```text
wbf-matrix-rpc-cli [--data-dir D] [--passphrase-file F] call <method> [params JSON]
wbf-matrix-rpc-cli [--data-dir D] [--passphrase-file F] --stdin      # 每行一個 {"method","params"}
```

- `call`：送一則 RPC，`result` 印到 stdout（一個 JSON 物件），錯誤印到 stderr、exit code 照 /docs/design/rpc-specs/rpc-spec.md §5.2 那一欄（已經定好，例：`105` → 130、1402 → 5）。
- `--stdin`：每行一則請求，每則回應印一行（JSON Lines，帶 `id` 與 `code`）。遇到第一個錯就停，exit code 用那個錯的。
- 解鎖在送命令之前做：`--passphrase-file`，沒給而 vault 要 passphrase 就問終端；`plain` 模式不問（/docs/design/rpc-specs/local-interface.md §5：daemon 不自己問終端）。
- `id` 由 rpc-cli 發，使用者不用管。

### 2.2 便利命令：只收「RPC 本身做不到、一定要在前端這邊做」的事

| 命令 | 為什麼要在前端 | 做什麼 |
|---|---|---|
| `account add <mxid> [--password-file F]` | 密碼要從終端或檔案讀（daemon 不問終端） | 讀密碼 → `account.add { password }`（沒有 `local.key` 時先 `vault.create`） |
| `account destroy <mxid> [--yes]` | 不可逆的確認在前端問（`crates/wbf-daemon/src/handle/accounts.rs` 檔頭：RPC 不問） | 問一次 → `account.destroy` |
| `put <file\|-> [--name] [--mimetype] [--cipher] …` | 讀本地檔（或 stdin）、走資料平面 | `media.create` → `PUT` 到它給的 URL → 印 manifest（/docs/design/rpc-specs/data-plane.md 的兩步） |
| `get <mxc\|--manifest F> [-o OUT] [--range START-END]` | 寫本地檔或 stdout | `media.open` → `GET`（有 `--range` 就帶 `Range`） |
| `watch [events…] [--count N] [--timeout S]` | 推播是串流 | `subscribe` → 每則推播一行 JSON Lines，直到 Ctrl+C、收滿 `N` 則或逾時 |

其他全部用 `call`。

### 2.3 現在的命令去哪

| 現在的命令 | 之後 |
|---|---|
| `account status`／`switch`／`del`、`whoami`、`ping`、`status`、`abort`、`info`、`media-stats`、`media-gc`、`rooms`、`read`、`files`、`recent`、`key-backup …`、`recovery …`、`remove-passphrase` | `call <對應的 method>`（對照表在實作時寫進 rpc-cli 的規格，例：`rooms` ＝ `call room.list '{"sync":"both"}'`） |
| `set-passphrase` | `call vault.set_passphrase`，新 passphrase 照舊從檔案或終端讀（便利命令的一部分，跟解鎖同一段程式） |
| `login`／`account add` | §2.2 的 `account add` |
| `account destroy` | §2.2 |
| `upload` | `put`；或 `call upload.file '{"path":"<絕對路徑>"}'`（daemon 讀那個檔、有續傳狀態檔） |
| `upload --stream` | `put -`（stdin 走資料平面）。core 的 `upload_stream` 本來就註明禁止開成 RPC（`crates/wbf-core/src/misc_ops.rs`） |
| `download` | `get`；或 `call media.export_to`（daemon 寫檔、有完成時的驗證） |
| `download --no-cache` | `call media.export_to '{"no_cache":true,…}'`（照樣排隊，只是不留池裡那份）。直寫的 `Core::download_direct` 沒有別的呼叫點就刪 |
| `seek` | `get --range` |
| `watch` | §2.2 的 `watch room.message`（`--since`、`once` 這些模式拿掉：那是前端邏輯） |
| `send --text` | `call room.send_text`。⚠️ 現在的 CLI 在加密房會自己做 `room.get` → `room.refresh_devices` → 送 → 1401 重試一次，那是前端邏輯，rpc-cli **不**做；要的人照順序叫三次 `call` |
| `send --file` | `put` ＋ `call room.send_attachment`（＝ UI 的兩步），或 `call room.send_file '{"path":…}'` |
| `files --save DIR` | 拿掉：RPC 的 `room.files` 不收 `save`，存檔是前端的事 |
| `--token` 模式 | **拿掉**。它是「不碰 vault、不碰帳號目錄」的路，在 daemon 模型下沒有對應（/docs/design/overview/architecture-v2.md §6.1 留的問題，答案是不要了） |

📎 現在的 CLI 有一些輸出的改形（例：`account switch` 印成 `{ok,current,switched_from}`），之後一律印 RPC 原本的 `result`，不改形。

### 2.4 名字

照 /docs/design/overview/architecture-v2.md §0.1 早就定的：改走 RPC 的這一步才改名。binary 叫 `wbf-matrix-rpc-cli`，目錄 `apps/wbf-matrix-rpc-cli`。
`hello` 的 client 名稱要 `wbf-matrix` 開頭（`crates/wbf-daemon/src/protocol.rs` 的 `CLIENT_NAME_PREFIX`），剛好符合。

## 3. client 程式放哪：`wbf_daemon::client`

一份就好：起 daemon（token 五步、讀 ready 那一行）、連線、`hello`、`call`、收推播、資料平面的 `PUT`／`GET`。

- 放在 `wbf-daemon` 的 library 裡（建議）：加解密（`pack.rs` 的 `RpcKeys`、`seal`、`open`）、`protocol.rs`、`message.rs`、`token::shred` 都已經在這裡，而且是公開的；client 那半只是另一邊。
  /docs/design/overview/architecture-v2.md §0.2 說「之後 Rust 端要用 RPC 的時候接口直接重用」，指的就是它。
- 測試裡那三份自己拼的 client 換成它：三份變一份（全域 A4）。
- 代價：rpc-cli 要依賴 `wbf-daemon` 的 library（會帶進 hyper 那些）。rpc-cli 本來就要 spawn 同一包的 daemon，可以接受。
  另一條路是拆一個 `wbf-rpc-client` crate，等真的有第二個 Rust 前端再拆不遲。
- 找 daemon 的執行檔：rpc-cli 同一個目錄的 `wbf-matrix-client-daemon`（Windows 加 `.exe`），`--daemon-path` 可以覆寫。

## 4. 跟著改的

- **conf 自動生成**：現在是 CLI 呼叫 `wbf_core::conf` 寫 `wbf.conf`（/docs/design/rpc-specs/wbf-cli-spec.md §10.3）。改成 daemon `-s` 起來時做，規則不變（不存在才寫、不改寫既有的、不寫秘密）。
- **daemon 的單發模式**（`daemon <命令>`，/docs/design/overview/architecture-v2.md §0.2 寫「還沒做」）：甲之後「腳本裡跑一條命令」由 rpc-cli 的 `call` 做，單發模式就沒有使用者了。建議改成**不做**、從文件拿掉「還沒做」。
- **core 裡只剩 CLI 在用的方法**（`download_direct`、`upload_stream`、`seek_read`、`watch` 等）：實作時逐支查，沒有別的呼叫點就刪。
- **文件**：
  - /docs/design/rpc-specs/wbf-cli-spec.md 改寫成 rpc-cli 的規格（§3 命令、§4 exit code、§8 驗收、§10 conf）。
  - /docs/design/overview/architecture-v2.md §0.2、§4、§6、§6.1 改現況。
  - `scripts/acceptance.sh` 改用新命令。
- **規格逐支核對**：順便把 /docs/design/rpc-specs/rpc-spec.md 每支 method 的參數與預設值對一次實作，規格沒寫清楚的補上。
  - 📎 `room.list`／`room.history`／`media.info` 的 `sync` 預設是 `local` 是**刻意的**：UI 先拿本地的立刻顯示、再 `both` 補一次，刷新兩次（/docs/design/daemon/daemon-runtime.md §3.4）。
    舊 CLI 每次都帶 `both`／`server`，只因為它是從零開始的單發工具、沒有「先顯示一次」那一步；rpc-cli 的 `call` 照 RPC 的預設，要哪一種由使用者在 params 帶。
  - `room.files` 不收 `save`：存檔是前端的事，規格上寫明。

## 5. issue #26（登入時偶發 panic）

- **CLI 那半跟著消失**：rpc-cli 不再自己開 `Core` 與 runtime，那個「CLI 結束時 matrix-sdk 的寫入還在半路」的程序就不存在了。
- **daemon 那半要老實講**：一般 Matrix 帳號的 matrix-sdk `Client` 是每次用時 `restore`、用完就丟（`crates/wbf-core/src/handles.rs` 的 `backend_of`），丟掉之後它在背景的 store 寫入，daemon 關機時理論上一樣可能在半路。
  而本機測試 server 對一般 Matrix 帳號那條路碰不到（登入先探活、一定走 wbf），**這裡驗不到**。
  建議：A-2 合併後在 #26 寫明 CLI 那半已經沒有了，daemon 那半留著，等有一般 Matrix 帳號的測試環境再查。

## 6. 要維護者決定的

1. **拿 token 的方式**：甲（一次執行一支 daemon；別的前端的 daemon 在跑時拒絕）。建議甲。
2. **命令面**：一個通用 `call`（加 `--stdin` 批次）＋ §2.2 的五個便利命令；`send --text` 的加密房流程、`--token` 模式、`files --save`、`watch` 的各種模式拿掉。建議照這樣。
3. **client 放哪**：`wbf_daemon::client`，測試的三份換成它。建議放這裡。
4. **daemon 的單發模式**：改成不做。建議不做。

## 7. 驗收

- 對真 server 用 rpc-cli 走一輪：`account add` → `call room.list` → `call room.send_text` → 另一個帳號的 `watch room.message` 收到 → `put` ＋ `call room.send_attachment` → `get` 拿回來跟原檔 `cmp` → `get --range` 跟 `dd` 切的同一段 `cmp`。
- `scripts/acceptance.sh` 改寫後照原本的七條跑過（200 MiB、三種 cipher、`put -` 從 stdin）。
- 別的 daemon 佔著資料目錄時：rpc-cli 拒絕、exit 1、訊息講清楚、資料目錄沒有多出任何東西。
- **token 檔不留**：每次執行完（含 daemon 起不來、rpc-cli 被 Ctrl+C）資料目錄裡都沒有 token 檔。
- **daemon 不留**：每次執行完 daemon 程序都結束了（含 `watch` 時按 Ctrl+C：rpc-cli 送 `daemon.shutdown` 再走）。
- 測試的三份 client 換成 `wbf_daemon::client` 之後，loopback／process／real_server 全綠。
- 規格核對：/docs/design/rpc-specs/rpc-spec.md §10 的每一列對過 dispatch 表。

## 8. 不做的

- 附掛到別的前端起的 daemon（§1 的丙，/docs/design/overview/architecture-v2.md §7 第 4 點）。
- 互動式 REPL：`--stdin` 批次已經夠腳本用。
- rpc-cli 自己的重試、進度條、輸出改形：它是薄封裝。

📎 規模估計：rpc-cli 約 600 行、`wbf_daemon::client` 約 400 行；刪掉舊 CLI 約 2000 行，以及 core 裡沒人用的幾支方法。

# 架構 v2：daemon、RPC、與四個前端

> 維護者 2026-09-09 定的方向。這份文件講**分層與介面**，不講功能——功能在
> [`plan-v1.md`](plan-v1.md)、[`chat-model.md`](chat-model.md)、[`local-cache-db.md`](local-cache-db.md)、
> [`wbf-cli-spec.md`](wbf-cli-spec.md)。RPC 的逐條訊息在 [`rpc-spec.md`](rpc-spec.md)（2026-09-12 第一版）。

## 0. 一句話

```
wbfuwunel ──wbf-pack（二進位）──> daemon ──127.0.0.1 加密的 JSON over WS ＋ HTTP──> rpc-cli / Desktop / Android / Python
```

**daemon 是本體**：所有通訊協議、加解密（matrix-sdk）、客製的 wbf 協議、本地資料、session 都在它裡面。
前端一律是**薄包裝**，只講兩件事：本地的 RPC（控制，加密的 JSON over WS）與本地的 HTTP（媒體）。

## 0.1 兩個 binary 的名字（維護者 2026-09-10 定）

⚠️ 拆開之後**兩支 binary 都有命令列介面**，所以「CLI」這個詞從此不指任何一個——
🚫 文件裡不要單獨寫 CLI，那會同時包含兩者。

| 角色 | 正式名稱 | 文件裡寫 |
|---|---|---|
| 常駐、持有本地資料庫、與 homeserver 連線 | `wbf-matrix-client-daemon` | **daemon**（只有在講「用命令列直接叫它起來除錯」那一面時才寫 `daemon-cli`） |
| 下指令的前端，走 RPC | `wbf-matrix-rpc-cli` | **rpc-cli** |

📎 為什麼不叫 kernel（初稿的名字）：跟 Linux kernel 打架。
📎 為什麼簡稱不縮成 `rpc`：RPC 是**協議**的名字（這一章、`rpc-spec.md`、「走 RPC 還是 uniffi」），
拿去當 binary 的名字之後，「RPC 掛了」就分不出是協議還是那支程式——正是這一節要根除的那種語病。
📎 `server` 這個字在這個 repo 已經指過 homeserver 與 wbfuwunel，🚫 不要再拿它指這裡的任何東西。

✅ **2026-09-12（PR #24）：搬完了。** 命令的「做什麼」全部在 `crates/wbf-core`，
`apps/wbf-cli` 只剩「解析參數 → 叫一個 core 方法 → 印 JSON」。⚠️ **目錄名還叫 `wbf-cli`**：
改名留到它真的變成 RPC 前端的那支 PR，🚫 現在改只會製造一次沒有內容的大 diff。

## 0.2 daemon 的命令列就是 RPC 的內部入口（維護者 2026-09-12 定）

> ✅ **2026-09-13：獨佔真的實作了**（`crates/wbf-daemon/src/lock.rs`）。對 `<data dir>/daemon.lock`
> 拿一把 OS 層的鎖（`std::fs::File::try_lock`／`try_lock_shared`，Rust 1.89 起在標準庫裡），
> **拿不到就不啟動**，而且這一步排在碰那個目錄任何東西之前（尤其是刪舊的 `daemon.json`）。
>
> | 誰 | 拿哪一種 | 效果 |
> |---|---|---|
> | **daemon**（會寫） | 排他 | 這個目錄只有我在寫；別的 daemon 起不來，唯讀工具也要等 |
> | **唯讀工具**（只看不改） | 共享 | 多個可以同時讀，但這期間不准有人寫 |
>
> ⭐ **鎖只是事實，「能力」才是前提**（維護者 2026-09-13）。daemon 裡有一個全局狀態
> `WriteAccess`：**起手是 false**（沒有寫的能力）。
>
> ```text
> 要寫  ── is_granted()? ── true ─> 動手
>                        └ false ─> 去拿排他鎖 ── 成功 ─> 設成 true，動手
>                                              └ 失敗 ─> 回 109「我沒有寫的權限」
> ```
>
> | 起法 | 什麼時候拿 |
> |---|---|
> | `-s` | **啟動的第一件事**（`-s` 就是「我要寫」的意思）。拿不到就不啟動 —— 🚫 不要起來之後才發現寫不了 |
> | 單發、會寫的命令 | 那個命令進來的時候才拿；拿不到回 **`109`**（rpc-spec §5.1），🚫 不重試、🚫 不降級成唯讀跑一半 |
> | `--version` 這類 | 🚫 **不拿**：它連讀都不用（clap 印完就結束，根本走不到資料目錄） |
>
> **檢查點只有一個**：`Handle::call()` 進 dispatch 之前。判準是**反過來寫**的 ——
> 只有「正面認得、確定只在記憶體裡動」的那幾個（`hello`、`daemon.info`／`set_encryption`／`shutdown`）
> 算唯讀，**其餘一律要寫權**。⭐ 這樣新加一個 method 而忘了想它的人，得到的是「被要求拿鎖」，
> 🚫 不是「靜靜地寫進別人的資料庫」。
> 📎 名單短是刻意的：`account.list` 要開 vault 解目錄名、`room.list` 會讓 matrix-sdk 的 store 寫東西
> —— **看起來像唯讀的其實都會寫**。
>
> ⚠️ 所以 daemon 跑著的時候，唯讀工具拿共享鎖會**被拒** —— 那個拒絕就是答案：**去走 RPC，
> 不要動它的檔**。⭐「一邊寫一邊有人讀」的併發不是這把鎖給的，是 `cache.db` 的 WAL 給的
> （逐筆交易的粒度，比一把長命鎖精確）。
> ⚠️ 而且它是**勸告式**的：只擋有來問的程序，🚫 擋不住直接開 `cache.db` 亂寫的程式。
> 🚨 **在那之前這條只是文件上的規則**：`cache.db` 開在 WAL 模式（SQLite 本來就支援多程序併發）、
> port 預設隨機，所以兩個 daemon 可以同時開著同一個目錄互相踩 —— 維護者 2026-09-13 指出這件事。
> ⭐ 用 OS 的鎖而不是「寫 pid 進檔案再檢查它活著嗎」：後者必然有 race，而且 `kill -9` 之後
> 會留下擋住下一次的殘留鎖；OS 的鎖由核心在程序結束時自動放手。
> 📎 單發命令列（下面那張表）之後接的就是同一支 `lock::acquire`。

**daemon 就是 client 本體**，而且**真的能用命令列操作**。它**只有兩種起法**，而且**互斥**
（維護者 2026-09-13）：

| 帶了什麼 | 是什麼 |
|---|---|
| `-s` | 常駐 daemon |
| 沒有 `-s`、帶了一個命令 | 單發（跑完就死） |
| **兩個都帶** | 🚫 **報錯** —— 它們是兩種不同的起法，不是「以某一邊為準」。使用者搞錯了，我們🚫 不替他猜 |
| 兩個都沒帶 | 🚫 報錯（沒事可做） |

⭐ 這條寫進型別裡（`StartMode`），🚫 不散在各處 `if cli.serve`：兩種起法的差別**不只是要不要開
port**，還包括要不要拿寫權、要不要寫 `daemon.json`、要不要宣告 ready。


| 起法 | 做什麼 |
|---|---|
| `daemon <命令> <參數>` | **單發**：起一次、執行那一個命令、就死。**測試性質**（除錯、腳本、驗收），不是日常操作 |
| `daemon -s`（`--server`） | 常駐：開本地 RPC 的 WS（§4）與資料平面，等前端來連。**大多數時候是這個** |

🚨 **常駐的 daemon 獨佔資料庫**（`cache.db`、matrix-sdk 的 store，§1 第二列），所以
**`-s` 跑著的時候，🚫 不可以再叫一次 daemon 的單發命令**——那會去跟真正的 daemon 搶
store，結果不是鎖住就是壞資料。規則：

| 常駐的 daemon 在跑時，再叫 `daemon …` | 結果 |
|---|---|
| 要碰資料庫的命令（`login`、`rooms`、`send`、`recent`……幾乎全部） | **跳錯、exit 非 0**，一個字都不做。錯誤訊息要講「daemon 已在常駐，改用 rpc-cli 或前端」 |
| **不獨佔的命令**（`--version`、`--help`、印預設路徑之類，**不開任何 store**） | 照常 |

⭐ 判準是**「這個命令會不會開 store」**，不是命令名單——名單會漏，漏掉的那個就是 fail open。
實作上在「開 store」那一個入口做偵測（lock 檔或 store 自己的鎖），🚫 不在每個命令各判一次。
而 `-s` 起來時反過來也一樣：store 已被別人（另一個 `-s`、或還沒死的單發）持有就拒絕啟動，
🚫 不接手、不等待——兩個常駐同時活著比拒絕一次嚴重得多。

📎 沒有 `-s` 在跑的時候，單發就沒有這個問題——它自己就是那一次的獨佔者，做完就放。

⭐ **兩種起法走同一條路**：命令列的參數**先轉成 RPC 格式的訊息**，再丟給內部的 handle 執行。
🚫 不要有「命令列直接叫 core」與「RPC 叫 core」兩套分派——那是同一個問題的兩份實作，
遲早漂移（全域 A4）。判準：`daemon rooms --account a` 與 WS 上收到
`{"method":"rooms","params":{"account":"a"}}` 進到 handle 的時候**長得一模一樣**。

```
命令列 arg ──解析──> RPC 訊息 ──┐
                                ├──> daemon handle ──> core ──> wbf-sdk ──> server
本地 WS 收到的 RPC 訊息 ──解密──┘
```

**rpc-cli 為什麼送 RPC 訊息、🚫 不送命令列參數**：

- RPC 的 WS 才是**通用標準**——不是每個前端都跟著 daemon 的 binary 走（web、Android、Python 都不會），
  把「參數格式」當成介面，每個前端都要再包一次 daemon 的命令列，不優雅也不好管。
- 統一走 RPC，之後**Rust 端要用 RPC 的時候接口直接重用**（例如 Desktop 若用 Rust 原生，或第二個 Rust 工具）。
- 所以 rpc-cli 的定位收窄成：**真的從另一個程序丟 RPC 進來，測整條路有沒有通**。
  它的命令列只是 RPC 訊息的薄封裝，🚫 不長自己的邏輯。

📎 這也回答了「要不要單獨一支 rpc-cli」：日常操作用 daemon 自己的命令列就夠；rpc-cli 留著是
因為「同程序內轉一圈」證明不了 WS、加密、token 那一段真的通。它可以很小。

⚠️ 對 §0.1 表的修正：daemon 那一列「只有講除錯那一面才寫 `daemon-cli`」仍然成立，
但**日常操作也是它**，不是 rpc-cli。

## 1. 為什麼要這一層（三個現在就在痛的點）

不是為了架構美感。現在的 CLI 是「一個命令一個程序」，這件事撞到三面牆：

| 痛 | 現在 | daemon 之後 |
|---|---|---|
| **每個命令都要解鎖** | 以前靠 `unlock.ticket` 省打字：**明文主金鑰落地** 15 分鐘。2026-09-13 連它一起拿掉了（local-cache-db §4） | 解鎖一次，主金鑰只在 daemon 的記憶體裡。**那個妥協整個消失** |
| **matrix-sdk 的 store 是獨佔的** | Desktop 開著就不能同時用 CLI（`crypto.db` 被鎖）。上游為此有 `enable_cross_process_store_lock`，但那是 iOS notification extension 的權宜之計，代價是每次操作搶鎖 | 一個程序持有 store，問題不存在 |
| **收不了推送** | roadmap 要把 event（含金鑰的 to-device）改走 WS 推送，而一命令一程序的東西**沒有人在線上收**，斷開的期間就是漏 | daemon 常駐，連線與游標由它維護 |

第三點是決定性的：**daemon 不是風格選擇，是 WS 推送這條路推出來的必然**。

## 2. 分層

```
┌─────────────────────────────────────────────────────────┐
│ 前端（薄）  CLI          Desktop        Android      Python │
│             Rust         Rust 原生      Kotlin       任意   │
└───────┬─────────┬────────────┬─────────────┬────────────────┘
        └─────────┴────────────┴─────────────┘
  控制 ws://127.0.0.1（加密的 JSON）  ＋  資料 http://127.0.0.1（bytes，Range）  §4
                              │
┌─────────────────────────────┴───────────────────────────┐
│ daemon（crates/wbf-core ＋ crates/wbf-daemon）           │
│   session 與多帳號狀態、vault 解鎖一次、RPC 服務          │
│   事件分發、進度回報、與 server 的四條長連線與游標（§6.1）│
└───────┬──────────────────────────┬──────────────────────┘
        │                          │
┌───────┴─────────┐      ┌─────────┴──────────┐
│ wbf-sdk         │      │ matrix-sdk（vendor）│
│ wbf-pack 協議   │      │ Olm／Megolm、store  │
│ chunk 加解密    │      │ 裝置驗證、backup    │
│ cache.db、媒體池│      └────────────────────┘
└───────┬─────────┘
        │  wbf-pack（二進位，加密）
┌───────┴─────────┐
│ wbfuwunel       │
└─────────────────┘
```

兩條界線，語意完全不同：

| 界線 | 協議 | 加密 | 跨信任邊界？ |
|---|---|---|---|
| daemon ↔ server | **wbf-pack**（二進位） | 是（E2EE 的密文在裡面流動） | **是**：server 不可信 |
| 前端 ↔ daemon（控制） | **加密的 JSON over WS** | 是（XChaCha20-Poly1305，token 導出的金鑰） | **否**：同一台機器、同一個使用者 |
| 前端 ↔ daemon（資料） | **HTTP，支援 Range** | 否（明文 bytes，只在 loopback 上；靠 capability URL 擋） | 否 |

⚠️ 控制平面**加密的收穫主要是完整性**（§4.4），不是機密性——能監聽 loopback 的人通常也能讀記憶體與 token 檔。
資料平面則刻意**不加密**：它要餵給播放器與圖片元件，那些只吃普通的 HTTP；防護靠 capability URL（§4.8）。

## 3. daemon 的職責邊界

**daemon 做**：

- 與 server 的所有通訊（wbf-pack WS、以及還沒搬過來的 HTTP）
- 加解密：matrix-sdk 的 Olm／Megolm、我們的 chunk 加解密
- 本地資料：`local.key`／vault、`cache.db`、媒體池、房間金鑰備份
- **session 與多帳號**：同時登入多個帳號，每個 RPC 請求指定用誰
- 事件分發：把收到的事件推給訂閱中的前端
- 長工作的進度（上傳、下載、`recent` 同步）

**daemon 不做**：

- 🚫 **不管 UI 狀態**：哪個房間被選中、捲到哪、草稿——那是前端的事
- 🚫 **不管顯示格式**：時間怎麼寫、名字怎麼縮、訊息怎麼排版
- 🚫 **不代前端做決定**：要不要下載一個 2 GB 的檔、要不要接受一個裝置驗證，一律問過

判準：**跨越這條界線的只有資料，不是政策**（全域 CLAUDE.md A4）。daemon 不需要知道前端是誰、長什麼樣。

## 4. 本地介面：兩個平面，兩個 port（維護者 2026-09-09 定案）

```
控制平面   ws://127.0.0.1:<rpc port>     加密的 JSON（binary frame）   命令、狀態、事件、進度
資料平面   http://127.0.0.1:<data port>  bytes                        媒體：GET 帶 Range、PUT 上傳
```

三種傳輸、三個名字，**沒有任何兩個長得像**——這是刻意的，免得寫的人搞混：

| 誰跟誰 | 傳輸 | 內容 | 叫什麼 |
|---|---|---|---|
| daemon ↔ wbfuwunel | WebSocket | 長度前綴的**二進位 pack** | **wire**（`wbf-wire`） |
| 前端 ↔ daemon（控制） | WebSocket | **加密的 JSON**，一 frame 一則 | **RPC** |
| 前端 ↔ daemon（媒體） | HTTP | bytes | **資料平面** |

### 4.1 為什麼分成兩個 port

**兩種連線的特性完全相反**：

| | 控制平面 | 資料平面 |
|---|---|---|
| 訊息大小 | 幾百 byte | 幾百 MB |
| 連線壽命 | 整場開著 | 一個傳輸一條 |
| 並行 | 一條就夠 | 播放器會同時開好幾條發 Range |
| buffer／timeout | 小、短 | 大、長 |

混在一個 listener 上這四項全要折衷，還要寫 HTTP upgrade 的分流。

### 4.2 控制平面為什麼是 WebSocket

**因為前端可能是瀏覽器，而瀏覽器不能開裸 TCP socket**（維護者 2026-09-09：Desktop 可能先用 web
當速成框架試 RPC）。JS 只有 WebSocket、fetch、WebTransport 三種，其中只有 WS 是雙向的。

📎 中途評估過**裸 TCP ＋ NDJSON**（一行一個 JSON，省掉 WS 的握手與 frame），
在「所有前端都是原生程式」的前提下它更輕。web 前端一進來這個前提就沒了，所以放棄。
📎 也評估過 **UDS／named pipe**（省 port、認證交給檔案權限），查完跨平台之後放棄：
Rust 在 Windows 沒有 `UnixListener`，而 **Python 在 Windows 根本不支援 `AF_UNIX`**——
Windows 是主力平台、Python 是要支援的前端，那個組合直接不成立。

WS 的成本不高：Rust 已經在用 `tokio-tungstenite`（對 server 那條），Python 有 `websockets`，
Kotlin 的 OkHttp 內建，JS 原生。

### 4.3 token：前端產生，前端叫起 daemon

**前端與 daemon 必然成對出現**（維護者 2026-09-09）——有 rpc-cli 就有 daemon，有 Desktop 就有 daemon。
所以 token 由**前端**產生，不是 daemon。⭐ 而它落在磁碟上**只是為了交給一個還沒啟動的程序**，
所以那段落地時間要盡量短（維護者 2026-09-13 定的五步）：

| 步 | 誰 | 做什麼 |
|---|---|---|
| 1 | 前端 | 產生 **256 byte 隨機檔**（例如 `<data dir>/daemon.token`，Unix **0600**） |
| 2 | 前端 | spawn `daemon -s --token-file <路徑>`。🚫 **不用命令列傳內容**：那會進 `ps` 輸出 |
| 3 | daemon | 讀它、導出金鑰（§4.4）、開 port、**宣告 ready** |
| 4 | 前端 | 收到 ready（或自己 `hello` 成功）之後**抹掉**那個檔 |
| 5 | — | 此後 token 只活在兩個程序的記憶體裡，**磁碟上沒有** |

- ⚠️ **daemon 讀完就不再回頭讀那個路徑**，所以第 4 步刪掉它不會讓任何東西壞掉（`loopback.rs`
  有一條測這件事）。反過來說，🚫 **不要設計「重讀 token」**——那會把第 5 步整個推翻。
- 🚨 **誰起的誰動**（維護者 2026-09-13）：token 檔屬於**叫起 daemon 的那個程序**。
  daemon **默認完全不動它** —— 🚫 不抹、🚫 不刪、🚫 不改權限、🚫 不寫回去。它只讀一次。
  ⭐ 理由：daemon 不知道是誰起的它、也不知道那個前端收到 ready 了沒有，而「替別人清掉他的檔」
  猜錯的代價是前端還沒讀完設定就少了一個檔。
  ⭐ 代價寫明：前端在第 3、4 步之間死掉，那個檔會留在磁碟上 —— 那是**那個前端**下次啟動時要處理的
  （重新產生一把，舊的照第 4 步抹掉），🚫 不是 daemon 的事。
- 🚫 **不重用既有的 token**：抹掉之後就沒有既有的那份了；重連就重產生。一把 token 活得越久，
  它落在磁碟上的那幾秒就越不只是那幾秒（備份、快照、同步資料夾都會抄走它）。
- **daemon 的 token 檔權限是 fail closed 的**：Unix 上 group／other 有任何讀取位元就**拒絕啟動**，
  🚫 不是印個警告照跑。
- 📎 這比「daemon 產生、前端去讀」好的地方：**web 前端不必去讀檔案**——token 本來就是它給的。

**第 3 步的 ready 是一個「邊緣」，不是一個狀態**（不然前端會把上一次殘留的 port 當成這一次的）：

1. daemon 綁定**之前**先刪掉舊的 `<data dir>/daemon.json`（刪不掉就不啟動）。
2. 綁好之後才 **temp＋rename** 寫進去（`{ "rpc_port", "data_port", "pid", "instance" }`）——前端 watch 到
   它出現時，port 一定已經在聽，而且一定不是上一次的。
3. 同時 **stdout 印一行 JSON**：`{"ready":true,"rpc_port":…,"data_port":…,"pid":…,"instance":"<uuid>"}`。
   spawn daemon 的那個程序手上有 pipe，這樣它不必去 watch 檔案。🚫 stdout 只有這一行，其餘一律 stderr。
   📎 `instance` 是這次啟動鑄的 UUID v4，`hello` 與 `daemon.info` 回的是**同一個**（rpc-spec §1.3）：
   前端拿它判斷「還是剛才那一個 daemon 嗎」——⚠️ 🚫 不要拿 port 或 pid 判斷，那兩個都會被重複使用。
4. daemon 結束時刪掉 `daemon.json`。

**第 4 步的「抹掉」有規定的做法**（`wbf_daemon::token::shred`，維護者 2026-09-13 指定）：
**隨機 bytes → 填滿 `0xFF` → 填滿 `0x00`，每遍都 flush＋sync，最後才 `remove_file`**。

- 順序的理由：隨機那遍是唯一「連舊值的統計痕跡都蓋掉」的一遍；後兩遍讓人**看得出這個檔被故意清過**。
- 先 sync 再解除連結：反過來的話目錄項先消失，覆蓋就寫進一個沒有名字的檔。
- ⚠️ **覆蓋是盡力不是保證**：SSD 的抹寫層與 CoW 檔案系統（APFS、Btrfs、ZFS、VSS 快照）可能把舊內容
  留在別處，那不是使用者空間管得到的。🚫 所以別把它當成「這個 token 從此不可能被撿回來」——
  真正的防線是**它只有幾秒鐘在磁碟上**加上 0600。📎 但盡力仍然值得：它擋掉最廉價的那一類
  （`undelete`、目錄項還在的救援工具、被抄走的備份）。

### 4.4 每則控制訊息都加密（token 就是金鑰材料）

**WS 上一律 binary frame**，每個 frame 是 RPC 自己的極簡 pack：`ver(1) ‖ type(1) ‖ data`
（維護者 2026-09-12 定；欄位與兩個階段的規則在 rpc-spec §1，這裡只放密碼學的部分）。
`type = 0x02` 時 `data` 是密文：

```
data    = nonce(24) ‖ XChaCha20-Poly1305(key, nonce, aad, JSON bytes)
key_c2k = BLAKE3 derive_key("wbf-matrix-client rpc client-to-daemon v1", token)
key_k2c = BLAKE3 derive_key("wbf-matrix-client rpc daemon-to-client v1", token)
aad     = "wbf-rpc v1"
```

- **兩個方向不同金鑰**：不然攻擊者可以把 daemon 的回應原封送回去當請求（反射）。導兩把是免費的。
- **nonce 每則隨機 24 byte**：XChaCha 的 nonce 夠長，隨機碰撞機率可忽略，不必維護計數器
  （計數器碰到重連就要處理狀態）。
- **加密本身就是認證**：沒有 token 就送不出解得開的包，第一包就驗不過 → 關連線。
  ⚠️ 但**關之前先送一包 `type = 0x01`（明文）講原因**（`BAD_TOKEN` 之類，rpc-spec §1.4）——
  不然 token 錯的人只看到斷線，什麼提示都沒有。`0x01` 在預設狀態下**只有這一種用途**，
  而且它的 JSON **跟正常回應同一個形狀**（`code` 9xxx、`result.close`），前端的 frame 翻譯器只有一條路。
- **加密是 daemon 的全局狀態 `encryption_enforced`，預設開**：開著時 client 送 `0x01` 一律拒絕；
  只有走密文呼叫 `daemon.set_encryption { enforced: false }` 才降級（除錯用，rpc-spec §1.1）。
  🚫 所以 `hello` **不必再帶 token 欄位**，它只用來協商協議版本（一個協商表，不是一個數字）、報上 client 名字
  （正式名稱、`wbf-matrix` 開頭，rpc-spec §1.3）。
- frame 上限 **1 MiB**：超過就關連線（🚫 不讓對方用一個巨大 frame 把記憶體吃光）；
  這個數字跟 §4.8「超過就走資料平面」是同一個。

**為什麼要加密（而不是只靠 token 認證）**

維護者 2026-09-09：既然可選，傾向用**有保護力**的。
⚠️ 老實說收穫的邊界：加密擋的攻擊者有限——能監聽 loopback 的人（要 root／管理員或 npcap）
通常也能讀你的記憶體與 token 檔。**真正的收穫是完整性**：Poly1305 標籤讓「改一個 bit 讓
`false` 變 `true`」這種事驗不過。🚫 所以這裡要的是 **AEAD**，不是任何形式的自製混淆——
沒有標籤的東西擋不住惡意修改，而控制平面上一個被改過的 `true` 就足以刪掉東西。

📎 用的是 `XChaCha20-Poly1305`，跟 chunk 加密、vault、目錄名加密同一個 crate，沒有新依賴。

📎 因為訊息是加密的，**內容是 JSON 就夠了，不需要 BSON**（維護者 2026-09-09 定）：
加密之後本來就是二進位，BSON 只會多一個每個語言都要裝的依賴，還讓解密後的 log 變得不可讀。

### 4.5 daemon 的兩段式狀態

daemon 起來的第一件事是**試著自解密**（`plain` 模式的 `local.key` 解得開；`passphrase` 模式解不開）。
解不開就**停在未解鎖狀態**——但兩個 port 照樣開著：

| | 控制平面（WS） | 資料平面（HTTP） |
|---|---|---|
| **未解鎖** | 開著，但只接受 `hello` 與 `vault.unlock`，其他一律回 `locked` | **503 Service Unavailable** |
| **已解鎖** | 全部 method | 正常 |

```jsonc
{ "method": "vault.unlock", "params": { "passphrase_file": "/path/to/pw" }, "id": 1 }
// 或 { "passphrase_base64": "…" }：passphrase 是任意 bytes（local-cache-db §12），不一定是字串
```

- **passphrase 只留在 daemon 的記憶體裡，直到 daemon 關閉**（維護者 2026-09-09）。
  ⚠️ 所以 `unlock.ticket`（明文主金鑰落地 15 分鐘，local-cache-db §4 自己標記為妥協）**整個消失**——
  這是 daemon 最直接的安全收穫。📎 2026-09-13 起連 CLI 那一側也沒有了（同 §4）：那個痛點不存在了。
- 🚫 **沒有「鎖回去」**：`Core` 解鎖一次就活到程序結束，daemon 沒有 `vault.lock`（rpc-spec §3.1）。
  UI 的 lock／unlock 是 **UI 自己那一層**的事 —— daemon 照樣連著、照樣寫 DB、照樣發通知，
  ⭐ 因為使用者按 lock 通常只是暫時離開，回來要看到這段時間的訊息。真的要讓金鑰離開記憶體
  就是 `daemon.shutdown` 再 `daemon -s`。
- 🚫 daemon **不自己去問終端**：那樣 Desktop 與 Android 沒辦法解鎖。passphrase 一律從 RPC 進來。
- 未解鎖時資料平面回 **503 而不是 404**：媒體確實存在，只是現在打不開——這個區別對前端有意義。

### 4.6 訊息形狀（加密之前的內容）

**形狀借自 JSON-RPC 2.0 的習慣，但這是我們自己的協議**（維護者 2026-09-09 定）：
`method` ＋ `params` ＋ `id`、有 `id` 要回、沒有 `id` 不回——這幾條照抄，因為它們好用。

⚠️ 但**回應的形狀不一樣**（標準是 `result`／`error` 二選一，這裡是 `code`／`msg` 平鋪），
所以 🚫 **不宣告 `"jsonrpc": "2.0"`**，也不要拿現成的 JSON-RPC library 來接——
與其假裝相容然後在某個角落炸掉，不如一開始就說清楚這是自己的東西。
版本識別走 `hello` 的 `protocol` 欄位（§4.4）。

**請求**：

```json
{
  "method": "room.send_text",
  "params": { "account": "@alice:localhost", "room": "!abc:localhost", "body": "hi" },
  "id": 1
}
```

**回應**（`code`／`msg` 平鋪在頂層，🚫 不是標準的 `result`／`error` 二選一）：

```json
{
  "code": 0,
  "msg": "ok",
  "result": { "event_id": "$xyz" },
  "id": 1
}
```

```json
{
  "code": 1001,
  "msg": "the vault is locked; call vault.unlock first",
  "result": null,
  "id": 1
}
```

為什麼是 `code`／`msg` 而不是標準的 `result`／`error` 二選一：**一個欄位就判斷得出成敗**，
前端不必先看有沒有 `error` 欄位再決定讀哪邊。代價是不能用現成 library——
但我們本來就要自己寫（外面還包著一層加密），那個代價是零。

**推播**（沒有 `id` 的請求）：

```json
{
  "method": "room.message",
  "params": { "account": "…", "room": "…", "message": { } }
}
```

```json
{
  "method": "progress",
  "params": { "id": 17, "done": 1200, "total": 4096 }
}
```

📎 推播刻意跟請求**同構**（都是 `method` ＋ `params`），所以只有兩種形狀要記；
分辨方式就是那條照抄來的規矩：**有 `id` 要回、沒有 `id` 不回**。

規矩：

- **`id` 由前端給**，單調遞增即可；daemon 原樣回。一條連線上可以同時有很多個未完成的 `id`。
- **`method` 是 `名詞.動詞`**（`account.add`、`room.send_text`、`media.open`），不是 CLI 的字串命令列——
  🚫 不要讓前端組命令列字串再由 daemon 解析，那是把 shell 的問題搬進 RPC。
- **`code` 是穩定的整數**：`0` 是成功，其他值一個意思一個號碼、**定了就不改**（前端會拿它做判斷）。
  `msg` 是給人看的，🚫 前端不要拿它做邏輯。code 表在 `rpc-spec.md`。
- **失敗時 `result` 是 `null`**，🚫 不要省略那個欄位——欄位固定在，弱型別的前端少一種 undefined 要處理。
- **推播要先訂閱**（`subscribe`／`unsubscribe`），🚫 不預設把所有事件推給每條連線。
- **framing 由 WS 給**：一個 binary frame 就是一則訊息，🚫 我們不自己切。

### 4.7 連線數：實務上 1:1，但不寫死

維護者 2026-09-09：**幾乎必然只會 1:1，RPC 只服務一個 client。**

所以 🚫 **不做任何「哪個 client 是主」的概念**——沒有主從、沒有權限分級、沒有連線間的協調。

但也**不硬性拒絕第二條連線**：那反而要寫拒絕邏輯與錯誤碼，而且 Desktop 開著時想跑一下 CLI 就被擋
（開發時很煩）。事件推播本來就要用 broadcast channel，多一條訂閱者是免費的。

**結論：允許多條連線，每條都平等、各自訂閱，daemon 不記得誰比較重要。**

### 4.8 大資料走資料平面，不走 RPC

⚠️ 下載一個 2 GB 的檔不可能塞進 JSON，改成 binary frame 串流也會逼**每個前端各自實作一次串流組裝**。

⚠️ 而且**不能給檔案路徑**（2026-09-09 維護者指出，這是初稿的錯）：媒體池裡的東西是**加密的**
（local-cache-db §8），給前端一個池裡的路徑，它讀到的是密文；daemon 先解密寫到某個路徑，那就是
**明文落地**——整個加密池的意義就沒了。

所以 bytes 走資料平面，而它是 HTTP **因為媒體最終要餵給既有的消費者，而那些消費者只吃 URL 或路徑**：

| 消費者 | 吃什麼 |
|---|---|
| Android ExoPlayer | URL（或自訂 DataSource） |
| Desktop 的影片（GStreamer／ffmpeg／libmpv） | URL 或路徑 |
| 圖片解碼器 | bytes 或 Reader |

既然不能給路徑，剩下的通用介面就只有 **URL**。所以 loopback HTTP 不是多造一個輪子，
是**把加密池接上這些現成輪子的唯一接頭**。Range 也不是額外工作：媒體池的 64 KiB 分段
本來就是為隨機讀設計的（local-cache-db §8.1），`seek` 的語意早就定好了（約定 §7）。

**資料平面的認證：每個資源一張 capability URL，🚫 沒有全域 token**

```
http://127.0.0.1:<data port>/media/<resource token>
```

- token **綁單一資源**（這一個檔、這一次傳輸）、**有 TTL**、**可以重複使用**。
  ⚠️ 不能「用一次就失效」：播放器 seek 一次就是一次新的 Range 請求，一個影片會發幾十次。
- token **放在 URL path 裡，不是 header**。理由是實務的：**有些媒體元件只吃 URL、不讓你設 header**
  （Android 的 ExoPlayer 可以設，很多圖片元件不行）。做成 capability URL，任何吃 URL 的東西都能直接用。
- 代價老實寫：URL 會進到那個元件自己的 log。在 loopback 上、短命、綁單一資源，可接受。
- 認不得的 token、過期的 token：一律 **404**，🚫 不分辨「不存在」與「過期」（那會變成探測工具）。
- **未解鎖時一律 503**（§4.5）：媒體確實存在，只是現在打不開——這跟 404「沒這個東西」不一樣。

**播放／顯示**：

```jsonc
{ "method": "media.open", "params": { "account": "…", "event": "$xyz" }, "id": 9 }
{ "code": 0, "msg": "ok", "id": 9, "result": {
    "url": "http://127.0.0.1:51235/media/9f3a…",
    "mimetype": "video/x-matroska", "size": 1073741824, "expires_in": 3600 } }
```

前端把 `url` 直接交給播放器／圖片元件，它自己發 Range。daemon 邊解密邊吐，
🚫 **不把整個檔案讀進記憶體**。

**上傳**：

```jsonc
{ "method": "media.create", "params": { "account": "…", "room": "…", "name": "video.mkv" }, "id": 12 }
{ "code": 0, "msg": "ok", "result": { "url": "http://127.0.0.1:51235/upload/7c1b…" }, "id": 12 }
// 前端 PUT bytes 進去；daemon 邊收邊加密邊走 chunk 上傳
{ "method": "progress", "params": { "id": 12, "done": 52428800, "total": 1073741824 } }
```

⚠️ **Android 沒有別的選擇**：SAF 給的是 `content://` URI，**根本沒有檔案路徑可給**。
所以 PUT 這條路在 Android 上不是「比較好」，是必要的。

**另存新檔**（使用者明確要把明文放到自己選的位置）仍然走路徑：

```jsonc
{ "method": "media.save_to", "params": { "account": "…", "event": "$xyz", "out": "/home/me/video.mkv" }, "id": 15 }
```

這裡明文落地是**使用者要的**，不是我們偷偷做的——這條界線要守住。rpc-cli 的 `download -o` 就是它。

📎 **小東西**（頭像縮圖之類）可以直接 base64 進 RPC 的 result，省一次來回。界線放在
單則 RPC 訊息 **1 MiB**（跟 §4.4 的 frame 上限同一個數），超過一律走資料平面。

📎 效能：多一次 loopback 的記憶體複製，但**少了一次磁碟往返**（本來是「解密→寫檔→播放器讀檔」，
現在是「解密→socket→播放器」），還不用清暫存檔。控制平面那邊一趟往返 < 0.1 ms，
比它後面接的 SQLite 查詢與 AEAD 解密都便宜——**不是瓶頸**。

### 4.9 閘門鏈，與一則附件訊息的完整流程（維護者 2026-09-12 定）

**理想狀態：資料庫只有 daemon 碰。** daemon 是後端，UI 只是 RPC call；
誰解密由 daemon 決定（matrix-sdk 的 Megolm、我們的 chunk 解密），解完存進 `cache.db`，
**前端看到的一律是明文**。反過來，前端送出的也是明文，要不要加密是 daemon 依房間狀態決定。

整條閘門鏈，從外到內：

```
homeserver  <=>  daemon 的 WS 協議層（wire、四條連線 §6.1）
            <=>  daemon handle（命令本體：core）
            <=>  RPC 轉換（JSON ↔ handle 的型別；命令列 arg 也在這裡轉，§0.2）
            <=>  本地 WS（加密的 JSON，開給前端）
```

每一層只跟隔壁講話：前端不知道 wire，wire 不知道 RPC。

**上傳一個大檔當附件**（走我們自己的分片協議）會拆成兩三個來回，這是複雜化的地方，寫清楚：

| # | 誰 | 做什麼 |
|---|---|---|
| 1 | 前端 → daemon | RPC `media.create`（§4.8）：檔名、大小、目標房間 |
| 2 | daemon → server | 去 server **建檔**（`Upload/Create`），拿回檔案的 URL／id |
| 3 | daemon → 前端 | RPC 回 result：server 的 URL ＋ 資料平面的 PUT URL |
| 4 | 前端 → daemon | 以那個 URL 為基礎**發一則附件訊息**（RPC `room.send`，內容是明文） |
| 5 | daemon → server | 房間有 E2EE 就 Megolm 加密、沒有就明文，送到 server。附件宣告（約定 §5.2）在這一步帶 |
| 6 | 前端 → daemon | **同時**開始 PUT bytes 到資料平面的 URL（一個 HTTP 連線，不斷送） |
| 7 | daemon → server | 邊收邊做 chunk 加密、邊走 `Upload/*` 上傳到 homeserver |
| 8 | daemon → 前端 | 不斷回 `progress`（§4.8） |

⚠️ 第 4 與第 6 步**並行**：訊息不必等檔案傳完才發（訊息裡只有 URL 與描述），
接收端拿到訊息時檔案可能還在傳——這正是分片協議與 `seek` 存在的原因（約定 §7）。
⚠️ 第 5 步失敗與第 7 步失敗是**兩件事**，各自回錯誤、各自可重試，🚫 不要綁成一個交易。

## 5. 四個前端怎麼接

| 前端 | 怎麼啟動 daemon | 怎麼講話 |
|---|---|---|
| **daemon 自己的命令列** | 就是它自己：`daemon -s` 常駐；`daemon <命令>` 單發是測試性質，常駐中再叫會跳錯（§0.2） | arg → RPC 訊息 → handle，不經 socket |
| **rpc-cli** | 連不到就自己 spawn 一個（使用者無感）。定位是**從外部程序丟 RPC 測整條路**（§0.2） | 同一份 RPC |

📎 ⚠️ **RPC vs uniffi 這個決策還沒定**：如果不要程序隔離，Desktop 與 Android 也可以用 uniffi 直接綁 library（matrix-rust-sdk 自己就是這樣給 Element X 用的），完全不需要 RPC。兩條路的取捨是**安全隔離 vs 簡單**，不是工作量——見 §8 第 7 點。
| **Desktop** | 可能先用 **web** 當速成框架試 RPC（維護者 2026-09-09 改變主意），之後再看要不要原生 Rust。內嵌或 spawn 都行 | 同一份 RPC |
| **Android**（JNI `.so`） | JNI 只有 **`daemon_start(data_dir, token_path) -> {rpc_port, data_port}`** 與 **`daemon_stop()`** | Kotlin 直接連 loopback WS（OkHttp 內建）；媒體 URL 直接餵 ExoPlayer |
| **Python** | spawn daemon 程序 | 同一份 RPC |

⚠️ **Android 那格是這個設計最大的收穫**：JNI 最痛的是跨語言型別轉換與生命週期，包一個大介面等於維護第二套 API。
只包「啟動／停止」兩個函數，其餘全部走 WS——**JNI 表面積是兩個函數，而且永遠不會長大**。
同程序內連 loopback 有點繞，但成本可忽略，換到的是「介面只有一個」。

📎 Android 的 WS 生命週期：**app 開著才連**，關掉或縮到背景就斷，回來再連。
維護者 2026-09-09：背景長連線（foreground service、FCM 喚醒）現在不考慮。
但斷線重連是常態，所以**協議必須是「拉窗＋游標＋ack」而不是純推送**——見 §6。

## 6. 這個架構回過頭對 server 協議的要求

daemon 常駐、但連線會斷（手機切背景、筆電睡眠、網路換手）。所以與 server 的每一種事件流都要能**從游標補齊**：

| 流 | 現況 |
|---|---|
| 房間事件 | ✅ `Event/Recent` 已經是拉窗＋水位（`cg_seq`） |
| **to-device（金鑰）** | 🔁 **server 端 2026-09-12 實作完了**（wbfuwunel #41 提案 → #42 共用核心 → #43 實作 → #44 文件）：`0x16 Device` 七個 subtype，推送為主、`Fetch` 補洞、**`ItemsDestroy` 才刪**。線上格式的權威在那邊的 `wbf-wire-format.md` §3.2 與 `wbf-to-device.md`。⚠️ **client 端還一個字都沒寫**——要做什麼、哪三處跟 `Event` 相反、為什麼要等 daemon，在 [`to-device-client.md`](to-device-client.md) |

📎 當初判斷「server 端要加的是**一個新的 opcode**，不是一套新機制」——那個判斷成立了：
`get_to_device_events` 本來就吃游標、`remove_to_device_events` 就是刪除，wbfuwunel 這三支
做的是**把它們接到通道上**，外加一個「銷毀是帶結果的命令」的閉環。它對 to-device 的內容
從頭到尾都是瞎的（只存 `type`／`sender`／`content`），這套沒有改變那件事。

### 6.1 事件流是**每個帳號一組**（維護者 2026-09-13 定）

daemon 的推播（rpc-spec §4）不是憑空來的：**每個已經登入的帳號，daemon 都對它的 homeserver
維持一組連線**，事件從那裡進來、解密、寫進資料庫，然後才變成 RPC 的推播。

```
帳號 A ── 連 A 的 homeserver ──┐
帳號 B ── 連 B 的 homeserver ──┼──> daemon（解密、寫 DB）──> RPC 推播 ──> 各前端
帳號 C ── 連 C 的 homeserver ──┘
```

- ⚠️ **「一組」不是「一條」**：對 wbfuwunel 是四條 WS（§6.1.1），對一般 homeserver 是一條
  HTTP 的 sync。所以三個帳號同時登入、都在 wbf server 上，就是 **12 條 WS**。
- 帳號各自獨立：一個帳號的連線斷了、落後了、被登出了，🚫 不影響別的帳號。
  推播因此**一定帶 `user`**（rpc-spec §4）——前端要分得出這是誰的事件。
- 📎 這也是 `sync.state` 推播的來源：它講的是**那一個帳號**跟它的 server 之間的狀態，
  🚫 不是「daemon 連上網了沒」。

**用哪一種連線：預設 HTTP，wbf 專用的 homeserver 才走四條 WS**（維護者 2026-09-13 定）。

| homeserver | 走什麼 | 事件從哪來 |
|---|---|---|
| 一般 Matrix（Synapse、Dendrite…） | **HTTP**（matrix-sdk 的 `/sync`） | 長輪詢 |
| **wbfuwunel**（我們自己那套） | **四條 WS**（§6.1.1） | `Event/Push` 推送＋`Recent` 補洞 |

- ⭐ 判準是**「這台 server 講不講 wbf-pack」**，🚫 不是網域名、🚫 不是使用者設定裡的一個勾。
  問法是現成的：`Hello`／`Ping` 拿 `ServerHello` 的 features（rpc-spec §3.8 的 `server.ping`）。
- **不確定就落到 HTTP**：連不上 WS、`Hello` 不回、features 認不得 —— 一律當成一般 homeserver。
  ⭐ 壞在「用了比較慢但一定能動的那條」，🚫 不壞在「以為對方懂我們的協議」。
- conf 的 `TRANSPORT`（CLI 規格 §10）是**上限不是下限**：設成 `http` 就一律 HTTP（除錯用）；
  設成 `ws`（預設）仍然要探測，探不到照樣 HTTP。

### 6.1.1 對 wbfuwunel：一個帳號開**四條** WS，一條一個用途（維護者 2026-09-12 定）

🚫 **不是一條線全包。** 理由是**佇列是每條連線一份的**：server 端每條連線有自己的送出
佇列（`wbf_ws_send_queue_len`），塞滿了就**丟推送並標 `gap`**。把一次 200 MiB 的媒體下載
跟金鑰推送擺在同一條線上，那條佇列被媒體佔滿的時候，掉的是**金鑰**。

| 連線 | 只做 | 為什麼單獨一條 |
|---|---|---|
| **房間** | 訂閱房間事件（`Event/Subscribe`／`Push`／`Recent`） | 量大但可重拉——它自己塞爆自己沒關係 |
| **金鑰** | 裝置層（`Device/*`：訂閱、`Fetch`、銷毀） | 🚨 **掉了就沒了**。它必須有一條安靜的線，不跟任何大流量共享佇列 |
| **媒體** | 上傳與下載（`Upload/*`、`Download/*`、`Stream/*`）。📎 2026-09-21 落地時拆成**兩條**：上傳一條、下載一條（維護者定；`link-pool.md` §1，所以實際是五條） | 資料平面，長時間高頻寫。⭐ 它是最會把佇列塞爆的那個，所以它**只能塞爆自己**——而上傳塞爆時不該拖住下載 |
| **雜項** | 跟訂閱無關、跟會話無關的單發事件 | 一問一答的東西不該排在別人的長佇列後面 |

⚠️ 這四條是**那一個帳號的**（§6.1）：兩個帳號登在同一台 wbf server 上也是各開各的四條，
🚫 不共用 —— 它們的 `access_token` 不同，連線本來就分得開，而共用會讓一個帳號的流量塞爆另一個的。

⭐ 這四條的分界線是**「誰會塞爆佇列」與「掉了救不救得回來」的交叉**，
🚫 不是「照 kind 分類」——雜項那條收的就是各種不同 kind。

⚠️ **`gap` 的意義因此變成局部的**：房間那條標 `gap`，只表示房間事件漏了，
🚫 不要當成整個 daemon 落後。每條線各自補各自的（房間 `Recent`、金鑰 `Fetch`）。

### 6.2 一條連線上仍然會有好幾段會話——`seq` 屬於會話（wbfuwunel #42／#44 定）

⚠️ **分成四條不代表每條線上只有一段會話。** 媒體那條最明顯：兩個上傳可以同時跑
（wbfuwunel 的 `e2e7` 就有一段叫 `two uploads interleaved on one connection`）；
房間那條也有——一個常駐的 `Subscribe` 會話，加上補洞時開的 `Recent` 會話。

> ⭐ **`id` 是一段會話的名字，`seq` 是那段會話裡的計數。** 換一個 `id` 就是新會話，`seq` 歸零。

| 🚫 daemon 不能這樣寫 | 會壞成什麼 |
|---|---|
| 連線層一個 `next_seq` | 同一條線上兩段會話互相看起來像對方漏號，`gap` 的判斷全毀 |
| 每個 kind 一個 `next_seq` | 同 kind 兩段會話交錯（兩個上傳）就分不出哪包是誰的 |

⚠️ 這條**現在就要記住**，因為它決定了 daemon 裡連線那一層的資料結構：
收到的包要先照 `id` 分派到會話，才輪到 `seq`。📎 而 `seq` 🚫 不是重送機制——
WS 不會掉單一 frame，跳號只代表 server 故意丟了一包（佇列滿），補救是帶游標重新要。

## 7. 現有 crate 怎麼重組

```
crates/wbf-wire     不動：pack 的 codec
crates/wbf-sdk      不動：協議、chunk 加解密、cache.db、媒體池、vault、matrix backend
crates/wbf-core     ✅ 做了（#24）：常駐狀態（多帳號 session、解鎖一次）、事件分發、命令本體。**沒有 RPC**
crates/wbf-daemon   還沒有：core ＋ RPC 服務 ＋ 資料平面。library ＋ binary。**自己的命令列**也在這裡（§0.2：arg → RPC 訊息 → handle）
apps/wbf-cli        ✅ 瘦身了（#24）：只剩參數解析與 JSON 輸出。⚠️ 下一步它的參數解析搬進 daemon 的命令列，
                    剩下的殼變成 rpc-cli（只封裝 RPC 訊息、丟到本地 WS；§0.2）——那時才改名
```

**`core` 與 `daemon` 刻意分開**，因為它們的命運不同：

| | 是什麼 | 選 RPC 時 | 選 uniffi 時（§8 第 7 點） |
|---|---|---|---|
| `wbf-core` | 狀態與邏輯，不知道有誰在跟它講話 | 用 | **照樣用** |
| `wbf-daemon` | 把 core 開一扇門出去 | 用 | 不需要 |

所以那個還沒定的決策**不會擋住開工**：先做 `wbf-core`，它兩條路都要。
✅ **2026-09-12 做完了**（#24），而 RPC vs uniffi 仍然沒定——這正是「先做 core」想買到的東西。

- **`wbf-sdk` 保持是純 library**（plan-v1 §7.2 的方向不變）：core 是它的使用者，不是它的一部分。
- **`wbf-daemon` 同時是 library 與 binary**：Desktop 內嵌用 library，其他人 spawn binary。
- ⚠️ **`wbf-core` 的公開介面不能假設「同程序」**：方法收 `&self`、參數與回傳用簡單型別、
  事件用 channel 而不是回呼引用、自己持有 tokio runtime 不要求宿主提供。
  🚫 公開介面上不要出現複雜生命週期、trait object、`impl Trait`。
  這樣它之後包 RPC 或包 uniffi 都不用改——**這是現在唯一要守的紀律**。
  ✅ #24 三位審查者逐條核實過公開面：60+ 個 `pub fn` 全數回傳可序列化 DTO，唯一的 `&`
  回傳是 `data_dir() -> &Path`，錯誤是有 `Serialize` 的 `CoreError { kind, message }`。
  ⚠️ **紀律不會因為做完就失效**：之後往 core 加方法一樣要過這一條。

### 7.1 現在那支 CLI 的假設要重新檢視

CLI 規格 §9 那些簡化（沒有互動模式、不存密碼、stdout 只印一個 JSON 物件）都建立在
「它是開發與除錯工具，不是產品面」上。變成 rpc-cli 之後：

- ~~`unlock.ticket` 可以拿掉~~ ✅ 2026-09-13 拿掉了（§1、local-cache-db §4）。
- `--token` 模式要重想：那是「不碰 vault、不碰帳號目錄」的路徑，在 daemon 模型下是什麼意思？
- stdout「只印一個 JSON 物件」對 `watch` 這種串流命令本來就有例外（JSON Lines），RPC 的推播會讓這種情況變多。

## 8. 還開著的

1. **RPC 的逐條訊息規格**（`rpc-spec.md`）：method 清單、每個的 params／result、錯誤 code 表、事件清單。這份文件只定形狀。
2. **HTTP 的去留**：`/sync` 之外 matrix-sdk 還會打哪些 HTTP、頻率多少——**要實測過再決定**要不要做 loopback shim
   （把 SDK 的 HTTP 轉進 WS）。⚠️ matrix-sdk 的 transport **不可插拔**（`ClientBuilder::http_client()` 只吃
   具體的 `reqwest::Client`，`HttpClient` 是 `pub(crate)`），所以 shim 的做法是把 `homeserver_url` 指到本機的
   listener，不是實作一個 trait。
3. ~~**fork submodule 的範圍**~~ ✅ 2026-09-10 查清楚也定了：維護者建了
   [`WhiteBirchForumTeam/matrix-rust-sdk`](https://github.com/WhiteBirchForumTeam/matrix-rust-sdk)，
   submodule 已經指過去。**要改的只有一行**：
   `crates/matrix-sdk/src/client/mod.rs` 的 `pub(crate) fn base_client()` → `pub`。
   我們需要的其餘全部**本來就是 `pub`**：`BaseClient::olm_machine()`、
   `OlmMachine::receive_sync_changes`（收 to-device）、`encrypt_room_event_raw`（自己 Megolm 加密）、
   `share_room_key`、`get_missing_sessions`。
   ⚠️ 順帶更正一個一直寫錯的說法：`Room` 上**根本沒有 `encrypt`**（公開或私有都沒有），
   所以「fork 露出 `Room::encrypt` **vs** 走 `OlmMachine`」不是二選一——**兩條路都是走 `OlmMachine`**，
   差別只在怎麼拿到它。chat-model §6 與 handover §7 的同一句話要跟著改。
   📎 有一條不用 fork 的路但不能出貨：`Client::olm_machine_for_testing()` 是 `pub`，
   掛在 `testing` feature 底下，而那個 feature 會把 `wiremock`、`matrix-sdk-test`、
   `assert_matches2` 拖進出貨的 binary。當 spike 驗接線可以。
4. **daemon 的生命週期**：誰負責關掉它、閒置多久自己結束、多個前端同時連著時誰說了算、
   兩個前端同時 spawn 時怎麼收斂（lock 檔？先到先贏？）。
   ⚠️ 這是這份架構裡**複雜度真正的所在**——RPC 本身是機械工作，生命週期不是。
   現在定會是憑空猜，要等 Desktop 的實際使用模式出來。
5. **資料平面 token 的 TTL 與撤銷**：TTL 多長（播一部長片要多久？）、`logout` 時要不要立刻讓所有 token 失效
   （應該要）、同一個資源重複開要不要發新 token。
6. **縮圖的批次**：一次要 50 張縮圖時，50 次 `media.open` 太吵。是走 base64 進 RPC（§4.7 的 1 MiB 規則），
   還是發一張涵蓋多個資源的 token？後者違反「一張 token 一個資源」，要想清楚再定。
7. ⚠️ **RPC 還是 uniffi**（這份文件假設 RPC）：
   📎 **Desktop 一旦走 web，這題實質上已經倒向 RPC**——web 前端沒有別的路。uniffi 只剩 Android 用得上。
   RPC 的**唯一**硬理由是**程序隔離＝安全隔離**——UI 要解圖片、解影片，那是 CVE 大戶；
   daemon 持有金鑰。分成兩個程序，UI 被一張惡意圖片打穿也拿不到 Megolm 金鑰與 `local.key`。
   維護者 2026-09-09 自己也提到同一件事：不想 Python 直接包 Rust，怕「Rust 掛了全部一起死」。
   反過來，uniffi（matrix-rust-sdk 自己就用它給 Element X）可以零序列化直接綁，Desktop 與 Android 都省事。
   **建議等 Desktop 有原型、摸到實際使用模式再定**；在那之前 `wbf-core` 的介面兩條路都能接（§7）。
   📎 如果選 uniffi，這一章（§4）整個不需要——但 `wbf-core` 不會白做。
8. **Desktop 的 UI 框架**：維護者 2026-09-09 改變主意——**Desktop 可以先用 web** 當速成框架驗證 RPC，
   Android 仍然不走 web。所以候選分兩批：短期的 web（Tauri／Electron／純瀏覽器頁面），
   長期若要原生則是 egui／iced／slint／gtk-rs 這一類。
   ⚠️ 原生 Rust GUI 的傳統弱項（長列表虛擬化、IME 中文輸入）要單獨驗；web 那批沒有這個問題，
   但多一層 runtime。評估維度見 handover §7。

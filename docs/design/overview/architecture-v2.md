# 架構 v2：daemon、RPC、與四個前端

> 維護者 2026-09-09 定的方向。這份文件講**分層與介面**，不講功能——功能在
> [`/docs/design/rooms/chat-model.md`](../rooms/chat-model.md)、[`/docs/design/storage/local-cache-db.md`](../storage/local-cache-db.md)、
> [`/docs/design/rpc-specs/wbf-cli-spec.md`](../rpc-specs/wbf-cli-spec.md)。本地介面的形狀在 [`/docs/design/rpc-specs/local-interface.md`](../rpc-specs/local-interface.md)，RPC 的逐條訊息在 [`/docs/design/rpc-specs/rpc-spec.md`](../rpc-specs/rpc-spec.md)。

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
📎 為什麼簡稱不縮成 `rpc`：RPC 是**協議**的名字（這一章、`/docs/design/rpc-specs/rpc-spec.md`、「走 RPC 還是 uniffi」），
拿去當 binary 的名字之後，「RPC 掛了」就分不出是協議還是那支程式——正是這一節要根除的那種語病。
📎 `server` 這個字在這個 repo 已經指過 homeserver 與 wbfuwunel，🚫 不要再拿它指這裡的任何東西。

命令的「做什麼」全部在 `crates/wbf-core`。⚠️ `apps/wbf-cli`（binary 也叫 `wbf-cli`）現在是「解析參數 → 叫一個 core 方法 → 印 JSON」，
還不是 rpc-cli；改走 RPC 的那一步才改名，🚫 先改名只會製造一次沒有內容的大 diff（§6）。

## 0.2 daemon 的命令列就是 RPC 的內部入口（維護者 2026-09-12 定）

**daemon 就是 client 本體**，而且能用命令列操作。它**只有兩種起法**，而且**互斥**（維護者 2026-09-13）：

| 帶了什麼 | 是什麼 |
|---|---|
| `-s`（`--server`） | **常駐**：開本地 RPC 的 WS（/docs/design/rpc-specs/local-interface.md）與資料平面，等前端來連。**大多數時候是這個** |
| 沒有 `-s`、帶了一個命令（`daemon <命令> <參數>`） | **單發**：起一次、執行那一個命令、就死。**測試性質**（除錯、腳本、驗收），不是日常操作。還沒做：現在帶命令會印 not implemented、exit 1 |
| **兩個都帶** | 🚫 **報錯** —— 它們是兩種不同的起法，不是「以某一邊為準」。使用者搞錯了，我們🚫 不替他猜 |
| 兩個都沒帶 | 🚫 報錯（沒事可做） |

⭐ 這條寫進型別裡（`StartMode`，`crates/wbf-daemon/src/main.rs`），🚫 不散在各處 `if cli.serve`：兩種起法的差別
**不只是要不要開 port**，還包括要不要拿寫權、要不要寫 `daemon.json`、要不要宣告 ready。

**資料目錄的獨佔：OS 鎖＋寫權**（維護者 2026-09-13）。daemon 對 `<data dir>/daemon.lock` 拿一把 OS 層的鎖
（`crates/wbf-daemon/src/lock.rs`，`std::fs::File::try_lock`／`try_lock_shared`），**拿不到就不做**，
而且這一步排在碰那個目錄任何東西之前（尤其是刪舊的 `daemon.json`）。

| 誰 | 拿哪一種 | 效果 |
|---|---|---|
| **daemon**（會寫） | 排他 | 這個目錄只有我在寫；別的 daemon 起不來，唯讀工具也要等 |
| **唯讀工具**（只看不改） | 共享 | 多個可以同時讀，但這期間不准有人寫 |

⭐ **鎖只是事實，「能力」才是前提**。daemon 裡有一個全局狀態 `WriteAccess`：**起手是 false**（沒有寫的能力）。

```text
要寫  ── is_granted()? ── true ─> 動手
                       └ false ─> 去拿排他鎖 ── 成功 ─> 設成 true，動手
                                             └ 失敗 ─> 回 109「我沒有寫的權限」
```

| 起法 | 什麼時候拿 |
|---|---|
| `-s` | **啟動的第一件事**（`-s` 就是「我要寫」的意思）。拿不到就不啟動 —— 🚫 不要起來之後才發現寫不了 |
| 單發、會寫的命令 | 那個命令進來的時候才拿；拿不到回 **`109`**（/docs/design/rpc-specs/rpc-spec.md §5.1），🚫 不重試、🚫 不降級成唯讀跑一半 |
| `--version` 這類 | 🚫 **不拿**：它連讀都不用（clap 印完就結束，根本走不到資料目錄） |

**檢查點只有一個**：`Handle::call()` 進 dispatch 之前。判準是**反過來寫**的 ——
只有「正面認得、確定只在記憶體裡動」的那幾個（`hello`、`daemon.info`／`set_encryption`／`shutdown`）
算唯讀，**其餘一律要寫權**。⭐ 這樣新加一個 method 而忘了想它的人，得到的是「被要求拿鎖」，
🚫 不是「靜靜地寫進別人的資料庫」。
📎 名單短是刻意的：`account.list` 要開 vault 解目錄名、`room.list` 會讓 matrix-sdk 的 store 寫東西
—— **看起來像唯讀的其實都會寫**。

- 為什麼要鎖：`cache.db` 開在 WAL 模式（SQLite 本來就支援多程序併發）、port 預設隨機，沒有鎖的話
  兩個 daemon 可以同時開著同一個目錄互相踩（維護者 2026-09-13 指出）。
- `-s` 起來時目錄已被別人（另一個 `-s`、或還沒死的單發）持有就拒絕啟動，🚫 不接手、不等待——
  兩個常駐同時活著比拒絕一次嚴重得多。
- 🚨 所以 **`-s` 跑著的時候再叫單發**：要寫權的命令拿不到鎖 → 跳錯、exit 非 0、一個字都不做，
  訊息要講「daemon 已在常駐，改用 rpc-cli 或前端」；`--version`、`--help` 這類不碰資料目錄的照常。
  沒有 `-s` 在跑時，單發自己就是那一次的獨佔者，做完就放。
- ⚠️ 唯讀工具拿共享鎖，在 daemon 跑著時會**被拒** —— 那個拒絕就是答案：**去走 RPC，不要動它的檔**。
  ⭐「一邊寫一邊有人讀」的併發不是這把鎖給的，是 `cache.db` 的 WAL 給的（逐筆交易的粒度，比一把長命鎖精確）。
- ⚠️ 鎖是**勸告式**的：只擋有來問的程序，🚫 擋不住直接開 `cache.db` 亂寫的程式。`apps/wbf-cli` 現在就不拿這把鎖（它直接叫 core）。
- ⭐ 用 OS 的鎖而不是「寫 pid 進檔案再檢查它活著嗎」：後者必然有 race，而且 `kill -9` 之後
  會留下擋住下一次的殘留鎖；OS 的鎖由核心在程序結束時自動放手。

⭐ **兩種起法走同一條路**：命令列的參數**先轉成 RPC 格式的訊息**，再丟給內部的 handle 執行。
🚫 不要有「命令列直接叫 core」與「RPC 叫 core」兩套分派——那是同一個問題的兩份實作，
遲早漂移（全域 A4）。判準：單發的 `room.list --user a` 與 WS 上收到
`{"method":"room.list","params":{"user":"a"}}` 進到 handle 的時候**長得一模一樣**。

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

📎 日常操作是 `daemon -s` 加前端；rpc-cli 留著是因為「同程序內轉一圈」證明不了 WS、加密、token 那一段真的通。它可以很小。

## 1. 為什麼要這一層（三個現在就在痛的點）

不是為了架構美感。「一個命令一個程序」這件事撞到三面牆：

| 痛 | 一命令一程序 | daemon |
|---|---|---|
| **每個命令都要解鎖** | 每次都要 passphrase；想省打字只能讓明文主金鑰落地（舊的 `unlock.ticket`，已拿掉，/docs/design/storage/vault-and-keys.md §1） | 解鎖一次，主金鑰只在 daemon 的記憶體裡 |
| **matrix-sdk 的 store 是獨佔的** | Desktop 開著就不能同時用命令列（`crypto.db` 被鎖）。上游為此有 `enable_cross_process_store_lock`，但那是 iOS notification extension 的權宜之計，代價是每次操作搶鎖 | 一個程序持有 store，問題不存在 |
| **收不了推送** | event（含金鑰的 to-device）走 WS 推送，而一命令一程序的東西**沒有人在線上收**，斷開的期間就是漏 | daemon 常駐，連線與游標由它維護 |

第三點是決定性的：**daemon 不是風格選擇，是 WS 推送這條路推出來的必然**。

## 2. 分層

```
┌─────────────────────────────────────────────────────────┐
│ 前端（薄）  rpc-cli      Desktop        Android      Python │
│             Rust         Rust 原生      Kotlin       任意   │
└───────┬─────────┬────────────┬─────────────┬────────────────┘
        └─────────┴────────────┴─────────────┘
  控制 ws://127.0.0.1（加密的 JSON）  ＋  資料 http://127.0.0.1（bytes，Range）  /docs/design/rpc-specs/local-interface.md
                              │
┌─────────────────────────────┴───────────────────────────┐
│ daemon（crates/wbf-core ＋ crates/wbf-daemon）           │
│   session 與多帳號狀態、vault 解鎖一次、RPC 服務          │
│   事件分發、進度回報、與 server 的五條長連線與游標（§5.1）│
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

界線的語意完全不同：

| 界線 | 協議 | 加密 | 跨信任邊界？ |
|---|---|---|---|
| daemon ↔ server | **wbf-pack**（二進位） | 是（E2EE 的密文在裡面流動） | **是**：server 不可信 |
| 前端 ↔ daemon（控制） | **加密的 JSON over WS** | 是（XChaCha20-Poly1305，token 導出的金鑰） | **否**：同一台機器、同一個使用者 |
| 前端 ↔ daemon（資料） | **HTTP，支援 Range** | 否（明文 bytes，只在 loopback 上；靠 capability URL 擋） | 否 |

⚠️ 控制平面**加密的收穫主要是完整性**（/docs/design/rpc-specs/local-interface.md §4），不是機密性——能監聽 loopback 的人通常也能讀記憶體與 token 檔。
資料平面則刻意**不加密**：它要餵給播放器與圖片元件，那些只吃普通的 HTTP；防護靠 capability URL（/docs/design/rpc-specs/local-interface.md §8）。

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

## 4. 四個前端怎麼接

| 前端 | 怎麼啟動 daemon | 怎麼講話 |
|---|---|---|
| **daemon 自己的命令列** | 就是它自己：`daemon -s` 常駐；`daemon <命令>` 單發是測試性質，常駐中再叫會跳錯（§0.2；單發還沒做） | arg → RPC 訊息 → handle，不經 socket |
| **rpc-cli** | 連不到就自己 spawn 一個（使用者無感）。定位是**從外部程序丟 RPC 測整條路**（§0.2；還沒做，現在的 `apps/wbf-cli` 直接叫 core，§6） | 同一份 RPC |
| **Desktop** | 可能先用 **web** 當速成框架試 RPC（維護者 2026-09-09），之後再看要不要原生 Rust。內嵌或 spawn 都行 | 同一份 RPC |
| **Android**（JNI `.so`） | JNI 只有 **`daemon_start(data_dir, token_path) -> {rpc_port, data_port}`** 與 **`daemon_stop()`** | Kotlin 直接連 loopback WS（OkHttp 內建）；媒體 URL 直接餵 ExoPlayer |
| **Python** | spawn daemon 程序 | 同一份 RPC |

📎 ⚠️ **RPC vs uniffi 還沒定**：如果不要程序隔離，Desktop 與 Android 也可以用 uniffi 直接綁 library（matrix-rust-sdk 自己就是這樣給 Element X 用的），完全不需要 RPC。兩條路的取捨是**安全隔離 vs 簡單**，不是工作量——見 §7 第 7 點。

⚠️ **Android 那格是這個設計最大的收穫**：JNI 最痛的是跨語言型別轉換與生命週期，包一個大介面等於維護第二套 API。
只包「啟動／停止」兩個函數，其餘全部走 WS——**JNI 表面積是兩個函數，而且永遠不會長大**。
同程序內連 loopback 有點繞，但成本可忽略，換到的是「介面只有一個」。

📎 Android 的 WS 生命週期：**app 開著才連**，關掉或縮到背景就斷，回來再連。
維護者 2026-09-09：背景長連線（foreground service、FCM 喚醒）現在不考慮。
但斷線重連是常態，所以**協議必須是「拉窗＋游標＋ack」而不是純推送**——見 §5。

## 5. 這個架構回過頭對 server 協議的要求

daemon 常駐、但連線會斷（手機切背景、筆電睡眠、網路換手）。所以與 server 的每一種事件流都要能**從游標補齊**：

| 流 | 怎麼補齊 |
|---|---|
| 房間事件 | `Event/Recent` 拉窗＋水位（`cg_seq`）；什麼時候補是 UI 的事（/docs/design/rooms/room-sync.md） |
| **to-device（金鑰）** | `0x16 Device`：推送為主、`Fetch` 補洞、**`ItemsDestroy` 才刪**。線上格式在 wbfuwunel 的 /docs/design/wbf-wire-format.md §3.2 與 wbf-to-device.md；client 端在 /docs/design/keys/to-device-client.md、/docs/design/keys/key-sync.md |

📎 server 端沒有為此發明新機制：`get_to_device_events` 本來就吃游標、`remove_to_device_events` 就是刪除，
wbfuwunel 只是把它們接到通道上，外加「銷毀是帶結果的命令」的閉環。server 對 to-device 的內容仍然是瞎的
（只存 `type`／`sender`／`content`）。

### 5.1 事件流是**每個帳號一組**（維護者 2026-09-13 定）

daemon 的推播（/docs/design/rpc-specs/rpc-spec.md §4）不是憑空來的：**每個已經登入的帳號，daemon 都對它的 homeserver
維持一組連線**，事件從那裡進來、解密、寫進資料庫，然後才變成 RPC 的推播。

```
帳號 A ── 連 A 的 homeserver ──┐
帳號 B ── 連 B 的 homeserver ──┼──> daemon（解密、寫 DB）──> RPC 推播 ──> 各前端
帳號 C ── 連 C 的 homeserver ──┘
```

- ⚠️ **「一組」不是「一條」**：對 wbfuwunel 是五條 WS（§5.1.1），對一般 homeserver 是一條
  HTTP 的 sync。所以三個帳號同時登入、都在 wbf server 上，就是 **15 條 WS**。
- 帳號各自獨立：一個帳號的連線斷了、落後了、被登出了，🚫 不影響別的帳號。
  推播因此**一定帶 `user`**（/docs/design/rpc-specs/rpc-spec.md §4）——前端要分得出這是誰的事件。
- 📎 連線狀態的推播（`link.state`，/docs/design/rpc-specs/rpc-spec.md §4）講的是**那一個帳號**的某一條線，
  🚫 不是「daemon 連上網了沒」。

**用哪一種連線：預設 HTTP，wbf 專用的 homeserver 才走 WS**（維護者 2026-09-13 定）。

| homeserver | 走什麼 | 事件從哪來 |
|---|---|---|
| 一般 Matrix（Synapse、Dendrite…） | **HTTP**（matrix-sdk 的 `/sync`） | 長輪詢 |
| **wbfuwunel**（我們自己那套） | **五條 WS**（§5.1.1） | `Event/Push` 推送＋`Recent` 補洞 |

- ⭐ 判準是**「這台 server 講不講 wbf-pack」**，🚫 不是網域名、🚫 不是使用者設定裡的一個勾。
  問法：不帶 token 的 WS `Hello`，看回來的 `protocol` 認不認得（/docs/design/daemon/account-session.md §1）。
- **不確定就落到 HTTP**：連不上 WS、`Hello` 不回、協議認不得 —— 一律當成一般 homeserver。
  ⭐ 壞在「用了比較慢但一定能動的那條」，🚫 不壞在「以為對方懂我們的協議」。
- conf 的 `TRANSPORT`（/docs/design/rpc-specs/wbf-cli-spec.md §10）是**上限不是下限**：設成 `http` 就一律 HTTP、daemon 不開 WS 線（除錯用）；
  設成 `ws`（預設）仍然要探測，探不到照樣 HTTP。

### 5.1.1 對 wbfuwunel：一個帳號開**五條** WS，一條一個用途（維護者 2026-09-12 定、09-21 與 09-29 拆細）

🚫 **不是一條線全包。** 理由是**佇列是每條連線一份的**：server 端每條連線有自己的送出
佇列（`wbf_ws_send_queue_len`），塞滿了就**丟推送並標 `gap`**。把一次 200 MiB 的媒體下載
跟金鑰推送擺在同一條線上，那條佇列被媒體佔滿的時候，掉的是**金鑰**。

五條是 `Misc`（一問一答）、`Upload`、`Download`、`Rooms`（房間事件的訂閱）、`Keys`（金鑰的訂閱與 `Fetch`／銷毀）；
各自只做什麼、為什麼單獨一條、命令怎麼挑線，在 /docs/design/daemon/link-pool.md §1、§2。

- ⭐ 分界線是**「誰會塞爆佇列」與「掉了救不救得回來」的交叉**，🚫 不是「照 kind 分類」——`Misc` 收的就是各種不同 kind。
  媒體最會塞爆佇列，所以它**只能塞爆自己**（上傳與下載再各一條，一邊塞爆不拖另一邊）；金鑰🚨 **掉了就沒了**，所以要一條安靜的線。
- ⚠️ 這五條是**那一個帳號的**（§5.1）：兩個帳號登在同一台 wbf server 上也是各開各的，
  🚫 不共用 —— 它們的 `access_token` 不同，連線本來就分得開，而共用會讓一個帳號的流量塞爆另一個的。
- ⚠️ **`gap` 的意義因此是局部的**：房間那條標 `gap`，只表示房間事件漏了，🚫 不要當成整個 daemon 落後。
  各補各的：房間由 UI 叫 `sync.recent` 補（/docs/design/rooms/room-sync.md），金鑰由 daemon 從佇列頭 `Fetch`（/docs/design/keys/key-sync.md）。

### 5.2 一條連線上仍然會有好幾段會話——`seq` 屬於會話（wbfuwunel #42／#44 定）

⚠️ **一條線一個用途不代表每條線上只有一段會話。** 金鑰那條就是：一個常駐的 `Device/Subscribe` 會話，
加上補洞時的 `Fetch`／`ItemsDestroy`。server 也允許同一條線上多段會話交錯（wbfuwunel 的 `e2e7`
有一段叫 `two uploads interleaved on one connection`）。

> ⭐ **`id` 是一段會話的名字，`seq` 是那段會話裡的計數。** 換一個 `id` 就是新會話，`seq` 歸零。

| 🚫 daemon 不能這樣寫 | 會壞成什麼 |
|---|---|
| 連線層一個 `next_seq` | 同一條線上兩段會話互相看起來像對方漏號，`gap` 的判斷全毀 |
| 每個 kind 一個 `next_seq` | 同 kind 兩段會話交錯（兩個上傳）就分不出哪包是誰的 |

所以連線那一層收到的包先照 `id` 分派到會話，才輪到 `seq`（/docs/design/daemon/ws-receive-dispatch.md）。📎 `seq` 🚫 不是重送機制——
WS 不會掉單一 frame，跳號只代表 server 故意丟了一包（佇列滿），補救是帶游標重新要。

## 6. 現有 crate 怎麼重組

```
crates/wbf-wire     pack 的 codec
crates/wbf-sdk      協議、chunk 加解密、cache.db、媒體池、vault、matrix backend、crypto 引擎（OlmEngine）
crates/wbf-core     常駐狀態（多帳號 session、解鎖一次）、連線池、事件分發、命令本體。**沒有 RPC**
crates/wbf-daemon   core ＋ RPC 服務。library ＋ binary（wbf-matrix-client-daemon）。**自己的命令列**也在這裡（§0.2）
                    還沒做：資料平面（data_port 現在是 0）、單發命令
apps/wbf-cli        參數解析與 JSON 輸出，直接叫 core（不經 RPC）。
                    還沒做：改成 rpc-cli——只封裝 RPC 訊息、丟到本地 WS（§0.2），那時才改名
```

**`core` 與 `daemon` 刻意分開**，因為它們的命運不同：

| | 是什麼 | 選 RPC 時 | 選 uniffi 時（§7 第 7 點） |
|---|---|---|---|
| `wbf-core` | 狀態與邏輯，不知道有誰在跟它講話 | 用 | **照樣用** |
| `wbf-daemon` | 把 core 開一扇門出去 | 用 | 不需要 |

所以 RPC vs uniffi 還沒定也**不擋工作**：`wbf-core` 兩條路都要。

- **`wbf-sdk` 保持是純 library**（§8）：core 是它的使用者，不是它的一部分。
- **`wbf-daemon` 同時是 library 與 binary**：Desktop 內嵌用 library，其他人 spawn binary。
- ⚠️ **`wbf-core` 的公開介面不能假設「同程序」**：方法收 `&self`、參數與回傳用簡單型別、
  事件用 channel 而不是回呼引用、自己持有 tokio runtime 不要求宿主提供。
  🚫 公開介面上不要出現複雜生命週期、trait object、`impl Trait`；回傳是可序列化的 DTO，錯誤是 `CoreError { kind, message }`。
  這樣它之後包 RPC 或包 uniffi 都不用改。⚠️ 往 core 加方法一樣要過這一條。

### 6.1 現在那支 CLI 的假設要重新檢視

/docs/design/rpc-specs/wbf-cli-spec.md §9 那些簡化（沒有互動模式、不存密碼、stdout 只印一個 JSON 物件）都建立在
「它是開發與除錯工具，不是產品面」上。變成 rpc-cli 之後要重想的：

- `--token` 模式：那是「不碰 vault、不碰帳號目錄」的路徑，在 daemon 模型下是什麼意思？
- stdout「只印一個 JSON 物件」對 `watch` 這種串流命令本來就有例外（JSON Lines），RPC 的推播會讓這種情況變多。

## 7. 還開著的

編號固定（別的文件用「§7 第 N 點」引用），已經解決的第 1、2 點拿掉了，🚫 不重排。

- **第 3 點：fork 的 submodule**。`vendor/matrix-rust-sdk` 指向維護者建的
  [`WhiteBirchForumTeam/matrix-rust-sdk`](https://github.com/WhiteBirchForumTeam/matrix-rust-sdk)，
  跟上游只差一行：`crates/matrix-sdk/src/client/mod.rs` 的 `base_client()` 從 `pub(crate)` 改成 `pub`。
  ⚠️ `Room` 上**沒有 `encrypt`**：自己 Megolm 加密一律走 `OlmMachine`（`encrypt_room_event_raw`、`share_room_key`、
  `get_missing_sessions`、`receive_sync_changes` 本來就是 `pub`）。wbf 帳號的 `OlmEngine` 自己開 `OlmMachine`，不經 Client（§8）。
  📎 `Client::olm_machine_for_testing()` 也拿得到，但它掛在 `testing` feature 底下，會把 `wiremock` 等拖進出貨的 binary，只能當 spike。
- **第 4 點：daemon 的生命週期**：誰負責關掉它、閒置多久自己結束、多個前端同時連著時誰說了算。
  兩個 daemon 搶同一個資料目錄已經有答案：§0.2 的排他鎖，後來的起不來。
  ⚠️ 這是這份架構裡**複雜度真正的所在**——RPC 本身是機械工作，生命週期不是。要等 Desktop 的實際使用模式出來再定。
- **第 5 點：資料平面 token 的 TTL 與撤銷**：TTL 多長（播一部長片要多久？）、`logout` 時要不要立刻讓所有 token 失效
  （應該要）、同一個資源重複開要不要發新 token。暫定值在 /docs/design/rpc-specs/rpc-spec.md §6.2。
- **第 6 點：縮圖的批次**：一次要 50 張縮圖時，50 次 `media.open` 太吵。是走 base64 進 RPC（/docs/design/rpc-specs/local-interface.md §8 的 1 MiB 規則），
  還是發一張涵蓋多個資源的 token？後者違反「一張 token 一個資源」，要想清楚再定。
- **第 7 點：RPC 還是 uniffi**（這份文件假設 RPC）：
  📎 **Desktop 一旦走 web，這題實質上倒向 RPC**——web 前端沒有別的路。uniffi 只剩 Android 用得上。
  RPC 的**唯一**硬理由是**程序隔離＝安全隔離**——UI 要解圖片、解影片，那是 CVE 大戶；
  daemon 持有金鑰。分成兩個程序，UI 被一張惡意圖片打穿也拿不到 Megolm 金鑰與 `local.key`。
  維護者 2026-09-09 也提到同一件事：不想 Python 直接包 Rust，怕「Rust 掛了全部一起死」。
  反過來，uniffi（matrix-rust-sdk 自己就用它給 Element X）可以零序列化直接綁，Desktop 與 Android 都省事。
  **建議等 Desktop 有原型、摸到實際使用模式再定**；在那之前 `wbf-core` 的介面兩條路都能接（§6）。
  📎 如果選 uniffi，/docs/design/rpc-specs/local-interface.md 整份不需要——但 `wbf-core` 不會白做。
- **第 8 點：Desktop 的 UI 框架**：維護者 2026-09-09 定 **Desktop 可以先用 web** 當速成框架驗證 RPC，
  Android 不走 web。候選分兩批：短期的 web（Tauri／Electron／純瀏覽器頁面），
  長期若要原生則是 egui／iced／slint／gtk-rs 這一類。
  ⚠️ 原生 Rust GUI 的傳統弱項（長列表虛擬化、IME 中文輸入）要單獨驗；web 那批沒有這個問題，
  但多一層 runtime。評估維度見 /docs/handover.md §7。
- **第 9 點：續傳狀態檔放哪**：現在寫在被上傳的檔旁邊（`<file>.wbf-upload.json`，/docs/design/rpc-specs/wbf-cli-spec.md §6），那是一命令一程序時定的。
  daemon 不一定有那個目錄的寫入權，Android 的 SAF 連路徑都沒有（`crates/wbf-core/src/upload_ops.rs` 檔頭）。

## 8. 耦合方向：上游 SDK 是可以拆掉的零件，不是地基（維護者 2026-09-05 定）

維護者的原話，照錄：「不要依賴太重，能切乾淨就切乾淨，蓋下去之後，要拆開來就難了。現在剛起步，這是重點的重點。」

- **我們自己的東西越多，對上游的依賴越低。** 上游 `matrix-sdk` 不是要一次淘汰，是隨著我們的 work 長大自然變薄，最後變成 fallback。
- **WS 層是我們自己的協定**（pack）。wbf 帳號已經不建 matrix-sdk 的 Client（/docs/design/daemon/account-session.md）；上游那套只剩一般 Matrix 帳號在用。
- **crypto 只當「加密解密的引用」，引擎是我們的**：`matrix-sdk-crypto` 的 `OlmMachine` 包在 `crypto_engine::OlmEngine` 裡，呼叫者只看得到它。
  沒抽 trait：只有一個實作，抽了是儀式；真的要換引擎時再抽。
- **`matrix-sdk-base` 的型別不滲進我們的介面。**
- **整體架構往 Telegram 對齊**（房間、對話、媒體的使用方式），但**不丟掉 E2EE 的本質**。聊天模型在 /docs/design/rooms/chat-model.md。
- **與聯邦對接能兼容就盡量兼容**；我們自幹的 feature 是 extension，可以不兼容。
- 🚨 **任何會 breaking Matrix 兼容的地方，都要提出來審查**，由維護者定案要不要兼容。這條沒有例外。

落到程式上的規則：

| 規則 | 意思 |
|---|---|
| CLI 與 UI 只看我們的型別 | `matrix_sdk::Room`、`ruma::events::…` 不出現在 `wbf-sdk`／`wbf-core` 的 pub 介面 |
| 上游只出現在兩個模組 | `wbf-sdk/src/backend/matrix_sdk.rs`（一般 Matrix 帳號的房間）與 `wbf-sdk/src/crypto_engine.rs`（E2EE 引擎）；其他檔不 `use matrix_sdk` |
| 跨邊界只傳資料，不傳規則 | adapter 不知道 CLI 的政策（要不要警告、要不要落地）；CLI 不知道 adapter 底下是 HTTP 還是 pack |
| 每個 PR 要寫「這次新增了對上游的哪些依賴」 | 讓依賴的增長是看得見的，不是蓋下去才發現 |

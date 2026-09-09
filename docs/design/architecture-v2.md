# 架構 v2：kernel、RPC、與四個前端

> 維護者 2026-09-09 定的方向。這份文件講**分層與介面**，不講功能——功能在
> [`plan-v1.md`](plan-v1.md)、[`chat-model.md`](chat-model.md)、[`local-cache-db.md`](local-cache-db.md)、
> [`wbf-cli-spec.md`](wbf-cli-spec.md)。RPC 的逐條訊息在 [`rpc-spec.md`](rpc-spec.md)（還沒寫）。

## 0. 一句話

```
wbfuwunel ──wbf-pack（二進位，可加密）──> kernel ──127.0.0.1 JSON WS（明文）──> CLI / Desktop / Android / Python
```

**kernel 是內核**：所有通訊協議、加解密（matrix-sdk）、客製的 wbf 協議、本地資料、session 都在它裡面。
前端一律是**薄包裝**，只講一種語言：本地的 JSON RPC。

## 1. 為什麼要這一層（三個現在就在痛的點）

不是為了架構美感。現在的 CLI 是「一個命令一個程序」，這件事撞到三面牆：

| 痛 | 現在 | kernel 之後 |
|---|---|---|
| **每個命令都要解鎖** | 所以有了 `unlock.ticket`：**明文主金鑰落地** 15 分鐘。local-cache-db §4 自己標記這是妥協，接受它只因為「CLI 不是產品面」 | 解鎖一次，主金鑰只在 kernel 的記憶體裡。**那個妥協整個消失** |
| **matrix-sdk 的 store 是獨佔的** | Desktop 開著就不能同時用 CLI（`crypto.db` 被鎖）。上游為此有 `enable_cross_process_store_lock`，但那是 iOS notification extension 的權宜之計，代價是每次操作搶鎖 | 一個程序持有 store，問題不存在 |
| **收不了推送** | roadmap 要把 event（含金鑰的 to-device）改走 WS 推送，而一命令一程序的東西**沒有人在線上收**，斷開的期間就是漏 | kernel 常駐，連線與游標由它維護 |

第三點是決定性的：**kernel 不是風格選擇，是 WS 推送這條路推出來的必然**。

## 2. 分層

```
┌─────────────────────────────────────────────────────────┐
│ 前端（薄）  CLI          Desktop        Android      Python │
│             Rust         Rust 原生      Kotlin       任意   │
└───────┬─────────┬────────────┬─────────────┬────────────────┘
        └─────────┴────────────┴─────────────┘
     控制 ws://127.0.0.1（JSON）  ＋  資料 http://127.0.0.1（bytes，Range）  §4
                              │
┌─────────────────────────────┴───────────────────────────┐
│ kernel（crates/wbf-kernel）                              │
│   session 與多帳號狀態、vault 解鎖一次、RPC 服務          │
│   事件分發、進度回報、與 server 的長連線與游標            │
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
| kernel ↔ server | **wbf-pack**（二進位） | 是（E2EE 的密文在裡面流動） | **是**：server 不可信 |
| 前端 ↔ kernel（控制） | **JSON over WS** | **否，一律明文** | **否**：同一台機器、同一個使用者 |
| 前端 ↔ kernel（資料） | **HTTP，支援 Range** | 否（明文 bytes，只在 loopback 上） | 否 |

前端那條**不加密是刻意的**（維護者 2026-09-09）：它在 loopback 上、兩端是同一個人的同一台機器，加密只會讓四個語言各自實作一次密碼學，而**風險換不到東西**。真正的防護是 §4.3 的認證。

## 3. kernel 的職責邊界

**kernel 做**：

- 與 server 的所有通訊（wbf-pack WS、以及還沒搬過來的 HTTP）
- 加解密：matrix-sdk 的 Olm／Megolm、我們的 chunk 加解密
- 本地資料：`local.key`／vault、`cache.db`、媒體池、房間金鑰備份
- **session 與多帳號**：同時登入多個帳號，每個 RPC 請求指定用誰
- 事件分發：把收到的事件推給訂閱中的前端
- 長工作的進度（上傳、下載、`recent` 同步）

**kernel 不做**：

- 🚫 **不管 UI 狀態**：哪個房間被選中、捲到哪、草稿——那是前端的事
- 🚫 **不管顯示格式**：時間怎麼寫、名字怎麼縮、訊息怎麼排版
- 🚫 **不代前端做決定**：要不要下載一個 2 GB 的檔、要不要接受一個裝置驗證，一律問過

判準：**跨越這條界線的只有資料，不是政策**（全域 CLAUDE.md A4）。kernel 不需要知道前端是誰、長什麼樣。

## 4. 本地介面：兩個平面，兩個 port

```
控制平面   ws://127.0.0.1:<rpc port>     JSON    命令、狀態、事件、進度
資料平面   http://127.0.0.1:<data port>  bytes   媒體進出，支援 Range
```

### 4.1 為什麼分成兩個 port（維護者 2026-09-09 定）

**因為兩種連線的特性完全相反**：

| | 控制平面 | 資料平面 |
|---|---|---|
| 訊息大小 | 幾百 byte | 幾百 MB |
| 連線壽命 | 整場開著 | 一個傳輸一條 |
| 並行 | 一條就夠 | 播放器會同時開好幾條發 Range |
| buffer／timeout | 小、短 | 大、長 |

混在一個 listener 上，這四項全都要折衷，還要寫 HTTP upgrade 的分流。
分開之後兩邊各自調自己的，而且**資料平面完全不必懂 WS，控制平面完全不必懂 Range**。

### 4.2 為什麼控制平面是 WS 而不是 HTTP／stdio

要三件事同時成立：**雙向**（kernel 推事件給前端）、**多工**（一個下載跑著同時能送訊息）、**每個語言都好接**。
WS 三個都給；HTTP 要靠 SSE 或輪詢補雙向，stdio 在 Android 上不成立。

### 4.3 認證：loopback 不等於安全

⚠️ **同一台機器上任何程序都連得到 `127.0.0.1`**，包括別的使用者跑的東西。兩個平面的認證方式**刻意不同**：

**控制平面：一把全域 token**

1. kernel 啟動時綁 `127.0.0.1:0`（隨機 port），產生 32 byte 隨機 token。
2. 兩個 port 與控制平面的 token 寫進 `<data dir>/kernel.json`，**Unix 0600**；跟 `unlock.ticket` 同一套權限規矩（CLI 規格 §7.1）：

   ```json
   { "rpc_port": 51234, "data_port": 51235, "token": "<base64, 32 bytes>" }
   ```

3. 前端讀那個檔，握手的第一個訊息帶 token。**不對就關連線**，🚫 不回「token 錯」以外的資訊。
4. kernel 結束時刪掉那個檔。讀到過期的檔（連不上那個 port）就當作沒有 kernel。

**資料平面：每個資源一張 capability URL，🚫 沒有全域 token**

```
http://127.0.0.1:<data port>/media/<resource token>
```

- token **綁單一資源**（這一個檔、這一次傳輸）、**有 TTL**、**可以重複使用**。
  ⚠️ 不能「用一次就失效」：播放器 seek 一次就是一次新的 Range 請求，一個影片會發幾十次。
- token **放在 URL path 裡，不是 header**。理由是實務的：**有些媒體元件只吃 URL、不讓你設 header**
  （Android 的 ExoPlayer 可以設，很多圖片元件不行）。做成 capability URL，任何吃 URL 的東西都能直接用。
- 代價老實寫：URL 會進到那個元件自己的 log。在 loopback 上、短命、綁單一資源，可接受。
- 認不得的 token、過期的 token：一律 **404**，🚫 不分辨「不存在」與「過期」（那會變成探測工具）。

🚫 **兩個平面都不用「反正是 localhost 就放行」**：那等於同機器上任何程式都能讀你的訊息、拿你的 session。

### 4.4 控制平面的訊息形狀

三種，都是一行 JSON：

```jsonc
// 前端 → kernel
{ "id": 17, "method": "room.send_text", "params": { "account": "@alice:localhost", "room": "!abc:localhost", "body": "hi" } }

// kernel → 前端（對應某個 id）
{ "id": 17, "ok": true,  "result": { "event_id": "$xyz" } }
{ "id": 17, "ok": false, "error": { "code": "not_logged_in", "message": "…" } }

// kernel → 前端（沒有 id：推播）
{ "event": "room.message", "data": { "account": "…", "room": "…", "message": { … } } }
{ "event": "progress",     "data": { "id": 17, "done": 1200, "total": 4096 } }
```

- **`id` 由前端給**，單調遞增即可；kernel 原樣回。一個連線上可以同時有很多個未完成的 `id`。
- **`method` 是 `名詞.動詞`**（`account.add`、`room.send_text`、`media.download`），不是 CLI 的字串命令列——
  🚫 不要讓前端組命令列字串再由 kernel 解析，那是把 shell 的問題搬進 RPC。
- **錯誤有機器可讀的 `code`**（穩定、可比對）加上給人看的 `message`。exit code 那套留給 CLI 自己映射。
- **推播要先訂閱**（`subscribe`／`unsubscribe`），🚫 不預設把所有事件推給每個連線。

### 4.5 大資料走資料平面，不走 RPC

⚠️ 下載一個 2 GB 的檔不可能塞進 JSON，改成 binary frame 串流也會逼**每個前端各自實作一次串流組裝**。

⚠️ 而且**不能給檔案路徑**（2026-09-09 維護者指出，這是初稿的錯）：媒體池裡的東西是**加密的**
（local-cache-db §8），給前端一個池裡的路徑，它讀到的是密文；kernel 先解密寫到某個路徑，那就是
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

**播放／顯示**：

```jsonc
{ "id": 9, "method": "media.open", "params": { "account": "…", "event": "$xyz" } }
{ "id": 9, "ok": true, "result": {
    "url": "http://127.0.0.1:51235/media/9f3a…",
    "mimetype": "video/x-matroska", "size": 1073741824, "expires_in": 3600 } }
```

前端把 `url` 直接交給播放器／圖片元件，它自己發 Range。kernel 邊解密邊吐，
🚫 **不把整個檔案讀進記憶體**。

**上傳**：

```jsonc
{ "id": 12, "method": "media.create", "params": { "account": "…", "room": "…", "name": "video.mkv" } }
{ "id": 12, "ok": true, "result": { "url": "http://127.0.0.1:51235/upload/7c1b…" } }
// 前端 PUT bytes 進去；kernel 邊收邊加密邊走 chunk 上傳
{ "event": "progress", "data": { "id": 12, "done": 52428800, "total": 1073741824 } }
```

⚠️ **Android 沒有別的選擇**：SAF 給的是 `content://` URI，**根本沒有檔案路徑可給**。
所以 PUT 這條路在 Android 上不是「比較好」，是必要的。

**另存新檔**（使用者明確要把明文放到自己選的位置）仍然走路徑：

```jsonc
{ "id": 15, "method": "media.save_to", "params": { "account": "…", "event": "$xyz", "out": "/home/me/video.mkv" } }
```

這裡明文落地是**使用者要的**，不是我們偷偷做的——這條界線要守住。CLI 的 `download -o` 就是它。

📎 **小東西**（頭像縮圖之類）可以直接 base64 進 RPC 的 result，省一次來回。界線放在
單則 RPC 訊息 **1 MiB**，超過一律走資料平面。

📎 效能：多一次 loopback 的記憶體複製，但**少了一次磁碟往返**（本來是「解密→寫檔→播放器讀檔」，
現在是「解密→socket→播放器」），還不用清暫存檔。控制平面那邊一趟往返 < 0.1 ms，
比它後面接的 SQLite 查詢與 AEAD 解密都便宜——**不是瓶頸**。

## 5. 四個前端怎麼接

| 前端 | 怎麼啟動 kernel | 怎麼講話 |
|---|---|---|
| **CLI** | 連不到就自己 spawn 一個（使用者無感）。開發時不必先手動起 daemon | 同一份 RPC |
| **Desktop**（Rust 原生） | 同程序起一個 task（內嵌），或連外部的 | 同一份 RPC |
| **Android**（JNI `.so`） | JNI 只有 **`kernel_start(data_dir) -> {rpc_port, data_port, token}`** 與 **`kernel_stop()`** | Kotlin 直接連 loopback WS；媒體 URL 直接餵 ExoPlayer |
| **Python** | spawn kernel 程序 | 同一份 RPC |

⚠️ **Android 那格是這個設計最大的收穫**：JNI 最痛的是跨語言型別轉換與生命週期，包一個大介面等於維護第二套 API。
只包「啟動／停止」兩個函數，其餘全部走 WS＋JSON——**JNI 表面積是兩個函數，而且永遠不會長大**。
同程序內連 loopback 有點繞，但成本可忽略，換到的是「介面只有一個」。

📎 Android 的 WS 生命週期：**app 開著才連**，關掉或縮到背景就斷，回來再連。
維護者 2026-09-09：背景長連線（foreground service、FCM 喚醒）現在不考慮。
但斷線重連是常態，所以**協議必須是「拉窗＋游標＋ack」而不是純推送**——見 §6。

## 6. 這個架構回過頭對 server 協議的要求

kernel 常駐、但連線會斷（手機切背景、筆電睡眠、網路換手）。所以與 server 的每一種事件流都要能**從游標補齊**：

| 流 | 現況 |
|---|---|
| 房間事件 | ✅ `Event/Recent` 已經是拉窗＋水位（`cg_seq`） |
| **to-device（金鑰）** | ❌ 還沒有。設計時要**跟 `Recent` 同構**：拉窗＋游標＋ack |

📎 好消息：wbfuwunel 那邊 `get_to_device_events(user, device, since, to)` **本來就吃游標**，
`remove_to_device_events(user, device, until)` 就是 ack 之後的清理。所以 server 端要加的是**一個新的 opcode**，
不是一套新機制——它對 to-device 的內容本來就是瞎的（`add_to_device_event` 只存 `type` 字串與不透明的 `content`）。

## 7. 現有 crate 怎麼重組

```
crates/wbf-wire     不動：pack 的 codec
crates/wbf-sdk      不動：協議、chunk 加解密、cache.db、媒體池、vault、matrix backend
crates/wbf-kernel   新：常駐狀態（多帳號 session、解鎖一次）、RPC 服務、事件分發
apps/wbf-cli        瘦身：變成 RPC 的一個前端；命令的「做什麼」搬進 kernel
```

- **`wbf-sdk` 保持是純 library**（plan-v1 §7.2 的方向不變）：kernel 是它的使用者，不是它的一部分。
- **`wbf-kernel` 同時是 library 與 binary**：Desktop 內嵌用 library，其他人 spawn binary。
- ⚠️ **kernel 的公開介面要 FFI 友善**（Android 要 JNI）：方法收 `&self`、參數與回傳用簡單型別，
  🚫 公開介面上不要出現複雜生命週期、trait object、`impl Trait`；kernel 自己持有 tokio runtime，不要求宿主提供。
  好消息是這個約束**只作用在 `kernel_start`／`kernel_stop` 兩個函數上**（§5），其餘都在 WS 後面。

### 7.1 CLI 的假設要重新檢視

CLI 規格 §9 那些簡化（沒有互動模式、不存密碼、stdout 只印一個 JSON 物件）都建立在
「CLI 是開發與除錯工具，不是產品面」上。變成 RPC 前端之後：

- `unlock.ticket` **可以拿掉**（§1）：解鎖狀態在 kernel 裡。
- `--token` 模式要重想：那是「不碰 vault、不碰帳號目錄」的路徑，在 kernel 模型下是什麼意思？
- stdout「只印一個 JSON 物件」對 `watch` 這種串流命令本來就有例外（JSON Lines），RPC 的推播會讓這種情況變多。

## 8. 還開著的

1. **RPC 的逐條訊息規格**（`rpc-spec.md`）：method 清單、每個的 params／result、錯誤 code 表、事件清單。這份文件只定形狀。
2. **HTTP 的去留**：`/sync` 之外 matrix-sdk 還會打哪些 HTTP、頻率多少——**要實測過再決定**要不要做 loopback shim
   （把 SDK 的 HTTP 轉進 WS）。⚠️ matrix-sdk 的 transport **不可插拔**（`ClientBuilder::http_client()` 只吃
   具體的 `reqwest::Client`，`HttpClient` 是 `pub(crate)`），所以 shim 的做法是把 `homeserver_url` 指到本機的
   listener，不是實作一個 trait。
3. **fork submodule 的範圍**：`Client::base_client()` 或 `olm_machine()` 目前是 `pub(crate)`，
   而 to-device 要餵給 SDK 需要 `OlmMachine::receive_sync_changes`（那個是 pub）。
   維護者還沒定要不要 fork——這跟 `Room::encrypt`（chat-model／handover §7）是**同一個決策**，一起定。
4. **kernel 的生命週期**：誰負責關掉它、閒置多久自己結束、多個前端同時連著時誰說了算、
   兩個前端同時 spawn 時怎麼收斂（lock 檔？先到先贏？）。
   ⚠️ 這是這份架構裡**複雜度真正的所在**——RPC 本身是機械工作，生命週期不是。
   現在定會是憑空猜，要等 Desktop 的實際使用模式出來。
5. **資料平面 token 的 TTL 與撤銷**：TTL 多長（播一部長片要多久？）、`logout` 時要不要立刻讓所有 token 失效
   （應該要）、同一個資源重複開要不要發新 token。
6. **縮圖的批次**：一次要 50 張縮圖時，50 次 `media.open` 太吵。是走 base64 進 RPC（§4.5 的 1 MiB 規則），
   還是發一張涵蓋多個資源的 token？後者違反「一張 token 一個資源」，要想清楚再定。
7. **Desktop 的 UI 框架**：不走 web（維護者 2026-09-09），候選是 egui／iced／slint／gtk-rs 這一類。
   評估維度見 handover §7；長列表虛擬化與 IME（中文輸入）是原生 Rust GUI 的傳統弱項，要單獨驗。

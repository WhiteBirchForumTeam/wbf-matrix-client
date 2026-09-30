# 本地介面：兩個平面，兩個 port（維護者 2026-09-09 定案）

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

## 1 為什麼分成兩個 port

**兩種連線的特性完全相反**：

| | 控制平面 | 資料平面 |
|---|---|---|
| 訊息大小 | 幾百 byte | 幾百 MB |
| 連線壽命 | 整場開著 | 一個傳輸一條 |
| 並行 | 一條就夠 | 播放器會同時開好幾條發 Range |
| buffer／timeout | 小、短 | 大、長 |

混在一個 listener 上這四項全要折衷，還要寫 HTTP upgrade 的分流。

## 2 控制平面為什麼是 WebSocket

**因為前端可能是瀏覽器，而瀏覽器不能開裸 TCP socket**（維護者 2026-09-09：Desktop 可能先用 web
當速成框架試 RPC）。JS 只有 WebSocket、fetch、WebTransport 三種，其中只有 WS 是雙向的。

📎 評估過**裸 TCP ＋ NDJSON**（一行一個 JSON，省掉 WS 的握手與 frame），
在「所有前端都是原生程式」的前提下它更輕。web 前端一進來這個前提就沒了，所以放棄。
📎 也評估過 **UDS／named pipe**（省 port、認證交給檔案權限），查完跨平台之後放棄：
Rust 在 Windows 沒有 `UnixListener`，而 **Python 在 Windows 根本不支援 `AF_UNIX`**——
Windows 是主力平台、Python 是要支援的前端，那個組合直接不成立。

WS 的成本不高：Rust 已經在用 `tokio-tungstenite`（對 server 那條），Python 有 `websockets`，
Kotlin 的 OkHttp 內建，JS 原生。

## 3 token：前端產生，前端叫起 daemon

**前端與 daemon 必然成對出現**（維護者 2026-09-09）——有 rpc-cli 就有 daemon，有 Desktop 就有 daemon。
所以 token 由**前端**產生，不是 daemon。⭐ 而它落在磁碟上**只是為了交給一個還沒啟動的程序**，
所以那段落地時間要盡量短（維護者 2026-09-13 定的五步）：

| 步 | 誰 | 做什麼 |
|---|---|---|
| 1 | 前端 | 產生 **256 byte 隨機檔**（例如 `<data dir>/daemon.token`，Unix **0600**） |
| 2 | 前端 | spawn `daemon -s --token-file <路徑>`。🚫 **不用命令列傳內容**：那會進 `ps` 輸出 |
| 3 | daemon | 讀它、導出金鑰（§4）、開 port、**宣告 ready** |
| 4 | 前端 | 收到 ready（或自己 `hello` 成功）之後**抹掉**那個檔 |
| 5 | — | 此後 token 只活在兩個程序的記憶體裡，**磁碟上沒有** |

- ⚠️ **daemon 讀完就不再回頭讀那個路徑**，所以第 4 步刪掉它不會讓任何東西壞掉（`crates/wbf-daemon/tests/loopback.rs`
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
   📎 `instance` 是這次啟動鑄的 UUID v4，`hello` 與 `daemon.info` 回的是**同一個**（/docs/design/rpc-specs/rpc-spec.md §1.3）：
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

## 4 每則控制訊息都加密（token 就是金鑰材料）

**WS 上一律 binary frame**，每個 frame 是 RPC 自己的極簡 pack：`ver(1) ‖ type(1) ‖ data`
（維護者 2026-09-12 定；欄位與兩個階段的規則在 /docs/design/rpc-specs/rpc-spec.md §1，這裡只放密碼學的部分）。
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
  ⚠️ 但**關之前先送一包 `type = 0x01`（明文）講原因**（`BAD_TOKEN` 之類，/docs/design/rpc-specs/rpc-spec.md §1.4）——
  不然 token 錯的人只看到斷線，什麼提示都沒有。`0x01` 在預設狀態下**只有這一種用途**，
  而且它的 JSON **跟正常回應同一個形狀**（`code` 9xxx、`result.close`），前端的 frame 翻譯器只有一條路。
- **加密是 daemon 的全局狀態 `encryption_enforced`，預設開**：開著時 client 送 `0x01` 一律拒絕；
  只有走密文呼叫 `daemon.set_encryption { enforced: false }` 才降級（除錯用，/docs/design/rpc-specs/rpc-spec.md §1.1）。
  🚫 所以 `hello` **不帶 token 欄位**，它只用來協商協議版本（一個協商表，不是一個數字）、報上 client 名字
  （正式名稱、`wbf-matrix` 開頭，/docs/design/rpc-specs/rpc-spec.md §1.3）。
- frame 上限 **1 MiB**：超過就關連線（🚫 不讓對方用一個巨大 frame 把記憶體吃光）；
  這個數字跟 §8「超過就走資料平面」是同一個。

**為什麼要加密（而不是只靠 token 認證）**

維護者 2026-09-09：既然可選，傾向用**有保護力**的。
⚠️ 老實說收穫的邊界：加密擋的攻擊者有限——能監聽 loopback 的人（要 root／管理員或 npcap）
通常也能讀你的記憶體與 token 檔。**真正的收穫是完整性**：Poly1305 標籤讓「改一個 bit 讓
`false` 變 `true`」這種事驗不過。🚫 所以這裡要的是 **AEAD**，不是任何形式的自製混淆——
沒有標籤的東西擋不住惡意修改，而控制平面上一個被改過的 `true` 就足以刪掉東西。

📎 用的是 `XChaCha20-Poly1305`，跟 chunk 加密、vault、目錄名加密同一個 crate，沒有新依賴。

📎 因為訊息是加密的，**內容是 JSON 就夠了，不需要 BSON**（維護者 2026-09-09 定）：
加密之後本來就是二進位，BSON 只會多一個每個語言都要裝的依賴，還讓解密後的 log 變得不可讀。

## 5 daemon 的兩段式狀態

daemon 起來時**一律是未解鎖**（`plain` 模式也一樣：前端要叫一次不帶參數的 `vault.unlock`）——但兩個 port 照樣開著：

| | 控制平面（WS） | 資料平面（HTTP） |
|---|---|---|
| **未解鎖** | 開著，但只接受 `hello`、`daemon.*`、`vault.*`；其他一律回 `1001`（有 `local.key` 但鎖著）或 `1002`（還沒有 `local.key`，去 `vault.create`） | **503 Service Unavailable** |
| **已解鎖** | 全部 method | 正常 |

```jsonc
{ "method": "vault.unlock", "params": { "passphrase_base64": "…" }, "id": 1 }
// passphrase 是任意 bytes（/docs/design/storage/vault-and-keys.md §3），不一定是字串；plain 模式不帶 params
```

- **passphrase 只留在 daemon 的記憶體裡，直到 daemon 關閉**（維護者 2026-09-09）。
  所以不需要 `unlock.ticket` 那種「明文主金鑰落地」的妥協（/docs/design/storage/vault-and-keys.md §1）——這是 daemon 最直接的安全收穫。
- 🚫 **沒有「鎖回去」**：`Core` 解鎖一次就活到程序結束，daemon 沒有 `vault.lock`（/docs/design/rpc-specs/rpc-spec.md §3.1）。
  UI 的 lock／unlock 是 **UI 自己那一層**的事 —— daemon 照樣連著、照樣寫 DB、照樣發通知，
  ⭐ 因為使用者按 lock 通常只是暫時離開，回來要看到這段時間的訊息。真的要讓金鑰離開記憶體
  就是 `daemon.shutdown` 再 `daemon -s`。
- 🚫 daemon **不自己去問終端**：那樣 Desktop 與 Android 沒辦法解鎖。passphrase 一律從 RPC 進來（🚫 也不收 `passphrase_file`，/docs/design/rpc-specs/rpc-spec.md §3.1）。
- 未解鎖時資料平面回 **503 而不是 404**：媒體確實存在，只是現在打不開——這個區別對前端有意義。

## 6 訊息形狀（加密之前的內容）

**形狀借自 JSON-RPC 2.0 的習慣，但這是我們自己的協議**（維護者 2026-09-09 定）：
`method` ＋ `params` ＋ `id`、有 `id` 要回、沒有 `id` 不回——這幾條照抄，因為它們好用。

⚠️ 但**回應的形狀不一樣**（標準是 `result`／`error` 二選一，這裡是 `code`／`msg` 平鋪），
所以 🚫 **不宣告 `"jsonrpc": "2.0"`**，也不要拿現成的 JSON-RPC library 來接——
與其假裝相容然後在某個角落炸掉，不如一開始就說清楚這是自己的東西。
版本識別走 `hello` 的 `protocol` 欄位（/docs/design/rpc-specs/rpc-spec.md §1.3）。

**請求**：

```json
{
  "method": "room.send_text",
  "params": { "user": "@alice:localhost", "room": "!abc:localhost", "body": "hi" },
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
  "params": { "user": "…", "room": "…", "message": { } }
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
  `msg` 是給人看的，🚫 前端不要拿它做邏輯。code 表在 /docs/design/rpc-specs/rpc-spec.md §5。
- **失敗時 `result` 是 `null`**，🚫 不要省略那個欄位——欄位固定在，弱型別的前端少一種 undefined 要處理。
- **推播要先訂閱**（`subscribe`／`unsubscribe`），🚫 不預設把所有事件推給每條連線。
- **framing 由 WS 給**：一個 binary frame 就是一則訊息，🚫 我們不自己切。

## 7 連線數：實務上 1:1，但不寫死

維護者 2026-09-09：**幾乎必然只會 1:1，RPC 只服務一個 client。**

所以 🚫 **不做任何「哪個 client 是主」的概念**——沒有主從、沒有權限分級、沒有連線間的協調。

但也**不硬性拒絕第二條連線**：那反而要寫拒絕邏輯與錯誤碼，而且 Desktop 開著時想跑一下 CLI 就被擋
（開發時很煩）。事件推播本來就要用 broadcast channel，多一條訂閱者是免費的。

**結論：允許多條連線，每條都平等、各自訂閱，daemon 不記得誰比較重要。**

## 8 大資料走資料平面，不走 RPC

還沒做：daemon 還沒開資料平面（`media.open`／`media.create` 與兩個 HTTP 路徑，/docs/design/rpc-specs/rpc-spec.md §6、§10）。

⚠️ 下載一個 2 GB 的檔不可能塞進 JSON，改成 binary frame 串流也會逼**每個前端各自實作一次串流組裝**。

⚠️ 而且**不能給檔案路徑**（維護者 2026-09-09）：媒體池裡的東西是**加密的**
（media-pool），給前端一個池裡的路徑，它讀到的是密文；daemon 先解密寫到某個路徑，那就是
**明文落地**——整個加密池的意義就沒了。

所以 bytes 走資料平面，而它是 HTTP **因為媒體最終要餵給既有的消費者，而那些消費者只吃 URL 或路徑**：

| 消費者 | 吃什麼 |
|---|---|
| Android ExoPlayer | URL（或自訂 DataSource） |
| Desktop 的影片（GStreamer／ffmpeg／libmpv） | URL 或路徑 |
| 圖片解碼器 | bytes 或 Reader |

既然不能給路徑，剩下的通用介面就只有 **URL**。所以 loopback HTTP 不是多造一個輪子，
是**把加密池接上這些現成輪子的唯一接頭**。Range 也不是額外工作：媒體池的 64 KiB 分段
本來就是為隨機讀設計的（/docs/design/media/media-pool.md §1），`seek` 的語意早就定好了（/docs/design/media/wbf-client-convention-for-chunk.md §7）。

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
- **未解鎖時一律 503**（§5）：媒體確實存在，只是現在打不開——這跟 404「沒這個東西」不一樣。

**播放／顯示**：

```jsonc
{ "method": "media.open", "params": { "user": "…", "room": "…", "event_id": "$xyz" }, "id": 9 }
{ "code": 0, "msg": "ok", "id": 9, "result": {
    "url": "http://127.0.0.1:51235/media/9f3a…",
    "mimetype": "video/x-matroska", "size": 1073741824, "expires_in": 3600 } }
```

前端把 `url` 直接交給播放器／圖片元件，它自己發 Range。daemon 邊解密邊吐，
🚫 **不把整個檔案讀進記憶體**。

**上傳**：

```jsonc
{ "method": "media.create", "params": { "user": "…", "room": "…", "name": "video.mkv" }, "id": 12 }
{ "code": 0, "msg": "ok", "result": { "upload_id": 77, "mxc": "…", "url": "http://127.0.0.1:51235/upload/7c1b…", "expires_in": 3600 }, "id": 12 }
// 前端 PUT bytes 進去；daemon 邊收邊加密邊走 chunk 上傳。進度就是 PUT 送出去多少，🚫 不走 RPC（/docs/design/rpc-specs/rpc-spec.md §4）
```

⚠️ **Android 沒有別的選擇**：SAF 給的是 `content://` URI，**根本沒有檔案路徑可給**。
所以 PUT 這條路在 Android 上不是「比較好」，是必要的。

**另存新檔**（使用者明確要把明文放到自己選的位置）仍然走路徑：

```jsonc
{ "method": "media.save_to", "params": { "user": "…", "manifest": { … }, "out": "/home/me/video.mkv" }, "id": 15 }
```

這裡明文落地是**使用者要的**，不是我們偷偷做的——這條界線要守住。rpc-cli 的 `download -o` 就是它。

📎 **小東西**（頭像縮圖之類）可以直接 base64 進 RPC 的 result，省一次來回。界線放在
單則 RPC 訊息 **1 MiB**（跟 §4 的 frame 上限同一個數），超過一律走資料平面。

📎 效能：多一次 loopback 的記憶體複製，但**少了一次磁碟往返**（本來是「解密→寫檔→播放器讀檔」，
現在是「解密→socket→播放器」），還不用清暫存檔。控制平面那邊一趟往返 < 0.1 ms，
比它後面接的 SQLite 查詢與 AEAD 解密都便宜——**不是瓶頸**。

## 9 閘門鏈，與一則附件訊息的完整流程（維護者 2026-09-12 定）

**理想狀態：資料庫只有 daemon 碰。** daemon 是後端，UI 只是 RPC call；
誰解密由 daemon 決定（matrix-sdk 的 Megolm、我們的 chunk 解密），解完存進 `cache.db`，
**前端看到的一律是明文**。反過來，前端送出的也是明文，要不要加密是 daemon 依房間狀態決定。

整條閘門鏈，從外到內：

```
homeserver  <=>  daemon 的 WS 協議層（wire、五條連線 /docs/design/overview/architecture-v2.md §5.1.1）
            <=>  daemon handle（命令本體：core）
            <=>  RPC 轉換（JSON ↔ handle 的型別；命令列 arg 也在這裡轉，/docs/design/overview/architecture-v2.md §0.2）
            <=>  本地 WS（加密的 JSON，開給前端）
```

每一層只跟隔壁講話：前端不知道 wire，wire 不知道 RPC。

**上傳一個大檔當附件**（走我們自己的分片協議）會拆成兩三個來回，這是複雜化的地方，寫清楚：

| # | 誰 | 做什麼 |
|---|---|---|
| 1 | 前端 → daemon | RPC `media.create`（§8）：檔名、大小、目標房間 |
| 2 | daemon → server | 去 server **建檔**（`Upload/Create`），拿回檔案的 URL／id |
| 3 | daemon → 前端 | RPC 回 result：server 的 URL ＋ 資料平面的 PUT URL |
| 4 | 前端 → daemon | 以那個 `upload_id` 為基礎**發一則附件訊息**（RPC `room.send_attachment`，內容是明文） |
| 5 | daemon → server | 房間有 E2EE 就 Megolm 加密、沒有就明文，送到 server。附件宣告（/docs/design/media/wbf-client-convention-for-chunk.md §5.2）在這一步帶 |
| 6 | 前端 → daemon | **同時**開始 PUT bytes 到資料平面的 URL（一個 HTTP 連線，不斷送） |
| 7 | daemon → server | 邊收邊做 chunk 加密、邊走 `Upload/*` 上傳到 homeserver |
| 8 | daemon → 前端 | `Seal` 完成之後 PUT 才回 `200`，body 是 manifest（/docs/design/rpc-specs/rpc-spec.md §6.2）。進度就是 PUT 送出去多少，🚫 不走 RPC |

⚠️ 第 4 與第 6 步**並行**：訊息不必等檔案傳完才發（訊息裡只有 URL 與描述），
接收端拿到訊息時檔案可能還在傳——這正是分片協議與 `seek` 存在的原因（/docs/design/media/wbf-client-convention-for-chunk.md §7）。
⚠️ 第 5 步失敗與第 7 步失敗是**兩件事**，各自回錯誤、各自可重試，🚫 不要綁成一個交易。


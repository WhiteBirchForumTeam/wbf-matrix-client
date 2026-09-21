# WS 收包分派：一條連線、任何順序、依會話表交付

> 維護者 2026-09-21 定的形狀（daemon-runtime §11 的第 4 階段，SDK 這一半）。
> 之前 `WsChannel` 是「送一個、等一個」，而且把不是回覆的推播丟在地上（PR #49 的暫時做法）——
> 維護者指出那是設計錯誤：**送一個等一個沒錯，錯的是「下一個收到的就是我的回覆」**。
> 收到什麼就依它的種類處理，順序完全亂掉也不該出事；會話狀態另外維護一張 id 表。
> 這份文件是那張表的形狀，實作在 `crates/wbf-sdk/src/{transport,sessions,link}.rs`。

## 0. 一句話

一條 WebSocket 連線 ＝ **一個讀取 task ＋ 一個送出 task ＋ 一張會話表**。送與收是兩件事：送只等佇列有位子，
永遠不等回覆；收進來的每個 pack 由讀取 task 查表交給在等的那個人，沒人等就是「無主」，計數、不亂交。
每個收到的 pack 都會經過一個**鉤子**，之後 daemon 的 RPC 面拿它決定要不要送到 UI；這一層只呼叫，不判斷。

跟 server 那邊同形（wire-format §5：兩端對稱的四步「封裝 → 送出 → 接收 → 拆包 → 派發」；
server 的實作是 `api/client/wbf/ws.rs` 的接收迴圈與 `service/streams/` 的註冊表）。

## 1. 三層，各回答一個問題

| 層 | 檔案 | 只回答 | 🚫 不做 |
|---|---|---|---|
| **傳輸** | `transport.rs` | bytes 怎麼進出：`FrameSource`（收一個 binary frame）、`FrameSink`（送一個）。tungstenite 一組實作、記憶體對接一組實作（測試用） | 任何協議邏輯。它不知道什麼是 pack |
| **會話表** | `sessions.rs` | 這個 pack 是誰的：`SessionKey` → `PackSink`；分派規則；無主計數；鉤子 | 網路。它是純資料結構，同步、可單元測試 |
| **連線** | `link.rs` | 把上面兩個接起來：`WsLink` 起兩個 task、給呼叫端「登記 → 送 → 等」的 API；關線時把表清掉 | 重連。那是第 8 階段監督者的事 |
| 通道 | `channel.rs`（既有） | `WsChannel` 變成 `WsLink` 的薄殼，`PackChannel` 介面**不動**：`HttpChannel`、假 server、`WbfClient` 一個字不改 | |

⭐ 四條線的設計（architecture-v2 §6.1.1：房間、金鑰、媒體、雜項）**不進這一層**。一條 `WsLink` 一張表；daemon 開四條就是四個實例，
哪個命令走哪條線由 daemon 決定，link 自己不知道它是做什麼的。這就是「通用」：只用一條線也行，只是擠。

## 2. 會話表的鍵：`id` 是會話的名字，`seq` 是會話內的計數

照 wire-format §4.1（PR #42 定案）。兩種鍵：

| 鍵 | 誰 | 一個項目收幾個 pack |
|---|---|---|
| `Session(id)` | 具名會話：`Recent`、`Device/Fetch`、`ItemsDestroy`、`Subscribe`（`id` 由 client 選，`id::SESSION` 型別） | 多個：所有抄這個 `id` 的 pack，包含推播（`Push`、`CryptoState`、`DeviceChanged`）與 **`Superseded`**（它的 `id` 就是訂閱的 id，wire-format §3.4） |
| `Reply { id, seq }` | 一問一答：`Hello`、`Ping`、`Info`、`Read`、`Send`、橋、`Upload/Chunk`／`Status`／`Seal`／`Abort`、`Unsubscribe` | 一個：`id` 與 `seq` 都抄回的那個 Control 回應 |

### 2.1 分派規則（讀取 task 每收一個 pack 走一次）

```
1. pack.id ≠ 0 而且表裡有 Session(pack.id)            → 交給它
2. 表裡有 Reply { pack.id, pack.seq }                  → 交給它
3. pack 是 Control 回應（IS_RESPONSE）而且表裡有 Reply { 0, pack.seq }
                                                       → 交給它（Upload/Create 的例外：server 把新發的上傳 id 放在標頭；
                                                          `expect_ack` 只對 Create 放行這種 id，這裡只負責送到）
4. 都沒有                                              → 無主：計數、debug log、交給診斷用的無主接收器（有登記的話），否則丟
```

- 📌 `unmatched` 是**診斷數字不是錯**：呼叫端在 `request_stream` 裡喊停之後（handle 丟掉、表項拿掉），server 那端的會話還在、後面的 Batch 照送，那些就算在這裡。
  🚫 不要拿它當連線壞了的訊號（PR #52 審查 cirno 💡4）。

- ⭐ **活著的會話擁有它 `id` 底下的每一個 pack**（第 1 條排最前）。所以🚫 不要拿一段活著的會話的 id 去發一問一答的請求
  （例：拿訂閱的 id 送 `Unsubscribe`）——那個 Ack 會進訂閱的收件匣。現在 `WbfClient` 每個命令都發新的會話 id，沒有這個問題；
  這條是給以後的人看的。
- 🚨 **fail closed**：無主的 pack 🚫 永遠不交給「剛好在等」的人。第 4 條之前沒有任何「猜」。
- 🚨 **登記一定在送出之前**。回覆比登記早到就變無主——server 那邊補窗曾因「先收窗再註冊」漏事件（wbf-event-push.md §7），同型的錯。

### 2.2 為什麼是 `Reply { id, seq }` 而不是 `Unordered(seq)`

`Upload/Chunk` 的 `id` 是上傳 id、`seq` 是塊索引，回應兩個都抄。用 `(id, seq)` 當鍵，將來滑動窗口（同一個上傳多塊在飛）
天然分得開；只用 `seq` 會把兩個上傳的第 3 塊混成一個。

## 3. 會話項：一個 handler 加一個結束規則（維護者要的彈性）

```rust
pub trait PackSink: Send {
    fn route(&self) -> Route;                            // Oneshot、Stream、Subscription：只給鉤子與 log 看
    fn deliver(&mut self, pack: Pack) -> Delivery;       // Kept（項目留著）或 Finished（讀取 task 把它從表裡拿掉）
    fn fail(self: Box<Self>, reason: SdkError);          // 關線時每一項都會被叫到
}
```

讀取 task 只做 `deliver`，自己不解讀 pack。內建三種，都只是把 pack 丟進 tokio channel：

| 實作 | 結束 | 滿了 | 誰用 |
|---|---|---|---|
| `OneshotSink` | 收到一個就 `Finished` | 不會滿 | `Reply` 鍵的全部 |
| `StreamSink`（有界 mpsc） | 消費端說夠了（`request_stream` 的 `on_pack` 回 false）就從表裡拿掉；或 pack 帶 `IS_LAST`／是 `Control/Error` | 🚨 **不丟、不擋讀取 task**：這段會話**失敗**（消費端收到 `Protocol` 錯）。串流的窗有 server 的則數與位元組上限，一個正常的消費者不會塞滿它；塞滿是消費端的 bug，要響 | `Recent`、`Fetch`、`ItemsDestroy`、`Subscribe` 的 Ack＋`CryptoState` |
| `SubscriptionSink`（有界 mpsc，長活） | `Unsubscribe`（呼叫端關 handle）、或收到帶這個 id 的 `Control/Error`（`Superseded`；當成終點交給消費端） | **丟那個 pack、在 handle 上標 `gap`**。跟 server 的 `gap` 同義：推播只是「不用輪詢」，正確性由 `Recent`／`Fetch`／1506 守 | Device 訂閱、之後的 Event 訂閱 |

- 將來有特規的 spec，寫一個新的 `PackSink` 登記進表，讀取 task 與表一個字不改。
- 每個 handle 帶 `connection_id`（程序內遞增），錯誤與 log 能說「第 3 條連線關了，2 個會話收到錯」。
- **世代號**：`register` 每次發一個 `SessionGeneration`，handle 的 Drop 只拿自己那一代（`remove_if`）。同一個 id 先後兩段會話（`IS_LAST` 收掉舊的、同 id 再開新的）時，
  舊 handle 晚一點才 drop 🚫 不能把新會話從表裡拿掉（PR #52 審查 salvia 🟡3）。
- 📎 為什麼串流不擋讀取 task：擋住的話同一條線上的訂閱跟著停；而且會有死鎖的形狀——串流的消費端如果在等同一條線上的另一個回覆，
  讀取 task 卡在交付、回覆永遠進不來。讓它失敗比讓它掛著好，而且失敗是可見的。

## 4. 鉤子：每個收到的 pack 都經過，這一層不判斷

```rust
pub struct Received<'a> {
    pub connection_id: u64,
    pub session: Option<SessionKey>,   // None ＝ 無主
    pub route: Route,                  // Oneshot、Stream、Subscription、Unmatched
    pub pack: &'a Pack,
}
pub type ReceivedHook = Arc<dyn Fn(&Received) + Send + Sync>;
```

- `WsLink::start` 帶一個 `ReceivedHook`，預設是空函數。讀取 task 每 decode 一個 pack：**鎖內查表**（`classify`，不動表）→ **放鎖叫鉤子** → **鎖內交付**（`dispatch`）；
  不管有沒有人在等，無主的也叫，`route = Unmatched`。
- 鉤子在表鎖**之外**（PR #52 審查 cirno／salvia 🟡2：`std::sync::Mutex` 不可重入，鎖內叫鉤子的話鉤子裡摸回同一條 link 就自死鎖）：
  鉤子裡可以讀同一條 link 的同步狀態（`unmatched()`、`is_closed()`）；🚫 不能在裡面 block 等這條 link 的回覆，那是等自己。
  `Received.session`／`route` 是查表那一刻的答案；交付在放鎖之後，中間項目被拿掉的話交付會算成無主（差一個 pack、只影響計數）。
- **要不要送到 UI 是鉤子那頭決定的**（之後 daemon 的 RPC 面那支「送到 UI」的函數）。link 只有呼叫，沒有過濾。
- 鉤子是同步、不可等待的：要做慢事（寫庫、推 RPC）就自己丟進自己的佇列，讀取 task 不被它拖住。
  daemon-runtime §4.1 的「先寫庫、後發事件」仍由那頭守。
- 鉤子與會話表是兩條路：會話項交付給在等的呼叫者，鉤子交付給 UI 那條線。同一個 pack 兩邊都拿得到。

## 5. 送出：一條佇列、一個 task、天然保序

- `WsLink::send(pack)`：encode → 進有界 mpsc（`SEND_QUEUE_PACKS`）→ 送出 task 逐一寫進 sink。佇列滿就等（背壓），🚫 不丟。
  有序類（`Chunk`）由呼叫端按 `seq` 送，單一 task 寫 sink 所以順序不會亂。
- 送出 task 寫 sink 失敗 → 直接走 §7 的 `shut_down`（🚫 不是只有自己停：讀取 task 那邊 socket 可能還活著，看不出異狀）；之後每個 `send` 立刻回 `Network`。

## 6. ACK 與重送：規則在表之上，不在表裡

- `WsLink::request(pack, timeout)`：登記 `Reply` → 送 → 等；逾時就把項目拿掉（晚到的回覆變無主）。
- **逾時分兩種**（PR #52 審查 cirno 🟡3）：連線還活著只是 server 沒聲→ `SdkError::Timeout`（可以重試）；連線沒了→ `Network`（要重連）。
  `request`、`StreamHandle::next`、`Subscription::next` 都這樣分，daemon 不用靠字串判。
- `WsLink::request_with_policy(pack, AckPolicy { attempts, timeout })`：登記一次，逾時而且還有次數就**原樣**重送（同 id、同 seq、同 bytes）。
  第一次的回覆晚到與第二次的回覆同鍵：先到的交付，後到的無主，計數。
- 🚨 **預設 `attempts = 1`，不重送**。fail closed：`Upload/Create` 重送會開出兩個上傳。冪等的呼叫點自己開：
  `Chunk`（現在就有 `Corrupt` 重送一次）、`Event/Send`（server 以 `txn_id` 去重）、`Session/Login`（wire-format §6.3.2 維護者建議 3 秒沒回重送、連三次算無回應）。
- 哪些要 Ack 看 `WANT_ACK`，跟現在一樣。

## 7. 關線：表整張清空，每一項收到錯

關線只有**一條路** `shut_down(reason)`（PR #52 審查 rumia／cirno／salvia 🔴：第一版 writer 死了只是自己停、`close()` 不停 writer，
在等的人要等到各自的 300 秒）。三個入口都走它，第一個叫到的人做事、之後的都是 no-op：

| 入口 | 什麼時候 |
|---|---|
| 讀取 task 結束 | 對方 `Close`、socket 錯、連續 `CORRUPT_FRAME_BUDGET` 個 frame 解不開（跟 server 的 `wbf_ws_corrupt_budget` 同一個想法：一個壞 frame 不值得斷線，一連串就是這條線只剩壞的） |
| 送出 task 寫 sink 失敗 | 對方不收了、socket 寫不進去 |
| 呼叫端 `close()`（含 `Drop`） | 主動關 |

做的事，照順序：

1. `closed` 設 true（之後 `send` 開頭就回 `Network`，不進佇列）。
2. `SessionTable::fail_all(reason)`：每一項 `fail(Network(reason))`，在等的請求全部回錯、串流與訂閱的 channel 關掉。
3. 兩個 task 都 abort（自己那個也可以：在下一個 await 點停）；送出 task 停了 sink 就丟了，對方看到的是「對方關了」、不是任何 bytes。

重連不在這層。

## 8. 測試：傳輸與分派拆開

- `sessions.rs` 的表是同步純資料結構，單元測試直接餵 pack：分派四條規則、Create 例外、活會話擁有它的 id、無主計數、`fail_all`、串流塞滿失敗、訂閱塞滿標 gap。
- `tests/dispatch.rs`：`transport::memory_pair()` 對接，測試扮 server 從另一頭送 bytes，順序故意亂：回覆比登記早到→無主；Batch 串流中間插推播→各歸各的；
  `Superseded` 打進訂閱→終點；三個會話在等時關線→三個都收到 `Network`；鉤子看到每一個 pack（含無主）；重送政策逾時重送一次；
  送出那半死了→在等的人有限時間內收到 `Network`；`close()` 之後 `send` 立刻回錯、對面收不到任何 bytes；同 id 兩代會話舊 handle 晚 drop 不誤刪；安靜是 `Timeout`、斷線是 `Network`。
- `tests/pipeline.rs` 與 `e2e_crypto_engine.rs` 的斷言**不動**：底下換通道，上面應該照樣過。

## 9. 拿掉的東西

- `WsChannel::receive_pack` 裡「推播型且 id 不符就丟」與 `dropped_pushes` 整段。
- `protocol::is_unsolicited_push` 與它的向量測試：分類現在由表做，不由 subtype 猜。

## 10. 之後

- e2ee-walkthrough §16.5 第 8、10 列與 to-device-client §8 第 4、5 列可以勾（推播收得到、`Superseded` 收得到）。
- daemon 第 4 階段的另一半（daemon-runtime §5：推播封裝、`desync`、兩條佇列）接在鉤子後面。
- `Subscribe` 帶 `cd_seq` 補窗、`Event/Subscribe`（0x04）的 codec 是第 6 階段，這一支只保證通道收得到。

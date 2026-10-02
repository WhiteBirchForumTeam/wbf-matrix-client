# 一條線上的請求：送收分開、動作封在請求裡（維護者 2026-10-02 定）

> 這份文件管的是**一條線上的請求怎麼送、回覆來了做什麼**，五條線（/docs/design/daemon/link-pool.md §1）都照這裡。
> 底下那層（一條 WebSocket ＝ 一個讀取 task ＋ 一個送出 task ＋ 一張會話表，任何順序都不出事）在 /docs/design/daemon/ws-receive-dispatch.md，不變；
> 誰擁有線、什麼時候開關在 /docs/design/daemon/link-pool.md。下載是第一個照這份做的（/docs/design/media/media-download.md §5、§6）。

## 0. 一句話

**送跟收是兩件事。** 每條線一條發送 queue、一張在途表：請求送出去就不管了；它要「拿到回覆之後做什麼」，**封在請求裡**（動作）。
回覆到了（或失敗了），從在途表找回那個請求，**執行它的動作**。🚫 沒有人握著線等回覆。

**預設無序、無狀態**（維護者 2026-10-02）：WebSocket 保證順序，我們🚫 不拿它的保證。收到什麼就看類型處理；
真的要守順序的是例外，由動作自己守（例：上傳收到第 n 塊的 Ack 才送 n+1，§5）。

## 1. 跟之前的差別

| 層 | 之前 | 現在 |
|---|---|---|
| 傳輸與會話表（`WsLink`，/docs/design/daemon/ws-receive-dispatch.md） | 送收分開、照 (id, seq) 交付、允許很多請求在途 | **不變** |
| 線（連線池的一格） | 一條線一次一個命令：借線的人握著 `tokio::Mutex` 的 guard，送出後原地等（舊的 /docs/design/daemon/link-pool.md §5） | 一條線一條發送 queue ＋ 一張在途表；🚫 沒有獨佔的借用 |
| `WbfClient` | 方法都是 `&mut self`：seq 計數器、hello 的結果在它身上，所以一次只能一個 | seq 由**線**發（atomic）、hello 的結果放在線上共用；送出不再需要 `&mut` |
| 呼叫的人 | `let reply = client.call(…).await` | 交一個「請求 ＋ 動作」給線；回覆到了，動作在那條線的處理端執行 |

「送一個等一個沒錯，錯的是『下一個收到的就是我的回覆』」（/docs/design/daemon/ws-receive-dispatch.md 開頭）那一條，這裡往上推一層：
**錯的也包括「送出的人得原地等它的回覆」**——那會讓同一條線上的第二件事排在第一件的網路來回後面。

## 2. 形狀

```
                     一條線（連線池的一格，活得比底下的 WsLink 久）
  ┌────────────────────────────────────────────────────────────────────────────┐
  │ 發送 queue：[ 請求 { 封包怎麼造, 動作, 逾時 }, … ]                            │
  │   一般放尾巴；插隊的放最前面（例：播放器 seek，/docs/design/media/media-download.md §6） │
  │        │ 發送端：線活著就一個一個取出、配 seq、在途表登記、交給 WsLink 送出              │
  │        ▼                                                                    │
  │ 在途表：(id, seq) → 動作            （登記一定在送出之前，同 ws-receive-dispatch §2.1） │
  │        ▲                                                                    │
  │        │ 回覆、逾時、線斷：從在途表拿出動作                                       │
  │ 處理端：一個 task，照到達順序執行動作（動作可能再往發送 queue 塞新的請求）           │
  └────────────────────────────────────────────────────────────────────────────┘
```

- **發送端只做「取出 → 登記 → 送」**，🚫 不等回覆。底下 `WsLink` 的送出佇列是有界的（/docs/design/daemon/ws-receive-dispatch.md §5），滿了發送端就等——
  所以插隊只在**我們這層**的發送 queue 有效：交給 `WsLink` 之後照 FIFO。發送端因此一次只交一個、交完才取下一個，🚫 不把整條 queue 倒進 `WsLink`。
- **處理端一條線一個 task**，照回覆到達的順序執行動作。同一條線上會碰同一份狀態的動作（例：寫同一個池檔）因此天然串行，🚫 不必另外加鎖；
  動作要短（解密一塊、寫一段、塞下一個請求），🚫 不在動作裡等網路——要等就再送一個請求、把後續封進它的動作。
- **線斷了發送 queue 不丟**：queue 與在途表屬於「線」，不屬於某一條 `WsLink`。線死了（/docs/design/daemon/ws-receive-dispatch.md §7），在途的每個動作收到 `Network`；
  還沒送的留在 queue 裡，連線池重開（/docs/design/daemon/link-pool.md §3、§3.1）之後發送端接著送。

**實作**（core 的 `link_requests.rs`，`RequestLine`）：發送端每送一個請求（`WsLink::send_request`），就交給一個小 task 等回覆（`PendingReply::wait`）；
那個 task 手上拿著的就是這個請求的動作——在途表就是這些 task 加上 `WsLink` 的會話表。回覆過了 `expect_ack`，連同動作交回擁有者的收件匣（`LineReply`）。
線從連線池借**分身**（`LinkPool::find_ws_link`，🚫 不握池那格的鎖），沒開就每秒再看一次。
發送 queue 由**用這條線的那一方**擁有：`Download` 只有下載處理端一個用戶，`RequestLine` 就在它身上；`Misc` 這種很多命令共用的線搬過來時，`RequestLine` 放進池的那一格。

## 3. 動作是資料，🚫 不是閉包

```rust
enum DownloadRequest {                      // 例：Download 線（core 的 download_queue.rs，/docs/design/media/media-download.md §5.2）
    Info { mxc },                           // Download/Info：回來之後驗區塊，主檔走「下一塊」、掛著的 seek 送它們的 Read
    Chunk { mxc, index },                   // Download/Read：回來之後解開，主檔在等就落地、走「下一塊」；只有 seek 在等就存進暫存檔、交給 GET
}
```

- **請求本身就是動作**：它說得出「這是什麼、回來之後做什麼」；**誰在等它**（主檔、哪個 GET）記在處理端的在途表（§6），同一個請求只送一次。
- 每條線一個 enum，處理端 `match` 它。**資料比閉包好**：測試能直接造一個動作餵處理端、能印出來（除錯時看得到在途表裡是什麼）、能比較（同一塊不重複要，§6）。
- 動作🚫 不帶金鑰或明文：要的東西（manifest、池、暫存檔）在處理端自己的狀態裡，動作只帶「是誰的、第幾塊」。
- 動作執行時一定拿到一個結果：`Ok(回覆)`、`Err(Timeout)`（有請求在等，線卻整段沒有回應）、`Err(Network)`（線斷了）、`Err(Server)`（server 拒絕）。**怎麼處置是動作的事**（§4）。

## 4. 失敗：每個動作都要說得出去處

| 收到 | 一般的處置 |
|---|---|
| `Network`（線斷了） | 同一個請求重新排回發送 queue（放最前面，順序不亂），等線重開再送。進度停在原地 |
| `Timeout`（有請求在等，線卻整段沒有回應） | 同上，排回去重送；同一個請求連續逾時幾次（每條線自己定）就當 server 那邊出事，交給上層（下載：這個檔停下、推一則失敗） |

**逾時看的是整條線，🚫 不是單一個請求**（維護者 2026-10-02：「逾時應該是沒收到回應」）：有請求在等、而一段時間內這條線一個回應都沒收到，在途的才全部交回 `Timeout`；
線還在回應（例：很多檔交替回塊），排得再後面的請求也🚫 逾時。計時從「上一次收到回應」算，線從「沒有請求在等」變成「有」的那一刻也算起點。
多久算沒回應由擁有者給（`Download` 是 60 秒，跟 server 的 `wbf_ws_idle_timeout` 一樣，wbfuwunel #103，2026-10-02）；線真的死了不用等它——心跳 34 秒內就關線、在途的收到 `Network`（/docs/design/daemon/ws-receive-dispatch.md §5.1）。
等回應的那一方（例：GET 等一塊）🚫 另外自己限時。送了第幾次也🚫 記在線上：重不重送是擁有者的事，它自己記（下載記在在途表那一筆）。
⚠️ 已知邊界（PR #69 審查，三位都判 🟢）：「上一次收到回應」是在等回覆的小 task 排到時才記，所以一個剛好在滿 60 秒那一瞬間到的回覆可能被判逾時——
後果是那個請求多送一次、晚到的回覆變無主（每個請求仍然恰好交回一次）。
| `Server`（拒絕、NotFound、Forbidden…） | 不重送。交給上層（下載：壞檔或沒權限，job 移除） |
| 回覆的內容驗不過（AEAD、長度） | 重送一次；還是不行就交給上層（下載：壞檔，/docs/design/media/media-download.md §3.3） |

🚫 不能重送的請求（例：`Upload/Create` 重送會開出兩個上傳，/docs/design/daemon/ws-receive-dispatch.md §6）在動作裡就寫明不重送。

## 5. 要守順序的例外

預設無序。下面這幾種靠**動作串起來**守順序（收到上一個的回覆才送下一個），🚫 不靠 WebSocket 的順序：

| 情況 | 為什麼要守 | 怎麼守 |
|---|---|---|
| `Upload/Chunk` | server 只收下一個 seq（亂了回 `OutOfOrder` 1503） | 一個上傳同時一塊在途：第 n 塊的動作收到 Ack 才送 n+1。之後要滑動窗口，鍵本來就是 `(上傳 id, seq)`，分得開（/docs/design/daemon/ws-receive-dispatch.md §2.2） |
| 主檔的下載 | 主檔只順序 append（/docs/design/media/media-download.md §0） | 一個檔同時一塊在途：第 n 塊落地的動作才送 n+1 |
| `Recent` 的一串 `Batch`、`Device/Fetch` 的窗 | 一個會話裡照 seq 來 | 它們是具名會話（`Session(id)`），一個會話自己一個收件匣，裡面照 seq 處理；不走在途表的一問一答 |
| 訂閱的推播 | 有 seq 與 gap | 同上，訂閱的收件匣自己處理（/docs/design/rooms/room-sync.md） |

📌 **同一個房的兩則訊息誰先**：🚫 不在這層排。要保序的是 UI——它等上一則的回覆再送下一則（狀態放 UI，/docs/design/keys/e2ee-rpc.md §0）。

## 6. 同一件事🚫 不要兩次

在途表的鍵是 (id, seq)，但同一件事可能被兩個人要（例：播放器 seek 要的那一塊，剛好就是主檔正在拉的那一塊）。
處理端另外記「這件事正在途」（下載：請求本身當鍵，`Chunk { mxc, index }` → 等它的人），第二個要的人🚫 不再送，把自己掛到在途那一個上（多一個要交付的人）。
排著還沒送的同一個請求，被插隊的人要時一起提到最前面。

## 7. seq 與 id 由線發

- 一問一答的 seq：發送端一個計數器（只有發送端發號），**從 2³¹ 往上**（`link_requests::FIRST_SEQ`）。同一條 `WsLink` 上還有兩個發號的：
  開線時的 `WbfClient`（`hello`，還沒搬過來的 CLI 路徑也是）從 1 往上、心跳從 `u32::MAX` 往下（/docs/design/daemon/ws-receive-dispatch.md §5.1），
  三邊要撞到得先發二十億個請求；真撞到（`register` 回 `Usage`）就把請求放回 queue，下一輪換下一個號。
- 具名會話的 id：線上一個 atomic 計數器（`id::SESSION` 型別）。
- hello 的結果（feature、`recent_max_*`…）開線時拿到，放在線上，大家讀同一份。⚠️ 再 hello 會蓋掉宣告（/docs/design/daemon/link-pool.md §7），所以只有開線的人 hello。

## 8. 搬過來的順序

一條線一條線搬，搬完的線就沒有「借線」那條路：

1. **`Download`**：跟下載處理端一起做（/docs/design/media/media-download.md §5、§6），它是這個形狀的第一個用戶（2026-10-02 做完）。
   CLI 的 `download --no-cache`／`seek` 還在這條線上用舊的借線方式（等 CLI 改走 RPC 一併收掉）；seq 錯開，見 §7。
2. `Misc`：一問一答最多的線；呼叫點從 `client.call(…).await` 改成交請求，命令本身要等結果的（RPC 要回應）就把「回覆 RPC」封進動作。
3. `Upload`：照 §5 的例外，一個上傳一塊在途。
4. `Rooms`／`Keys`：訂閱本來就是收件匣（具名會話），改的是它們偶爾發的一問一答（`ItemsDestroy`、`Unsubscribe`）。

還沒搬的線維持舊的借線方式（/docs/design/daemon/link-pool.md §5）。

## 9. 跟 server 的關係（不靠，但要知道）

wbfuwunel 對**一條連線**上的封包是照到達順序一次處理一個，回應也排在同一條 TCP 上送回（new-tuwunel 的 `src/api/client/wbf/ws.rs` 開頭）。
所以同一條線上同時在途的兩個請求，server 那邊還是排隊的：插隊的請求省下的是「等前一個回覆回來才送」的那一個來回，🚫 不是前一個回覆的傳輸時間。
這不改變這份文件的形狀——我們🚫 不靠 server 的順序，也🚫 不靠它的並行；要真的不排在別人後面，靠的是分線（/docs/design/daemon/link-pool.md §1）。

## 10. 明確不做的

- 🚫 握著線等回覆：要等就把後續封進動作。
- 🚫 靠 WebSocket 的順序推論「這是誰的回覆」：鍵是 (id, seq)（/docs/design/daemon/ws-receive-dispatch.md §2）。
- 🚫 閉包當動作：測不了、印不出、比不了（§3）。
- 🚫 在動作裡做網路或等別的動作：再送一個請求（§2）。

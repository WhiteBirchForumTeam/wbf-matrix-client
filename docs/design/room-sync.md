# 跟上游：訂閱房間事件、補窗、推播寫進快取

> 維護者 2026-09-22 定的順序，原話在 §0。實作：sdk `protocol.rs`／`client.rs` 的 `Event/Subscribe`／`Unsubscribe`／`Push` codec 與
> `room_subscription`；core `room_sync.rs`。server 的語意在 wbfuwunel `wbf-event-push.md`（推送）與 `room-seq-and-recent.md`（補窗）。
> 🚫 這支**不接 RPC**：daemon 的訂閱（rpc-spec §3.9，每條 RPC 連線想收什麼）與 daemon 跟上游是兩件事，中繼那層之後再談。

## 0. 規矩（維護者原話，2026-09-22）

> 先做明文房間，再做 E2EE。明文房間的邏輯：先查詢當下水位線，然後叫訂閱，接著再叫 Recent 同步剛剛的水位線，這樣確保基本沒有洞。可以都使用訂閱的那隻連線。第二個就是訂閱金鑰。
>
> rpc 先不做還沒到那邊，rpc 的訂閱和 daemon 訂閱上游是兩件事，有點像是中繼概念，但是先純一點，別搞那麼複雜。
>
> 訂閱事件是純的，有新訊息就來一批。多批進來，跳號無所謂。主動補窗不是——有窗口限制，這裡其實是有一個 job 的：
> 一次拿 320，每個 pack 10 筆，跑 32 次；不夠就繼續要，直到補滿最新的 1000 則。起始同步的總量是一個彈性變數，先設一千試試。

## 1. 順序

```
start_room_sync(account, max_events = 1000)
  1. cg_seq ← cache.db 的 sync_state（沒有 ＝ None）
  2. 拿 Subscriptions 線 → Event/Subscribe（帳號層、不帶 rooms、不帶 cg_seq）→ 等 Ack{latest_g_seq, joined, skipped}
  3. 同一條線、同一個 guard：Recent{cg_seq} 一窗一窗翻（sdk recent_sync）到 more=false 或拿滿 max_events
     → 每批寫一次 DB → 水位 = 第一窗第一個 Batch 的 fs
  4. 放掉 guard；背景 task 讀訂閱的 handle
       Push{fs, ls, gap, events} → 原樣 upsert（照房分組）→ 水位只往前推到 fs → commit 之後發 room.message
       gap（server 說的、或本地收件匣滿過）→ 再跑一次第 3 步，從**現在的水位**起
  5. 線死了（Network）／server 送 Error → task 結束、發 sync.state: disconnected；🚫 不背景重連
stop_room_sync(account) → task 送 Event/Unsubscribe（訂閱的 id）、等 Ack、結束；線留在池裡
```

- **為什麼 Subscribe 不帶 `cg_seq`**：server 的補窗有上限（則數、位元組），截斷時只給一個 gap bit，client 還是得再 `Recent`。自己補窗一條路走完比較乾淨；
  server 自己在 `wbf-event-push.md` §9 也建議「先註冊再收窗，重複的事件 client 用 event_id 去重」——`upsert_events` 本來就冪等。
- **為什麼先訂再補**：Subscribe 的 Ack 之後 server 登記完成，之後的每則新事件都會推來；補窗補的是 Ack 之前的。兩段之間重疊的事件寫兩次也沒事。
- **補窗是一個 job**（server 約定：一窗預設 320、上限 500；每 Batch 預設 10、上限 100；一窗 8 MiB）：`recent_sync` 一窗收完看 `more`，
  有就帶 `before = 最後的 ls` 再要一窗，直到追平或到總量。到 1000 還沒追平 → 停，水位仍推到最新（比它新的全拿到了）；更舊的那段留給翻歷史（`sync=both`）。
- **推播是純的**：一包來寫一包，`seq` 跳號不管（server 丟掉的包也佔號）；水位只認 `fs`，只往前推（`advance_cg_seq`）。

## 2. 🚨 帶 `gap` 的那包不推水位

洞在**舊水位跟這包之間**。先把水位推到這包的 `fs` 再 `Recent(cg_seq)`，server 只給比 `cg_seq` 新的——洞就永遠補不回來。
所以：這包的事件照樣寫（冪等），水位不動，補窗 job 拿舊水位起、推到第一窗的 `fs`（這包也在那窗裡）。
測試 `a_room_sync_subscribes_fills_the_window_then_follows_pushes_and_gaps` 釘著：gap 之後假 server 收到的 `Recent` 帶的是舊水位。

本地收件匣滿過（`take_gap`）同理：在處理下一包**之前**先補，那時水位還在丟包之前。

## 3. 事件與狀態

| 什麼時候 | 發什麼 |
|---|---|
| 第 1 步 | `sync.state: catching_up`（帶舊水位） |
| 第 3 步做完 | `sync.state: caught_up`（帶新水位；到總量停下也算） |
| 每包 commit 之後 | 每則一個 `room.message`（自己送的也發，收的人自己濾；密文原樣、`decrypted: false`） |
| gap 補窗前後 | 同 1、3 |
| task 結束（線死、退訂） | `sync.state: disconnected` |

寫入與水位走 cache 的單一寫入者（daemon-runtime §2）：事件沒落地水位不會前進；`room.message` 在 commit 之後才發（PR #32 的規矩）。

## 4. 生命週期

- 一個帳號一個 task（`Core::room_syncs`），第二個 `start` 回 `AccountBusy`。
- 登出：`stop_room_sync_of` 先收 task，再 `close_links`（順序反了也收得掉——task 看到 Network 就結束——但這樣乾淨）。
- `Core` 丟掉：handle 的 `Drop` abort task。
- task 🚫 不握 `Core`（它是 `'static`）：握 `Arc<LinkPool>`、`Arc<ServerCache>`、`EventSink`。補 gap 要線就跟池要；池發現線死了會叫 opener，
  而 task 給的 opener 一律回錯——線死了訂閱也死了，重開是下一次 `start_room_sync` 的事（link-pool.md §3 的規矩，第 8 階段的監督者做重連）。
- 訂閱線跟金鑰訂閱（下一支）共用：`RoomSubscription` 是 link 的一個會話，guard 放掉之後線還能用；金鑰的 `Device/Subscribe` 是另一個會話。

## 5. 不在這支

- RPC（`sync.subscribe` 之類）：維護者說先純一點。
- 金鑰訂閱（`Device/Subscribe`、`pull_to_device`）：第二支。
- 背景重連、退避：第 8 階段。
- 密文解密：推來的密文原樣存，跟 `Recent` 那條路一樣（local-cache-db.md §7.2）。
- `DeviceChanged`（同一個訂閱會話送來）：認得、不消費，E2EE 那支接。

## 6. 測試

- sdk `tests/unit.rs`：`Subscribe`（帳號層／點名）、`Unsubscribe` 逐 byte 對 server 向量；`Ack`、`Push`（含 gap）、`DeviceChanged` 解得回向量的值；
  `bc` 對不上事件數是 Protocol；缺 `gap` 欄位當 true。
- core `room_sync.rs`（記憶體對接的假 server：答 Hello、Subscribe、Recent 照 `cg_seq` 給窗、Unsubscribe；測試主動推 Push）：
  訂閱→補窗 5 則→水位→第二個 start 被擋→推一包寫進去、水位前進、`room.message` 在 commit 之後→帶 gap 的包觸發 `Recent(舊水位)` 補回洞→退訂、`disconnected`；
  線死了 task 結束、`disconnected`、`stop` 是 no-op。
- 真 server（`--ignored`，`WBF_E2E_*`）：alice 登入、`start_room_sync`，bob 用 HTTP 送一則，alice 的 `room.message` 在時限內到、cache 有它、水位前進；`stop`。

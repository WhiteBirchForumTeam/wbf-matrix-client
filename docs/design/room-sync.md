# 訂閱線的內容：訂閱房間事件、推播寫進快取、水位不跨洞

> 維護者 2026-09-22 定，原話在 §0。實作：sdk `protocol.rs`／`client.rs` 的 `Event/Subscribe`／`Unsubscribe`／`Push` codec 與
> `room_subscription`；core `room_sync.rs`（`init_connection`、收推播的 task）。server 的語意在 wbfuwunel `wbf-event-push.md`（推送）與
> `room-seq-and-recent.md`（`Recent`）。
> 🚫 這支**不接 RPC**：daemon 的訂閱（rpc-spec §3.9，每條 RPC 連線想收什麼）與 daemon 跟上游是兩件事（中繼），先純一點。

## 0. 規矩（維護者原話，2026-09-22）

> 先做明文房間，再做 E2EE。第二個就是訂閱金鑰。
>
> 現在應該會開多個連線，初始化、啟動連線池時，應該要寫一個通用 init_connection，把 connection 帶入那個 function，function 裡面根據連線的狀態，
> 例如是 room 專用的，就啟動訂閱，至於補窗啥的，其實可以直接無視。
>
> 補窗交由 UI，daemon 只管訂閱當下，什麼時候補由 UI trigger。UI 看不到這邊怎麼整合，只知道萬一收到回應還有 more 就繼續下發新的 rpc 命令。
>
> 有沒有洞 UI 完全不知道。UI 只根據 Recent 判斷是否滾到了；而且 UI 可以總是呼叫 Recent，無副作用。

## 1. 形狀

```
池開一條線（link_pool::open_link）
  connect(Bearer) → hello(features_of(role)) → init_connection(account, role, client)
                                                 ├─ Subscriptions：Event/Subscribe（帳號層、不帶 rooms、不帶 cg_seq）→ 等 Ack → 起收推播的 task
                                                 └─ 其他角色：不做事
收推播的 task（一帳號一個；線在池裡、task 只握訂閱的會話）
  Push{fs, ls, gap, events} → 原樣 upsert（照房分組）→ 水位往前推到 fs（§2 的例外）→ commit 之後發 room.message
  線死了 / server 送 Error → 發 link.state: closed（帶原因）、task 結束；🚫 不背景重連（下次要用重開就重訂）
UI
  想補就叫 sync.recent（走 Misc 線；冪等、隨時可叫）：回應的 caught_up 是 false 就再叫，直到追平或它自己決定夠了
Core::open_subscriptions(target)  ＝ 拿一次訂閱線（開的時候就訂了）；Core::close_subscriptions(target) ＝ 收 task、關那條線
```

- **daemon 只管訂閱當下**：沒有補窗 job、沒有起始總量、不發任何「有洞」的訊號。跟官方 Matrix 的分工同形：`/sync` 迴圈只追加 server 給的，
  `limited: true` 之後往回補是 client 決定的事（`/messages`）；這裡 `Push` 是 `/sync`、`gap` 是 `limited`、`sync.recent` 是 `/messages`。
- **`Subscribe` 不帶 `cg_seq`**：server 的補窗有上限、截斷只給一個 gap bit；反正補窗是 UI 的事，daemon 不要它。
- **訂閱是純的**：一包來寫一包，`seq` 跳號不管（server 丟掉的包也佔號）；水位只認 `fs`、只往前推（`advance_cg_seq`）。
- 線死了池要到下次取用才發現；task 先發一次 `link.state: closed`（帶「the room subscription ended: …」），UI 不必等。下次取用時池會再發一次 Closed→Opened。

## 2. 🚨 daemon 唯一要守的：水位不能跨過洞

UI 補窗是從水位起 `Recent`（server 只給比 `cg_seq` 新的）。水位一旦推過洞，洞就永遠補不回來。所以：

- 帶 `gap` 的包：事件照寫（冪等），**水位不動**，記下凍結點 ＝ 這包的 `fs`（洞在舊水位跟它之間）。
- 之後**正常的包也不推**（推了一樣跨過洞），直到看到水位 ≥ 凍結點——那只可能是 UI 叫的 `sync.recent` 推的（它從舊水位起、推到那一刻最新的 `fs`），
  洞補過了才解凍、恢復推。
- 本地漏一包——收件匣滿過（`take_gap`）、一包解不開（`Protocol`）、cache 寫失敗（PR #58 審查 cirno 🟡1）——同理：凍結點 ＝ 現在的水位**＋1**
  （設成水位本身下一包一比就等於它、立刻解凍，抓過一次；漏掉的那包 `fs` ≥ 水位＋1，所以只有 Recent 真的推過去才算補過）。
- 判斷跟寫入在同一個 cache 工作裡，跟 `Recent` 的寫入排同一條 queue（daemon-runtime §2）：🚫 不靠時序。

測試 `the_watermark_never_crosses_a_hole_and_refilling_is_the_uis_job` 釘著：gap 包寫了、水位不動；之後正常的包也不動；UI 的 `sync.recent`
從洞之前的水位起、補回洞、推水位；再來一包才恢復推。變異：把「有洞不推」拿掉 → 紅。

## 3. 事件

| 什麼時候 | 發什麼 |
|---|---|
| 每包 commit 之後 | 每則一個 `room.message`（自己送的也發，收的人自己濾；密文原樣、`decrypted: false`） |
| 線開／關 | 池的 `link.state`（`Subscriptions`）；task 因線死結束時多發一次 `closed` 帶原因 |

🚫 不發 `sync.state`（catching_up／caught_up 是 UI 自己叫 `sync.recent` 的結果，daemon 不知道；`sync.state` 的 variant 留著，還沒人發）。

## 4. 生命週期

- 一個帳號一個 task（`Core::room_syncs`）；線重開時 `init_connection` 換掉舊的（舊的線已經死了）。
- 登出：`stop_room_sync_of` 先收 task，再 `close_links`。`Core` 丟掉：handle 的 `Drop` abort task。
- task 🚫 不握 `Core`、不握線：握 `Arc<ServerCache>`、`EventSink`、訂閱的會話。guard 放掉之後線照樣能用（`Misc` 之類的命令不受影響；
  金鑰的 `Device/Subscribe` 是同一條線上另一個會話，下一支）。
- `Event/Unsubscribe` 的 codec 與 `room_unsubscribe` 在 sdk 裡（對著向量），core 現在不用它：關訂閱線就是關線，server 斷線自動退訂。

## 5. 不在這支

- RPC（開／關訂閱線的命令）：維護者說先純一點；`open_subscriptions`／`close_subscriptions` 先只給 core 與測試用。
- 金鑰訂閱（`Device/Subscribe`、`pull_to_device`）：第二支。
- 背景重連、退避：第 8 階段。
- 密文解密：推來的密文原樣存（local-cache-db.md §7.2）。
- `DeviceChanged`（同一個訂閱會話送來）：認得、不消費，E2EE 那支接。

## 6. 測試

- sdk `tests/unit.rs`：`Subscribe`（帳號層／點名）、`Unsubscribe` 逐 byte 對 server 向量；`Ack`、`Push`（含 gap）、`DeviceChanged` 解得回向量的值；
  `bc` 對不上事件數是 Protocol；缺 `gap` 欄位當 true。
- core `room_sync.rs`（記憶體對接的假 server：答 Hello、Subscribe、Recent 照 `cg_seq` 給窗；測試主動推 Push；兩條線可以共用同一份事件）：
  §2 那條；本地漏一包（`bc` 對不上的壞包）凍結、UI 的 Recent 補過才解凍（`a_pack_that_could_not_be_read_freezes_the_watermark_like_a_gap`；
  cache 寫失敗那條走同一支 `freeze_before_next_push`，但寫失敗本身製造不出來，沒有直接測）；線死了 task 結束、`link.state: closed` 帶原因。
- 真 server（`--ignored`，`WBF_E2E_*`）：alice 登入、`open_subscriptions`；bob（另一個 Core）`Event/Send` 送一則；alice 的 `room.message` 在時限內到、
  cache 有它、水位前進；`close_subscriptions`；兩邊登出。
- ⚠️ 沒測的：本地收件匣灌爆那條凍結；真 server 的 gap（要讓 server 的推送佇列滿）。

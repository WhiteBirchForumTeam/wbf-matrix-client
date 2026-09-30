# 已讀有三層（維護者 2026-09-13 給的草案方向）

> 這份講「已讀」怎麼分層、UI 怎麼標已讀、送上游的 receipt 是 private 還是 public。從 /docs/design/daemon/daemon-runtime.md 拆出來。
> 還沒做：`room.read`、`daemon.reload_conf`、送 receipt。`cache.rs` 已有 `read_positions` 表與 `set_read_position`／`get_read_position`，還沒有 RPC 叫它們。

「已讀」在這個系統裡指過三件**不同**的事，混在一起講是下一個 bug 的溫床：

| 層 | 存在哪 | 誰改它 | 意思 |
|---|---|---|---|
| **1. 快取水位** | `sync_state.cg_seq`（每帳號） | daemon 回答 UI 叫的 `sync.recent` 時（/docs/design/rooms/room-sync.md §1） | 「這個帳號的事件我抓到哪裡了」。🚫 **跟人有沒有看過完全無關** |
| **2. 本地已讀** | `read_positions`（每帳號每房間） | **只有 UI 明講才會改** | 「這台機器上的這個人看到哪裡了」 |
| **3. 遠端已讀** | homeserver 的 read receipt | UI 明講、而且要求送上游時 | 「其他裝置／其他人看得到的已讀」，又分 **private／public** |

## 1 預設是「沒有讀」

🚨 **daemon 把事件寫進 `cache.db` ≠ 已讀。** 第 1 層前進的時候，第 2、3 層**一動也不動**：
`read_positions` 沒有那一列就是沒讀過，遠端也是未讀。

📎 為什麼要特別寫這一條：`read_positions` 當初是配著 CLI 設計的（那時候「印出來」約等於「看過了」）。
UI 分離之後那個等式不成立了 —— **只有 UI 說看過了才算看過**。

## 2 UI 怎麼標已讀：`room.read`

```jsonc
{ "method": "room.read",
  "params": { "room": "!r:localhost", "user": "@a:localhost",
              "g_seq": 123, "sync": "both", "visible_to_others": false }, "id": 7 }   // visible_to_others 選填，§3
```

| `sync` | 效果 |
|---|---|
| `local`（預設） | 只寫 `read_positions`。**本地已讀、遠端仍未讀** |
| `server` | 只送上游的 read receipt，🚫 不寫本地（對帳用，跟 /docs/design/daemon/daemon-runtime.md §3.1 同一套語意） |
| `both` | 送上游 ＋ 寫本地 |

- **位置怎麼指**：`event_id` 是權威，`g_seq`／`r_seq` 是算術用的捷徑（/docs/design/storage/local-cache-db.md §5）。
  三個至少要給一個；給 seq 的時候 daemon 自己去查那一則的 `event_id` 再送上游
  —— ⚠️ Matrix 的 read receipt 吃的是 `event_id`，🚫 沒有序號這種東西。
- **未讀數還是算出來的**（`read_positions` 對 `events`），🚫 不是一個推播欄位。
  UI 收到 `room.message` 之後**重新讀一次未讀**（本地讀，毫秒），🚫 不要自己 +1
  —— 自己加會在多裝置、多前端的情況下漂掉。

## 3 private 還是 public：呼叫可以指定，沒指定就照 conf 的預設（維護者 2026-09-30 定）

Matrix 有兩種 receipt：`m.read`（**public**，同房間的人看得到）與 `m.read.private`（只有自己的其他裝置看得到）。

- `room.read` 多一個**選填**的 `visible_to_others: bool`：有帶就照參數送；沒帶就照 `wbf.conf` 的 `READ_RECEIPTS`（`private`／`public`）；
  conf 也沒寫、或寫了認不得的值，一律 **private**（fail closed）。只影響 `sync: "server"`／`"both"`（`local` 不送上游）。
- 改預設：**UI 把設定寫進 `wbf.conf`**，然後叫 **`daemon.reload_conf`** 讓 daemon graceful reload，之後沒帶參數的已讀照新預設。
- ⭐ 為什麼沒帶參數時落到 conf、最後落到 private：**「要不要被看見」是使用者的長期偏好**。第一個忘了帶參數的地方
  🚫 不該把使用者曝光出去——那種錯誤是**不可回收的**（別人已經看到了）。參數是給 UI 做「這一則例外」用的。
- ⚠️ 因此 `daemon.reload_conf` 這個 method 是這條的一部分，🚫 不是附帶：沒有它，改預設就要重開 daemon（斷掉所有上游會話）。

⚠️ 這一節是**方向草案**（維護者原話：「執行上有沒有問題我不確定」）。實作時要回頭確認兩件事：
matrix-sdk 送 private receipt 的介面長什麼樣、以及 wbfuwunel 那邊對兩種 receipt 的支援。

## 4 新房間

- 上游會話寫進 `rooms`／`room_list` 之後發一則 `room.message`（那則邀請或第一則訊息）。
- ⚠️ 「房間列表變了」值不值得一個獨立的推播（`room.list_changed`），這一版**先不加** ——
  先看 `room.message` 夠不夠用，🚫 不預先發明。


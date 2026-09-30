# 已讀有三層（維護者 2026-09-13 給的草案方向）

「已讀」在這個系統裡指過三件**不同**的事，混在一起講是下一個 bug 的溫床：

| 層 | 存在哪 | 誰改它 | 意思 |
|---|---|---|---|
| **1. 快取水位** | `sync_state.cg_seq`（每帳號） | daemon 回答 UI 叫的 `sync.recent` 時（daemon-runtime.md §4.3） | 「這個帳號的事件我抓到哪裡了」。🚫 **跟人有沒有看過完全無關** |
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
              "g_seq": 123, "sync": "local" }, "id": 7 }
```

| `sync` | 效果 |
|---|---|
| `local`（預設） | 只寫 `read_positions`。**本地已讀、遠端仍未讀** |
| `server` | 只送上游的 read receipt，🚫 不寫本地（對帳用，跟 daemon-runtime.md §3.1 同一套語意） |
| `both` | 送上游 ＋ 寫本地 |

- **位置怎麼指**：`event_id` 是權威，`g_seq`／`r_seq` 是算術用的捷徑（local-cache-db §5）。
  三個至少要給一個；給 seq 的時候 daemon 自己去查那一則的 `event_id` 再送上游
  —— ⚠️ Matrix 的 read receipt 吃的是 `event_id`，🚫 沒有序號這種東西。
- **未讀數還是算出來的**（`read_positions` 對 `events`），🚫 不是一個推播欄位。
  UI 收到 `room.message` 之後**重新讀一次未讀**（本地讀，毫秒），🚫 不要自己 +1
  —— 自己加會在多裝置、多前端的情況下漂掉。

## 3 private 還是 public：conf 決定，🚫 不是每次呼叫決定

Matrix 有兩種 receipt：`m.read`（**public**，同房間的人看得到）與 `m.read.private`（只有自己的其他裝置看得到）。

- **預設 private。** 送 `sync: "server"`／`"both"` 的已讀，daemon 一律送 private。
- 要公開：**UI 把設定寫進 `wbf.conf`**（例如 `READ_RECEIPTS=public`），然後叫一個
  **`daemon.reload_conf`** 讓 daemon graceful reload；之後的已讀才會是 public。
- ⭐ 為什麼是 conf 而不是每次呼叫帶一個 `public: true`：**這是使用者對「我要不要被看見」的長期偏好**，
  🚫 不是某一次操作的選項。放在呼叫上，第一個忘了帶的地方就會把使用者曝光出去
  —— 而那種錯誤是**不可回收的**（別人已經看到了）。
- ⚠️ 因此 `daemon.reload_conf` 這個 method 是這條的一部分，🚫 不是附帶：
  沒有它，改設定就要重開 daemon（斷掉所有上游會話）。

⚠️ 這一節是**方向草案**（維護者原話：「執行上有沒有問題我不確定」）。實作時要回頭確認兩件事：
matrix-sdk 送 private receipt 的介面長什麼樣、以及 wbfuwunel 那邊對兩種 receipt 的支援。

## 4 新房間

- 上游會話寫進 `rooms`／`room_list` 之後發一則 `room.message`（那則邀請或第一則訊息）。
- ⚠️ 「房間列表變了」值不值得一個獨立的推播（`room.list_changed`），這一版**先不加** ——
  先看 `room.message` 夠不夠用，🚫 不預先發明。


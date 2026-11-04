# 已讀有三層（維護者 2026-09-13 給方向、2026-11-05 定形狀）

> 這份講「已讀」怎麼分層、UI 怎麼標已讀與拿未讀數、送上游的回條是 private 還是 public、conf 怎麼重讀。從 /docs/design/daemon/daemon-runtime.md 拆出來。
> 現況：形狀 2026-11-05 定，**還沒實作**（`room.read`、`room.get` 的 `unread`、`READ_RECEIPTS`、`daemon.reload_conf`）。`cache.rs` 已有 `read_positions` 表與 `set_read_position`／`get_read_position`，還沒有 RPC 叫它們。

「已讀」在這個系統裡指過三件**不同**的事，混在一起講是下一個 bug 的溫床：

| 層 | 存在哪 | 誰改它 | 意思 |
|---|---|---|---|
| **1. 快取水位** | `sync_state.cg_seq`（每帳號） | daemon 回答 UI 叫的 `sync.recent` 時（/docs/design/rooms/room-sync.md §1） | 「這個帳號的事件我抓到哪裡了」。**跟人有沒有看過完全無關** |
| **2. 本地已讀** | `read_positions`（每帳號每房間） | **只有 UI 明講才會改**（§3） | 「這台機器上的這個人看到哪裡了」 |
| **3. 遠端已讀** | homeserver 的 read receipt | UI 明講、而且要求送上游時（§3） | 「其他裝置／其他人看得到的已讀」，又分 **private／public**（§5） |

## 1 預設是「沒有讀」

🚨 **daemon 把事件寫進 `cache.db` ≠ 已讀。** 第 1 層前進的時候，第 2、3 層**一動也不動**：
`read_positions` 沒有那一列就是沒讀過，遠端也是未讀。

📎 為什麼要特別寫這一條：`read_positions` 當初是配著 CLI 設計的（那時候「印出來」約等於「看過了」）。
UI 分離之後那個等式不成立了 —— **只有 UI 說看過了才算看過**。

## 2 已讀是一條水位，不是每則一個旗

Matrix 的 read receipt 指著**一則** `event_id`，意思是「讀到這則為止，它和它之前的全部都讀過了」。**沒有**逐則的已讀。
別人那邊的「誰讀了」也一樣是每個人一條水位（這版收不到，§8）。

所以本地是**每個（帳號，房間）一列水位**（`read_positions`），未讀數是「水位之後還有幾則該算的」：

```text
快取裡這間房有 100 則訊息，水位在第 95 則
  → 未讀＝第 96–100 則裡「該算的」那幾則（§2.1）；其中有 2 則是自己送的 → count = 3
```

### 2.1 什麼算一則未讀

水位之後（照房間內的順序 `r_seq`）、而且同時是：

- **訊息**：`class` 是 `msg`、**不**是狀態事件（有 `state_key` 的不算）。edit、表情、收回本身都不算（它們是改別則的關係事件）。
- **別人送的**：這個帳號自己送的不算（別台裝置送的也是自己）。
- **還在**：已經被收回的不算。

`mentions`＝上面那些裡，`m.mentions.user_ids` 有自己、或 `m.mentions.room` 是 `true` 的則數。

- 沒有水位那一列（從沒標過）→ 快取裡這間房所有該算的都是未讀（§1）。
- ⚠️ **只數快取裡有的**：快取有洞時（`/docs/handover.md` §7 那個「`[last_ls, 舊水位)` 的永久洞」）未讀數會**偏少**。
  **不**拿 `r_seq` 相減：`r_seq` 連狀態事件、表情、自己送的都編號，相減會**偏多**，而且多出來的那幾則 UI 點進去找不到。
- 未讀數是**算出來的**，**不**存、**不**是推播欄位。UI 收到 `room.message` 之後**重新讀一次**（本地讀，毫秒），
  禁止自己 +1 —— 自己加會在多裝置、多前端的情況下漂掉。

### 2.2 本地水位只往前

`room.read` 給的位置比本地水位**舊**（`r_seq` 比較小）→ 本地**不動**，回的 `read_upto` 還是原本那則。

- 為什麼：多個前端、多個視窗同時在標，晚到的舊位置不該把未讀數變多。
- 「標成未讀」在 Matrix 是另一個東西（`m.marked_unread`），這版不做（§8）。
- **上游照 UI 給的送**，daemon **不**先擋（daemon 是中轉站，收不收由 server 決定）。

## 3 UI 怎麼標已讀：`room.read`

```jsonc
{ "method": "room.read",
  "params": { "room": "!r:localhost", "user": "@alice:localhost",
              "event_id": "$Zm9vYmFy", "sync": "both", "visible_to_others": false }, "id": 7 }
```

| 參數 | 意思 |
|---|---|
| `room` | 房間 id，必填 |
| `event_id`／`g_seq`／`r_seq` | 讀到哪一則：**恰好給一個**，零個或兩個以上回 `102`。`event_id` 是權威，seq 是算術用的捷徑（/docs/design/storage/local-cache-db.md §5）；給 seq 的時候 daemon 去本地查那一則的 `event_id` 再送上游 —— ⚠️ Matrix 的 read receipt 吃的是 `event_id`，**沒有**序號這種東西 |
| `sync` | 見下表，預設 `local` |
| `visible_to_others` | 選填，§5 |
| `user`／`server` | 共同欄位（/docs/design/rpc-specs/rpc-spec.md §2） |

| `sync` | 效果 |
|---|---|
| `local`（預設） | 只寫 `read_positions`。**本地已讀、遠端仍未讀** |
| `server` | 只送上游的 read receipt，**不**寫本地（對帳用，跟 /docs/design/daemon/daemon-runtime.md §3.1 同一套語意） |
| `both` | **先送上游、成功了再寫本地**。上游失敗 → 回錯誤、本地**不**寫，UI 用同一個呼叫重試就好 |

回應（`local`／`both` 是寫完之後本地的樣子；`server` 是本地沒動的樣子）：

```jsonc
{ "read_upto": { "event_id": "$Zm9vYmFy", "r_seq": 95 },   // 從沒標過是 null
  "unread": { "count": 3, "mentions": 1 },                    // §2.1
  "sent": "m.read.private" }                                  // 送了哪一種；local 是 null
```

拒絕（都是**什麼都不寫、什麼都不送**）：

| 情況 | 碼 |
|---|---|
| 位置零個或兩個以上、`sync` 認不得 | `102` |
| 給的是 seq、或 `sync` 是 `local`／`both`，而本地查不到那一則（要比順序、要換 `event_id`） | `1100` |
| 上游拒了（`server`／`both`） | `1400` |

上游怎麼送：`crates/wbf-sdk/src/matrix_endpoint.rs` 的端點表加一列 `RECEIPT`（跟 `REDACT` 同一套）——
wbf 帳號走橋 `0x15 0x22` Receipt（wbfuwunel 的 /docs/bridge-specs/0x15-receipt.md：meta `{"room_id","receipt_type","event_id"}`、data `{}`），
一般 Matrix 帳號走 HTTP `POST /_matrix/client/v3/rooms/{room_id}/receipt/{receipt_type}/{event_id}`。
回條是 EDU、本來就不加密，加密房也一樣送。

## 4 UI 怎麼拿未讀數：`room.get` 多帶 `unread`

`room.list` 只回房間 id（維護者 2026-10-05），所以未讀數跟著**每間房的** `room.get` 回：

```jsonc
{ "room": "!r:localhost", "name": "…", /* 其餘 Conversation 欄位不變 */
  "unread": { "count": 3, "mentions": 1, "read_upto": { "event_id": "$Zm9vYmFy", "r_seq": 95 } } }
```

- **不論 `sync` 是哪一種都從本地算**（§2.1）：`sync` 管的是房間狀態要不要問 server，未讀數只有本地有。
- 跟 /docs/design/rooms/chat-model.md §2.5 的 `Unread` 同一個東西；那邊的 `local_offset` 改名 `read_upto`，兩邊用同一個名字。

## 5 private 還是 public：呼叫可以指定，沒指定就照 conf 的預設（維護者 2026-09-30 定）

Matrix 有兩種 receipt：`m.read`（**public**，同房間的人看得到）與 `m.read.private`（只有自己的其他裝置看得到）。
wbfuwunel 的橋兩種都收（/docs/bridge-specs/0x15-receipt.md `0x22`），上游 SDK 也送得出兩種，2026-11-05 查過。

- `room.read` 的 `visible_to_others: bool`：有帶就照參數送；沒帶就照 `wbf.conf` 的 `READ_RECEIPTS`（`private`／`public`）；
  conf 也沒寫、或寫了認不得的值，一律 **private**（fail closed，認不得的值進 `warnings`）。只影響 `sync: "server"`／`"both"`（`local` 不送上游）。
- 改預設：**UI 把設定寫進 `wbf.conf`**，然後叫 **`daemon.reload_conf`**（§6），之後沒帶參數的已讀照新預設。
- ⭐ 為什麼沒帶參數時落到 conf、最後落到 private：**「要不要被看見」是使用者的長期偏好**。第一個忘了帶參數的地方
  不該把使用者曝光出去 —— 那種錯誤是**不可回收的**（別人已經看到了）。參數是給 UI 做「這一則例外」用的。

## 6 `daemon.reload_conf`：立刻重讀，會影響連線的鍵例外（維護者 2026-11-05）

```jsonc
{ "method": "daemon.reload_conf", "params": {}, "id": 8 }
→ { "ok": true, "changed": ["READ_RECEIPTS"], "needs_restart": ["TRANSPORT"], "warnings": [] }
```

- 讀的是**啟動時那一份**（同一個 `--config`，沒給就是資料目錄裡的 `wbf.conf`），規則跟啟動時一樣：檔案不在＝內建預設、`--config` 明指的不在或語法壞了＝錯誤。
- **整份換、不套一半**：讀失敗 → 回 `1100`（帶原因）、**全部照舊**。讀成功 → 能重讀的鍵一次換掉，`changed` 列出值真的變了、而且已經生效的鍵。
- **上游會話不斷、本地連線不掉**（graceful）。所以**會影響連線的鍵不重讀**：值變了也照舊用啟動時的，列進 `needs_restart`，daemon 完整重開才載入。
  UI 拿 `needs_restart` 決定要不要提示使用者重開，**不**必去解析 `warnings` 的字。

| 鍵 | 重讀之後 |
|---|---|
| `SERVER_BACKUP`、`LOCAL_ROOM_KEYS` | 下一個用到它的 RPC 起 |
| `READ_RECEIPTS` | 下一個沒帶 `visible_to_others` 的 `room.read` 起 |
| `TRANSPORT` | **不重讀**（會影響連線：決定開不開 WS 線，/docs/design/overview/architecture-v2.md §5.1）。值變了 → 進 `needs_restart`，完整重開才生效 |

- 之後新加的鍵要在這張表上選一邊；**會影響連線的一律放「不重讀」那邊**。
- 認不得的鍵照舊進 `warnings`（/docs/design/rpc-specs/wbf-cli-spec.md §10.4：警告、忽略、照跑）。

## 7 新房間

- 上游會話寫進 `rooms`／`room_list` 之後發一則 `room.message`（那則邀請或第一則訊息）。
- ⚠️ 「房間列表變了」值不值得一個獨立的推播（`room.list_changed`），這一版**先不加** ——
  先看 `room.message` 夠不夠用，禁止預先發明。

## 8 這版不做（維護者 2026-11-05）

- **誰讀了我的訊息**（別人的 receipt）、**我在別台裝置讀到哪**：server 不推 receipt（wbfuwunel 的 /docs/design/events/event-push.md §6）。等 server 推了再做。
- `m.fully_read`（橋 `0x21` ReadMarkers）、`m.marked_unread`（標成未讀）、討論串的回條（`thread_id`）。


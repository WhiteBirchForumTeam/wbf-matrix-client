# 提案：to-device 的訂閱、推送與補齊（`0x16 Device`）

> 給 wbfuwunel 的提案，2026-09-10。client 端這邊的來源是
> [`architecture-v2.md`](architecture-v2.md) §6（每一種事件流都要能從游標補齊）。
> server 端對照的是 `wbf-wire-format.md` §3.3（`0x16 Device` 這一格是空的）、
> `wbf-event-push.md`（PR #36 已實作的訂閱與推送）、`room-seq-and-recent.md`（`Recent` 的拉窗）。
>
> ⚠️ 這份是**提案**，不是定案。§7 列的四件事要 wbfuwunel 那邊拍板。

## 0. 一句話

to-device（Megolm 金鑰、裝置驗證、SSSS secret）走 `0x16 Device`，
形狀跟 `0x14 Event` **同構**：**推送是主路，`Fetch` 是斷線之後補洞**，
差別只有一個——**它要 ack 才刪**。

## 1. 為什麼不併進 `Event/Recent`

維護者 2026-09-10 問過「device key 能不能混進 event 一起發」。查完之後的結論是**不併**，
但理由跟直覺想的不一樣，三條裡有兩條是假的：

| 一開始以為的理由 | 查證結果 |
|---|---|
| 「`Recent` 是帳號層，to-device 是裝置層」 | ❌ **不成立**。`Recent` 的游標完全在 client 手上，server 對它無狀態；連線是 `Session/Login` 換來的，server 知道 `device_id`，要按裝置回不同內容做得到 |
| 「金鑰要放另一個 db」 | ❌ **不是問題**。金鑰現在就在 matrix-sdk 的 crypto store（帳號目錄的 `m/`），跟 `cache.db` 本來就是兩個 db |
| **「一個游標同時代表兩件事，而其中一件是破壞性的」** | ✅ **這條才是真的**，見下 |

```
拉一窗 → 房間事件寫進 cache.db ✅ → 金鑰匯進 crypto store ❌（磁碟滿／store 壞）
                                    ↑ 游標已經前進 = 已經 ack = server 刪了
```

房間事件的游標前進只表示「我快取好了」，**錯了還能重拉**（永久保存）。
to-device 的游標前進表示「可以刪了」，**錯了就沒了**。要讓一個游標同時是這兩個意思，
就得讓 `cache.db` 與 `crypto.db` **原子性地一起 commit**——兩個 db、兩套失敗模式，做不到。

⭐ 另一個獨立的理由：**to-device 不只有房間金鑰**。`m.key.verification.*`（SAS）與
`m.secret.send`（SSSS）也跑在上面，而**驗證是互動的**。如果金鑰只在「拉房間歷史」時
順便帶回來，使用者按下「驗證這台裝置」之後，對面要等到有人去拉歷史才收得到——流程會卡住。

📎 所以 to-device 需要自己的推送時機，不能寄生在房間事件的節奏上。

## 2. 游標：跟 `g_seq` 同一個號碼空間

`add_to_device_event` 呼叫的是 `globals.next_count()`，**跟 PDU 的 count 同一支**。
所以 to-device 的 count 與 `g_seq` 在同一個號碼空間，可以直接比大小。

但它們是**兩個不同的水位**，所以名字要分開：

| | client 端存的水位 | 語意 | 誰推進 |
|---|---|---|---|
| 房間事件 | `cg_seq` | 我快取到哪 | client 自己，免 ack，**可重讀** |
| **to-device** | **`cd_seq`** | 我**durable 收下並處理完**到哪 | `Device/Ack` 之後 server 才刪 |

⚠️ **`g_seq` 寫進 PDU 的 `unsigned`，to-device 的 count 沒有地方放**——它不是 PDU，
事件 JSON 是 `{ type, sender, content }` 原樣。所以 count 必須由 `Device/Batch`／`Push`
的 **meta 帶**（§3）。

## 3. pack：`0x16 Device` 的六個 subtype

`wbf-wire-format.md` §3.3 早就把 `0x16` 留給「devices、to-device、dehydrated」，
底下一個 subtype 都還沒分配。編號照 `0x14 Event` 那套排，好對照：

| subtype | 方向 | meta | data | 順序類別 |
|---|---|---|---|---|
| `0x01 Fetch` | client → server | `{ "limit": 100?, "cd_seq": <count>?, "to": <count>? }`，`id` 由 client 選 | 無 | 無序 |
| `0x02 Batch` | **server → client** | `{ "tc", "bc", "oldest", "newest", "counts": [...], "r" }`；`id` 抄 `Fetch`，`seq` 從 0 嚴格 +1 | `bc` 則事件，u32 大端長度 ＋ JSON | 有序 |
| `0x03 Ack` | client → server | `{ "until": <count> }` | 無 | 無序 |
| `0x04 Subscribe` | client → server | `{ "device_id": "…", "cd_seq": <count>? }`，`id` 由 client 選（§5） | 無 | 無序 |
| `0x05 Unsubscribe` | client → server | `{}` | 無 | 無序 |
| `0x06 Push` | **server → client** | `{ "bc", "oldest", "newest", "counts": [...], "gap": bool }`；`id` 抄 `Subscribe`，`seq` 每推一次 +1 | 同 `Batch` 的切法 | 事件驅動 |

### 3.1 跟 `Event` 那一套刻意不同的三處

**① 順序是舊 → 新，不是新 → 舊。**

`Event/Batch`／`Push` 是新到舊（讀歷史當然從最新看起）。to-device 相反，理由有兩個：

- **ack 是前綴操作**：`Ack{until}` 刪的是「到這個 count 為止」，舊→新才能處理一則推進一則；
  新→舊要整批收齊才敢 ack。
- **`m.key.verification.*` 是有序的**：一個 SAS flow 的步驟顛倒過來就跑不動。

**② `fs`／`ls` 換成 `oldest`／`newest`。**

`Event` 那邊 `fs`／`ls` 的定義是「這批**最新**／**最舊**的 `g_seq`」——它是語意的，不是位置的。
順序一翻，同樣的欄位名會指到相反的東西，而那種錯只會在半夜爆炸。
所以這裡**換名字**，讓它不可能讀錯。

**③ 多一個 `counts` 陣列。**

`Event` 的每則事件自己的 `unsigned` 裡有 `g_seq`，client 拿得到逐則的號。
to-device 沒有這個位置，所以 meta 要帶 `counts: [c1, c2, …]`（`bc` 個，跟 data 的事件一一對應）。

⚠️ 沒有它就只能整批 ack：批次中間匯入失敗時，要嘛整批重來、要嘛冒險 ack。
有了逐則的 count，client 可以 ack 到失敗的前一則。

## 4. 推送與補齊：`gap` 那一套原封不動搬過來

`wbf-event-push.md` §4 的規矩全部適用，而且**在 to-device 上更成立**：

- **推送絕不阻塞**：`try_send`，佇列滿就丟，下一次推得進去的 `Push` 帶 `gap: true`。
- client 看到 `gap` 就用 **`Device/Fetch(cd_seq)`** 補一窗（房間事件那邊是 `Recent`）。
- ⭐ **掉了的不重送在這裡是安全的**——因為 to-device **`Ack` 之前不刪**，補得回來。
  這一點比房間事件還乾淨：房間事件靠「永久保存」補，to-device 靠「還沒 ack」補。
- **`seq` 只是這條連線的推送序號**，🚫 client 不要拿跳號算少了幾則；水位只認 `newest`。
- **`gap` 只對「下一個 `Push`」有意義**：重連、切回前景，一律 `Fetch` 對一次。

## 5. 訂閱時要帶 `device_id`（維護者 2026-09-10 定的做法）

⚠️ 先更正一個我一開始想錯的方向：`wbf-event-push.md` §1 定「訂閱者的識別碼是連線
（`connection_id`）」**沒有問題**——那是**協議層**，協議層只認連線是對的。
🚫 這件事不該去動 WS 那一層。

問題在**登記的時候**。`Ack` 是破壞性的，同一個裝置開兩條連線都訂閱 to-device：

```
連線 A 收到 count=500 的 m.room_key，匯入成功，Ack{until:500}
連線 B 也收到了，但還在處理 → server 已經刪了 → B 那邊失敗就救不回來
```

**做法**（維護者 2026-09-10）：`Device/Subscribe` 的 meta **帶 `device_id`**，
server 用它判斷是不是重複訂閱，並把那條 `connection_id` **綁定到該裝置**。

```jsonc
{ "device_id": "ABCDEFG", "cd_seq": 12345 }
```

- server 端多一張 `device → connection_id` 的表；已經有人在訂就回 `Error(Conflict)`，
  訊息說已經有另一條連線在收。
- 連線斷掉（`ConnectionGuard` drop）時解除綁定，下一條連得上。
- 🚫 **不要「後來的踢掉先來的」**：那會讓一個手滑開兩個 rpc-cli 的人靜默地換掉正在同步的那條。

⚠️ **`device_id` 必須跟 session 的對得上，對不上就拒絕。** 連線是 `Session/Login` 換來的，
session 裡那個才是權威；client 帶進來的只是**明示意圖**，不是身分來源。
🚫 不驗的話，裝置 A 可以訂閱裝置 B 的 to-device，然後 `Ack` 把 B 的金鑰刪光——
那是同一個帳號底下的橫向破壞，而 to-device 正是「刪了就救不回來」的東西。

📎 為什麼還要帶（既然 session 已經知道）：`Subscribe` 是**登記**的動作，把鍵明寫在請求裡，
server 端那張表要用哪個鍵、client 端在訂什麼，兩邊都不必從 session 推。
出錯時錯誤訊息也講得出「你用 `ABCDEFG` 訂，但這條連線的 session 是 `HIJKLMN`」。

## 6. 保留期：`Ack` 是唯一的刪除入口

`remove_to_device_events(user, device, until)` 已經在了，`Ack` 就是叫它。

⚠️ 但要問清楚一件事：**沒有 ack 的 to-device 會留多久？** 一台裝置從此不再上線
（手機掉了、重灌），它的 to-device 就永遠堆著。房間事件沒有這個問題（本來就永久保存，
而且是共用的）；to-device 是一人一份、一裝置一份的私有佇列。

這是 §7 要 wbfuwunel 定的其中一條。

## 7. ⚠️ 要 wbfuwunel 拍板的四件事

1. **ack 的語意**：`Ack{until}` 表示「收到」還是「處理完」？
   client 這邊想要的是**後者**（匯進 crypto store 成功才 ack），
   因為前者一失敗就永遠救不回來。代價是 server 要留久一點。
2. **同一裝置多條連線**（§5）：`Subscribe` 帶 `device_id`、server 綁 `connection_id`、
   重複訂閱回 `Error(Conflict)`——這個做法可以嗎？綁定的表放哪（`channels` service 旁邊？）？
3. **保留上限**（§6）：沒 ack 的 to-device 留多久？有沒有筆數上限？滿了丟最舊的還是拒收？
4. **`limit` 的上界**：跟 `Recent` 一樣由 `Hello` 的 features 宣告嗎？
   `wbf_push_max_events_per_pack`（現在是 10）要不要有 to-device 自己的一個？

## 8. client 端會怎麼用它（給 server 端理解脈絡）

```
daemon 啟動、Login → Subscribe{device_id, cd_seq: 上次存的}   ← 先登記，再補洞
                  → Device/Fetch(cd_seq) 補一窗    ← 離線期間漏的
                  → 逐則匯進 crypto store（OlmMachine::receive_sync_changes）
                  → Ack{until: 最後一則成功的 count}
                  → 之後靠 Push；收到 gap 就再 Fetch 一次
```

📎 `OlmMachine::receive_sync_changes` 是 matrix-sdk 的公開 API，吃的就是一串 to-device
事件；client 這邊不需要 server 對內容做任何理解——**server 對 to-device 的內容本來就是瞎的**
（`add_to_device_event` 只存 `type`／`sender`／`content`），這個提案不改變那件事。

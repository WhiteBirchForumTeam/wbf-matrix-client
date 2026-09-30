# client 端怎麼接 `0x16 Device`（to-device：金鑰、驗證、SSSS secret）

> 🚨 **線上格式的權威不在這裡**，在 wbfuwunel 的兩份：
> - `docs/design/wbf-wire-format.md` §3.2（`0x16` 的各列）、§4.1（`seq` 屬於會話）
> - `docs/design/wbf-to-device.md`（整套設計與理由）
>
> 🚫 **這份不重複定義 meta 欄位與 byte 佈局**——同一個規則兩份文件一定會漂，而漂的那天
> 沒有人會收到通知（全域 A4）。這裡只寫**三件那邊不會寫的事**：為什麼是這個形狀（§1、§2）、
> **client 最容易寫錯的地方**（§3–§7）、以及實作在哪（§8）。core 怎麼接起來（訂閱線、收金鑰的 task、事件）在 key-sync.md。

## 0. 一句話

to-device 走 `0x16 Device`，形狀跟 `0x14 Event` 同構：**推送是主路、`Fetch` 補洞**，
差別只有一個——**client 拿到之後要叫 server 銷毀**，而那是破壞性的。

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
to-device 的游標前進表示「可以刪了」，**錯了就沒了**。

⭐ 另一個獨立的理由：**to-device 不只有房間金鑰**。`m.key.verification.*`（SAS）與
`m.secret.send`（SSSS）也跑在上面，而**驗證是互動的**。如果金鑰只在「拉房間歷史」時
順便帶回來，使用者按下「驗證這台裝置」之後，對面要等到有人去拉歷史才收得到——流程會卡住。

📎 server 的做法把兩件事拆開：刪除是逐則指名的 `ItemsDestroy`（§4），而「處理到哪」由 server 佇列頭代表、client 不帶游標（§7）。

## 2. 兩個水位，同一個號碼空間

server 的 `add_to_device_event` 用的是 `globals.next_count()`——**跟 PDU 的 `g_seq` 同一支**，
所以兩邊的號碼可以直接比大小。但兩邊的水位不在同一個地方：

| | 水位在哪 | 意思 | 前進的條件 |
|---|---|---|---|
| 房間事件 | client 的 `cg_seq` | 我快取到哪 | 追平那一窗（可重讀，錯了重拉） |
| **to-device** | **server 的佇列頭**（最舊還沒銷毀的那則） | 我**已經處理完**到哪 | `ItemsDestroy` 成功（§4、§7） |

client 另外在 `m/td.json` 記 `cd_seq`（匯進 crypto store 的最新 count）與待銷毀清單。`cd_seq` 🚫 只是紀錄、不當 `Fetch` 的游標（§7）。

⚠️ **to-device 的 count 沒有地方放在事件裡**（它不是 PDU，存的就是 `{type, sender, content}`），
所以每一則的 count 由 pack 的 meta 用 `counts` 陣列帶——這也是為什麼下一節那三件事很重要。

### 2.1 `cd_seq` 寫進 `m/` 裡面（維護者 2026-09-12 定）

⭐ **跟房間金鑰同一個資料夾** —— 維護者的一句話就是判準：
**「你同步到哪，就應該寫到哪。」** 落地是 `m/td.json`（`cd_seq` ＋ 待銷毀清單，`to_device_state.rs`）；
`m/` 裡其他的是 matrix-sdk 自己的 store，🚫 不動它的 schema。

🚫 **不進 `cache.db`**，理由是**失效模式要跟它描述的東西綁在一起**。`td.json` 說的是
「crypto store 已經收到哪、哪些可以刪」，而那個 store 就在 `m/`：

| 發生什麼 | `m/`（含 `td.json`） | 後果 |
|---|---|---|
| `logout`／`account destroy` | 一起沒 | ✅ 下次 `login` 從頭拉，正確 |
| store 壞掉、照 vault-and-keys.md §1.1 手動刪 `m/` 重來 | 一起沒 | ✅ 同上 |
| `cache.db` 被重建（`OpenOutcome::Rebuilt`） | **不受影響** | ✅ 待銷毀清單還在，沒銷成的下次照樣補送 |

⚠️ **放進 `cache.db` 的話這三列全錯**：`cache.db` 活著、`m/` 被刪掉重登入（**我們自己的 `key-backup import` 流程就會走到**），
`td.json` 卻還描述著一個已經不存在的 store——任何之後拿它做判斷的程式都會被騙，而那些是金鑰。

⭐ **fail closed 的形狀**：水位跟它保護的 store 同生共死。`m/` 沒了，水位就該沒了；
而把它寫在 `m/` 裡面，那件事**不需要任何人記得去做**——🚫 不是「刪 store 時順手也刪水位」
那種散在各處的承諾（全域 A6：漏掉的那個不會 fail closed）。

📎 `sync_state` 那張表是 `cg_seq` 的家，`cd_seq` 不進去；local-cache-db.md §5 的 schema
旁邊有一行註記說明它為什麼不在那裡。

## 3. 三件跟 `Event` 相反的事（照 `Event` 的直覺寫一定錯）

`0x16` 的 subtype 編號刻意跟 `0x14` 對齊（同號同位置），所以很容易整段抄過來。
⚠️ **有三處是反的**：

| | `0x14 Event` | `0x16 Device` |
|---|---|---|
| **順序** | 新 → 舊（讀歷史從最新看起） | ⚠️ **舊 → 新** |
| **邊界欄位** | `fs`／`ls`（這批最新／最舊的 `g_seq`） | ⚠️ **`ot`／`nt`**（這批最舊／最新的 count） |
| **每則的號碼** | 事件自己的 `unsigned` 裡有 `g_seq` | ⚠️ **meta 的 `counts` 陣列**，跟 data 一一對應 |

- **為什麼順序要反**：銷毀是**逐則推進**的，而 `m.key.verification.*`（SAS）的步驟**有序**——
  顛倒過來那個流程跑不動。
- **為什麼不沿用 `fs`／`ls`**：那兩個名字的定義是語意的（「最新」「最舊」），不是位置的。
  順序一翻，同一組名字會指到相反的東西。🚫 **不要混用**。
- **為什麼要 `counts`**：沒有它就只能**整批銷毀**——批次中間匯入失敗時，要嘛整批重來，
  要嘛冒險銷毀還沒處理好的。

📎 翻頁把手也不同：`Recent` 用 `before` 往舊的翻；`Device/Fetch` 沒有翻頁參數，**翻頁靠銷毀**：
這窗 `ItemsDestroy` 完，再叫一次不帶 `cd_seq` 的 `Fetch` 就是下一窗（§7；wbfuwunel #87／#88）。

## 4. 銷毀是**帶結果的命令**，不是回執（🚨 最容易寫錯的地方）

🚫 不是「收到就等於刪掉」的回執，也不是前綴刪除：

```
client ──▶ Device/ItemsDestroy { tc } + data（tc × 8 byte，每個是一則的 count）
       ◀── Control/Ack                只表示「命令收到」
       ◀── Device/ItemsDestroyed { tc, bc } + data（bc × 8 byte）  真的沒了的那些
```

client 端因此要守四條：

1. 🚫 **收到 `Ack` 不要把待刪清單清掉**。`Ack` 只說「命令到了」。清掉的依據是
   `ItemsDestroyed` 裡真的回來的那些 count。
2. **送的是清單，不是水位**。我們 durable 存下來的那些 count，**未必是收到那串的前綴**
   （中間有一則匯入失敗就不是了）。
3. **沒回來的留在清單上，下次再送**。命令冪等、重送安全；「已經不在的算銷毀成功」是
   server 明文保證的——我們要的是「遠端沒有這把了」這個**狀態**，不是「這次是我刪的」這個事件。
4. **`tc` 一定要跟 data 的長度對得上**，對不上 server 回 `InvalidRequest` 而且**一則都不刪**。

⚠️ **所以本地要存的是待銷毀清單**（哪些可以刪），🚫 不是一個數字：把「我處理到哪」跟「可以刪哪些」綁成一件事，
正是 server 拒絕前綴刪除的理由。`cd_seq`（處理到哪）另外記著，只是紀錄（§2）。

## 5. 訂閱：`device_id` 是**明示意圖**，不是身分

`Device/Subscribe` 的 meta 帶 `device_id`，server 會**跟這條連線 session 裡的那個比對**，
不合回 `Forbidden`(1302)。

📎 既然 session 已經知道，為什麼還要帶：讓**認錯自己裝置的 client 在這一步就知道**，
而不是把別人的金鑰匯進自己的 store、再把它們銷毀掉。

### 5.1 🚨 收到 `Superseded`(1505) 要當成「被接手」，🚫 不是斷線

⚠️ 一個裝置同時只有一條連線在收，而且**後來的接手先來的**（維護者 2026-09-12 定）。被接手的那條收到：

```
Control/Error   code = Superseded   code_id = 1505   IS_LAST
id = 它自己當初 Subscribe 用的 id
```

- ⭐ `id` 是**自己的**，所以不必為這件事準備第二套解析：對回自己的訂閱就知道死的是哪一段會話。
- ⚠️ **連線本身沒關**（server 只接手 to-device 這一路）：那條線還活著，但已經沒有金鑰會進來。
- 🚨 **絕對不要原地馬上重訂**：對面也會被我們踢掉，然後它也重訂，兩條互踢到天荒地老。
  正確的反應是**停掉這條的 to-device 收取，並讓上層知道**（另一個地方接手了）。
- 我們的做法（維護者：「如果哪個被 close，應該嘗試再開」，link-pool.md §3.1）：收金鑰的 task 收到 1505 就停、發 `keys.state: stopped`、
  **關掉金鑰那條線**；daemon 的看線迴圈下一輪（15 秒後）重開、重訂。這不是「原地馬上重訂」：
  同一個裝置 id 只有這個資料目錄的 session 有，而資料目錄同時只有一個 daemon 能寫——真正會接手我們的，是我們自己那條已經半死、server 還沒發現的舊連線。
  要是真有兩個程式拿同一個 session（資料目錄被整份複製），兩邊會每 15 秒互相接手一次，而不是無間斷地互踢；那是複製資料目錄的錯。

📎 為什麼 server 選搶佔而不是拒絕：**卡死的代價不對稱**。搶佔最壞是重推一次**還在佇列裡**
的東西（那些項目沒被銷毀）；拒絕最壞是先來的其實已經死了（半開連線），於是到 idle timeout
（300 秒）為止這個裝置根本訂不進來，卡死不動的話⛔ 這個 device id 永久廢掉。手機換網路就會踩到。

## 6. to-device 有**自己的一條連線**，而那條線上仍有兩段會話

⭐ **金鑰是獨立的一條 WS**（維護者 2026-09-12 定，architecture-v2.md §5.1）：daemon 跟 server 開五條——雜項、上傳、下載、房間、**金鑰**
（`LinkRole`，link-pool.md §1）。

🚨 **這件事對 to-device 特別重要**，因為 server 端的送出佇列是**每條連線一份**的：
佇列滿了 server 就丟推送並標 `gap`。房間事件掉了可以 `Recent` 重拉、媒體掉了可以重下，
而**金鑰掉了就是永久解不開那些訊息**。⭐ 所以它不跟任何大流量共享佇列——
媒體那條會把佇列塞爆是預期中的事，它只能塞爆自己。

📎 這也讓 `gap` 的意思變乾淨：`Device/Push` 標 `gap`，就**只**表示 to-device 漏了，
拿 `Device/Fetch` 補，🚫 跟房間那條的狀態無關。

### 6.1 但「一條線一段會話」仍然不成立

wbfuwunel PR #42／#44 定的規則，對我們是**直接的約束**：

> ⭐ `id` 是**會話**的名字，`seq` 是那段會話裡的計數。換一個 `id` 就是新會話，`seq` 歸零。

⚠️ **`seq` 不屬於連線、也不屬於 kind。** 就算 to-device 獨佔一條線，那條線上**照樣**
同時有兩段會話：常駐的 `Subscribe`（`Push` 抄它的 `id`）與補洞用的 `Fetch`（`Batch` 抄它的）——
而且補洞期間推送不會停。

| 🚫 錯誤的實作 | 會壞成什麼 |
|---|---|
| 連線層一個 `next_seq` | `Push` 與 `Batch` 互相看起來像對方漏號，`gap` 那套判斷全毀 |
| 每個 kind 一個 `next_seq` | 同 kind 兩段會話交錯（這裡就是 `Subscribe` 與 `Fetch`）分不出哪包是誰的 |

📎 而 `seq` **不是重送機制**：WebSocket 不會掉單一 frame（掉了就是連線沒了），
所以跳號只代表 server **故意**丟了一包（佇列滿），那件事 `gap` 已經明講。
補救是「從佇列頭重新要」（`Fetch`，§7），🚫 不是「重送第幾號封包」。

## 7. 一次啟動的完整流程

```
Session/Login
  → Device/Subscribe { device_id }                    ← 先登記，再追平（🚫 不帶 cd_seq）
  → Device/Fetch   { }                                 ← 🚫 不帶 cd_seq：server 從佇列最舊還沒銷毀的給
  → 逐則匯進 crypto store（OlmMachine::receive_sync_changes_msc4186，舊→新）
      匯成功：那些 count 進待銷毀清單（durable）；cd_seq 記一下處理到哪（只是紀錄）
  → Device/ItemsDestroy { tc } + counts
  → 收 Ack（只是收到）→ 收 ItemsDestroyed → 把真的沒了的從清單移除
  → more: true 就再 Fetch（一樣不帶 cd_seq：銷掉的不會再回來，自然是下一窗）；more: false 追平
  → 之後推來一包就匯銷那一包；gap、匯失敗、壞包 → 再 Fetch 一次（從頭，沒銷的都在）
```

- **先 `Subscribe` 再 `Fetch`**：反過來的話，兩者之間到的那幾則沒有人收。重複拿到無害
  （匯入與銷毀都冪等），漏掉是永久的。
- 🚨 **`Fetch` 不帶 `cd_seq`：佇列頭就是水位**（維護者 2026-09-26，wbfuwunel #87／#88）。client 的游標只要跑到一則還沒進 store 的 item 前面，
  那則就再也問不到：Ack 前推來的先匯、推播匯失敗後下一包成功、`CryptoState.gap` 沒接，三條都會。佇列本身沒有洞（銷毀前不刪），
  所以讓 server 從最舊還沒銷毀的給，`ItemsDestroy` 就是唯一的「處理完了」（也就是 ACK）。代價是「匯了但還沒銷成」的那幾則會再回來一次。
- `OlmMachine::receive_sync_changes_msc4186` 是 matrix-sdk 的公開 API，吃的就是一串 to-device 事件（🚫 不用 `receive_sync_changes`：它把「沒給 OTK 數量」當 0，e2ee-walkthrough.md §7）。
  📎 **server 對內容是瞎的**（只存 `type`／`sender`／`content`），這套不改變那件事。
- ⚠️ **保留期是無窮 TTL**：沒被銷毀的永遠留著，`ItemsDestroy` 是唯一的刪除入口。
  所以「先不刪，之後再說」不會掉東西——但會讓佇列一直長大（server 端有
  `!admin user to-device-queue` 看得到）。🚫 不要把「反正不會掉」當成不實作銷毀的理由。

## 8. client 端的實作在哪

| # | 做什麼 | 在哪 |
|---|---|---|
| 1 | `Kind::Device = 0x16` 與八個 subtype 常數 | `crates/wbf-wire/src/pack.rs`（`pack::device`） |
| 2 | meta 型別與 `counts`／`tc × 8 byte` 的編解碼（對 server 向量逐 byte） | `wbf-sdk/src/protocol.rs` 的 Device 段：`device_fetch`／`device_subscribe`／`device_unsubscribe`／`device_items_destroy`、`parse_device_batch`、`parse_items_destroyed`、`parse_subscribe_reply`、`CryptoStateMeta`、`DevicePushMeta`；連線上是 `WbfClient::device_fetch_window`／`device_subscription`／`device_items_destroy`／`device_unsubscribe` |
| 3 | `cd_seq`（只是紀錄）與待銷毀清單的落地 | `wbf-sdk/src/to_device_state.rs` → **`m/td.json`**（§2.1），🚫 不進 `cache.db` |
| 4 | 訂閱／追平／匯入／銷毀的狀態機 | core `key_sync.rs`（key-sync.md）：推來的與拉的都走 `OlmEngine::import_items`（維護者 2026-09-24：同一支）；gap／匯失敗／壞包就從佇列頭再拉 |
| 5 | `Superseded`(1505)（§5.1） | 它的 id 是訂閱的 id，會話表把它交進訂閱的 handle 當終點；core 的 task 收到就停、發 `keys.state: stopped`、關掉金鑰那條線 |
| 6 | 說出口的退出（§4）：下線前 `Unsubscribe` 解除持有 | `WbfClient::device_unsubscribe()`；core 登出前叫（`unsubscribe_keys_of`） |
| 7 | 「匯入 → 落地 → 銷毀」鎖成一步，呼叫者拿不到錯的順序 | `crypto_engine::OlmEngine::import_items`（`Fetch` 的一窗、或推來的一包）／`pull_to_device` |

⚠️ **`ItemsDestroy` 只有持有這台裝置佇列的連線能做**（server `device.rs` 回 `Forbidden`）：所以順序是 `Subscribe` → `Fetch` → 匯入 → `ItemsDestroy`，跟 §7 一致；🚫 不能只 Fetch 不 Subscribe 就想銷毀。`Fetch`／`ItemsDestroy` 因此走金鑰那條線（key-sync.md §1）。

## 9. 落地版跟原提案的不同

舊 commit 與舊留言裡的名字對照：`Ack { until }`（前綴刪除）→ `ItemsDestroy`＋`ItemsDestroyed`（§4）；`oldest`／`newest` → `ot`／`nt`；重複訂閱回 `Conflict` → 後來的接手＋`Superseded`（§5.1）；`Fetch { to }` 與 `Fetch`／`Subscribe` 的 `cd_seq` → 拿掉（§3、§7）。

# client 端怎麼接 `0x16 Device`（to-device：金鑰、驗證、SSSS secret）

> 2026-09-10 寫成**給 wbfuwunel 的提案**；2026-09-12 **server 端實作完了**
> （wbfuwunel PR #41 提案 → #42 共用核心 → #43 實作 → #44 補文件），所以這份改寫成
> **client 端的接線文件**。
>
> 🚨 **線上格式的權威不在這裡**，在 wbfuwunel 的兩份：
> - `docs/design/wbf-wire-format.md` §3.2（`0x16` 的七列）、§4.1（`seq` 屬於會話）
> - `docs/design/wbf-to-device.md`（整套設計與理由）
>
> 🚫 **這份不重複定義 meta 欄位與 byte 佈局**——同一個規則兩份文件一定會漂，而漂的那天
> 沒有人會收到通知（全域 A4）。這裡只寫**三件那邊不會寫的事**：為什麼是這個形狀（§1、§2）、
> **client 最容易寫錯的地方**（§3–§6）、以及我們自己要做什麼（§8）。

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

📎 server 最後的做法比這份提案更乾淨：**水位跟刪除根本不是同一個東西**（§4）。

## 2. 兩個水位，同一個號碼空間

server 的 `add_to_device_event` 用的是 `globals.next_count()`——**跟 PDU 的 `g_seq` 同一支**，
所以兩邊的號碼可以直接比大小，但它們是**兩個水位**：

| | client 存的 | 意思 | 前進的條件 |
|---|---|---|---|
| 房間事件 | `cg_seq` | 我快取到哪 | 追平那一窗（可重讀，錯了重拉） |
| **to-device** | **`cd_seq`** | 我**已經處理完**到哪 | 匯進 crypto store 成功 |

⚠️ **to-device 的 count 沒有地方放在事件裡**（它不是 PDU，存的就是 `{type, sender, content}`），
所以每一則的 count 由 pack 的 meta 用 `counts` 陣列帶——這也是為什麼下一節那三件事很重要。

### 2.1 `cd_seq` 寫進 `m/` 裡面（維護者 2026-09-12 定）

⭐ **跟房間金鑰同一個資料夾** —— 維護者的一句話就是判準：
**「你同步到哪，就應該寫到哪。」**

🚫 **不進 `cache.db`**，理由是**失效模式要跟它描述的東西綁在一起**。`cd_seq` 說的是
「crypto store 已經收到哪」，而那個 store 就在 `m/`：

| 發生什麼 | `m/`（含 `cd_seq`） | 後果 |
|---|---|---|
| `logout`／`account destroy` | 一起沒 | ✅ 下次 `login` 從頭拉，正確 |
| store 壞掉、照 §4.1 手動刪 `m/` 重來 | 一起沒 | ✅ 同上 |
| `cache.db` 被重建（`OpenOutcome::Rebuilt`） | **不受影響** | ✅ 水位還在，🚫 不會重拉一批已經匯過的 |

⚠️ **放進 `cache.db` 的話這三列全錯**，而錯得最重的是第二列：`cache.db` 活著、`m/` 被刪掉
重登入（**我們自己的 `key-backup import` 流程就會走到**），`cd_seq` 卻還停在舊的高水位——
那段區間**永遠不會再拉**，而那些是金鑰。

⭐ **fail closed 的形狀**：水位跟它保護的 store 同生共死。`m/` 沒了，水位就該沒了；
而把它寫在 `m/` 裡面，那件事**不需要任何人記得去做**——🚫 不是「刪 store 時順手也刪水位」
那種散在各處的承諾（全域 A6：漏掉的那個不會 fail closed）。

📎 `sync_state` 那張表是 `cg_seq` 的家，`cd_seq` 不進去；local-cache-db §6 的 schema
旁邊有一行註記說明它為什麼不在那裡。

🔲 **還沒定的只剩落地格式**：`m/` 裡面是 matrix-sdk 自己的 store（我們🚫 不動它的 schema），
所以是同目錄下的一個小檔（一個數字 ＋ 待銷毀清單，§4）還是別的，等實作那支決定。

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

📎 翻頁把手也不同：`Recent` 用 `before` 往舊的翻，`Device/Fetch` **沒有** `before`，
下一窗帶上一窗的 `nt` 當 `cd_seq`（`cd_seq` 同時是底與把手）。原提案有個 `to`，**server 拿掉了**。

## 4. 銷毀是**帶結果的命令**，不是回執（🚨 最容易寫錯的地方）

原提案寫的是 `Ack { until }`——收到就等於刪掉、而且是前綴刪除。**server 推翻了它**：

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

⚠️ **這條閉環決定了我們的本地狀態要有兩份**：`cd_seq`（處理到哪）與**待銷毀清單**（哪些可以刪）。
把它們合成一個數字，就是把「我處理到哪」跟「可以刪哪些」綁成一件事——正是 server 拒絕前綴刪除的理由。

## 5. 訂閱：`device_id` 是**明示意圖**，不是身分

`Device/Subscribe` 的 meta 帶 `device_id`，server 會**跟這條連線 session 裡的那個比對**，
不合回 `Forbidden`(1302)。

📎 既然 session 已經知道，為什麼還要帶：讓**認錯自己裝置的 client 在這一步就知道**，
而不是把別人的金鑰匯進自己的 store、再把它們銷毀掉。

### 5.1 🚨 收到 `Superseded`(1505) 要當成「被接手」，🚫 不是斷線

⚠️ 一個裝置同時只有一條連線在收，而且**後來的接手先來的**（維護者 2026-09-12 定，
推翻了本文原本寫的「拒絕後來的」）。被接手的那條收到：

```
Control/Error   code = Superseded   code_id = 1505   IS_LAST
id = 它自己當初 Subscribe 用的 id
```

- ⭐ `id` 是**自己的**，所以不必為這件事準備第二套解析：對回自己的訂閱就知道死的是哪一段會話。
- ⚠️ **連線本身沒關**（server 只接手 to-device 這一路）。🚫 不要因為這個錯誤去重連——
  📎 在我們這邊金鑰是獨佔一條線的（§6），所以那條線收到 `Superseded` 之後就沒事做了，
  但**它仍然是活的**：重連只會再觸發一次搶佔。
- 🚨 **絕對不要自動重訂**：對面也會被我們踢掉，然後它也重訂，兩條互踢到天荒地老。
  正確的反應是**停掉這條的 to-device 收取，並讓上層知道**（另一個地方接手了）。

📎 為什麼 server 選搶佔而不是拒絕：**卡死的代價不對稱**。搶佔最壞是重推一次**還在佇列裡**
的東西（那些項目沒被銷毀）；拒絕最壞是先來的其實已經死了（半開連線），於是到 idle timeout
（300 秒）為止這個裝置根本訂不進來，卡死不動的話⛔ 這個 device id 永久廢掉。手機換網路就會踩到。

## 6. to-device 有**自己的一條連線**，而那條線上仍有兩段會話

⭐ **daemon 跟 server 開四條 WS，金鑰是獨立的一條**（維護者 2026-09-12 定，
architecture-v2 §6.1）：房間一條、**金鑰一條**、媒體一條、雜項一條。

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
補救是「帶游標重新要」（`Fetch`），🚫 不是「重送第幾號封包」。

## 7. 一次啟動的完整流程

```
Session/Login
  → Device/Subscribe { device_id, cd_seq: 上次存的 }   ← 先登記，再補洞
  → Device/Fetch   { cd_seq }                          ← 離線期間漏的，一窗 1000 則
  → 逐則匯進 crypto store（OlmMachine::receive_sync_changes，舊→新）
      每匯成功一則：cd_seq = 那則的 count；那個 count 進待銷毀清單（durable）
  → Device/ItemsDestroy { tc } + counts
  → 收 Ack（只是收到）→ 收 ItemsDestroyed → 把真的沒了的從清單移除
  → r > 0 就再 Fetch（cd_seq = 上一窗的 nt）；r == 0 這一窗結束
  → 之後靠 Push；meta 的 gap: true 就再 Fetch 一次補
```

- **先 `Subscribe` 再 `Fetch`**：反過來的話，兩者之間到的那幾則沒有人收。重複拿到無害
  （匯入與銷毀都冪等），漏掉是永久的。
- `OlmMachine::receive_sync_changes` 是 matrix-sdk 的公開 API，吃的就是一串 to-device 事件。
  📎 **server 對內容是瞎的**（只存 `type`／`sender`／`content`），這套不改變那件事。
- ⚠️ **保留期是無窮 TTL**：沒被銷毀的永遠留著，`ItemsDestroy` 是唯一的刪除入口。
  所以「先不刪，之後再說」不會掉東西——但會讓佇列一直長大（server 端有
  `!admin user to-device-queue` 看得到）。🚫 不要把「反正不會掉」當成不實作銷毀的理由。

## 8. client 端要做的（🔁 2026-09-21 更新狀態）

| # | 做什麼 | 在哪 | 狀態 |
|---|---|---|---|
| 1 | `Kind::Device = 0x16` 進 pack 的 kind 表 | `crates/wbf-wire/src/pack.rs` | ✅ 含八個 subtype 常數（`pack::device`） |
| 2 | 八個 subtype 的 meta 型別與 `counts`／`tc × 8 byte` 的編解碼 | `wbf-sdk/src/protocol.rs` 的 Device 段：`device_fetch`／`device_subscribe`／`device_items_destroy`、`parse_device_batch`、`parse_items_destroyed`、`parse_subscribe_reply`、`CryptoStateMeta` | ✅ 對 server 向量逐 byte；`WbfClient::device_fetch_window`／`device_subscribe`／`device_items_destroy` |
| 3 | `cd_seq` 與待銷毀清單的落地 | `wbf-sdk/src/to_device_state.rs` → **`m/td.json`**（§2.1），🚫 不進 `cache.db` | ✅ |
| 4 | 訂閱／補洞／匯入／銷毀的狀態機 | core `key_sync.rs`（[key-sync.md](key-sync.md)）：訂閱線開好就 `Device/Subscribe` → `pull_to_device` 追平 → 收金鑰的 task；推來的與拉的都走 `OlmEngine::import_items`（維護者 2026-09-24：同一支） | ✅ 2026-09-24；🚫 `Subscribe` 不帶 `cd_seq`（補窗由 `pull_to_device` 做） |
| 5 | `Superseded`(1505) 的處理（§5.1） | 錯誤詞表已有 1505；它的 id 是訂閱的 id，會話表把它交進訂閱的 handle 當終點；core 的 task 收到就停、發 `keys.state: stopped`，🚫 不重訂、🚫 不關線 | ✅ 2026-09-24 |
| 6 | 說出口的退出（§4）：下線前 `Unsubscribe` 解除持有 | `WbfClient::device_unsubscribe()`；core 登出前叫（`unsubscribe_keys_of`） | ✅ 2026-09-24 |
| 7 | 「匯入 → 落地 → 銷毀」鎖成一步，呼叫者拿不到錯的順序 | `crypto_engine::OlmEngine::import_items`（吃「一批 items」：`Fetch` 的一窗、或推來的一包）／`pull_to_device` | ✅ 對真 server 走通 |

⚠️ 實跑補的一條：**`ItemsDestroy` 只有持有這台裝置佇列的連線能做**（server `device.rs` 回 `Forbidden`），所以順序是 `Subscribe` → `Fetch` → 匯入 → `ItemsDestroy`，跟 §7 一致；🚫 不能只 Fetch 不 Subscribe 就想銷毀。

⭐ 第 4 條決定了順序：**這件事排在 daemon 之後**，handover §7 第 3 項。

## 9. 落地版跟這份原提案的五處不同（紀錄）

📎 留著是因為下一個讀舊 commit 或舊留言的人會撞到這些名字。

| 原提案 | 落地的 | 為什麼 |
|---|---|---|
| `0x03 Ack { until }`，前綴刪除 | **`0x03 ItemsDestroy { tc }` ＋ data**，逐則指名 | 刪除是命令不是回執；前綴把「處理到哪」與「可以刪哪些」綁成一件事 |
| `Ack` 之後就算刪了 | **`Ack` 只表示收到命令**，結果由 `0x07 ItemsDestroyed` 帶回 | 命令要有回應，回應要講結果 |
| 六個 subtype | **七個**（多 `ItemsDestroyed`） | 同上 |
| `oldest` / `newest` | **`ot` / `nt`** | 跟線上其他欄位（`bc`、`tc`、`fs`、`ls`、`r`）一致 |
| 重複訂閱回 `Conflict` | **後來的接手**＋`Superseded`(1505) | §5.1 的代價不對稱 |
| `Fetch { to }` | **拿掉** | 它對到的是 `cd_seq` 的位置不是翻頁把手，而唯一想得到的情境不需要它 |

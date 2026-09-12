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

### 2.1 🔲 `cd_seq` 存哪裡（client 端的決定，還沒定）

**建議：存在帳號目錄的 `m/` 旁邊，🚫 不要放進 `cache.db`。**

理由是**失效模式要跟它描述的東西綁在一起**：`cd_seq` 說的是「crypto store 已經收到哪」，
而 `cache.db` 是**可以被重建的**（`Cache::open` 的 `OpenOutcome::Rebuilt`，local-cache-db §6.1）。
⚠️ 重建一次 `cache.db`，`cd_seq` 就跟著歸零或消失，而 crypto store 沒事——
於是 client 會**重拉一批已經匯過的**（無害，冪等）或**以為自己落後**。反過來更糟：
`cache.db` 活著但 `m/` 被刪掉重登入（我們自己的 `key-backup import` 流程就會遇到），
`cd_seq` 卻還停在舊的高水位，那段區間**永遠不會再拉**——而那些是金鑰。

⭐ **fail closed 的形狀**：水位跟它保護的 store 同生共死。`m/` 沒了，水位就該沒了。

🔲 這條要維護者點頭，而且它會動到 local-cache-db §6 的 `sync_state`（那張表是 `cg_seq` 的家，
`cd_seq` **不進那張表**）。

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
- ⚠️ **連線本身沒關**——它的房間訂閱照常跑。🚫 不要因為這個錯誤去重連整條連線。
- 🚨 **絕對不要自動重訂**：對面也會被我們踢掉，然後它也重訂，兩條互踢到天荒地老。
  正確的反應是**停掉這條的 to-device 收取，並讓上層知道**（另一個地方接手了）。

📎 為什麼 server 選搶佔而不是拒絕：**卡死的代價不對稱**。搶佔最壞是重推一次**還在佇列裡**
的東西（那些項目沒被銷毀）；拒絕最壞是先來的其實已經死了（半開連線），於是到 idle timeout
（300 秒）為止這個裝置根本訂不進來，卡死不動的話⛔ 這個 device id 永久廢掉。手機換網路就會踩到。

## 6. 一條連線上會有**兩段以上的會話**同時在跑

wbfuwunel PR #42／#44 定了一條規則，對我們是**直接的約束**：

> ⭐ `id` 是**會話**的名字，`seq` 是那段會話裡的計數。換一個 `id` 就是新會話，`seq` 歸零。

⚠️ **`seq` 不屬於連線、也不屬於 kind。** daemon 上最正常的形狀就是一條連線同時背著
房間推送（`Event/Push`）與 to-device 推送（`Device/Push`）——

| 🚫 錯誤的實作 | 會壞成什麼 |
|---|---|
| 連線層一個 `next_seq` | 兩種推送互相看起來像對方漏號，`gap` 那套判斷全毀 |
| 每個 kind 一個 `next_seq` | 同 kind 兩段會話交錯（例如兩個上傳）就分不出哪包是誰的 |

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

## 8. client 端要做的（還一個字都沒寫）

| # | 做什麼 | 在哪 |
|---|---|---|
| 1 | `Kind::Device = 0x16` 進 pack 的 kind 表 | `crates/wbf-wire/src/pack.rs`（現在只有 `Event = 0x14`） |
| 2 | 七個 subtype 的 meta 型別與 `counts`／`tc × 8 byte` 的編解碼 | `wbf-sdk`，跟 `Event` 那組放一起 |
| 3 | `cd_seq` 與待銷毀清單的落地（§2.1 待定） | 帳號目錄，🚫 不進 `cache.db` |
| 4 | 訂閱／補洞／匯入／銷毀的狀態機 | ⚠️ **daemon 才有意義**——「一個命令一個程序」的東西沒有人在線上收（architecture-v2 §1） |
| 5 | `Superseded`(1505) 的處理（§5.1） | 錯誤詞表要跟上 server 的 §3.4 |

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

# daemon 的執行期：每帳號的上游會話、事件流、進度與取消

> 2026-09-13 第一版。**這份講「跑起來之後發生什麼」**：daemon 對每個帳號維持什麼連線、
> 事件怎麼從 homeserver 一路變成前端收到的推播、長工作的進度歸誰、取消到底停掉什麼。
>
> 🚫 **不重複**別的文件：分層與介面在 [`architecture-v2.md`](architecture-v2.md)（§6.1 定了
> 「每個帳號一組」與傳輸選擇）、RPC 的逐條訊息在 [`rpc-spec.md`](rpc-spec.md)（§3.9 訂閱與取消、
> §4 四種推播）、線上格式的權威在 wbfuwunel 那邊的 `wbf-wire-format.md`。
>
> 🚨 **狀態：草案**。這份是「要動手做的那一支」的設計，還沒實作；凍結的時點是那支合併。

## 0. 一張圖

```
                     ┌── 帳號 A 的上游會話 ──┐
homeserver A ────────┤  wbf？四條 WS         ├──┐
                     └  一般？HTTP /sync     ┘  │
                     ┌── 帳號 B 的上游會話 ──┐  │   解密 → 寫 cache.db
homeserver B ────────┤  …                    ├──┼──> 發 CoreEvent ──> daemon 翻成 RPC 推播
                     └                       ┘  │                        │
                                                │                        ├─> 連線 1（訂了 room.message）
長工作（upload／recent／download）──發 Progress──┘                        └─> 連線 2（只訂了自己的 progress）
```

三件事在這份文件裡定：**誰去連**（§1）、**事件怎麼變成推播**（§2）、**長工作的進度與取消**（§3、§4）。

## 1. 每個帳號一組上游會話

### 1.1 誰開、什麼時候開、什麼時候關

**一個「監督者」（supervisor）管所有帳號的會話**，🚫 不是每個請求自己去連。

| 事件 | 監督者做什麼 |
|---|---|
| vault 解鎖成功（`vault.unlock`／`vault.create`） | 掃出**所有已登入**的帳號，各起一個會話 |
| `account.add` 成功 | 幫那一個帳號起會話 |
| `account.del`／`destroy` | 停掉那一個帳號的會話（先停再刪檔） |
| `daemon.shutdown` | 全部停掉，等它們收攤 |
| 會話自己掉線 | 退避重連（§1.3），🚫 不放棄、🚫 不無限快轉 |

- ⚠️ **vault 沒解鎖就不連**：連線要 `access_token`，而 token 封在 `session.sealed` 裡。
  沒解鎖之前 daemon 什麼都讀不到 —— 這也是 `1002`／`1001` 那兩個閘門存在的理由。
- ⚠️ **沒有寫權就不連**（architecture-v2 §0.2）：上游會話會寫 `cache.db`，而寫要有能力。
  單發命令那條路本來就不該起長命的上游會話。
- 📎 一個帳號**只有一組**會話：`account.switch` 只是換「預設對誰動作」，🚫 不影響誰在連。

### 1.2 用哪一種傳輸：預設 HTTP，探到 wbf 才走四條 WS

判準與「不確定落 HTTP」的理由在 architecture-v2 §6.1，這裡只定**怎麼探**：

```
起會話 ──> conf TRANSPORT=http？ ──是──> HTTP（結束，不探）
             │否
             └─> 開一條 WS，送 Hello ──成功（拿到 ServerHello）──> wbf：開四條 WS
                                     └─失敗／逾時／認不得──────> HTTP
```

- **探測結果記在會話裡，不落地**：daemon 重開就重探。⭐ 一台 server 可能升級成 wbf、
  也可能被換掉；把「它是 wbf」寫進檔案，就得處理「它不再是」那天。
- 探測用的那條 WS **就是四條裡的「雜項」那條**，🚫 不另外開一條再丟掉。
- ⚠️ 逾時要短（秒級）：探測擋在「使用者剛登入、什麼都還沒看到」的路上。

### 1.3 斷線、重連、`sync.state`

前端看得到的只有 `sync.state` 這一個推播（rpc-spec §4），它講的是**那一個帳號**：

| 狀態 | 什麼時候 |
|---|---|
| `catching_up` | 連上了，但還在補洞（`Recent` 還沒拉平，或 HTTP 的第一輪 `/sync` 還沒回） |
| `caught_up` | 追平了：之後進來的都是新的 |
| `disconnected` | 斷了（網路、server 關了、token 失效） |

- 重連退避：**指數、有上限、有抖動**（例如 1s → 2s → 4s …最多 30s，±20%）。
  ⭐ 抖動是必要的：daemon 一次管 N 個帳號，同一台 server 掉線時它們會同時醒來。
- ⚠️ **`token` 失效（401）不是「斷線」**：那是這個帳號**登出了**，重連只會一直被拒。
  → 停掉會話、發一次 `disconnected`，並讓 `account.list` 的 `logged_in` 說實話。
- 重連成功之後**一定先補洞再說 `caught_up`**（wbf 走 `Event/Recent` 從 `cg_seq`，
  HTTP 走 `/sync` 的 `since`）。🚫 不要一連上就說追平了。

### 1.4 四條線各自的用途

架構在 architecture-v2 §6.1.1（房間／金鑰／媒體／雜項，分界是「誰會塞爆佇列」×「掉了救不救得回」）。
執行期要補的只有兩點：

- **一條線斷掉不等於會話結束**：房間那條斷了就重連房間那條，🚫 不要把金鑰那條也拖下水。
- **`gap` 是局部的**：房間那條標 `gap` → 只補房間（`Recent`）；金鑰那條標 `gap` → 只補金鑰（`Fetch`）。

## 2. 事件怎麼變成推播

### 2.1 core 發什麼

`CoreEvent`（`crates/wbf-core/src/event.rs`）是唯一的出口，四種：

| variant | 什麼時候 | 對到哪個推播 |
|---|---|---|
| `Note { job, text }` | 一句給人看的話 | `progress`（帶 `note`） |
| `Progress { job, done, total, text }` | 長工作的數字進度 | `progress` |
| `Message { user, message }` | 收到一則訊息（上游會話或 `watch`） | `room.message` |
| `SyncState { user, state, cg_seq }` | 那個帳號的連線狀態變了 | `sync.state` |

⚠️ **帳號相關的事件一定帶 `user`**：事件是每個帳號一組的（§1），前端要分得出這是誰的。
🚫 不要讓前端從 `message.conversation` 反推帳號 —— 兩個帳號可能在同一個房間裡。

### 2.2 daemon 怎麼分派

- **訂閱是每條連線一份**（rpc-spec §3.9）：連線關掉就沒了，🚫 不落地、🚫 不跨連線共用。
- `subscribe { events, user? }`：`user` 給了就只收那個帳號的。`"*"` 是全訂。
- **`progress` 例外**：發出那個長工作的**那條連線**自動收到自己請求的進度，不必訂閱
  —— 不然每個前端都要多寫一步，而它想要的東西是明擺著的。
- ⚠️ **掉事件是允許的**（`EVENT_QUEUE` 滿了、慢的訂閱者）：推播是「不用輪詢」，
  🚫 不是「保證看得到全部」。掉了的處置是**重查狀態**（`room.history`／`sync.recent`），
  而不是重播 —— 這跟 server 端 `gap` 的處置是同一條原則。

### 2.3 一則訊息的完整路徑

```
homeserver ──事件──> 上游會話 ──解密（OlmMachine／matrix-sdk）──> 寫 cache.db
                                                                    │
                                          ⭐ 寫完之後才發事件 ───────┘
                                                                    ↓
                                     CoreEvent::Message ──> 各連線的訂閱過濾 ──> room.message
```

🚨 **順序是「先寫庫、後發事件」**：前端收到推播之後八成會馬上去查（例如捲到那一則），
反過來的話它查到的是還沒有那則訊息的資料庫。

## 3. 進度歸誰：`job`

長工作的進度必須說得出**是哪一個請求的**（rpc-spec §4 的 `progress.id`），可是 core
不知道「請求」這種東西，而三個長工作的事件在同一條廣播上是交錯的。

做法是 `crates/wbf-core/src/job.rs`：daemon 把整個工作包在 `run_as_job(請求的 id, …)` 裡，
`EventSink` 發事件時自己去問「現在在哪個工作裡」（`tokio` 的 task-local）。

- ⭐ **一個地方標、一個地方讀**：中間十幾層一個字都不用改。
  🚫 不走「每個長工作的方法多收一個 `job` 參數」—— 那要改十幾個公開簽名，而中間任何一層
  忘了往下傳，事件就默默變成無主的，那種漏法不會有人發現。
- ⚠️ **限制寫在那個模組裡**：task-local 不跟著 `tokio::spawn` 走。core 與 sdk 現在的長工作
  都在呼叫者的 task 上跑，所以沒問題；哪天有人在 core 裡 `spawn` 一個會發進度的 task，
  那些事件會變成 `job: None`。有一條測試釘住這個限制。
- `job: None` 不是錯誤：CLI 直接叫 core、daemon 的背景工作（上游會話）都是 `None`。
  **背景工作的進度不推給任何人**，🚫 不要硬塞給某條連線。

## 4. 取消

`cancel { id }`（rpc-spec §3.9）要停掉的是**這條連線上**那個還在跑的請求。

```rust
// 每個請求各自 spawn 的那一層
let response = tokio::select! {
    response = handle.call(request) => response,
    _ = cancelled => Response::error(id, code::CANCELLED, "cancelled"),
};
```

- ⭐ 用 `select!` 而不是 `JoinHandle::abort()`：**取消之後還要回一則 `105`**，而被 abort 的
  task 講不出話。`select!` 落選的那一邊會被 drop —— 對 async 來說那就是真的停下來
  （下一個 await 點不會再被輪詢）。
- **已經回完的 `id` → `{ ok: true, was_running: false }`**，🚫 不是錯誤：
  前端按取消的時候它可能剛好跑完，那不是誰做錯了。
- ⚠️ **取消會留下半成品**，而處置各自不同：
  | 工作 | 取消之後 |
  |---|---|
  | 上傳 | 狀態檔還在（CLI 規格 §6），**可以續傳**；🚫 不自動 `abort` 掉伺服器那邊 |
  | 下載 | 媒體池的段是可續的（local-cache-db §8）；`--no-cache` 的直接下載會刪掉半個檔 |
  | `sync.recent` | 水位只在**整批寫完**之後前進，所以取消不會留下「假裝拉過」的洞 |
- 🚫 **不能取消的東西不假裝可以**：`cancel` 一個不是長工作的 `id`（例如 `account.list`）
  多半只會拿到 `was_running: false`，因為它早就回完了。

## 5. 失敗與邊界

| 狀況 | 行為 |
|---|---|
| vault 鎖著 | 🚫 不起上游會話；`1002`／`1001` 由閘門回答（rpc-spec §3.1） |
| 沒有寫權（別的 daemon 佔著） | 🚫 不起上游會話；會寫的 method 回 `109` |
| 上游斷線，但有請求正在跑 | 那個請求照它自己的錯誤路徑失敗（`1300` network），🚫 不因為「等一下會重連」就掛在那裡 |
| 前端關掉連線 | 它的訂閱沒了；**它發起的長工作繼續跑**（rpc-spec §1.2 第 6 點），進度沒人收就沒人收 |
| daemon 關閉 | 先停上游會話、再等在跑的請求收攤，最後才關 listener |

## 6. 分階段

| 階段 | 內容 | 狀態 |
|---|---|---|
| 1 | core 的事件形狀（`Note`／`Progress`／`Message`／`SyncState`）＋ `job` | 🔁 做了，還沒送審 |
| 2 | daemon 的訂閱、推播封裝、`progress` 自動路由 | ❌ |
| 3 | `cancel`（§4） | ❌ |
| 4 | sdk 的 `Event/Subscribe`（`0x04`）／`Unsubscribe`（`0x05`）／`Push`（`0x06`） | ❌ 向量已經有，codec 還沒寫 |
| 5 | 上游會話：探測、兩種傳輸的收事件迴圈、寫庫、發事件 | ❌ |
| 6 | 監督者：跟著解鎖／登入／登出起停，退避重連 | ❌ |

## 7. 明確不做的

- 🚫 **不重播掉掉的事件**：沒有「從第 N 個事件開始重送」這種東西。掉了就重查狀態（§2.2）。
- 🚫 **不把「這台 server 是 wbf」寫進設定檔**：每次起會話重探（§1.2）。
- 🚫 **不做跨帳號的合併事件流**：每個帳號各自一組，前端要合併是前端的事。
- 🚫 **不在 daemon 裡做通知（notification）政策**：什麼該響、什麼該安靜是 UI 的事，
  daemon 只負責「有一則新訊息」這個事實。

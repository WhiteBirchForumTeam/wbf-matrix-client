# daemon 的執行期：多帳號怎麼落到資料庫、UI 怎麼拿、命令怎麼往上游走

> **這份講執行期**：多帳號如何落地到 db、db 如何與 UI 互動、命令如何被解讀成上游同步——資料在哪、誰寫、寫的時候會不會撞、UI 的每一個動作實際走哪條路。
> 🚫 不重複分層（[`/docs/design/overview/architecture-v2.md`](../overview/architecture-v2.md)）、逐條訊息（[`/docs/design/rpc-specs/rpc-spec.md`](../rpc-specs/rpc-spec.md)）、
> 資料庫 schema（[`/docs/design/storage/local-cache-db.md`](../storage/local-cache-db.md)、/docs/design/messages/read-receipts.md）。
>
> §3 的 `sync` 參數、/docs/design/messages/read-receipts.md 的已讀三層是維護者 2026-09-13 定的方向。

## 0. 先把三個字分開

「sync」這個字在這個專案裡指過三件不同的事，混用是本文件要根除的第一個問題：

| 說法 | 意思 | 誰做 | 什麼時候 |
|---|---|---|---|
| **上游同步** | homeserver → `cache.db` | daemon（上游會話收推播；`Recent` 與進房 backfill 由 UI 叫，§4.3） | 推播一直在收；`Recent` 只在 UI 叫的時候 |
| **本地讀** | `cache.db` → UI | daemon 回答 RPC 的讀命令 | UI 每次要顯示東西 |
| **matrix-sdk 的 `/sync`** | matrix-sdk 自己的同步，寫它自己的 store（`m/`） | matrix-sdk | 一般 Matrix 帳號的房間命令前各跑一次（`synced_backend_of`）；wbf 帳號沒有 |

🚨 **UI 拿聊天紀錄是「本地讀」**，🚫 不是上游同步。UI 每捲一頁就打一次 homeserver 是錯的設計：
資料早就在 `cache.db` 裡了（daemon 收到事件時就寫進去了），UI 要的只是「把它讀出來」。

```
homeserver ──上游同步──> cache.db ──本地讀──> UI
     ↑                       │
     └── UI 明確要求時 ──────┘   （sync.recent：UI 起來時、以及它決定要補的時候叫；補不補是 UI 的事，§4.3）
```

## 1. 資料在哪：每帳號一份 vs 每 server 一份

```
<data dir>/
  local.key                    一台機器一把主金鑰
  daemon.lock  daemon.json     獨佔與 ready（/docs/design/overview/architecture-v2.md §0.2、/docs/design/rpc-specs/local-interface.md §3）
  s/<加密的 server 名>/
    cache.db                   🚨 **一個 server 一份，這台機器上所有這個 server 的帳號共用**
    media/                     媒體池：一個加密池、一把鑰，不分帳號
    a/<加密的帳號名>/
      session.sealed           access_token（每帳號）
      m/                       matrix-sdk 的 crypto store（每帳號；wbf 帳號由 OlmEngine 開、只有 crypto store，一般 Matrix 帳號還有 Client 的 state store）
      k/  r/                   本地房間金鑰快照、recovery key（每帳號）
```

這張圖決定了併發的形狀：

| 檔案 | 誰會同時碰 | 有沒有競爭 |
|---|---|---|
| `m/`（matrix-sdk store） | **只有那一個帳號**的上游會話 | ❌ 沒有。一帳號一個 store、一個 writer |
| `session.sealed`／`k/`／`r/` | 那一個帳號，而且只在登入／登出／備份時 | ❌ 沒有 |
| **`cache.db`** | **同一個 server 上每一個登入中的帳號**，同時 | 🚨 **有**，見 §2 |
| 媒體池 | 任何帳號的下載 | 🟡 有，但池的格式是「一檔一鎖、順序 append」（media-pool），本來就設計成多寫入者 |

## 2. 誰寫 `cache.db`

### 2.1 為什麼要單一寫入者

- `Cache::open` 每次呼叫都開一條新的 SQLite 連線，而 SQLCipher 每次開都要重導金鑰（PBKDF2，不是免費的）。
- `PRAGMA journal_mode = WAL`（多讀一寫），`foreign_keys = ON`。
- ⚠️ 兩條各自的連線同時寫**不會**立刻 `SQLITE_BUSY`：`rusqlite` 開連線時自己就設了 `busy_timeout = 5000`（`inner_connection.rs`），它們是**排隊**。
  釘住這個假設的測試是 `two_raw_connections_serialise_instead_of_failing`（兩條連線、兩條執行緒、各 200 次寫，`database is locked` 0 次）；哪天 rusqlite 改掉那個預設，它會紅。

所以單一寫入者防的不是「撞鎖就失敗」，而是：

| # | 理由 |
|---|---|
| 1 | 🚨 **等的時候是同步阻塞**。`upsert_*` 是 blocking 呼叫，卡在 async task 裡就是**卡住一條 tokio 工作執行緒**，最壞 5 秒。⭐ 所以寫入者要跑在**自己的 OS 執行緒**上，🚫 不在 runtime 的工作執行緒上 |
| 2 | 🚨 **順序**。兩批事件誰先 commit 決定水位（`cg_seq`）落在哪，而搶鎖的順序 ≠ 收到的順序 → 水位可能**倒退**。一條 queue 從根本解決，🚫 不必在每個寫入點做 `max()` 防禦 |
| 3 | **成本**。每個操作都 `Cache::open` 要付一次 SQLCipher 導金鑰。連線留著重用 |
| 4 | 那 5 秒真的用完時**仍然會失敗** —— 單一寫入者連這個尾巴也消掉 |

📎 為什麼 daemon 的排他鎖（/docs/design/overview/architecture-v2.md §0.2）不管這件事：那把鎖擋的是**別的程序**。
同一個 daemon 裡面的兩個 task 都在鎖的**裡面**，它一個字都沒說。

### 2.2 定案：一個 server 一個寫入者

```
帳號 A 的上游會話 ──┐
帳號 B 的上游會話 ──┼──> 這個 server 的 **cache 寫入者**（唯一）──> cache.db
UI 觸發的寫（已讀…）─┘
```

- **每個 server dir 一個寫入者**，前面掛一條 **queue**（維護者 2026-09-13：「一樣 db queue 的概念，
  **無限長，沒有 limit**」）。要寫的人把工作丟進去就走，🚫 不必等別人寫完。
- 🚨 **寫入者跑在自己的 OS 執行緒上**，🚫 不是 `tokio::spawn`：SQLite 的寫是**同步阻塞**的
  （拿不到鎖會等，最壞 5 秒），擺在 runtime 的工作執行緒上就是卡住別人的 future。
  📎 順帶的好處：`ServerCache::open` 因此🚫 **不需要** tokio runtime，CLI 那種一次性的用法也能開。
  ⭐ 重點是**寫入者只有一個**，🚫 不是「大家各開一條連線然後靠 SQLite 去擋」。
- ⚠️ **無上限的 queue 要有代價的自覺**：寫得比收得慢的時候，那些工作會累積在記憶體裡。
  🚫 不加上限是刻意的（丟掉一則已經收到的事件比慢更糟），但**要看得見**：queue 長度在 `daemon.info` 的 `cache_queue`。
  漲到 `QUEUE_WARN_AT`（1 萬件）的那一次發一則 `Note` 說出來（邊緣觸發：退回去再漲上來才會再發）。
- **讀不走那個寫入者**：讀各自開唯讀連線，WAL 本來就允許「一個寫、多個讀」。
  ⚠️ 讀連線也要付 SQLCipher 的開檔成本，所以要**留著重用**，🚫 不要每個請求開一次。
- **`busy_timeout` 已經有了**：`rusqlite` 開連線就設 5 秒（§2.1），所以跨程序的情況
  （單發命令、未來的唯讀工具）本來就是等而不是失敗。🚫 **不用自己再設一次**。
- 🚨 **「只有一個」是靠註冊表的鎖成立的，🚫 不是靠時序**：
  `Core::server_cache_of` 把註冊表的鎖**握滿「查、開、放進去」整段**。
  ⚠️ 查完就放掉鎖、開完再 `or_insert` 是**錯的**：後到的那個**已經開過庫、已經起過寫入者**了，
  而第一次開同一個檔的那段（建表）是排他的、`busy_timeout` 救不了——八條執行緒同時要同一個 server 時，大部分呼叫端直接 `database is locked`。
  📎 代價是開庫期間別的呼叫端會等 —— 它們等的本來就是同一份東西。
  🚫 **`Arc::ptr_eq` 驗不出這件事**（輸家拿到的就是贏家那份，位址相等），所以
  `ServerCache` 另外按目錄記「起過幾條寫入者」，測試斷言的是那個數字。
- **寫入的粒度是「一批」**：`Recent` 的一個 `Batch`、一次推送進來的一組事件，
  🚫 不要一則事件開一個交易（那是 N 倍的 fsync）。

### 2.3 「要等它真的落地才回」怎麼辦（維護者 2026-09-13 提的問題）

> **「有些東西需要真的落地才會回 ack……需要事後處理的，你怎麼確定落地這件事，
> 感覺有點複雜，這裡不優雅不乾淨。」**

問題是真的：丟進 queue 就走的東西，**呼叫者不知道它成功了沒**。而 `room.read`、`room.send_text`
這種要回一則 `{ ok: true }` 的，🚫 不能在還沒寫進去的時候就說好了。

**做法：同一條 queue，工作可以附一張「回執」。** 🚫 不是兩套機制，是一套機制加一個可選欄位。

```rust
struct Work {
    /// 真正要對資料庫做的事。
    apply: Box<dyn FnOnce(&mut Cache) -> Result<Done, CoreError> + Send>,
    /// 有人在等就帶著；沒人等就是 `None`。
    receipt: Option<oneshot::Sender<Result<Done, CoreError>>>,
}
```

呼叫端只有兩個入口，名字就說得出差別：

| 入口 | 語意 | 誰用 |
|---|---|---|
| `writer.post(work, emit_after_commit)` | **丟進去就走**。仍然照順序執行，只是沒人等；commit 成功之後由寫入者發 `emit_after_commit` | 沒人等結果的背景寫（CLI 的 `watch`） |
| `writer.run(work).await` | 丟進去**並等它 commit**，拿回結果或錯誤（同步版 `run_blocking`：`Recent` 的收批回呼是同步的） | 任何要回應 RPC 的寫入；訂閱線收推播（`room_sync.rs`：等 commit 再發 `room.message`，§4.1）；`sync.recent` 的每一批 |

- ⭐ **回執是 `oneshot`，🚫 不是一個 `job.complete` 事件。** 差別是決定性的：
  事件要對號（誰的完成？）、可能因為慢而被丟掉（廣播會 lagged）、而且錯誤沒地方放。
  `oneshot` 三件事都不會 —— **收得到、對得上、錯誤原樣回來**，而且型別逼你處理。
- ⭐ **一條規則就講完該用哪個**：**回應的內容取決於這次寫入的，就 `run().await`**；
  其餘 `post`。🚫 不要靠「這個 method 感覺比較重要」去猜。
- ⚠️ **水位的正確性靠順序**：`sync.recent` 每批事件各一個工作，全部 commit 之後才推水位（`cg_seq`，`sync_ops.rs` 的 `pull_recent`）；
  中途失敗水位不動，下次從舊水位重拉（冪等）。所以🚫 不會出現「事件沒寫進去但水位前進了」那種洞。推播那條路🚫 不碰水位（§4.3）。
- 📎 呼叫者被取消（§8）不會取消已經排進去的寫入：它照樣落地。⭐ 這是對的 ——
  資料庫🚫 不該因為「發問的人走了」就留下半套。

**「落地」的定義**：SQLite 交易 commit 成功。WAL 模式下這代表**程序當掉也不會丟**
（`kill -9`、panic 都不會）；只有作業系統當機或斷電才可能丟掉最後幾個交易（`synchronous = NORMAL`）。
⚠️ 如果哪天需要「斷電也不丟」，那是把 `synchronous` 調成 `FULL` 的決定，代價是每次 commit 一次 fsync
—— 🚫 現在不調，但要知道那個旋鈕在哪。

**為什麼不用更簡單的 `Mutex<Cache>`**（考慮過，記下來免得有人再想一次）：那樣連 queue 都不用，
每個寫入者自己搶鎖、自己拿結果，確實更少零件。🚫 不採用是因為**背景寫入會失去順序**：
上游會話 `spawn` 兩批事件出去，它們搶鎖的順序不保證是收到的順序，於是水位可能**倒退**
（後commit的那批帶著比較舊的 `cg_seq`）。要修就得在每個寫入點做 `max()` 防禦 ——
⭐ 那才是真的不乾淨：把一個順序問題散成 N 個地方的防禦。queue 是**一個地方**解決它。

### 2.3.1 媒體也走寫入者：下載是一塊一步，DB 只在點上碰

`media`、`event_media` 跟其他表一樣只由寫入者寫。能這樣做，是因為下載的形狀（/docs/design/media/media-download.md §5.4）：
寫池檔都在那個帳號的下載處理端自己的 task 上（網路是 `Download` 線的發送端，/docs/design/daemon/link-requests.md），**DB 只在幾個點上碰一下**——

| 時機 | 入口 | 為什麼 |
|---|---|---|
| job 開檔：`media_begin` ＋ 拿暫存名 | `run` | 回答（暫存名）取決於這次寫入 |
| 每 1.5 秒：`media_progress`（段數，只給顯示） | `post` | 丟了只是顯示慢一點 |
| 收尾：`media_finish` | `run` | 之後才放掉認領、才叫醒等它的人（/docs/design/media/media-download.md §5.1 那條順序） |
| 壞檔：`media_reset` | `post` | |
| `media.gc`：`sweep` ＋ `collect_garbage` | `run`（一整件） | 掃描要的是一致的那一刻：列與池檔對照的中間不准有別人改列 |

⚠️ 代價說實話：`media.gc` 那一件會在寫入執行緒上跑完整個掃描（列與 `pending/`、`media/<hh>/` 的目錄、刪檔）；池很大的時候，這段時間同一台 server 的新訊息排在它後面。
它是使用者按的（或一個程序裡每台 server 第一次起下載處理端時一次），🚫 不在背景定時跑。哪天池大到這件事看得出來，再把「列出要刪的」與「刪檔」拆開。

讀（`find_media`、`find_media_block_for`、`find_event_attachment`）走讀連線，跟其他讀一樣。🚫 正式碼沒有繞過寫入者直接開 `cache.db` 的路（`Core::cache_of` 只在測試建置存在）。

### 2.4 為什麼 `m/` 不需要這一套

matrix-sdk 的 store 是**每帳號一份**，而一個帳號只有一台 OlmMachine 在寫它（wbf 帳號是那個帳號長活的 `OlmEngine`）。
🚫 所以不要為了「對稱」也給它包一層 —— 沒有第二個寫入者的東西不需要序列化。

⚠️ 但有一個例外要記著：**同一個帳號的兩件事同時跑**（例如收金鑰的 task 在匯入，同時使用者送一則加密訊息）
仍然共用那個 store。matrix-sdk 自己處理這件事（它內部有鎖），我們🚫 不要在外面再包一層去猜它的鎖。

### 2.5 要驗的（🚫 不靠推測）

`server_cache.rs` 的單元測試守著：
- **回執真的等到了**：`run()` 回來之後，**另一條連線**立刻讀得到那筆（🚫 不是「大概好了」）。
- **`post` 的順序**：連續丟 N 件，寫進去的順序與發出的事件順序都跟丟的一樣。
- **兩個帳號、同一個房間、同一批事件同時灌**：🚫 不出現 `database is locked`，
  而且兩個帳號各自都讀得到全部（混存但不混視野）。
- **queue 長度看得見**、跑完歸零。
- ⭐ **釘住那個假設**：兩條各自的連線同時寫會**排隊**（`rusqlite` 的 5 秒 `busy_timeout`）——
  哪天那個預設變了，這條會紅（§2.1）。

還沒驗：
- 兩個帳號登在同一台 server，同時灌事件，**跑很久**（幾分鐘）：不能出現 `database is locked`。
- 一邊寫一邊讀（UI 在捲歷史、上游在寫新事件）：讀不能被餓死，也不能讀到半個交易。
- 殺掉 daemon（`kill -9`）之後重開：WAL 要能自己回復，🚫 不能留下壞掉的 `cache.db`。

## 3. UI 拿東西走哪條路：`sync` 這個參數

### 3.1 大方向：RPC 是對**本地資料庫**的呼叫（維護者 2026-09-13 定）

> **「rpc 呼叫大部分都是對 local db 的呼叫，是更加抽象、更加高階的。
> rpc-server 收到 UI 來的 sync，其實就是需要 UI 說明白，是什麼的 sync？」**

所以🚫 **不改語意、也不砍掉上游那條路**，而是**補一個參數**讓呼叫者說清楚要哪一種：

```jsonc
{ "method": "room.list", "params": { "user": "@a:localhost", "sync": "local" }, "id": 1 }
```

| `sync` | daemon 做什麼 | 寫 `cache.db` 嗎 | 什麼時候用 |
|---|---|---|---|
| **`local`（預設，沒帶就是它）** | 只讀 `cache.db` | ❌ | **常態**。UI 顯示東西都走這個 |
| `server` | 打上游，拿到最新結果**直接回傳** | 🚫 **不寫** | 「我要看 server 現在到底怎麼說」——除錯、對帳 |
| `both` | 打上游 → **寫進 `cache.db`** → 再從本地讀一次 → 回傳 | ✅ | 使用者按「重新整理」、或 UI 知道本地那段有洞 |

- ⭐ `both` 回傳的是**本地讀的結果**，不是上游的原始回應 —— 這樣它跟 `local` 的形狀一模一樣，
  UI 🚫 不需要為兩種模式寫兩套解析。
- ⚠️ `server` **刻意不寫庫**：它是「看一眼」，🚫 不是「同步」。要同步就用 `both`。
  📎 分開的理由：對帳的時候你要看得到「上游說 A、本地存的是 B」，如果 `server` 順手把 B 改成 A，
  那個差異就永遠看不到了。
- 🚫 **不做 `sync: "auto"`**（「本地有就本地、沒有就上游」）：那會讓同一個呼叫的延遲從毫秒跳到秒
  而 UI 無從預期。要不要打上游是**呼叫者的決定**。

### 3.2 哪些 method 要補這個參數

判準（維護者 2026-09-13）：**本地快取有的都要這個模式**。

| method | 補嗎 | 為什麼 |
|---|---|---|
| `room.list`、`room.get` | ✅ | `room_list` 表就是它的本地版 |
| `room.history`、`room.files` | ✅ | 跟其他讀命令同一個 `sync`，🚫 不另外有 `source` 這種第二個名字講同一件事 |
| `media.info` | ✅ | ⭐ **媒體不可變**：`file_size`／`chunk_size`／`mimetype` 上傳完就不會變，本地 `media` 表存的就是同一份事實 —— 🚫 沒理由為這些跑一趟 server（維護者 2026-09-13）。⚠️ `total_len`／`truncated`／`description`／`verified` 只有問過 server 才有，`local` 時**讓它們不在**，🚫 不編造；反過來 `cached`（下載到哪了）是**上游答不出來**的 |
| 讀已讀位置（§6） | ✅ | `read_positions` 有 |
| `account.list`、`recovery.list`／`show` | ❌ | **已登入的帳號 always local**：那是這台機器的檔案，不是快取，🚫 沒有「上游版本」可言 |
| `sync.recent` | ❌ | 它**本身就是**上游拉。加 `sync=local` 沒有意義 |
| `server.ping`、`backup.*` | ❌ | server 端狀態，本地沒有那份東西 |
| `room.send_*`、`upload.*`、`account.add`／`del` | ❌ | 動作不是查詢 |

### 3.3 UI 的每一個動作實際發生什麼

| UI 做什麼 | RPC | 上游 |
|---|---|---|
| 開 app、顯示帳號列表 | `account.list` | ❌ |
| 顯示房間列表（全部帳號） | `room.list`（每個帳號一次） | ❌ `sync=local` |
| 列表上看得到、還沒名字的房間（`refreshed_at` 是 `null`） | `room.get { sync: "both" }`（一間一次，/docs/design/rooms/chat-model.md §2.1） | ✅ 只問那一間，寫回庫 |
| **點開一個房間** | `room.history { sync: "local" }` | ❌ |
| 使用者按「重新整理」 | `room.list { sync: "both" }` | ✅ 只問加入了哪些房 |
| 房間內往上捲，捲到快取的盡頭 | `room.history { sync: "both", before }` | ✅ 逐房 backfill，寫回庫 |
| 收到新訊息 | —（推播 `room.message`） | ✅（背景，上游會話） |
| 送一則訊息 | `room.send_text` | ✅ |
| 標記已讀 | `room.read`（§6） | 🟡 看 `sync` |
| 下載附件 | `media.export_to`／`media.open` | 🟡 命中媒體池就不用 |
| 切換「現在看哪個帳號」 | **不用 RPC** | ❌ |

🚨 最後一列是重點：**「UI 現在在看哪個帳號」是 UI 自己的狀態**，🚫 不是 daemon 的。
daemon 這邊 `account.switch` 只決定「沒帶 `user` 的命令預設對誰」，
而多帳號的 UI **每個命令都該明確帶 `user`**，🚫 不要依賴那個預設。

### 3.4 「點開房間才 sync」怎麼落地

維護者 2026-09-13：*「等點開房間才會去跑 sync 和拿房間資訊。」*

- **點開房間 = 先 `sync: "local"`**（立刻有東西看），**之後**才視情況補洞。
  🚫 不要「先去 server 拉完再顯示」——那是把毫秒變成秒。
- 有沒有洞是**看得出來的**：`cache.db` 的 `events` 有 `r_seq`（房內序號），
  一段連續的 `r_seq` 中間缺號就是洞（/docs/design/storage/local-cache-db.md §5）。有洞才發第二個 `sync: "both"`。
- 補洞的範圍是**那一個房間**，🚫 不是全域 `Recent`。全域 `Recent` 是 UI 起來時另外叫的 `sync.recent`（§4.3），🚫 不是 daemon 自己的事。
- 🚨 **往回翻一律拿 `event_id`**（維護者 2026-09-14）：UI 拿手上最舊那則當 `before`，`both` **永遠問上游**、
  寫進去、照上游順序從本地讀回。daemon 換算：wbf 查本地 `g_seq` → `Recent{rooms, before}`；matrix `/context` → `/messages`。
  🚫 一般 Matrix 房（沒有 `r_seq`）`local` 不答。細節與理由在 /docs/design/rpc-specs/rpc-spec.md §3.3「往回翻」。

### 3.5 ⚠️ 跟這個模型不同的地方：CLI，與 backend 怎麼選

- ⚠️ **CLI 刻意不走 `local` 預設**：`rooms` 一律 `Both`；`read`／`files` 的 `--from-cache` → `Local`，沒帶 → **`Both`**（打上游＋寫穿快取）。
  CLI 沒有常駐的東西可以依賴，每個命令自己去 sync 是唯一選擇。

**backend 怎麼選**（維護者 2026-09-13 定）。🚨 **`transport` 就是選 backend，🚫 不是「wbf 底下再挑一條管子」**：

| `transport` | 協議 | 誰實作 |
|---|---|---|
| **`ws`**（**沒帶就是它**） | wbf 客製協議 | `wbf-sdk`（`BackendKind::WbfSdk`） |
| **`http`** | 原生 Matrix HTTP | `matrix-sdk`（`BackendKind::MatrixSdk`） |

🚫 **wbf 協議一律 WS**，底下不再分。pack-over-HTTP 只剩 debug 用途，🚫 不是「wbf 的 HTTP 模式」。

```text
http ─────────────────────────> matrix-sdk（永遠）
ws ──┬── 這台不講 wbf ────────> matrix-sdk（🚫 不報錯，那是 no-op）
     ├── 這個 method 還沒有 ws ─> matrix-sdk（🚧 暫時清單）
     └── 其他 ────────────────> wbf
```

🚨 **沒帶 `transport` 就是 `ws`**，所以**預設走的就是那條分岔**：對方講 wbf 就用 wbf，
不講就 **fallback 到 matrix-sdk** —— ⚠️ 兩種都🚫 不報錯。
📎 那份預設只有一個地方寫著：`Transport::default()`（`wbf-sdk` 的 `channel.rs`）。
🚫 daemon 的 `Settings` 不自己寫死一份 —— ⭐ 同一個預設有兩個地方決定，遲早只有一邊被改到。

**`wbf_core::backend_choice` 的三塊**：

1. **探測** `Core::get_backend_kind` —— 規矩在 /docs/design/daemon/account-session.md §1–§2：不帶 token 的 WS `Hello`、以 server URL 為鍵、探不到不記；
   wbf 帳號登入時就把 backend 記進 session，之後不再探。
   ⭐ 連不上／不回／看不懂一律 `MatrixSdk`，所以它**不回 `Err`**：探測失敗不是錯誤，是一個答案。🚫 不寫進磁碟（那是 server 那邊的事實，它會變）。
2. **規則** `get_backend_for(transport, server_speaks_wbf, home)` —— 純函數，所以上面那張表
   逐格測得到。⚠️ 只有一種情況報錯：那個 feature 只有 wbf 有，而這條路到不了它 ——
   它就是**關的**，而「因為你選了 http」跟「因為對方不是 wbf」訊息分開講。
3. **唯一的閘門** `Core::client_of(account, transport, home, role)` —— 探測＋規則＋從連線池拿線（/docs/design/daemon/link-pool.md §2）都在這裡。
   （探測不能走閘門，不然它會叫到自己。）

**`MethodHome`**：每個呼叫點自己說出它住在哪一邊 —— 🚫 不是一串字串比對（名字跟實際走哪條會漂移）。
`room.history`／`files` 是 `BothSides`；`sync.recent`／`upload.*`／`media.*`／`server.ping` 是 `WbfSdkOnly`；
`room.list`／`get`／`send_text` 先看 `session.backend`（`is_wbf_account`）分流：wbf 帳號那半（`wbf_rooms.rs`，走橋，/docs/design/daemon/account-session.md §6）是 `WbfSdkOnly`，一般 Matrix 帳號走 Client（`synced_backend_of`）。
🚧 暫時清單（`StillOnMatrixSdk`）現在沒有正式呼叫點。

⭐ rpc-spec 看不到 backend 怎麼選 —— 那正是 `sync` 這個參數的價值：它講的是「要不要去問上游」，🚫 不是「用哪個協議去問」。
上游那條路一行都不少，只是從「唯一的路」變成「說出來才走的那條」。

📎 **代價**：每個 server 第一次用到 wbf 那條路時會多一次 `Hello`（探測自己開一條 WS）；探到答案之後每個 server 在一個 daemon 生命週期裡只探一次。
⚠️ 但**探不到**（一般 homeserver 沒有 WS 端點）那次不會被記住，所以之後每個用到 wbf-only 功能的
呼叫都會再試一次 handshake。⭐ 可以接受 —— 那些呼叫本來就會失敗（那個功能在那台 server 上是關的），
而記一個錯的結論會讓**能動的**帳號也不能動。

## 4. 上游會話：多帳號怎麼落到資料庫

### 4.1 一個帳號一組會話

誰開、什麼時候開關：wbf 帳號是連線池的五條線，解鎖／登入後 daemon 全開、背景看著（/docs/design/daemon/link-pool.md §1、§3.1）；
一般 Matrix 帳號沒有常駐的上游會話（§5.5）。執行期要補的是**事件進來之後的順序**：

```
事件進來 ──> 解密 ──> 交給那個 server 的寫入者（§2.3）
                                    │
                        commit 成功之後才發 CoreEvent::Message
                                                        ↓
                                            各連線的訂閱過濾 ──> room.message
```

🚨 **先寫庫、後發事件**：前端收到推播八成馬上去查（捲到那一則、更新未讀數），
反過來的話它查到的是還沒有那則訊息的資料庫。

兩種寫法都守這條：
- `post(work, emit_after_commit)`：事件跟著工作一起交出去，**由寫入者**在 commit 成功之後發；失敗就一則都不發（CLI 的 `watch`）。
- `run(work).await`：等到回執、成功了呼叫端才發（訂閱線 `room_sync.rs`）；寫失敗只講一聲、不發。

🚫 不要 `post` 完就自己發：`post` 是丟進去就走，事件會**跑在 commit 前面**。
⭐ 所以「順序對不對」不是每個呼叫點的紀律：想在不等的情況下發事件，就得把它交給寫入者。

### 4.2 兩個帳號在同一台 server 上

- **事件各自進來、各自解密**（金鑰是每帳號的），但**寫進同一個 `cache.db`**。
- 事件本體（`events` 表）是**共用的**：同一則訊息 A 與 B 都收到，庫裡只有一列。
- **「誰看得到」是另一張表**（`events_synced_log`，/docs/design/storage/local-cache-db.md §5）：
  一則事件對每個看得到它的帳號各有一列。⭐ 這就是「混存但不混視野」的機制。
- 所以 §2.2 那個「一個 server 一個寫入者」不只是為了避免撞鎖，
  也是因為**兩個帳號寫的是同一批列**（`rooms`、`users`、`events` 都要 upsert）。

### 4.3 補洞是 UI 的事（維護者 2026-09-23；/docs/design/rooms/room-sync.md §0 原話）

**daemon 不自己叫 `Recent`**（「daemon 只有訂閱新事件，不主動叫 Recent。誰負責記有沒漏，是 UI 層的事」）：

- daemon 開訂閱線就只訂（`init_connection`），推播一包寫一包、🚫 不碰水位。
- UI 起來時叫 `sync.recent`（帶 `since`＝它自己記的起點，或不帶就用 daemon 存的上一次水位）；回應 `caught_up` 是 false 就再叫。
  什麼時候補、補到哪、漏了要不要管，全是 UI 的事（推播漏掉的 UI 不叫就不補）。
- `sync.recent` 是**唯一**動水位（`cg_seq`）的地方；🚫 沒有 daemon 自己的排程、沒有「追平了」的事件（`sync.state` 的 variant 留著，沒人發）。

## 5. 事件送給誰：多帳號的扇出

### 5.1 規則：homeserver 來的一律帶 `user`

維護者 2026-09-13：*「幾乎每個 event 都要放這是誰的帳號的事件，除非是 daemon 本身的控制流。」*

| 推播 | 帶 `user` 嗎 |
|---|---|
| `room.message` | ✅ |
| `sync.state` | ✅ |
| `link.state`、`keys.state`、`pack.received`（/docs/design/daemon/link-pool.md §6） | ✅ |
| 之後所有從 homeserver 來的（已讀回條、輸入中、房間狀態變更、裝置驗證…） | ✅ **一律** |
| `progress`、`note` | ❌ 它們是**請求**的進度與說明，用 `id` 對得起來 |
| `vault.state` | ❌ 它是 daemon 這台機器的狀態，跟帳號無關 |

⭐ 判準一句話：**這件事是「某個帳號在它的 homeserver 上發生的」嗎？是就帶 `user`。**

### 5.2 UI 要不要收別的帳號的事件

**要，而且預設全收。** 使用者正在看帳號 A，帳號 B 來了訊息 —— UI 要能在側邊欄顯示未讀。
所以：

- `subscribe` 不帶 `user` ＝ **全部帳號**（/docs/design/rpc-specs/rpc-spec.md §3.9）。帶了才是只收那一個。
- 🚫 **daemon 不替 UI 決定「哪些值得看」**：它只送「發生了什麼」，
  要不要響、要不要跳紅點、要不要靜音某個帳號 —— 那是 UI 的事（§6）。

### 5.3 🚨 掉了事件要**講出來**

政策是「推播不保證看得到全部，掉了就重查」。那句話的前提是：
**UI 得知道自己掉了東西**，否則它永遠不會去重查 —— 它以為自己什麼都收到了。

- daemon 的每條連線各自從 core 的廣播收事件。收到 `RecvError::Lagged(n)` 的時候
  （這條連線讀太慢、被覆蓋掉 n 則），**必須送一則 `desync { missed: n }` 給 UI**（不帶 `user`：掉的是這條 RPC 連線上的推播，不分帳號）。
- UI 收到之後的動作是**重讀**（房間列表、開著的那個房間的最新一頁、未讀數）——
  ⭐ 全部都是本地讀，很便宜，所以這個補救是廉價的。
- 🚫 **不重播**（我們沒有留著那些事件），🚫 **也不假裝沒事**。

📎 同一條原則在上游那一側已經有了：server 推送掉包會標 `gap`（/docs/design/overview/architecture-v2.md §5.1.1）。
⭐ 我們自己的推播是同一個問題，🚫 沒有理由用不同的答案。

### 5.4 媒體的 bytes 不走 RPC，進度只有背景下載走推播（維護者 2026-09-13 定 bytes、2026-10-01 定背景下載的進度）

**媒體的 bytes 從來不經過 RPC 通道**，所以「一個 2 GB 上傳發上萬則 `progress`、把 `room.message` 擠掉」這個情境不存在。
上傳的 PUT 與讀的 GET 都做了（/docs/design/rpc-specs/data-plane.md）。**背景下載**沒有 HTTP 可看，進度走推播 `media.download`，
每個檔最多每秒一則＋狀態改變（/docs/design/media/media-download.md §5.5）——低頻，擠不掉訊息。

```
上傳：UI ──HTTP PUT bytes──> daemon ──切塊、加密──> homeserver
      ⭐ 進度 = UI 自己那個 HTTP 請求送出去多少，🚫 不是 daemon 推回來的數字

下載：UI ──HTTP GET──> daemon ──向 homeserver 要 chunk──> 邊拿邊吐給 UI
      ⭐ 進度 = UI 自己那個 HTTP 回應收到多少
```

- 🚨 **上游慢的時候，daemon 的 HTTP server 要「卡住」**：**停止送資料、但連線開著**，
  等拿到下一塊再繼續吐。🚫 **不要回一個空回應、也不要斷線** ——
  那會讓 UI 以為「傳完了」或「失敗了」，而事實是「還在等」。
- 所以 **RPC 通道基本上永遠是暢通的**：它上面只有一問一答的控制訊息與低頻的推播（含節流過的 `media.download`），
  🚫 沒有 bytes、🚫 沒有每塊一則的進度。§5.3 的 `desync` 仍然要有（慢的訂閱者還是會落後），
  但「進度把訊息擠掉」這個情境**不存在**。

⚠️ **例外：路徑版的 method**（`room.send_file`、`upload.file`）——
那是 daemon **自己讀本機檔案**去傳，UI 沒有 HTTP 可看，所以它們的進度只能走 RPC 的 `progress`。
📎 那是給 rpc-cli 與「Desktop 拖一個本機檔進來」用的路徑，頻率低、一次一個檔，
⭐ 所以**節流與分佇列先不做**（維護者 2026-09-13：「這個你可以先不做，先一步步來」）。
👉 哪天 RPC 上真的出現高頻進度（例如有人把資料平面接回 RPC），再回來看這一節。


### 5.5 HTTP 那條路也要寫進 `cache.db`

還沒做：一般 Matrix 帳號沒有常駐的收事件迴圈；它們的快取只在 `sync=both`（與 CLI 的 `watch`）時寫。

⚠️ `sync: "local"` 讀的是 `cache.db`。走 **HTTP** 的帳號（一般 homeserver）事件是進
**matrix-sdk 自己的 store**（`m/`）—— 如果上游會話不把它們**鏡射**進 `cache.db`，
那些帳號的 `room.list`／`room.history` 用 `sync: "local"` 會是**空的**。

- 所以 HTTP 那條路的收事件迴圈，最後一步跟 WS 那條**一樣**：交給 §2.3 的寫入者。
  ⭐ 兩條路只有「事件從哪來」不同，**落地的路徑只有一條**。
- 🚫 **不要讓 `sync: "local"` 對 HTTP 帳號改讀 matrix-sdk 的 store** —— 那就變成兩個真相來源，
  而 `room_list`／未讀／`events_synced_log` 那些我們自己的表在那邊根本不存在。

## 6. 通知（notification）不在 rpc-spec 裡

維護者 2026-09-13 想過要不要有「開啟通知」這個開關，結論是**不要**：

| 平台 | 長連線 | 通知從哪來 |
|---|---|---|
| **Desktop** | 一直開著（daemon 跟 UI 都在同一台機器上） | 就是這條長連線的推播；要不要跳通知是 UI 決定 |
| **Android** | **只在 app 活著的時候**；app 關了連線就關 | **FCM**，那是完全另一條管道（Google → app） |

- 🚫 **daemon 不需要知道平台**，也不需要一個「通知開關」：它只負責「有一則新訊息」這個事實，
  而那個事實在兩個平台上都一樣。
- ⭐ 差別在**誰在聽**：Desktop 有一條永遠在的連線，Android 沒有。
  Android 背景收不到推播不是 daemon 的問題（那時候根本沒有 daemon 在手機上跑）。
- 📎 FCM 那條路要怎麼接（誰去註冊 token、server 端怎麼推）**不在這份文件裡**，
  它是 Android 那支 UI 加上 server 的事。

## 7. 進度歸誰：`job`

長工作的進度必須說得出是哪一個請求的（/docs/design/rpc-specs/rpc-spec.md §4 的 `progress.id`），可是 core 不知道
「請求」這種東西，而三個長工作的事件在同一條廣播上是交錯的。

做法是 `crates/wbf-core/src/job.rs`：daemon 把整個工作包在 `run_as_job(請求的 id, …)` 裡，
`EventSink` 發事件時自己去問「現在在哪個工作裡」（tokio 的 task-local）。

- ⭐ **一個地方標、一個地方讀**，中間十幾層一個字都不用改。
  🚫 不走「每個長工作多收一個 `job` 參數」：那要改十幾個公開簽名，而中間任何一層忘了往下傳，
  事件就默默變成無主的 —— 那種漏法不會有人發現。
- ⚠️ 限制：task-local 不跟著 `tokio::spawn` 走（模組註解裡有，**有測試釘住**）。
- **`job: None` 的事件🚫 不自動推給哪條連線**：背景工作（收推播、看線的迴圈）講的話不屬於任何請求，只有訂了 `progress`／`note`（或 `"*"`）的連線收得到（/docs/design/daemon/link-pool.md §6）。

## 8. 取消

還沒做（§10 第 5 階段）；下面是定下的形狀。

`cancel { id }` 停掉的是**這條連線上**那個還在跑的請求。

```rust
let response = tokio::select! {
    response = handle.call(request) => response,
    _ = cancelled => Response::error(id, code::CANCELLED, "cancelled"),
};
```

- ⭐ 用 `select!` 而不是 `JoinHandle::abort()`：取消之後**還要回一則 `105`**，而被 abort 的
  task 講不出話。`select!` 落選的那一邊會被 drop —— 對 async 來說那就是真的停下來。
- **已經回完的 `id` → `{ ok: true, was_running: false }`**，🚫 不是錯誤。
- ⚠️ 取消留下的東西，各自不同：

  | 工作 | 取消之後 |
  |---|---|
  | 上傳 | 狀態檔還在（/docs/design/rpc-specs/wbf-cli-spec.md §6），可續傳；🚫 不自動去 `abort` server 那邊 |
  | 下載 | 媒體池的段是可續的；`no_cache` 的直接下載刪掉半個檔 |
  | `sync.recent` | 水位只在整批寫完之後前進，所以🚫 不會留下「假裝拉過」的洞 |

## 9. 失敗與邊界

| 狀況 | 行為 |
|---|---|
| vault 鎖著 | 🚫 不起上游會話（token 在 `session.sealed` 裡，讀不到） |
| 沒有寫權（別的 daemon 佔著） | 🚫 不起上游會話；會寫的 method 回 `109` |
| 上游斷線，但有請求正在跑 | 那個請求照它自己的錯誤路徑失敗（`1300`），🚫 不掛在那裡等重連 |
| 前端關掉連線 | 訂閱沒了；**它發起的長工作繼續跑**（/docs/design/rpc-specs/rpc-spec.md §1.2），進度沒人收就沒人收 |
| `cache.db` 寫失敗 | 推播那包：講一聲、🚫 不發 `room.message`、不重試（水位不歸推播管，UI 下次 `sync.recent` 重拉，§4.3）。`sync.recent` 的一批：整次回錯、水位不推進，下次從舊水位重拉 |
| daemon 關閉 | 先停上游會話 → 等在跑的請求收攤 → 關 listener |

## 10. 分階段

| 階段 | 內容 | 狀態 |
|---|---|---|
| 1 | core 的事件形狀（`Note`／`Progress`／`Message`／`SyncState`）＋ `job` | ✅ |
| 2 | **`cache.db` 的單一寫入者**（`wbf_core::server_cache`）：一個 server 一個寫入**執行緒** ＋無上限 queue ＋`post`／`run` 兩個入口（§2.3）＋讀連線重用，含併發測試（§2.5） | ✅ 媒體也走它（§2.3.1） |
| 3 | **`sync` 參數**（§3）：`room.list`／`get`／`history`／`files`／`media.info`，預設 `local`，**回應回報這次用了哪一種** | ✅ |
| 4 | daemon 的訂閱、推播封裝、`progress` 自動路由、**`desync`**（§5.3）；SDK 的收包分派（/docs/design/daemon/ws-receive-dispatch.md）；daemon 那半在 /docs/design/daemon/link-pool.md §6 | ✅ 還沒做：兩條佇列分開＋進度節流（§5.4 定了先不做） |
| 5 | `cancel`（§8） | 還沒做 |
| 6 | sdk 的 `Event/Subscribe`（`0x04`）／`Unsubscribe`（`0x05`）／`Push`（`0x06`）與 core 的 `room_sync.rs`（/docs/design/rooms/room-sync.md） | ✅ |
| 7 | 上游會話：探測、收事件迴圈、寫庫、發事件 | ✅ wbf 帳號（連線池 /docs/design/daemon/link-pool.md、收推播寫快取再發 `room.message`）。還沒做：一般 Matrix 帳號的收事件迴圈（§5.5） |
| 8 | 監督者：跟著解鎖／登入／登出起停，退避重連 | 線的那半有了（/docs/design/daemon/link-pool.md §3.1）。還沒做：task panic 收攤、重連時重探 backend |
| 9 | **已讀三層**（/docs/design/messages/read-receipts.md）：`room.read`、`READ_RECEIPTS` conf 鍵、`daemon.reload_conf` | 還沒做 |

## 11. 明確不做的

- 🚫 **不重播掉掉的事件**：推播是「不用輪詢」，不是「保證看得到全部」。掉了就重查（本地讀很便宜）。
- 🚫 **不把「這台 server 是 wbf」寫進設定檔**：探測結果只在記憶體、daemon 重開就重探（/docs/design/daemon/account-session.md §1）。wbf 帳號登入時記進 session 的 `backend` 是那個 session 的屬性，不是 server 的設定（/docs/design/daemon/account-session.md §2）。
- 🚫 **不做跨帳號的合併事件流**：每個帳號各自一組，要合併是 UI 的事。
- 🚫 **不在 daemon 裡做通知政策**（§6）。
- 🚫 **不做「UI 現在在看哪個帳號」的伺服器端狀態**（§3.3）。

## 12. 考慮過、沒走的路（2026-09-13 動手前的重新檢視）

維護者要求動手前跳出框架看一次。以下是**認真考慮過**的替代方案與不走的理由 ——
🚫 寫在這裡是為了讓下一個人不必再想一次，也讓「當初為什麼這樣」有答案。

### 12.1 直接用 matrix-sdk 的 store，不要自己的 `cache.db`

**為什麼誘人**：少一個資料庫、少一套 schema、少這一整章的併發設計。

🚫 **不走**，三個各自足夠的理由：

1. **wbf 那條路根本不經過 matrix-sdk**。`Event/Push`／`Recent` 的事件是我們自己收的，
   而 matrix-sdk 的 store 🚫 不是設計成「外面的人往裡面寫」。
2. **它是每帳號一份**，所以「同一個房間、兩個帳號」會存兩份，而我們的 `events_synced_log`
   正是為了不那樣。
3. **它的 schema 不是我們的模型**：`room_list`、未讀位置、媒體池的指針、`r_seq`／`g_seq`
   —— chat-model 定的那些概念在那邊不存在，硬塞就是在別人的表上蓋自己的房子。

📎 但**它還是要在**：裝置金鑰、Olm session、SSSS 都在 `m/`，那是 matrix-sdk 的職責，
🚫 我們不搬。**兩個庫各管各的**：`m/` 管密碼學狀態，`cache.db` 管聊天內容。

### 12.2 `cache.db` 改成一帳號一份

**為什麼誘人**：§2 那一整章（單一寫入者、queue、回執）**會直接消失** ——
一個帳號一個庫、一個寫入者，天生沒有競爭。刪帳號也變成 `rm` 一個檔。

🚫 **不走**（而且這是 2026-08 就定案的，/docs/design/storage/local-cache-db.md §5）：

- 同一個房間裡的兩個帳號會**各存一份**全部事件與媒體指針，而群組房間裡這是常態。
- 跨帳號的東西（搜尋、媒體去重）會從「一句 SQL」變成「N 個庫合併」。

⚠️ **但代價要說實話**：換來的是這一章的複雜度，以及「一個庫壞掉，這台 server 上所有帳號一起壞」。
⭐ 判斷是：單一寫入者是**一個地方**的複雜度（約一百多行、可以測），而合併 N 個庫是**每個查詢**的複雜度。

### 12.3 用 SQLite 當事件匯流排（UI 輪詢變更表）

**為什麼誘人**：不用推播、不用訂閱、不用處理 lagged。

🚫 **不走**：UI 🚫 **沒有** `cache.db` 的存取權（它連金鑰都沒有，那是整個 daemon 架構的前提），
所以「輪詢一張表」對它來說仍然是 RPC —— 只是把推播換成了輪詢，延遲與耗電都更差。

### 12.4 讓 core 直接吐推播，daemon 只是轉發

**為什麼誘人**：少一層翻譯。

🚫 **不走**：`user` 是 core 知道的，但 `id`（哪個請求）、訂閱、加密、每條連線的過濾都是 daemon 的。
core 一旦認識「連線」與「請求 id」，/docs/design/overview/architecture-v2.md §6 那條「公開面只有可序列化 DTO」就破了 ——
⭐ 而那條是 uniffi／FFI 那條路的前提。

### 12.5 這次檢視改掉的四件事

結論已寫進各節：先寫庫、後發事件（§4.1）；`Lagged` 要送 `desync`（§5.3）；媒體的 bytes 不走 RPC、背景下載的進度是節流過的推播（§5.4）；HTTP 帳號也鏡射進 `cache.db`（§5.5）。
📎 教訓：**把兩條規則寫在同一份文件的不同章節，不代表它們相容**。

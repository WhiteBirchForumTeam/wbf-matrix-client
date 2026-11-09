# 收攤：task panic、關機、取消（草稿，等維護者點頭）

> 這份回答：daemon 裡的一個 task 意外結束、daemon 要關、UI 要取消一個請求時，各自該發生什麼，現在發生的又是什麼。
> 對應 /docs/handover.md §7 第 3 點（daemon 穩健性）與 /docs/design/daemon/daemon-runtime.md §10 的第 5、8 階段；本 repo issue #26 的 daemon 那半也在這裡。
> 全域規則 P 條（正式碼不用會讓整支程式死掉的方法）管「不要 panic」；這份管「真的 panic 了（多半是第三方的碼）之後，失敗要有去處」。

## 0. 一句話

每個長活的 task 意外結束時要**講一聲、把自己佔的東西放掉**，讓既有的「看到不在就重開」那條路接手；RPC 請求 panic 要回 `108`、不要讓 UI 永遠等；
daemon 關機要先讓在跑的東西收完再丟掉 runtime；`cancel { id }` 照 /docs/design/daemon/daemon-runtime.md §8 已經定下的形狀做。

## 1. 現況：五個洞（2026-11-09，main `4163df5`）

| # | 情境 | 現在發生什麼 | 證據 |
|---|---|---|---|
| 1 | 一個 RPC 請求的處理途中 panic | 那個 task 被 tokio 吞掉：**沒有回應**、UI 一直等；`in_flight` 裡那個 id 不清（之後它的 `progress` 還會被當成這條連線的） | `crates/wbf-daemon/src/server.rs` 的 `serve_connection`：請求 `tokio::spawn` 之後只有正常路徑會回、會清 |
| 2 | 房間線、金鑰線的收推播 task panic | 正常結束時 task 會自己 `close` 那格線，看線迴圈下一輪重開重訂；**panic 跳過這一步**：socket 還活著、訂閱已經死了，看線迴圈看到「開著」就不動——這個帳號**靜靜地收不到推播**，直到線因為別的原因斷 | `crates/wbf-core/src/room_sync.rs`、`key_sync.rs` 的 `run`；/docs/design/daemon/link-pool.md §3 |
| 3 | 下載處理端 panic | `DownloadHandler` 的 `Drop` 把 `stopped` 設成 true，而 `Core::downloader_of` 只要表裡有就回那一個——**這個帳號之後的下載全部回「已經停了（登出了？）」**，直到重新登入 | `crates/wbf-core/src/download_queue.rs` 的 `Drop for DownloadHandler`；`media_ops.rs` 的 `downloader_of` |
| 4 | daemon 關機（`daemon.shutdown`） | `RpcServer::run` 一回來 `main` 就結束、runtime 被丟掉：在跑的請求、訂閱 task、金鑰後台**寫到一半被取消**（crypto store 的 `save_changes`、`cache.db` 的寫入者佇列）。/docs/design/daemon/daemon-runtime.md §9 寫的「等在跑的請求收攤」沒做 | `crates/wbf-daemon/src/main.rs` 結尾；issue #26 的「被取消」 |
| 5 | UI 要停一個長請求 | `cancel` 沒做（rpc-spec 表上是 ❌） | /docs/design/rpc-specs/rpc-spec.md §3.9、§10 |

📎 已經沒問題的：金鑰後台（`key_share`）task 結束之後，下一個要用它的呼叫會重起一個（`is_finished()` 就換新）；驗證的觀察者有 drop 守衛（PR #84）。

## 2. 形狀（提案）

### 2.1 一個共用的「看著 task」：panic 才動手

```rust
// crates/wbf-core/src/supervised.rs（示意）
pub(crate) fn spawn_supervised<F>(what: &'static str, events: EventSink, work: F, on_panic: impl FnOnce() + Send + 'static) -> JoinHandle<()>
where F: Future<Output = ()> + Send + 'static
// = tokio::spawn(AssertUnwindSafe(work).catch_unwind())，Err(panic) → note「<what> stopped unexpectedly: <訊息>」→ on_panic()
```

- **只在 panic 時**動手，正常結束、被 `stop`、被 abort 都照現在的路走。
  ⚠️ 不用 drop 守衛：舊 task 是在新的一條線開好**之後**才被換掉、abort（`room_syncs.insert` 回傳的舊 handle），drop 守衛會在那時候去關「那一格」，關到的是新線。panic 時 task 一定還是那一格的主人，所以只在 panic 時關是安全的。
- `on_panic` 做的就是那個 task **正常收攤會做的那一步**，禁止另外發明恢復邏輯：

| task | panic 時 | 之後誰接手 |
|---|---|---|
| 房間線收推播（`room_sync`） | 關 `Rooms` 那格線 | 看線迴圈下一輪（最多 15 秒）重開、重訂 |
| 金鑰線收金鑰（`key_sync`） | 發 `keys.state: stopped`、關 `Keys` 那格線 | 同上 |
| 下載處理端（`download_queue`） | 從 `Core.downloaders` 拿掉自己；等它的人收到 `failed` | 下一個 `media.download` 重起一個處理端（跟登入後第一次一樣，續傳照池裡的段） |
| 金鑰後台（`key_share`） | 只講一聲 | 已經會自己重起（§1 📎） |
| 連線的讀／寫／心跳（`wbf-sdk` 的 `link.rs`） | 標成 closed（跟對方關線同一條路） | 下一次取用或看線迴圈重開 |

- 測試：`spawn_supervised` 本身用一個會 panic 的 future 測（note 有發、`on_panic` 叫了一次、正常結束不叫）；每個呼叫點用假 server 讓 task 在處理一包時 panic（測試用的注入點，只在 `#[cfg(test)]`），斷言「那格線被關、看線那一輪重開」。

### 2.2 RPC 請求 panic：回 `108`

- `serve_connection` 裡每個請求的 future 包 `catch_unwind`：panic → 回 `108 internal`（`msg` 帶 panic 的訊息），**不**關連線（/docs/design/rpc-specs/rpc-spec.md §5.1 的 `108` 本來就是這個意思）。
- `in_flight` 的移除改成守衛：不管怎麼結束都清掉。
- 測試：一個只在 `#[cfg(test)]` 存在的 method 故意 panic → 回 `108`、同一條連線的下一個請求照常、`in_flight` 是空的。

### 2.3 關機：先收完再丟 runtime

照 /docs/design/daemon/daemon-runtime.md §9 那一列（「先停上游會話 → 等在跑的請求收攤 → 關 listener」）補齊：

1. `daemon.shutdown` 回完 → 新請求一律 `107`（已經有）。
2. **等在跑的 RPC 請求**做完，最多 10 秒；到了還沒完的照 `cancel` 的路回 `105`。
3. 停兩個收推播 task（送 `stop`、等手上那一包做完，最多 5 秒——現有的 `STOP_TIMEOUT`）、停看線迴圈、`close_all` 每個池。
4. 每個 `cache.db` 明確 `ServerCache::close()`：它會把佇列剩的寫完、join 寫入執行緒才回來。
   ⚠️ 光丟掉 `Core` **不夠**：`ServerCache` 沒有會 join 的 `Drop`，丟掉只是關掉送出端，寫入執行緒在背景把佇列寫完——程序接著結束的話它可能寫到一半被殺。
5. `runtime.shutdown_timeout(5 秒)`：上游 SDK 自己 spawn 的背景工作（crypto store 的寫入）有時間做完；禁止直接丟掉 runtime。
6. Ctrl+C（Windows 的 console 事件、Unix 的 SIGINT／SIGTERM）走同一條路，等於 `daemon.shutdown`。

- 測試：一個正在跑的慢請求，`daemon.shutdown` 之後它照樣拿到自己的回應、之後才收到關閉通知；`cache.db` 在關機前排的寫入，重開之後讀得到。

### 2.4 `cancel { id }`

形狀已經定在 /docs/design/daemon/daemon-runtime.md §8 與 rpc-spec §3.9，照做：

- 每條連線記「`id` → 取消的開關」；`cancel { id }` 只取消**這條連線**發的請求。
- 被取消的請求回 `105`；`cancel` 自己回 `{ ok: true, was_running: bool }`（已經回完或不認得的 `id` 是 `false`，**不是**錯誤）。
- 用 `select!` 讓請求的 future 落選、被 drop（禁止用 `abort()`：被 abort 的 task 講不出 `105`）。
- 取消留下什麼照 §8 的表：上傳可續、下載的段可續、`sync.recent` 水位不前進。
- 測試：慢請求被取消 → `105`、`was_running: true`；再取消一次 → `was_running: false`；別條連線的 `id` 取消不到。

## 3. 這支不做（理由）

| 項目 | 為什麼 |
|---|---|
| 重連時重探 backend（`forget_backend_probe` 沒有呼叫點） | wbf 帳號的 backend 登入時就記在 session，跟線死不死無關（/docs/design/daemon/link-pool.md §3 📎）；看線迴圈也刻意**不**探 server（維護者 2026-09-29）。重探沒有使用者，做了只是多敲 server。建議把「留給監督者」那幾句改成「不做」 |
| 進度節流、分佇列 | 維護者 2026-09-13：「這個你可以先不做」；RPC 上沒有高頻進度（/docs/design/daemon/daemon-runtime.md §5.4） |
| `apps/wbf-cli` 改走 RPC | 量大（CLI 2357 行全部直接用 `Core`），而且要先有一個 RPC 客戶端（加密、hello、`daemon.json`）。**另開一支**（A-2），先寫它自己的設計。issue #26 的 CLI 那半（一個命令一個程序、runtime 被丟掉）在那支收掉 |

## 4. 要維護者決定的

1. **切法**：這份（A-1：panic 收攤、關機、`cancel`）一支 PR，CLI 改走 RPC（A-2）下一支。還是要合成一支？
2. **重探 backend 不做**：同意的話，link-pool.md、account-session.md、daemon-runtime.md §10 裡「還沒做：重探」改成「不做」，`forget_backend_probe` 只留登入那個呼叫點。
3. **關機等多久**：請求 10 秒、收推播 task 5 秒、runtime 5 秒。要不要改？
4. **issue #26 的 CLI 那半**：A-2 之前要不要先在 `apps/wbf-cli/src/main.rs` 結尾加 `runtime.shutdown_timeout`（#26 自己說那是「繞過症狀」）？我傾向不加，等 A-2 直接收掉。

## 5. 驗收（點頭之後照這張做）

- 單元：§2.1–§2.4 每一節的「測試」那幾條。
- 變異：拿掉 `on_panic` 的關線、拿掉請求的 `catch_unwind`、拿掉關機時的「等請求」、拿掉 `cancel` 的 `select!`，各自對應的測試要紅。
- 真 server：整套 e2e 照跑（這支不改上游行為）；多一條「`daemon.shutdown` 時正在跑的 `sync.recent` 照樣回完」。

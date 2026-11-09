# 收攤：每個 task 都有終止接口、正常結束（草稿，等維護者點頭）

> 這份回答：daemon 裡每個背景 task 怎麼被叫停、關機時誰先停誰後停、UI 的 `cancel` 怎麼讓一個請求停下來。
> 對應 /docs/handover.md §7 第 3 點（daemon 穩健性）、/docs/design/daemon/daemon-runtime.md §8（取消）與 §10 的第 5、8 階段；本 repo issue #26 的 daemon 那半也在這裡。

## 0. 原則（維護者 2026-11-09）

> 「正式碼不應該用 panic 的方法，需要事先保留終止接口，正常結束。」

1. **每個長活的 task 一開始就留終止接口**：一個 stop 開關，task 在**安全點**（一包處理完、一批寫完之後，**不**在寫到一半的地方）看它，看到就自己收攤、`return`。
2. **叫停的人等它結束**：持有者握著 `JoinHandle`，`stop().await` ＝ 送開關＋等它回來。關機、登出、取消、換線一律走這條。
3. **abort、丟掉 runtime 不是正常結束**：它們讓 task 停在任何一個 `await` 上，寫到一半的東西就寫到一半。
4. **不設計成「panic 了再接住」**：我們的碼由 crate 根的 clippy 閘門擋住 `unwrap`／`expect`／`panic!`（/docs/handover.md §8）；第三方會 panic 的呼叫，在**那一個呼叫點**先驗輸入或包住、轉成 `Err` 走正常錯誤路（現有的例：`crates/wbf-sdk/src/crypto_engine.rs` 的 `encrypt_and_send`、`vault::decode_nonce`），禁止在 task 外層兜一層。
   📎 草稿第一版提過「整個 task 包 `catch_unwind`、panic 了再收拾」，維護者打回，就是這一條。

## 1. 盤點：每個 task 現在怎麼結束（2026-11-09，main `4163df5`）

只算正式碼；`file:line` 是那個 `spawn` 的位置。

### 1.1 已經有終止接口的

| task | 怎麼叫停 | 還差什麼 |
|---|---|---|
| 房間線收推播（`crates/wbf-core/src/room_sync.rs:139`） | `stop` oneshot，`Core::stop_room_sync_of` 送了等 5 秒（登出用） | 換線（`init_rooms` 插新的）、`Core` 丟掉時是 **abort**；等不到 5 秒也是丟掉＝abort |
| 金鑰線收金鑰（`key_sync.rs:236`） | 同上，`stop_key_sync_of` | 同上。⚠️ 這個最危險：迴圈裡在匯入 crypto store、補 OTK、寫 cache、上傳備份，abort 會停在任何一步之間 |
| 看線迴圈（`crates/wbf-daemon/src/server.rs:54`） | 看 daemon 的 shutdown 廣播 | `RpcServer::run` 一回來就被 `AbortOnDrop` abort，可能正在 `init_keys` 的中間 |
| RPC 連線的收送（`server.rs:60`、`:106`） | shutdown 廣播、對方關、送出端都丟了 | 沒人等它們 |
| 傳統格式的下載（`matrix_download.rs:252`） | `Command::Cancel`（`media.cancel`、登出） | 關機沒送 Cancel |
| `cache.db` 的寫入執行緒（`server_cache.rs:132`） | 送出端都丟了就把佇列寫完再結束；`ServerCache::close()` 會 join | 只有登出、摧毀叫 `close()`；**關機沒叫**，丟掉就變成沒人 join 的執行緒，程序一結束可能寫到一半 |

### 1.2 沒有終止接口、只能 abort 的

| task | 現在 | 後果 |
|---|---|---|
| 金鑰後台（`key_share.rs:346`） | 輸入 channel 永遠有送出端在，所以只能 abort；`stop_room_key_share_of` 就是 abort | 分房間金鑰、發 to-device、上傳備份做到一半被切：to-device 送出去了、「已送」卻沒記下 |
| 分塊下載處理端（`download_queue.rs:449`） | 兩條 channel 永遠有送出端；`stopped` 旗子有設但**迴圈沒看**；`Downloader::stop` 是 abort | 寫檔是同步的、DB 走寫入者，所以檔案本身不壞；但完成的推播、等它的人會掉 |
| 下載線的發送端（`link_requests.rs:85`） | 無窮迴圈，沒有出口；只會跟著處理端被 abort | 只碰網路 |
| 一條 WS 線的讀、寫、心跳（`crates/wbf-sdk/src/link.rs:161`–`163`） | `shut_down` 一律 abort 三個 | 寫到一半的 frame 被切；不碰磁碟 |
| 每條 RPC 連線的推播 task（`server.rs:134`） | 連線結束時 abort | 只碰網路 |
| 驗證的觀察者（`verify_ops.rs:98`） | 沒有任何訊號，登出也不停；最多跑 15 分鐘 | 握著 `OlmEngine`，登出要刪 `m/` 時 Windows 上可能刪不掉；完成時會寫 crypto store |
| 正在跑的 RPC 請求（`server.rs:229`） | 沒人能叫停、沒人等 | 關機時停在任何一步（送訊息、登出清理、匯入…） |
| 開線的一次性工作（`crates/wbf-daemon/src/handle/mod.rs:614`） | 沒人能叫停、沒人等 | 同看線迴圈 |
| 資料平面的 HTTP 連線（`crates/wbf-daemon/src/data_plane.rs:331`） | 不看 shutdown | 上傳、串流到一半被切 |

### 1.3 關機現在做什麼

`daemon.shutdown` → 兩個 accept 迴圈停、每條 RPC 連線送關閉通知、看線迴圈被 abort → `main` 回來 → **runtime 被丟掉**：上面所有 task 停在當下的 `await`，`cache.db` 的寫入執行緒沒人 join。
沒有任何地方叫 `close_links`、`stop_*_of` 或 `ServerCache::close()`。

📎 issue #26 的 panic（`matrix-sdk-sqlite` 的 `set_kv_blob` 在 `interact(..).await.unwrap()` 拿到 `Cancelled`）就是「runtime 被丟掉時寫到一半」——正常關機做完，那個條件就不出現，不必去接那個 panic。

## 2. 形狀（提案）

### 2.1 一種終止接口，到處用同一種

每個長活的 task 由一個「持有者」結構握著，持有者提供同一組：

```rust
// 示意
struct XxxHandle { stop: watch::Sender<bool>（或 oneshot）, task: JoinHandle<()> }
impl XxxHandle {
    /// 送開關、等 task 自己 return。等不到 `timeout` 就講一聲（note），回 Err——那是 bug 的訊號，不是正常路徑。
    async fn stop(self, timeout: Duration) -> Result<(), StopTimedOut>
}
```

- task 的迴圈在**安全點**看開關：`select!` 只放在「等下一包／下一件事」那一個 `await` 上；處理一包的途中不看。
- `Drop` 不再 abort。持有者被丟掉而沒先 `stop()` ＝ 呼叫端的 bug，`Drop` 只送開關（讓它自己收攤），不等。
- 逐個改：

| task | 改成 |
|---|---|
| 房間線、金鑰線收推播 | 已有開關；換線時**先 `stop()` 舊的再起新的**（舊線已經死了，它很快就收），`Drop` 改成只送開關 |
| 金鑰後台 | 加開關；手上那一件做完才看（分完一間房、上傳完一批），`stop_room_key_share_of` 改成 `stop()` |
| 分塊下載處理端 | 迴圈的 `select!` 加開關；手上那一包寫完才看；`Downloader::stop` 改成送開關＋等 |
| 下載線的發送端 | 加開關（處理端收攤時送） |
| WS 線的讀、寫、心跳 | `shut_down` 改成送開關：寫的那個把佇列裡剩的送完或放棄、讀的那個停止讀、心跳停；三個都等 |
| 驗證的觀察者 | 加開關；登出時跟其他三個一起停（不再握著引擎） |
| 看線迴圈 | 已看 shutdown；`AbortOnDrop` 改成關機時等它這一輪做完 |
| 資料平面的連線 | hyper 的 graceful shutdown：不收新請求、等手上的回完 |

### 2.2 正在跑的 RPC 請求與 `cancel`：一個請求一個取消開關

- 每個請求帶一個取消開關（`CancellationToken`，連線的、daemon 的開關是它的上層：連線關了、daemon 要關都會往下傳）。
- **長工作**（`room.send_file`、`upload.file`、`media.export_to`、`sync.recent`，rpc-spec §3.9 列的那幾支）在安全點看它：送完一塊、寫完一批之後。看到就收攤、回 `Err(Cancelled)` → `105`。
  收攤留下什麼照 /docs/design/daemon/daemon-runtime.md §8 的表：上傳的狀態檔可續、下載的段可續、`sync.recent` 水位不前進。
- **短請求**不看開關：它們很快就做完，做完照常回結果。
- `cancel { id }`：只取消**這條連線**發的請求；回 `{ ok: true, was_running: bool }`（已經回完或不認得的 `id` 是 `false`，**不是**錯誤）。
- ⚠️ 這改了 /docs/design/daemon/daemon-runtime.md §8 定的「`select!` 讓請求的 future 落選、被 drop」：被 drop 的 future 也是停在任何一個 `await` 上（跟 abort 同一件事），所以改成合作式的開關。§8 那段要跟著改。

### 2.3 關機：照順序叫停、每一步等完

`daemon.shutdown`（或 Ctrl+C：Windows 的 console 事件、Unix 的 SIGINT／SIGTERM）之後：

1. 新請求一律 `107`、兩個 accept 迴圈停（已經有）。
2. 等正在跑的 RPC 請求做完，最多 10 秒；到了還沒完的，送取消開關，再等它們回 `105`。
3. 看線迴圈停（等它這一輪做完）。
4. `Core::shutdown()`：每個帳號照登出的順序停背景（收推播兩條、金鑰後台、下載處理端、傳統下載、驗證觀察者），再 `close_all` 每個池。**不**登出、**不**刪任何東西。
5. 每個 `cache.db` `close()`：佇列寫完、join 寫入執行緒。
6. 資料平面 graceful shutdown、RPC 連線送完關閉通知。
7. `main` 回來；runtime 最後才丟（到這時已經沒有在跑的 task）。

- 每一步有上限（§4 第 3 點），超過就講一聲、往下一步走，**不**卡住關機。

## 3. 這支不做（理由）

| 項目 | 為什麼 |
|---|---|
| 重連時重探 backend（`forget_backend_probe` 沒有呼叫點） | wbf 帳號的 backend 登入時就記在 session，跟線死不死無關（/docs/design/daemon/link-pool.md §3 📎）；看線迴圈也刻意**不**探 server（維護者 2026-09-29）。重探沒有使用者 |
| 進度節流、分佇列 | 維護者 2026-09-13：「這個你可以先不做」；RPC 上沒有高頻進度（/docs/design/daemon/daemon-runtime.md §5.4） |
| `apps/wbf-cli` 改走 RPC | 量大（CLI 2357 行全部直接用 `Core`），而且要先有一個 RPC 客戶端（加密、hello、`daemon.json`）。另開一支（A-2）；issue #26 的 CLI 那半在那支收掉 |
| 接住 task 的 panic | §0 第 4 點 |

## 4. 要維護者決定的

1. **切法**：這份（A-1：終止接口、關機、`cancel`）一支 PR，CLI 改走 RPC（A-2）下一支。
2. **重探 backend 不做**：同意的話，link-pool.md、account-session.md、daemon-runtime.md §10 裡「還沒做：重探」改成「不做」。
3. **等多久、等不到怎麼辦**：請求 10 秒、每個背景 task 5 秒（現有的 `STOP_TIMEOUT`）。等不到時我的提案是講一聲（note）、**不**再 abort、往下一步走——關機最後 runtime 被丟掉時它才停。要不要改？
4. **`cancel` 從「drop 掉 future」改成合作式開關**（§2.2 的 ⚠️）：短請求被 `cancel` 時照樣做完、回正常結果，`cancel` 回 `was_running: true`。

## 5. 驗收（點頭之後照這張做）

- 每個 §2.1 表上的 task：`stop()` 之後它**自己 return**（`JoinHandle` 回 `Ok`、不是 cancelled），而且手上那一件做完了（例：下載處理端手上那一包寫進檔案、金鑰後台手上那間房的 to-device 送完而且記了「已送」）。
- `Drop` 不再 abort：持有者丟掉之後 task 照樣把手上的做完才結束。
- 關機：正在跑的慢請求照樣拿到自己的回應、之後才收到關閉通知；關機前排進 `cache.db` 的寫入，重開之後讀得到；關機之後沒有還活著的 task（runtime 的 metrics 數 0）。
- `cancel`：長請求被取消 → `105`、`was_running: true`、留下的東西照 §2.2；再取消一次 → `was_running: false`；別條連線的 `id` 取消不到；短請求被取消 → 正常結果。
- 變異：把任一個 task 的開關拿掉（換回 abort）、關機不等請求、`cache.db` 不 `close()`，各自對應的測試要紅。
- 真 server：整套 e2e 照跑；多一條「`daemon.shutdown` 時正在跑的 `sync.recent` 照樣回完、重開 daemon 之後水位是它寫到的那裡」。

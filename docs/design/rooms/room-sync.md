# 訂閱線的內容：訂閱房間事件、推播寫進快取；水位只由 UI 的 Recent 動

> 維護者 2026-09-22／23 定，原話在 §0。實作：sdk `protocol.rs`／`client.rs` 的 `Event/Subscribe`／`Unsubscribe`／`Push` codec 與
> `room_subscription`；core `room_sync.rs`（`init_connection`、收推播的 task）；`sync.recent` 的 `since`（daemon `handle/rooms.rs`、core `sync_ops.rs`）。
> server 的語意在 wbfuwunel 的 wbf-event-push.md（推送）與 wbfuwunel 的 room-seq-and-recent.md（`Recent`）。
> 🚫 這支**不接開／關訂閱線的 RPC**：daemon 的訂閱（rpc-spec.md §3.9，每條 RPC 連線想收什麼）與 daemon 跟上游是兩件事（中繼），先純一點。
> 維護者 2026-09-29 定：訂閱線**總是由 daemon 搞定**——`vault.unlock`／`account.add` 之後開、常駐時背景迴圈看著、被關掉的重開（link-pool.md §3.1）；
> 房間與金鑰各自一條線（`LinkRole::Rooms`／`Keys`）。UI 要不要收推播是 RPC 的 `subscribe`，🚫 沒有 `sync.open`／`sync.close`。

## 0. 規矩（維護者原話）

2026-09-22：
> 先做明文房間，再做 E2EE。第二個就是訂閱金鑰。
> 啟動連線池時，應該要寫一個通用 init_connection，把 connection 帶入那個 function，根據連線的狀態，例如是 room 專用的，就啟動訂閱，至於補窗啥的，其實可以直接無視。
> 補窗交由 UI，daemon 只管訂閱當下，什麼時候補由 UI trigger。UI 只知道萬一收到回應還有 more 就繼續下發新的 rpc 命令。
> 有沒有洞 UI 完全不知道。UI 只根據 Recent 判斷是否滾到了；而且 UI 可以總是呼叫 Recent，無副作用。

2026-09-23：
> g_seq 漏了那些我都不管，永遠拿不到也不管。UI 層透過 rpc 叫時，應該拿到 g_seq 自然知道最後一筆的 ID，從最後一筆叫。不夠的話 UI 自己再叫。
> daemon 只有訂閱新事件，不主動叫 Recent。誰負責記有沒漏，是 UI 層的事。
> UI 拿到的這個事件是全局 sync，而不是點進房間的事。UI 起來的當下會主動 sync；點進房間叫跟房間有關的 sync，根據 r_seq 過濾。

## 1. 形狀

```
池開一條線（link_pool::open_link）
  connect(Bearer) → hello(features_of(role)) → init_connection(account, role, client)
                                                 ├─ Rooms：Event/Subscribe（帳號層、不帶 rooms、不帶 cg_seq）→ 等 Ack → 起收推播的 task
                                                 ├─ Keys：金鑰那半（key-sync.md）
                                                 └─ 其他角色：不做事
收推播的 task（一帳號一個；線在池裡、task 只握訂閱的會話）
  Push{events} → 原樣 upsert（照房分組；存不了的擋掉、數出來）→ commit 之後發 room.message。🚫 不碰水位。
  gap／本地丟包／壞包／寫失敗 → 講一聲（Note），繼續收。🚫 不記洞、不補。
  線死了 / server 送 Error → task 把訂閱那格線關掉（socket 活著也一樣）、池發 link.state: closed（帶原因）、task 結束；🚫 task 不重訂（daemon 的看線迴圈下一輪重開就重訂，link-pool.md §3.1）
UI
  起來時 sync.recent（全局、g_seq 游標）：帶 since（它自己記的起點：上次的 cg_seq_after、或手上最後一則 room.message 的 g_seq），
  沒帶就用 daemon 存的上一次 Recent 的水位；回應 caught_up 是 false 就再叫。補不補、補到哪，全是 UI 的事。
  點進房間 room.history（單房）：本地翻頁用 r_seq；sync=both 打上游 Recent{rooms:[這房], before: 這房最舊那則的 g_seq}（跟訂閱線無關）。
誰開這條線：Core::ensure_links（link_keeper.rs）——daemon 在 vault.unlock／account.add 之後、與常駐的看線迴圈每一輪叫它；開的時候就訂了
```

- **水位（`cg_seq`）只由 `sync.recent` 動**：拉完推到這次最新的 `fs`。推播寫進庫的事件帶著 `r_seq`／`g_seq`，之後點進房間本地翻頁直接用得到，
  但**不推水位**——所以下次 UI 叫 Recent（沒帶 `since`）會從上次 Recent 的水位重拉，推播已經寫過的那段重寫一次（冪等）；推播漏掉的那段自然補回。
  UI 想省這一趟就帶 `since`＝它手上最後一則的 `g_seq`，代價是漏掉的不補——維護者：永遠拿不到也不管。
- **daemon 沒有洞的概念**：server 的 `gap`、本地收件匣滿、一包解不開、cache 寫失敗、存不了的事件——都只講一聲（`Note`）、繼續收。
  維護者 2026-09-23 定：🚫 daemon 不凍結水位、不判「什麼算洞」——誰記有沒有漏是 UI 的事。
- **跟官方 Matrix 同形**：`Push` 是 `/sync` 迴圈只追加 server 給的、`sync.recent` 是 client 決定何時往回補的 `/messages`。
- **`Subscribe` 不帶 `cg_seq`**：server 的補窗有上限、截斷只給一個 gap bit；反正補窗是 UI 的事，daemon 不要它。
- **訂閱是純的**：一包來寫一包，`seq` 跳號不管。
- **訂閱會話結束＝那條線作廢**：server 送 `Error`（例如被另一台裝置接手的 1505）時 socket 可能還活著，池的殞死偵測（`is_closed`）看不出來——
  task 收攤時自己 `LinkPool::close(Rooms, "the room subscription ended: …")`，`link.state: closed` 由池發、UI 不必等；看線迴圈下一輪看到那格是空的才會重開、重訂。
  理由：只發事件不關線的話，池會把活著的舊線交回去、永遠不再訂。🚫 task 自己不重訂：重開的只有看線迴圈這一條路。

## 2. 存不了的事件

缺 `room_id`、`event_id` 或 `sender` 的事件 `cache.db` 放不進去（server 給的完整 Pdu 一定有，這是防禦）。存不存得了只有一支在判：
`IncomingEvent::storable_identity`，`upsert_events` 與進料口共用。推播那條在分組時就擋掉——🚫 不替一則不在庫裡的事件發 `room.message`；
`upsert_events_counted` 回不寫了幾則，Push 與 Recent 都用 `Note` 講出來（`RecentSummary.skipped_without_room` 也含它們）。

## 3. 事件

| 什麼時候 | 發什麼 |
|---|---|
| 每包 commit 之後 | 每則一個 `room.message`（自己送的也發，收的人自己濾；加密的有金鑰就解好再發，沒金鑰的是 `undecryptable`、`decrypted: false`） |
| 推來 `DeviceChanged` | 原樣轉成 `devices.changed`，要不要 refresh 是 UI 的事（e2ee-rpc.md §4） |
| 漏包（gap／丟包／壞包／寫失敗／存不了） | 一則 `Note`，只是講一聲 |
| 線開／關 | 池的 `link.state`（`rooms`）；task 因訂閱結束而關線時 `closed` 帶「the room subscription ended: …」 |

🚫 不發 `sync.state`（追平與否是 UI 自己叫 `sync.recent` 的結果；variant 留著，還沒人發）。

## 4. 生命週期

- 一個帳號一個 task（`Core::room_syncs`）；線重開時 `init_connection` 換掉舊的（舊的線已經死了）。
- 登出：`stop_room_sync_of` 先收 task，再 `close_links`。`Core` 丟掉：handle 的 `Drop` abort task。
- task 🚫 不握 `Core`、不握線：握 `Arc<ServerCache>`、`EventSink`、訂閱的會話、池的 `Arc`（只為了收攤時關那格）。
  金鑰的 `Device/Subscribe` 在自己那條線（`Keys`）上，跟這個 task 無關。
- `Event/Unsubscribe` 的 codec 與 `room_unsubscribe` 在 sdk 裡（對著向量），core 不用它：關訂閱線就是關線，server 斷線自動退訂。

## 5. 不在這支

- RPC（開／關訂閱線的命令）：🚫 不做（維護者 2026-09-29：訂閱線總是由 daemon 搞定）。開線走 `ensure_links`。
- 金鑰訂閱（`Device/Subscribe`、`pull_to_device`）：key-sync.md。
- 背景重開：link-pool.md §3.1。
- 密文解密：`room_crypto.rs`（task 寫庫前叫它）。推來的有金鑰就解，密文明文一起存；沒金鑰只存密文，金鑰到了由金鑰那半補解（e2ee-rpc.md §6）。
- 還沒做：task 內 panic 的收攤。panic 不走 `pool.close`，會留下「線活著、沒 task」而且沒有 `closed`；
  文件化的結束路徑（Error／線死／`stop_room_sync_of`）都收口了，panic 要監督者統一收攤（daemon-runtime.md §10 第 8 階段）。

## 6. 測試

- sdk `tests/unit.rs`：`Subscribe`（帳號層／點名）、`Unsubscribe` 逐 byte 對 server 向量；`Ack`、`Push`（含 gap）、`DeviceChanged` 解得回向量的值；
  `bc` 對不上事件數是 Protocol；缺 `gap` 欄位當 true。
- core `room_sync.rs`（記憶體對接的假 server：答 Hello、Subscribe、Recent 照 `cg_seq` 給窗；測試主動推 Push；兩條線共用同一份事件）：
  `the_task_only_writes_pushes_and_never_touches_the_watermark`——訂了、沒叫 Recent；推一包寫進去、`room.message` 在 commit 之後、水位不動；
  gap 包、壞包、之後的包都一樣寫得了的寫、水位不動、漏的不補；UI 的 `sync.recent` 帶 `since` 才動水位、才補回；沒帶 `since` 從存的水位起。
  `an_event_that_can_never_be_stored_is_reported_and_not_announced`；`a_dead_line_ends_the_task_and_says_so`；
  `an_ended_subscription_closes_the_line_so_the_next_open_subscribes_again`（server 送 Error、socket 活著 → 那格關了、`closed` 帶原因 → 下次 `acquire` 走 `open` → 第二個 `Subscribe`、新訂閱收得到）；
  `a_gap_in_a_push_before_the_ack_is_reported_too`（假 server 在 Ack 之前先推一包帶 gap 的）。
- 真 server（`--ignored`，`WBF_E2E_*`）：alice 登入、`ensure_links`（五條都開起來）；bob（另一個 Core）`Event/Send` 送一則；alice 的 `room.message` 在時限內到、
  cache 有它、水位不動；兩邊登出（task 跟著收掉）。
- ⚠️ 沒測的：本地收件匣灌爆那條 `Note`；真 server 的 gap。

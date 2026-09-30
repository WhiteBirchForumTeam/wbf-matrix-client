# 金鑰那條線：`Device/Subscribe`、上線追平、推來就匯、異常就從佇列頭拉——推的與拉的走同一支

> 維護者 2026-09-24 定，原話在 §0。實作：sdk `protocol.rs` 的 `DevicePushMeta`／`SubscribeReply::Push`（`Push` 跟 `Batch` 共用 `parse_device_items`）、
> `client.rs` 的 `DeviceSubscription::next`、`crypto_engine.rs` 的 `OlmEngine::import_items`（吃「一批 items」）；
> core `key_sync.rs`（`olm_engine_of`、`init_keys`、收金鑰的 task）、`link_pool.rs` 的 `reuse`；事件 `CoreEvent::Keys` → RPC `keys.state`。
> server 的語意在 wbfuwunel 的 `/docs/design/wbf-to-device.md`；client 端要守的線上規則（順序、銷毀、會話、被接手）與理由在 /docs/design/keys/to-device-client.md。這份講 core 怎麼把它們接起來。

## 0. 規矩（維護者原話，2026-09-24）

> 處理一個 key event 不論是遠端來的 push 還是主動 pull，他的封包應該大同小異，其實是共用同個處理函數，這個處理函數收到 key 做的事都一樣，
> 解碼落地資料庫，銷毀金鑰。所以 pull、push 時最根本的 handle 封包應該調用一樣的 function。

> `keys.state` 有點多餘，但是我傾向保留。不然 RPC 無從知道。

> 訂閱線被關其實應該主動再開，這部分先不做無妨，可以等 server 支援更多連線時，更乾淨再做。

> `Device/Fetch`／`ItemsDestroy` 走訂閱線而不是 Misc：同意。

2026-09-29（server #85 把每台裝置的 WS 上限放到 8，/docs/design/daemon/link-pool.md §3.1）：金鑰**自己一條線**（`LinkRole::Keys`），
訂閱線總是由 daemon 搞定——解鎖／登入之後開、常駐時背景迴圈看著、被關掉的重開。上面「訂閱線被關應該主動再開」就是這一條。

2026-09-26（來由在 §1「佇列頭就是水位」）：

> 往上報其實不該主動放 cd_seq，而是讓 server 端去找最舊的發下來。
> 金鑰本身有沒有被 read 過，其實需要確認機制，不然漏了就是真漏了。現在刪除金鑰是 app 端保證……server 根本不知道遠端到底有沒有收到。
> 我還在思考應該要有 client 端的 ACK，但感覺用這做法就沒必要了，所以只剩下伺服端修改。（wbfuwunel #87）

## 1. 形狀

```
鉤子（Core::ensure_links，/docs/design/daemon/link-pool.md §3.1）→ 池開金鑰那條線（link_pool::open_link）→ init_connection(account, Keys, client) → init_keys
       不是 wbf 帳號（金鑰在 matrix-sdk 的 Client 裡）→ 講一聲、跳過（鉤子本來就只替 wbf 帳號開線，這是多一道防線）
       Device/Subscribe{device_id}（不帶 cd_seq）→ Ack ＋ CryptoState
       Ack 之前推來的（early pushes）→ 不單獨匯：還沒銷、還在佇列裡，下面的追平會一起拉回
       pull_to_device：Fetch（🚫 不帶 cd_seq：server 從佇列最舊還沒銷毀的給）一窗一窗到 more=false，每窗 import_items   ← 同一支
       發 keys.state: caught_up（匯了幾則、幾把新房間金鑰）；這幾窗帶來的新房間金鑰 → 去 cache 補解（/docs/design/keys/e2ee-rpc.md §6）
       訂閱時跟著來的 CryptoState 交給狀態機 → send_outgoing_requests：上傳裝置金鑰、一次性金鑰、fallback key（/docs/design/keys/e2ee-rpc.md §5；失敗只講一聲）
       起收金鑰的 task（握 Arc<OlmEngine>、Arc<LinkPool>、Arc<ServerCache>、EventSink、訂閱會話）
       ⚠️ 這一段回錯（m/ 開不起來、Subscribe 被拒、追平壞包）＝這條線沒開成：fail loud，
          因為「金鑰沒在收」靠 UI 看不出來，線開不起來看得出來（接受的取捨）。
          沒進 store 的都還在佇列裡，下次開線的追平會拉回（恢復靠重開線：看線迴圈每 15 秒試一次）
收金鑰的 task（佇列頭就是水位：沒銷的都還在，從頭拉一次就回來）
  Push{gap:false, items} → pool.reuse(Keys) 拿線 → import_items → keys.state: caught_up → 帶來的新房間金鑰去 cache 補解、發 room.message
  Push{gap:true} / import_items 回錯 / 一包解不開 / 本地收件匣滿過 / CryptoState{gap:true} → 記「要拉」
  每處理完一個事件（含 60 秒閒置逾時）：有「要拉」就 pull_to_device 一次；失敗就留著，下一個事件或下一分鐘再拉（🚫 不原地狂試）
  CryptoState            → 存量交給狀態機，它要補（不到 50 把、fallback key 到期）就走 Keys 線上傳（/docs/design/keys/e2ee-rpc.md §5）
  訂閱結束（server 送 Error：被同一裝置後來的連線接手的 1505；或線死了）→ keys.state: stopped 帶原因、關掉這格線（link.state: closed）、task 結束
                           🚫 task 不重訂（/docs/design/keys/to-device-client.md §5.1）；關線是為了讓看線迴圈下一輪看到它不在、重開重訂
登出：停兩個 task → Device/Unsubscribe（說出口的退出）→ close_links → 丟掉長活引擎 → 刪 m/
```

- **同一支**：`OlmEngine::import_items(client, items)`＝匯進 crypto store（commit 了才回）→ `cd_seq` 與待銷毀清單落地（`m/td.json`）→
  對 server `ItemsDestroy` 那一批 → 只清回來的。`Fetch` 的一窗與推來的一包都是 `(count, 事件)` 舊→新，差別只在 meta（`Batch` 多 `tc`／`r`／`more`，`Push` 多 `gap`），
  解的那半也共用（`parse_device_items`）。core 不解封包、不碰 store。
- **`Fetch`／`ItemsDestroy` 走金鑰那條線**：server 只讓持有這台裝置佇列的連線銷毀（/docs/design/keys/to-device-client.md §8），所以 task 用線時跟池 `reuse` `Keys` 那一格
  ——`reuse` 只拿開著的線，🚫 不開（開線是 `open_link` 的事，會再跑一次 `init_connection`、換掉 task 自己）。線不在就講一聲：東西還在 server 佇列裡，下次開線的追平會拉回。
- **佇列頭就是水位**（維護者 2026-09-26，wbfuwunel #87；規則與「帶游標會漏的三條路」在 /docs/design/keys/to-device-client.md §7）：`Fetch` 🚫 不帶 `cd_seq`，
  `ItemsDestroy` 是唯一的「處理完了」，`m/td.json` 的 `cd_seq` 只是紀錄。所以 core 這邊「要拉」一律從頭拉，不必記任何位置；
  🚫 不要用「不先匯、標落後」之類的狀態去補游標，那是在補自己挖的坑。代價是「匯了但還沒銷成」的那幾則下次會再回來、重複匯入一次（冪等）。
  server 把這條寫成承諾（wbfuwunel #88）：`Fetch{}` 是正確叫法、翻頁靠銷毀；`cd_seq` 欄位 server 還收（不做 breaking），但 client 不送。client 的**型別直接沒有** `DeviceFetchRequest.cd_seq`／`DeviceSubscribeRequest.cd_seq`，🚫 不靠「記得填 None」。server 的黃金向量 `device_fetch`／`device_subscribe` 還帶 `cd_seq`，client 的向量測試先拿掉它再比（`tests/unit.rs`）；還沒做：不帶游標的向量（wbfuwunel #91）。
- **單則壞掉的 item 不會卡住佇列**：上游狀態機遇到解不出形狀的 to-device 是記成 `Invalid` 跳過（`receive_to_device_event` 的 "Skip invalid events"），不是整批報錯；它跟著那一窗被銷掉。會讓 `import_items` 整批回錯的只有本地 crypto store 寫不進去——那種時候本來就什麼都匯不了，task 每分鐘重拉一次、`Note` 講一聲，`keys.state` 不再發 `caught_up`。server 端**刻意不給逃生口**（wbfuwunel #88：卡住是 client 端要處理的事，「跳過」等於把「永遠解不開」交給最不知情的那一層）——而 client 端本來就不會卡在單則上。📎 解不開的加密 to-device（Olm session 壞了）也是「處理過了」、會被銷：那則裡的房間金鑰要靠金鑰請求或備份補，跟一般 Matrix client 一樣。
- **長活的引擎**：`Core::crypto_engines` 一帳號一個 `Arc<OlmEngine>`，第一次要用才開（鑰匙就是 login 用的 `vault.matrix_store_key()`），
  只給 wbf 帳號（matrix-sdk 帳號的金鑰在它自己的 Client 裡）。登出丟掉（Windows 上開著刪不掉 store）。

## 2. 事件

| 什麼時候 | 發什麼 |
|---|---|
| 上線追平完、或推來一包匯完 | `keys.state { user, state: "caught_up", imported, room_keys }`——`room_keys` 是這一輪帶進來的新房間金鑰數，UI 拿它決定要不要重解密文 |
| 訂閱結束（被接手、線死） | `keys.state { user, state: "stopped", reason }`：這台裝置不再收金鑰 |
| 線不在、匯入失敗、拉失敗、上傳金鑰失敗、補解失敗 | `Note`，只是講一聲 |
| 新房間金鑰讓 cache 裡的舊密文解開了 | 每一則一個 `room.message`（同 `event_id`，UI 當更新；/docs/design/keys/e2ee-rpc.md §6） |

## 3. 不在這支

- `DeviceChanged` 與 refresh：不在金鑰這條線上。房間那條線原樣轉給 UI，refresh 是 UI 叫的（/docs/design/keys/e2ee-rpc.md §2、§4）。
- 線怎麼開、怎麼重開：/docs/design/daemon/link-pool.md §1、§3.1。

## 4. 測試

- sdk `tests/unit.rs`：`Push` 對 server 向量 `device_push` 解得回 `(count, 事件)`；`bc`／`counts` 對不上、不遞增是 Protocol（跟 `Batch` 同一支）。
- core `key_sync.rs`（`test_support.rs` 的假 server 多答 `Device/Subscribe`（Ack＋CryptoState）、`Fetch`（佇列裡比 `cd_seq` 新的；client 不帶就是從頭，並記下每次帶了什麼）、`ItemsDestroy`（從佇列刪、Ack＋`ItemsDestroyed`）、`Unsubscribe`）：
  `the_line_subscribes_keys_catches_up_and_imports_pushes_through_one_path`——開線前佇列裡有 1、2 → 訂了、拉到、匯完就銷毀、`caught_up{imported:2}`；
  推 3 → 銷毀 [3]；佇列多 4、5 但只推 5 帶 gap → 從佇列頭拉一次：4、5 一起匯、銷毀；收 task（登出那條路）收得掉。
  `a_taken_over_key_subscription_stops_says_so_and_closes_its_line`——server 對金鑰會話送 Error → task 停、`stopped` 帶原因、`Keys` 那格關了（`link.state: closed`）、🚫 不重訂。
  `an_early_push_does_not_let_the_catch_up_skip_older_queued_keys`——`td.json` 先塞一個比佇列還新的舊記錄 99、佇列 1、2、3、Ack 前推 3 → 追平照樣從佇列頭拉：[1,2,3] 一起匯、銷毀、`Fetch` 只帶過 `None`。
  `a_crypto_state_with_gap_pulls_the_queue`——`CryptoState{gap:true}` → 後面跟一次 `Fetch`，佇列裡的拉回。
  `a_later_push_never_makes_an_earlier_queued_key_unreachable_and_failed_pulls_are_retried`——佇列 1、2 只推 2（沒 gap）→ 2 匯銷；
  `CryptoState{gap}` 觸發的 `Fetch` 被注入失敗 →「要拉」留著；推 3 → 3 匯銷、重拉 → 1 回來；三則都銷、每次 `Fetch` 都不帶游標。
- 真 server（`--ignored`）：`a_key_subscription_receives_a_room_key_shared_by_another_device_over_the_real_server`——同一帳號兩台裝置：
  B（core）登入、上傳裝置金鑰、`ensure_links`（五條都開、金鑰那條訂了）；A（sdk 層）查到 B、把房間金鑰用 to-device 分給 B；B 的 task 收 Push 匯進 store，`caught_up{room_keys ≥ 1}`；B 登出（走退訂）。
- ⚠️ 沒測的：本地收件匣灌爆那條；真 server 的 gap；真正的匯入失敗（假 server 造不出來：OlmMachine 對壞事件是略過不是報錯）；`CryptoState` 不帶 gap 那條 `Note`。

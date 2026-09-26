# 訂閱線的金鑰那半：`Device/Subscribe`、上線追平、推來就匯、異常就從佇列頭拉——推的與拉的走同一支

> 維護者 2026-09-24 定，原話在 §0。實作：sdk `protocol.rs` 的 `DevicePushMeta`／`SubscribeReply::Push`（`Push` 跟 `Batch` 共用 `parse_device_items`）、
> `client.rs` 的 `DeviceSubscription::next`、`crypto_engine.rs` 的 `OlmEngine::import_items`（原 `import_window`，改吃「一批 items」）；
> core `key_sync.rs`（`olm_engine_of`、`init_keys`、收金鑰的 task）、`link_pool.rs` 的 `reuse`；事件 `CoreEvent::Keys` → RPC `keys.state`。
> server 的語意在 wbfuwunel `wbf-to-device.md`；client 端的原提案與踩坑在 [to-device-client.md](to-device-client.md)。

## 0. 規矩（維護者原話，2026-09-24）

> 處理一個 key event 不論是遠端來的 push 還是主動 pull，他的封包應該大同小異，其實是共用同個處理函數，這個處理函數收到 key 做的事都一樣，
> 解碼落地資料庫，銷毀金鑰。所以 pull、push 時最根本的 handle 封包應該調用一樣的 function。

> `keys.state` 有點多餘，但是我傾向保留。不然 RPC 無從知道。

> 訂閱線被關其實應該主動再開，這部分先不做無妨，可以等 server 支援更多連線時，更乾淨再做。

> `Device/Fetch`／`ItemsDestroy` 走訂閱線而不是 Misc：同意。

2026-09-26（PR #60 首審抓到三條會永久漏金鑰的路之後）：

> 往上報其實不該主動放 cd_seq，而是讓 server 端去找最舊的發下來。
> 金鑰本身有沒有被 read 過，其實需要確認機制，不然漏了就是真漏了。現在刪除金鑰是 app 端保證……server 根本不知道遠端到底有沒有收到。
> 我還在思考應該要有 client 端的 ACK，但感覺用這做法就沒必要了，所以只剩下伺服端修改。（wbfuwunel #87）

## 1. 形狀

```
池開一條訂閱線（link_pool::open_link）→ init_connection(account, Subscriptions, client)
  ├─ 房間那半（room-sync.md）
  └─ 金鑰那半（init_keys）
       不是 wbf 帳號（金鑰在 matrix-sdk 的 Client 裡）→ 講一聲、跳過；房間那半照常
       Device/Subscribe{device_id}（不帶 cd_seq）→ Ack ＋ CryptoState
       Ack 之前推來的（early pushes）→ 不單獨匯：還沒銷、還在佇列裡，下面的追平會一起拉回
       pull_to_device：Fetch（🚫 不帶 cd_seq：server 從佇列最舊還沒銷毀的給）一窗一窗到 more=false，每窗 import_items   ← 同一支
       發 keys.state: caught_up（匯了幾則、幾把新房間金鑰）
       起收金鑰的 task（握 Arc<OlmEngine>、Arc<LinkPool>、EventSink、訂閱會話）
       ⚠️ 這一段回錯（m/ 開不起來、Subscribe 被拒、追平壞包）＝整條訂閱線沒開成，房間那半也一起：fail loud，
          因為「金鑰沒在收」靠 UI 看不出來，線開不起來看得出來（PR #60 審查 cirno 🟡；接受的取捨）。
          沒進 store 的都還在佇列裡，下次開線的追平會拉回（rumia 🟡：恢復靠重開線，沒有背景重試——訂閱線自動重開等 server 支援更多連線）
收金鑰的 task（佇列頭就是水位：沒銷的都還在，從頭拉一次就回來）
  Push{gap:false, items} → pool.reuse(Subscriptions) 拿線 → import_items → keys.state: caught_up
  Push{gap:true} / import_items 回錯 / 一包解不開 / 本地收件匣滿過 / CryptoState{gap:true} → 記「要拉」
  每處理完一個事件（含 60 秒閒置逾時）：有「要拉」就 pull_to_device 一次；失敗就留著，下一個事件或下一分鐘再拉（🚫 不原地狂試）
  CryptoState            → 講一聲（OTK 存量的用途是補上傳金鑰：E2EE 的 RPC 面那支）
  訂閱結束（server 送 Error：被另一台裝置接手的 1505；或線死了）→ keys.state: stopped 帶原因、task 結束
                           🚫 不重訂（to-device-client §5.1）、🚫 不關線（房間訂閱還在同一條線上；線真死了房間那半會關）
登出：停兩個 task → Device/Unsubscribe（說出口的退出）→ close_links → 丟掉長活引擎 → 刪 m/
```

- **同一支**：`OlmEngine::import_items(client, items)`＝匯進 crypto store（commit 了才回）→ `cd_seq` 與待銷毀清單落地（`m/td.json`）→
  對 server `ItemsDestroy` 那一批 → 只清回來的。`Fetch` 的一窗與推來的一包都是 `(count, 事件)` 舊→新，差別只在 meta（`Batch` 多 `tc`／`r`／`more`，`Push` 多 `gap`），
  解的那半也共用（`parse_device_items`）。core 不解封包、不碰 store。
- **`Fetch`／`ItemsDestroy` 走訂閱線**：server 只讓持有這台裝置佇列的連線銷毀（to-device-client §8 實跑補的那條），所以 task 用線時跟池 `reuse` 訂閱那一格
  ——`reuse` 只拿開著的線，🚫 不開（開線是 `open_link` 的事，會再跑一次 `init_connection`、換掉 task 自己）。線不在就講一聲：東西還在 server 佇列裡，下次開線的追平會拉回。
- **佇列頭就是水位**（維護者 2026-09-26，wbfuwunel #87）：server 的佇列沒有洞——每一則存到我們 `ItemsDestroy` 才刪（無窮 TTL），`Fetch` 舊→新從頭給。
  所以 `Fetch` 🚫 不帶 `cd_seq`，讓 server 從最舊還沒銷毀的給；`ItemsDestroy` 是唯一的「處理完了」。`m/td.json` 的 `cd_seq` 只是紀錄，🚫 不當游標。
  帶游標的話，游標只要跑到一則還沒進 store 的 item 前面，那則就再也問不到：Ack 前推來的先匯、推播匯失敗後下一包成功、`CryptoState.gap` 沒接，
  三條都會（PR #60 首審 rumia／cirno／salvia 的三條 🔴；中間那版用「不先匯、落後中」補，是在補自己挖的坑）。不帶游標之後這三條在結構上消失，
  代價是「匯了但還沒銷成」的那幾則下次會再回來、重複匯入一次（冪等）。
- ⚠️ **一則永遠匯不進去的 item 會擋住後面全部**：`import_items` 整批匯，它讓所在那一窗每次都回錯、一則都沒銷，而佇列頭永遠是它。寧可卡也不越過（越過是永遠解不開），但卡住要有人看得到、有出口：`keys.state` 不會再發 `caught_up`、`Note` 每分鐘講一次；server 端的逃生口（admin 手動銷、或 `ItemsDestroy` 帶「跳過」語意）在 wbfuwunel #87 第 4 點，等維護者定。📎 `receive_to_device` 只有在某則根本不是 to-device 事件的形狀時才回錯（OlmMachine 對解不開的是略過不是報錯），所以這條靠的是 server 給的形狀本來就對。
- **長活的引擎**：`Core::crypto_engines` 一帳號一個 `Arc<OlmEngine>`，第一次要用才開（鑰匙就是 login 用的 `vault.matrix_store_key()`），
  只給 wbf 帳號（matrix-sdk 帳號的金鑰在它自己的 Client 裡）。登出丟掉（Windows 上開著刪不掉 store）。

## 2. 事件

| 什麼時候 | 發什麼 |
|---|---|
| 上線追平完、或推來一包匯完 | `keys.state { user, state: "caught_up", imported, room_keys }`——`room_keys` 是這一輪帶進來的新房間金鑰數，UI 拿它決定要不要重解密文 |
| 訂閱結束（被接手、線死） | `keys.state { user, state: "stopped", reason }`：這台裝置不再收金鑰 |
| 線不在、匯入失敗、拉失敗、CryptoState | `Note`，只是講一聲 |

## 3. 不在這支

- `DeviceChanged`（房間訂閱那個會話送來）→ `refresh_room_devices`：core 還沒有那支例行程序（e2ee-walkthrough §16.6），E2EE 的 RPC 面接。
- OTK／裝置金鑰上傳、`org.wbftw.device_versions` 的宣告：同上。
- 金鑰獨立一條線（設計上五條）與訂閱線死了主動重開：等 server 支援更多連線（維護者）。
- 訂閱線被關的自動重開：同上。

## 4. 測試

- sdk `tests/unit.rs`：`Push` 對 server 向量 `device_push` 解得回 `(count, 事件)`；`bc`／`counts` 對不上、不遞增是 Protocol（跟 `Batch` 同一支）。
- core `key_sync.rs`（`test_support.rs` 的假 server 多答 `Device/Subscribe`（Ack＋CryptoState）、`Fetch`（佇列裡比 `cd_seq` 新的；client 不帶就是從頭，並記下每次帶了什麼）、`ItemsDestroy`（從佇列刪、Ack＋`ItemsDestroyed`）、`Unsubscribe`）：
  `the_line_subscribes_keys_catches_up_and_imports_pushes_through_one_path`——開線前佇列裡有 1、2 → 訂了、拉到、匯完就銷毀、`caught_up{imported:2}`；
  推 3 → 銷毀 [3]；佇列多 4、5 但只推 5 帶 gap → 從佇列頭拉一次：4、5 一起匯、銷毀；關訂閱線 task 收掉。
  `a_taken_over_key_subscription_stops_and_says_so_but_keeps_the_line`——server 對金鑰會話送 Error → task 停、`stopped` 帶原因、🚫 不重訂、線不關、房間那半照收。
  `an_early_push_does_not_let_the_catch_up_skip_older_queued_keys`——`td.json` 先塞一個比佇列還新的舊記錄 99、佇列 1、2、3、Ack 前推 3 → 追平照樣從佇列頭拉：[1,2,3] 一起匯、銷毀、`Fetch` 只帶過 `None`。
  `a_crypto_state_with_gap_pulls_the_queue`——`CryptoState{gap:true}` → 後面跟一次 `Fetch`，佇列裡的拉回。
  `a_later_push_never_makes_an_earlier_queued_key_unreachable_and_failed_pulls_are_retried`——佇列 1、2 只推 2（沒 gap）→ 2 匯銷；
  `CryptoState{gap}` 觸發的 `Fetch` 被注入失敗 →「要拉」留著；推 3 → 3 匯銷、重拉 → 1 回來；三則都銷、每次 `Fetch` 都不帶游標。
- 真 server（`--ignored`）：`a_key_subscription_receives_a_room_key_shared_by_another_device_over_the_real_server`——同一帳號兩台裝置：
  B（core）登入、上傳裝置金鑰、`open_subscriptions`；A（sdk 層）查到 B、把房間金鑰用 to-device 分給 B；B 的 task 收 Push 匯進 store，`caught_up{room_keys ≥ 1}`；B 登出（走退訂）。
- ⚠️ 沒測的：本地收件匣灌爆那條；真 server 的 gap；真正的匯入失敗（假 server 造不出來：OlmMachine 對壞事件是略過不是報錯）；`CryptoState` 不帶 gap 那條 `Note`。

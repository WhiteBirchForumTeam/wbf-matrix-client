# 訂閱線的金鑰那半：`Device/Subscribe`、上線追平、推來一包就匯——推的與拉的走同一支

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

## 1. 形狀

```
池開一條訂閱線（link_pool::open_link）→ init_connection(account, Subscriptions, client)
  ├─ 房間那半（room-sync.md）
  └─ 金鑰那半（init_keys）
       不是 wbf 帳號（金鑰在 matrix-sdk 的 Client 裡）→ 講一聲、跳過；房間那半照常
       Device/Subscribe{device_id}（不帶 cd_seq）→ Ack ＋ CryptoState
       Ack 之前推來的（early pushes）→ 🚫 不匯（下面的追平會連同佇列裡更舊的一起拉回；先匯它水位就越過舊的）
       pull_to_device：從 m/td.json 的 cd_seq 起 Fetch 一窗一窗到 more=false，每窗 import_items   ← 同一支
       發 keys.state: caught_up（匯了幾則、幾把新房間金鑰）
       起收金鑰的 task（握 Arc<OlmEngine>、Arc<LinkPool>、EventSink、訂閱會話）
       ⚠️ 這一段回錯（m/ 開不起來、Subscribe 被拒、追平壞包）＝整條訂閱線沒開成，房間那半也一起：fail loud，
          因為「金鑰沒在收」靠 UI 看不出來，線開不起來看得出來（PR #60 審查 cirno 🟡；接受的取捨）
收金鑰的 task（不變式：**水位不越過任何還沒匯進 store 的 item**——水位只前進、沒回退，金鑰掉了就是永遠解不開）
  Push{gap:false, items} → pool.reuse(Subscriptions) 拿線 → import_items → keys.state: caught_up
  Push{gap:true}         → 🚫 不先匯這包：從水位起 pull_to_device 一次，漏的連同這包一起回來
  import_items 回錯     → 退回從水位 pull（沒落地的那則還在佇列裡，但不會再推一次）；pull 也失敗就標「落後中」：
                           之後每一包推播都改走 pull（🚫 不匯），直到 pull 成功——不然下一包成功就把水位推過那則失敗的
  本地收件匣滿過 / 一包解不開 → 一樣 pull 一次（東西還在 server 佇列裡，沒銷毀前不會掉）
  CryptoState{gap:true}  → 一樣 pull（它跟 Push 共用這條訂閱的 gap 旗，server 給一次就清掉：不接就永遠不知道漏了）
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
- **gap、early push、匯失敗為什麼都不先匯**：同一件事——佇列裡可能還有 count 比這包小、還沒進 store 的；先匯這包水位就過了它們，
  `Fetch{cd_seq}` 再也拉不到（水位沒有回退）。從水位起拉一次，這包連同漏的一起回來；重複拿到無害（匯入與銷毀都冪等，
  server 對已經不在的 count 也回「沒了」）。拉也失敗就標「落後中」，之後的推播一律走拉到拉成；代價是一則永遠匯不進去的 item 會擋住後面的（這是寧可卡也不越過：越過是永遠解不開）。
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
- core `key_sync.rs`（`test_support.rs` 的假 server 多答 `Device/Subscribe`（Ack＋CryptoState）、`Fetch`（佇列裡比 `cd_seq` 新的）、`ItemsDestroy`（從佇列刪、Ack＋`ItemsDestroyed`）、`Unsubscribe`）：
  `the_line_subscribes_keys_catches_up_and_imports_pushes_through_one_path`——開線前佇列裡有 1、2 → 訂了、拉到、匯完就銷毀、`cd_seq`＝2、`caught_up{imported:2}`；
  推 3 → 銷毀 [3]、`cd_seq`＝3；佇列多 4、5 但只推 5 帶 gap → 不先匯 5，從 3 拉一次：4、5 一起匯、銷毀；關訂閱線 task 收掉。
  `a_taken_over_key_subscription_stops_and_says_so_but_keeps_the_line`——server 對金鑰會話送 Error → task 停、`stopped` 帶原因、🚫 不重訂、線不關、房間那半照收。
  `an_early_push_does_not_let_the_catch_up_skip_older_queued_keys`——佇列 1、2、3，Ack 前推 3 → 追平拉到 [1,2,3] 一起匯、銷毀。
  `a_crypto_state_with_gap_pulls_the_queue`——`CryptoState{gap:true}` → 後面跟一次 `Fetch`，佇列裡的拉回。
  `a_failed_import_falls_back_to_the_watermark_and_later_pushes_pull_until_it_succeeds`——假 server 注入 `ItemsDestroy` 失敗（import_items 回錯）且接下來那次 `Fetch` 也失敗 → 標落後；
  下一包推播不匯、改走 `Fetch`（fetch 次數 +1），拉成了兩則都銷毀。
- 真 server（`--ignored`）：`a_key_subscription_receives_a_room_key_shared_by_another_device_over_the_real_server`——同一帳號兩台裝置：
  B（core）登入、上傳裝置金鑰、`open_subscriptions`；A（sdk 層）查到 B、把房間金鑰用 to-device 分給 B；B 的 task 收 Push 匯進 store，`caught_up{room_keys ≥ 1}`；B 登出（走退訂）。
- ⚠️ 沒測的：本地收件匣灌爆那條；真 server 的 gap；`CryptoState` 推來只講一聲。

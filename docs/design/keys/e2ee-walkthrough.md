# E2EE 從建房到退出：每一步發生什麼、我們缺什麼

> 2026-09-15 應維護者要求整理（「以免漏掉甚麼」）：E2EE 每一步發生什麼、wbf 這邊怎麼對應。
> daemon 與 UI 怎麼分工、RPC 長怎樣，權威在 /docs/design/keys/e2ee-rpc.md；金鑰那條線在 /docs/design/keys/key-sync.md。
> ⚠️ **這份是理解用的走讀，不是線上格式的權威**：Matrix 的權威是 spec（client-server API 的 End-to-End Encryption 一章）；
> wbf 的線上格式權威在 wbfuwunel 的 `/docs/design/wbf-wire-format.md`、`wbf-to-device.md`、`docs/bridge-specs/`。
> 📎 標 **（待查 vendor）** 的是我憑理解寫、還沒對 `vendor/matrix-rust-sdk` 逐行確認的細節，實作那一支要先確認。

角色：**我們是 Alice**（裝置 A1）、**Bob**（先有 B1，後來登入 B2）、之後拉進來的 **Carol**（C1）。

## 0. 名詞：三種金鑰、兩種加密

**每台裝置自己的金鑰**（登入時產生，存在 crypto store —— 我們帳號目錄的 `m/`）：

| 金鑰 | 用途 | 怎麼公開 |
|---|---|---|
| Ed25519 指紋金鑰 | 簽名：證明「這台裝置就是它」 | `/keys/upload` 上傳 device keys |
| Curve25519 身分金鑰 | 跟別的裝置建 Olm 通道 | 同上 |
| **一次性金鑰（OTK）** ＋ 一把 fallback key | 別人**第一次**要跟這台建 Olm 通道時，每次拿走一把 | `/keys/upload` 上傳一批；server 記得剩幾把 |

**每個使用者的交叉簽章金鑰**（master、self-signing、user-signing）：用來說「這台新裝置是我本人的」「我驗證過 Bob」。
沒有它，別人只知道「Bob 有一台沒驗證的裝置」。

**兩種加密**：

- **Olm**：兩台裝置之間的**一對一**通道（double ratchet）。**只拿來傳 to-device**：房間金鑰（`m.room_key`）、轉發的金鑰（`m.forwarded_room_key`）、
  SSSS 秘密（`m.secret.send`）、金鑰請求、驗證流程。
- **Megolm**：房間訊息的群組加密。**每個「發送裝置 × 房間」一條**：發送端建一個 outbound session（棘輪＋遞增的 message index），
  把 session key 用 Olm 分別發給**每一台**收件裝置。收件端拿到的是「從第 N 格開始」的 inbound session —— **只解得開第 N 格以後的**。

⭐ **房間訊息走 Megolm；Megolm 的金鑰走 Olm；Olm 通道要先拿到對方的 OTK 才建得起來。** 下面每一步都是這三句話的展開。

## 1. 建房間

```
POST /createRoom
  initial_state: [{ type: m.room.encryption,
                    content: { algorithm: m.megolm.v1.aes-sha2, rotation_period_ms?, rotation_period_msgs? } }]
```

- `m.room.encryption` 一旦出現就**關不掉** —— 所以 `cache.db` 的 `rooms.encrypted` 只升不降（/docs/design/storage/local-cache-db.md §5）。
- 這一步**還沒有任何金鑰**，只是宣告這個房要加密。
- `history_visibility`（`shared`／`invited`／`joined`）決定之後**邀請中**的人要不要也收到金鑰（第 2、8 步）。

**我們**：沒有建房的 RPC；之後可走橋 `CreateRoom`（`0x13/0x20`）。

## 2. 邀請 Bob

```
POST /rooms/{room}/invite { user_id: @bob }
```

- 只改成員狀態，金鑰還沒動。
- Alice 的 client 從這時開始**追蹤 Bob 的裝置清單**（tracked users）。
- 還沒加入的 Bob 會不會在 Alice 說話時收到金鑰：看 `history_visibility`（**待查 vendor**：matrix-sdk 對 `joined` 以外的設定是否把 invited 算進收件人）。

**我們**：沒有邀請的 RPC；之後可走橋 `Invite`（`0x13/0x24`）。

## 3. Alice 第一次說話（最複雜的一步）

matrix-sdk 的 `room.send` 在裡面依序做：

```
① 收件人 = 房間成員（joined，視可見性加 invited）
② /keys/query：問這些人各有哪些裝置、每台的 Ed25519／Curve25519
     ⭐ 只問「髒」的使用者；誰髒了靠 /sync 的 device_lists.changed
③ /keys/claim：對「還沒有 Olm session」的每台裝置各拿一把 OTK → 本地建 outbound Olm session
④ 建 outbound Megolm session（這個房、這台 A1），index = 0
⑤ 對每台收件裝置：m.room_key { room_id, session_id, session_key } 用 Olm 加密 → /sendToDevice
     ⚠️ 依信任策略可能跳過某些裝置 → 對方收到 m.room_key.withheld（說明為什麼沒給）
⑥ 用 Megolm 加密 content → { algorithm, ciphertext, session_id, sender_key*, device_id* }   （* 舊欄位）
⑦ PUT /rooms/{room}/send/m.room.encrypted/{txn_id}
⑧ 同一個 session 之後的訊息只跑 ⑥⑦，index 一直往上
```

**Megolm session 什麼時候換新（輪換）**：

- 滿 `rotation_period_msgs`（預設 100 則）或 `rotation_period_ms`（預設一週）。
- 🚨 **收件裝置集合變少時**（有人離開、被踢、某台裝置被刪）：舊 session key 在被移除的人手上，繼續用就等於讓他解得開之後的訊息。
  matrix-sdk 在 ⑤ 算收件人時發現少了人，就丟掉舊的、開新的（**待查 vendor**：確切的判斷在 `group_sessions` 的 share 流程）。
- 收件人**變多**不必換：把**目前的** session 從**目前的** index 發給新裝置就好。

**我們**：
- matrix-sdk 帳號：`MatrixBackend::send_text` → `room.send`，①–⑧ 全在 matrix-sdk 裡，送之前 `sync_once`。
- wbf 帳號：sdk `encrypt_and_send` 只在本機做⑤的「建／換 session、排好要送的」、再加密、⑦ 是原生 `Event/Send`（`0x14/0x02`）帶 `room_version`；
  ②③與⑤的「送出去」走橋、在後台（`key_share.rs`，/docs/design/keys/e2ee-rpc.md §3.1），送訊息🚫 等它（維護者 2026-10-05）；② 的「誰髒了」改成房間版本號（§16、/docs/design/keys/e2ee-rpc.md §3）。
  加密房的附件走資料平面（/docs/design/rpc-specs/data-plane.md §5、§6）：附件宣告跟密文同一個 `Event/Send`。還沒做：路徑版送檔進加密房（/docs/design/keys/e2ee-rpc.md §8）。

## 4. Bob 的 B1 收到並解開 Alice 的訊息

```
⓪ B1 上線時早已 /keys/upload：device keys ＋ 一批 OTK ＋ fallback key

① to-device 裡有 A1 發來的 m.room.encrypted（Olm，type 0 = pre-key message）
   → 用被 claim 走的那把 OTK 建 inbound Olm session → 解開 → m.room_key
   → crypto store 存 inbound Megolm session：(room, sender_key, session_id, 從 index 0 開始)
   → 那把 OTK 用掉；device_one_time_keys_count 變少 → B1 補傳一批 /keys/upload

② timeline 裡有 m.room.encrypted（Megolm）
   → 用 (room, session_id) 找 inbound session → 用密文裡的 message index 解 → 明文事件
   → 驗證：session 的 sender_key 對不對得上發送裝置；Ed25519 簽名與交叉簽章決定「已驗證／未驗證／未知裝置」
```

🚨 **①② 的先後不保證**：to-device 晚到，timeline 先到 → **UTD（解不開）**。金鑰到了要**按 `session_id` 重解**（matrix-sdk 的 R2D2 就在做這件事）。
📎 Megolm 密文外殼的 `session_id` 與 message index **不用金鑰就讀得到**，所以「哪些列等哪把金鑰」查得出來。

**我們**：
- matrix-sdk 那條（`/sync`、`/messages`）：①② 在 SDK 裡，交出明文（`raw_event` 是 NULL，/docs/design/messages/edits-and-redactions.md §7）。
- WS 那條：① 走 `0x16 Device`（/docs/design/keys/key-sync.md）；② 收到時有金鑰就解、密文明文一起存，金鑰晚到就按 `session_id` 補解（§7、/docs/design/keys/e2ee-rpc.md §6）。

## 5. Bob 回話

與第 3 步對稱，發送裝置換成 **B1**：B1 建**自己的** outbound Megolm session，對 Alice 的每台裝置 `/keys/claim` ＋ `/sendToDevice` 送 `m.room_key`，
Alice 照第 4 步收下。

⭐ 一個房間裡**每個人的每台裝置各有一條** Megolm session —— 熱鬧的房會有很多把房間金鑰。這是「按 `session_id` 精準重解」的前提。

## 6. Bob 在新裝置 B2 登入（最容易漏的一步）

```
① B2 登入：產生自己的 device keys、OTK → /keys/upload
② server 通知所有跟 Bob 同房的人：device_lists.changed 出現 @bob
③ Alice 的 client 把 Bob 標成髒
④ Alice 下一次說話（第 3 步 ②）：/keys/query 看到 B2 → /keys/claim → 把目前的 Megolm session 從目前的 index 發給 B2
```

B2 讀得到什麼：

- ✅ ④ 之後 Alice 送的。
- ❌ **B2 登入前**的歷史：B2 沒有那段金鑰。補救三條：
  - **金鑰備份**：B2 用 recovery key 打開 server 上的備份，拿回 B1 存進去的房間金鑰（matrix-sdk 帳號有 `key-backup`，/docs/design/keys/room-key-backup.md；wbf 帳號還沒接）。
  - **向自己的其他裝置要**：B2 發 `m.room_key_request`，B1 **只在確認 B2 是 Bob 本人（交叉簽章驗證過）時**才轉 `m.forwarded_room_key`。
  - **邀請時一起給歷史金鑰**（MSC4268 的 room key bundle；vendor 的 `OlmMachine::share_room_key_bundle_data`）。

🚨 **Alice 沒收到 ② 的 `device_lists.changed`** → ③ 不會發生 → Alice 一直只發金鑰給 B1 → **B2 永遠解不開 Alice 之後的新訊息**，而 Alice 看不出來。
wbf 帳號用房間版本號守這一步：Alice 帶舊號碼送會被 1506 擋下、補完金鑰才送得出去（§16）。

## 7. 我們這邊怎麼收、怎麼解

matrix-sdk 帳號：金鑰與訊息都在 matrix-sdk 的 `/sync` 裡（只有部分操作前 `sync_once`；daemon 沒有常駐 sync）。wbf 帳號：

```
收金鑰（to-device）
  0x16 Device  Subscribe → Fetch 追平 → Push（/docs/design/keys/to-device-client.md §7、/docs/design/keys/key-sync.md §1）
    → OlmMachine::receive_sync_changes_msc4186（解 Olm、存 inbound session 進 m/）
    → crypto store commit 之後才 ItemsDestroy（/docs/design/keys/to-device-client.md §4：先刪後匯、匯失敗 = 那把金鑰永遠沒了）
    → 回傳的 RoomKeyInfo (room, session_id) → 補解 cache.db 裡這個房、用這把 session、還沒解的（/docs/design/keys/e2ee-rpc.md §6）

收訊息（timeline；房間推播、room.history、sync.recent 三個入口都過 room_crypto::to_incoming）
  寫入前 OlmMachine::decrypt_room_event
    解得開 → IncomingEvent::Decrypted { ciphertext: Some(原樣), cleartext }
            → 密文明文一起存（raw_event 留密文、content_json 是明文、decrypted = 1）
    解不開 → 只存密文、標成沒解；session_id 不另存欄位，補解時從 raw_event 的 content.session_id 查（cache.rs list_undecrypted_ciphertexts）
```

⚠️ **必須用 `receive_sync_changes_msc4186`，🚫 不用 `receive_sync_changes`**：後者把「沒給 OTK 數量」當成 0
（`update_key_counts(…, is_missing_count_zero: true)`），每收一次就以為金鑰用光、產生一批去上傳；前者把「沒給」當「不知道」。

## 8. 把 Carol 拉進來、Carol 說話

```
① Alice /invite Carol → Carol /join
② device_lists.changed 出現 @carol（新的同房使用者）→ Alice 開始追蹤 Carol
③ Alice 下一次說話：/keys/query Carol 的裝置 → /keys/claim → 把目前的 session 從目前的 index 發給 C1（收件人變多，不必輪換）
④ Carol 讀不到加入前的歷史：Megolm 往回解不了；除非有人給她歷史金鑰（key bundle、轉發）
⑤ Carol 說話 = 第 3 步：C1 建自己的 outbound session，發給 A1、B1、B2
```

⚠️ **「Carol 看不看得到加入前的訊息」是兩件分開的事**：server 的 `history_visibility` 管「給不給她密文」，金鑰管「她解不解得開」。

## 9. 把 Bob 踢出去

```
① POST /rooms/{room}/kick { user_id: @bob }
② Alice 下一次說話：收件人裡 B1、B2 不在了 → 收件裝置集合變少 → 🚨 丟掉舊 outbound session，開新的
③ Carol 那邊同理：她下一次說話也會輪換
```

- ✅ Bob **之後**的讀不到：server 不再給他密文，新 session 的金鑰也沒給他。
- ❌ Bob **已經拿到的**撤不回來：以前收過的照樣解得開（E2EE 的本質；「刪除」只能靠 redact 叫大家別顯示）。
- 🚨 自己實作送訊息時，**「收件人集合變少一定輪換」必須有測試** —— 漏了，被踢的人讀得到之後的訊息。

## 10. 自己退出房間

```
① POST /rooms/{room}/leave（之後可 /forget）
② 自己：丟掉這個房的 outbound session；不再收到這個房的事件
③ 其他人：下一次說話時收件人少了 Alice → 輪換 → Alice 讀不到之後的
④ Alice 本地：已收的歷史與 inbound session 還在 → 以前的仍解得開；cache.db 照 /docs/design/messages/edits-and-redactions.md 保留，清不清是 destroy 的事
```

## 11. 容易漏的清單

| # | 容易漏的 | 漏了會怎樣 |
|---|---|---|
| 1 | **裝置清單變動**（`device_lists.changed`／`left`） | 對方新裝置解不開我們的訊息；被移除的裝置可能還收到新金鑰 |
| 2 | **OTK 剩幾把**（`device_one_time_keys_count`）＋ unused fallback keys | 用完之後別人建不了 Olm 通道 → 金鑰送不進來 |
| 3 | **收件人集合變少要輪換** Megolm | 被踢、離開的人讀得到之後的訊息 |
| 4 | **to-device 比 timeline 晚到** | UTD；要按 `session_id` 重解 |
| 5 | **匯入 commit 之後才 `ItemsDestroy`** | 先刪後匯、匯失敗 → 金鑰永遠沒了 |
| 6 | **分享金鑰前的信任檢查**：身分金鑰變了（pin violation）、只發給驗證過的裝置 | 金鑰悄悄發給冒充的裝置 |
| 7 | **`m.room_key.withheld`** | UI 只會說「解不開」，說不出是被刻意不給 |
| 8 | **邀請中的成員要不要給金鑰**（`history_visibility`） | 加入後讀不到邀請期間的，或給太多 |
| 9 | **金鑰備份跟上**（新的 inbound session 要上傳） | 換裝置拿不回歷史 |
| 10 | **`encrypt_room_event_raw` 前一定先分享過金鑰** | 否則 panic（它的文件寫明） |
| 11 | **`get_missing_sessions`／`share_room_key` 同時只跑一個**；crypto store 只有一個持有者 | 重複建 session、狀態錯亂 |
| 12 | **自己的其他裝置也是收件人**（Alice 若有 A2） | A2 看不到自己送的 |
| 13 | **OTK 用掉要補傳**；fallback key 被用過要換 | 同 #2 |
| 14 | **Megolm index 的重播檢查** | 被重播的舊密文當成新訊息 |

## 12. 現況與缺口對照

matrix-sdk 帳號：每一步都在 matrix-sdk 的 `Client` 裡走 HTTP。wbf 帳號：沒有 `Client`（/docs/design/daemon/account-session.md §2），每一步走 WS：

| 步驟 | Matrix HTTP | wbf 帳號走的 | 在哪 |
|---|---|---|---|
| 上傳自己的金鑰 | `/keys/upload` | 橋 `0x17 0x20` | /docs/design/keys/e2ee-rpc.md §5 |
| 查別人的裝置 | `/keys/query` | 橋 `0x17 0x21` | /docs/design/keys/e2ee-rpc.md §2 |
| **誰的裝置變了** | `device_lists.changed`／`left` | **換了形狀**：成員清單帶裝置版本號與房間版本號（`0x13 0x29`）、送出時比對（1506）、`Event/DeviceChanged`（`0x14 0x07`，加速） | §16、/docs/design/keys/e2ee-rpc.md §2–§4 |
| OTK 剩幾把 | `device_one_time_keys_count`、unused fallback keys | `Device/CryptoState`（`0x16 0x08`，每個 `Device/Subscribe` 之後一定跟一個） | /docs/design/keys/e2ee-rpc.md §5 |
| 拿 OTK 建 Olm | `/keys/claim` | 橋 `0x17 0x22` | `share_room_key` |
| **收**房間金鑰 | to-device | `0x16 Device` | /docs/design/keys/key-sync.md |
| **發**房間金鑰 | `/sendToDevice` | 橋 `0x16 0x25` | `share_room_key` |
| 送訊息 | `/send` | `Event/Send`（加密訊息帶 `room_version`） | /docs/design/keys/e2ee-rpc.md §3 |
| 收訊息 | timeline | `Recent`、Push、`room.history` 拉上游 | /docs/design/keys/e2ee-rpc.md §6 |
| 建房、邀請、踢人、離開 | `/createRoom` 等 | 橋批 1 | 還沒做：沒有 RPC |
| 金鑰備份 | `/room_keys/*` | 橋 `0x17 0x30`–`0x3D` | 還沒做：wbf 帳號沒接（/docs/design/keys/e2ee-rpc.md §8） |
| 簽章上傳、交叉簽章金鑰 | `/keys/signatures/upload`、`/keys/device_signing/upload` | 橋 `0x17 0x25`、`0x24`（換金鑰要 UIAA） | 還沒做：交叉簽章（/docs/design/keys/e2ee-rpc.md §8） |

還沒做的，照 /docs/design/keys/e2ee-rpc.md §8 列一次：路徑版送檔進加密房（資料平面那條可以）；wbf 帳號的金鑰備份與「向自己其他裝置要金鑰」；房間自己設的換金鑰期限（`rotation_period_*`）；交叉簽章（分享策略仍是 `AllDevices`）。

## 13. server 補齊之後：只把 matrix-sdk-crypto 當狀態機用，行不行

⭐ **行，而且這正是它設計的用法。** `vendor/matrix-rust-sdk/crates/matrix-sdk-crypto/README.md` 開頭就寫明它是
「a no-network-IO implementation of a state machine」：**把 server 給的東西推進去，把要送的請求拉出來**，網路自己來。
Element 的行動版就是在自己的網路層上用它。wbf 帳號就是這樣做的（`crypto_engine::OlmEngine`）。

分層，由低到高：

| 層 | 是什麼 | 我們用不用 |
|---|---|---|
| `vodozemac` | Olm／Megolm 的**演算法**（棘輪、加解密） | 🚫 不直接用：用它就得自己重寫下一層的全部規則 |
| **`matrix-sdk-crypto` 的 `OlmMachine`** | **狀態機**：裝置追蹤、session 管理、金鑰分享與輪換規則、驗證、備份（`BackupMachine`）、重播檢查、信任判斷 | ✅ **用這一層** |
| `matrix-sdk-sqlite` | crypto store 的 SQLite 實作（就是 `m/`） | ✅ 照用 |
| `matrix-sdk` 的 `Client`／`Room` | 高階 client：自己的 sync 迴圈、HTTP、`room.send` | wbf 帳號 🚫 不用（維護者定的耦合方向：上游 SDK 可以拆掉，crypto 只當引用）；matrix-sdk 帳號照用 |

**狀態機替我們做的**（不用自己寫、也🚫 不該自己寫）：Olm／Megolm 加解密、「哪台裝置缺 Olm session」、「這次該不該輪換」、
OTK 該不該補、金鑰請求與轉發、交叉簽章與驗證狀態、重播檢查、`withheld` 的產生與解讀。

**我們自己要寫的「細節邏輯」**（在 matrix-sdk 裡是 `Client` 做的）：

| # | 要自己寫的 | 對應 OlmMachine 的 API |
|---|---|---|
| 1 | **推進去**：to-device、裝置清單變動、OTK 數量 → 一次一窗 | `receive_sync_changes_msc4186(EncryptionSyncChanges{ to_device_events, changed_devices, one_time_keys_counts, unused_fallback_keys, next_batch_token: None })` |
| 2 | **拉出來送**：一個迴圈把狀態機要送的請求送出去，回應再交回去 | `outgoing_requests()` → 依型別送（KeysUpload／KeysQuery／KeysClaim／ToDevice／SignatureUpload／KeysBackup…）→ `mark_request_as_sent(request_id, response)` |
| 3 | **回應轉型**：把 server 回的 body 解成 ruma 的 response 型別交給 `mark_request_as_sent` | 走橋的 `BridgeReply.body` 就是 Matrix 回應的原樣 JSON，ruma 的 `IncomingResponse` 解得動 |
| 4 | **成員變動時更新追蹤**：加入、邀請、離開 | `update_tracked_users(users)`（離開的人怎麼處理 **待查 vendor**） |
| 5 | **送訊息前的準備**：收件人清單、`EncryptionSettings`（從房間的 `m.room.encryption` 狀態＋我們的信任策略）、補 session、分享金鑰 | `get_missing_sessions(users)` → 送 KeysClaim；`share_room_key(room, users, settings)` → 送 ToDevice |
| 6 | **加密後送出** | `encrypt_room_event_raw(room, type, content)` → WS `Event/Send{ type: m.room.encrypted }` |
| 7 | **解密與重解** | `decrypt_room_event(event, room, settings)`；匯入回傳的 `RoomKeyInfo` 決定重解哪一批 |
| 8 | **信任策略**：只發給驗證過的裝置？身分金鑰變了怎麼辦？ | `EncryptionSettings` 的分享策略、`DecryptionSettings` 的 `sender_device_trust_requirement` —— 🚨 **要明確選，並照抄或刻意偏離 matrix-sdk 的預設**，🚫 不能默默用最寬的 |
| 9 | **鎖**：同一時間只有一個 `get_missing_sessions`／`share_room_key`；crypto store 只有一個持有者 | daemon 裡一個帳號一份長活的 `OlmEngine`（/docs/design/keys/key-sync.md §1） |
| 10 | **金鑰備份迴圈**：新 session 上傳到 server 備份 | `backup_machine()` 產生的請求 → 同 #2（還沒做） |

⚠️ 還要注意兩件事：

- **一個 `m/` 只有一個持有者**：wbf 帳號沒有 `Client`，它的 `m/` 只有 `OlmEngine` 開；matrix-sdk 帳號的 `m/` 只有 `Client` 開。🚫 不要讓兩者同時開同一個 `m/`。
- **「算法現成」不等於「規則現成」**：#4、#5、#8 決定了「誰收得到金鑰」—— 那就是 E2EE 的安全邊界。這幾條每一條都要有測試，特別是第 9 步的輪換與第 6 步的新裝置。

## 14. 要向 server 要的（合成一個 issue）

開成 wbfuwunel #65，都做了：OTK 數量變成獨立的 `Device/CryptoState`；裝置清單變動**沒有照推播的形狀做**，改成房間版本號（§16；理由在 wbfuwunel 的 /docs/design/e2ee-send-guard-problem.md：推播鏈任一環掉了訊息照樣送出、沒人知道）；
`/keys/*`、`/sendToDevice`、`/room_keys/*` 全部上橋。

## 15. client 的建議順序

都做完了：`0x16` 的編解碼（/docs/design/keys/to-device-client.md §8）→ 收金鑰（/docs/design/keys/key-sync.md）→ 收到時解與補解（/docs/design/keys/e2ee-rpc.md §6）→ 自己加密送出、送出前比對房間版本號（/docs/design/keys/e2ee-rpc.md §3；本文 §16）。

## 16. 送出前比對房間版本號：client 要對齊的（issue #45，wbfuwunel 的 /docs/design/wbf-room-device-version.md）

server 不再推「誰的裝置清單變了」那份清單，改成一個**可以在收訊息那一刻檢查的條件**：每個帳號一個裝置版本號 `序號-雜湊`、
每個房間一個房間版本號（u64，client 當不透明的值、只比相不相等），有約定的 client 送加密訊息時帶房間版本號，過期就被 `1506 RoomDevicesChanged` 擋下，**訊息不會送出**。
推播（`Event/DeviceChanged`）只是加速；全掉光也只是多被擋一次，🚫 不是正確性的來源。

### 16.1 四件事，client 端各落在哪

| # | server 給的 | client 端 |
|---|---|---|
| 1 | `Hello.features` 帶 `"org.wbftw.device_versions"` 才推 `DeviceChanged`；**宣告後加密訊息漏帶 `room_version` 會被 `InvalidRequest` 拒** | `protocol::DEVICE_VERSIONS_FEATURE`；`WbfClient::hello(name, features)`。`Misc`／`Rooms` 兩條線宣告（`link_pool::features_of`，/docs/design/keys/e2ee-rpc.md §2）：加密房每則都走 `encrypt_and_send`、帶 `room_version` |
| 2 | 成員清單（`0x13 0x29`／HTTP `/members`）最外層 `org.wbftw.room_version`，每個 `join` 成員 `unsigned["org.wbftw.device_version"]`；橋不再收 `at` | `device_version::RoomDeviceVersions::from_members_body`（沒有號碼就是錯，不是 0）、`diff_from`（誰要重查、誰離開）；`WbfClient::room_device_versions(room_id)`（走橋的 Members，`membership=join`） |
| 3 | `Event/Send` meta 多 `room_version`；只查 `m.room.encrypted`；對不上回 1506，meta 帶目前的號碼 | `SendRequest::room_version`、`WbfErrorCode::RoomDevicesChanged`、`SdkError::current_room_version`；`encrypt_and_send` 帶 `room_version`，1506 回 `SendOutcome::RoomDevicesChanged`；重拿成員、重查是 `refresh_room_devices`，建通道、建／換金鑰、送出去是後台的 `distribute_room_key`（/docs/design/keys/e2ee-rpc.md §3.1），重送是再叫一次（同 `txn_id`，等金鑰就緒） |
| 4 | `Event/DeviceChanged`（`0x14 0x07`）：`{user_id, device_version, rooms, gap}`，跟 `Push` 共用 `id`／`seq`／`gap` | `pack::event::DEVICE_CHANGED`、`protocol::DeviceChangedMeta`（缺 `gap` 當 true）；`Rooms` 線的收包迴圈原樣轉成 `devices.changed`（`room_sync.rs`，/docs/design/keys/e2ee-rpc.md §4） |

順手一起的：`Device/CryptoState`（`0x16 0x08`）→ `pack::device::CRYPTO_STATE`、`protocol::CryptoStateMeta`（`unused_fallback_key_types` 缺欄位是錯，🚫 不補成空：
`[]` 對 OlmMachine 是「都用掉了，該換」，「沒給」是「server 不支援」）。五條向量（`send_encrypted_with_room_version`、
`error_room_devices_changed`、`event_device_changed`、`device_crypto_state`、`device_crypto_state_empty`）都有測試比對。

### 16.2 收到 1506 之後（wbfuwunel 的 /docs/design/wbf-room-device-version.md §7.2 的迴圈；daemon 做到「重拿、交給後台」為止，分金鑰是後台的事，重送是 UI 的事，重送時等金鑰就緒）

```
送 Event/Send{ type: m.room.encrypted, room_version: V }  ──→  Error 1506 { room_version: V' }
  ↓ 訊息沒送出；V' 只用來知道自己過期，🚫 不拿它直接重送（變了的裝置還沒有金鑰）
重拿 Members  → RoomDeviceVersions { room_version: V'', members }      ← 名單與號碼是同一刻的
  ↓ diff_from(上一份)
changed（新加入、版本號變了的）→ 只對他們 /keys/query → 餵 OlmMachine（update_tracked_users／mark_user_as_changed）
left（不在了的）              → 換一把新的房間金鑰（OlmMachine 的 share 策略本來就會做，見 §9）
  ↓ 交給後台對 V'' 分金鑰 → 回 1401 給 UI
（後台，Keys 線）get_missing_sessions → /keys/claim 建好 Olm 通道 → share_room_key 排好（有人離開就換一把）→ /sendToDevice → 全部 Ack → 「對 V'' 就緒」
（UI）帶 V'' 重送（同一個 txn_id）→ daemon 等「對 V'' 就緒」（最多 2 秒，等不到回 1402）→ 加密 → Misc 線 Event/Send
同一個 txn_id 已經送成功過的，server 回原本的 event_id，不會再被擋。加密用的金鑰已經在 server 上了，對方收到訊息時金鑰只會更早、不會更晚。
```

📎 雜湊可以自己驗：`device_version::compute_device_keys_hash(user_id, /keys/query 的回應)` 照 wbfuwunel 的 /docs/design/wbf-room-device-version.md §3.4 重算（黃金向量 `810b7c3be4` 有測試釘住），
對得上表示看到的是同一組金鑰。`unhashable` 是 server 算不出來時的佔位字，永遠對不上，序號照樣前進。
規則裡最容易漏的一條：過濾掉別人的簽章之後 `signatures` 空了，要**整個欄位拿掉**，不是留 `{}`。

### 16.3 分三支

分支都合了。程式碼的落點：協議層（向量、subtype 常數、`room_version`、1506、`DeviceChangedMeta`／`CryptoStateMeta`、裝置版本號）在 `wbf-wire` 的 `pack.rs` 與 `wbf-sdk` 的 `protocol.rs`、`device_version.rs`；
橋的通用入口是 `WbfClient::call_bridge`（先過 `Hello.features` 的 `bridge` 閘門），`room_device_versions`、`send_to_device` 在它上面；
收發金鑰、refresh、加解密是 `crypto_engine::OlmEngine`（feature `matrix`，同一個 `m/` 上的 `OlmMachine`，所有請求走橋）；推播的通道是 `link::WsLink` 的會話表（/docs/design/daemon/ws-receive-dispatch.md）。

🚨 **只在「每則加密訊息都走 `encrypt_and_send`、帶 `room_version`」的連線上宣告 `org.wbftw.device_versions`**：宣告的連線送加密訊息漏帶號碼是 `InvalidRequest`，
沒有「送出前比對」的連線宣告了，就是把自己所有加密訊息擋掉。daemon 的 `Misc`／`Rooms` 兩條線宣告（/docs/design/keys/e2ee-rpc.md §2）。

### 16.4 對真 server 走通時踩到的坑

- **已追蹤的人不會因為 `update_tracked_users` 再查一次**：A 上傳金鑰時狀態機就順手查過自己（那時 B 還沒上傳），之後只靠 `track_users` 永遠看不到 B。
  要的是「這個人變了、重查」——`OlmEngine::mark_users_changed`（走 `device_lists.changed` 同一個入口），也就是 §16.2 裡收到 1506 之後對 `diff.changed` 要做的事。
- **`ItemsDestroy` 只有持有這台裝置佇列的連線能做**（/docs/design/keys/to-device-client.md §8）：拉→匯入→銷毀之前要先訂閱。要一直收用 `device_subscription`；
  `device_subscribe` 只等到 `CryptoState` 就放手，給一次走完的流程。
- **`ItemsDestroyed` 只抄 `id`、`seq` 是 0**（`Ack` 才抄命令的 seq）；向量裡命令的 seq 剛好也是 0，靠向量看不出來。
- ruma 組請求時對要 token 的端點一定要給 token：引擎給一個占位字串、只取 body，真的 `Authorization` 由橋在 server 那端填（client 蓋不掉）。
- **訂閱之後推播隨時會來**：別人 claim 了我一把 OTK，server 立刻推 `CryptoState`（id 是訂閱的 id）。🚫 不假設「下一個收到的就是我的回覆」（維護者 2026-09-21：送一個等一個沒錯，錯的是這個假設）；
  每個 pack 由會話表依 id 交付（/docs/design/daemon/ws-receive-dispatch.md §2），訂閱的 id 底下的每一個（Ack、CryptoState、Push、Superseded）都進訂閱的 handle，順序無所謂。
- **server 的 WS `Event/Send` 把 txn_id 去重鍵在帳號**（`wbf/send.rs` 傳 `sender_device: None`）：同一帳號另一台裝置、甚至另一個房重用 txn_id，會拿到上次那則的 event_id。
  測試要每輪唯一的 txn_id。HTTP 那條是以裝置為鍵的；這個差異要回報給 server。

### 16.5 整套分發邏輯對照 server 的設計（維護者 2026-09-21 要求逐條確認）

server 那邊的設計（wbfuwunel 的 /docs/design/wbf-room-device-version.md §1、§5.1、§6、§7.2；wbfuwunel 的 /docs/design/wbf-event-push.md §1；wbfuwunel 的 /docs/design/wbf-to-device.md §4）一句話：**幾乎都靠版本號**——
房間版本號變了就代表成員或裝置有變，去看誰的裝置版本號不一樣，只重查那個人，狀態機比出哪台裝置新了／沒了，補發或輪換。
推播只是加速，正確性由送出時的 1506 守；下線說出口退訂，上線主動確認一次，訂閱中也沒有空窗。**由 UI 主導什麼時候做；SDK 只負責每一步能正常呼叫、收到東西自動處理對。**

下表是那套邏輯的每一步，對到 client 的方法：

| # | server 設計的那一步 | client 的方法 |
|---|---|---|
| 1 | **點進房間／送出前**拿一次成員清單：房間版本號 ＋ 每個 `join` 成員的裝置版本號（同一刻） | `WbfClient::room_device_versions(room_id)` → `RoomDeviceVersions` |
| 2 | **跟上次那份比**：誰新加入、誰的裝置版本號變了（要重查）、誰不在了（要換房間金鑰） | `RoomDeviceVersions::diff_from(&previous)` → `MembersDiff { changed, left }`。「上次那份」存在 UI（/docs/design/keys/e2ee-rpc.md §1），refresh 與送出時帶回來 |
| 3 | **只重查變了的人的裝置**；哪台裝置新了／沒了由狀態機自己比 | `OlmEngine::mark_users_changed(diff.changed)` → `send_outgoing_requests`（KeysQuery 走橋 `0x17 0x21`）。上游 `OlmMachine` 對每台裝置逐台追蹤，回應進來自己算差 |
| 4 | **自己驗雜湊**：查回來的金鑰照 wbfuwunel 的 /docs/design/wbf-room-device-version.md §3.4 重算 ＝ 清單上那個人的雜湊 → 看到的是同一組 | `OlmEngine::mismatched_device_hashes`（`refresh_room_devices` 裡：對不上再查一次，還不對就 `Protocol` 拒發；`unhashable` 與沒查過的不算） |
| 5 | **補發／輪換房間金鑰**：新裝置補發、有人離開換一把（上游的 sharing strategy 決定） | 只在後台：`OlmEngine::distribute_room_key(client, room, users)` 缺 Olm session 先 claim → 上游 `share_room_key` 建／換、排好 → 逐一 `sendToDevice`（`Keys` 線的橋）；快到期先 `discard_room_key` 提早換；全部 Ack 後 `room_key_state` 是 `Ready` 才拿來加密（/docs/design/keys/e2ee-rpc.md §3、§3.1）。分享策略明確選 `AllDevices`（`crypto_engine::room_key_share_settings`；交叉簽章做好後換 `IdentityBased`，那是唯一要改的地方） |
| 6 | **帶房間版本號送出**；對不上 1506 → daemon 自動重拿房間狀態（交給後台對新狀態分金鑰）、把新的房間狀態放進錯誤的 `data` 回給 UI；重送由 UI 決定（/docs/design/keys/e2ee-rpc.md §3） | `OlmEngine::encrypt_and_send`（金鑰沒就緒就不加密）→ `SendOutcome::{Sent, RoomKeyNotReady, RoomDevicesChanged}`；重拿是 `refresh_room_devices(previous)`；重送是再叫一次（同 `txn_id`）。RPC 是 `room.send_text`＋1401／1402 |
| 7 | **上線主動確認一次**：to-device 追平；開著的房各拿一次成員清單 | `OlmEngine::pull_to_device`（/docs/design/keys/key-sync.md §1）；成員清單就是第 1 步，什麼時候叫由 UI 決定 |
| 8 | **訂閱中也沒有空窗**：`DeviceChanged` 推來就更新號碼、標記重查；掉了有 `gap`；全掉光最壞被 1506 擋一次 | `DeviceChangedMeta`；daemon 原樣轉成 `devices.changed`，**要不要叫 refresh 是 UI 的事**（/docs/design/keys/e2ee-rpc.md §4）。正確性仍由第 6 步的 1506 守 |
| 9 | **下線就關掉訂閱**：說出口的退出；斷線 server 也自動退（兩條路都要有） | `WbfClient::device_unsubscribe()`（core `unsubscribe_keys_of`） |
| 10 | **同一台裝置只有一條連線在收**；被接手的那條收到 `Superseded` 要停、不重訂 | 它的 id 是訂閱的 id，會話表把它交進訂閱的 handle 當終點（`Subscription::next` 先回那則 Error、再回 None）；core 的 task 停、發 `keys.state: stopped`（/docs/design/keys/to-device-client.md §5.1） |
| 11 | **UI 主導、SDK 自動**：SDK 收到東西自己搞定，UI 只決定什麼時候叫哪個方法 | 上面每一列都是可呼叫的方法；`import_items` 把「匯入 → 落地 → 銷毀」鎖成一步；daemon 的 RPC 面見 /docs/design/keys/e2ee-rpc.md |

走的就是 server 設計的那條「版本號變了 → 看誰不一樣 → 只查那個人 → 狀態機比裝置 → 補發 → 帶號碼送 → 1506 回到第 1 步」的路。
正確性放在 1506、不放在推播：推播全掉也只是慢一拍（被擋一次才知道要重查）。

🚫 「上次那份成員清單」不在 SDK 層存，daemon 也🚫 不存：它**存在 UI**（維護者 2026-09-29，/docs/design/keys/e2ee-rpc.md §1，`RoomDevices`），送出與 refresh 時帶回來。「這輪金鑰發給誰」由 crypto store 裡上游自己記。

### 16.6 誰呼叫：UI 與 daemon 的分界（維護者 2026-09-21 定、2026-09-29 精確化）

每個動作誰叫、什麼時候叫、回什麼，權威在 /docs/design/keys/e2ee-rpc.md（§1 狀態放哪、§2 refresh、§3 送出與 1401、§4 `devices.changed`、§5 自己的金鑰、§6 解密）。這裡只留原則。

原則一句話：**訊息是 UI 的，金鑰是 daemon 的。** UI 決定什麼時候確認、什麼時候送、要不要重送；daemon 負責金鑰永遠補齊。
wbf-sdk 只提供方法，不在這兩者之間選邊。

1506 其實是兩件事疊在一起，分開給：

- **金鑰面**（daemon）：房間版本號過期 ＝ 現在的裝置集合裡有人沒拿到房間金鑰。修法是成員清單 → 比出誰變了 → 重查那個人 → 交給後台建通道、補發或輪換、送到，
  然後把新的房間狀態放進錯誤的 `data` 交回 UI（🚫 等後台；UI 重送時才等金鑰就緒）。只有 daemon 做得到（OlmMachine 與 crypto store 在它手上），而且不管 UI 之後重不重送都該做——這是金鑰衛生，不是那則訊息的事。
- **訊息面**（UI）：這則要不要再送、送幾次、畫面顯示傳送中還是失敗。daemon 🚫 不自動重送：使用者可能已經撤回或改了，daemon 不會知道；重試次數是每則訊息的政策，放 UI 才自然。

⭐ **一支例行程序、兩種觸發**：`refresh_room_devices` 由 UI 叫（點進房間、收到 `devices.changed`、自己覺得號碼舊了），或由 daemon 在被 1506 擋時自動叫。內容一樣，只有「誰按下去」不同。
UI 帶新狀態、同一個 `txn_id` 重送；之間版本號又變就再吃一次 1506，由 UI 的次數自然收斂，daemon 不幫忙。
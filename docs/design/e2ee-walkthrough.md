# E2EE 從建房到退出：每一步發生什麼、我們缺什麼

> 2026-09-15 應維護者要求整理（「以免漏掉甚麼」）。目的是讓「E2EE 全走 WS」拆 server issue、拆 client 分支時有一份共同的底圖。
> ⚠️ **這份是理解用的走讀，不是線上格式的權威**：Matrix 的權威是 spec（client-server API 的 End-to-End Encryption 一章）；
> wbf 的線上格式權威在 wbfuwunel 的 `docs/design/wbf-wire-format.md`、`wbf-to-device.md`、`docs/bridge-specs/`。
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

- `m.room.encryption` 一旦出現就**關不掉** —— 所以 `cache.db` 的 `rooms.encrypted` 只升不降（local-cache-db.md §6）。
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

**我們**：`MatrixBackend::send_text` → `room.send`，①–⑧ 全在 matrix-sdk 裡，送之前 `sync_once`。
**改走 WS 的缺口**：②③⑤ 的 `/keys/query`、`/keys/claim`、`/sendToDevice` 不在橋上；② 的「誰髒了」要 `device_lists.changed`，WS 沒有；
⑦ 可以換成原生 `Event/Send`（`0x14/0x02`），而且**能帶 `attachments`**（附件宣告，約定 §5.2 一直卡住的那件事）。

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
- matrix-sdk 那條（`/sync`、`/messages`）：①② 在 SDK 裡，交出明文（`raw_event` 是 NULL，local-cache-db.md §7.7）。
- WS 那條（`Recent`）：只有 ② 的密文，而且**不解**，存成 `general`。

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
  - **金鑰備份**：B2 用 recovery key 打開 server 上的備份，拿回 B1 存進去的房間金鑰（我們有 `key-backup`，local-cache-db.md §10）。
  - **向自己的其他裝置要**：B2 發 `m.room_key_request`，B1 **只在確認 B2 是 Bob 本人（交叉簽章驗證過）時**才轉 `m.forwarded_room_key`。
  - **邀請時一起給歷史金鑰**（MSC4268 的 room key bundle；vendor 的 `OlmMachine::share_room_key_bundle_data`）。

🚨 **Alice 沒收到 ② 的 `device_lists.changed`** → ③ 不會發生 → Alice 一直只發金鑰給 B1 → **B2 永遠解不開 Alice 之後的新訊息**，而 Alice 看不出來。
這是「送訊息改走 WS 之前，一定要先把裝置清單變動搬過來」的原因。

## 7. 我們這邊怎麼收、怎麼解（目標）

```
收金鑰（to-device）
  現在：matrix-sdk 的 /sync（只有部分操作前 sync_once；daemon 沒有常駐 sync）
  目標：0x16 Device  Subscribe → Fetch 補洞 → Push（to-device-client.md §7）
        → OlmMachine::receive_sync_changes_msc4186（解 Olm、存 inbound session 進 m/）
        → crypto store commit 之後才 ItemsDestroy（to-device-client.md §4：先刪後匯、匯失敗 = 那把金鑰永遠沒了）
        → 回傳的 RoomKeyInfo (room, session_id) → 重解 cache.db 裡對應的 general 列

收訊息（timeline）
  現在：WS Recent → 存成 general（密文）
  目標：寫入前 OlmMachine::decrypt_room_event
        解得開 → IncomingEvent::Decrypted { ciphertext: Some(原樣), cleartext }
                → cache.upsert_events 把 general 補成真正的類別（§7 已經做好；raw_event 留密文）
        解不開 → 維持 general，記下 megolm_session_id，等金鑰到了重解
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
④ Alice 本地：已收的歷史與 inbound session 還在 → 以前的仍解得開；cache.db 照 local-cache-db.md §7 保留，清不清是 destroy 的事
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

🔁 **2026-09-20 更新**：寫這份時 WS 上缺的那一半，server 都補齊了（wbfuwunel `wbf-e2ee.md`、`wbf-room-device-version.md`，PR #67–#75）。
「WS 上有嗎」那欄寫的是 **server 現在的樣子**；「現在」那欄仍是 client 目前走的路。client 要怎麼跟上見 §16。

| 步驟 | 需要 | 現在 | WS 上有嗎 |
|---|---|---|---|
| 上傳自己的金鑰 | `/keys/upload` | matrix-sdk | ✅ 橋 `0x17 0x20` |
| 查別人的裝置 | `/keys/query` | matrix-sdk | ✅ 橋 `0x17 0x21` |
| **誰的裝置變了** | `device_lists.changed`／`left` | `/sync` | ✅ **換了形狀**：成員清單帶裝置版本號與房間版本號（`0x13 0x29`）、送出時比對（1506）、`Event/DeviceChanged`（`0x14 0x07`，加速）；§16 |
| OTK 剩幾把 | `device_one_time_keys_count`、unused fallback keys | `/sync` | ✅ `Device/CryptoState`（`0x16 0x08`，每個 `Device/Subscribe` 之後一定跟一個） |
| 拿 OTK 建 Olm | `/keys/claim` | matrix-sdk | ✅ 橋 `0x17 0x22` |
| **收**房間金鑰 | to-device | `/sync` | ✅ `0x16 Device` |
| **發**房間金鑰 | `/sendToDevice` | matrix-sdk | ✅ 橋 `0x16 0x25` |
| 送訊息 | `/send` | matrix-sdk | ✅ `Event/Send`（帶 `attachments`；加密訊息再帶 `room_version`） |
| 收訊息 | timeline | `/sync`、`/messages` | ✅ `Recent`、Push |
| 建房、邀請、踢人、離開 | `/createRoom` 等 | 沒做 | ✅ 橋批 1 |
| 金鑰備份 | `/room_keys/*` | matrix-sdk | ✅ 橋 `0x17 0x30`–`0x3D` |
| 簽章上傳、交叉簽章金鑰 | `/keys/signatures/upload`、`/keys/device_signing/upload` | matrix-sdk | ✅ 橋 `0x17 0x25`、`0x24`（換金鑰要 UIAA） |

📎 原本這裡寫「收這一半夠用、送這一半缺四支端點與裝置清單變動」——現在兩半都在 WS 上了。順序仍然是先收後送（§15），理由變成「收不必等任何人、送要先把§16 做完」。

## 13. server 補齊之後：只把 matrix-sdk-crypto 當狀態機用，行不行

⭐ **行，而且這正是它設計的用法。** `vendor/matrix-rust-sdk/crates/matrix-sdk-crypto/README.md` 開頭就寫明它是
「a no-network-IO implementation of a state machine」：**把 server 給的東西推進去，把要送的請求拉出來**，網路自己來。
Element 的行動版就是在自己的網路層上用它。

分層，由低到高：

| 層 | 是什麼 | 我們用不用 |
|---|---|---|
| `vodozemac` | Olm／Megolm 的**演算法**（棘輪、加解密） | 🚫 不直接用：用它就得自己重寫下一層的全部規則 |
| **`matrix-sdk-crypto` 的 `OlmMachine`** | **狀態機**：裝置追蹤、session 管理、金鑰分享與輪換規則、驗證、備份（`BackupMachine`）、重播檢查、信任判斷 | ✅ **用這一層** |
| `matrix-sdk-sqlite` | crypto store 的 SQLite 實作（就是 `m/`） | ✅ 照用 |
| `matrix-sdk` 的 `Client`／`Room` | 高階 client：自己的 sync 迴圈、HTTP、`room.send` | 🚫 E2EE 全走 WS 之後可以不用（維護者定的耦合方向：上游 SDK 可以拆掉，crypto 只當引用） |

**狀態機替我們做的**（不用自己寫、也🚫 不該自己寫）：Olm／Megolm 加解密、「哪台裝置缺 Olm session」、「這次該不該輪換」、
OTK 該不該補、金鑰請求與轉發、交叉簽章與驗證狀態、重播檢查、`withheld` 的產生與解讀。

**我們自己要寫的「細節邏輯」**（原本在 matrix-sdk 的 `Client` 裡）：

| # | 要自己寫的 | 對應 OlmMachine 的 API |
|---|---|---|
| 1 | **推進去**：to-device、裝置清單變動、OTK 數量 → 一次一窗 | `receive_sync_changes_msc4186(EncryptionSyncChanges{ to_device_events, changed_devices, one_time_keys_counts, unused_fallback_keys, next_batch_token: None })` |
| 2 | **拉出來送**：一個迴圈把狀態機要送的請求送出去，回應再交回去 | `outgoing_requests()` → 依型別送（KeysUpload／KeysQuery／KeysClaim／ToDevice／SignatureUpload／KeysBackup…）→ `mark_request_as_sent(request_id, response)` |
| 3 | **回應轉型**：把 server 回的 body 解成 ruma 的 response 型別交給 `mark_request_as_sent` | 走橋的 `BridgeReply.body` 就是 Matrix 回應的原樣 JSON，ruma 的 `IncomingResponse` 解得動（**待查**：確切轉法） |
| 4 | **成員變動時更新追蹤**：加入、邀請、離開 | `update_tracked_users(users)`（離開的人怎麼處理 **待查 vendor**） |
| 5 | **送訊息前的準備**：收件人清單、`EncryptionSettings`（從房間的 `m.room.encryption` 狀態＋我們的信任策略）、補 session、分享金鑰 | `get_missing_sessions(users)` → 送 KeysClaim；`share_room_key(room, users, settings)` → 送 ToDevice |
| 6 | **加密後送出** | `encrypt_room_event_raw(room, type, content)` → WS `Event/Send{ type: m.room.encrypted, attachments }` |
| 7 | **解密與重解** | `decrypt_room_event(event, room, settings)`；匯入回傳的 `RoomKeyInfo` 決定重解哪一批 |
| 8 | **信任策略**：只發給驗證過的裝置？身分金鑰變了怎麼辦？ | `EncryptionSettings` 的分享策略、`DecryptionSettings` 的 `sender_device_trust_requirement` —— 🚨 **要明確選，並照抄或刻意偏離 matrix-sdk 的預設**，🚫 不能默默用最寬的 |
| 9 | **鎖**：同一時間只有一個 `get_missing_sessions`／`share_room_key`；crypto store 只有一個持有者（別讓 matrix-sdk `Client` 同時開著同一個 `m/`） | 自己加（daemon 裡一個帳號一份 OlmMachine） |
| 10 | **金鑰備份迴圈**：新 session 上傳到 server 備份 | `backup_machine()` 產生的請求 → 同 #2 |

⚠️ 還要注意兩件事：

- **換掉 matrix-sdk `Client` 是分階段的**：送訊息、金鑰備份、驗證流程現在都還在 `Client` 上。過渡期兩者**共用同一個 `m/`**，所以 #9 的「只有一個持有者」要先想清楚（同一個程序裡共用同一個 OlmMachine，或保證不同時開）。
- **「算法現成」不等於「規則現成」**：#4、#5、#8 決定了「誰收得到金鑰」—— 那就是 E2EE 的安全邊界。這幾條每一條都要有測試，特別是第 9 步的輪換與第 6 步的新裝置。

## 14. 要向 server 要的（合成一個 issue）

✅ 開成 wbfuwunel #65，三件都做完了：1 變成 `Device/CryptoState`（不帶 `Batch`／`Push`，獨立一個 pack）；2 **沒照這裡提的形狀做**，
改成房間版本號（§16，理由在 server 的 `e2ee-send-guard-problem.md`：推播鏈任一環掉了訊息照樣送出、沒人知道）；3 全部上橋，連備份一起。

1. `0x16` 的 `Batch`／`Push` 帶 **OTK 數量**與 **unused fallback keys**（相容的增補）。
2. **裝置清單變動**（`changed`／`left`）走 WS 推送（放 `0x16`，或另一個訂閱）。
3. `/keys/upload`、`/keys/query`、`/keys/claim`、`/sendToDevice` 搬上橋（批 2）；金鑰備份的 `/room_keys/*` 視需要一起。

## 15. client 的建議順序

1. `wbf-wire`／`wbf-sdk`：`Kind::Device` 七個 subtype 的編解碼，對現有向量（`device_subscribe`…`device_items_destroyed`）逐 byte 驗。
2. core：訂閱、補洞、匯入 OlmMachine、`cd_seq` 與待銷毀清單落在 `m/`、`ItemsDestroy`；自己送 `outgoing_requests`（過渡期 HTTP）。**會叫 server 刪東西，審查最嚴。**
3. core：WS 密文當場解；`cache.db` 存 `megolm_session_id`；按匯入回傳的 `RoomKeyInfo` 重解。
4. （server 補齊裝置清單變動與批 2 之後）送訊息改成自己加密＋`Event/Send`＋`attachments`，同時把 #11 清單裡跟「送」有關的條目逐條寫成測試。

📎 2026-09-20 起，第 4 步的括號已經成立（§12）。但「送」多了一件事：**送出前比對房間版本號**（§16），做完才能在 `Hello` 宣告。

## 16. 送出前比對房間版本號：client 要對齊的（issue #45，server 的 `wbf-room-device-version.md`）

server 不再推「誰的裝置清單變了」那份清單，改成一個**可以在收訊息那一刻檢查的條件**：每個帳號一個裝置版本號 `序號-雜湊`、
每個房間一個房間版本號（u64），有約定的 client 送加密訊息時帶房間版本號，過期就被 `1506 RoomDevicesChanged` 擋下，**訊息不會送出**。
推播（`Event/DeviceChanged`）只是加速；全掉光也只是多被擋一次，🚫 不是正確性的來源。

### 16.1 四件事，client 端各落在哪

| # | server 給的 | client 端 | 狀態 |
|---|---|---|---|
| 1 | `Hello.features` 帶 `"org.wbftw.device_versions"` 才推 `DeviceChanged`；**宣告後加密訊息漏帶 `room_version` 會被 `InvalidRequest` 拒** | `protocol::DEVICE_VERSIONS_FEATURE`；`WbfClient::hello(name, features)` | ✅ 常數與參數在；🚫 **16.3 做完之前沒有任何一條路可以宣告它** |
| 2 | 成員清單（`0x13 0x29`／HTTP `/members`）最外層 `org.wbftw.room_version`，每個 `join` 成員 `unsigned["org.wbftw.device_version"]`；橋不再收 `at` | `device_version::RoomDeviceVersions::from_members_body`（沒有號碼就是錯，不是 0）、`diff_from`（誰要重查、誰離開） | ✅ `WbfClient::room_device_versions(room_id)`（走橋的 Members，`membership=join`） |
| 3 | `Event/Send` meta 多 `room_version`；只查 `m.room.encrypted`；對不上回 1506，meta 帶目前的號碼 | `SendRequest::room_version`、`WbfErrorCode::RoomDevicesChanged`、`SdkError::current_room_version` | ✅ 協議層在；`/keys/*` 與發 to-device 走橋的入口在（`call_bridge`、`send_to_device`）；重查→補發→重送的迴圈等 OlmMachine |
| 4 | `Event/DeviceChanged`（`0x14 0x07`）：`{user_id, device_version, rooms, gap}`，跟 `Push` 共用 `id`／`seq`／`gap` | `pack::event::DEVICE_CHANGED`、`protocol::DeviceChangedMeta`（缺 `gap` 當 true） | ✅ 解得開；收包迴圈還沒分派它 |

順手一起的：`Device/CryptoState`（`0x16 0x08`）→ `pack::device::CRYPTO_STATE`、`protocol::CryptoStateMeta`（`unused_fallback_key_types` 缺欄位是錯，🚫 不補成空：
`[]` 對 OlmMachine 是「都用掉了，該換」，「沒給」是「server 不支援」）。向量檔重抄，五條新向量（`send_encrypted_with_room_version`、
`error_room_devices_changed`、`event_device_changed`、`device_crypto_state`、`device_crypto_state_empty`）都有測試比對。

### 16.2 收到 1506 之後（§7.2 的迴圈，client 的政策：重試一次）

```
送 Event/Send{ type: m.room.encrypted, room_version: V }  ──→  Error 1506 { room_version: V' }
  ↓ 訊息沒送出；V' 只用來知道自己過期，🚫 不拿它直接重送（金鑰還沒補發）
重拿 Members  → RoomDeviceVersions { room_version: V'', members }      ← 名單與號碼是同一刻的
  ↓ diff_from(上一份)
changed（新加入、版本號變了的）→ 只對他們 /keys/query → 餵 OlmMachine（update_tracked_users／mark_user_as_changed）
left（不在了的）              → 換一把新的房間金鑰（OlmMachine 的 share 策略本來就會做，見 §9）
  ↓ get_missing_sessions → /keys/claim；share_room_key → /sendToDevice（都走橋）
帶 V'' 重送（同一個 txn_id）。同一個 txn_id 已經送成功過的，server 回原本的 event_id，不會再被擋。
```

📎 雜湊可以自己驗：`device_version::compute_device_keys_hash(user_id, /keys/query 的回應)` 照 server §3.4 重算（黃金向量 `810b7c3be4` 有測試釘住），
對得上表示看到的是同一組金鑰。`unhashable` 是 server 算不出來時的佔位字，永遠對不上，序號照樣前進。
規則裡最容易漏的一條（server PR #75 才抓到）：過濾掉別人的簽章之後 `signatures` 空了，要**整個欄位拿掉**，不是留 `{}`。

### 16.3 分三支

| 支 | 內容 | 宣告 feature？ |
|---|---|---|
| **1**（✅ PR #46 已合併） | 協議層：向量、subtype 常數、`room_version`、1506、`DeviceChangedMeta`／`CryptoStateMeta`、`device_version` 模組。**行為不變** | 🚫 |
| **2**（✅ 已做） | 橋的通用入口 `WbfClient::call_bridge`（先過 `Hello.features` 的 `bridge` 閘門；`IS_BRIDGED` 的請求／回覆，形狀取自 PR #43）＋ `Kind::Room`／`Keys`；`room_device_versions` 打 Members；`send_to_device`；`/keys/*` 五支的 `BridgedEndpoint` 常數（第 3 支的 OlmMachine 迴圈直接用 `call_bridge`） | 🚫 |
| 3 | OlmMachine 的 `outgoing_requests` 迴圈、送出前比對、16.2 的迴圈、收包分派 `DeviceChanged`／`CryptoState`；**#45 的驗收**（Bob 登新裝置 → 帶舊號碼送 → 1506 → 修 → 重送 → Bob 新裝置收到房間金鑰）走通 | ✅ 這一支才開 |

🚨 為什麼 1、2 不能宣告：宣告的連線送加密訊息漏帶號碼是 `InvalidRequest`，而 1、2 還沒有「送出前比對」——宣告了就是把自己所有加密訊息擋掉。

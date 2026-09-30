# 事件的處理：原始事件永遠不動，最終內容另存（維護者 2026-09-14 定）

> **狀態：已實作（schema v5）**，§7 第 1、4 點還沒拍板，實作怎麼繞開它們見 §8。
> §1 描述的是改之前的樣子，留著當理由。

## 1 為什麼要改：現在的做法是「轉換完才存」

現在一窗事件回來，先過 `event_json::messages_from_json` 轉成 `Message`、再 `upsert_messages` 寫進 `events.message_json`：

```
server 事件 JSON ──messages_from_json──> Vec<Message> ──upsert_messages──> events（一則一列）
                        │
                        └─ aggregate：reaction／edit／redaction 只折進「同一批」裡的目標
```

實測（2026-09-14）看到的行為：

| 情況 | 本地存成 |
|---|---|
| 目標在前一批，edit／reaction／redaction 在後一批 | 目標不變；edit 變成一則獨立的 `"* changed"` 文字訊息；reaction、redaction 各是一列 `Unsupported` |
| 已經解密存進來的加密訊息，之後 server 回它被 redact 的版本（`decrypted=0`） | 「明文不被密文蓋」那條規則擋下，**本地仍是原本的明文** |

⚠️ 維護者：**這是 client 的處理策略，不是協議上的 bug**。`recent` 是每個 `Batch`（預設 10 則）各叫一次 `aggregate`，
而事件新到舊送來，edit／redaction 永遠比目標新 —— 所以「只折進同一批」在實務上幾乎都折不到。
⭐ 要改的是**模型**：把「收到了什麼」和「最後該顯示什麼」分成兩件事存。

## 2 原則

- 🚫 **原始事件永遠不改**。收到什麼存什麼：密文就留著密文，被 redact 了也還在 —— 所以可以復原。
- 🚫 **每一則自己的明文也不改**（維護者 2026-09-14 改定）：解密結果寫進 `content_json` 之後就不動，edit 🚫 不寫回目標。
  目標那一列只記「目前要顯示哪個 edit」（`ref_event_id`）與「最後一次變動的 server 時間」（`modified_timestamp`），顯示時引用（§5）；
  redact 只在目標打勾 `is_redacted`。
- ⭐ **先解密再寫入**：寫進去的時候就已經處理過，只有解不開的才停在「未處理」。
  wbf 帳號 2026-09-29 起照這條做：收到時有金鑰就解；解不開的，金鑰晚到時由金鑰那半找出來補解、補寫 `content_json`（`raw_event` 的密文不動，e2ee-rpc.md §6）。
- 📎 **redact 要不要真的清掉本地的唯一快取，是裝置端的選擇，🚫 不是協議保證**（維護者 2026-09-14）。
  client 選擇不清，redact 對它就是「標記」而不是「抹除」—— 在這個 client 上不算 bug。

## 3 `events` 表的欄位

| 欄 | 型別 | 預設 | 意思 |
|---|---|---|---|
| **`raw_event`** | TEXT | NULL | 原始事件 JSON。**原本的 `message_json` 改名**；⚠️ 內容也變了：以前存的是轉換過的 `Message`，之後存 server 給的原樣。🚫 第一次寫入之後**永遠不覆蓋**（只從 NULL 補）。matrix-sdk 解開的事件拿不到密文 → NULL（§7 第 3 點） |
| **`content_json`** | TEXT | **NULL** | 這一則**自己的**明文 `content`（edit 是它的 `m.new_content`）。⭐ **NULL ＝還沒處理**（維護者：用 NULL 而不是空字串，才分得出處理過沒有）。🚫 寫進去就不改 —— edit 不寫回目標（維護者 2026-09-14 從 `final_body` 改名改義） |
| **`is_processed`** | BOOL | 0 | 這一則處理完了沒。解不開的是 0；edit／redact 的效果在**目標**上，目標還沒到（或還沒解開）之前也是 0；其他分完類就是 1 |
| **`is_redacted`** | BOOL | 0 | 被 redact 過就打勾。**redact 優先**：打勾之後顯示「已刪除」，任何 edit 都不看 |
| **`class`** | | `general` | `general`（還不知道是什麼的密文）／`msg`／`edit`／`redact`／`reaction` |
| **`ref_event_id`** | TEXT | NULL | **兩種意思，看 `class`**（維護者 2026-09-14）：edit／redact／reaction → 指向的**目標的 `event_id`**；msg → **目前要顯示的那個 edit 的 `event_id`**（沒被 edit 過是 NULL）。**加索引** `(room, ref_event_id)`（反查「誰參照這則」）。⚠️ 所以每一條用它的查詢都要帶 `class` 條件 |
| **`modified_timestamp`** | INTEGER | **0** | 這一列最後一次變動的 server 時間：寫入時＝**0**（所以第一個 edit 一定贏，發送端時鐘偏了也一樣）；換成新的 edit 時＝那個 edit 的 `origin_server_ts`；被 redact 時＝那個 redact 的。edit 寫入時拿它**直接比大小**，讀取不必再查參照（§5） |

⚠️ **`ref_event_id` 是 local-cache-db.md §5 第 2 條原則的刻意例外**（「字串識別碼在整個 DB 裡各只出現一次，其餘全走整數外鍵」）：
這裡**沒辦法用外鍵**，因為目標那一列**可能還不存在**（只先讀到 edit／redact）。
📎 只有關係事件才填，數量遠少於訊息。
⭐ 用 `event_id` 而不是 `g_seq` 參照（維護者 2026-09-14）：關係事件本身就帶著目標的 `event_id`（`m.relates_to.event_id`、`redacts`），
寫入當下就填得進去；🚫 **不帶目標的 `g_seq`**，要等目標到了才知道 —— 而且一般 Matrix server 根本沒有 `g_seq`。
🚫 **不另加外鍵欄**（維護者 2026-09-14 討論過）：`ref_event_id` 加索引已經查得到「誰參照這則」，外鍵只是同一個關係的第二份。

## 4 各類事件怎麼處理

```
收到事件 → 先解密（明文事件不用解）
  ├─ 解不開           → class=general，content_json=NULL，is_processed=0
  ├─ msg              → content_json=內容，class=msg，is_processed=1
  ├─ edit             → content_json=new_content，class=edit，ref_event_id=目標
  │                     目標在本地：照 §5 決定換不換目標的 ref_event_id／modified_timestamp，自己 is_processed=1
  │                     不在（或還沒解開）：自己 is_processed=0 等著；目標的 content_json 🚫 永遠不動
  ├─ redact           → class=redact，ref_event_id=目標
  │                     目標在本地：目標 is_redacted=1、modified_timestamp=redact 的時間，自己 is_processed=1；不在：等著
  └─ reaction         → class=reaction，ref_event_id=目標，is_processed=1（讀取時聚合）
```

- **明文事件**（沒加密的房間）：收到當下就分類，不用等解密。
- ⭐ **目標還沒到本地**：edit、redact 停在 `is_processed=0`（reaction 不用等，讀取時聚合）。
  **目標寫入時，先查「誰參照這則」**（維護者 2026-09-14），redact 先、edit 後：

  ```sql
  SELECT id FROM events WHERE room = ? AND ref_event_id = <目標的 event_id> AND class = 'redact' AND is_processed = 0
  SELECT id FROM events WHERE room = ? AND ref_event_id = <目標的 event_id> AND class = 'edit'   AND is_processed = 0
  ```

  有索引，這一查很快。不另開暫存表 —— `is_processed=0` 加上 `ref_event_id` 就是那張表。
- **reaction 聚合**：讀取時用同一個索引查 `class = reaction AND ref_event_id = <這則>`。

## 5 edit：全量取代，目標記著目前顯示哪一個

⭐ **Matrix 的 edit 是全量修改，不是 delta**（查證於 vendor 的 matrix-sdk，它照 spec v1.17「validity of replacement events」）：

- `m.new_content` 是**一份完整的新 content**，缺它就是無效的 edit（`MissingNewContent`）。
- 🚫 **不能 edit 一個 edit**（`OriginalEventIsReplacement`）：每個 edit 都直接指向**原始事件**，沒有鏈。
- 有多個 edit 時**用最新的那一個**，🚫 不重播中間的（matrix-sdk-ui 的 `resolve_edits`）。

所以不必記錄或重播 edit 的歷史，**只要知道最新的是哪一個**，而且 🚫 **不必把它寫回目標**（維護者 2026-09-14）：
全量的 `m.new_content` 本來就存在 edit 自己那列，目標只記**指向它**（`ref_event_id`）與它的時間（`modified_timestamp`）。

**寫入一個 edit 時**（維護者 2026-09-14）：

```
目標不在本地（或還沒解開）                         → edit 等著（is_processed=0），目標寫入時再處理
目標在本地 AND 目標 is_redacted=0 AND edit 有效（下表）AND edit 比目前的新
                                                    → 目標.ref_event_id = edit.event_id
                                                      目標.modified_timestamp = edit.origin_server_ts
否則                                                → 跳過
（目標在本地時，不管換不換，edit 自己 is_processed=1；目標的 content_json 🚫 永遠不動）
```

「比目前的新」**一律看 `modified_timestamp`**（維護者 2026-09-14）：`(edit.origin_server_ts, edit.event_id) > (目標.modified_timestamp, 目標.ref_event_id)`，
平手比 `event_id`（字典序大的新；還沒有 edit 的 NULL 最小）。訊息寫入時 `modified_timestamp = 0`，所以第一個 edit 不管時間戳多少都會贏。

**讀取一則訊息時**（維護者 2026-09-14）：

```
1. is_redacted=1                  → 「已刪除」的記號
2. 還沒解開（class=general）       → 「解不開」的記號（kind=undecryptable，跟已刪除一樣 UI 直接渲染）
3. ref_event_id 是 NULL           → 顯示自己的 content_json
   不是 NULL                      → 以被指的那個 edit 為準（維護者 2026-09-14）：
                                    讀者還沒同步到它   → 「過時」的記號（kind=outdated），原文與 edit 內容都不給
                                    讀者 hide 了它     → 這則跟著隱藏（不出現在結果裡）
                                    它被 redact        → 「已刪除」（正常流程會先重設指標，這條是防線）
                                    否則               → 取它的 m.new_content 替換（m.relates_to 留自己原本的）；edited_by = 它的 sender
```

🚨 第 3 步在讀取端**再驗一次**：那一列必須真的是「指回這則、同一個 sender、沒被 redact 的 edit」，對不上就顯示原文（A6：不假定寫入端永遠對）。
🚨 **可見性跟 `events` 表完全一致**（維護者 2026-09-14）：`events_synced_log` 替每一則寫入的事件記一列，edit、redact 也一樣；
指向的 edit 讀者沒有那一列，就是還沒同步到（🚫 不叫「禁止」：通常只是這個帳號還沒拉到那一段，同步之後就好）。⭐ 回的是**單則訊息的記號**，🚫 不是整個請求的錯誤 —— 同一頁的其他訊息照常、翻頁不會卡住。
📎 redact **不套**這條：`is_redacted=1` 就顯示已刪除，不管讀者有沒有同步過那個 redact 事件 —— 那是「少給」，不會洩漏內容（維護者 2026-09-14）。

⭐ **新舊比 `origin_server_ts`，以 server 為準**（維護者 2026-09-14）：有沒有 `g_seq` 都同一條規則，所以一般 Matrix server 也比得出來。
📎 spec 規定 server 端聚合 `m.replace` 時也是這樣選「最新」（`origin_server_ts`，平手比 `event_id`），跟別的 client 看到的一致。
📎 時間戳由發送者的 homeserver 蓋，理論上可以亂填 —— 但有效的 edit 必須跟目標同一個 sender，所以能亂排的只有他自己那幾個 edit，
換不到「改別人的訊息」。

⭐ 這個設計順便解掉幾件事：

- **先到後到都一樣**：比大小的結果跟到達順序無關。
- **目標那列的明文從沒被改過**，所以 redact 掉目前顯示的 edit 時退得回去（§6）。
- 沒有「套了」與「跳過」要分（原 §7 第 4 點）：跳過的就是比較舊的，而目前生效的是哪個，目標那列自己記著。
- 讀取不必掃參照：一次唯一索引 `(room, event_id)` 就拿到 edit 的內容。

🚨 **無效的 edit 必須忽略**（spec 規定；有兩條是資安相關）：

| 規則 | 不遵守會怎樣 |
|---|---|
| **edit 的 sender 必須等於原始事件的 sender** | 🚨 別人可以「改」你的訊息 |
| 原始事件與 edit 都不能是 state 事件 | |
| edit 不能改變事件的 type | |
| 原始事件本身不能是 edit | |
| **原始事件是加密的，edit 也必須是加密的** | 🚨 用明文 edit 蓋掉一則加密訊息 |

## 6 redact

- 目標 `is_redacted=1`，UI 看到的是「已刪除」。目標還沒解開（`general`）也照打勾：刪不刪跟看不看得懂無關。
- 🚫 **不動 `raw_event`、`content_json`**：密文與明文都還在，可以復原（§2）。
- 目標的 `modified_timestamp` 換成 redact 的 server 時間。
- 被 redact 的是**某則訊息目前顯示的 edit**：那則訊息重新從剩下的、沒被 redact 的有效 edit 裡取最新的
  （`ref_event_id`／`modified_timestamp` 換成它的）；一個都不剩就 `ref_event_id = NULL`、`modified_timestamp = 0`（回到沒被 edit 過的樣子，晚到的 edit 照樣能贏），顯示原文。
  📎 只有這個情況要掃一次參照，平常的讀寫都不用。

## 7 拍板紀錄

✅ ~~關係事件怎麼找回目標~~：用 `ref_event_id`（維護者 2026-09-14），見 §3、§4。

1. ✅ ~~**一般 Matrix server 沒有 `g_seq`**，edit 比不出新舊~~：一律比 `origin_server_ts`，平手比 `event_id`（維護者 2026-09-14），見 §5。
2. ✅ ~~**`content_json` 存什麼格式**~~：**解密後的整份 `content` JSON**（維護者 2026-09-14），讀取時從它組出 `Message`。
   edit 的 `content_json` 是它的 `m.new_content`。
3. ✅ ~~**解密還沒接**~~：2026-09-29 接了（e2ee-rpc.md §6）——WS 收到的密文有金鑰就解、密文明文一起存；沒金鑰的金鑰晚到時補解。下面兩點是當時的紀錄（最後走的是 `OlmEngine` 自己解，不是 matrix-sdk 的 `Room::decrypt_event`）。
   - ✅ 這一支只做**明文事件**的分類與 edit／redact／reaction，密文一律 `general`；
     把 WS 收到的密文交給 matrix-sdk 的 `Room::decrypt_event`（它是 `pub`）另開一支。
   - ✅ **matrix-sdk 解開的事件拿不到密文**（`DecryptedRoomEvent` 只有明文）：`raw_event` 放 NULL（維護者 2026-09-14）。
     走 WS 自己撈的，密文就是自己的，照存。
4. ✅ ~~**「套了」與「跳過」怎麼分**~~：不存在了 —— 目標那列的 `ref_event_id` 就是目前生效的那個，跳過的就是比較舊的（維護者 2026-09-14），見 §5。

## 8 實作（2026-09-14，schema v5）

**接縫**：上游給的事件一律先變成 `wbf_sdk::IncomingEvent`（原樣，三種：`Plain`／`Decrypted { ciphertext, cleartext }`／`Undecrypted { ciphertext, reason }`），
backend 的 `history` 回 `EventPage`（原樣、照上游順序，`next` 從原始順序取），watch 的 `Update::NewEvents` 也是原樣。
寫庫的路存原樣；不寫庫的路（`sync=server`、watch 的通知）用 `event_json::messages_from_incoming` 只折同一頁。
分類與 edit 的有效性規則在 `wbf_sdk::incoming`（純 JSON、有單元測試），SQL 在 `cache.rs`。

**跟 §3 的表不一樣的地方**：

| 項目 | 做法 | 為什麼 |
|---|---|---|
| 多一欄 `event_type` | 明文的 `type` | `content_json` 只有 `content`（§7 第 2 點），而加密事件的 `raw_event` 是 `m.room.encrypted`、matrix-sdk 解開的甚至是 NULL —— 沒有這欄就不知道要怎麼顯示，也驗不了「edit 不能改 type」 |
| 拿掉 `kind` | 讀取時從 `content_json` 算 | 同一個事實的第二份（local-cache-db.md §5 原則）；`files` 改用 `json_extract(content_json, '$.msgtype')` |
| 留著 `decrypted` | NULL／0／1 | 「原本是不是加密事件」要明說 —— edit 的資安規則（加密的目標配明文 edit）靠它，🚫 不靠「`raw_event` 是 NULL 所以大概是加密的」這種巧合 |
| 不存解不開的原因 | 讀出來一律 `NotDecryptedHere` | matrix-sdk 給的 UTD 原因只在那一次請求有意義 |
| 解不開、過時有專用的 `kind` | `MessageKind::Undecryptable`／`Outdated`（JSON `"undecryptable"`／`"outdated"`） | 維護者 2026-09-14：跟 `deleted` 一樣是 UI 直接渲染的記號，🚫 不再混在 `unsupported` 裡 |

**寫入**（`upsert_events` 的同一個 transaction 裡）：

- 🚫 **不寫**：沒有 `event_id` 或 `sender` 的（佔位值相等會讓 edit 的 sender 比對放行）；事件自帶的 `room_id` 跟參數不一樣的。
- 同一則再來：`raw_event`／`r_seq`／`g_seq` 只從 NULL 補；server 蓋了 `redacted_because` 就 `is_redacted = 1`（只升不降）；
  原本是 `general`、這次帶明文 → 照明文分類，當作第一次處理。其他一律不動（`content_json` 不被覆蓋）。
- msg／edit 的內容是wbf-client-convention-for-chunk.md §5 的檔 → 建 `media` 與 `event_media`（edit 的檔掛在 edit 自己那列）。
- 每一則寫入時都查「有沒有 redact、edit 在等這則」，redact 先處理；自己是 edit／redact 時，目標在就處理，不在就等。

**讀取**：`find_current_edit` 照 §5；reaction 🚨 只算讀者自己同步過、沒 hide、沒被 redact 的（PR #39 審查 rumia🟡、cirno💡1、salvia）。
📎 Delete for me 在 UI 上是對**原訊息**發 hidden；對 edit／reaction 那一列發的情況一樣照規則走：有參照就以參照物為準。
⚠️ 目前顯示哪個 edit 記在目標那一列，是所有帳號共用的：帳號 B 同步到較新的 edit，帳號 A 沒同步過它，A 讀這則拿到的是 `outdated`，
🚫 不會退回 A 看得到的舊版本（那要每次讀取都掃參照，正是 `modified_timestamp` 要省掉的）。

**已知的限制**：

- `files` 用目標自己的 `content_json` 判斷是不是檔：把一則文字 edit 成檔（或反過來）不會改變它在不在 `files` 裡。
- 解密（§7 第 3 點，另開一支）。


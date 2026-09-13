# RPC 規格：前端 ↔ daemon 的每一則訊息

> 2026-09-12 第一版。形狀（加密、framing、`code`／`msg`、有 `id` 要回）在
> [`architecture-v2.md`](architecture-v2.md) §4，**這裡不重複**；這份只定**逐條**：
> method 清單、每個的 `params`／`result`、code 表、推播清單、資料平面的 HTTP 介面。
> 它是 `crates/wbf-daemon` 與 rpc-cli 的前提（handover §7 第 3 項），也是
> `wbf-core::CoreErrorKind` 配號的權威（PR #24 刻意留空等這份）。
>
> 🚨 **狀態：草案**（維護者 2026-09-12）。這裡定得很完整，但 **daemon 一行都還沒有**，大部分 method 底下的東西
> 也還沒有——只有 matrix-sdk 那一側是現成的。實作時撞到的每一個變數都可以改回這份文件；
> **凍結的時點是 daemon 第一版合併**，那之後 `code` 的號碼與 `method` 的名字才**定了就不改**，
> 其餘（欄位可以加、推播可以加、新的 method 可以加）永遠是相容的變動，不動 `protocol` 版號。
>
> ⭐ **「做完」的判準：底層走的是我們自己跟 homeserver 的 WS（wbf-pack）才算**。走 matrix-sdk 的 HTTP
> 只是現在能動，未來要全面遷移到 WS（architecture-v2 §6.1 的四條線）；HTTP fallback 也一樣不算。
> 每個 method 的現況在 §10。

## 0. 一句話

```
前端 ──{ method, params, id }──> daemon ──{ code, msg, result, id }──> 前端
daemon ──{ method, params }（沒有 id）──> 前端        推播：progress、room.message、…
```

一條連線、任意多個未完成的 `id`、每個 `id` 一個回應；長工作的中途狀態用推播帶著那個 `id` 回來。

## 1. 線上封裝：RPC 自己的極簡 pack（維護者 2026-09-12 定）

**WS 上一律 binary frame**，🚫 沒有 text frame。每一個 frame 就是一個 pack：

```
pack = ver(1 byte) ‖ type(1 byte) ‖ data(變長，到 frame 結尾)
```

| 欄位 | 值 | 意思 |
|---|---|---|
| `ver` | `0x01` | pack 格式的版本。起始值 1。⚠️ 這是**封裝**的版本，🚫 不是 `hello` 談的 `protocol`（那是訊息內容的版本） |
| `type` | `0x00` | 未定。🚫 不能送，收到就是 `BAD_FRAME` |
| | `0x01` | **明文**：`data` 就是 JSON bytes |
| | `0x02` | **密文**：`data = nonce(24) ‖ XChaCha20-Poly1305(key, nonce, aad, JSON bytes)`（金鑰與 aad 在 architecture-v2 §4.4） |
| `data` | | **沒有長度欄位**——長度由 WS frame 給 |

- ⭐ **`type` 只回答一件事：這包是明文還是密文。** 它🚫 不表示種類、不表示方向、不表示成敗——那些都在 JSON 裡。
- 📎 這跟 homeserver 那套 `wbf-pack`（`wbf-wire`）**無關**，只是借它「極簡二進位前綴」的做法。🚫 不共用 codec。
- **daemon 送出去的每一則都經過同一個封裝器**：`pack(type, json) -> bytes`／`unpack(bytes) -> (type, json)`，
  🚫 不要在每個 method 各自組 bytes——「哪些該加密」的判斷只能有一個地方（§1.1）。
- frame 上限 1 MiB（含前綴）。超過 → `BAD_FRAME`。
- `ver` 認不得 → `BAD_FRAME`。第一版只認 `0x01`。

### 1.1 兩個階段：明文階段、密文階段

```
連上 ──> [密文階段]  client 送 0x02（含 hello）、daemon 回 0x02
                │
                └── 協議層錯誤 ──> daemon 送 0x01 的 close 通知 ──> WS close
```

- **client 預設永遠送 `0x02`，包括 `hello`。** 沒有 token 就送不出解得開的包，第一包就驗不過（architecture-v2 §4.4）。
- daemon 有一個**全局狀態 `encryption_enforced`，預設開**。開著的時候：client 送來 `0x01` → `BAD_FRAME`（🚫 不接受降級）；
  daemon 的正常回應與推播一律 `0x02`。
- **`0x01` 在 enforce 開著時只有一種用途：協議層錯誤的 close 通知（§1.4）。** 那是唯一一種「對方可能沒有金鑰」的情況，
  用密文告知等於沒告知；而且**它一定緊接著關連線**，所以不存在「UI 收到一包明文卻以為是正常回應」的問題。
- **手動降級**：`daemon.set_encryption { enforced: false }`（本身要走 `0x02` 送）把全局狀態關掉。之後 daemon 接受 `0x01` 的請求、
  對 `0x01` 的請求用 `0x01` 回、推播用那條連線最後一次請求的 type。開回去用同一個 method。
  ⚠️ 這是**除錯用**（抓包看明文），🚫 不是給前端省事的：Desktop／Android 永遠送 `0x02`。
  📎 `daemon.info` 回 `encryption_enforced` 讓人看得到現在是哪一種。

### 1.2 連線的生命週期

1. 前端連上 `ws://127.0.0.1:<rpc port>`（port 在 `<data dir>/daemon.json`，architecture-v2 §4.3）。
2. **第一則必須是 `hello`**。它之前送任何別的 method → `9003`，關連線。
3. `hello` 要過兩關（§1.3）：**client 名字**要以 `wbf-matrix` 開頭；**protocol** 要在 daemon 支援的那組裡。任一不過 → 關連線。
4. 解不開的包（token 不對）、`ver`／`type` 不對、超過 1 MiB → 關連線。
5. **關連線之前一定先送一則 `0x01` 的 close 通知**（§1.4）——不然 token 錯的人只看到連線斷掉，什麼提示都沒有。
6. 之後任意順序、任意並行。前端關掉連線＝它所有訂閱作廢、所有未完成的長工作**繼續跑**
   （上傳到一半不會因為 Desktop 關掉而中斷；要停要明講 `cancel`）。

### 1.3 `hello`

```jsonc
{ "method": "hello", "params": { "protocols": [2, 1], "client": "wbf-matrix-rpc-cli 0.1.0" }, "id": 0 }
{ "code": 0, "msg": "ok", "id": 0, "result": {
    "protocol": 2,                    // 談定的那一個
    "daemon": "wbf-matrix-client-daemon 0.1.0",
    "instance": "3f2b1c4a-5d6e-4f80-9a1b-2c3d4e5f6071",  // 這次啟動的 UUID
    "pid": 4242,
    "uptime_seconds": 12,
    "data_dir": "C:/Users/me/AppData/Roaming/wbf-matrix-client",
    "unlocked": false,
    "key_mode": "passphrase",         // "plain" | "passphrase" | null（還沒有 local.key）
    "encryption_enforced": true
} }
```

**`instance`：這次啟動的身分**（維護者 2026-09-13）。daemon 起來時鑄一個 UUID v4，活著的期間不變。

- ⭐ 它回答的是前端唯一問得出口的那個問題：**「我現在講話的還是剛才那一個 daemon 嗎？」**
  🚫 port 答不了（會被重複使用）、🚫 pid 也答不了（會被回收）。同一個 `instance` ＝ 同一個實例，
  所以前端手上的所有狀態（訂閱、進行中的 `id`、解鎖與否）都還算數；換了就是全部重來。
- 同一個值同時出現在**四個地方**：stdout 的 ready 那行、`<data dir>/daemon.json`、`hello` 的
  result、`daemon.info` 的 result —— ⭐ 一個值一個來源，🚫 不各鑄一個。
- `pid` 與 `uptime_seconds` 一起回：`pid` 給人（去 kill 它、去看 log），`uptime_seconds` 讓前端
  一眼看出「它是不是剛剛才重開過」。⚠️ **判斷同不同一個實例只准用 `instance`**，🚫 不要用 pid。

**`client`：正式名稱，`wbf-matrix` 開頭**。

- 格式 `<正式名稱> <版本>`：`wbf-matrix-rpc-cli 0.1.0`、`wbf-matrix-desktop 0.3.0`、`wbf-matrix-android 0.1.0`。
  🚫 不是簡稱（`rpc-cli`）——簡稱是文件裡的寫法（architecture-v2 §0.1），不是線上的識別。
- daemon 做**基礎檢查**：不以 `wbf-matrix` 開頭 → `BAD_CLIENT`（§1.4），關連線，**跟 protocol 不對一樣拒絕**。
  這不是安全機制（token 才是），是擋掉「有人拿別的東西亂連」與「寫錯名字」的第一道門。
- 之後只進 log。

**`protocols`：一個協商表，不是一個數字**。

- 前端送**它會講的全部版本**，新的在前。daemon 也有一組**它支援的**，取交集裡最大的那個回在 `result.protocol`。
  沒有交集 → `PROTOCOL_MISMATCH`（§1.4），關連線。
- ⭐ **常態是 daemon 升級、前端沒升**：daemon 版本往上走的時候**維持能講舊協議**，舊前端照用。
  只有 **breaking**（舊協議真的沒辦法再服務）才把那個版本從表裡拿掉——那時候舊前端一連上來就被**明確拒絕**，
  🚫 不是連上了之後某個 method 突然壞掉。
- 反過來前端比 daemon 新（前端送 `[3, 2]`、daemon 只會 `[2, 1]`）→ 談成 `2`，前端自己降級。
- 第一版：雙方都只有 `[1]`。**談定之後那條連線上的每一則都是那個版本的形狀**，🚫 中途不換。

📎 `msg` 一律英文（CLI 規格 §4 同一條）。語言協商考慮過，維護者 2026-09-12 判定多餘：`msg` 是給人看的除錯字串，
使用者看到的字由前端照 `code` 自己翻。

`hello` 與 `vault.*`、`daemon.*` 是**未解鎖時也接受**的全部（architecture-v2 §4.5）；其他一律 `1001`。

### 1.4 協議層錯誤：`0x01` 的 close 通知，然後關連線

🚨 daemon **關掉一條連線之前一定先送一包 `type = 0x01`（明文）**。它的 JSON **跟正常回應同一個形狀**
（`code`／`msg`／`result`／`id`，architecture-v2 §4.6），🚫 不是另一套：

```jsonc
{ "code": 9001, "msg": "could not decrypt the first frame; the daemon token does not match",
  "result": { "close": "BAD_TOKEN" }, "id": null }
```

⭐ 這樣前端的 frame 翻譯器只有一條路：**先看 `type`，`0x02` 就解密、`0x01` 就直接 JSON decode，然後全部進同一個
`{ code, msg, result, id }` 的處理**。🚫 不要為了 close 通知另寫一個 parser。

| `code` | `result.close` | 什麼時候 |
|---|---|---|
| 9001 | `BAD_TOKEN` | `0x02` 的包解不開（AEAD 標籤驗不過） |
| 9002 | `BAD_FRAME` | 不是 binary frame、`ver` 認不得、`type` 是 `0x00`、enforce 開著卻收到 `0x01`、超過 1 MiB、解開之後不是 JSON |
| 9003 | `HELLO_REQUIRED` | 第一則不是 `hello`（**含 `hello` 之前送來一則寫壞的請求**：還沒談成協議，fail closed） |
| 9004 | `BAD_CLIENT` | `client` 不以 `wbf-matrix` 開頭 |
| 9005 | `PROTOCOL_MISMATCH` | `protocols` 跟 daemon 的沒有交集 |
| 9006 | `SHUTTING_DOWN` | daemon 要關了 |

- **`9000–9099` 是協議層**：這條連線本身出了問題，回完就關。**其餘一切**——房間操作失敗、衝突、上游 homeserver 的錯誤、
  vault 鎖著——都是**請求層**的，用當時的加密狀態送（enforce 開著就是 `0x02`）。
  判準：**連線還能不能用**。能用 → 請求層、密文；不能用 → 協議層、明文、關。
- ⚠️ **「解開之後是 JSON、但不是一個請求」不是協議層的事**：那是 `100`（§5.1），回完**連線照用**。
  判準還是同一條 —— 封裝壞了（解不開、不是 JSON）連線就沒救了；一則寫壞的請求只是那一則壞了。
  📎 例外是 `hello` 之前：還沒談成協議，所以寫壞的請求一律 `9003` 關掉。
- `id`：對得上某個請求（`hello` 被拒）就帶那個 `id`；對不上（解不開、shutdown）就 `null`。🚫 不省略欄位。
  📎 `100` 的 `id`：JSON 裡的 `id` 剛好是整數就帶著它回（`method` 壞掉時前端還配得起來），否則 `null`。
- `result.close` 是**大寫底線**的字串，跟 `code` 一對一——留著是給人讀 log 用，前端判斷用 `code`。
- 這則之後緊接 WS close frame（status 1008 policy violation；`SHUTTING_DOWN` 用 1001 going away）。
- ⚠️ 明文包**只出現在關連線前**，而且**內容裡永遠沒有秘密**（不回 token、不回解出來的東西）。

## 2. 共同的 params 欄位

| 欄位 | 型別 | 意思 |
|---|---|---|
| `user` | string? | 對哪個帳號動作，mxid 或 localpart。**沒給 = `current`**。＝ `wbf-core::Target.user` |
| `server` | string? | 同名 localpart 在多個 server 時消歧。＝ `Target.server` |
| `transport` | `"ws"` \| `"http"` | 跟 wbfuwunel 講話走哪條。**預設 `ws`**。只有標了「有 `transport`」的 method 認得它 |
| `sync` | `"local"` \| `"server"` \| `"both"` | **要本地的還是上游的**。**預設 `local`**。只有標了「有 `sync`」的 method 認得它 |

🚨 **`sync`：RPC 大部分是對本地資料庫的呼叫**（維護者 2026-09-13 定；執行期細節在
[`daemon-runtime.md`](daemon-runtime.md) §3）。UI 顯示東西走本地，要打上游得**明講**：

| 值 | daemon 做什麼 | 寫 `cache.db` |
|---|---|---|
| **`local`（預設）** | 只讀 `cache.db` | ❌ |
| `server` | 打上游、拿到什麼就回什麼 | 🚫 **不寫**（這是「看一眼」，不是同步） |
| `both` | 打上游 → 寫進 `cache.db` → **再從本地讀一次**回傳 | ✅ |

- ⭐ `both` 回的是**本地讀的結果**，形狀跟 `local` 一模一樣 —— UI 🚫 不必為兩種模式寫兩套解析。
- 🚫 **沒有 `auto`**：延遲從毫秒跳到秒這件事，要由呼叫者決定，🚫 不是 daemon 猜。
- 判準：**本地快取有那份東西的 method 才有這個參數**。帳號列表、recovery key 那些是這台機器的檔案
  （沒有「上游版本」），`sync.recent`／`server.ping`／`backup.*` 本來就是上游的。
- ⚠️ 回傳的型別裡**只有 server 知道的欄位**，`local` 時就讓它們**不在**，🚫 不編造
  （`media.info` 的 `total_len`／`truncated`／`verified` 就是）。⭐ 少一個欄位是誠實，填一個假數字不是。

🚨 **認得 `sync` 的 method，回應要回報這次用的是哪一種**（維護者 2026-09-13）：

```jsonc
{ "code": 0, "msg": "ok", "id": 1, "sync": "local", "result": [ … ] }
```

- **只要那個 method 認得 `sync` 就帶，不管請求有沒有帶** —— ⭐ 這樣前端知道「沒帶時預設是什麼」，
  🚫 不必去記規格，也不必猜這份資料是本地的還是剛從上游拿的。
- ⚠️ **失敗的回應也帶**：「我去打了上游然後失敗」跟「我只讀本地然後沒有」是兩件事。
- 🚫 **值認不得（`102`）時不帶**：那次它一種都沒用，宣稱用了哪一種是說謊。
- 不認得 `sync` 的 method **沒有這個欄位**（🚫 不是 `null`）。

- ⚠️ **`server_backup` 不在 RPC 上**：那是 `wbf.conf` 的開關（CLI 規格 §10），**由 daemon 讀 conf 填進 `Target`**。
  前端不該替使用者決定要不要備份，而 daemon 就是 conf 的主人（它是 client 本體，architecture-v2 §0.2）。
- 認不得的欄位**忽略**（前端可以比 daemon 新）；缺必填欄位 → `102`。
- 路徑欄位（`path`、`out`）是 **daemon 那台機器的路徑**。1:1 同機所以通常就是前端的路徑，
  但 Android 沒有路徑可給——那些走資料平面（§6）。

## 3. method 清單

`名詞.動詞`（§4.6）。「core」欄是它包的 `wbf-core` 方法，沒有的標 ⚠️。

### 3.1 daemon 與 vault（未解鎖也接受）

| method | params | result | core |
|---|---|---|---|
| `hello` | §1.3 | §1.3 | — |
| `daemon.info` | — | `{ version, instance, pid, data_dir, unlocked, key_mode, encryption_enforced, protocols: [int], rpc_port, data_port, uptime_seconds, connections, server_backup_setting, local_room_keys_setting }`。`instance`／`pid` 同 §1.3；後兩個是 conf 的開關（`"on"`／`"off"`），跟 `backup.status` 回的同一組 | `key_mode`、`is_unlocked` |
| `daemon.set_encryption` | `{ enforced: bool }`。本身必須走 `0x02` 送（§1.1） | `{ encryption_enforced }` | — 全局狀態，除錯用 |
| `daemon.shutdown` | — | `{ ok: true }`；回完之後才關 | — ⚠️ 生命週期整體還沒定（architecture-v2 §8 第 4 點），這條只是「有人能把它關掉」的最低限度 |
| `daemon.reload_conf` | — | `{ ok: true, changed: [string], warnings: [string] }` | — 重讀 `wbf.conf`（**graceful**：🚫 不斷上游會話、🚫 不掉連線）。⭐ 前端改設定（例如已讀要不要公開，daemon-runtime §6.3）之後叫它，🚫 不必重開 daemon |
| `vault.create` | `{ passphrase_base64?: string }`。**fresh 資料目錄的起手式**：帶了就是 `passphrase` 模式，沒帶就是 `plain` | `{ ok: true, key_mode }` | `create_vault`。已經有 `local.key` → `1100`（🚫 不覆蓋：那會把既有帳號全鎖在門外）。建完就是**解鎖狀態** |
| `vault.unlock` | `{ passphrase_base64?: string }`。`plain` 模式不帶；`passphrase` 模式帶**原始 bytes** 的 base64（local-cache-db §12） | `{ ok: true, key_mode }` | `unlock` |
| `vault.set_passphrase` | `{ passphrase_base64: string }` | `{ ok: true, key_mode: "passphrase" }` | `set_passphrase(Some)` |
| `vault.remove_passphrase` | — | `{ ok: true, key_mode: "plain" }` | `set_passphrase(None)` |

🚨 **沒有 `vault.lock`**（維護者 2026-09-13 定）。**daemon 不提供「鎖上」這個 feature**，
`Core` 的生命週期就是「啟動時解鎖一次、活到程序結束」。

| 想要的 | 怎麼做 |
|---|---|
| **UI 的 lock／unlock**（暫時離開、閒置） | **UI 自己那一層**鎖畫面。🚫 不動 daemon：daemon 照樣連著 server、照樣寫 DB、照樣發通知——⭐ 使用者離開時收到訊息，回來就該看到它，而不是一段空白 |
| **真的鎖上**（金鑰離開記憶體） | `daemon.shutdown`，要用再 `daemon -s` 起一次。⭐ 這才是「鎖 vault」的真正意思：停掉所有訂閱、關掉所有連線、程序結束、`Vault` 在 drop 時被 zeroize |

為什麼不做一個 `vault.lock`：**它做不到它名字承諾的事。** 請求各自跑，`call()` 一進來就先拿一份
`Arc<Core>`，所以「把 `Handle.core` 換成一個沒解鎖的」只換得掉**之後**進來的請求 —— 已經拿到舊那份的
長工作（`sync.recent`、`upload.file`、`media.save_to`…）會繼續用解鎖狀態跑完，而金鑰要等**最後一個**
持有者放手才會被抹掉。那時回 `{ok: true}` 是在說謊，而一個回報成功、邊界卻沒成立的安全操作
**比明確失敗危險得多**。⭐ 與其做一個「盡量鎖」，不如只留一條真的做得到的路（shutdown）。
📎 這條在 PR #30 進來、PR #31 的審查（cirno🔴、rumia🔴）發現它擋不住 in-flight 請求，
維護者 2026-09-13 決定整條拿掉。

🚨 **fresh 資料目錄的起手式是 `vault.create`，🚫 不是 `account.add`**（維護者定調前的第一版讓
`account.add` 自己偷建一把 plain 的，PR #31 審查 rumia🔴、salvia🔴 指出那是能力退化）：

- 那把偷建的只能是 **plain**，所以想要 passphrase 的前端被迫「先落一份 plain `local.key` → 再
  `vault.set_passphrase` 重包」。⭐ 中間那段時間磁碟上的主金鑰**沒有 passphrase 保護**，
  而 `vault.set_passphrase` 又要求 vault 已經解鎖 —— fresh 狀態下那條路根本走不到。
- 所以「要不要 passphrase」在**建的那一步**就要決定，跟 CLI 的 `login` 一樣一步到位。
- 沒建就去叫別的 method：閘門回 **`1002`**（不是 `1001`）——⭐ 「還沒有 vault」與「有但鎖著」的
  下一步不同（`vault.create` vs `vault.unlock`），所以🚫 不共用一個 code；`msg` 裡直接寫下一步。

⚠️ passphrase 用 base64 而不是字串：它是任意 bytes（可以是一個 mp3）。🚫 不提供 `passphrase_file`
——那是「daemon 替前端讀檔」，web 前端根本給不出檔案路徑，而 rpc-cli 自己讀了再送不多一行。

### 3.2 帳號

| method | params | result | core |
|---|---|---|---|
| `account.add` | `{ server: string, user: string, password: string, device_name?: string }`。`device_name` 預設 `"wbf-matrix-client"` | `LoginResult`：`{ user_id, device_id, server, switched_from? }` | `log_in`。⚠️ **要先 `vault.create`**：🚫 它不替前端建 vault |
| `account.list` | — | `AccountStatus`：`{ accounts: [{ user_id?, server, localpart, logged_in, current }], undecryptable_hint? }` | `account_status` |
| `account.switch` | `{ user, server? }` | `SwitchResult`：`{ current, switched_from?, logged_in }` | `switch_current` |
| `account.whoami` | `{ user?, server? }` | `{ user_id, device_id, server }` | `whoami` |
| `account.del` | `{ user, server?, accept_history_loss?: bool }` | `{ user }` | `log_out`。閘門擋下 → `1021` |
| `account.destroy` | `{ user, server?, accept_history_loss?: bool }` | `DestroyResult`：`{ user, events_removed, media_removed, pool_files_removed, recovery_key_destroyed }` | `destroy_account` |

⚠️ `password` 在 RPC 上是明文字串——它在加密的 frame 裡，而且**只在這一則**。daemon 🚫 不留、不進 log、不進任何推播。
🚫 沒有「確認」這種互動：`account.destroy` 沒有 `--yes`，前端要問就自己問（daemon 不代前端做決定，§3）。

### 3.3 房間

| method | params | result | core |
|---|---|---|---|
| `room.list` | `{ user?, server?, sync? }`。**有 `sync`**（§2） | `[Conversation]`（chat-model §2.1） | `list_conversations`。⚠️ core 現在只有「先跟上游 sync 一輪」那條，`local` 要接 `cache.db` 的 `room_list` |
| `room.get` | `{ room, user?, server?, sync? }`。**有 `sync`** | `Conversation` | `conversation`。同上 |
| `room.send_text` | `{ room, body, user?, server? }` | `{ event_id }` | `send_text` |
| `room.send_file` | `{ room, path, caption?, cipher?, chunk_size?, name?, mimetype?, sha256?, transport?, user?, server? }`。**路徑版**：daemon 自己讀檔、上傳、送事件，一則回應。給有路徑的前端（rpc-cli、Desktop 拖檔） | `{ event_id, mxc, attachment_declared, manifest }`。⚠️ `manifest` 含金鑰：前端要存就自己用私有權限存（CLI 規格 §5），daemon 不落地 | `send_file`。長工作：推 `progress` |
| `room.send_attachment` | `{ room, upload_id, caption?, user?, server? }`。**資料平面版**的後半：`media.create` 之後、bytes 還在 PUT 的時候就能送（architecture-v2 §4.9 第 4 步） | `{ event_id, mxc, attachment_declared }` | ⚠️ core 沒有——現在 `send_file` 是「傳完再送」一條龍。要拆成「建檔→（送事件 ∥ 傳 bytes）」 |
| `room.history` | `{ room, limit, before?, types?, sender?, user?, server?, sync? }`。**有 `sync`** | `MessagePage`：`{ events: [Message], next? }` | 見下面的「`before` 是哪一套座標」 |
| `room.files` | `{ room, limit, before?, user?, server?, sync? }`。**有 `sync`** | `FilePage`：`{ files: [{ event_id, sender, ts, manifest }], next? }` | `files(save_to: None)`。⚠️ CLI 的 `--save` 是前端的事：拿到 manifest 自己寫檔 |
| `room.read` | `{ room, event_id? \| g_seq? \| r_seq?, user?, server?, sync? }`。**有 `sync`**：`local` 只寫本地、`server` 只送上游、`both` 兩邊 | `{ ok: true, event_id }` | ⚠️ core 沒有。已讀有三層、預設 private（daemon-runtime §6） |

🚨 **`before` 是哪一套座標，看 `sync`**（`room.history`／`room.files`；PR #32 審查 cirno🔴）：

| `sync` | `before` 收什麼 | 回應的 `next` 是什麼 |
|---|---|---|
| `local`（預設） | 本地 `r_seq` 的**數字** | 本地 `r_seq` |
| `server` | server 的**翻頁 token** | server 的翻頁 token |
| `both` | 🚫 **不接受 `before`**（帶了就是 `1100`＝`CoreErrorKind::Usage`） | 本地 `r_seq` |

🚨 **`both` 那一格是權宜的，它會消失**。問題不在 `both`，在**現在只有一個 backend 拿得到歷史**：

| backend | 上游怎麼定位 | 本地怎麼定位 | 對得上嗎 |
|---|---|---|---|
| matrix-sdk `/messages`（現在唯一有歷史的） | 不透明 token | `r_seq` | ❌ 沒有翻譯 |
| **wbf**（`Event/*`） | `r_seq`／`g_seq`（server 自己塞的） | `r_seq` | ✅ **同一套** |

⚠️ 所以在 matrix backend 上，`both` 帶一個 token 進來，好一點是解析失敗（而且是在**已經抓完、
已經寫進庫之後**才失敗）；🚨 壞的是那個 token 剛好長得像數字 —— 它會指到本地一個不相干的位置，
然後**看起來像成功**。協議上 token 就是不透明字串，🚫 不該賭它的長相。

⭐ **出口**：wbf 那條線的房間歷史 API 正在 server 端開發中（維護者 2026-09-13）。接上之後上下兩半
講同一種 `r_seq`，這一格就該改回「本地 `r_seq`」—— ⚠️ **前端的介面不會因此改**：`sync` 三個值、
`before` 一個欄位，變的只是「`both` 也收得下 `before`」。

📎 在那之前也不擋路：**點開房間 = 不帶 `before` 的 `both`**（拉最新的一頁順便入庫），
**往回翻 = `local`**（資料已經在庫裡了，翻頁很便宜）。要一頁頁跟 server 翻就整條都用 `server`。

🚫 **沒有 `room.watch`**。CLI 的 `watch tail|wait|once` 是「一個命令一個程序」的產物；daemon 常駐，
新訊息走**訂閱＋推播**（§4）。rpc-cli 要模擬 `watch once --timeout` 就是「訂閱、等第一則、退訂」。

### 3.4 同步

| method | params | result | core |
|---|---|---|---|
| `sync.recent` | `{ max_events?: 10000, window?: 320, batch?: 10, from_scratch?: bool, user?, server? }`（三層的意思在 CLI 規格 §3.5） | `RecentSummary`：`{ pulled, written, windows, batches, caught_up, cg_seq_before?, cg_seq_after?, skipped_without_room }` | `recent`。長工作：推 `progress` |

📎 daemon 之後 `recent` 應該是**它自己排程跑**的（連上 server 就補洞），這條 method 是「現在就跑一輪」。
排程怎麼訂還沒定，跟 architecture-v2 §6 的四條連線一起做。

### 3.5 上傳（不進房間的裸上傳；有 `transport`）

| method | params | result | core |
|---|---|---|---|
| `upload.file` | `{ path, cipher?, chunk_size?, name?, mimetype?, sha256?, transport?, user?, server? }` | `Manifest`（CLI 規格 §5） | `upload_file`。長工作 |
| `upload.status` | `{ upload_id, transport?, user?, server? }` | `UploadStatusReport` | `upload_status` |
| `upload.abort` | `{ upload_id, state_file?: path, transport?, user?, server? }` | `{ ok: true }` | `abort_upload` |

`upload --stream`（stdin）在 daemon 模型下**就是資料平面的 PUT**（§6.2），沒有對應的 method。

### 3.6 媒體（有 `transport`）

| method | params | result | core |
|---|---|---|---|
| `media.info` | `{ mxc, manifest?, transport?, user?, server?, sync? }`。**有 `sync`** | `MediaInfo`。⭐ 媒體**不可變**，所以 `local` 答得出 `file_size`／`chunk_size`／`content_type`，加上上游答不出來的 `cached: { complete, chunks_written, bytes_on_disk }`。⚠️ `total_len`／`truncated`／`description`／`verified` 只有問過 server 才有，`local` 時**不在** | `media_info`。`both` 順手把 server 說的寫進 `media` 表 |
| `media.open` | `{ manifest, user?, server? }` 或 `{ event_id, room, user?, server? }`（daemon 從快取找 manifest） | `{ url, mimetype?, size, expires_in }`。`url` 是資料平面的 capability URL（§6.1） | ⚠️ core 缺「給一個 reader」的形狀：現在 `download_to` 直接寫檔、`seek_read` 一次回整段 bytes。daemon 要的是 `PoolReader`（local-cache-db §8.6）接到 HTTP Range 上 |
| `media.create` | `{ room?, name, size?, mimetype?, cipher?, chunk_size?, sha256?, user?, server? }` | `{ upload_id, mxc, url, expires_in }`。`url` 是資料平面的 PUT URL（§6.2）。`mxc` 在這一步就有（server 的 `Create` 就配好 id）——所以 `room.send_attachment` 不必等傳完 | ⚠️ core 缺（同 `room.send_attachment`） |
| `media.save_to` | `{ manifest, out: path, no_cache?: bool, transport?, user?, server? }` | `DownloadResult` 或（`no_cache`）`DirectDownloadResult` | `download_to`／`download_direct`。長工作。**明文落地是使用者要的**（§4.8） |
| `media.stats` | `{ user?, server? }` | `MediaStats` | `media_stats` |
| `media.gc` | `{ quota_mib?: 2048, protect_days?: 7, user?, server? }` | `MediaGcReport` | `collect_media_garbage` |

`seek` 沒有 method：`media.open` 的 URL 上發 `Range` 就是 seek（同一個語意，約定 §7）。

### 3.7 房間金鑰備份

| method | params | result | core |
|---|---|---|---|
| `backup.status` | `{ user?, server? }` | `BackupStatusReport` 加 conf 的兩個開關 `server_backup_setting`／`local_room_keys_setting`（daemon 從 conf 填） | `backup_status` |
| `backup.upload` | `{ user?, server? }` | `UploadResult` | `upload_room_keys(also_save_snapshot = conf 的 LOCAL_ROOM_KEYS)` |
| `backup.save` | `{ user?, server? }` | `{ bytes }` | `save_room_key_snapshot` |
| `backup.import` | `{ user?, server? }` | `ImportResult` | `import_room_key_snapshot` |
| `backup.restore` | `{ user?, server? }` | `RecoveryStateReport` | `restore_from_recovery_key`。沒保管 → `1020` |
| `backup.create_recovery_key` | `{ user?, server? }` | `{ recovery_key }` ⚠️ 秘密，只回這一次 | `create_recovery_key` |
| `recovery.list` | — | `{ users: [mxid] }` | `list_recovery_key_users` |
| `recovery.show` | `{ user }` | `{ user, recovery_key }` ⚠️ 秘密 | `find_recovery_key`。沒有 → `1020` |

### 3.8 server

| method | params | result | core |
|---|---|---|---|
| `server.ping` | `{ transport?, user?, server? }` | `ServerHello` | `ping(client_name = daemon 的名字與版本)` |

### 3.9 訂閱、取消

| method | params | result |
|---|---|---|
| `subscribe` | `{ events: [string], user?, server? }`。`events` 是 §4 的推播名，`"*"` 全訂。`user` 給了就只收那個帳號的 | `{ subscribed: [string] }` |
| `unsubscribe` | `{ events: [string] }` | `{ subscribed: [string] }`（剩下的） |
| `cancel` | `{ id: number }`——**要取消的那個請求的 `id`** | `{ ok: true, was_running: bool }`。被取消的請求自己收到 `105` |

- 訂閱是**每條連線一份**，連線關了就沒了。
- `cancel` 只對長工作有意義（`room.send_file`、`upload.file`、`media.save_to`、`sync.recent`）；
  對已經回完的 `id` → `was_running: false`，不是錯誤。

## 4. 推播（沒有 `id` 的請求，daemon → 前端）

| method | params | 什麼時候 |
|---|---|---|
| `progress` | `{ id: number, done: number, total?: number, note?: string }`。`id` 是**哪個請求**的進度 | 長工作跑的時候。`total` 不知道就不帶（串流）。`note` 給人看，🚫 不做邏輯 |
| `room.message` | `{ user, room, message: Message }`（chat-model §2.2，含 `decrypted`／`undecryptable_reason`） | 這個帳號收到一則新訊息（sync 或 `Event/Push` 進來、解完密、寫進快取**之後**） |
| `sync.state` | `{ user, state: "connected"\|"disconnected"\|"catching_up"\|"caught_up", cg_seq? }` | 跟 server 的連線狀態變了 |
| `vault.state` | `{ unlocked: bool }` | 另一條連線解鎖或鎖上了——多條連線各自平等（§4.7），所以要互相通知 |
| `desync` | `{ missed: number, user? }` | 🚨 **這條連線漏掉了推播**（它讀得太慢、事件被覆蓋掉）。收到就**重讀**（房間列表、開著那間的最新一頁、未讀數）——全都是本地讀，很便宜。🚫 daemon 不重播（沒留著），但🚫 也不假裝沒事 |

- 推播**要先 `subscribe`**（§4.6）。`progress` 例外：**發出長工作的那條連線自動收到自己請求的 `progress`**，不必訂——不然每個前端都要多寫一步。
- 推播是「不用輪詢」，🚫 不是「保證看得到全部」：慢的訂閱者會掉事件（`wbf-core::event::EVENT_QUEUE`），掉了就重查狀態。
  🚨 **但掉了一定要發 `desync`**：不講的話 UI 永遠不會去重查（它以為自己收齊了）。
- 🚨 **媒體的進度🚫 不走這裡**（維護者 2026-09-13）：UI 的上傳／下載是**資料平面的 HTTP**（§6），
  進度就是那個 HTTP 傳輸自己的進度。⭐ 所以 RPC 通道上🚫 沒有 bytes、🚫 沒有每塊一則的進度，
  它基本上永遠是暢通的。
  📎 `progress` 只給**daemon 自己在跑的長工作**：路徑版的 `room.send_file`／`upload.file`／
  `media.save_to`（daemon 讀本機檔，UI 沒有 HTTP 可看）、`sync.recent`、`backup.*`——都是低頻的。
- ⚠️ core 現在的 `CoreEvent::Progress` 是一句字串，`room.message` 對得上 `CoreEvent::Message`；
  `sync.state` 與結構化的 `progress` 是 **core 要補的 variant**（daemon PR 順手做，🚫 不在 daemon 裡 parse 那句字串）。

## 5. code 表（定了就不改）

`code` 是整數。`0` 成功。**`1–999` 是請求層的 RPC 錯誤**（daemon 自己擋下、沒碰 core、連線照用）；
**`1000–1999` 一對一對到 `CoreErrorKind`**；**`9000–9099` 是協議層**（回完就關連線，§1.4）；
之後有新層（例如 uniffi 綁定）從 `2000` 起。

### 5.1 RPC 層

| code | 名字 | 什麼時候 |
|---|---|---|
| 100 | `bad_request` | 解出來不是 JSON 物件、沒有 `method`、`id` 不是整數 |
| 101 | `unknown_method` | 沒這個 method |
| 102 | `invalid_params` | 缺必填、型別不對、base64 解不開、路徑不是絕對路徑 |
| 105 | `cancelled` | 這個請求被 `cancel` 掉了 |
| 106 | `busy` | 同一個帳號已經有一個同種的長工作在跑（例如兩個 `sync.recent`）。🚫 不排隊，讓前端決定 |
| 107 | `daemon_shutting_down` | `daemon.shutdown` 之後進來的任何請求 |
| 108 | `internal` | daemon 自己組不出回應（它的 bug，例如 result 序列化失敗）。🚫 不是前端的錯，所以🚫 不關連線 |
| 109 | `no_write_access` | 這個 daemon **沒有寫這個資料目錄的權**：別人握著排他鎖（architecture-v2 §0.2）。⚠️ 跟 `1001`（vault 鎖著）不是同一件事 —— 那是「還沒解鎖」，這是「這個目錄現在是別人的」。前端該做的是去連**那一個** daemon，🚫 不是重試 |

### 5.2 core 層 ＝ `CoreErrorKind` 的號碼

| code | `CoreErrorKind` | rpc-cli 的 exit code（CLI 規格 §4） |
|---|---|---|
| 1001 | `locked` | 1 |
| 1002 | `no_key_file` | 1 |
| 1003 | `need_passphrase` | 1 |
| 1004 | `unexpected_passphrase` | 1 |
| 1005 | `wrong_passphrase` | 1 |
| 1010 | `no_such_account` | 1 |
| 1011 | `ambiguous_account` | 1 |
| 1012 | `not_logged_in` | 1 |
| 1020 | `no_recovery_key_here` | 1 |
| 1021 | `history_would_be_lost` | 1 |
| 1100 | `usage` | 1 |
| 1200 | `io` | 1 |
| 1300 | `network` | 4 |
| 1400 | `server` | 2 |
| 1500 | `integrity` | 3 |
| 1600 | `timeout` | 5 |

- 號碼**留了縫**（1006–1009、1013–1019……）：同一族拆新 variant 就填進去，🚫 不重排。
- `usage`（1100）是過渡桶子（`error.rs` 自己標的）：每次前端需要分辨就拆一個新號碼出去，🚫 讓前端 parse `msg`。
- `msg` 就是 `CoreError.message`，給人看。**`kind` 的名字不另外放進回應**——`code` 就是它，一個欄位夠了（§4.6）。
- RPC 層錯誤（1xx）的 exit code 一律 **1**（用法錯），除了 `105` 是 **130**（跟 Ctrl-C 一樣的慣例）。
- 協議層（9xxx）的 exit code：`9001` token 錯 → **1**；其餘 → **4**（網路：連線建不起來）。

## 6. 資料平面（HTTP，`http://127.0.0.1:<data port>`）

架構在 §4.8；這裡只定路徑與狀態碼。**沒有全域 token**，每個 URL 自己就是 capability。

### 6.1 讀：`GET /media/<token>`

| | |
|---|---|
| 來源 | `media.open` 的 `url` |
| 支援 | `Range: bytes=a-b`（單一 range）；沒 `Range` 就整檔 |
| 回 | `200`（整檔）／`206 Partial Content`（有 Range）；`Content-Type` 是 manifest 的 mimetype，沒有就 `application/octet-stream`；`Accept-Ranges: bytes`；`Content-Length` |
| `416` | Range 超出檔尾 |
| `404` | 認不得或過期的 token。🚫 不分辨兩者 |
| `503` | 未解鎖 |
| `502` | 從 server 拉塊失敗（快取沒有、server 又拿不到）。⚠️ 半途失敗時 HTTP 已經回 200 了，只能斷連線——這跟 `seek` 的「stdout 已印出去的不收回」是同一件事 |

daemon 邊解密邊吐（媒體池 64 KiB 段各自 AEAD），🚫 不整檔進記憶體。同一個 token 可以重複 GET（播放器 seek）。

🚨 **上游慢下來的時候：停止送 bytes，但連線開著**（維護者 2026-09-13 定）。
homeserver 給不出下一塊，daemon 就**卡在那裡**，等拿到了再繼續吐。

- 🚫 **不要回一個空回應**（UI 會以為「傳完了」）、🚫 **不要斷線**（UI 會以為「失敗了」）——
  事實是「還在等」，而 HTTP 表達「還在等」的方式就是**不送資料但不關連線**。
- ⭐ **這也是進度的來源**：UI 的下載進度就是它那個 GET 收到多少 bytes，
  🚫 不是 daemon 從 RPC 推回去的數字（§4）。上傳同理 —— 進度是它那個 PUT 送出去多少。
- ⚠️ 真的失敗（`502`）跟「慢」要分得開：**拿不到**才斷，**還在拿**就等。
  🚫 不要把逾時設得比 homeserver 的慢速還短，那會把「慢」誤判成「壞」。

### 6.2 寫：`PUT /upload/<token>`

| | |
|---|---|
| 來源 | `media.create` 的 `url` |
| body | 明文 bytes，**一條連線送到底**。`Content-Length` 有就用（＝固定大小上傳），沒有（chunked transfer）就是串流上傳（`0/0` 哨兵那套，約定 §2） |
| 回 | `200` 加 JSON body ＝ `Manifest`（CLI 規格 §5）。**回應在 `Seal` 完成之後才到**，所以 PUT 的回應就是「傳完了」 |
| `4xx`／`5xx` | JSON body `{ code, msg }`，code 用 §5 的表 |
| `404`／`503` | 同 §6.1 |
| `409` | 這個 token 已經有一個 PUT 在進行 |

- 進度走 RPC 的 `progress`，`id` 是 `media.create` 那一則的 `id`——所以前端要留著那個 `id`。
- 中途斷線：daemon 保留狀態檔（CLI 規格 §6），同一個 token **重新 PUT 可以續傳**，daemon 從 `Status` 問到收了幾塊、回 `100 Continue` 之前先跳過那些 bytes。⚠️ 續傳的細節（怎麼告訴前端從第幾 byte 送）第一版不做，先整個重送。
- token TTL 與撤銷（architecture-v2 §8 第 5 點）**第一版**：TTL 1 小時、`account.del` 時全部作廢（🚫 沒有 `vault.lock` 可以掛，§3.1）。

## 7. 一次完整的例子：Desktop 送一個 2 GB 的影片進 E2EE 房

```jsonc
→ { "method": "media.create", "params": { "room": "!r:localhost", "name": "v.mkv", "size": 2147483648, "mimetype": "video/x-matroska" }, "id": 12 }
← { "code": 0, "msg": "ok", "id": 12, "result": { "upload_id": 77, "mxc": "mxc://localhost/abc", "url": "http://127.0.0.1:51235/upload/7c1b…", "expires_in": 3600 } }

   前端同時做兩件事：
   (a) PUT http://127.0.0.1:51235/upload/7c1b…   ← bytes 開始流
   (b)
→ { "method": "room.send_attachment", "params": { "room": "!r:localhost", "upload_id": 77, "caption": "看這個" }, "id": 13 }
← { "code": 0, "msg": "ok", "id": 13, "result": { "event_id": "$e1", "mxc": "mxc://localhost/abc", "attachment_declared": true } }

← { "method": "progress", "params": { "id": 12, "done": 536870912, "total": 2147483648 } }
← { "method": "progress", "params": { "id": 12, "done": 1073741824, "total": 2147483648 } }
   …
   (a) 的 HTTP 回應到了：200，body 是 manifest → 傳完
```

(b) 失敗（`1400`）不影響 (a)；(a) 失敗（PUT 回 5xx）之後訊息已經在房間裡指著一個傳不完的檔——
這是 architecture-v2 §4.9 說的「兩件事」，前端要決定是重傳還是撤回訊息。

## 8. 跟 core 的差距（daemon PR 要順手補的）

| 缺什麼 | 給誰用 |
|---|---|
| 「建檔 → 拿 reader 邊收邊傳」的拆分：`send_file` 現在是一條龍 | `media.create`、`room.send_attachment`、`PUT /upload` |
| `PoolReader` 接到 HTTP Range：`download_to` 只會寫檔 | `GET /media` |
| `CoreEvent` 補 `SyncState` 與結構化的 `Progress { id, done, total }` | §4 |
| `CoreErrorKind` 配上 §5.2 的號碼（`impl From<CoreErrorKind> for u32` 之類，一個地方） | 所有錯誤回應 |

## 9. 明確不做的

- 🚫 沒有批次請求（JSON-RPC 的陣列形式）：一條連線本來就能並行。
- 🚫 沒有「哪個前端是主」：§4.7。
- 🚫 沒有壓縮：frame 上限 1 MiB，大的走資料平面。
- 🚫 沒有 WS text frame、🚫 pack 裡沒有長度欄位、🚫 `type` 不表示種類：一包一則、長度由 WS 給、種類在 JSON 裡（§1）。
- 🚫 daemon 不問終端、不彈視窗、不讀 passphrase 檔：全部從 RPC 進來（§4.5）。
- 🚫 `msg` 不當邏輯用、🚫 `code` 不重排、🚫 `method` 不改名——改名等於新 method 加舊的廢棄，廢棄的回 `101` 前先活一個版本。

## 10. 每個 method 的實作現況（2026-09-13；判準見檔頭）

「底層」是它最後跟 homeserver 講話走哪條。✅ 只給 **WS**；matrix-sdk 的 HTTP 與 HTTP fallback 都是 🔁「能動、要遷」；
core 沒有的是 ❌。daemon 那一層：pack、加密、hello、連線狀態機、WS listener、conf（`Settings`）、
**下表有 core 對應的 method 全部接上了**（`crates/wbf-daemon` 第二版）；訂閱／推播／cancel、資料平面 HTTP ❌。
這張表的「底層」只看 core 以下。📎 2026-09-13 對真 wbfuwunel 走過 `account.add → whoami → server.ping → room.list → sync.recent →
backup.status → account.del`（`tests/real_server.rs`，`--ignored`）。

| method | core | 底層 | 判定 |
|---|---|---|---|
| `hello`、`daemon.info`／`set_encryption`／`shutdown` | ✅ daemon 層 | 本機 | ✅ |
| `subscribe`／`unsubscribe`／`cancel` | ❌（daemon 層） | — | ❌ |
| `desync` 推播（§4） | ❌（daemon 層） | — | ❌ |
| **`sync` 參數**（§2：`room.list`／`get`／`history`／`files`／`media.info`）＋回應回報用了哪一種 | ✅ `SyncMode`：`local` 讀 `cache.db`、`server` 不寫庫、`both` 寫完再讀本地 | 本機（`local`）／同下面那幾列 | ✅ |
| `room.read`、`daemon.reload_conf` | ❌ | — | ❌ |
| `daemon.set_encryption`、conf 的 `server_backup`／`local_room_keys`／`transport` 填進 Target | ✅ daemon 層 | 本機 | ✅ |
| `vault.create`／`unlock`／`set_passphrase`／`remove_passphrase` | ✅ | 本機 | ✅ |
| `account.add` | ✅ | HTTP `/login` ＋ matrix-sdk | 🔁 `Session/Login` 只有 wire 常數（handover §6） |
| `account.list`／`switch` | ✅ | 本機 | ✅ |
| `account.whoami` | ✅ | HTTP `/whoami` | 🔁 |
| `account.del`／`destroy` | ✅ | HTTP `/logout` ＋ 本機 | 🔁 |
| `room.list`／`get` | ✅ | matrix-sdk `/sync` | 🔁 |
| `room.send_text` | ✅ | matrix-sdk `Room::send` | 🔁 `Event/Send` 等附件宣告（約定 §5.2）一起做 |
| `room.send_file` | ✅ | 上傳 **WS** ＋ 事件 matrix-sdk | 🔁 一半 |
| `room.send_attachment`、`media.create` | ❌ | — | ❌ |
| `room.history`（`source: server`） | ✅ | matrix-sdk `/messages` | 🔁 |
| `room.history`／`room.files`（`source: cache`） | ✅ | 本機 `cache.db` | ✅ |
| `sync.recent` | ✅ | **WS** `Event/Recent`＋`Batch` | ✅ |
| `room.message` 推播 | ✅（`CoreEvent::Message`，來自 `watch`） | matrix-sdk `/sync` | 🔁 daemon 版要接 `Event/Subscribe`／`Push` |
| `upload.file`／`status`／`abort` | ✅ | **WS**（`--transport http` 是 fallback） | ✅ |
| `media.info` | ✅ | **WS** `Info` | ✅ |
| `media.save_to` | ✅ | **WS** `Read`＋媒體池 | ✅ |
| `media.open`（Range） | ❌ 缺 `PoolReader` 接口 | 池讀 ✅、缺塊補拉 **WS** | ❌ |
| `media.stats`／`gc` | ✅ | 本機 | ✅ |
| `backup.*`、`recovery.*` | ✅ | matrix-sdk（backup／SSSS 全是 HTTP） | 🔁 而且金鑰的 to-device 收發要等 `0x16 Device`（to-device-client.md） |
| `server.ping` | ✅ | **WS** `Hello`／`Ping` | ✅ |
| `sync.state`／`vault.state` 推播 | ❌ | — | ❌ |

📎 讀法：✅ 那幾列是 wbf-sdk 第 2 步的產物（上傳／下載／`recent`／ping），它們從一開始就是 WS。
🔁 那些全部掛在 matrix-sdk 上，遷移的順序跟 architecture-v2 §6.1 四條線一致：房間（`Subscribe`／`Push`）→ 金鑰（`Device`）→ session（`Session/*`）。

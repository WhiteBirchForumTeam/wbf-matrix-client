# 交叉簽章、裝置驗證、金鑰備份：wbf 帳號補齊

> 現況：維護者 2026-11-07 核可（§10 五點照草稿）、同日實作；驗收項目與結果在 §11。實作時跟草稿不同的地方寫在 §12。
> 程式：sdk 的 `cross_signing.rs`、`key_backup.rs`、`secret_storage.rs`、`verification.rs`；core 的 `identity_ops.rs`（金鑰線開好與每批匯完之後）、
> `verify_ops.rs`（`verify.*`、`devices.list`）、`backup_ops.rs`（wbf 帳號那一半）；daemon 的 `handle/verify.rs`。
> 補的是 /docs/design/keys/e2ee-rpc.md §8 列的缺口：交叉簽章、wbf 帳號的金鑰備份、向自己其他裝置要金鑰；加上 /docs/handover.md §7 第 2 項剩下的裝置驗證。

## 0 範圍

- **只補 wbf 帳號。** 一般 Matrix 帳號有 matrix-sdk 的 `Client`，交叉簽章與備份由上游做（/docs/design/keys/room-key-backup.md §3）；wbf 帳號沒有 `Client`，直接開 `OlmMachine`（`crates/wbf-sdk/src/crypto_engine.rs` 的 `OlmEngine`），這四件都是空的。
- **server 不用改。** 要用的端點都已經走橋：交叉簽章 `0x17 0x24`／`0x25`、備份 `0x17 0x30`–`0x3D`、account data `0x11 0x25`／`0x26`（wbfuwunel 的 /docs/bridge-specs/0x17-keys.md、/docs/bridge-specs/0x11-account.md）；驗證訊息走原本的 to-device（wbfuwunel 的 /docs/design/keys/e2ee-over-channel.md §1：「不動」）。
- **RPC 跟一般帳號同一組。** `backup.*`、`recovery.*` 照 /docs/design/rpc-specs/rpc-spec.md §3 原本的形狀，UI 不必分帳號種類。新加的 `verify.*`、`devices.list` **只有 wbf 帳號**：一般 Matrix 帳號沒有常駐的同步迴圈（/docs/handover.md §6），
  收不到對方的驗證訊息，叫了回 `1100`（§12）。
- **「Matrix 原本的金鑰分發不動」**（維護者 2026-09-17，wbfuwunel 的 /docs/design/keys/e2ee-over-channel.md §3）：分享策略、解密時的信任要求都照 matrix-sdk 的預設（§8）。

## 1 四件事，上游給了什麼、我們補什麼

| 事 | 上游（`matrix-sdk-crypto`，不靠 `Client`） | 我們補 |
|---|---|---|
| 交叉簽章 | `OlmMachine::bootstrap_cross_signing` 產三個請求；私鑰存 crypto store | 照順序送那三個請求、回報結果（`UploadSigningKeysRequest` **不**從 `outgoing_requests()` 出來，要自己送） |
| 裝置驗證（SAS） | `VerificationRequest`、`Sas`：表情、確認、取消；收到的 to-device 驗證事件已經經過 `receive_sync_changes_msc4186` 進去了 | 把每一步回傳的請求送出去；`confirm()` 回的簽章要自己送；RPC 與推播 |
| 金鑰備份 | `BackupMachine`（`enable_backup_v1`、`backup()`）、`BackupDecryptionKey`（產生、解密備份） | 建版本、上傳迴圈、下載解密匯入；`KeysBackupRequest` 一樣要自己送 |
| secret storage（recovery key） | `secret_storage::SecretStorageKey`（產生、加解密、base58） | account data 的讀寫與「哪個秘密放哪個名字」那層（matrix-sdk 那層綁 `Client`，自己寫約 200–300 行） |
| 向自己裝置要金鑰 | `automatic-room-key-forwarding` feature（解不開時自動要、回應自己已驗證裝置的請求）；`query_missing_secrets_from_other_sessions` | 開 feature（§7）；備份金鑰從秘密收件匣拿出來、對過 server 再存 |

## 2 端點表要補的列

照 /docs/design/rooms/room-actions.md 的做法：一張端點表兩條路（wbf 帳號走橋、一般帳號同一個 HTTP 端點），加在 `crates/wbf-sdk/src/matrix_endpoint.rs`。

| 名字 | 橋 | HTTP |
|---|---|---|
| `SIGNING_KEYS_UPLOAD` | `0x17 0x24` | `POST /keys/device_signing/upload` |
| `SET_ACCOUNT_DATA` | `0x11 0x26` | `PUT /user/{user_id}/account_data/{event_type}` |
| `CREATE_BACKUP_VERSION` | `0x17 0x30` | `POST /room_keys/version` |
| `LATEST_BACKUP_INFO` | `0x17 0x31` | `GET /room_keys/version` |
| `ADD_BACKUP_KEYS` | `0x17 0x35` | `PUT /room_keys/keys?version=` |
| `GET_BACKUP_KEYS` | `0x17 0x38` | `GET /room_keys/keys?version=` |

讀、寫 account data 已經在端點表裡（`GET_ACCOUNT_DATA`、`SET_ACCOUNT_DATA`，房間動作那支加的）；簽章上傳用 `protocol.rs` 既有的 `BRIDGE_SIGNATURES_UPLOAD`（ruma 的型別化請求）。
這幾支只有 wbf 帳號會走（一般帳號由 `Client` 自己送），放進端點表是為了同一套變數檢查與回應解法。刪除備份版本**不**加（/docs/design/keys/room-key-backup.md §9：不可逆，要刪去別的 client）。

## 3 交叉簽章：什麼時候建

- **第一台裝置自動建**：開 `Keys` 線、自己的金鑰上傳完之後（`init_keys`，/docs/design/keys/e2ee-rpc.md §5），查自己（`KeysQuery`）在 server 上有沒有 master key；**沒有**就 `bootstrap_cross_signing(false)`，照順序送三個請求。第一次上傳 server **不**要密碼（UIAA），wbfuwunel 的 /docs/bridge-specs/0x17-keys.md `0x24`。
  一般帳號的 `Client` 開的是 `auto_enable_cross_signing: true`（/docs/design/keys/room-key-backup.md §3），這樣兩種帳號一致。
- **server 上已經有**（別台裝置建的）：**不**建。這台是「自己的、還沒驗證的裝置」，等 §5 的自我驗證或 §4 的 `backup.restore` 把私鑰拿過來。
- 中途斷了：私鑰在送之前就存進 crypto store；下次開線再查一次，server 還沒有就重送同一份（重送一模一樣的不要密碼）。
- **換掉**交叉簽章（server 要密碼）這支**不做**（§9）。
- ⚠️ **會讓房間版本號變**：上傳交叉簽章金鑰、簽自己的裝置都會改自己的裝置版本號（雜湊也變；簽別人只讓序號 +1、雜湊不變，wbfuwunel 的 /docs/design/keys/room-device-version.md）。
  所以第一台建交叉簽章、每次自我驗證或 `backup.restore` 簽完這台，自己在的每間加密房的房間版本號都換了。走的是既有的路：server 推 `devices.changed`、UI 帶舊號碼送出時被 1506 擋、daemon 自動重拿之後回 1401、UI 重送（/docs/design/keys/e2ee-rpc.md §3、§4）。
  這支沒改那條路，也沒有另外測「建完、驗證完之後的第一則被 1506 擋、重送」這一步（由 /docs/design/keys/e2ee-rpc.md §9 的測試守）。

## 4 金鑰備份與 recovery key

### 4.1 備份版本與上傳

- **建版本**：跟交叉簽章同一個時機——server 上沒有備份版本就建一個（`BackupDecryptionKey::new` → `sign_backup` → `CREATE_BACKUP_VERSION` → `enable_backup_v1`），解密金鑰存 crypto store（`save_decryption_key`）。
  跟一般帳號的 `auto_enable_backups` 一樣：**在產生 recovery key 之前，server 那份換機器也解不開**（/docs/design/keys/room-key-backup.md §3 那段警告照用）。
- server 上**已經有**版本、而這台沒有它的解密金鑰：**不**建新的、**不**上傳，等 §4.2 的 restore 或 §5 的自我驗證把金鑰拿過來。
  這台的上傳要是開著就關掉（`disable_backup`，存著的金鑰不刪）：上游開引擎時會把 store 存的舊版本自動接上，server 的版本被別台換掉之後，不關就會每來一把新金鑰往舊版本送一次、被拒一次（PR #84 審查）。server 上沒有任何版本而不建時一樣。
- **上傳：`Keys` 線的後台自動做**（要點頭，§10 第 2 點）：每次建了自己的房間金鑰、或匯進一批別人的，就跑 `backup()` 到沒有東西為止。
  維護者 2026-09-09 定「命令觸發」是因為一命令一程序時「每個命令都要等上傳」（/docs/design/keys/room-key-backup.md §6）；daemon 的後台**不**擋任何命令，所以這個理由對 wbf 帳號不成立。
  `backup.upload` 照留，意思變成「等後台追平才回」。一次最多繞 1000 批（約 10 萬把），繞滿還有就停、發一則 `progress` 說「還有、再叫一次」，而**不是**默默當成追平。
- **`SERVER_BACKUP` 管的是「這台建不建版本、傳不傳」**，不管「收到的備份金鑰用不用」：自我驗證收到的、`backup.restore` 打開的備份金鑰，對上 server 的版本就存起來、還原，不看開關（先存才還原的理由在 §5.2）。
  存的時候上游會把上傳標成開著（`backup.status` 的 `uploading_locally` 是 `true`），但後台的三個上傳點都先看 `SERVER_BACKUP`，關著就一個 byte 都不送（PR #84 審查）。

### 4.2 recovery key（secret storage）

| RPC | wbf 帳號做什麼 |
|---|---|
| `backup.create_recovery_key` | server 上已經有預設的 secret storage key 就拒絕（1100）：換一把會讓使用者抄下來的那串立刻打不開，輪替不在這支（§9）。沒有才 `SecretStorageKey::new` → 寫 `m.secret_storage.key.<id>` 與 `m.secret_storage.default_key` → 把四個秘密（`m.cross_signing.master`／`self_signing`／`user_signing`、`m.megolm_backup.v1`）加密寫進 account data → 封進 `<data dir>/r/`（/docs/design/keys/room-key-backup.md §8）→ 回 `{ recovery_key }`，只回這一次 |
| `backup.restore { recovery_key? }` | 拿 recovery key 打開 secret storage：有帶就用帶的（換機器時這台的 `r/` 沒有；用成功才封進 `r/`），沒帶就用 `r/` 封著的 → `import_cross_signing_keys`（這台用 self-signing key 簽自己、上傳簽章 → **這台變成已驗證**）→ 存備份解密金鑰、`enable_backup_v1` → 下載全部備份、解密、`import_room_keys` → 走既有的「金鑰到了補解」（/docs/design/keys/e2ee-rpc.md §6），解開的發 `room.message`。簽完這台之後的下載失敗（例：整份超過 2 MiB）**不**回錯：這台已經被信任、recovery key 照樣封進 `r/`，發一則 `progress`，再叫一次就是重試（PR #84 審查） |
| `backup.status` | 欄位照舊；`recovery_state` 照上游 `Recovery::check_recovery_state` 的算法：沒有 secret storage 是 `Disabled`；有，而這台三把交叉簽章私鑰齊、備份那半也齊（上傳開著，或 server 上根本沒有備份）是 `Enabled`；其餘 `Incomplete`。`recovery_enabled` 只認 `Enabled`（跟一般帳號同一句話，PR #84 審查）。`backup.restore`、`backup.upload` 回的也是同一支算的 |
| `backup.save`／`backup.import`（本地快照） | 上游 `export_room_keys`＋`encrypt_room_key_export` 有，接上同一個 `k/snapshot` |

- `account.del` 的閘門（/docs/design/keys/room-key-backup.md §7）：wbf 帳號**照舊只看第二關**（這台保管著 recovery key）。第一關（server 上有備份、secret storage 設好）現在問得出來了，但這支沒接（§12）。
- recovery key 的格式是 secret storage 的 base58（跟 Element 的「安全金鑰」同一種），**不**收 passphrase 版（§9）。

## 5 裝置驗證（to-device 的 SAS）

只做表情／數字比對（SAS）。兩種對象：

- **自己的另一台裝置**（自我驗證）：`OwnUserIdentity::request_verification`，送給自己所有裝置。
- **別人的某一台裝置**：`Device::request_verification`，送給那一台。（驗證「別人這個人」要在房間裡發請求，這支**不做**，§9。）

**要不要接受、表情對不對，一律 UI 問使用者**（/docs/design/overview/architecture-v2.md §3：**不**代前端做決定）。daemon 只轉送、只照命令做。

### 5.1 RPC 與推播（形狀示意，未實跑）

| method | params | 回 |
|---|---|---|
| `verify.request` | `{ user_id, device_id?, user?, server? }`：`user_id` 是自己時 `device_id` 可省（送給自己所有被交叉簽章簽過的裝置） | `VerifyState`（`requested`，帶 `flow_id`） |
| `verify.accept` | `{ flow_id, user_id, … }`：接受對方的請求、或對方開始的 SAS | `VerifyState` |
| `verify.start_sas` | `{ flow_id, user_id, … }` | `VerifyState` |
| `verify.confirm` | `{ flow_id, user_id, … }`：使用者說表情一樣 | `VerifyState` |
| `verify.cancel` | `{ flow_id, user_id, … }` | `VerifyState` |
| `verify.list` | `{ user?, server? }` | `{ flows: [VerifyState] }`：還在進行的 |

每一支都回那個流程**做完這一步之後**的樣子（跟推播同一個形狀）；流程不在狀態機裡（從沒有過、已經結束、daemon 重開了）是 `1100`。

推播 `verify.state`（示意；e2e 斷言的是同一組欄位）：

```json
{ "user": "@alice:localhost", "flow_id": "<…>", "other_user": "@alice:localhost", "other_device": "<…>",
  "we_started": false, "state": "keys_exchanged",
  "emoji": [{ "symbol": "🐶", "description": "Dog" }, "…共 7 個"], "decimals": [1234, 5678, 9012] }
```

`state`：`requested` → `ready` → `started` → `keys_exchanged`（帶 `emoji`／`decimals`）→ `confirmed`（自己按了、等對方）→ `done`；任何一步都可能 `cancelled`（帶 `reason`）。
推播要訂閱 `verify.state`（/docs/design/rpc-specs/rpc-spec.md §3.9）。

### 5.2 收到請求、確認之後

- **收到請求前先查對方的裝置**：上游收到請求時查不到對方那台的金鑰就**丟掉**請求。標準 Matrix 靠 `/sync` 的 `device_lists.changed` 先讓它重查，我們沒有那條，
  所以金鑰線每匯一批之前，這批裡有驗證請求就先追蹤、重查送請求的人（`OlmEngine::import_items`）。剛登入的自己的裝置、沒有共同房間的別人都是這種情況（e2e 實測踩到）。
- `confirm()` 回的簽章（自我驗證是 self-signing key 簽那台；別人是 user-signing key 簽那個人）**自己送**（`0x17 0x25`），回覆的 `failures` 不是空的就當失敗回給 UI（server 的 `0x25` 個別失敗仍是 200）。
- **自我驗證**完：上游自動排「跟對方要缺的秘密」，由後台送出去。交叉簽章私鑰收到就被上游匯入；**備份解密金鑰**落在秘密收件匣，daemon 拿出來對過 server 的備份版本（`backup_key_matches`）才存、`enable_backup_v1`，然後**自動下載還原**（§10 第 3 點），跟 `backup.restore` 後半段同一條路。
  順序是**存 → 清收件匣 → 下載**（PR #84 審查）：這把金鑰只在收件匣裡，先清或先下載的話，下載一失敗（整份超過 2 MiB 就會）這台就永遠拿不到它。
  問不到 server、存不進去時收件匣留著，下一批匯完再試；下載失敗時金鑰已經存著、上傳照開；要再拉一次是 `backup.restore`（要 recovery key）。整份超過 2 MiB 的根本解法是逐房下載（§11.4 的已知限制）。
- **完成就重查雙方的金鑰**（隔 3 秒再查一次）：剛上傳的簽章要回到 crypto store，`devices.list` 才看得到；對方的簽章可能比「完成」晚一步才上傳。
- **金鑰線每匯完一批就把狀態機排的請求送掉**（`identity_ops::after_import`）：驗證的回應、別台要的秘密、轉給自己裝置的房間金鑰都是收的時候排好的，沒人送就永遠停在那裡。
- ⚠️ **上游會把完成、取消的流程清出它的快取**（每次 `outgoing_requests` 的 `garbage_collect`），之後用 flow id 查不到。所以看流程的一律**先拿到上游的物件、一直握著**（sdk 的 `WatchedFlow`）：
  觀察者每 250 毫秒看一次握著的那份、有變就推；做完一步要回狀態的 RPC 也是動手前先拿。e2e 實測踩到：只靠 flow id 重查，`done` 永遠推不出去。
- ⚠️ **驗證流程只在記憶體裡**（上游的 `VerificationCache` 不存檔）：daemon 重開，進行中的全部消失、**不**發 `cancelled`。UI 收到 daemon 重開（RPC 連線重連，/docs/design/rooms/room-sync.md §3.1）就把畫面上的驗證當作沒了。上游自己 10 分鐘逾時。

## 6 給 UI 的信任資訊

`devices.list { user_id, user?, server? }`（示意）：

```json
{ "user_id": "@bob:localhost", "identity_verified": true,
  "devices": [{ "device_id": "<…>", "display_name": "<…>", "verified": true, "is_this_device": false }] }
```

- `verified`：這台被它主人的交叉簽章簽過、而且那個人被我信任（自己：這台有自己的 master 私鑰）；別台的話，我本地手動信任過也算。
  ⚠️ **這台自己只看交叉簽章**：上游把「自己這台」一律標成本地信任，用它的 `is_verified()` 會讓還沒被驗證的新裝置說自己是 verified（e2e 實測踩到）。
- **先重查那個人的金鑰**（走金鑰線）再讀 crypto store：讀的才是 server 現在的簽章（§12）。
- `display_name` 沒有時**不出現**（不是 `null`），跟 `VerifyState` 的選填欄位同一個規矩。

## 7 向自己其他裝置要金鑰

- 開 `matrix-sdk-crypto` 的 `automatic-room-key-forwarding`（§10 第 4 點）。matrix-sdk 的預設 feature 就有它（`vendor/matrix-rust-sdk/crates/matrix-sdk/Cargo.toml` 的 `default`），我們的依賴沒開；開了才是「Matrix 原本的樣子」：
  - 解不開時自動向自己的裝置要（上游只在「自己至少有一台已驗證的裝置」時才要）；
  - 回應別人的請求：自己已驗證的裝置給全部，別人只給當初分給他們的那一段。
- 上游是在「解不開」那一刻把請求排進去，**沒人送**：`room.history` 補解之後還有解不開的，就把排好的送出去（只用開著的金鑰線）。
  對方回的 `m.forwarded_room_key` 走既有的「金鑰到了補解」，下一次讀就是明文（V11 驗過：沒有備份、只靠自己的裝置）。

## 8 照 matrix-sdk 預設、不改的

- **分享策略維持 `AllDevices`**（上游 `CollectStrategy` 的預設）。`crypto_engine.rs` 的 `room_key_share_settings` 那句「交叉簽章好了換 `IdentityBased`」拿掉，改寫成「照 matrix-sdk 預設（維護者 2026-09-17：Matrix 原本的金鑰分發不動）」。
- **解密的信任要求維持 `Untrusted`**（`decryption_settings`）。

## 9 這支不做

- 換掉交叉簽章（server 要密碼 UIAA）——之後要做就是一支 `keys.reset_identity { password }`。
- QR 驗證（要開上游的 `qrcode` feature）、在房間裡驗證「別人這個人」。
- recovery 的 passphrase 版、刪除 server 上的備份版本。

## 10 維護者點頭的（2026-11-07 核可）

1. **交叉簽章與備份版本：第一台裝置自動建**（§3、§4.1），跟一般帳號的 `auto_enable_*` 一致。
2. **備份上傳：wbf 帳號由 `Keys` 線後台自動做**（§4.1），不再是命令觸發；`backup.upload` 變成「等追平」。
3. **自我驗證完拿到備份金鑰就自動下載還原**（§5.2），不等 UI 叫 `backup.restore`。
4. **開 `automatic-room-key-forwarding`**（§7）。
5. **RPC 形狀**：`verify.*` 六支、`verify.state` 推播、`devices.list`（§5.1、§6）。

## 11 驗收（維護者 2026-11-07：先寫進文件、寫完自主驗證）

每一條都要有一個測試對應；做完在 PR 裡逐條填「哪個測試、結果」。**真 server 那組才是驗收本體**，單元與假 server 的是讓壞掉時知道壞在哪一層。

### 11.1 真 server（本機 wbfuwunel，daemon 的 `real_server.rs`，`--ignored`）

同一個帳號開多台裝置＝多個 daemon handle、各自的資料目錄。帳號是每次跑時**現註冊**的（要 `WBF_E2E_REGISTRATION_TOKEN`）：
別的 e2e 也登入 alice、bob，她們早就有交叉簽章與備份了，「第一台裝置建」只有全新的帳號測得到。下表的 alice／bob／carol 指的是三個角色。

| # | 情境 | 通過的條件 |
|---|---|---|
| V1 | alice 第一台（A1）登入、開線 | server 上 alice 有 master／self-signing／user-signing（另一個 client 的 `KeysQuery` 看得到）；有備份版本（`LATEST_BACKUP_INFO`）；A1 的 `devices.list(alice)` 裡 A1 是 `verified` |
| V2 | A1 建一間加密房、送一則，`backup.upload` | 回來之後 server 那份備份的 `count` ≥ 1 |
| V3 | A1 `backup.create_recovery_key` | 回一串 recovery key；account data 有 `m.secret_storage.default_key` 與它指的 key；四個秘密都在 |
| V4 | alice 第二台（A2）登入 | A2 **不**建新的 master（server 上的 master 公鑰跟 V1 一樣）；A2 的 `devices.list(alice)` 裡 A2 **不是** `verified`；A2 讀 V2 那則是解不開的 |
| V5 | A2 `verify.request`（自己）→ A1 收到 `verify.state requested` → A1 `verify.accept` → A2 `verify.start_sas` → A1 `verify.accept` | 兩邊都推 `keys_exchanged`，**兩邊的 7 個表情逐一相同** |
| V6 | 兩邊 `verify.confirm` | 兩邊都推 `done`；A1 與 A2 的 `devices.list(alice)` 裡兩台都 `verified` |
| V7 | V6 之後（不叫任何 RPC） | A2 自動拿到備份金鑰、還原：收到 V2 那則的 `room.message`（同一個 `event_id`、解開的） |
| V8 | alice 第三台（A3）登入，`backup.restore { recovery_key }`（V3 那串） | A3 變成 `verified`；讀得到 V2 那則 |
| V9 | A3 用**錯的** recovery key `backup.restore` | 回錯誤；A3 **仍然不是** `verified`、什麼都沒匯入（fail closed） |
| V10 | bob 對 alice 的 A1 `verify.request { user_id: alice, device_id: A1 }`，兩邊走完 SAS | bob 的 `devices.list(alice)` 裡 A1 是 `verified`、`identity_verified` 是 `true` |
| V11 | 向自己裝置要金鑰：另一個帳號（carol），兩台都是 conf `SERVER_BACKUP=off`（沒有備份可還原，只剩這條路）。C1 在加密房送一則 → C2 登入、跟 C1 自我驗證 → C2 讀那則 | C2 收到那則解開的 `room.message`（金鑰是 C1 轉過來的） |
| V12 | `verify.cancel` 中途取消 | 兩邊都推 `cancelled`；之後那個 `flow_id` 的 `verify.confirm` 回錯誤 |

### 11.2 單元與假 server

| 層 | 驗什麼 |
|---|---|
| sdk | 端點表新列的前 4 bytes 逐 byte 釘；secret storage 那層：建 → 寫進 account data 的內容 → 用同一把 key 讀回四個秘密；錯的 key → 錯誤、**不**回任何秘密；沒有 `default_key` → 沒有 secret storage |
| core 假 server | server 沒有 master → 送 `SIGNING_KEYS_UPLOAD` 再送 `SIGNATURES_UPLOAD`（順序）；已經有 → 什麼都不送；沒有備份版本 → 建；有版本但這台沒金鑰 → **不**建、**不**上傳；`SERVER_BACKUP=off` → 不建不傳；簽章回覆的 `failures` 不是空的 → 回錯誤。PR #84 審查後加：server 的版本被換掉 → 這台關掉上傳；對上了但下載失敗 → 金鑰照樣存著；第二次建 recovery key → 拒絕、server 上什麼都沒變、第一串照樣打得開；`recovery_state` 三種值跟上游同一個算法 |
| daemon | `verify.*`、`devices.list`、`backup.restore` 的壞參數是 102；`verify.state` 推播的形狀；`daemon.info` 之類既有的不受影響 |

### 11.3 收尾

- 全套測試、clippy（`-D warnings`）各一次；真 server e2e 整套（含舊的 20 條）。
- 變異驗證（先 commit）：至少拿掉「server 有 master 就不建」、「錯的 recovery key 不匯入」、「`failures` 不是空的就錯」、「`SERVER_BACKUP=off` 不上傳」各一次，對應的測試要紅。
- **沒驗到的老實寫**：一般 Matrix 帳號那條（本機 server 走不到 HTTP 的那幾個端點）。

### 11.4 結果（2026-11-07，本機 wbfuwunel）

| # | 測試（daemon 的 `real_server.rs`） | 結果 |
|---|---|---|
| V1–V9、V12 | `cross_signing_backup_recovery_and_self_verification_across_three_devices` | ✅ |
| V10 | `verifying_another_users_device_trusts_that_device_and_identity` | ✅ |
| V11 | `a_new_device_gets_old_room_keys_from_a_verified_device_without_a_backup` | ✅ |

- 單元與假 server：core `identity_ops.rs` 六條（§11.2 core 那一列全部，加 secret storage 的建、讀回、錯的 key）、sdk `verification.rs` 一條與 `tests/unit.rs` 的端點位元組、daemon 的 102 表與 `verify.state` 推播形狀。
- 收尾：全套 998 條、clippy（`-D warnings`）乾淨；真 server e2e 整套 23 條全過（舊的 20 條裡 `login_ping_rooms_recent_and_logout_over_the_daemon` 原本釘著「wbf 帳號 `backup.status` 回 1100」，改成「走橋、server 上有備份」）。
- 變異驗證，全紅：拿掉「server 上有 master 就不建」、簽章 `failures` 當成功、`SERVER_BACKUP=off` 照樣建、匯入前**不**重查送驗證請求的人（真 server 的 V10）。
  「錯的 recovery key 不匯入」造不出有意義的變異：擋它的是上游 `from_account_data` 的 MAC 檢查，拿掉它錯的 key 也解不開秘密、缺一把就整個不匯——三層都是密碼學或「缺就不匯」，沒有一條分支改掉會變成匯了。
- 已知限制：整份備份超過 2 MiB 時橋回 `TooLarge`（約幾千把金鑰），還原會失敗；要改成逐房下載（`0x17 0x39`）再另開一支。

## 12 實作時跟草稿不同的地方（2026-11-07，照預設做、寫在這裡給維護者審）

| 草稿 | 實際 | 為什麼 |
|---|---|---|
| `verify.*`、`devices.list` 兩種帳號都接 | **只有 wbf 帳號**，一般帳號回 `1100` | 一般帳號沒有常駐的同步迴圈，對方的驗證訊息收不到，接了也走不完 |
| `verify.request` 回 `{ flow_id }`、其他回 `{ ok }` | 每一支都回 `VerifyState`（含 `flow_id`） | UI 不必等推播就知道做完這一步的樣子；跟推播同一個形狀 |
| `devices.list` 只讀 crypto store | 先重查那個人的金鑰再讀 | 只讀本地，對方剛上傳的簽章看不到（驗證完「還不是 verified」） |
| 自己這台的 `verified` 用上游的 `is_verified()` | 自己這台只看交叉簽章 | 上游把自己這台一律標成本地信任（§6） |
| 登出閘門第一關兩種帳號一致 | wbf 帳號照舊只看第二關 | 不在這支的驗收裡；要接另開一支（第二關才是真的，/docs/design/keys/room-key-backup.md §7） |
| （草稿沒寫） | 匯入前先追蹤、重查送驗證請求的人；每批匯完送掉排好的請求；握著 `WatchedFlow`；`room.history` 送出要金鑰的請求 | 都是 e2e 實測踩到的（§5.2、§7） |
| §11.2 secret storage 在 sdk 單元測 | 在 core 的假 server 測（`identity_ops.rs`） | 要一台會存 account data 的 server；core 的假 server 已經有 |

## 13 審查後改的（PR #84 第一輪，2026-11-07）

| 意見 | 改成 | 在哪 |
|---|---|---|
| 收件匣的備份金鑰先刪才還原，下載失敗就永遠丟了（rumia） | 存 → 清收件匣 → 下載；`restore_room_keys` 也是對上就先存、再下載（oliver） | §5.2 |
| `backup.restore` 簽完這台、下載失敗時整支回錯（oliver） | 只講一聲、照樣回狀態，recovery key 照樣封進 `r/` | §4.2 |
| `recovery_state` 沒算備份金鑰、`recovery_enabled` 跟一般帳號語意不同（rumia、salvia） | 照上游 `check_recovery_state`；`recovery_enabled` 只認 `Enabled`；三支 RPC 同一支算 | §4.2 |
| 第二次 `backup.create_recovery_key` 會讓抄下來的那串失效（rumia） | 已經有 secret storage 就拒絕 | §4.2 |
| server 的版本換掉之後這台一直往舊版本傳（oliver） | `NoKeyHere`／`NoBackup` 時關掉上傳 | §4.1 |
| 上傳繞滿 1000 批是靜默的 `Ok`（cirno） | 回 `StoppedAtLimit`、發 `progress` | §4.1 |
| 收到的備份金鑰不看 `SERVER_BACKUP`，文件沒寫（cirno） | 行為不變，契約寫進 §4.1 | §4.1 |
| `display_name` 送 `null`（cirno） | 沒有就不出現 | §6 |
| 觀察者 task panic 時清單不清（rumia） | 用 drop 時清的守衛 | `verify_ops.rs` |
| `upload_quietly` 的帳號字寫成 `"room keys"`（rumia） | 傳帳號的 user id | `key_share.rs` |
| （修完重跑 e2e 時踩到）core 的 `an_encrypted_conversation_survives_a_new_device_over_the_real_server` 連跑幾次都紅：金鑰與密文同時到，兩邊都沒解到 | 房間推播寫進 cache 之後再解一次 | /docs/design/keys/e2ee-rpc.md §6 |

沒改的：只給 QR 的驗證請求照樣推 `requested`（rumia）——UI 該知道有人來問，`verify.start_sas` 會回錯；
任何人送驗證請求就被追蹤（oliver）——有上限（上游 10 分鐘逾時），要擋另開一支；`count` 缺時當 0（salvia）——只是計數。
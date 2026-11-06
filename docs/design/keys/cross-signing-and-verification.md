# 交叉簽章、裝置驗證、金鑰備份：wbf 帳號補齊（草稿）

> 現況：**草稿**（2026-11-07），等維護者看過、說開工。§10 是要點頭的地方。
> 補的是 /docs/design/keys/e2ee-rpc.md §8 列的缺口：交叉簽章、wbf 帳號的金鑰備份、向自己其他裝置要金鑰；加上 /docs/handover.md §7 第 2 項剩下的裝置驗證。

## 0 範圍

- **只補 wbf 帳號。** 一般 Matrix 帳號有 matrix-sdk 的 `Client`，交叉簽章與備份由上游做（/docs/design/keys/room-key-backup.md §3）；wbf 帳號沒有 `Client`，直接開 `OlmMachine`（`crates/wbf-sdk/src/crypto_engine.rs` 的 `OlmEngine`），這四件都是空的。
- **server 不用改。** 要用的端點都已經走橋：交叉簽章 `0x17 0x24`／`0x25`、備份 `0x17 0x30`–`0x3D`、account data `0x11 0x25`／`0x26`（wbfuwunel 的 /docs/bridge-specs/0x17-keys.md、/docs/bridge-specs/0x11-account.md）；驗證訊息走原本的 to-device（wbfuwunel 的 /docs/design/keys/e2ee-over-channel.md §1：「不動」）。
- **RPC 跟一般帳號同一組。** `backup.*`、`recovery.*` 照 /docs/design/rpc-specs/rpc-spec.md §3 原本的形狀，UI 不必分帳號種類；新加的 `verify.*`、`devices.list` 兩種帳號都接（一般帳號走 `Client` 的同名功能）。
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

讀 account data 已經有（`protocol.rs` 的 `BRIDGE_ACCOUNT_DATA`），一併搬進端點表。刪除備份版本**不**加（/docs/design/keys/room-key-backup.md §9：不可逆，要刪去別的 client）。

## 3 交叉簽章：什麼時候建

- **第一台裝置自動建**：開 `Keys` 線、自己的金鑰上傳完之後（`init_keys`，/docs/design/keys/e2ee-rpc.md §5），查自己（`KeysQuery`）在 server 上有沒有 master key；**沒有**就 `bootstrap_cross_signing(false)`，照順序送三個請求。第一次上傳 server **不**要密碼（UIAA），wbfuwunel 的 /docs/bridge-specs/0x17-keys.md `0x24`。
  一般帳號的 `Client` 開的是 `auto_enable_cross_signing: true`（/docs/design/keys/room-key-backup.md §3），這樣兩種帳號一致。
- **server 上已經有**（別台裝置建的）：**不**建。這台是「自己的、還沒驗證的裝置」，等 §5 的自我驗證或 §4 的 `backup.restore` 把私鑰拿過來。
- 中途斷了：私鑰在送之前就存進 crypto store；下次開線再查一次，server 還沒有就重送同一份（重送一模一樣的不要密碼）。
- **換掉**交叉簽章（server 要密碼）這支**不做**（§9）。

## 4 金鑰備份與 recovery key

### 4.1 備份版本與上傳

- **建版本**：跟交叉簽章同一個時機——server 上沒有備份版本就建一個（`BackupDecryptionKey::new` → `sign_backup` → `CREATE_BACKUP_VERSION` → `enable_backup_v1`），解密金鑰存 crypto store（`save_decryption_key`）。
  跟一般帳號的 `auto_enable_backups` 一樣：**在產生 recovery key 之前，server 那份換機器也解不開**（/docs/design/keys/room-key-backup.md §3 那段警告照用）。
- server 上**已經有**版本、而這台沒有它的解密金鑰：**不**建新的、**不**上傳，等 §4.2 的 restore 或 §5 的自我驗證把金鑰拿過來。
- **上傳：`Keys` 線的後台自動做**（要點頭，§10 第 2 點）：每次建了自己的房間金鑰、或匯進一批別人的，就跑 `backup()` 到沒有東西為止。
  維護者 2026-09-09 定「命令觸發」是因為一命令一程序時「每個命令都要等上傳」（/docs/design/keys/room-key-backup.md §6）；daemon 的後台**不**擋任何命令，所以這個理由對 wbf 帳號不成立。
  `backup.upload` 照留，意思變成「等後台追平才回」。

### 4.2 recovery key（secret storage）

| RPC | wbf 帳號做什麼 |
|---|---|
| `backup.create_recovery_key` | `SecretStorageKey::new` → 寫 `m.secret_storage.key.<id>` 與 `m.secret_storage.default_key` → 把四個秘密（`m.cross_signing.master`／`self_signing`／`user_signing`、`m.megolm_backup.v1`）加密寫進 account data → 封進 `<data dir>/r/`（/docs/design/keys/room-key-backup.md §8）→ 回 `{ recovery_key }`，只回這一次 |
| `backup.restore` | 拿 `r/` 封著的 key 打開 secret storage → `import_cross_signing_keys`（這台用 self-signing key 簽自己、上傳簽章 → **這台變成已驗證**）→ 存備份解密金鑰、`enable_backup_v1` → 下載全部備份、解密、`import_room_keys` → 走既有的「金鑰到了補解」（/docs/design/keys/e2ee-rpc.md §6），解開的發 `room.message` |
| `backup.status` | 欄位照舊；`recovery_state` 從「secret storage 有沒有、這台有沒有四個秘密」算 |
| `backup.save`／`backup.import`（本地快照） | 上游 `export_room_keys`＋`encrypt_room_key_export` 有，接上同一個 `k/snapshot` |

- `account.del` 的閘門（/docs/design/keys/room-key-backup.md §7）：wbf 帳號現在只看第二關；有了這些之後第一關也問得出來，兩種帳號一致。
- recovery key 的格式是 secret storage 的 base58（跟 Element 的「安全金鑰」同一種），**不**收 passphrase 版（§9）。

## 5 裝置驗證（to-device 的 SAS）

只做表情／數字比對（SAS）。兩種對象：

- **自己的另一台裝置**（自我驗證）：`OwnUserIdentity::request_verification`，送給自己所有裝置。
- **別人的某一台裝置**：`Device::request_verification`，送給那一台。（驗證「別人這個人」要在房間裡發請求，這支**不做**，§9。）

**要不要接受、表情對不對，一律 UI 問使用者**（/docs/design/overview/architecture-v2.md §3：**不**代前端做決定）。daemon 只轉送、只照命令做。

### 5.1 RPC 與推播（形狀示意，未實跑）

| method | params | 回 |
|---|---|---|
| `verify.request` | `{ user_id, device_id?, user?, server? }`：`user_id` 是自己時 `device_id` 可省（送給自己所有裝置） | `{ flow_id }` |
| `verify.accept` | `{ flow_id, user_id, … }`：接受對方的請求、或對方開始的 SAS | `{ ok }` |
| `verify.start_sas` | `{ flow_id, user_id, … }` | `{ ok }` |
| `verify.confirm` | `{ flow_id, user_id, … }`：使用者說表情一樣 | `{ ok }` |
| `verify.cancel` | `{ flow_id, user_id, … }` | `{ ok }` |
| `verify.list` | `{ user?, server? }` | `{ flows: [VerifyState] }`：還在進行的 |

推播 `verify.state`（示意）：

```json
{ "user": "@alice:localhost", "flow_id": "<…>", "other_user": "@alice:localhost", "other_device": "<…>",
  "we_started": false, "state": "keys_exchanged",
  "emoji": [{ "symbol": "🐶", "description": "Dog" }, "…共 7 個"], "decimals": [1234, 5678, 9012] }
```

`state`：`requested` → `ready` → `started` → `keys_exchanged`（帶 `emoji`／`decimals`）→ `confirmed`（自己按了、等對方）→ `done`；任何一步都可能 `cancelled`（帶 `reason`）。

### 5.2 確認之後

- `confirm()` 回的簽章（自我驗證是 self-signing key 簽那台；別人是 user-signing key 簽那個人）**自己送**（`0x17 0x25`），回覆的 `failures` 不是空的就當失敗回給 UI（server 的 `0x25` 個別失敗仍是 200）。
- **自我驗證**完：上游自動排「跟對方要缺的秘密」，由後台送出去。交叉簽章私鑰收到就被上游匯入；**備份解密金鑰**落在秘密收件匣，daemon 拿出來對過 server 的備份版本（`backup_key_matches`）才存、`enable_backup_v1`，然後**自動下載還原**（要點頭，§10 第 3 點），跟 `backup.restore` 後半段同一條路。
- ⚠️ **驗證流程只在記憶體裡**（上游的 `VerificationCache` 不存檔）：daemon 重開，進行中的全部消失、**不**發 `cancelled`。UI 收到 daemon 重開（RPC 連線重連，/docs/design/rooms/room-sync.md §3.1）就把畫面上的驗證當作沒了。上游自己 10 分鐘逾時。

## 6 給 UI 的信任資訊

`devices.list { user_id, user?, server? }`（示意）：

```json
{ "user_id": "@bob:localhost", "identity_verified": true,
  "devices": [{ "device_id": "<…>", "display_name": "<…>", "verified": true, "is_this_device": false }] }
```

- `verified`：這台被它主人的交叉簽章簽過、而且（別人的話）那個人被我驗證過；或我本地手動驗證過。
- 只讀 crypto store，**不**上網（裝置清單由既有的 `KeysQuery` 追蹤更新）。

## 7 向自己其他裝置要金鑰

- 開 `matrix-sdk-crypto` 的 `automatic-room-key-forwarding`（要點頭，§10 第 4 點）。matrix-sdk 的預設 feature 就有它（`vendor/matrix-rust-sdk/crates/matrix-sdk/Cargo.toml` 的 `default`），我們的依賴沒開；開了才是「Matrix 原本的樣子」：
  - 解不開時自動向自己的裝置要（上游只在「自己至少有一台已驗證的裝置」時才要）；
  - 回應別人的請求：自己已驗證的裝置給全部，別人只給當初分給他們的那一段。
- 要來的金鑰走既有的「金鑰到了補解」（/docs/design/keys/e2ee-rpc.md §6）。⚠️ 實作時要確認轉發來的金鑰也出現在 `ImportReport.room_keys`。

## 8 照 matrix-sdk 預設、不改的

- **分享策略維持 `AllDevices`**（上游 `CollectStrategy` 的預設）。`crypto_engine.rs` 的 `room_key_share_settings` 那句「交叉簽章好了換 `IdentityBased`」拿掉，改寫成「照 matrix-sdk 預設（維護者 2026-09-17：Matrix 原本的金鑰分發不動）」。
- **解密的信任要求維持 `Untrusted`**（`decryption_settings`）。

## 9 這支不做

- 換掉交叉簽章（server 要密碼 UIAA）——之後要做就是一支 `keys.reset_identity { password }`。
- QR 驗證（要開上游的 `qrcode` feature）、在房間裡驗證「別人這個人」。
- recovery 的 passphrase 版、刪除 server 上的備份版本。

## 10 要維護者點頭的

1. **交叉簽章與備份版本：第一台裝置自動建**（§3、§4.1），跟一般帳號的 `auto_enable_*` 一致。
2. **備份上傳：wbf 帳號由 `Keys` 線後台自動做**（§4.1），不再是命令觸發；`backup.upload` 變成「等追平」。
3. **自我驗證完拿到備份金鑰就自動下載還原**（§5.2），不等 UI 叫 `backup.restore`。
4. **開 `automatic-room-key-forwarding`**（§7）。
5. **RPC 形狀**：`verify.*` 六支、`verify.state` 推播、`devices.list`（§5.1、§6）。

## 11 怎麼驗（計畫）

- sdk：端點表新列的前 4 bytes 逐 byte 釘；secret storage 那層對上游 `SecretStorageKey` 來回加解密。
- core 假 server：bootstrap 送三個請求的順序、server 已有 master 時不建；備份上傳迴圈；restore 解密匯入後補解。
- 真 server（本機 wbfuwunel）：alice 第一台登入 → 自動有交叉簽章與備份版本 → `backup.create_recovery_key` → 第二台登入、兩邊 `verify.*` 走完（測試比對兩邊表情一樣）→ 第二台 `devices.list` 看到自己已驗證、拿到備份金鑰；第三台登入 `backup.restore` → 讀得到登入前的加密訊息。

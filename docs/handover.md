# 交接：現在在哪、怎麼跑、下一步

> 給下一個接手的人（人或 agent）。2026-09-06 寫，每次交接更新。設計理由不在這裡，在 `docs/design/`；這裡只講**現況、怎麼跑、坑、下一步**。

## 1. 現況一句話

第 1 步（`wbf-wire` codec）、第 2 步（`wbf-sdk` 密碼層／通道／上傳下載、`apps/wbf-cli`）做完；
第 3 步（接 matrix-sdk 做房間）第一版做完：`rooms`、`send --text|--file`、`watch`、`read`、`files` 對本機 wbfuwunel 全走過。
本地資料庫：第一個 PR（vault 與金鑰，#11）合併；第二個 PR（`cache.db`：SQLCipher 快取、多帳號混存、CLI `recent`／`--from-cache`／寫穿／`accounts`／`forget-account`）2026-09-07 做完送審。
PR #1–#12 全部合併。**沒有 UI，媒體池（local-cache-db §8）還沒有。**

## 2. 讀哪些文件、什麼順序

| 順序 | 檔 | 講什麼 |
|---|---|---|
| 1 | [`README.md`](../README.md) | 佈局、狀態表、怎麼跑測試、貢獻規則 |
| 2 | [`design/plan-v1.md`](design/plan-v1.md) | 範圍、順序、進度；**§7.1**（本地不存）與 **§7.2**（耦合方向：上游 SDK 是可拆的零件）是所有程式的前提 |
| 3 | [`design/wbf-client-convention-for-chunk.md`](design/wbf-client-convention-for-chunk.md) | client 之間的約定：每塊怎麼加密、事件區塊、seek；**§5.2 送事件要宣告附件**（等 server 定案） |
| 4 | [`design/chat-model.md`](design/chat-model.md) | 聊天模型（Conversation／Message）、怎麼接 Matrix、Telegram 有 Matrix 沒有的逐列定案、`r_seq`／`g_seq`、§6 第 3 步範圍與差異 |
| 5 | [`design/wbf-cli-spec.md`](design/wbf-cli-spec.md) | CLI 每個命令、exit code、manifest、狀態檔、session 檔、驗收腳本 |
| 6 | [`design/local-cache-db.md`](design/local-cache-db.md) | 本地資料庫：主金鑰、三把子金鑰、SQLCipher、媒體檔案空間、配額。檔頭有進度表：§4／§5.3 做了，`cache.db`（§3、§6）與媒體（§8）還沒 |

server 端的權威在 wbfuwunel repo：`docs/design/chunked-upload-spec.md`（線上規格）、`room-seq-and-recent.md`、`media-attachments.md`（提案）、`wbf-vectors.json`（整份複製到本 repo，不手改）。

## 3. 程式碼在哪

```
crates/wbf-wire/         pack、EncryptedFileInfo、CRC-32C；純函數。tests/vectors.rs 對 docs/design/wbf-vectors.json
crates/wbf-sdk/src/
  cipher.rs chunk_block.rs chunk_crypto.rs   密碼層（約定 §2–§4、§7）；tests/client_vectors.rs 產生並比對 wbf-client-vectors.json
  protocol.rs channel.rs client.rs upload.rs download.rs manifest.rs login.rs error.rs   通道與上傳／下載（線上規格）
  chat.rs                聊天模型與 ChatBackend trait，沒有 Matrix 型別
  vault.rs               local.key、三把子金鑰、session.sealed（local-cache-db §4）；沒有 SQLite、沒有 matrix-sdk
  cache.rs               cache.db（feature `cache`，SQLCipher；local-cache-db §6）：users／rooms／events／events_synced_log／room_list／sync_state／read_positions／media／event_media
  event_json.rs          原始 Matrix 事件 JSON → Message；matrix backend 與 recent 共用，不掛 feature
  backend/matrix_sdk.rs  唯一 `use matrix_sdk` 的檔（feature `matrix`，預設關）；store 吃 vault 的第二把子金鑰
apps/wbf-cli/src/        main.rs（參數、exit code）、unlock.rs（passphrase 來源、unlock ticket）、accounts.rs（每個帳號的資料放哪、current、--account 解析）、commands.rs（第 2 步命令、Context）、rooms.rs（第 3 步命令、寫穿快取）、recent.rs（Event/Recent 進料）
scripts/acceptance.sh    CLI 規格 §8 的驗收，對本機 wbfuwunel 跑
vendor/matrix-rust-sdk   上游 submodule，path dependency；只在 backend/matrix_sdk.rs 出現
```

## 4. 怎麼跑

```bash
# Windows：先 export PATH="/c/Strawberry/perl/bin:$PATH"，不然 openssl-sys（SQLCipher 用）編不起來（local-cache-db §3）
cargo test --workspace                       # 不含 matrix feature，快；含 cache feature 的測試要 --features cache
cargo test -p wbf-sdk --features matrix      # 事件轉換、aggregate、錯密碼分類（起迷你 403 server）
cargo clippy -p wbf-sdk -p wbf-cli --features wbf-sdk/matrix --all-targets -- -D warnings
cargo fmt -p wbf-wire -p wbf-sdk -p wbf-cli  # 🚫 不要 --all：會格式化 submodule
```

對真 server（本機 wbfuwunel，Windows）：

1. 把 server 的 exe **複製**到別處再跑（維護者會重編 `target/`，直接跑會鎖檔）。最新 code 在 `target/e2e/`，`target/release/` 可能是舊的。
2. config 最小集：`server_name = "localhost"`、`port = 6167`、`allow_registration = true`、`registration_token = "<自訂>"`、`database_path`、`log = "warn"`。啟動要十幾秒，輪詢 `/_matrix/client/versions` 到 200。
3. 註冊測試帳號：`POST /_matrix/client/v3/register` 帶 `auth.type = m.login.registration_token`。
4. `WBF_E2E_SERVER=... WBF_E2E_USER=... WBF_E2E_PASSWORD_FILE=... cargo test -p wbf-sdk --test e2e_local_server -- --ignored`（第 2 步的驗收）；`WBF_PASSWORD_FILE=... scripts/acceptance.sh`（CLI 的驗收，200 MiB 約 80 秒；`WBF_ACCEPT_SIZE_MIB=16` 快跑）。
5. 第 3 步的手動流程：`login` → 用 token `createRoom`（`initial_state` 帶 `m.room.encryption`）→ `rooms` → `send --text` → `read` → `send --file` → `files --save` → `download --manifest`。token 現在在 `session.sealed` 裡讀不到，`createRoom` 那步的 token 用 curl 另外登入一次拿（驗收腳本就是這樣做）。
8. 快取的手動流程（一個帳號）：`recent`（第一次 `cg_seq_before` 是 null）→ `read <room> --from-cache` → `recent` 再跑一次（`pulled` 應該是 0）。
9. 多帳號混存（兩個帳號 alice、bob，同一個 `--data-dir`）：alice `login` → 建只有 alice 的房 A 與邀 bob 的房 C，各送幾則 → `recent` → bob `login`（alice 不 logout）→ `accounts` 兩個、current 是 bob → `rooms` 只有 C → `read A --from-cache` 0 則、`read C --from-cache` 0 則（bob 還沒親自拿過）→ `recent` → `read C --from-cache` 有了，而且 alice 解過的那幾則是明文 → `--account alice read A --from-cache` 仍有 → alice `logout`（`cache.db` 還在）→ bob `logout`（`cache.db` 被刪）。`forget-account @alice:localhost --yes` 後 alice 的 `--from-cache` 全空、bob 的不受影響。2026-09-07 跑過一次全對。
7. vault 的手動流程：`login --passphrase-file pw`（建 `passphrase` 模式的 `local.key`）→ `rooms`（走 ticket，不問）→ `lock` → `rooms`（問 passphrase；非互動就 exit 1）→ `remove-passphrase` → `rooms`（不問）。
6. 測完 `taskkill //F //IM <複本名>.exe`。

## 5. 坑（都踩過）

- `cargo fmt --all` 會格式化 `vendor/matrix-rust-sdk`（path dependency）。用 `-p`。commit 前看 `git -C vendor/matrix-rust-sdk status` 是空的。
- 帶 `--features matrix` 的第一次編譯很久（matrix-sdk 全家）；放背景。
- Windows 的 autocrlf 會把向量 JSON 換成 CRLF，`client_vectors.rs` 比對前有 normalize；`*.sh` 靠 `.gitattributes` 保持 LF。
- `main` 有分支保護，文件也走 PR。只推 `origin`，鏡像 `wbftw` 由維護者同步。
- matrix-sdk 的錯誤不能 parse Display 字串抓 errcode（永遠抓不到），用 `client_api_error_kind()`。有回歸測試。
- wbfuwunel 對 `Create` 的回應標頭 `id` 是新上傳 id，不是線上規格說的抄回 0；SDK 兩種都收，只對 `Create` 放寬。
- `watch` 對齊「現在」要一次 sync，debug build 啟動超過一秒；測時序要留餘裕。
- Windows 主執行緒棧只有 1 MB，debug build 的 `send` 會爆棧；`main.rs` 把 runtime 跑在 64 MiB 棧的執行緒上。新增大的 async 路徑如果又爆，先懷疑這個。
- `logout` 一定要連 `matrix/` 一起刪：Matrix logout 讓裝置失效，留著的 crypto store 會擋下一次 `login`（"account in the store doesn't match"）。`cache.db` 反過來要留（維護者定），只有這個 server 最後一個帳號登出才刪。
- 快取讀寫都要帶「我是誰」（mxid）：`Context::cache()` 回 `(Cache, me)`；漏帶就變成別人的視角。SDK 端沒有預設值可以偷懶。
- 編 SQLCipher（`cache` feature，wbf-cli 預設帶）在 Windows 要 Strawberry Perl 在 PATH 前面，不然 openssl-sys 的 build script 掛在 `Configure`。

## 6. 已知的洞（不是忘了，是等別人）

| 洞 | 卡在哪 | 影響 |
|---|---|---|
| **E2EE 房送檔案沒宣告附件**（約定 §5.2） | server 的 `Event/Send` 是提案；matrix-sdk 的 `Room::send` 不能加 header | server 端媒體計數 0，過保護期（≥ 7 天）被清。CLI 送檔會印警告 |
| `RoomCrypto` trait 還沒有 | 加密全在 matrix-sdk 裡，沒東西可包 | 接管送訊息那一版出現 |

## 7. 下一步（維護者 2026-09-06 同意的順序）

1. ~~本地資料庫第一個 PR：vault 與金鑰~~ 做了（2026-09-06 送審）。
2. ~~本地資料庫第二個 PR：`cache.db`~~ 做了（PR #13）。`recent` 2026-09-08 依 wbfuwunel #33 改成拉窗＋`Event/Batch` 串流（issue #15）：`WbfClient::recent_window`／`recent_sync`、`PackChannel::request_stream`、wire 多了 `Kind::Session` 與 `event::BATCH`、向量檔換新。rusqlite 0.40 與 matrix-sdk 合得來，代價是 Windows 要 Strawberry Perl（local-cache-db §3）。
2c. 媒體池（local-cache-db §8）：一個加密池、一把鑰、整檔、順序 append、進度每 1–2 秒 flush；`media`／`event_media` 表已經建好。
3. 附件宣告：等 server 定案。期間寫設計：用 `matrix-sdk-crypto` 的 `OlmMachine` 自己 Megolm 加密、走 `Event/Send` pack（這也是 `RoomCrypto` trait 出現的地方）。**走 fork submodule 露出 `Room::encrypt`，還是走 `OlmMachine`，維護者還沒定**；建議後者（plan-v1 §7.2 的方向）。
4. chat-model §6 剩的：`room`、建房、邀請、改權限、置頂、已讀送出、裝置驗證、標準附件下載。穿插。
5. UI 框架比較文件。

## 8. 規矩（維護者定，全域 CLAUDE.md 也有）

- 一律開分支送 PR，merge commit，不 rebase、不 squash、不 amend、不 force push。
- 每個 PR 描述要列「新增了對上游的哪些依賴」（plan-v1 §7.2）。
- 會 breaking Matrix 兼容的設計先寫給維護者，不自己選。
- 本地不落地任何聊天內容（plan-v1 §7.1），直到 local-cache-db 那一版。
- 審查者 cirno／rumia／salvia 每個 PR 都會來；逐條回應，能改就改，不改講理由。

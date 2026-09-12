# 交接：現在在哪、怎麼跑、下一步

> 給下一個接手的人（人或 agent）。2026-09-06 寫、2026-09-10 更新，每次交接更新。設計理由不在這裡，在 `docs/design/`；這裡只講**現況、怎麼跑、坑、下一步**。

## 1. 現況一句話

第 1 步（`wbf-wire` codec）、第 2 步（`wbf-sdk` 密碼層／通道／上傳下載、`apps/wbf-cli`）做完；
第 3 步（接 matrix-sdk 做房間）第一版做完：`rooms`、`send --text|--file`、`watch`、`read`、`files` 對本機 wbfuwunel 全走過。
本地資料庫三步都合併了：vault 與金鑰（#11）、`cache.db` 多帳號混存（#13）、媒體儲存池（#14）。`Event/Recent` 跟上 server 的拉窗＋`Batch` 串流（#16）。
PR #19 做完資料目錄的兩層路徑加密、`account` 一族與房間金鑰備份；PR #20 定了架構 v2 的形狀。
PR #1–#20 全部合併。**沒有 UI。**

## 2. 讀哪些文件、什麼順序

| 順序 | 檔 | 講什麼 |
|---|---|---|
| 1 | [`README.md`](../README.md) | 佈局、狀態表、怎麼跑測試、貢獻規則 |
| 1.5 | [`design/architecture-v2.md`](design/architecture-v2.md) | **daemon／RPC／四個前端的分層**（維護者 2026-09-09 定的方向）。要動介面之前先看這份 |
| 1.6 | [`design/to-device-push-proposal.md`](design/to-device-push-proposal.md) | 給 wbfuwunel 的 `0x16 Device` 提案（to-device 的訂閱／推送／補齊）。**還沒定案**，四個問題等 server 端拍板 |
| 2 | [`design/plan-v1.md`](design/plan-v1.md) | 範圍、順序、進度；**§7.1**（本地不存）與 **§7.2**（耦合方向：上游 SDK 是可拆的零件）是所有程式的前提 |
| 3 | [`design/wbf-client-convention-for-chunk.md`](design/wbf-client-convention-for-chunk.md) | client 之間的約定：每塊怎麼加密、事件區塊、seek；**§5.2 送事件要宣告附件**（等 server 定案） |
| 4 | [`design/chat-model.md`](design/chat-model.md) | 聊天模型（Conversation／Message）、怎麼接 Matrix、Telegram 有 Matrix 沒有的逐列定案、`r_seq`／`g_seq`、§6 第 3 步範圍與差異 |
| 5 | [`design/wbf-cli-spec.md`](design/wbf-cli-spec.md) | CLI 每個命令、exit code、manifest、狀態檔、session 檔、驗收腳本 |
| 6 | [`design/local-cache-db.md`](design/local-cache-db.md) | 本地資料庫：主金鑰與五把子金鑰（§4）、房間金鑰備份（§10）、佈局（§5.6）、`cache.db` 的 schema 與多帳號混存規則（§6）、媒體儲存池與它的檔案格式（§8、§8.8）。檔頭的進度表三段都 ✅ |

server 端的權威在 wbfuwunel repo：`docs/design/chunked-upload-spec.md`（線上規格）、`room-seq-and-recent.md`、`media-attachments.md`（提案）、`wbf-vectors.json`（整份複製到本 repo，不手改）。

## 3. 程式碼在哪

```
crates/wbf-wire/         pack、EncryptedFileInfo、CRC-32C；純函數。tests/vectors.rs 對 docs/design/wbf-vectors.json
crates/wbf-sdk/src/
  cipher.rs chunk_block.rs chunk_crypto.rs   密碼層（約定 §2–§4、§7）；tests/client_vectors.rs 產生並比對 wbf-client-vectors.json
  protocol.rs channel.rs client.rs upload.rs download.rs manifest.rs login.rs error.rs   通道與上傳／下載（線上規格）
  chat.rs                聊天模型與 ChatBackend trait，沒有 Matrix 型別
  vault.rs               local.key、六把子金鑰、session.sealed、封 recovery key（local-cache-db §4）；沒有 SQLite、沒有 matrix-sdk
  account_dir.rs         資料目錄名的確定性加密（`<b58 nonce>_<b58 密文>`，local-cache-db §11）；沒有 IO
  room_keys.rs           本地金鑰快照放哪、用什麼 passphrase、權限（local-cache-db §10.4）；不碰 matrix-sdk
  cache.rs               cache.db（feature `cache`，SQLCipher；local-cache-db §6）：users／rooms／events／events_synced_log／room_list／sync_state／read_positions／media／event_media
  media_pool.rs          媒體儲存池的落地格式（64 KiB 段各自 AEAD、暫定段、續傳、BLAKE3 檔名）；沒有 SQL、沒有網路
  media.rs               fetch／collect_garbage／sweep：下載管線、池、cache.db 三者唯一的交會點（feature `cache`）
  event_json.rs          原始 Matrix 事件 JSON → Message；matrix backend 與 recent 共用，不掛 feature
  backend/matrix_sdk.rs  唯一 `use matrix_sdk` 的檔（feature `matrix`，預設關）；store 吃 vault 的第二把子金鑰
crates/wbf-core/src/     lib.rs（`Core`：解鎖一次的 vault、多帳號入口）、accounts.rs（資料目錄佈局、`DataDirMap`）、recovery.rs（`r/` 的 recovery key）
                         ⚠️ 公開介面不能假設同程序（architecture-v2 §7）：`&self`、簡單型別、🚫 不問終端、🚫 不碰 ticket
apps/wbf-cli/src/        main.rs（參數、exit code）、unlock.rs（passphrase 來源、unlock ticket）、conf.rs（wbf.conf 的解析與自動生成）、commands.rs（第 2 步命令、Context）、rooms.rs（第 3 步命令、寫穿快取）、recent.rs（Event/Recent 進料）
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
10. 媒體池：`upload --sha256 --manifest m.json` → `download --manifest m.json -o a`（`source: server`，池裡出現 `media/<hh>/<hash>`）→ 再 `download` 一次（`source: cache`）→ `media-stats` → 大檔用 `--transport http` 下到一半殺掉 → `media-stats` 看到 `incomplete_files: 1` → 再 `download` 的第一行進度從上次快照的塊數開始、sha 對 → `media-gc --quota-mib 1 --protect-days 0` 清光。2026-09-08 跑過一次全對。
9b. 金鑰備份與閘門的手動流程（2026-09-09 跑過）：`login alice` → `key-backup status`（`server_backup_exists` 應為 true，證明 `auto_enable_backups` 生效）→ `logout`（**應該被閘門擋，exit 1**）→ `key-backup recovery`（產生並封進 `r/`）→ `recovery list` → `logout`（這次過）→ 確認 `r/` **還在** → 重 `login` → `key-backup restore` → `key-backup save`／`import` → `account destroy`（`recovery list` 應該空掉）。
   ⚠️ 閘門的 principal 迴歸：`account switch @alice`（有 recovery key）後 `account del @bob:localhost`（沒有）**必須被擋** —— 擋不住就是又用了 current 的狀態（PR #19 審查 rumia／salvia 🔴1）。
   ⚠️ data dir 用短路徑（例如 `C:/Users/<you>/AppData/Local/Temp/wt`）：加密過的目錄名很長，scratchpad 那種深路徑在 Windows 會撞 MAX_PATH（餘裕算法見 local-cache-db §11.4.1）。
9. 多帳號混存（兩個帳號 alice、bob，同一個 `--data-dir`）。（PR #19 起是 `account` 一族：`account status`／`switch`／`del`／`destroy`；下面的舊命令名要照著換）：alice `login` → 建只有 alice 的房 A 與邀 bob 的房 C，各送幾則 → `recent` → bob `login`（alice 不 logout）→ `accounts` 兩個、current 是 bob → `rooms` 只有 C → `read A --from-cache` 0 則、`read C --from-cache` 0 則（bob 還沒親自拿過）→ `recent` → `read C --from-cache` 有了，而且 alice 解過的那幾則是明文 → `--account alice read A --from-cache` 仍有 → alice `logout`（`cache.db` 還在）→ bob `logout`（`cache.db` 被刪）。`forget-account @alice:localhost --yes` 後 alice 的 `--from-cache` 全空、bob 的不受影響。2026-09-07 跑過一次全對。
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
- `logout` 一定要連 `m/`（matrix-sdk 的 crypto store）一起刪：Matrix logout 讓裝置失效，留著的 crypto store 會擋下一次 `login`（"account in the store doesn't match"）。`cache.db` 反過來要留（維護者定），只有這個 server 最後一個帳號登出才刪。
- 快取讀寫都要帶「我是誰」（mxid）：`Context::cache()` 回 `(Cache, me)`；漏帶就變成別人的視角。SDK 端沒有預設值可以偷懶。
- 編 SQLCipher（`cache` feature，wbf-cli 預設帶）在 Windows 要 Strawberry Perl 在 PATH 前面，不然 openssl-sys 的 build script 掛在 `Configure`。

## 6. 已知的洞（不是忘了，是等別人）

| 洞 | 卡在哪 | 影響 |
|---|---|---|
| **E2EE 房送檔案沒宣告附件**（約定 §5.2） | server 的 `Event/Send` 是提案；matrix-sdk 的 `Room::send` 不能加 header | server 端媒體計數 0，過保護期（≥ 7 天）被清。CLI 送檔會印警告 |
| `RoomCrypto` trait 還沒有 | 加密全在 matrix-sdk 裡，沒東西可包 | 接管送訊息那一版出現 |
| ~~房間金鑰沒有任何備份~~ | ✅ PR #19 做了：server 端標準 backup、本地全量快照、`logout` 的兩關閘門、`r/` 獨立保管 | — |
| `Session/*`（WS 上的 Login／Refresh／Logout）只加了 wire 常數 | client 登入仍走 HTTP `/login` 加 matrix-sdk | 沒影響；要把登入搬到 WS 時再做 |
| 斷線後 `recent` 不自動續 | 命令 exit、下次從水位重來；server 不記狀態、寫入冪等 | 多拉一輪；UI 那版做自動從最後的 `ls` 續 |

## 7. 下一步（維護者 2026-09-06 同意的順序，2026-09-10 更新）

做完的（都合併了）：資料目錄兩層路徑加密＋`account` 一族＋房間金鑰備份（#19，設計在 #18）、架構 v2 的形狀（#20）、vault 與金鑰（#11）、`cache.db` 多帳號混存（#13；rusqlite 0.40 與 matrix-sdk 合得來，代價是 Windows 要 Strawberry Perl，local-cache-db §3）、媒體儲存池（#14）、`recent` 改成拉窗＋`Event/Batch` 串流與三層分工 `RecentPlan { max_events, window, batch }`（#16，issue #15）。

還沒做的：

0. ✅ **房間金鑰備份與周邊**（維護者 2026-09-09 提；2026-09-10 全部做完）：設計定案在 local-cache-db §10 與 CLI 規格 §3.1／§3.6／§10。
   ✅ **PR #19 合併了大半**：兩層路徑加密、`account` 一族、CLI 輸出英文、金鑰備份（server 端 backup、本地全量快照、`key-backup` 六個子命令（status／upload／save／import／restore／recovery）、`logout` 的兩關閘門、`r/` 獨立保管與 `recovery list`／`show`）。
   ✅ **2026-09-10 收掉了剩下兩塊**：`wbf.conf`（CLI 規格 §10：解析、旗標 > 環境 > conf > 預設、自動生成、`SERVER_BACKUP`／`LOCAL_ROOM_KEYS` 兩個開關）、passphrase 吃原始 bytes（local-cache-db §12）。**這一項到此關掉。**
   ⚠️ PR #19 是 **breaking 的**：舊的 data dir（明文目錄名）一律**砍掉重來，🚫 不寫遷移**。本機測試環境要重新 `login`。
   ✅ **2026-09-09 對真 server 跑過全流**（步驟見 §4 的 9b），含 principal 修正的迴歸（current=alice 刪 bob 時看的是 bob 的狀態）。
   ✅ **Windows MAX_PATH 已解**：nonce 縮到 12 byte、目錄名縮成 `s`／`a`／`m`／`k`／`r`，最長路徑 230 → 184、餘裕 20 → 76；路徑太長時的錯誤訊息也不再誤導成「it was made with another key file」。前後對照與教訓在 local-cache-db §11.4.1。
1. chat-model §6 剩的房間功能：`room`、建房、邀請、改權限、置頂、已讀送出、裝置驗證、標準附件下載。穿插。
2. 附件宣告：等 server 定案（`media-attachments.md` 仍是提案）。期間寫設計：用 `matrix-sdk-crypto` 的 `OlmMachine` 自己 Megolm 加密、走 `Event/Send` pack（這也是 `RoomCrypto` trait 出現的地方）。✅ **2026-09-10 定了**：走 `OlmMachine::encrypt_room_event_raw`。⚠️ `Room` 上沒有 `encrypt`，所以那從來不是二選一；fork 已建，只差一行 `base_client()` 改 `pub`（architecture-v2 §8.3）。
3. ⚠️ **架構 v2 的落地**（`design/architecture-v2.md`，2026-09-09 定形狀、2026-09-10 定名字）：`wbf-daemon` crate、RPC 規格書（`rpc-spec.md` 還沒寫）、CLI 瘦身成前端。
   排在這裡是因為它會改變所有介面，愈晚做代價愈大；但它依賴兩個未定的決策（fork submodule 的範圍、server 的 `Event/ToDevice`）。
4. UI 框架比較文件。UI 的同步流程已經有 SDK 介面可接：開一個 task 跑 `recent_sync`，callback 把每個 Batch 丟 channel 給寫 DB 的 task（chat-model §4.3）；媒體用 `media::fetch` 加 `PoolReader`。
5. 串流／seek 對著媒體池讀（local-cache-db §8.6）：`seek` 命令現在仍直接打 server。
6. ⚠️ UI 落地前要確認「進房逐房翻頁」真的存在：`recent` 被 `max_events` 停下時，`[last_ls, 舊水位)` 那段是永久洞，只有逐房 `/messages` 會補（PR #16 審查記錄）。

## 8. 規矩（維護者定，全域 CLAUDE.md 也有）

- 一律開分支送 PR，merge commit，不 rebase、不 squash、不 amend、不 force push。
- 每個 PR 描述要列「新增了對上游的哪些依賴」（plan-v1 §7.2）。
- 會 breaking Matrix 兼容的設計先寫給維護者，不自己選。
- 本地不落地任何聊天內容（plan-v1 §7.1），直到 local-cache-db 那一版。
- 審查者 cirno／rumia／salvia 每個 PR 都會來；逐條回應，能改就改，不改講理由。

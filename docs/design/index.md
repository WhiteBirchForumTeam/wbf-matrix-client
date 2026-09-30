# 設計文件索引

一個資料夾回答一類問題。現況、怎麼跑、坑、下一步在 [`/docs/handover.md`](../handover.md)，不在這裡。

**引用的寫法**（維護者 2026-09-30 定）：程式碼註解與文件裡提到另一份文件，一律寫從 repo 根目錄算的完整路徑加章節，
例如 `/docs/design/overview/architecture-v2.md §8`；只有指同一份文件自己的章節才只寫 `§8`。搬家或改名時 grep 完整路徑就找得到每一處。

## `overview/`：整體的分層

| 文件 | 講什麼 |
|---|---|
| [`/docs/design/overview/architecture-v2.md`](overview/architecture-v2.md) | daemon／RPC／前端的分層、daemon 的職責邊界、現有 crate 怎麼分工、§8 耦合方向（上游 SDK 是可拆的零件） |

## `rpc-specs/`：前端看得到的介面

| 文件 | 講什麼 |
|---|---|
| [`/docs/design/rpc-specs/rpc-spec.md`](rpc-specs/rpc-spec.md) | 前端 ↔ daemon 的每一條 method、`params`／`result`、錯誤碼、推播 |
| [`/docs/design/rpc-specs/local-interface.md`](rpc-specs/local-interface.md) | 本地介面：控制平面與資料平面兩個 port、token、每則訊息的加密、訊息形狀、閘門鏈 |
| [`/docs/design/rpc-specs/data-plane.md`](rpc-specs/data-plane.md) | 資料平面怎麼用：capability URL、`PUT /upload`、上傳的兩步（HTTP 傳完 → RPC 帶 mxc 發訊息）、附件宣告、一般 Matrix 的傳統上傳、`GET /media` |
| [`/docs/design/rpc-specs/wbf-cli-spec.md`](rpc-specs/wbf-cli-spec.md) | 命令列的命令、參數、輸出、exit code、狀態檔、conf 檔、驗收腳本 |

## `daemon/`：daemon 跑起來之後

| 文件 | 講什麼 |
|---|---|
| [`/docs/design/daemon/daemon-runtime.md`](daemon/daemon-runtime.md) | 多帳號怎麼落到 `cache.db`、單一寫入者、本地讀與上游拉（`sync` 參數）、事件扇出、`job` 與 `cancel`、分階段 |
| [`/docs/design/daemon/account-session.md`](daemon/account-session.md) | 帳號的會話：探活、登入、登出、誰用 matrix-sdk 的 Client |
| [`/docs/design/daemon/link-pool.md`](daemon/link-pool.md) | 連線池：一個帳號五條線、解鎖／登入後全開、背景看線重開 |
| [`/docs/design/daemon/ws-receive-dispatch.md`](daemon/ws-receive-dispatch.md) | WS 收包分派：一條連線、任何順序、依會話表交付 |

## `rooms/`：房間

| 文件 | 講什麼 |
|---|---|
| [`/docs/design/rooms/chat-model.md`](rooms/chat-model.md) | 聊天模型（Conversation／Peer／Message／Role）、怎麼對到 Matrix、Telegram 的形狀 |
| [`/docs/design/rooms/room-sync.md`](rooms/room-sync.md) | 房間的訂閱線：訂閱事件、推播寫進快取、水位只由 UI 的 `Recent` 推 |

## `messages/`：訊息

| 文件 | 講什麼 |
|---|---|
| [`/docs/design/messages/edits-and-redactions.md`](messages/edits-and-redactions.md) | 原始事件永遠不動、最終內容另存：edit 與 redact 怎麼存、怎麼顯示 |
| [`/docs/design/messages/read-receipts.md`](messages/read-receipts.md) | 已讀的三層：預設沒讀、UI 怎麼標、private／public 由 conf 決定 |

## `keys/`：E2EE 與金鑰

| 文件 | 講什麼 |
|---|---|
| [`/docs/design/keys/e2ee-rpc.md`](keys/e2ee-rpc.md) | E2EE 的 RPC 面（最權威）：狀態放 UI、金鑰由 daemon 自動、1506 之後的處理、收到即解與補解 |
| [`/docs/design/keys/key-sync.md`](keys/key-sync.md) | 金鑰的訂閱線：`Device/Subscribe`、上線追平、推來就匯 |
| [`/docs/design/keys/to-device-client.md`](keys/to-device-client.md) | client 端怎麼接 `0x16 Device`（to-device：金鑰、驗證、SSSS） |
| [`/docs/design/keys/room-key-backup.md`](keys/room-key-backup.md) | 房間金鑰的備份：server 一份、本地一份，recovery key 放哪、`logout` 的閘門 |
| [`/docs/design/keys/e2ee-walkthrough.md`](keys/e2ee-walkthrough.md) | E2EE 從建房到退出每一步發生什麼（理解用，不是權威） |

## `storage/`：本地資料

| 文件 | 講什麼 |
|---|---|
| [`/docs/design/storage/local-cache-db.md`](storage/local-cache-db.md) | `cache.db`：定位是快取、存什麼、SQLCipher、與 matrix-sdk store 的關係、schema |
| [`/docs/design/storage/vault-and-keys.md`](storage/vault-and-keys.md) | 本地的金鑰：主金鑰與子金鑰、路徑兩層加密、passphrase |

## `media/`：檔案怎麼加密、怎麼存

| 文件 | 講什麼 |
|---|---|
| [`/docs/design/media/wbf-client-convention-for-chunk.md`](media/wbf-client-convention-for-chunk.md) | client 之間的約定：每塊怎麼加密、事件區塊、串流、seek |
| [`/docs/design/media/media-pool.md`](media/media-pool.md) | 本地媒體池：整檔放進一個加密的池、配額與清理、池的檔案格式 |
| [`/docs/design/media/wbf-client-vectors.json`](media/wbf-client-vectors.json) | 分塊約定的向量（sdk `tests/client_vectors.rs` 讀它） |

## `wire/`：線上格式的向量

| 文件 | 講什麼 |
|---|---|
| [`/docs/design/wire/wbf-vectors.json`](wire/wbf-vectors.json) | 從 wbfuwunel 整份複製的黃金向量，🚫 不手改（wire `tests/vectors.rs`、sdk `tests/unit.rs` 讀它） |

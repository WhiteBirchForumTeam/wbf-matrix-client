# 設計文件索引

一個資料夾回答一類問題。檔名在整個 `docs/design/` 裡不重複，所以程式碼註解與文件裡寫的「`e2ee-rpc.md` §3」這種只帶檔名的引用，照檔名找得到。
現況、怎麼跑、坑、下一步在 [`../handover.md`](../handover.md)，不在這裡。

## `overview/`：整體的分層與範圍

| 文件 | 講什麼 |
|---|---|
| [`architecture-v2.md`](overview/architecture-v2.md) | daemon／RPC／前端的分層、兩個平面、token 與加密；要動介面之前先看這份 |
| [`plan-v1.md`](overview/plan-v1.md) | v1 的範圍、佈局、依賴、順序；§7.1 本地不存、§7.2 耦合方向 |

## `rpc-specs/`：前端看得到的介面

| 文件 | 講什麼 |
|---|---|
| [`rpc-spec.md`](rpc-specs/rpc-spec.md) | 前端 ↔ daemon 的每一條 method、`params`／`result`、錯誤碼、推播、媒體的 HTTP 介面 |
| [`wbf-cli-spec.md`](rpc-specs/wbf-cli-spec.md) | 命令列的命令、參數、輸出、exit code、狀態檔 |

## `daemon/`：daemon 跑起來之後

| 文件 | 講什麼 |
|---|---|
| [`daemon-runtime.md`](daemon/daemon-runtime.md) | 多帳號怎麼落到 `cache.db`、單一寫入者、本地讀與上游拉（`sync` 參數）、事件扇出、分階段 |
| [`account-session.md`](daemon/account-session.md) | 帳號的會話：探活、登入、登出、誰用 matrix-sdk 的 Client |
| [`link-pool.md`](daemon/link-pool.md) | 連線池：一個帳號五條線、解鎖／登入後全開、背景看線重開 |
| [`ws-receive-dispatch.md`](daemon/ws-receive-dispatch.md) | WS 收包分派：一條連線、任何順序、依會話表交付 |

## `rooms/`：房間與訊息

| 文件 | 講什麼 |
|---|---|
| [`chat-model.md`](rooms/chat-model.md) | 聊天模型（Conversation／Peer／Message／Role）、怎麼對到 Matrix、Telegram 的形狀 |
| [`room-sync.md`](rooms/room-sync.md) | 房間的訂閱線：訂閱事件、推播寫進快取、水位只由 UI 的 `Recent` 推 |

## `keys/`：E2EE 與金鑰

| 文件 | 講什麼 |
|---|---|
| [`e2ee-rpc.md`](keys/e2ee-rpc.md) | E2EE 的 RPC 面：狀態放 UI、金鑰由 daemon 自動、1506 之後的處理、收到即解與補解 |
| [`key-sync.md`](keys/key-sync.md) | 金鑰的訂閱線：`Device/Subscribe`、上線追平、推來就匯 |
| [`to-device-client.md`](keys/to-device-client.md) | client 端怎麼接 `0x16 Device`（to-device：金鑰、驗證、SSSS） |
| [`e2ee-walkthrough.md`](keys/e2ee-walkthrough.md) | E2EE 從建房到退出每一步發生什麼、`OlmMachine` 之外要自己寫的、#45 的落點（理解用，不是權威） |

## `storage/`：本地資料

| 文件 | 講什麼 |
|---|---|
| [`local-cache-db.md`](storage/local-cache-db.md) | 加密的 vault 與金鑰、`cache.db` 的 schema 與多帳號混存、媒體池 |

## `media/`：檔案怎麼加密、怎麼讀

| 文件 | 講什麼 |
|---|---|
| [`wbf-client-convention-for-chunk.md`](media/wbf-client-convention-for-chunk.md) | client 之間的約定：每塊怎麼加密、事件區塊、串流、seek |
| [`wbf-client-vectors.json`](media/wbf-client-vectors.json) | 上面那份約定的向量（sdk `tests/client_vectors.rs` 讀它） |

## `wire/`：線上格式的向量

| 文件 | 講什麼 |
|---|---|
| [`wbf-vectors.json`](wire/wbf-vectors.json) | 從 wbfuwunel 整份複製的黃金向量，🚫 不手改（wire `tests/vectors.rs`、sdk `tests/unit.rs` 讀它） |

# wbf-matrix-client

wbfuwunel 的 client 端：一個可以拿來寫 desktop、Android、CLI 的 Rust SDK，加上第一個用它的程式（CLI）。
UI 框架的選擇延後到 SDK 能用之後。規劃與進度看 [`docs/design/plan-v1.md`](docs/design/plan-v1.md)。

## 現在有什麼

| | 狀態 |
|---|---|
| `crates/wbf-wire` | 做完。線上協議的 codec（pack、`EncryptedFileInfo`、CRC-32C），純函數；`cargo test -p wbf-wire` 對著 server 產生的黃金向量跑 |
| `crates/wbf-sdk` | 做中。密碼層、WebSocket／HTTP 通道、登入、分塊上傳／下載／seek／續傳／串流都有了；`cargo test -p wbf-sdk` 對著 [`wbf-client-vectors.json`](docs/design/wbf-client-vectors.json)、RFC／NIST 向量、記憶體版 server 跑，`tests/e2e_local_server.rs` 對著真的 wbfuwunel 跑（`--ignored`，見檔頭）。房間與 matrix-sdk 是第 3 步 |
| `apps/wbf-cli` | 還沒。介面已定在 [`docs/design/wbf-cli-spec.md`](docs/design/wbf-cli-spec.md) |
| `apps/desktop` | 之後 |

## 設計文件

| 檔 | 內容 |
|---|---|
| [`plan-v1.md`](docs/design/plan-v1.md) | v1 範圍、佈局、依賴、驗收、順序、進度 |
| [`wbf-client-convention-for-chunk.md`](docs/design/wbf-client-convention-for-chunk.md) | client 之間的約定：每塊怎麼加密、描述長什麼樣、串流怎麼收尾、房間事件怎麼放、seek 怎麼算。server 不讀這些 |
| [`wbf-cli-spec.md`](docs/design/wbf-cli-spec.md) | CLI 的命令、參數、輸出、exit code、manifest、狀態檔、驗收腳本 |
| [`chat-model.md`](docs/design/chat-model.md) | 聊天模型與房間設計：Conversation／Peer／Message／Role 的定義、怎麼接到 Matrix、Telegram 有而 Matrix 沒有的一律標「審」 |
| [`local-cache-db.md`](docs/design/local-cache-db.md) | 之後的本地資料庫：加密的暫存快取、主金鑰與 local password、與 matrix-sdk store 的分工（現在還不做，見 plan-v1 §7.1） |
| [`wbf-vectors.json`](docs/design/wbf-vectors.json) | 線上協議的黃金向量，從 wbfuwunel **整份複製**、不手改；server 規格改了就重新複製，測試紅了就是漂移 |

## 關聯專案

| | 在哪 | 角色 |
|---|---|---|
| **wbfuwunel** | Forgejo [`amaid/wbfuwunel`](http://ai.zooy.cc:30008/amaid/wbfuwunel)、公開鏡像 [`WhiteBirchForumTeam/wbfuwunel`](https://github.com/WhiteBirchForumTeam/wbfuwunel) | **server 端的權威。** 本 repo 對的是它，不是 Matrix 規格。它是 [`matrix-construct/tuwunel`](https://github.com/matrix-construct/tuwunel) 的 fork，分岔會持續變大，與 Matrix 規格的相容性不是它的目標；所以本 client 以 fork 為準，fork 與上游 Matrix 不一致時，照 fork。線上規格在它的 `docs/design/chunked-upload-spec.md`，黃金向量在 `docs/design/wbf-vectors.json`，本 repo 只複製規格與向量，不共用程式碼 |
| **matrix-rust-sdk** | `vendor/matrix-rust-sdk` submodule，指上游 [`matrix-org/matrix-rust-sdk`](https://github.com/matrix-org/matrix-rust-sdk) | Matrix 基礎（登入、sync、房間、E2EE）。先用上游；需要改就把 submodule 改指自己的 fork |

本 repo 自己也有兩個位置：開發在 Forgejo [`amaid/wbf-matrix-client`](http://ai.zooy.cc:30008/amaid/wbf-matrix-client)（PR 在這裡開），公開鏡像在
GitHub [`WhiteBirchForumTeam/wbf-matrix-client`](https://github.com/WhiteBirchForumTeam/wbf-matrix-client)。

## 取得與建置

```bash
git clone --recurse-submodules http://ai.zooy.cc:30008/amaid/wbf-matrix-client.git
```

已 clone 但少了 submodule：

```bash
git submodule update --init
```

工具鏈釘在 `rust-toolchain.toml`（1.95.0，與 submodule 的 `rust-version` 一致），rustup 會自動用它。

```bash
cargo test --workspace
```

## 貢獻規則

- 程式碼一律開分支送 PR，不直接合到 `main`；合併用 merge commit，不 squash、不 rebase。
- 歷史只往前長：要修就再 commit 一次，不 amend、不 force push。
- 命名空間 `org.wbftw.wbfuwunel.<名字>`（Matrix 自訂型別與 key）。

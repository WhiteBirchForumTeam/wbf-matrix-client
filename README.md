# wbf-matrix-client

**English** · [繁體中文](#繁體中文)

A Rust client for [**wbfuwunel**](https://github.com/WhiteBirchForumTeam/wbfuwunel) — a Matrix homeserver fork that adds its own binary wire protocol for large, end-to-end-encrypted media and low-latency event delivery.

The client is built as a **long-running daemon** that owns everything hard — protocols, encryption, local storage, sessions — and exposes it to thin frontends (a CLI today; desktop, Android and Python later) over an encrypted local RPC.

> [!WARNING]
> **Early development.** There is no graphical client yet, interfaces still change, and nothing here has been audited. Do not rely on it to protect real conversations.

## Why

- **Big encrypted media that actually works.** Files are split into chunks and each chunk is encrypted on its own (AEAD), so uploads and downloads can resume, seek, and stream instead of starting over.
- **One daemon, many frontends.** The daemon holds the databases and connections; frontends only speak a small, documented RPC. A UI never has to reimplement Matrix, crypto or sync.
- **Local first.** What the UI shows is read from an encrypted local cache. Talking to the server is something a caller asks for explicitly, not something every scroll does.
- **Works with ordinary Matrix servers too.** The daemon probes each homeserver: if it speaks the wbf protocol, it is used; if not, the client falls back to standard Matrix (via [matrix-rust-sdk](https://github.com/matrix-org/matrix-rust-sdk)). Features that only exist in the wbf protocol — chunked encrypted media, for example — are unavailable there.

## Architecture

```text
homeserver ──wbf protocol (WS) / Matrix HTTP──> daemon ──encrypted JSON-RPC over loopback WS──> frontends
                                                   │   ──HTTP with Range (media)────────────────>
                                                   ├─ encrypted vault (local.key, sealed sessions)
                                                   ├─ cache.db (SQLCipher, one per server)
                                                   └─ media pool (encrypted, resumable)
```

- **Control plane** — JSON messages over a WebSocket bound to `127.0.0.1`, encrypted with XChaCha20-Poly1305 using keys derived from a per-launch token.
- **Data plane** *(planned)* — plain local HTTP with `Range` for media, so players and viewers can stream directly. Not implemented yet.
- **Backend choice** — `transport: ws` (the default) means the wbf protocol; `transport: http` means standard Matrix. Asking for `ws` against an ordinary homeserver is a no-op, not an error.

## Workspace

| Path | What it is | Status |
|---|---|---|
| [`crates/wbf-wire`](crates/wbf-wire) | Wire codec: packs, `EncryptedFileInfo`, CRC-32C. Pure functions, no async | ✅ Done — tested against the server's golden vectors |
| [`crates/wbf-sdk`](crates/wbf-sdk) | Protocol client: per-chunk AEAD, WebSocket channel, chunked upload / download / seek / resume / streaming, encrypted vault, `cache.db`, media pool, matrix-sdk adapter | 🟢 Working |
| [`crates/wbf-core`](crates/wbf-core) | Everything a command *does*: accounts, rooms, messages, media, backend probing, the single writer for `cache.db`. No CLI, no RPC | 🟢 Working |
| [`crates/wbf-daemon`](crates/wbf-daemon) | The daemon binary `wbf-matrix-client-daemon`: RPC server, data-directory locking, token lifecycle | 🟡 RPC methods wired; push, subscriptions, cancel and the media HTTP plane not yet |
| [`apps/wbf-cli`](apps/wbf-cli) | Command-line frontend `wbf-cli`: login, rooms, send, watch, upload, download, seek, accounts, media cache | 🟢 Working (calls `wbf-core` directly for now) |
| Desktop / Android / Python | Frontends over RPC | ⏳ Not started |

## Getting started

### Requirements

- **Rust 1.95.0** — pinned in [`rust-toolchain.toml`](rust-toolchain.toml); `rustup` picks it up automatically.
- **Git with submodules** — `vendor/matrix-rust-sdk` is a submodule.
- **Windows only:** [Strawberry Perl](https://strawberryperl.com/) on `PATH` *before* running `cargo` — it is needed to build OpenSSL for SQLCipher ([details](docs/design/local-cache-db.md)).

### Build and test

```bash
git clone --recurse-submodules https://github.com/WhiteBirchForumTeam/wbf-matrix-client.git
cd wbf-matrix-client
cargo test --workspace
```

Already cloned without submodules:

```bash
git submodule update --init
```

A few notes:

- The `matrix` feature of `wbf-sdk` (the matrix-sdk adapter) is off by default; `apps/wbf-cli` turns it on. To test it on its own: `cargo test -p wbf-sdk --features matrix`.
- Tests that need a real wbfuwunel server are `#[ignore]`d; the file header of each explains how to run it.
- ⚠️ `cargo fmt --all` also reformats the vendored matrix-rust-sdk. Format only this workspace:
  `cargo fmt -p wbf-wire -p wbf-sdk -p wbf-core -p wbf-daemon -p wbf-cli`

## Documentation

Design documents live in [`docs/design`](docs/design). They are written in Traditional Chinese.

| Document | Covers |
|---|---|
| [`architecture-v2.md`](docs/design/architecture-v2.md) | Layers: daemon, RPC, frontends; the two planes; tokens and encryption |
| [`rpc-spec.md`](docs/design/rpc-spec.md) | Every RPC method, error code, push message, and the media HTTP API |
| [`daemon-runtime.md`](docs/design/daemon-runtime.md) | How the running daemon handles many accounts, the single `cache.db` writer, local vs. upstream reads |
| [`local-cache-db.md`](docs/design/local-cache-db.md) | The encrypted vault, `cache.db` schema, and media pool |
| [`chat-model.md`](docs/design/chat-model.md) | Conversations, peers, messages, roles — and how they map onto Matrix |
| [`wbf-cli-spec.md`](docs/design/wbf-cli-spec.md) | CLI commands, flags, output, exit codes |
| [`wbf-client-convention-for-chunk.md`](docs/design/wbf-client-convention-for-chunk.md) | Client-to-client conventions for chunk encryption, streaming, and seeking |
| [`to-device-client.md`](docs/design/to-device-client.md) | Wiring up to-device messages (`0x16 Device`) |
| [`plan-v1.md`](docs/design/plan-v1.md) | v1 scope, layout, dependencies, milestones |
| [`handover.md`](docs/handover.md) | Where things stand right now, known gaps, next steps |

## Related projects

| Project | Role |
|---|---|
| [**wbfuwunel**](https://github.com/WhiteBirchForumTeam/wbfuwunel) | **The server this client targets, and the authority on the protocol.** A fork of [tuwunel](https://github.com/matrix-construct/tuwunel) that diverges on purpose; when it and the Matrix spec disagree, this client follows wbfuwunel. The wire spec and golden vectors are copied from it — no code is shared. |
| [**matrix-rust-sdk**](https://github.com/WhiteBirchForumTeam/matrix-rust-sdk) | Matrix foundations (login, sync, rooms, E2EE). Vendored as a submodule pointing at our fork of [matrix-org/matrix-rust-sdk](https://github.com/matrix-org/matrix-rust-sdk). |

## Contributing

Development and pull requests happen on [Forgejo (`amaid/wbf-matrix-client`)](http://ai.zooy.cc:30008/amaid/wbf-matrix-client); this GitHub repository is a mirror.

- Every code change goes through a branch and a pull request — nothing is pushed straight to `main`.
- History only moves forward: fix mistakes with a new commit. No rebase, no amend, no force push. Pull requests are merged with a merge commit.
- Custom Matrix event types and keys use the `org.wbftw.wbfuwunel.<name>` namespace.

## License

Licensed under the [Apache License 2.0](LICENSE). Copyright 2026 WBFT.

---

# 繁體中文

[English](#wbf-matrix-client) · **繁體中文**

[**wbfuwunel**](https://github.com/WhiteBirchForumTeam/wbfuwunel) 的 Rust client。wbfuwunel 是一個 Matrix homeserver 的 fork，它在標準 Matrix 之外加了**自己的二進位協議**，用來傳大型的端對端加密媒體，並低延遲地送出事件。

這個 client 的形狀是一個**常駐的 daemon**：所有難的東西 —— 協議、加解密、本地資料、session —— 都在它裡面，再透過加密的本機 RPC 提供給**薄的前端**（現在是命令列；之後是桌面、Android、Python）。

> [!WARNING]
> **仍在早期開發。** 還沒有圖形介面、介面還會變、也沒有經過任何安全稽核。請不要拿它保護真實的對話。

## 為什麼

- **大型加密媒體要真的能用。** 檔案切成區塊、每塊各自加密（AEAD），所以上傳與下載可以**續傳、跳著讀、邊收邊播**，而不是斷了就重來。
- **一個 daemon，多個前端。** 資料庫與連線都在 daemon 裡；前端只講一套小而有文件的 RPC。做 UI 的人🚫 不必重寫 Matrix、加密或同步。
- **本地優先。** UI 顯示的東西讀自加密的本地快取。要去問 server，是呼叫端**明講**才做的事，🚫 不是每捲一次就打一次。
- **一般的 Matrix server 也能用。** daemon 會探測每一台 homeserver：講 wbf 協議就用它，不講就退回標準 Matrix（透過 [matrix-rust-sdk](https://github.com/matrix-org/matrix-rust-sdk)）。⚠️ 只有 wbf 協議才有的功能（例如分塊加密媒體）在那種 server 上是關的。

## 架構

```text
homeserver ──wbf 協議（WS）／Matrix HTTP──> daemon ──加密的 JSON-RPC over 本機 WS──> 前端
                                              │   ──HTTP with Range（媒體）──────────>
                                              ├─ 加密的 vault（local.key、封存的 session）
                                              ├─ cache.db（SQLCipher，一台 server 一份）
                                              └─ 媒體池（加密、可續傳）
```

- **控制平面** —— JSON 訊息走綁在 `127.0.0.1` 的 WebSocket，用 XChaCha20-Poly1305 加密，金鑰由每次啟動的 token 導出。
- **資料平面**（*規劃中*）—— 本機 HTTP，支援 `Range`，播放器與檢視器可以直接串流媒體。⚠️ 還沒實作。
- **backend 怎麼選** —— `transport: ws`（預設）＝ wbf 協議；`transport: http` ＝ 標準 Matrix。對一般 homeserver 指定 `ws` 是 no-op，🚫 不是錯誤。

## 專案結構

| 路徑 | 是什麼 | 狀態 |
|---|---|---|
| [`crates/wbf-wire`](crates/wbf-wire) | 線上協議的 codec：pack、`EncryptedFileInfo`、CRC-32C。純函數、無 async | ✅ 完成 —— 對著 server 的黃金向量測 |
| [`crates/wbf-sdk`](crates/wbf-sdk) | 協議 client：每塊 AEAD、WebSocket 通道、分塊上傳／下載／seek／續傳／串流、加密 vault、`cache.db`、媒體池、matrix-sdk adapter | 🟢 可用 |
| [`crates/wbf-core`](crates/wbf-core) | 每個命令「做什麼」：帳號、房間、訊息、媒體、backend 探測、`cache.db` 的單一寫入者。沒有命令列、沒有 RPC | 🟢 可用 |
| [`crates/wbf-daemon`](crates/wbf-daemon) | daemon 本體 `wbf-matrix-client-daemon`：RPC 服務、資料目錄獨佔、token 生命週期 | 🟡 RPC method 已接上；推播、訂閱、cancel、媒體 HTTP 平面還沒有 |
| [`apps/wbf-cli`](apps/wbf-cli) | 命令列前端 `wbf-cli`：登入、房間、送訊息、watch、上傳、下載、seek、多帳號、媒體快取 | 🟢 可用（目前直接叫 `wbf-core`） |
| 桌面／Android／Python | 走 RPC 的前端 | ⏳ 還沒開始 |

## 開始

### 需要

- **Rust 1.95.0** —— 釘在 [`rust-toolchain.toml`](rust-toolchain.toml)，`rustup` 會自動用它。
- **帶 submodule 的 Git** —— `vendor/matrix-rust-sdk` 是 submodule。
- **只有 Windows：** 跑 `cargo` 之前，[Strawberry Perl](https://strawberryperl.com/) 要排在 `PATH` 前面 —— SQLCipher 要靠它編 OpenSSL（[細節](docs/design/local-cache-db.md)）。

### 建置與測試

```bash
git clone --recurse-submodules https://github.com/WhiteBirchForumTeam/wbf-matrix-client.git
cd wbf-matrix-client
cargo test --workspace
```

已經 clone 但少了 submodule：

```bash
git submodule update --init
```

幾件事：

- `wbf-sdk` 的 `matrix` feature（matrix-sdk adapter）預設關閉，`apps/wbf-cli` 才打開。單獨測它：`cargo test -p wbf-sdk --features matrix`。
- 需要真的 wbfuwunel server 的測試標了 `#[ignore]`，怎麼跑寫在各自的檔頭。
- ⚠️ `cargo fmt --all` 會連 vendor 進來的 matrix-rust-sdk 一起格式化。只格式化這個 workspace：
  `cargo fmt -p wbf-wire -p wbf-sdk -p wbf-core -p wbf-daemon -p wbf-cli`

## 文件

設計文件在 [`docs/design`](docs/design)，以繁體中文撰寫。

| 文件 | 內容 |
|---|---|
| [`architecture-v2.md`](docs/design/architecture-v2.md) | 分層：daemon、RPC、前端；兩個平面；token 與加密 |
| [`rpc-spec.md`](docs/design/rpc-spec.md) | 每一條 RPC method、錯誤碼、推播訊息，以及媒體的 HTTP API |
| [`daemon-runtime.md`](docs/design/daemon-runtime.md) | daemon 跑起來之後：多帳號、`cache.db` 的單一寫入者、本地讀與上游拉 |
| [`local-cache-db.md`](docs/design/local-cache-db.md) | 加密的 vault、`cache.db` 的 schema、媒體池 |
| [`chat-model.md`](docs/design/chat-model.md) | 對話、對象、訊息、角色 —— 以及它們怎麼對到 Matrix |
| [`wbf-cli-spec.md`](docs/design/wbf-cli-spec.md) | 命令列的命令、參數、輸出、exit code |
| [`wbf-client-convention-for-chunk.md`](docs/design/wbf-client-convention-for-chunk.md) | client 之間的約定：區塊加密、串流、seek |
| [`to-device-client.md`](docs/design/to-device-client.md) | to-device 訊息（`0x16 Device`）怎麼接 |
| [`plan-v1.md`](docs/design/plan-v1.md) | v1 範圍、佈局、依賴、里程碑 |
| [`handover.md`](docs/handover.md) | 現在在哪、已知的洞、下一步 |

## 關聯專案

| 專案 | 角色 |
|---|---|
| [**wbfuwunel**](https://github.com/WhiteBirchForumTeam/wbfuwunel) | **這個 client 對接的 server，也是協議的權威。** 它是 [tuwunel](https://github.com/matrix-construct/tuwunel) 的 fork，而且刻意分岔；它跟 Matrix 規格不一致時，本 client 照 wbfuwunel。線上規格與黃金向量從它複製過來 —— 🚫 不共用程式碼。 |
| [**matrix-rust-sdk**](https://github.com/WhiteBirchForumTeam/matrix-rust-sdk) | Matrix 的基礎（登入、同步、房間、E2EE）。以 submodule 引入，指向我們 fork 的 [matrix-org/matrix-rust-sdk](https://github.com/matrix-org/matrix-rust-sdk)。 |

## 參與開發

開發與 pull request 在 [Forgejo（`amaid/wbf-matrix-client`）](http://ai.zooy.cc:30008/amaid/wbf-matrix-client)；GitHub 上的這個 repo 是鏡像。

- 所有程式碼改動都走分支加 pull request —— 🚫 不直接推到 `main`。
- 歷史只往前長：要修就再 commit 一次。🚫 不 rebase、不 amend、不 force push。PR 用 merge commit 合併。
- 自訂的 Matrix 事件型別與 key 用 `org.wbftw.wbfuwunel.<名字>` 命名空間。

## 授權

以 [Apache License 2.0](LICENSE) 授權。Copyright 2026 WBFT。

//! wbf-daemon：client 本體的 RPC 那一面（architecture-v2 §2、rpc-spec）。
//!
//! ```text
//! 命令列 arg ──解析──> RPC 訊息 ──┐
//!                                 ├──> handle ──> wbf-core ──> wbf-sdk ──> homeserver
//! 本地 WS 收到的包 ──connection──┘
//! ```
//!
//! | 模組 | 回答什麼 |
//! |---|---|
//! | `pack` | bytes ↔ (明文／密文, JSON)。RPC 自己的極簡 pack |
//! | `message` | 訊息形狀與 code 表 |
//! | `protocol` | `hello` 的兩關 |
//! | `connection` | 一條連線的狀態機；**出去的包該不該加密只在這裡判斷** |
//! | `handle` | method → core |
//! | `server` | loopback 的 WS listener |
//! | `settings` | 從 `wbf.conf` 讀進來、要填進 `Target` 的那幾個值 |
//! | `token` | `daemon.token` 的生命週期：權限、三遍覆蓋之後抹掉 |
//! | `lock` | 資料目錄的獨佔：OS 層排他鎖，拿不到就不啟動 |
//!
//! 🚫 這個 crate 不印任何東西到 stdout／stderr（`main.rs` 例外）。

pub mod connection;
pub mod handle;
pub mod lock;
pub mod message;
pub mod pack;
pub mod protocol;
pub mod server;
pub mod settings;
pub mod token;

// 維護者 2026-09-23：正式碼不用會讓整支程式收掉的方法（unwrap／expect／panic／索引）——每個失敗要有去處；測試建置放行（測試要看到它炸）。
#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::unreachable,
        clippy::todo,
        clippy::unimplemented,
        clippy::indexing_slicing
    )
)]
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
/// 推播：core 的事件 → RPC 的推播，與每條連線的訂閱集合（rpc-spec §3.9、§4）。
pub mod push;
pub mod server;
pub mod settings;
pub mod token;

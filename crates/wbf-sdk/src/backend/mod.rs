//! `ChatBackend` 的實作們。房間這一側的上游只出現在這個目錄底下（E2EE 引擎另在 `crypto_engine.rs`，/docs/design/overview/architecture-v2.md §8）。

#[cfg(feature = "matrix")]
pub mod matrix_sdk;

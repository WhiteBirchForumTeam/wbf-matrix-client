//! wbfuwunel 的 client SDK。權威是 `docs/design/wbf-client-convention-for-chunk.md`（約定規格書）
//! 與 server repo 的 `chunked-upload-spec.md`（線上規格）；`tests/client_vectors.rs` 對著
//! `docs/design/wbf-client-vectors.json` 跑。
//!
//! 分層：
//! - `cipher`／`chunk_block`／`chunk_crypto`：不需要網路的部分，每塊怎麼加密、事件區塊長什麼樣、seek 怎麼算。
//! - `protocol`：pack 怎麼組、Ack 怎麼讀。
//! - `channel`：一個 pack 進一個 pack 出（WebSocket／HTTP）。
//! - `client` + `upload` + `download`：`WbfClient`，一條通道上的命令。
//! - `login`：純 HTTP 拿 token。
//! - `manifest`：CLI 印的 manifest 與上傳狀態檔。
//! - `media_pool`：媒體儲存池（local-cache-db.md §8），整檔明文的加密池；`media`（feature `cache`）把下載管線、池與 `cache.db` 接起來。
//! - `cache`（feature `cache`）：`cache.db`，SQLCipher 的本地快取（local-cache-db.md §6）；金鑰從 `vault` 來。
//! - `vault`：本地金鑰庫（`local.key`、子金鑰、`session.sealed`），local-cache-db.md §4。
//!
//! - `incoming`：上游給的事件原樣（`IncomingEvent`）、一頁的游標、事件分類與 edit 的有效性規則（local-cache-db.md §7）。
//! - `event_json`：原始 Matrix 事件 JSON → `Message`，matrix backend 與 `recent` 共用。
//! - `chat`：聊天模型與 `ChatBackend` trait；`backend/matrix_sdk`（feature `matrix`）是第一個實作，唯一 `use matrix_sdk` 的地方。

pub mod account_dir;
pub mod backend;
#[cfg(feature = "cache")]
pub mod cache;
pub mod channel;
pub mod chat;
pub mod chunk_block;
pub mod chunk_crypto;
pub mod cipher;
pub mod client;
pub mod download;
pub mod error;
pub mod error_code;
pub mod event_json;
pub mod incoming;
pub mod login;
pub mod manifest;
#[cfg(feature = "cache")]
pub mod media;
pub mod media_pool;
pub mod protocol;
pub mod room_keys;
pub mod upload;
pub mod vault;

pub use account_dir::{find_dir_name_plaintext, to_dir_name, DirScope};
pub use channel::{Channel, PackChannel, Transport};
pub use chat::{
    Attachment, ChatBackend, Conversation, ConversationKind, Message, MessageKind, Update,
    WatchControl, WatchEnd,
};
pub use chunk_block::{BlockError, ChunkedBlock};
pub use chunk_crypto::{CryptoError, DescriptionSlot, FileCipher, Link, SeekTarget};
pub use cipher::Cipher;
pub use client::{OnBatch, RecentPlan, RecentSync, RecentWindow, WbfClient};
pub use download::{DownloadReport, SeekResult};
pub use error::SdkError;
pub use incoming::{EventPage, IncomingEvent};
pub use login::Session;
pub use manifest::{Manifest, UploadState};
pub use room_keys::{get_snapshot_status, SnapshotStatus};
pub use upload::SentSummary;
pub use vault::{Key32, KeyMode, Unlock, Vault};

//! wbfuwunel 的 client SDK。權威是 `docs/design/wbf-client-convention-for-chunk.md`（約定規格書）；
//! `tests/client_vectors.rs` 對著 `docs/design/wbf-client-vectors.json` 跑。
//!
//! 目前只有不需要網路的部分：每塊怎麼加密、描述怎麼加密、事件區塊長什麼樣、seek 怎麼算。
//! 通道與上傳／下載在下一個 PR。

pub mod chunk_block;
pub mod chunk_crypto;
pub mod cipher;

pub use chunk_block::{BlockError, ChunkedBlock};
pub use chunk_crypto::{CryptoError, DescriptionSlot, FileCipher, Link, SeekTarget};
pub use cipher::Cipher;

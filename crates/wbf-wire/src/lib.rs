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
        clippy::indexing_slicing,
        clippy::string_slice
    )
)]
//! wbfuwunel 線上協議的 codec。權威是 server repo 的 `docs/design/chunked-upload-spec.md`；
//! `tests/vectors.rs` 對著複製來的 `docs/design/wbf-vectors.json` 跑，向量沒跟上就紅。
//!
//! 這個 crate 只回答「byte 怎麼排」：沒有 tokio、沒有 matrix、沒有 IO。

pub mod encrypted_file_info;
pub mod pack;

pub use encrypted_file_info::EncryptedFileInfo;
pub use pack::{DecodeError, EncodeError, Kind, Pack};

/// CRC-32C（Castagnoli），與 server 同一個 crate。
///
/// Args:
///     bytes: 任意 bytes, example: b"123456789"
/// Return:
///     u32  example: 0xE306_9283；空輸入回 0
pub fn crc32c(bytes: &[u8]) -> u32 {
    crc32c::crc32c(bytes)
}

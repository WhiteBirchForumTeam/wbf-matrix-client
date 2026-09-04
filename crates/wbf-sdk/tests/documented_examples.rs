//! docstring 裡的 `example:` 值，一個函數一個測試，名字就是函數名。
//! 這裡失敗 = docstring 在說謊。
//!
//! 涵蓋範圍老實說：只有純函數（輸入輸出都寫得成字面值的）。`FileCipher::generate`、
//! `Cipher::default_for_this_machine` 這種依環境的沒有 example，不在這裡。

use wbf_sdk::chunk_crypto::{
    build_nonce, choose_chunk_size, choose_stream_chunk_size, chunk_count, expected_plain_len,
    locate, Link, SeekTarget,
};
use wbf_sdk::{ChunkedBlock, Cipher, DescriptionSlot};

#[test]
fn cipher_from_name() {
    assert_eq!(Cipher::from_name("aes-256-gcm"), Some(Cipher::Aes256Gcm));
}

#[test]
fn cipher_name() {
    assert_eq!(Cipher::ChaCha20Poly1305.name(), "chacha20-poly1305");
    assert_eq!(Cipher::None.name(), "none");
}

#[test]
fn description_slot_nonce_index() {
    assert_eq!(DescriptionSlot::Create.nonce_index(), 0xFFFF_FFFF);
    assert_eq!(DescriptionSlot::Seal.nonce_index(), 0xFFFF_FFFE);
}

#[test]
fn build_nonce_example() {
    assert_eq!(
        build_nonce([0, 1, 2, 3, 4, 5, 6, 7], 1),
        [0, 1, 2, 3, 4, 5, 6, 7, 0, 0, 0, 1]
    );
}

#[test]
fn chunk_count_example() {
    assert_eq!(chunk_count(132056, 65536), Some(3));
    assert_eq!(chunk_count(0, 65536), Some(0));
}

#[test]
fn expected_plain_len_example() {
    assert_eq!(expected_plain_len(132056, 65536, 2), Some(984));
}

#[test]
fn locate_example() {
    assert_eq!(
        locate(71680, 65536),
        Some(SeekTarget {
            index: 1,
            offset: 6144
        })
    );
}

#[test]
fn choose_chunk_size_example() {
    assert_eq!(choose_chunk_size(1024), 65536);
    assert_eq!(choose_chunk_size(50 * 1024 * 1024), 1_048_576);
}

#[test]
fn choose_stream_chunk_size_example() {
    assert_eq!(choose_stream_chunk_size(Link::MobileOrUnknown), 65536);
    assert_eq!(choose_stream_chunk_size(Link::WifiOrWired), 1_048_576);
}

#[test]
fn chunked_block_to_description_json() {
    let block = ChunkedBlock {
        v: 1,
        cipher: Cipher::None,
        key: None,
        nonce_base: None,
        chunk_size: 16,
        file_size: None,
        name: None,
        mimetype: None,
        sha256: None,
    };
    assert_eq!(
        block.to_description_json(),
        br#"{"v":1,"cipher":"none","chunk_size":16}"#
    );
}

#[test]
fn chunked_block_from_description_json() {
    let block =
        ChunkedBlock::from_description_json(br#"{"v":1,"cipher":"none","chunk_size":16}"#).unwrap();
    assert_eq!(block.chunk_size, 16);
    assert_eq!(block.cipher, Cipher::None);
}

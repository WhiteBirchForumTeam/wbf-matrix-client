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
        block.to_description_json().unwrap(),
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

// ---- device_version（server 的 wbf-room-device-version.md）----

#[test]
fn device_version_parse() {
    use wbf_sdk::device_version::DeviceVersion;
    assert_eq!(
        DeviceVersion::parse("3-810b7c3be4"),
        Some(DeviceVersion {
            seq: 3,
            hash: "810b7c3be4".into()
        })
    );
}

#[test]
fn device_version_to_text() {
    use wbf_sdk::device_version::DeviceVersion;
    assert_eq!(
        DeviceVersion {
            seq: 3,
            hash: "810b7c3be4".into()
        }
        .to_text(),
        "3-810b7c3be4"
    );
}

#[test]
fn room_device_versions_from_members_body() {
    use wbf_sdk::device_version::RoomDeviceVersions;
    let body = serde_json::json!({"chunk":[{"type":"m.room.member","state_key":"@bob:localhost","content":{"membership":"join"},"unsigned":{"org.wbftw.device_version":"3-810b7c3be4"}}],"org.wbftw.room_version":81234});
    let versions = RoomDeviceVersions::from_members_body(&body).unwrap();
    assert_eq!(versions.room_version, 81234);
    assert_eq!(versions.members["@bob:localhost"].to_text(), "3-810b7c3be4");
}

/// 黃金向量的輸入寫在 `device_version.rs` 的單元測試（太長不放 docstring）；這裡只釘住輸出的形狀。
#[test]
fn compute_device_keys_hash() {
    use wbf_sdk::device_version::{compute_device_keys_hash, HASH_HEX_LEN};
    let hash = compute_device_keys_hash("@bob:localhost", &serde_json::json!({})).unwrap();
    assert_eq!(hash.len(), HASH_HEX_LEN);
    assert!(hash
        .bytes()
        .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()));
}

#[test]
fn sdk_error_current_room_version() {
    let error = wbf_sdk::protocol::server_error(
        br#"{"code":"RoomDevicesChanged","code_id":1506,"message":"stale","room_version":81240}"#,
    );
    assert_eq!(error.current_room_version(), Some(81240));
}

// ---- Device 的原生 pack 與 to_device_state ----

#[test]
fn device_items_destroy() {
    let pack = wbf_sdk::protocol::device_items_destroy(&[4712, 4713], 1, 1);
    assert_eq!(pack.meta, br#"{"tc":2}"#);
    assert_eq!(pack.data.len(), 16);
}

#[test]
fn decode_counts() {
    let pack = wbf_sdk::protocol::device_items_destroy(&[4712, 4713], 1, 1);
    assert_eq!(
        wbf_sdk::protocol::decode_counts(&pack.data).unwrap(),
        vec![4712, 4713]
    );
}

#[test]
fn to_device_state_mark_processed_and_destroyed() {
    let mut state = wbf_sdk::to_device_state::ToDeviceState::default();
    state.mark_processed(4712);
    assert_eq!(state.cd_seq, Some(4712));
    assert_eq!(state.to_destroy, vec![4712]);
    state.mark_destroyed(&[4712]);
    assert!(state.to_destroy.is_empty());
}

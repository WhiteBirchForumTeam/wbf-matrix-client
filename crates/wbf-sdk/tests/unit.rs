//! 拒絕路徑與「對不對」的測試。向量檔（`client_vectors.rs`）只證明實作沒變；
//! 這裡的 RFC 8439 與 NIST GCM 兩條證明底下的 AEAD 呼叫是標準的那個。

use wbf_sdk::chunk_crypto::{chunk_count, expected_plain_len, locate, MAX_CHUNK_INDEX};
use wbf_sdk::{ChunkedBlock, Cipher, CryptoError, DescriptionSlot, FileCipher};

fn unhex(text: &str) -> Vec<u8> {
    hex::decode(text.replace([' ', '\n'], "")).expect("valid hex")
}

/// RFC 8439 §2.8.2 的 AEAD 測試向量。
#[test]
fn chacha20_poly1305_matches_rfc_8439() {
    let key: [u8; 32] = unhex("808182838485868788898a8b8c8d8e8f909192939495969798999a9b9c9d9e9f")
        .try_into()
        .unwrap();
    let nonce: [u8; 12] = unhex("070000004041424344454647").try_into().unwrap();
    let aad = unhex("50515253c0c1c2c3c4c5c6c7");
    let plain = b"Ladies and Gentlemen of the class of '99: If I could offer you only one tip for the future, sunscreen would be it.";
    let expected = unhex(
        "d31a8d34648e60db7b86afbc53ef7ec2a4aded51296e08fea9e2b5a736ee62d6\
         3dbea45e8ca9671282fafb69da92728b1a71de0a9e060b2905d6a5b67ecd3b36\
         92ddbd7f2d778b8c9803aee328091b58fab324e4fad675945585808b4831d7bc\
         3ff4def08e4b7a9de576d26586cec64b6116\
         1ae10b594f09e26a7e902ecbd0600691",
    );
    let sealed = Cipher::ChaCha20Poly1305.seal(&key, &nonce, &aad, plain);
    assert_eq!(sealed, expected);
    assert_eq!(
        Cipher::ChaCha20Poly1305
            .open(&key, &nonce, &aad, &sealed)
            .unwrap(),
        plain
    );
}

/// McGrew & Viega《The Galois/Counter Mode of Operation (GCM)》（NIST 收錄的 GCM 提案）附錄 B 的 Test Case 16。
/// 附錄 B 的編號：1–6 是 AES-128、7–12 是 AES-192、**13–18 是 AES-256**；TC16 是 TC4 的輸入配 256-bit 金鑰
/// `feffe992…8308` 重複兩次，密文 `522dc1f0…`、標籤 `76fc6ece…`。同輸入配 128-bit 金鑰是 TC4（密文 `42831ec2…`），
/// 不是這一條。
#[test]
fn aes_256_gcm_matches_nist_test_case_16() {
    let key: [u8; 32] = unhex("feffe9928665731c6d6a8f9467308308feffe9928665731c6d6a8f9467308308")
        .try_into()
        .unwrap();
    let nonce: [u8; 12] = unhex("cafebabefacedbaddecaf888").try_into().unwrap();
    let aad = unhex("feedfacedeadbeeffeedfacedeadbeefabaddad2");
    let plain = unhex(
        "d9313225f88406e5a55909c5aff5269a86a7a9531534f7da2e4c303d8a318a72\
         1c3c0c95956809532fcf0e2449a6b525b16aedf5aa0de657ba637b39",
    );
    let expected = unhex(
        "522dc1f099567d07f47f37a32a84427d643a8cdcbfe5c0c97598a2bd2555d1aa\
         8cb08e48590dbb3da7b08b1056828838c5f61e6393ba7a0abcc9f662\
         76fc6ece0f4e1768cddf8853bb2d551b",
    );
    let sealed = Cipher::Aes256Gcm.seal(&key, &nonce, &aad, &plain);
    assert_eq!(sealed, expected);
    assert_eq!(
        Cipher::Aes256Gcm.open(&key, &nonce, &aad, &sealed).unwrap(),
        plain
    );
}

fn fixed(cipher: Cipher) -> FileCipher {
    FileCipher::with_fixed(cipher, [0x11; 32], [0x22; 8], 16)
}

#[test]
fn tampered_chunk_is_rejected() {
    for cipher in [Cipher::ChaCha20Poly1305, Cipher::Aes256Gcm] {
        let file_cipher = fixed(cipher);
        let mut sealed = file_cipher.seal_chunk(0, b"0123456789abcdef").unwrap();
        sealed[3] ^= 0x01;
        assert_eq!(
            file_cipher.open_chunk(0, &sealed, 16),
            Err(CryptoError::TagInvalid),
            "{cipher:?}"
        );
    }
}

#[test]
fn chunk_opened_under_wrong_index_is_rejected() {
    let file_cipher = fixed(Cipher::ChaCha20Poly1305);
    let sealed = file_cipher.seal_chunk(0, b"0123456789abcdef").unwrap();
    assert_eq!(
        file_cipher.open_chunk(1, &sealed, 16),
        Err(CryptoError::TagInvalid)
    );
}

#[test]
fn chunk_opened_with_description_nonce_is_rejected() {
    // 描述與塊的 AAD 不同，拿描述密文當塊解必須失敗（約定 §3：網域分開）。
    let file_cipher = fixed(Cipher::Aes256Gcm);
    let sealed = file_cipher.seal_description(DescriptionSlot::Create, b"0123456789abcdef");
    assert!(file_cipher
        .open_description(DescriptionSlot::Seal, &sealed)
        .is_err());
    let as_chunk = Cipher::Aes256Gcm.open(
        &[0x11; 32],
        &wbf_sdk::chunk_crypto::build_nonce([0x22; 8], 0xFFFF_FFFF),
        wbf_sdk::chunk_crypto::CHUNK_AAD,
        &sealed,
    );
    assert!(as_chunk.is_none());
}

#[test]
fn wrong_length_is_rejected_before_decrypting() {
    let file_cipher = fixed(Cipher::ChaCha20Poly1305);
    let sealed = file_cipher.seal_chunk(0, b"0123456789abcdef").unwrap();
    assert_eq!(
        file_cipher.open_chunk(0, &sealed, 15),
        Err(CryptoError::LengthMismatch {
            expected: 31,
            actual: 32
        })
    );
    assert_eq!(
        file_cipher.open_chunk(0, &sealed[..31], 16),
        Err(CryptoError::LengthMismatch {
            expected: 32,
            actual: 31
        })
    );
}

#[test]
fn plaintext_mode_still_checks_length() {
    let file_cipher = fixed(Cipher::None);
    let sealed = file_cipher.seal_chunk(0, b"0123456789abcdef").unwrap();
    assert_eq!(sealed, b"0123456789abcdef");
    assert_eq!(
        file_cipher.open_chunk(0, &sealed, 16).unwrap(),
        b"0123456789abcdef"
    );
    assert_eq!(
        file_cipher.open_chunk(0, &sealed, 8),
        Err(CryptoError::LengthMismatch {
            expected: 8,
            actual: 16
        })
    );
    assert_eq!(file_cipher.nonce_base(), None);
}

#[test]
fn oversized_and_reserved_indices_are_rejected() {
    let file_cipher = fixed(Cipher::ChaCha20Poly1305);
    assert_eq!(
        file_cipher.seal_chunk(0, &[0u8; 17]),
        Err(CryptoError::ChunkTooLong {
            chunk_size: 16,
            actual: 17
        })
    );
    assert_eq!(
        file_cipher.seal_chunk(MAX_CHUNK_INDEX + 1, b"x"),
        Err(CryptoError::IndexReserved(MAX_CHUNK_INDEX + 1))
    );
    assert_eq!(
        file_cipher.open_chunk(0xFFFF_FFFF, b"x", 1),
        Err(CryptoError::IndexReserved(0xFFFF_FFFF))
    );
    assert!(file_cipher.seal_chunk(MAX_CHUNK_INDEX, b"x").is_ok());
}

#[test]
fn generate_never_reuses_key_or_nonce_base() {
    let first = FileCipher::generate(Cipher::ChaCha20Poly1305, 65536);
    let second = FileCipher::generate(Cipher::ChaCha20Poly1305, 65536);
    assert_ne!(first, second);
    assert_ne!(first.nonce_base(), second.nonce_base());
    let plaintext_mode = FileCipher::generate(Cipher::None, 65536);
    assert_eq!(plaintext_mode.nonce_base(), None);
    assert!(plaintext_mode.to_event_block(1).key.is_none());
}

#[test]
fn default_cipher_is_never_plaintext() {
    assert!(Cipher::default_for_this_machine().is_encrypting());
}

#[test]
fn cipher_names_roundtrip_and_unknown_is_none() {
    for cipher in [Cipher::ChaCha20Poly1305, Cipher::Aes256Gcm, Cipher::None] {
        assert_eq!(Cipher::from_name(cipher.name()), Some(cipher));
    }
    assert_eq!(Cipher::from_name("aes-128-gcm"), None);
    assert_eq!(Cipher::from_name(""), None);
    assert_eq!(Cipher::from_name("ChaCha20-Poly1305"), None);
}

#[test]
fn event_block_json_ignores_unknown_fields_and_rejects_bad_base64() {
    let with_extra = r#"{"v":1,"cipher":"none","chunk_size":16,"file_size":1,"future_field":true}"#;
    let block: ChunkedBlock = serde_json::from_str(with_extra).unwrap();
    block.check_as_event_block().unwrap();

    let short_key = r#"{"v":1,"cipher":"aes-256-gcm","key":"AAEC","nonce_base":"oKGio6SlpqE=","chunk_size":16,"file_size":1}"#;
    assert!(serde_json::from_str::<ChunkedBlock>(short_key).is_err());
}

#[test]
fn description_mismatch_is_detected() {
    let file_cipher = fixed(Cipher::ChaCha20Poly1305);
    let block = file_cipher.to_event_block(40);
    let mut other = block.to_description();
    assert!(block.is_consistent_with_description(&other));
    other.chunk_size = 32;
    assert!(!block.is_consistent_with_description(&other));
    let mut other = block.to_description();
    other.file_size = None;
    assert!(
        block.is_consistent_with_description(&other),
        "missing file_size is a streaming Create, not a mismatch"
    );
    other.nonce_base = Some([0; 8]);
    assert!(!block.is_consistent_with_description(&other));
}

#[test]
fn length_helpers_reject_zero_chunk_size_and_out_of_range_index() {
    assert_eq!(chunk_count(1, 0), None);
    assert_eq!(chunk_count(0, 16), Some(0));
    assert_eq!(expected_plain_len(40, 16, 3), None);
    assert_eq!(expected_plain_len(40, 0, 0), None);
    assert_eq!(locate(5, 0), None);
    assert_eq!(chunk_count(u64::MAX, 1), None, "more chunks than indices");
}

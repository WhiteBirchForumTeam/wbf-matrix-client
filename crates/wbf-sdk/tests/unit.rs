//! 拒絕路徑與「對不對」的測試。向量檔（`client_vectors.rs`）只證明實作沒變；
//! 這裡的 RFC 8439 與 NIST GCM 兩條證明底下的 AEAD 呼叫是標準的那個。

use wbf_sdk::chunk_crypto::{chunk_count, expected_plain_len, locate, MAX_CHUNK_INDEX};
use wbf_sdk::error_code::WbfErrorCode;
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

/// `Event/Recent` 對著 server 產生的 `wbf-vectors.json`（pack-pipeline §6）：請求要逐 byte 一樣（meta 的鍵序也是，
/// `id` 是 client 選的），回應是 `Event/Batch`，data 是 u32 大端長度前綴的事件。
#[test]
fn event_recent_and_batch_match_server_vectors() {
    use wbf_sdk::protocol::{self, event_seqs, BatchMeta, RecentRequest};
    use wbf_sdk::SdkError;
    use wbf_wire::Pack;
    let vectors: serde_json::Value =
        serde_json::from_str(include_str!("../../../docs/design/wbf-vectors.json")).unwrap();
    let pack_named = |name: &str| -> Pack {
        let entry = vectors["packs"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["name"] == name)
            .unwrap_or_else(|| panic!("vector {name}"));
        Pack::decode(&hex::decode(entry["bytes_hex"].as_str().unwrap()).unwrap()).unwrap()
    };

    let expected = [
        (
            "recent_first_start",
            RecentRequest {
                rooms: None,
                limit: 2,
                cg_seq: None,
                before: None,
                batch: Some(1),
            },
        ),
        (
            "recent_with_cached_g_seq",
            RecentRequest {
                rooms: None,
                limit: 320,
                cg_seq: Some(4700),
                before: None,
                batch: Some(10),
            },
        ),
        (
            "recent_next_window",
            RecentRequest {
                rooms: None,
                limit: 320,
                cg_seq: Some(4700),
                before: Some(4711),
                batch: Some(10),
            },
        ),
    ];
    for (name, request) in expected {
        let vector = pack_named(name);
        let ours = protocol::recent(&request, vector.id, vector.seq);
        assert_eq!(ours, vector, "{name}");
        assert_eq!(
            ours.encode().unwrap(),
            vector.encode().unwrap(),
            "{name} bytes"
        );
    }

    // 點名一個房間的歷史（wbfuwunel #51）。
    // ⚠️ 這一筆**比語意、不比 byte**：server 的向量是手寫的 JSON 字串，這筆的 key 順序是
    // `rooms, before, limit, batch`，而上面三筆是 `limit, cg_seq, before, batch` —— 一個 struct 的
    // 序列化順序只能有一種，兩邊不可能同時逐 byte 對上。server 用 JSON 解析、不看順序，所以
    // 線上是相容的；🚫 但不要因此把上面三筆也改成比語意 —— 它們對得上，就該繼續逐 byte 釘住。
    let vector = pack_named("recent_one_room_history");
    let ours = protocol::recent(
        &RecentRequest {
            rooms: Some(vec!["!r:localhost".to_string()]),
            limit: 50,
            cg_seq: None,
            before: Some(4711),
            batch: Some(10),
        },
        vector.id,
        vector.seq,
    );
    let meta_of =
        |pack: &Pack| -> serde_json::Value { serde_json::from_slice(&pack.meta).unwrap() };
    assert_eq!(
        meta_of(&ours),
        meta_of(&vector),
        "recent_one_room_history meta"
    );
    assert_eq!(
        (
            ours.kind,
            ours.subtype,
            ours.flags,
            ours.id,
            ours.seq,
            &ours.data
        ),
        (
            vector.kind,
            vector.subtype,
            vector.flags,
            vector.id,
            vector.seq,
            &vector.data
        ),
        "recent_one_room_history 其他欄位逐一相等"
    );

    // 一窗兩個 Batch：seq 0 的 r = 1，seq 1 的 r = 0；id 都抄請求的 10。
    let request = pack_named("recent_first_start");
    let first = protocol::expect_batch(&request, pack_named("batch_first"), 0).expect("seq 0");
    let (meta, events) = protocol::parse_batch(&first).unwrap();
    assert_eq!(
        meta,
        BatchMeta {
            tc: 2,
            bc: 1,
            fs: 4712,
            ls: 4712,
            r: 1,
            more: true
        }
    );
    assert_eq!(events.len(), 1);
    assert_eq!(event_seqs(&events[0]), (Some(2), Some(4712)));
    assert_eq!(events[0]["room_id"], "!r:localhost");
    let last = protocol::expect_batch(&request, pack_named("batch_last"), 1).expect("seq 1");
    let (meta, events) = protocol::parse_batch(&last).unwrap();
    assert_eq!(
        meta,
        BatchMeta {
            tc: 2,
            bc: 1,
            fs: 4711,
            ls: 4711,
            r: 0,
            // ⚠️ `r = 0` 但 `more = true`：這窗結束了，但它停在上限（limit 2 剛好滿），後面還有。
            more: true
        }
    );
    assert_eq!(event_seqs(&events[0]), (Some(1), Some(4711)));
    // seq 跳號、id 沒抄都要拒。
    assert!(protocol::expect_batch(&request, pack_named("batch_last"), 0).is_err());
    let other = pack_named("recent_with_cached_g_seq");
    assert!(protocol::expect_batch(&other, pack_named("batch_first"), 0).is_err());
    // 空窗：一個 bc = 0、r = 0、fs = ls = 0 的 Batch。
    let empty = protocol::expect_batch(&other, pack_named("batch_empty_window"), 0).unwrap();
    let (meta, events) = protocol::parse_batch(&empty).unwrap();
    assert_eq!(
        meta,
        BatchMeta {
            tc: 0,
            bc: 0,
            fs: 0,
            ls: 0,
            r: 0,
            more: false
        }
    );
    assert!(events.is_empty());
    // 🚨 沒有 `more` 的 Batch（舊 server）要當 `true`：不確定就再問一趟，🚫 不假設拿完了。
    let without_more: BatchMeta =
        serde_json::from_str(r#"{"tc":0,"bc":0,"fs":0,"ls":0,"r":0}"#).unwrap();
    assert!(without_more.more, "缺欄位＝還有");
    // 走 HTTP 的 Recent：server 回 Error(Unsupported)，expect_batch 變 Server 錯。
    match protocol::expect_batch(&request, pack_named("error_unsupported"), 0) {
        Err(SdkError::Server { code, .. }) => assert_eq!(code, "Unsupported"),
        other => panic!("expected Server(Unsupported), got {other:?}"),
    }
    // 🚨 server 向量裡的每一個 Error：**認碼只看 `code_id`**（issue #29 第 2 項）。
    for (name, expected) in [
        ("error_superseded", WbfErrorCode::Superseded),
        ("error_rate_limited", WbfErrorCode::RateLimited),
        ("error_out_of_order", WbfErrorCode::OutOfOrder),
        ("error_unsupported", WbfErrorCode::Unsupported),
        (
            "error_too_many_connections",
            WbfErrorCode::TooManyConnections,
        ),
        ("error_invalid_request", WbfErrorCode::InvalidRequest),
    ] {
        let error = protocol::server_error(&pack_named(name).meta);
        assert_eq!(error.wbf_code(), Some(expected), "{name}");
        assert_eq!(
            u64::from(expected.id()),
            serde_json::from_slice::<serde_json::Value>(&pack_named(name).meta).unwrap()["code_id"]
                .as_u64()
                .unwrap(),
            "{name}：表上的號要跟 server 向量一樣"
        );
    }
    // 🚫 `code_id: 0` 是「欄位漏了」的預設值（server 表：0 永遠不是合法的碼）：
    // 🚫 不准變成 `Some(0)` —— 重送判斷那邊 `from_id(0)` 本來就認不得，但 log 會印出「(0)」，
    // 讀的人會以為 server 真的回了碼 0。字串、負數也一樣當沒有。
    for meta in [
        br#"{"code":"Corrupt","code_id":0,"message":"m"}"#.as_slice(),
        br#"{"code":"Corrupt","code_id":"1002","message":"m"}"#.as_slice(),
        br#"{"code":"Corrupt","code_id":-1,"message":"m"}"#.as_slice(),
        br#"{"code":"Corrupt","message":"m"}"#.as_slice(),
    ] {
        let error = protocol::server_error(meta);
        let SdkError::Server { code_id, .. } = &error else {
            panic!("{error:?}")
        };
        assert_eq!(*code_id, None, "{}", String::from_utf8_lossy(meta));
        assert_eq!(
            error.to_string(),
            "server Corrupt: m",
            "log 不准印出假的序號"
        );
    }
    match protocol::server_error(&pack_named("error_too_many_connections").meta) {
        SdkError::Server { code, meta, .. } => {
            assert_eq!(code, "TooManyConnections");
            assert_eq!(meta["max_connections"], 4);
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(
        event_seqs(&serde_json::json!({ "content": {} })),
        (None, None),
        "no unsigned = no seq"
    );
}

/// Batch data 的 u32 前綴切法：round trip、空的、多一個 byte、長度指過檔尾（server review 建議先落這個測試）。
#[test]
fn length_prefixed_events_round_trip_and_reject_misaligned_data() {
    use wbf_sdk::protocol::{join_length_prefixed, split_length_prefixed};
    let items: Vec<Vec<u8>> = vec![b"{}".to_vec(), Vec::new(), vec![0xffu8; 70000]];
    let joined = join_length_prefixed(&items);
    assert_eq!(joined.len(), 4 * 3 + 2 + 70000);
    let split = split_length_prefixed(&joined).unwrap();
    assert_eq!(split.len(), 3);
    assert_eq!(split[0], b"{}");
    assert!(split[1].is_empty());
    assert_eq!(split[2].len(), 70000);
    assert!(split_length_prefixed(&[]).unwrap().is_empty());
    assert!(
        split_length_prefixed(&joined[..joined.len() - 1]).is_err(),
        "truncated last item"
    );
    assert!(split_length_prefixed(&[0, 0, 0]).is_err(), "stray bytes");
    assert!(
        split_length_prefixed(&[0, 0, 0, 9, 1]).is_err(),
        "prefix past end"
    );
    let mut extra = joined.clone();
    extra.push(0);
    assert!(split_length_prefixed(&extra).is_err(), "one trailing byte");
}

/// `Event/Send` 的 meta 鍵序照 media-attachments.md §3：room_id、type、txn_id、attachments。
#[test]
fn event_send_meta_shape() {
    use wbf_sdk::protocol::{self, SendRequest};
    let request = SendRequest {
        room_id: "!r:localhost".into(),
        event_type: "m.room.encrypted".into(),
        txn_id: "t1".into(),
        attachments: vec!["mxc://localhost/1122334455667788".into()],
    };
    let pack = protocol::send_event(
        &request,
        br#"{"algorithm":"m.megolm.v1.aes-sha2"}"#.to_vec(),
        7,
    );
    assert_eq!(pack.kind, wbf_wire::Kind::Event);
    assert_eq!(pack.subtype, wbf_wire::pack::event::SEND);
    assert_eq!(
        String::from_utf8(pack.meta).unwrap(),
        r#"{"room_id":"!r:localhost","type":"m.room.encrypted","txn_id":"t1","attachments":["mxc://localhost/1122334455667788"]}"#
    );
}

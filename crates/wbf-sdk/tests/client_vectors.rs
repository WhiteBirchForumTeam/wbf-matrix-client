//! 對著 `docs/design/wbf-client-vectors.json`（約定規格書 §9）跑。
//! 這裡紅 = 這個 crate 與約定漂移了，或向量檔過期了。
//!
//! 向量檔由這個測試本身產生：`WBF_WRITE_CLIENT_VECTORS=1 cargo test -p wbf-sdk --test client_vectors`
//! 會把現在的實作輸出整份覆寫過去。所以它證明的是「實作沒有變」，不是「實作是對的」；
//! 「對」靠 `unit.rs` 裡對 RFC 8439 與 NIST 向量的那兩條，加上讀約定規格書的人。
//! 改約定時：先改程式、重新產生、看 diff 是不是你要的、再 commit。

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use wbf_sdk::chunk_crypto::{build_nonce, chunk_count, expected_plain_len, locate};
use wbf_sdk::{ChunkedBlock, Cipher, DescriptionSlot, FileCipher};

const VECTORS_PATH: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../docs/design/wbf-client-vectors.json"
);

#[derive(Serialize, Deserialize)]
struct Vectors {
    format_version: u32,
    convention_v: u32,
    /// 每個 cipher 一個檔：固定 key／nonce_base，一個小檔切幾塊。
    files: Vec<FileVector>,
    /// 約定 §7。
    seek: Vec<SeekVector>,
    /// 事件區塊或描述，必須被拒絕的樣本；`error` 是 `BlockError` 的變體名。
    rejected_blocks: Vec<RejectedBlockVector>,
}

#[derive(Serialize, Deserialize)]
struct FileVector {
    name: String,
    cipher: String,
    key_hex: String,
    nonce_base_hex: String,
    chunk_size: u32,
    plaintext_hex: String,
    chunks: Vec<ChunkVector>,
    /// 串流的 `Create`：不含 `file_size`、`sha256`。
    create_description_json: String,
    create_description_data_hex: String,
    /// `Seal`：完整。
    seal_description_json: String,
    seal_description_data_hex: String,
    /// 房間事件的 `org.wbftw.wbfuwunel.chunked`。
    event_block_json: String,
}

#[derive(Serialize, Deserialize)]
struct ChunkVector {
    index: u32,
    nonce_hex: String,
    plain_len: usize,
    data_hex: String,
}

#[derive(Serialize, Deserialize)]
struct SeekVector {
    chunk_size: u32,
    pos: u64,
    index: u32,
    offset: usize,
}

#[derive(Serialize, Deserialize)]
struct RejectedBlockVector {
    name: String,
    /// `event_block` 或 `description`：用哪個檢查。
    checked_as: String,
    json: String,
    error: String,
}

fn fixed_key() -> [u8; 32] {
    core::array::from_fn(|position| position as u8)
}

fn fixed_nonce_base() -> [u8; 8] {
    [0xA0, 0xA1, 0xA2, 0xA3, 0xA4, 0xA5, 0xA6, 0xA7]
}

/// 40 byte、chunk_size 16 → 三塊，最後一塊 8 byte。
fn sample_plaintext() -> Vec<u8> {
    (0..40u8)
        .map(|position| position.wrapping_mul(7).wrapping_add(3))
        .collect()
}

fn build_file_vector(cipher: Cipher) -> FileVector {
    let chunk_size = 16u32;
    let plaintext = sample_plaintext();
    let file_size = plaintext.len() as u64;
    let file_cipher = FileCipher::with_fixed(cipher, fixed_key(), fixed_nonce_base(), chunk_size);

    let chunks = plaintext
        .chunks(chunk_size as usize)
        .enumerate()
        .map(|(index, plain)| {
            let index = index as u32;
            let nonce = file_cipher
                .nonce_base()
                .map(|nonce_base| hex::encode(build_nonce(nonce_base, index)))
                .unwrap_or_default();
            ChunkVector {
                index,
                nonce_hex: nonce,
                plain_len: plain.len(),
                data_hex: hex::encode(
                    file_cipher
                        .seal_chunk(index, plain)
                        .expect("index and length in range"),
                ),
            }
        })
        .collect();

    let mut event_block = file_cipher.to_event_block(file_size);
    event_block.name = Some("sample.bin".to_string());
    event_block.mimetype = Some("application/octet-stream".to_string());
    event_block.sha256 = Some(hex::encode(Sha256::digest(&plaintext)));

    let create_description = ChunkedBlock {
        file_size: None,
        sha256: None,
        ..event_block.to_description()
    };
    let create_json = serde_json::to_vec(&create_description).expect("serializes");
    let seal_json = event_block.to_description_json();

    FileVector {
        name: format!("{} 40 bytes in 16-byte chunks", cipher.name()),
        cipher: cipher.name().to_string(),
        key_hex: if cipher.is_encrypting() {
            hex::encode(fixed_key())
        } else {
            String::new()
        },
        nonce_base_hex: if cipher.is_encrypting() {
            hex::encode(fixed_nonce_base())
        } else {
            String::new()
        },
        chunk_size,
        plaintext_hex: hex::encode(&plaintext),
        chunks,
        create_description_json: String::from_utf8(create_json.clone()).expect("utf-8"),
        create_description_data_hex: hex::encode(
            file_cipher.seal_description(DescriptionSlot::Create, &create_json),
        ),
        seal_description_json: String::from_utf8(seal_json.clone()).expect("utf-8"),
        seal_description_data_hex: hex::encode(
            file_cipher.seal_description(DescriptionSlot::Seal, &seal_json),
        ),
        event_block_json: serde_json::to_string(&event_block).expect("serializes"),
    }
}

fn build_vectors() -> Vectors {
    let key_b64 = "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=";
    let nonce_b64 = "oKGio6SlpqE=";
    Vectors {
        format_version: 1,
        convention_v: 1,
        files: vec![
            build_file_vector(Cipher::ChaCha20Poly1305),
            build_file_vector(Cipher::Aes256Gcm),
            build_file_vector(Cipher::None),
        ],
        seek: vec![
            SeekVector {
                chunk_size: 65536,
                pos: 0,
                index: 0,
                offset: 0,
            },
            SeekVector {
                chunk_size: 65536,
                pos: 65535,
                index: 0,
                offset: 65535,
            },
            SeekVector {
                chunk_size: 65536,
                pos: 65536,
                index: 1,
                offset: 0,
            },
            SeekVector {
                chunk_size: 65536,
                pos: 71680,
                index: 1,
                offset: 6144,
            },
            SeekVector {
                chunk_size: 1_048_576,
                pos: 150_000_000,
                index: 143,
                offset: 53_632,
            },
        ],
        rejected_blocks: vec![
            RejectedBlockVector {
                name: "v 2".into(),
                checked_as: "event_block".into(),
                json: format!(
                    r#"{{"v":2,"cipher":"chacha20-poly1305","key":"{key_b64}","nonce_base":"{nonce_b64}","chunk_size":16,"file_size":40}}"#
                ),
                error: "UnknownVersion".into(),
            },
            RejectedBlockVector {
                name: "encrypted without key".into(),
                checked_as: "event_block".into(),
                json: format!(
                    r#"{{"v":1,"cipher":"aes-256-gcm","nonce_base":"{nonce_b64}","chunk_size":16,"file_size":40}}"#
                ),
                error: "MissingKey".into(),
            },
            RejectedBlockVector {
                name: "plaintext with key".into(),
                checked_as: "event_block".into(),
                json: format!(
                    r#"{{"v":1,"cipher":"none","key":"{key_b64}","chunk_size":16,"file_size":40}}"#
                ),
                error: "UnexpectedKey".into(),
            },
            RejectedBlockVector {
                name: "encrypted without nonce_base".into(),
                checked_as: "event_block".into(),
                json: format!(
                    r#"{{"v":1,"cipher":"chacha20-poly1305","key":"{key_b64}","chunk_size":16,"file_size":40}}"#
                ),
                error: "MissingNonceBase".into(),
            },
            RejectedBlockVector {
                name: "event block without file_size".into(),
                checked_as: "event_block".into(),
                json: format!(
                    r#"{{"v":1,"cipher":"chacha20-poly1305","key":"{key_b64}","nonce_base":"{nonce_b64}","chunk_size":16}}"#
                ),
                error: "MissingFileSize".into(),
            },
            RejectedBlockVector {
                name: "chunk_size 0".into(),
                checked_as: "event_block".into(),
                json: format!(
                    r#"{{"v":1,"cipher":"chacha20-poly1305","key":"{key_b64}","nonce_base":"{nonce_b64}","chunk_size":0,"file_size":40}}"#
                ),
                error: "ChunkSizeZero".into(),
            },
            RejectedBlockVector {
                name: "description carrying key".into(),
                checked_as: "description".into(),
                json: format!(
                    r#"{{"v":1,"cipher":"chacha20-poly1305","key":"{key_b64}","nonce_base":"{nonce_b64}","chunk_size":16}}"#
                ),
                error: "UnexpectedKey".into(),
            },
            RejectedBlockVector {
                name: "unknown cipher".into(),
                checked_as: "event_block".into(),
                json: r#"{"v":1,"cipher":"aes-128-gcm","chunk_size":16,"file_size":40}"#.into(),
                error: "ParseError".into(),
            },
        ],
    }
}

fn is_generating() -> bool {
    std::env::var_os("WBF_WRITE_CLIENT_VECTORS").is_some()
}

fn load_vectors() -> Vectors {
    let text = std::fs::read_to_string(VECTORS_PATH)
        .expect("wbf-client-vectors.json exists; generate with WBF_WRITE_CLIENT_VECTORS=1");
    let vectors: Vectors = serde_json::from_str(&text).expect("wbf-client-vectors.json parses");
    assert_eq!(
        vectors.format_version, 1,
        "vector file format changed; update this test"
    );
    assert_eq!(vectors.convention_v, 1);
    vectors
}

fn unhex(hex_text: &str) -> Vec<u8> {
    hex::decode(hex_text).expect("valid hex in vectors")
}

fn file_cipher_of(vector: &FileVector) -> FileCipher {
    let cipher = Cipher::from_name(&vector.cipher).expect("known cipher in vectors");
    let key: [u8; 32] = if cipher.is_encrypting() {
        unhex(&vector.key_hex).try_into().expect("32 bytes")
    } else {
        [0; 32]
    };
    let nonce_base: [u8; 8] = if cipher.is_encrypting() {
        unhex(&vector.nonce_base_hex).try_into().expect("8 bytes")
    } else {
        [0; 8]
    };
    FileCipher::with_fixed(cipher, key, nonce_base, vector.chunk_size)
}

/// 產生器。平常被 `#[ignore]`；設環境變數才跑，而且跑完就結束（不驗）。
#[test]
fn write_vectors_when_asked() {
    if !is_generating() {
        return;
    }
    let text = serde_json::to_string_pretty(&build_vectors()).expect("serializes");
    std::fs::write(VECTORS_PATH, text + "\n").expect("write vectors");
}

#[test]
fn vectors_match_current_implementation() {
    if is_generating() {
        return;
    }
    let on_disk = std::fs::read_to_string(VECTORS_PATH).expect("vectors exist");
    let regenerated = serde_json::to_string_pretty(&build_vectors()).expect("serializes") + "\n";
    assert!(on_disk == regenerated, "implementation output differs from wbf-client-vectors.json; if intended, regenerate and review the diff");
}

#[test]
fn chunks_seal_and_open() {
    if is_generating() {
        return;
    }
    let vectors = load_vectors();
    assert_eq!(vectors.files.len(), 3, "one file per cipher");
    for file in &vectors.files {
        let file_cipher = file_cipher_of(file);
        let plaintext = unhex(&file.plaintext_hex);
        let file_size = plaintext.len() as u64;
        assert_eq!(
            chunk_count(file_size, file.chunk_size),
            Some(file.chunks.len() as u32),
            "{}",
            file.name
        );

        for chunk in &file.chunks {
            let expected_len = expected_plain_len(file_size, file.chunk_size, chunk.index)
                .expect("index in range");
            assert_eq!(
                expected_len, chunk.plain_len,
                "{} chunk {}",
                file.name, chunk.index
            );
            let start = chunk.index as usize * file.chunk_size as usize;
            let plain = &plaintext[start..start + expected_len];

            let sealed = file_cipher.seal_chunk(chunk.index, plain).expect("seals");
            assert_eq!(
                hex::encode(&sealed),
                chunk.data_hex,
                "{} chunk {} ciphertext",
                file.name,
                chunk.index
            );
            assert_eq!(sealed.len(), file_cipher.sealed_len(expected_len));

            let opened = file_cipher
                .open_chunk(chunk.index, &sealed, expected_len)
                .expect("opens");
            assert_eq!(
                opened, plain,
                "{} chunk {} roundtrip",
                file.name, chunk.index
            );

            if let Some(nonce_base) = file_cipher.nonce_base() {
                assert_eq!(
                    hex::encode(build_nonce(nonce_base, chunk.index)),
                    chunk.nonce_hex
                );
            }
        }
    }
}

#[test]
fn descriptions_seal_and_open() {
    if is_generating() {
        return;
    }
    let vectors = load_vectors();
    for file in &vectors.files {
        let file_cipher = file_cipher_of(file);
        for (slot, json, data_hex) in [
            (
                DescriptionSlot::Create,
                &file.create_description_json,
                &file.create_description_data_hex,
            ),
            (
                DescriptionSlot::Seal,
                &file.seal_description_json,
                &file.seal_description_data_hex,
            ),
        ] {
            let sealed = file_cipher.seal_description(slot, json.as_bytes());
            assert_eq!(
                hex::encode(&sealed),
                *data_hex,
                "{} {:?} description",
                file.name,
                slot
            );
            let opened = file_cipher.open_description(slot, &sealed).expect("opens");
            assert_eq!(opened, json.as_bytes());
            let description =
                ChunkedBlock::from_description_json(&opened).expect("valid description");
            assert!(
                description.key.is_none(),
                "descriptions never carry the key"
            );
        }
        let event_block: ChunkedBlock =
            serde_json::from_str(&file.event_block_json).expect("event block parses");
        event_block
            .check_as_event_block()
            .expect("event block valid");
        let seal_description =
            ChunkedBlock::from_description_json(file.seal_description_json.as_bytes())
                .expect("valid");
        let create_description =
            ChunkedBlock::from_description_json(file.create_description_json.as_bytes())
                .expect("valid");
        assert!(event_block.is_consistent_with_description(&seal_description));
        assert!(event_block.is_consistent_with_description(&create_description));
        assert_eq!(
            event_block.to_description_json(),
            file.seal_description_json.as_bytes()
        );
        assert_eq!(
            FileCipher::from_event_block(&event_block).expect("valid"),
            file_cipher
        );
        if file_cipher.cipher.is_encrypting() {
            assert!(file.event_block_json.contains("\"key\""));
            assert!(!file.seal_description_json.contains("\"key\""));
        } else {
            assert!(!file.event_block_json.contains("\"key\""));
            assert!(!file.event_block_json.contains("nonce_base"));
        }
    }
}

#[test]
fn seek_vectors() {
    if is_generating() {
        return;
    }
    let vectors = load_vectors();
    for vector in &vectors.seek {
        let target = locate(vector.pos, vector.chunk_size).expect("in range");
        assert_eq!(
            (target.index, target.offset),
            (vector.index, vector.offset),
            "pos {}",
            vector.pos
        );
        assert_eq!(
            u64::from(target.index) * u64::from(vector.chunk_size) + target.offset as u64,
            vector.pos
        );
    }
}

#[test]
fn rejected_block_vectors() {
    if is_generating() {
        return;
    }
    let vectors = load_vectors();
    assert!(!vectors.rejected_blocks.is_empty());
    for vector in &vectors.rejected_blocks {
        let parsed: Result<ChunkedBlock, _> = serde_json::from_str(&vector.json);
        let Ok(block) = parsed else {
            assert_eq!(vector.error, "ParseError", "{}", vector.name);
            continue;
        };
        let result = match vector.checked_as.as_str() {
            "event_block" => block.check_as_event_block(),
            "description" => block.check_as_description(),
            other => panic!("unknown checked_as {other}"),
        };
        let error = result.expect_err(&format!("{} must be rejected", vector.name));
        let variant = format!("{error:?}");
        let variant = variant.split('(').next().expect("non-empty");
        assert_eq!(variant, vector.error, "{}", vector.name);
    }
}

//! 對著 `docs/design/wbf-vectors.json`（從 server repo 複製）跑。
//! 這裡紅 = 這個 crate 與 server 的線上格式漂移了，或複製的向量檔過期了。
//! 規格書 §11 寫了 client 該驗什麼；每個段落一個測試。

use serde::Deserialize;
use wbf_wire::{crc32c, DecodeError, EncodeError, EncryptedFileInfo, Kind, Pack};

const VECTORS_JSON: &str = include_str!("../../../docs/design/wbf-vectors.json");

#[derive(Deserialize)]
struct Vectors {
    format_version: u32,
    crc32c: Vec<Crc32cVector>,
    encrypted_file_info: Vec<EncryptedFileInfoVector>,
    packs: Vec<PackVector>,
    rejected: Vec<RejectedVector>,
}

#[derive(Deserialize)]
struct Crc32cVector {
    input_hex: String,
    crc: u32,
}

#[derive(Deserialize)]
struct EncryptedFileInfoVector {
    bytes_hex: String,
    file_size: u64,
    chunk_size: u32,
    chunk_count: u32,
}

#[derive(Deserialize)]
struct PackVector {
    name: String,
    bytes_hex: String,
    kind: u8,
    subtype: u8,
    flags: u8,
    id: u64,
    seq: u32,
    meta_hex: String,
    data_hex: String,
}

#[derive(Deserialize)]
struct RejectedVector {
    name: String,
    bytes_hex: String,
    error: String,
}

fn load_vectors() -> Vectors {
    let vectors: Vectors = serde_json::from_str(VECTORS_JSON).expect("wbf-vectors.json parses");
    assert_eq!(
        vectors.format_version, 1,
        "vector file format changed; update this test"
    );
    vectors
}

fn unhex(hex_text: &str) -> Vec<u8> {
    hex::decode(hex_text).expect("valid hex in vectors")
}

#[test]
fn crc32c_vectors() {
    let vectors = load_vectors();
    assert!(!vectors.crc32c.is_empty());
    for vector in &vectors.crc32c {
        assert_eq!(
            crc32c(&unhex(&vector.input_hex)),
            vector.crc,
            "input {}",
            vector.input_hex
        );
    }
    // 規格 §9 的自檢向量，寫死一份以免向量檔本身被改壞。
    assert_eq!(crc32c(b"123456789"), 0xE306_9283);
}

#[test]
fn encrypted_file_info_round_trips() {
    let vectors = load_vectors();
    assert!(!vectors.encrypted_file_info.is_empty());
    for vector in &vectors.encrypted_file_info {
        let bytes = unhex(&vector.bytes_hex);
        let expected = EncryptedFileInfo {
            file_size: vector.file_size,
            chunk_size: vector.chunk_size,
            chunk_count: vector.chunk_count,
        };
        assert_eq!(
            EncryptedFileInfo::from_bytes(&bytes),
            Some(expected),
            "{}",
            vector.bytes_hex
        );
        assert_eq!(
            expected.to_bytes().as_slice(),
            bytes.as_slice(),
            "{}",
            vector.bytes_hex
        );
    }
}

#[test]
fn packs_decode_and_re_encode_identically() {
    let vectors = load_vectors();
    assert!(!vectors.packs.is_empty());
    for vector in &vectors.packs {
        let bytes = unhex(&vector.bytes_hex);
        let kind = Kind::from_byte(vector.kind)
            .unwrap_or_else(|| panic!("{}: kind {}", vector.name, vector.kind));
        let expected = Pack {
            kind,
            subtype: vector.subtype,
            flags: vector.flags,
            id: vector.id,
            seq: vector.seq,
            meta: unhex(&vector.meta_hex),
            data: unhex(&vector.data_hex),
        };
        let decoded =
            Pack::decode(&bytes).unwrap_or_else(|error| panic!("{}: {error}", vector.name));
        assert_eq!(decoded, expected, "{}", vector.name);
        assert_eq!(
            expected.encode().expect("fits u32"),
            bytes,
            "{}: re-encode",
            vector.name
        );
    }
}

#[test]
fn rejected_packs_fail_with_the_named_error() {
    let vectors = load_vectors();
    assert!(!vectors.rejected.is_empty());
    for vector in &vectors.rejected {
        let error: DecodeError = match Pack::decode(&unhex(&vector.bytes_hex)) {
            Err(error) => error,
            Ok(pack) => panic!(
                "{}: decoded {pack:?}, expected {}",
                vector.name, vector.error
            ),
        };
        assert_eq!(error.name(), vector.error, "{}: got {error}", vector.name);
    }
}

#[test]
fn encrypted_file_info_rejects_wrong_length() {
    assert_eq!(EncryptedFileInfo::from_bytes(&[0u8; 15]), None);
    assert_eq!(EncryptedFileInfo::from_bytes(&[0u8; 17]), None);
}

#[test]
fn encode_rejects_reserved_flags() {
    // 與 rejected[reserved_flag] 對稱：decode 會拒的 flags，encode 也不該做出來。
    let pack = Pack {
        kind: Kind::Upload,
        subtype: wbf_wire::pack::upload::CHUNK,
        flags: 0x80,
        id: 0x1122_3344_5566_7788,
        seq: 0,
        meta: Vec::new(),
        data: vec![0xde, 0xad, 0xbe, 0xef, 0x01],
    };
    assert_eq!(pack.encode(), Err(EncodeError::ReservedFlags(0x80)));
}

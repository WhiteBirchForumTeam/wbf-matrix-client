//! 每塊與描述的加解密、塊的長度規則、seek 的算法、`chunk_size` 的選法（約定 §2、§3、§4、§7）。
//!
//! 所有檢查 fail closed：長度不對、索引保留、標籤不對，一律 `Err`，不回部分內容。

use crate::chunk_block::{BlockError, ChunkedBlock, NONCE_BASE_LEN};
use crate::cipher::{Cipher, KEY_LEN, NONCE_LEN, TAG_LEN};

/// 塊密文的 AAD（約定 §3）。
pub const CHUNK_AAD: &[u8] = b"wbf-chunk-v1";
/// 描述密文的 AAD（約定 §4）。
pub const DESCRIPTION_AAD: &[u8] = b"wbf-desc-v1";
/// 塊索引的上限；再上去是描述保留的兩個 nonce（約定 §3）。
pub const MAX_CHUNK_INDEX: u32 = 0xFFFF_FFFD;
/// `Create` 描述用的保留索引。
pub const CREATE_DESCRIPTION_INDEX: u32 = 0xFFFF_FFFF;
/// `Seal` 描述用的保留索引。
pub const SEAL_DESCRIPTION_INDEX: u32 = 0xFFFF_FFFE;

/// 約定 §2 的表：依明文大小選 `chunk_size` 的門檻。
pub const LARGE_FILE_THRESHOLD: u64 = 50 * 1024 * 1024;
pub const SMALL_CHUNK_SIZE: u32 = 64 * 1024;
pub const LARGE_CHUNK_SIZE: u32 = 1024 * 1024;

/// 描述有兩份，各自一個 nonce（約定 §4）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DescriptionSlot {
    Create,
    Seal,
}

impl DescriptionSlot {
    /// Return:
    ///     u32  `Create` = 0xFFFF_FFFF，`Seal` = 0xFFFF_FFFE
    pub fn nonce_index(self) -> u32 {
        match self {
            DescriptionSlot::Create => CREATE_DESCRIPTION_INDEX,
            DescriptionSlot::Seal => SEAL_DESCRIPTION_INDEX,
        }
    }
}

/// 串流上傳看的是線路（約定 §2）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Link {
    /// 行動網路、或判斷不出來。
    MobileOrUnknown,
    WifiOrWired,
}

/// 塊或描述解不開的原因。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CryptoError {
    /// 索引 > `MAX_CHUNK_INDEX`。
    IndexReserved(u32),
    /// `Read` 回來的長度不是預期（約定 §3.1 第 3 條）。
    LengthMismatch { expected: usize, actual: usize },
    /// AEAD 標籤驗證失敗（約定 §3.1 第 4 條）。
    TagInvalid,
    /// 明文長度超過 `chunk_size`（上傳端塞錯）。
    ChunkTooLong { chunk_size: u32, actual: usize },
}

impl std::fmt::Display for CryptoError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CryptoError::IndexReserved(index) => {
                write!(formatter, "chunk index {index:#x} is reserved")
            }
            CryptoError::LengthMismatch { expected, actual } => {
                write!(formatter, "chunk length {actual}, expected {expected}")
            }
            CryptoError::TagInvalid => write!(formatter, "AEAD tag invalid"),
            CryptoError::ChunkTooLong { chunk_size, actual } => {
                write!(
                    formatter,
                    "plaintext chunk {actual} bytes exceeds chunk_size {chunk_size}"
                )
            }
        }
    }
}

impl std::error::Error for CryptoError {}

/// 一個檔的加密參數：`cipher`、`key`、`nonce_base`、`chunk_size`。明文模式沒有 key 與 nonce_base。
/// 從事件區塊來（下載）或隨機產生（上傳）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileCipher {
    pub cipher: Cipher,
    key: Option<[u8; KEY_LEN]>,
    nonce_base: Option<[u8; NONCE_BASE_LEN]>,
    pub chunk_size: u32,
}

impl FileCipher {
    /// 下載端：從通過 `check_as_event_block` 的區塊取參數。
    ///
    /// Args:
    ///     block: 事件區塊
    /// Return:
    ///     Ok(FileCipher)
    ///     Err(BlockError)   `check_as_event_block` 不過
    pub fn from_event_block(block: &ChunkedBlock) -> Result<FileCipher, BlockError> {
        block.check_as_event_block()?;
        Ok(FileCipher {
            cipher: block.cipher,
            key: block.key,
            nonce_base: block.nonce_base,
            chunk_size: block.chunk_size,
        })
    }

    /// 上傳端：新的一檔，`key` 與 `nonce_base` 由 CSPRNG 產生。同一個檔重傳要再叫一次（約定 §2）。
    /// 明文模式（`Cipher::None`）兩者都是 None。
    ///
    /// Args:
    ///     cipher: example: Cipher::default_for_this_machine()
    ///     chunk_size: example: choose_chunk_size(file_size)
    /// Return:
    ///     FileCipher
    pub fn generate(cipher: Cipher, chunk_size: u32) -> FileCipher {
        if !cipher.is_encrypting() {
            return FileCipher {
                cipher,
                key: None,
                nonce_base: None,
                chunk_size,
            };
        }
        let mut key = [0u8; KEY_LEN];
        let mut nonce_base = [0u8; NONCE_BASE_LEN];
        getrandom::getrandom(&mut key).expect("OS CSPRNG available");
        getrandom::getrandom(&mut nonce_base).expect("OS CSPRNG available");
        FileCipher {
            cipher,
            key: Some(key),
            nonce_base: Some(nonce_base),
            chunk_size,
        }
    }

    /// 測試與向量用：固定的參數。
    pub fn with_fixed(
        cipher: Cipher,
        key: [u8; KEY_LEN],
        nonce_base: [u8; NONCE_BASE_LEN],
        chunk_size: u32,
    ) -> FileCipher {
        if cipher.is_encrypting() {
            FileCipher {
                cipher,
                key: Some(key),
                nonce_base: Some(nonce_base),
                chunk_size,
            }
        } else {
            FileCipher {
                cipher,
                key: None,
                nonce_base: None,
                chunk_size,
            }
        }
    }

    /// 上傳端：把參數寫成事件區塊（約定 §5），其餘欄位呼叫者填。
    ///
    /// Args:
    ///     file_size: 明文總長（事件在 `Seal` 之後才送，一定知道）
    /// Return:
    ///     ChunkedBlock  `name`／`mimetype`／`sha256` 都是 None
    pub fn to_event_block(&self, file_size: u64) -> ChunkedBlock {
        ChunkedBlock {
            v: crate::chunk_block::CONVENTION_V,
            cipher: self.cipher,
            key: self.key,
            nonce_base: self.nonce_base,
            chunk_size: self.chunk_size,
            file_size: Some(file_size),
            name: None,
            mimetype: None,
            sha256: None,
        }
    }

    /// Return:
    ///     Option<[u8; 8]>  加密模式有，明文模式 None
    pub fn nonce_base(&self) -> Option<[u8; NONCE_BASE_LEN]> {
        self.nonce_base
    }

    /// 約定 §3：`ct_i = AEAD(key, nonce_base ‖ u32_be(i), "wbf-chunk-v1", pt_i)`。明文模式回 `plain` 的複本。
    ///
    /// Args:
    ///     index: 塊索引，0 起, example: 0
    ///     plain: 明文塊，長度 ≤ chunk_size, example: b"hello"
    /// Return:
    ///     Ok(Vec<u8>)        密文 ‖ 標籤（明文模式：就是明文）
    ///     Err(CryptoError)   `IndexReserved`、`ChunkTooLong`
    pub fn seal_chunk(&self, index: u32, plain: &[u8]) -> Result<Vec<u8>, CryptoError> {
        if index > MAX_CHUNK_INDEX {
            return Err(CryptoError::IndexReserved(index));
        }
        if plain.len() > self.chunk_size as usize {
            return Err(CryptoError::ChunkTooLong {
                chunk_size: self.chunk_size,
                actual: plain.len(),
            });
        }
        Ok(self.seal_with_index(index, CHUNK_AAD, plain))
    }

    /// 約定 §3.1 第 3、4 條：先驗長度，再驗標籤。長度不對就不碰密碼。
    ///
    /// Args:
    ///     index: 塊索引, example: 0
    ///     sealed: `Read` 回來的 data
    ///     expected_plain_len: `expected_plain_len(file_size, chunk_size, index)` 的結果, example: 65536
    /// Return:
    ///     Ok(Vec<u8>)        明文塊，長度剛好 `expected_plain_len`
    ///     Err(CryptoError)   `IndexReserved`、`LengthMismatch`、`TagInvalid`
    pub fn open_chunk(
        &self,
        index: u32,
        sealed: &[u8],
        expected_plain_len: usize,
    ) -> Result<Vec<u8>, CryptoError> {
        if index > MAX_CHUNK_INDEX {
            return Err(CryptoError::IndexReserved(index));
        }
        let expected_sealed_len = self.sealed_len(expected_plain_len);
        if sealed.len() != expected_sealed_len {
            return Err(CryptoError::LengthMismatch {
                expected: expected_sealed_len,
                actual: sealed.len(),
            });
        }
        let plain = self
            .open_with_index(index, CHUNK_AAD, sealed)
            .ok_or(CryptoError::TagInvalid)?;
        if plain.len() != expected_plain_len {
            // 長度驗過了，AEAD 不會改長度；留這條是不靠巧合。
            return Err(CryptoError::LengthMismatch {
                expected: expected_plain_len,
                actual: plain.len(),
            });
        }
        Ok(plain)
    }

    /// 約定 §4：描述用保留索引的 nonce 與 `"wbf-desc-v1"` 加密。明文模式直接回 JSON。
    ///
    /// Args:
    ///     slot: `Create` 或 `Seal`
    ///     description_json: `ChunkedBlock::to_description_json()` 的結果
    /// Return:
    ///     Vec<u8>  `Create`／`Seal` 的 data
    pub fn seal_description(&self, slot: DescriptionSlot, description_json: &[u8]) -> Vec<u8> {
        self.seal_with_index(slot.nonce_index(), DESCRIPTION_AAD, description_json)
    }

    /// Args:
    ///     slot: `Info` 還回的是 `Seal` 那份（server 整份覆蓋）；只在沒 `Seal` 過時才是 `Create`
    ///     data: `Info` 還回的描述
    /// Return:
    ///     Ok(Vec<u8>)        描述 JSON bytes
    ///     Err(CryptoError)   `TagInvalid`（含 data 短於 16 byte）
    pub fn open_description(
        &self,
        slot: DescriptionSlot,
        data: &[u8],
    ) -> Result<Vec<u8>, CryptoError> {
        self.open_with_index(slot.nonce_index(), DESCRIPTION_AAD, data)
            .ok_or(CryptoError::TagInvalid)
    }

    /// Args:
    ///     plain_len: 明文塊長度
    /// Return:
    ///     usize  加密模式 `plain_len + 16`，明文模式 `plain_len`
    pub fn sealed_len(&self, plain_len: usize) -> usize {
        if self.cipher.is_encrypting() {
            plain_len + TAG_LEN
        } else {
            plain_len
        }
    }

    fn seal_with_index(&self, index: u32, aad: &[u8], plain: &[u8]) -> Vec<u8> {
        let (Some(key), Some(nonce_base)) = (self.key, self.nonce_base) else {
            return plain.to_vec();
        };
        self.cipher
            .seal(&key, &build_nonce(nonce_base, index), aad, plain)
    }

    fn open_with_index(&self, index: u32, aad: &[u8], sealed: &[u8]) -> Option<Vec<u8>> {
        let (Some(key), Some(nonce_base)) = (self.key, self.nonce_base) else {
            return Some(sealed.to_vec());
        };
        self.cipher
            .open(&key, &build_nonce(nonce_base, index), aad, sealed)
    }
}

/// 約定 §3：`nonce_i = nonce_base ‖ u32_be(i)`。
///
/// Args:
///     nonce_base: 8 byte, example: [0, 1, 2, 3, 4, 5, 6, 7]
///     index: example: 1
/// Return:
///     [u8; 12]  example: [0, 1, 2, 3, 4, 5, 6, 7, 0, 0, 0, 1]
pub fn build_nonce(nonce_base: [u8; NONCE_BASE_LEN], index: u32) -> [u8; NONCE_LEN] {
    let mut nonce = [0u8; NONCE_LEN];
    nonce[..NONCE_BASE_LEN].copy_from_slice(&nonce_base);
    nonce[NONCE_BASE_LEN..].copy_from_slice(&index.to_be_bytes());
    nonce
}

/// 約定 §3.1 第 2 條：`chunk_count == ceil(file_size / chunk_size)`。
///
/// Args:
///     file_size: example: 132056
///     chunk_size: example: 65536
/// Return:
///     Some(u32)  example: 3；`file_size` 0 回 Some(0)
///     None       `chunk_size` 0，或塊數超過 `MAX_CHUNK_INDEX + 1`
pub fn chunk_count(file_size: u64, chunk_size: u32) -> Option<u32> {
    if chunk_size == 0 {
        return None;
    }
    let count = file_size.div_ceil(u64::from(chunk_size));
    if count > u64::from(MAX_CHUNK_INDEX) + 1 {
        return None;
    }
    Some(count as u32)
}

/// 約定 §3.1 第 3 條：非最後一塊 = `chunk_size`；最後一塊 = `file_size − i × chunk_size`。
///
/// Args:
///     file_size: example: 132056
///     chunk_size: example: 65536
///     index: example: 2
/// Return:
///     Some(usize)  example: 984
///     None         `index` 不小於塊數、或 `chunk_size` 0
pub fn expected_plain_len(file_size: u64, chunk_size: u32, index: u32) -> Option<usize> {
    let count = chunk_count(file_size, chunk_size)?;
    if index >= count {
        return None;
    }
    let start = u64::from(index) * u64::from(chunk_size);
    let remaining = file_size - start;
    Some(remaining.min(u64::from(chunk_size)) as usize)
}

/// 約定 §7：seek 的結果，只要讀 `index` 那一塊，明文從 `offset` 起。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SeekTarget {
    pub index: u32,
    pub offset: usize,
}

/// 約定 §7：`i = pos / chunk_size`，`off = pos − i × chunk_size`。不管 `pos` 有沒有超過檔尾，
/// 那是呼叫者拿 `file_size` 先擋的事（CLI 規格 §3.3.1：`--at` 不小於總長是用法錯）。
///
/// Args:
///     pos: 明文位置, example: 71680
///     chunk_size: example: 65536
/// Return:
///     Some(SeekTarget)  example: SeekTarget { index: 1, offset: 6144 }
///     None              `chunk_size` 0，或索引超過 `MAX_CHUNK_INDEX`
pub fn locate(pos: u64, chunk_size: u32) -> Option<SeekTarget> {
    if chunk_size == 0 {
        return None;
    }
    let index = pos / u64::from(chunk_size);
    if index > u64::from(MAX_CHUNK_INDEX) {
        return None;
    }
    Some(SeekTarget {
        index: index as u32,
        offset: (pos % u64::from(chunk_size)) as usize,
    })
}

/// 約定 §2 的表：固定大小上傳依明文大小選。
///
/// Args:
///     file_size: example: 1024
/// Return:
///     u32  < 50 MiB 回 65536，否則 1048576
pub fn choose_chunk_size(file_size: u64) -> u32 {
    if file_size < LARGE_FILE_THRESHOLD {
        SMALL_CHUNK_SIZE
    } else {
        LARGE_CHUNK_SIZE
    }
}

/// 約定 §2 的表：串流上傳依線路選。
///
/// Args:
///     link: example: Link::MobileOrUnknown
/// Return:
///     u32  `MobileOrUnknown` 回 65536，`WifiOrWired` 回 1048576
pub fn choose_stream_chunk_size(link: Link) -> u32 {
    match link {
        Link::MobileOrUnknown => SMALL_CHUNK_SIZE,
        Link::WifiOrWired => LARGE_CHUNK_SIZE,
    }
}

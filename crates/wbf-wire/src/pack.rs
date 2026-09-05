//! pack：固定 32 byte 外框 + meta + data，兩段各一個 CRC-32C（規格 §2）。
//!
//! 版面（所有整數 big-endian）：
//! ```text
//! 0   version(1) kind(1) subtype(1) flags(1)
//! 4   id(8)
//! 12  seq(4)
//! 16  meta_len(4)
//! 20  meta(n)
//! 20+n  meta_crc(4)   蓋 0..20+n
//! 24+n  data_len(4)
//! 28+n  data(m)
//! 28+n+m  data_crc(4) 只蓋 data；空 data 時是 0
//! ```

use crate::crc32c;

/// 線上格式版本，目前只有 1。
pub const VERSION: u8 = 1;

/// meta 與 data 都空時的 pack 長度，也是任何 pack 的最小長度。
pub const MIN_PACK_LEN: usize = 32;

/// `kind` 欄位。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Kind {
    Control = 0x01,
    Upload = 0x03,
    Download = 0x04,
    /// 房間事件的領域（wire-format §3.3）：目前 `Recent`、`Send`。
    Event = 0x14,
}

impl Kind {
    /// Args:
    ///     byte: 線上的 kind, example: 0x03
    /// Return:
    ///     Some(Kind)  認得
    ///     None        不認得（解碼時是 `DecodeError::UnknownKind`）
    pub fn from_byte(byte: u8) -> Option<Kind> {
        match byte {
            0x01 => Some(Kind::Control),
            0x03 => Some(Kind::Upload),
            0x04 => Some(Kind::Download),
            0x14 => Some(Kind::Event),
            _ => None,
        }
    }
}

/// `Kind::Control` 的 subtype。
pub mod control {
    pub const HELLO: u8 = 0x01;
    pub const ACK: u8 = 0x02;
    pub const ERROR: u8 = 0x03;
    pub const PING: u8 = 0x04;
    pub const PONG: u8 = 0x05;
}

/// `Kind::Upload` 的 subtype。
pub mod upload {
    pub const CREATE: u8 = 0x01;
    pub const CHUNK: u8 = 0x02;
    pub const STATUS: u8 = 0x03;
    pub const SEAL: u8 = 0x04;
    pub const ABORT: u8 = 0x05;
}

/// `Kind::Download` 的 subtype。
pub mod download {
    pub const INFO: u8 = 0x01;
    pub const READ: u8 = 0x02;
}

/// `Kind::Event` 的 subtype（server 的 room-seq-and-recent.md §2、media-attachments.md §3）。
pub mod event {
    /// 跨房間「在 `cg_seq` 之後的事件」。
    pub const RECENT: u8 = 0x01;
    /// 送事件，meta 帶 `attachments` 宣告附件。
    pub const SEND: u8 = 0x02;
}

/// `flags` 欄位的位元。沒列的位元必須是 0。
pub mod flags {
    pub const META_ENCRYPTED: u8 = 0x01;
    pub const WANT_ACK: u8 = 0x02;
    pub const IS_RESPONSE: u8 = 0x04;
    pub const IS_LAST: u8 = 0x08;
    /// 所有定義過的位元；`flags & !KNOWN != 0` 就是 `ReservedFlags`。
    pub const KNOWN: u8 = META_ENCRYPTED | WANT_ACK | IS_RESPONSE | IS_LAST;
}

/// 一個 pack 的欄位。`encode` 與 `decode` 互為反函數（向量測試保證）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pack {
    pub kind: Kind,
    pub subtype: u8,
    pub flags: u8,
    /// 上傳 id；`Create` 與所有 Download 請求為 0。
    pub id: u64,
    /// `Chunk`：塊索引（0 起）；其他請求：請求號，回應抄回。
    pub seq: u32,
    pub meta: Vec<u8>,
    pub data: Vec<u8>,
}

/// 解碼失敗的類別。名字與 `wbf-vectors.json` 的 `rejected[].error` 一致。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DecodeError {
    /// 連 32 byte 的最小外框都不到。
    TooShort,
    UnsupportedVersion(u8),
    UnknownKind(u8),
    ReservedFlags(u8),
    MetaCrc {
        expected: u32,
        actual: u32,
    },
    DataCrc {
        expected: u32,
        actual: u32,
    },
    /// `meta_len`／`data_len` 說的長度超出實際 bytes。
    Truncated,
    /// `data_crc` 之後還有 bytes。
    TrailingBytes,
}

impl DecodeError {
    /// Return:
    ///     &str  向量檔用的類別名, example: "MetaCrc"
    pub fn name(&self) -> &'static str {
        match self {
            DecodeError::TooShort => "TooShort",
            DecodeError::UnsupportedVersion(_) => "UnsupportedVersion",
            DecodeError::UnknownKind(_) => "UnknownKind",
            DecodeError::ReservedFlags(_) => "ReservedFlags",
            DecodeError::MetaCrc { .. } => "MetaCrc",
            DecodeError::DataCrc { .. } => "DataCrc",
            DecodeError::Truncated => "Truncated",
            DecodeError::TrailingBytes => "TrailingBytes",
        }
    }
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DecodeError::UnsupportedVersion(version) => {
                write!(out, "UnsupportedVersion({version})")
            }
            DecodeError::UnknownKind(kind) => write!(out, "UnknownKind({kind:#04x})"),
            DecodeError::ReservedFlags(flags) => write!(out, "ReservedFlags({flags:#04x})"),
            DecodeError::MetaCrc { expected, actual }
            | DecodeError::DataCrc { expected, actual } => {
                write!(
                    out,
                    "{}(expected {expected:#010x}, actual {actual:#010x})",
                    self.name()
                )
            }
            other => out.write_str(other.name()),
        }
    }
}

impl std::error::Error for DecodeError {}

/// 編碼失敗：長度欄位是 u32，放不下的段拒絕編碼。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EncodeError {
    MetaTooLong(usize),
    DataTooLong(usize),
    /// `flags` 帶了沒定義的位元；對方 decode 會拒（`DecodeError::ReservedFlags`），所以這邊先擋。
    ReservedFlags(u8),
}

impl std::fmt::Display for EncodeError {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EncodeError::MetaTooLong(len) => write!(out, "meta is {len} bytes, more than u32"),
            EncodeError::DataTooLong(len) => write!(out, "data is {len} bytes, more than u32"),
            EncodeError::ReservedFlags(flags) => {
                write!(out, "reserved flag bits set: {flags:#04x}")
            }
        }
    }
}

impl std::error::Error for EncodeError {}

impl Pack {
    /// 驗的與 `decode` 一樣多：flags 的保留位元擋，subtype 對不對 kind 不擋（兩邊都不擋，那是組包層的事）。
    ///
    /// Return:
    ///     Ok(Vec<u8>)        線上 bytes，長度 = 32 + meta.len() + data.len()
    ///     Err(EncodeError)   meta 或 data 超過 u32，或 flags 帶了保留位元
    pub fn encode(&self) -> Result<Vec<u8>, EncodeError> {
        if self.flags & !flags::KNOWN != 0 {
            return Err(EncodeError::ReservedFlags(self.flags));
        }
        let meta_len = u32::try_from(self.meta.len())
            .map_err(|_| EncodeError::MetaTooLong(self.meta.len()))?;
        let data_len = u32::try_from(self.data.len())
            .map_err(|_| EncodeError::DataTooLong(self.data.len()))?;

        let mut bytes = Vec::with_capacity(MIN_PACK_LEN + self.meta.len() + self.data.len());
        bytes.extend_from_slice(&[VERSION, self.kind as u8, self.subtype, self.flags]);
        bytes.extend_from_slice(&self.id.to_be_bytes());
        bytes.extend_from_slice(&self.seq.to_be_bytes());
        bytes.extend_from_slice(&meta_len.to_be_bytes());
        bytes.extend_from_slice(&self.meta);
        let meta_crc = crc32c(&bytes);
        bytes.extend_from_slice(&meta_crc.to_be_bytes());
        bytes.extend_from_slice(&data_len.to_be_bytes());
        bytes.extend_from_slice(&self.data);
        bytes.extend_from_slice(&crc32c(&self.data).to_be_bytes());
        Ok(bytes)
    }

    /// 檢查順序固定：長度、version、kind、flags、meta 段（長度再 CRC）、data 段（長度再 CRC）、尾巴。
    /// 先驗標頭欄位再驗 CRC，所以 version／kind／flags 錯的 pack 回的是那個錯，不是 `MetaCrc`。
    ///
    /// Args:
    ///     bytes: 一整個 pack（WebSocket 一個 binary message）, example: 見 `docs/design/wbf-vectors.json`
    /// Return:
    ///     Ok(Pack)
    ///     Err(DecodeError)  類別見 `DecodeError`；任何一項不過就拒絕，不會回半個 pack
    pub fn decode(bytes: &[u8]) -> Result<Pack, DecodeError> {
        if bytes.len() < MIN_PACK_LEN {
            return Err(DecodeError::TooShort);
        }
        if bytes[0] != VERSION {
            return Err(DecodeError::UnsupportedVersion(bytes[0]));
        }
        let kind = Kind::from_byte(bytes[1]).ok_or(DecodeError::UnknownKind(bytes[1]))?;
        let subtype = bytes[2];
        let flags = bytes[3];
        if flags & !flags::KNOWN != 0 {
            return Err(DecodeError::ReservedFlags(flags));
        }
        let id = u64::from_be_bytes(bytes[4..12].try_into().expect("8 bytes"));
        let seq = u32::from_be_bytes(bytes[12..16].try_into().expect("4 bytes"));

        let meta_len = read_len(bytes, 16);
        let meta_end = 20usize
            .checked_add(meta_len)
            .ok_or(DecodeError::Truncated)?;
        // meta 之後至少還要 meta_crc(4) + data_len(4)。
        if meta_end.checked_add(8).ok_or(DecodeError::Truncated)? > bytes.len() {
            return Err(DecodeError::Truncated);
        }
        let meta_crc_expected = read_u32(bytes, meta_end);
        let meta_crc_actual = crc32c(&bytes[..meta_end]);
        if meta_crc_actual != meta_crc_expected {
            return Err(DecodeError::MetaCrc {
                expected: meta_crc_expected,
                actual: meta_crc_actual,
            });
        }

        let data_len = read_len(bytes, meta_end + 4);
        let data_start = meta_end + 8;
        let data_end = data_start
            .checked_add(data_len)
            .ok_or(DecodeError::Truncated)?;
        let pack_end = data_end.checked_add(4).ok_or(DecodeError::Truncated)?;
        if pack_end > bytes.len() {
            return Err(DecodeError::Truncated);
        }
        let data = &bytes[data_start..data_end];
        let data_crc_expected = read_u32(bytes, data_end);
        let data_crc_actual = crc32c(data);
        if data_crc_actual != data_crc_expected {
            return Err(DecodeError::DataCrc {
                expected: data_crc_expected,
                actual: data_crc_actual,
            });
        }
        if pack_end != bytes.len() {
            return Err(DecodeError::TrailingBytes);
        }

        Ok(Pack {
            kind,
            subtype,
            flags,
            id,
            seq,
            meta: bytes[20..meta_end].to_vec(),
            data: data.to_vec(),
        })
    }
}

/// 呼叫者要先確認 `bytes.len() >= at + 4`。
fn read_u32(bytes: &[u8], at: usize) -> u32 {
    debug_assert!(
        bytes.len() >= at + 4,
        "caller must bounds-check before read_u32"
    );
    u32::from_be_bytes(bytes[at..at + 4].try_into().expect("4 bytes"))
}

fn read_len(bytes: &[u8], at: usize) -> usize {
    // u32 → usize 在 32-bit 目標上也不會截斷。
    read_u32(bytes, at) as usize
}

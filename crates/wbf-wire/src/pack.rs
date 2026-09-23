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
    /// 串流訊息的草稿（wire-format §3）：`Draft`、`Keypoint`、`Append`……client 這邊還沒用，先認得它才能解 server 的向量。
    Stream = 0x02,
    Upload = 0x03,
    Download = 0x04,
    /// 連線背後的 session（wire-format §6.3）：`Login`、`Refresh`、`Logout`。client 這邊還沒用，先認得它才能解 server 的向量。
    Session = 0x10,
    /// 帳號（bridge-specs `0x11-account.md`）：沒有原生的 pack，全是走橋的 Matrix 端點（profile、account data）。
    Account = 0x11,
    /// 房間（wire-format §3.3）：沒有原生的 pack，全是走橋的 Matrix 端點（`Members` 等，bridge-specs `0x13-room.md`）。
    Room = 0x13,
    /// 房間事件的領域（wire-format §3.3）：`Recent`、`Send`、`Batch`。
    Event = 0x14,
    /// to-device（wire-format §3.2；to-device-client.md）：`Fetch`、`Batch`、`ItemsDestroy`、`Subscribe`……client 這邊還沒接。
    Device = 0x16,
    /// E2EE 的金鑰（wire-format §3.3）：沒有原生的 pack，全是走橋的 `/keys/*` 與 `/room_keys/*`（bridge-specs `0x17-keys.md`）。
    Keys = 0x17,
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
            0x02 => Some(Kind::Stream),
            0x03 => Some(Kind::Upload),
            0x04 => Some(Kind::Download),
            0x10 => Some(Kind::Session),
            0x11 => Some(Kind::Account),
            0x13 => Some(Kind::Room),
            0x14 => Some(Kind::Event),
            0x16 => Some(Kind::Device),
            0x17 => Some(Kind::Keys),
            _ => None,
        }
    }
}

/// `id` 欄位（wire-format §2.2，wbfuwunel PR #46）：`[id_type 1 byte] ‖ [值 7 byte 大端]`。
///
/// 一個 id 自己就說得出它是什麼。server 驗型別跟 `(kind, subtype)` 對不對得上，不符 → `InvalidRequest`；
/// 需要會話的包（`Event/Recent`、`Subscribe`、`Device/*`）填 0 也是 `InvalidRequest`。
/// server 鑄的 id（上傳 id、`g_seq`）回來時已經組好，client 原樣抄回去；**只有 client 自己鑄的會話號要經過 [`id::compose`]**。
pub mod id {
    /// 沒有會話：整個 id 必須是 0（`Hello`、`Ping`、`Download/*`、`Upload/Create`）。
    pub const NONE: u8 = 0x00;
    /// client 自己挑的會話號（`Event/Recent`、`Event/Subscribe`、`Device/*`）。
    pub const SESSION: u8 = 0x01;
    /// 事件位置 `g_seq`（`Stream/*` 草稿的錨）。
    pub const G_SEQ: u8 = 0x02;
    /// 上傳 id（`Upload/Chunk`／`Status`／`Seal`／`Abort`）。去掉型別 byte 就是 mxc 的 media id。
    pub const UPLOAD: u8 = 0x03;

    /// 值只有 56 bit。
    pub const MAX_VALUE: u64 = (1 << 56) - 1;

    /// 組一個 id。
    ///
    /// Args:
    ///     id_type: example: id::SESSION
    ///     value: example: 20
    /// Return:
    ///     Some(u64)  `0x01_00000000000014`
    ///     None       值超過 56 bit（拒絕，🚫 不截斷）
    pub fn compose(id_type: u8, value: u64) -> Option<u64> {
        if value > MAX_VALUE {
            return None;
        }
        Some(((id_type as u64) << 56) | value)
    }

    /// 跟 [`compose`] 一樣，但 `value` 超過 56 bit 就**截掉高位**而不是回 `None`——給「自己遞增、本來就想取模」的會話號用。
    ///
    /// Args:
    ///     id_type: example: SESSION
    ///     value: example: 0x22334455667788
    /// Return:
    ///     u64  `id_type` byte ‖ `value & MAX_VALUE`
    pub fn compose_masked(id_type: u8, value: u64) -> u64 {
        ((id_type as u64) << 56) | (value & MAX_VALUE)
    }

    /// Args:
    ///     id: 線上的 id, example: 0x0322334455667788
    /// Return:
    ///     u8  型別 byte, example: 0x03
    pub fn type_of(id: u64) -> u8 {
        (id >> 56) as u8
    }

    /// Args:
    ///     id: example: 0x0322334455667788
    /// Return:
    ///     u64  去掉型別 byte 的值, example: 0x22334455667788
    pub fn value_of(id: u64) -> u64 {
        id & MAX_VALUE
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
    /// 跨房間「在 `cg_seq` 之後的事件」：請求一窗，回應是一串 `Batch`（不是 `Ack`）。
    pub const RECENT: u8 = 0x01;
    /// 送事件，meta 帶 `attachments` 宣告附件。
    pub const SEND: u8 = 0x02;
    /// 只有 server → client：`Recent` 一窗裡的一批事件，meta `{ tc, bc, fs, ls, r }`，data 是 u32 大端長度前綴的事件 JSON。
    pub const BATCH: u8 = 0x03;
    /// 訂閱新事件的推送；`id` 由 client 選，之後每個 `Push`、`DeviceChanged` 抄它。
    pub const SUBSCRIBE: u8 = 0x04;
    pub const UNSUBSCRIBE: u8 = 0x05;
    /// 只有 server → client：訂閱中的房間有新事件，meta `{ bc, fs, ls, gap }`。
    pub const PUSH: u8 = 0x06;
    /// 只有 server → client：某人的裝置版本號變了，meta `{ user_id, device_version, rooms, gap }`
    /// （wbfuwunel `wbf-room-device-version.md` §6）。**只推給 `Hello.features` 宣告過
    /// `org.wbftw.device_versions` 的連線**；`id`、`seq`、`gap` 跟這條連線的 `Push` 共用。
    pub const DEVICE_CHANGED: u8 = 0x07;
}

/// `Kind::Device` 的 subtype（wbfuwunel `wbf-to-device.md`、`wbf-e2ee.md` §3；client 這邊的解讀在 `to-device-client.md`）。
/// 這些是**原生**的 pack（不帶 `IS_BRIDGED`）；同一個 kind 從 `0x20` 起是走橋的 Matrix 端點。
pub mod device {
    /// 從 `cd_seq` 起拉 to-device 佇列；回應是一串 `Batch`。
    pub const FETCH: u8 = 0x01;
    /// 只有 server → client：`Fetch` 一窗裡的一批，meta `{ tc, bc, ot, nt, counts, r, more }`。
    pub const BATCH: u8 = 0x02;
    /// 帶結果的銷毀命令：data 是 `tc` 個 u64 大端的 count，回應是 `ItemsDestroyed`。
    pub const ITEMS_DESTROY: u8 = 0x03;
    /// 訂閱這個裝置的佇列；`id` 由 client 選，之後每個 `Push`、`CryptoState` 抄它。
    pub const SUBSCRIBE: u8 = 0x04;
    pub const UNSUBSCRIBE: u8 = 0x05;
    /// 只有 server → client：佇列有新東西，meta `{ bc, ot, nt, counts, gap }`。
    pub const PUSH: u8 = 0x06;
    /// 只有 server → client：`ItemsDestroy` 的結果，meta `{ tc, bc }`。
    pub const ITEMS_DESTROYED: u8 = 0x07;
    /// 只有 server → client：自己的金鑰存量，meta `{ otk_counts, unused_fallback_key_types, gap }`；
    /// 每個 `Subscribe` 之後一定跟一個，收包迴圈🚫 不能把它當成下一個請求的回覆。
    pub const CRYPTO_STATE: u8 = 0x08;
}

/// kind `0x10 Session`（wire-format §6.3）。
pub mod session {
    pub const LOGIN: u8 = 0x01;
    pub const REFRESH: u8 = 0x02;
    pub const LOGOUT: u8 = 0x03;
}

/// `flags` 欄位的位元。沒列的位元必須是 0。
pub mod flags {
    pub const META_ENCRYPTED: u8 = 0x01;
    pub const WANT_ACK: u8 = 0x02;
    pub const IS_RESPONSE: u8 = 0x04;
    pub const IS_LAST: u8 = 0x08;
    /// 走橋：這個 pack 是 Matrix 端點的呼叫，server 轉成內部 HTTP 請求（wbfuwunel #56，
    /// `docs/design/wbf-api-bridge.md`、`docs/bridge-specs/index.md`）。server 對它的每個回覆也帶這一位：
    /// 成功 `01 01 02 14`、失敗 `01 01 03 14`。bit5–bit7 仍然保留。
    pub const IS_BRIDGED: u8 = 0x10;
    /// 所有定義過的位元；`flags & !KNOWN != 0` 就是 `ReservedFlags`。
    pub const KNOWN: u8 = META_ENCRYPTED | WANT_ACK | IS_RESPONSE | IS_LAST | IS_BRIDGED;
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
        // 長度已經驗過；這裡用 slice pattern 取標頭，🚫 不索引（索引越界是 panic，而這支程式不該有 panic 的路）。
        let [version, kind_byte, subtype, flags, ..] = bytes else {
            return Err(DecodeError::TooShort);
        };
        if *version != VERSION {
            return Err(DecodeError::UnsupportedVersion(*version));
        }
        let kind = Kind::from_byte(*kind_byte).ok_or(DecodeError::UnknownKind(*kind_byte))?;
        let (subtype, flags) = (*subtype, *flags);
        if flags & !flags::KNOWN != 0 {
            return Err(DecodeError::ReservedFlags(flags));
        }
        let id = read_u64(bytes, 4).ok_or(DecodeError::TooShort)?;
        let seq = read_u32(bytes, 12).ok_or(DecodeError::TooShort)?;

        let meta_len = read_len(bytes, 16).ok_or(DecodeError::Truncated)?;
        let meta_end = 20usize
            .checked_add(meta_len)
            .ok_or(DecodeError::Truncated)?;
        // meta 之後至少還要 meta_crc(4) + data_len(4)。
        if meta_end.checked_add(8).ok_or(DecodeError::Truncated)? > bytes.len() {
            return Err(DecodeError::Truncated);
        }
        let meta_crc_expected = read_u32(bytes, meta_end).ok_or(DecodeError::Truncated)?;
        let meta_crc_actual = crc32c(bytes.get(..meta_end).ok_or(DecodeError::Truncated)?);
        if meta_crc_actual != meta_crc_expected {
            return Err(DecodeError::MetaCrc {
                expected: meta_crc_expected,
                actual: meta_crc_actual,
            });
        }

        let data_len = read_len(bytes, meta_end + 4).ok_or(DecodeError::Truncated)?;
        let data_start = meta_end + 8;
        let data_end = data_start
            .checked_add(data_len)
            .ok_or(DecodeError::Truncated)?;
        let pack_end = data_end.checked_add(4).ok_or(DecodeError::Truncated)?;
        if pack_end > bytes.len() {
            return Err(DecodeError::Truncated);
        }
        let data = bytes
            .get(data_start..data_end)
            .ok_or(DecodeError::Truncated)?;
        let data_crc_expected = read_u32(bytes, data_end).ok_or(DecodeError::Truncated)?;
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
            meta: bytes
                .get(20..meta_end)
                .ok_or(DecodeError::Truncated)?
                .to_vec(),
            data: data.to_vec(),
        })
    }
}

/// Return:
///     Some(u32)  `at` 起的 4 byte 大端
///     None       不夠 4 byte
fn read_u32(bytes: &[u8], at: usize) -> Option<u32> {
    let chunk = bytes.get(at..)?.first_chunk::<4>()?;
    Some(u32::from_be_bytes(*chunk))
}

/// Return:
///     Some(u64)  `at` 起的 8 byte 大端
///     None       不夠 8 byte
fn read_u64(bytes: &[u8], at: usize) -> Option<u64> {
    let chunk = bytes.get(at..)?.first_chunk::<8>()?;
    Some(u64::from_be_bytes(*chunk))
}

fn read_len(bytes: &[u8], at: usize) -> Option<usize> {
    // u32 → usize 在 32-bit 目標上也不會截斷。
    read_u32(bytes, at).map(|len| len as usize)
}

#[cfg(test)]
mod id_tests {
    use super::id;

    #[test]
    fn a_typed_id_composes_and_splits_back_to_the_same_parts() {
        let upload = id::compose(id::UPLOAD, 0x22334455667788).unwrap();
        assert_eq!(upload, 0x0322334455667788);
        assert_eq!(id::type_of(upload), id::UPLOAD);
        assert_eq!(id::value_of(upload), 0x22334455667788);
        assert_eq!(id::compose(id::SESSION, 20).unwrap(), 0x0100000000000014);
        assert_eq!(id::type_of(0), id::NONE);
    }

    #[test]
    fn a_value_over_56_bits_is_refused_not_truncated() {
        assert!(id::compose(id::SESSION, id::MAX_VALUE).is_some());
        assert!(id::compose(id::SESSION, id::MAX_VALUE + 1).is_none());
    }
}

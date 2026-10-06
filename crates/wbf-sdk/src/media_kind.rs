//! 一個媒體檔是哪一種格式、整檔驗過沒有（/docs/design/rpc-specs/data-plane.md §7.1、/docs/design/media/media-download.md §12.3）。
//! 兩個都是 enum、DB 與 JSON 都存整數（維護者 2026-10-06）。

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// 媒體檔的格式，看**事件內容**判斷（🚫 看帳號）。`0` 🚫 是任何一種。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MediaKind {
    /// `org.wbftw.wbfuwunel.chunked`：每塊各自 AEAD
    WbfChunked = 1,
    /// Matrix 的 `file`（`EncryptedFile` v2）：整檔一條 AES-256-CTR，只有整檔 SHA-256 能驗
    MatrixEncrypted = 2,
    /// Matrix 只有 `url`：沒加密、沒有 hash
    MatrixPlain = 3,
}

impl MediaKind {
    /// Return:
    ///     u8   1／2／3
    pub fn to_number(self) -> u8 {
        self as u8
    }

    /// Args:
    ///     number: DB 或 JSON 讀來的, example: 2
    /// Return:
    ///     Some(MediaKind)   1／2／3
    ///     None              其他任何值（含 0）：🚫 猜成哪一種
    pub fn from_number(number: i64) -> Option<MediaKind> {
        match number {
            1 => Some(MediaKind::WbfChunked),
            2 => Some(MediaKind::MatrixEncrypted),
            3 => Some(MediaKind::MatrixPlain),
            _ => None,
        }
    }

    /// 讀這個檔要不要看 `verified`（/docs/design/rpc-specs/data-plane.md §8.2）：只有 Matrix 加密檔是「有 hash 可比、而加密本身擋不住竄改」。
    ///
    /// Return:
    ///     bool   true ＝ 沒驗過或驗不過就不可信（GET 412、匯出 1501）
    pub fn is_trust_gated_on_hash(self) -> bool {
        self == MediaKind::MatrixEncrypted
    }
}

/// 整檔跟發送者給的 hash 比對的結果。驗過（`Matched`／`Mismatched`）就🚫 再驗。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Verification {
    /// 還沒驗、不知道（下載中、驗證中，或沒有 hash 可比）
    Unknown = 0,
    /// 驗了，正確
    Matched = 1,
    /// 驗了，不正確
    Mismatched = 2,
}

impl Verification {
    /// Return:
    ///     u8   0／1／2
    pub fn to_number(self) -> u8 {
        self as u8
    }

    /// Args:
    ///     number: DB 或 JSON 讀來的, example: 1
    /// Return:
    ///     Some(Verification)   0／1／2
    ///     None                 其他任何值
    pub fn from_number(number: i64) -> Option<Verification> {
        match number {
            0 => Some(Verification::Unknown),
            1 => Some(Verification::Matched),
            2 => Some(Verification::Mismatched),
            _ => None,
        }
    }

    /// 拿發送者給的 hash 跟算出來的比（hex 或 base64 都是字串，不分大小寫的只有 hex：呼叫者給同一種寫法）。
    ///
    /// Args:
    ///     expected: 發送者給的；None ＝ 沒給, example: Some("9f86d0…")
    ///     actual: 我們算的, example: "9f86d0…"
    /// Return:
    ///     Verification   沒給是 `Unknown`；一樣是 `Matched`；不一樣是 `Mismatched`
    pub fn of_hex(expected: Option<&str>, actual: &str) -> Verification {
        match expected {
            None => Verification::Unknown,
            Some(expected) if expected.eq_ignore_ascii_case(actual) => Verification::Matched,
            Some(_) => Verification::Mismatched,
        }
    }
}

impl Serialize for MediaKind {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_u8(self.to_number())
    }
}

impl<'de> Deserialize<'de> for MediaKind {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let number = i64::deserialize(deserializer)?;
        MediaKind::from_number(number)
            .ok_or_else(|| serde::de::Error::custom(format!("unknown media kind {number}")))
    }
}

impl Serialize for Verification {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_u8(self.to_number())
    }
}

impl<'de> Deserialize<'de> for Verification {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let number = i64::deserialize(deserializer)?;
        Verification::from_number(number)
            .ok_or_else(|| serde::de::Error::custom(format!("unknown verification {number}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_numbers_round_trip_and_nothing_else_is_accepted() {
        for kind in [
            MediaKind::WbfChunked,
            MediaKind::MatrixEncrypted,
            MediaKind::MatrixPlain,
        ] {
            assert_eq!(
                MediaKind::from_number(i64::from(kind.to_number())),
                Some(kind)
            );
        }
        for verification in [
            Verification::Unknown,
            Verification::Matched,
            Verification::Mismatched,
        ] {
            assert_eq!(
                Verification::from_number(i64::from(verification.to_number())),
                Some(verification)
            );
        }
        assert_eq!(MediaKind::from_number(0), None, "0 is no kind");
        assert_eq!(MediaKind::from_number(4), None);
        assert_eq!(Verification::from_number(3), None);
        assert_eq!(Verification::from_number(-1), None);
    }

    #[test]
    fn json_carries_the_number() {
        assert_eq!(
            serde_json::to_value(MediaKind::MatrixEncrypted).unwrap(),
            serde_json::json!(2)
        );
        assert_eq!(
            serde_json::to_value(Verification::Mismatched).unwrap(),
            serde_json::json!(2)
        );
        assert!(serde_json::from_value::<MediaKind>(serde_json::json!(0)).is_err());
        assert_eq!(
            serde_json::from_value::<Verification>(serde_json::json!(1)).unwrap(),
            Verification::Matched
        );
    }

    #[test]
    fn only_matrix_encrypted_is_gated_on_its_hash() {
        assert!(MediaKind::MatrixEncrypted.is_trust_gated_on_hash());
        assert!(!MediaKind::WbfChunked.is_trust_gated_on_hash());
        assert!(!MediaKind::MatrixPlain.is_trust_gated_on_hash());
    }

    #[test]
    fn comparing_hashes() {
        assert_eq!(Verification::of_hex(None, "ab"), Verification::Unknown);
        assert_eq!(
            Verification::of_hex(Some("AB"), "ab"),
            Verification::Matched
        );
        assert_eq!(
            Verification::of_hex(Some("ab"), "cd"),
            Verification::Mismatched
        );
    }
}

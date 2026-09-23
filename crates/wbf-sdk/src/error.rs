//! SDK 對外的錯誤。分類對齊 CLI 規格 §4 的 exit code：用法錯、server 拒絕、完整性失敗、網路。

use crate::chunk_block::BlockError;
use crate::chunk_crypto::CryptoError;
use wbf_wire::{DecodeError, EncodeError};

#[derive(Debug)]
pub enum SdkError {
    /// 呼叫者給錯：seek 超過檔尾、空檔、chunk_size 0…（CLI exit 1）。
    Usage(String),
    /// server 回 `Error` pack、或 HTTP 非 2xx（CLI exit 2）。`meta` 是整份 Error meta，含 `expected_seq` 這類額外欄位。
    ///
    /// ⚠️ 這個變體裝著**三種來源**：wbf 的 `Error` pack、Matrix HTTP 的 `errcode`（`M_FORBIDDEN`）、
    /// 我們自己合成的（HTTP 401 的 `Unauthorized`、`HTTP_502`）。所以 `code` 只能給人看 ——
    /// 🚨 **程式要判斷 wbf 的錯誤，用 [`SdkError::wbf_code`]**（只看 `code_id`），🚫 不要比 `code` 字串。
    Server {
        /// 名字，給人看的, example: "OutOfOrder"、"M_FORBIDDEN"
        code: String,
        message: String,
        meta: serde_json::Value,
        /// wbf `Error` pack 帶的序號（wbfuwunel `wbf-wire-format.md` §3.4）。
        /// `None` ＝ 不是 wbf pack 來的（Matrix、合成的），或 pack 裡沒有合法的非 0 整數。
        code_id: Option<u64>,
    },
    /// 約定 §3.1 任一條不過、CRC 不對、事件與 `Info` 對不上（CLI exit 3）。
    Integrity(String),
    /// 連不上、斷線、HTTP 層失敗（CLI exit 4）。
    Network(String),
    /// 對方講的不是這個協議：pack 解不開（CRC 除外）、回應的 id／seq 對不上、Ack meta 不是預期的 JSON。
    Protocol(String),
    Io(std::io::Error),
    /// 等逾時：`watch once --timeout` 到了還沒有事件（CLI exit 5）。
    Timeout(String),
}

impl std::fmt::Display for SdkError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SdkError::Usage(message) => write!(formatter, "usage: {message}"),
            // ⚠️ 帶上 `code_id`：不認得的碼要「原樣留在 log」（server 表的規則），而名字之外那個號才是權威。
            SdkError::Server {
                code,
                message,
                code_id: Some(code_id),
                ..
            } => write!(formatter, "server {code} ({code_id}): {message}"),
            SdkError::Server { code, message, .. } => write!(formatter, "server {code}: {message}"),
            SdkError::Integrity(message) => write!(formatter, "integrity: {message}"),
            SdkError::Network(message) => write!(formatter, "network: {message}"),
            SdkError::Protocol(message) => write!(formatter, "protocol: {message}"),
            SdkError::Io(error) => write!(formatter, "io: {error}"),
            SdkError::Timeout(message) => write!(formatter, "timeout: {message}"),
        }
    }
}

impl std::error::Error for SdkError {}

impl From<BlockError> for SdkError {
    fn from(error: BlockError) -> Self {
        SdkError::Integrity(error.to_string())
    }
}

impl From<CryptoError> for SdkError {
    fn from(error: CryptoError) -> Self {
        SdkError::Integrity(error.to_string())
    }
}

impl From<DecodeError> for SdkError {
    /// CRC 不對是完整性問題（線上規格 §5 說重送，那是通道層的事；到這裡就是壞的），其他是對方不講協議。
    fn from(error: DecodeError) -> Self {
        match error {
            DecodeError::MetaCrc { .. } | DecodeError::DataCrc { .. } => {
                SdkError::Integrity(error.to_string())
            }
            other => SdkError::Protocol(other.to_string()),
        }
    }
}

impl From<EncodeError> for SdkError {
    fn from(error: EncodeError) -> Self {
        SdkError::Usage(error.to_string())
    }
}

/// 我們自己的值序列化不了（serde 回錯）。理論上到不了：這裡的型別都是純欄位——但那不是 panic 的理由（維護者 2026-09-23）。
///
/// Args:
///     what: 序列化的是什麼，給人看, example: "Session"
/// Return:
///     SdkError::Usage
pub(crate) fn cannot_serialize(what: &str, error: serde_json::Error) -> SdkError {
    SdkError::Usage(format!("{what} could not be serialized: {error}"))
}

impl From<std::io::Error> for SdkError {
    fn from(error: std::io::Error) -> Self {
        SdkError::Io(error)
    }
}

impl SdkError {
    /// 🚨 **只給人看、給測試斷言名字用** —— 🚫 **程式決策請用 [`SdkError::wbf_code`]**（只看 `code_id`）。
    /// 這個字串同時裝著 wbf 的名字、Matrix 的 `errcode` 與我們合成的碼，拿它判斷就是在賭三者不撞名
    /// （PR #37 審查 cirno💡）。
    ///
    /// Return:
    ///     Option<&str>  `Server` 的 code, example: "OutOfOrder"；其他變體 None
    pub fn server_code(&self) -> Option<&str> {
        match self {
            SdkError::Server { code, .. } => Some(code),
            _ => None,
        }
    }

    /// 這是不是一個**認得的** wbf 錯誤碼。🚨 **只看 `code_id`**，🚫 不看名字（issue #29 第 2 項）。
    ///
    /// Return:
    ///     Some(WbfErrorCode)  wbf `Error` pack 來的、而且 `code_id` 在表上
    ///     None                不是 `Server`、不是 wbf pack 來的、或不認得的碼 ——
    ///                         呼叫端一律當「失敗了，不知道能不能重試」：🚫 不重試
    pub fn wbf_code(&self) -> Option<crate::error_code::WbfErrorCode> {
        match self {
            SdkError::Server {
                code_id: Some(code_id),
                ..
            } => crate::error_code::WbfErrorCode::from_id(*code_id),
            _ => None,
        }
    }

    // ---- 從 Matrix 錯誤來的 `Error` 多帶的欄位（wbfuwunel #56，wire-format §3.4 表下）----
    //
    // ⚠️ 只讀 **wbf `Error` pack 的 meta**：直接打 Matrix HTTP 的錯誤（`login.rs`）meta 不是這個形狀，這幾個一律 None／false。
    // ⚠️ 欄位「不出現」就是沒有值：server 2026-09-15 起不再寫 `"soft_logout": false`、`"retry_after_ms": null`。
    // 📎 認碼仍然只看 `code_id`（`wbf_code`）；這幾個是**附加的原因**，給「能不能 refresh、要等多久」這種決定用。

    /// Return:
    ///     Some(u16)  Matrix 的 HTTP 狀態碼, example: 429；只收 100–599 的整數
    ///     None       不是 `Server`、沒有這個欄位、或形狀不對
    pub fn matrix_status(&self) -> Option<u16> {
        self.server_meta_field("status")?
            .as_u64()
            .filter(|status| (100..=599).contains(status))
            .map(|status| status as u16)
    }

    /// Return:
    ///     Some(&str)  Matrix body 的 `errcode`, example: "M_USER_LOCKED"；空字串不算
    ///     None        不是 `Server`、沒有、或不是字串
    pub fn matrix_errcode(&self) -> Option<&str> {
        self.server_meta_field("errcode")?
            .as_str()
            .filter(|errcode| !errcode.is_empty())
    }

    /// Return:
    ///     Some(u64)  server 說要等多久再試（毫秒）, example: 700
    ///     None       沒說（包括限速但不知道要等多久）
    pub fn retry_after_ms(&self) -> Option<u64> {
        self.server_meta_field("retry_after_ms")?.as_u64()
    }

    /// `RoomDevicesChanged`（1506）帶的**目前**房間版本號（wbfuwunel `wbf-room-device-version.md` §7.1）。
    ///
    /// 📎 這個號碼只能拿來「知道自己過期了」，🚫 不能直接拿它重送：金鑰還沒補發給變了的裝置。
    /// 重送前要重拿成員清單（那份帶的號碼才跟名單同一刻）。
    ///
    /// Return:
    ///     Some(u64)  server 說的目前號碼, example: 81240
    ///     None       不是 `Server`、沒有這個欄位、或不是非負整數
    pub fn current_room_version(&self) -> Option<u64> {
        self.server_meta_field("room_version")?.as_u64()
    }

    /// session 是不是 **soft logout**（token 過期但可以 refresh）。
    ///
    /// 🚨 **只有 JSON 的 `true` 才算**：不出現、`false`、字串 `"true"`、數字都是 false。
    /// 錯判成 true 的後果是拿一個已經被撤銷的 session 去 refresh；錯判成 false 只是要使用者重新登入 —— 所以往 false 那邊倒。
    ///
    /// Return:
    ///     bool  true ＝ server 明說 `"soft_logout": true`
    pub fn is_soft_logout(&self) -> bool {
        self.server_meta_field("soft_logout")
            .is_some_and(|value| value.as_bool() == Some(true))
    }

    /// 走橋的回覆是不是 Matrix 的 404（bridge-specs index.md §1.2：meta 帶 `status`）。
    /// 🚨 **只認整數 `404`**：字串 `"404"`、`errcode` 像 `M_NOT_FOUND` 但 `status` 不是 404 的都不算——
    /// 錯判成「沒有」會把一個真的錯誤（被拒、壞掉）當成空值吞掉。
    ///
    /// Return:
    ///     bool  true ＝ `Server` 而且 meta 的 `status` 是整數 404
    pub fn is_not_found(&self) -> bool {
        self.server_meta_field("status")
            .is_some_and(|status| status.as_u64() == Some(404))
    }

    fn server_meta_field(&self, key: &str) -> Option<&serde_json::Value> {
        match self {
            SdkError::Server { meta, .. } => meta.get(key),
            _ => None,
        }
    }
}

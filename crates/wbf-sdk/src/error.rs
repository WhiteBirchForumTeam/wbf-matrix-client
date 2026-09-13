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

impl From<std::io::Error> for SdkError {
    fn from(error: std::io::Error) -> Self {
        SdkError::Io(error)
    }
}

impl SdkError {
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
}

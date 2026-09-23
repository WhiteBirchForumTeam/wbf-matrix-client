//! 事件區塊 `org.wbftw.wbfuwunel.chunked` 與描述（約定 §4、§5）。
//!
//! 兩者是同一組欄位，描述只少 `key`，所以用同一個 struct，用兩個檢查函數分別問
//! 「這份能當事件區塊嗎」「這份能當描述嗎」。不認得的欄位忽略（約定 §4）。

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::cipher::{Cipher, KEY_LEN};

/// 本 SDK 認得的約定版本（約定 §8）。
pub const CONVENTION_V: u32 = 1;
/// `nonce_base` 的長度，byte。
pub const NONCE_BASE_LEN: usize = 8;

/// 事件區塊，也是描述（`key` 為 None 的那份）。欄位語意見約定 §4 的表。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChunkedBlock {
    pub v: u32,
    pub cipher: Cipher,
    /// 加密模式必要、明文模式必須沒有；描述裡永遠沒有。
    #[serde(default, skip_serializing_if = "Option::is_none", with = "base64_key")]
    pub key: Option<[u8; KEY_LEN]>,
    /// 加密模式必要、明文模式沒有。
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "base64_nonce_base"
    )]
    pub nonce_base: Option<[u8; NONCE_BASE_LEN]>,
    pub chunk_size: u32,
    /// 缺 = 還不知道（串流上傳的 `Create` 描述）。事件區塊裡必要。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_size: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mimetype: Option<String>,
    /// 整檔明文 SHA-256，十六進位小寫。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
}

/// 檢查不過的原因。任一個都是「當成解不開的檔」（約定 §5）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BlockError {
    UnknownVersion(u32),
    /// 加密模式缺 `key`（只有事件區塊會報；描述本來就沒有）。
    MissingKey,
    /// 明文模式帶了 `key`，或描述裡出現 `key`。
    UnexpectedKey,
    MissingNonceBase,
    UnexpectedNonceBase,
    ChunkSizeZero,
    /// 事件區塊缺 `file_size`。
    MissingFileSize,
}

impl std::fmt::Display for BlockError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BlockError::UnknownVersion(v) => write!(formatter, "unknown convention version {v}"),
            BlockError::MissingKey => write!(formatter, "encrypted mode but no key"),
            BlockError::UnexpectedKey => write!(formatter, "key present where it must not be"),
            BlockError::MissingNonceBase => write!(formatter, "encrypted mode but no nonce_base"),
            BlockError::UnexpectedNonceBase => {
                write!(formatter, "plaintext mode but nonce_base present")
            }
            BlockError::ChunkSizeZero => write!(formatter, "chunk_size is 0"),
            BlockError::MissingFileSize => write!(formatter, "event block has no file_size"),
        }
    }
}

impl std::error::Error for BlockError {}

impl ChunkedBlock {
    /// 約定 §3.1 第 1 條，事件區塊那一面：加密模式要有 `key` 與 `nonce_base`，明文模式兩者都不能有，
    /// `file_size` 必要。
    ///
    /// Return:
    ///     Ok(())            可以拿去解
    ///     Err(BlockError)   哪一條不過
    pub fn check_as_event_block(&self) -> Result<(), BlockError> {
        self.check_shared_rules()?;
        if self.cipher.is_encrypting() && self.key.is_none() {
            return Err(BlockError::MissingKey);
        }
        if self.file_size.is_none() {
            return Err(BlockError::MissingFileSize);
        }
        Ok(())
    }

    /// 約定 §4：描述永遠沒有 `key`；`file_size` 可以缺（串流的 `Create`）。
    ///
    /// Return:
    ///     Ok(())            是一份合法描述
    ///     Err(BlockError)   哪一條不過
    pub fn check_as_description(&self) -> Result<(), BlockError> {
        self.check_shared_rules()?;
        if self.key.is_some() {
            return Err(BlockError::UnexpectedKey);
        }
        Ok(())
    }

    fn check_shared_rules(&self) -> Result<(), BlockError> {
        if self.v != CONVENTION_V {
            return Err(BlockError::UnknownVersion(self.v));
        }
        if self.chunk_size == 0 {
            return Err(BlockError::ChunkSizeZero);
        }
        if self.cipher.is_encrypting() {
            if self.nonce_base.is_none() {
                return Err(BlockError::MissingNonceBase);
            }
        } else {
            if self.key.is_some() {
                return Err(BlockError::UnexpectedKey);
            }
            if self.nonce_base.is_some() {
                return Err(BlockError::UnexpectedNonceBase);
            }
        }
        Ok(())
    }

    /// 事件區塊去掉 `key` 就是描述（約定 §4：「只少 `key`」）。
    ///
    /// Return:
    ///     ChunkedBlock  `key` 為 None，其他欄位照抄
    pub fn to_description(&self) -> ChunkedBlock {
        ChunkedBlock {
            key: None,
            ..self.clone()
        }
    }

    /// 描述的 JSON bytes：`Create`／`Seal` 的 data 加密前的明文。鍵序固定（struct 順序），
    /// 讓向量檔可以逐 byte 比。
    ///
    /// Return:
    ///     Ok(Vec<u8>)  compact JSON，UTF-8，example: {"v":1,"cipher":"none","chunk_size":16}
    ///     Err(Usage)   序列化不了（純欄位，理論上到不了）
    pub fn to_description_json(&self) -> Result<Vec<u8>, crate::error::SdkError> {
        serde_json::to_vec(&self.to_description())
            .map_err(|error| crate::error::cannot_serialize("ChunkedBlock description", error))
    }

    /// Args:
    ///     json: `Info` 還回、解密後的描述, example: br#"{"v":1,"cipher":"none","chunk_size":16}"#
    /// Return:
    ///     Ok(ChunkedBlock)  解析成功且 `check_as_description` 過
    ///     Err(String)       JSON 壞掉、`cipher` 不認得、或檢查不過；訊息給人看
    pub fn from_description_json(json: &[u8]) -> Result<ChunkedBlock, String> {
        let block: ChunkedBlock =
            serde_json::from_slice(json).map_err(|error| error.to_string())?;
        block
            .check_as_description()
            .map_err(|error| error.to_string())?;
        Ok(block)
    }

    /// 兩份對不上就拒絕（約定 §4：「兩份不一致時以事件為準；下載端可以拿描述交叉核對，不一致就拒絕」）。
    /// 只比兩邊都有的欄位：描述可能缺 `file_size`／`sha256`（串流的 `Create` 那份）。
    ///
    /// Args:
    ///     description: `Info` 還回的那份
    /// Return:
    ///     bool  1 = 一致
    pub fn is_consistent_with_description(&self, description: &ChunkedBlock) -> bool {
        let both_or_absent = |mine: &Option<String>, theirs: &Option<String>| match (mine, theirs) {
            (Some(a), Some(b)) => a == b,
            _ => true,
        };
        self.v == description.v
            && self.cipher == description.cipher
            && self.nonce_base == description.nonce_base
            && self.chunk_size == description.chunk_size
            && match (self.file_size, description.file_size) {
                (Some(a), Some(b)) => a == b,
                _ => true,
            }
            && both_or_absent(&self.name, &description.name)
            && both_or_absent(&self.mimetype, &description.mimetype)
            && both_or_absent(&self.sha256, &description.sha256)
    }
}

/// `Option<[u8; N]>` 與 base64 字串互轉（RFC 4648 標準字母表、帶 `=`，約定 §4）。長度不對就拒絕。
macro_rules! base64_fixed_option {
    ($module:ident, $len:expr) => {
        mod $module {
            use super::*;

            pub fn serialize<S: Serializer>(
                value: &Option<[u8; $len]>,
                serializer: S,
            ) -> Result<S::Ok, S::Error> {
                match value {
                    Some(bytes) => serializer.serialize_str(&BASE64.encode(bytes)),
                    None => serializer.serialize_none(),
                }
            }

            pub fn deserialize<'de, D: Deserializer<'de>>(
                deserializer: D,
            ) -> Result<Option<[u8; $len]>, D::Error> {
                let text: Option<String> = Option::deserialize(deserializer)?;
                let Some(text) = text else { return Ok(None) };
                let bytes = BASE64.decode(&text).map_err(serde::de::Error::custom)?;
                let fixed: [u8; $len] = bytes.try_into().map_err(|bytes: Vec<u8>| {
                    serde::de::Error::custom(format!(
                        "expected {} bytes, got {}",
                        $len,
                        bytes.len()
                    ))
                })?;
                Ok(Some(fixed))
            }
        }
    };
}

base64_fixed_option!(base64_key, KEY_LEN);
base64_fixed_option!(base64_nonce_base, NONCE_BASE_LEN);

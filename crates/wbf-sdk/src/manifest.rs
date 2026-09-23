//! CLI 規格 §5 的 manifest 與 §6 的上傳狀態檔。兩者都含 `key`，寫檔時是機密。

use serde::{Deserialize, Serialize};

use crate::chunk_block::ChunkedBlock;
use crate::chunk_crypto::FileCipher;
use crate::error::SdkError;

/// `upload` 印的、`download`／`seek` 吃的。`block` 逐字就是事件區塊 `org.wbftw.wbfuwunel.chunked`。
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Manifest {
    pub server: String,
    pub mxc: String,
    pub block: ChunkedBlock,
}

impl Manifest {
    /// Args:
    ///     json: manifest 檔內容
    /// Return:
    ///     Ok(Manifest)      解析成功且 `block` 過 `check_as_event_block`
    ///     Err(SdkError)     `Usage`（JSON 壞）、`Integrity`（區塊不過）
    pub fn from_json(json: &[u8]) -> Result<Manifest, SdkError> {
        let manifest: Manifest = serde_json::from_slice(json)
            .map_err(|error| SdkError::Usage(format!("manifest: {error}")))?;
        manifest.block.check_as_event_block()?;
        Ok(manifest)
    }

    /// Return:
    ///     Ok(Vec<u8>)  pretty JSON
    ///     Err(Usage)   序列化不了（純欄位，理論上到不了）
    pub fn to_json(&self) -> Result<Vec<u8>, crate::error::SdkError> {
        serde_json::to_vec_pretty(self)
            .map_err(|error| crate::error::cannot_serialize("Manifest", error))
    }

    pub fn file_cipher(&self) -> Result<FileCipher, SdkError> {
        Ok(FileCipher::from_event_block(&self.block)?)
    }

    /// Return:
    ///     u64  明文總長（`check_as_event_block` 保證 `file_size` 有）
    pub fn file_size(&self) -> u64 {
        self.block.file_size.unwrap_or(0)
    }
}

/// 上傳中的一切，`Create` 之後就該落地（CLI 規格 §6），續傳時讀回來。
/// `block.file_size` 在串流模式是 None；串流沒有續傳，狀態只活在記憶體。
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct UploadState {
    pub server: String,
    pub user_id: String,
    pub upload_id: u64,
    pub mxc: String,
    /// `chunk_max_bytes`：這個上傳每塊 data 的上限（`Create` Ack）。
    pub chunk_max_bytes: u64,
    pub block: ChunkedBlock,
}

impl UploadState {
    pub fn from_json(json: &[u8]) -> Result<UploadState, SdkError> {
        let state: UploadState = serde_json::from_slice(json)
            .map_err(|error| SdkError::Usage(format!("upload state: {error}")))?;
        // 狀態檔的區塊可能沒有 file_size（串流不落地，所以實際上都有），用描述那一面驗，再另外要求 key。
        state
            .block
            .check_as_description()
            .or_else(|_| state.block.check_as_event_block())?;
        Ok(state)
    }

    /// Return:
    ///     Ok(Vec<u8>)  pretty JSON
    ///     Err(Usage)   序列化不了（純欄位，理論上到不了）
    pub fn to_json(&self) -> Result<Vec<u8>, crate::error::SdkError> {
        serde_json::to_vec_pretty(self)
            .map_err(|error| crate::error::cannot_serialize("UploadState", error))
    }

    /// CLI 規格 §3.2：狀態檔的 server 與 user 跟現在的不一樣就拒絕，不拿 A server 的上傳去打 B server。
    ///
    /// Return:
    ///     bool  1 = 同一個 server、同一個 user
    pub fn is_for(&self, server: &str, user_id: &str) -> bool {
        self.server.trim_end_matches('/') == server.trim_end_matches('/') && self.user_id == user_id
    }

    pub fn file_cipher(&self) -> Result<FileCipher, SdkError> {
        // 事件區塊那一面要 file_size；上傳狀態在串流模式沒有，所以自己組。
        let mut block = self.block.clone();
        if block.file_size.is_none() {
            block.file_size = Some(0);
        }
        Ok(FileCipher::from_event_block(&block)?)
    }
}

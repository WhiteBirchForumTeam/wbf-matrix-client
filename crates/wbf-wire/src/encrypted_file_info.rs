//! `Create` 的 meta：固定 16 byte 二進位（規格 §3.1）。

/// `Create` 的 meta。三個欄位都是**明文**的事實；server 靠它們算塊的位置，不看密文。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EncryptedFileInfo {
    /// 明文總長，byte。串流模式為 0。
    pub file_size: u64,
    /// 明文塊大小，byte。0 = 用 server 預設（64 KiB）。
    pub chunk_size: u32,
    /// 總塊數，必須等於 `ceil(file_size / chunk_size)`。串流模式為 0。
    pub chunk_count: u32,
}

/// `EncryptedFileInfo` 固定的線上長度。
pub const ENCRYPTED_FILE_INFO_LEN: usize = 16;

impl EncryptedFileInfo {
    /// Return:
    ///     [u8; 16]  `file_size` ‖ `chunk_size` ‖ `chunk_count`，全部 big-endian
    pub fn to_bytes(self) -> [u8; ENCRYPTED_FILE_INFO_LEN] {
        let mut bytes = [0u8; ENCRYPTED_FILE_INFO_LEN];
        bytes[0..8].copy_from_slice(&self.file_size.to_be_bytes());
        bytes[8..12].copy_from_slice(&self.chunk_size.to_be_bytes());
        bytes[12..16].copy_from_slice(&self.chunk_count.to_be_bytes());
        bytes
    }

    /// Args:
    ///     bytes: 線上的 meta, example: 00000000000203d8 00010000 00000003
    /// Return:
    ///     Some(EncryptedFileInfo)  長度剛好 16
    ///     None                     長度不是 16（server 對這種 Create 回 Conflict）
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        let bytes: &[u8; ENCRYPTED_FILE_INFO_LEN] = bytes.try_into().ok()?;
        Some(Self {
            file_size: u64::from_be_bytes(bytes[0..8].try_into().expect("8 bytes")),
            chunk_size: u32::from_be_bytes(bytes[8..12].try_into().expect("4 bytes")),
            chunk_count: u32::from_be_bytes(bytes[12..16].try_into().expect("4 bytes")),
        })
    }

    /// 串流模式（規格 §3.1）：大小未知，最後一塊帶 `IS_LAST` 才結束。
    ///
    /// Return:
    ///     bool  1 = `file_size` 與 `chunk_count` 都是 0
    pub fn is_streaming(self) -> bool {
        self.file_size == 0 && self.chunk_count == 0
    }
}

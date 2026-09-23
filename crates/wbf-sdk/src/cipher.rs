//! `cipher` 欄位：三個值、怎麼選預設、一次 AEAD 呼叫（約定 §3）。
//!
//! 這裡只管「用哪個演算法」；nonce、AAD、索引的規則在 `chunk_crypto`。

use aes_gcm::aead::{Aead, KeyInit, Payload};
use serde::{Deserialize, Serialize};

/// 每檔一個，寫在描述與事件區塊的 `cipher`。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Cipher {
    #[serde(rename = "chacha20-poly1305")]
    ChaCha20Poly1305,
    #[serde(rename = "aes-256-gcm")]
    Aes256Gcm,
    /// 明文模式：塊就是明文、描述就是 JSON、事件區塊沒有 `key`。
    #[serde(rename = "none")]
    None,
}

/// 兩個 AEAD 都是 32 byte 金鑰。
pub const KEY_LEN: usize = 32;
/// 兩個 AEAD 都是 12 byte nonce。
pub const NONCE_LEN: usize = 12;
/// 兩個 AEAD 都是 16 byte 標籤，附在密文後。
pub const TAG_LEN: usize = 16;

impl Cipher {
    /// Args:
    ///     name: `cipher` 欄位的字串, example: "aes-256-gcm"
    /// Return:
    ///     Some(Cipher)  三個認得的值之一
    ///     None          其他任何字串（約定 §5：拒絕）
    pub fn from_name(name: &str) -> Option<Cipher> {
        match name {
            "chacha20-poly1305" => Some(Cipher::ChaCha20Poly1305),
            "aes-256-gcm" => Some(Cipher::Aes256Gcm),
            "none" => Some(Cipher::None),
            _ => None,
        }
    }

    /// Return:
    ///     &str  example: "chacha20-poly1305"；`Cipher::None` 回 "none"
    pub fn name(self) -> &'static str {
        match self {
            Cipher::ChaCha20Poly1305 => "chacha20-poly1305",
            Cipher::Aes256Gcm => "aes-256-gcm",
            Cipher::None => "none",
        }
    }

    /// Return:
    ///     bool  1 = 塊與描述要加密（`ChaCha20Poly1305`／`Aes256Gcm`），0 = 明文模式
    pub fn is_encrypting(self) -> bool {
        !matches!(self, Cipher::None)
    }

    /// SDK 的預設（約定 §3）：有硬體 AES 就 `Aes256Gcm`，否則 `ChaCha20Poly1305`。永遠不回 `None`：
    /// 明文模式是房間決定的，不是預設。
    ///
    /// Return:
    ///     Cipher  `Aes256Gcm` 或 `ChaCha20Poly1305`
    pub fn default_for_this_machine() -> Cipher {
        if is_hardware_aes_available() {
            Cipher::Aes256Gcm
        } else {
            Cipher::ChaCha20Poly1305
        }
    }

    /// 一次 AEAD 加密。`Cipher::None` 時原樣回傳（明文模式沒有標籤）。
    ///
    /// Args:
    ///     key: 32 byte, example: [0x11; 32]
    ///     nonce: 12 byte, example: nonce_base ‖ u32_be(i)
    ///     aad: example: b"wbf-chunk-v1"
    ///     plain: 明文, example: b"hello"
    /// Return:
    ///     Ok(Vec<u8>)             密文 ‖ 16 byte 標籤；`None` 模式就是 `plain` 的複本
    ///     Err(SealFailed)         AEAD 底層回錯（只有滿位會；輸入被 chunk_size 擋住，理論上到不了）
    pub fn seal(
        self,
        key: &[u8; KEY_LEN],
        nonce: &[u8; NONCE_LEN],
        aad: &[u8],
        plain: &[u8],
    ) -> Result<Vec<u8>, crate::chunk_crypto::CryptoError> {
        let payload = Payload { msg: plain, aad };
        match self {
            Cipher::ChaCha20Poly1305 => chacha20poly1305::ChaCha20Poly1305::new(key.into())
                .encrypt(nonce.into(), payload)
                .map_err(|_| crate::chunk_crypto::CryptoError::SealFailed),
            Cipher::Aes256Gcm => aes_gcm::Aes256Gcm::new(key.into())
                .encrypt(nonce.into(), payload)
                .map_err(|_| crate::chunk_crypto::CryptoError::SealFailed),
            Cipher::None => Ok(plain.to_vec()),
        }
    }

    /// 一次 AEAD 解密並驗標籤。`Cipher::None` 時原樣回傳。
    ///
    /// Args:
    ///     key: 32 byte
    ///     nonce: 12 byte
    ///     aad: 必須與加密時相同
    ///     sealed: 密文 ‖ 標籤
    /// Return:
    ///     Some(Vec<u8>)  明文
    ///     None           標籤不對、`sealed` 不到 16 byte、或 key／nonce／aad 任一不同
    pub fn open(
        self,
        key: &[u8; KEY_LEN],
        nonce: &[u8; NONCE_LEN],
        aad: &[u8],
        sealed: &[u8],
    ) -> Option<Vec<u8>> {
        let payload = Payload { msg: sealed, aad };
        match self {
            Cipher::ChaCha20Poly1305 => chacha20poly1305::ChaCha20Poly1305::new(key.into())
                .decrypt(nonce.into(), payload)
                .ok(),
            Cipher::Aes256Gcm => aes_gcm::Aes256Gcm::new(key.into())
                .decrypt(nonce.into(), payload)
                .ok(),
            Cipher::None => Some(sealed.to_vec()),
        }
    }
}

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
fn is_hardware_aes_available() -> bool {
    cpufeatures::new!(cpuid_aes, "aes");
    cpuid_aes::get()
}

#[cfg(target_arch = "aarch64")]
fn is_hardware_aes_available() -> bool {
    cpufeatures::new!(cpuid_aes, "aes");
    cpuid_aes::get()
}

#[cfg(not(any(target_arch = "x86", target_arch = "x86_64", target_arch = "aarch64")))]
fn is_hardware_aes_available() -> bool {
    false
}

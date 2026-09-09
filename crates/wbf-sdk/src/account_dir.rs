//! 資料目錄裡兩層目錄名的加密（local-cache-db.md §11）。
//!
//! ```text
//! servers/<b58 nonce>_<b58 密文>/            ← 正規化過的 server host
//!   accounts/<b58 nonce>_<b58 密文>/         ← localpart
//! ```
//!
//! 外面只看得到 Base58，看不出這台機器連過哪家 server、有誰的帳號。
//! 這個模組只做「明文 ↔ 一段目錄名」的轉換：🚫 不碰檔案系統、🚫 不知道佈局長什麼樣（那是呼叫者的事）。
//!
//! 兩件事撐起整個設計：
//!
//! - **nonce 由明文確定性導出，而且照樣寫進名字裡**。寫進去是因為解密時要先有 nonce，
//!   而它是從還沒解出來的明文導出的；確定性是為了 `login` 能直接算出路徑去定位，不必先掃描。
//! - 🚫 **固定 nonce 會洩漏明文**：ChaCha20 是 stream cipher，同 key 同 nonce 的兩份密文
//!   XOR 起來就是兩份明文的 XOR。從明文導出正好保證「不同明文 → 不同 nonce」。
//! - **nonce 用 12 byte 而不是 XChaCha 的 24**：目錄名進路徑，而 Windows 的 MAX_PATH 是 260。
//!   理由與安全性推導見 `NONCE_LEN`。

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Nonce};

use crate::vault::Key32;
use crate::SdkError;

/// 分隔 nonce 與密文。Base58 的字母表沒有底線，所以它不會出現在兩段裡面。
const SEPARATOR: char = '_';

/// ChaCha20-Poly1305 的 nonce 長度。
///
/// ⚠️ 這裡**不用 XChaCha20**（那是 24 byte），因為 nonce 要寫進目錄名，而目錄名進路徑：
/// 24 byte 的 base58 是 33 字元、兩層就 66，在 Windows 的 MAX_PATH（260）下是付不起的
/// （2026-09-09 對真 server 驗證時撞到：長一點的 data dir 直接開不了 sqlite）。
///
/// 12 byte 夠不夠：nonce 是 `BLAKE3 keyed_hash(key, …‖plaintext)` 的前 12 byte，
/// 碰撞要兩個**不同明文**的 hash 前 96 bit 相同——生日界是 2^48 個明文，
/// 而這裡的明文是「這台機器的 server host 與 localpart」，數量是個位數。
const NONCE_LEN: usize = 12;

/// 一段目錄名的字元上限。Windows 單一路徑元件是 255；留餘裕給呼叫者接副檔名之類。
const MAX_DIR_NAME_CHARS: usize = 200;

/// 目錄名在哪一層。決定 aad 與 nonce 的 context —— 換句話說，
/// 兩層的名字互相解不開，帳號目錄搬到另一個 server 目錄底下也解不開。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DirScope<'a> {
    /// `servers/<這一段>/`，明文是正規化過的 server host。
    Server,
    /// `servers/<…>/accounts/<這一段>/`，明文是 localpart；綁住它上面那層的 host 明文。
    Account { server_host: &'a str },
    /// `recovery/<這一段>`，明文是 `recovery-key@bob:matrix.org`（local-cache-db.md §10.9）。
    ///
    /// 它**不在帳號目錄底下**，因為 `logout` 要把帳號目錄整個清掉而 recovery key 要留著：
    /// 前者是“這台機器上的裝置狀態”，後者是“回到 server 備份的鑰匙”。
    Recovery,
}

impl DirScope<'_> {
    fn aad(&self) -> Vec<u8> {
        match self {
            DirScope::Server => b"wbf-matrix-client server dir v1".to_vec(),
            DirScope::Account { server_host } => {
                let mut aad = b"wbf-matrix-client account dir v1".to_vec();
                aad.extend_from_slice(server_host.as_bytes());
                aad
            }
            DirScope::Recovery => b"wbf-matrix-client recovery name v1".to_vec(),
        }
    }

    /// nonce 的導出輸入。各段之間夾 `0x00`，所以 ("a", "bc") 與 ("ab", "c") 導不出同一個 nonce。
    fn nonce_input(&self, plaintext: &str) -> Vec<u8> {
        let mut input = match self {
            DirScope::Server => b"wbf server-dir-nonce v1".to_vec(),
            DirScope::Account { server_host } => {
                let mut input = b"wbf account-dir-nonce v1".to_vec();
                input.push(0);
                input.extend_from_slice(server_host.as_bytes());
                input
            }
            DirScope::Recovery => b"wbf recovery-name-nonce v1".to_vec(),
        };
        input.push(0);
        input.extend_from_slice(plaintext.as_bytes());
        input
    }
}

/// 明文 → 目錄名。同一把金鑰、同一個 scope、同一個明文永遠得到同一個名字。
///
/// Args:
///     key: vault 的第六把子金鑰, example: vault.account_dir_key()
///     scope: example: DirScope::Account { server_host: "localhost:6167" }
///     plaintext: 正規化過的 host 或 localpart, example: "alice"
/// Return:
///     Ok(String)   目錄名, example: "3vQB7B6MrGQZaxCuFg4oh_2NEpo7TZRRrLZSizrsHo"
///     Err(Usage)   明文是空的、或名字會超過 200 字元（🚫 不截斷：截斷就解不回來了）
pub fn to_dir_name(key: &Key32, scope: DirScope<'_>, plaintext: &str) -> Result<String, SdkError> {
    if plaintext.is_empty() {
        return Err(SdkError::Usage(
            "cannot encode an empty directory name".into(),
        ));
    }
    let nonce = nonce_of(key, scope, plaintext);
    let ciphertext = ChaCha20Poly1305::new(key.as_bytes().into())
        .encrypt(
            Nonce::from_slice(&nonce),
            Payload {
                msg: plaintext.as_bytes(),
                aad: &scope.aad(),
            },
        )
        .map_err(|_| SdkError::Usage("cannot encrypt this directory name".into()))?;
    let name = format!(
        "{}{SEPARATOR}{}",
        bs58::encode(nonce).into_string(),
        bs58::encode(ciphertext).into_string()
    );
    if name.chars().count() > MAX_DIR_NAME_CHARS {
        return Err(SdkError::Usage(format!(
            "the encrypted directory name would be {} characters, over the {MAX_DIR_NAME_CHARS} limit",
            name.chars().count()
        )));
    }
    Ok(name)
}

/// 目錄名 → 明文。**不是這把金鑰的、格式不對的、被動過的一律 `None`**（fail closed）：
/// 呼叫者該把解不開的目錄當作不存在，🚫 不要猜、不要刪。
///
/// Args:
///     key: example: vault.account_dir_key()
///     scope: example: DirScope::Server
///     dir_name: 磁碟上那個目錄的名字, example: "3vQB7B6MrGQZaxCuFg4oh_2NEpo7TZRRrLZSizrsHo"
/// Return:
///     Some(String)   明文, example: "alice"
///     None           沒有底線、Base58 解不開、nonce 長度不對、AEAD 驗不過、
///                    不是合法 UTF-8、或 nonce 與明文對不上（不是 `to_dir_name` 產生的）
pub fn find_dir_name_plaintext(key: &Key32, scope: DirScope<'_>, dir_name: &str) -> Option<String> {
    let (nonce_part, ciphertext_part) = dir_name.split_once(SEPARATOR)?;
    let nonce = bs58::decode(nonce_part).into_vec().ok()?;
    if nonce.len() != NONCE_LEN {
        return None;
    }
    let ciphertext = bs58::decode(ciphertext_part).into_vec().ok()?;
    let plaintext = ChaCha20Poly1305::new(key.as_bytes().into())
        .decrypt(
            Nonce::from_slice(&nonce),
            Payload {
                msg: &ciphertext,
                aad: &scope.aad(),
            },
        )
        .ok()?;
    let plaintext = String::from_utf8(plaintext).ok()?;
    // nonce 是明文導出的，所以「解得開」還不夠：對不上代表這不是 to_dir_name 產生的名字。
    // 少了這一步，同一把金鑰下同一個明文就會有不只一個合法名字，定位與列舉會對不起來。
    (nonce_of(key, scope, &plaintext) == nonce.as_slice()).then_some(plaintext)
}

fn nonce_of(key: &Key32, scope: DirScope<'_>, plaintext: &str) -> [u8; NONCE_LEN] {
    let hash = blake3::keyed_hash(key.as_bytes(), &scope.nonce_input(plaintext));
    let mut nonce = [0u8; NONCE_LEN];
    nonce.copy_from_slice(&hash.as_bytes()[..NONCE_LEN]);
    nonce
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> Key32 {
        Key32([7u8; 32])
    }

    fn account() -> DirScope<'static> {
        DirScope::Account {
            server_host: "localhost:6167",
        }
    }

    #[test]
    fn round_trip_and_deterministic() {
        let first = to_dir_name(&key(), account(), "alice").unwrap();
        let second = to_dir_name(&key(), account(), "alice").unwrap();
        assert_eq!(
            first, second,
            "同一個明文要得到同一個目錄名，login 才定位得到"
        );
        assert!(first.contains(SEPARATOR));
        assert_eq!(
            find_dir_name_plaintext(&key(), account(), &first).as_deref(),
            Some("alice")
        );
    }

    #[test]
    fn different_plaintexts_get_different_nonces() {
        let alice = to_dir_name(&key(), account(), "alice").unwrap();
        let bob = to_dir_name(&key(), account(), "bob").unwrap();
        let alice_nonce = alice.split(SEPARATOR).next().unwrap();
        let bob_nonce = bob.split(SEPARATOR).next().unwrap();
        assert_ne!(
            alice_nonce, bob_nonce,
            "nonce 重用會讓兩份密文 XOR 洩漏明文 XOR"
        );
    }

    #[test]
    fn scopes_do_not_open_each_others_names() {
        let as_server = to_dir_name(&key(), DirScope::Server, "alice").unwrap();
        assert_eq!(
            find_dir_name_plaintext(&key(), account(), &as_server),
            None,
            "server 層的名字不該用 account 層的 aad 解得開"
        );
        let as_account = to_dir_name(&key(), account(), "alice").unwrap();
        assert_eq!(
            find_dir_name_plaintext(&key(), DirScope::Server, &as_account),
            None
        );
    }

    #[test]
    fn an_account_dir_moved_to_another_server_does_not_open() {
        let under_localhost = to_dir_name(&key(), account(), "alice").unwrap();
        let other = DirScope::Account {
            server_host: "matrix.org",
        };
        assert_eq!(
            find_dir_name_plaintext(&key(), other, &under_localhost),
            None,
            "aad 綁著上一層的 host，搬過去就該解不開"
        );
    }

    #[test]
    fn another_key_gets_nothing() {
        let name = to_dir_name(&key(), account(), "alice").unwrap();
        assert_eq!(
            find_dir_name_plaintext(&Key32([9u8; 32]), account(), &name),
            None
        );
    }

    #[test]
    fn malformed_names_are_none_not_panics() {
        let key = key();
        for name in [
            "",
            "alice",
            "_",
            "notbase58!_x",
            "3vQB7B6MrGQZaxCuFg4oh_",
            // nonce 那段長度不對（Base58 解得開，但不是 24 byte）
            "2NEpo7_2NEpo7TZRRrLZSizrsHo",
        ] {
            assert_eq!(
                find_dir_name_plaintext(&key, account(), name),
                None,
                "{name}"
            );
        }
    }

    #[test]
    fn a_name_with_a_foreign_nonce_is_rejected() {
        // 拿 bob 的 nonce 配 alice 的密文：AEAD 會先擋下來，但就算擋不住，nonce 比對也要擋。
        let alice = to_dir_name(&key(), account(), "alice").unwrap();
        let bob = to_dir_name(&key(), account(), "bob").unwrap();
        let spliced = format!(
            "{}{SEPARATOR}{}",
            bob.split(SEPARATOR).next().unwrap(),
            alice.split(SEPARATOR).nth(1).unwrap()
        );
        assert_eq!(find_dir_name_plaintext(&key(), account(), &spliced), None);
    }

    #[test]
    fn empty_plaintext_is_rejected() {
        assert!(to_dir_name(&key(), account(), "").is_err());
    }

    #[test]
    fn a_localpart_too_long_to_encode_is_an_error_not_a_truncation() {
        let long = "a".repeat(200);
        let error = to_dir_name(&key(), account(), &long).unwrap_err();
        assert!(
            format!("{error}").contains("over the 200"),
            "要報錯，🚫 不能截斷——截斷就解不回原本的 localpart 了：{error}"
        );
    }
}

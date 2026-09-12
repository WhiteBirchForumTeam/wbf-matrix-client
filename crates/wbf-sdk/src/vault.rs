//! 本地金鑰庫（local-cache-db.md §4、§5.3、§5.6）：一把 32 byte 主金鑰放 `local.key`，
//! 明文（`Plain`）或被 passphrase 包住（`PassphraseWrapped`）；子金鑰用 BLAKE3 從主金鑰導出，不落地。
//! `session.sealed` 用第三把子金鑰封住 session 與 token。
//!
//! 這裡沒有 SQLite、沒有 matrix-sdk：兩個世界只從 `Vault` 拿各自的子金鑰（§5.3 的那一條線）。
//! 🚫 主金鑰、子金鑰、passphrase 都不印、不進錯誤訊息。
//! 用字：**passphrase** 是解 `local.key` 的那句話；**password** 一律指 Matrix 帳號密碼，這個檔裡沒有它。

use std::path::{Path, PathBuf};

use argon2::{Algorithm, Argon2, Params, Version};
use base64::Engine;
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use serde::{Deserialize, Serialize};
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

use crate::error::SdkError;
use crate::login::Session;

pub const KEY_FILE_NAME: &str = "local.key";
pub const SEALED_SESSION_FILE_NAME: &str = "session.sealed";

/// 子金鑰的 BLAKE3 context（§4）。字串帶版本：換字串就是換金鑰。
const CACHE_KEY_CONTEXT: &str = "wbf-matrix-client cache sqlcipher v1";
const MATRIX_STORE_KEY_CONTEXT: &str = "wbf-matrix-client matrix-sdk store v1";
const SESSION_KEY_CONTEXT: &str = "wbf-matrix-client session v1";
const MEDIA_STORE_KEY_CONTEXT: &str = "wbf-matrix-client media store v1";
const ACCOUNT_DIR_KEY_CONTEXT: &str = "wbf-matrix-client account directory v1";
const ROOM_KEY_BACKUP_KEY_CONTEXT: &str = "wbf-matrix-client room key backup v1";

/// `session.sealed` 的 AEAD 附加資料：綁住用途，拿別的檔的密文換過來解不開。
const SESSION_AAD: &[u8] = b"wbf-matrix-client session.sealed v1";
/// recovery key 的 AEAD 附加資料：跟 session 的密文互換也解不開。
/// 🚫 字串不要改：改了就是換金鑰，已經封好的 recovery key 全部打不開。
const RECOVERY_AAD: &[u8] = b"wbf-matrix-client recovery.sealed v1";
/// `local.key` 包主金鑰的 AEAD 附加資料。
const WRAP_AAD: &[u8] = b"wbf-matrix-client local.key v1";

const KEY_FILE_VERSION: u32 = 1;
const SEALED_VERSION: u32 = 1;

/// Argon2id 預設參數（§4）：64 MiB、3 輪、1 lane。寫進檔裡，之後調高不用遷移。
const ARGON2_M_KIB: u32 = 65536;
const ARGON2_T: u32 = 3;
const ARGON2_P: u32 = 1;
/// 讀檔時的上限（PR #11 審查 rumia 🟡1／salvia 🟡2）：參數來自 `local.key`，一把塞了 `m_kib = 2_000_000` 的檔
/// 每次解鎖吃 2 GiB。超過就拒絕，不算。1 GiB、10 輪、8 lane 遠高於任何合理設定。
const ARGON2_MAX_M_KIB: u32 = 1_048_576;
const ARGON2_MAX_T: u32 = 10;
const ARGON2_MAX_P: u32 = 8;

/// 32 byte 的金鑰，drop 時歸零。主金鑰與每一把子金鑰都是這個型別。
#[derive(Clone, Zeroize, ZeroizeOnDrop)]
pub struct Key32(pub [u8; 32]);

impl Key32 {
    fn random() -> Result<Key32, SdkError> {
        let mut bytes = [0u8; 32];
        getrandom::getrandom(&mut bytes)
            .map_err(|error| SdkError::Io(std::io::Error::other(format!("csprng: {error}"))))?;
        Ok(Key32(bytes))
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl std::fmt::Debug for Key32 {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Key32(<redacted>)")
    }
}

/// 開 vault 時給的東西。`Plain` 的 `local.key` 配 `NoPassphrase`，`PassphraseWrapped` 配 `Passphrase`，配錯就 `Err`（§4）。
pub enum Unlock {
    NoPassphrase,
    /// 🚫 不接受空字串：「沒設 passphrase」是 `Plain` 模式，不是 passphrase 等於空字串。
    /// ⚠️ **原始 bytes，不是字串**（local-cache-db.md §12）：passphrase 只餵給本機的
    /// Argon2id，永遠不出這台機器，所以它可以是 UTF-8 的中文、可以是一個 mp3。
    /// 🚫 不驗 UTF-8、🚫 不去尾換行——那是 `--password-file`（要送給 homeserver）的規則。
    Passphrase(Zeroizing<Vec<u8>>),
}

/// `local.key` 現在是哪一種鎖法。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyMode {
    Plain,
    Passphrase,
}

/// `local.key` 在磁碟上的樣子。`mode` 不認得的值 serde 直接拒絕。
#[derive(Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
enum KeyFile {
    Plain {
        v: u32,
        master: String,
    },
    Passphrase {
        v: u32,
        kdf: KdfParams,
        nonce: String,
        wrapped: String,
    },
}

#[derive(Serialize, Deserialize, Clone)]
struct KdfParams {
    name: String,
    m_kib: u32,
    t: u32,
    p: u32,
    salt: String,
}

#[derive(Serialize, Deserialize)]
struct SealedFile {
    v: u32,
    nonce: String,
    sealed: String,
}

/// 開好的金鑰庫：主金鑰在記憶體、目錄在哪。子金鑰每次要用時導一次。
pub struct Vault {
    dir: PathBuf,
    master: Key32,
    mode: KeyMode,
}

impl Vault {
    /// 建新的 `local.key`。目錄裡已經有一個就拒絕：不會靜默換掉一把還鎖著資料的主金鑰。
    ///
    /// Args:
    ///     dir: example: "<data dir>/wbf-cli"
    ///     unlock: example: Unlock::NoPassphrase
    /// Return:
    ///     Ok(Vault)
    ///     Err(Usage)   已經有 local.key、或 passphrase 是空字串
    pub fn create(dir: &Path, unlock: &Unlock) -> Result<Vault, SdkError> {
        let key_path = dir.join(KEY_FILE_NAME);
        if key_path.exists() {
            return Err(SdkError::Usage(format!(
                "{} already exists; open it instead of creating a new one",
                key_path.display()
            )));
        }
        std::fs::create_dir_all(dir)?;
        let vault = Vault {
            dir: dir.to_path_buf(),
            master: Key32::random()?,
            mode: KeyMode::Plain,
        };
        vault.write_key_file(unlock)
    }

    /// 開既有的 `local.key`。
    ///
    /// Args:
    ///     dir: example: "<data dir>/wbf-cli"
    ///     unlock: example: Unlock::Passphrase(b"hunter2".to_vec().into())
    /// Return:
    ///     Ok(Vault)
    ///     Err(Usage)   沒有 local.key、檔案壞了、模式與 unlock 配不上、passphrase 錯
    pub fn open(dir: &Path, unlock: &Unlock) -> Result<Vault, SdkError> {
        let key_path = dir.join(KEY_FILE_NAME);
        let bytes = std::fs::read(&key_path).map_err(|error| {
            SdkError::Usage(format!("no key file at {}: {error}", key_path.display()))
        })?;
        let file: KeyFile = serde_json::from_slice(&bytes).map_err(|error| {
            SdkError::Usage(format!(
                "key file {} is not recognised: {error}",
                key_path.display()
            ))
        })?;
        let (master, mode) = match (file, unlock) {
            (KeyFile::Plain { v, master }, Unlock::NoPassphrase) => {
                if v != KEY_FILE_VERSION {
                    return Err(SdkError::Usage(format!(
                        "key file version {v} is not supported"
                    )));
                }
                (decode_key32(&master, "master")?, KeyMode::Plain)
            }
            (KeyFile::Plain { .. }, Unlock::Passphrase(_)) => {
                return Err(SdkError::Usage(
                    "this key file has no passphrase; do not pass one".into(),
                ))
            }
            (KeyFile::Passphrase { .. }, Unlock::NoPassphrase) => return Err(SdkError::Usage(
                "this key file is locked with a passphrase; pass --passphrase-file or unlock first"
                    .into(),
            )),
            (
                KeyFile::Passphrase {
                    v,
                    kdf,
                    nonce,
                    wrapped,
                },
                Unlock::Passphrase(passphrase),
            ) => {
                if v != KEY_FILE_VERSION {
                    return Err(SdkError::Usage(format!(
                        "key file version {v} is not supported"
                    )));
                }
                let kek = derive_kek(passphrase, &kdf)?;
                let nonce = decode_base64(&nonce, "nonce")?;
                let wrapped = decode_base64(&wrapped, "wrapped")?;
                let master = XChaCha20Poly1305::new(kek.as_bytes().into())
                    .decrypt(
                        XNonce::from_slice(&nonce),
                        Payload {
                            msg: &wrapped,
                            aad: WRAP_AAD,
                        },
                    )
                    .map_err(|_| SdkError::Usage("wrong passphrase".into()))?;
                let master: [u8; 32] = master.as_slice().try_into().map_err(|_| {
                    SdkError::Usage("key file: wrapped master key has the wrong length".into())
                })?;
                (Key32(master), KeyMode::Passphrase)
            }
        };
        Ok(Vault {
            dir: dir.to_path_buf(),
            master,
            mode,
        })
    }

    /// 只看 `local.key` 是哪種鎖法，不解它。CLI 用這個決定要不要問 passphrase。
    ///
    /// Return:
    ///     Ok(KeyMode)
    ///     Err(Usage)   沒有 local.key、或檔案不認得
    pub fn read_mode(dir: &Path) -> Result<KeyMode, SdkError> {
        let key_path = dir.join(KEY_FILE_NAME);
        let bytes = std::fs::read(&key_path).map_err(|error| {
            SdkError::Usage(format!("no key file at {}: {error}", key_path.display()))
        })?;
        let file: KeyFile = serde_json::from_slice(&bytes).map_err(|error| {
            SdkError::Usage(format!(
                "key file {} is not recognised: {error}",
                key_path.display()
            ))
        })?;
        Ok(match file {
            KeyFile::Plain { .. } => KeyMode::Plain,
            KeyFile::Passphrase { .. } => KeyMode::Passphrase,
        })
    }

    /// 用已經解開的主金鑰接回去（`set_passphrase` 重包 `local.key` 時用）。
    /// ⚠️ 不驗證那把金鑰是不是這個目錄的——呼叫端必須是從同一個 `Vault` 拿到它的。
    pub fn from_master(dir: &Path, master: Key32, mode: KeyMode) -> Vault {
        Vault {
            dir: dir.to_path_buf(),
            master,
            mode,
        }
    }

    /// 換鎖法：設 passphrase、改 passphrase、或拿掉 passphrase。只重寫 `local.key`，DB 與 session 不動（主金鑰沒變）。
    pub fn set_unlock(&mut self, unlock: &Unlock) -> Result<(), SdkError> {
        let rewritten = Vault {
            dir: self.dir.clone(),
            master: self.master.clone(),
            mode: self.mode,
        }
        .write_key_file(unlock)?;
        self.mode = rewritten.mode;
        Ok(())
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn mode(&self) -> KeyMode {
        self.mode
    }

    /// 給 `set_passphrase` 重包用（同一個目錄、同一把主金鑰）。🚫 除此之外不要拿它做別的事。
    pub fn master_key(&self) -> &Key32 {
        &self.master
    }

    /// SQLCipher 的 raw key（cache.db）。
    pub fn cache_key(&self) -> Key32 {
        self.derive(CACHE_KEY_CONTEXT)
    }

    /// matrix-sdk 的 `SqliteStoreConfig::key`（crypto.db、state.db）。
    pub fn matrix_store_key(&self) -> Key32 {
        self.derive(MATRIX_STORE_KEY_CONTEXT)
    }

    /// 媒體檔案空間（§8）。這一版還沒有人用，先導出來讓 context 字串一次定完。
    pub fn media_store_key(&self) -> Key32 {
        self.derive(MEDIA_STORE_KEY_CONTEXT)
    }

    /// 本地的房間金鑰備份（`room_keys`；local-cache-db.md §10.4）：檔案內容的加密與檔名的 keyed hash 都用它。
    pub fn room_key_backup_key(&self) -> Key32 {
        self.derive(ROOM_KEY_BACKUP_KEY_CONTEXT)
    }

    /// 資料目錄裡兩層目錄名的加密（`account_dir`；local-cache-db.md §11.2）。
    /// `s/` 與 `a/` 共用這一把，靠 aad 分。
    pub fn account_dir_key(&self) -> Key32 {
        self.derive(ACCOUNT_DIR_KEY_CONTEXT)
    }

    fn session_key(&self) -> Key32 {
        self.derive(SESSION_KEY_CONTEXT)
    }

    fn derive(&self, context: &str) -> Key32 {
        Key32(blake3::derive_key(context, self.master.as_bytes()))
    }

    /// 把 session（含 access_token）封進 `path`（CLI 放帳號目錄的 `session.sealed`）。
    ///
    /// Args:
    ///     path: example: "<data dir>/s/<b58>_<b58>/a/<b58>_<b58>/session.sealed"
    pub fn seal_session(&self, path: &Path, session: &Session) -> Result<(), SdkError> {
        let plaintext = Zeroizing::new(serde_json::to_vec(session).expect("Session serializes"));
        let nonce = random_nonce()?;
        let sealed = XChaCha20Poly1305::new(self.session_key().as_bytes().into())
            .encrypt(
                XNonce::from_slice(&nonce),
                Payload {
                    msg: &plaintext,
                    aad: SESSION_AAD,
                },
            )
            .map_err(|_| SdkError::Io(std::io::Error::other("seal session")))?;
        let file = SealedFile {
            v: SEALED_VERSION,
            nonce: encode_base64(&nonce),
            sealed: encode_base64(&sealed),
        };
        write_private(
            path,
            &serde_json::to_vec_pretty(&file).expect("SealedFile serializes"),
        )
    }

    /// 把 `key-backup recovery` 產生的那串 recovery key 封進 `path`（維護者 2026-09-09）。
    ///
    /// 用第三把子金鑰（跟 `session.sealed` 同一把，AAD 不同所以兩者的密文換不過去）。
    ///
    /// ⚠️ 這是**方便性的保管**，不是「使用者擁有」的證明——它跟 crypto store 在同一台機器上，
    /// 一起被拿走就一起沒了。閘門（CLI 的 `refuse_if_history_would_be_lost`）拿它當第 2 關，
    /// 🚫 不問使用者（local-cache-db.md §10.8）。
    ///
    /// Args:
    ///     path: example: "<data dir>/r/<b58 nonce>_<b58 密文>"
    ///     recovery_key: 🚫 不印、不 log
    pub fn seal_recovery_key(&self, path: &Path, recovery_key: &str) -> Result<(), SdkError> {
        let nonce = random_nonce()?;
        let sealed = XChaCha20Poly1305::new(self.session_key().as_bytes().into())
            .encrypt(
                XNonce::from_slice(&nonce),
                Payload {
                    msg: recovery_key.as_bytes(),
                    aad: RECOVERY_AAD,
                },
            )
            .map_err(|_| SdkError::Io(std::io::Error::other("seal recovery key")))?;
        let file = SealedFile {
            v: SEALED_VERSION,
            nonce: encode_base64(&nonce),
            sealed: encode_base64(&sealed),
        };
        write_private(
            path,
            &serde_json::to_vec_pretty(&file).expect("SealedFile serializes"),
        )
    }

    /// Return:
    ///     Ok(Some(String))   封著的 recovery key
    ///     Ok(None)           沒有這個檔（這個帳號還沒跑過 `key-backup recovery`）
    ///     Err(Usage)         檔案壞了、或不是這把主金鑰封的
    pub fn unseal_recovery_key(&self, path: &Path) -> Result<Option<Zeroizing<String>>, SdkError> {
        let bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let file: SealedFile = serde_json::from_slice(&bytes).map_err(|error| {
            SdkError::Usage(format!("{} is not readable: {error}", path.display()))
        })?;
        if file.v != SEALED_VERSION {
            return Err(SdkError::Usage(format!(
                "{} has version {}, this build understands {SEALED_VERSION}",
                path.display(),
                file.v
            )));
        }
        let nonce = decode_base64(&file.nonce, "nonce")?;
        let sealed = decode_base64(&file.sealed, "sealed")?;
        let plaintext = XChaCha20Poly1305::new(self.session_key().as_bytes().into())
            .decrypt(
                XNonce::from_slice(&nonce),
                Payload {
                    msg: &sealed,
                    aad: RECOVERY_AAD,
                },
            )
            .map_err(|_| {
                SdkError::Usage(format!(
                    "cannot open {}; it was sealed with another key file",
                    path.display()
                ))
            })?;
        let text = String::from_utf8(plaintext)
            .map_err(|_| SdkError::Usage(format!("{} does not hold text", path.display())))?;
        Ok(Some(Zeroizing::new(text)))
    }

    /// Return:
    ///     Ok(Some(Session))  `path` 存在而且解得開
    ///     Ok(None)           沒有這個檔
    ///     Err(Usage)         檔案壞了、或不是這把主金鑰封的
    pub fn unseal_session(&self, path: &Path) -> Result<Option<Session>, SdkError> {
        let bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let file: SealedFile = serde_json::from_slice(&bytes).map_err(|error| {
            SdkError::Usage(format!("{} is not recognised: {error}", path.display()))
        })?;
        if file.v != SEALED_VERSION {
            return Err(SdkError::Usage(format!(
                "{} version {} is not supported",
                path.display(),
                file.v
            )));
        }
        let nonce = decode_base64(&file.nonce, "nonce")?;
        let sealed = decode_base64(&file.sealed, "sealed")?;
        let plaintext = Zeroizing::new(
            XChaCha20Poly1305::new(self.session_key().as_bytes().into())
                .decrypt(
                    XNonce::from_slice(&nonce),
                    Payload {
                        msg: &sealed,
                        aad: SESSION_AAD,
                    },
                )
                .map_err(|_| {
                    SdkError::Usage(format!(
                        "{} was not sealed by this key file; run `login` again",
                        path.display()
                    ))
                })?,
        );
        let session = serde_json::from_slice(&plaintext).map_err(|error| {
            SdkError::Usage(format!("{} content is broken: {error}", path.display()))
        })?;
        Ok(Some(session))
    }

    pub fn delete_sealed_session(&self, path: &Path) -> Result<(), SdkError> {
        match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    fn write_key_file(mut self, unlock: &Unlock) -> Result<Vault, SdkError> {
        let file = match unlock {
            Unlock::NoPassphrase => {
                self.mode = KeyMode::Plain;
                KeyFile::Plain {
                    v: KEY_FILE_VERSION,
                    master: encode_base64(self.master.as_bytes()),
                }
            }
            Unlock::Passphrase(passphrase) => {
                if passphrase.is_empty() {
                    return Err(SdkError::Usage(
                        "passphrase must not be empty; use `remove-passphrase` for no passphrase"
                            .into(),
                    ));
                }
                let mut salt = [0u8; 16];
                getrandom::getrandom(&mut salt).map_err(|error| {
                    SdkError::Io(std::io::Error::other(format!("csprng: {error}")))
                })?;
                let kdf = KdfParams {
                    name: "argon2id".into(),
                    m_kib: ARGON2_M_KIB,
                    t: ARGON2_T,
                    p: ARGON2_P,
                    salt: encode_base64(&salt),
                };
                let kek = derive_kek(passphrase, &kdf)?;
                let nonce = random_nonce()?;
                let wrapped = XChaCha20Poly1305::new(kek.as_bytes().into())
                    .encrypt(
                        XNonce::from_slice(&nonce),
                        Payload {
                            msg: self.master.as_bytes(),
                            aad: WRAP_AAD,
                        },
                    )
                    .map_err(|_| SdkError::Io(std::io::Error::other("wrap master key")))?;
                self.mode = KeyMode::Passphrase;
                KeyFile::Passphrase {
                    v: KEY_FILE_VERSION,
                    kdf,
                    nonce: encode_base64(&nonce),
                    wrapped: encode_base64(&wrapped),
                }
            }
        };
        write_private(
            &self.dir.join(KEY_FILE_NAME),
            &serde_json::to_vec_pretty(&file).expect("KeyFile serializes"),
        )?;
        Ok(self)
    }
}

/// Argon2id 從 passphrase 導 KEK。參數從檔裡來，所以舊檔用舊參數解得開。
fn derive_kek(passphrase: &[u8], kdf: &KdfParams) -> Result<Key32, SdkError> {
    if kdf.name != "argon2id" {
        return Err(SdkError::Usage(format!(
            "key file uses kdf {:?}, which this build does not know",
            kdf.name
        )));
    }
    if kdf.m_kib > ARGON2_MAX_M_KIB || kdf.t > ARGON2_MAX_T || kdf.p > ARGON2_MAX_P {
        return Err(SdkError::Usage(format!(
            "key file kdf params (m_kib {}, t {}, p {}) exceed the limits ({ARGON2_MAX_M_KIB}, {ARGON2_MAX_T}, {ARGON2_MAX_P}); refusing to unlock",
            kdf.m_kib, kdf.t, kdf.p
        )));
    }
    let salt = decode_base64(&kdf.salt, "salt")?;
    let params = Params::new(kdf.m_kib, kdf.t, kdf.p, Some(32))
        .map_err(|error| SdkError::Usage(format!("key file kdf params: {error}")))?;
    let mut kek = Key32([0u8; 32]);
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
        .hash_password_into(passphrase, &salt, &mut kek.0)
        .map_err(|error| SdkError::Usage(format!("argon2: {error}")))?;
    Ok(kek)
}

fn random_nonce() -> Result<[u8; 24], SdkError> {
    let mut nonce = [0u8; 24];
    getrandom::getrandom(&mut nonce)
        .map_err(|error| SdkError::Io(std::io::Error::other(format!("csprng: {error}"))))?;
    Ok(nonce)
}

fn encode_base64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn decode_base64(text: &str, field: &str) -> Result<Vec<u8>, SdkError> {
    base64::engine::general_purpose::STANDARD
        .decode(text)
        .map_err(|error| SdkError::Usage(format!("key file field {field}: {error}")))
}

fn decode_key32(text: &str, field: &str) -> Result<Key32, SdkError> {
    let bytes = Zeroizing::new(decode_base64(text, field)?);
    let array: [u8; 32] = bytes
        .as_slice()
        .try_into()
        .map_err(|_| SdkError::Usage(format!("key file field {field}: expected 32 bytes")))?;
    Ok(Key32(array))
}

/// 含金鑰或 token 的檔：Unix 0600 建立；Windows 靠使用者目錄的 ACL（CLI 規格 §5）。
/// 先寫到同目錄的暫存檔再 rename：寫到一半斷電不會留下半個 `local.key`。
pub fn write_private(path: &Path, bytes: &[u8]) -> Result<(), SdkError> {
    use std::io::Write;
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)?;
    }
    // 暫存檔名帶 pid：兩個 process 同時寫同一個檔不會競寫同一個暫存檔（PR #11 審查 rumia 🟢3）。
    let scratch_path = path.with_extension(format!(
        "{}.{}.tmp",
        path.extension()
            .and_then(|extension| extension.to_str())
            .unwrap_or(""),
        std::process::id()
    ));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&scratch_path)?;
    // mode() 只在建立新檔時生效；覆寫既有檔（舊版留下的 0644）權限不會變，這裡無條件再設一次。
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    // Windows 的 rename 不覆蓋既有檔，先移走。
    if path.exists() {
        std::fs::remove_file(path)?;
    }
    std::fs::rename(&scratch_path, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("wbf-vault-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn sample_session() -> Session {
        Session {
            server: "http://localhost:6167".into(),
            user_id: "@alice:localhost".into(),
            device_id: "DEV".into(),
            access_token: "syt_secret".into(),
            store_dir: None,
        }
    }

    #[test]
    fn plain_roundtrip_and_derived_keys_are_stable() {
        let dir = scratch_dir("plain");
        let created = Vault::create(&dir, &Unlock::NoPassphrase).unwrap();
        let opened = Vault::open(&dir, &Unlock::NoPassphrase).unwrap();
        assert_eq!(
            created.master_key().as_bytes(),
            opened.master_key().as_bytes()
        );
        assert_eq!(
            created.cache_key().as_bytes(),
            opened.cache_key().as_bytes()
        );
        assert_ne!(
            opened.cache_key().as_bytes(),
            opened.matrix_store_key().as_bytes()
        );
        assert_ne!(
            opened.matrix_store_key().as_bytes(),
            opened.session_key().as_bytes()
        );
        assert_eq!(opened.mode(), KeyMode::Plain);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn create_refuses_to_overwrite() {
        let dir = scratch_dir("overwrite");
        Vault::create(&dir, &Unlock::NoPassphrase).unwrap();
        assert!(Vault::create(&dir, &Unlock::NoPassphrase).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn passphrase_mode_needs_the_right_passphrase() {
        let dir = scratch_dir("passphrase");
        let passphrase = Unlock::Passphrase(b"hunter2".to_vec().into());
        let created = Vault::create(&dir, &passphrase).unwrap();
        assert_eq!(created.mode(), KeyMode::Passphrase);
        let opened = Vault::open(&dir, &passphrase).unwrap();
        assert_eq!(
            created.master_key().as_bytes(),
            opened.master_key().as_bytes()
        );
        assert!(Vault::open(&dir, &Unlock::Passphrase(b"hunter3".to_vec().into())).is_err());
        assert!(Vault::open(&dir, &Unlock::NoPassphrase).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn plain_mode_rejects_a_passphrase_and_empty_passphrase_is_not_a_passphrase() {
        let dir = scratch_dir("mismatch");
        Vault::create(&dir, &Unlock::NoPassphrase).unwrap();
        assert!(Vault::open(&dir, &Unlock::Passphrase(b"x".to_vec().into())).is_err());
        let dir2 = scratch_dir("empty");
        assert!(Vault::create(&dir2, &Unlock::Passphrase(Vec::new().into())).is_err());
        assert!(!dir2.join(KEY_FILE_NAME).exists());
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&dir2);
    }

    #[test]
    fn set_unlock_keeps_the_master_key() {
        let dir = scratch_dir("rewrap");
        let mut vault = Vault::create(&dir, &Unlock::NoPassphrase).unwrap();
        let before = vault.master_key().clone();
        let sealed = dir.join(SEALED_SESSION_FILE_NAME);
        vault.seal_session(&sealed, &sample_session()).unwrap();
        vault
            .set_unlock(&Unlock::Passphrase(b"pw".to_vec().into()))
            .unwrap();
        let reopened = Vault::open(&dir, &Unlock::Passphrase(b"pw".to_vec().into())).unwrap();
        assert_eq!(before.as_bytes(), reopened.master_key().as_bytes());
        // session.sealed 沒動，還解得開。
        assert_eq!(
            reopened
                .unseal_session(&sealed)
                .unwrap()
                .unwrap()
                .access_token,
            "syt_secret"
        );
        vault.set_unlock(&Unlock::NoPassphrase).unwrap();
        assert!(Vault::open(&dir, &Unlock::NoPassphrase).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sealed_session_roundtrip_and_other_key_cannot_open() {
        let dir = scratch_dir("session");
        let vault = Vault::create(&dir, &Unlock::NoPassphrase).unwrap();
        let sealed = dir
            .join("accounts")
            .join("alice")
            .join(SEALED_SESSION_FILE_NAME);
        assert!(vault.unseal_session(&sealed).unwrap().is_none());
        vault.seal_session(&sealed, &sample_session()).unwrap();
        let raw = std::fs::read_to_string(&sealed).unwrap();
        assert!(!raw.contains("syt_secret"));
        assert_eq!(
            vault.unseal_session(&sealed).unwrap().unwrap().user_id,
            "@alice:localhost"
        );
        let other = Vault::from_master(&dir, Key32([7u8; 32]), KeyMode::Plain);
        assert!(other.unseal_session(&sealed).is_err());
        vault.delete_sealed_session(&sealed).unwrap();
        assert!(vault.unseal_session(&sealed).unwrap().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 這些字串一旦進過任何一個已存在的 `local.key`／`session.sealed`，就是檔案格式的一部分：
    /// 改一個字，舊 vault 會靜默導出不同的子金鑰，資料等於鎖死（PR #11 審查 rumia 🟢2）。
    /// 這裡失敗＝有人改了格式，要換版本字串（v2）而不是改 v1。
    #[test]
    fn context_and_aad_strings_are_frozen() {
        assert_eq!(CACHE_KEY_CONTEXT, "wbf-matrix-client cache sqlcipher v1");
        assert_eq!(
            MATRIX_STORE_KEY_CONTEXT,
            "wbf-matrix-client matrix-sdk store v1"
        );
        assert_eq!(SESSION_KEY_CONTEXT, "wbf-matrix-client session v1");
        assert_eq!(MEDIA_STORE_KEY_CONTEXT, "wbf-matrix-client media store v1");
        assert_eq!(
            ACCOUNT_DIR_KEY_CONTEXT,
            "wbf-matrix-client account directory v1"
        );
        assert_eq!(
            ROOM_KEY_BACKUP_KEY_CONTEXT,
            "wbf-matrix-client room key backup v1"
        );
        assert_eq!(SESSION_AAD, b"wbf-matrix-client session.sealed v1");
        assert_eq!(WRAP_AAD, b"wbf-matrix-client local.key v1");
        // 導出的子金鑰也釘住：主金鑰全 7 時 cache key 的前 4 byte。改 BLAKE3 用法或 context 都會炸。
        let vault = Vault::from_master(Path::new("."), Key32([7u8; 32]), KeyMode::Plain);
        let cache = vault.cache_key();
        let matrix = vault.matrix_store_key();
        assert_ne!(cache.as_bytes(), matrix.as_bytes());
        assert_eq!(
            blake3::derive_key("wbf-matrix-client cache sqlcipher v1", &[7u8; 32]),
            *cache.as_bytes()
        );
    }

    #[test]
    fn oversized_argon2_params_in_key_file_are_refused() {
        let dir = scratch_dir("argon2-limit");
        Vault::create(&dir, &Unlock::Passphrase(b"pw".to_vec().into())).unwrap();
        let path = dir.join(KEY_FILE_NAME);
        let text = std::fs::read_to_string(&path)
            .unwrap()
            .replace("\"m_kib\": 65536", "\"m_kib\": 2000000");
        assert!(text.contains("2000000"), "fixture did not rewrite m_kib");
        std::fs::write(&path, text).unwrap();
        let error = match Vault::open(&dir, &Unlock::Passphrase(b"pw".to_vec().into())) {
            Ok(_) => panic!("oversized argon2 params were accepted"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("exceed the limits"), "{error}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unknown_mode_and_version_are_rejected() {
        let dir = scratch_dir("unknown");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(KEY_FILE_NAME),
            r#"{"v":1,"mode":"","master":"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="}"#,
        )
        .unwrap();
        assert!(Vault::open(&dir, &Unlock::NoPassphrase).is_err());
        std::fs::write(
            dir.join(KEY_FILE_NAME),
            r#"{"v":2,"mode":"plain","master":"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="}"#,
        )
        .unwrap();
        assert!(Vault::open(&dir, &Unlock::NoPassphrase).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}

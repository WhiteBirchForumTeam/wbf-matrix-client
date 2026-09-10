//! `wbf-core`：常駐狀態。解鎖一次的 vault、資料目錄的佈局、多帳號。
//!
//! 這一層在 [`architecture-v2.md`](../../../docs/design/architecture-v2.md) §7 的位置：
//!
//! ```text
//! 前端（rpc-cli / Desktop / Android / Python）
//!     │  RPC（加密的 JSON over WS）      ← wbf-daemon 開的門，🚫 不在這個 crate
//! wbf-core     ← 你在這裡：狀態與邏輯，不知道有誰在跟它講話
//!     │
//! wbf-sdk      ← 純 library：協議、chunk 加解密、cache.db、媒體池、vault、matrix backend
//! ```
//!
//! **為什麼跟 `wbf-daemon` 分開**：RPC 還是 uniffi 那個決策（§8 第 7 點）還沒定，
//! 而這一層**兩條路都要**。所以它先做，🚫 它不能知道自己被誰包起來。
//!
//! # 公開介面的紀律（§7，這是現在唯一要守的）
//!
//! 🚫 **不能假設「同程序」**——之後包 RPC 或包 uniffi 都不該回來改這裡：
//!
//! | 規矩 | 為什麼 |
//! |---|---|
//! | 方法收 `&self` | 呼叫端可能是好幾條連線同時進來 |
//! | 參數與回傳用簡單型別 | 要序列化成 JSON，或跨 FFI 邊界 |
//! | 事件用 channel，不用回呼引用 | 回呼綁著呼叫端的生命週期，跨程序沒有那種東西 |
//! | 自己持有 runtime，不要求宿主提供 | Android 的宿主是 JVM，沒有 tokio |
//! | 🚫 公開介面上不要有複雜生命週期、trait object、`impl Trait` | 那些過不了 FFI，也序列化不了 |
//!
//! # 這一層**不**做的
//!
//! - 🚫 **不問終端**：passphrase 一律由呼叫端餵進來（§4.5——那樣 Desktop 與 Android
//!   才解得開）。⚠️ 所以 `unlock` 吃的是 bytes，不是「檔案路徑」也不是「去問使用者」。
//! - 🚫 **不碰 `unlock.ticket`**：那是「一個命令一個程序」的妥協（local-cache-db §4 自己
//!   標記過），常駐之後**整個消失**（§1）。ticket 留在 rpc-cli 那邊，直到 daemon 接手。
//! - 🚫 **不管 UI 狀態、不管顯示格式、不代前端做決定**（§3）。

pub mod accounts;
pub mod recovery;

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use wbf_sdk::vault::{KeyMode, Vault};
use wbf_sdk::{SdkError, Unlock};

pub use accounts::{AccountDir, AccountSummary, DataDirMap};

/// 一個資料目錄的常駐狀態。
///
/// **解鎖一次**：`unlock` 成功之後主金鑰活在這個物件裡，直到它被丟掉。
/// ⚠️ 這正是 `unlock.ticket`（明文主金鑰落地 15 分鐘）消失的原因——
/// 常駐之後沒有人需要把它寫到磁碟上。
///
/// 🚫 `Core` 自己**不會**去問終端、不讀 passphrase 檔、不寫 ticket。那些是前端的事。
pub struct Core {
    data_dir: PathBuf,
    /// 解一次就留著。`OnceLock` 讓 `unlock` 收 `&self`（§7 的紀律）。
    vault: OnceLock<Vault>,
}

impl Core {
    /// 認一個資料目錄，**還沒解鎖**。
    ///
    /// 🚫 這一步不碰磁碟：`local.key` 在不在、是哪種模式，要問 [`Core::key_mode`]。
    /// 這樣呼叫端可以先建好物件，再決定怎麼解鎖（RPC 進來、或前端自己讀檔）。
    ///
    /// Args:
    ///     data_dir: example: "<data dir>"
    pub fn open(data_dir: &Path) -> Core {
        Core {
            data_dir: data_dir.to_path_buf(),
            vault: OnceLock::new(),
        }
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    /// 這個資料目錄的 `local.key` 要不要 passphrase。
    ///
    /// Return:
    ///     Ok(Some(KeyMode))   有 `local.key`：`Plain` 不用問，`Passphrase` 要
    ///     Ok(None)            還沒有 `local.key`（沒 `login` 過）
    ///     Err(Usage)          有檔但讀不懂
    pub fn key_mode(&self) -> Result<Option<KeyMode>, SdkError> {
        if !self.data_dir.join(wbf_sdk::vault::KEY_FILE_NAME).exists() {
            return Ok(None);
        }
        Vault::read_mode(&self.data_dir).map(Some)
    }

    pub fn is_unlocked(&self) -> bool {
        self.vault.get().is_some()
    }

    /// 解鎖。**冪等**：已經解開就直接回 `Ok`，🚫 不重跑 Argon2。
    ///
    /// ⚠️ `passphrase` 是**原始 bytes**（local-cache-db §12）：它可以是中文、可以是一個
    /// mp3。🚫 這一層不驗 UTF-8、不去尾換行——那些是「怎麼拿到 passphrase」的問題，
    /// 屬於前端（`rpc-cli` 讀檔或問終端，Desktop 從輸入框，Android 從對話框）。
    ///
    /// Args:
    ///     passphrase: `Plain` 模式傳 `None`, example: Some(b"hunter2".as_slice())
    /// Return:
    ///     Ok(())       解開了，或本來就開著
    ///     Err(Usage)   沒有 `local.key`、passphrase 錯、或模式配不上（`Plain` 卻給了 passphrase）
    pub fn unlock(&self, passphrase: Option<&[u8]>) -> Result<(), SdkError> {
        if self.is_unlocked() {
            return Ok(());
        }
        if self.key_mode()?.is_none() {
            return Err(SdkError::Usage(format!(
                "no key file in {}; log in first",
                self.data_dir.display()
            )));
        }
        let unlock = match passphrase {
            Some(bytes) => Unlock::Passphrase(zeroize::Zeroizing::new(bytes.to_vec())),
            None => Unlock::NoPassphrase,
        };
        let vault = Vault::open(&self.data_dir, &unlock)?;
        let _ = self.vault.set(vault);
        Ok(())
    }

    /// 已經解開的 vault。
    ///
    /// Return:
    ///     Ok(&Vault)
    ///     Err(Usage)   還沒解鎖——呼叫端要先叫 `unlock`（RPC 那邊回 `locked`，§4.5）
    pub fn vault(&self) -> Result<&Vault, SdkError> {
        self.vault.get().ok_or_else(|| {
            SdkError::Usage(format!(
                "the vault in {} is locked; unlock it first",
                self.data_dir.display()
            ))
        })
    }

    /// 把一把**已經開好**的 vault 放進來，回傳最後裝在裡面的那把。
    ///
    /// 兩個呼叫端，都是 `unlock` 涵蓋不了的：
    ///
    /// - **`login`**：`local.key` 還不存在，vault 是**建**出來的。🚫 `Core` 不長出「建」
    ///   的那半——那需要「要不要設 passphrase」的政策，是前端的決定（§3）。
    /// - **rpc-cli 的 `unlock.ticket`**：從落地的主金鑰直接組 vault，沒有 passphrase 可餵。
    ///   ⚠️ 那是「一個命令一個程序」的妥協，daemon 接手之後整條消失。
    ///
    /// **冪等**，跟 [`Core::unlock`] 一樣：已經有一把就把傳進來的丟掉、回原本那把。
    /// 📎 兩把都來自同一個資料目錄，所以是同一把主金鑰——丟掉的那把沒有資訊。
    pub fn adopt_unlocked_vault(&self, vault: Vault) -> &Vault {
        let _ = self.vault.set(vault);
        self.vault.get().expect("just set, or already there")
    }

    /// 資料目錄現在有什麼（`s/*/a/*` 兩層與 `r/`）。⚠️ **快照，不是快取**：
    /// 會刪檔的命令要當場刷新（accounts 模組的模組註解寫了為什麼）。
    pub fn refresh_data_dir_map(&self) -> Result<DataDirMap, SdkError> {
        accounts::refresh_data_dir_map(&self.data_dir, self.vault()?)
    }

    /// 這台機器上的每個帳號。
    pub fn list_accounts(&self) -> Result<Vec<AccountSummary>, SdkError> {
        self.refresh_data_dir_map()?.list_accounts(self.vault()?)
    }

    /// 使用者打的那串 → 帳號目錄。當場刷新一次再比對（大小寫不敏感）。
    ///
    /// Args:
    ///     user: mxid 或 localpart, example: "@alice:localhost"
    ///     server: example: Some("http://localhost:6167")
    pub fn find_account(&self, user: &str, server: Option<&str>) -> Result<AccountDir, SdkError> {
        self.refresh_data_dir_map()?.find_account_dir(user, server)
    }

    /// `current` 指到的那個帳號。
    ///
    /// Return:
    ///     Ok(AccountDir)
    ///     Err(Usage)   沒登入過、或 `current` 指到的目錄解不開
    pub fn current_account(&self) -> Result<AccountDir, SdkError> {
        let current = accounts::read_current(&self.data_dir)?.ok_or_else(|| {
            SdkError::Usage(format!(
                "no current account in {}; log in first",
                self.data_dir.display()
            ))
        })?;
        let key = self.vault()?.account_dir_key();
        accounts::find_account_of_current(&self.data_dir, &key, &current).ok_or_else(|| {
            SdkError::Usage(format!(
                "the current account has no readable directory in {}; log in again",
                self.data_dir.display()
            ))
        })
    }

    /// 這台機器保管著誰的 recovery key（`r/`，logout 不碰它）。
    pub fn list_recovery_key_users(&self) -> Result<Vec<String>, SdkError> {
        recovery::list_users(&self.data_dir, self.vault()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("wbf-core-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn an_empty_data_dir_has_no_key_file_and_will_not_pretend_otherwise() {
        let dir = scratch("empty");
        let core = Core::open(&dir);
        assert_eq!(core.key_mode().unwrap(), None);
        assert!(!core.is_unlocked());
        // 🚫 沒有 local.key 就是解不開，不是「解開了但空的」。
        assert!(core.unlock(None).is_err());
        // 每個要 vault 的方法都該擋下來，而不是拿一個空的往下走。
        assert!(core.vault().is_err());
        assert!(core.list_accounts().is_err());
        assert!(core.current_account().is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unlocking_is_once_and_idempotent() {
        let dir = scratch("once");
        let vault = Vault::create(&dir, &Unlock::NoPassphrase).unwrap();
        drop(vault);

        let core = Core::open(&dir);
        assert_eq!(core.key_mode().unwrap(), Some(KeyMode::Plain));
        core.unlock(None).unwrap();
        assert!(core.is_unlocked());
        // 冪等：再叫一次不會重跑 Argon2、也不會換掉那把 vault。
        core.unlock(None).unwrap();
        assert!(core.list_accounts().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_binary_passphrase_goes_in_as_raw_bytes() {
        // local-cache-db §12：passphrase 是任意 bytes，這一層一個都不動。
        let dir = scratch("bytes");
        let passphrase = "早安\u{0}世界".as_bytes();
        Vault::create(
            &dir,
            &Unlock::Passphrase(zeroize::Zeroizing::new(passphrase.to_vec())),
        )
        .unwrap();

        let core = Core::open(&dir);
        assert_eq!(core.key_mode().unwrap(), Some(KeyMode::Passphrase));
        // 🚫 少一個 byte 就是另一句話。
        assert!(core.unlock(Some("早安\u{0}世界\n".as_bytes())).is_err());
        assert!(!core.is_unlocked(), "解失敗不該留下半開的狀態");
        core.unlock(Some(passphrase)).unwrap();
        assert!(core.is_unlocked());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_passphrase_vault_refuses_to_open_without_one() {
        let dir = scratch("needs-pp");
        Vault::create(
            &dir,
            &Unlock::Passphrase(zeroize::Zeroizing::new(b"pw".to_vec())),
        )
        .unwrap();
        let core = Core::open(&dir);
        // 模式配不上就拒絕，🚫 不要靜默當成 plain。
        assert!(core.unlock(None).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}

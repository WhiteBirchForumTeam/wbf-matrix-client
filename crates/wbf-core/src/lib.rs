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
//! - 🚫 **不寫任何「解鎖狀態」到磁碟**：以前 CLI 有一張 `unlock.ticket`（明文主金鑰落地 15 分鐘），
//!   2026-09-13 整條拿掉了（local-cache-db §4）。解鎖狀態只活在這個物件裡。
//! - 🚫 **不管 UI 狀態、不管顯示格式、不代前端做決定**（§3）。

// ⚠️ 這兩個是**內部**：它們的型別（`DataDirMap`、`AccountDir`）帶著路徑與 `Vault`，
// 跨不了 RPC 也綁不了 uniffi。公開面只走 `Core` 的方法與可序列化的 DTO
//（PR #24 審查 cirno🔴）。
mod account_ops;
mod accounts;
mod backup_ops;
pub mod conf;
mod error;
pub mod event;
mod handles;
/// 「現在跑的是哪一個工作」——事件的歸屬（rpc-spec §4）。
pub mod job;
mod login_ops;
mod media_ops;
mod misc_ops;
mod recovery;
mod rooms_ops;
mod server_cache;
mod session_ops;
mod sync_ops;
mod upload_ops;

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use wbf_sdk::vault::{KeyMode, Vault};
use wbf_sdk::Unlock;

pub use account_ops::{AccountStatus, SwitchResult, WhoAmI};
pub use accounts::AccountSummary;
use accounts::{AccountDir, DataDirMap};
pub use backup_ops::{BackupStatusReport, ImportResult, RecoveryStateReport, UploadResult};
pub use error::{CoreError, CoreErrorKind};
use event::EventSink;
pub use event::{CoreEvent, SyncState};
pub use login_ops::LoginResult;
pub use media_ops::{DirectDownloadResult, DownloadResult, MediaGcReport, MediaStats};
pub use misc_ops::{MediaInfo, SeekResult, SeekSummary, ServerHello, UploadStatusReport};
pub use rooms_ops::{
    cipher_for_plaintext_room, FileEntry, FilePage, HistoryQuery, MessagePage, SyncMode,
};
pub use session_ops::{DestroyResult, LogoutResult};
pub use sync_ops::{watch_mode_from_name, RecentSummary, WatchMode, WatchSummary};
pub use upload_ops::{SendFileResult, UploadRequest};

/// 幾乎每個操作都要回答的三件事。
///
/// 📎 把它們綁成一個型別不只是為了少打字：**RPC 的 `params` 就是這個形狀**
/// （§4.6），所以 daemon 那邊直接反序列化成它，🚫 不必再拆成一串位置參數。
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Target {
    /// 對哪個帳號動作。**`None` = 用 `current`**。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// 哪台 server：同名 localpart 在多個 server 時消歧，或覆蓋 session 裡那個。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server: Option<String>,
    /// conf 的 `SERVER_BACKUP`（CLI 規格 §10）。
    ///
    /// ⚠️ 由**呼叫端**帶進來：core 不讀 conf，那是「代前端做決定」（§3）。
    /// 🚫 預設是 `false`，但那只是 `Default` 的值——真正的預設（開著）在前端那邊，
    /// 因為「認不得的值落到安全值」是 §10.4 的規矩，不是這一層的。
    #[serde(default)]
    pub server_backup: bool,
}

impl Target {
    /// `current` 帳號、不覆蓋 server。
    pub fn current(server_backup: bool) -> Target {
        Target {
            user: None,
            server: None,
            server_backup,
        }
    }

    pub(crate) fn user(&self) -> Option<&str> {
        self.user.as_deref()
    }

    pub(crate) fn server(&self) -> Option<&str> {
        self.server.as_deref()
    }
}

/// 一個資料目錄的常駐狀態。
///
/// **解鎖一次**：`unlock` 成功之後主金鑰活在這個物件裡，直到它被丟掉。
/// ⚠️ 這正是 `unlock.ticket`（明文主金鑰落地 15 分鐘）不再存在的原因——
/// 常駐之後沒有人需要把它寫到磁碟上（local-cache-db §4，2026-09-13 拿掉）。
///
/// 🚫 `Core` 自己**不會**去問終端、不讀 passphrase 檔。那些是前端的事。
pub struct Core {
    data_dir: PathBuf,
    /// 解一次就留著。`OnceLock` 讓 `unlock` 收 `&self`（§7 的紀律）。
    vault: OnceLock<Vault>,
    /// core 往外講話的唯一管道（§7：事件用 channel）。🚫 core 不印東西。
    pub(crate) events: EventSink,
    /// 一個 server dir 一份：那個 `cache.db` 的**唯一寫入者**與讀連線
    /// （`server_cache`、daemon-runtime §2）。⚠️ 開一次就留著 ——
    /// 每次重開要付 SQLCipher 導金鑰的成本，而且**多個寫入者就沒有順序可言**。
    pub(crate) server_caches: std::sync::Mutex<
        std::collections::HashMap<PathBuf, std::sync::Arc<crate::server_cache::ServerCache>>,
    >,
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
            events: EventSink::new(),
            server_caches: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    /// 所有 `cache.db` 寫入者加起來還有幾件在排隊。
    ///
    /// Return:
    ///     usize  0 = 都寫完了；⚠️ 一直漲就是**寫得比收得慢**（daemon-runtime §2.2）
    ///
    /// ⭐ queue 沒有上限是刻意的（丟掉已經收到的事件比慢更糟），所以它**必須看得見** ——
    /// daemon 把這個數字放進 `daemon.info`。
    pub fn cache_queue_len(&self) -> usize {
        self.server_caches
            .lock()
            .expect("the server-cache registry is never poisoned")
            .values()
            .map(|cache| cache.queued())
            .sum()
    }

    /// 訂閱 core 的事件（進度之類）。⚠️ 訂閱**之前**發生的收不到。
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<CoreEvent> {
        self.events.subscribe()
    }

    /// 這個資料目錄的 `local.key` 要不要 passphrase。
    ///
    /// Return:
    ///     Ok(Some(KeyMode))   有 `local.key`：`Plain` 不用問，`Passphrase` 要
    ///     Ok(None)            還沒有 `local.key`（沒 `login` 過）
    ///     Err(Usage)          有檔但讀不懂
    ///
    /// ⚠️ 「在不在」與「讀它」是兩步，中間檔案被刪掉會走到 `Err` 而不是 `Ok(None)`
    /// （rumia🟢3）。這種 race 消不掉，而且落在**拒絕**那一邊，可以接受。
    pub fn key_mode(&self) -> Result<Option<KeyMode>, CoreError> {
        if !self.data_dir.join(wbf_sdk::vault::KEY_FILE_NAME).exists() {
            return Ok(None);
        }
        Ok(Vault::read_mode(&self.data_dir).map(Some)?)
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
    ///     Ok(())                     解開了，或本來就開著
    ///     Err(NoKeyFile)             這個資料目錄沒登入過
    ///     Err(NeedPassphrase)        要 passphrase 但沒給
    ///     Err(UnexpectedPassphrase)  是 plain 模式卻給了
    ///     Err(WrongPassphrase)       打錯了
    ///
    /// ⚠️ 這四種**分得出來**是刻意的（PR #24 審查 rumia🟡）：前端要據此決定「跳輸入框」
    /// 還是「說打錯了」，而 daemon 的 `vault.unlock`（§4.5）要回結構化的錯誤。
    /// 🚫 不要讓呼叫端去 parse 人話。
    pub fn unlock(&self, passphrase: Option<&[u8]>) -> Result<(), CoreError> {
        if self.is_unlocked() {
            return Ok(());
        }
        let Some(mode) = self.key_mode()? else {
            return Err(CoreError::new(
                CoreErrorKind::NoKeyFile,
                format!("no key file in {}; log in first", self.data_dir.display()),
            ));
        };
        // 模式先比對，🚫 不丟給 `Vault::open` 用「配不上」拒絕——那樣吐的是 vault 層的
        // 人話，分不出「要問使用者」與「他打錯了」。
        match (mode, passphrase) {
            (KeyMode::Passphrase, None) => {
                return Err(CoreError::new(
                    CoreErrorKind::NeedPassphrase,
                    format!("{} is passphrase-protected", self.data_dir.display()),
                ))
            }
            (KeyMode::Plain, Some(_)) => {
                return Err(CoreError::new(
                    CoreErrorKind::UnexpectedPassphrase,
                    format!(
                        "{} has no passphrase; do not pass one",
                        self.data_dir.display()
                    ),
                ))
            }
            _ => {}
        }
        let unlock = match passphrase {
            Some(bytes) => Unlock::Passphrase(zeroize::Zeroizing::new(bytes.to_vec())),
            None => Unlock::NoPassphrase,
        };
        // 模式對得上還開不了，就只剩「打錯了」這一種。
        let vault = Vault::open(&self.data_dir, &unlock)
            .map_err(|error| CoreError::new(CoreErrorKind::WrongPassphrase, format!("{error}")))?;
        let _ = self.vault.set(vault);
        Ok(())
    }

    /// 已經解開的 vault。
    ///
    /// Return:
    ///     Ok(&Vault)
    ///     Err(Locked)   還沒解鎖——呼叫端要先叫 `unlock`（RPC 那邊回 `locked`，§4.5）
    pub(crate) fn vault(&self) -> Result<&Vault, CoreError> {
        self.vault
            .get()
            .ok_or_else(|| CoreError::locked(&self.data_dir))
    }

    /// 把一把**已經開好**的 vault 放進來，回傳最後裝在裡面的那把。
    ///
    /// 兩個呼叫端，都是 `unlock` 涵蓋不了的：
    ///
    /// - **`login`**：`local.key` 還不存在，vault 是**建**出來的。🚫 `Core` 不長出「建」
    ///   的那半——那需要「要不要設 passphrase」的政策，是前端的決定（§3）。
    /// - **daemon 的 `vault.create`**（rpc-spec §3.1）：同一件事走 RPC 進來，「要不要 passphrase」
    ///   由前端在那一步決定。
    ///
    /// **冪等**，跟 [`Core::unlock`] 一樣：已經有一把就把傳進來的丟掉、回原本那把。
    /// 📎 兩把都來自同一個資料目錄，所以是同一把主金鑰——丟掉的那把沒有資訊。
    pub(crate) fn adopt_unlocked_vault(&self, vault: Vault) -> &Vault {
        let _ = self.vault.set(vault);
        // 📎 `OnceLock::set` 只有兩種結果：裝進去了，或本來就有一個。兩種之後 `get()`
        // 都是 `Some`，而 `OnceLock` 自己保證這件事沒有 race（rumia🟢1 要的那行註解）。
        self.vault
            .get()
            .expect("set() either stored ours or found one already there")
    }

    /// 資料目錄現在有什麼（`s/*/a/*` 兩層與 `r/`）。⚠️ **快照，不是快取**：
    /// 會刪檔的命令要當場刷新（accounts 模組的模組註解寫了為什麼）。
    pub(crate) fn refresh_data_dir_map(&self) -> Result<DataDirMap, CoreError> {
        Ok(accounts::refresh_data_dir_map(
            &self.data_dir,
            self.vault()?,
        )?)
    }

    /// 這台機器上的每個帳號。
    pub fn list_accounts(&self) -> Result<Vec<AccountSummary>, CoreError> {
        Ok(self.refresh_data_dir_map()?.list_accounts(self.vault()?)?)
    }

    /// 使用者打的那串 → 帳號目錄。當場刷新一次再比對（大小寫不敏感）。
    ///
    /// Args:
    ///     user: mxid 或 localpart, example: "@alice:localhost"
    ///     server: example: Some("http://localhost:6167")
    pub(crate) fn find_account(
        &self,
        user: &str,
        server: Option<&str>,
    ) -> Result<AccountDir, CoreError> {
        Ok(self
            .refresh_data_dir_map()?
            .find_account_dir(user, server)?)
    }

    /// `current` 指到的那個帳號。
    ///
    /// Return:
    ///     Ok(AccountDir)
    ///     Err(Usage)   沒登入過、或 `current` 指到的目錄解不開
    pub(crate) fn current_account(&self) -> Result<AccountDir, CoreError> {
        let current = accounts::read_current(&self.data_dir)?.ok_or_else(|| {
            CoreError::new(
                CoreErrorKind::NoSuchAccount,
                format!(
                    "no current account in {}; log in first",
                    self.data_dir.display()
                ),
            )
        })?;
        let key = self.vault()?.account_dir_key();
        accounts::find_account_of_current(&self.data_dir, &key, &current).ok_or_else(|| {
            CoreError::new(
                CoreErrorKind::NoSuchAccount,
                format!(
                    "the current account has no readable directory in {}; log in again",
                    self.data_dir.display()
                ),
            )
        })
    }

    /// 這台機器保管著誰的 recovery key（`r/`，logout 不碰它）。
    pub fn list_recovery_key_users(&self) -> Result<Vec<String>, CoreError> {
        Ok(recovery::list_users(&self.data_dir, self.vault()?)?)
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
    fn the_three_ways_unlocking_can_fail_are_told_apart() {
        // ⚠️ 這是 daemon 的 `vault.unlock`（§4.5）要回結構化錯誤的前提：前端得知道
        // 該「跳輸入框」還是該說「打錯了」，🚫 不能靠 parse 人話（PR #24 審查 rumia🟡）。
        let dir = scratch("unlock-kinds");
        assert_eq!(
            Core::open(&dir).unlock(None).unwrap_err().kind,
            CoreErrorKind::NoKeyFile
        );

        Vault::create(
            &dir,
            &Unlock::Passphrase(zeroize::Zeroizing::new(b"pw".to_vec())),
        )
        .unwrap();
        let core = Core::open(&dir);
        assert_eq!(
            core.unlock(None).unwrap_err().kind,
            CoreErrorKind::NeedPassphrase,
            "要 passphrase 卻沒給 ≠ 打錯了"
        );
        assert_eq!(
            core.unlock(Some(b"nope")).unwrap_err().kind,
            CoreErrorKind::WrongPassphrase
        );
        assert!(!core.is_unlocked(), "失敗不該留下半開的狀態");
        core.unlock(Some(b"pw")).unwrap();

        // plain 模式卻給了 passphrase：🚫 不靜默忽略。
        let plain = scratch("unlock-plain");
        Vault::create(&plain, &Unlock::NoPassphrase).unwrap();
        assert_eq!(
            Core::open(&plain).unlock(Some(b"pw")).unwrap_err().kind,
            CoreErrorKind::UnexpectedPassphrase
        );
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&plain);
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

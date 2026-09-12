//! core 內部的「把東西打開」那一層：session、matrix backend、`cache.db`、媒體池。
//!
//! ⚠️ 這整個模組是 **`pub(crate)`**，而且要一直是。`Session`（裡面有 `access_token`）、
//! `MatrixBackend`、`Cache`、`MediaPool` 全是程序內的 handle：序列化不了、跨不了 FFI，
//! 而 `Session` 還帶著秘密——**它一個欄位都不該離開這個 crate**
//!（architecture-v2 §7；PR #24 審查 cirno🔴）。
//!
//! 外面看得到的是 `Core` 上那些回**可序列化 DTO** 的方法；handle 活在這裡，被它們用。

use crate::error::{CoreError, CoreErrorKind};
use wbf_sdk::backend::matrix_sdk::MatrixBackend;
use wbf_sdk::cache::{Cache, CacheIdentity, OpenOutcome};
use wbf_sdk::login::Session;
use wbf_sdk::media_pool::MediaPool;

use crate::accounts::AccountDir;
use crate::Core;

impl Core {
    /// 這個帳號封著的 session。
    ///
    /// 🚫 **回傳值不准離開這個 crate**：`Session::access_token` 是秘密。要給外面看
    /// 「我是誰」用 [`crate::WhoAmI`]。
    ///
    /// Return:
    ///     Ok(Session)
    ///     Err(Usage)   沒登入（`session.sealed` 不在）、或解不開
    pub(crate) fn session_of(&self, account: &AccountDir) -> Result<Session, CoreError> {
        self.vault()?
            .unseal_session(&account.session_path())
            .map_err(CoreError::from)?
            .ok_or_else(|| {
                CoreError::new(
                    CoreErrorKind::NotLoggedIn,
                    format!(
                        "{} is not logged in ({} missing); log in first",
                        account.label(),
                        account.session_path().display()
                    ),
                )
            })
    }

    /// 這個帳號的 matrix-sdk backend（store 在帳號目錄的 `m/`，金鑰是第二把子金鑰）。
    ///
    /// Args:
    ///     server_backup: conf 的 `SERVER_BACKUP`（CLI 規格 §10）, example: true
    ///
    /// ⚠️ 這個旗標由**呼叫端**帶進來，🚫 core 自己不讀 conf——那是「代前端做決定」（§3）。
    pub(crate) async fn backend_of(
        &self,
        account: &AccountDir,
        server_backup: bool,
    ) -> Result<MatrixBackend, CoreError> {
        let session = self.session_of(account)?;
        if session.store_dir.is_none() {
            return Err(CoreError::new(
                CoreErrorKind::NotLoggedIn,
                "this session has no matrix store (logged in with an older build or --token); log in again",
            ));
        }
        Ok(MatrixBackend::restore(
            &session,
            &account.matrix_store_dir(),
            &self.vault()?.matrix_store_key(),
            server_backup,
        )
        .await?)
    }

    /// 這個帳號所屬 server 的 `cache.db`（local-cache-db.md §6，同 server 的帳號共用）。
    ///
    /// server 不符、解不開就重建（§1），重建時發一個 `Progress` 事件說一聲——
    /// 🚫 不是 `eprintln!`：core 不印東西（`event` 模組的模組註解寫了為什麼）。
    pub(crate) fn cache_of(&self, account: &AccountDir, server: &str) -> Result<Cache, CoreError> {
        let identity = CacheIdentity {
            server: server.to_string(),
        };
        let (cache, outcome) =
            Cache::open(&account.server_dir(), &self.vault()?.cache_key(), &identity)?;
        match outcome {
            OpenOutcome::Reused => {}
            OpenOutcome::Created => self
                .events
                .progress(format!("created {}", cache.path().display())),
            OpenOutcome::Rebuilt => self.events.progress(format!(
                "rebuilt {} (it was for another server, or could not be opened)",
                cache.path().display()
            )),
        }
        Ok(cache)
    }

    /// 這個帳號所屬 server 的媒體儲存池（local-cache-db.md §8），跟 `cache.db` 同層。
    pub(crate) fn pool_of(&self, account: &AccountDir) -> Result<MediaPool, CoreError> {
        Ok(MediaPool::open(
            &account.server_dir(),
            self.vault()?.media_store_key(),
        )?)
    }
}

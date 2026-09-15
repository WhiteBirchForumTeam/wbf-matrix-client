//! 登入：建 `local.key`（如果還沒有）、走 matrix-sdk 登入、封 session、切成 current。

use serde::Serialize;

use wbf_sdk::backend::matrix_sdk::MatrixBackend;
use wbf_sdk::vault::{KeyMode, Vault};
use wbf_sdk::Unlock;

use crate::accounts::AccountDir;
use crate::error::{CoreError, CoreErrorKind};
use crate::Core;

/// `login`／`account add` 的結果。
///
/// 🚫 刻意**沒有** `access_token`：那是秘密，不過邊界（`handles` 模組註解）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct LoginResult {
    pub user_id: String,
    pub device_id: String,
    pub server: String,
    /// 登入成功自動切成 current（CLI 規格 §3.1.1）；換掉的是誰。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub switched_from: Option<String>,
}

impl Core {
    /// 建這個資料目錄的 `local.key`。
    ///
    /// ⚠️ 「要不要設 passphrase」是**前端的決定**（§3），所以它在這裡就是一個參數：
    /// 給 `Some(bytes)` 就是 passphrase 模式，`None` 就是 plain。🚫 core 不問、不猜。
    ///
    /// Return:
    ///     Ok(KeyMode)   建好了，回它是哪種模式
    ///     Err(Usage)    已經有一把了（🚫 不覆蓋：那會把既有的帳號全部鎖在外面）
    pub fn create_vault(&self, passphrase: Option<&[u8]>) -> Result<KeyMode, CoreError> {
        if self.key_mode()?.is_some() {
            return Err(CoreError::new(
                CoreErrorKind::Usage,
                format!(
                    "{} already has a key file; it will not be overwritten",
                    self.data_dir.display()
                ),
            ));
        }
        let unlock = match passphrase {
            Some(bytes) => Unlock::Passphrase(zeroize::Zeroizing::new(bytes.to_vec())),
            None => Unlock::NoPassphrase,
        };
        let vault = Vault::create(&self.data_dir, &unlock)?;
        let mode = vault.mode();
        self.adopt_unlocked_vault(vault);
        Ok(mode)
    }

    /// 登入一個帳號。**要先解鎖或先 [`Core::create_vault`]**：store 的金鑰與
    /// `session.sealed` 都從 vault 來。
    ///
    /// Args:
    ///     user: mxid 或 localpart, example: "@alice:localhost"
    ///     password: 🚫 不印、不 log；⚠️ 它要送給 homeserver，所以是**字串**不是任意
    ///         bytes（local-cache-db §12.4：跟 passphrase 分家的理由）
    ///     device_name: example: "wbf-cli"
    pub async fn log_in(
        &self,
        server: &str,
        user: &str,
        password: &str,
        device_name: &str,
        server_backup: bool,
    ) -> Result<LoginResult, CoreError> {
        let vault = self.vault()?;
        // 🚨 全程握著帳號生命週期鎖（`account_lock`）：destroy／logout 正在刪目錄的時候不准建，反之亦然。
        // ⚠️ 排在碰任何目錄之前（下面那行就可能刪 `m/`）。
        let _lifecycle = crate::account_lock::lock_account_lifecycle(&self.data_dir)?;
        let dir_key = vault.account_dir_key();
        // 帳號目錄由 server host 加 localpart 決定（store 在 login 前就要有路徑）。
        let account = AccountDir::locate(&self.data_dir, &dir_key, server, user)?;
        // 🚨 這台 server 的目錄正在（或上次刪到一半停在）被刪：🚫 不准在上面建東西（維護者 2026-09-15）。
        let server_lock = account
            .server_dir()
            .join(crate::account_lock::TO_BE_DELETED_LOCK_FILE_NAME);
        if server_lock.exists() {
            return Err(CoreError::new(
                CoreErrorKind::ServerPendingRemoval,
                format!(
                    "the local data for this server is being removed ({} exists); \
                     if no destroy is running, a previous one stopped part way: delete {} by hand, then log in again",
                    server_lock.display(),
                    account.server_dir().display()
                ),
            ));
        }
        // 沒有 session 卻留著 `m/`：上次沒走 logout（或舊版的 logout 沒刪），那個 store
        // 綁著已經失效的裝置。消費端自己再清一次。
        if !account.is_logged_in() && account.matrix_store_dir().exists() {
            self.events
                .progress("removing a matrix store left over from a previous device");
            account.delete_matrix_store()?;
        }
        let (_backend, session) = MatrixBackend::login(
            server,
            user,
            password,
            device_name,
            &account.matrix_store_dir(),
            &vault.matrix_store_key(),
            server_backup,
        )
        .await?;
        // ⚠️ server 回的 `user_id` 才是**權威**（大小寫、localpart 正規化可能跟打的不一樣）：
        // 目錄名對不上就搬過去。
        let account = self.move_to_canonical_dir(account, &dir_key, server, &session.user_id)?;
        vault.seal_session(&account.session_path(), &session)?;
        // 🚨 session 換了，拿舊 token 探到的 backend 就不算數了（PR #33 審查 rumia🟡）。
        // ⚠️ 這是兩個「session 被替換」的地方之一，另一個是 `log_out_account`。
        self.forget_backend_probe(&account);
        let switched_from = self.switch_current_to(&account)?;
        Ok(LoginResult {
            user_id: session.user_id,
            device_id: session.device_id,
            server: session.server,
            switched_from,
        })
    }

    /// 目錄名是拿 `--user` 打的那串算的，但權威是 server 回的 mxid。對不上就搬。
    ///
    /// 🚫 目標已經存在就**拒絕**，不合併、不覆蓋：那是兩個帳號的資料撞在一起。
    fn move_to_canonical_dir(
        &self,
        account: AccountDir,
        dir_key: &wbf_sdk::vault::Key32,
        server: &str,
        canonical_user_id: &str,
    ) -> Result<AccountDir, CoreError> {
        let canonical = AccountDir::locate(&self.data_dir, dir_key, server, canonical_user_id)?;
        if canonical.dir == account.dir {
            return Ok(account);
        }
        if canonical.dir.exists() {
            return Err(CoreError::new(
                CoreErrorKind::Usage,
                format!(
                    "server says you are {canonical_user_id} but {} already exists; log that account out first",
                    canonical.dir.display()
                ),
            ));
        }
        std::fs::create_dir_all(canonical.dir.parent().expect("account dir has a parent"))
            .map_err(|error| CoreError::new(CoreErrorKind::Io, format!("{error}")))?;
        std::fs::rename(&account.dir, &canonical.dir)
            .map_err(|error| CoreError::new(CoreErrorKind::Io, format!("{error}")))?;
        Ok(canonical)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("wbf-core-login-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn creating_a_vault_is_the_callers_passphrase_decision() {
        let dir = scratch("create");
        let core = Core::open(&dir);
        // 🚫 core 不問要不要 passphrase——`None` 就是 plain。
        assert_eq!(core.create_vault(None).unwrap(), KeyMode::Plain);
        assert!(core.is_unlocked(), "建完就是解開的");
        // 🚫 已經有一把就拒絕：覆蓋等於把既有的帳號全鎖在外面。
        assert_eq!(
            core.create_vault(None).unwrap_err().kind,
            CoreErrorKind::Usage
        );

        let with_passphrase = scratch("create-pp");
        let core = Core::open(&with_passphrase);
        assert_eq!(
            core.create_vault(Some(b"hunter2")).unwrap(),
            KeyMode::Passphrase
        );
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&with_passphrase);
    }
}

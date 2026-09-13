//! 登出與摧毀：裝置層（`session.sealed`、`m/`、`k/`）與資料層（`cache.db` 的忘掉鏈）。
//!
//! ⚠️ 這一家是**會刪檔**的，所以 fail closed 的規矩最多；每一條的理由都在
//! local-cache-db.md §10.7 與 §6。
//!
//! 🚫 這裡的訊息**不提任何命令名字**（`wbf-cli key-backup recovery` 那種）：core 不知道
//! 呼叫它的是 rpc-cli、Desktop 還是 Android（§3）。它只說**條件**，前端照
//! [`CoreErrorKind::HistoryWouldBeLost`] 補上自己的那句話。

use serde::Serialize;
use wbf_sdk::backend::matrix_sdk::{BackupStatus, MatrixBackend};
use wbf_sdk::login::logout;
use wbf_sdk::room_keys;

use crate::accounts::{self, AccountDir};
use crate::error::{CoreError, CoreErrorKind};
use crate::recovery;
use crate::Core;

/// `logout`／`account del` 的結果。
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct LogoutResult {
    /// 登出的是誰（權威描述）, example: "@alice:localhost on http://localhost:6167"
    pub user: String,
}

/// `account destroy` 的結果：裝置層加資料層一起報。
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DestroyResult {
    pub user: String,
    pub events_removed: u64,
    pub media_removed: u64,
    pub pool_files_removed: u64,
    /// 有沒有連 recovery key 一起摧毀。⚠️ 摧毀之後 server 上那份備份**永遠解不開**。
    pub recovery_key_destroyed: bool,
}

impl Core {
    /// 裝置層的登出：讓 token 失效，刪這個帳號的 `session.sealed`、`m/`、`k/`。
    ///
    /// `cache.db` 裡的紀錄**留著**（之後再登入還在）——要連那些一起清是
    /// [`Core::destroy_account`]。
    ///
    /// ⚠️ `m/` 不能留：Matrix 的 logout 讓裝置失效，下次 `login` 是新裝置，舊的 crypto
    /// store 會擋登入（"account in the store doesn't match"，2026-09-07 實跑踩到）。
    ///
    /// 🚫 這裡**不刪 `r/` 的 recovery key**：它正是清完之後唯一回得去的路（§10.8）。
    ///
    /// Args:
    ///     user: 完整 mxid, example: "@bob:matrix.org"
    ///     accept_history_loss: 使用者明說接受失去歷史，兩關閘門都跳過
    pub async fn log_out(
        &self,
        user: &str,
        server: Option<&str>,
        accept_history_loss: bool,
        server_backup: bool,
    ) -> Result<LogoutResult, CoreError> {
        let account = self.find_account_by_full_mxid(user, server)?;
        let user = self
            .log_out_account(&account, accept_history_loss, server_backup)
            .await?;
        Ok(LogoutResult { user })
    }

    /// `account destroy`：裝置層加資料層。
    ///
    /// 先 `log_out` 那一整套，再跑忘掉鏈（local-cache-db.md §6）把這個帳號在 `cache.db` 裡
    /// **獨有**的東西清掉——別的帳號也持有的一律不動（維護者 2026-09-09）。
    ///
    /// ⚠️ 這個命令**連 recovery key 一起摧毀**，所以之後 server 上那份備份永遠解不開。
    /// 🚫 呼叫端要先問過使用者：core 不做「你確定嗎」，那是前端的事（§3）。
    pub async fn destroy_account(
        &self,
        user: &str,
        server: Option<&str>,
        accept_history_loss: bool,
        server_backup: bool,
    ) -> Result<DestroyResult, CoreError> {
        // 維護者 2026-09-10：會刪檔的命令，路徑**當場刷新一次**再比對——帳號目錄與
        // recovery key 都從同一份快照來，中間不再掃第二次（掃兩次就有兩個不同時刻的答案）。
        let map = self.refresh_data_dir_map()?;
        let account = self.find_account_by_full_mxid(user, server)?;
        // 🚫 在 logout 之前先問：`recovery::del` 只認精確的 mxid，而使用者打的那串大小寫
        // 可能跟封存時用的權威 mxid 不同（PR #19 審查 rumia🟡1／salvia）。
        let kept_recovery_key_user_id = map.find_recovery_key_user_id(user)?.map(str::to_string);
        let server = self.server_of(&account, server)?;

        // 先裝置層再資料層：反過來的話 logout 要用的 session 已經被刪了。
        let described = self
            .log_out_account(&account, accept_history_loss, server_backup)
            .await?;

        // ⚠️ destroy 的語意是「什麼都不留」，所以連 recovery key 也摧毀。
        // 🚫 `log_out` 不做這件事——它留著它正是為了讓歷史救得回來。
        let recovery_key_destroyed = match &kept_recovery_key_user_id {
            Some(kept) => {
                recovery::del(&self.data_dir, self.vault()?, kept)?;
                self.events.progress(format!(
                    "destroyed the recovery key kept here for {kept}; the server-side backup can no longer be opened"
                ));
                true
            }
            None => false,
        };

        let mut cache = self.cache_of(&account, &server)?;
        // 🚫 不拿使用者打的字串去查 `users` 列：那裡存的是權威 mxid，精確比對差一個大小寫
        // 就查不到，然後 destroy 會回 `events_removed: 0`，看起來像「本來就沒有」，其實是
        // 全部殘留（PR #21 審查 salvia🔴——跟 recovery key 那條是同一個形狀）。
        let cached_mxids = cache.list_account_mxids()?;
        let report = match accounts::find_matching_plaintext(&cached_mxids, user, "cached account")?
        {
            Some(cached_mxid) => cache.forget_account(cached_mxid)?,
            // 真的沒有這個帳號的快取列（`login` 之後還沒 `recent` 過就是這樣）。
            None => Default::default(),
        };

        // DB 先、檔案後（§6 的忘掉鏈）：列已經刪了，現在刪池裡沒人指的檔。
        // 刪不掉只說一聲，下次 media-gc 的 sweep 會再收。
        let pool = self.pool_of(&account)?;
        let mut pool_files_removed = 0u64;
        for file in &report.orphan_pool_files {
            match pool.remove(file) {
                Ok(()) => pool_files_removed += 1,
                Err(error) => self
                    .events
                    .progress(format!("could not remove pool file {file}: {error}")),
            }
        }
        Ok(DestroyResult {
            user: described,
            events_removed: report.events_removed,
            media_removed: report.media_removed,
            pool_files_removed,
            recovery_key_destroyed,
        })
    }

    /// 這個帳號在哪個 server：`--server` 覆蓋優先，否則問它封著的 session。
    fn server_of(
        &self,
        account: &AccountDir,
        override_: Option<&str>,
    ) -> Result<String, CoreError> {
        match override_ {
            Some(server) => Ok(server.to_string()),
            None => Ok(self.session_of(account)?.server),
        }
    }

    /// [`Core::log_out`] 與 [`Core::destroy_account`] 共用的本體。
    ///
    /// Return:
    ///     Ok(String)   這次登出的是誰（權威描述）
    async fn log_out_account(
        &self,
        account: &AccountDir,
        accept_history_loss: bool,
        server_backup: bool,
    ) -> Result<String, CoreError> {
        let described = self.describe_account(account);
        self.refuse_if_history_would_be_lost(account, accept_history_loss, server_backup)
            .await?;
        let vault = self.vault()?;
        match vault.unseal_session(&account.session_path())? {
            Some(session) => {
                logout(&session).await?;
                vault.delete_sealed_session(&account.session_path())?;
            }
            // 已經登出但目錄還在（上次清到一半、或 del 一個登出中的帳號）：本地照樣清乾淨。
            None => self.events.progress(format!(
                "{} is already logged out; cleaning up the local files",
                account.label()
            )),
        }
        // 🚨 session 沒了，拿它探到的 backend 也不算數了（PR #33 審查 rumia🟡）。
        // ⚠️ 放在 match 之後：兩條分支（剛刪掉、本來就沒有）都是「現在沒有 session」。
        // 📎 這是兩個「session 被替換」的地方之一，另一個是 `log_in` 封新 session 那一行。
        self.forget_backend_probe(account);
        account.delete_matrix_store()?;
        // 維護者 2026-09-09：離開這台機器就清乾淨——本地的房間金鑰備份跟著走（§10.7）。
        // 上面的閘門已經確認過「server 那份救得回來」，或使用者明說接受失去它。
        room_keys::del_snapshot(&account.dir)?;
        accounts::clear_current_if(&self.data_dir, account)?;
        // 這個 server 最後一個帳號登出：快取沒有主人了，整個丟。
        let server_dir = account.server_dir();
        if !accounts::has_any_logged_in_account(&server_dir) {
            // 🚨 **先關、再刪**：註冊表裡的寫入者與讀連線還握著 cache.db（`close_server_cache` 的表）。
            self.close_server_cache(&server_dir)?;
            if wbf_sdk::cache::remove_cache(&server_dir)? {
                self.events.progress(format!(
                    "removed {} (no account on this server is logged in any more)",
                    server_dir.join(wbf_sdk::cache::CACHE_FILE_NAME).display()
                ));
            }
        }
        Ok(described)
    }

    /// `logout`／`account del`／`account destroy` 的閘門（local-cache-db.md §10.7）。
    ///
    /// 這些命令會連 `m/`（crypto store）與 `k/`（本地快照）一起刪。在還沒有 recovery key 的
    /// 預設狀態下，**server 端備份的私鑰就在那個 store 裡**——照樣登出的話歷史就回不來了。
    ///
    /// 兩關，都要過：
    ///
    /// 1. **server 那份救得回來嗎**：`exists_on_server && recovery_enabled`，正面認得才算。
    /// 2. **這台機器保管著這個帳號的 recovery key 嗎**：查 `r/`。
    ///    ⚠️ 第 1 關只說得出「SSSS 設好了」——跑過產生 recovery key、印出來、沒抄就關掉
    ///    終端的人也會通過第 1 關。第 2 關才確認得了「刪完之後這裡還有東西打得開那份備份」。
    ///
    /// 🚫 第 2 關**不問使用者**：key 產生的當下就封進 `r/` 了，而那個目錄 logout 不碰。
    async fn refuse_if_history_would_be_lost(
        &self,
        account: &AccountDir,
        accept_history_loss: bool,
        server_backup: bool,
    ) -> Result<(), CoreError> {
        if accept_history_loss || !account.is_logged_in() {
            // 已經登出的帳號沒有 session 可以問 server，也沒有 token 要失效；只是清本地殘留。
            return Ok(());
        }
        let Some(backend) = self.find_backend_of(account, server_backup).await else {
            return Err(refusal(
                account,
                "the server could not be reached, so it is unknown whether\n       \
                 the backup there can still be decrypted",
            ));
        };
        let status = backend.backup_status().await.ok();
        if !is_history_recoverable(status.as_ref()) {
            return Err(refusal(
                account,
                "the server-side backup cannot be decrypted yet - its key lives in the crypto store\n       \
                 that is about to be deleted.\n       \
                 Create a recovery key first",
            ));
        }
        // 🚫 不用使用者打的字串：封存時用的是 server 的權威 mxid。解不出 session 就是問不出
        // 這個帳號是誰——擋下來，不要拿佔位值去查（查不到會變成「沒保管」，方向剛好相反）。
        let Some(session) = self.vault()?.unseal_session(&account.session_path())? else {
            return Err(refusal(
                account,
                "its session could not be opened, so it is unknown which account this is",
            ));
        };
        if recovery::find(&self.data_dir, self.vault()?, &session.user_id)?.is_some() {
            return Ok(());
        }
        Err(refusal(
            account,
            "the server says secret storage is set up, but this machine is not keeping that account's\n       \
             recovery key, so nothing here could open the backup afterwards.\n       \
             Create a recovery key (it is sealed under the data dir's r/, which survives logout),\n       \
             or list the keys kept here",
        ))
    }

    /// 用**這個帳號自己的** session 與 store 開一個 backend（閘門要拿它問 server）。
    ///
    /// ⚠️ 🚫 不要改成「current 帳號」那條路：`account del <user>` 的目標是 `<user>`，
    /// 用 current 的狀態判斷會放行不該放行的刪除（PR #19 審查 rumia／salvia 🔴1：
    /// current 有 recovery key 就把別的帳號的金鑰刪了）。
    ///
    /// Return:
    ///     Some(MatrixBackend)  開起來了，而且已經 sync 過一次
    ///     None                 沒 session、store 開不了、連不上——閘門會因此擋下來（fail closed）
    async fn find_backend_of(
        &self,
        account: &AccountDir,
        server_backup: bool,
    ) -> Option<MatrixBackend> {
        let backend = self.backend_of(account, server_backup).await.ok()?;
        // recovery 的狀態要 sync 過才是真的（它從 account data／secret storage 來）。
        let _ = backend.sync_once(None, std::time::Duration::ZERO).await;
        Some(backend)
    }
}

/// 這個帳號的歷史**救得回來嗎**——只有正面認得才算數（local-cache-db.md §10.7）。
///
/// 🚫 不寫成「沒有 recovery key 才擋」：上游哪天多一種 `RecoveryState`，那種寫法會默默放行。
fn is_history_recoverable(status: Option<&BackupStatus>) -> bool {
    status.is_some_and(|status| status.exists_on_server && status.recovery_enabled)
}

/// 閘門擋下來時的訊息：`why` 是這一次為什麼擋，後面接一律相同的出路。
///
/// 🚫 **不提命令名字**：core 不知道呼叫它的是誰（§3）。前端看到
/// [`CoreErrorKind::HistoryWouldBeLost`] 再補上自己那句 `wbf-cli …`。
fn refusal(account: &AccountDir, why: &str) -> CoreError {
    CoreError::new(
        CoreErrorKind::HistoryWouldBeLost,
        format!(
            "this would delete {}'s room keys on this machine (m/ and k/), and\n       \
             {why}.",
            account.label()
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_is_only_recoverable_when_both_halves_are_true() {
        let status = |exists_on_server, recovery_enabled| BackupStatus {
            exists_on_server,
            enabled_locally: true,
            recovery_enabled,
            recovery_state: "Enabled".into(),
        };
        assert!(is_history_recoverable(Some(&status(true, true))));
        // 🚫 缺任何一半都不算——而且「問不到」也不算（fail closed）。
        assert!(!is_history_recoverable(Some(&status(true, false))));
        assert!(!is_history_recoverable(Some(&status(false, true))));
        assert!(!is_history_recoverable(None));
    }

    #[test]
    fn the_refusal_says_the_condition_but_never_names_a_command() {
        // 🚫 core 不知道呼叫它的是 rpc-cli、Desktop 還是 Android（§3）。
        let dir = std::env::temp_dir().join(format!("wbf-core-refusal-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let vault = wbf_sdk::vault::Vault::create(&dir, &wbf_sdk::Unlock::NoPassphrase).unwrap();
        let account = AccountDir::locate(
            &dir,
            &vault.account_dir_key(),
            "http://localhost:6167",
            "@alice:localhost",
        )
        .unwrap();

        let error = refusal(&account, "the server could not be reached");
        assert_eq!(error.kind, CoreErrorKind::HistoryWouldBeLost);
        assert!(!error.message.contains("wbf-cli"), "{}", error.message);
        assert!(error.message.contains("alice"), "{}", error.message);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

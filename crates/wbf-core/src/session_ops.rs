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
    /// 帳號目錄 `a/<b58>/` 刪掉了沒（#25：「什麼都不留」連目錄一起）。
    pub account_dir_removed: bool,
    /// 這是那台 server 最後一個帳號，整個 `s/<b58>/`（含媒體池）一起刪掉了沒。
    pub server_dir_removed: bool,
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
        // 🚨 全程握著帳號生命週期鎖（`account_lock`）：登入正在建同一個目錄時不准刪。
        let _lifecycle = crate::account_lock::lock_account_lifecycle(&self.data_dir)?;
        let account = self.find_account_by_full_mxid(user, server)?;
        let user = self
            .log_out_account(&account, accept_history_loss, server_backup)
            .await?;
        Ok(LogoutResult { user })
    }

    /// `account destroy`：閘門 → 資料層 → 裝置層 → 目錄，**什麼都不留**（#25，維護者 2026-09-15）。
    ///
    /// 順序是這支的重點：
    ///
    /// 1. **閘門先**（`refuse_if_history_would_be_lost`）：之後每一步都會刪東西，🚫 不准刪到一半才被擋。
    /// 2. **忘掉鏈趁 `cache.db` 還在時跑**，而且走這台 server 的**唯一寫入者**（`ServerCache`）。
    ///    ⚠️ 以前放在登出之後：最後一個帳號登出時 `cache.db` 已經被刪，忘掉鏈又開出一個**空的**
    ///    （`events_removed: 0`、多留一個殘骸）；而且它用 `cache_of` 另開寫入連線，繞過唯一寫入者。
    /// 3. 池檔（DB 先、檔案後）。
    /// 4. 裝置層（`log_out` 那一整套，閘門已經過了）。
    /// 5. recovery key。
    /// 6. **最後才刪目錄**：帳號目錄；那台 server 一個帳號都不剩就整個 server 目錄。
    ///    ⭐ 放最後是 fail closed：前面任何一步失敗，目錄還在，`account status` 看得到殘留、重跑 destroy 能接著清。
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
        // 🚨 **全程**握著帳號生命週期鎖（`account_lock`，PR #40 審查 rumia🔴 第三輪、維護者 2026-09-15）：
        // 這期間登入被拒，所以「最後一個帳號」的判斷到刪完都成立 —— 🚫 不靠「再掃一次目錄」當同步。
        let _lifecycle = crate::account_lock::lock_account_lifecycle(&self.data_dir)?;
        // 維護者 2026-09-10：會刪檔的命令，路徑**當場刷新一次**再比對——帳號目錄與
        // recovery key 都從同一份快照來，中間不再掃第二次（掃兩次就有兩個不同時刻的答案）。
        let map = self.refresh_data_dir_map()?;
        let account = self.find_account_by_full_mxid(user, server)?;
        // 🚫 在 logout 之前先問：`recovery::del` 只認精確的 mxid，而使用者打的那串大小寫
        // 可能跟封存時用的權威 mxid 不同（PR #19 審查 rumia🟡1／salvia）。
        let kept_recovery_key_user_id = map.find_recovery_key_user_id(user)?.map(str::to_string);
        let server = self.server_of(&account, server)?;

        self.refuse_if_history_would_be_lost(&account, accept_history_loss, server_backup)
            .await?;

        let report = self.forget_cached_account(&account, &server, user).await?;

        // DB 先、檔案後（§6 的忘掉鏈）：列已經刪了，現在刪池裡沒人指的檔。
        // 刪不掉只說一聲，下次 media-gc 的 sweep 會再收（整個 server 目錄被刪時也一起走）。
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
        drop(pool);

        let described = self.log_out_account_past_the_gate(&account).await?;

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

        let (account_dir_removed, server_dir_removed) =
            self.remove_destroyed_account_dirs(&account)?;
        Ok(DestroyResult {
            user: described,
            events_removed: report.events_removed,
            media_removed: report.media_removed,
            pool_files_removed,
            recovery_key_destroyed,
            account_dir_removed,
            server_dir_removed,
        })
    }

    /// 忘掉鏈（local-cache-db.md §6），走這台 server 的**唯一寫入者**。
    ///
    /// 🚫 `cache.db` 不在就不開：開了等於為了「忘掉」建一個空的出來（#25 同一支的 bug）。
    /// 🚫 不拿使用者打的字串去查 `users` 列：那裡存的是權威 mxid，精確比對差一個大小寫
    /// 就查不到，然後 destroy 會回 `events_removed: 0`，看起來像「本來就沒有」，其實是
    /// 全部殘留（PR #21 審查 salvia🔴——跟 recovery key 那條是同一個形狀）。
    ///
    /// Args:
    ///     user: 使用者打的 mxid（大小寫可能跟快取裡的不同）, example: "@ALICE:LocalHost"
    /// Return:
    ///     Ok(ForgetReport)  沒有 `cache.db`、或快取裡沒有這個帳號 → 全零
    async fn forget_cached_account(
        &self,
        account: &AccountDir,
        server: &str,
        user: &str,
    ) -> Result<wbf_sdk::cache::ForgetReport, CoreError> {
        let Some(cache) = self.find_server_cache_if_present(account, server)? else {
            return Ok(Default::default());
        };
        let user = user.to_string();
        cache
            .run(move |cache| {
                let cached_mxids = cache.list_account_mxids()?;
                match accounts::find_matching_plaintext(&cached_mxids, &user, "cached account")? {
                    Some(cached_mxid) => cache.forget_account(cached_mxid),
                    // 真的沒有這個帳號的快取列（`login` 之後還沒 `recent` 過就是這樣）。
                    None => Ok(Default::default()),
                }
            })
            .await
        // ⚠️ `cache` 在這裡放掉：接下來的登出可能要關它（最後一個帳號），還有人拿著會關不掉。
    }

    /// destroy 的最後一步：刪帳號目錄；那台 server 除了它沒有別的帳號，就整個 server 目錄。
    ///
    /// 🚨 會刪整棵目錄，所以路徑**正面認得**才動：必須正好是 `<data dir>/s/<x>/a/<y>`。
    /// 形狀不對就拒絕（回 Usage），🚫 不猜、不往上刪。
    ///
    /// 🚨 **帳號目錄永遠最後刪**（PR #40 審查 rumia🔴）：帳號目錄一旦不在，`find_account_by_full_mxid` 就找不到它，
    /// 重跑 destroy 也清不了剩下的東西。所以「最後一個帳號」這條先判斷（🚫 不先刪帳號目錄再看 `a/` 空了沒）、
    /// 先關快取，再刪 server 目錄裡的其他東西，最後才刪帳號目錄 —— 前面任何一步失敗，帳號都還在。
    ///
    /// Return:
    ///     Ok((account_dir_removed, server_dir_removed))
    ///     Err(Usage)  路徑形狀不對
    ///     Err(Io)     關不掉快取、刪不掉（帳號目錄還在的話可以重跑）
    fn remove_destroyed_account_dirs(
        &self,
        account: &AccountDir,
    ) -> Result<(bool, bool), CoreError> {
        let server_dir = account.server_dir();
        if !is_account_dir_of(&self.data_dir, &account.dir) {
            return Err(CoreError::new(
                CoreErrorKind::Usage,
                format!(
                    "refusing to remove {}: it is not an account directory of this data dir",
                    account.dir.display()
                ),
            ));
        }
        if has_another_account_entry(&server_dir.join(accounts::ACCOUNTS_DIR_NAME), &account.dir) {
            return Ok((remove_path_if_present(&account.dir)?, false));
        }
        // 🚨 **先關、再刪**：註冊表可能還握著這台 server 的 cache.db（登出時已經關過就是 no-op）。
        // 關不掉就在這裡停：什麼都還沒刪。
        self.close_server_cache(&server_dir)?;
        let removal = remove_server_dir_with_the_account_last(
            &server_dir,
            &account.dir,
            &mut remove_path_if_present,
            &mut |dir| std::fs::remove_dir(dir),
        )?;
        match &removal.left_behind {
            None if removal.server_dir_removed => self.events.progress(format!(
                "removed {} (no account on this server is left)",
                server_dir.display()
            )),
            None => {}
            Some(left_behind) => self.events.progress(format!(
                "the account is gone, but the cleanup was incomplete: {left_behind}"
            )),
        }
        Ok((removal.account_dir_removed, removal.server_dir_removed))
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

    /// [`Core::log_out`] 的本體：閘門加上 [`Core::log_out_account_past_the_gate`]。
    ///
    /// Return:
    ///     Ok(String)   這次登出的是誰（權威描述）
    async fn log_out_account(
        &self,
        account: &AccountDir,
        accept_history_loss: bool,
        server_backup: bool,
    ) -> Result<String, CoreError> {
        self.refuse_if_history_would_be_lost(account, accept_history_loss, server_backup)
            .await?;
        self.log_out_account_past_the_gate(account).await
    }

    /// 裝置層的清理。🚨 **呼叫前一定要先過 `refuse_if_history_would_be_lost`**：這裡會刪 `m/` 與 `k/`。
    /// `log_out` 與 `destroy_account` 共用；destroy 把閘門提到最前面，所以分開。
    ///
    /// Return:
    ///     Ok(String)   這次登出的是誰（權威描述）
    async fn log_out_account_past_the_gate(
        &self,
        account: &AccountDir,
    ) -> Result<String, CoreError> {
        let described = self.describe_account(account);
        let vault = self.vault()?;
        match vault.unseal_session(&account.session_path())? {
            Some(session) => {
                // account-session.md §4：1 封池 → 2 HTTP 登出（只有成與不成）→ 3 關池 → 4 清本地 → 5 解封。
                // 封池是一個 guard：這個函數怎麼離開（不成、`?`、成功走到底）都會解封（PR #54 審查 🔴：第一版只在失敗分支解封）。
                let _logging_out = self.logging_out_guard(account);
                // 不成：`?` 原樣回錯、guard 解封，連線照常收新封包（維護者 2026-09-21：no-op）。
                logout(&session).await?;
                // 成了：token 在 server 那邊已經沒了，這個帳號的線全關、釋放資源（link-pool.md §3）。
                // `close_all` 等正在用線的命令做完才收那條；遠端先關了的只是丟掉，不二次跳錯。
                // 🚫 不在池裡另存一份「登出了沒」：那件事的真相是 server 的 token 表與本地的 session.sealed；「登出中」是帳號的狀態。
                self.close_links(account, "logged out").await;
                vault.delete_sealed_session(&account.session_path())?;
            }
            // 已經登出但目錄還在（上次清到一半、或 del 一個登出中的帳號）：本地照樣清乾淨。
            None => self.events.progress(format!(
                "{} is already logged out; cleaning up the local files",
                account.label()
            )),
        }
        // 📎 探活以 server 為鍵、不帶 token（account-session.md §1）：session 沒了不影響它，這裡不再忘掉探測結果。
        // 已經登出但目錄還在那條分支：池照理說是空的（沒 session 開不了線），還是掃一次——消費端自己再問一次。
        self.close_links(account, "logged out").await;
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

/// `account_dir` 正好是 `<data dir>/s/<x>/a/<y>` 嗎（destroy 刪目錄前的防線）。
///
/// Args:
///     data_dir: example: "<data dir>"
///     account_dir: example: "<data dir>/s/<b58>_<b58>/a/<b58>_<b58>"
/// Return:
///     bool  true 只在四層都對得上：上一層叫 `a`、再上一層的上一層是 `<data dir>/s`，而且兩個名字都不是空的
fn is_account_dir_of(data_dir: &std::path::Path, account_dir: &std::path::Path) -> bool {
    let Some(accounts_dir) = account_dir.parent() else {
        return false;
    };
    let Some(server_dir) = accounts_dir.parent() else {
        return false;
    };
    let is_named = |path: &std::path::Path| {
        path.file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| !name.is_empty() && name != "." && name != "..")
    };
    is_named(account_dir)
        && is_named(server_dir)
        && accounts_dir.file_name() == Some(std::ffi::OsStr::new(accounts::ACCOUNTS_DIR_NAME))
        && server_dir.parent() == Some(data_dir.join(accounts::SERVERS_DIR_NAME).as_path())
}

/// `a/` 底下除了這個帳號，還有沒有別的東西。
///
/// Return:
///     bool  true ＝ 還有（或讀不出來 —— 🚫 不因為一個讀取錯誤就刪掉整台 server 的共用資料）；
///           false ＝ 只剩這個帳號、或 `a/` 根本不在
fn has_another_account_entry(
    accounts_dir: &std::path::Path,
    account_dir: &std::path::Path,
) -> bool {
    let entries = match std::fs::read_dir(accounts_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return false,
        Err(_) => return true,
    };
    for entry in entries {
        match entry {
            Ok(entry) if entry.path() == account_dir => continue,
            // 別的名字、或這一筆讀不出來：都算還有。
            _ => return true,
        }
    }
    false
}

/// 刪一個路徑（目錄就整棵）。
///
/// Return:
///     Ok(true)   刪了
///     Ok(false)  本來就不在
///     Err(Io)    刪不掉
fn remove_path_if_present(path: &std::path::Path) -> Result<bool, CoreError> {
    let result = match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => std::fs::remove_dir_all(path),
        Ok(_) => std::fs::remove_file(path),
        Err(error) => Err(error),
    };
    match result {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(remove_error(path, error)),
    }
}

/// [`remove_server_dir_with_the_account_last`] 的結果。
struct ServerDirRemoval {
    account_dir_removed: bool,
    server_dir_removed: bool,
    /// 帳號目錄已經刪掉之後、收空目錄時沒收掉的那一個（給人看的一句話）；None ＝ 沒有。
    left_behind: Option<String>,
}

/// 整個 server 目錄，**帳號目錄最後**（PR #40 審查 rumia🔴×2）。
///
/// ⭐ 保證的是：**回 `Err` ⇒ 帳號目錄還在**，所以重跑 destroy 找得到它、能接著清。為了讓這句話成立：
///
/// 0. 放下 `to_be_deleted.lock`（維護者 2026-09-15）：從這一刻起到整個目錄刪完，登入這台 server 一律被拒 ——
///    程序中途當掉也一樣。放不下就回 Err，什麼都沒刪。
/// 1. 刪 `a/` 與 `to_be_deleted.lock` 以外的東西（`cache.db`、媒體池…）—— 失敗就回 Err，帳號沒動。
/// 2. **再看一次** `a/` 底下是不是只剩這個帳號：有別的，就只刪這個帳號的目錄、收回 `to_be_deleted.lock`、🚫 不收 server 目錄。
///    📎 呼叫端握著 `account.lock`，正常流程裡不會有新帳號冒出來；這一步是防線，🚫 不是同步。
/// 3. 刪帳號目錄 —— 失敗就回 Err（`remove_dir_all` 最後一步才刪目錄本身，所以失敗時目錄還在）。
/// 4. 收尾：空的 `a/` → server 目錄**改名**、名字最前面加 🗑️（`s/🗑️<b58>_<b58>`） → 刪裡面的 `to_be_deleted.lock` → 刪那個空目錄。
///    🚫 **不回 Err**：這時帳號已經不在，回 Err 等於叫人重跑卻找不到它；只記在 `left_behind`。
///    ⭐ `to_be_deleted.lock` 排在 `a/` 與改名之後：改名前任何一步沒成，標記都還在原本的位置，登入照樣被擋；
///    改名之後原本的路徑就沒了，登入建的是新目錄，而留下的 `🗑️…` 掃描時一律跳過（維護者 2026-09-15）。
///    改名的目標已經存在（上一次也停在這裡）就停下、🚫 不覆蓋，交給人手動收。
///
/// Args:
///     server_dir: example: "<data dir>/s/<b58>_<b58>"
///     account_dir: 必須是 `server_dir/a/` 底下的那個帳號（呼叫端已經驗過形狀）
///     remove: 刪一個路徑（目錄就整棵）；測試用它在指定的一步失敗或插入一個新帳號
///     remove_empty_dir: 只刪空目錄；測試用它模擬收尾失敗
/// Return:
///     Ok(ServerDirRemoval)
///     Err(Io)  第 0、1 或 3 步失敗；帳號目錄還在
fn remove_server_dir_with_the_account_last(
    server_dir: &std::path::Path,
    account_dir: &std::path::Path,
    remove: &mut dyn FnMut(&std::path::Path) -> Result<bool, CoreError>,
    remove_empty_dir: &mut dyn FnMut(&std::path::Path) -> std::io::Result<()>,
) -> Result<ServerDirRemoval, CoreError> {
    let accounts_dir = server_dir.join(accounts::ACCOUNTS_DIR_NAME);
    let server_lock = server_dir.join(crate::account_lock::TO_BE_DELETED_LOCK_FILE_NAME);
    if !server_dir.exists() {
        return Ok(ServerDirRemoval {
            account_dir_removed: false,
            server_dir_removed: false,
            left_behind: None,
        });
    }
    std::fs::write(&server_lock, b"").map_err(|error| {
        CoreError::new(
            CoreErrorKind::Io,
            format!(
                "could not mark {} for removal: {error}",
                server_dir.display()
            ),
        )
    })?;
    let entries = std::fs::read_dir(server_dir).map_err(|error| remove_error(server_dir, error))?;
    let mut others = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| remove_error(server_dir, error))?;
        if entry.path() != accounts_dir && entry.path() != server_lock {
            others.push(entry.path());
        }
    }
    for other in &others {
        remove(other)?;
    }
    if has_another_account_entry(&accounts_dir, account_dir) {
        let account_dir_removed = remove(account_dir)?;
        let left_behind = match std::fs::remove_file(&server_lock) {
            Ok(()) => None,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => Some(format!(
                "{} could not be removed: {error}",
                server_lock.display()
            )),
        };
        return Ok(ServerDirRemoval {
            account_dir_removed,
            server_dir_removed: false,
            left_behind,
        });
    }
    let account_dir_removed = remove(account_dir)?;
    let incomplete = |path: &std::path::Path, error: std::io::Error| ServerDirRemoval {
        account_dir_removed,
        server_dir_removed: false,
        left_behind: Some(format!("{} could not be removed: {error}", path.display())),
    };
    match remove_empty_dir(&accounts_dir) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Ok(incomplete(&accounts_dir, error)),
    }
    let Some(renamed) = crate::account_lock::to_to_be_deleted_dir(server_dir) else {
        return Ok(incomplete(
            server_dir,
            std::io::Error::other("the server directory has no name to rename"),
        ));
    };
    if renamed.exists() {
        return Ok(incomplete(
            server_dir,
            std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                format!(
                    "{} is already there from an earlier removal",
                    renamed.display()
                ),
            ),
        ));
    }
    if let Err(error) = std::fs::rename(server_dir, &renamed) {
        return Ok(incomplete(server_dir, error));
    }
    let renamed_lock = renamed.join(crate::account_lock::TO_BE_DELETED_LOCK_FILE_NAME);
    match std::fs::remove_file(&renamed_lock) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Ok(incomplete(&renamed_lock, error)),
    }
    match remove_empty_dir(&renamed) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Ok(incomplete(&renamed, error)),
    }
    Ok(ServerDirRemoval {
        account_dir_removed,
        server_dir_removed: true,
        left_behind: None,
    })
}

fn remove_error(path: &std::path::Path, error: std::io::Error) -> CoreError {
    CoreError::new(
        CoreErrorKind::Io,
        format!("could not remove {}: {error}", path.display()),
    )
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
    use wbf_sdk::IncomingEvent;

    const SERVER: &str = "http://localhost:6167";

    fn scratch_unlocked(name: &str) -> (std::path::PathBuf, Core) {
        let dir =
            std::env::temp_dir().join(format!("wbf-core-destroy-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        wbf_sdk::vault::Vault::create(&dir, &wbf_sdk::Unlock::NoPassphrase).unwrap();
        let core = Core::open(&dir);
        core.unlock(None).unwrap();
        (dir, core)
    }

    fn account_of(core: &Core, dir: &std::path::Path, user: &str) -> AccountDir {
        let key = core.vault().unwrap().account_dir_key();
        let account = AccountDir::locate(dir, &key, SERVER, user).unwrap();
        std::fs::create_dir_all(&account.dir).unwrap();
        account
    }

    fn seed_events(core: &Core, account: &AccountDir, user: &str, event_id: &str) {
        let cache = core.server_cache_of(account, SERVER).unwrap();
        let event = IncomingEvent::Plain {
            event: serde_json::json!({
                "type": "m.room.message", "event_id": event_id, "room_id": "!r", "sender": "@carol:localhost",
                "origin_server_ts": 1, "content": { "msgtype": "m.text", "body": event_id },
            }),
        };
        let user = user.to_string();
        cache
            .run_blocking(move |cache| cache.upsert_events(&user, "!r", &[event]))
            .unwrap();
    }

    /// 🚨 #25 的三件事一起驗：這台 server **最後一個**帳號被 destroy ——
    ///
    /// - 忘掉鏈真的清到東西（以前在登出刪掉 cache.db 之後才跑，回 0）；
    /// - 🚫 沒有為了忘掉而重建一個空的 cache.db；
    /// - 帳號目錄與整個 server 目錄都刪掉，`account status` 列不出它。
    #[tokio::test]
    async fn destroying_the_last_account_forgets_first_and_leaves_nothing() {
        let (dir, core) = scratch_unlocked("last");
        let alice = account_of(&core, &dir, "@alice:localhost");
        let server_dir = alice.server_dir();
        seed_events(&core, &alice, "@alice:localhost", "$a");

        // 🚫 不封 session：登出不必連網路，閘門也跳過（已登出的帳號）。
        let result = core
            .destroy_account("@ALICE:LocalHost", Some(SERVER), true, false)
            .await
            .expect("destroy 要成功");

        assert_eq!(
            result.events_removed, 1,
            "🚨 忘掉鏈要在 cache.db 被刪之前跑"
        );
        assert!(result.account_dir_removed);
        assert!(result.server_dir_removed);
        assert!(!alice.dir.exists());
        assert!(!server_dir.exists(), "🚫 不准留下（或重建出）空的 cache.db");
        assert!(
            core.account_status().unwrap().accounts.is_empty(),
            "什麼都不留"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 🚨 同 server 還有別的帳號：只刪這個帳號的目錄，server 目錄與 cache.db 留著；
    /// 而且忘掉鏈走**唯一寫入者**，🚫 不另開 `cache_of` 連線（daemon 裡一台 server 只准一個寫入者，#32）。
    #[tokio::test]
    async fn destroying_one_account_keeps_the_server_and_goes_through_the_one_writer() {
        let (dir, core) = scratch_unlocked("one-of-two");
        let alice = account_of(&core, &dir, "@alice:localhost");
        let bob = account_of(&core, &dir, "@bob:localhost");
        let server_dir = alice.server_dir();
        // bob 算登入中（`has_any_logged_in_account` 只看 session 檔在不在）：cache.db 不會在登出時被刪。
        std::fs::write(bob.session_path(), b"not a real session").unwrap();
        seed_events(&core, &alice, "@alice:localhost", "$a");
        seed_events(&core, &bob, "@bob:localhost", "$b");

        let result = core
            .destroy_account("@alice:localhost", Some(SERVER), true, false)
            .await
            .expect("destroy 要成功");

        assert_eq!(result.events_removed, 1);
        assert!(result.account_dir_removed);
        assert!(!result.server_dir_removed, "bob 還在");
        assert!(!alice.dir.exists());
        assert!(bob.dir.exists() && server_dir.join(wbf_sdk::cache::CACHE_FILE_NAME).exists());
        assert_eq!(
            crate::handles::get_raw_cache_opens_for(&server_dir),
            0,
            "🚨 destroy 不准繞過唯一寫入者另開連線"
        );
        assert_eq!(crate::server_cache::get_writers_started_for(&server_dir), 1);
        let bob_view = core
            .server_cache_of(&bob, SERVER)
            .unwrap()
            .read()
            .await
            .history("@bob:localhost", "!r", None, 10)
            .unwrap();
        assert_eq!(bob_view.len(), 1, "別的帳號的東西不動");
        drop(bob_view);
        core.close_server_cache(&server_dir).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn server_dir_fixture(
        name: &str,
    ) -> (std::path::PathBuf, std::path::PathBuf, std::path::PathBuf) {
        let root =
            std::env::temp_dir().join(format!("wbf-core-destroy-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let server_dir = root.join("s").join("srv");
        let account_dir = server_dir.join("a").join("acct");
        std::fs::create_dir_all(account_dir.join("m")).unwrap();
        std::fs::create_dir_all(server_dir.join("media")).unwrap();
        std::fs::write(server_dir.join(wbf_sdk::cache::CACHE_FILE_NAME), b"db").unwrap();
        std::fs::write(server_dir.join("media").join("pool"), b"bytes").unwrap();
        (root, server_dir, account_dir)
    }

    fn is_cache_db(path: &std::path::Path) -> bool {
        path.file_name() == Some(std::ffi::OsStr::new(wbf_sdk::cache::CACHE_FILE_NAME))
    }

    /// 🚨 **回 Err ⇒ 帳號目錄還在**（PR #40 審查 rumia🔴）：server 目錄裡別的東西刪不掉時，帳號目錄沒動，
    /// 所以重跑 destroy 找得到它、能接著清。
    #[test]
    fn a_failure_before_the_account_dir_is_removed_leaves_it_for_a_retry() {
        let (root, server_dir, account_dir) = server_dir_fixture("order");
        let mut fail_on_cache_db = |path: &std::path::Path| {
            if is_cache_db(path) {
                Err(CoreError::new(
                    CoreErrorKind::Io,
                    "simulated: cache.db is busy",
                ))
            } else {
                remove_path_if_present(path)
            }
        };
        let error = remove_server_dir_with_the_account_last(
            &server_dir,
            &account_dir,
            &mut fail_on_cache_db,
            &mut |dir| std::fs::remove_dir(dir),
        )
        .err()
        .expect("刪不掉 cache.db 要回 Err");
        assert_eq!(error.kind, CoreErrorKind::Io);
        assert!(
            account_dir.exists(),
            "🚨 帳號目錄還在：重跑 destroy 找得到它"
        );

        let removal = remove_server_dir_with_the_account_last(
            &server_dir,
            &account_dir,
            &mut remove_path_if_present,
            &mut |dir| std::fs::remove_dir(dir),
        )
        .unwrap();
        assert!(removal.account_dir_removed && removal.server_dir_removed);
        assert!(!server_dir.exists(), "重跑之後整個清掉");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 🚨 帳號目錄**刪掉之後**的收尾失敗 🚫 不回 Err（PR #40 審查 rumia🔴 第二輪）：回 Err 等於叫呼叫端重跑，
    /// 但重跑已經找不到帳號。留下的只有空目錄，記在 `left_behind`。
    #[test]
    fn a_failure_after_the_account_dir_is_gone_is_reported_but_not_an_error() {
        let (root, server_dir, account_dir) = server_dir_fixture("tail");
        let removal = remove_server_dir_with_the_account_last(
            &server_dir,
            &account_dir,
            &mut remove_path_if_present,
            &mut |_| Err(std::io::Error::other("simulated: permission denied")),
        )
        .expect("帳號目錄已經刪掉了，收尾失敗不能回 Err");
        assert!(removal.account_dir_removed);
        assert!(!removal.server_dir_removed);
        assert!(removal.left_behind.is_some());
        assert!(!account_dir.exists());
        let mut leftovers: Vec<_> = std::fs::read_dir(&server_dir)
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name())
            .collect();
        leftovers.sort();
        assert_eq!(
            leftovers,
            vec![
                std::ffi::OsString::from("a"),
                std::ffi::OsString::from("to_be_deleted.lock")
            ],
            "只剩空的 a/，而 🚨 to_be_deleted.lock 留著：登入照樣被擋"
        );
        assert_eq!(std::fs::read_dir(server_dir.join("a")).unwrap().count(), 0);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 🚨 `to_be_deleted.lock` 從第一步就放下、**比帳號目錄晚**才收：刪帳號目錄的那一刻它一定在（維護者 2026-09-15）。
    #[test]
    fn the_server_lock_is_down_before_anything_is_removed_and_up_only_at_the_end() {
        let (root, server_dir, account_dir) = server_dir_fixture("server-lock");
        let server_lock = server_dir.join(crate::account_lock::TO_BE_DELETED_LOCK_FILE_NAME);
        let lock_to_watch = server_lock.clone();
        let mut removed_while_unmarked = Vec::new();
        let mut watch_the_marker = |path: &std::path::Path| {
            if !lock_to_watch.exists() {
                removed_while_unmarked.push(path.to_path_buf());
            }
            remove_path_if_present(path)
        };
        let removal = remove_server_dir_with_the_account_last(
            &server_dir,
            &account_dir,
            &mut watch_the_marker,
            &mut |dir| std::fs::remove_dir(dir),
        )
        .unwrap();
        assert!(
            removed_while_unmarked.is_empty(),
            "🚨 沒標記就刪了：{removed_while_unmarked:?}"
        );
        assert!(removal.server_dir_removed);
        assert!(!server_dir.exists(), "刪完整個目錄，標記跟著消失");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// ⭐ 改名之後才出事：原本的路徑已經不在（登入不會被擋），留下的是 `🗑️…`，掃描時跳過（維護者 2026-09-15）。
    #[test]
    fn a_failure_after_the_rename_leaves_only_a_to_be_deleted_dir() {
        let (root, server_dir, account_dir) = server_dir_fixture("renamed");
        let removal = remove_server_dir_with_the_account_last(
            &server_dir,
            &account_dir,
            &mut remove_path_if_present,
            &mut |dir| {
                let is_trash = dir.file_name().is_some_and(|name| {
                    crate::account_lock::is_to_be_deleted_dir_name(&name.to_string_lossy())
                });
                if is_trash {
                    Err(std::io::Error::other("simulated: permission denied"))
                } else {
                    std::fs::remove_dir(dir)
                }
            },
        )
        .unwrap();
        assert!(removal.account_dir_removed && !removal.server_dir_removed);
        assert!(removal.left_behind.is_some());
        assert!(
            !server_dir.exists(),
            "原本的路徑已經不在：登入這台 server 不會被 to_be_deleted.lock 擋住"
        );
        let renamed = root
            .join("s")
            .join(format!("{}srv", crate::account_lock::TO_BE_DELETED_PREFIX));
        assert!(renamed.exists(), "留下的是一看就知道是垃圾的目錄");
        assert!(
            !renamed
                .join(crate::account_lock::TO_BE_DELETED_LOCK_FILE_NAME)
                .exists(),
            "標記在改名之後才收"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 改名的目標已經在（上一次也停在這裡）：🚫 不覆蓋，停下；`to_be_deleted.lock` 留著、登入照樣被擋。
    #[test]
    fn an_earlier_to_be_deleted_dir_stops_the_rename_and_keeps_the_marker() {
        let (root, server_dir, account_dir) = server_dir_fixture("rename-taken");
        let earlier = root
            .join("s")
            .join(format!("{}srv", crate::account_lock::TO_BE_DELETED_PREFIX));
        std::fs::create_dir_all(earlier.join("leftover")).unwrap();
        let removal = remove_server_dir_with_the_account_last(
            &server_dir,
            &account_dir,
            &mut remove_path_if_present,
            &mut |dir| std::fs::remove_dir(dir),
        )
        .unwrap();
        assert!(removal.account_dir_removed && !removal.server_dir_removed);
        assert!(server_dir
            .join(crate::account_lock::TO_BE_DELETED_LOCK_FILE_NAME)
            .exists());
        assert!(earlier.join("leftover").exists(), "🚫 不碰上一次留下的東西");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 🚨 `to_be_deleted.lock` 在 → 登入這台 server 被拒，在碰目錄、連網路之前（維護者 2026-09-15）。
    #[tokio::test]
    async fn a_login_to_a_server_being_removed_is_refused() {
        let (dir, core) = scratch_unlocked("pending-removal");
        let server = "http://127.0.0.1:1";
        let key = core.vault().unwrap().account_dir_key();
        let account = AccountDir::locate(&dir, &key, server, "@alice:localhost").unwrap();
        std::fs::create_dir_all(account.server_dir()).unwrap();
        std::fs::write(
            account
                .server_dir()
                .join(crate::account_lock::TO_BE_DELETED_LOCK_FILE_NAME),
            b"",
        )
        .unwrap();

        let error = core
            .log_in(server, "@alice:localhost", "password", "test", false)
            .await
            .unwrap_err();
        assert_eq!(
            error.kind,
            CoreErrorKind::ServerPendingRemoval,
            "{}",
            error.message
        );
        assert!(!account.dir.exists(), "🚫 沒建帳號目錄");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 🚨 清到一半有人建了新帳號（PR #40 審查 rumia🟡）：只刪這個帳號的目錄，🚫 不收 server 目錄、不碰新帳號。
    #[test]
    fn an_account_created_during_cleanup_is_left_alone() {
        let (root, server_dir, account_dir) = server_dir_fixture("race");
        let newcomer = server_dir.join("a").join("newcomer");
        let newcomer_to_create = newcomer.clone();
        let mut create_a_newcomer_while_removing = |path: &std::path::Path| {
            if is_cache_db(path) {
                std::fs::create_dir_all(&newcomer_to_create).unwrap();
            }
            remove_path_if_present(path)
        };
        let removal = remove_server_dir_with_the_account_last(
            &server_dir,
            &account_dir,
            &mut create_a_newcomer_while_removing,
            &mut |dir| std::fs::remove_dir(dir),
        )
        .unwrap();
        assert!(removal.account_dir_removed);
        assert!(!removal.server_dir_removed);
        assert!(!account_dir.exists());
        assert!(newcomer.exists(), "🚫 不碰中途新建的帳號");
        // ⭐ 刪帳號目錄之前那次再檢查看到了它：這是正常結果，🚫 不是「收不掉」的警告。
        // （沒有那次檢查也不會刪到新帳號 —— `remove_dir` 會拒絕非空的 a/ —— 但會變成 left_behind。）
        assert!(removal.left_behind.is_none(), "{:?}", removal.left_behind);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 🚨 關不掉快取（有人還拿著）→ destroy 失敗、**什麼目錄都沒刪**；放掉之後重跑就清乾淨。
    #[tokio::test]
    async fn a_destroy_that_cannot_close_the_cache_removes_nothing_and_can_be_retried() {
        let (dir, core) = scratch_unlocked("retry");
        let alice = account_of(&core, &dir, "@alice:localhost");
        let server_dir = alice.server_dir();
        seed_events(&core, &alice, "@alice:localhost", "$a");
        let held = core.server_cache_of(&alice, SERVER).unwrap();

        let error = core
            .destroy_account("@alice:localhost", Some(SERVER), true, false)
            .await
            .unwrap_err();
        assert_eq!(error.kind, CoreErrorKind::Io, "{}", error.message);
        assert!(alice.dir.exists(), "🚨 帳號目錄還在");
        assert!(server_dir.exists());

        drop(held);
        let result = core
            .destroy_account("@alice:localhost", Some(SERVER), true, false)
            .await
            .expect("放掉之後重跑要成功");
        assert!(result.server_dir_removed);
        assert!(!server_dir.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ⭐ `cache.db` 不在就不開：跟「在不在」同一把鎖判斷（PR #40 審查 rumia🟡），🚫 不建一個空的。
    #[test]
    fn a_missing_cache_db_is_not_created_just_to_look_inside() {
        let (dir, core) = scratch_unlocked("if-present");
        let alice = account_of(&core, &dir, "@alice:localhost");
        let server_dir = alice.server_dir();
        assert!(core
            .find_server_cache_if_present(&alice, SERVER)
            .unwrap()
            .is_none());
        assert!(!server_dir.join(wbf_sdk::cache::CACHE_FILE_NAME).exists());
        assert_eq!(crate::server_cache::get_writers_started_for(&server_dir), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 🚨 登入／登出正握著帳號生命週期鎖 → destroy **被拒、什麼都沒動**（PR #40 審查 rumia🔴 第三輪）。
    #[tokio::test]
    async fn a_destroy_is_refused_while_another_account_operation_holds_the_lock() {
        let (dir, core) = scratch_unlocked("busy-destroy");
        let alice = account_of(&core, &dir, "@alice:localhost");
        seed_events(&core, &alice, "@alice:localhost", "$a");
        let in_progress = crate::account_lock::lock_account_lifecycle(&dir).unwrap();

        let error = core
            .destroy_account("@alice:localhost", Some(SERVER), true, false)
            .await
            .unwrap_err();
        assert_eq!(error.kind, CoreErrorKind::AccountBusy, "{}", error.message);
        assert!(
            alice.dir.exists()
                && alice
                    .server_dir()
                    .join(wbf_sdk::cache::CACHE_FILE_NAME)
                    .exists()
        );
        let still_there = core
            .server_cache_of(&alice, SERVER)
            .unwrap()
            .read()
            .await
            .history("@alice:localhost", "!r", None, 10)
            .unwrap();
        assert_eq!(still_there.len(), 1, "🚫 忘掉鏈也沒跑");

        drop(in_progress);
        core.close_server_cache(&alice.server_dir()).unwrap();
        core.destroy_account("@alice:localhost", Some(SERVER), true, false)
            .await
            .expect("放手之後 destroy 照常");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 🚨 destroy 正握著鎖 → 同時進來的**登入被拒**，而且在碰任何目錄、連任何網路之前就被拒。
    #[tokio::test]
    async fn a_login_is_refused_while_a_destroy_holds_the_lock() {
        let (dir, core) = scratch_unlocked("busy-login");
        let destroying = crate::account_lock::lock_account_lifecycle(&dir).unwrap();
        // server 位址是一個不會有人聽的埠：真的去連就會變成 Network，而不是 AccountBusy。
        let error = core
            .log_in(
                "http://127.0.0.1:1",
                "@alice:localhost",
                "password",
                "test",
                false,
            )
            .await
            .unwrap_err();
        assert_eq!(error.kind, CoreErrorKind::AccountBusy, "{}", error.message);
        drop(destroying);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// logout 也拿同一把鎖。
    #[tokio::test]
    async fn a_logout_is_refused_while_the_lock_is_held() {
        let (dir, core) = scratch_unlocked("busy-logout");
        let _alice = account_of(&core, &dir, "@alice:localhost");
        let held = crate::account_lock::lock_account_lifecycle(&dir).unwrap();
        let error = core
            .log_out("@alice:localhost", Some(SERVER), true, false)
            .await
            .unwrap_err();
        assert_eq!(error.kind, CoreErrorKind::AccountBusy, "{}", error.message);
        drop(held);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn only_a_path_shaped_like_an_account_dir_may_be_removed() {
        let data_dir = std::path::Path::new("/data");
        assert!(is_account_dir_of(
            data_dir,
            std::path::Path::new("/data/s/srv/a/acct")
        ));
        assert!(
            !is_account_dir_of(data_dir, std::path::Path::new("/data/s/srv/a")),
            "上一層不是 a/"
        );
        assert!(!is_account_dir_of(
            data_dir,
            std::path::Path::new("/data/s/srv")
        ));
        assert!(
            !is_account_dir_of(data_dir, std::path::Path::new("/other/s/srv/a/acct")),
            "別的 data dir"
        );
        assert!(
            !is_account_dir_of(data_dir, std::path::Path::new("/data/x/srv/a/acct")),
            "不是 s/"
        );
        assert!(
            !is_account_dir_of(data_dir, std::path::Path::new("/data/s/srv/b/acct")),
            "不是 a/"
        );
        assert!(
            !is_account_dir_of(data_dir, std::path::Path::new("/data/s/srv/a/..")),
            "🚫 .. 不算名字"
        );
        assert!(!is_account_dir_of(data_dir, std::path::Path::new("/")));
    }

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

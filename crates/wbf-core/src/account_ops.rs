//! 帳號家族的操作：列出、切換、找出「使用者打的那串是誰」，以及 recovery key 的查詢。
//!
//! ⚠️ 這裡的每個公開方法都**收字串、回可序列化的 DTO**（architecture-v2 §7）。
//! `AccountDir`、`DataDirMap`、`Session`、`Vault` 一個都不過邊界——帳號的身分在邊界上
//! 就是一串 **mxid**（PR #24 審查 cirno🔴）。

use serde::Serialize;
use zeroize::Zeroizing;

use crate::error::{CoreError, CoreErrorKind};

use wbf_sdk::vault::KeyMode;

use crate::accounts::{self, AccountDir};
use crate::recovery;
use crate::{Core, Target};

/// 「我是誰」。🚫 刻意**沒有** `access_token`：那是秘密，不過邊界（`handles` 模組註解）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct WhoAmI {
    pub user_id: String,
    pub device_id: String,
    pub server: String,
}

/// `account status` 的結果。
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct AccountStatus {
    pub accounts: Vec<crate::AccountSummary>,
    /// `s/` 底下有目錄但一個都解不開時該說的那句話（換過 `local.key`、或舊版留下的）。
    /// ⚠️ 這是**資料**不是顯示：前端自己決定要印還是要跳視窗（§3）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub undecryptable_hint: Option<String>,
}

/// `account switch` 的結果。
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SwitchResult {
    /// 現在的 current，給人看的一句話, example: "@alice:localhost on http://localhost:6167"
    pub current: String,
    /// 換掉的是誰；本來就沒有 current（第一次登入）或本來就是它時是 `None`
    #[serde(skip_serializing_if = "Option::is_none")]
    pub switched_from: Option<String>,
    /// 這個帳號登入著嗎。⚠️ 切到一個登出的帳號是允許的，但要看得出來
    pub logged_in: bool,
}

impl Core {
    /// 這台機器上的每個帳號，加上「一個都解不開」的提示。
    pub fn account_status(&self) -> Result<AccountStatus, CoreError> {
        let map = self.refresh_data_dir_map()?;
        Ok(AccountStatus {
            undecryptable_hint: map.find_undecryptable_layout_hint(),
            accounts: map.list_accounts(self.vault()?)?,
        })
    }

    /// 把 `current` 指到這個帳號。
    ///
    /// Args:
    ///     user: **完整 mxid**, example: "@bob:matrix.org"
    ///     server: 同名 localpart 在多個 server 時消歧, example: Some("http://localhost:6167")
    pub fn switch_current(
        &self,
        user: &str,
        server: Option<&str>,
    ) -> Result<SwitchResult, CoreError> {
        let account = self.find_account_by_full_mxid(user, server)?;
        let switched_from = self.switch_current_to(&account)?;
        Ok(SwitchResult {
            current: self.describe_account(&account),
            logged_in: account.is_logged_in(),
            switched_from,
        })
    }

    /// 這台機器保管的 recovery key 裡，對應這串 mxid 的那一把。
    ///
    /// ⚠️ **會回傳秘密本身**——只有「使用者明說要看」的路徑該叫它（rpc-cli 的
    /// `recovery show`）。🚫 不要拿它做判斷用；要問「有沒有保管」用
    /// [`Core::list_recovery_key_users`]。
    ///
    /// Return:
    ///     Ok(Some(Zeroizing<String>))   那串 key
    ///     Ok(None)                      沒保管
    ///     Err(Usage)                    只差大小寫的保管著好幾把（🚫 不猜）
    pub fn find_recovery_key(&self, user: &str) -> Result<Option<Zeroizing<String>>, CoreError> {
        let map = self.refresh_data_dir_map()?;
        let Some(user_id) = map.find_recovery_key_user_id(user)? else {
            return Ok(None);
        };
        Ok(recovery::find(&self.data_dir, self.vault()?, user_id)?)
    }

    /// 「我是誰」。⚠️ 這是**問過 server** 的答案，不是本地那份的複述——
    /// `--token` 之外的路徑本地就有，但 `whoami` 的語意是「server 認為我是誰」。
    pub async fn whoami(&self, target: &Target) -> Result<WhoAmI, CoreError> {
        let account = self.account_or_current(target)?;
        let session = self.session_of(&account)?;
        let who = wbf_sdk::login::whoami(&session).await?;
        Ok(WhoAmI {
            user_id: who.user_id,
            device_id: who.device_id,
            server: session.server,
        })
    }

    /// `current` 指到誰（完整 mxid）。`logout` 要拿它當目標。
    pub fn current_user_id(&self) -> Result<String, CoreError> {
        let account = self.current_account()?;
        Ok(self.session_of(&account)?.user_id)
    }

    /// 設或拿掉 `local.key` 的 passphrase。**只重包主金鑰**，其他檔案不動。
    ///
    /// ⚠️ 呼叫端要記得作廢舊的 ticket——它是用舊 passphrase 換來的。
    ///
    /// Args:
    ///     passphrase: `None` 就是拿掉（變成 plain 模式）
    pub fn set_passphrase(&self, passphrase: Option<&[u8]>) -> Result<KeyMode, CoreError> {
        let unlock = match passphrase {
            Some(bytes) => wbf_sdk::Unlock::Passphrase(zeroize::Zeroizing::new(bytes.to_vec())),
            None => wbf_sdk::Unlock::NoPassphrase,
        };
        // ⚠️ `set_unlock` 要 `&mut Vault`，而 core 裡那把是共享的——所以另外開一把來改，
        // 改完之後記憶體裡那把的主金鑰沒變（只有檔案的包裝法變了）。
        let mut vault = wbf_sdk::vault::Vault::from_master(
            &self.data_dir,
            self.vault()?.master_key().clone(),
            self.vault()?.mode(),
        );
        vault.set_unlock(&unlock)?;
        Ok(vault.mode())
    }

    /// 這個帳號的 homeserver URL（`session.sealed` 裡那個，權威）。
    ///
    /// 📎 給「這份 manifest 是不是這台 server 的」那種核對用。
    pub fn current_server(&self, target: &Target) -> Result<String, CoreError> {
        let account = self.account_or_current(target)?;
        Ok(self.session_of(&account)?.server)
    }

    // ---- 以下 pub(crate)：回傳裡有 `AccountDir`，不過邊界 ----

    /// `switch`／`del`／`destroy` 的 `<user>`：**一律完整 mxid**（CLI 規格 §3.1）——
    /// 這些命令會登出、會刪檔，變更的對象不該靠猜。只給 localpart 就報錯並列出本機的帳號，
    /// 🚫 不推測、🚫 不拿唯一一個頂替。
    pub(crate) fn find_account_by_full_mxid(
        &self,
        user: &str,
        server: Option<&str>,
    ) -> Result<AccountDir, CoreError> {
        if !user.starts_with('@') || !user.contains(':') {
            let known = self.account_status()?;
            let names: Vec<String> = known
                .accounts
                .iter()
                .map(|summary| match &summary.user_id {
                    Some(user_id) => user_id.clone(),
                    None => format!("{} on {} (logged out)", summary.localpart, summary.server),
                })
                .collect();
            let names = if names.is_empty() {
                "none".to_string()
            } else {
                names.join(", ")
            };
            return Err(CoreError::new(
                CoreErrorKind::Usage,
                format!(
                    "expected a full Matrix ID like @bob:matrix.org, got \"{user}\"
       accounts on this machine: {names}"
                ),
            ));
        }
        self.find_account(user, server)
    }

    /// 給人看的一句話。登入中的用 `session.sealed` 裡的**權威** mxid 與 server URL；
    /// 登出的只剩目錄名解出來的明文，組不出可靠的 mxid，所以老實說它登出了。
    pub(crate) fn describe_account(&self, account: &AccountDir) -> String {
        match self.session_of(account) {
            Ok(session) => format!("{} on {}", session.user_id, session.server),
            Err(_) => format!("{} (not logged in)", account.label()),
        }
    }

    /// 改 `current`，並發一個 `Progress` 事件（CLI 規格 §3.1.1 的 switch 提示）。
    ///
    /// Return:
    ///     Ok(Some(String))   換掉的是誰
    ///     Ok(None)           本來就沒有 current，或本來就是它
    pub(crate) fn switch_current_to(
        &self,
        account: &AccountDir,
    ) -> Result<Option<String>, CoreError> {
        let dir_key = self.vault()?.account_dir_key();
        let previous = accounts::read_current(&self.data_dir)?
            .and_then(|current| {
                accounts::find_account_of_current(&self.data_dir, &dir_key, &current)
            })
            .filter(|previous| previous.dir != account.dir)
            .map(|previous| self.describe_account(&previous));
        accounts::write_current(&self.data_dir, account)?;
        let now = self.describe_account(account);
        self.events.progress(match &previous {
            Some(previous) => format!("switched to {now} (was {previous})"),
            None => format!("switched to {now} (no previous account)"),
        });
        Ok(previous)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wbf_sdk::vault::Vault;
    use wbf_sdk::Unlock;

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("wbf-core-acc-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn unlocked(dir: &std::path::Path) -> Core {
        Vault::create(dir, &Unlock::NoPassphrase).unwrap();
        let core = Core::open(dir);
        core.unlock(None).unwrap();
        core
    }

    #[test]
    fn a_bare_localpart_is_refused_and_the_message_lists_what_is_here() {
        let dir = scratch("bare");
        let core = unlocked(&dir);
        // 🚫 會刪檔的命令不准靠猜：只給 localpart 就報錯。
        let error = core.find_account_by_full_mxid("alice", None).unwrap_err();
        let message = format!("{error}");
        assert!(message.contains("expected a full Matrix ID"), "{message}");
        assert!(message.contains("none"), "沒有帳號時要說 none：{message}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn switching_emits_progress_and_reports_what_it_replaced() {
        let dir = scratch("switch");
        let core = unlocked(&dir);
        let key = core.vault().unwrap().account_dir_key();
        for user in ["@alice:localhost", "@bob:localhost"] {
            let account = AccountDir::locate(&dir, &key, "http://localhost:6167", user).unwrap();
            std::fs::create_dir_all(&account.dir).unwrap();
        }

        let mut events = core.subscribe();
        let first = core.switch_current("@alice:localhost", None).unwrap();
        assert_eq!(first.switched_from, None, "第一次沒有前一個");
        assert!(!first.logged_in, "只有目錄、沒有 session.sealed");
        assert!(first.current.contains("alice"));

        let second = core.switch_current("@bob:localhost", None).unwrap();
        assert!(second.switched_from.unwrap().contains("alice"));

        // 事件走 channel，🚫 core 不印東西。
        let crate::CoreEvent::Progress(line) = events.try_recv().unwrap() else {
            panic!("switch 發的是 Progress，不是別的");
        };
        assert!(line.contains("switched to"), "{line}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn account_status_carries_the_undecryptable_hint_as_data() {
        let dir = scratch("hint");
        let core = unlocked(&dir);
        let key = core.vault().unwrap().account_dir_key();
        let account =
            AccountDir::locate(&dir, &key, "http://localhost:6167", "@alice:localhost").unwrap();
        std::fs::create_dir_all(&account.dir).unwrap();
        // 自己的金鑰解得開就不該有提示。
        assert_eq!(core.account_status().unwrap().undecryptable_hint, None);

        // 換一把金鑰：解不開，提示要是**資料**，不是 core 印出去的東西。
        let other_dir = scratch("hint-other");
        Vault::create(&other_dir, &Unlock::NoPassphrase).unwrap();
        std::fs::copy(
            other_dir.join(wbf_sdk::vault::KEY_FILE_NAME),
            dir.join(wbf_sdk::vault::KEY_FILE_NAME),
        )
        .unwrap();
        let core = Core::open(&dir);
        core.unlock(None).unwrap();
        let status = core.account_status().unwrap();
        assert!(status.accounts.is_empty());
        assert!(status.undecryptable_hint.is_some());
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&other_dir);
    }
}

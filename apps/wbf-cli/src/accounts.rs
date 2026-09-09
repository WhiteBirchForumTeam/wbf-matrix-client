//! 每個帳號的資料放哪（CLI 規格 §7；local-cache-db.md §5.6、§11）：
//!
//! ```text
//! <data dir>/
//!   local.key、unlock.ticket            一台機器一把主金鑰（vault）
//!   current                             目前帳號：一行 "<加密的 server 目錄名>/<加密的帳號目錄名>"
//!   servers/<b58>_<b58>/                正規化過的 server host，加密（§11.2）
//!     cache.db                          這個 server 上所有帳號共用的快取
//!     accounts/<b58>_<b58>/             localpart，加密
//!       session.sealed                  這個帳號的 session（第三把子金鑰封住）
//!       matrix/                         matrix-sdk store，綁 device；logout 刪
//! ```
//!
//! **兩層目錄名都是加密的**，所以這個模組的每個進入點都要第六把子金鑰（`vault.account_dir_key()`）。
//! 明文的 server URL 與 mxid 仍然在 `session.sealed` 裡，不從目錄名反推。
//!
//! 路徑映射刻意**沒有**全域的可變 map（維護者 2026-09-09 說「全局變數 map，或寫成 function」，
//! 這裡選後者）：加密是確定性的，所以定位單一帳號用 `AccountDir::locate` 直接算得出來；
//! 只有「列出全部」才需要掃描，而掃描的結果就是 `list_accounts` 的回傳值，不必存成狀態。

use std::path::{Path, PathBuf};

use serde::Serialize;
use wbf_sdk::account_dir::{find_dir_name_plaintext, to_dir_name, DirScope};
use wbf_sdk::vault::{write_private, Key32, Vault, SEALED_SESSION_FILE_NAME};
use wbf_sdk::SdkError;

pub const SERVERS_DIR_NAME: &str = "servers";
pub const ACCOUNTS_DIR_NAME: &str = "accounts";
pub const MATRIX_STORE_DIR_NAME: &str = "matrix";
pub const CURRENT_FILE_NAME: &str = "current";

/// 一個帳號在磁碟上的位置。`server_host` 與 `localpart` 是明文，`dir` 裡的兩段是加密的。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccountDir {
    /// 正規化過的 server host（明文），example: "localhost:6167"
    pub server_host: String,
    /// localpart（明文），example: "alice"
    pub localpart: String,
    /// `<data dir>/servers/<b58>_<b58>/accounts/<b58>_<b58>`
    pub dir: PathBuf,
    /// `current` 檔記的就是這兩段（都是加密後的名字）。
    server_dir_name: String,
    account_dir_name: String,
}

impl AccountDir {
    /// 算出這個帳號的目錄。**不掃描、不碰磁碟**：加密是確定性的，同一把金鑰配同一個帳號永遠是同一條路徑。
    ///
    /// Args:
    ///     data_dir: example: "<data dir>"
    ///     key: example: vault.account_dir_key()
    ///     server: example: "http://localhost:6167"
    ///     user: mxid 或 localpart, example: "@alice:localhost"
    /// Return:
    ///     Ok(AccountDir)
    ///     Err(Usage)   localpart 是空的、或加密後的名字太長（§11.4）
    pub fn locate(
        data_dir: &Path,
        key: &Key32,
        server: &str,
        user: &str,
    ) -> Result<AccountDir, SdkError> {
        let server_host = server_host_of(server);
        let localpart = localpart_of(user).to_string();
        let server_dir_name = to_dir_name(key, DirScope::Server, &server_host)?;
        let account_dir_name = to_dir_name(
            key,
            DirScope::Account {
                server_host: &server_host,
            },
            &localpart,
        )?;
        Ok(AccountDir {
            dir: data_dir
                .join(SERVERS_DIR_NAME)
                .join(&server_dir_name)
                .join(ACCOUNTS_DIR_NAME)
                .join(&account_dir_name),
            server_host,
            localpart,
            server_dir_name,
            account_dir_name,
        })
    }

    pub fn session_path(&self) -> PathBuf {
        self.dir.join(SEALED_SESSION_FILE_NAME)
    }

    pub fn matrix_store_dir(&self) -> PathBuf {
        self.dir.join(MATRIX_STORE_DIR_NAME)
    }

    /// `servers/<b58>_<b58>/`：`cache.db` 與媒體池在這一層，同 server 的帳號共用。
    pub fn server_dir(&self) -> PathBuf {
        self.dir
            .parent()
            .and_then(Path::parent)
            .map(Path::to_path_buf)
            .unwrap_or_else(|| self.dir.clone())
    }

    pub fn is_logged_in(&self) -> bool {
        self.session_path().exists()
    }

    /// `current` 檔的內容：兩段**加密後**的目錄名。🚫 不寫明文（寫了等於把剛加密的名字再漏一次）。
    pub fn key(&self) -> String {
        format!("{}/{}", self.server_dir_name, self.account_dir_name)
    }

    /// 給人看的名字（錯誤訊息用），example: "alice on localhost:6167"
    pub fn label(&self) -> String {
        format!("{} on {}", self.localpart, self.server_host)
    }

    /// 裝置層的狀態：matrix-sdk 的 store（綁 device_id）。logout 或換裝置時丟；`cache.db`（綁 server）與 `local.key` 不動。
    pub fn delete_matrix_store(&self) -> Result<(), SdkError> {
        match std::fs::remove_dir_all(self.matrix_store_dir()) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }
}

/// `accounts` 命令印的一列。
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct AccountSummary {
    /// 權威的完整 mxid，來源是 `session.sealed`。**登出的帳號沒有**（目錄名只解得出 localpart 與 host，
    /// 組不出可靠的 mxid），所以是 `null` —— 🚫 不自己拼一個出來騙人。
    pub user_id: Option<String>,
    pub server: String,
    pub localpart: String,
    pub logged_in: bool,
    pub current: bool,
}

/// 掃 `servers/*/accounts/*` **兩層**，逐一解密目錄名（local-cache-db.md §11.5）。
///
/// 解不開的目錄一律跳過（fail closed）：可能是別把 `local.key` 建的，也可能是舊版留下的明文佈局。
/// 🚫 不猜、🚫 不刪、🚫 不報錯——當它不存在。
///
/// Args:
///     data_dir: example: "<data dir>"
///     vault: 導目錄金鑰、也用來解 session 拿權威 mxid, example: context.vault()?
/// Return:
///     Ok(Vec<AccountSummary>)   照 server、localpart 排序；一個都沒有就是空的
pub fn list_accounts(data_dir: &Path, vault: &Vault) -> Result<Vec<AccountSummary>, SdkError> {
    let key = &vault.account_dir_key();
    let current = read_current(data_dir)?;
    let mut summaries = Vec::new();
    let Ok(server_entries) = std::fs::read_dir(data_dir.join(SERVERS_DIR_NAME)) else {
        return Ok(summaries);
    };
    for server_entry in server_entries {
        let server_entry = server_entry?;
        let server_dir_name = server_entry.file_name().to_string_lossy().into_owned();
        let Some(server_host) = find_dir_name_plaintext(key, DirScope::Server, &server_dir_name)
        else {
            continue;
        };
        let scope = DirScope::Account {
            server_host: &server_host,
        };
        let Ok(account_entries) = std::fs::read_dir(server_entry.path().join(ACCOUNTS_DIR_NAME))
        else {
            continue;
        };
        for account_entry in account_entries {
            let account_entry = account_entry?;
            if !account_entry.file_type()?.is_dir() {
                continue;
            }
            let account_dir_name = account_entry.file_name().to_string_lossy().into_owned();
            let Some(localpart) = find_dir_name_plaintext(key, scope, &account_dir_name) else {
                continue;
            };
            let session_path = account_entry.path().join(SEALED_SESSION_FILE_NAME);
            summaries.push(AccountSummary {
                // 解不開的 session 不擋掉整份清單：那個帳號就當登出的看待（fail closed）。
                user_id: vault
                    .unseal_session(&session_path)
                    .ok()
                    .flatten()
                    .map(|session| session.user_id),
                logged_in: session_path.exists(),
                current: current.as_deref()
                    == Some(format!("{server_dir_name}/{account_dir_name}").as_str()),
                server: server_host.clone(),
                localpart,
            });
        }
    }
    summaries.sort_by(|left, right| {
        left.server
            .cmp(&right.server)
            .then(left.localpart.cmp(&right.localpart))
    });
    Ok(summaries)
}

/// `servers/` 底下有目錄，但一個都解不開 —— 多半是舊版（明文目錄名）留下的，或換過 `local.key`。
/// 維護者 2026-09-09：不寫遷移，砍掉重來，所以這裡只回一句提示給呼叫者印（local-cache-db.md §11.7）。
///
/// Return:
///     Some(String)   該印的那一行
///     None           沒有 `servers/`、或至少解得開一個
pub fn find_undecryptable_layout_hint(data_dir: &Path, key: &Key32) -> Option<String> {
    let servers = data_dir.join(SERVERS_DIR_NAME);
    let entries: Vec<_> = std::fs::read_dir(&servers).ok()?.flatten().collect();
    if entries.is_empty() {
        return None;
    }
    let any_readable = entries.iter().any(|entry| {
        find_dir_name_plaintext(key, DirScope::Server, &entry.file_name().to_string_lossy())
            .is_some()
    });
    (!any_readable).then(|| {
        format!(
            "warning: no directory in {} could be decrypted with this local.key; if this data dir was made by an older build, delete it and run `login` again",
            servers.display()
        )
    })
}

/// 這個 server 底下還有沒有任何登入中的帳號（`logout` 用：都沒有就把 `cache.db` 一起刪）。
pub fn has_any_logged_in_account(server_dir: &Path) -> bool {
    std::fs::read_dir(server_dir.join(ACCOUNTS_DIR_NAME))
        .map(|entries| {
            entries
                .flatten()
                .any(|entry| entry.path().join(SEALED_SESSION_FILE_NAME).exists())
        })
        .unwrap_or(false)
}

/// Return:
///     Ok(Some(String))   `current` 的內容：兩段加密後的目錄名
///     Ok(None)           沒登入過
pub fn read_current(data_dir: &Path) -> Result<Option<String>, SdkError> {
    match std::fs::read_to_string(data_dir.join(CURRENT_FILE_NAME)) {
        Ok(text) => {
            let trimmed = text.trim();
            Ok((!trimmed.is_empty()).then(|| trimmed.to_string()))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

pub fn write_current(data_dir: &Path, account: &AccountDir) -> Result<(), SdkError> {
    write_private(&data_dir.join(CURRENT_FILE_NAME), account.key().as_bytes())
}

pub fn clear_current_if(data_dir: &Path, account: &AccountDir) -> Result<(), SdkError> {
    if read_current(data_dir)?.as_deref() == Some(account.key().as_str()) {
        match std::fs::remove_file(data_dir.join(CURRENT_FILE_NAME)) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

/// `current` 的內容（兩段密文）→ 目錄。解不開或目錄不在就當沒有（fail closed）。
///
/// Args:
///     data_dir: example: "<data dir>"
///     key: example: vault.account_dir_key()
///     current: `read_current` 的回傳, example: "3vQ…_2NE…/5Ab…_7Cd…"
/// Return:
///     Some(AccountDir)
///     None   格式不對、解不開、或那個目錄不存在
pub fn find_account_of_current(data_dir: &Path, key: &Key32, current: &str) -> Option<AccountDir> {
    let (server_dir_name, account_dir_name) = current.split_once('/')?;
    let server_host = find_dir_name_plaintext(key, DirScope::Server, server_dir_name)?;
    let localpart = find_dir_name_plaintext(
        key,
        DirScope::Account {
            server_host: &server_host,
        },
        account_dir_name,
    )?;
    let dir = data_dir
        .join(SERVERS_DIR_NAME)
        .join(server_dir_name)
        .join(ACCOUNTS_DIR_NAME)
        .join(account_dir_name);
    dir.is_dir().then(|| AccountDir {
        server_host,
        localpart,
        dir,
        server_dir_name: server_dir_name.to_string(),
        account_dir_name: account_dir_name.to_string(),
    })
}

/// `--account <mxid 或 localpart>` 解析：給了 `server` 就直接算路徑，沒給就掃所有 server 找同名 localpart。
///
/// Args:
///     data_dir: example: "<data dir>"
///     vault: example: context.vault()?
///     user: example: "@alice:localhost"
///     server: example: Some("http://localhost:6167")
/// Return:
///     Ok(AccountDir)   剛好一個、或 `server` 有給且找得到
///     Err(Usage)       零個；或多個 server 都有這個 localpart 而 `server` 沒給
pub fn find_account(
    data_dir: &Path,
    vault: &Vault,
    user: &str,
    server: Option<&str>,
) -> Result<AccountDir, SdkError> {
    let localpart = localpart_of(user);
    if let Some(server) = server {
        let account = AccountDir::locate(data_dir, &vault.account_dir_key(), server, user)?;
        if account.dir.is_dir() {
            return Ok(account);
        }
        return Err(SdkError::Usage(format!(
            "no account {user} on {server} in {}; run `login` first",
            data_dir.display()
        )));
    }
    let matches: Vec<AccountSummary> = list_accounts(data_dir, vault)?
        .into_iter()
        .filter(|summary| summary.localpart == localpart)
        .collect();
    match matches.as_slice() {
        [] => Err(SdkError::Usage(format!(
            "no account {user} in {}; run `login` first",
            data_dir.display()
        ))),
        [one] => AccountDir::locate(
            data_dir,
            &vault.account_dir_key(),
            &one.server,
            &one.localpart,
        ),
        many => Err(SdkError::Usage(format!(
            "{user} exists on {} servers ({}); pass --server too",
            many.len(),
            many.iter()
                .map(|summary| summary.server.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ))),
    }
}

/// `http://localhost:6167` → `localhost:6167`；`https://matrix.example.org` → `matrix.example.org`（預設 port 不帶）。
///
/// ⚠️ 這是**加密的輸入**（§11.3），不是檔名了：所以要正規化到底（小寫），
/// 🚫 不再過濾 `[A-Za-z0-9._-]` —— 那是為了當檔名才做的，留著只會讓不同的 host 撞成同一個目錄。
pub fn server_host_of(server: &str) -> String {
    let without_scheme = server
        .trim_end_matches('/')
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(server);
    let host_port = without_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or(without_scheme);
    let is_https = server.starts_with("https://");
    let host = match host_port.rsplit_once(':') {
        Some((host, port)) if port.chars().all(|c| c.is_ascii_digit()) => {
            let default = if is_https { "443" } else { "80" };
            if port == default {
                host.to_string()
            } else {
                format!("{host}:{port}")
            }
        }
        _ => host_port.to_string(),
    };
    host.to_lowercase()
}

/// `@alice:localhost` → `alice`；`alice` → `alice`。
pub fn localpart_of(user: &str) -> &str {
    let stripped = user.strip_prefix('@').unwrap_or(user);
    stripped
        .split_once(':')
        .map(|(local, _)| local)
        .unwrap_or(stripped)
}

#[cfg(test)]
mod tests {
    use super::*;

    use wbf_sdk::Unlock;

    /// 測試用的資料目錄，裡面建好一把 `local.key`（目錄名的加密要它）。
    fn scratch_dir_with_vault(name: &str) -> (PathBuf, Vault) {
        let dir = std::env::temp_dir().join(format!("wbf-accounts-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let vault = Vault::create(&dir, &Unlock::NoPassphrase).unwrap();
        (dir, vault)
    }

    #[test]
    fn server_host_drops_scheme_and_default_port() {
        assert_eq!(server_host_of("http://localhost:6167"), "localhost:6167");
        assert_eq!(
            server_host_of("https://matrix.example.org"),
            "matrix.example.org"
        );
        assert_eq!(
            server_host_of("https://matrix.example.org:443/"),
            "matrix.example.org"
        );
        assert_eq!(server_host_of("http://example.org:80"), "example.org");
        assert_eq!(server_host_of("http://10.0.0.1:8008/path"), "10.0.0.1:8008");
    }

    #[test]
    fn server_host_is_lowercased_so_one_server_gets_one_directory() {
        // 加密是逐 byte 的：漏了這一步，同一台 server 打成大寫就會長出第二個目錄（§11.3）。
        assert_eq!(
            server_host_of("https://MATRIX.example.ORG"),
            server_host_of("https://matrix.example.org")
        );
    }

    #[test]
    fn localpart_parsing() {
        assert_eq!(localpart_of("@alice:localhost"), "alice");
        assert_eq!(localpart_of("alice"), "alice");
    }

    #[test]
    fn locate_is_deterministic_and_hides_both_levels() {
        let (data_dir, vault) = scratch_dir_with_vault("locate");
        let key = vault.account_dir_key();
        let alice =
            AccountDir::locate(&data_dir, &key, "http://localhost:6167", "@alice:localhost")
                .unwrap();
        let again =
            AccountDir::locate(&data_dir, &key, "http://localhost:6167", "@alice:localhost")
                .unwrap();
        assert_eq!(alice, again);
        assert_eq!(alice.server_host, "localhost:6167");
        assert_eq!(alice.localpart, "alice");

        let path = alice.dir.display().to_string();
        assert!(
            !path.contains("alice") && !path.contains("localhost"),
            "路徑不該洩漏 server 或帳號：{path}"
        );
        assert!(alice.key().contains('/') && !alice.key().contains("alice"));
        let _ = std::fs::remove_dir_all(&data_dir);
    }

    #[test]
    fn list_and_current_round_trip() {
        let (data_dir, vault) = scratch_dir_with_vault("list");
        let key = vault.account_dir_key();
        assert!(list_accounts(&data_dir, &vault).unwrap().is_empty());
        assert_eq!(read_current(&data_dir).unwrap(), None);

        let alice =
            AccountDir::locate(&data_dir, &key, "http://localhost:6167", "@alice:localhost")
                .unwrap();
        std::fs::create_dir_all(&alice.dir).unwrap();
        // 不是真的 session.sealed（解不開），所以 logged_in 是 true 但 user_id 拿不到——
        // 這正是「解不開的 session 不擋掉整份清單」那條。
        std::fs::write(alice.session_path(), b"x").unwrap();
        let bob =
            AccountDir::locate(&data_dir, &key, "http://localhost:6167", "@bob:localhost").unwrap();
        std::fs::create_dir_all(&bob.dir).unwrap();
        write_current(&data_dir, &alice).unwrap();

        let listed = list_accounts(&data_dir, &vault).unwrap();
        assert_eq!(listed.len(), 2);
        assert!(listed[0].logged_in && listed[0].current && listed[0].localpart == "alice");
        assert_eq!(listed[0].user_id, None, "session 解不開就沒有權威 mxid");
        assert!(!listed[1].logged_in && !listed[1].current && listed[1].localpart == "bob");
        assert!(has_any_logged_in_account(&alice.server_dir()));

        let current = read_current(&data_dir).unwrap().unwrap();
        assert_eq!(
            find_account_of_current(&data_dir, &key, &current).unwrap(),
            alice
        );
        clear_current_if(&data_dir, &alice).unwrap();
        assert_eq!(read_current(&data_dir).unwrap(), None);
        let _ = std::fs::remove_dir_all(&data_dir);
    }

    #[test]
    fn another_key_sees_nothing_and_gets_a_hint() {
        let (data_dir, vault) = scratch_dir_with_vault("otherkey");
        let alice = AccountDir::locate(
            &data_dir,
            &vault.account_dir_key(),
            "http://localhost:6167",
            "@alice:localhost",
        )
        .unwrap();
        std::fs::create_dir_all(&alice.dir).unwrap();

        // 另一台機器的 local.key（同一份 data dir 被拷走的情境）。
        let (other_dir, other_vault) = scratch_dir_with_vault("otherkey-2");
        assert!(
            list_accounts(&data_dir, &other_vault).unwrap().is_empty(),
            "別把金鑰不該看到任何帳號"
        );
        assert!(
            find_undecryptable_layout_hint(&data_dir, &other_vault.account_dir_key()).is_some()
        );
        assert!(
            find_undecryptable_layout_hint(&data_dir, &vault.account_dir_key()).is_none(),
            "自己的金鑰解得開就不該印提示"
        );
        let _ = std::fs::remove_dir_all(&data_dir);
        let _ = std::fs::remove_dir_all(&other_dir);
    }

    #[test]
    fn find_account_needs_a_server_when_the_localpart_is_ambiguous() {
        let (data_dir, vault) = scratch_dir_with_vault("ambiguous");
        for server in ["http://localhost:6167", "https://matrix.example.org"] {
            let account = AccountDir::locate(
                &data_dir,
                &vault.account_dir_key(),
                server,
                "@alice:whatever",
            )
            .unwrap();
            std::fs::create_dir_all(&account.dir).unwrap();
        }
        let error = find_account(&data_dir, &vault, "alice", None).unwrap_err();
        assert!(format!("{error}").contains("2 servers"), "{error}");
        assert!(find_account(&data_dir, &vault, "alice", Some("http://localhost:6167")).is_ok());
        let _ = std::fs::remove_dir_all(&data_dir);
    }
}

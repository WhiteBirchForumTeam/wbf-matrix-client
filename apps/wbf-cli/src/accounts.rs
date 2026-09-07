//! 每個帳號的資料放哪（CLI 規格 §7；local-cache-db.md §5.6 的修訂版）：
//!
//! ```text
//! <data dir>/
//!   local.key、unlock.ticket            一台機器一把主金鑰（vault）
//!   current                             目前帳號：一行 "<server host>/<localpart>"
//!   servers/<server host>/
//!     cache.db                          這個 server 上所有帳號共用的快取
//!     accounts/<localpart>/
//!       session.sealed                  這個帳號的 session（第三把子金鑰封住）
//!       matrix/                         matrix-sdk store，綁 device；logout 刪
//! ```
//!
//! 目錄名只是定位（做過檔名安全化）；真正的 server URL 與 mxid 在 `session.sealed` 裡，不從目錄名反推。

use std::path::{Path, PathBuf};

use serde::Serialize;
use wbf_sdk::vault::{write_private, SEALED_SESSION_FILE_NAME};
use wbf_sdk::SdkError;

pub const SERVERS_DIR_NAME: &str = "servers";
pub const ACCOUNTS_DIR_NAME: &str = "accounts";
pub const MATRIX_STORE_DIR_NAME: &str = "matrix";
pub const CURRENT_FILE_NAME: &str = "current";

/// 一個帳號在磁碟上的位置。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccountDir {
    /// 檔名安全化過的 server host，example: "localhost_6167"
    pub server_key: String,
    /// 檔名安全化過的 localpart，example: "alice"
    pub localpart: String,
    /// `<data dir>/servers/<server_key>/accounts/<localpart>`
    pub dir: PathBuf,
}

impl AccountDir {
    /// Args:
    ///     data_dir: example: "<data dir>"
    ///     server: example: "http://localhost:6167"
    ///     user: mxid 或 localpart, example: "@alice:localhost"
    pub fn locate(data_dir: &Path, server: &str, user: &str) -> AccountDir {
        let server_key = server_key(server);
        let localpart = sanitize(localpart_of(user));
        let dir = server_dir(data_dir, &server_key)
            .join(ACCOUNTS_DIR_NAME)
            .join(&localpart);
        AccountDir {
            server_key,
            localpart,
            dir,
        }
    }

    pub fn session_path(&self) -> PathBuf {
        self.dir.join(SEALED_SESSION_FILE_NAME)
    }

    pub fn matrix_store_dir(&self) -> PathBuf {
        self.dir.join(MATRIX_STORE_DIR_NAME)
    }

    /// `servers/<server_key>/`：`cache.db` 在這一層，同 server 的帳號共用。
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

    /// `current` 檔的內容。
    pub fn key(&self) -> String {
        format!("{}/{}", self.server_key, self.localpart)
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

pub fn server_dir(data_dir: &Path, server_key: &str) -> PathBuf {
    data_dir.join(SERVERS_DIR_NAME).join(server_key)
}

/// `accounts` 命令印的一列。
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct AccountSummary {
    pub server: String,
    pub localpart: String,
    pub logged_in: bool,
    pub current: bool,
}

/// 掃 `servers/*/accounts/*`，不開 vault、不解 session（只看檔案在不在）。
pub fn list_accounts(data_dir: &Path) -> Result<Vec<AccountSummary>, SdkError> {
    let current = read_current(data_dir)?;
    let mut summaries = Vec::new();
    let servers = data_dir.join(SERVERS_DIR_NAME);
    let Ok(server_entries) = std::fs::read_dir(&servers) else {
        return Ok(summaries);
    };
    for server_entry in server_entries {
        let server_entry = server_entry?;
        let server_key = server_entry.file_name().to_string_lossy().into_owned();
        let Ok(account_entries) = std::fs::read_dir(server_entry.path().join(ACCOUNTS_DIR_NAME))
        else {
            continue;
        };
        for account_entry in account_entries {
            let account_entry = account_entry?;
            if !account_entry.file_type()?.is_dir() {
                continue;
            }
            let localpart = account_entry.file_name().to_string_lossy().into_owned();
            let key = format!("{server_key}/{localpart}");
            summaries.push(AccountSummary {
                logged_in: account_entry.path().join(SEALED_SESSION_FILE_NAME).exists(),
                current: current.as_deref() == Some(key.as_str()),
                server: server_key.clone(),
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
///     Ok(Some(String))   `current` 的內容，example: "localhost_6167/alice"
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

/// `current` 的內容 → 目錄。指到的目錄不存在就當沒有。
pub fn account_from_key(data_dir: &Path, key: &str) -> Option<AccountDir> {
    let (server_key, localpart) = key.split_once('/')?;
    let dir = server_dir(data_dir, server_key)
        .join(ACCOUNTS_DIR_NAME)
        .join(localpart);
    dir.is_dir().then(|| AccountDir {
        server_key: server_key.to_string(),
        localpart: localpart.to_string(),
        dir,
    })
}

/// `--account <mxid 或 localpart>` 解析：在所有 server 底下找同名 localpart。
///
/// Return:
///     Ok(AccountDir)   剛好一個、或 `server` 有給且找得到
///     Err(Usage)       零個；或多個 server 都有這個 localpart 而 `server` 沒給
pub fn find_account(
    data_dir: &Path,
    user: &str,
    server: Option<&str>,
) -> Result<AccountDir, SdkError> {
    let localpart = sanitize(localpart_of(user));
    if let Some(server) = server {
        let account = AccountDir::locate(data_dir, server, user);
        if account.dir.is_dir() {
            return Ok(account);
        }
        return Err(SdkError::Usage(format!(
            "no account {user} on {server} in {}; run `login` first",
            data_dir.display()
        )));
    }
    let matches: Vec<AccountSummary> = list_accounts(data_dir)?
        .into_iter()
        .filter(|summary| summary.localpart == localpart)
        .collect();
    match matches.as_slice() {
        [] => Err(SdkError::Usage(format!(
            "no account {user} in {}; run `login` first",
            data_dir.display()
        ))),
        [one] => Ok(AccountDir {
            server_key: one.server.clone(),
            localpart: one.localpart.clone(),
            dir: server_dir(data_dir, &one.server)
                .join(ACCOUNTS_DIR_NAME)
                .join(&one.localpart),
        }),
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

/// PR #11 的單一目錄佈局（頂層直接放 `session.sealed`／`matrix/`／`cache.db`）。看到就報錯叫人刪，不搬移：快取不是權威，session 重登就有。
pub fn reject_legacy_layout(data_dir: &Path) -> Result<(), SdkError> {
    let leftovers: Vec<&str> = [SEALED_SESSION_FILE_NAME, MATRIX_STORE_DIR_NAME, "cache.db"]
        .into_iter()
        .filter(|name| data_dir.join(name).exists())
        .collect();
    if leftovers.is_empty() {
        return Ok(());
    }
    Err(SdkError::Usage(format!(
        "{} holds files from the single-account layout ({}); delete them (local.key can stay) and run `login` again",
        data_dir.display(),
        leftovers.join(", ")
    )))
}

/// `http://localhost:6167` → `localhost_6167`；`https://matrix.example.org` → `matrix.example.org`（預設 port 不帶）。
pub fn server_key(server: &str) -> String {
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
    let key = match host_port.rsplit_once(':') {
        Some((host, port)) if port.chars().all(|c| c.is_ascii_digit()) => {
            let default = if is_https { "443" } else { "80" };
            if port == default {
                host.to_string()
            } else {
                format!("{host}_{port}")
            }
        }
        _ => host_port.to_string(),
    };
    sanitize(&key)
}

/// `@alice:localhost` → `alice`；`alice` → `alice`。
pub fn localpart_of(user: &str) -> &str {
    let stripped = user.strip_prefix('@').unwrap_or(user);
    stripped
        .split_once(':')
        .map(|(local, _)| local)
        .unwrap_or(stripped)
}

/// 只留 `[A-Za-z0-9._-]`，其他換 `_`；空的變 `_`。
fn sanitize(text: &str) -> String {
    let sanitized: String = text
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if sanitized.is_empty() || sanitized == "." || sanitized == ".." {
        "_".to_string()
    } else {
        sanitized
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_key_drops_scheme_and_default_port() {
        assert_eq!(server_key("http://localhost:6167"), "localhost_6167");
        assert_eq!(
            server_key("https://matrix.example.org"),
            "matrix.example.org"
        );
        assert_eq!(
            server_key("https://matrix.example.org:443/"),
            "matrix.example.org"
        );
        assert_eq!(server_key("http://example.org:80"), "example.org");
        assert_eq!(server_key("http://10.0.0.1:8008/path"), "10.0.0.1_8008");
    }

    #[test]
    fn localpart_and_sanitize() {
        assert_eq!(localpart_of("@alice:localhost"), "alice");
        assert_eq!(localpart_of("alice"), "alice");
        assert_eq!(sanitize("a/b:c d"), "a_b_c_d");
        assert_eq!(sanitize(".."), "_");
        assert_eq!(sanitize(""), "_");
    }

    #[test]
    fn locate_current_and_list_roundtrip() {
        let data_dir = std::env::temp_dir().join(format!("wbf-accounts-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&data_dir);
        assert!(list_accounts(&data_dir).unwrap().is_empty());
        assert_eq!(read_current(&data_dir).unwrap(), None);

        let alice = AccountDir::locate(&data_dir, "http://localhost:6167", "@alice:localhost");
        assert_eq!(alice.key(), "localhost_6167/alice");
        assert_eq!(
            alice.server_dir(),
            data_dir.join("servers").join("localhost_6167")
        );
        std::fs::create_dir_all(&alice.dir).unwrap();
        std::fs::write(alice.session_path(), b"x").unwrap();
        let bob = AccountDir::locate(&data_dir, "http://localhost:6167", "bob");
        std::fs::create_dir_all(&bob.dir).unwrap();
        write_current(&data_dir, &alice).unwrap();

        let listed = list_accounts(&data_dir).unwrap();
        assert_eq!(listed.len(), 2);
        assert!(listed[0].logged_in && listed[0].current && listed[0].localpart == "alice");
        assert!(!listed[1].logged_in && !listed[1].current && listed[1].localpart == "bob");
        assert!(has_any_logged_in_account(&alice.server_dir()));

        assert_eq!(
            account_from_key(&data_dir, &read_current(&data_dir).unwrap().unwrap()),
            Some(alice.clone())
        );
        assert_eq!(find_account(&data_dir, "bob", None).unwrap(), bob);
        assert!(find_account(&data_dir, "carol", None).is_err());
        assert!(find_account(&data_dir, "bob", Some("http://other:1")).is_err());

        // 同 localpart 兩個 server：要 --server。
        let bob2 = AccountDir::locate(&data_dir, "http://other:1", "bob");
        std::fs::create_dir_all(&bob2.dir).unwrap();
        assert!(find_account(&data_dir, "bob", None).is_err());
        assert_eq!(
            find_account(&data_dir, "bob", Some("http://other:1")).unwrap(),
            bob2
        );

        clear_current_if(&data_dir, &bob).unwrap();
        assert!(read_current(&data_dir).unwrap().is_some());
        clear_current_if(&data_dir, &alice).unwrap();
        assert_eq!(read_current(&data_dir).unwrap(), None);
        std::fs::remove_file(alice.session_path()).unwrap();
        assert!(!has_any_logged_in_account(&alice.server_dir()));

        assert!(reject_legacy_layout(&data_dir).is_ok());
        std::fs::write(data_dir.join("session.sealed"), b"old").unwrap();
        assert!(reject_legacy_layout(&data_dir).is_err());
        let _ = std::fs::remove_dir_all(&data_dir);
    }
}

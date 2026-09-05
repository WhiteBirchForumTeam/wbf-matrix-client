//! Session 檔（CLI 規格 §7）與含金鑰的檔案（manifest、狀態檔）怎麼寫：權限只給自己。

use std::path::{Path, PathBuf};

use wbf_sdk::{SdkError, Session};

/// CLI 規格 §7 的預設位置。
///
/// Return:
///     Ok(PathBuf)      Windows `%APPDATA%\wbf-cli\session.json`；macOS `~/Library/Application Support/wbf-cli/session.json`；
///                      其他 `$XDG_CONFIG_HOME/wbf-cli/session.json`，沒設就 `~/.config/wbf-cli/session.json`
///     Err(Usage)       找不到家目錄
pub fn default_session_path() -> Result<PathBuf, SdkError> {
    let base = if cfg!(windows) {
        std::env::var_os("APPDATA").map(PathBuf::from)
    } else if cfg!(target_os = "macos") {
        std::env::var_os("HOME").map(|home| PathBuf::from(home).join("Library/Application Support"))
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
    };
    let base = base
        .ok_or_else(|| SdkError::Usage("cannot find a config directory; pass --session".into()))?;
    Ok(base.join("wbf-cli").join("session.json"))
}

pub fn read_session(path: &Path) -> Result<Session, SdkError> {
    let bytes = std::fs::read(path).map_err(|error| {
        SdkError::Usage(format!(
            "no session at {}: {error}; run `login` first",
            path.display()
        ))
    })?;
    serde_json::from_slice(&bytes).map_err(|error| {
        SdkError::Usage(format!(
            "session file {} is broken: {error}",
            path.display()
        ))
    })
}

pub fn write_session(path: &Path, session: &Session) -> Result<(), SdkError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    write_private(
        path,
        &serde_json::to_vec_pretty(session).expect("Session serializes"),
    )
}

pub fn delete_session(path: &Path) -> Result<(), SdkError> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

/// 含金鑰或 token 的檔：Unix 0600 建立；Windows 靠使用者目錄的 ACL（CLI 規格 §5）。
pub fn write_private(path: &Path, bytes: &[u8]) -> Result<(), SdkError> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    // mode() 只在建立新檔時生效；覆寫既有檔（舊版留下的 0644）權限不會變，這裡無條件再設一次。
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    file.write_all(bytes)?;
    Ok(())
}

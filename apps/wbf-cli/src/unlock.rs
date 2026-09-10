//! CLI 怎麼打開 vault（local-cache-db.md §4 的 CLI 那一列）：資料目錄在哪、passphrase 從哪來、unlock ticket。
//!
//! 用字：**passphrase** 是解 `local.key` 的那句話；**password** 一律指 Matrix 帳號密碼（只有 `login` 用）。
//! passphrase 來源的優先順序：`--passphrase-file` → 有效的 `unlock.ticket` → `local.key` 是 `Plain` 就不用 passphrase → 問終端。
//! ticket 仿 `sudo`：passphrase 解鎖成功後把主金鑰加 `expires_at` 寫到 `<data dir>/unlock.ticket`（0600），
//! 期內的命令不再問；`lock` 刪掉它。⚠️ 那 15 分鐘的安全性等於 `Plain` 模式，維護者明說接受（CLI 不是產品面）。

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine;
use serde::{Deserialize, Serialize};
use wbf_sdk::vault::write_private;
use wbf_sdk::{Key32, KeyMode, SdkError, Unlock, Vault};
use zeroize::Zeroizing;

pub const TICKET_FILE_NAME: &str = "unlock.ticket";
/// 舊版（PR #9 之前）的明文 session 檔；看到它只提示，不讀。
const LEGACY_SESSION_FILE_NAME: &str = "session.json";

/// CLI 規格 §7 的預設資料目錄。
///
/// Return:
///     Ok(PathBuf)      Windows `%APPDATA%\wbf-cli`；macOS `~/Library/Application Support/wbf-cli`；
///                      其他 `$XDG_DATA_HOME/wbf-cli`，沒設就 `~/.local/share/wbf-cli`
///     Err(Usage)       找不到家目錄
pub fn default_data_dir() -> Result<PathBuf, SdkError> {
    let base = if cfg!(windows) {
        std::env::var_os("APPDATA").map(PathBuf::from)
    } else if cfg!(target_os = "macos") {
        std::env::var_os("HOME").map(|home| PathBuf::from(home).join("Library/Application Support"))
    } else {
        std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share"))
            })
    };
    let base = base
        .ok_or_else(|| SdkError::Usage("cannot find a data directory; pass --data-dir".into()))?;
    Ok(base.join("wbf-cli"))
}

/// 全域參數裡跟解鎖有關的部分，`Context` 抄一份。
pub struct UnlockOptions {
    pub data_dir: PathBuf,
    pub passphrase_file: Option<PathBuf>,
    /// 0 就不寫 ticket。
    pub unlock_ttl: Duration,
    pub quiet: bool,
}

impl UnlockOptions {
    fn ticket_path(&self) -> PathBuf {
        self.data_dir.join(TICKET_FILE_NAME)
    }

    /// 開既有的 vault（沒有就 `Err`：只有 `login` 會建）。
    ///
    /// Return:
    ///     Ok(Vault)
    ///     Err(Usage)   沒有 local.key、passphrase 錯、passphrase 檔讀不到、終端不能問
    pub fn open_vault(&self) -> Result<Vault, SdkError> {
        if !self.data_dir.join(wbf_sdk::vault::KEY_FILE_NAME).exists() {
            let legacy = self.data_dir.join(LEGACY_SESSION_FILE_NAME);
            let hint = if legacy.exists() {
                format!(
                    "; {} is an old plaintext session — delete it and the m/ directory next to it",
                    legacy.display()
                )
            } else {
                String::new()
            };
            return Err(SdkError::Usage(format!(
                "no key file in {}; run `login` first{hint}",
                self.data_dir.display()
            )));
        }
        if Vault::read_mode(&self.data_dir)? == KeyMode::Plain {
            // Plain 模式不看 ticket：給了 passphrase 檔就讓 Vault::open 用「配不上」拒絕，不靜默忽略。
            let unlock = match &self.passphrase_file {
                Some(path) => Unlock::Passphrase(read_password_file(path)?),
                None => Unlock::NoPassphrase,
            };
            return Vault::open(&self.data_dir, &unlock);
        }
        if let Some(path) = &self.passphrase_file {
            let passphrase = read_password_file(path)?;
            let vault = Vault::open(&self.data_dir, &Unlock::Passphrase(passphrase))?;
            self.write_ticket(&vault)?;
            return Ok(vault);
        }
        if let Some(master) = self.read_valid_ticket()? {
            return Ok(Vault::from_master(
                &self.data_dir,
                master,
                KeyMode::Passphrase,
            ));
        }
        let passphrase = prompt_password_on_terminal("passphrase: ")?;
        let vault = Vault::open(&self.data_dir, &Unlock::Passphrase(passphrase))?;
        self.write_ticket(&vault)?;
        Ok(vault)
    }

    /// `login` 用：有 `local.key` 就照 `open_vault` 開；沒有就建一把，
    /// 給了 `--passphrase-file` 就直接是 `Passphrase` 模式，否則 `Plain`。
    pub fn open_or_create_vault(&self) -> Result<Vault, SdkError> {
        if self.data_dir.join(wbf_sdk::vault::KEY_FILE_NAME).exists() {
            return self.open_vault();
        }
        let unlock = match &self.passphrase_file {
            Some(path) => Unlock::Passphrase(read_password_file(path)?),
            None => Unlock::NoPassphrase,
        };
        let vault = Vault::create(&self.data_dir, &unlock)?;
        if vault.mode() == KeyMode::Passphrase {
            self.write_ticket(&vault)?;
        }
        self.progress(format!(
            "created {} ({} mode)",
            vault.dir().join(wbf_sdk::vault::KEY_FILE_NAME).display(),
            match vault.mode() {
                KeyMode::Plain => "plain",
                KeyMode::Passphrase => "passphrase",
            }
        ));
        Ok(vault)
    }

    pub fn delete_ticket(&self) -> Result<bool, SdkError> {
        match std::fs::remove_file(self.ticket_path()) {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error.into()),
        }
    }

    fn write_ticket(&self, vault: &Vault) -> Result<(), SdkError> {
        if self.unlock_ttl.is_zero() {
            return Ok(());
        }
        let ticket = Ticket {
            v: 1,
            master: base64::engine::general_purpose::STANDARD.encode(vault.master_key().as_bytes()),
            expires_at: now_unix() + self.unlock_ttl.as_secs(),
        };
        write_private(
            &self.ticket_path(),
            &serde_json::to_vec(&ticket).expect("Ticket serializes"),
        )?;
        self.progress(format!(
            "unlocked for {} s (ticket at {}; `lock` removes it)",
            self.unlock_ttl.as_secs(),
            self.ticket_path().display()
        ));
        Ok(())
    }

    /// Return:
    ///     Ok(Some(Key32))  有 ticket、沒過期、權限對
    ///     Ok(None)         沒有 ticket；過期或壞掉的 ticket 順手刪掉也算 None
    ///     Err(Io)          刪不掉
    fn read_valid_ticket(&self) -> Result<Option<Key32>, SdkError> {
        let path = self.ticket_path();
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        if !is_private_mode(&path)? {
            self.progress(format!(
                "ignoring {}: it is readable by others; run `lock`",
                path.display()
            ));
            return Ok(None);
        }
        let ticket: Option<Ticket> = serde_json::from_slice(&bytes).ok();
        let master = ticket
            .filter(|ticket| ticket.v == 1 && ticket.expires_at > now_unix())
            .and_then(|ticket| {
                base64::engine::general_purpose::STANDARD
                    .decode(&ticket.master)
                    .ok()
            })
            .and_then(|bytes| <[u8; 32]>::try_from(bytes.as_slice()).ok())
            .map(Key32);
        if master.is_none() {
            // 過期或壞掉：不認、也不留著。
            let _ = std::fs::remove_file(&path);
        }
        Ok(master)
    }

    fn progress(&self, line: String) {
        if !self.quiet {
            eprintln!("{line}");
        }
    }
}

#[derive(Serialize, Deserialize)]
struct Ticket {
    v: u32,
    master: String,
    expires_at: u64,
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

/// Unix 上 ticket 的模式必須是 0600（group／other 沒有任何位元）；Windows 靠目錄 ACL，一律算對。
#[cfg(unix)]
fn is_private_mode(path: &Path) -> Result<bool, SdkError> {
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(path)?.permissions().mode();
    Ok(mode & 0o077 == 0)
}

#[cfg(not(unix))]
fn is_private_mode(_path: &Path) -> Result<bool, SdkError> {
    Ok(true)
}

/// `--password-file`／`--passphrase-file` 的規則（CLI 規格 §3.1）：整檔就是那句話，去掉結尾一個換行。
pub fn read_password_file(path: &Path) -> Result<Zeroizing<String>, SdkError> {
    let text = Zeroizing::new(std::fs::read_to_string(path)?);
    let trimmed = text
        .strip_suffix('\n')
        .map(|stripped| stripped.strip_suffix('\r').unwrap_or(stripped))
        .unwrap_or(&text);
    Ok(Zeroizing::new(trimmed.to_string()))
}

/// 從終端不回顯地讀 password 或 passphrase。stdin 不是終端（腳本、管線、`</dev/null`）就直接拒絕：
/// rpassword 在 Windows 會繞過 stdin 直接開 console 等人打字，被導向時整個命令會掛在那裡（2026-09-06 實跑踩到）。
///
/// Args:
///     label: example: "passphrase: "
/// Return:
///     Ok(Zeroizing<String>)
///     Err(Usage)   stdin 不是終端
pub fn prompt_password_on_terminal(label: &str) -> Result<Zeroizing<String>, SdkError> {
    use std::io::IsTerminal;
    if !std::io::stdin().is_terminal() {
        return Err(SdkError::Usage(format!(
            "{} needed but stdin is not a terminal; pass it with a --password-file / --passphrase-file",
            label.trim_end_matches(": ")
        )));
    }
    Ok(Zeroizing::new(rpassword::prompt_password(label)?))
}

/// 問兩次、要一樣（設新的 passphrase 用）。
pub fn prompt_new_passphrase() -> Result<Zeroizing<String>, SdkError> {
    let first = prompt_password_on_terminal("new passphrase: ")?;
    let second = prompt_password_on_terminal("again: ")?;
    if *first != *second {
        return Err(SdkError::Usage("the two passwords differ".into()));
    }
    Ok(first)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_options(name: &str, ttl: u64) -> UnlockOptions {
        let dir = std::env::temp_dir().join(format!("wbf-unlock-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        UnlockOptions {
            data_dir: dir,
            passphrase_file: None,
            unlock_ttl: Duration::from_secs(ttl),
            quiet: true,
        }
    }

    fn password_file(dir: &Path, text: &str) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let path = dir.join("pw.txt");
        std::fs::write(&path, text).unwrap();
        path
    }

    #[test]
    fn passphrase_file_unlock_writes_a_ticket_and_ticket_alone_opens() {
        let mut options = scratch_options("ticket", 60);
        let pw = password_file(&options.data_dir, "hunter2\n");
        options.passphrase_file = Some(pw);
        let created = options.open_or_create_vault().unwrap();
        assert_eq!(created.mode(), KeyMode::Passphrase);
        assert!(options.data_dir.join(TICKET_FILE_NAME).exists());
        options.passphrase_file = None;
        let via_ticket = options.open_vault().unwrap();
        assert_eq!(
            created.master_key().as_bytes(),
            via_ticket.master_key().as_bytes()
        );
        assert!(options.delete_ticket().unwrap());
        assert!(!options.delete_ticket().unwrap());
        let _ = std::fs::remove_dir_all(&options.data_dir);
    }

    #[test]
    fn expired_ticket_is_removed_and_not_used() {
        let mut options = scratch_options("expired", 60);
        let pw = password_file(&options.data_dir, "hunter2");
        options.passphrase_file = Some(pw);
        options.open_or_create_vault().unwrap();
        let path = options.data_dir.join(TICKET_FILE_NAME);
        let mut ticket: Ticket = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        ticket.expires_at = now_unix() - 1;
        std::fs::write(&path, serde_json::to_vec(&ticket).unwrap()).unwrap();
        assert!(options.read_valid_ticket().unwrap().is_none());
        assert!(!path.exists());
        let _ = std::fs::remove_dir_all(&options.data_dir);
    }

    #[test]
    fn ttl_zero_writes_no_ticket_and_plain_mode_never_does() {
        let mut options = scratch_options("nottl", 0);
        let pw = password_file(&options.data_dir, "hunter2");
        options.passphrase_file = Some(pw);
        options.open_or_create_vault().unwrap();
        assert!(!options.data_dir.join(TICKET_FILE_NAME).exists());
        let _ = std::fs::remove_dir_all(&options.data_dir);

        let plain = scratch_options("plain", 60);
        let vault = plain.open_or_create_vault().unwrap();
        assert_eq!(vault.mode(), KeyMode::Plain);
        assert!(!plain.data_dir.join(TICKET_FILE_NAME).exists());
        assert!(plain.open_vault().is_ok());
        // Plain 模式給了 passphrase 檔：拒絕，不是靜默忽略。
        let mut with_pw = plain;
        with_pw.passphrase_file = Some(password_file(&with_pw.data_dir, "x"));
        assert!(with_pw.open_vault().is_err());
        let _ = std::fs::remove_dir_all(&with_pw.data_dir);
    }

    #[test]
    fn password_file_strips_one_trailing_newline_only() {
        let dir = std::env::temp_dir().join(format!("wbf-unlock-pwfile-{}", std::process::id()));
        let path = password_file(&dir, "abc\r\n");
        assert_eq!(&*read_password_file(&path).unwrap(), "abc");
        std::fs::write(&path, "abc\n\n").unwrap();
        assert_eq!(&*read_password_file(&path).unwrap(), "abc\n");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

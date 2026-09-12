//! CLI 怎麼打開 vault（local-cache-db.md §4 的 CLI 那一列）：資料目錄在哪、passphrase 從哪來、unlock ticket。
//!
//! 用字：**passphrase** 是解 `local.key` 的那句話；**password** 一律指 Matrix 帳號密碼（只有 `login` 用）。
//! passphrase 來源的優先順序：`--passphrase-file` → 有效的 `unlock.ticket` → `local.key` 是 `Plain` 就不用 passphrase → 問終端。
//! ⚠️ 這整個模組是「**一個命令一個程序**」的產物：ticket、問終端、讀 passphrase 檔，
//! 都是為了「每次執行都要重新解鎖」而存在。daemon 常駐之後（architecture-v2 §1、§4.5）
//! **ticket 整條消失**，passphrase 改從 RPC 進來——所以 🚫 這些都沒有搬進 `wbf-core`。
//! 這裡的責任是「把 passphrase 生出來」，解鎖本身交給 `Core`。
//!
//! ticket 仿 `sudo`：passphrase 解鎖成功後把主金鑰加 `expires_at` 寫到 `<data dir>/unlock.ticket`（0600），
//! 期內的命令不再問；`lock` 刪掉它。⚠️ 那 15 分鐘的安全性等於 `Plain` 模式，維護者明說接受（CLI 不是產品面）。

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use wbf_core::{Core, CoreError, CoreErrorKind};

use base64::Engine;
use serde::{Deserialize, Serialize};
use wbf_sdk::vault::write_private;
use wbf_sdk::{Key32, KeyMode};
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
pub fn default_data_dir() -> Result<PathBuf, CoreError> {
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
    let base = base.ok_or_else(|| {
        CoreError::new(
            CoreErrorKind::Usage,
            format!("{}", "cannot find a data directory; pass --data-dir"),
        )
    })?;
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

    /// 讓 `core` 解鎖。**rpc-cli 這一側的責任就是「把 passphrase 生出來」**：
    /// `--passphrase-file` → 有效的 ticket → 問終端。解鎖本身在 `Core`。
    ///
    /// 📎 daemon 沒有這整條：passphrase 從 RPC 的 `vault.unlock` 進來（§4.5），
    /// 而 ticket **整條消失**（§1）。
    ///
    /// Return:
    ///     Ok(())       解開了，或本來就開著
    ///     Err(...)     沒有 local.key、passphrase 錯、passphrase 檔讀不到、終端不能問
    pub fn unlock(&self, core: &Core) -> Result<(), CoreError> {
        if core.is_unlocked() {
            return Ok(());
        }
        let Some(mode) = core.key_mode()? else {
            let legacy = self.data_dir.join(LEGACY_SESSION_FILE_NAME);
            let hint = if legacy.exists() {
                format!(
                    "; {} is an old plaintext session — delete it and the m/ directory next to it",
                    legacy.display()
                )
            } else {
                String::new()
            };
            return Err(CoreError::new(
                CoreErrorKind::NoKeyFile,
                format!(
                    "no key file in {}; run `login` first{hint}",
                    self.data_dir.display()
                ),
            ));
        };
        if mode == KeyMode::Plain {
            // ⚠️ Plain 模式**不看 ticket**。給了 passphrase 檔就讓 core 用
            // `UnexpectedPassphrase` 拒絕，🚫 不靜默忽略。
            return match &self.passphrase_file {
                Some(path) => core.unlock(Some(&read_passphrase_file(path)?)),
                None => core.unlock(None),
            };
        }
        if let Some(path) = &self.passphrase_file {
            core.unlock(Some(&read_passphrase_file(path)?))?;
            return self.write_ticket_for(core);
        }
        if let Some(master) = self.read_valid_ticket()? {
            return core.unlock_with_master_key(master.0, KeyMode::Passphrase);
        }
        core.unlock(Some(&prompt_passphrase_on_terminal("passphrase: ")?))?;
        self.write_ticket_for(core)
    }

    /// 把主金鑰寫進 ticket（仿 `sudo`）。⚠️ **明文主金鑰落地**——
    /// local-cache-db §4 自己標記這是妥協，接受它只因為「一個命令一個程序」。
    /// daemon 接手之後這整件事消失。
    pub fn write_ticket_for(&self, core: &Core) -> Result<(), CoreError> {
        if self.unlock_ttl.is_zero() {
            return Ok(());
        }
        let (master, mode) = core.export_master_key_for_ticket()?;
        // ⚠️ plain 模式**永遠不寫**：ticket 省的是「再問一次 passphrase」，而 plain 模式
        // 根本不問。寫了只是把主金鑰多複製一份到磁碟上，換不到任何東西。
        // 🚫 這條檢查放在這裡而不是靠呼叫端記得——它是這個函式自己的不變量。
        if mode == KeyMode::Plain {
            return Ok(());
        }
        let ticket = Ticket {
            v: 1,
            master: base64::engine::general_purpose::STANDARD.encode(master),
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
    /// 拿掉 ticket（`lock`，或換過 passphrase 之後）。
    ///
    /// Return:
    ///     Ok(true)    本來有
    ///     Ok(false)   本來就沒有
    pub fn delete_ticket(&self) -> Result<bool, CoreError> {
        match std::fs::remove_file(self.ticket_path()) {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error.into()),
        }
    }

    fn read_valid_ticket(&self) -> Result<Option<Key32>, CoreError> {
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
fn is_private_mode(path: &Path) -> Result<bool, CoreError> {
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(path)?.permissions().mode();
    Ok(mode & 0o077 == 0)
}

#[cfg(not(unix))]
fn is_private_mode(_path: &Path) -> Result<bool, CoreError> {
    Ok(true)
}

/// `--passphrase-file` 的規則（local-cache-db.md §12）：**整檔原始 bytes**。
///
/// 🚫 不去尾換行、🚫 不驗 UTF-8、🚫 不 trim：passphrase 只餵給本機的 Argon2id，永遠不出這台
/// 機器，所以它可以是中文、可以是一個 mp3。⚠️ 這代表 `echo hunter2 > pw`（結尾有 `\n`）跟
/// `printf hunter2 > pw` 是**兩個不同的 passphrase**——檔案就是檔案，🚫 我們不替使用者猜
/// 哪個 byte 不算數。
///
/// 🚫 `--password-file` 不走這個（§12.4）：那句話要送給 homeserver，Matrix 規定它是 JSON
/// 字串，塞不進任意 bytes。兩者長得像，但一個是本機的鑰匙、一個是要上線的憑證。
///
/// Args:
///     path: example: "/tmp/pw"
/// Return:
///     Ok(Zeroizing<Vec<u8>>)   整檔，一個 byte 都不動
///     Err(Io)                  讀不到
pub fn read_passphrase_file(path: &Path) -> Result<Zeroizing<Vec<u8>>, CoreError> {
    Ok(Zeroizing::new(std::fs::read(path)?))
}

/// `--password-file` 的規則（CLI 規格 §3.1）：整檔就是那句話，去掉結尾一個換行。
pub fn read_password_file(path: &Path) -> Result<Zeroizing<String>, CoreError> {
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
pub fn prompt_password_on_terminal(label: &str) -> Result<Zeroizing<String>, CoreError> {
    use std::io::IsTerminal;
    if !std::io::stdin().is_terminal() {
        return Err(CoreError::new(CoreErrorKind::Usage, format!(
            "{} needed but stdin is not a terminal; pass it with a --password-file / --passphrase-file",
            label.trim_end_matches(": ")
        )));
    }
    Ok(Zeroizing::new(rpassword::prompt_password(label)?))
}

/// 從終端讀 passphrase：那一行的 UTF-8 bytes，不含結尾換行。
///
/// ⚠️ 終端只打得出字，所以這是 §12 那個「任意 bytes」的天然子集——同一句話從終端打
/// 與用 `printf` 寫進檔案是**同一個** passphrase，用 `echo` 寫的（多一個 `\n`）不是。
pub fn prompt_passphrase_on_terminal(label: &str) -> Result<Zeroizing<Vec<u8>>, CoreError> {
    Ok(Zeroizing::new(
        prompt_password_on_terminal(label)?.as_bytes().to_vec(),
    ))
}

/// 問兩次、要一樣（設新的 passphrase 用）。
pub fn prompt_new_passphrase() -> Result<Zeroizing<Vec<u8>>, CoreError> {
    let first = prompt_passphrase_on_terminal("new passphrase: ")?;
    let second = prompt_passphrase_on_terminal("again: ")?;
    if *first != *second {
        return Err(CoreError::new(
            CoreErrorKind::Usage,
            format!("{}", "the two passphrases differ"),
        ));
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

    /// 這一側的責任是「把 passphrase 生出來」，所以測的是**來源的優先序**與 ticket，
    /// 🚫 不是解鎖本身（那在 `wbf-core` 的測試裡）。
    /// 中文加一個 0x00：兩者以前都過不了 `read_to_string`（local-cache-db §12）。
    const BINARY_PASSPHRASE: &[u8] = "早安\u{0}世界".as_bytes();

    fn new_core(options: &UnlockOptions) -> Core {
        Core::open(&options.data_dir)
    }

    #[test]
    fn a_passphrase_file_unlocks_and_leaves_a_ticket_that_alone_opens_next_time() {
        let mut options = scratch_options("ticket", 60);
        let pw = password_file(&options.data_dir, "hunter2\n");
        options.passphrase_file = Some(pw);
        let core = new_core(&options);
        core.create_vault(Some(b"hunter2\n")).unwrap();
        options.write_ticket_for(&core).unwrap();
        assert!(options.data_dir.join(TICKET_FILE_NAME).exists());

        // 下一個命令：沒有 `--passphrase-file`，靠 ticket 開。
        options.passphrase_file = None;
        let next = new_core(&options);
        options.unlock(&next).unwrap();
        assert!(next.is_unlocked());

        assert!(options.delete_ticket().unwrap());
        assert!(!options.delete_ticket().unwrap());
        // ticket 沒了、又沒有 passphrase 檔：這時只剩問終端，而測試裡沒有終端。
        let without = new_core(&options);
        assert!(options.unlock(&without).is_err());
        let _ = std::fs::remove_dir_all(&options.data_dir);
    }

    #[test]
    fn expired_ticket_is_removed_and_not_used() {
        let mut options = scratch_options("expired", 60);
        let pw = password_file(&options.data_dir, "hunter2");
        options.passphrase_file = Some(pw);
        let core = new_core(&options);
        core.create_vault(Some(b"hunter2")).unwrap();
        options.write_ticket_for(&core).unwrap();

        let path = options.data_dir.join(TICKET_FILE_NAME);
        let mut ticket: Ticket = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        ticket.expires_at = now_unix() - 1;
        std::fs::write(&path, serde_json::to_vec(&ticket).unwrap()).unwrap();
        // 過期的不但不用，還要**刪掉**：留著只是一份過期的明文主金鑰。
        assert!(options.read_valid_ticket().unwrap().is_none());
        assert!(!path.exists());
        let _ = std::fs::remove_dir_all(&options.data_dir);
    }

    #[test]
    fn ttl_zero_writes_no_ticket_and_plain_mode_never_does() {
        let mut options = scratch_options("nottl", 0);
        let pw = password_file(&options.data_dir, "hunter2");
        options.passphrase_file = Some(pw);
        let core = new_core(&options);
        core.create_vault(Some(b"hunter2")).unwrap();
        options.write_ticket_for(&core).unwrap();
        assert!(
            !options.data_dir.join(TICKET_FILE_NAME).exists(),
            "ttl 0 不寫"
        );
        let _ = std::fs::remove_dir_all(&options.data_dir);

        let plain = scratch_options("plain", 60);
        let core = new_core(&plain);
        core.create_vault(None).unwrap();
        // ⚠️ plain 模式**永遠不寫 ticket**：沒有 passphrase 要省，寫它只是把主金鑰落地。
        plain.write_ticket_for(&core).unwrap();
        assert!(!plain.data_dir.join(TICKET_FILE_NAME).exists());

        // plain 模式給了 passphrase 檔：**拒絕**，🚫 不是靜默忽略。
        let mut with_pw = plain;
        with_pw.passphrase_file = Some(password_file(&with_pw.data_dir, "x"));
        let fresh = new_core(&with_pw);
        assert_eq!(
            with_pw.unlock(&fresh).unwrap_err().kind,
            CoreErrorKind::UnexpectedPassphrase
        );
        let _ = std::fs::remove_dir_all(&with_pw.data_dir);
    }

    #[test]
    fn a_passphrase_file_is_taken_byte_for_byte() {
        // §12：檔案就是檔案。`echo` 寫的（結尾 \n）與 `printf` 寫的是兩個不同的 passphrase，
        // 🚫 不替使用者猜哪個 byte 不算數——猜錯的那天是 local.key 打不開。
        let dir = std::env::temp_dir().join(format!("wbf-pp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let with_newline = dir.join("echo");
        std::fs::write(&with_newline, b"hunter2\n").unwrap();
        assert_eq!(
            &**read_passphrase_file(&with_newline).unwrap(),
            b"hunter2\n"
        );

        let without = dir.join("printf");
        std::fs::write(&without, b"hunter2").unwrap();
        assert_eq!(&**read_passphrase_file(&without).unwrap(), b"hunter2");

        // 不是合法 UTF-8 也照讀（維護者：可以是一個 mp3）。
        let binary = dir.join("mp3");
        std::fs::write(&binary, [0xffu8, 0xfe, 0x00, 0x80]).unwrap();
        assert_eq!(
            &**read_passphrase_file(&binary).unwrap(),
            &[0xffu8, 0xfe, 0x00, 0x80]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_binary_passphrase_opens_the_vault_it_created() {
        let mut options = scratch_options("binary-pp", 0);
        let path = options.data_dir.join("pp");
        std::fs::create_dir_all(&options.data_dir).unwrap();
        // 中文加一個 0x00：兩者以前都過不了 `read_to_string`。
        std::fs::write(&path, BINARY_PASSPHRASE).unwrap();
        options.passphrase_file = Some(path.clone());
        let core = new_core(&options);
        assert_eq!(
            core.create_vault(Some(BINARY_PASSPHRASE)).unwrap(),
            KeyMode::Passphrase
        );
        // 同一個檔再開一次：這一側把 bytes 原樣讀出來餵給 core。
        assert!(options.unlock(&new_core(&options)).is_ok());

        // 少一個 byte 就是另一句話。
        let mut longer = BINARY_PASSPHRASE.to_vec();
        longer.push(b'\n');
        std::fs::write(&path, &longer).unwrap();
        assert_eq!(
            options.unlock(&new_core(&options)).unwrap_err().kind,
            CoreErrorKind::WrongPassphrase,
            "多一個換行不該還開得起來"
        );
        let _ = std::fs::remove_dir_all(&options.data_dir);
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

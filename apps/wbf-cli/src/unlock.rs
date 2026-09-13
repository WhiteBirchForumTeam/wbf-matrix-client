//! CLI 怎麼打開 vault（local-cache-db.md §4 的 CLI 那一列）：資料目錄在哪、passphrase 從哪來。
//!
//! 用字：**passphrase** 是解 `local.key` 的那句話；**password** 一律指 Matrix 帳號密碼（只有 `login` 用）。
//! passphrase 來源的優先順序：`--passphrase-file` → `local.key` 是 `Plain` 就不用 passphrase → 問終端。
//! ⚠️ 這整個模組是「**一個命令一個程序**」的產物：問終端、讀 passphrase 檔，都是為了
//! 「每次執行都要重新解鎖」而存在。daemon 常駐之後（architecture-v2 §1、§4.5）passphrase
//! 改從 RPC 的 `vault.unlock` 進來——所以 🚫 這些都沒有搬進 `wbf-core`。
//! 這裡的責任是「把 passphrase 生出來」，解鎖本身交給 `Core`。
//!
//! 🚫 **沒有 `unlock.ticket`**（維護者 2026-09-13 拿掉）：那張票是仿 `sudo`、為了「每個命令都要
//! 重新解鎖」而把**明文主金鑰落地** 15 分鐘的妥協。daemon 之後單發命令只剩 debug／test 的用途
//! （常駐時碰資料庫的命令一律跳錯，architecture-v2 §0.2），省那幾次打字換不到落地一份主金鑰。

use std::path::{Path, PathBuf};
use wbf_core::{Core, CoreError, CoreErrorKind};

use wbf_sdk::KeyMode;
use zeroize::Zeroizing;

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
            "cannot find a data directory; pass --data-dir",
        )
    })?;
    Ok(base.join("wbf-cli"))
}

/// 全域參數裡跟解鎖有關的部分，`Context` 抄一份。
pub struct UnlockOptions {
    pub data_dir: PathBuf,
    pub passphrase_file: Option<PathBuf>,
}

impl UnlockOptions {
    /// 讓 `core` 解鎖。**rpc-cli 這一側的責任就是「把 passphrase 生出來」**：
    /// `--passphrase-file` → 問終端。解鎖本身在 `Core`。
    ///
    /// 📎 daemon 沒有這整條：passphrase 從 RPC 的 `vault.unlock` 進來（§4.5）。
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
            // ⚠️ 給了 passphrase 檔就讓 core 用 `UnexpectedPassphrase` 拒絕，🚫 不靜默忽略。
            return match &self.passphrase_file {
                Some(path) => core.unlock(Some(&read_passphrase_file(path)?)),
                None => core.unlock(None),
            };
        }
        match &self.passphrase_file {
            Some(path) => core.unlock(Some(&read_passphrase_file(path)?)),
            None => core.unlock(Some(&prompt_passphrase_on_terminal("passphrase: ")?)),
        }
    }
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
            "the two passphrases differ",
        ));
    }
    Ok(first)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_options(name: &str) -> UnlockOptions {
        let dir = std::env::temp_dir().join(format!("wbf-unlock-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        UnlockOptions {
            data_dir: dir,
            passphrase_file: None,
        }
    }

    fn password_file(dir: &Path, text: &str) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let path = dir.join("pw.txt");
        std::fs::write(&path, text).unwrap();
        path
    }

    /// 這一側的責任是「把 passphrase 生出來」，所以測的是**來源的優先序**，
    /// 🚫 不是解鎖本身（那在 `wbf-core` 的測試裡）。
    /// 中文加一個 0x00：兩者以前都過不了 `read_to_string`（local-cache-db §12）。
    const BINARY_PASSPHRASE: &[u8] = "早安\u{0}世界".as_bytes();

    fn new_core(options: &UnlockOptions) -> Core {
        Core::open(&options.data_dir)
    }

    #[test]
    fn a_plain_vault_refuses_a_passphrase_file_instead_of_ignoring_it() {
        let mut options = scratch_options("plain");
        let core = Core::open(&options.data_dir);
        core.create_vault(None).unwrap();
        options.passphrase_file = Some(password_file(&options.data_dir, "x"));
        let fresh = Core::open(&options.data_dir);
        assert_eq!(
            options.unlock(&fresh).unwrap_err().kind,
            CoreErrorKind::UnexpectedPassphrase
        );
        let _ = std::fs::remove_dir_all(&options.data_dir);
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
        let mut options = scratch_options("binary-pp");
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

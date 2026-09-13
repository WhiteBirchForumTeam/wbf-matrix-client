//! 資料目錄的獨佔（architecture-v2 §0.2，維護者 2026-09-13 要求實作）。
//!
//! 🚨 **在這之前這條規則只寫在文件裡，一行實作都沒有** —— 而且兩層「以為會擋」的東西都不擋：
//!
//! | 以為會擋 | 實際上 |
//! |---|---|
//! | 資料庫的鎖 | `cache.db` 開在 **WAL** 模式，SQLite 的 WAL 本來就是設計給**多程序併發**的；matrix-sdk 的 store 同理 |
//! | port 重綁 | 預設 `--rpc-port 0`（隨機挑），所以兩個 daemon 根本不會撞到同一個 port |
//!
//! 所以獨佔要自己拿：起來時對 `<data dir>/daemon.lock` 拿一把**作業系統層的排他鎖**，
//! 拿不到就不啟動。
//!
//! ⭐ **為什麼是 OS 的鎖，不是「寫一個 pid 進檔案然後檢查它還活著嗎」**：後者一定有 race
//! （檢查完到寫進去之間，那個 pid 可能死掉、也可能剛好被回收給別人），而且 daemon 被 `kill -9`
//! 之後那個檔會留下來擋住下一次。OS 的鎖是**核心在程序結束時自動放手**的，🚫 沒有殘留鎖要清。
//!
//! 📎 鎖檔**不刪、也不寫東西進去**：刪掉會跟「另一個程序正要開它」對撞；而「誰握著」不放這裡 ——
//! Windows 的排他鎖連讀都擋，所以寫進去的字沒人讀得到。⭐ 那個問題已經有答案了：
//! `<data dir>/daemon.json` 裡有現在這個 daemon 的 `pid` 與 `instance`（§4.3）。
//!
//! 📎 用的是 `std::fs::File::try_lock`（Rust 1.89 起在標準庫裡，本專案 MSRV 1.95）——
//! 🚫 不引 `fs2`／`fs4`：標準庫已經有同一個東西了。

use std::fs::{File, TryLockError};
use std::path::{Path, PathBuf};

pub const LOCK_FILE_NAME: &str = "daemon.lock";

/// 拿到的獨佔權。⚠️ **活著就是鎖著**：這個值被丟掉（或程序結束）鎖才放開，
/// 所以呼叫端要把它一路拿到 daemon 結束為止，🚫 不要 `let _ = acquire(...)`。
#[derive(Debug)]
pub struct DataDirLock {
    /// 只是拿著不用：鎖綁在這個開著的檔案 handle 上。
    _file: File,
    path: PathBuf,
}

impl DataDirLock {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[derive(Debug)]
pub enum LockError {
    /// 別人握著：那個資料目錄已經有一個 daemon 在用。
    HeldByAnother(PathBuf),
    /// 開不了鎖檔（目錄不存在、唯讀、權限不足…）。
    Io(PathBuf, std::io::Error),
}

impl std::fmt::Display for LockError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LockError::HeldByAnother(path) => write!(
                formatter,
                "another daemon is already using this data directory (lock: {}); \
                 stop it first, or use a different --data-dir",
                path.display()
            ),
            LockError::Io(path, error) => {
                write!(
                    formatter,
                    "cannot open the lock file {}: {error}",
                    path.display()
                )
            }
        }
    }
}

/// 拿下這個資料目錄的獨佔權。
///
/// Args:
///     data_dir: example: "<data dir>"
/// Return:
///     Ok(DataDirLock)                    拿到了；**丟掉它就放手**
///     Err(HeldByAnother)                 已經有 daemon 在用這個目錄
///     Err(Io)                            鎖檔開不了
///
/// ⚠️ 這一步要**排在碰任何東西之前**（尤其是刪 `daemon.json`）：沒拿到鎖就動那個目錄，
/// 等於去動別人的檔案。
pub fn acquire(data_dir: &Path) -> Result<DataDirLock, LockError> {
    let path = data_dir.join(LOCK_FILE_NAME);
    if let Err(error) = std::fs::create_dir_all(data_dir) {
        return Err(LockError::Io(path, error));
    }
    let file = match File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
    {
        Ok(file) => file,
        Err(error) => return Err(LockError::Io(path, error)),
    };
    match file.try_lock() {
        Ok(()) => {}
        // 拿不到就是別人握著。🚫 不等、不重試：daemon 不是「排隊等前一個結束」的東西。
        Err(TryLockError::WouldBlock) => return Err(LockError::HeldByAnother(path)),
        Err(TryLockError::Error(error)) => return Err(LockError::Io(path, error)),
    }
    Ok(DataDirLock { _file: file, path })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_second_daemon_on_the_same_data_dir_is_refused_and_the_first_keeps_the_lock() {
        let dir = tempfile::tempdir().unwrap();
        let first = acquire(dir.path()).expect("第一個拿得到");
        match acquire(dir.path()) {
            Err(LockError::HeldByAnother(path)) => assert_eq!(path, *first.path()),
            other => panic!("第二個不該拿得到：{other:?}"),
        }
        // 第一個放手之後，下一個拿得到（🚫 不留殘留鎖）。
        drop(first);
        let second = acquire(dir.path()).expect("放手之後拿得到");
        assert!(second.path().exists());
    }

    /// 不同的資料目錄互不相干：一台機器上開兩個 daemon 是合法的，只要它們各有各的目錄。
    #[test]
    fn two_different_data_dirs_do_not_block_each_other() {
        let one = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let _first = acquire(one.path()).unwrap();
        let _second = acquire(other.path()).expect("另一個目錄不該被擋");
    }
}

//! 資料目錄的獨佔（architecture-v2 §0.2，維護者 2026-09-13 要求實作）。
//!
//! 🚨 **在這之前這條規則只寫在文件裡，一行實作都沒有** —— 而且兩層「以為會擋」的東西都不擋：
//!
//! | 以為會擋 | 實際上 |
//! |---|---|
//! | 資料庫的鎖 | `cache.db` 開在 **WAL** 模式，SQLite 的 WAL 本來就是設計給**多程序併發**的；matrix-sdk 的 store 同理 |
//! | port 重綁 | 預設 `--rpc-port 0`（隨機挑），所以兩個 daemon 根本不會撞到同一個 port |
//!
//! 所以獨佔要自己拿：起來時對 `<data dir>/daemon.lock` 拿一把**作業系統層的鎖**，拿不到就不啟動。
//!
//! ## 兩種鎖，對應兩種意圖（維護者 2026-09-13）
//!
//! | 誰 | 拿哪一種 | 意思 |
//! |---|---|---|
//! | daemon（**會寫**資料庫） | [`lock_for_writing`]：**排他** | 「這個目錄現在只有我在寫」 |
//! | 唯讀的工具（只看不改） | [`lock_for_reading`]：**共享** | 「我只讀；別人也可以一起讀，但這期間不准有人寫」 |
//!
//! 這是 OS 檔案鎖的標準語意，三條講完：**共享＋共享可以、共享＋排他不行、排他＋排他不行**。
//!
//! ⚠️ **一個要說清楚的後果**：daemon 活著的時候一直握著排他鎖，所以這期間任何人來要共享鎖都會被拒。
//! 這不是缺陷 —— 這把鎖回答的問題就只有一個：「誰有權碰這個目錄的檔案」。
//! ⭐ 「daemon 在寫、同時有人在讀」那種併發**不是靠這把鎖達成的**，是靠 `cache.db` 自己的 WAL
//! （SQLite 的 WAL 本來就是一個寫、多個讀，而且是**逐筆交易**的粒度，比一把長命鎖精確得多）。
//! 📎 架構上的答案仍然是：🚫 除了 daemon，沒有人該碰那些檔案 —— 要資料就走 RPC。
//! 共享鎖留給**離線**的唯讀工具（daemon 沒在跑的時候檢查資料目錄），而它被拒絕本身就是答案：
//! **現在有一個 daemon 在跑，去跟它講話，不要動它的檔。**
//!
//! ⚠️ 還有一件不能忘的：這是**勸告式**的鎖 —— 它只擋「有來問」的程序。🚫 它不會、也不可能阻止
//! 某個程式直接打開 `cache.db` 亂寫。它保護的是**我們自己這些程序之間**的約定。
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

/// 拿到的權利（排他或共享）。⚠️ **活著就是鎖著**：這個值被丟掉（或程序結束）鎖才放開，
/// 所以呼叫端要把它一路拿到用完為止，🚫 不要 `let _ = lock_for_writing(...)`。
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
    /// 別人握著一把不相容的鎖：要寫的時候有別人在（讀或寫），或要讀的時候有人在寫。
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

/// **要寫**這個資料目錄：拿排他鎖。daemon 走這條。
///
/// Args:
///     data_dir: example: "<data dir>"
/// Return:
///     Ok(DataDirLock)      拿到了；**丟掉它就放手**
///     Err(HeldByAnother)   別人在用（另一個 daemon 在寫，或有唯讀工具正在讀）
///     Err(Io)              鎖檔開不了
///
/// ⚠️ 這一步要**排在碰任何東西之前**（尤其是刪 `daemon.json`）：沒拿到鎖就動那個目錄，
/// 等於去動別人的檔案。
pub fn lock_for_writing(data_dir: &Path) -> Result<DataDirLock, LockError> {
    acquire(data_dir, Intent::Write)
}

/// **只讀**這個資料目錄：拿共享鎖。多個唯讀的程序可以同時拿到。
///
/// Args:
///     data_dir: example: "<data dir>"
/// Return:
///     Ok(DataDirLock)      拿到了；這期間🚫 沒有人寫得了
///     Err(HeldByAnother)   有 daemon 正握著寫鎖 —— ⭐ 這個答案本身就有用：
///                          **去跟那個 daemon 講話（RPC），不要動它的檔**
///     Err(Io)              鎖檔開不了
pub fn lock_for_reading(data_dir: &Path) -> Result<DataDirLock, LockError> {
    acquire(data_dir, Intent::Read)
}

#[derive(Clone, Copy)]
enum Intent {
    Read,
    Write,
}

fn acquire(data_dir: &Path, intent: Intent) -> Result<DataDirLock, LockError> {
    let path = data_dir.join(LOCK_FILE_NAME);
    if let Err(error) = std::fs::create_dir_all(data_dir) {
        return Err(LockError::Io(path, error));
    }
    // ⚠️ 共享鎖也要用可寫的 handle 開：Windows 上唯讀 handle 拿不到鎖。
    // 檔案內容從頭到尾是空的（📎 見模組註解），所以「可寫」不代表我們會寫。
    let file = match File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
    {
        Ok(file) => file,
        Err(error) => return Err(LockError::Io(path, error)),
    };
    let taken = match intent {
        Intent::Write => file.try_lock(),
        Intent::Read => file.try_lock_shared(),
    };
    match taken {
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
        let first = lock_for_writing(dir.path()).expect("第一個拿得到");
        match lock_for_writing(dir.path()) {
            Err(LockError::HeldByAnother(path)) => assert_eq!(path, *first.path()),
            other => panic!("第二個不該拿得到：{other:?}"),
        }
        // 第一個放手之後，下一個拿得到（🚫 不留殘留鎖）。
        drop(first);
        let second = lock_for_writing(dir.path()).expect("放手之後拿得到");
        assert!(second.path().exists());
    }

    /// 不同的資料目錄互不相干：一台機器上開兩個 daemon 是合法的，只要它們各有各的目錄。
    #[test]
    fn two_different_data_dirs_do_not_block_each_other() {
        let one = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let _first = lock_for_writing(one.path()).unwrap();
        let _second = lock_for_writing(other.path()).expect("另一個目錄不該被擋");
    }

    /// 共享＋共享可以：多個唯讀的程序同時看同一個資料目錄是允許的。
    #[test]
    fn many_readers_can_hold_the_lock_at_the_same_time() {
        let dir = tempfile::tempdir().unwrap();
        let _one = lock_for_reading(dir.path()).expect("第一個讀者");
        let _another = lock_for_reading(dir.path()).expect("第二個讀者也拿得到");
        let _third = lock_for_reading(dir.path()).expect("第三個也是");
    }

    /// 共享＋排他不行，**兩個方向都不行**。
    #[test]
    fn a_reader_and_a_writer_cannot_hold_it_at_the_same_time() {
        let dir = tempfile::tempdir().unwrap();
        // 有人在讀 → 寫不進來（🚫 不會「反正只是讀，讓他寫」）。
        let reader = lock_for_reading(dir.path()).unwrap();
        assert!(
            matches!(
                lock_for_writing(dir.path()),
                Err(LockError::HeldByAnother(_))
            ),
            "有讀者的時候不該讓人拿寫鎖"
        );
        drop(reader);

        // 有人在寫 → 讀不進來。⭐ 這正是 daemon 跑著的時候唯讀工具會遇到的情況，
        // 而那個拒絕就是答案：去走 RPC，不要動它的檔。
        let writer = lock_for_writing(dir.path()).unwrap();
        assert!(
            matches!(
                lock_for_reading(dir.path()),
                Err(LockError::HeldByAnother(_))
            ),
            "有寫者的時候不該讓人拿讀鎖"
        );
        drop(writer);

        let _after = lock_for_reading(dir.path()).expect("放手之後讀得了");
    }
}

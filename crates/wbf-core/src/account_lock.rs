//! 帳號生命週期鎖：`<data dir>/account.lock`（PR #40 審查 rumia🔴，維護者 2026-09-15 定形狀）。
//!
//! 登入、登出、摧毀都會建立或刪除帳號目錄與 server 目錄底下的東西。兩個同時跑，
//! destroy 可能刪掉一個剛登入的帳號寫下的 `m/`、`session.sealed`，或是登入在 destroy 收尾時冒出一個新目錄。
//! 「再掃一次目錄」只能縮小窗口，🚫 不是同步。所以這三個操作**全程**拿同一把排他鎖：
//!
//! | 誰先拿到 | 另一個 |
//! |---|---|
//! | destroy／logout | 登入被拒（`AccountBusy`），🚫 不排隊 |
//! | 登入 | destroy／logout 被拒（`AccountBusy`） |
//!
//! ⭐ 仿 `wbf-daemon` 的 `daemon.lock`：**作業系統層的鎖**（`File::try_lock`），核心在 handle 關掉或程序結束時自動放手，
//! 🚫 沒有殘留鎖要清；檔案**不刪、不寫內容**（刪掉會跟「另一個正要開它」對撞）。
//! 同一個程序裡兩個請求各開各的 handle，一樣互斥（Windows `LockFileEx`、Unix `flock` 都是以 handle 為單位）。
//!
//! 📎 範圍是**整個資料目錄**一把，不是一個帳號一把：鎖檔名如果帶 server 或帳號，就等於在目錄外面留下
//! 「這裡有過哪個 server」的痕跡（local-cache-db.md §11）；而這三個操作很少發生，一起排隊的代價很小。
//! 📎 🚫 不併進 `daemon.lock`：那把是「這個目錄現在誰有權寫」，daemon 活著就一直握著；這把只在一次操作的期間握著。

use std::fs::{File, TryLockError};
use std::path::Path;

use crate::error::{CoreError, CoreErrorKind};

pub const ACCOUNT_LOCK_FILE_NAME: &str = "account.lock";

/// `s/<b58>/server.lock`：**這個 server 目錄正在被刪**（維護者 2026-09-15）。
///
/// 跟 `account.lock` 不一樣，它是**磁碟上的標記**、🚫 不是 OS 鎖：程序當掉它也還在。
/// destroy 最後一個帳號時第一步放下、整個目錄刪完才消失；它還在，登入這台 server 就一律拒絕
/// （`ServerPendingRemoval`），🚫 core 不自己收拾，由使用者手動刪那個目錄。
pub const SERVER_LOCK_FILE_NAME: &str = "server.lock";

/// 收掉 `server.lock` 之前，server 目錄先改名成 `<原名>_to_be_delete`（維護者 2026-09-15）。
///
/// 改名之後原本的 `s/<b58>` 就不存在了：之後登入這台 server 建的是全新的目錄，🚫 不會被上一次沒刪完的東西擋住；
/// 而留下來的 `*_to_be_delete` 一看就知道是垃圾。掃描資料目錄時一律跳過它（`accounts::refresh_data_dir_map`）。
/// 📎 真的目錄名是 `<base58>_<base58>`，正好一個 `_`（base58 沒有 `_`）；帶這個後綴的名字至少有三個，
/// 所以不會跟真的 server 目錄撞名。
pub const TO_BE_DELETED_SUFFIX: &str = "_to_be_delete";

/// Return:
///     bool  這個 `s/` 底下的名字是不是「等著被刪」的舊 server 目錄
pub fn is_to_be_deleted_dir_name(dir_name: &str) -> bool {
    dir_name.ends_with(TO_BE_DELETED_SUFFIX)
}

/// 拿到的鎖。⚠️ **活著就是鎖著**：呼叫端要把它拿到操作結束，🚫 不要 `let _ = lock_account_lifecycle(...)`。
#[derive(Debug)]
pub(crate) struct AccountLifecycleLock {
    /// 只是拿著不用：鎖綁在這個開著的 handle 上。
    _file: File,
}

/// Args:
///     data_dir: example: "<data dir>"
/// Return:
///     Ok(AccountLifecycleLock)  拿到了；丟掉它就放手
///     Err(AccountBusy)          另一個登入／登出／摧毀正在進行
///     Err(Io)                   鎖檔開不了
pub(crate) fn lock_account_lifecycle(data_dir: &Path) -> Result<AccountLifecycleLock, CoreError> {
    let path = data_dir.join(ACCOUNT_LOCK_FILE_NAME);
    let io_error = |error: std::io::Error| {
        CoreError::new(
            CoreErrorKind::Io,
            format!("cannot open the account lock {}: {error}", path.display()),
        )
    };
    std::fs::create_dir_all(data_dir).map_err(io_error)?;
    // ⚠️ 用可寫的 handle 開：Windows 上唯讀 handle 拿不到鎖。檔案內容從頭到尾是空的。
    let file = File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .map_err(io_error)?;
    match file.try_lock() {
        Ok(()) => Ok(AccountLifecycleLock { _file: file }),
        // 🚫 不等、不重試：呼叫端（前端）決定要不要稍後再來。
        Err(TryLockError::WouldBlock) => Err(CoreError::new(
            CoreErrorKind::AccountBusy,
            "another login, logout or destroy is in progress on this data directory; try again when it has finished",
        )),
        Err(TryLockError::Error(error)) => Err(io_error(error)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_second_holder_is_refused_until_the_first_lets_go() {
        let dir =
            std::env::temp_dir().join(format!("wbf-core-account-lock-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let first = lock_account_lifecycle(&dir).expect("第一個拿得到");
        let refused = lock_account_lifecycle(&dir).unwrap_err();
        assert_eq!(refused.kind, CoreErrorKind::AccountBusy);
        drop(first);
        lock_account_lifecycle(&dir).expect("放手之後拿得到，🚫 沒有殘留鎖");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

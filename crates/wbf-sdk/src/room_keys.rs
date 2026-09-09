//! 本地的房間金鑰備份（local-cache-db.md §10.4）：`room-keys/snapshot` 一個檔，
//! 內容就是上游 `export_room_keys` 倒出來的**全量加密快照**。
//!
//! 為什麼要有它：房間金鑰（Megolm inbound session）平常只活在 matrix-sdk 的 `crypto.db` 裡，
//! 那個目錄 `logout` 會刪、壞掉也叫人刪。server 端的標準 backup 是主力，但在使用者產生
//! recovery key 之前，**解開它的私鑰也只在本機的 crypto store 裡**——所以本地這一份是那段期間
//! 唯一救得回歷史的東西（§10.2）。
//!
//! 這個模組只回答兩件事：**檔案放哪**、**用什麼 passphrase**。真正的匯出與匯入是上游做的
//! （`MatrixBackend::save_room_key_snapshot` / `import_room_key_snapshot`），所以這裡不吃
//! `matrix` feature，也**不需要把任何金鑰讀進我們的記憶體**：上游直接寫檔、直接讀檔。
//!
//! 加密：上游的匯出格式（Element 的 `MEGOLM SESSION DATA`）本身就是加密的，
//! passphrase 是這裡從 vault 第五把子金鑰導出的 **32 byte 隨機值**（不是使用者打的字），
//! PBKDF2 500,000 輪。🚫 **不再包一層我們自己的 AEAD**：passphrase 已經是 vault 保護的，
//! 多一層不增加安全性，只多一個要維護的格式（全域 CLAUDE.md A2）。
//!
//! 🚫 passphrase 不進錯誤訊息、不 log、不寫進別的檔。

use std::path::{Path, PathBuf};

use base64::Engine;
use zeroize::Zeroizing;

use crate::vault::Key32;
use crate::SdkError;

pub const ROOM_KEYS_DIR_NAME: &str = "room-keys";
const SNAPSHOT_FILE_NAME: &str = "snapshot";
/// 寫的時候先寫這個再 rename：寫到一半斷電不會把上一份好的蓋成半個檔。
const SNAPSHOT_TEMP_FILE_NAME: &str = "snapshot.tmp";

/// 這個帳號的快照檔在哪。
///
/// Args:
///     account_dir: example: "<data dir>/servers/<b58>_<b58>/accounts/<b58>_<b58>"
/// Return:
///     PathBuf   example: "<account dir>/room-keys/snapshot"
pub fn snapshot_path(account_dir: &Path) -> PathBuf {
    account_dir
        .join(ROOM_KEYS_DIR_NAME)
        .join(SNAPSHOT_FILE_NAME)
}

/// 寫入時的暫存檔（同一個目錄，才 rename 得動）。
pub fn snapshot_temp_path(account_dir: &Path) -> PathBuf {
    account_dir
        .join(ROOM_KEYS_DIR_NAME)
        .join(SNAPSHOT_TEMP_FILE_NAME)
}

/// 快照的 passphrase：vault 第五把子金鑰的 base64。
///
/// 上游的 API 只吃字串，但這裡餵的是 32 byte 隨機金鑰而不是人打的字——
/// 所以「passphrase 太弱被暴力破」在這裡不存在。
///
/// Args:
///     key: example: vault.room_key_backup_key()
/// Return:
///     Zeroizing<String>   base64，用完就抹掉
pub fn snapshot_passphrase(key: &Key32) -> Zeroizing<String> {
    Zeroizing::new(base64::engine::general_purpose::STANDARD.encode(key.as_bytes()))
}

/// 快照的狀態（`key-backup status` 印，🚫 不含金鑰內容）。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SnapshotStatus {
    pub exists: bool,
    pub bytes: u64,
    /// 上次存的時間（Unix 秒）。拿不到就是 `None`。
    pub saved_at: Option<u64>,
}

/// Args:
///     account_dir: example: "<data dir>/servers/<b58>_<b58>/accounts/<b58>_<b58>"
/// Return:
///     SnapshotStatus   沒有檔案時 `exists` 是 false，其他欄位是 0／None
pub fn get_snapshot_status(account_dir: &Path) -> SnapshotStatus {
    let Ok(metadata) = std::fs::metadata(snapshot_path(account_dir)) else {
        return SnapshotStatus::default();
    };
    SnapshotStatus {
        exists: true,
        bytes: metadata.len(),
        saved_at: metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|since| since.as_secs()),
    }
}

/// 建好 `room-keys/` 並把權限收成只有自己（Unix 0700）。
///
/// ⚠️ 上游的 `export_room_keys` 用 `File::create` 寫檔，那走 umask 預設（多半是 0644），
/// 而這個檔是**全部房間金鑰的密文**。內容層有 PBKDF2-500k 加 32 byte 隨機 passphrase 頂著，
/// 但跟 vault 全面 0600 的紀律不一致（PR #19 審查 rumia🟡2／salvia🟡4）。
/// 所以：目錄先收成 0700，寫完的檔再收成 0600（`set_snapshot_permissions`）。
///
/// Args:
///     dir: **`room-keys/` 本身**, example: snapshot_path(&account.dir).parent()
/// Return:
///     Ok(())   目錄在，權限也對了
///     Err(Io)  建不起來
pub fn prepare_dir(dir: &Path) -> Result<(), SdkError> {
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// 把寫好的快照收成 0600。`rename` 保留來源檔的權限，所以這一步要在 rename **之前**做。
///
/// Args:
///     path: 剛寫好的暫存檔, example: snapshot_temp_path(&account.dir)
/// Return:
///     Ok(())   收好了（非 Unix 平台是 no-op）
///     Err(Io)  改不動
pub fn set_snapshot_permissions(path: &Path) -> Result<(), SdkError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

/// 刪掉這個帳號的本地快照（`logout` 與 `account destroy` 用；local-cache-db.md §10.7）。
///
/// Return:
///     Ok(true)    刪掉了
///     Ok(false)   本來就沒有
///     Err(Io)     刪不掉
pub fn del_snapshot(account_dir: &Path) -> Result<bool, SdkError> {
    match std::fs::remove_dir_all(account_dir.join(ROOM_KEYS_DIR_NAME)) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_passphrase_is_the_key_not_a_word() {
        let passphrase = snapshot_passphrase(&Key32([7u8; 32]));
        // 32 byte base64 = 44 字元；重點是它不是人打得出來的東西。
        assert_eq!(passphrase.len(), 44);
        assert_eq!(
            &*passphrase,
            &*snapshot_passphrase(&Key32([7u8; 32])),
            "同一把金鑰要導出同一個 passphrase，不然存進去的讀不回來"
        );
        assert_ne!(
            &*passphrase,
            &*snapshot_passphrase(&Key32([9u8; 32])),
            "別把金鑰要導出別的 passphrase"
        );
    }

    #[test]
    fn status_of_a_missing_snapshot_is_not_an_error() {
        let dir = std::env::temp_dir().join(format!("wbf-snap-none-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let status = get_snapshot_status(&dir);
        assert_eq!(status, SnapshotStatus::default());
        assert!(!status.exists);
    }

    #[test]
    fn status_reads_a_written_snapshot_and_del_removes_it() {
        let dir = std::env::temp_dir().join(format!("wbf-snap-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(ROOM_KEYS_DIR_NAME)).unwrap();
        std::fs::write(snapshot_path(&dir), b"-----BEGIN MEGOLM SESSION DATA-----").unwrap();

        let status = get_snapshot_status(&dir);
        assert!(status.exists && status.bytes > 0);

        assert!(del_snapshot(&dir).unwrap());
        assert!(!get_snapshot_status(&dir).exists);
        assert!(!del_snapshot(&dir).unwrap(), "已經刪過就回 false，不報錯");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

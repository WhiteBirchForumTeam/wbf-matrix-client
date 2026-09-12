//! 房間金鑰備份：server 端的標準 Matrix key backup、本地的全量快照、recovery key。
//! 設計在 local-cache-db.md §10。
//!
//! 🚫 這裡**不判斷 conf 的兩個開關**（`SERVER_BACKUP`／`LOCAL_ROOM_KEYS`）：那是「要不要
//! 叫我」的決定，屬於前端（§3「🚫 不代前端做決定」）。core 被叫到就做，🚫 不會回一句
//! 「好了」卻什麼都沒做。

use serde::Serialize;
use zeroize::Zeroizing;

use wbf_sdk::room_keys;

use crate::accounts::AccountDir;
use crate::error::{CoreError, CoreErrorKind};
use crate::recovery;
use crate::Core;

/// `key-backup status`。
///
/// ⚠️ 這裡面**只有上游與磁碟的實況**。conf 那兩個開關現在是什麼，是前端自己的值，
/// 由前端自己加進輸出——core 不知道有 conf 這種東西。
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct BackupStatusReport {
    pub server_backup_exists: bool,
    pub uploading_locally: bool,
    /// ⚠️ 只說得出「SSSS 設好了」，**說不出那串 key 在誰手上**——名字刻意不叫
    /// `has_recovery_key`（PR #19 審查）。
    pub recovery_enabled: bool,
    pub recovery_state: String,
    pub local_snapshot: bool,
    pub local_snapshot_bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub local_snapshot_saved_at: Option<u64>,
}

/// `key-backup upload`。
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct UploadResult {
    pub server_backup_exists: bool,
    pub recovery_enabled: bool,
    /// 沒順手存本地快照時是 `None`（呼叫端把 `also_save_snapshot` 關掉）。
    pub local_snapshot_bytes: Option<u64>,
}

/// `key-backup import`。
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ImportResult {
    pub imported: usize,
    pub total: usize,
}

/// `key-backup restore`／`recovery` 之後 server 端的狀態。
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RecoveryStateReport {
    pub recovery_enabled: bool,
    pub recovery_state: String,
}

impl Core {
    /// server 上有沒有備份、本機有沒有在上傳、本地快照多大。
    pub async fn backup_status(
        &self,
        user: Option<&str>,
        server: Option<&str>,
        server_backup: bool,
    ) -> Result<BackupStatusReport, CoreError> {
        let account = self.account_or_current(user, server)?;
        let backend = self.backend_of(&account, server_backup).await?;
        backend.sync_once(None, std::time::Duration::ZERO).await?;
        let status = backend.backup_status().await?;
        let snapshot = room_keys::get_snapshot_status(&account.dir);
        Ok(BackupStatusReport {
            server_backup_exists: status.exists_on_server,
            uploading_locally: status.enabled_locally,
            recovery_enabled: status.recovery_enabled,
            recovery_state: status.recovery_state,
            local_snapshot: snapshot.exists,
            local_snapshot_bytes: snapshot.bytes,
            local_snapshot_saved_at: snapshot.saved_at,
        })
    }

    /// 把 crypto store 裡的金鑰推上 server 的備份，**傳完才回來**。
    ///
    /// Args:
    ///     also_save_snapshot: 順手也更新本地快照（local-cache-db §10.5：兩份備份的用途
    ///         不同，但沒有理由讓使用者記得跑兩個命令）。呼叫端把本地那份關掉時傳 `false`
    pub async fn upload_room_keys(
        &self,
        user: Option<&str>,
        server: Option<&str>,
        also_save_snapshot: bool,
        server_backup: bool,
    ) -> Result<UploadResult, CoreError> {
        let account = self.account_or_current(user, server)?;
        let backend = self.backend_of(&account, server_backup).await?;
        backend.sync_once(None, std::time::Duration::ZERO).await?;
        self.events
            .progress("uploading room keys to the server backup...");
        backend.upload_room_keys().await?;
        let local_snapshot_bytes = match also_save_snapshot {
            true => Some(self.save_snapshot_of(&account, &backend).await?),
            false => None,
        };
        let status = backend.backup_status().await?;
        Ok(UploadResult {
            server_backup_exists: status.exists_on_server,
            recovery_enabled: status.recovery_enabled,
            local_snapshot_bytes,
        })
    }

    /// 把 crypto store 裡的**全部**房間金鑰倒進本地快照（全量覆蓋）。
    ///
    /// 🚫 `login` 之後**不要**叫它：剛登入的 crypto store 幾乎沒有金鑰，存了也是空的
    /// （PR #19 審查 rumia🟡3／salvia🟡2）。
    pub async fn save_room_key_snapshot(
        &self,
        user: Option<&str>,
        server: Option<&str>,
        server_backup: bool,
    ) -> Result<u64, CoreError> {
        let account = self.account_or_current(user, server)?;
        let backend = self.backend_of(&account, server_backup).await?;
        self.save_snapshot_of(&account, &backend).await
    }

    /// 把本地快照餵回 crypto store（重新 `login`、或刪過 `m/` 之後用）。
    pub async fn import_room_key_snapshot(
        &self,
        user: Option<&str>,
        server: Option<&str>,
        server_backup: bool,
    ) -> Result<ImportResult, CoreError> {
        let account = self.account_or_current(user, server)?;
        let backend = self.backend_of(&account, server_backup).await?;
        let key = self.vault()?.room_key_backup_key();
        let (imported, total) = backend
            .import_room_key_snapshot(
                &room_keys::snapshot_path(&account.dir),
                &room_keys::snapshot_passphrase(&key),
            )
            .await?;
        Ok(ImportResult { imported, total })
    }

    /// 用這台機器保管的 recovery key 把**這台裝置**恢復（解 SSSS、拿回備份的解密金鑰）。
    ///
    /// ⚠️ **重新 `login` 之後一定要跑**：新裝置的 crypto store 沒有 SSSS 的 secrets，
    /// `RecoveryState` 會是 `Incomplete`，server 上那份備份解不開
    /// （2026-09-09 對真 server 驗證時發現的缺口）。
    pub async fn restore_from_recovery_key(
        &self,
        user: Option<&str>,
        server: Option<&str>,
        server_backup: bool,
    ) -> Result<RecoveryStateReport, CoreError> {
        let account = self.account_or_current(user, server)?;
        let backend = self.backend_of(&account, server_backup).await?;
        backend.sync_once(None, std::time::Duration::ZERO).await?;
        let user_id = self.session_of(&account)?.user_id;
        let key = recovery::find(&self.data_dir, self.vault()?, &user_id)?.ok_or_else(|| {
            CoreError::new(
                CoreErrorKind::NoRecoveryKeyHere,
                format!(
                    "no recovery key is kept here for {user_id}; create one, or restore it from wherever you wrote it down"
                ),
            )
        })?;
        backend.recover_with(&key).await?;
        let status = backend.backup_status().await?;
        Ok(RecoveryStateReport {
            recovery_enabled: status.recovery_enabled,
            recovery_state: status.recovery_state,
        })
    }

    /// 產生 recovery key，並**封進 `r/`**（🚫 不是帳號目錄——`logout` 會把那裡清光，
    /// 而 recovery key 正是清完之後唯一回得去的路；§10.8）。
    ///
    /// ⚠️ **回傳值是秘密**。呼叫端要負責讓使用者看到並抄下來：這台機器保管著它，但
    /// 機器沒了就兩份都沒了。
    pub async fn create_recovery_key(
        &self,
        user: Option<&str>,
        server: Option<&str>,
        server_backup: bool,
    ) -> Result<Zeroizing<String>, CoreError> {
        let account = self.account_or_current(user, server)?;
        let backend = self.backend_of(&account, server_backup).await?;
        backend.sync_once(None, std::time::Duration::ZERO).await?;
        let recovery_key = backend.enable_recovery().await?;
        let user_id = self.session_of(&account)?.user_id;
        recovery::save(&self.data_dir, self.vault()?, &user_id, &recovery_key)?;
        Ok(Zeroizing::new(recovery_key))
    }

    /// 沒給 `user` 就用 `current`。
    pub(crate) fn account_or_current(
        &self,
        user: Option<&str>,
        server: Option<&str>,
    ) -> Result<AccountDir, CoreError> {
        match user {
            Some(user) => self.find_account(user, server),
            None => self.current_account(),
        }
    }

    async fn save_snapshot_of(
        &self,
        account: &AccountDir,
        backend: &wbf_sdk::backend::matrix_sdk::MatrixBackend,
    ) -> Result<u64, CoreError> {
        let key = self.vault()?.room_key_backup_key();
        Ok(backend
            .save_room_key_snapshot(
                &room_keys::snapshot_path(&account.dir),
                &room_keys::snapshot_temp_path(&account.dir),
                &room_keys::snapshot_passphrase(&key),
            )
            .await?)
    }
}

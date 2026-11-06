//! 房間金鑰備份：server 端的標準 Matrix key backup、本地的全量快照、recovery key。
//! 設計在 /docs/design/keys/room-key-backup.md；wbf 帳號那一半（沒有 matrix-sdk 的 `Client`，直接在 `OlmMachine` 上走橋）在
//! /docs/design/keys/cross-signing-and-verification.md §4。每支先看帳號種類、走各自的那條，回的形狀一樣：UI 不必分帳號種類。
//!
//! 這裡**不判斷 conf 的兩個開關**（`SERVER_BACKUP`／`LOCAL_ROOM_KEYS`）：那是「要不要
//! 叫我」的決定，屬於前端（/docs/design/overview/architecture-v2.md §3「**不代前端做決定**」）。core 被叫到就做，**不**會回一句
//! 「好了」卻什麼都沒做。

use serde::Serialize;
use zeroize::Zeroizing;

use wbf_sdk::crypto_engine::OlmEngine;
use wbf_sdk::{cross_signing, key_backup, room_keys, secret_storage};

use crate::accounts::AccountDir;
use crate::backend_choice::MethodHome;
use crate::error::{CoreError, CoreErrorKind};
use crate::link_pool::{LinkRole, PooledClient};
use crate::recovery;
use crate::{Core, Target};

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
    pub async fn backup_status(&self, target: &Target) -> Result<BackupStatusReport, CoreError> {
        let account = self.account_or_current(target)?;
        if self.is_wbf_account(&account)? {
            return self.wbf_backup_status(&account).await;
        }
        let backend = self.backend_of(&account, target.server_backup).await?;
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
    ///     also_save_snapshot: 順手也更新本地快照（/docs/design/keys/room-key-backup.md §5：兩份備份的用途
    ///         不同，但沒有理由讓使用者記得跑兩個命令）。呼叫端把本地那份關掉時傳 `false`
    pub async fn upload_room_keys(
        &self,
        target: &Target,
        also_save_snapshot: bool,
    ) -> Result<UploadResult, CoreError> {
        let account = self.account_or_current(target)?;
        if self.is_wbf_account(&account)? {
            return self
                .wbf_upload_room_keys(&account, target.server_backup, also_save_snapshot)
                .await;
        }
        let backend = self.backend_of(&account, target.server_backup).await?;
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
    /// `login` 之後**不要**叫它：剛登入的 crypto store 幾乎沒有金鑰，存了也是空的
    /// （PR #19 審查 rumia🟡3／salvia🟡2）。
    pub async fn save_room_key_snapshot(&self, target: &Target) -> Result<u64, CoreError> {
        let account = self.account_or_current(target)?;
        if self.is_wbf_account(&account)? {
            let engine = self.olm_engine_of(&account).await?;
            return self.wbf_save_snapshot(&account, &engine).await;
        }
        let backend = self.backend_of(&account, target.server_backup).await?;
        self.save_snapshot_of(&account, &backend).await
    }

    /// 把本地快照餵回 crypto store（重新 `login`、或刪過 `m/` 之後用）。
    pub async fn import_room_key_snapshot(
        &self,
        target: &Target,
    ) -> Result<ImportResult, CoreError> {
        let account = self.account_or_current(target)?;
        let key = self.vault()?.room_key_backup_key();
        if self.is_wbf_account(&account)? {
            let engine = self.olm_engine_of(&account).await?;
            let (imported, total) = key_backup::import_snapshot(
                &engine,
                &room_keys::snapshot_path(&account.dir),
                &room_keys::snapshot_passphrase(&key),
            )
            .await?;
            return Ok(ImportResult { imported, total });
        }
        let backend = self.backend_of(&account, target.server_backup).await?;
        let (imported, total) = backend
            .import_room_key_snapshot(
                &room_keys::snapshot_path(&account.dir),
                &room_keys::snapshot_passphrase(&key),
            )
            .await?;
        Ok(ImportResult { imported, total })
    }

    /// 用 recovery key 把**這台裝置**恢復（解 SSSS、拿回交叉簽章私鑰與備份的解密金鑰）。
    ///
    /// ⚠️ **重新 `login` 之後一定要跑**：新裝置的 crypto store 沒有 SSSS 的 secrets，
    /// `RecoveryState` 會是 `Incomplete`，server 上那份備份解不開
    /// （2026-09-09 對真 server 驗證時發現的缺口）。
    ///
    /// Args:
    ///     recovery_key: 使用者抄下來的那串（換機器時這台的 `r/` 沒有）；打開成功才封進 `r/`。None 就用 `r/` 封著的, example: Some("EsTc 1234 …")
    /// Return:
    ///     Ok(RecoveryStateReport)
    ///     Err(NoRecoveryKeyHere)  沒帶、`r/` 也沒有
    ///     Err(Usage)              recovery key 打不開（什麼都沒匯）、server 上沒有 secret storage
    pub async fn restore_from_recovery_key(
        &self,
        target: &Target,
        recovery_key: Option<&str>,
    ) -> Result<RecoveryStateReport, CoreError> {
        let account = self.account_or_current(target)?;
        let user_id = self.session_of(&account)?.user_id;
        let key = match recovery_key {
            Some(given) => Zeroizing::new(given.to_string()),
            None => recovery::find(&self.data_dir, self.vault()?, &user_id)?.ok_or_else(|| {
                CoreError::new(
                    CoreErrorKind::NoRecoveryKeyHere,
                    format!(
                        "no recovery key is kept here for {user_id}; create one, or restore it from wherever you wrote it down"
                    ),
                )
            })?,
        };
        if self.is_wbf_account(&account)? {
            let report = self.wbf_restore(&account, &key).await?;
            if recovery_key.is_some() {
                recovery::save(&self.data_dir, self.vault()?, &user_id, &key)?;
            }
            return Ok(report);
        }
        let backend = self.backend_of(&account, target.server_backup).await?;
        backend.sync_once(None, std::time::Duration::ZERO).await?;
        backend.recover_with(&key).await?;
        if recovery_key.is_some() {
            recovery::save(&self.data_dir, self.vault()?, &user_id, &key)?;
        }
        let status = backend.backup_status().await?;
        Ok(RecoveryStateReport {
            recovery_enabled: status.recovery_enabled,
            recovery_state: status.recovery_state,
        })
    }

    /// 產生 recovery key，並**封進 `r/`**（**不是**帳號目錄——`logout` 會把那裡清光，
    /// 而 recovery key 正是清完之後唯一回得去的路；/docs/design/keys/room-key-backup.md §8）。
    ///
    /// ⚠️ **回傳值是秘密**。呼叫端要負責讓使用者看到並抄下來：這台機器保管著它，但
    /// 機器沒了就兩份都沒了。
    pub async fn create_recovery_key(
        &self,
        target: &Target,
    ) -> Result<Zeroizing<String>, CoreError> {
        let account = self.account_or_current(target)?;
        let user_id = self.session_of(&account)?.user_id;
        if self.is_wbf_account(&account)? {
            let (engine, mut client) = self.wbf_backup_parts(&account).await?;
            let recovery_key = secret_storage::create_secret_storage(&engine, &mut client).await?;
            recovery::save(&self.data_dir, self.vault()?, &user_id, &recovery_key)?;
            return Ok(recovery_key);
        }
        let backend = self.backend_of(&account, target.server_backup).await?;
        backend.sync_once(None, std::time::Duration::ZERO).await?;
        let recovery_key = backend.enable_recovery().await?;
        recovery::save(&self.data_dir, self.vault()?, &user_id, &recovery_key)?;
        Ok(Zeroizing::new(recovery_key))
    }

    /// 沒給 `user` 就用 `current`。
    pub(crate) fn account_or_current(&self, target: &Target) -> Result<AccountDir, CoreError> {
        match target.user() {
            Some(user) => self.find_account(user, target.server()),
            None => self.current_account(),
        }
    }

    /// wbf 帳號的備份要的：這個帳號的引擎與金鑰線（備份、account data 都走橋）。
    async fn wbf_backup_parts(
        &self,
        account: &AccountDir,
    ) -> Result<(std::sync::Arc<OlmEngine>, PooledClient), CoreError> {
        let engine = self.olm_engine_of(account).await?;
        let client = self
            .client_of(
                account,
                wbf_sdk::Transport::WebSocket,
                MethodHome::WbfSdkOnly,
                LinkRole::Keys,
            )
            .await?;
        Ok((engine, client))
    }

    /// wbf 帳號的 `backup.status`：server 有沒有備份、這台有沒有在上傳、secret storage 在不在、這台有沒有三把交叉簽章私鑰。
    /// `recovery_state` 照上游 `RecoveryState` 的字：沒有 secret storage 是 `Disabled`；有而這台私鑰齊是 `Enabled`、不齊是 `Incomplete`。
    async fn wbf_backup_status(
        &self,
        account: &AccountDir,
    ) -> Result<BackupStatusReport, CoreError> {
        let (engine, mut client) = self.wbf_backup_parts(account).await?;
        let exists_on_server = key_backup::find_latest_backup(&mut client).await?.is_some();
        let has_secret_storage = secret_storage::has_secret_storage(&engine, &mut client).await?;
        drop(client);
        let machine = engine.machine();
        let uploading_locally = machine.backup_machine().enabled().await;
        let keys = machine.cross_signing_status().await;
        let recovery_state = match (has_secret_storage, keys.is_complete()) {
            (false, _) => "Disabled",
            (true, true) => "Enabled",
            (true, false) => "Incomplete",
        };
        let snapshot = room_keys::get_snapshot_status(&account.dir);
        Ok(BackupStatusReport {
            server_backup_exists: exists_on_server,
            uploading_locally,
            recovery_enabled: has_secret_storage,
            recovery_state: recovery_state.to_string(),
            local_snapshot: snapshot.exists,
            local_snapshot_bytes: snapshot.bytes,
            local_snapshot_saved_at: snapshot.saved_at,
        })
    }

    /// wbf 帳號的 `backup.upload`：備份接上（server 沒有就建）、傳到上游說沒有為止才回（後台也在傳，這支是「等追平」）。
    ///
    /// Return:
    ///     Err(Usage)   server 上的備份不是這台打得開的（先 `backup.restore`）
    async fn wbf_upload_room_keys(
        &self,
        account: &AccountDir,
        create_if_missing: bool,
        also_save_snapshot: bool,
    ) -> Result<UploadResult, CoreError> {
        let (engine, mut client) = self.wbf_backup_parts(account).await?;
        let setup = key_backup::ensure_backup(&engine, &mut client, create_if_missing).await?;
        if setup.find_enabled_version().is_none() {
            return Err(CoreError::new(
                CoreErrorKind::Usage,
                format!(
                    "{}: this device cannot write to the server backup ({setup:?}); restore with the recovery key first",
                    account.label()
                ),
            ));
        }
        self.events
            .progress("uploading room keys to the server backup...");
        key_backup::upload_room_keys(&engine, &mut client).await?;
        let recovery_enabled = secret_storage::has_secret_storage(&engine, &mut client).await?;
        drop(client);
        let local_snapshot_bytes = match also_save_snapshot {
            true => Some(self.wbf_save_snapshot(account, &engine).await?),
            false => None,
        };
        Ok(UploadResult {
            server_backup_exists: true,
            recovery_enabled,
            local_snapshot_bytes,
        })
    }

    /// wbf 帳號的 recovery：打開 secret storage、匯交叉簽章私鑰、簽自己這台（別人才信）、有備份金鑰就整份還原並補解 cache 裡的密文。
    /// recovery key 不對就在第一步停下、什麼都沒匯（fail closed，`secret_storage::restore_secrets`）。
    async fn wbf_restore(
        &self,
        account: &AccountDir,
        recovery_key: &str,
    ) -> Result<RecoveryStateReport, CoreError> {
        let (engine, mut client) = self.wbf_backup_parts(account).await?;
        let restored = secret_storage::restore_secrets(&engine, &mut client, recovery_key).await?;
        cross_signing::sign_this_device(&engine, &mut client).await?;
        if let Some(backup_key) = restored.backup_key {
            let report = key_backup::restore_room_keys(&engine, &mut client, backup_key).await?;
            drop(client);
            self.events.progress(format!(
                "restored {} of {} room key(s) from the server backup",
                report.imported, report.total
            ));
            let (cache, me) = self.server_cache_and_me(account)?;
            crate::identity_ops::decrypt_restored(
                &engine,
                &cache,
                &self.events,
                &me,
                &report.sessions,
            )
            .await;
        }
        Ok(RecoveryStateReport {
            recovery_enabled: true,
            recovery_state: "Enabled".to_string(),
        })
    }

    async fn wbf_save_snapshot(
        &self,
        account: &AccountDir,
        engine: &OlmEngine,
    ) -> Result<u64, CoreError> {
        let key = self.vault()?.room_key_backup_key();
        Ok(key_backup::save_snapshot(
            engine,
            &room_keys::snapshot_path(&account.dir),
            &room_keys::snapshot_temp_path(&account.dir),
            &room_keys::snapshot_passphrase(&key),
        )
        .await?)
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

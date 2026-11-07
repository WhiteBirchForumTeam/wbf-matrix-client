//! wbf 帳號的房間金鑰備份：server 端的標準 Matrix key backup（`m.megolm_backup.v1.curve25519-aes-sha2`）與本地快照
//! （/docs/design/keys/cross-signing-and-verification.md §4、/docs/design/keys/room-key-backup.md）。
//! 狀態機是上游的 `BackupMachine`；這裡建版本、送它排好的上傳、下載解密匯入。
//! 上游的 `KeysBackupRequest` **不**從 `outgoing_requests()` 出來，只有 [`upload_room_keys`] 送它。

use std::collections::BTreeMap;
use std::path::Path;

use matrix_sdk::ruma;
use matrix_sdk_crypto::backups::MegolmV1BackupKey;
use matrix_sdk_crypto::olm::ExportedRoomKey;
use matrix_sdk_crypto::store::types::BackupDecryptionKey;
use matrix_sdk_crypto::types::requests::AnyIncomingResponse;
use matrix_sdk_crypto::types::RoomKeyBackupInfo;
use ruma::api::client::backup::{add_backup_keys, RoomKeyBackup};
use ruma::events::secret::request::SecretName;
use ruma::{OwnedRoomId, UInt};
use serde_json::{json, Map, Value};
use zeroize::Zeroizing;

use crate::channel::PackChannel;
use crate::client::WbfClient;
use crate::crypto_engine::OlmEngine;
use crate::matrix_endpoint;
use crate::SdkError;

/// 本地快照的 PBKDF2 輪數：跟 matrix-sdk 的 `export_room_keys` 同一個數（/docs/design/keys/room-key-backup.md §4）。
const SNAPSHOT_ROUNDS: u32 = 500_000;

/// 一次上傳最多繞幾批：上游一批最多 100 把，繞到這裡還有東西就停、回 [`UploadOutcome::StoppedAtLimit`]，下次再接著傳。
const UPLOAD_ROUNDS_LIMIT: usize = 1_000;

/// [`upload_room_keys`] 停在哪。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UploadOutcome {
    /// 上游說沒有要傳的了
    CaughtUp { batches: usize },
    /// 繞滿 [`UPLOAD_ROUNDS_LIMIT`] 批還有：剩下的留給下一次
    StoppedAtLimit { batches: usize },
}

/// [`ensure_backup`] 之後這台跟 server 那份備份的關係。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BackupSetup {
    /// server 上有、這台有它的解密金鑰：上傳開著
    Enabled { version: String },
    /// server 上本來沒有：這台建了一個新版本、上傳開著
    Created { version: String },
    /// server 上有、這台沒有它的解密金鑰：**不**建、**不**傳，等 restore 或自我驗證把金鑰拿過來
    NoKeyHere { version: String },
    /// server 上沒有、也沒叫它建
    NoBackup,
}

impl BackupSetup {
    /// Return:
    ///     Option<&str>  上傳開著的那個版本；沒開是 None
    pub fn find_enabled_version(&self) -> Option<&str> {
        match self {
            BackupSetup::Enabled { version } | BackupSetup::Created { version } => Some(version),
            BackupSetup::NoKeyHere { .. } | BackupSetup::NoBackup => None,
        }
    }
}

/// 從備份還原的結果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RestoreReport {
    /// 這次新匯進 crypto store 的把數
    pub imported: usize,
    /// 備份裡解得開的總把數
    pub total: usize,
    /// 匯進來的 `(房間, session_id)`：呼叫者拿它去補解 cache 裡的密文
    pub sessions: Vec<(String, String)>,
}

/// server 現在的備份版本與它的資訊。
///
/// Return:
///     Ok(Some((version, info)))
///     Ok(None)        server 上沒有任何版本（404）
///     Err(Server)     其他拒絕
///     Err(Protocol)   回的資訊解不開
pub async fn find_latest_backup<C: PackChannel>(
    client: &mut WbfClient<C>,
) -> Result<Option<(String, RoomKeyBackupInfo)>, SdkError> {
    let body = match client
        .call_matrix_endpoint(&matrix_endpoint::LATEST_BACKUP_INFO, &Map::new(), None)
        .await
    {
        Ok(body) => body,
        Err(error) if error.is_not_found() => return Ok(None),
        Err(error) => return Err(error),
    };
    let version = body
        .get("version")
        .and_then(Value::as_str)
        .ok_or_else(|| SdkError::Protocol(format!("LatestBackupInfo has no version: {body}")))?
        .to_string();
    let info: RoomKeyBackupInfo = serde_json::from_value(body.clone())
        .map_err(|error| SdkError::Protocol(format!("LatestBackupInfo: {error}")))?;
    Ok(Some((version, info)))
}

/// 讓這台跟 server 那份備份接上（/docs/design/keys/cross-signing-and-verification.md §4.1）：
/// server 有、這台有對得上的解密金鑰 → 開上傳；server 沒有而 `create_if_missing` → 建一個新版本、開上傳；
/// server 有、這台的金鑰對不上（或沒有）→ **不**建新的、**不**傳。
/// 後兩種情況，上傳要是開著（上游開引擎時會把 store 存的舊版本自動接上）就關掉：server 的版本被別台換過了。
///
/// Args:
///     create_if_missing: conf 的 `SERVER_BACKUP` 開著時是 true
/// Return:
///     Ok(BackupSetup)
///     Err(Server)／Err(Protocol)  問不到 server、建版本被拒、crypto store 寫不進去
pub async fn ensure_backup<C: PackChannel>(
    engine: &OlmEngine,
    client: &mut WbfClient<C>,
    create_if_missing: bool,
) -> Result<BackupSetup, SdkError> {
    let backups = engine.machine().backup_machine();
    match find_latest_backup(client).await? {
        Some((version, info)) => {
            let saved = backups
                .get_backup_keys()
                .await
                .map_err(crypto_store_error)?
                .decryption_key;
            match saved {
                Some(key) if key.backup_key_matches(&info) => {
                    enable_with(engine, key, &version).await?;
                    Ok(BackupSetup::Enabled { version })
                }
                _ => {
                    disable_stale_backup(engine).await?;
                    Ok(BackupSetup::NoKeyHere { version })
                }
            }
        }
        None if create_if_missing => {
            let key = BackupDecryptionKey::new();
            let mut info = key.to_backup_info();
            // 用這台的裝置金鑰（與 master，有的話）簽：別的裝置靠簽章判斷這份備份是不是自己人建的。
            backups
                .sign_backup(&mut info)
                .await
                .map_err(|error| SdkError::Protocol(format!("signing the backup: {error}")))?;
            let body = serde_json::to_value(&info)
                .map_err(|error| crate::error::cannot_serialize("backup info", error))?;
            let reply = client
                .call_matrix_endpoint(
                    &matrix_endpoint::CREATE_BACKUP_VERSION,
                    &Map::new(),
                    Some(&body),
                )
                .await?;
            let version = reply
                .get("version")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    SdkError::Protocol(format!("CreateBackupVersion has no version: {reply}"))
                })?
                .to_string();
            enable_with(engine, key, &version).await?;
            Ok(BackupSetup::Created { version })
        }
        None => {
            disable_stale_backup(engine).await?;
            Ok(BackupSetup::NoBackup)
        }
    }
}

/// 上傳開著就關掉（存著的解密金鑰不刪）。
async fn disable_stale_backup(engine: &OlmEngine) -> Result<(), SdkError> {
    let backups = engine.machine().backup_machine();
    if !backups.enabled().await {
        return Ok(());
    }
    backups.disable_backup().await.map_err(crypto_store_error)
}

/// 存解密金鑰、開上傳。
async fn enable_with(
    engine: &OlmEngine,
    key: BackupDecryptionKey,
    version: &str,
) -> Result<(), SdkError> {
    let backups = engine.machine().backup_machine();
    let public: MegolmV1BackupKey = key.megolm_v1_public_key();
    public.set_version(version.to_string());
    backups
        .save_decryption_key(Some(key), Some(version.to_string()))
        .await
        .map_err(crypto_store_error)?;
    backups
        .enable_backup_v1(public)
        .await
        .map_err(crypto_store_error)
}

/// 把還沒上傳的房間金鑰都傳上去（一批一批，直到上游說沒有了）。上傳沒開就什麼都不送。
///
/// Return:
///     Ok(CaughtUp { batches })        傳完了；上傳沒開是 `batches: 0`
///     Ok(StoppedAtLimit { batches })  繞滿上限還有沒傳的
///     Err(Server)     server 拒（例：版本被別台刪了是 404）：這一批留著，下次再傳
///     Err(Protocol)   crypto store 壞了、回應解不開
pub async fn upload_room_keys<C: PackChannel>(
    engine: &OlmEngine,
    client: &mut WbfClient<C>,
) -> Result<UploadOutcome, SdkError> {
    let _one_at_a_time = engine.backup_upload.lock().await;
    let machine = engine.machine();
    let mut batches = 0usize;
    for _ in 0..UPLOAD_ROUNDS_LIMIT {
        let Some((request_id, request)) = machine
            .backup_machine()
            .backup()
            .await
            .map_err(crypto_store_error)?
        else {
            return Ok(UploadOutcome::CaughtUp { batches });
        };
        let mut variables = Map::new();
        variables.insert("version".into(), json!(request.version));
        let body = json!({ "rooms": request.rooms });
        let reply = client
            .call_matrix_endpoint(&matrix_endpoint::ADD_BACKUP_KEYS, &variables, Some(&body))
            .await?;
        let etag = reply
            .get("etag")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let count = reply
            .get("count")
            .and_then(Value::as_u64)
            .and_then(UInt::new)
            .unwrap_or_default();
        let response = add_backup_keys::v3::Response::new(etag, count);
        machine
            .mark_request_as_sent(&request_id, AnyIncomingResponse::KeysBackup(&response))
            .await
            .map_err(|error| SdkError::Protocol(format!("crypto: {error}")))?;
        batches += 1;
    }
    Ok(UploadOutcome::StoppedAtLimit { batches })
}

/// 用解密金鑰把 server 那份備份整份拉下來、解開、匯進 crypto store。對上 server 的版本就**先**把金鑰存起來、開上傳，
/// 再下載：下載失敗時金鑰已經在 store 裡，不會跟著丟。
/// 單把解不開的跳過（跟上游一樣）；**金鑰對不上 server 現在的版本就什麼都不存、不匯**（fail closed）。
///
/// Args:
///     key: 備份的解密金鑰（recovery 打開的、或自我驗證時別台送來的）
/// Return:
///     Ok(RestoreReport)
///     Ok(...) 的 `total` 是 0：備份是空的
///     Err(Usage)      server 上沒有備份、或這把金鑰不是它的
///     Err(Server)     下載被拒（整份超過 2 MiB 是 `TooLarge`）：金鑰已存、上傳已開
///     Err(Protocol)   crypto store 寫不進去、回應解不開
pub async fn restore_room_keys<C: PackChannel>(
    engine: &OlmEngine,
    client: &mut WbfClient<C>,
    key: BackupDecryptionKey,
) -> Result<RestoreReport, SdkError> {
    let Some((version, info)) = find_latest_backup(client).await? else {
        return Err(SdkError::Usage(
            "there is no backup on the server to restore from".into(),
        ));
    };
    if !key.backup_key_matches(&info) {
        return Err(SdkError::Usage(format!(
            "this key does not open the server's backup (version {version})"
        )));
    }
    enable_with(engine, key.clone(), &version).await?;
    download_and_import(engine, client, &key, &version).await
}

/// 把 `version` 那份備份整份拉下來、用 `key` 解開、匯進 crypto store。呼叫者已經對過 `key` 是這個版本的。
async fn download_and_import<C: PackChannel>(
    engine: &OlmEngine,
    client: &mut WbfClient<C>,
    key: &BackupDecryptionKey,
    version: &str,
) -> Result<RestoreReport, SdkError> {
    let mut variables = Map::new();
    variables.insert("version".into(), json!(version));
    let body = client
        .call_matrix_endpoint(&matrix_endpoint::GET_BACKUP_KEYS, &variables, None)
        .await?;
    let rooms: BTreeMap<OwnedRoomId, RoomKeyBackup> =
        serde_json::from_value(body.get("rooms").cloned().unwrap_or_else(|| json!({})))
            .map_err(|error| SdkError::Protocol(format!("GetBackupKeys: {error}")))?;
    let mut decrypted = Vec::new();
    for (room_id, room) in rooms {
        for (session_id, data) in room.sessions {
            let Ok(data) = data.deserialize() else {
                continue;
            };
            let Ok(room_key) = key.decrypt_session_data(data.session_data) else {
                continue;
            };
            decrypted.push(ExportedRoomKey::from_backed_up_room_key(
                room_id.clone(),
                session_id,
                room_key,
            ));
        }
    }
    let result = engine
        .machine()
        .store()
        .import_room_keys(decrypted, Some(version), |_, _| {})
        .await
        .map_err(crypto_store_error)?;
    let sessions = result
        .keys
        .iter()
        .flat_map(|(room, by_sender)| {
            by_sender
                .values()
                .flatten()
                .map(move |session| (room.to_string(), session.clone()))
        })
        .collect();
    Ok(RestoreReport {
        imported: result.imported_count,
        total: result.total_count,
        sessions,
    })
}

/// 自我驗證完，別台送來的備份解密金鑰落在上游的秘密收件匣（它**不**自動匯）：拿出來、對過 server 現在的版本才用，
/// 用了就整份還原（/docs/design/keys/cross-signing-and-verification.md §5.2）。收件匣沒有就什麼都不做。
/// 收件匣在金鑰**存進 store 之後**才清：問不到 server、存不進去時留著，下一批 to-device 匯完再試。
/// 對不上的照樣清掉（不是這份備份的金鑰，留著也沒用）。
///
/// Return:
///     Ok(Some(RestoreReport))  拿到、對上、還原了
///     Ok(None)                 收件匣沒有、或對不上 server 的備份
///     Err(...)                 同 [`restore_room_keys`]；下載那步失敗時金鑰已存、收件匣已清，要再拉一次是 `backup.restore`（要 recovery key）
pub async fn adopt_backup_key_from_inbox<C: PackChannel>(
    engine: &OlmEngine,
    client: &mut WbfClient<C>,
) -> Result<Option<RestoreReport>, SdkError> {
    let store = engine.machine().store();
    let secrets = store
        .get_secrets_from_inbox(&SecretName::RecoveryKey)
        .await
        .map_err(crypto_store_error)?;
    if secrets.is_empty() {
        return Ok(None);
    }
    let Some((version, info)) = find_latest_backup(client).await? else {
        return Ok(None);
    };
    let matching = secrets.iter().find_map(|secret| {
        BackupDecryptionKey::from_base64(secret)
            .ok()
            .filter(|key| key.backup_key_matches(&info))
    });
    if let Some(key) = &matching {
        enable_with(engine, key.clone(), &version).await?;
    }
    store
        .delete_secrets_from_inbox(&SecretName::RecoveryKey)
        .await
        .map_err(crypto_store_error)?;
    match matching {
        Some(key) => Ok(Some(
            download_and_import(engine, client, &key, &version).await?,
        )),
        None => Ok(None),
    }
}

/// 這台存著的備份解密金鑰（base64，放進 secret storage 用）。
///
/// Return:
///     Ok(Some(Zeroizing<String>))
///     Ok(None)        這台沒有
pub async fn find_backup_secret(engine: &OlmEngine) -> Result<Option<Zeroizing<String>>, SdkError> {
    Ok(engine
        .machine()
        .backup_machine()
        .get_backup_keys()
        .await
        .map_err(crypto_store_error)?
        .decryption_key
        .map(|key| Zeroizing::new(key.to_base64())))
}

/// 把 crypto store 裡**全部**房間金鑰倒成本地快照（跟一般帳號同一個格式、同一個檔，/docs/design/keys/room-key-backup.md §4）。
/// 先寫暫存檔再 rename：要嘛舊的完整、要嘛新的完整。
///
/// Args:
///     path: example: "<account dir>/k/snapshot"
///     temp_path: example: "<account dir>/k/snapshot.tmp"
///     passphrase: vault 第五把子金鑰的 base64
/// Return:
///     Ok(u64)   快照的 byte 數
pub async fn save_snapshot(
    engine: &OlmEngine,
    path: &Path,
    temp_path: &Path,
    passphrase: &str,
) -> Result<u64, SdkError> {
    if let Some(parent) = path.parent() {
        crate::room_keys::prepare_dir(parent)?;
    }
    let keys = engine
        .machine()
        .store()
        .export_room_keys(|_| true)
        .await
        .map_err(crypto_store_error)?;
    let encrypted = matrix_sdk_crypto::encrypt_room_key_export(&keys, passphrase, SNAPSHOT_ROUNDS)
        .map_err(|error| crate::error::cannot_serialize("room key export", error))?;
    std::fs::write(temp_path, encrypted.as_bytes())?;
    crate::room_keys::set_snapshot_permissions(temp_path)?;
    std::fs::rename(temp_path, path)?;
    Ok(std::fs::metadata(path)?.len())
}

/// 把本地快照餵回 crypto store。
///
/// Return:
///     Ok((imported, total))
///     Err(Usage)   沒有快照、解不開、格式壞掉
pub async fn import_snapshot(
    engine: &OlmEngine,
    path: &Path,
    passphrase: &str,
) -> Result<(usize, usize), SdkError> {
    let file = std::fs::File::open(path).map_err(|error| {
        SdkError::Usage(format!(
            "there is no room key snapshot at {}: {error}",
            path.display()
        ))
    })?;
    let keys = matrix_sdk_crypto::decrypt_room_key_export(file, passphrase)
        .map_err(|error| SdkError::Usage(format!("cannot open the room key snapshot: {error}")))?;
    let result = engine
        .machine()
        .store()
        .import_exported_room_keys(keys, |_, _| {})
        .await
        .map_err(crypto_store_error)?;
    Ok((result.imported_count, result.total_count))
}

fn crypto_store_error(error: impl std::fmt::Display) -> SdkError {
    SdkError::Protocol(format!("crypto store: {error}"))
}

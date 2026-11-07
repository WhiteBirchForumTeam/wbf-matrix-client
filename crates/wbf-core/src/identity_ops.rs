//! 金鑰線上的身分與備份（/docs/design/keys/cross-signing-and-verification.md §3、§4.1、§5.2、§7）：
//! 開線時交叉簽章與備份版本就位；每匯進一批 to-device 之後，把狀態機排好的請求送掉（回應驗證、回秘密、轉金鑰給自己的裝置）、
//! 別台送來的備份金鑰拿來還原、新的房間金鑰上傳到備份。全部只講一聲、**不**讓金鑰線失敗：收金鑰比這些重要。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use wbf_sdk::channel::PackChannel;
use wbf_sdk::client::WbfClient;
use wbf_sdk::cross_signing::{self, CrossSigningOutcome};
use wbf_sdk::crypto_engine::{ImportReport, OlmEngine};
use wbf_sdk::key_backup::{self, BackupSetup, UploadOutcome};

use crate::event::EventSink;
use crate::room_crypto::{decrypt_stored, StoredToDecrypt};
use crate::server_cache::ServerCache;

/// conf 的 `SERVER_BACKUP`，給背景用（RPC 有自己的 `Target::server_backup`）。daemon 起來與 `daemon.reload_conf` 時設；core 不讀 conf。
#[derive(Debug, Default)]
pub(crate) struct BackgroundBackup(AtomicBool);

impl BackgroundBackup {
    pub(crate) fn set(&self, on: bool) {
        self.0.store(on, Ordering::SeqCst);
    }

    pub(crate) fn is_on(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// 開線時：交叉簽章（server 上沒有就建）→ 備份（`SERVER_BACKUP` 開著時：server 上沒有就建、有而這台有金鑰就接上）→ 把還沒傳的傳上去。
///
/// Args:
///     client: 金鑰線
///     label: 給人看的帳號, example: "@alice:localhost"
pub(crate) async fn set_up_identity_and_backup<C: PackChannel>(
    engine: &OlmEngine,
    client: &mut WbfClient<C>,
    events: &EventSink,
    label: &str,
    server_backup: bool,
) {
    match cross_signing::ensure_cross_signing(engine, client).await {
        Ok(CrossSigningOutcome::Created) => {
            events.progress(format!("keys: set up cross-signing for {label}"))
        }
        Ok(CrossSigningOutcome::OwnedElsewhere) => events.progress(format!(
            "keys: {label} has cross-signing from another device; verify this device to trust it"
        )),
        Ok(CrossSigningOutcome::AlreadySetUp) => {}
        Err(error) => events.progress(format!(
            "keys: cross-signing for {label} is not set up (tried again when the keys line reopens): {error}"
        )),
    }
    if !server_backup {
        return;
    }
    match key_backup::ensure_backup(engine, client, true).await {
        Ok(setup) => {
            if let BackupSetup::Created { version } = &setup {
                events.progress(format!(
                    "keys: created server backup version {version} for {label}"
                ));
            }
            if setup.find_enabled_version().is_some() {
                upload_quietly(engine, client, events, label).await;
            }
        }
        Err(error) => events.progress(format!(
            "keys: the server backup for {label} is not set up (tried again when the keys line reopens): {error}"
        )),
    }
}

/// 每匯完一批（推來的一包、或拉到追平的幾窗）之後：送掉狀態機排的請求 → 別台送來的備份金鑰拿來還原 → 新金鑰上傳。
/// 有人要跟這台驗證的流程由呼叫者另外看（[`crate::verify_ops`]）。
pub(crate) async fn after_import<C: PackChannel>(
    engine: &OlmEngine,
    client: &mut WbfClient<C>,
    cache: &Arc<ServerCache>,
    events: &EventSink,
    me: &str,
    reports: &[ImportReport],
    server_backup: bool,
) {
    // 驗證的回應、別台要的秘密、自己的裝置要的房間金鑰：狀態機收的時候就排好了，沒人送就永遠停在那裡。
    if let Err(error) = engine.send_outgoing_requests(client).await {
        events.progress(format!(
            "keys: sending what the crypto state machine queued failed (sent with the next batch): {error}"
        ));
    }
    match key_backup::adopt_backup_key_from_inbox(engine, client).await {
        Ok(Some(report)) => {
            events.progress(format!(
                "keys: restored {} of {} room key(s) from the server backup with the key another device sent",
                report.imported, report.total
            ));
            decrypt_restored(engine, cache, events, me, &report.sessions).await;
        }
        Ok(None) => {}
        Err(error) => events.progress(format!(
            "keys: the backup key another device sent could not be used: {error}"
        )),
    }
    let has_new_keys = reports.iter().any(|report| !report.room_keys.is_empty());
    if server_backup && has_new_keys {
        upload_quietly(engine, client, events, me).await;
    }
}

/// 從備份還原進來的每一把：去 cache 找用它加密、還沒解的訊息解開、發 `room.message`（跟金鑰到了同一條路）。
pub(crate) async fn decrypt_restored(
    engine: &OlmEngine,
    cache: &Arc<ServerCache>,
    events: &EventSink,
    me: &str,
    sessions: &[(String, String)],
) {
    for (room, session) in sessions {
        let which = StoredToDecrypt::Session(session.clone());
        if let Err(error) = decrypt_stored(engine, cache, events, me, room, which, true).await {
            events.progress(format!(
                "keys: a restored room key for {room} arrived, but decrypting the stored messages failed: {error}"
            ));
        }
    }
}

/// 上傳還沒進備份的房間金鑰；失敗只講一聲（上游留著那一批，下次再傳）。
pub(crate) async fn upload_quietly<C: PackChannel>(
    engine: &OlmEngine,
    client: &mut WbfClient<C>,
    events: &EventSink,
    label: &str,
) {
    match key_backup::upload_room_keys(engine, client).await {
        Ok(UploadOutcome::CaughtUp { .. }) => {}
        Ok(UploadOutcome::StoppedAtLimit { batches }) => events.progress(format!(
            "keys: uploaded {batches} batch(es) of room keys to the server backup for {label}; more are left (continued with the next new key, or backup.upload)"
        )),
        Err(error) => events.progress(format!(
            "keys: uploading room keys to the server backup for {label} failed (tried again with the next new key): {error}"
        )),
    }
}

#[cfg(test)]
mod tests {
    //! /docs/design/keys/cross-signing-and-verification.md §11.2 的 core 假 server 那一列：交叉簽章與備份就位的每個分支、
    //! 簽章的 `failures`、secret storage 建了讀得回、錯的 recovery key 什麼都不匯。真 server 那組（§11.1）在 daemon 的 `real_server.rs`。

    use std::sync::{Arc, Mutex};

    use serde_json::{json, Value};
    use wbf_sdk::cross_signing::{self, CrossSigningOutcome};
    use wbf_sdk::key_backup::{self, BackupSetup};
    use wbf_sdk::matrix_endpoint as endpoints;
    use wbf_sdk::protocol::{BridgedEndpoint, BRIDGE_SIGNATURES_UPLOAD};
    use wbf_sdk::secret_storage;
    use wbf_wire::Kind;

    use super::*;
    use crate::test_support::{
        core_with_wbf_account, memory_client_with_hello, scratch, FakeServer, ME,
    };

    fn position_of(fake: &FakeServer, endpoint: BridgedEndpoint) -> Option<usize> {
        fake.bridge_calls
            .lock()
            .unwrap()
            .iter()
            .position(|call| *call == (endpoint.kind, endpoint.subtype))
    }

    /// 同一個人在另一台裝置上建好的交叉簽章公鑰（`SigningKeysUpload` 的 body）：造「server 上已經有」。
    async fn signing_keys_of_another_device(dir: &std::path::Path) -> Value {
        let other = OlmEngine::open(
            &dir.join("other-m"),
            &wbf_sdk::Key32([9u8; 32]),
            ME,
            "OTHER",
        )
        .await
        .unwrap();
        let requests = other
            .machine()
            .bootstrap_cross_signing(false)
            .await
            .unwrap();
        let keys = &requests.upload_signing_keys_req;
        let to_json = |key: &Option<matrix_sdk_crypto::types::CrossSigningKey>| {
            serde_json::to_value(key.as_ref().unwrap().to_raw::<Value>()).unwrap()
        };
        json!({
            "master_key": to_json(&keys.master_key),
            "self_signing_key": to_json(&keys.self_signing_key),
            "user_signing_key": to_json(&keys.user_signing_key),
        })
    }

    #[tokio::test]
    async fn a_first_device_uploads_its_cross_signing_keys_before_the_signatures() {
        let dir = scratch("xsign-first");
        let (core, account) = core_with_wbf_account(&dir).await;
        let engine = core.olm_engine_of(&account).await.unwrap();
        let (mut client, fake) = memory_client_with_hello(Arc::new(Mutex::new(Vec::new()))).await;

        let outcome = cross_signing::ensure_cross_signing(&engine, &mut client)
            .await
            .unwrap();

        assert_eq!(outcome, CrossSigningOutcome::Created);
        let keys =
            position_of(&fake, endpoints::SIGNING_KEYS_UPLOAD.bridge).expect("SigningKeysUpload");
        let signatures = position_of(&fake, BRIDGE_SIGNATURES_UPLOAD).expect("SignaturesUpload");
        assert!(
            keys < signatures,
            "the keys go up before the signatures over them"
        );
        let uploaded = fake.rooms.lock().unwrap().signing_keys.clone().unwrap();
        for name in ["master_key", "self_signing_key", "user_signing_key"] {
            assert_eq!(uploaded[name]["user_id"], ME, "{name}: {uploaded}");
        }
        assert!(engine.machine().cross_signing_status().await.is_complete());
        // 再叫一次：server 上已經有、這台也有私鑰 → 什麼都不送。
        let calls = fake.bridge_calls.lock().unwrap().len();
        let again = cross_signing::ensure_cross_signing(&engine, &mut client)
            .await
            .unwrap();
        assert_eq!(again, CrossSigningOutcome::AlreadySetUp);
        let uploads_after = fake.bridge_calls.lock().unwrap()[calls..]
            .iter()
            .filter(|call| **call == (Kind::Keys, endpoints::SIGNING_KEYS_UPLOAD.bridge.subtype))
            .count();
        assert_eq!(uploads_after, 0, "nothing is uploaded twice");
    }

    #[tokio::test]
    async fn a_device_that_finds_cross_signing_on_the_server_creates_nothing() {
        let dir = scratch("xsign-elsewhere");
        let (core, account) = core_with_wbf_account(&dir).await;
        let engine = core.olm_engine_of(&account).await.unwrap();
        let (mut client, fake) = memory_client_with_hello(Arc::new(Mutex::new(Vec::new()))).await;
        fake.rooms.lock().unwrap().signing_keys = Some(signing_keys_of_another_device(&dir).await);

        let outcome = cross_signing::ensure_cross_signing(&engine, &mut client)
            .await
            .unwrap();

        assert_eq!(outcome, CrossSigningOutcome::OwnedElsewhere);
        assert_eq!(
            position_of(&fake, endpoints::SIGNING_KEYS_UPLOAD.bridge),
            None
        );
        assert_eq!(position_of(&fake, BRIDGE_SIGNATURES_UPLOAD), None);
        assert!(!engine.machine().cross_signing_status().await.has_master);
    }

    #[tokio::test]
    async fn signatures_the_server_did_not_accept_are_an_error_even_with_200() {
        let dir = scratch("xsign-failures");
        let (core, account) = core_with_wbf_account(&dir).await;
        let engine = core.olm_engine_of(&account).await.unwrap();
        let (mut client, fake) = memory_client_with_hello(Arc::new(Mutex::new(Vec::new()))).await;
        fake.rooms.lock().unwrap().signature_failures = Some(json!({
            ME: { "DEV": { "errcode": "M_INVALID_SIGNATURE", "error": "bad" } }
        }));

        let error = cross_signing::ensure_cross_signing(&engine, &mut client)
            .await
            .expect_err("failures are not success");

        match error {
            wbf_sdk::SdkError::Server { code, meta, .. } => {
                assert_eq!(code, "SignatureFailures");
                assert_eq!(meta[ME]["DEV"]["errcode"], "M_INVALID_SIGNATURE", "{meta}");
            }
            other => panic!("expected the server's failures, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn the_keys_line_creates_a_backup_only_when_server_backup_is_on() {
        for server_backup in [false, true] {
            let dir = scratch(&format!("backup-{server_backup}"));
            let (core, account) = core_with_wbf_account(&dir).await;
            let engine = core.olm_engine_of(&account).await.unwrap();
            let (mut client, fake) =
                memory_client_with_hello(Arc::new(Mutex::new(Vec::new()))).await;

            set_up_identity_and_backup(&engine, &mut client, &core.events, ME, server_backup).await;

            let asked = position_of(&fake, endpoints::LATEST_BACKUP_INFO.bridge).is_some();
            let created = position_of(&fake, endpoints::CREATE_BACKUP_VERSION.bridge).is_some();
            assert_eq!(
                (asked, created),
                (server_backup, server_backup),
                "SERVER_BACKUP={server_backup}"
            );
            assert_eq!(
                engine.machine().backup_machine().enabled().await,
                server_backup,
                "uploading is on exactly when the backup was set up"
            );
        }
    }

    #[tokio::test]
    async fn a_backup_this_device_cannot_open_is_neither_replaced_nor_written() {
        let dir = scratch("backup-foreign");
        let (core, account) = core_with_wbf_account(&dir).await;
        let engine = core.olm_engine_of(&account).await.unwrap();
        let (mut client, fake) = memory_client_with_hello(Arc::new(Mutex::new(Vec::new()))).await;
        let mut foreign = serde_json::to_value(
            matrix_sdk_crypto::store::types::BackupDecryptionKey::new().to_backup_info(),
        )
        .unwrap();
        foreign["version"] = json!("7");
        fake.rooms.lock().unwrap().backup = Some(foreign);

        let setup = key_backup::ensure_backup(&engine, &mut client, true)
            .await
            .unwrap();
        let batches = key_backup::upload_room_keys(&engine, &mut client)
            .await
            .unwrap();

        assert_eq!(
            setup,
            BackupSetup::NoKeyHere {
                version: "7".into()
            }
        );
        assert_eq!(
            position_of(&fake, endpoints::CREATE_BACKUP_VERSION.bridge),
            None
        );
        assert_eq!(batches, UploadOutcome::CaughtUp { batches: 0 });
        assert_eq!(position_of(&fake, endpoints::ADD_BACKUP_KEYS.bridge), None);
    }

    /// rumia／oliver（PR #84）：server 的版本被別台換掉之後，這台**不**再往舊版本傳。
    #[tokio::test]
    async fn a_backup_replaced_elsewhere_stops_this_device_uploading_to_the_old_one() {
        let dir = scratch("backup-replaced");
        let (core, account) = core_with_wbf_account(&dir).await;
        let engine = core.olm_engine_of(&account).await.unwrap();
        let (mut client, fake) = memory_client_with_hello(Arc::new(Mutex::new(Vec::new()))).await;
        let created = key_backup::ensure_backup(&engine, &mut client, true)
            .await
            .unwrap();
        assert_eq!(
            created,
            BackupSetup::Created {
                version: "1".into()
            }
        );
        assert!(engine.machine().backup_machine().enabled().await);

        let mut replaced = serde_json::to_value(
            matrix_sdk_crypto::store::types::BackupDecryptionKey::new().to_backup_info(),
        )
        .unwrap();
        replaced["version"] = json!("2");
        fake.rooms.lock().unwrap().backup = Some(replaced);
        let setup = key_backup::ensure_backup(&engine, &mut client, true)
            .await
            .unwrap();

        assert_eq!(
            setup,
            BackupSetup::NoKeyHere {
                version: "2".into()
            }
        );
        assert!(
            !engine.machine().backup_machine().enabled().await,
            "uploading to version 1 stopped"
        );
    }

    /// rumia／oliver（PR #84）：對上了、下載失敗（真 server 是 2 MiB 的 `TooLarge`）→ 金鑰已經存著、上傳開著，**不**跟著丟。
    #[tokio::test]
    async fn a_restore_whose_download_fails_keeps_the_backup_key() {
        let dir = scratch("restore-download-fails");
        let (core, account) = core_with_wbf_account(&dir).await;
        let engine = core.olm_engine_of(&account).await.unwrap();
        let (mut client, fake) = memory_client_with_hello(Arc::new(Mutex::new(Vec::new()))).await;
        key_backup::ensure_backup(&engine, &mut client, true)
            .await
            .unwrap();
        let secret = key_backup::find_backup_secret(&engine)
            .await
            .unwrap()
            .expect("the creating device holds the key");
        let key =
            matrix_sdk_crypto::store::types::BackupDecryptionKey::from_base64(&secret).unwrap();

        let other_dir = scratch("restore-download-fails-other");
        let (other_core, other_account) = core_with_wbf_account(&other_dir).await;
        let other = other_core.olm_engine_of(&other_account).await.unwrap();
        let (mut other_client, other_fake) =
            memory_client_with_hello(Arc::new(Mutex::new(Vec::new()))).await;
        {
            let mut target = other_fake.rooms.lock().unwrap();
            target.backup = fake.rooms.lock().unwrap().backup.clone();
            target.backup_download_fails = true;
        }

        let result = key_backup::restore_room_keys(&other, &mut other_client, key).await;

        assert!(result.is_err(), "the download failed: {result:?}");
        assert!(
            key_backup::find_backup_secret(&other)
                .await
                .unwrap()
                .is_some(),
            "the key is kept"
        );
        assert!(other.machine().backup_machine().enabled().await);
    }

    /// rumia（PR #84）：第二次建 recovery key 被拒，第一串照樣打得開。
    #[tokio::test]
    async fn a_second_recovery_key_is_refused_and_the_first_still_opens() {
        let dir = scratch("ssss-twice");
        let (core, account) = core_with_wbf_account(&dir).await;
        let engine = core.olm_engine_of(&account).await.unwrap();
        let (mut client, fake) = memory_client_with_hello(Arc::new(Mutex::new(Vec::new()))).await;
        set_up_identity_and_backup(&engine, &mut client, &core.events, ME, true).await;
        let first = secret_storage::create_secret_storage(&engine, &mut client)
            .await
            .unwrap();
        let written = fake.rooms.lock().unwrap().account_data.clone();

        let second = secret_storage::create_secret_storage(&engine, &mut client).await;

        assert!(
            matches!(second, Err(wbf_sdk::SdkError::Usage(_))),
            "{second:?}"
        );
        assert_eq!(
            fake.rooms.lock().unwrap().account_data,
            written,
            "nothing on the server changed"
        );
        assert!(
            secret_storage::restore_secrets(&engine, &mut client, &first)
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn the_secret_storage_opens_with_its_recovery_key_and_with_nothing_else() {
        let dir = scratch("ssss");
        let (core, account) = core_with_wbf_account(&dir).await;
        let engine = core.olm_engine_of(&account).await.unwrap();
        let (mut client, fake) = memory_client_with_hello(Arc::new(Mutex::new(Vec::new()))).await;
        assert!(!secret_storage::has_secret_storage(&engine, &mut client)
            .await
            .unwrap());
        set_up_identity_and_backup(&engine, &mut client, &core.events, ME, true).await;

        let recovery_key = secret_storage::create_secret_storage(&engine, &mut client)
            .await
            .unwrap();

        let written = fake.rooms.lock().unwrap().account_data.clone();
        let key_id = written["m.secret_storage.default_key"]["key"]
            .as_str()
            .unwrap()
            .to_string();
        for secret in [
            "m.cross_signing.master",
            "m.cross_signing.self_signing",
            "m.cross_signing.user_signing",
            "m.megolm_backup.v1",
        ] {
            assert!(
                written[secret]["encrypted"][&key_id].is_object(),
                "{secret}: {written:?}"
            );
        }

        // 同一個人的另一台裝置、同一份 server 狀態。
        let other_dir = scratch("ssss-other");
        let (other_core, other_account) = core_with_wbf_account(&other_dir).await;
        let other = other_core.olm_engine_of(&other_account).await.unwrap();
        let (mut other_client, other_fake) =
            memory_client_with_hello(Arc::new(Mutex::new(Vec::new()))).await;
        {
            let source = fake.rooms.lock().unwrap();
            let mut target = other_fake.rooms.lock().unwrap();
            target.account_data = source.account_data.clone();
            target.signing_keys = source.signing_keys.clone();
        }

        let wrong = matrix_sdk_crypto::secret_storage::SecretStorageKey::new().to_base58();
        let refused = secret_storage::restore_secrets(&other, &mut other_client, &wrong).await;
        assert!(
            matches!(refused, Err(wbf_sdk::SdkError::Usage(_))),
            "a wrong recovery key is refused"
        );
        assert!(
            !other.machine().cross_signing_status().await.has_master,
            "nothing was imported with the wrong key"
        );

        let restored = secret_storage::restore_secrets(&other, &mut other_client, &recovery_key)
            .await
            .unwrap();
        assert!(other.machine().cross_signing_status().await.is_complete());
        let backup_key = restored
            .backup_key
            .expect("the backup key was in the secret storage");
        let info: matrix_sdk_crypto::types::RoomKeyBackupInfo =
            serde_json::from_value(fake.rooms.lock().unwrap().backup.clone().unwrap()).unwrap();
        assert!(
            backup_key.backup_key_matches(&info),
            "it is the key of the server's backup"
        );
    }
}

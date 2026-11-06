//! secret storage（SSSS）：把交叉簽章私鑰與備份解密金鑰用 recovery key 加密、放進 account data；換機器時用同一串 recovery key 拿回來
//! （/docs/design/keys/cross-signing-and-verification.md §4.2）。加解密是上游的 `SecretStorageKey`；
//! 「哪個秘密放哪個 account data、預設用哪把 key」那層 matrix-sdk 綁在 `Client` 上，這裡照它的格式自己寫：
//!
//! | account data | 內容 |
//! |---|---|
//! | `m.secret_storage.key.<id>` | 那把 key 的描述（演算法、MAC 檢查用的 iv／mac） |
//! | `m.secret_storage.default_key` | `{"key": "<id>"}` |
//! | `m.cross_signing.master`／`self_signing`／`user_signing`、`m.megolm_backup.v1` | `{"encrypted": {"<id>": {iv, ciphertext, mac}}}` |

use matrix_sdk::ruma;
use matrix_sdk_crypto::secret_storage::SecretStorageKey;
use matrix_sdk_crypto::store::types::{BackupDecryptionKey, CrossSigningKeyExport};
use ruma::events::secret::request::SecretName;
use ruma::events::secret_storage::key::SecretStorageKeyEventContent;
use ruma::events::EventContentFromType as _;
use serde_json::{json, Map, Value};
use zeroize::Zeroizing;

use crate::channel::PackChannel;
use crate::client::WbfClient;
use crate::crypto_engine::OlmEngine;
use crate::matrix_endpoint;
use crate::SdkError;

const DEFAULT_KEY_EVENT: &str = "m.secret_storage.default_key";

/// 交叉簽章三把私鑰的秘密名字，照 [`CrossSigningKeyExport`] 的欄位順序。
const CROSS_SIGNING_SECRETS: [SecretName; 3] = [
    SecretName::CrossSigningMasterKey,
    SecretName::CrossSigningSelfSigningKey,
    SecretName::CrossSigningUserSigningKey,
];

/// 從 secret storage 拿回來的東西。
pub struct RestoredSecrets {
    /// 備份解密金鑰；secret storage 裡沒放就是 None
    pub backup_key: Option<BackupDecryptionKey>,
}

/// 建一把新的 secret storage key，把這台的交叉簽章私鑰（與備份解密金鑰，有的話）放進去，設成預設。
/// 預設那一筆**最後**才寫：讀的人看到預設 key 時，秘密一定已經在了。
///
/// Return:
///     Ok(Zeroizing<String>)  recovery key（base58，四個字一組）——秘密，只回這一次
///     Err(Usage)             這台沒有三把交叉簽章私鑰（先自我驗證、或用 recovery key 還原）
///     Err(Server)／Err(Protocol)  account data 寫不進去
pub async fn create_secret_storage<C: PackChannel>(
    engine: &OlmEngine,
    client: &mut WbfClient<C>,
) -> Result<Zeroizing<String>, SdkError> {
    let machine = engine.machine();
    let export = machine
        .export_cross_signing_keys()
        .await
        .map_err(crypto_store_error)?
        .unwrap_or_default();
    // 匯出的型別在 Drop 時清記憶體、不讓搬出來：拷一份進同樣會清的 `Zeroizing`。
    let cross_signing = [
        export.master_key.clone().map(Zeroizing::new),
        export.self_signing_key.clone().map(Zeroizing::new),
        export.user_signing_key.clone().map(Zeroizing::new),
    ];
    if cross_signing.iter().any(Option::is_none) {
        return Err(SdkError::Usage(
            "this device does not hold the cross-signing private keys; verify it with another device, \
             or restore with the recovery key, before creating a new one"
                .into(),
        ));
    }
    let me = machine.user_id().to_string();
    let key = SecretStorageKey::new();
    let key_content = serde_json::to_value(key.event_content())
        .map_err(|error| crate::error::cannot_serialize("secret storage key", error))?;
    set_account_data(client, &me, &key.event_type().to_string(), &key_content).await?;
    for (name, secret) in CROSS_SIGNING_SECRETS
        .iter()
        .zip(cross_signing.iter().flatten())
    {
        put_secret(client, &me, &key, name, secret).await?;
    }
    if let Some(backup) = crate::key_backup::find_backup_secret(engine).await? {
        put_secret(client, &me, &key, &SecretName::RecoveryKey, &backup).await?;
    }
    set_account_data(
        client,
        &me,
        DEFAULT_KEY_EVENT,
        &json!({ "key": key.key_id() }),
    )
    .await?;
    Ok(Zeroizing::new(key.to_base58()))
}

/// 用 recovery key 打開 secret storage，把交叉簽章私鑰匯進這台（之後由呼叫者簽自己這台）。
/// ⚠️ fail closed：recovery key 打不開（MAC 對不上）、或三把私鑰缺任何一把，就什麼都**不**匯。
///
/// Args:
///     recovery_key: base58 的那串（有沒有空白都可以）
/// Return:
///     Ok(RestoredSecrets)
///     Err(Usage)      server 上沒有 secret storage、recovery key 不對、缺私鑰、私鑰跟 server 上的公鑰對不上
///     Err(Server)／Err(Protocol)  讀不到 account data、crypto store 寫不進去
pub async fn restore_secrets<C: PackChannel>(
    engine: &OlmEngine,
    client: &mut WbfClient<C>,
    recovery_key: &str,
) -> Result<RestoredSecrets, SdkError> {
    let machine = engine.machine();
    let me = machine.user_id().to_string();
    let key = open_default_key(client, &me, recovery_key).await?;
    let mut secrets = Vec::with_capacity(CROSS_SIGNING_SECRETS.len());
    for name in &CROSS_SIGNING_SECRETS {
        let secret = get_secret(client, &me, &key, name).await?.ok_or_else(|| {
            SdkError::Usage(format!(
                "the secret storage has no {}; nothing was imported",
                name.as_str()
            ))
        })?;
        secrets.push(secret);
    }
    let backup_key = match get_secret(client, &me, &key, &SecretName::RecoveryKey).await? {
        Some(secret) => Some(BackupDecryptionKey::from_base64(&secret).map_err(|error| {
            SdkError::Usage(format!(
                "the backup key in the secret storage is not valid: {error}"
            ))
        })?),
        None => None,
    };
    let mut export = CrossSigningKeyExport::default();
    let [master, self_signing, user_signing] = <[Zeroizing<String>; 3]>::try_from(secrets)
        .map_err(|_| SdkError::Protocol("expected three cross-signing secrets".into()))?;
    export.master_key = Some(master.to_string());
    export.self_signing_key = Some(self_signing.to_string());
    export.user_signing_key = Some(user_signing.to_string());
    // 上游拿私鑰跟 server 上的公鑰比：先重查自己，比的才是 server 現在的那份。
    engine
        .mark_users_changed(&[machine.user_id().to_string()])
        .await?;
    engine.send_outgoing_requests(client).await?;
    machine
        .import_cross_signing_keys(export)
        .await
        .map_err(|error| {
            SdkError::Usage(format!(
                "the cross-signing keys in the secret storage do not match the server: {error}"
            ))
        })?;
    Ok(RestoredSecrets { backup_key })
}

/// Return:
///     Ok(bool)  server 上有沒有預設的 secret storage key
pub async fn has_secret_storage<C: PackChannel>(
    engine: &OlmEngine,
    client: &mut WbfClient<C>,
) -> Result<bool, SdkError> {
    let me = engine.machine().user_id().to_string();
    Ok(get_account_data(client, &me, DEFAULT_KEY_EVENT)
        .await?
        .is_some())
}

/// 預設的那把 key，用 recovery key 打開（MAC 檢查在上游的 `from_account_data`）。
async fn open_default_key<C: PackChannel>(
    client: &mut WbfClient<C>,
    me: &str,
    recovery_key: &str,
) -> Result<SecretStorageKey, SdkError> {
    let default = get_account_data(client, me, DEFAULT_KEY_EVENT)
        .await?
        .ok_or_else(|| SdkError::Usage("there is no secret storage on the server".into()))?;
    let key_id = default
        .get("key")
        .and_then(Value::as_str)
        .ok_or_else(|| SdkError::Protocol(format!("{DEFAULT_KEY_EVENT} has no key: {default}")))?;
    let event_type = format!("m.secret_storage.key.{key_id}");
    let content = get_account_data(client, me, &event_type)
        .await?
        .ok_or_else(|| {
            SdkError::Usage(format!(
                "the secret storage key {key_id} is missing on the server"
            ))
        })?;
    let raw = serde_json::value::to_raw_value(&content)
        .map_err(|error| crate::error::cannot_serialize("secret storage key", error))?;
    let content = SecretStorageKeyEventContent::from_parts(&event_type, &raw)
        .map_err(|error| SdkError::Protocol(format!("{event_type}: {error}")))?;
    SecretStorageKey::from_account_data(recovery_key, content).map_err(|error| {
        SdkError::Usage(format!(
            "this recovery key does not open the secret storage: {error}"
        ))
    })
}

/// 一個秘密：沒放、或不是用這把 key 加密的都是 None。
async fn get_secret<C: PackChannel>(
    client: &mut WbfClient<C>,
    me: &str,
    key: &SecretStorageKey,
    name: &SecretName,
) -> Result<Option<Zeroizing<String>>, SdkError> {
    let Some(content) = get_account_data(client, me, name.as_str()).await? else {
        return Ok(None);
    };
    let Some(encrypted) = content
        .get("encrypted")
        .and_then(|encrypted| encrypted.get(key.key_id()))
    else {
        return Ok(None);
    };
    let encrypted = serde_json::from_value(encrypted.clone())
        .map_err(|error| SdkError::Protocol(format!("{}: {error}", name.as_str())))?;
    let plaintext = Zeroizing::new(key.decrypt(&encrypted, name).map_err(|error| {
        SdkError::Usage(format!("{} cannot be decrypted: {error}", name.as_str()))
    })?);
    let secret = String::from_utf8(plaintext.to_vec())
        .map_err(|error| SdkError::Protocol(format!("{} is not text: {error}", name.as_str())))?;
    Ok(Some(Zeroizing::new(secret)))
}

/// 放一個秘密：既有的 `encrypted` 裡別把 key 加密的那幾份留著，只換掉這把 key 的那份（matrix-sdk 的 `put_secret` 同一個做法）。
async fn put_secret<C: PackChannel>(
    client: &mut WbfClient<C>,
    me: &str,
    key: &SecretStorageKey,
    name: &SecretName,
    secret: &str,
) -> Result<(), SdkError> {
    let mut encrypted_by_key = get_account_data(client, me, name.as_str())
        .await?
        .and_then(|content| content.get("encrypted").cloned())
        .and_then(|encrypted| match encrypted {
            Value::Object(map) => Some(map),
            _ => None,
        })
        .unwrap_or_default();
    let encrypted = serde_json::to_value(key.encrypt(secret.as_bytes().to_vec(), name))
        .map_err(|error| crate::error::cannot_serialize("encrypted secret", error))?;
    encrypted_by_key.insert(key.key_id().to_string(), encrypted);
    set_account_data(
        client,
        me,
        name.as_str(),
        &json!({ "encrypted": Value::Object(encrypted_by_key) }),
    )
    .await
}

/// Return:
///     Ok(Some(Value))  那筆 account data 的內容
///     Ok(None)         沒寫過（404）
async fn get_account_data<C: PackChannel>(
    client: &mut WbfClient<C>,
    me: &str,
    event_type: &str,
) -> Result<Option<Value>, SdkError> {
    match client
        .call_matrix_endpoint(
            &matrix_endpoint::GET_ACCOUNT_DATA,
            &account_data_variables(me, event_type),
            None,
        )
        .await
    {
        Ok(content) => Ok(Some(content)),
        Err(error) if error.is_not_found() => Ok(None),
        Err(error) => Err(error),
    }
}

async fn set_account_data<C: PackChannel>(
    client: &mut WbfClient<C>,
    me: &str,
    event_type: &str,
    content: &Value,
) -> Result<(), SdkError> {
    client
        .call_matrix_endpoint(
            &matrix_endpoint::SET_ACCOUNT_DATA,
            &account_data_variables(me, event_type),
            Some(content),
        )
        .await
        .map(|_| ())
}

fn account_data_variables(user_id: &str, event_type: &str) -> Map<String, Value> {
    let mut variables = Map::new();
    variables.insert("user_id".into(), json!(user_id));
    variables.insert("event_type".into(), json!(event_type));
    variables
}

fn crypto_store_error(error: impl std::fmt::Display) -> SdkError {
    SdkError::Protocol(format!("crypto store: {error}"))
}

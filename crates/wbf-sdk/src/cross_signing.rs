//! 交叉簽章：這個帳號有沒有、沒有就建（第一台裝置），以及「誰的哪台裝置被信任」（/docs/design/keys/cross-signing-and-verification.md §3、§6）。
//! 狀態機是上游的 `OlmMachine`；這裡只把它要送的請求照順序走橋送出去、把答案交回去。
//! 上游的 `UploadSigningKeysRequest` **不**從 `outgoing_requests()` 出來，所以 `OlmEngine::send_outgoing_requests` 永遠送不到它——只有這裡送。

use matrix_sdk::ruma;
use matrix_sdk_crypto::types::requests::AnyIncomingResponse;
use ruma::api::client::keys::upload_signing_keys;
use ruma::{DeviceId, TransactionId, UserId};
use serde::Serialize;
use serde_json::{Map, Value};

use crate::channel::PackChannel;
use crate::client::WbfClient;
use crate::crypto_engine::OlmEngine;
use crate::matrix_endpoint;
use crate::SdkError;

/// [`ensure_cross_signing`] 做了什麼。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CrossSigningOutcome {
    /// 這台本來就有私鑰、server 上也有：什麼都沒送
    AlreadySetUp,
    /// server 上沒有：建了（或重送上次沒送成的同一份）並上傳
    Created,
    /// server 上有、這台沒有私鑰（別台裝置建的）：**不**建，等自我驗證或 recovery key 把私鑰拿過來
    OwnedElsewhere,
}

/// 一台裝置在「我」眼中被不被信任（`devices.list` 的一列）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DeviceTrust {
    pub device_id: String,
    pub display_name: Option<String>,
    /// 被它主人的交叉簽章簽過、而且那個人被我信任（自己的話：我有自己的私鑰）；或（別台才算）我本地手動信任過。
    /// ⚠️ 這台自己只看交叉簽章：上游把「自己這台」一律標成本地信任，那不代表別人信它。
    pub verified: bool,
    pub is_this_device: bool,
}

/// 一個人的身分與裝置在「我」眼中被不被信任。
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct UserTrust {
    pub user_id: String,
    /// 這個人的交叉簽章身分被我驗證過（自己：這台有自己的 master 私鑰）；沒有身分是 false
    pub identity_verified: bool,
    /// 照 `device_id` 排序
    pub devices: Vec<DeviceTrust>,
}

/// 這個帳號的交叉簽章就位了沒；server 上沒有就建（/docs/design/keys/cross-signing-and-verification.md §3）。
/// 先重查自己（`/keys/query`），答案才是 server 現在的樣子。
///
/// Args:
///     client: 走橋用的連線（金鑰線）
/// Return:
///     Ok(CrossSigningOutcome)
///     Err(Server)    上傳被拒：例如 server 上已經有別的而要密碼（401，換掉既有的才會）、簽章有 `failures`
///     Err(Protocol)  crypto store 寫不進去、回應解不開
pub async fn ensure_cross_signing<C: PackChannel>(
    engine: &OlmEngine,
    client: &mut WbfClient<C>,
) -> Result<CrossSigningOutcome, SdkError> {
    let machine = engine.machine();
    let me = machine.user_id().to_owned();
    engine.mark_users_changed(&[me.to_string()]).await?;
    engine.send_outgoing_requests(client).await?;
    let has_private_master = machine.cross_signing_status().await.has_master;
    let is_on_server = machine
        .get_identity(&me, None)
        .await
        .map_err(crypto_store_error)?
        .is_some();
    match (has_private_master, is_on_server) {
        (true, true) => Ok(CrossSigningOutcome::AlreadySetUp),
        (false, true) => Ok(CrossSigningOutcome::OwnedElsewhere),
        // 這台有私鑰而 server 沒有＝上次建了沒送成：`bootstrap_cross_signing(false)` 拿同一份重送（重送一模一樣的不要密碼）。
        (_, false) => {
            upload_new_identity(engine, client).await?;
            Ok(CrossSigningOutcome::Created)
        }
    }
}

/// 照上游的順序送三個請求：裝置金鑰（有的話）→ 交叉簽章公鑰 → 簽章。
async fn upload_new_identity<C: PackChannel>(
    engine: &OlmEngine,
    client: &mut WbfClient<C>,
) -> Result<(), SdkError> {
    let machine = engine.machine();
    let requests = machine
        .bootstrap_cross_signing(false)
        .await
        .map_err(|error| SdkError::Protocol(format!("cross-signing bootstrap: {error}")))?;
    if let Some(request) = &requests.upload_keys_req {
        engine.send_one(client, request).await?;
    }
    let signing_keys = &requests.upload_signing_keys_req;
    let mut body = Map::new();
    for (name, key) in [
        ("master_key", &signing_keys.master_key),
        ("self_signing_key", &signing_keys.self_signing_key),
        ("user_signing_key", &signing_keys.user_signing_key),
    ] {
        if let Some(key) = key {
            let raw: ruma::serde::Raw<Value> = key.to_raw();
            let value = raw.deserialize().map_err(|error| {
                SdkError::Protocol(format!("cross-signing key {name}: {error}"))
            })?;
            body.insert(name.to_string(), value);
        }
    }
    client
        .call_matrix_endpoint(
            &matrix_endpoint::SIGNING_KEYS_UPLOAD,
            &Map::new(),
            Some(&Value::Object(body)),
        )
        .await?;
    // 交回去：上游這時才把私鑰標成「已經分享」（`mark_as_shared`）。
    let uploaded = upload_signing_keys::v3::Response::new();
    machine
        .mark_request_as_sent(
            &TransactionId::new(),
            AnyIncomingResponse::SigningKeysUpload(&uploaded),
        )
        .await
        .map_err(|error| SdkError::Protocol(format!("crypto: {error}")))?;
    engine
        .send_signature_upload(client, requests.upload_signatures_req)
        .await
}

/// 用這台的 self-signing 私鑰簽自己這台、上傳簽章（從 secret storage 匯進私鑰之後：別人才會信這台）。
/// 已經被自己的交叉簽章簽過就什麼都不送。
///
/// Return:
///     Ok(())
///     Err(Usage)     這台沒有 self-signing 私鑰（沒匯進來就簽不了）
///     Err(Server)    簽章被拒（`failures` 不是空的也算）
///     Err(Protocol)  自己這台不在 crypto store 裡、或 store 壞了
pub async fn sign_this_device<C: PackChannel>(
    engine: &OlmEngine,
    client: &mut WbfClient<C>,
) -> Result<(), SdkError> {
    let machine = engine.machine();
    if !machine.cross_signing_status().await.has_self_signing {
        return Err(SdkError::Usage(
            "this device has no self-signing key to sign itself with".into(),
        ));
    }
    let device = machine
        .get_device(machine.user_id(), machine.device_id(), None)
        .await
        .map_err(crypto_store_error)?
        .ok_or_else(|| SdkError::Protocol("this device is not in its own crypto store".into()))?;
    if device.is_cross_signed_by_owner() {
        return Ok(());
    }
    let upload = device
        .verify()
        .await
        .map_err(|error| SdkError::Protocol(format!("signing this device: {error}")))?;
    engine.send_signature_upload(client, upload).await?;
    // 重查自己：剛上傳的簽章要回到 store 裡的那份裝置上，`devices.list` 才看得到它被簽了。
    engine
        .mark_users_changed(&[machine.user_id().to_string()])
        .await?;
    engine.send_outgoing_requests(client).await?;
    Ok(())
}

/// 這個人的身分與每台已知裝置被不被信任。只讀 crypto store、**不**上網（裝置清單由既有的 `/keys/query` 追蹤更新）。
///
/// Args:
///     user_id: example: "@bob:localhost"
/// Return:
///     Ok(UserTrust)  沒查過這個人就是空的裝置清單、`identity_verified` false
///     Err(Usage)     user_id 不合法
///     Err(Protocol)  crypto store 讀不出來
pub async fn list_devices(engine: &OlmEngine, user_id: &str) -> Result<UserTrust, SdkError> {
    let machine = engine.machine();
    let user = UserId::parse(user_id)
        .map_err(|error| SdkError::Usage(format!("bad user id {user_id:?}: {error}")))?;
    let identity_verified = machine
        .get_identity(&user, None)
        .await
        .map_err(crypto_store_error)?
        .is_some_and(|identity| identity.is_verified());
    let this_device: &DeviceId = machine.device_id();
    let is_me = user == machine.user_id();
    let mut devices: Vec<DeviceTrust> = machine
        .get_user_devices(&user, None)
        .await
        .map_err(crypto_store_error)?
        .devices()
        .map(|device| {
            let is_this_device = is_me && device.device_id() == this_device;
            DeviceTrust {
                device_id: device.device_id().to_string(),
                display_name: device.display_name().map(str::to_string),
                verified: device.is_cross_signing_trusted()
                    || (!is_this_device && device.is_locally_trusted()),
                is_this_device,
            }
        })
        .collect();
    devices.sort_by(|left, right| left.device_id.cmp(&right.device_id));
    Ok(UserTrust {
        user_id: user.to_string(),
        identity_verified,
        devices,
    })
}

fn crypto_store_error(error: matrix_sdk_crypto::store::CryptoStoreError) -> SdkError {
    SdkError::Protocol(format!("crypto store: {error}"))
}

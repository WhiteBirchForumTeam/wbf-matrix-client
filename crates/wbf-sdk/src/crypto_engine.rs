//! E2EE 引擎：把 `matrix-sdk-crypto` 的 `OlmMachine` **只當狀態機用**（e2ee-walkthrough.md §13）——
//! server 給的推進去（to-device、自己的 OTK 存量），它要送的拉出來（`outgoing_requests`）走我們的橋送，回應再交回去。
//! 網路、水位、銷毀都在這個 crate 自己手上；🚫 沒有 `/sync`、🚫 沒有 matrix-sdk 的 `Client`。
//!
//! 這是 wbf-sdk 裡**第二個**碰上游的地方（第一個是 `backend/matrix_sdk`）；兩者共用同一個 sqlite crypto store（`m/`），
//! ⚠️ 同一時間只能有一個持有者（§13 第 9 條）——過渡期由呼叫端保證不同時開。
//!
//! 涵蓋到哪：這一版做「收」與「發金鑰」：金鑰上傳／查詢／claim、收 to-device、把房間金鑰分給一群人。
//! 加密房間事件並帶房間版本號送出（`Event/Send`）、1506 的重送迴圈在下一支。

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::path::Path;

use matrix_sdk::ruma;
use matrix_sdk::{SqliteCryptoStore, SqliteStoreConfig};
use matrix_sdk_crypto::store::types::RoomKeyInfo;
use matrix_sdk_crypto::types::requests::{AnyOutgoingRequest, OutgoingRequest, ToDeviceRequest};
use matrix_sdk_crypto::{
    DecryptionSettings, EncryptionSettings, EncryptionSyncChanges, OlmMachine, OlmMachineBuilder,
    TrustRequirement,
};
use ruma::api::auth_scheme::SendAccessToken;
use ruma::api::client::keys::{claim_keys, get_keys, upload_keys, upload_signatures};
use ruma::api::client::sync::sync_events::DeviceLists;
use ruma::api::client::to_device::send_event_to_device;
use ruma::api::{IncomingResponseExt as _, OutgoingRequestExt as _, SupportedVersions};
use ruma::events::AnyToDeviceEvent;
use ruma::serde::Raw;
use ruma::{DeviceId, OneTimeKeyAlgorithm, OwnedRoomId, OwnedUserId, RoomId, UInt, UserId};

use crate::channel::PackChannel;
use crate::client::WbfClient;
use crate::protocol::{self, BridgedEndpoint, NoVariables};
use crate::vault::Key32;
use crate::SdkError;

/// 一輪 `send_outgoing_requests` 最多繞幾圈：狀態機每送完一批可能又生出下一批（上傳完金鑰就要查、查完就要 claim），
/// 但不會沒完沒了；繞到這個數還有東西，是我們或上游的 bug，停下來報錯比轉到天荒地老好。
const OUTGOING_ROUNDS_LIMIT: usize = 16;

/// 走橋時 ruma 請求要一個 base URL 才能組 `http::Request`；橋只看 body，這個字串不會出現在線上。
const BRIDGE_BASE_URL: &str = "http://bridge.invalid";

pub struct OlmEngine {
    machine: OlmMachine,
}

impl OlmEngine {
    /// 開（或建）這台裝置的 crypto store 並載入狀態機。
    ///
    /// Args:
    ///     store_dir: 帳號目錄的 `m/`（跟 `backend/matrix_sdk` 同一個目錄、同一把 key）, example: "<account dir>/m"
    ///     store_key: `Vault::matrix_store_key()`
    ///     user_id: example: "@alice:localhost"
    ///     device_id: example: "RJYKSTBOIE"
    /// Return:
    ///     Ok(OlmEngine)
    ///     Err(Usage)      user_id／device_id 不合法
    ///     Err(Protocol)   store 開不了或壞了（含 key 不對）
    pub async fn open(
        store_dir: &Path,
        store_key: &Key32,
        user_id: &str,
        device_id: &str,
    ) -> Result<OlmEngine, SdkError> {
        let user_id = UserId::parse(user_id)
            .map_err(|error| SdkError::Usage(format!("bad user id {user_id:?}: {error}")))?;
        let device_id: &DeviceId = device_id.into();
        std::fs::create_dir_all(store_dir)?;
        let store = SqliteCryptoStore::open_with_config(
            &SqliteStoreConfig::new(store_dir).key(Some(store_key.as_bytes())),
        )
        .await
        .map_err(|error| {
            SdkError::Protocol(format!(
                "cannot open the crypto store at {}: {error}",
                store_dir.display()
            ))
        })?;
        let machine = OlmMachineBuilder::new(&user_id, device_id)
            .with_crypto_store(store)
            .build()
            .await
            .map_err(crypto_store_error)?;
        Ok(OlmEngine { machine })
    }

    pub fn machine(&self) -> &OlmMachine {
        &self.machine
    }

    /// 把一批 to-device 事件（`Device/Fetch` 拉到的，舊→新）與自己的 OTK 存量推進狀態機。
    /// 房間金鑰、金鑰請求與轉發、驗證的 to-device 都在這裡被吃掉；不認得的型別上游自己略過。
    ///
    /// Args:
    ///     events: 每則 `{type, sender, content}`, example: vec![json!({"type":"m.room.encrypted","sender":"@a:x","content":{…}})]
    ///     otk_counts: 自己每種演算法剩幾把 OTK（`CryptoState` 或 `/keys/upload` 回的）；None ＝ 這一批沒帶
    ///     unused_fallback_key_types: ⚠️ `Some(&[])` 是「都用掉了，該換」、None 是「沒給」，兩者不同
    /// Return:
    ///     Ok(Vec<RoomKeyInfo>)  這一批帶進來的**新房間金鑰**（呼叫者拿它決定重解哪些密文）
    ///     Err(Protocol)         某則不是合法的 to-device 事件、或 store 寫入失敗
    pub async fn receive_to_device(
        &self,
        events: Vec<serde_json::Value>,
        otk_counts: Option<&BTreeMap<String, u64>>,
        unused_fallback_key_types: Option<&[String]>,
    ) -> Result<Vec<RoomKeyInfo>, SdkError> {
        let mut to_device_events = Vec::with_capacity(events.len());
        for event in events {
            let raw: Raw<AnyToDeviceEvent> = Raw::from_json(
                serde_json::value::to_raw_value(&event)
                    .map_err(|error| SdkError::Protocol(format!("to-device event: {error}")))?,
            );
            to_device_events.push(raw);
        }
        let counts: BTreeMap<OneTimeKeyAlgorithm, UInt> = otk_counts
            .into_iter()
            .flat_map(|counts| counts.iter())
            .map(|(algorithm, count)| {
                (
                    OneTimeKeyAlgorithm::from(algorithm.as_str()),
                    UInt::new(*count).unwrap_or(UInt::MAX),
                )
            })
            .collect();
        let fallback: Option<Vec<OneTimeKeyAlgorithm>> = unused_fallback_key_types.map(|types| {
            types
                .iter()
                .map(|algorithm| OneTimeKeyAlgorithm::from(algorithm.as_str()))
                .collect()
        });
        let changes = EncryptionSyncChanges {
            to_device_events,
            changed_devices: &DeviceLists::default(),
            one_time_keys_counts: &counts,
            unused_fallback_keys: fallback.as_deref(),
            next_batch_token: None,
        };
        let (_processed, room_keys) = self
            .machine
            .receive_sync_changes_msc4186(changes, &decryption_settings())
            .await
            .map_err(olm_error)?;
        Ok(room_keys)
    }

    /// 把狀態機要送的每一個請求走橋送出去、回應交回去，直到它沒東西要送。
    /// 上傳自己的金鑰、補 OTK、查別人的裝置、claim OTK、發 to-device、上傳簽章都在這裡。
    ///
    /// Return:
    ///     Ok(usize)      送了幾個請求
    ///     Err(Usage)     狀態機要送房間內的驗證訊息（`RoomMessage`）——WS 這條路還沒接，🚫 不靜默丟掉
    ///     Err(Server)    橋或 Matrix 端點拒絕（原樣往上，這一個請求下次 `outgoing_requests` 還會再出現）
    ///     Err(Protocol)  回應解不成 ruma 的型別、或繞了 OUTGOING_ROUNDS_LIMIT 圈還有東西
    pub async fn send_outgoing_requests<C: PackChannel>(
        &self,
        client: &mut WbfClient<C>,
    ) -> Result<usize, SdkError> {
        let mut sent = 0usize;
        for _ in 0..OUTGOING_ROUNDS_LIMIT {
            let requests = self
                .machine
                .outgoing_requests()
                .await
                .map_err(crypto_store_error)?;
            if requests.is_empty() {
                return Ok(sent);
            }
            for request in requests {
                self.send_one(client, &request).await?;
                sent += 1;
            }
        }
        Err(SdkError::Protocol(format!(
            "the crypto state machine still has requests to send after {OUTGOING_ROUNDS_LIMIT} rounds"
        )))
    }

    /// 開始追蹤這些人（之後 `outgoing_requests` 會有他們的 `/keys/query`）。加入、被邀請進來時叫。
    ///
    /// Args:
    ///     users: example: &["@bob:localhost".to_string()]
    pub async fn track_users(&self, users: &[String]) -> Result<(), SdkError> {
        let users = parse_user_ids(users)?;
        self.machine
            .update_tracked_users(users.iter().map(OwnedUserId::as_ref))
            .await
            .map_err(crypto_store_error)
    }

    /// 這些人的裝置變了（成員清單的裝置版本號跟上次不同、或 `DeviceChanged` 推來的）：下一次 `outgoing_requests`
    /// 會重新 `/keys/query` 他們。已經追蹤中的人只靠 `track_users` **不會**再查——那是 1506 之後一定要走這裡的原因。
    ///
    /// Args:
    ///     users: example: &["@bob:localhost".to_string()]
    pub async fn mark_users_changed(&self, users: &[String]) -> Result<(), SdkError> {
        // 跟 `/sync` 的 `device_lists.changed` 同一個入口：狀態機把他們標成要重查。
        let mut device_lists = DeviceLists::new();
        device_lists.changed = parse_user_ids(users)?;
        let changes = EncryptionSyncChanges {
            to_device_events: Vec::new(),
            changed_devices: &device_lists,
            one_time_keys_counts: &BTreeMap::new(),
            unused_fallback_keys: None,
            next_batch_token: None,
        };
        self.machine
            .receive_sync_changes_msc4186(changes, &decryption_settings())
            .await
            .map_err(olm_error)?;
        Ok(())
    }

    /// 這個人在 crypto store 裡已知的裝置（查過 `/keys/query` 之後才會有）。
    ///
    /// Args:
    ///     user_id: example: "@bob:localhost"
    /// Return:
    ///     Ok(Vec<String>)  裝置 id，排序；沒查過或沒有裝置就是空的
    pub async fn known_devices_of(&self, user_id: &str) -> Result<Vec<String>, SdkError> {
        let user_id = UserId::parse(user_id)
            .map_err(|error| SdkError::Usage(format!("bad user id {user_id:?}: {error}")))?;
        let devices = self
            .machine
            .get_user_devices(&user_id, None)
            .await
            .map_err(crypto_store_error)?;
        let mut ids: Vec<String> = devices.keys().map(|id| id.to_string()).collect();
        ids.sort();
        Ok(ids)
    }

    /// 把 `room_id` 目前的房間金鑰分給這些人的裝置（缺 Olm session 的先 claim OTK 建），全部走橋。
    /// 上游決定該不該輪換、發給哪些裝置（`settings.sharing_strategy`）。
    ///
    /// Args:
    ///     client: 走橋用的連線
    ///     room_id: example: "!r:localhost"
    ///     users: 房間裡要拿金鑰的人（含自己）, example: &["@alice:localhost".to_string()]
    ///     settings: example: EncryptionSettings::default()
    /// Return:
    ///     Ok(usize)     送了幾個 to-device 請求（0 ＝ 每台裝置都已經有這把）
    pub async fn share_room_key<C: PackChannel>(
        &self,
        client: &mut WbfClient<C>,
        room_id: &str,
        users: &[String],
        settings: EncryptionSettings,
    ) -> Result<usize, SdkError> {
        let room_id: OwnedRoomId = RoomId::parse(room_id)
            .map_err(|error| SdkError::Usage(format!("bad room id {room_id:?}: {error}")))?;
        let users = parse_user_ids(users)?;
        // 先把追蹤中的查詢送完，名單才是最新的。
        self.send_outgoing_requests(client).await?;
        if let Some((request_id, claim)) = self
            .machine
            .get_missing_sessions(users.iter().map(OwnedUserId::as_ref))
            .await
            .map_err(crypto_store_error)?
        {
            let reply = self
                .call_bridge_with(client, protocol::BRIDGE_KEYS_CLAIM, claim)
                .await?;
            let response = parse_response::<claim_keys::v3::Response>(&reply, "KeysClaim")?;
            self.machine
                .mark_request_as_sent(&request_id, &response)
                .await
                .map_err(olm_error)?;
        }
        let requests = self
            .machine
            .share_room_key(&room_id, users.iter().map(OwnedUserId::as_ref), settings)
            .await
            .map_err(olm_error)?;
        let count = requests.len();
        for request in requests {
            self.send_to_device_request(client, &request).await?;
        }
        Ok(count)
    }

    async fn send_one<C: PackChannel>(
        &self,
        client: &mut WbfClient<C>,
        request: &OutgoingRequest,
    ) -> Result<(), SdkError> {
        let request_id = request.request_id();
        match request.request() {
            AnyOutgoingRequest::KeysUpload(upload) => {
                let reply = self
                    .call_bridge_with(client, protocol::BRIDGE_KEYS_UPLOAD, upload.clone())
                    .await?;
                let response = parse_response::<upload_keys::v3::Response>(&reply, "KeysUpload")?;
                self.machine
                    .mark_request_as_sent(request_id, &response)
                    .await
                    .map_err(olm_error)
            }
            AnyOutgoingRequest::KeysQuery(query) => {
                let mut ruma_request = get_keys::v3::Request::new();
                ruma_request.device_keys = query.device_keys.clone();
                ruma_request.timeout = query.timeout;
                let reply = self
                    .call_bridge_with(client, protocol::BRIDGE_KEYS_QUERY, ruma_request)
                    .await?;
                let response = parse_response::<get_keys::v3::Response>(&reply, "KeysQuery")?;
                self.machine
                    .mark_request_as_sent(request_id, &response)
                    .await
                    .map_err(olm_error)
            }
            AnyOutgoingRequest::KeysClaim(claim) => {
                let reply = self
                    .call_bridge_with(client, protocol::BRIDGE_KEYS_CLAIM, claim.clone())
                    .await?;
                let response = parse_response::<claim_keys::v3::Response>(&reply, "KeysClaim")?;
                self.machine
                    .mark_request_as_sent(request_id, &response)
                    .await
                    .map_err(olm_error)
            }
            AnyOutgoingRequest::ToDeviceRequest(to_device) => {
                self.send_to_device_request(client, to_device).await
            }
            AnyOutgoingRequest::SignatureUpload(upload) => {
                let reply = self
                    .call_bridge_with(client, protocol::BRIDGE_SIGNATURES_UPLOAD, upload.clone())
                    .await?;
                let response =
                    parse_response::<upload_signatures::v3::Response>(&reply, "SignaturesUpload")?;
                self.machine
                    .mark_request_as_sent(request_id, &response)
                    .await
                    .map_err(olm_error)
            }
            AnyOutgoingRequest::RoomMessage(_) => Err(SdkError::Usage(
                "the crypto state machine wants to send an in-room verification event; \
                 that path is not on the wbf channel yet"
                    .into(),
            )),
        }
    }

    /// 一個 to-device 請求走橋（`0x16 0x25`），成功後交回狀態機（它才會把「這把金鑰發給過誰」記下來）。
    async fn send_to_device_request<C: PackChannel>(
        &self,
        client: &mut WbfClient<C>,
        request: &ToDeviceRequest,
    ) -> Result<(), SdkError> {
        let body = serde_json::to_vec(&serde_json::json!({ "messages": request.messages }))
            .expect("to-device messages serialize");
        client
            .send_to_device(
                &request.event_type.to_string(),
                request.txn_id.as_str(),
                body,
            )
            .await?;
        self.machine
            .mark_request_as_sent(&request.txn_id, &send_event_to_device::v3::Response::new())
            .await
            .map_err(olm_error)
    }

    /// 一個 ruma 請求 → 它的 HTTP body → 走橋。路徑與 method 由 server 的表決定，這裡只要 body 對。
    async fn call_bridge_with<C: PackChannel, R>(
        &self,
        client: &mut WbfClient<C>,
        endpoint: BridgedEndpoint,
        request: R,
    ) -> Result<protocol::BridgeReply, SdkError>
    where
        R: ruma::api::OutgoingRequest,
        R::Authentication:
            ruma::api::auth_scheme::AuthScheme<Input<'static> = SendAccessToken<'static>>,
        R::PathBuilder:
            ruma::api::path_builder::PathBuilder<Input<'static> = Cow<'static, SupportedVersions>>,
    {
        let body = ruma_request_body(request)?;
        client.call_bridge(endpoint, &NoVariables {}, body).await
    }
}

/// 只把 Olm session 的密文交給狀態機解，信任要求先照上游的預設（🚨 送出那一半決定「誰收得到金鑰」時要明確選，§13 第 8 條）。
fn decryption_settings() -> DecryptionSettings {
    DecryptionSettings {
        sender_device_trust_requirement: TrustRequirement::Untrusted,
    }
}

fn parse_user_ids(users: &[String]) -> Result<Vec<OwnedUserId>, SdkError> {
    users
        .iter()
        .map(|user| {
            UserId::parse(user)
                .map_err(|error| SdkError::Usage(format!("bad user id {user:?}: {error}")))
        })
        .collect()
}

/// ruma 組請求時，要 token 的端點沒給 token 會直接拒絕；這裡給一個占位字串——只取 body，header 整個丟掉，
/// 真正的 `Authorization` 由橋在 server 那端用這條連線的 session 填（wbf-api-bridge.md §2.2 規則 1，client 蓋不掉）。
const PLACEHOLDER_ACCESS_TOKEN: &str = "not-sent-over-the-bridge";

/// ruma 請求組成 HTTP 請求（用一個假的 base URL）只為了拿它的 body bytes。
fn ruma_request_body<R>(request: R) -> Result<Vec<u8>, SdkError>
where
    R: ruma::api::OutgoingRequest,
    R::Authentication:
        ruma::api::auth_scheme::AuthScheme<Input<'static> = SendAccessToken<'static>>,
    R::PathBuilder:
        ruma::api::path_builder::PathBuilder<Input<'static> = Cow<'static, SupportedVersions>>,
{
    let versions = SupportedVersions::from_parts(&["v1.11".to_string()], &BTreeMap::new());
    let http_request = request
        .try_into_http_request::<Vec<u8>>(
            BRIDGE_BASE_URL,
            SendAccessToken::IfRequired(PLACEHOLDER_ACCESS_TOKEN),
            Cow::Owned(versions),
        )
        .map_err(|error| SdkError::Protocol(format!("cannot build the request body: {error}")))?;
    Ok(http_request.into_body())
}

/// 橋回的 body → ruma 的回應型別。狀態碼照橋的 `status`（一定是 2xx，`expect_bridge_reply` 擋過）。
fn parse_response<T: ruma::api::IncomingResponse>(
    reply: &protocol::BridgeReply,
    endpoint_name: &str,
) -> Result<T, SdkError> {
    let http_response = http::Response::builder()
        .status(reply.status)
        .header(http::header::CONTENT_TYPE, "application/json")
        .body(reply.body.as_slice())
        .map_err(|error| SdkError::Protocol(format!("{endpoint_name}: {error}")))?;
    T::try_from_http_response(http_response)
        .map_err(|error| SdkError::Protocol(format!("{endpoint_name} response: {error}")))
}

fn crypto_store_error(error: matrix_sdk_crypto::store::CryptoStoreError) -> SdkError {
    SdkError::Protocol(format!("crypto store: {error}"))
}

fn olm_error(error: matrix_sdk_crypto::OlmError) -> SdkError {
    SdkError::Protocol(format!("crypto: {error}"))
}

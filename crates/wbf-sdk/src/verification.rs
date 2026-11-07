//! 裝置驗證（to-device 的 SAS：比對表情或數字，/docs/design/keys/cross-signing-and-verification.md §5）。
//! 狀態機是上游的 `VerificationRequest`／`Sas`；這裡只把每一步回的請求走橋送出去，並把上游的狀態翻成 `verify.state` 的形狀。
//! 收到的驗證事件不經過這裡：它們是 to-device，跟房間金鑰一起由 `OlmEngine::receive_to_device` 交給狀態機。
//! ⚠️ 上游的流程只在記憶體：引擎（daemon）重開，進行中的全部消失。

use matrix_sdk::ruma;
use matrix_sdk_crypto::types::requests::OutgoingVerificationRequest;
use matrix_sdk_crypto::{Sas, VerificationRequest};
use ruma::{DeviceId, UserId};
use serde::{Deserialize, Serialize};

use crate::channel::PackChannel;
use crate::client::WbfClient;
use crate::crypto_engine::OlmEngine;
use crate::SdkError;

/// 一個驗證流程走到哪（`verify.state` 的 `state`）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerifyStage {
    /// 請求送出或收到了，對方還沒接受
    Requested,
    /// 雙方都接受了請求，還沒開始比對
    Ready,
    /// 開始比對了，還在交換金鑰
    Started,
    /// 表情與數字出來了，等使用者比對
    KeysExchanged,
    /// 這邊說一樣了，等對方
    Confirmed,
    Done,
    Cancelled,
}

/// 一個表情：符號與它的英文名字（上游 SAS 規格的表）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerifyEmoji {
    /// example: "🐶"
    pub symbol: String,
    /// example: "Dog"
    pub description: String,
}

/// 一個驗證流程現在的樣子（`verify.state` 推播與 `verify.list` 的一列）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerifyState {
    pub flow_id: String,
    /// 跟誰驗（自我驗證是自己）
    pub other_user: String,
    /// 對方哪一台；請求還沒被接受時不知道
    pub other_device: Option<String>,
    pub we_started: bool,
    pub state: VerifyStage,
    /// `keys_exchanged` 之後才有：7 個
    #[serde(skip_serializing_if = "Option::is_none")]
    pub emoji: Option<Vec<VerifyEmoji>>,
    /// `keys_exchanged` 之後才有：3 個四位數
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decimals: Option<[u16; 3]>,
    /// `cancelled` 才有
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// 開一個驗證請求。
///
/// Args:
///     user_id: 要驗誰；自己就是自我驗證, example: "@alice:localhost"
///     device_id: 對方哪一台；`user_id` 是自己時可以省（送給自己所有裝置）, example: Some("ABCDEFGHIJ")
/// Return:
///     Ok(String)     flow id（之後每一步都帶它）
///     Err(Usage)     自己還沒有交叉簽章身分（省略 device_id 時）、驗別人沒給 device_id、那台裝置不認得
///     Err(Server)／Err(Protocol)  送不出去
pub async fn request_verification<C: PackChannel>(
    engine: &OlmEngine,
    client: &mut WbfClient<C>,
    user_id: &str,
    device_id: Option<&str>,
) -> Result<String, SdkError> {
    let machine = engine.machine();
    let user = parse_user(user_id)?;
    // 先把對方的裝置查清楚：沒查過的裝置找不到。
    engine.track_users(&[user.to_string()]).await?;
    engine.send_outgoing_requests(client).await?;
    let (request, outgoing) = match device_id {
        None if user == machine.user_id() => machine
            .get_identity(&user, None)
            .await
            .map_err(crypto_store_error)?
            .and_then(|identity| identity.own())
            .ok_or_else(|| {
                SdkError::Usage("this account has no cross-signing identity yet".into())
            })?
            .request_verification()
            .await
            .map_err(crypto_store_error)?,
        None => {
            return Err(SdkError::Usage(
                "verifying another user needs the device id".into(),
            ))
        }
        Some(device_id) => {
            let device_id: &DeviceId = device_id.into();
            machine
                .get_device(&user, device_id, None)
                .await
                .map_err(crypto_store_error)?
                .ok_or_else(|| SdkError::Usage(format!("{user} has no known device {device_id}")))?
                .request_verification()
        }
    };
    send(engine, client, outgoing).await?;
    Ok(request.flow_id().as_str().to_string())
}

/// 接受：對方開始的比對（SAS）優先，不然是對方的請求。
///
/// Return:
///     Ok(())
///     Err(Usage)   沒有這個流程、或沒有東西可以接受（自己開始的、已經接受過、取消了）
pub async fn accept<C: PackChannel>(
    engine: &OlmEngine,
    client: &mut WbfClient<C>,
    user_id: &str,
    flow_id: &str,
) -> Result<(), SdkError> {
    let (request, sas) = find_flow(engine, user_id, flow_id)?;
    let outgoing = match (&sas, &request) {
        (Some(sas), _) => sas.accept(),
        (None, Some(request)) => request.accept(),
        (None, None) => None,
    };
    let outgoing = outgoing
        .ok_or_else(|| SdkError::Usage(format!("verification {flow_id} has nothing to accept")))?;
    send(engine, client, outgoing).await
}

/// 從一個雙方都接受了的請求開始比對。
///
/// Return:
///     Ok(())
///     Err(Usage)   沒有這個請求、或它還沒 ready
pub async fn start_sas<C: PackChannel>(
    engine: &OlmEngine,
    client: &mut WbfClient<C>,
    user_id: &str,
    flow_id: &str,
) -> Result<(), SdkError> {
    let (request, _) = find_flow(engine, user_id, flow_id)?;
    let request = request
        .ok_or_else(|| SdkError::Usage(format!("there is no verification request {flow_id}")))?;
    let (_, outgoing) = request
        .start_sas()
        .await
        .map_err(crypto_store_error)?
        .ok_or_else(|| SdkError::Usage(format!("verification {flow_id} is not ready")))?;
    send(engine, client, outgoing).await
}

/// 使用者說表情一樣：送確認；最後一步回的簽章（自我驗證是 self-signing key 簽那台、別人是 user-signing key 簽那個人）也送。
/// 之後把狀態機排的東西（自我驗證完要跟對方要的秘密）一起送掉。
///
/// Return:
///     Ok(())
///     Err(Usage)   沒有這個比對、或表情還沒出來
///     Err(Server)  簽章被拒（`failures` 不是空的也算）
pub async fn confirm<C: PackChannel>(
    engine: &OlmEngine,
    client: &mut WbfClient<C>,
    user_id: &str,
    flow_id: &str,
) -> Result<(), SdkError> {
    let (_, sas) = find_flow(engine, user_id, flow_id)?;
    let sas = sas.ok_or_else(|| {
        SdkError::Usage(format!("verification {flow_id} has not started comparing"))
    })?;
    if !sas.can_be_presented() || sas.is_cancelled() {
        return Err(SdkError::Usage(format!(
            "verification {flow_id} has nothing to confirm"
        )));
    }
    let (outgoing, signatures) = sas.confirm().await.map_err(crypto_store_error)?;
    for request in outgoing {
        send(engine, client, request).await?;
    }
    if let Some(upload) = signatures {
        engine.send_signature_upload(client, upload).await?;
    }
    engine.send_outgoing_requests(client).await?;
    Ok(())
}

/// 取消。已經結束（完成或取消）的什麼都不送。
///
/// Return:
///     Ok(())
///     Err(Usage)   沒有這個流程
pub async fn cancel<C: PackChannel>(
    engine: &OlmEngine,
    client: &mut WbfClient<C>,
    user_id: &str,
    flow_id: &str,
) -> Result<(), SdkError> {
    let (request, sas) = find_flow(engine, user_id, flow_id)?;
    let outgoing = match (&sas, &request) {
        (Some(sas), _) => sas.cancel(),
        (None, Some(request)) => request.cancel(),
        (None, None) => None,
    };
    if let Some(outgoing) = outgoing {
        send(engine, client, outgoing).await?;
    }
    Ok(())
}

/// 一個流程現在的樣子（查一次就丟；要一直看的用 [`WatchedFlow`]）。
///
/// Return:
///     Ok(Some(VerifyState))
///     Ok(None)      狀態機裡沒有這個流程（從沒有過、已經結束被清掉、或引擎重開了）
///     Err(Usage)    user_id 不合法
pub fn find_state(
    engine: &OlmEngine,
    user_id: &str,
    flow_id: &str,
) -> Result<Option<VerifyState>, SdkError> {
    Ok(WatchedFlow::find(engine, user_id, flow_id)?.map(|mut flow| flow.state(engine)))
}

/// 握著一個流程的上游物件。⚠️ 上游每次送請求（`outgoing_requests`）都會把完成、取消的流程從它的快取清掉，
/// 之後用 flow id 就查不到了；物件本身是共享的把手，被清掉之後照樣讀得到最後的狀態。
/// 所以要看到「完成」「取消」的人（觀察者、做完一步要回狀態的 RPC）要在動手**之前**拿到它、一直握著。
pub struct WatchedFlow {
    other_user: String,
    flow_id: String,
    request: Option<VerificationRequest>,
    sas: Option<Sas>,
}

impl WatchedFlow {
    /// Return:
    ///     Ok(Some(WatchedFlow))
    ///     Ok(None)      狀態機裡沒有這個流程
    ///     Err(Usage)    user_id 不合法
    pub fn find(
        engine: &OlmEngine,
        user_id: &str,
        flow_id: &str,
    ) -> Result<Option<WatchedFlow>, SdkError> {
        let user = parse_user(user_id)?;
        let machine = engine.machine();
        let request = machine.get_verification_request(&user, flow_id);
        let sas = find_sas(engine, &user, flow_id);
        if request.is_none() && sas.is_none() {
            return Ok(None);
        }
        Ok(Some(WatchedFlow {
            other_user: user_id.to_string(),
            flow_id: flow_id.to_string(),
            request,
            sas,
        }))
    }

    /// 現在的樣子。比對（SAS）開始之後才有的那個物件，在它還在狀態機裡時補拿進來。
    pub fn state(&mut self, engine: &OlmEngine) -> VerifyState {
        if self.sas.is_none() {
            if let Ok(user) = UserId::parse(&self.other_user) {
                self.sas = find_sas(engine, &user, &self.flow_id);
            }
        }
        let other_user = self.other_user.clone();
        let flow_id = self.flow_id.clone();
        if let Some(sas) = &self.sas {
            let (state, reason) = if sas.is_cancelled() {
                (
                    VerifyStage::Cancelled,
                    sas.cancel_info().map(|info| info.reason().to_string()),
                )
            } else if sas.is_done() {
                (VerifyStage::Done, None)
            } else if sas.have_we_confirmed() {
                (VerifyStage::Confirmed, None)
            } else if sas.can_be_presented() {
                (VerifyStage::KeysExchanged, None)
            } else {
                (VerifyStage::Started, None)
            };
            let shows_codes =
                state == VerifyStage::KeysExchanged || state == VerifyStage::Confirmed;
            return VerifyState {
                flow_id,
                other_user,
                other_device: Some(sas.other_device().device_id().to_string()),
                we_started: sas.we_started(),
                state,
                emoji: sas.emoji().filter(|_| shows_codes).map(|emoji| {
                    emoji
                        .iter()
                        .map(|item| VerifyEmoji {
                            symbol: item.symbol.to_string(),
                            description: item.description.to_string(),
                        })
                        .collect()
                }),
                decimals: sas
                    .decimals()
                    .filter(|_| shows_codes)
                    .map(|(first, second, third)| [first, second, third]),
                reason,
            };
        }
        // `find` 保證兩個至少有一個；比對沒開始就是只有請求。
        let (state, reason, other_device, we_started) = match &self.request {
            Some(request) if request.is_cancelled() => (
                VerifyStage::Cancelled,
                request.cancel_info().map(|info| info.reason().to_string()),
                request.other_device_id(),
                request.we_started(),
            ),
            Some(request) => (
                if request.is_done() {
                    VerifyStage::Done
                } else if request.is_ready() {
                    VerifyStage::Ready
                } else {
                    VerifyStage::Requested
                },
                None,
                request.other_device_id(),
                request.we_started(),
            ),
            None => (VerifyStage::Cancelled, None, None, false),
        };
        VerifyState {
            flow_id,
            other_user,
            other_device: other_device.map(|device| device.to_string()),
            we_started,
            state,
            emoji: None,
            decimals: None,
            reason,
        }
    }
}

fn find_sas(engine: &OlmEngine, user: &UserId, flow_id: &str) -> Option<Sas> {
    engine
        .machine()
        .get_verification(user, flow_id)
        .and_then(|verification| verification.sas_v1())
        .map(|sas| *sas)
}

/// 一則 to-device 事件如果是「有人要跟這台驗證」，回 (對方, flow id)。
///
/// Args:
///     event: `{type, sender, content}`, example: json!({"type": "m.key.verification.request", "sender": "@a:x", "content": {"transaction_id": "t1", …}})
/// Return:
///     Option<(String, String)>  不是請求、或缺欄位是 None
pub fn find_incoming_request(event: &serde_json::Value) -> Option<(String, String)> {
    if event.get("type")?.as_str()? != "m.key.verification.request" {
        return None;
    }
    let sender = event.get("sender")?.as_str()?.to_string();
    let flow_id = event
        .get("content")?
        .get("transaction_id")?
        .as_str()?
        .to_string();
    Some((sender, flow_id))
}

/// 狀態機裡這個流程的請求與比對（任一個都可能沒有）。
///
/// Return:
///     Ok((request, sas))
///     Err(Usage)   user_id 不合法、或兩個都沒有
fn find_flow(
    engine: &OlmEngine,
    user_id: &str,
    flow_id: &str,
) -> Result<(Option<VerificationRequest>, Option<Sas>), SdkError> {
    let machine = engine.machine();
    let user = parse_user(user_id)?;
    let request = machine.get_verification_request(&user, flow_id);
    let sas = find_sas(engine, &user, flow_id);
    if request.is_none() && sas.is_none() {
        return Err(SdkError::Usage(format!(
            "there is no verification {flow_id} with {user_id}"
        )));
    }
    Ok((request, sas))
}

/// 驗證的每一步都是 to-device（房間裡的驗證這支不做）。
async fn send<C: PackChannel>(
    engine: &OlmEngine,
    client: &mut WbfClient<C>,
    outgoing: OutgoingVerificationRequest,
) -> Result<(), SdkError> {
    match outgoing {
        OutgoingVerificationRequest::ToDevice(request) => {
            engine.send_to_device_request(client, &request).await
        }
        OutgoingVerificationRequest::InRoom(_) => Err(SdkError::Usage(
            "verifying inside a room is not supported; verify a device instead".into(),
        )),
    }
}

fn parse_user(user_id: &str) -> Result<ruma::OwnedUserId, SdkError> {
    UserId::parse(user_id)
        .map_err(|error| SdkError::Usage(format!("bad user id {user_id:?}: {error}")))
}

fn crypto_store_error(error: impl std::fmt::Display) -> SdkError {
    SdkError::Protocol(format!("crypto store: {error}"))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::find_incoming_request;

    #[test]
    fn only_a_request_with_a_sender_and_a_transaction_id_is_an_incoming_request() {
        let request = json!({
            "type": "m.key.verification.request", "sender": "@a:localhost",
            "content": { "transaction_id": "t1", "from_device": "D", "methods": ["m.sas.v1"], "timestamp": 1 }
        });
        assert_eq!(
            find_incoming_request(&request),
            Some(("@a:localhost".to_string(), "t1".to_string()))
        );
        let start = json!({ "type": "m.key.verification.start", "sender": "@a:localhost", "content": { "transaction_id": "t1" } });
        assert_eq!(
            find_incoming_request(&start),
            None,
            "only requests open a flow"
        );
        let no_sender =
            json!({ "type": "m.key.verification.request", "content": { "transaction_id": "t1" } });
        assert_eq!(find_incoming_request(&no_sender), None);
        let no_flow = json!({ "type": "m.key.verification.request", "sender": "@a:localhost", "content": {} });
        assert_eq!(find_incoming_request(&no_flow), None);
    }
}

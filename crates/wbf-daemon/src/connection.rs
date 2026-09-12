//! 一條 RPC 連線的狀態機（rpc-spec §1.1–§1.4）：bytes 進、`Inbound` 出；訊息進、bytes 出。
//!
//! 這裡**不碰 socket、不碰 core**，所以整個可以離線測。它負責的判斷只有三個：
//! 1. 這包能不能拆（`pack`）、拆出來是不是 JSON；
//! 2. `hello` 談成了沒（名字、protocol）；
//! 3. **出去的包該明文還是密文**——這是全 daemon 唯一判斷這件事的地方。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use serde::Deserialize;
use serde_json::Value;

use crate::message::{CloseReason, Request, Response};
use crate::pack::{self, PackError, PackType, RpcKeys, Side};
use crate::protocol;

/// 全 daemon 共用的加密狀態（rpc-spec §1.1）：預設開。關掉只為除錯。
#[derive(Clone)]
pub struct EncryptionPolicy(Arc<AtomicBool>);

impl EncryptionPolicy {
    pub fn enforced() -> EncryptionPolicy {
        EncryptionPolicy(Arc::new(AtomicBool::new(true)))
    }

    pub fn is_enforced(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }

    pub fn set_enforced(&self, enforced: bool) {
        self.0.store(enforced, Ordering::SeqCst);
    }
}

/// 收到一包之後要做的事。
#[derive(Debug, PartialEq)]
pub enum Inbound {
    /// `hello` 過了兩關；`protocol` 是談定的。回應由呼叫端組（它才知道 daemon 的狀態）。
    HelloAccepted { id: Option<u64>, protocol: u32 },
    /// 正常請求，交給 handle。
    Request(Request),
    /// 協議層錯誤：先送這個 `Response`（明文），再關連線。
    Close(Response),
}

#[derive(Deserialize)]
struct HelloParams {
    #[serde(default)]
    protocols: Vec<u32>,
    #[serde(default)]
    client: String,
}

pub struct Connection {
    keys: Arc<RpcKeys>,
    policy: EncryptionPolicy,
    /// 談定的 protocol；`None` 就是還沒 `hello`。
    protocol: Option<u32>,
    /// 最後一包進來是明文還是密文。enforce 關掉時，出去的包照這個。
    last_inbound: PackType,
}

impl Connection {
    pub fn new(keys: Arc<RpcKeys>, policy: EncryptionPolicy) -> Connection {
        Connection {
            keys,
            policy,
            protocol: None,
            last_inbound: PackType::Cipher,
        }
    }

    pub fn protocol(&self) -> Option<u32> {
        self.protocol
    }

    /// 收到一個 WS binary frame。
    ///
    /// Args:
    ///     frame: 整個 frame 的 bytes
    /// Return:
    ///     Inbound  見各 variant；`Close` 時呼叫端要送它再關
    pub fn receive(&mut self, frame: &[u8]) -> Inbound {
        let (pack_type, json_bytes) = match pack::open(&self.keys, Side::Daemon, frame) {
            Ok(opened) => opened,
            Err(PackError::CannotDecrypt) => {
                return Inbound::Close(Response::close(
                    None,
                    CloseReason::BadToken,
                    "could not decrypt the frame; the daemon token does not match",
                ))
            }
            Err(error) => {
                return Inbound::Close(Response::close(
                    None,
                    CloseReason::BadFrame,
                    format!("bad frame: {error:?}"),
                ))
            }
        };
        if pack_type == PackType::Plain && self.policy.is_enforced() {
            return Inbound::Close(Response::close(
                None,
                CloseReason::BadFrame,
                "plaintext frames are not accepted while encryption is enforced",
            ));
        }
        self.last_inbound = pack_type;
        let request: Request = match serde_json::from_slice(&json_bytes) {
            Ok(request) => request,
            Err(error) => {
                return Inbound::Close(Response::close(
                    None,
                    CloseReason::BadFrame,
                    format!("frame is not a request object: {error}"),
                ))
            }
        };
        if request.method == "hello" {
            return self.hello(request);
        }
        if self.protocol.is_none() {
            return Inbound::Close(Response::close(
                request.id,
                CloseReason::HelloRequired,
                "the first request on a connection must be hello",
            ));
        }
        Inbound::Request(request)
    }

    fn hello(&mut self, request: Request) -> Inbound {
        let params: HelloParams = match serde_json::from_value(request.params) {
            Ok(params) => params,
            Err(error) => {
                return Inbound::Close(Response::close(
                    request.id,
                    CloseReason::BadFrame,
                    format!("hello params: {error}"),
                ))
            }
        };
        if !protocol::is_client_name_acceptable(&params.client) {
            return Inbound::Close(Response::close(
                request.id,
                CloseReason::BadClient,
                format!(
                    "client name must start with {:?}",
                    protocol::CLIENT_NAME_PREFIX
                ),
            ));
        }
        let Some(agreed) = protocol::negotiate(&params.protocols) else {
            return Inbound::Close(Response::close(
                request.id,
                CloseReason::ProtocolMismatch,
                format!(
                    "no common protocol: client offers {:?}, daemon speaks {:?}",
                    params.protocols,
                    protocol::SUPPORTED_PROTOCOLS
                ),
            ));
        };
        // 重複的 hello 只重談一次；已談定的連線再 hello 不換版本（rpc-spec §1.3）。
        let protocol = *self.protocol.get_or_insert(agreed);
        Inbound::HelloAccepted {
            id: request.id,
            protocol,
        }
    }

    /// 正常回應與推播出去的 type：enforce 開著一律密文；關著就跟著對方最後一包。
    fn outbound_type(&self) -> PackType {
        if self.policy.is_enforced() {
            PackType::Cipher
        } else {
            self.last_inbound
        }
    }

    pub fn seal_response(&self, response: &Response) -> Vec<u8> {
        self.seal(
            self.outbound_type(),
            &serde_json::to_vec(response).expect("Response serialises"),
        )
    }

    pub fn seal_push(&self, push: &Request) -> Vec<u8> {
        self.seal(
            self.outbound_type(),
            &serde_json::to_vec(push).expect("Request serialises"),
        )
    }

    /// 協議層的 close 通知**一律明文**（rpc-spec §1.4）：對方可能沒有金鑰。
    pub fn seal_close(&self, notice: &Response) -> Vec<u8> {
        self.seal(
            PackType::Plain,
            &serde_json::to_vec(notice).expect("Response serialises"),
        )
    }

    fn seal(&self, pack_type: PackType, json: &[u8]) -> Vec<u8> {
        pack::seal(&self.keys, Side::Daemon, pack_type, json)
    }
}

/// 從 `params` 讀 `Value`，給 handle 用。
pub fn params_or_empty_object(params: &Value) -> Value {
    if params.is_null() {
        Value::Object(Default::default())
    } else {
        params.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys() -> Arc<RpcKeys> {
        Arc::new(RpcKeys::from_token(&[9u8; 256]))
    }

    fn client_frame(keys: &RpcKeys, pack_type: PackType, json: &str) -> Vec<u8> {
        pack::seal(keys, Side::Client, pack_type, json.as_bytes())
    }

    fn hello_json() -> &'static str {
        r#"{"method":"hello","params":{"protocols":[1],"client":"wbf-matrix-rpc-cli 0.1.0"},"id":0}"#
    }

    fn close_code(inbound: &Inbound) -> Option<u32> {
        match inbound {
            Inbound::Close(response) => Some(response.code),
            _ => None,
        }
    }

    #[test]
    fn a_good_hello_is_accepted_and_the_protocol_is_agreed() {
        let keys = keys();
        let mut connection = Connection::new(keys.clone(), EncryptionPolicy::enforced());
        let inbound = connection.receive(&client_frame(&keys, PackType::Cipher, hello_json()));
        assert_eq!(
            inbound,
            Inbound::HelloAccepted {
                id: Some(0),
                protocol: 1
            }
        );
        assert_eq!(connection.protocol(), Some(1));
    }

    #[test]
    fn anything_before_hello_is_hello_required_with_the_request_id() {
        let keys = keys();
        let mut connection = Connection::new(keys.clone(), EncryptionPolicy::enforced());
        let inbound = connection.receive(&client_frame(
            &keys,
            PackType::Cipher,
            r#"{"method":"account.list","id":5}"#,
        ));
        let Inbound::Close(notice) = inbound else {
            panic!("{inbound:?}")
        };
        assert_eq!(notice.code, 9003);
        assert_eq!(notice.id, Some(5));
        assert_eq!(notice.result["close"], "HELLO_REQUIRED");
    }

    #[test]
    fn a_wrong_token_is_bad_token_and_the_notice_goes_out_in_plaintext() {
        let keys = keys();
        let other = RpcKeys::from_token(&[1u8; 256]);
        let mut connection = Connection::new(keys.clone(), EncryptionPolicy::enforced());
        let inbound = connection.receive(&client_frame(&other, PackType::Cipher, hello_json()));
        assert_eq!(close_code(&inbound), Some(9001));
        let Inbound::Close(notice) = inbound else {
            unreachable!()
        };
        let bytes = connection.seal_close(&notice);
        // 明文：前綴之後就是 JSON，任何人都讀得到。
        assert_eq!(&bytes[..2], &[0x01, 0x01]);
        let json: Value = serde_json::from_slice(&bytes[2..]).unwrap();
        assert_eq!(json["code"], 9001);
        assert_eq!(json["id"], Value::Null);
    }

    #[test]
    fn plaintext_is_refused_while_enforced_and_accepted_after_downgrade() {
        let keys = keys();
        let policy = EncryptionPolicy::enforced();
        let mut connection = Connection::new(keys.clone(), policy.clone());
        let plain_hello = client_frame(&keys, PackType::Plain, hello_json());
        assert_eq!(close_code(&connection.receive(&plain_hello)), Some(9002));

        policy.set_enforced(false);
        let mut connection = Connection::new(keys.clone(), policy.clone());
        assert!(matches!(
            connection.receive(&plain_hello),
            Inbound::HelloAccepted { .. }
        ));
        // 降級後對明文請求用明文回。
        let bytes = connection.seal_response(&Response::ok(Some(0), Value::Null));
        assert_eq!(&bytes[..2], &[0x01, 0x01]);
    }

    #[test]
    fn responses_are_ciphertext_while_enforced_and_decrypt_on_the_client_side() {
        let keys = keys();
        let mut connection = Connection::new(keys.clone(), EncryptionPolicy::enforced());
        connection.receive(&client_frame(&keys, PackType::Cipher, hello_json()));
        let bytes = connection.seal_response(&Response::ok(Some(0), serde_json::json!({ "x": 1 })));
        assert_eq!(&bytes[..2], &[0x01, 0x02]);
        let (_, json) = pack::open(&keys, Side::Client, &bytes).unwrap();
        let response: Response = serde_json::from_slice(&json).unwrap();
        assert_eq!(response.result["x"], 1);
    }

    #[test]
    fn bad_client_name_and_no_common_protocol_are_refused_with_the_hello_id() {
        let keys = keys();
        let mut connection = Connection::new(keys.clone(), EncryptionPolicy::enforced());
        let bad_name = r#"{"method":"hello","params":{"protocols":[1],"client":"rpc-cli"},"id":0}"#;
        let inbound = connection.receive(&client_frame(&keys, PackType::Cipher, bad_name));
        assert_eq!(close_code(&inbound), Some(9004));
        assert_eq!(connection.protocol(), None);

        let mut connection = Connection::new(keys.clone(), EncryptionPolicy::enforced());
        let too_new =
            r#"{"method":"hello","params":{"protocols":[7],"client":"wbf-matrix-x 1"},"id":2}"#;
        let inbound = connection.receive(&client_frame(&keys, PackType::Cipher, too_new));
        assert_eq!(close_code(&inbound), Some(9005));
        let Inbound::Close(notice) = inbound else {
            unreachable!()
        };
        assert_eq!(notice.id, Some(2));
    }

    #[test]
    fn a_frame_that_is_not_a_request_object_is_bad_frame() {
        let keys = keys();
        let mut connection = Connection::new(keys.clone(), EncryptionPolicy::enforced());
        let inbound = connection.receive(&client_frame(&keys, PackType::Cipher, "[1,2,3]"));
        assert_eq!(close_code(&inbound), Some(9002));
        let inbound = connection.receive(&client_frame(&keys, PackType::Cipher, "not json"));
        assert_eq!(close_code(&inbound), Some(9002));
    }

    #[test]
    fn a_second_hello_does_not_renegotiate_the_protocol() {
        let keys = keys();
        let mut connection = Connection::new(keys.clone(), EncryptionPolicy::enforced());
        connection.receive(&client_frame(&keys, PackType::Cipher, hello_json()));
        let again = connection.receive(&client_frame(&keys, PackType::Cipher, hello_json()));
        assert_eq!(
            again,
            Inbound::HelloAccepted {
                id: Some(0),
                protocol: 1
            }
        );
    }
}

//! 加密之前的訊息形狀（architecture-v2 §4.6）與 code 表（rpc-spec §5、§1.4）。
//!
//! 只有兩種形狀：`Request`（有 `id` 要回、沒 `id` 是推播）與 `Response`（`code`／`msg`／`result`／`id`
//! 平鋪，🚫 不是 JSON-RPC 的 `result`／`error` 二選一）。協議層的 close 通知**也是 `Response`**
//! （`code` 9xxx、`result.close`）：前端翻譯器只有一條路。

use serde::{Deserialize, Serialize};
use serde_json::Value;
use wbf_core::CoreError;

/// 請求（有 `id`）或推播（沒 `id`）。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Request {
    pub method: String,
    #[serde(default)]
    pub params: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<u64>,
}

impl Request {
    /// 推播：沒有 `id`。
    pub fn push(method: &str, params: Value) -> Request {
        Request {
            method: method.to_string(),
            params,
            id: None,
        }
    }
}

/// 回應。**失敗時 `result` 是 `null`，欄位一定在**（弱型別的前端少一種 undefined）。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Response {
    pub code: u32,
    pub msg: String,
    pub result: Value,
    /// 對得上請求就帶；對不上（解不開、shutdown）是 `null`。🚫 不省略。
    pub id: Option<u64>,
}

impl Response {
    pub fn ok(id: Option<u64>, result: Value) -> Response {
        Response {
            code: 0,
            msg: "ok".to_string(),
            result,
            id,
        }
    }

    pub fn error(id: Option<u64>, code: u32, msg: impl Into<String>) -> Response {
        Response {
            code,
            msg: msg.into(),
            result: Value::Null,
            id,
        }
    }

    /// core 的錯誤 → `code` 用 [`wbf_core::CoreErrorKind::rpc_code`]（那張表只在 core 那邊）。
    pub fn from_core_error(id: Option<u64>, error: &CoreError) -> Response {
        Response::error(id, error.kind.rpc_code(), error.message.clone())
    }

    /// 協議層的 close 通知（rpc-spec §1.4）：同一個形狀，`code` 9xxx，`result.close` 給人讀 log。
    pub fn close(id: Option<u64>, reason: CloseReason, msg: impl Into<String>) -> Response {
        Response {
            code: reason.code(),
            msg: msg.into(),
            result: serde_json::json!({ "close": reason.name() }),
            id,
        }
    }
}

/// 請求層的 RPC 錯誤（rpc-spec §5.1）：daemon 自己擋下、沒碰 core、連線照用。
pub mod code {
    pub const OK: u32 = 0;
    pub const BAD_REQUEST: u32 = 100;
    pub const UNKNOWN_METHOD: u32 = 101;
    pub const INVALID_PARAMS: u32 = 102;
    pub const CANCELLED: u32 = 105;
    pub const BUSY: u32 = 106;
    pub const DAEMON_SHUTTING_DOWN: u32 = 107;
    /// daemon 自己組不出回應（它的 bug）。🚫 不是前端的錯，所以🚫 不關連線。
    pub const INTERNAL: u32 = 108;
}

/// 協議層：這條連線本身出了問題，回完就關（rpc-spec §1.4）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloseReason {
    BadToken,
    BadFrame,
    HelloRequired,
    BadClient,
    ProtocolMismatch,
    ShuttingDown,
}

impl CloseReason {
    /// `9000–9099`，定了就不改。
    pub fn code(self) -> u32 {
        match self {
            CloseReason::BadToken => 9001,
            CloseReason::BadFrame => 9002,
            CloseReason::HelloRequired => 9003,
            CloseReason::BadClient => 9004,
            CloseReason::ProtocolMismatch => 9005,
            CloseReason::ShuttingDown => 9006,
        }
    }

    /// `result.close` 的字串：大寫底線，給人讀 log 用；前端判斷用 `code`。
    pub fn name(self) -> &'static str {
        match self {
            CloseReason::BadToken => "BAD_TOKEN",
            CloseReason::BadFrame => "BAD_FRAME",
            CloseReason::HelloRequired => "HELLO_REQUIRED",
            CloseReason::BadClient => "BAD_CLIENT",
            CloseReason::ProtocolMismatch => "PROTOCOL_MISMATCH",
            CloseReason::ShuttingDown => "SHUTTING_DOWN",
        }
    }

    /// 之後緊接的 WS close frame 的 status。
    pub fn ws_close_status(self) -> u16 {
        match self {
            // 1001 going away
            CloseReason::ShuttingDown => 1001,
            // 1008 policy violation
            _ => 1008,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wbf_core::CoreErrorKind;

    #[test]
    fn a_failed_response_keeps_result_as_explicit_null() {
        let json =
            serde_json::to_value(Response::error(Some(3), code::UNKNOWN_METHOD, "no")).unwrap();
        assert_eq!(
            json,
            serde_json::json!({ "code": 101, "msg": "no", "result": null, "id": 3 })
        );
    }

    #[test]
    fn a_close_notice_has_the_same_shape_as_any_response() {
        let json =
            serde_json::to_value(Response::close(None, CloseReason::BadToken, "nope")).unwrap();
        assert_eq!(
            json,
            serde_json::json!({ "code": 9001, "msg": "nope", "result": { "close": "BAD_TOKEN" }, "id": null })
        );
    }

    #[test]
    fn core_errors_carry_the_core_code() {
        let error = CoreError::new(CoreErrorKind::Locked, "locked");
        assert_eq!(Response::from_core_error(Some(1), &error).code, 1001);
    }

    #[test]
    fn close_codes_are_the_ones_in_rpc_spec_section_1_4() {
        let table = [
            (CloseReason::BadToken, 9001, "BAD_TOKEN"),
            (CloseReason::BadFrame, 9002, "BAD_FRAME"),
            (CloseReason::HelloRequired, 9003, "HELLO_REQUIRED"),
            (CloseReason::BadClient, 9004, "BAD_CLIENT"),
            (CloseReason::ProtocolMismatch, 9005, "PROTOCOL_MISMATCH"),
            (CloseReason::ShuttingDown, 9006, "SHUTTING_DOWN"),
        ];
        for (reason, code, name) in table {
            assert_eq!(reason.code(), code);
            assert_eq!(reason.name(), name);
        }
        assert_eq!(CloseReason::ShuttingDown.ws_close_status(), 1001);
        assert_eq!(CloseReason::BadToken.ws_close_status(), 1008);
    }

    #[test]
    fn a_request_without_id_is_a_push_and_serialises_without_the_field() {
        let push = Request::push("progress", serde_json::json!({ "id": 7 }));
        assert_eq!(
            serde_json::to_string(&push).unwrap(),
            r#"{"method":"progress","params":{"id":7}}"#
        );
        let parsed: Request = serde_json::from_str(r#"{"method":"hello","id":0}"#).unwrap();
        assert_eq!(parsed.id, Some(0));
        assert_eq!(parsed.params, Value::Null);
    }
}

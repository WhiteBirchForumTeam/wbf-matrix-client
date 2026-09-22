//! 純 HTTP 的 Matrix 登入（CLI 規格 §1）：`POST /_matrix/client/v3/login`、`logout`、`whoami`。
//! 不拖 matrix-sdk；第 3 步接 matrix-sdk 後這裡仍是「拿 token」的最短路。

use serde::{Deserialize, Serialize};

use crate::error::SdkError;

/// CLI 規格 §7 的 session 檔內容。🚫 `access_token` 不印、不 log、不進錯誤訊息。
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Session {
    pub server: String,
    pub user_id: String,
    pub device_id: String,
    pub access_token: String,
    /// matrix-sdk 的 store 目錄（crypto 與 state 兩個 sqlite）；純 HTTP 登入的 session 沒有（CLI 規格 §7）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub store_dir: Option<String>,
    /// 登入時探到的那一邊（account-session.md §2）：這個帳號之後的命令走哪一套。
    /// None ＝ 舊版封的、或用 token 接的：消費端用 `store_dir` 與探活自己判（不改舊行為）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend: Option<SessionBackend>,
}

/// 登入時定下的那一邊（account-session.md §2）。
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SessionBackend {
    /// 一般 Matrix：matrix-sdk 的 Client，`m/` 裡有 state store 與 crypto store。
    MatrixSdkClient,
    /// wbf server：沒有 Client，`m/` 只有 `OlmEngine` 開的 crypto store；房間、訊息、媒體、金鑰全走 WS。
    WbfSdk,
}

#[derive(Deserialize)]
struct LoginResponse {
    user_id: String,
    device_id: String,
    access_token: String,
}

#[derive(Deserialize)]
struct MatrixError {
    errcode: String,
    error: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct Whoami {
    pub user_id: String,
    pub device_id: String,
}

/// Args:
///     server: example: "http://localhost:6167"
///     user: mxid 或 localpart, example: "@alice:localhost"
///     password: 🚫 不印、不 log
///     device_name: example: "wbf-cli"
/// Return:
///     Ok(Session)
///     Err(SdkError)     `Server`（M_FORBIDDEN 等，code 是 errcode）、`Network`
pub async fn login_with_password(
    server: &str,
    user: &str,
    password: &str,
    device_name: &str,
) -> Result<Session, SdkError> {
    let body = serde_json::json!({
        "type": "m.login.password",
        "identifier": { "type": "m.id.user", "user": user },
        "password": password,
        "initial_device_display_name": device_name,
    });
    let response = reqwest::Client::new()
        .post(matrix_url(server, "login"))
        .json(&body)
        .send()
        .await
        .map_err(|error| SdkError::Network(format!("login: {error}")))?;
    let login: LoginResponse = parse_matrix_response(response).await?;
    Ok(Session {
        server: server.trim_end_matches('/').to_string(),
        user_id: login.user_id,
        device_id: login.device_id,
        access_token: login.access_token,
        store_dir: None,
        backend: None,
    })
}

/// `POST /_matrix/client/v3/logout`：token 失效。
pub async fn logout(session: &Session) -> Result<(), SdkError> {
    let response = reqwest::Client::new()
        .post(matrix_url(&session.server, "logout"))
        .bearer_auth(&session.access_token)
        .json(&serde_json::json!({}))
        .send()
        .await
        .map_err(|error| SdkError::Network(format!("logout: {error}")))?;
    let _: serde_json::Value = parse_matrix_response(response).await?;
    Ok(())
}

/// `GET /_matrix/client/v3/account/whoami`。
pub async fn whoami(session: &Session) -> Result<Whoami, SdkError> {
    let response = reqwest::Client::new()
        .get(matrix_url(&session.server, "account/whoami"))
        .bearer_auth(&session.access_token)
        .send()
        .await
        .map_err(|error| SdkError::Network(format!("whoami: {error}")))?;
    parse_matrix_response(response).await
}

fn matrix_url(server: &str, path: &str) -> String {
    format!("{}/_matrix/client/v3/{path}", server.trim_end_matches('/'))
}

/// 2xx 解成 T；其他狀態碼解 Matrix 的 `{ errcode, error }` 變 `SdkError::Server`。
async fn parse_matrix_response<T: serde::de::DeserializeOwned>(
    response: reqwest::Response,
) -> Result<T, SdkError> {
    let status = response.status();
    let body = response
        .bytes()
        .await
        .map_err(|error| SdkError::Network(format!("response body: {error}")))?;
    if status.is_success() {
        return serde_json::from_slice(&body)
            .map_err(|error| SdkError::Protocol(format!("matrix response: {error}")));
    }
    let (code, message) = match serde_json::from_slice::<MatrixError>(&body) {
        Ok(matrix_error) => (matrix_error.errcode, matrix_error.error.unwrap_or_default()),
        Err(_) => (
            format!("HTTP_{}", status.as_u16()),
            String::from_utf8_lossy(&body).into_owned(),
        ),
    };
    Err(SdkError::Server {
        code,
        message,
        meta: serde_json::Value::Null,
        // 🚫 不是 wbf `Error` pack 來的：沒有 `code_id`（`wbf_code()` 因此是 None）。
        code_id: None,
    })
}

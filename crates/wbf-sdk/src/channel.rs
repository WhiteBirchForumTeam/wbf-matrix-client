//! 一個 pack 進、一個 pack 出的通道（線上規格 §1）：WebSocket 主要、HTTP 給測試與腳本。
//!
//! 兩者都是「送一個、等一個」；連發與滑動窗口不在 v1（本機 64 KiB 一塊來回夠快，先求對）。

use futures_util::{SinkExt, StreamExt};
use http::header::AUTHORIZATION;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use wbf_wire::Pack;

use crate::error::SdkError;

/// 通道的契約。實作只管 bytes 來回；回應是不是 Ack、id／seq 對不對，是 `protocol::expect_ack` 的事。
#[allow(async_fn_in_trait)]
pub trait PackChannel {
    /// Return:
    ///     Ok(Pack)          對方回的那個 pack（可能是 Error pack，這裡不解讀）
    ///     Err(SdkError)     `Network`（送不出、收不到）、`Protocol`／`Integrity`（回來的 bytes 解不開）
    async fn request(&mut self, pack: Pack) -> Result<Pack, SdkError>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Transport {
    WebSocket,
    Http,
}

impl Transport {
    /// Args:
    ///     name: CLI `--transport` 的值, example: "ws"
    /// Return:
    ///     Some(Transport)  "ws" 或 "http"
    ///     None             其他
    pub fn from_name(name: &str) -> Option<Transport> {
        match name {
            "ws" => Some(Transport::WebSocket),
            "http" => Some(Transport::Http),
            _ => None,
        }
    }
}

/// 兩種通道的總和；`PackChannel` 不能做成 trait object（async fn），所以用 enum。
pub enum Channel {
    WebSocket(Box<WsChannel>),
    Http(HttpChannel),
}

impl Channel {
    /// Args:
    ///     server: homeserver base URL, example: "http://localhost:6167"
    ///     access_token: 🚫 不印、不 log
    ///     transport: example: Transport::WebSocket
    /// Return:
    ///     Ok(Channel)
    ///     Err(SdkError)     `Usage`（URL 壞掉）、`Network`（連不上）、`Server`（401：token 無效）
    pub async fn connect(
        server: &str,
        access_token: &str,
        transport: Transport,
    ) -> Result<Channel, SdkError> {
        match transport {
            Transport::WebSocket => Ok(Channel::WebSocket(Box::new(
                WsChannel::connect(server, access_token).await?,
            ))),
            Transport::Http => Ok(Channel::Http(HttpChannel::new(server, access_token)?)),
        }
    }
}

impl PackChannel for Channel {
    async fn request(&mut self, pack: Pack) -> Result<Pack, SdkError> {
        match self {
            Channel::WebSocket(channel) => channel.request(pack).await,
            Channel::Http(channel) => channel.request(pack).await,
        }
    }
}

/// `GET /_wbf/v1/ws`，一個 binary message 一個 pack。
pub struct WsChannel {
    socket: WebSocketStream<MaybeTlsStream<TcpStream>>,
}

impl WsChannel {
    pub async fn connect(server: &str, access_token: &str) -> Result<WsChannel, SdkError> {
        let url = ws_url(server)?;
        let mut request = url
            .as_str()
            .into_client_request()
            .map_err(|error| SdkError::Usage(format!("websocket url {url}: {error}")))?;
        let bearer = format!("Bearer {access_token}").parse().map_err(|_| {
            SdkError::Usage("access token contains characters not allowed in a header".into())
        })?;
        request.headers_mut().insert(AUTHORIZATION, bearer);
        let (socket, _response) =
            tokio_tungstenite::connect_async(request)
                .await
                .map_err(|error| match error {
                    tokio_tungstenite::tungstenite::Error::Http(response)
                        if response.status() == 401 =>
                    {
                        SdkError::Server {
                            code: "Unauthorized".into(),
                            message: "websocket upgrade refused: token invalid".into(),
                            meta: serde_json::Value::Null,
                        }
                    }
                    other => SdkError::Network(format!("websocket connect {url}: {other}")),
                })?;
        Ok(WsChannel { socket })
    }
}

impl PackChannel for WsChannel {
    async fn request(&mut self, pack: Pack) -> Result<Pack, SdkError> {
        let bytes = pack.encode()?;
        self.socket
            .send(Message::Binary(bytes.into()))
            .await
            .map_err(|error| SdkError::Network(format!("websocket send: {error}")))?;
        loop {
            let message = self
                .socket
                .next()
                .await
                .ok_or_else(|| {
                    SdkError::Network("websocket closed before a response arrived".into())
                })?
                .map_err(|error| SdkError::Network(format!("websocket receive: {error}")))?;
            match message {
                Message::Binary(bytes) => return Ok(Pack::decode(&bytes)?),
                Message::Close(frame) => {
                    return Err(SdkError::Network(format!(
                        "websocket closed by server: {frame:?}"
                    )));
                }
                // ping／pong 由 tungstenite 自動回；文字 frame 不在協議裡，跳過等下一個。
                Message::Ping(_) | Message::Pong(_) | Message::Text(_) | Message::Frame(_) => {
                    continue
                }
            }
        }
    }
}

/// `POST /_wbf/v1/pack`，body 一個 pack。
pub struct HttpChannel {
    client: reqwest::Client,
    url: String,
    bearer: String,
}

impl HttpChannel {
    pub fn new(server: &str, access_token: &str) -> Result<HttpChannel, SdkError> {
        Ok(HttpChannel {
            client: reqwest::Client::new(),
            url: format!("{}/_wbf/v1/pack", server.trim_end_matches('/')),
            bearer: format!("Bearer {access_token}"),
        })
    }
}

impl PackChannel for HttpChannel {
    async fn request(&mut self, pack: Pack) -> Result<Pack, SdkError> {
        let bytes = pack.encode()?;
        let response = self
            .client
            .post(&self.url)
            .header(AUTHORIZATION, &self.bearer)
            .header(http::header::CONTENT_TYPE, "application/octet-stream")
            .body(bytes)
            .send()
            .await
            .map_err(|error| SdkError::Network(format!("http pack: {error}")))?;
        let status = response.status();
        let body = response
            .bytes()
            .await
            .map_err(|error| SdkError::Network(format!("http pack body: {error}")))?;
        // 線上規格 §1：HTTP 一律 200，沒 token 401 但 body 仍是 pack。其他狀態碼是 server 之外的東西（proxy）在講話。
        if status.as_u16() != 200 && status.as_u16() != 401 {
            return Err(SdkError::Network(format!("http pack: status {status}")));
        }
        Ok(Pack::decode(&body)?)
    }
}

/// Args:
///     server: example: "http://localhost:6167"
/// Return:
///     Ok(String)   example: "ws://localhost:6167/_wbf/v1/ws"
///     Err(Usage)   不是 http:// 或 https://
fn ws_url(server: &str) -> Result<String, SdkError> {
    let server = server.trim_end_matches('/');
    let (scheme, rest) = server
        .split_once("://")
        .ok_or_else(|| SdkError::Usage(format!("server url has no scheme: {server}")))?;
    let ws_scheme = match scheme {
        "http" => "ws",
        "https" => "wss",
        other => {
            return Err(SdkError::Usage(format!(
                "server url scheme {other} is not http or https"
            )))
        }
    };
    Ok(format!("{ws_scheme}://{rest}/_wbf/v1/ws"))
}

#[cfg(test)]
mod tests {
    use super::ws_url;

    #[test]
    fn ws_url_maps_scheme_and_strips_trailing_slash() {
        assert_eq!(
            ws_url("http://localhost:6167/").unwrap(),
            "ws://localhost:6167/_wbf/v1/ws"
        );
        assert_eq!(
            ws_url("https://example.org").unwrap(),
            "wss://example.org/_wbf/v1/ws"
        );
        assert!(ws_url("localhost:6167").is_err());
        assert!(ws_url("ftp://x").is_err());
    }
}

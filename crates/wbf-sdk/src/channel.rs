//! 一個 pack 進、一個 pack 出的通道（線上規格 §1）：WebSocket 主要、HTTP 給測試與腳本。
//!
//! 兩者都是「送一個、等一個」；連發與滑動窗口不在 v1（本機 64 KiB 一塊來回夠快，先求對）。
//! 唯一的例外是 `request_stream`：`Event/Recent` 的回應是一串 `Batch`（pack-pipeline §6），送一個、收到呼叫者說停。

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use http::header::AUTHORIZATION;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use wbf_wire::Pack;

use crate::error::SdkError;

/// 一個請求從送出到收到回應的上限。與 server 的 `wbf_ws_idle_timeout` 預設相同：對方黑洞了就回 `Network`，
/// 不讓 client 永遠掛著。
pub const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

/// 通道的契約。實作只管 bytes 來回；回應是不是 Ack、id／seq 對不對，是 `protocol::expect_ack` 的事。
#[allow(async_fn_in_trait)]
pub trait PackChannel {
    /// Return:
    ///     Ok(Pack)          對方回的那個 pack（可能是 Error pack，這裡不解讀）
    ///     Err(SdkError)     `Network`（送不出、收不到）、`Protocol`／`Integrity`（回來的 bytes 解不開）
    async fn request(&mut self, pack: Pack) -> Result<Pack, SdkError>;

    /// 送一個、收多個：每收到一個 pack 就叫 `on_pack`，它回 `Ok(true)` 繼續等下一個、`Ok(false)` 停。
    /// 每個 pack 之間最多等 `per_pack_timeout`。HTTP 一次只回一個 pack，所以只叫一次 `on_pack`。
    ///
    /// Return:
    ///     Ok(())            `on_pack` 說停了
    ///     Err(SdkError)     `Network`（逾時、斷線）、`on_pack` 回的錯原樣往上
    async fn request_stream(
        &mut self,
        pack: Pack,
        per_pack_timeout: Duration,
        on_pack: &mut (dyn FnMut(Pack) -> Result<bool, SdkError> + Send),
    ) -> Result<(), SdkError>;
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

    async fn request_stream(
        &mut self,
        pack: Pack,
        per_pack_timeout: Duration,
        on_pack: &mut (dyn FnMut(Pack) -> Result<bool, SdkError> + Send),
    ) -> Result<(), SdkError> {
        match self {
            Channel::WebSocket(channel) => {
                channel
                    .request_stream(pack, per_pack_timeout, on_pack)
                    .await
            }
            Channel::Http(channel) => {
                channel
                    .request_stream(pack, per_pack_timeout, on_pack)
                    .await
            }
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

impl WsChannel {
    async fn send_pack(&mut self, pack: Pack) -> Result<(), SdkError> {
        let bytes = pack.encode()?;
        tokio::time::timeout(
            REQUEST_TIMEOUT,
            self.socket.send(Message::Binary(bytes.into())),
        )
        .await
        .map_err(|_| SdkError::Network("websocket send timed out".into()))?
        .map_err(|error| SdkError::Network(format!("websocket send: {error}")))
    }

    /// 等下一個 binary message；ping／pong／文字 frame 跳過。
    async fn receive_pack(&mut self, timeout: Duration) -> Result<Pack, SdkError> {
        loop {
            let message = tokio::time::timeout(timeout, self.socket.next())
                .await
                .map_err(|_| SdkError::Network("websocket response timed out".into()))?
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

impl PackChannel for WsChannel {
    async fn request(&mut self, pack: Pack) -> Result<Pack, SdkError> {
        self.send_pack(pack).await?;
        self.receive_pack(REQUEST_TIMEOUT).await
    }

    async fn request_stream(
        &mut self,
        pack: Pack,
        per_pack_timeout: Duration,
        on_pack: &mut (dyn FnMut(Pack) -> Result<bool, SdkError> + Send),
    ) -> Result<(), SdkError> {
        self.send_pack(pack).await?;
        loop {
            let response = self.receive_pack(per_pack_timeout).await?;
            if !on_pack(response)? {
                return Ok(());
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
        let client = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|error| SdkError::Network(format!("http client: {error}")))?;
        Ok(HttpChannel {
            client,
            url: format!("{}/_wbf/v1/pack", server.trim_end_matches('/')),
            bearer: format!("Bearer {access_token}"),
        })
    }
}

impl PackChannel for HttpChannel {
    /// HTTP 一請求一回應：串流的請求（`Recent`）server 會回 `Error(Unsupported)`，這裡照樣交給 `on_pack` 去判。
    /// `on_pack` 說「還要」就是錯：這條通道給不出第二個 pack，不能無聲當成功（PR #16 審查 rumia 🟢4）。
    async fn request_stream(
        &mut self,
        pack: Pack,
        _per_pack_timeout: Duration,
        on_pack: &mut (dyn FnMut(Pack) -> Result<bool, SdkError> + Send),
    ) -> Result<(), SdkError> {
        let response = self.request(pack).await?;
        if on_pack(response)? {
            return Err(SdkError::Protocol(
                "the HTTP channel served a single pack but the caller wanted more; use the WebSocket channel".into(),
            ));
        }
        Ok(())
    }

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
        match status.as_u16() {
            200 => Ok(Pack::decode(&body)?),
            // body 是 pack 就照 pack 的 Error 走；不是（proxy 的 401 頁）也要是 Unauthorized，跟 WebSocket 升級被拒同一個分類。
            401 => Pack::decode(&body).map_err(|_| SdkError::Server {
                code: "Unauthorized".into(),
                message: "http pack refused: token invalid".into(),
                meta: serde_json::Value::Null,
            }),
            _ => Err(SdkError::Network(format!("http pack: status {status}"))),
        }
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

//! 通道（線上規格 §1）：WebSocket 主要、HTTP 給測試與腳本。
//!
//! `PackChannel` 的契約是「送一個、等一個」與「送一個、收到呼叫者說停」（`request_stream`：`Event/Recent` 的回應是一串 `Batch`）。
//! WebSocket 底下是 `link::WsLink`（ws-receive-dispatch.md）：送與收是兩個 task，回覆依會話表交付，所以推播與回覆交錯、順序亂掉都不出事；
//! 長活的訂閱走 `subscribe`。HTTP 一請求一回應，不能訂閱。

use std::time::Duration;

use http::header::AUTHORIZATION;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use wbf_wire::Pack;

use crate::error::SdkError;
use crate::link::{Heartbeat, Subscription, WsLink};
use crate::sessions::ReceivedHook;

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

    /// 訂閱：長活的會話，之後抄這個 id 的每個 pack（`Ack`、`CryptoState`、`Push`、`Superseded`）都從 handle 來（ws-receive-dispatch.md §3）。
    /// 預設回 `Usage`：只有 WebSocket 通道能收推播。
    ///
    /// Return:
    ///     Ok(Subscription)
    ///     Err(Usage)        這種通道不能長活收、或 pack 的 id 是 0
    ///     Err(Network)      送不出去
    async fn subscribe(&mut self, pack: Pack) -> Result<Subscription, SdkError> {
        let _ = pack;
        Err(SdkError::Usage(
            "this channel cannot hold a subscription; use the WebSocket channel".into(),
        ))
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Transport {
    /// 🚨 **預設**：沒指定就是這條（維護者 2026-09-13）。⭐ 這個 `Default` 是
    /// 「沒帶 transport 要用什麼」的**唯一一份答案** —— 🚫 不要在別的層再寫一次，
    /// 同一個預設有兩個地方決定，遲早只有一邊被改到。
    #[default]
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

impl Channel {
    /// Return:
    ///     Some(n)   WebSocket：這條連線至今收到幾個沒人等的 pack（ws-receive-dispatch.md §2.1 第 4 條）
    ///     None      HTTP：沒有這個數（一請求一回應）
    pub fn unmatched(&self) -> Option<u64> {
        match self {
            Channel::WebSocket(channel) => Some(channel.link().unmatched()),
            Channel::Http(_) => None,
        }
    }

    /// Return:
    ///     bool  1 = WebSocket 那條線關了（之後每個請求都回 Network；連線池拿這個決定要不要重開）。HTTP 永遠是 0：它沒有「開著」這回事
    pub fn is_closed(&self) -> bool {
        match self {
            Channel::WebSocket(channel) => channel.link().is_closed(),
            Channel::Http(_) => false,
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

    async fn subscribe(&mut self, pack: Pack) -> Result<Subscription, SdkError> {
        match self {
            Channel::WebSocket(channel) => channel.subscribe(pack).await,
            Channel::Http(channel) => channel.subscribe(pack).await,
        }
    }
}

/// `GET /_wbf/v1/ws`：一條 `WsLink`（讀取 task ＋ 送出 task ＋ 會話表，ws-receive-dispatch.md）。
/// `PackChannel` 的兩個方法在它上面是「登記 → 送 → 等」；推播與 `Superseded` 走 `subscribe`。
pub struct WsChannel {
    link: WsLink,
}

impl WsChannel {
    pub async fn connect(server: &str, access_token: &str) -> Result<WsChannel, SdkError> {
        WsChannel::connect_with_hook(server, access_token, crate::sessions::no_hook()).await
    }

    /// 同上，每收一個 pack 叫一次 `hook`（ws-receive-dispatch.md §4：之後 daemon 的 RPC 面用它決定要不要送到 UI；這裡只呼叫）。心跳是預設的（24 秒）。
    pub async fn connect_with_hook(
        server: &str,
        access_token: &str,
        hook: ReceivedHook,
    ) -> Result<WsChannel, SdkError> {
        WsChannel::connect_with_heartbeat(server, access_token, hook, Heartbeat::DEFAULT).await
    }

    /// 同上，心跳的間隔自己給（ws-receive-dispatch.md §5.1；測試對真 server 用短的）。
    pub async fn connect_with_heartbeat(
        server: &str,
        access_token: &str,
        hook: ReceivedHook,
        heartbeat: Heartbeat,
    ) -> Result<WsChannel, SdkError> {
        WsChannel::connect_inner(server, Some(access_token), hook, heartbeat).await
    }

    /// **不帶 token** 的連線：只給探活（account-session.md §1）。server 允許未登入的升級，30 秒內只接受 `Hello`／`Ping`，
    /// 之後自己關；所以拿到 `Hello` 的答案就把它丟掉。🚫 不要拿它做別的事。
    pub async fn connect_anonymous(server: &str) -> Result<WsChannel, SdkError> {
        WsChannel::connect_inner(server, None, crate::sessions::no_hook(), Heartbeat::OFF).await
    }

    async fn connect_inner(
        server: &str,
        access_token: Option<&str>,
        hook: ReceivedHook,
        heartbeat: Heartbeat,
    ) -> Result<WsChannel, SdkError> {
        let url = ws_url(server)?;
        let mut request = url
            .as_str()
            .into_client_request()
            .map_err(|error| SdkError::Usage(format!("websocket url {url}: {error}")))?;
        if let Some(access_token) = access_token {
            let bearer = format!("Bearer {access_token}").parse().map_err(|_| {
                SdkError::Usage("access token contains characters not allowed in a header".into())
            })?;
            request.headers_mut().insert(AUTHORIZATION, bearer);
        }
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
                            // 🚫 不是 wbf `Error` pack 來的：沒有 `code_id`（`wbf_code()` 因此是 None）。
                            code_id: None,
                        }
                    }
                    other => SdkError::Network(format!("websocket connect {url}: {other}")),
                })?;
        let (source, sink) = crate::transport::split_socket(socket);
        Ok(WsChannel {
            link: WsLink::start_with_heartbeat(source, sink, hook, heartbeat),
        })
    }

    /// 底下那條連線：診斷（`unmatched`、`is_closed`）與需要 `AckPolicy` 的呼叫點用。
    pub fn link(&self) -> &WsLink {
        &self.link
    }

    /// 拿一條已經起好的 `WsLink` 當通道：給測試（`transport::memory_pair` 對接）與自己採傳輸的嵌入者用。正式的路是 `connect`。
    pub fn from_link(link: WsLink) -> WsChannel {
        WsChannel { link }
    }
}

impl PackChannel for WsChannel {
    async fn request(&mut self, pack: Pack) -> Result<Pack, SdkError> {
        self.link.request(pack, REQUEST_TIMEOUT).await
    }

    async fn request_stream(
        &mut self,
        pack: Pack,
        per_pack_timeout: Duration,
        on_pack: &mut (dyn FnMut(Pack) -> Result<bool, SdkError> + Send),
    ) -> Result<(), SdkError> {
        let mut stream = self.link.open_stream(pack).await?;
        loop {
            match stream.next(per_pack_timeout).await? {
                Some(response) => {
                    if !on_pack(response)? {
                        return Ok(());
                    }
                }
                None => {
                    return Err(SdkError::Protocol(format!(
                        "session {:?} ended before the caller was done with it",
                        stream.key()
                    )))
                }
            }
        }
    }

    async fn subscribe(&mut self, pack: Pack) -> Result<Subscription, SdkError> {
        self.link.subscribe(pack).await
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
                // 🚫 不是 wbf `Error` pack 來的：沒有 `code_id`（`wbf_code()` 因此是 None）。
                code_id: None,
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

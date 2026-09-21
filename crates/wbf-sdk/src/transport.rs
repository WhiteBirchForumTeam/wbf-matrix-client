//! bytes 怎麼進出：一個 binary frame 一個 pack 的 bytes（ws-receive-dispatch.md §1）。
//!
//! 這層🚫 不知道什麼是 pack：收到的是 `Vec<u8>`，送出的也是。兩組實作：tungstenite 的 WebSocket（正式），
//! 與記憶體對接（`memory_pair`，給測試餵亂序的 pack）。讀與寫是兩個 trait，因為它們住在兩個 task 裡（`link.rs`）。

use std::future::Future;

use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use crate::error::SdkError;

/// 收 frame 的那一半。
pub trait FrameSource: Send + 'static {
    /// Return:
    ///     Ok(Some(bytes))   一個 binary frame 的內容
    ///     Ok(None)          對方把連線收乾淨了（之後不會再有）
    ///     Err(Network)      連線壞了（含對方送 Close frame 帶理由：理由在訊息裡）
    fn receive(&mut self) -> impl Future<Output = Result<Option<Vec<u8>>, SdkError>> + Send;
}

/// 送 frame 的那一半。
pub trait FrameSink: Send + 'static {
    /// Return:
    ///     Ok(())            寫進去了（不代表對方收到）
    ///     Err(Network)      連線壞了
    fn send(&mut self, bytes: Vec<u8>) -> impl Future<Output = Result<(), SdkError>> + Send;
}

pub type WsSocket = WebSocketStream<MaybeTlsStream<TcpStream>>;

pub struct WsFrameSource(SplitStream<WsSocket>);
pub struct WsFrameSink(SplitSink<WsSocket, Message>);

/// 把一條連好的 WebSocket 拆成收與送兩半。
pub fn split_socket(socket: WsSocket) -> (WsFrameSource, WsFrameSink) {
    let (sink, stream) = socket.split();
    (WsFrameSource(stream), WsFrameSink(sink))
}

impl FrameSource for WsFrameSource {
    /// ping／pong 由 tungstenite 自動回；文字 frame 不在協議裡，跳過等下一個。
    async fn receive(&mut self) -> Result<Option<Vec<u8>>, SdkError> {
        loop {
            match self.0.next().await {
                None => return Ok(None),
                Some(Err(error)) => {
                    return Err(SdkError::Network(format!("websocket receive: {error}")))
                }
                Some(Ok(Message::Binary(bytes))) => return Ok(Some(bytes.to_vec())),
                Some(Ok(Message::Close(frame))) => {
                    return Err(SdkError::Network(format!(
                        "websocket closed by server: {frame:?}"
                    )))
                }
                Some(Ok(
                    Message::Ping(_) | Message::Pong(_) | Message::Text(_) | Message::Frame(_),
                )) => continue,
            }
        }
    }
}

impl FrameSink for WsFrameSink {
    async fn send(&mut self, bytes: Vec<u8>) -> Result<(), SdkError> {
        self.0
            .send(Message::Binary(bytes.into()))
            .await
            .map_err(|error| SdkError::Network(format!("websocket send: {error}")))
    }
}

/// 記憶體對接的一端：`source` 收對面送的，`sink` 送給對面。
pub struct MemoryEnd {
    pub source: MemoryFrameSource,
    pub sink: MemoryFrameSink,
}

pub struct MemoryFrameSource(mpsc::Receiver<Vec<u8>>);
pub struct MemoryFrameSink(mpsc::Sender<Vec<u8>>);

/// 兩端對接：一端給 `WsLink`，另一端給測試扮 server。任一端的 `sink` 被丟掉，對面的 `source` 就收到 `Ok(None)`（＝對方關了）。
///
/// Args:
///     capacity: 每個方向最多積幾個 frame, example: 64
/// Return:
///     (MemoryEnd, MemoryEnd)   (client 這端, server 那端)
pub fn memory_pair(capacity: usize) -> (MemoryEnd, MemoryEnd) {
    let (client_to_server, server_from_client) = mpsc::channel(capacity);
    let (server_to_client, client_from_server) = mpsc::channel(capacity);
    (
        MemoryEnd {
            source: MemoryFrameSource(client_from_server),
            sink: MemoryFrameSink(client_to_server),
        },
        MemoryEnd {
            source: MemoryFrameSource(server_from_client),
            sink: MemoryFrameSink(server_to_client),
        },
    )
}

impl FrameSource for MemoryFrameSource {
    async fn receive(&mut self) -> Result<Option<Vec<u8>>, SdkError> {
        Ok(self.0.recv().await)
    }
}

impl FrameSink for MemoryFrameSink {
    async fn send(&mut self, bytes: Vec<u8>) -> Result<(), SdkError> {
        self.0
            .send(bytes)
            .await
            .map_err(|_| SdkError::Network("memory transport: the peer is gone".into()))
    }
}

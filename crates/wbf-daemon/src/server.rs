//! 控制平面的 listener：`ws://127.0.0.1:<port>`，一條連線一個 `Connection` 加一個 writer task。
//!
//! 這裡只做 socket 與 frame 的搬運；判斷全在 `connection.rs`（該不該加密、hello）與
//! `handle.rs`（method）。收到不是 binary 的 frame 一律 `BAD_FRAME`（rpc-spec §1：沒有 text frame）。

use std::net::SocketAddr;
use std::sync::Arc;

use futures_util::{SinkExt, StreamExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::Message;

use crate::connection::{Connection, EncryptionPolicy, Inbound};
use crate::handle::Handle;
use crate::message::{CloseReason, Response};
use crate::pack::RpcKeys;

pub struct RpcServer {
    listener: TcpListener,
    keys: Arc<RpcKeys>,
    policy: EncryptionPolicy,
    handle: Arc<Handle>,
}

impl RpcServer {
    /// 綁 loopback。`port` 給 0 就是隨機 port（`local_addr` 才知道）。
    pub async fn bind(
        port: u16,
        keys: Arc<RpcKeys>,
        policy: EncryptionPolicy,
        handle: Arc<Handle>,
    ) -> std::io::Result<RpcServer> {
        let listener = TcpListener::bind(("127.0.0.1", port)).await?;
        Ok(RpcServer {
            listener,
            keys,
            policy,
            handle,
        })
    }

    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    /// 一直 accept，直到 `daemon.shutdown`。
    pub async fn run(self) {
        let mut shutdown = self.handle.shutdown_signal();
        loop {
            tokio::select! {
                accepted = self.listener.accept() => {
                    let Ok((stream, _)) = accepted else { continue };
                    tokio::spawn(serve_connection(
                        stream,
                        self.keys.clone(),
                        self.policy.clone(),
                        self.handle.clone(),
                    ));
                }
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        return;
                    }
                }
            }
        }
    }
}

/// 送出去的東西：一般的包，或「送這個之後關」。
enum Outgoing {
    Frame(Vec<u8>),
    CloseAfter(Vec<u8>, CloseReason),
}

async fn serve_connection(
    stream: TcpStream,
    keys: Arc<RpcKeys>,
    policy: EncryptionPolicy,
    handle: Arc<Handle>,
) {
    let Ok(websocket) = tokio_tungstenite::accept_async(stream).await else {
        return;
    };
    let _counted = handle.connection_opened();
    let (mut sink, mut source) = websocket.split();
    let (outgoing_tx, mut outgoing_rx) = mpsc::channel::<Outgoing>(64);
    let mut shutdown = handle.shutdown_signal();

    let writer = tokio::spawn(async move {
        while let Some(outgoing) = outgoing_rx.recv().await {
            match outgoing {
                Outgoing::Frame(bytes) => {
                    if sink.send(Message::Binary(bytes.into())).await.is_err() {
                        return;
                    }
                }
                Outgoing::CloseAfter(bytes, reason) => {
                    let _ = sink.send(Message::Binary(bytes.into())).await;
                    let _ = sink
                        .send(Message::Close(Some(CloseFrame {
                            code: CloseCode::from(reason.ws_close_status()),
                            reason: reason.name().into(),
                        })))
                        .await;
                    return;
                }
            }
        }
    });

    // `Connection` 由這個 task 獨佔；handle 的呼叫各自 spawn，回應經 channel 回來再由這裡封包。
    let connection = Arc::new(tokio::sync::Mutex::new(Connection::new(keys, policy)));
    loop {
        let message = tokio::select! {
            next = source.next() => next,
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    let notice = Response::close(None, CloseReason::ShuttingDown, "the daemon is shutting down");
                    let bytes = connection.lock().await.seal_close(&notice);
                    let _ = outgoing_tx.send(Outgoing::CloseAfter(bytes, CloseReason::ShuttingDown)).await;
                    break;
                }
                continue;
            }
        };
        let Some(Ok(message)) = message else { break };
        let frame = match message {
            Message::Binary(bytes) => bytes.to_vec(),
            Message::Close(_) => break,
            Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => continue,
            Message::Text(_) => {
                let notice = Response::close(
                    None,
                    CloseReason::BadFrame,
                    "text frames are not part of the protocol",
                );
                let bytes = connection.lock().await.seal_close(&notice);
                let _ = outgoing_tx
                    .send(Outgoing::CloseAfter(bytes, CloseReason::BadFrame))
                    .await;
                break;
            }
        };
        let inbound = connection.lock().await.receive(&frame);
        match inbound {
            Inbound::Close(notice) => {
                let reason = close_reason_of(&notice);
                let bytes = connection.lock().await.seal_close(&notice);
                let _ = outgoing_tx.send(Outgoing::CloseAfter(bytes, reason)).await;
                break;
            }
            Inbound::HelloAccepted { id, protocol } => {
                let result = handle.hello_result(protocol).await;
                let bytes = connection
                    .lock()
                    .await
                    .seal_response(&Response::ok(id, result));
                if outgoing_tx.send(Outgoing::Frame(bytes)).await.is_err() {
                    break;
                }
            }
            Inbound::Request(request) => {
                let handle = handle.clone();
                let connection = connection.clone();
                let outgoing_tx = outgoing_tx.clone();
                tokio::spawn(async move {
                    let response = handle.call(request).await;
                    let bytes = connection.lock().await.seal_response(&response);
                    let _ = outgoing_tx.send(Outgoing::Frame(bytes)).await;
                    // 回應已經排進 writer 了，這時才廣播 shutdown：close 通知一定排在它後面。
                    handle.begin_shutdown_if_requested();
                });
            }
        }
    }
    drop(outgoing_tx);
    let _ = writer.await;
}

/// 從 close 通知的 `code` 反查 reason（給 WS close frame 的 status 用）。
fn close_reason_of(notice: &Response) -> CloseReason {
    [
        CloseReason::BadToken,
        CloseReason::BadFrame,
        CloseReason::HelloRequired,
        CloseReason::BadClient,
        CloseReason::ProtocolMismatch,
        CloseReason::ShuttingDown,
    ]
    .into_iter()
    .find(|reason| reason.code() == notice.code)
    .unwrap_or(CloseReason::BadFrame)
}

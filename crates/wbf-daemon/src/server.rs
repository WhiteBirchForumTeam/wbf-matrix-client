//! 控制平面的 listener：`ws://127.0.0.1:<port>`，一條連線一個 `Connection` 加一個 writer task。
//!
//! 這裡只做 socket 與 frame 的搬運；判斷全在 `connection.rs`（該不該加密、hello）與
//! `handle.rs`（method）。收到不是 binary 的 frame 一律 `BAD_FRAME`（rpc-spec §1：沒有 text frame）。

use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::Arc;

use futures_util::{SinkExt, StreamExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, mpsc};
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::Message;

use crate::connection::{Connection, EncryptionPolicy, Inbound};
use crate::handle::Handle;
use crate::message::{code, CloseReason, Request, Response};
use crate::pack::RpcKeys;
use crate::push::{self, Subscriptions};

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
    // 這條連線訂了什麼、現在有哪些請求在跑（rpc-spec §3.9、§4：`progress` 不用訂，發那個請求的連線自己收得到）。
    // ⚠️ std 的 Mutex：只在同步的一小段裡拿，🚫 不跨 await。
    let subscriptions = Arc::new(std::sync::Mutex::new(Subscriptions::default()));
    let in_flight = Arc::new(std::sync::Mutex::new(HashSet::<u64>::new()));
    let push_task = tokio::spawn(forward_pushes(
        handle.clone(),
        connection.clone(),
        outgoing_tx.clone(),
        subscriptions.clone(),
        in_flight.clone(),
    ));
    loop {
        let message = tokio::select! {
            next = source.next() => next,
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    let notice = Response::close(None, CloseReason::ShuttingDown, "the daemon is shutting down");
                    if let Some(bytes) = sealed_or_log(connection.lock().await.seal_close(&notice), "the close notice") {
                        let _ = outgoing_tx.send(Outgoing::CloseAfter(bytes, CloseReason::ShuttingDown)).await;
                    }
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
                if let Some(bytes) = sealed_or_log(
                    connection.lock().await.seal_close(&notice),
                    "the close notice",
                ) {
                    let _ = outgoing_tx
                        .send(Outgoing::CloseAfter(bytes, CloseReason::BadFrame))
                        .await;
                }
                break;
            }
        };
        let inbound = connection.lock().await.receive(&frame);
        match inbound {
            Inbound::Close(notice) => {
                let reason = close_reason_of(&notice);
                if let Some(bytes) = sealed_or_log(
                    connection.lock().await.seal_close(&notice),
                    "the close notice",
                ) {
                    let _ = outgoing_tx.send(Outgoing::CloseAfter(bytes, reason)).await;
                }
                break;
            }
            Inbound::Reply(response) => {
                let Some(bytes) =
                    sealed_or_log(connection.lock().await.seal_response(&response), "a reply")
                else {
                    continue;
                };
                if outgoing_tx.send(Outgoing::Frame(bytes)).await.is_err() {
                    break;
                }
            }
            Inbound::HelloAccepted { id, protocol } => {
                let result = handle.hello_result(protocol).await;
                let sealed = connection
                    .lock()
                    .await
                    .seal_response(&Response::ok(id, result));
                let Some(bytes) = sealed_or_log(sealed, "the hello reply") else {
                    continue;
                };
                if outgoing_tx.send(Outgoing::Frame(bytes)).await.is_err() {
                    break;
                }
            }
            Inbound::Request(request) => {
                // 訂閱是**這條連線**的事，不碰 core：在這裡就回，🚫 不進 handle。
                if request.method == "subscribe" || request.method == "unsubscribe" {
                    let response = subscription_response(&subscriptions, &request);
                    let sealed = connection.lock().await.seal_response(&response);
                    let Some(bytes) = sealed_or_log(sealed, "a subscribe reply") else {
                        continue;
                    };
                    if outgoing_tx.send(Outgoing::Frame(bytes)).await.is_err() {
                        break;
                    }
                    continue;
                }
                let handle = handle.clone();
                let connection = connection.clone();
                let outgoing_tx = outgoing_tx.clone();
                let in_flight = in_flight.clone();
                tokio::spawn(async move {
                    let response = match request.id {
                        // 請求的 `id` 就是它的 job：core 在這個工作裡發的 `progress` 都帶這個號，推播那邊拿它對回「是這條連線發的」。
                        Some(id) => {
                            in_flight
                                .lock()
                                .unwrap_or_else(|p| p.into_inner())
                                .insert(id);
                            let response =
                                wbf_core::job::run_as_job(id, handle.call(request)).await;
                            in_flight
                                .lock()
                                .unwrap_or_else(|p| p.into_inner())
                                .remove(&id);
                            response
                        }
                        None => handle.call(request).await,
                    };
                    let sealed = connection.lock().await.seal_response(&response);
                    if let Some(bytes) = sealed_or_log(sealed, "a response") {
                        let _ = outgoing_tx.send(Outgoing::Frame(bytes)).await;
                    }
                    // 回應已經排進 writer 了，這時才廣播 shutdown：close 通知一定排在它後面。
                    handle.begin_shutdown_if_requested();
                });
            }
        }
    }
    push_task.abort();
    drop(outgoing_tx);
    let _ = writer.await;
}

/// 封不起來（序列化不了、OS 給不出 nonce；理論上到不了）就講一聲、回 `None`：呼叫端當作沒東西可送，🚫 不炸掉這條連線。
///
/// Args:
///     what: 封的是什麼，給人看, example: "a push"
/// Return:
///     Some(Vec<u8>)  封好的 frame
///     None           封不起來，已經講過了
fn sealed_or_log(sealed: Result<Vec<u8>, std::io::Error>, what: &str) -> Option<Vec<u8>> {
    match sealed {
        Ok(bytes) => Some(bytes),
        Err(error) => {
            eprintln!("rpc: cannot seal {what}: {error}");
            None
        }
    }
}

/// 這條連線的推播 task（link-pool.md §6）：core 的每一則事件 → 要不要送由訂閱集合與「是不是自己發的工作」決定 → 封包送出。
/// 讀太慢被覆蓋掉 n 則 → 送 `desync { missed: n }`（daemon-runtime §5.3）。連線關了（writer 沒了）就結束。
async fn forward_pushes(
    handle: Arc<Handle>,
    connection: Arc<tokio::sync::Mutex<Connection>>,
    outgoing_tx: mpsc::Sender<Outgoing>,
    subscriptions: Arc<std::sync::Mutex<Subscriptions>>,
    in_flight: Arc<std::sync::Mutex<HashSet<u64>>>,
) {
    let core = handle.core().await;
    let mut events = core.subscribe();
    loop {
        let request = match events.recv().await {
            Ok(event) => {
                let push = push::push_of(&event);
                let is_my_job = push.job.is_some_and(|job| {
                    in_flight
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .contains(&job)
                });
                let wanted = subscriptions
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .wants(&push.request.method, push.user.as_deref());
                if !(is_my_job || wanted) {
                    continue;
                }
                push.request
            }
            // 什麼都沒訂的連線本來就收不到推播，漏了也沒有東西可漏：🚫 不發 `desync`（推播要先 subscribe，這則也不例外；PR #53 審查 rumia 🔴2）。
            Err(broadcast::error::RecvError::Lagged(missed)) => {
                let is_subscribed = !subscriptions
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .is_empty();
                if !is_subscribed {
                    continue;
                }
                push::desync(missed)
            }
            Err(broadcast::error::RecvError::Closed) => return,
        };
        let Some(bytes) = sealed_or_log(connection.lock().await.seal_push(&request), "a push")
        else {
            continue;
        };
        if outgoing_tx.send(Outgoing::Frame(bytes)).await.is_err() {
            return;
        }
    }
}

/// `subscribe`／`unsubscribe`（rpc-spec §3.9）：改這條連線的集合，回改完之後的。
fn subscription_response(
    subscriptions: &std::sync::Mutex<Subscriptions>,
    request: &Request,
) -> Response {
    let params = crate::connection::params_or_empty_object(&request.params);
    let (names, user) = match push::parse_subscription_params(&params) {
        Ok(parsed) => parsed,
        Err(message) => return Response::error(request.id, code::INVALID_PARAMS, message),
    };
    let mut subscriptions = subscriptions.lock().unwrap_or_else(|p| p.into_inner());
    let subscribed = match request.method.as_str() {
        "subscribe" => subscriptions.subscribe(&names, user),
        _ => subscriptions.unsubscribe(&names),
    };
    Response::ok(request.id, serde_json::json!({ "subscribed": subscribed }))
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

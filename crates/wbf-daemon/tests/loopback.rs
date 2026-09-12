//! 對真的 loopback WS 走一遍（rpc-spec §1）：hello、請求、token 錯、text frame、shutdown。
//! 這裡的 client 只用 `wbf_daemon::pack` 封包，其餘是 tokio-tungstenite 的原生 client——
//! 證明的是「另一個程序照規格寫就接得上」，不是「daemon 自己跟自己講話」。

use std::sync::Arc;

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::Message;
use wbf_daemon::connection::EncryptionPolicy;
use wbf_daemon::handle::Handle;
use wbf_daemon::pack::{self, PackType, RpcKeys, Side};
use wbf_daemon::server::RpcServer;

const TOKEN: [u8; 256] = [42u8; 256];

struct Daemon {
    port: u16,
    handle: Arc<Handle>,
    _dir: tempfile::TempDir,
}

async fn start_daemon() -> Daemon {
    let dir = tempfile::tempdir().unwrap();
    let policy = EncryptionPolicy::enforced();
    let handle = Handle::new(dir.path(), policy.clone());
    let server = RpcServer::bind(
        0,
        Arc::new(RpcKeys::from_token(&TOKEN)),
        policy,
        handle.clone(),
    )
    .await
    .unwrap();
    let port = server.local_addr().unwrap().port();
    handle.set_ports(port, 0).await;
    tokio::spawn(server.run());
    Daemon {
        port,
        handle,
        _dir: dir,
    }
}

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn connect(port: u16) -> Socket {
    let (socket, _) = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}"))
        .await
        .unwrap();
    socket
}

async fn send(socket: &mut Socket, keys: &RpcKeys, pack_type: PackType, json: Value) {
    let frame = pack::seal(keys, Side::Client, pack_type, json.to_string().as_bytes());
    socket.send(Message::Binary(frame.into())).await.unwrap();
}

/// 收下一包，回 (type, JSON)。
async fn receive(socket: &mut Socket, keys: &RpcKeys) -> (PackType, Value) {
    loop {
        match socket.next().await.unwrap().unwrap() {
            Message::Binary(bytes) => {
                let (pack_type, json) = pack::open(keys, Side::Client, &bytes).unwrap();
                return (pack_type, serde_json::from_slice(&json).unwrap());
            }
            Message::Ping(_) | Message::Pong(_) => continue,
            other => panic!("unexpected {other:?}"),
        }
    }
}

async fn expect_close(socket: &mut Socket) -> u16 {
    loop {
        match socket.next().await {
            Some(Ok(Message::Close(Some(frame)))) => return frame.code.into(),
            Some(Ok(Message::Close(None))) | None => return 0,
            Some(Ok(_)) => continue,
            Some(Err(error)) => panic!("{error}"),
        }
    }
}

fn hello() -> Value {
    json!({ "method": "hello", "params": { "protocols": [1], "client": "wbf-matrix-rpc-cli 0.0.0" }, "id": 0 })
}

#[tokio::test]
async fn hello_then_a_request_over_ciphertext() {
    let daemon = start_daemon().await;
    let keys = RpcKeys::from_token(&TOKEN);
    let mut socket = connect(daemon.port).await;
    send(&mut socket, &keys, PackType::Cipher, hello()).await;
    let (pack_type, reply) = receive(&mut socket, &keys).await;
    assert_eq!(pack_type, PackType::Cipher);
    assert_eq!(reply["code"], 0, "{reply}");
    assert_eq!(reply["id"], 0);
    assert_eq!(reply["result"]["protocol"], 1);
    assert_eq!(reply["result"]["unlocked"], false);
    assert_eq!(reply["result"]["encryption_enforced"], true);

    send(
        &mut socket,
        &keys,
        PackType::Cipher,
        json!({ "method": "daemon.info", "id": 7 }),
    )
    .await;
    let (_, reply) = receive(&mut socket, &keys).await;
    assert_eq!(reply["code"], 0, "{reply}");
    assert_eq!(reply["id"], 7);
    assert_eq!(reply["result"]["rpc_port"], daemon.port);
    assert_eq!(reply["result"]["protocols"], json!([1]));

    // 沒解鎖：帳號那些是 1001，而且連線還活著（請求層錯誤不關連線）。
    send(
        &mut socket,
        &keys,
        PackType::Cipher,
        json!({ "method": "account.list", "params": {}, "id": 8 }),
    )
    .await;
    let (_, reply) = receive(&mut socket, &keys).await;
    assert_eq!(reply["code"], 1001);
    assert_eq!(reply["result"], Value::Null);
    send(
        &mut socket,
        &keys,
        PackType::Cipher,
        json!({ "method": "daemon.info", "id": 9 }),
    )
    .await;
    let (_, reply) = receive(&mut socket, &keys).await;
    assert_eq!(reply["id"], 9);
}

#[tokio::test]
async fn a_wrong_token_gets_a_plaintext_notice_then_a_close_frame() {
    let daemon = start_daemon().await;
    let wrong = RpcKeys::from_token(&[1u8; 256]);
    let mut socket = connect(daemon.port).await;
    send(&mut socket, &wrong, PackType::Cipher, hello()).await;
    // 用錯的鑰也讀得到：因為它是明文。
    let (pack_type, notice) = receive(&mut socket, &wrong).await;
    assert_eq!(pack_type, PackType::Plain);
    assert_eq!(notice["code"], 9001);
    assert_eq!(notice["result"]["close"], "BAD_TOKEN");
    assert_eq!(notice["id"], Value::Null);
    assert_eq!(expect_close(&mut socket).await, 1008);
}

#[tokio::test]
async fn a_text_frame_and_a_request_before_hello_are_refused() {
    let daemon = start_daemon().await;
    let keys = RpcKeys::from_token(&TOKEN);
    let mut socket = connect(daemon.port).await;
    socket.send(Message::Text("{}".into())).await.unwrap();
    let (pack_type, notice) = receive(&mut socket, &keys).await;
    assert_eq!(pack_type, PackType::Plain);
    assert_eq!(notice["code"], 9002);
    assert_eq!(expect_close(&mut socket).await, 1008);

    let mut socket = connect(daemon.port).await;
    send(
        &mut socket,
        &keys,
        PackType::Cipher,
        json!({ "method": "daemon.info", "id": 3 }),
    )
    .await;
    let (_, notice) = receive(&mut socket, &keys).await;
    assert_eq!(notice["code"], 9003);
    assert_eq!(notice["id"], 3);
    assert_eq!(expect_close(&mut socket).await, 1008);
}

#[tokio::test]
async fn shutdown_notifies_open_connections_and_stops_accepting() {
    let daemon = start_daemon().await;
    let keys = RpcKeys::from_token(&TOKEN);
    let mut socket = connect(daemon.port).await;
    send(&mut socket, &keys, PackType::Cipher, hello()).await;
    receive(&mut socket, &keys).await;
    send(
        &mut socket,
        &keys,
        PackType::Cipher,
        json!({ "method": "daemon.shutdown", "id": 1 }),
    )
    .await;
    let (_, reply) = receive(&mut socket, &keys).await;
    assert_eq!(reply["code"], 0);
    let (pack_type, notice) = receive(&mut socket, &keys).await;
    assert_eq!(pack_type, PackType::Plain);
    assert_eq!(notice["code"], 9006);
    assert_eq!(expect_close(&mut socket).await, 1001);
    assert!(daemon.handle.is_shutting_down());
    // 之後連不上（listener 已經關了）。
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert!(
        tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{}", daemon.port))
            .await
            .is_err()
    );
}

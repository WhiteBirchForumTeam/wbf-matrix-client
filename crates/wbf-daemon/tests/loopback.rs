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
use wbf_daemon::settings::Settings;

const TOKEN: [u8; 256] = [42u8; 256];

struct Daemon {
    port: u16,
    handle: Arc<Handle>,
    _dir: tempfile::TempDir,
}

async fn start_daemon() -> Daemon {
    let dir = tempfile::tempdir().unwrap();
    let policy = EncryptionPolicy::enforced();
    let handle = Handle::new(dir.path(), policy.clone(), Settings::default());
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
    assert_eq!(reply["result"]["connections"], 1);
    // 第二條連線進來，數字跟著變；它關掉之後回到 1。
    let mut other = connect(daemon.port).await;
    send(&mut other, &keys, PackType::Cipher, hello()).await;
    receive(&mut other, &keys).await;
    send(
        &mut other,
        &keys,
        PackType::Cipher,
        json!({ "method": "daemon.info", "id": 1 }),
    )
    .await;
    let (_, reply) = receive(&mut other, &keys).await;
    assert_eq!(reply["result"]["connections"], 2);
    other.close(None).await.unwrap();
    drop(other);
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    send(
        &mut socket,
        &keys,
        PackType::Cipher,
        json!({ "method": "daemon.info", "id": 2 }),
    )
    .await;
    let (_, reply) = receive(&mut socket, &keys).await;
    assert_eq!(reply["result"]["connections"], 1);

    // 這個資料目錄還沒有 local.key：帳號那些是 1002（訊息指向 vault.create），
    // 而且連線還活著（請求層錯誤不關連線）。
    send(
        &mut socket,
        &keys,
        PackType::Cipher,
        json!({ "method": "account.list", "params": {}, "id": 8 }),
    )
    .await;
    let (_, reply) = receive(&mut socket, &keys).await;
    assert_eq!(reply["code"], 1002, "{reply}");
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

/// architecture-v2 §4.3 的第 4、5 步：前端在 ready 之後**抹掉** token 檔，而 daemon 照樣服務。
///
/// ⭐ 這條釘住的是「daemon 讀完就不再回頭讀那個路徑」—— 哪天有人加了一段「重讀 token」
/// （例如想支援換 token），這裡會紅，而那正是要停下來想的時候。
#[tokio::test]
async fn the_daemon_keeps_serving_after_the_frontend_shreds_the_token_file() {
    let dir = tempfile::tempdir().unwrap();
    let token_path = dir.path().join("daemon.token");
    // 第 1 步：前端寫 256 byte。
    std::fs::write(&token_path, TOKEN).unwrap();

    // 第 2、3 步：daemon 從那個檔導金鑰、開 port。
    let token = std::fs::read(&token_path).unwrap();
    let keys = wbf_daemon::pack::RpcKeys::from_token_file(&token).expect("256 bytes");
    let policy = EncryptionPolicy::enforced();
    let handle = Handle::new(dir.path(), policy.clone(), Settings::default());
    let server = RpcServer::bind(0, Arc::new(keys), policy, handle.clone())
        .await
        .unwrap();
    let port = server.local_addr().unwrap().port();
    tokio::spawn(server.run());

    // 第 4 步：前端抹掉它。
    assert!(wbf_daemon::token::shred(&token_path).unwrap(), "本來有");
    assert!(!token_path.exists());

    // 第 5 步：token 只在記憶體裡——新連線照樣談得成、請求照樣回。
    let client_keys = RpcKeys::from_token(&TOKEN);
    let mut socket = connect(port).await;
    send(&mut socket, &client_keys, PackType::Cipher, hello()).await;
    let (pack_type, reply) = receive(&mut socket, &client_keys).await;
    assert_eq!(pack_type, PackType::Cipher);
    assert_eq!(reply["code"], 0, "{reply}");
    send(
        &mut socket,
        &client_keys,
        PackType::Cipher,
        json!({ "method": "daemon.info", "id": 1 }),
    )
    .await;
    let (_, reply) = receive(&mut socket, &client_keys).await;
    assert_eq!(reply["code"], 0, "{reply}");
    assert_eq!(reply["result"]["unlocked"], false);
}

// ---- 訂閱與推播（rpc-spec §3.9、§4；link-pool.md §6）----

fn link_event(user: &str) -> wbf_core::CoreEvent {
    wbf_core::CoreEvent::Link {
        user: user.to_string(),
        role: wbf_core::LinkRole::Subscriptions,
        state: wbf_core::LinkState::Opened,
        reason: None,
    }
}

/// 沒訂就收不到；訂了 `"*"` 就收到；退訂之後又收不到。「收不到」的證據：事件之後送一個請求，下一包一定是那個請求的回應。
#[tokio::test]
async fn pushes_reach_only_the_connections_that_subscribed() {
    let daemon = start_daemon().await;
    let keys = RpcKeys::from_token(&TOKEN);
    let mut socket = connect(daemon.port).await;
    send(&mut socket, &keys, PackType::Cipher, hello()).await;
    receive(&mut socket, &keys).await;
    let core = daemon.handle.core().await;

    core.emit_event(link_event("@alice:localhost"));
    send(
        &mut socket,
        &keys,
        PackType::Cipher,
        json!({ "method": "daemon.info", "id": 1 }),
    )
    .await;
    let (_, reply) = receive(&mut socket, &keys).await;
    assert_eq!(reply["id"], 1, "沒訂：下一包是回應，不是推播 {reply}");

    send(
        &mut socket,
        &keys,
        PackType::Cipher,
        json!({ "method": "subscribe", "params": { "events": ["*"] }, "id": 2 }),
    )
    .await;
    let (_, reply) = receive(&mut socket, &keys).await;
    assert_eq!(reply["code"], 0, "{reply}");
    assert_eq!(reply["result"]["subscribed"], json!(["*"]));

    core.emit_event(link_event("@alice:localhost"));
    let (pack_type, push) = receive(&mut socket, &keys).await;
    assert_eq!(pack_type, PackType::Cipher, "推播也走密文");
    assert_eq!(push["method"], "link.state", "{push}");
    assert!(push.get("id").is_none(), "推播沒有 id：{push}");
    assert_eq!(
        push["params"],
        json!({ "user": "@alice:localhost", "role": "subscriptions", "state": "opened" })
    );

    send(
        &mut socket,
        &keys,
        PackType::Cipher,
        json!({ "method": "unsubscribe", "params": { "events": ["*"] }, "id": 3 }),
    )
    .await;
    let (_, reply) = receive(&mut socket, &keys).await;
    assert_eq!(reply["result"]["subscribed"], json!([]));
    core.emit_event(link_event("@alice:localhost"));
    send(
        &mut socket,
        &keys,
        PackType::Cipher,
        json!({ "method": "daemon.info", "id": 4 }),
    )
    .await;
    let (_, reply) = receive(&mut socket, &keys).await;
    assert_eq!(reply["id"], 4, "退訂之後又收不到 {reply}");

    // 參數錯是 102，連線活著。
    send(
        &mut socket,
        &keys,
        PackType::Cipher,
        json!({ "method": "subscribe", "params": { "events": "nope" }, "id": 5 }),
    )
    .await;
    let (_, reply) = receive(&mut socket, &keys).await;
    assert_eq!(reply["code"], 102, "{reply}");
}

/// `user` 過濾：只訂 alice 的，bob 的不推；沒有帳號的事件（progress）照推。
#[tokio::test]
async fn a_subscription_scoped_to_a_user_drops_other_users_events() {
    let daemon = start_daemon().await;
    let keys = RpcKeys::from_token(&TOKEN);
    let mut socket = connect(daemon.port).await;
    send(&mut socket, &keys, PackType::Cipher, hello()).await;
    receive(&mut socket, &keys).await;
    send(
        &mut socket,
        &keys,
        PackType::Cipher,
        json!({ "method": "subscribe", "params": { "events": ["link.state", "progress"], "user": "@alice:localhost" }, "id": 1 }),
    )
    .await;
    let (_, reply) = receive(&mut socket, &keys).await;
    assert_eq!(
        reply["result"]["subscribed"],
        json!(["link.state", "progress"])
    );
    let core = daemon.handle.core().await;
    core.emit_event(link_event("@bob:localhost"));
    core.emit_event(wbf_core::CoreEvent::Progress {
        job: None,
        done: 1,
        total: Some(2),
        text: "half".into(),
    });
    core.emit_event(link_event("@alice:localhost"));
    let (_, first) = receive(&mut socket, &keys).await;
    assert_eq!(
        first["method"], "progress",
        "bob 的被濾掉，先到的是沒有帳號的進度 {first}"
    );
    assert_eq!(
        first["params"],
        json!({ "done": 1, "total": 2, "note": "half" })
    );
    let (_, second) = receive(&mut socket, &keys).await;
    assert_eq!(second["method"], "link.state");
    assert_eq!(second["params"]["user"], "@alice:localhost");
}

/// 讀太慢被覆蓋掉：第一包是 `desync { missed }`，🚫 不假裝沒事。
/// 📎 `#[tokio::test]` 是單執行緒的 runtime：這個迴圈同步地發 400 則，推播 task 一則都還沒讀，佇列深度 256 → 一定 Lagged。
#[tokio::test]
async fn falling_behind_the_broadcast_is_reported_as_desync() {
    let daemon = start_daemon().await;
    let keys = RpcKeys::from_token(&TOKEN);
    let mut socket = connect(daemon.port).await;
    send(&mut socket, &keys, PackType::Cipher, hello()).await;
    receive(&mut socket, &keys).await;
    send(
        &mut socket,
        &keys,
        PackType::Cipher,
        json!({ "method": "subscribe", "params": { "events": ["*"] }, "id": 1 }),
    )
    .await;
    receive(&mut socket, &keys).await;
    let core = daemon.handle.core().await;
    for _ in 0..400 {
        core.emit_event(link_event("@alice:localhost"));
    }
    let (_, first) = receive(&mut socket, &keys).await;
    assert_eq!(first["method"], "desync", "{first}");
    let missed = first["params"]["missed"].as_u64().unwrap();
    assert!(missed >= 100, "400 則進 256 深的佇列，至少漏 144：{missed}");
    let (_, next) = receive(&mut socket, &keys).await;
    assert_eq!(
        next["method"], "link.state",
        "desync 之後是還在佇列裡的那些"
    );
}

/// 沒訂任何東西的連線就算讀太慢也不收 `desync`：它本來就收不到推播，漏了沒東西可漏（PR #53 審查 rumia 🔴2）。
/// 退訂之後也一樣。證據：大量事件之後送一個請求，下一包就是它的回應。
#[tokio::test]
async fn an_unsubscribed_connection_never_gets_desync() {
    let daemon = start_daemon().await;
    let keys = RpcKeys::from_token(&TOKEN);
    let mut socket = connect(daemon.port).await;
    send(&mut socket, &keys, PackType::Cipher, hello()).await;
    receive(&mut socket, &keys).await;
    let core = daemon.handle.core().await;
    for _ in 0..400 {
        core.emit_event(link_event("@alice:localhost"));
    }
    send(
        &mut socket,
        &keys,
        PackType::Cipher,
        json!({ "method": "daemon.info", "id": 1 }),
    )
    .await;
    let (_, reply) = receive(&mut socket, &keys).await;
    assert_eq!(reply["id"], 1, "沒訂：下一包是回應，不是 desync {reply}");
    // 訂了再退掉：同樣不收。
    send(
        &mut socket,
        &keys,
        PackType::Cipher,
        json!({ "method": "subscribe", "params": { "events": ["*"] }, "id": 2 }),
    )
    .await;
    receive(&mut socket, &keys).await;
    send(
        &mut socket,
        &keys,
        PackType::Cipher,
        json!({ "method": "unsubscribe", "params": { "events": ["*"] }, "id": 3 }),
    )
    .await;
    receive(&mut socket, &keys).await;
    for _ in 0..400 {
        core.emit_event(link_event("@alice:localhost"));
    }
    send(
        &mut socket,
        &keys,
        PackType::Cipher,
        json!({ "method": "daemon.info", "id": 4 }),
    )
    .await;
    let (_, reply) = receive(&mut socket, &keys).await;
    assert_eq!(reply["id"], 4, "退訂了：下一包是回應，不是 desync {reply}");
}

/// `daemon.info` 報得出開著幾條上游的線；這個資料目錄沒有帳號，所以是 0。
#[tokio::test]
async fn daemon_info_reports_the_number_of_open_links() {
    let daemon = start_daemon().await;
    let keys = RpcKeys::from_token(&TOKEN);
    let mut socket = connect(daemon.port).await;
    send(&mut socket, &keys, PackType::Cipher, hello()).await;
    receive(&mut socket, &keys).await;
    send(
        &mut socket,
        &keys,
        PackType::Cipher,
        json!({ "method": "daemon.info", "id": 1 }),
    )
    .await;
    let (_, reply) = receive(&mut socket, &keys).await;
    assert_eq!(reply["result"]["links"], 0, "{reply}");
}

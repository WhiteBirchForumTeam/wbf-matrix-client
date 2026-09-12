//! 對真的 wbfuwunel 走一遍網路型 method（rpc-spec §10 判準：走我們自己的 WS 才算做完）。
//!
//! `--ignored`；環境變數：
//!   WBF_E2E_SERVER        example: http://localhost:6167
//!   WBF_E2E_USER          example: @alice:localhost
//!   WBF_E2E_PASSWORD_FILE 整檔就是密碼（去掉結尾一個換行）
//!
//! 流程：account.add → account.whoami → server.ping（WS）→ room.list → sync.recent（WS）
//! → backup.status → account.del。每一步看 code，🚫 不看 msg。

use std::sync::Arc;

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::Message;
use wbf_daemon::connection::EncryptionPolicy;
use wbf_daemon::handle::Handle;
use wbf_daemon::pack::{self, PackType, RpcKeys, Side};
use wbf_daemon::server::RpcServer;
use wbf_daemon::settings::Settings;

const TOKEN: [u8; 256] = [7u8; 256];

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

struct Client {
    socket: Socket,
    keys: RpcKeys,
    next_id: u64,
}

impl Client {
    async fn connect(port: u16) -> Client {
        let (socket, _) = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}"))
            .await
            .unwrap();
        let mut client = Client {
            socket,
            keys: RpcKeys::from_token(&TOKEN),
            next_id: 0,
        };
        let hello = client
            .call(
                "hello",
                json!({ "protocols": [1], "client": "wbf-matrix-rpc-cli e2e" }),
            )
            .await;
        assert_eq!(hello["code"], 0, "{hello}");
        client
    }

    async fn call(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        let request = json!({ "method": method, "params": params, "id": id });
        let frame = pack::seal(
            &self.keys,
            Side::Client,
            PackType::Cipher,
            request.to_string().as_bytes(),
        );
        self.socket
            .send(Message::Binary(frame.into()))
            .await
            .unwrap();
        loop {
            match self.socket.next().await.unwrap().unwrap() {
                Message::Binary(bytes) => {
                    let (_, json) = pack::open(&self.keys, Side::Client, &bytes).unwrap();
                    let reply: Value = serde_json::from_slice(&json).unwrap();
                    if reply["id"] == id {
                        return reply;
                    }
                }
                Message::Ping(_) | Message::Pong(_) => continue,
                other => panic!("unexpected {other:?}"),
            }
        }
    }
}

#[tokio::test]
#[ignore = "needs a running wbfuwunel: WBF_E2E_SERVER, WBF_E2E_USER, WBF_E2E_PASSWORD_FILE"]
async fn login_ping_rooms_recent_and_logout_over_the_daemon() {
    let server = std::env::var("WBF_E2E_SERVER").expect("WBF_E2E_SERVER");
    let user = std::env::var("WBF_E2E_USER").expect("WBF_E2E_USER");
    let password_file = std::env::var("WBF_E2E_PASSWORD_FILE").expect("WBF_E2E_PASSWORD_FILE");
    let password = std::fs::read_to_string(password_file).unwrap();
    let password = password.strip_suffix('\n').unwrap_or(&password).to_string();

    // data dir 用短路徑：加密過的目錄名很長（handover §4 9b）。
    let dir = tempfile::Builder::new()
        .prefix("wd")
        .tempdir_in(std::env::temp_dir())
        .unwrap();
    let policy = EncryptionPolicy::enforced();
    let handle = Handle::new(dir.path(), policy.clone(), Settings::default());
    let rpc = RpcServer::bind(
        0,
        Arc::new(RpcKeys::from_token(&TOKEN)),
        policy,
        handle.clone(),
    )
    .await
    .unwrap();
    let port = rpc.local_addr().unwrap().port();
    tokio::spawn(rpc.run());

    let mut client = Client::connect(port).await;

    let reply = client
        .call(
            "account.add",
            json!({ "server": server, "user": user, "password": password, "device_name": "wbf-daemon e2e" }),
        )
        .await;
    assert_eq!(reply["code"], 0, "account.add: {reply}");
    assert_eq!(reply["result"]["user_id"], user);
    // 第一次登入建了 local.key（plain）。
    let info = client.call("daemon.info", Value::Null).await;
    assert_eq!(info["result"]["unlocked"], true);
    assert_eq!(info["result"]["key_mode"], "plain");

    let reply = client.call("account.whoami", json!({})).await;
    assert_eq!(reply["code"], 0, "whoami: {reply}");
    assert_eq!(reply["result"]["user_id"], user);

    // WS：Hello／Ping。
    let reply = client.call("server.ping", json!({})).await;
    assert_eq!(reply["code"], 0, "ping: {reply}");
    assert!(reply["result"]["features"].is_array(), "{reply}");

    let reply = client.call("room.list", json!({})).await;
    assert_eq!(reply["code"], 0, "room.list: {reply}");
    assert!(reply["result"].is_array());

    // WS：Event/Recent。
    let reply = client
        .call("sync.recent", json!({ "max_events": 100 }))
        .await;
    assert_eq!(reply["code"], 0, "recent: {reply}");
    assert!(reply["result"]["caught_up"].is_boolean(), "{reply}");

    let reply = client.call("backup.status", json!({})).await;
    assert_eq!(reply["code"], 0, "backup.status: {reply}");
    assert_eq!(reply["result"]["server_backup_setting"], "on");

    // 沒 recovery key：閘門擋（1021）；accept_history_loss 才過。
    let reply = client.call("account.del", json!({ "user": user })).await;
    assert_eq!(reply["code"], 1021, "gate: {reply}");
    let reply = client
        .call(
            "account.del",
            json!({ "user": user, "accept_history_loss": true }),
        )
        .await;
    assert_eq!(reply["code"], 0, "account.del: {reply}");

    let reply = client.call("account.list", json!({})).await;
    assert_eq!(reply["code"], 0);
    let accounts = reply["result"]["accounts"].as_array().unwrap();
    assert!(
        accounts.iter().all(|account| account["logged_in"] == false),
        "{reply}"
    );
}

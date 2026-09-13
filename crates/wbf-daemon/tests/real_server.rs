//! 對真的 wbfuwunel 走一遍網路型 method（rpc-spec §10 判準：走我們自己的 WS 才算做完）。
//!
//! `--ignored`；環境變數：
//!   WBF_E2E_SERVER        example: http://localhost:6167
//!   WBF_E2E_USER          example: @alice:localhost
//!   WBF_E2E_PASSWORD_FILE 整檔就是密碼（去掉結尾一個換行）
//!
//! 流程：vault.create（**passphrase 模式**）→ account.add → account.whoami → server.ping（WS）
//! → room.list → sync.recent（WS）→ backup.status → **daemon 重開 → vault.unlock → whoami**
//! → account.del。每一步看 code，🚫 不看 msg。
//!
//! ⭐ 這裡刻意走 passphrase 模式（plain 由單元測試涵蓋）：要驗的是「先建加密倉庫、再登入」這條路
//! 對**真的 server** 也成立 —— 登入寫出來的 `session.sealed` 與 matrix store 都是用那把被 passphrase
//! 包住的主金鑰派生的，所以 daemon 重開之後要能用同一句 passphrase 解回來（rpc-spec §3.1）。

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
/// base64("hunter2")。passphrase 是任意 bytes（local-cache-db §12），RPC 上一律 base64。
const PASSPHRASE_BASE64: &str = "aHVudGVyMg==";

/// 在這個資料目錄上起一個 daemon，回它的 port。**叫兩次就是「重開一次」**。
async fn start_daemon(data_dir: &std::path::Path) -> u16 {
    let policy = EncryptionPolicy::enforced();
    let handle = Handle::new(data_dir, policy.clone(), Settings::default());
    let rpc = RpcServer::bind(0, Arc::new(RpcKeys::from_token(&TOKEN)), policy, handle)
        .await
        .unwrap();
    let port = rpc.local_addr().unwrap().port();
    tokio::spawn(rpc.run());
    port
}

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
    let port = start_daemon(dir.path()).await;
    let mut client = Client::connect(port).await;

    // fresh 資料目錄的起手式（rpc-spec §3.1）：🚫 account.add 不替前端建 vault，
    // 而「要不要 passphrase」就在**建的這一步**決定，🚫 不是登入之後再重包。
    let reply = client
        .call(
            "vault.create",
            json!({ "passphrase_base64": PASSPHRASE_BASE64 }),
        )
        .await;
    assert_eq!(reply["code"], 0, "vault.create: {reply}");
    assert_eq!(reply["result"]["key_mode"], "passphrase");

    let reply = client
        .call(
            "account.add",
            json!({ "server": server, "user": user, "password": password, "device_name": "wbf-daemon e2e" }),
        )
        .await;
    assert_eq!(reply["code"], 0, "account.add: {reply}");
    assert_eq!(reply["result"]["user_id"], user);
    let info = client.call("daemon.info", Value::Null).await;
    assert_eq!(info["result"]["unlocked"], true);
    assert_eq!(info["result"]["key_mode"], "passphrase");

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

    // daemon 重開（真正的「鎖上」就是這條，rpc-spec §3.1）：同一個資料目錄、新的 Handle。
    // ⭐ 驗的是登入寫出來的東西真的被那把 passphrase 包住的主金鑰保護著。
    let port = start_daemon(dir.path()).await;
    let mut client = Client::connect(port).await;
    let info = client.call("daemon.info", Value::Null).await;
    assert_eq!(info["result"]["unlocked"], false, "{info}");
    assert_eq!(info["result"]["key_mode"], "passphrase");
    // 鎖著：帳號那些是 1001（有 local.key，所以🚫 不是 1002）。
    let reply = client.call("account.whoami", json!({})).await;
    assert_eq!(reply["code"], 1001, "{reply}");
    // 錯的 passphrase 開不了。
    let reply = client
        .call("vault.unlock", json!({ "passphrase_base64": "d3Jvbmc=" }))
        .await;
    assert_eq!(reply["code"], 1005, "{reply}");
    // 對的開得了，而且 session.sealed 解得回來 —— whoami 是「真的用回那個 access_token」。
    let reply = client
        .call(
            "vault.unlock",
            json!({ "passphrase_base64": PASSPHRASE_BASE64 }),
        )
        .await;
    assert_eq!(reply["code"], 0, "vault.unlock: {reply}");
    let reply = client.call("account.whoami", json!({})).await;
    assert_eq!(reply["code"], 0, "whoami after restart: {reply}");
    assert_eq!(reply["result"]["user_id"], user);

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

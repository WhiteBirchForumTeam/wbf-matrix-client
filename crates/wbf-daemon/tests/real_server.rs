//! 對真的 wbfuwunel 走一遍網路型 method（/docs/design/rpc-specs/rpc-spec.md §10 判準：走我們自己的 WS 才算做完）。
//!
//! `--ignored`；環境變數：
//!   WBF_E2E_SERVER        example: http://localhost:6167
//!   WBF_E2E_USER          example: @alice:localhost
//!   WBF_E2E_PASSWORD_FILE 整檔就是密碼（去掉結尾一個換行）
//!
//! 流程：vault.create（**passphrase 模式**）→ account.add（鉤子在背景開五條線）→ account.whoami → server.ping（WS）
//! → room.list（local 與 both，後者走橋的 JoinedRooms）→ sync.recent（WS）→ backup.status（wbf 帳號走橋）→ **daemon 重開 → vault.unlock → whoami**
//! → account.del。每一步看 code，**不**看 msg。
//!
//! ⭐ 這裡刻意走 passphrase 模式（plain 由單元測試涵蓋）：要驗的是「先建加密倉庫、再登入」這條路
//! 對**真的 server** 也成立 —— 登入寫出來的 `session.sealed` 與 matrix store 都是用那把被 passphrase
//! 包住的主金鑰派生的，所以 daemon 重開之後要能用同一句 passphrase 解回來（/docs/design/rpc-specs/rpc-spec.md §3.1）。

use std::sync::Arc;

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::Message;
use wbf_daemon::connection::EncryptionPolicy;
use wbf_daemon::data_plane::{AccessKeys, DataServer};
use wbf_daemon::handle::Handle;
use wbf_daemon::pack::{self, PackType, RpcKeys, Side};
use wbf_daemon::protocol::CURRENT_PROTOCOL;
use wbf_daemon::server::RpcServer;
use wbf_daemon::settings::Settings;

const TOKEN: [u8; 256] = [7u8; 256];
/// base64("hunter2")。passphrase 是任意 bytes（/docs/design/storage/vault-and-keys.md §3），RPC 上一律 base64。
const PASSPHRASE_BASE64: &str = "aHVudGVyMg==";

/// 一個跑著的 daemon。⚠️ 拿著 `task` 才停得掉它 —— 「重開」必須是**真的停掉再起**，
/// **不是**「再起一個」：兩個 daemon 同時開同一個資料目錄正是 /docs/design/overview/architecture-v2.md §0.2 禁止的事
/// （PR #31 審查 cirno🔴）。
struct Daemon {
    port: u16,
    /// 資料平面（/docs/design/rpc-specs/data-plane.md）的 port。
    data_port: u16,
    task: tokio::task::JoinHandle<()>,
}

async fn start_daemon(data_dir: &std::path::Path) -> Daemon {
    start_daemon_with(data_dir, Settings::default()).await
}

async fn start_daemon_with(data_dir: &std::path::Path, settings: Settings) -> Daemon {
    let policy = EncryptionPolicy::enforced();
    let handle = Handle::new(data_dir, policy.clone(), settings);
    handle.set_access_keys(AccessKeys::from_token(&TOKEN));
    let rpc = RpcServer::bind(
        0,
        Arc::new(RpcKeys::from_token(&TOKEN)),
        policy,
        handle.clone(),
    )
    .await
    .unwrap();
    let port = rpc.local_addr().unwrap().port();
    let data = DataServer::bind(0, handle.clone()).await.unwrap();
    let data_port = data.local_addr().unwrap().port();
    handle.set_ports(port, data_port).await;
    // 跟 `main.rs` 一樣兩個 listener 一起跑、一起停。
    let task = tokio::spawn(async move {
        tokio::join!(rpc.run(), data.run());
    });
    Daemon {
        port,
        data_port,
        task,
    }
}

/// 停掉一個 daemon：走**真的那條路**（`daemon.shutdown`），關掉連線，等 server 收攤，
/// 然後確認舊 port 真的不收連線了。
///
/// ⭐ 這裡刻意不用「丟掉 task」那種便宜作法：那樣就算 shutdown 整條壞掉測試也會綠。
/// 鉤子在背景開線（`vault.unlock`／`account.add` 之後，/docs/design/daemon/link-pool.md §3.1）：等 `daemon.info` 的線數到 `want`。
async fn wait_for_links(client: &mut Client, want: u64) -> Value {
    for _ in 0..150 {
        let info = client.call("daemon.info", Value::Null).await;
        if info["result"]["links"] == want {
            return info;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    panic!("the daemon never reached {want} open links");
}

async fn stop_daemon(daemon: Daemon, mut client: Client) {
    let reply = client.call("daemon.shutdown", Value::Null).await;
    assert_eq!(reply["code"], 0, "daemon.shutdown: {reply}");
    drop(client);
    tokio::time::timeout(std::time::Duration::from_secs(10), daemon.task)
        .await
        .expect("the daemon stopped within 10 s")
        .expect("the daemon task did not panic");
    assert!(
        tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{}", daemon.port))
            .await
            .is_err(),
        "舊 port 停掉之後不該還收連線"
    );
}

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

struct Client {
    socket: Socket,
    keys: RpcKeys,
    next_id: u64,
    /// 等回應時收到的推播（沒有 `id` 的），照順序。
    pushes: Vec<Value>,
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
            pushes: Vec::new(),
        };
        let hello = client
            .call(
                "hello",
                json!({ "protocols": [CURRENT_PROTOCOL], "client": "wbf-matrix-rpc-cli e2e" }),
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
        )
        .unwrap();
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
                    if reply["id"].is_null() {
                        self.pushes.push(reply);
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

    // data dir 用短路徑：加密過的目錄名很長（/docs/handover.md §4 9b）。
    let dir = tempfile::Builder::new()
        .prefix("wd")
        .tempdir_in(std::env::temp_dir())
        .unwrap();
    let daemon = start_daemon(dir.path()).await;
    let mut client = Client::connect(daemon.port).await;

    // fresh 資料目錄的起手式（/docs/design/rpc-specs/rpc-spec.md §3.1）：account.add **不**替前端建 vault，
    // 而「要不要 passphrase」就在**建的這一步**決定，**不是**登入之後再重包。
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

    // 登入成功就觸發鉤子：五條線（misc、upload、download、rooms、keys）在背景開起來（/docs/design/daemon/link-pool.md §3.1）。
    wait_for_links(&mut client, 5).await;

    // WS：Hello／Ping。走已經開著的 misc，**不**再開一條。
    let reply = client.call("server.ping", json!({})).await;
    assert_eq!(reply["code"], 0, "ping: {reply}");
    assert!(reply["result"]["features"].is_array(), "{reply}");
    let info = client.call("daemon.info", Value::Null).await;
    assert_eq!(info["result"]["links"], 5, "同一條 misc 線：{info}");

    let reply = client.call("room.list", json!({})).await;
    assert_eq!(reply["code"], 0, "room.list: {reply}");
    assert!(reply["result"].is_array());
    // wbf 帳號沒有 matrix-sdk 的 Client（/docs/design/daemon/account-session.md §2）：`sync=both` 走橋的 JoinedRooms（只有 id，維護者 2026-10-05）。
    let reply = client.call("room.list", json!({ "sync": "both" })).await;
    assert_eq!(reply["code"], 0, "room.list sync=both: {reply}");
    assert!(reply["result"].is_array());

    // WS：Event/Recent。
    let reply = client
        .call("sync.recent", json!({ "max_events": 100 }))
        .await;
    assert_eq!(reply["code"], 0, "recent: {reply}");
    assert!(reply["result"]["caught_up"].is_boolean(), "{reply}");

    // 備份對 wbf 帳號走橋（/docs/design/keys/cross-signing-and-verification.md §4）：金鑰線開好時就建了（`SERVER_BACKUP` 預設開）。
    let reply = client.call("backup.status", json!({})).await;
    assert_eq!(reply["code"], 0, "backup.status on a wbf account: {reply}");
    assert_eq!(reply["result"]["server_backup_exists"], true, "{reply}");
    let before = info["result"]["instance"]
        .as_str()
        .expect("instance")
        .to_string();

    // daemon 重開（真正的「鎖上」就是這條，/docs/design/rpc-specs/rpc-spec.md §3.1）。
    // ⚠️ **先停掉第一個**：`daemon.shutdown` → 關連線 → 等它收攤 → 確認舊 port 不收連線了。
    // 禁止直接再起一個：兩個 daemon 同時開同一個資料目錄是 /docs/design/overview/architecture-v2.md §0.2 禁止的，而且那樣
    // 就算 shutdown 壞掉這條測試也會綠（PR #31 審查 cirno🔴）。
    stop_daemon(daemon, client).await;
    let daemon = start_daemon(dir.path()).await;
    let mut client = Client::connect(daemon.port).await;
    // ⭐ 驗的是登入寫出來的東西真的被那把 passphrase 包住的主金鑰保護著。
    let info = client.call("daemon.info", Value::Null).await;
    assert_eq!(info["result"]["unlocked"], false, "{info}");
    assert_eq!(info["result"]["key_mode"], "passphrase");
    // ⭐ 換了一個實例：instance 一定不同（port 會重用、pid 會回收，這個不會）。
    assert_ne!(info["result"]["instance"], before.as_str(), "{info}");
    // 鎖著：帳號那些是 1001（有 local.key，所以**不是** 1002）。
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
    // 重開之後解鎖就觸發鉤子：五條線又開起來。
    wait_for_links(&mut client, 5).await;
    let reply = client.call("server.ping", json!({})).await;
    assert_eq!(reply["code"], 0, "ping after restart: {reply}");

    let reply = client
        .call(
            "account.del",
            json!({ "user": user, "accept_history_loss": true }),
        )
        .await;
    assert_eq!(reply["code"], 0, "account.del: {reply}");
    // 登出：token 撤了，這個帳號的線全關（/docs/design/daemon/link-pool.md §3）。
    let info = client.call("daemon.info", Value::Null).await;
    assert_eq!(info["result"]["links"], 0, "{info}");

    let reply = client.call("account.list", json!({})).await;
    assert_eq!(reply["code"], 0);
    let accounts = reply["result"]["accounts"].as_array().unwrap();
    assert!(
        accounts.iter().all(|account| account["logged_in"] == false),
        "{reply}"
    );

    // 收攤：第二個也照同一條路停掉，**不**留一個還開著資料目錄的 daemon 給 tempdir 收
    // （Windows 上那會讓刪除失敗，而且它掩蓋的正是「有東西還握著 store」這件事）。
    stop_daemon(daemon, client).await;
}

/// 🚨 **房間歷史往回翻，定位一律是 `event_id`**（wbfuwunel #51；維護者 2026-09-14）。
///
/// 額外要 `WBF_E2E_ROOM`：一個 `WBF_E2E_USER` 在裡面的房間（這條測試會往裡面送 7 則）；選填 `WBF_E2E_ENCRYPTED_ROOM`（一間加密房，驗沒帶 `room_devices` 的送出被拒、**不**送明文——1100）。
///
/// ⭐ 原本刻意讓**兩條上游路線都被真的打到**；#61 起訂閱線常開、自己送的會被推回來寫進本地，`/context` 那條在這個流程裡到不了了
/// （改由 core 的 `an_anchor_that_is_not_in_the_local_cache_is_refused_not_answered_empty` 單元測試守）：
///
/// | 步驟 | 走哪條 | 為什麼一定是它 |
/// |---|---|---|
/// | `sync=server` 第一頁 | wbf `Recent{rooms}` | 沒帶 `before`，而 server 講 wbf |
/// | `sync=server` 第二頁 | wbf `Recent{rooms, before: g_seq}` | 訂閱線推來的已經寫進本地，錨點查得到 `g_seq`（#61 之前這一格是 `/context`） |
/// | `sync=both` 兩頁 | wbf `Recent{rooms, before: g_seq}` | 第一頁寫進去了，第二頁的錨點本地查得到 |
/// | `sync=local` | 本地 | 驗 `both` 真的寫進去了，而且本地也能拿 `event_id` 接著翻 |
///
/// 📎 每一段都拿**自己剛送的 7 則**比對，**不**假設房間原本是空的。
#[tokio::test]
#[ignore = "needs a running wbfuwunel: WBF_E2E_SERVER, WBF_E2E_USER, WBF_E2E_PASSWORD_FILE, WBF_E2E_ROOM"]
async fn room_history_pages_back_by_event_id_over_both_upstream_paths() {
    let server = std::env::var("WBF_E2E_SERVER").expect("WBF_E2E_SERVER");
    let user = std::env::var("WBF_E2E_USER").expect("WBF_E2E_USER");
    let room = std::env::var("WBF_E2E_ROOM").expect("WBF_E2E_ROOM");
    let password_file = std::env::var("WBF_E2E_PASSWORD_FILE").expect("WBF_E2E_PASSWORD_FILE");
    let password = std::fs::read_to_string(password_file).unwrap();
    let password = password.strip_suffix('\n').unwrap_or(&password).to_string();

    let dir = tempfile::Builder::new()
        .prefix("wh")
        .tempdir_in(std::env::temp_dir())
        .unwrap();
    let daemon = start_daemon(dir.path()).await;
    let mut client = Client::connect(daemon.port).await;
    let reply = client.call("vault.create", json!({})).await;
    assert_eq!(reply["code"], 0, "vault.create: {reply}");
    let reply = client
        .call(
            "account.add",
            json!({ "server": server, "user": user, "password": password, "device_name": "wbf-daemon history e2e" }),
        )
        .await;
    assert_eq!(reply["code"], 0, "account.add: {reply}");
    // 訂閱線在背景開（/docs/design/daemon/link-pool.md §3.1）：等五條都開好再送，下面送的 7 則才一定會被推回來（訂閱不補訂閱之前的，那是 UI 叫 `sync.recent` 的事）。
    wait_for_links(&mut client, 5).await;
    // 照 UI 的順序（維護者 2026-10-05）：列表只有 id，看得到的那間再 `room.get` 拿樣子、寫進本地；送文字只看本地記的加不加密。
    let rooms = client.call("room.list", json!({ "sync": "both" })).await;
    assert_eq!(rooms["code"], 0, "room.list: {rooms}");
    let listed = rooms["result"]
        .as_array()
        .and_then(|entries| entries.iter().find(|entry| entry["id"] == room.as_str()))
        .unwrap_or_else(|| panic!("the joined list has {room}: {rooms}"));
    assert!(
        listed["refreshed_at"].is_null() && listed["name"].is_null(),
        "a fresh data dir has only the id: {listed}"
    );
    let fetched = client
        .call("room.get", json!({ "room": room, "sync": "both" }))
        .await;
    assert_eq!(fetched["code"], 0, "room.get: {fetched}");
    let rooms = client.call("room.list", json!({})).await;
    let listed = rooms["result"]
        .as_array()
        .and_then(|entries| entries.iter().find(|entry| entry["id"] == room.as_str()))
        .unwrap_or_else(|| panic!("still listed: {rooms}"));
    assert!(
        listed["refreshed_at"].is_u64() && listed["encrypted"] == false,
        "room.get wrote what it fetched: {listed}"
    );

    // 加密房沒帶 `room_devices` 要被拒（1100）、**不**送明文：加不加密看的是剛拿到的房間（/docs/design/keys/e2ee-rpc.md §7）。
    // 選填 `WBF_E2E_ENCRYPTED_ROOM`：一間 `WBF_E2E_USER` 在裡面的加密房。
    if let Ok(encrypted_room) = std::env::var("WBF_E2E_ENCRYPTED_ROOM") {
        let fetched = client
            .call(
                "room.get",
                json!({ "room": encrypted_room, "sync": "both" }),
            )
            .await;
        assert_eq!(fetched["code"], 0, "room.get: {fetched}");
        let reply = client
            .call(
                "room.send_text",
                json!({ "room": encrypted_room, "body": "must not go out in plaintext" }),
            )
            .await;
        assert_eq!(reply["code"], 1100, "加密房的明文送出要被拒：{reply}");
        assert!(
            reply.to_string().contains("room_devices"),
            "refused because the room is encrypted, not because it is unknown: {reply}"
        );
        // /docs/design/keys/e2ee-rpc.md 的 RPC 形狀：`room.refresh_devices` 回的整份原樣當 `room_devices` 帶回來，送出去的是密文、帶那個號碼。
        let refreshed = client
            .call("room.refresh_devices", json!({ "room": encrypted_room }))
            .await;
        assert_eq!(refreshed["code"], 0, "room.refresh_devices: {refreshed}");
        assert!(
            refreshed["result"]["members"].get(&user).is_some(),
            "{refreshed}"
        );
        let reply = client
            .call(
                "room.send_text",
                json!({ "room": encrypted_room, "body": "encrypted over the daemon", "room_devices": refreshed["result"] }),
            )
            .await;
        assert_eq!(reply["code"], 0, "加密房帶 room_devices 送得出去：{reply}");
    }

    // 送 7 則，記下它們的 event_id（舊到新）。
    let mut sent = Vec::new();
    for index in 1..=7 {
        let reply = client
            .call(
                "room.send_text",
                json!({ "room": room, "body": format!("history e2e {index}") }),
            )
            .await;
        assert_eq!(reply["code"], 0, "room.send_text {index}: {reply}");
        sent.push(reply["result"]["event_id"].as_str().unwrap().to_string());
    }
    // 新到舊，這才是一頁該有的順序。
    let newest_first: Vec<String> = sent.iter().rev().cloned().collect();

    async fn page(
        client: &mut Client,
        room: &str,
        sync: &str,
        before: Option<&str>,
    ) -> (Vec<String>, Option<String>) {
        let mut params = json!({ "room": room, "limit": 3, "sync": sync });
        if let Some(before) = before {
            params["before"] = json!(before);
        }
        let reply = client.call("room.history", params).await;
        assert_eq!(
            reply["code"], 0,
            "room.history sync={sync} before={before:?}: {reply}"
        );
        assert_eq!(reply["sync"], sync, "回應要說出用了哪一種");
        let ids = reply["result"]["events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|event| event["id"].as_str().unwrap().to_string())
            .collect();
        let next = reply["result"]["next"].as_str().map(str::to_string);
        (ids, next)
    }

    // ── 訂閱線（#61 起 account.add 之後 daemon 自己開）把剛送的 7 則推回來、寫進本地：等它們都到了再往下，**不**賭時序 ──
    let mut local_newest = Vec::new();
    for _ in 0..100 {
        let reply = client
            .call(
                "room.history",
                json!({ "room": room, "limit": 7, "sync": "local" }),
            )
            .await;
        local_newest = reply["result"]["events"]
            .as_array()
            .map(|events| {
                events
                    .iter()
                    .filter_map(|event| event["id"].as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        if local_newest == newest_first {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    assert_eq!(local_newest, newest_first, "訂閱線推來的 7 則都寫進本地了");

    // ── sync=server：第一頁走 wbf；第二頁的錨點本地查得到（推播寫進去的）→ 也走 wbf `Recent{rooms, before: g_seq}` ──
    // 📎 #61 之前這裡測的是「錨點不在本地 → 要走 /context → wbf 帳號沒有 Client → 1100」；訂閱線常開之後，自己送的訊息一定會被推回來，
    //    那條路在這個流程裡到不了了（改由 core 的 `an_anchor_that_is_not_in_the_local_cache_is_refused_not_answered_empty` 守）。
    let (first, next) = page(&mut client, &room, "server", None).await;
    assert_eq!(first, newest_first[0..3], "server 第一頁（wbf）");
    assert_eq!(
        next.as_deref(),
        Some(newest_first[2].as_str()),
        "next 是這頁最舊那則"
    );
    let (second, _) = page(&mut client, &room, "server", next.as_deref()).await;
    assert_eq!(
        second,
        newest_first[3..6],
        "server 第二頁：錨點本地查得到，走 wbf 接著翻"
    );

    // ── sync=both：寫進去；第二頁的錨點本地查得到 → wbf Recent{rooms, before: g_seq} ──
    let (first, next) = page(&mut client, &room, "both", None).await;
    assert_eq!(first, newest_first[0..3], "both 第一頁");
    let (second, _) = page(&mut client, &room, "both", next.as_deref()).await;
    assert_eq!(second, newest_first[3..6], "both 第二頁要緊接著第一頁");

    // ── sync=local：both 寫進去了，本地也能用 event_id 接著翻 ──
    let (local_second, _) = page(&mut client, &room, "local", next.as_deref()).await;
    assert_eq!(
        local_second,
        newest_first[3..6],
        "本地拿同一個錨要得到同一頁"
    );

    let reply = client
        .call(
            "account.del",
            json!({ "user": user, "accept_history_loss": true }),
        )
        .await;
    assert_eq!(reply["code"], 0, "account.del: {reply}");
    stop_daemon(daemon, client).await;
}

/// 資料平面整條（/docs/design/rpc-specs/data-plane.md）：UI 先打 HTTP（`media.create` → `PUT`，拿回 manifest），再用 RPC 發訊息（`room.send_attachment` 帶 mxc）。
/// 要 `WBF_E2E_ROOM`（明文房）與 `WBF_E2E_ENCRYPTED_ROOM`（加密房），`WBF_E2E_USER` 都在裡面。
///
/// | 步驟 | 驗什麼 |
/// |---|---|
/// | 明文房：建檔 → PUT → 送附件（帶 manifest） | URL 是 `e-` 加密的、看不出 mxc；區塊是 `none`；附件宣告過得了 server 的歸屬檢查 |
/// | 加密房：建檔 → PUT → 送附件（帶 `room_devices`） | 區塊有金鑰；送出去的是密文、附件在同一個請求宣告 |
/// | 同一個 URL 傳完再 PUT 一次 | **不是** 200：server 已經收掉這個上傳 |
/// | `media.export_to` 那份 manifest | 從 server 拉回來、解開、整檔驗過，跟 PUT 的 bytes 一樣 |
/// | `room.files`（`both`） | 加密房那則解得開、認得出是檔案、mxc 對得上 |
/// | 加密房串流（沒給大小） | 傳完拿到真的大小，送得出去 |
#[tokio::test]
#[ignore = "needs a running wbfuwunel: WBF_E2E_SERVER, WBF_E2E_USER, WBF_E2E_PASSWORD_FILE, WBF_E2E_ROOM, WBF_E2E_ENCRYPTED_ROOM"]
async fn an_attachment_goes_over_the_data_plane_into_plain_and_encrypted_rooms() {
    let server = std::env::var("WBF_E2E_SERVER").expect("WBF_E2E_SERVER");
    let user = std::env::var("WBF_E2E_USER").expect("WBF_E2E_USER");
    let room = std::env::var("WBF_E2E_ROOM").expect("WBF_E2E_ROOM");
    let encrypted_room = std::env::var("WBF_E2E_ENCRYPTED_ROOM").expect("WBF_E2E_ENCRYPTED_ROOM");
    let password_file = std::env::var("WBF_E2E_PASSWORD_FILE").expect("WBF_E2E_PASSWORD_FILE");
    let password = std::fs::read_to_string(password_file).unwrap();
    let password = password.strip_suffix('\n').unwrap_or(&password).to_string();

    let dir = tempfile::Builder::new()
        .prefix("wd")
        .tempdir_in(std::env::temp_dir())
        .unwrap();
    let daemon = start_daemon(dir.path()).await;
    let mut client = Client::connect(daemon.port).await;
    let reply = client.call("vault.create", json!({})).await;
    assert_eq!(reply["code"], 0, "vault.create: {reply}");
    let reply = client
        .call(
            "account.add",
            json!({ "server": server, "user": user, "password": password, "device_name": "wbf-daemon data plane e2e" }),
        )
        .await;
    assert_eq!(reply["code"], 0, "account.add: {reply}");
    wait_for_links(&mut client, 5).await;
    let info = client.call("daemon.info", Value::Null).await;
    assert_eq!(info["result"]["data_port"], daemon.data_port, "{info}");

    // 幾塊大小的 bytes（不是塊大小的整數倍，最後一塊不滿）。
    let body: Vec<u8> = (0..200_003u32)
        .map(|position| (position % 251) as u8)
        .collect();

    // ── 明文房 ──
    let created = client
        .call(
            "media.create",
            json!({ "room": room, "name": "plain.bin", "size": body.len(), "mimetype": "application/octet-stream",
                     "source_uri": "file:///nowhere/plain.bin" }),
        )
        .await;
    assert_eq!(created["code"], 0, "media.create: {created}");
    let mxc = created["result"]["mxc"].clone();
    let url = created["result"]["url"].as_str().unwrap().to_string();
    assert!(
        url.contains("/upload/mxc/e-") && url.len() < 160,
        "URL 只帶用途與 mxc：{url}"
    );
    assert!(
        created["result"]["headers"]["Wbf-Upload-Meta"].is_string(),
        "上傳狀態在 header：{created}"
    );
    assert!(
        !url.contains(mxc.as_str().unwrap().trim_start_matches("mxc://")),
        "加密的 URL 看不出是哪個檔：{url}"
    );
    let (status, manifest) = put(daemon.data_port, &created["result"], &body).await;
    assert_eq!(status, 200, "{manifest}");
    assert_eq!(manifest["mxc"], mxc);
    assert_eq!(manifest["block"]["cipher"], "none", "明文房的附件是明文");
    let sent = client
        .call(
            "room.send_attachment",
            json!({ "room": room, "manifest": manifest, "caption": "plain e2e" }),
        )
        .await;
    assert_eq!(sent["code"], 0, "傳完就送得出去：{sent}");
    assert_eq!(sent["result"]["attachment_declared"], true);
    assert_eq!(sent["result"]["mxc"], mxc);

    // ── 加密房：固定大小 ──
    let refreshed = client
        .call("room.refresh_devices", json!({ "room": encrypted_room }))
        .await;
    assert_eq!(refreshed["code"], 0, "room.refresh_devices: {refreshed}");
    let created = client
        .call(
            "media.create",
            json!({ "room": encrypted_room, "name": "secret.bin", "size": body.len() }),
        )
        .await;
    assert_eq!(created["code"], 0, "media.create: {created}");
    let upload = created["result"].clone();
    let (status, manifest) = put(daemon.data_port, &upload, &body).await;
    assert_eq!(status, 200, "{manifest}");
    assert_ne!(manifest["block"]["cipher"], "none", "加密房的附件要加密");
    assert!(manifest["block"]["key"].is_string());
    let (status, again) = put(daemon.data_port, &upload, &body).await;
    assert_eq!(
        status, 502,
        "傳完的再 PUT 一次，server 那邊已經沒有這個上傳：{again}"
    );
    let sent = client
        .call(
            "room.send_attachment",
            json!({ "room": encrypted_room, "manifest": manifest, "room_devices": refreshed["result"] }),
        )
        .await;
    assert_eq!(sent["code"], 0, "加密房的附件：{sent}");
    let encrypted_event = sent["result"]["event_id"].as_str().unwrap().to_string();

    // 拉回來解開，跟送出去的一樣。
    let out = dir.path().join("secret.out");
    let out_uri = format!(
        "file:///{}",
        out.display()
            .to_string()
            .replace('\\', "/")
            .trim_start_matches('/')
    );
    let saved = client
        .call(
            "media.export_to",
            json!({ "manifest": manifest, "to": out_uri }),
        )
        .await;
    assert_eq!(saved["code"], 0, "media.export_to: {saved}");
    assert_eq!(saved["result"]["to"], out_uri);
    assert_eq!(std::fs::read(&out).unwrap(), body, "解回來要一模一樣");

    // 加密房那則：解得開、認得出是檔案、指著同一個 mxc。
    let mut found = Value::Null;
    for _ in 0..50 {
        let files = client
            .call(
                "room.files",
                json!({ "room": encrypted_room, "limit": 20, "sync": "both" }),
            )
            .await;
        assert_eq!(files["code"], 0, "room.files: {files}");
        if let Some(file) = files["result"]["files"]
            .as_array()
            .unwrap()
            .iter()
            .find(|file| file["event_id"] == encrypted_event.as_str())
        {
            found = file.clone();
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    assert_eq!(found["manifest"]["mxc"], manifest["mxc"], "{found}");

    // ── 讀（/docs/design/media/media-download.md §7）：池清掉 → media.open 排隊 → 從中間 seek → 等佇列拉完 → 整檔 ──
    let subscribed = client
        .call("subscribe", json!({ "events": ["media.download"] }))
        .await;
    assert_eq!(subscribed["code"], 0, "{subscribed}");
    let gc = client
        .call("media.gc", json!({ "quota_mib": 0, "protect_days": 0 }))
        .await;
    assert_eq!(gc["code"], 0, "media.gc: {gc}");
    let by_event = json!({ "room": encrypted_room, "event_id": encrypted_event });
    let opened = client.call("media.open", by_event.clone()).await;
    assert_eq!(opened["code"], 0, "media.open: {opened}");
    assert_eq!(opened["result"]["mxc"], manifest["mxc"]);
    assert_eq!(opened["result"]["size"], body.len());
    assert!(
        matches!(
            opened["result"]["state"].as_str(),
            Some("queued" | "downloading")
        ),
        "池清掉了，打開就排進佇列：{opened}"
    );
    let url = opened["result"]["url"].as_str().unwrap().to_string();
    assert!(url.contains("/media/mxc/e-") && url.len() < 160, "{url}");
    let (status, head, window) = get(daemon.data_port, &url, Some("bytes=150000-150999")).await;
    assert_eq!(status, 206, "{head}");
    assert_eq!(
        window,
        body[150_000..151_000],
        "seek 到中間，拿到的就是那一段"
    );
    assert!(
        head.to_ascii_lowercase().contains(&format!(
            "content-range: bytes 150000-150999/{}",
            body.len()
        )),
        "{head}"
    );
    // 分塊的檔不看 `verified`：還在下載也是 2xx；每個回應都說它是哪種、驗到哪（/docs/design/rpc-specs/data-plane.md §8.2）。
    assert!(
        head.to_ascii_lowercase().contains("wbf-media-kind: 1"),
        "{head}"
    );
    assert!(
        head.to_ascii_lowercase().contains("wbf-media-verified: "),
        "{head}"
    );
    let mut state = Value::Null;
    for _ in 0..150 {
        state = client.call("media.download", by_event.clone()).await;
        assert_eq!(state["code"], 0, "media.download: {state}");
        if state["result"]["state"] == "complete" {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    assert_eq!(state["result"]["state"], "complete", "佇列拉完了：{state}");
    // 上傳時區塊帶 sha256：下載完自動驗過、正確（/docs/design/media/media-download.md §12.3）。
    assert_eq!(
        (&state["result"]["kind"], &state["result"]["verified"]),
        (&json!(1), &json!(1)),
        "{state}"
    );
    let (status, head, whole) = get(daemon.data_port, &url, None).await;
    assert_eq!(status, 200, "{head}");
    assert!(
        head.to_ascii_lowercase().contains("wbf-media-verified: 1"),
        "{head}"
    );
    assert_eq!(whole, body, "整檔跟送出去的一樣");
    assert!(
        client
            .pushes
            .iter()
            .any(|push| push["method"] == "media.download"
                && push["params"]["mxc"] == manifest["mxc"]
                && push["params"]["state"] == "complete"),
        "完成有推播：{:?}",
        client.pushes
    );
    let stats = client.call("media.stats", json!({})).await;
    assert_eq!(
        stats["result"]["pending_on_disk"], 0,
        "主檔進了池、seek 暫存檔刪了：{stats}"
    );

    // ── 加密房：串流（沒給大小）──
    let created = client
        .call(
            "media.create",
            json!({ "room": encrypted_room, "name": "stream.bin" }),
        )
        .await;
    assert_eq!(created["code"], 0, "media.create: {created}");
    let (status, manifest) = put(daemon.data_port, &created["result"], &body).await;
    assert_eq!(status, 200, "{manifest}");
    assert_eq!(manifest["block"]["file_size"], body.len());
    let sent = client
        .call(
            "room.send_attachment",
            json!({ "room": encrypted_room, "manifest": manifest, "room_devices": refreshed["result"] }),
        )
        .await;
    assert_eq!(sent["code"], 0, "串流傳完就送得出去：{sent}");

    stop_daemon(daemon, client).await;
}

/// 別的 client（Element…）送來的標準附件（/docs/design/media/media-download.md §12）：
///
/// | 送的 | 要看到 |
/// |---|---|
/// | `m.file` 帶 `file`（上游 `AttachmentEncryptor` 加密、`/_matrix/media/v3/upload` 傳上去） | `kind` 2、下載完 `verified` 1、推播有 `verifying`、GET 200 整檔一樣 |
/// | 同上，但上傳的密文翻一個 bit | `verified` 2、GET **412** 而 body 照給、匯出寫了檔而且回 **1501** |
/// | `m.image` 只帶 `url` | `kind` 3、`verified` 0、GET 200 整檔一樣 |
///
/// 送的那一方用原生 HTTP（同一個 alice 另登一台裝置），**不**經過我們的程式碼：它就是「別人的 client」。
#[tokio::test]
#[ignore = "needs a running wbfuwunel: WBF_E2E_SERVER, WBF_E2E_USER, WBF_E2E_PASSWORD_FILE, WBF_E2E_ROOM"]
async fn a_standard_matrix_attachment_downloads_over_http_and_reads_over_the_data_plane() {
    let server = std::env::var("WBF_E2E_SERVER").expect("WBF_E2E_SERVER");
    let user = std::env::var("WBF_E2E_USER").expect("WBF_E2E_USER");
    let room = std::env::var("WBF_E2E_ROOM").expect("WBF_E2E_ROOM");
    let password_file = std::env::var("WBF_E2E_PASSWORD_FILE").expect("WBF_E2E_PASSWORD_FILE");
    let password = std::fs::read_to_string(password_file).unwrap();
    let password = password.strip_suffix('\n').unwrap_or(&password).to_string();

    let dir = tempfile::Builder::new()
        .prefix("wd")
        .tempdir_in(std::env::temp_dir())
        .unwrap();
    let daemon = start_daemon(dir.path()).await;
    let mut client = Client::connect(daemon.port).await;
    let reply = client.call("vault.create", json!({})).await;
    assert_eq!(reply["code"], 0, "vault.create: {reply}");
    let reply = client
        .call(
            "account.add",
            json!({ "server": server, "user": user, "password": password, "device_name": "wbf-daemon standard media e2e" }),
        )
        .await;
    assert_eq!(reply["code"], 0, "account.add: {reply}");
    wait_for_links(&mut client, 5).await;
    let subscribed = client
        .call("subscribe", json!({ "events": ["media.download"] }))
        .await;
    assert_eq!(subscribed["code"], 0, "subscribe: {subscribed}");
    let other = OtherClient::login(&server, &user, &password).await;
    let body: Vec<u8> = (0..150_001u32)
        .map(|position| (position % 253) as u8)
        .collect();

    // ── kind 2：對的檔 ──
    let (cipher, mut file) = encrypt_like_element(&body);
    let mxc = other.upload(cipher).await;
    file["url"] = json!(mxc);
    let event_id = other
        .send(
            &room,
            json!({ "msgtype": "m.file", "body": "enc.bin", "info": { "size": body.len(), "mimetype": "application/octet-stream" }, "file": file }),
        )
        .await;
    let by_event = json!({ "room": room, "event_id": event_id });
    let opened = open_once_seen(&mut client, &by_event).await;
    assert_eq!(opened["result"]["kind"], 2, "{opened}");
    let url = opened["result"]["url"].as_str().unwrap().to_string();
    let state = download_until_complete(&mut client, &by_event).await;
    assert_eq!(
        (&state["result"]["kind"], &state["result"]["verified"]),
        (&json!(2), &json!(1)),
        "{state}"
    );
    let (status, head, whole) = get(daemon.data_port, &url, None).await;
    assert_eq!(status, 200, "{head}");
    let head = head.to_ascii_lowercase();
    assert!(
        head.contains("wbf-media-kind: 2") && head.contains("wbf-media-verified: 1"),
        "{head}"
    );
    assert_eq!(whole, body, "解開跟送的一樣");
    let pushed: Vec<&Value> = client
        .pushes
        .iter()
        .filter(|push| push["method"] == "media.download" && push["params"]["mxc"] == json!(mxc))
        .collect();
    assert!(
        pushed
            .iter()
            .any(|push| push["params"]["state"] == "verifying")
            && pushed.iter().any(
                |push| push["params"]["state"] == "complete" && push["params"]["verified"] == 1
            ),
        "推播先 verifying、再 complete 帶 verified 1：{pushed:?}"
    );
    // 同一個 mxc、另一把金鑰的事件（寫錯或偽造）：本地那一列說了算，回 1500、**不**下載（/docs/design/media/media-download.md §5.3）。
    let (_, mut forged_file) = encrypt_like_element(&body);
    forged_file["url"] = json!(mxc);
    let forged_event = other
        .send(
            &room,
            json!({ "msgtype": "m.file", "body": "enc.bin", "info": { "size": body.len() }, "file": forged_file }),
        )
        .await;
    let forged = json!({ "room": room, "event_id": forged_event });
    let refused = call_once_seen(&mut client, "media.download", &forged).await;
    assert_eq!(refused["code"], 1500, "{refused}");

    // ── kind 2：密文被改過一個 bit ──
    let (mut cipher, mut file) = encrypt_like_element(&body);
    cipher[1000] ^= 0x04;
    let mxc = other.upload(cipher).await;
    file["url"] = json!(mxc);
    let event_id = other
        .send(
            &room,
            json!({ "msgtype": "m.file", "body": "bad.bin", "info": { "size": body.len() }, "file": file }),
        )
        .await;
    let by_event = json!({ "room": room, "event_id": event_id });
    let opened = open_once_seen(&mut client, &by_event).await;
    let url = opened["result"]["url"].as_str().unwrap().to_string();
    let state = download_until_complete(&mut client, &by_event).await;
    assert_eq!(
        state["result"]["verified"], 2,
        "驗不過也是完成、不刪檔：{state}"
    );
    let (status, head, whole) = get(daemon.data_port, &url, None).await;
    assert_eq!(status, 412, "沒驗過的傳統加密檔：412、body 照給：{head}");
    assert_eq!(whole.len(), body.len());
    assert_eq!(
        whole[1000],
        body[1000] ^ 0x04,
        "AES-CTR 擋不住竄改：同一個 bit 跟著翻"
    );
    let out = dir.path().join("bad.out");
    let out_uri = format!(
        "file:///{}",
        out.display()
            .to_string()
            .replace('\\', "/")
            .trim_start_matches('/')
    );
    let exported = client
        .call(
            "media.export_to",
            json!({ "room": room, "event_id": event_id, "to": out_uri }),
        )
        .await;
    assert_eq!(exported["code"], 1501, "照匯、回 unverified：{exported}");
    assert_eq!(exported["data"]["verified"], 2, "{exported}");
    assert_eq!(std::fs::read(&out).unwrap().len(), body.len(), "檔寫了");

    // ── kind 3：只有 url ──
    let mxc = other.upload(body.clone()).await;
    let event_id = other
        .send(
            &room,
            json!({ "msgtype": "m.image", "body": "p.png", "url": mxc, "info": { "size": body.len(), "mimetype": "image/png" } }),
        )
        .await;
    let by_event = json!({ "room": room, "event_id": event_id });
    let opened = open_once_seen(&mut client, &by_event).await;
    assert_eq!(opened["result"]["kind"], 3, "{opened}");
    let url = opened["result"]["url"].as_str().unwrap().to_string();
    let state = download_until_complete(&mut client, &by_event).await;
    assert_eq!(state["result"]["verified"], 0, "沒有 hash 可比：{state}");
    let (status, head, whole) = get(daemon.data_port, &url, None).await;
    assert_eq!(status, 200, "{head}");
    assert!(
        head.to_ascii_lowercase().contains("wbf-media-kind: 3"),
        "{head}"
    );
    assert_eq!(whole, body);

    // ── 清本地（`media.delete_local`，/docs/design/media/media-download.md §7.4）：之後只給 mxc 是 1100、從訊息點下載由那則重建 ──
    let deleted = client
        .call("media.delete_local", json!({ "mxc": mxc }))
        .await;
    println!("media.delete_local ← {deleted}");
    assert_eq!(
        (&deleted["code"], &deleted["result"]),
        (
            &json!(0),
            &json!({ "mxc": mxc, "removed": true, "cancelled": false })
        ),
        "{deleted}"
    );
    let by_mxc = client.call("media.download", json!({ "mxc": mxc })).await;
    println!("media.download {{ mxc }} ← {by_mxc}");
    assert_eq!(by_mxc["code"], 1100, "no local record: {by_mxc}");
    let from_message = json!({ "room": room, "event_id": event_id, "mxc": mxc });
    let state = download_until_complete(&mut client, &from_message).await;
    assert_eq!(state["result"]["kind"], 3, "{state}");
    let (status, _, whole) = get(daemon.data_port, &url, None).await;
    assert_eq!((status, whole), (200, body));

    other.logout().await;
    stop_daemon(daemon, client).await;
}

/// 房間動作（/docs/design/rooms/room-actions.md）：兩個帳號都登在**同一個 daemon**（共用 `cache.db`），一路看 server 怎麼答、本地的列怎麼跟著走。
///
/// 額外要 `WBF_E2E_USER_B`／`WBF_E2E_PASSWORD_B_FILE`（另一個帳號，例 `@bob:localhost`）。每一步看 code 與 server 的 body，**不**看 msg。
///
/// | 步驟 | 驗什麼 |
/// |---|---|
/// | alice `room.create`（明文） | 回 server 的 `{room_id}`；本地 alice 那列 `join` |
/// | alice `room.invite` bob | bob 是本機帳號 → bob 那列 `invite`（`room.list membership: ["invite"]`） |
/// | bob `room.join` | bob 那列 `join` |
/// | server 推 bob 的 `Invite/Push`（/docs/design/rooms/room-actions.md §3.3） | `room.membership { membership: "invite", invite }`：邀請者是 alice、`is_from_this_server: true`、縮減狀態含 bob 自己的成員事件；`room.list` 那一列帶同一份；`room.invites` 補一輪也拿得到 |
/// | bob `room.join` 之後 | `room.membership { membership: "join" }`（server 的 `Gone(join)`）、`room.invites` 不再有它 |
/// | alice `room.set_power_levels { events_default: 100 }` | alice 本地重算成 `channel`；bob `room.get sync=both` 看到 `can_send_message: false` |
/// | alice `room.set_name`、`room.get_state` | 寫進去的讀得回來 |
/// | alice 送字、`room.pin` | `room.get` 的 `state` 帶 `m.room.pinned_events` |
/// | alice `room.enable_encryption` | 本地標加密；沒帶 `room_devices` 送字 1100；帶了送出去，別的 client 從 server 拿到的是 `m.room.encrypted` |
/// | bob（被設成發不了言之後）`room.set_name` | 1400，`data` 是 server 的 403 `M_FORBIDDEN` |
/// | alice `room.kick` bob | bob 那列 `leave` |
/// | bob `room.forget` | 還有 alice 看得到 → `history_cleared: false` |
/// | alice `room.leave`、`room.forget` | 只剩自己 → `history_cleared: true`，列沒了 |
#[tokio::test]
#[ignore = "needs a running wbfuwunel: WBF_E2E_SERVER, WBF_E2E_USER, WBF_E2E_PASSWORD_FILE, WBF_E2E_USER_B, WBF_E2E_PASSWORD_B_FILE"]
async fn room_actions_walk_the_server_and_the_local_rows_of_two_accounts() {
    let server = std::env::var("WBF_E2E_SERVER").expect("WBF_E2E_SERVER");
    let alice = std::env::var("WBF_E2E_USER").expect("WBF_E2E_USER");
    let bob = std::env::var("WBF_E2E_USER_B").expect("WBF_E2E_USER_B");
    let read_password = |name: &str| {
        let text = std::fs::read_to_string(std::env::var(name).expect(name)).unwrap();
        text.strip_suffix('\n').unwrap_or(&text).to_string()
    };
    let (alice_password, bob_password) = (
        read_password("WBF_E2E_PASSWORD_FILE"),
        read_password("WBF_E2E_PASSWORD_B_FILE"),
    );

    let dir = tempfile::Builder::new()
        .prefix("wd")
        .tempdir_in(std::env::temp_dir())
        .unwrap();
    let daemon = start_daemon(dir.path()).await;
    let mut client = Client::connect(daemon.port).await;
    let reply = client.call("vault.create", json!({})).await;
    assert_eq!(reply["code"], 0, "vault.create: {reply}");
    for (user, password) in [(&alice, &alice_password), (&bob, &bob_password)] {
        let reply = client
            .call(
                "account.add",
                json!({ "server": server, "user": user, "password": password, "device_name": "wbf-daemon room actions e2e" }),
            )
            .await;
        assert_eq!(reply["code"], 0, "account.add {user}: {reply}");
    }
    wait_for_links(&mut client, 10).await;
    let subscribed = client
        .call("subscribe", json!({ "events": ["room.membership"] }))
        .await;
    assert_eq!(subscribed["code"], 0, "subscribe: {subscribed}");
    let membership_of = |listed: &Value, room: &str| -> Option<Value> {
        listed["result"]
            .as_array()?
            .iter()
            .find(|entry| entry["id"] == room)
            .map(|entry| entry["membership"].clone())
    };
    let every_membership = json!(["join", "invite", "knock", "leave", "ban"]);

    // ── 建房、邀請、加入 ──
    let created = client
        .call(
            "room.create",
            json!({ "user": alice, "encrypted": false, "name": "room actions e2e", "preset": "private_chat" }),
        )
        .await;
    println!("room.create ← {created}");
    assert_eq!(created["code"], 0, "{created}");
    let room = created["result"]["room_id"].as_str().unwrap().to_string();
    let listed = client.call("room.list", json!({ "user": alice })).await;
    assert_eq!(
        membership_of(&listed, &room),
        Some(json!("join")),
        "{listed}"
    );

    let invited = client
        .call(
            "room.invite",
            json!({ "user": alice, "room": room, "user_id": bob, "reason": "e2e" }),
        )
        .await;
    println!("room.invite ← {invited}");
    assert_eq!(
        (&invited["code"], &invited["result"]),
        (&json!(0), &json!({})),
        "{invited}"
    );
    let listed = client
        .call(
            "room.list",
            json!({ "user": bob, "membership": ["invite"] }),
        )
        .await;
    assert_eq!(
        membership_of(&listed, &room),
        Some(json!("invite")),
        "bob 是本機帳號：{listed}"
    );

    // ── 收邀請（/docs/design/rooms/room-actions.md §3.3）：server 從 bob 的房間線推 `Invite/Push` → `room.membership`；`room.list` 的那一列帶 `invite`；`room.invites` 補一輪拿得到同一間 ──
    let invited_push = wait_for_push(&mut client, |push| {
        push["method"] == "room.membership"
            && push["params"]["user"] == bob.as_str()
            && push["params"]["room"] == room.as_str()
            && push["params"]["membership"] == "invite"
    })
    .await;
    println!("room.membership ← {invited_push}");
    let pushed_invite = &invited_push["params"]["invite"];
    assert_eq!(pushed_invite["inviter"], alice.as_str(), "{invited_push}");
    assert_eq!(pushed_invite["reason"], "e2e", "{invited_push}");
    assert_eq!(pushed_invite["is_from_this_server"], true, "{invited_push}");
    assert!(
        pushed_invite["state"].as_array().is_some_and(|state| state
            .iter()
            .any(|event| event["type"] == "m.room.member" && event["state_key"] == bob.as_str())),
        "縮減狀態含 bob 自己的成員事件：{invited_push}"
    );
    let listed = client
        .call(
            "room.list",
            json!({ "user": bob, "membership": ["invite"] }),
        )
        .await;
    let listed_invite = listed["result"]
        .as_array()
        .and_then(|entries| entries.iter().find(|entry| entry["id"] == room.as_str()))
        .map(|entry| entry["invite"].clone())
        .unwrap_or(Value::Null);
    assert_eq!(
        &listed_invite, pushed_invite,
        "room.list 帶推播寫進來的那份：{listed}"
    );
    let fetched_invites = client.call("room.invites", json!({ "user": bob })).await;
    println!("room.invites ← {fetched_invites}");
    assert_eq!(fetched_invites["code"], 0, "{fetched_invites}");
    let one = fetched_invites["result"]["invites"]
        .as_array()
        .and_then(|invites| {
            invites
                .iter()
                .find(|invite| invite["room"] == room.as_str())
        })
        .cloned()
        .unwrap_or(Value::Null);
    assert_eq!(one["inviter"], alice.as_str(), "{fetched_invites}");
    assert_eq!(one["is_from_this_server"], true, "{fetched_invites}");

    let joined = client
        .call("room.join", json!({ "user": bob, "room": room }))
        .await;
    assert_eq!(joined["code"], 0, "{joined}");
    assert_eq!(joined["result"]["room_id"], room.as_str());
    // 接受了：server 推 `Invite/Gone(join)` → `room.membership { membership: "join" }`、沒有 `invite`
    let gone_push = wait_for_push(&mut client, |push| {
        push["method"] == "room.membership"
            && push["params"]["user"] == bob.as_str()
            && push["params"]["room"] == room.as_str()
            && push["params"]["membership"] == "join"
    })
    .await;
    assert!(gone_push["params"].get("invite").is_none(), "{gone_push}");
    let invites_after = client.call("room.invites", json!({ "user": bob })).await;
    assert!(
        invites_after["result"]["invites"]
            .as_array()
            .is_some_and(|invites| invites.iter().all(|invite| invite["room"] != room.as_str())),
        "加入之後不再是待處理的邀請：{invites_after}"
    );
    let listed = client.call("room.list", json!({ "user": bob })).await;
    assert_eq!(
        membership_of(&listed, &room),
        Some(json!("join")),
        "{listed}"
    );

    // ── 狀態：權限、名字、置頂 ──
    let fetched = client
        .call(
            "room.get",
            json!({ "user": alice, "room": room, "sync": "both" }),
        )
        .await;
    assert_eq!(fetched["code"], 0, "{fetched}");
    let state = fetched["result"]["state"]
        .as_array()
        .expect("room.get 帶整份狀態");
    assert!(
        state.iter().any(|event| event["type"] == "m.room.create"),
        "{fetched}"
    );
    assert!(
        state.iter().all(|event| event["type"] != "m.room.member"),
        "成員事件不放進 state"
    );

    let channel = client
        .call(
            "room.set_power_levels",
            json!({ "user": alice, "room": room, "events_default": 100 }),
        )
        .await;
    println!("room.set_power_levels ← {channel}");
    assert_eq!(channel["code"], 0, "{channel}");
    assert!(channel["result"]["event_id"].is_string(), "{channel}");
    let local = client
        .call("room.get", json!({ "user": alice, "room": room }))
        .await;
    assert_eq!(local["result"]["kind"], "channel", "本地跟著重算：{local}");
    let bob_view = client
        .call(
            "room.get",
            json!({ "user": bob, "room": room, "sync": "both" }),
        )
        .await;
    assert_eq!(bob_view["result"]["can_send_message"], false, "{bob_view}");

    let named = client
        .call(
            "room.set_name",
            json!({ "user": alice, "room": room, "name": "renamed" }),
        )
        .await;
    assert_eq!(named["code"], 0, "{named}");
    let name = client
        .call(
            "room.get_state",
            json!({ "user": alice, "room": room, "event_type": "m.room.name" }),
        )
        .await;
    assert_eq!(name["result"], json!({ "name": "renamed" }), "{name}");
    let missing = client
        .call(
            "room.get_state",
            json!({ "user": alice, "room": room, "event_type": "m.room.topic" }),
        )
        .await;
    assert_eq!(
        (&missing["code"], &missing["result"]),
        (&json!(0), &Value::Null),
        "沒有這一項是 null：{missing}"
    );

    let refused = client
        .call(
            "room.set_name",
            json!({ "user": bob, "room": room, "name": "mine now" }),
        )
        .await;
    println!("room.set_name (bob) ← {refused}");
    assert_eq!(refused["code"], 1400, "{refused}");
    assert_eq!(
        (&refused["data"]["status"], &refused["data"]["errcode"]),
        (&json!(403), &json!("M_FORBIDDEN")),
        "server 的錯照原樣：{refused}"
    );

    let sent = client
        .call(
            "room.send_text",
            json!({ "user": alice, "room": room, "body": "pin me" }),
        )
        .await;
    assert_eq!(sent["code"], 0, "建房時記了明文，送得出去：{sent}");
    let event_id = sent["result"]["event_id"].as_str().unwrap().to_string();
    let pinned = client
        .call(
            "room.pin",
            json!({ "user": alice, "room": room, "event_id": event_id, "pinned": true }),
        )
        .await;
    assert_eq!(pinned["code"], 0, "{pinned}");
    let again = client
        .call(
            "room.pin",
            json!({ "user": alice, "room": room, "event_id": event_id, "pinned": true }),
        )
        .await;
    assert_eq!(again["result"], json!({}), "已經置頂：不寫：{again}");
    let fetched = client
        .call(
            "room.get",
            json!({ "user": alice, "room": room, "sync": "both" }),
        )
        .await;
    let pins = fetched["result"]["state"]
        .as_array()
        .unwrap()
        .iter()
        .find(|event| event["type"] == "m.room.pinned_events")
        .map(|event| event["content"]["pinned"].clone());
    assert_eq!(pins, Some(json!([event_id])), "{fetched}");

    // ── 明文房升級加密 ──
    let enabled = client
        .call(
            "room.enable_encryption",
            json!({ "user": alice, "room": room }),
        )
        .await;
    println!("room.enable_encryption ← {enabled}");
    assert_eq!(enabled["code"], 0, "{enabled}");
    let local = client
        .call("room.get", json!({ "user": alice, "room": room }))
        .await;
    assert_eq!(local["result"]["encrypted"], true, "{local}");
    let plain = client
        .call(
            "room.send_text",
            json!({ "user": alice, "room": room, "body": "should not go out" }),
        )
        .await;
    assert_eq!(plain["code"], 1100, "本地知道加密了：不送明文：{plain}");
    let refreshed = client
        .call(
            "room.refresh_devices",
            json!({ "user": alice, "room": room }),
        )
        .await;
    assert_eq!(refreshed["code"], 0, "{refreshed}");
    let secret = client
        .call(
            "room.send_text",
            json!({ "user": alice, "room": room, "body": "now encrypted", "room_devices": refreshed["result"] }),
        )
        .await;
    assert_eq!(secret["code"], 0, "{secret}");
    let other = OtherClient::login(&server, &alice, &alice_password).await;
    let raw = other
        .event(&room, secret["result"]["event_id"].as_str().unwrap())
        .await;
    assert_eq!(raw["type"], "m.room.encrypted", "server 上是密文：{raw}");
    other.logout().await;

    // ── 踢人、忘記 ──
    let kicked = client
        .call(
            "room.kick",
            json!({ "user": alice, "room": room, "user_id": bob }),
        )
        .await;
    assert_eq!(kicked["code"], 0, "{kicked}");
    let listed = client
        .call(
            "room.list",
            json!({ "user": bob, "membership": every_membership }),
        )
        .await;
    assert_eq!(
        membership_of(&listed, &room),
        Some(json!("leave")),
        "{listed}"
    );
    let forgot = client
        .call("room.forget", json!({ "user": bob, "room": room }))
        .await;
    println!("room.forget (bob) ← {forgot}");
    assert_eq!(
        forgot["result"],
        json!({ "history_cleared": false }),
        "alice 還看得到：{forgot}"
    );

    let left = client
        .call("room.leave", json!({ "user": alice, "room": room }))
        .await;
    assert_eq!(left["code"], 0, "{left}");
    let forgot = client
        .call("room.forget", json!({ "user": alice, "room": room }))
        .await;
    println!("room.forget (alice) ← {forgot}");
    assert_eq!(
        forgot["result"],
        json!({ "history_cleared": true }),
        "{forgot}"
    );
    let listed = client
        .call(
            "room.list",
            json!({ "user": alice, "membership": every_membership }),
        )
        .await;
    assert_eq!(membership_of(&listed, &room), None, "{listed}");

    stop_daemon(daemon, client).await;
}

/// 訊息動作（/docs/design/messages/message-actions.md §7）：alice 回覆 bob、按表情、改字、收回表情、收回訊息，明文房與加密房各走一次；
/// bob 從 `room.history` 看到折完的結果，alice 自己讀時 `mine` 有值、收回後沒有。兩個帳號登在同一個 daemon。
///
/// | 步驟 | 驗什麼 |
/// |---|---|
/// | alice `room.send_text { reply_to }` | bob 讀到 `reply_to`；server 上那則的 `m.relates_to` 在明文那層（加密房也是），明文房的 `m.mentions` 是 `[bob]` |
/// | alice `room.react` 兩次 | 第二次回同一個 id（冪等）；bob 讀到 `by: [alice]`、沒有 `mine`；alice 讀到 `mine` |
/// | alice `room.edit` 自己那則 | bob 讀到新的字、`edited_by: alice` |
/// | alice `room.redact` 自己的表情（`mine`） | bob 讀到 `reactions` 空了；alice 的 `mine` 沒了 |
/// | bob `room.redact` alice 那則 | 1400，`data` 是 server 的 403（private_chat 的 `redact` 門檻 50） |
/// | alice `room.redact` 自己那則 | bob 讀到 `kind: deleted`、`reason` |
#[tokio::test]
#[ignore = "needs a running wbfuwunel: WBF_E2E_SERVER, WBF_E2E_USER, WBF_E2E_PASSWORD_FILE, WBF_E2E_USER_B, WBF_E2E_PASSWORD_B_FILE"]
async fn message_actions_reply_react_edit_and_redact_between_two_accounts() {
    let server = std::env::var("WBF_E2E_SERVER").expect("WBF_E2E_SERVER");
    let alice = std::env::var("WBF_E2E_USER").expect("WBF_E2E_USER");
    let bob = std::env::var("WBF_E2E_USER_B").expect("WBF_E2E_USER_B");
    let read_password = |name: &str| {
        let text = std::fs::read_to_string(std::env::var(name).expect(name)).unwrap();
        text.strip_suffix('\n').unwrap_or(&text).to_string()
    };
    let alice_password = read_password("WBF_E2E_PASSWORD_FILE");
    let bob_password = read_password("WBF_E2E_PASSWORD_B_FILE");

    let dir = tempfile::Builder::new()
        .prefix("wd")
        .tempdir_in(std::env::temp_dir())
        .unwrap();
    let daemon = start_daemon(dir.path()).await;
    let mut client = Client::connect(daemon.port).await;
    let reply = client.call("vault.create", json!({})).await;
    assert_eq!(reply["code"], 0, "vault.create: {reply}");
    for (user, password) in [(&alice, &alice_password), (&bob, &bob_password)] {
        let reply = client
            .call(
                "account.add",
                json!({ "server": server, "user": user, "password": password, "device_name": "wbf-daemon message actions e2e" }),
            )
            .await;
        assert_eq!(reply["code"], 0, "account.add {user}: {reply}");
    }
    wait_for_links(&mut client, 10).await;

    for encrypted in [false, true] {
        println!("── encrypted: {encrypted} ──");
        let created = client
            .call(
                "room.create",
                json!({ "user": alice, "encrypted": encrypted, "name": "message actions e2e", "preset": "private_chat" }),
            )
            .await;
        assert_eq!(created["code"], 0, "{created}");
        let room = created["result"]["room_id"].as_str().unwrap().to_string();
        let invited = client
            .call(
                "room.invite",
                json!({ "user": alice, "room": room, "user_id": bob }),
            )
            .await;
        assert_eq!(invited["code"], 0, "{invited}");
        let joined = client
            .call("room.join", json!({ "user": bob, "room": room }))
            .await;
        assert_eq!(joined["code"], 0, "{joined}");
        for user in [&alice, &bob] {
            let fetched = client
                .call(
                    "room.get",
                    json!({ "user": user, "room": room, "sync": "both" }),
                )
                .await;
            assert_eq!(fetched["result"]["encrypted"], encrypted, "{fetched}");
        }
        // 加密房：兩邊都拿房間狀態（之後每一則都帶它；這一輪裡沒有人加裝置，號碼不會變）。
        let mut devices = std::collections::BTreeMap::new();
        for user in [&alice, &bob] {
            let mut send = json!({ "user": user, "room": room });
            if encrypted {
                let refreshed = client
                    .call(
                        "room.refresh_devices",
                        json!({ "user": user, "room": room }),
                    )
                    .await;
                assert_eq!(refreshed["code"], 0, "{refreshed}");
                send["room_devices"] = refreshed["result"].clone();
            }
            devices.insert(user.clone(), send);
        }
        let with = |user: &String, fields: Value| {
            let mut params = devices[user].clone();
            for (name, value) in fields.as_object().unwrap() {
                params[name] = value.clone();
            }
            params
        };

        // ── bob 問、alice 回覆 ──
        let asked = client
            .call("room.send_text", with(&bob, json!({ "body": "要吃什麼" })))
            .await;
        assert_eq!(asked["code"], 0, "{asked}");
        let asked = asked["result"]["event_id"].as_str().unwrap().to_string();
        // 被回覆的人從 alice 的本地查：等 bob 那則進了 alice 的本地（加密房要等金鑰到、解得開）。
        history_until(&mut client, &alice, &room, &asked, |message| {
            message["kind"] == "text"
        })
        .await;
        let answered = client
            .call(
                "room.send_text",
                with(&alice, json!({ "body": "好", "reply_to": asked })),
            )
            .await;
        println!("room.send_text (reply) ← {answered}");
        assert_eq!(answered["code"], 0, "{answered}");
        let answer = answered["result"]["event_id"].as_str().unwrap().to_string();
        let seen = history_until(&mut client, &bob, &room, &answer, |message| {
            message["kind"] == "text"
        })
        .await;
        assert_eq!(seen["reply_to"], asked.as_str(), "{seen}");

        // ── 表情：按兩次只有一則 ──
        let reacted = client
            .call(
                "room.react",
                with(&alice, json!({ "event_id": asked, "key": "👍" })),
            )
            .await;
        println!("room.react ← {reacted}");
        assert_eq!(reacted["code"], 0, "{reacted}");
        let thumbs = reacted["result"]["event_id"].as_str().unwrap().to_string();
        history_until(&mut client, &alice, &room, &asked, |message| {
            message["reactions"][0]["mine"] == thumbs.as_str()
        })
        .await;
        let again = client
            .call(
                "room.react",
                with(&alice, json!({ "event_id": asked, "key": "👍" })),
            )
            .await;
        assert_eq!(
            again["result"]["event_id"],
            thumbs.as_str(),
            "冪等：{again}"
        );
        let seen = history_until(&mut client, &bob, &room, &asked, |message| {
            message["reactions"].is_array()
        })
        .await;
        assert_eq!(
            seen["reactions"],
            json!([{ "key": "👍", "by": [alice] }]),
            "bob 沒按過：沒有 mine：{seen}"
        );

        // ── 改字 ──
        let edited = client
            .call(
                "room.edit",
                with(&alice, json!({ "event_id": answer, "body": "好，吃拉麵" })),
            )
            .await;
        println!("room.edit ← {edited}");
        assert_eq!(edited["code"], 0, "{edited}");
        let edit = edited["result"]["event_id"].as_str().unwrap().to_string();
        let seen = history_until(&mut client, &bob, &room, &answer, |message| {
            message["body"] == "好，吃拉麵"
        })
        .await;
        assert_eq!(seen["edited_by"], alice.as_str(), "{seen}");
        assert_eq!(
            seen["reply_to"],
            asked.as_str(),
            "回覆關係留在原訊息上：{seen}"
        );

        // ── server 上的原樣（別人的 client 看到的）：關係在明文那層 ──
        // 所有加密的送出都在這之前：多登一台裝置會讓房間版本號變（之後的收回是明文、不受影響）。
        let other = OtherClient::login(&server, &alice, &alice_password).await;
        let raw_answer = other.event(&room, &answer).await;
        assert_eq!(
            raw_answer["content"]["m.relates_to"]["m.in_reply_to"]["event_id"],
            asked.as_str(),
            "{raw_answer}"
        );
        let raw_edit = other.event(&room, &edit).await;
        assert_eq!(
            raw_edit["content"]["m.relates_to"],
            json!({ "rel_type": "m.replace", "event_id": answer }),
            "{raw_edit}"
        );
        let raw_thumbs = other.event(&room, &thumbs).await;
        if encrypted {
            assert_eq!(raw_answer["type"], "m.room.encrypted", "{raw_answer}");
            assert_eq!(raw_thumbs["type"], "m.room.encrypted", "{raw_thumbs}");
        } else {
            assert_eq!(
                raw_answer["content"]["m.mentions"],
                json!({ "user_ids": [bob] }),
                "{raw_answer}"
            );
            assert_eq!(raw_thumbs["type"], "m.reaction", "{raw_thumbs}");
        }
        assert_eq!(
            raw_thumbs["content"]["m.relates_to"],
            json!({ "rel_type": "m.annotation", "event_id": asked, "key": "👍" }),
            "{raw_thumbs}"
        );
        other.logout().await;

        // ── 收回自己的表情（UI 拿 mine 叫 room.redact）──
        let undone = client
            .call(
                "room.redact",
                json!({ "user": alice, "room": room, "event_id": thumbs }),
            )
            .await;
        println!("room.redact (reaction) ← {undone}");
        assert_eq!(undone["code"], 0, "{undone}");
        assert_ne!(
            undone["result"]["event_id"],
            thumbs.as_str(),
            "回的是收回動作本身那則"
        );
        history_until(&mut client, &bob, &room, &asked, |message| {
            message.get("reactions").is_none()
        })
        .await;
        history_until(&mut client, &alice, &room, &asked, |message| {
            message.get("reactions").is_none()
        })
        .await;

        // ── 收回訊息：別人的要權限，自己的可以 ──
        let refused = client
            .call(
                "room.redact",
                json!({ "user": bob, "room": room, "event_id": answer }),
            )
            .await;
        println!("room.redact (bob, not his) ← {refused}");
        assert_eq!(refused["code"], 1400, "{refused}");
        assert_eq!(refused["data"]["status"], 403, "{refused}");
        let deleted = client
            .call(
                "room.redact",
                json!({ "user": alice, "room": room, "event_id": answer, "reason": "改天" }),
            )
            .await;
        assert_eq!(deleted["code"], 0, "{deleted}");
        let seen = history_until(&mut client, &bob, &room, &answer, |message| {
            message["kind"] == "deleted"
        })
        .await;
        assert_eq!(seen["reason"], "改天", "{seen}");
    }

    stop_daemon(daemon, client).await;
}

/// /docs/design/messages/read-receipts.md：bob 送兩則（第二則提到 alice），alice 看未讀、標已讀（本地與上游）、換 conf 的預設。
/// 私人的回條 bob 看不到、公開的看得到（server 上的事實，用另一個 client 的 /sync 看）。
#[tokio::test]
#[ignore = "needs a running wbfuwunel: WBF_E2E_SERVER, WBF_E2E_USER, WBF_E2E_PASSWORD_FILE, WBF_E2E_USER_B, WBF_E2E_PASSWORD_B_FILE"]
async fn read_receipts_mark_local_and_upstream_and_count_unread() {
    let server = std::env::var("WBF_E2E_SERVER").expect("WBF_E2E_SERVER");
    let alice = std::env::var("WBF_E2E_USER").expect("WBF_E2E_USER");
    let bob = std::env::var("WBF_E2E_USER_B").expect("WBF_E2E_USER_B");
    let read_password = |name: &str| {
        let text = std::fs::read_to_string(std::env::var(name).expect(name)).unwrap();
        text.strip_suffix('\n').unwrap_or(&text).to_string()
    };
    let alice_password = read_password("WBF_E2E_PASSWORD_FILE");
    let bob_password = read_password("WBF_E2E_PASSWORD_B_FILE");

    let dir = tempfile::Builder::new()
        .prefix("wd")
        .tempdir_in(std::env::temp_dir())
        .unwrap();
    let daemon = start_daemon(dir.path()).await;
    let mut client = Client::connect(daemon.port).await;
    let reply = client.call("vault.create", json!({})).await;
    assert_eq!(reply["code"], 0, "vault.create: {reply}");
    for (user, password) in [(&alice, &alice_password), (&bob, &bob_password)] {
        let reply = client
            .call(
                "account.add",
                json!({ "server": server, "user": user, "password": password, "device_name": "wbf-daemon read receipts e2e" }),
            )
            .await;
        assert_eq!(reply["code"], 0, "account.add {user}: {reply}");
    }
    wait_for_links(&mut client, 10).await;

    let created = client
        .call(
            "room.create",
            json!({ "user": alice, "encrypted": false, "name": "read receipts e2e", "preset": "private_chat" }),
        )
        .await;
    assert_eq!(created["code"], 0, "{created}");
    let room = created["result"]["room_id"].as_str().unwrap().to_string();
    let invited = client
        .call(
            "room.invite",
            json!({ "user": alice, "room": room, "user_id": bob }),
        )
        .await;
    assert_eq!(invited["code"], 0, "{invited}");
    let joined = client
        .call("room.join", json!({ "user": bob, "room": room }))
        .await;
    assert_eq!(joined["code"], 0, "{joined}");
    for user in [&alice, &bob] {
        let fetched = client
            .call(
                "room.get",
                json!({ "user": user, "room": room, "sync": "both" }),
            )
            .await;
        assert_eq!(fetched["code"], 0, "{fetched}");
    }

    let mut sent = Vec::new();
    for content in [
        json!({ "body": "第一則" }),
        json!({ "body": "第二則", "mentions": [alice] }),
    ] {
        let mut params = json!({ "user": bob, "room": room });
        for (name, value) in content.as_object().unwrap() {
            params[name] = value.clone();
        }
        let reply = client.call("room.send_text", params).await;
        assert_eq!(reply["code"], 0, "{reply}");
        sent.push(reply["result"]["event_id"].as_str().unwrap().to_string());
    }
    let (first, second) = (sent[0].clone(), sent[1].clone());
    history_until(&mut client, &alice, &room, &second, |message| {
        message["kind"] == "text"
    })
    .await;

    // ── 從沒標過：快取裡 bob 送的都算（含 bob 的加入，那是狀態事件），提到 alice 的是第二則 ──
    let fetched = client
        .call("room.get", json!({ "user": alice, "room": room }))
        .await;
    println!("room.get (unread) ← {}", fetched["result"]["unread"]);
    let unread = &fetched["result"]["unread"];
    assert!(unread["count"].as_u64().unwrap() >= 2, "{fetched}");
    assert_eq!(unread["mentions"], 1, "{fetched}");
    assert_eq!(unread["read_upto"], Value::Null, "{fetched}");

    // ── 標到第一則，兩邊都寫：沒帶 visible_to_others、conf 沒寫 → 私人 ──
    let marked = client
        .call(
            "room.read",
            json!({ "user": alice, "room": room, "event_id": first, "sync": "both" }),
        )
        .await;
    println!("room.read (both, private) ← {marked}");
    assert_eq!(marked["code"], 0, "{marked}");
    assert_eq!(marked["sync"], "both", "{marked}");
    assert_eq!(marked["result"]["sent"], "m.read.private", "{marked}");
    assert_eq!(marked["result"]["read_upto"]["event_id"], first.as_str());
    assert_eq!(
        marked["result"]["unread"],
        json!({ "count": 1, "mentions": 1 }),
        "{marked}"
    );

    // ── 換 conf 的預設成公開，重讀；這次沒帶參數就送 m.read ──
    std::fs::write(
        dir.path().join("wbf.conf"),
        "[read]\nREAD_RECEIPTS=public\n",
    )
    .unwrap();
    let reloaded = client.call("daemon.reload_conf", json!({})).await;
    println!("daemon.reload_conf ← {reloaded}");
    assert_eq!(reloaded["code"], 0, "{reloaded}");
    assert_eq!(reloaded["result"]["changed"], json!(["READ_RECEIPTS"]));
    let marked = client
        .call(
            "room.read",
            json!({ "user": alice, "room": room, "event_id": second, "sync": "both" }),
        )
        .await;
    println!("room.read (both, public by conf) ← {marked}");
    assert_eq!(marked["result"]["sent"], "m.read", "{marked}");
    assert_eq!(
        marked["result"]["unread"],
        json!({ "count": 0, "mentions": 0 }),
        "{marked}"
    );

    // ── 本地只往前：標回第一則不動 ──
    let marked_back = client
        .call(
            "room.read",
            json!({ "user": alice, "room": room, "event_id": first, "sync": "local" }),
        )
        .await;
    assert_eq!(marked_back["code"], 0, "{marked_back}");
    assert_eq!(marked_back["result"]["sent"], Value::Null);
    assert_eq!(
        marked_back["result"]["read_upto"]["event_id"],
        second.as_str(),
        "{marked_back}"
    );
    let fetched = client
        .call("room.get", json!({ "user": alice, "room": room }))
        .await;
    assert_eq!(
        fetched["result"]["unread"]["read_upto"]["event_id"],
        second.as_str()
    );

    // 📎 上游拒絕（1400、本地不寫）在 core 的單元測試驗：這台 server 對不存在的事件、不在的房間都回 200，
    // 只拒認不得的 `receipt_type`，而 daemon 只送 `m.read`／`m.read.private`（2026-11-06 實測）。

    // ── server 上的事實：bob 看得到 alice 的公開回條（第二則），看不到私人的（第一則）──
    let other = OtherClient::login(&server, &bob, &bob_password).await;
    let receipts = other.receipts_of(&room, &alice, &second).await;
    println!("bob's /sync m.receipt ← {receipts}");
    assert!(
        receipts[&second]["m.read"][&alice].is_object(),
        "{receipts}"
    );
    assert!(
        receipts
            .get(&first)
            .and_then(|by_type| by_type.get("m.read.private"))
            .is_none(),
        "私人的回條別人看不到：{receipts}"
    );
    other.logout().await;

    stop_daemon(daemon, client).await;
}

/// 交叉簽章、金鑰備份、recovery key、自我驗證（/docs/design/keys/cross-signing-and-verification.md §11.1 的 V1–V9、V12）。
/// 一台裝置＝一個 daemon（各自的資料目錄）；帳號是這次現註冊的（[`register_fresh_user`]）。
///
/// | 步驟 | 驗什麼 |
/// |---|---|
/// | V1 A1 登入 | server 上有三把交叉簽章公鑰、有備份版本；A1 在自己的 `devices.list` 裡是 verified |
/// | V2 A1 在加密房送一則、`backup.upload` | 備份的 `count` ≥ 1 |
/// | V3 `backup.create_recovery_key` | account data 有預設 key、四個秘密都用它加密 |
/// | V4 A2 登入 | master 公鑰沒變；A2 不是 verified；A2 讀那則解不開 |
/// | V5、V6 A2 對自己開驗證、A1 接受、比表情、雙方確認 | 兩邊 7 個表情一樣；雙方的 `devices.list` 都看到兩台 verified |
/// | V7 不叫任何 RPC | A2 收到那則解開的 `room.message`（備份金鑰由 A1 送來、自動還原） |
/// | V9 A3 用錯的 recovery key | 回錯誤、A3 仍然不是 verified |
/// | V8 A3 用對的 | A3 verified、讀得到那則 |
/// | V12 A1 開驗證又取消 | 兩邊推 `cancelled`；之後那個流程的 confirm 回錯 |
#[tokio::test]
#[ignore = "needs a running wbfuwunel: WBF_E2E_SERVER, WBF_E2E_REGISTRATION_TOKEN"]
async fn cross_signing_backup_recovery_and_self_verification_across_three_devices() {
    let server = std::env::var("WBF_E2E_SERVER").expect("WBF_E2E_SERVER");
    let (me, password) = register_fresh_user(&server, "xs").await;
    let other = OtherClient::login(&server, &me, &password).await;
    let account_data = |event_type: &str| {
        format!(
            "/_matrix/client/v3/user/{}/account_data/{event_type}",
            me.replace('@', "%40").replace(':', "%3A")
        )
    };

    // ── V1：第一台自動建交叉簽章與備份版本 ──
    let mut a1 = Device::login(&server, &me, &password, Settings::default()).await;
    // 金鑰線開好之後才建（`init_keys` 的最後一段），「五條線都開了」不保證已經建完：問到有為止。
    let mut keys = other.query_keys(&me).await;
    for _ in 0..50 {
        if keys["user_signing_keys"][&me].is_object() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        keys = other.query_keys(&me).await;
    }
    for kind in ["master_keys", "self_signing_keys", "user_signing_keys"] {
        assert!(keys[kind][&me].is_object(), "{kind} of {me}: {keys}");
    }
    let master = keys["master_keys"][&me].clone();
    let (status, backup) = other.get_json("/_matrix/client/v3/room_keys/version").await;
    assert_eq!(status, 200, "a backup version exists: {backup}");
    let a1_id = a1.device_id.clone();
    let trust = a1.devices_until_verified(&me, &a1_id).await;
    assert!(
        trust["devices"]
            .as_array()
            .unwrap()
            .iter()
            .any(|device| device["device_id"] == a1_id.as_str() && device["is_this_device"] == true),
        "{trust}"
    );

    // ── V2：送一則，備份裡有它的金鑰 ──
    let created = a1
        .call(
            "room.create",
            json!({ "encrypted": true, "name": "verification e2e", "preset": "private_chat" }),
        )
        .await;
    assert_eq!(created["code"], 0, "{created}");
    let room = created["result"]["room_id"].as_str().unwrap().to_string();
    let fetched = a1
        .call("room.get", json!({ "room": room, "sync": "both" }))
        .await;
    assert_eq!(fetched["code"], 0, "{fetched}");
    let refreshed = a1
        .call("room.refresh_devices", json!({ "room": room }))
        .await;
    assert_eq!(refreshed["code"], 0, "{refreshed}");
    let sent = a1
        .call(
            "room.send_text",
            json!({ "room": room, "body": "before the second device", "room_devices": refreshed["result"] }),
        )
        .await;
    assert_eq!(sent["code"], 0, "{sent}");
    let event_id = sent["result"]["event_id"].as_str().unwrap().to_string();
    let uploaded = a1.call("backup.upload", json!({})).await;
    assert_eq!(uploaded["code"], 0, "{uploaded}");
    let (_, backup) = other.get_json("/_matrix/client/v3/room_keys/version").await;
    assert!(backup["count"].as_u64().unwrap_or(0) >= 1, "{backup}");

    // ── V3：recovery key 與 secret storage ──
    let created_key = a1.call("backup.create_recovery_key", json!({})).await;
    assert_eq!(created_key["code"], 0, "{created_key}");
    let recovery_key = created_key["result"]["recovery_key"]
        .as_str()
        .unwrap()
        .to_string();
    let (status, default_key) = other
        .get_json(&account_data("m.secret_storage.default_key"))
        .await;
    assert_eq!(status, 200, "{default_key}");
    let key_id = default_key["key"].as_str().unwrap().to_string();
    let (status, key) = other
        .get_json(&account_data(&format!("m.secret_storage.key.{key_id}")))
        .await;
    assert_eq!(status, 200, "{key}");
    for secret in [
        "m.cross_signing.master",
        "m.cross_signing.self_signing",
        "m.cross_signing.user_signing",
        "m.megolm_backup.v1",
    ] {
        let (status, content) = other.get_json(&account_data(secret)).await;
        assert_eq!(status, 200, "{secret}: {content}");
        assert!(
            content["encrypted"][&key_id].is_object(),
            "{secret}: {content}"
        );
    }

    // ── V4：第二台不建新的，也還不被信任、讀不到舊的 ──
    let mut a2 = Device::login(&server, &me, &password, Settings::default()).await;
    let a2_id = a2.device_id.clone();
    let keys = other.query_keys(&me).await;
    assert_eq!(
        keys["master_keys"][&me], master,
        "the second device keeps the master key"
    );
    let trust = a2.call("devices.list", json!({ "user_id": me })).await;
    assert_eq!(trust["code"], 0, "{trust}");
    let row = trust["result"]["devices"]
        .as_array()
        .unwrap()
        .iter()
        .find(|device| device["device_id"] == a2_id.as_str())
        .cloned()
        .unwrap_or_else(|| panic!("A2 lists itself: {trust}"));
    assert_eq!(row["verified"], false, "{trust}");
    let fetched = a2
        .call("room.get", json!({ "room": room, "sync": "both" }))
        .await;
    assert_eq!(fetched["code"], 0, "{fetched}");
    history_until(&mut a2.client, &me, &room, &event_id, |message| {
        message["kind"] == "undecryptable"
    })
    .await;

    // ── V5、V6：自我驗證 ──
    let (on_a2, on_a1) = verify_with_sas(&mut a2, &mut a1, None).await;
    assert_eq!(on_a2.as_array().map(Vec::len), Some(7), "{on_a2}");
    assert_eq!(on_a2, on_a1, "both devices show the same emoji");
    a1.devices_until_verified(&me, &a2_id).await;
    a2.devices_until_verified(&me, &a1_id).await;
    a2.devices_until_verified(&me, &a2_id).await;

    // ── V7：A2 不叫任何 RPC，自動還原、補解那則 ──
    let wanted = event_id.clone();
    let opened = wait_for_push_within(&mut a2.client, 300, |push| {
        push["method"] == "room.message"
            && push["params"]["message"]["id"] == wanted.as_str()
            && push["params"]["message"]["kind"] == "text"
    })
    .await;
    assert_eq!(
        opened["params"]["message"]["body"], "before the second device",
        "{opened}"
    );

    // ── V9：錯的 recovery key 什麼都不匯 ──
    let mut a3 = Device::login(&server, &me, &password, Settings::default()).await;
    let a3_id = a3.device_id.clone();
    let wrong = matrix_sdk_crypto::secret_storage::SecretStorageKey::new().to_base58();
    let refused = a3
        .call("backup.restore", json!({ "recovery_key": wrong }))
        .await;
    assert_ne!(
        refused["code"], 0,
        "a wrong recovery key is refused: {refused}"
    );
    let trust = a3.call("devices.list", json!({ "user_id": me })).await;
    let row = trust["result"]["devices"]
        .as_array()
        .unwrap()
        .iter()
        .find(|device| device["device_id"] == a3_id.as_str())
        .cloned()
        .unwrap_or_else(|| panic!("A3 lists itself: {trust}"));
    assert_eq!(row["verified"], false, "nothing was imported: {trust}");

    // ── V8：對的 recovery key ──
    let restored = a3
        .call("backup.restore", json!({ "recovery_key": recovery_key }))
        .await;
    assert_eq!(restored["code"], 0, "{restored}");
    a3.devices_until_verified(&me, &a3_id).await;
    let fetched = a3
        .call("room.get", json!({ "room": room, "sync": "both" }))
        .await;
    assert_eq!(fetched["code"], 0, "{fetched}");
    history_until(&mut a3.client, &me, &room, &event_id, |message| {
        message["kind"] == "text"
    })
    .await;

    // ── V12：開了又取消 ──
    let requested = a1.call("verify.request", json!({ "user_id": me })).await;
    assert_eq!(requested["code"], 0, "{requested}");
    let flow_id = requested["result"]["flow_id"].as_str().unwrap().to_string();
    let seen = a2.wait_for_incoming_request(&me).await;
    assert_eq!(seen, flow_id);
    let cancelled = a1
        .call(
            "verify.cancel",
            json!({ "user_id": me, "flow_id": flow_id }),
        )
        .await;
    assert_eq!(cancelled["code"], 0, "{cancelled}");
    a1.wait_for_verify(&flow_id, "cancelled").await;
    a2.wait_for_verify(&flow_id, "cancelled").await;
    let late = a1
        .call(
            "verify.confirm",
            json!({ "user_id": me, "flow_id": flow_id }),
        )
        .await;
    assert_ne!(
        late["code"], 0,
        "a cancelled flow cannot be confirmed: {late}"
    );

    other.logout().await;
    for device in [a1, a2, a3] {
        device.stop().await;
    }
}

/// 驗別人的一台裝置（/docs/design/keys/cross-signing-and-verification.md §11.1 的 V10）：bob 對 alice 的那台開驗證、走完 SAS，
/// bob 的 `devices.list(alice)` 裡那台被信任、alice 這個人也被信任（SAS 的 MAC 含 master key，bob 用 user-signing key 簽了它）。
#[tokio::test]
#[ignore = "needs a running wbfuwunel: WBF_E2E_SERVER, WBF_E2E_REGISTRATION_TOKEN"]
async fn verifying_another_users_device_trusts_that_device_and_identity() {
    let server = std::env::var("WBF_E2E_SERVER").expect("WBF_E2E_SERVER");
    let (alice, alice_password) = register_fresh_user(&server, "va").await;
    let (bob, bob_password) = register_fresh_user(&server, "vb").await;
    let mut on_alice = Device::login(&server, &alice, &alice_password, Settings::default()).await;
    let mut on_bob = Device::login(&server, &bob, &bob_password, Settings::default()).await;
    let alice_device = on_alice.device_id.clone();

    let (seen_by_bob, seen_by_alice) =
        verify_with_sas(&mut on_bob, &mut on_alice, Some(&alice_device)).await;
    assert_eq!(
        seen_by_bob.as_array().map(Vec::len),
        Some(7),
        "{seen_by_bob}"
    );
    assert_eq!(seen_by_bob, seen_by_alice, "both sides show the same emoji");
    let trust = on_bob.devices_until_verified(&alice, &alice_device).await;
    assert_eq!(trust["identity_verified"], true, "{trust}");

    on_alice.stop().await;
    on_bob.stop().await;
}

/// 沒有備份時向自己的裝置要金鑰（/docs/design/keys/cross-signing-and-verification.md §11.1 的 V11）：兩台都 `SERVER_BACKUP=off`，
/// C1 送的那則，C2 跟 C1 自我驗證之後再讀，金鑰是 C1 轉過來的（server 上沒有任何備份版本可還原）。
#[tokio::test]
#[ignore = "needs a running wbfuwunel: WBF_E2E_SERVER, WBF_E2E_REGISTRATION_TOKEN"]
async fn a_new_device_gets_old_room_keys_from_a_verified_device_without_a_backup() {
    let server = std::env::var("WBF_E2E_SERVER").expect("WBF_E2E_SERVER");
    let (carol, password) = register_fresh_user(&server, "vc").await;
    let backup_off = || Settings {
        server_backup: false,
        ..Settings::default()
    };
    let mut c1 = Device::login(&server, &carol, &password, backup_off()).await;
    let created = c1
        .call(
            "room.create",
            json!({ "encrypted": true, "name": "key forwarding e2e", "preset": "private_chat" }),
        )
        .await;
    assert_eq!(created["code"], 0, "{created}");
    let room = created["result"]["room_id"].as_str().unwrap().to_string();
    let fetched = c1
        .call("room.get", json!({ "room": room, "sync": "both" }))
        .await;
    assert_eq!(fetched["code"], 0, "{fetched}");
    let refreshed = c1
        .call("room.refresh_devices", json!({ "room": room }))
        .await;
    assert_eq!(refreshed["code"], 0, "{refreshed}");
    let sent = c1
        .call(
            "room.send_text",
            json!({ "room": room, "body": "forwarded, not backed up", "room_devices": refreshed["result"] }),
        )
        .await;
    assert_eq!(sent["code"], 0, "{sent}");
    let event_id = sent["result"]["event_id"].as_str().unwrap().to_string();

    let mut c2 = Device::login(&server, &carol, &password, backup_off()).await;
    let fetched = c2
        .call("room.get", json!({ "room": room, "sync": "both" }))
        .await;
    assert_eq!(fetched["code"], 0, "{fetched}");
    history_until(&mut c2.client, &carol, &room, &event_id, |message| {
        message["kind"] == "undecryptable"
    })
    .await;
    let (on_c2, on_c1) = verify_with_sas(&mut c2, &mut c1, None).await;
    assert_eq!(on_c2, on_c1, "both devices show the same emoji");
    // 每讀一次，解不開的那則就向自己的裝置要一次金鑰（`room.history` 補解時送出去）。
    let opened = history_until(&mut c2.client, &carol, &room, &event_id, |message| {
        message["kind"] == "text"
    })
    .await;
    assert_eq!(opened["body"], "forwarded, not backed up", "{opened}");

    let other = OtherClient::login(&server, &carol, &password).await;
    let (status, backup) = other.get_json("/_matrix/client/v3/room_keys/version").await;
    assert_eq!(status, 404, "there is no backup to restore from: {backup}");
    other.logout().await;
    c1.stop().await;
    c2.stop().await;
}

/// `room.history sync=both` 一直問到 `event_id` 那則符合 `wanted` 為止（推播、金鑰都要時間；最多 10 秒），回那則。
async fn history_until(
    client: &mut Client,
    user: &str,
    room: &str,
    event_id: &str,
    wanted: impl Fn(&Value) -> bool,
) -> Value {
    let mut last = Value::Null;
    for _ in 0..50 {
        let page = client
            .call(
                "room.history",
                json!({ "user": user, "room": room, "limit": 20, "sync": "both" }),
            )
            .await;
        assert_eq!(page["code"], 0, "room.history: {page}");
        if let Some(found) = page["result"]["events"]
            .as_array()
            .and_then(|events| events.iter().find(|message| message["id"] == event_id))
        {
            last = found.clone();
            if wanted(found) {
                return last;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    panic!("{user} never saw {event_id} as wanted; last: {last}");
}

/// 上游的加密器（Element 這類 client 用的同一份）：回（密文、事件裡的 `file`，`url` 由呼叫者填）。
fn encrypt_like_element(plain: &[u8]) -> (Vec<u8>, Value) {
    use std::io::Read;
    let mut source = plain;
    let mut encryptor = matrix_sdk_crypto::AttachmentEncryptor::new(&mut source);
    let mut cipher = Vec::new();
    encryptor.read_to_end(&mut cipher).unwrap();
    (cipher, serde_json::to_value(encryptor.finish()).unwrap())
}

/// 事件經訂閱線進了這個帳號的快取，`media.open` 才找得到金鑰：輪詢到成功為止。
async fn open_once_seen(client: &mut Client, by_event: &Value) -> Value {
    let mut opened = Value::Null;
    for _ in 0..50 {
        opened = client.call("media.open", by_event.clone()).await;
        if opened["code"] == 0 {
            return opened;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    panic!("media.open never found the event: {opened}");
}

/// 事件進了快取之前是 1100（看不到這則）：輪詢到不是 1100 為止，回那個回應。
async fn call_once_seen(client: &mut Client, method: &str, params: &Value) -> Value {
    let mut reply = Value::Null;
    for _ in 0..50 {
        reply = client.call(method, params.clone()).await;
        if reply["code"] != 1100 {
            return reply;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    panic!("{method} never saw the event: {reply}");
}

/// 等到一則符合的推播（最多 10 秒）：推播只在等回應時收，所以拿 `daemon.info` 當輪詢。拿到的那則從佇列拿掉。
async fn wait_for_push(client: &mut Client, wanted: impl Fn(&Value) -> bool) -> Value {
    wait_for_push_within(client, 100, wanted).await
}

/// 同 [`wait_for_push`]，最多等 `tries` × 100 毫秒。
async fn wait_for_push_within(
    client: &mut Client,
    tries: u32,
    wanted: impl Fn(&Value) -> bool,
) -> Value {
    for _ in 0..tries {
        if let Some(position) = client.pushes.iter().position(&wanted) {
            return client.pushes.remove(position);
        }
        client.call("daemon.info", json!({})).await;
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    panic!("the push never came; seen: {:?}", client.pushes);
}

/// 註冊一個全新的帳號：別的測試也登入 alice、bob，她們早就有交叉簽章與備份了，「第一台裝置建」只有全新的帳號測得到。
///
/// Return:
///     (String, String)   (mxid, 密碼)
async fn register_fresh_user(server: &str, prefix: &str) -> (String, String) {
    let token = std::env::var("WBF_E2E_REGISTRATION_TOKEN").expect("WBF_E2E_REGISTRATION_TOKEN");
    let localpart = format!(
        "{prefix}{}",
        &uuid::Uuid::new_v4().simple().to_string()[..10]
    );
    let password = uuid::Uuid::new_v4().to_string();
    let http = reqwest::Client::new();
    let mut auth = json!({ "type": "m.login.registration_token", "token": token });
    for _ in 0..2 {
        let response = http
            .post(format!("{server}/_matrix/client/v3/register"))
            .json(&json!({ "username": localpart, "password": password, "auth": auth }))
            .send()
            .await
            .unwrap();
        let status = response.status().as_u16();
        let reply: Value = response.json().await.unwrap();
        if status == 200 {
            return (reply["user_id"].as_str().unwrap().to_string(), password);
        }
        assert_eq!(status, 401, "register {localpart}: {reply}");
        auth["session"] = reply["session"].clone();
    }
    panic!("register {localpart} never finished");
}

/// 一台裝置：自己的資料目錄、自己的 daemon、登入同一個帳號（/docs/design/keys/cross-signing-and-verification.md §11.1）。
struct Device {
    client: Client,
    daemon: Daemon,
    _dir: tempfile::TempDir,
    user: String,
    device_id: String,
}

impl Device {
    /// 起一個 daemon、登入、等五條線開好（金鑰線開好時交叉簽章與備份已經就位）。
    async fn login(server: &str, user: &str, password: &str, settings: Settings) -> Device {
        let dir = tempfile::Builder::new()
            .prefix("wd")
            .tempdir_in(std::env::temp_dir())
            .unwrap();
        let daemon = start_daemon_with(dir.path(), settings).await;
        let mut client = Client::connect(daemon.port).await;
        let subscribed = client
            .call(
                "subscribe",
                // `note` 與 `keys.state`：背景的金鑰線出事時只講一聲，訂起來失敗時才看得到原因。
                json!({ "events": ["verify.state", "room.message", "note", "keys.state"] }),
            )
            .await;
        assert_eq!(subscribed["code"], 0, "subscribe: {subscribed}");
        let reply = client.call("vault.create", json!({})).await;
        assert_eq!(reply["code"], 0, "vault.create: {reply}");
        let reply = client
            .call(
                "account.add",
                json!({ "server": server, "user": user, "password": password, "device_name": "wbf-daemon verification e2e" }),
            )
            .await;
        assert_eq!(reply["code"], 0, "account.add {user}: {reply}");
        let device_id = reply["result"]["device_id"].as_str().unwrap().to_string();
        wait_for_links(&mut client, 5).await;
        Device {
            client,
            daemon,
            _dir: dir,
            user: user.to_string(),
            device_id,
        }
    }

    async fn call(&mut self, method: &str, mut params: Value) -> Value {
        params["user"] = json!(self.user);
        self.client.call(method, params).await
    }

    /// 這個流程推到 `state` 的那一則。
    async fn wait_for_verify(&mut self, flow_id: &str, state: &str) -> Value {
        let user = self.user.clone();
        wait_for_push_within(&mut self.client, 200, |push| {
            push["method"] == "verify.state"
                && push["params"]["user"] == user.as_str()
                && push["params"]["flow_id"] == flow_id
                && push["params"]["state"] == state
        })
        .await["params"]
            .clone()
    }

    /// 有人跟這台開了驗證（還沒看過的那一個）：回它的 flow id。
    async fn wait_for_incoming_request(&mut self, from: &str) -> String {
        let user = self.user.clone();
        let push = wait_for_push_within(&mut self.client, 200, |push| {
            push["method"] == "verify.state"
                && push["params"]["user"] == user.as_str()
                && push["params"]["other_user"] == from
                && push["params"]["state"] == "requested"
                && push["params"]["we_started"] == false
        })
        .await;
        push["params"]["flow_id"].as_str().unwrap().to_string()
    }

    /// 輪詢 `devices.list` 到 `device_id` 被信任（最多 20 秒）。
    async fn devices_until_verified(&mut self, user_id: &str, device_id: &str) -> Value {
        let mut last = Value::Null;
        for _ in 0..100 {
            last = self
                .call("devices.list", json!({ "user_id": user_id }))
                .await;
            assert_eq!(last["code"], 0, "devices.list: {last}");
            let verified = last["result"]["devices"]
                .as_array()
                .unwrap()
                .iter()
                .any(|device| device["device_id"] == device_id && device["verified"] == true);
            if verified {
                return last["result"].clone();
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
        panic!(
            "{device_id} of {user_id} never became verified for {}: {last}",
            self.user
        );
    }

    async fn stop(self) {
        stop_daemon(self.daemon, self.client).await;
    }
}

/// `initiator` 對 `responder` 開驗證、走完 SAS（雙方接受、比表情、確認），回兩邊看到的表情（應該一樣）。
///
/// Args:
///     target_device: `initiator` 要驗哪一台；自我驗證給 None（送給自己所有裝置）
async fn verify_with_sas(
    initiator: &mut Device,
    responder: &mut Device,
    target_device: Option<&str>,
) -> (Value, Value) {
    let mut params = json!({ "user_id": responder.user });
    if let Some(device) = target_device {
        params["device_id"] = json!(device);
    }
    let requested = initiator.call("verify.request", params).await;
    assert_eq!(requested["code"], 0, "verify.request: {requested}");
    assert_eq!(requested["result"]["state"], "requested", "{requested}");
    let flow_id = requested["result"]["flow_id"].as_str().unwrap().to_string();
    let seen = responder.wait_for_incoming_request(&initiator.user).await;
    assert_eq!(seen, flow_id, "the responder sees the same flow");
    let flow = |user: &str| json!({ "user_id": user, "flow_id": flow_id });
    let accepted = responder.call("verify.accept", flow(&initiator.user)).await;
    assert_eq!(accepted["code"], 0, "verify.accept (request): {accepted}");
    initiator.wait_for_verify(&flow_id, "ready").await;
    let started = initiator
        .call("verify.start_sas", flow(&responder.user))
        .await;
    assert_eq!(started["code"], 0, "verify.start_sas: {started}");
    responder.wait_for_verify(&flow_id, "started").await;
    let accepted = responder.call("verify.accept", flow(&initiator.user)).await;
    assert_eq!(accepted["code"], 0, "verify.accept (sas): {accepted}");
    let on_initiator = initiator.wait_for_verify(&flow_id, "keys_exchanged").await;
    let on_responder = responder.wait_for_verify(&flow_id, "keys_exchanged").await;
    let responder_user = responder.user.clone();
    let confirmed = initiator
        .call("verify.confirm", flow(&responder_user))
        .await;
    assert_eq!(confirmed["code"], 0, "verify.confirm: {confirmed}");
    let confirmed = responder
        .call("verify.confirm", flow(&initiator.user))
        .await;
    assert_eq!(confirmed["code"], 0, "verify.confirm: {confirmed}");
    initiator.wait_for_verify(&flow_id, "done").await;
    responder.wait_for_verify(&flow_id, "done").await;
    (on_initiator["emoji"].clone(), on_responder["emoji"].clone())
}

async fn download_until_complete(client: &mut Client, by_event: &Value) -> Value {
    let mut state = Value::Null;
    for _ in 0..150 {
        state = client.call("media.download", by_event.clone()).await;
        assert_eq!(state["code"], 0, "media.download: {state}");
        if state["result"]["state"] == "complete" {
            return state;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    panic!("the download never completed: {state}");
}

/// 「別人的 client」：原生的 Matrix HTTP，**不**經過我們的程式碼。
struct OtherClient {
    http: reqwest::Client,
    server: String,
    access_token: String,
}

impl OtherClient {
    async fn login(server: &str, user: &str, password: &str) -> OtherClient {
        let http = reqwest::Client::new();
        let reply: Value = http
            .post(format!("{server}/_matrix/client/v3/login"))
            .json(&json!({ "type": "m.login.password", "identifier": { "type": "m.id.user", "user": user },
                           "password": password, "initial_device_display_name": "e2e other client" }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let access_token = reply["access_token"]
            .as_str()
            .unwrap_or_else(|| panic!("login: {reply}"))
            .to_string();
        OtherClient {
            http,
            server: server.to_string(),
            access_token,
        }
    }

    /// Return:
    ///     String   `content_uri`, example: "mxc://localhost/AbCdEf"
    async fn upload(&self, bytes: Vec<u8>) -> String {
        let reply: Value = self
            .http
            .post(format!("{}/_matrix/media/v3/upload", self.server))
            .bearer_auth(&self.access_token)
            .header("content-type", "application/octet-stream")
            .body(bytes)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        reply["content_uri"]
            .as_str()
            .unwrap_or_else(|| panic!("upload: {reply}"))
            .to_string()
    }

    /// Return:
    ///     String   event_id
    async fn send(&self, room: &str, content: Value) -> String {
        let room = room.replace('!', "%21").replace(':', "%3A");
        let txn = uuid::Uuid::new_v4();
        let reply: Value = self
            .http
            .put(format!(
                "{}/_matrix/client/v3/rooms/{room}/send/m.room.message/{txn}",
                self.server
            ))
            .bearer_auth(&self.access_token)
            .json(&content)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        reply["event_id"]
            .as_str()
            .unwrap_or_else(|| panic!("send: {reply}"))
            .to_string()
    }

    /// server 上那則事件原樣（`GET /rooms/{room}/event/{event_id}`）：看它送出去的是不是密文。
    async fn event(&self, room: &str, event_id: &str) -> Value {
        let encode = |text: &str| {
            text.replace('!', "%21")
                .replace('$', "%24")
                .replace(':', "%3A")
                .replace('/', "%2F")
                .replace('+', "%2B")
        };
        self.http
            .get(format!(
                "{}/_matrix/client/v3/rooms/{}/event/{}",
                self.server,
                encode(room),
                encode(event_id)
            ))
            .bearer_auth(&self.access_token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap()
    }

    /// 這間房的 `m.receipt`（/sync 的 ephemeral）：輪詢到 `user` 對 `event_id` 的回條出現為止（最多 10 秒），回 content。
    async fn receipts_of(&self, room: &str, user: &str, event_id: &str) -> Value {
        let mut content = Value::Null;
        for _ in 0..50 {
            let reply: Value = self
                .http
                .get(format!("{}/_matrix/client/v3/sync?timeout=0", self.server))
                .bearer_auth(&self.access_token)
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            if let Some(receipt) = reply["rooms"]["join"][room]["ephemeral"]["events"]
                .as_array()
                .and_then(|events| events.iter().find(|event| event["type"] == "m.receipt"))
            {
                content = receipt["content"].clone();
                if content[event_id]["m.read"][user].is_object() {
                    return content;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
        content
    }

    /// Return:
    ///     (u16, Value)   (狀態碼, body)
    async fn get_json(&self, path: &str) -> (u16, Value) {
        let response = self
            .http
            .get(format!("{}{path}", self.server))
            .bearer_auth(&self.access_token)
            .send()
            .await
            .unwrap();
        let status = response.status().as_u16();
        (status, response.json().await.unwrap_or(Value::Null))
    }

    /// 這個人在 server 上的交叉簽章公鑰與裝置（`POST /keys/query`）。
    async fn query_keys(&self, user: &str) -> Value {
        self.http
            .post(format!("{}/_matrix/client/v3/keys/query", self.server))
            .bearer_auth(&self.access_token)
            .json(&json!({ "device_keys": { user: [] } }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap()
    }

    async fn logout(&self) {
        let _ = self
            .http
            .post(format!("{}/_matrix/client/v3/logout", self.server))
            .bearer_auth(&self.access_token)
            .json(&json!({}))
            .send()
            .await;
    }
}

/// 裸 TCP 送一個 GET（`Connection: close`，讀到 EOF）。
///
/// Return:
///     (u16, String, Vec<u8>)   (狀態碼, 標頭原文, body)
async fn get(port: u16, url: &str, range: Option<&str>) -> (u16, String, Vec<u8>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let path = url
        .strip_prefix(&format!("http://127.0.0.1:{port}"))
        .unwrap_or_else(|| panic!("{url} is not on the data port {port}"));
    let range = range
        .map(|range| format!("Range: {range}\r\n"))
        .unwrap_or_default();
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    let head =
        format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n{range}Connection: close\r\n\r\n");
    stream.write_all(head.as_bytes()).await.unwrap();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).await.unwrap();
    let split = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .unwrap();
    let head = String::from_utf8(response[..split].to_vec()).unwrap();
    let status = head.split(' ').nth(1).unwrap().parse().unwrap();
    (status, head, response[split + 4..].to_vec())
}

/// 裸 TCP 送一個 PUT（`Connection: close`，讀到 EOF）：URL 與 header 照 `media.create` 回的。
///
/// Return:
///     (u16, Value)   (狀態碼, JSON body)
async fn put(port: u16, created: &Value, body: &[u8]) -> (u16, Value) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let url = created["url"].as_str().unwrap();
    let headers: String = created["headers"]
        .as_object()
        .unwrap()
        .iter()
        .map(|(name, value)| format!("{name}: {}\r\n", value.as_str().unwrap()))
        .collect();
    let path = url
        .strip_prefix(&format!("http://127.0.0.1:{port}"))
        .unwrap_or_else(|| panic!("{url} is not on the data port {port}"));
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    let head = format!(
        "PUT {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).await.unwrap();
    stream.write_all(body).await.unwrap();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).await.unwrap();
    let split = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .unwrap();
    let head = String::from_utf8_lossy(&response[..split]).to_string();
    let status = head.split(' ').nth(1).unwrap().parse().unwrap();
    let json = serde_json::from_slice(&response[split + 4..]).unwrap_or(Value::Null);
    (status, json)
}

//! 對真的 wbfuwunel 走一遍網路型 method（/docs/design/rpc-specs/rpc-spec.md §10 判準：走我們自己的 WS 才算做完）。
//!
//! `--ignored`；環境變數：
//!   WBF_E2E_SERVER        example: http://localhost:6167
//!   WBF_E2E_USER          example: @alice:localhost
//!   WBF_E2E_PASSWORD_FILE 整檔就是密碼（去掉結尾一個換行）
//!
//! 流程：vault.create（**passphrase 模式**）→ account.add（鉤子在背景開五條線）→ account.whoami → server.ping（WS）
//! → room.list（local 與 both，後者走橋的 JoinedRooms）→ sync.recent（WS）→ backup.status（wbf 帳號拒）→ **daemon 重開 → vault.unlock → whoami**
//! → account.del。每一步看 code，🚫 不看 msg。
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
/// 🚫 不是「再起一個」：兩個 daemon 同時開同一個資料目錄正是 /docs/design/overview/architecture-v2.md §0.2 禁止的事
/// （PR #31 審查 cirno🔴）。
struct Daemon {
    port: u16,
    /// 資料平面（/docs/design/rpc-specs/data-plane.md）的 port。
    data_port: u16,
    task: tokio::task::JoinHandle<()>,
}

async fn start_daemon(data_dir: &std::path::Path) -> Daemon {
    let policy = EncryptionPolicy::enforced();
    let handle = Handle::new(data_dir, policy.clone(), Settings::default());
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

    // fresh 資料目錄的起手式（/docs/design/rpc-specs/rpc-spec.md §3.1）：🚫 account.add 不替前端建 vault，
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

    // 登入成功就觸發鉤子：五條線（misc、upload、download、rooms、keys）在背景開起來（/docs/design/daemon/link-pool.md §3.1）。
    wait_for_links(&mut client, 5).await;

    // WS：Hello／Ping。走已經開著的 misc，🚫 不再開一條。
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

    // 備份還掛在 Client 上（/docs/design/daemon/account-session.md §6）：wbf 帳號明講拒絕（1100），🚫 不靜默失效。
    let reply = client.call("backup.status", json!({})).await;
    assert_eq!(
        reply["code"], 1100,
        "backup.status on a wbf account: {reply}"
    );
    let before = info["result"]["instance"]
        .as_str()
        .expect("instance")
        .to_string();

    // daemon 重開（真正的「鎖上」就是這條，/docs/design/rpc-specs/rpc-spec.md §3.1）。
    // ⚠️ **先停掉第一個**：`daemon.shutdown` → 關連線 → 等它收攤 → 確認舊 port 不收連線了。
    // 🚫 不可以直接再起一個：兩個 daemon 同時開同一個資料目錄是 /docs/design/overview/architecture-v2.md §0.2 禁止的，而且那樣
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

    // 收攤：第二個也照同一條路停掉，🚫 不留一個還開著資料目錄的 daemon 給 tempdir 收
    // （Windows 上那會讓刪除失敗，而且它掩蓋的正是「有東西還握著 store」這件事）。
    stop_daemon(daemon, client).await;
}

/// 🚨 **房間歷史往回翻，定位一律是 `event_id`**（wbfuwunel #51；維護者 2026-09-14）。
///
/// 額外要 `WBF_E2E_ROOM`：一個 `WBF_E2E_USER` 在裡面的房間（這條測試會往裡面送 7 則）；選填 `WBF_E2E_ENCRYPTED_ROOM`（一間加密房，驗沒帶 `room_devices` 的送出被拒、🚫 不送明文——1100）。
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
/// 📎 每一段都拿**自己剛送的 7 則**比對，🚫 不假設房間原本是空的。
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

    // 加密房沒帶 `room_devices` 要被拒（1100）、🚫 送明文：加不加密看的是剛拿到的房間（/docs/design/keys/e2ee-rpc.md §7）。
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

    // ── 訂閱線（#61 起 account.add 之後 daemon 自己開）把剛送的 7 則推回來、寫進本地：等它們都到了再往下，🚫 不賭時序 ──
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
/// | 同一個 URL 傳完再 PUT 一次 | 🚫 不是 200：server 已經收掉這個上傳 |
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
/// 送的那一方用原生 HTTP（同一個 alice 另登一台裝置），🚫 經過我們的程式碼：它就是「別人的 client」。
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
    // 同一個 mxc、另一把金鑰的事件（寫錯或偽造）：本地那一列說了算，回 1500、🚫 下載（/docs/design/media/media-download.md §5.3）。
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
        "驗不過也是完成、🚫 刪檔：{state}"
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
/// 額外要 `WBF_E2E_USER_B`／`WBF_E2E_PASSWORD_B_FILE`（另一個帳號，例 `@bob:localhost`）。每一步看 code 與 server 的 body，🚫 看 msg。
///
/// | 步驟 | 驗什麼 |
/// |---|---|
/// | alice `room.create`（明文） | 回 server 的 `{room_id}`；本地 alice 那列 `join` |
/// | alice `room.invite` bob | bob 是本機帳號 → bob 那列 `invite`（`room.list membership: ["invite"]`） |
/// | bob `room.join` | bob 那列 `join` |
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

    let joined = client
        .call("room.join", json!({ "user": bob, "room": room }))
        .await;
    assert_eq!(joined["code"], 0, "{joined}");
    assert_eq!(joined["result"]["room_id"], room.as_str());
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
        "成員事件🚫 放進 state"
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
    assert_eq!(again["result"], json!({}), "已經置頂：🚫 寫：{again}");
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
    assert_eq!(plain["code"], 1100, "本地知道加密了：🚫 送明文：{plain}");
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

/// 「別人的 client」：原生的 Matrix HTTP，🚫 經過我們的程式碼。
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

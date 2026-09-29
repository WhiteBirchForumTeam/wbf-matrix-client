//! 對真的 wbfuwunel 走一遍網路型 method（rpc-spec §10 判準：走我們自己的 WS 才算做完）。
//!
//! `--ignored`；環境變數：
//!   WBF_E2E_SERVER        example: http://localhost:6167
//!   WBF_E2E_USER          example: @alice:localhost
//!   WBF_E2E_PASSWORD_FILE 整檔就是密碼（去掉結尾一個換行）
//!
//! 流程：vault.create（**passphrase 模式**）→ account.add（鉤子在背景開五條線）→ account.whoami → server.ping（WS）
//! → room.list（local 與 both，後者走橋）→ sync.recent（WS）→ backup.status（wbf 帳號拒）→ **daemon 重開 → vault.unlock → whoami**
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

/// 一個跑著的 daemon。⚠️ 拿著 `task` 才停得掉它 —— 「重開」必須是**真的停掉再起**，
/// 🚫 不是「再起一個」：兩個 daemon 同時開同一個資料目錄正是 architecture-v2 §0.2 禁止的事
/// （PR #31 審查 cirno🔴）。
struct Daemon {
    port: u16,
    task: tokio::task::JoinHandle<()>,
}

async fn start_daemon(data_dir: &std::path::Path) -> Daemon {
    let policy = EncryptionPolicy::enforced();
    let handle = Handle::new(data_dir, policy.clone(), Settings::default());
    let rpc = RpcServer::bind(0, Arc::new(RpcKeys::from_token(&TOKEN)), policy, handle)
        .await
        .unwrap();
    let port = rpc.local_addr().unwrap().port();
    let task = tokio::spawn(rpc.run());
    Daemon { port, task }
}

/// 停掉一個 daemon：走**真的那條路**（`daemon.shutdown`），關掉連線，等 server 收攤，
/// 然後確認舊 port 真的不收連線了。
///
/// ⭐ 這裡刻意不用「丟掉 task」那種便宜作法：那樣就算 shutdown 整條壞掉測試也會綠。
/// 鉤子在背景開線（`vault.unlock`／`account.add` 之後，link-pool.md §3.1）：等 `daemon.info` 的線數到 `want`。
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
    let daemon = start_daemon(dir.path()).await;
    let mut client = Client::connect(daemon.port).await;

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

    // 登入成功就觸發鉤子：五條線（misc、upload、download、rooms、keys）在背景開起來（link-pool.md §3.1）。
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
    // wbf 帳號沒有 matrix-sdk 的 Client（account-session.md §2）：`sync=both` 走橋（JoinedRooms ＋ 每房 GetState ＋ m.direct）。
    let reply = client.call("room.list", json!({ "sync": "both" })).await;
    assert_eq!(reply["code"], 0, "room.list sync=both: {reply}");
    assert!(reply["result"].is_array());

    // WS：Event/Recent。
    let reply = client
        .call("sync.recent", json!({ "max_events": 100 }))
        .await;
    assert_eq!(reply["code"], 0, "recent: {reply}");
    assert!(reply["result"]["caught_up"].is_boolean(), "{reply}");

    // 備份還掛在 Client 上（account-session.md §6）：wbf 帳號明講拒絕（1100），🚫 不靜默失效。
    let reply = client.call("backup.status", json!({})).await;
    assert_eq!(
        reply["code"], 1100,
        "backup.status on a wbf account: {reply}"
    );
    let before = info["result"]["instance"]
        .as_str()
        .expect("instance")
        .to_string();

    // daemon 重開（真正的「鎖上」就是這條，rpc-spec §3.1）。
    // ⚠️ **先停掉第一個**：`daemon.shutdown` → 關連線 → 等它收攤 → 確認舊 port 不收連線了。
    // 🚫 不可以直接再起一個：兩個 daemon 同時開同一個資料目錄是 §0.2 禁止的，而且那樣
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
    // 登出：token 撤了，這個帳號的線全關（link-pool.md §3）。
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
    // 訂閱線在背景開（link-pool.md §3.1）：等五條都開好再送，下面送的 7 則才一定會被推回來（訂閱不補訂閱之前的，那是 UI 叫 `sync.recent` 的事）。
    wait_for_links(&mut client, 5).await;

    // wbf 帳號的送訊息走 `Event/Send` 明文：加密房要被拒（1100），而且是送之前問這一刻的 `m.room.encryption`（account-session.md §6）。
    // 選填 `WBF_E2E_ENCRYPTED_ROOM`：一間 `WBF_E2E_USER` 在裡面的加密房。
    if let Ok(encrypted_room) = std::env::var("WBF_E2E_ENCRYPTED_ROOM") {
        let reply = client
            .call(
                "room.send_text",
                json!({ "room": encrypted_room, "body": "must not go out in plaintext" }),
            )
            .await;
        assert_eq!(reply["code"], 1100, "加密房的明文送出要被拒：{reply}");
        // e2ee-rpc.md 的 RPC 形狀：`room.refresh_devices` 回的整份原樣當 `room_devices` 帶回來，送出去的是密文、帶那個號碼。
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

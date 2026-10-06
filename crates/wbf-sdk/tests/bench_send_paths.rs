//! 同一台 wbfuwunel、同一種加密房，比兩條送訊息的路：wbf（WS ＋ 橋，送出只用分好的金鑰）與 matrix-sdk 的 `Client`（HTTP）。
//! 量的是每則的延遲與每則打了幾個請求（wbf 數 pack、HTTP 數請求行），🚫 斷言快慢——只印報表。平常 `#[ignore]`；要跑：
//!
//! ```text
//! WBF_E2E_SERVER=http://127.0.0.1:6167 WBF_E2E_USER=alice WBF_E2E_PASSWORD_FILE=<檔> \
//! WBF_E2E_USER_B=bob WBF_E2E_PASSWORD_B_FILE=<檔> \
//!     cargo test -p wbf-sdk --features matrix --test bench_send_paths -- --ignored --nocapture --test-threads=1
//! ```
//!
//! 房裡三台裝置：alice 的 wbf 裝置 W、alice 的 matrix-sdk 裝置 M、bob 的 wbf 裝置 B。兩條路各用一個新房，都送 1＋`STEADY` 則。
//! - wbf：開房時 refresh＋分金鑰（後台會做的事，另計）→ 每則只有 `Event/Send`；快到期時照後台的規則提早換（另計，不算在送出時間裡）。
//! - matrix-sdk：每則 `Room::send`，送出前上游同步跑 `preshare_room_key`（成員、查金鑰、claim、to-device 全在送的路上）。
//!
//! HTTP 那條走一個本機的計數代理（只數 client → server 的請求），所以數字是 matrix-sdk 實際打出去的。
//! 代理會讓 HTTP 多一跳：比延遲要另跑一次 `WBF_BENCH_NO_PROXY=1`（直連、不數請求）。
#![cfg(feature = "matrix")]

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use wbf_sdk::chat::ChatBackend;
use wbf_sdk::crypto_engine::{OlmEngine, OutgoingRoomEvent, RoomKeyState, SendOutcome};
use wbf_sdk::link::Subscription;
use wbf_sdk::login::{login_with_password, logout, Session};
use wbf_sdk::protocol::{
    BRIDGE_ACCOUNT_DATA, BRIDGE_JOINED_ROOMS, BRIDGE_KEYS_CLAIM, BRIDGE_KEYS_QUERY,
    BRIDGE_KEYS_UPLOAD, BRIDGE_MEMBERS, BRIDGE_ROOM_STATE, BRIDGE_SEND_TO_DEVICE,
    BRIDGE_SIGNATURES_UPLOAD, BRIDGE_SIGNING_KEYS_UPLOAD, BRIDGE_STATE_EVENT,
};
use wbf_sdk::vault::Key32;
use wbf_sdk::{Channel, PackChannel, SdkError, Transport, WbfClient};
use wbf_wire::Pack;

const STEADY: usize = 100;

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

fn read_password(var: &str) -> Option<String> {
    Some(
        std::fs::read_to_string(env(var)?)
            .ok()?
            .trim_end_matches(['\r', '\n'])
            .to_string(),
    )
}

fn random_key() -> Key32 {
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes).unwrap();
    Key32(bytes)
}

// ---- 數請求 ----

/// 一本帳：每個請求記一個名字，量一段就拿前後的長度相減。
type Ledger = Arc<Mutex<Vec<String>>>;

fn ledger_len(ledger: &Ledger) -> usize {
    ledger.lock().unwrap().len()
}

fn ledger_slice(ledger: &Ledger, from: usize, to: usize) -> BTreeMap<String, usize> {
    let mut counts = BTreeMap::new();
    for name in &ledger.lock().unwrap()[from..to] {
        *counts.entry(name.clone()).or_insert(0) += 1;
    }
    counts
}

fn pack_name(pack: &Pack) -> String {
    let bridged = [
        (BRIDGE_MEMBERS, "bridge members"),
        (BRIDGE_JOINED_ROOMS, "bridge joined_rooms"),
        (BRIDGE_ROOM_STATE, "bridge room state"),
        (BRIDGE_STATE_EVENT, "bridge state event"),
        (BRIDGE_ACCOUNT_DATA, "bridge account_data"),
        (BRIDGE_SEND_TO_DEVICE, "bridge sendToDevice"),
        (BRIDGE_KEYS_UPLOAD, "bridge keys/upload"),
        (BRIDGE_KEYS_QUERY, "bridge keys/query"),
        (BRIDGE_KEYS_CLAIM, "bridge keys/claim"),
        (
            BRIDGE_SIGNING_KEYS_UPLOAD,
            "bridge keys/device_signing/upload",
        ),
        (BRIDGE_SIGNATURES_UPLOAD, "bridge keys/signatures/upload"),
    ];
    for (endpoint, name) in bridged {
        if endpoint.kind == pack.kind && endpoint.subtype == pack.subtype {
            return name.to_string();
        }
    }
    let kind = format!("{:?}", pack.kind);
    match (kind.as_str(), pack.subtype) {
        ("Event", 0x02) => "Event/Send".to_string(),
        _ => format!("{kind}/0x{:02x}", pack.subtype),
    }
}

/// 包一層 `PackChannel`：每送出一個請求 pack 記一筆，其餘原樣轉給底下那條。
struct CountingChannel {
    inner: Channel,
    ledger: Ledger,
}

impl CountingChannel {
    fn record(&self, pack: &Pack) {
        self.ledger.lock().unwrap().push(pack_name(pack));
    }
}

impl PackChannel for CountingChannel {
    async fn request(&mut self, pack: Pack) -> Result<Pack, SdkError> {
        self.record(&pack);
        self.inner.request(pack).await
    }

    async fn request_stream(
        &mut self,
        pack: Pack,
        per_pack_timeout: Duration,
        on_pack: &mut (dyn FnMut(Pack) -> Result<bool, SdkError> + Send),
    ) -> Result<(), SdkError> {
        self.record(&pack);
        self.inner
            .request_stream(pack, per_pack_timeout, on_pack)
            .await
    }

    async fn subscribe(&mut self, pack: Pack) -> Result<Subscription, SdkError> {
        self.record(&pack);
        self.inner.subscribe(pack).await
    }

    async fn send_only(&mut self, pack: Pack) -> Result<(), SdkError> {
        self.record(&pack);
        self.inner.send_only(pack).await
    }
}

/// HTTP 的路名：`/_matrix/client/v3/rooms/!x/send/m.room.encrypted/t1` → `rooms/send`、`/_matrix/client/v3/keys/claim` → `keys/claim`。
fn http_endpoint(method: &str, path: &str) -> String {
    let path = path.split('?').next().unwrap_or(path);
    let segments: Vec<&str> = path.split('/').filter(|part| !part.is_empty()).collect();
    let rest: Vec<&str> = segments
        .iter()
        .skip_while(|part| !matches!(**part, "v3" | "r0" | "v1" | "unstable"))
        .skip(1)
        .copied()
        .collect();
    let name = match rest.as_slice() {
        ["rooms", _, action, ..] => format!("rooms/{action}"),
        ["keys", action, ..] => format!("keys/{action}"),
        [first, ..] => first.to_string(),
        [] => path.to_string(),
    };
    format!("{method} {name}")
}

/// 本機計數代理：每條連線照抄兩個方向；client → server 那邊每讀完一個請求的 header 就記一筆、照 Content-Length 跳過 body。
async fn start_counting_proxy(upstream: String, ledger: Ledger) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((client, _)) = listener.accept().await else {
                return;
            };
            let upstream = upstream.clone();
            let ledger = ledger.clone();
            tokio::spawn(async move {
                let Ok(server) = TcpStream::connect(&upstream).await else {
                    return;
                };
                let (mut client_read, mut client_write) = client.into_split();
                let (mut server_read, mut server_write) = server.into_split();
                let upward = async move {
                    let mut buffer = vec![0u8; 64 * 1024];
                    let mut pending: Vec<u8> = Vec::new();
                    // 還有幾個 byte 是上一個請求的 body（照 Content-Length 跳過；reqwest 送 JSON 不用 chunked）。
                    let mut body_left = 0usize;
                    loop {
                        let read = match client_read.read(&mut buffer).await {
                            Ok(0) | Err(_) => break,
                            Ok(read) => read,
                        };
                        pending.extend_from_slice(&buffer[..read]);
                        loop {
                            if body_left > 0 {
                                let skipped = body_left.min(pending.len());
                                pending.drain(..skipped);
                                body_left -= skipped;
                                if body_left > 0 {
                                    break;
                                }
                            }
                            let Some(end) = find(&pending, b"\r\n\r\n") else {
                                break;
                            };
                            let head = String::from_utf8_lossy(&pending[..end]).to_string();
                            let mut lines = head.split("\r\n");
                            let mut request_line = lines.next().unwrap_or("?").split(' ');
                            let method = request_line.next().unwrap_or("?");
                            let path = request_line.next().unwrap_or("?");
                            ledger.lock().unwrap().push(http_endpoint(method, path));
                            body_left = lines
                                .filter_map(|line| line.split_once(':'))
                                .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                                .and_then(|(_, value)| value.trim().parse().ok())
                                .unwrap_or(0);
                            pending.drain(..end + 4);
                        }
                        if server_write.write_all(&buffer[..read]).await.is_err() {
                            break;
                        }
                    }
                };
                let downward = async move {
                    let _ = tokio::io::copy(&mut server_read, &mut client_write).await;
                };
                tokio::join!(upward, downward);
            });
        }
    });
    format!("http://{address}")
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

// ---- 報表 ----

struct Phase {
    name: String,
    durations: Vec<Duration>,
    requests: BTreeMap<String, usize>,
}

fn millis(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

fn percentile(sorted: &[Duration], fraction: f64) -> Duration {
    if sorted.is_empty() {
        return Duration::ZERO;
    }
    let position = ((sorted.len() - 1) as f64 * fraction).round() as usize;
    sorted[position.min(sorted.len() - 1)]
}

fn print_phase(phase: &Phase) {
    let mut sorted = phase.durations.clone();
    sorted.sort();
    let total: Duration = sorted.iter().sum();
    let count = sorted.len().max(1);
    let request_total: usize = phase.requests.values().sum();
    eprintln!(
        "[bench] {:<34} n={:<3} total={:>8.1}ms avg={:>6.2}ms p50={:>6.2}ms p95={:>6.2}ms max={:>7.2}ms requests={} ({:.2}/each)",
        phase.name,
        sorted.len(),
        millis(total),
        millis(total) / count as f64,
        millis(percentile(&sorted, 0.5)),
        millis(percentile(&sorted, 0.95)),
        millis(sorted.last().copied().unwrap_or_default()),
        request_total,
        request_total as f64 / count as f64,
    );
    for (name, times) in &phase.requests {
        eprintln!("[bench]     {times:>4} × {name}");
    }
}

// ---- 裝置 ----

struct WbfDevice<C: PackChannel> {
    session: Session,
    ws: WbfClient<C>,
    engine: OlmEngine,
    store_dir: PathBuf,
}

async fn open_engine(session: &Session, label: &str) -> (OlmEngine, PathBuf) {
    let store_dir =
        std::env::temp_dir().join(format!("wbf-bench-{}-{}", label, std::process::id()));
    let _ = std::fs::remove_dir_all(&store_dir);
    let engine = OlmEngine::open(
        &store_dir,
        &random_key(),
        &session.user_id,
        &session.device_id,
    )
    .await
    .expect("open the crypto store");
    (engine, store_dir)
}

async fn http_post(session: &Session, path: &str, body: serde_json::Value) -> serde_json::Value {
    reqwest::Client::new()
        .post(format!("{}{path}", session.server))
        .bearer_auth(&session.access_token)
        .json(&body)
        .send()
        .await
        .expect(path)
        .json()
        .await
        .expect("json body")
}

async fn create_shared_encrypted_room(alice: &Session, bob: &Session) -> String {
    let created = http_post(
        alice,
        "/_matrix/client/v3/createRoom",
        serde_json::json!({
            "preset": "private_chat",
            "invite": [bob.user_id],
            "initial_state": [{ "type": "m.room.encryption", "state_key": "", "content": { "algorithm": "m.megolm.v1.aes-sha2" } }]
        }),
    )
    .await;
    let room_id = created["room_id"].as_str().expect("room_id").to_string();
    let joined = http_post(
        bob,
        &format!("/_matrix/client/v3/join/{room_id}"),
        serde_json::json!({}),
    )
    .await;
    assert_eq!(joined["room_id"], room_id.as_str(), "{joined}");
    room_id
}

fn message(body: &str, txn_id: String) -> OutgoingRoomEvent {
    OutgoingRoomEvent {
        event_type: "m.room.message".into(),
        content: serde_json::json!({ "msgtype": "m.text", "body": body }),
        txn_id,
        attachments: Vec::new(),
    }
}

#[tokio::test]
#[ignore = "needs a running wbfuwunel with two accounts; see file header"]
async fn compare_sending_over_wbf_and_over_the_matrix_sdk_client() {
    let (Some(server), Some(user_a), Some(password_a), Some(user_b), Some(password_b)) = (
        env("WBF_E2E_SERVER"),
        env("WBF_E2E_USER"),
        read_password("WBF_E2E_PASSWORD_FILE"),
        env("WBF_E2E_USER_B"),
        read_password("WBF_E2E_PASSWORD_B_FILE"),
    ) else {
        eprintln!("WBF_E2E_* (incl. _B) not set; skipping");
        return;
    };
    let run_tag = format!(
        "{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|since| since.as_nanos())
            .unwrap_or(0)
    );

    // ---- 三台裝置都先上傳金鑰，兩條路面對同一組裝置 ----
    let ws_ledger: Ledger = Arc::new(Mutex::new(Vec::new()));
    let alice_session = login_with_password(&server, &user_a, &password_a, "bench alice W")
        .await
        .expect("alice W login");
    let channel = Channel::connect(
        &alice_session.server,
        &alice_session.access_token,
        Transport::WebSocket,
    )
    .await
    .expect("ws");
    let mut alice_ws = WbfClient::new(CountingChannel {
        inner: channel,
        ledger: ws_ledger.clone(),
    });
    alice_ws
        .hello(
            "bench alice W",
            &[wbf_sdk::protocol::DEVICE_VERSIONS_FEATURE],
        )
        .await
        .expect("hello");
    let (alice_engine, alice_store) = open_engine(&alice_session, "alice-w").await;
    let mut alice = WbfDevice {
        session: alice_session,
        ws: alice_ws,
        engine: alice_engine,
        store_dir: alice_store,
    };
    alice
        .engine
        .send_outgoing_requests(&mut alice.ws)
        .await
        .expect("alice W uploads keys");

    let bob_session = login_with_password(&server, &user_b, &password_b, "bench bob B")
        .await
        .expect("bob login");
    let bob_channel = Channel::connect(
        &bob_session.server,
        &bob_session.access_token,
        Transport::WebSocket,
    )
    .await
    .expect("bob ws");
    let mut bob_ws = WbfClient::new(bob_channel);
    bob_ws.hello("bench bob B", &[]).await.expect("bob hello");
    let (bob_engine, bob_store) = open_engine(&bob_session, "bob-b").await;
    let mut bob = WbfDevice {
        session: bob_session,
        ws: bob_ws,
        engine: bob_engine,
        store_dir: bob_store,
    };
    bob.engine
        .send_outgoing_requests(&mut bob.ws)
        .await
        .expect("bob uploads keys");

    let wbf_room = create_shared_encrypted_room(&alice.session, &bob.session).await;
    let matrix_room = create_shared_encrypted_room(&alice.session, &bob.session).await;

    let http_ledger: Ledger = Arc::new(Mutex::new(Vec::new()));
    let upstream = server
        .trim_start_matches("http://")
        .trim_end_matches('/')
        .to_string();
    // 代理多一跳：量延遲時設 `WBF_BENCH_NO_PROXY=1` 直連（HTTP 請求數就是 0，只看時間）。
    let matrix_server = if env("WBF_BENCH_NO_PROXY").is_some() {
        server.trim_end_matches('/').to_string()
    } else {
        start_counting_proxy(upstream, http_ledger.clone()).await
    };
    let matrix_store = std::env::temp_dir().join(format!("wbf-bench-m-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&matrix_store);
    let (matrix, matrix_session) = wbf_sdk::backend::matrix_sdk::MatrixBackend::login(
        &matrix_server,
        &user_a,
        &password_a,
        "bench alice M",
        &matrix_store,
        &random_key(),
        false,
    )
    .await
    .expect("matrix-sdk login");
    // 一次 sync：拿到房間、上傳 M 的金鑰（上游在 sync 裡處理 outgoing）。
    matrix
        .sync_once(None, Duration::ZERO)
        .await
        .expect("sync_once");
    matrix
        .sync_once(None, Duration::ZERO)
        .await
        .expect("sync_once again");

    let mut phases: Vec<Phase> = Vec::new();

    // ---- wbf ----
    // 開房（UI 點進房間）：refresh；後台：分金鑰。這兩步在使用者打字之前就做完。
    let before = ledger_len(&ws_ledger);
    let started = Instant::now();
    let refreshed = alice
        .engine
        .refresh_room_devices(&mut alice.ws, &wbf_room, None)
        .await
        .expect("refresh");
    let refresh_time = started.elapsed();
    let after_refresh = ledger_len(&ws_ledger);
    let members: Vec<String> = refreshed.versions.members.keys().cloned().collect();
    let room_version = refreshed.versions.room_version;
    let started = Instant::now();
    let shared = alice
        .engine
        .distribute_room_key(&mut alice.ws, &wbf_room, &members)
        .await
        .expect("distribute");
    let distribute_time = started.elapsed();
    let after_distribute = ledger_len(&ws_ledger);
    assert!(shared >= 1, "{shared}");
    phases.push(Phase {
        name: "wbf: open room (refresh)".into(),
        durations: vec![refresh_time],
        requests: ledger_slice(&ws_ledger, before, after_refresh),
    });
    phases.push(Phase {
        name: "wbf: background distribute".into(),
        durations: vec![distribute_time],
        requests: ledger_slice(&ws_ledger, after_refresh, after_distribute),
    });

    // 兩條路一則一則交錯送：同一段時間、同一台 server 的狀態，誰先誰後不會偏一邊。
    let mut wbf_durations = Vec::new();
    let mut wbf_requests: BTreeMap<String, usize> = BTreeMap::new();
    let mut rotate_durations = Vec::new();
    let mut rotate_requests: BTreeMap<String, usize> = BTreeMap::new();
    let mut matrix_durations = Vec::new();
    let mut matrix_requests: BTreeMap<String, usize> = BTreeMap::new();
    for index in 0..=STEADY {
        // ---- wbf ----
        let before = ledger_len(&ws_ledger);
        let started = Instant::now();
        let outcome = alice
            .engine
            .encrypt_and_send(
                &mut alice.ws,
                &wbf_room,
                room_version,
                &message(
                    &format!("wbf {index}"),
                    format!("bench-w-{index}-{run_tag}"),
                ),
            )
            .await
            .expect("wbf send");
        let elapsed = started.elapsed();
        assert!(
            matches!(outcome, SendOutcome::Sent { .. }),
            "message {index}: {outcome:?}"
        );
        let requests = ledger_slice(&ws_ledger, before, ledger_len(&ws_ledger));
        if index == 0 {
            phases.push(Phase {
                name: "wbf: first message".into(),
                durations: vec![elapsed],
                requests,
            });
        } else {
            for (name, times) in requests {
                *wbf_requests.entry(name).or_insert(0) += times;
            }
            wbf_durations.push(elapsed);
        }
        // 後台的 AfterSend：快到期就先丟再分（/docs/design/keys/e2ee-rpc.md §3.1），不算在送出時間裡。
        if let RoomKeyState::Ready {
            due_for_rotation: true,
            ..
        } = alice.engine.room_key_state(&wbf_room).await.unwrap()
        {
            let before = ledger_len(&ws_ledger);
            let started = Instant::now();
            alice.engine.discard_room_key(&wbf_room).await.unwrap();
            alice
                .engine
                .distribute_room_key(&mut alice.ws, &wbf_room, &members)
                .await
                .expect("rotate");
            rotate_durations.push(started.elapsed());
            for (name, times) in ledger_slice(&ws_ledger, before, ledger_len(&ws_ledger)) {
                *rotate_requests.entry(name).or_insert(0) += times;
            }
        }

        // ---- matrix-sdk ----
        let before = ledger_len(&http_ledger);
        let started = Instant::now();
        matrix
            .send_text(&matrix_room, &format!("matrix-sdk {index}"))
            .await
            .expect("matrix-sdk send");
        let elapsed = started.elapsed();
        let requests = ledger_slice(&http_ledger, before, ledger_len(&http_ledger));
        if index == 0 {
            phases.push(Phase {
                name: "matrix-sdk: first message".into(),
                durations: vec![elapsed],
                requests,
            });
        } else {
            for (name, times) in requests {
                *matrix_requests.entry(name).or_insert(0) += times;
            }
            matrix_durations.push(elapsed);
        }
    }
    phases.push(Phase {
        name: format!("wbf: next {STEADY} messages"),
        durations: wbf_durations,
        requests: wbf_requests,
    });
    phases.push(Phase {
        name: "wbf: background early rotation".into(),
        durations: rotate_durations,
        requests: rotate_requests,
    });
    phases.push(Phase {
        name: format!("matrix-sdk: next {STEADY} messages"),
        durations: matrix_durations,
        requests: matrix_requests,
    });

    eprintln!("[bench] ---- {STEADY}+1 messages per path, devices: alice W (wbf), alice M (matrix-sdk), bob B (wbf) ----");
    for phase in &phases {
        print_phase(phase);
    }

    drop(matrix);
    logout(&matrix_session).await.expect("logout M");
    logout(&alice.session).await.expect("logout W");
    logout(&bob.session).await.expect("logout B");
    let _ = std::fs::remove_dir_all(&alice.store_dir);
    let _ = std::fs::remove_dir_all(&bob.store_dir);
    let _ = std::fs::remove_dir_all(&matrix_store);
}

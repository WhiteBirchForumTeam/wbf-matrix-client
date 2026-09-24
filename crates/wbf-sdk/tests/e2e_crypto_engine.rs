//! E2EE 引擎對著真的 wbfuwunel 跑（e2ee-walkthrough.md §16.3 第 3 支的「收」那半）。平常 `#[ignore]`；要跑：
//!
//! ```text
//! WBF_E2E_SERVER=http://127.0.0.1:6167 WBF_E2E_USER=alice WBF_E2E_PASSWORD_FILE=<檔> \
//!     cargo test -p wbf-sdk --features matrix --test e2e_crypto_engine -- --ignored --nocapture --test-threads=1
//! ```
//!
//! ⚠️ `--test-threads=1`：兩條測試都用同一個帳號（alice），並行跑會互相分到對方裝置的房間金鑰、互相推 CryptoState。
//!
//! 走的是 e2ee-walkthrough §6 那條最容易漏的路：同一個帳號的**兩台裝置** A、B，各自只靠 WS（橋 ＋ `Device/Fetch`）——
//! A 上傳金鑰、查到 B、跟 B claim OTK 建 Olm、把一個加密房的房間金鑰用 to-device 發給 B；B 用 `Device/Fetch` 拉、匯進自己的
//! OlmMachine、拿到那把房間金鑰、叫 server 銷毀、再拉一次是空的。🚫 全程沒有 `/sync`、沒有 matrix-sdk 的 `Client`。
#![cfg(feature = "matrix")]

use std::path::PathBuf;
use std::time::Duration;

use matrix_sdk_crypto::EncryptionSettings;
use wbf_sdk::crypto_engine::{OlmEngine, OutgoingRoomEvent, SendOutcome};
use wbf_sdk::login::{login_with_password, logout, Session};
use wbf_sdk::protocol::{BRIDGE_FEATURE, DEVICE_FEATURE};
use wbf_sdk::to_device_state::ToDeviceState;
use wbf_sdk::vault::Key32;
use wbf_sdk::{Channel, Transport, WbfClient};

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

struct Target {
    server: String,
    user: String,
    password: String,
}

fn target() -> Option<Target> {
    let password = std::fs::read_to_string(env("WBF_E2E_PASSWORD_FILE")?)
        .ok()?
        .trim_end_matches(['\r', '\n'])
        .to_string();
    Some(Target {
        server: env("WBF_E2E_SERVER")?,
        user: env("WBF_E2E_USER")?,
        password,
    })
}

fn random_key() -> Key32 {
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes).unwrap();
    Key32(bytes)
}

/// 一台裝置：它的 session、WS 連線、自己的 crypto store（暫存目錄的 `m/`）。
struct Device {
    session: Session,
    ws: WbfClient<Channel>,
    engine: OlmEngine,
    store_dir: PathBuf,
}

async fn log_in_a_device(target: &Target, label: &str) -> Device {
    let session = login_with_password(&target.server, &target.user, &target.password, label)
        .await
        .expect("login");
    let channel = Channel::connect(&session.server, &session.access_token, Transport::WebSocket)
        .await
        .expect("ws");
    let mut ws = WbfClient::new(channel);
    let hello = ws.hello(label, &[]).await.expect("hello");
    for feature in [BRIDGE_FEATURE, DEVICE_FEATURE] {
        assert!(
            hello
                .features
                .iter()
                .any(|advertised| advertised == feature),
            "{feature}: {:?}",
            hello.features
        );
    }
    let store_dir =
        std::env::temp_dir().join(format!("wbf-e2e-crypto-{}-{}", label, std::process::id()));
    let _ = std::fs::remove_dir_all(&store_dir);
    let engine = OlmEngine::open(
        &store_dir,
        &random_key(),
        &session.user_id,
        &session.device_id,
    )
    .await
    .expect("open the crypto store");
    Device {
        session,
        ws,
        engine,
        store_dir,
    }
}

async fn create_encrypted_room(session: &Session) -> String {
    let created: serde_json::Value = reqwest::Client::new()
        .post(format!("{}/_matrix/client/v3/createRoom", session.server))
        .bearer_auth(&session.access_token)
        .json(&serde_json::json!({
            "preset": "private_chat",
            "initial_state": [{ "type": "m.room.encryption", "state_key": "", "content": { "algorithm": "m.megolm.v1.aes-sha2" } }]
        }))
        .send()
        .await
        .expect("createRoom")
        .json()
        .await
        .expect("createRoom body");
    created["room_id"].as_str().expect("room_id").to_string()
}

#[tokio::test]
#[ignore = "needs a running wbfuwunel; see file header"]
async fn a_room_key_travels_from_device_a_to_device_b_over_the_channel_only() {
    let Some(target) = target() else {
        eprintln!("WBF_E2E_* not set; skipping");
        return;
    };
    let mut a = log_in_a_device(&target, "wbf-sdk e2e crypto A").await;
    let mut b = log_in_a_device(&target, "wbf-sdk e2e crypto B").await;
    let user_id = a.session.user_id.clone();
    assert_ne!(a.session.device_id, b.session.device_id);

    // 1. 兩台各自把自己的金鑰（device keys、OTK、fallback）走橋上傳：狀態機的第一批 outgoing 就是 KeysUpload。
    let sent_by_a = a
        .engine
        .send_outgoing_requests(&mut a.ws)
        .await
        .expect("A uploads keys");
    let sent_by_b = b
        .engine
        .send_outgoing_requests(&mut b.ws)
        .await
        .expect("B uploads keys");
    assert!(
        sent_by_a >= 1 && sent_by_b >= 1,
        "A {sent_by_a} B {sent_by_b}"
    );

    // 2. A 追蹤自己這個帳號並標記「變了」（第 1 步上傳時狀態機已經順手查過自己，那時 B 還沒上傳；
    //    已追蹤的人只靠 track_users 不會再查）→ KeysQuery 走橋 → A 的 store 裡看得到 B 這台裝置。
    a.engine
        .track_users(std::slice::from_ref(&user_id))
        .await
        .unwrap();
    a.engine
        .mark_users_changed(std::slice::from_ref(&user_id))
        .await
        .unwrap();
    a.engine
        .send_outgoing_requests(&mut a.ws)
        .await
        .expect("A queries keys");
    let known = a.engine.known_devices_of(&user_id).await.unwrap();
    assert!(known.contains(&b.session.device_id), "A knows B: {known:?}");

    // 3. 一個加密房；A 把房間金鑰分給這個帳號的每台裝置（缺 Olm session 的先 claim OTK）。至少一個 to-device 給 B。
    let room_id = create_encrypted_room(&a.session).await;
    let to_device_requests = a
        .engine
        .share_room_key(
            &mut a.ws,
            &room_id,
            std::slice::from_ref(&user_id),
            EncryptionSettings::default(),
        )
        .await
        .expect("A shares the room key");
    assert!(to_device_requests >= 1, "{to_device_requests}");

    // 4. B 訂閱（持有這台裝置的佇列，銷毀才被准）：回來的 CryptoState 是 B 自己的 OTK 存量，餵給狀態機。
    //    A 剛 claim 走 B 一把 OTK，所以剩的比上傳的少；狀態機看到數字會決定要不要補。
    assert_eq!(
        b.engine.to_device_state().unwrap(),
        ToDeviceState::default()
    );
    let crypto_state =
        b.ws.device_subscribe(&b.session.device_id, Duration::from_secs(30))
            .await
            .expect("B subscribes");
    let remaining = crypto_state
        .otk_counts
        .get("signed_curve25519")
        .copied()
        .unwrap_or(0);
    assert!(remaining > 0, "{crypto_state:?}");
    b.engine
        .receive_to_device(
            Vec::new(),
            Some(&crypto_state.otk_counts),
            Some(&crypto_state.unused_fallback_key_types),
        )
        .await
        .expect("B feeds its OTK counts");

    // 5. B 從頭拉到追平（Fetch → 匯入 → 落地 → 銷毀，順序在 import_items 裡鎖死）：
    //    拿到 A 發的 Olm 密文 → 匯進狀態機 → 就是那個房間的房間金鑰；銷毀回來的 count ＝ 送的；清單清空。
    let reports = b
        .engine
        .pull_to_device(&mut b.ws, Duration::from_secs(30))
        .await
        .expect("B pulls its to-device queue");
    assert_eq!(reports.len(), 1, "{reports:?}");
    let report = &reports[0];
    assert!(report.imported >= 1, "{report:?}");
    assert!(
        report
            .room_keys
            .iter()
            .any(|key| key.room_id.as_str() == room_id),
        "B got a room key for {room_id}: {report:?}"
    );
    assert_eq!(report.destroyed.len(), report.imported, "{report:?}");
    assert_eq!(report.still_to_destroy, 0);
    let state = b.engine.to_device_state().unwrap();
    assert_eq!(state.cd_seq, report.cd_seq);
    assert!(state.cd_seq.is_some());
    assert!(state.to_destroy.is_empty(), "水位與清單落地了：{state:?}");

    // 5b. 再拉一次：空窗、什麼都沒動、水位不變。
    let again = b
        .engine
        .pull_to_device(&mut b.ws, Duration::from_secs(30))
        .await
        .unwrap();
    assert_eq!(
        (
            again.len(),
            again[0].imported,
            again[0].destroyed.len(),
            again[0].cd_seq
        ),
        (1, 0, 0, state.cd_seq),
        "{again:?}"
    );

    // 6. A 再分一次同一把：每台裝置都有了 → 沒有新的 to-device。
    let again_shared = a
        .engine
        .share_room_key(
            &mut a.ws,
            &room_id,
            std::slice::from_ref(&user_id),
            EncryptionSettings::default(),
        )
        .await
        .unwrap();
    assert_eq!(again_shared, 0);

    // 7. 下線：說出口的退出（不叫的話這條連線退了卻還佔著裝置）。
    b.ws.device_unsubscribe().await.expect("B unsubscribes");

    logout(&a.session).await.expect("logout A");
    logout(&b.session).await.expect("logout B");
    let _ = std::fs::remove_dir_all(&a.store_dir);
    let _ = std::fs::remove_dir_all(&b.store_dir);
}

// ---- 第 4 階段：訂閱長活，A 分房間金鑰時 server 推的 `Device/Push` 要打進 B 的 handle（ws-receive-dispatch.md §3）----

/// 跟上面同一條路，但 B 用 `device_subscription`（長活）：A 分房間金鑰 → server 推 `Push` 給 B 的訂閱 → handle 收到，
/// 而且同一條連線上 B 之後跑 `Fetch`（串流會話）也不會把訂閱的 pack 吃掉、沒有任何 pack 無主。
/// 📎 舊通道沒有長活訂閱這個 API（推播被丟在地上），所以這條沒有「舊版會紅」可比；它驗的是推播真的進了 handle。
#[tokio::test]
#[ignore = "needs a running wbfuwunel; see file header"]
async fn a_live_subscription_receives_the_push_for_a_room_key_shared_while_it_is_open() {
    let Some(target) = target() else {
        eprintln!("WBF_E2E_* not set; skipping");
        return;
    };
    let mut a = log_in_a_device(&target, "wbf-sdk e2e live A").await;
    let mut b = log_in_a_device(&target, "wbf-sdk e2e live B").await;
    let user_id = a.session.user_id.clone();
    for device in [&mut a, &mut b] {
        device
            .engine
            .send_outgoing_requests(&mut device.ws)
            .await
            .expect("uploads keys");
    }
    a.engine
        .track_users(std::slice::from_ref(&user_id))
        .await
        .unwrap();
    a.engine
        .mark_users_changed(std::slice::from_ref(&user_id))
        .await
        .unwrap();
    a.engine
        .send_outgoing_requests(&mut a.ws)
        .await
        .expect("A queries keys");

    // B 先訂閱、一直收；佇列是空的，所以沒有 early push。
    let mut live =
        b.ws.device_subscription(&b.session.device_id, Duration::from_secs(30))
            .await
            .expect("B subscribes for good");
    assert!(live.early_pushes.is_empty(), "{:?}", live.early_pushes);
    assert!(
        live.crypto_state
            .otk_counts
            .get("signed_curve25519")
            .copied()
            .unwrap_or(0)
            > 0
    );

    // A 分房間金鑰：至少一則 to-device 給 B → server 推 Push 到 B 的訂閱。
    let room_id = create_encrypted_room(&a.session).await;
    let shared = a
        .engine
        .share_room_key(
            &mut a.ws,
            &room_id,
            std::slice::from_ref(&user_id),
            EncryptionSettings::default(),
        )
        .await
        .expect("A shares the room key");
    assert!(shared >= 1);

    // A claim 走 B 一把 OTK 時 server 會先推 `CryptoState`（3b 在真 server 踩到的），然後才是 `Push`：兩者都進同一個 handle，順序由 server 定。
    let mut seen = Vec::new();
    while !seen.contains(&wbf_wire::pack::device::PUSH) {
        let pack = live
            .subscription
            .next(Duration::from_secs(30))
            .await
            .expect("the subscription is alive")
            .expect("a pack, not the end");
        assert_eq!(pack.kind, wbf_wire::Kind::Device, "{pack:?}");
        seen.push(pack.subtype);
        assert!(seen.len() <= 4, "still no Push after {seen:?}");
    }
    assert!(!live.subscription.take_gap());

    // 同一條連線上再拉一窗（串流會話）：Push 已經在訂閱那邊，Fetch 拿到的是佇列裡那一則本體，兩邊不打架。
    let window =
        b.ws.device_fetch_window(
            &wbf_sdk::protocol::DeviceFetchRequest {
                cd_seq: None,
                limit: None,
            },
            Duration::from_secs(30),
        )
        .await
        .expect("B fetches");
    assert!(window.tc >= 1, "{window:?}");
    assert_eq!(b.ws.channel().unmatched(), Some(0), "每一個 pack 都有主");

    b.ws.device_unsubscribe().await.expect("B unsubscribes");
    logout(&a.session).await.expect("logout A");
    logout(&b.session).await.expect("logout B");
    let _ = std::fs::remove_dir_all(&a.store_dir);
    let _ = std::fs::remove_dir_all(&b.store_dir);
}

// ---- #45 的驗收（3b）：Bob 登新裝置 → 帶舊號碼送 → 1506 → 補金鑰 → 重送 → Bob 新裝置收到房間金鑰、解得開 ----
//
// 多要一個帳號：WBF_E2E_USER_B、WBF_E2E_PASSWORD_B_FILE（沒設就跳過這條）。

struct TargetB {
    user: String,
    password: String,
}

fn target_b() -> Option<TargetB> {
    let password = std::fs::read_to_string(env("WBF_E2E_PASSWORD_B_FILE")?)
        .ok()?
        .trim_end_matches(['\r', '\n'])
        .to_string();
    Some(TargetB {
        user: env("WBF_E2E_USER_B")?,
        password,
    })
}

/// 登入一台裝置；`declare_device_versions` 是「這條連線會在每則加密訊息帶 `room_version`」的宣告——只有送出那台才開。
async fn log_in_device_of(
    server: &str,
    user: &str,
    password: &str,
    label: &str,
    declare_device_versions: bool,
) -> Device {
    let session = login_with_password(server, user, password, label)
        .await
        .expect("login");
    let channel = Channel::connect(&session.server, &session.access_token, Transport::WebSocket)
        .await
        .expect("ws");
    let mut ws = WbfClient::new(channel);
    let features: &[&str] = if declare_device_versions {
        &[wbf_sdk::protocol::DEVICE_VERSIONS_FEATURE]
    } else {
        &[]
    };
    ws.hello(label, features).await.expect("hello");
    let store_dir =
        std::env::temp_dir().join(format!("wbf-e2e-45-{}-{}", label, std::process::id()));
    let _ = std::fs::remove_dir_all(&store_dir);
    let engine = OlmEngine::open(
        &store_dir,
        &random_key(),
        &session.user_id,
        &session.device_id,
    )
    .await
    .expect("open the crypto store");
    Device {
        session,
        ws,
        engine,
        store_dir,
    }
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

/// 用 `Recent` 把這個房的密文事件拉下來（新→舊）直到看到 `wanted_event_id`，交給引擎解。
/// 實跑（2026-09-21）是 0 次輪詢就看到；留著輪詢只是防 server 端寫入與索引之間哪天出現一拍。
/// 📎 曾經以為 Recent 看不到帶 room_version 的加密訊息，追下去是 txn_id 跨輪重用被 server 去重、拿到上一輪別的房的 event_id——
/// 不是 Recent 的問題（見 e2ee-walkthrough §16.4）。
async fn read_room_events(
    device: &mut Device,
    room_id: &str,
    wanted_event_id: &str,
) -> Vec<serde_json::Value> {
    for attempt in 0..25 {
        let events = read_room_events_once(device, room_id).await;
        if events
            .iter()
            .any(|event| event["event_id"] == wanted_event_id)
        {
            eprintln!("[recent] {wanted_event_id} visible after {attempt} retries");
            return events;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    panic!("{wanted_event_id} never showed up in Recent for {room_id}");
}

async fn read_room_events_once(device: &mut Device, room_id: &str) -> Vec<serde_json::Value> {
    let mut events = Vec::new();
    let mut collect = |_: &wbf_sdk::protocol::BatchMeta, batch: Vec<serde_json::Value>| {
        events.extend(batch);
        Ok(())
    };
    device
        .ws
        .recent_window(
            &wbf_sdk::protocol::RecentRequest {
                rooms: Some(vec![room_id.to_string()]),
                limit: 50,
                cg_seq: None,
                before: None,
                batch: None,
            },
            Duration::from_secs(30),
            &mut collect,
        )
        .await
        .expect("Recent");
    events
        .into_iter()
        .filter(|event| event["type"] == "m.room.encrypted")
        .collect()
}

async fn decrypt_body(device: &Device, room_id: &str, event: &serde_json::Value) -> String {
    let clear = device
        .engine
        .decrypt_room_event(room_id, event)
        .await
        .unwrap_or_else(|error| {
            panic!(
                "{} cannot decrypt {}: {error}",
                device.session.device_id, event["event_id"]
            )
        });
    assert_eq!(clear["type"], "m.room.message", "{clear}");
    clear["content"]["body"].as_str().expect("body").to_string()
}

#[tokio::test]
#[ignore = "needs a running wbfuwunel with two accounts; see file header"]
async fn issue_45_acceptance_stale_room_version_is_refused_then_fixed_and_resent() {
    let (Some(target), Some(target_b)) = (target(), target_b()) else {
        eprintln!("WBF_E2E_* (incl. _B) not set; skipping");
        return;
    };
    // ⚠️ txn_id 每輪要不同：server 的 WS Send 把 txn 去重鍵在帳號（`sender_device: None`），重用會拿到上一輪別的房的 event_id。
    let run_tag = format!(
        "{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|since| since.as_nanos())
            .unwrap_or(0)
    );
    let txn_1 = format!("45-txn-1-{run_tag}");
    let txn_2 = format!("45-txn-2-{run_tag}");
    // Alice 的送出裝置宣告 feature：之後每則加密訊息都帶 room_version。Bob 的裝置只收，不宣告。
    let mut alice = log_in_device_of(
        &target.server,
        &target.user,
        &target.password,
        "45 alice A",
        true,
    )
    .await;
    let mut bob1 = log_in_device_of(
        &target.server,
        &target_b.user,
        &target_b.password,
        "45 bob B1",
        false,
    )
    .await;
    alice
        .engine
        .send_outgoing_requests(&mut alice.ws)
        .await
        .expect("alice uploads keys");
    bob1.engine
        .send_outgoing_requests(&mut bob1.ws)
        .await
        .expect("bob B1 uploads keys");

    // 1. Alice 開加密房、邀 Bob、Bob 加入（建房與成員操作走 HTTP，不在這一支的範圍）。
    let room_id = create_encrypted_room(&alice.session).await;
    http_post(
        &alice.session,
        &format!("/_matrix/client/v3/rooms/{room_id}/invite"),
        serde_json::json!({ "user_id": bob1.session.user_id }),
    )
    .await;
    let joined = http_post(
        &bob1.session,
        &format!("/_matrix/client/v3/join/{room_id}"),
        serde_json::json!({}),
    )
    .await;
    assert_eq!(joined["room_id"], room_id, "{joined}");

    // 2. Alice 點進房間：refresh（第一次，每個人都查）→ 房間金鑰發給 Alice 與 Bob B1。
    let first = alice
        .engine
        .refresh_room_devices(&mut alice.ws, &room_id, None)
        .await
        .expect("alice refresh #1");
    assert_eq!(first.versions.members.len(), 2, "{first:?}");
    assert!(first.versions.members.contains_key(&bob1.session.user_id));
    assert!(first.shared_to_device_requests >= 1, "{first:?}");
    assert!(first.rechecked.is_empty(), "雜湊第一次就對上：{first:?}");

    // 3. Alice 帶這份快照的號碼送：接受。Bob B1 拉 to-device 拿到房間金鑰、Recent 拉密文、解開。
    let sent = alice
        .engine
        .encrypt_and_send(
            &mut alice.ws,
            &first,
            &OutgoingRoomEvent {
                event_type: "m.room.message".into(),
                content: serde_json::json!({ "msgtype": "m.text", "body": "hi bob" }),
                txn_id: txn_1.clone(),
                attachments: Vec::new(),
            },
        )
        .await
        .expect("send #1");
    let SendOutcome::Sent {
        event_id: first_event_id,
    } = sent
    else {
        panic!("{sent:?}")
    };
    bob1.ws
        .device_subscribe(&bob1.session.device_id, Duration::from_secs(30))
        .await
        .expect("B1 subscribes");
    let reports = bob1
        .engine
        .pull_to_device(&mut bob1.ws, Duration::from_secs(30))
        .await
        .expect("B1 pulls");
    assert!(
        reports
            .iter()
            .flat_map(|report| report.room_keys.iter())
            .any(|key| key.room_id.as_str() == room_id),
        "{reports:?}"
    );
    let events = read_room_events(&mut bob1, &room_id, &first_event_id).await;
    let first_event = events
        .iter()
        .find(|event| event["event_id"] == first_event_id)
        .expect("B1 sees the event");
    assert_eq!(decrypt_body(&bob1, &room_id, first_event).await, "hi bob");

    // 4. Bob 在新裝置 B2 登入並上傳金鑰 → Bob 的裝置版本號變、房間版本號變。
    let mut bob2 = log_in_device_of(
        &target.server,
        &target_b.user,
        &target_b.password,
        "45 bob B2",
        false,
    )
    .await;
    bob2.engine
        .send_outgoing_requests(&mut bob2.ws)
        .await
        .expect("bob B2 uploads keys");
    assert_ne!(bob1.session.device_id, bob2.session.device_id);

    // 5. Alice 帶**舊**號碼送：server 擋下（1506，帶目前的號碼），訊息沒送。
    let stale = alice
        .engine
        .encrypt_and_send(
            &mut alice.ws,
            &first,
            &OutgoingRoomEvent {
                event_type: "m.room.message".into(),
                content: serde_json::json!({ "msgtype": "m.text", "body": "hi again" }),
                txn_id: txn_2.clone(),
                attachments: Vec::new(),
            },
        )
        .await
        .expect("send #2 (stale)");
    let SendOutcome::RoomDevicesChanged {
        current_room_version,
        ..
    } = stale
    else {
        panic!("stale version must be refused: {stale:?}")
    };
    let current_room_version = current_room_version.expect("1506 carries the current room version");
    assert!(
        current_room_version > first.versions.room_version,
        "{current_room_version} > {}",
        first.versions.room_version
    );

    // 6. 修：refresh（跟上一份比 → 只有 Bob 變了 → 只重查 Bob → 房間金鑰補給 B2）。
    let second = alice
        .engine
        .refresh_room_devices(&mut alice.ws, &room_id, Some(&first.versions))
        .await
        .expect("alice refresh #2");
    assert_eq!(
        second.diff.changed,
        vec![bob1.session.user_id.clone()],
        "{second:?}"
    );
    assert!(second.diff.left.is_empty());
    assert!(
        second.versions.room_version >= current_room_version,
        "{second:?}"
    );
    assert!(
        second.versions.members[&bob1.session.user_id].seq
            > first.versions.members[&bob1.session.user_id].seq
    );
    assert!(
        second.shared_to_device_requests >= 1,
        "B2 must get the room key: {second:?}"
    );
    let known = alice
        .engine
        .known_devices_of(&bob1.session.user_id)
        .await
        .unwrap();
    assert!(known.contains(&bob2.session.device_id), "{known:?}");

    // 7. 帶新號碼重送（同一個 txn_id）：接受。
    let resent = alice
        .engine
        .encrypt_and_send(
            &mut alice.ws,
            &second,
            &OutgoingRoomEvent {
                event_type: "m.room.message".into(),
                content: serde_json::json!({ "msgtype": "m.text", "body": "hi again" }),
                txn_id: txn_2.clone(),
                attachments: Vec::new(),
            },
        )
        .await
        .expect("send #2 (resend)");
    let SendOutcome::Sent {
        event_id: second_event_id,
    } = resent
    else {
        panic!("{resent:?}")
    };
    assert_ne!(second_event_id, first_event_id);

    // 8. Bob 的新裝置 B2 收到那一輪的房間金鑰、解得開重送的那則；舊裝置 B1 也解得開。
    bob2.ws
        .device_subscribe(&bob2.session.device_id, Duration::from_secs(30))
        .await
        .expect("B2 subscribes");
    let reports = bob2
        .engine
        .pull_to_device(&mut bob2.ws, Duration::from_secs(30))
        .await
        .expect("B2 pulls");
    assert!(
        reports
            .iter()
            .flat_map(|report| report.room_keys.iter())
            .any(|key| key.room_id.as_str() == room_id),
        "B2 got no room key: {reports:?}"
    );
    let events = read_room_events(&mut bob2, &room_id, &second_event_id).await;
    let second_event = events
        .iter()
        .find(|event| event["event_id"] == second_event_id)
        .expect("B2 sees the event");
    assert_eq!(
        decrypt_body(&bob2, &room_id, second_event).await,
        "hi again"
    );
    bob1.engine
        .pull_to_device(&mut bob1.ws, Duration::from_secs(30))
        .await
        .expect("B1 pulls again");
    let events = read_room_events(&mut bob1, &room_id, &second_event_id).await;
    let second_event = events
        .iter()
        .find(|event| event["event_id"] == second_event_id)
        .expect("B1 sees the event");
    assert_eq!(
        decrypt_body(&bob1, &room_id, second_event).await,
        "hi again"
    );

    for device in [&mut bob1, &mut bob2] {
        device.ws.device_unsubscribe().await.expect("unsubscribe");
    }
    for device in [&alice, &bob1, &bob2] {
        logout(&device.session).await.expect("logout");
        let _ = std::fs::remove_dir_all(&device.store_dir);
    }
}

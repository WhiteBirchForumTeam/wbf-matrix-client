//! E2EE 引擎對著真的 wbfuwunel 跑（e2ee-walkthrough.md §16.3 第 3 支的「收」那半）。平常 `#[ignore]`；要跑：
//!
//! ```text
//! WBF_E2E_SERVER=http://127.0.0.1:6167 WBF_E2E_USER=alice WBF_E2E_PASSWORD_FILE=<檔> \
//!     cargo test -p wbf-sdk --features matrix --test e2e_crypto_engine -- --ignored --nocapture
//! ```
//!
//! 走的是 e2ee-walkthrough §6 那條最容易漏的路：同一個帳號的**兩台裝置** A、B，各自只靠 WS（橋 ＋ `Device/Fetch`）——
//! A 上傳金鑰、查到 B、跟 B claim OTK 建 Olm、把一個加密房的房間金鑰用 to-device 發給 B；B 用 `Device/Fetch` 拉、匯進自己的
//! OlmMachine、拿到那把房間金鑰、叫 server 銷毀、再拉一次是空的。🚫 全程沒有 `/sync`、沒有 matrix-sdk 的 `Client`。
#![cfg(feature = "matrix")]

use std::path::PathBuf;
use std::time::Duration;

use matrix_sdk_crypto::EncryptionSettings;
use wbf_sdk::crypto_engine::OlmEngine;
use wbf_sdk::login::{login_with_password, logout, Session};
use wbf_sdk::protocol::{DeviceFetchRequest, BRIDGE_FEATURE, DEVICE_FEATURE};
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

    // 4. B 用 Device/Fetch 從頭拉：拿到 A 發的 Olm 密文，匯進狀態機 → 就是那個房間的房間金鑰。
    let mut state = ToDeviceState::load(&b.store_dir).unwrap();
    assert_eq!(state, ToDeviceState::default());
    let window =
        b.ws.device_fetch_window(
            &DeviceFetchRequest {
                cd_seq: state.cd_seq,
                limit: None,
            },
            Duration::from_secs(30),
        )
        .await
        .expect("B fetches its to-device queue");
    assert!(!window.items.is_empty(), "B's queue is empty");
    assert!(
        window
            .items
            .iter()
            .all(|(_, event)| event["type"] == "m.room.encrypted" && event["sender"] == user_id),
        "{:?}",
        window.items
    );
    let counts: Vec<u64> = window.items.iter().map(|(count, _)| *count).collect();
    let events: Vec<serde_json::Value> = window.items.into_iter().map(|(_, event)| event).collect();
    let room_keys = b
        .engine
        .receive_to_device(events, None, None)
        .await
        .expect("B imports");
    assert!(
        room_keys.iter().any(|key| key.room_id.as_str() == room_id),
        "B got a room key for {room_id}: {room_keys:?}"
    );
    for count in &counts {
        state.mark_processed(*count);
    }
    state.save(&b.store_dir).unwrap();
    assert_eq!(state.cd_seq, window.nt);

    // 5. 訂閱（持有這台裝置的佇列，銷毀才被准）：回來的 CryptoState 是 B 自己的 OTK 存量，餵給狀態機。
    //    A 剛 claim 走 B 一把 OTK，所以剩的比上傳的少；狀態機看到數字會決定要不要補。
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

    // 5b. 銷毀：只有 ItemsDestroyed 回來的才從清單拿掉；之後再拉是空的。
    let gone =
        b.ws.device_items_destroy(&state.to_destroy.clone(), Duration::from_secs(30))
            .await
            .expect("B destroys");
    assert_eq!(gone, counts);
    state.mark_destroyed(&gone);
    state.save(&b.store_dir).unwrap();
    assert!(state.to_destroy.is_empty());
    let again =
        b.ws.device_fetch_window(
            &DeviceFetchRequest {
                cd_seq: state.cd_seq,
                limit: None,
            },
            Duration::from_secs(30),
        )
        .await
        .unwrap();
    assert_eq!((again.tc, again.items.len()), (0, 0), "{again:?}");
    assert_eq!(
        ToDeviceState::load(&b.store_dir).unwrap(),
        state,
        "水位與清單落地了"
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

    logout(&a.session).await.expect("logout A");
    logout(&b.session).await.expect("logout B");
    let _ = std::fs::remove_dir_all(&a.store_dir);
    let _ = std::fs::remove_dir_all(&b.store_dir);
}

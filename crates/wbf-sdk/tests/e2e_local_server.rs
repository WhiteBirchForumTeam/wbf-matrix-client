//! 對著真的 wbfuwunel 跑（/docs/design/rpc-specs/wbf-cli-spec.md §8 的驗收項目，SDK 層那幾條）。平常 `#[ignore]`；要跑：
//!
//! ```text
//! WBF_E2E_SERVER=http://127.0.0.1:6167 WBF_E2E_USER=alice WBF_E2E_PASSWORD_FILE=<檔> \
//!     cargo test -p wbf-sdk --test e2e_local_server -- --ignored --nocapture
//! ```
//!
//! 這裡紅 = SDK 與真 server 的行為對不上（記憶體版 server 沒抓到的差異），或 server 沒起。

use std::io::Cursor;

use sha2::{Digest, Sha256};
use wbf_sdk::login::{login_with_password, logout, whoami};
use wbf_sdk::{Channel, ChunkedBlock, Cipher, FileCipher, SdkError, Transport, WbfClient};

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

struct Target {
    server: String,
    user: String,
    password: String,
}

fn target() -> Option<Target> {
    let server = env("WBF_E2E_SERVER")?;
    let user = env("WBF_E2E_USER")?;
    let password_file = env("WBF_E2E_PASSWORD_FILE")?;
    let password = std::fs::read_to_string(password_file)
        .ok()?
        .trim_end_matches(['\r', '\n'])
        .to_string();
    Some(Target {
        server,
        user,
        password,
    })
}

fn random_bytes(len: usize) -> Vec<u8> {
    let mut bytes = vec![0u8; len];
    getrandom::getrandom(&mut bytes).expect("os rng");
    bytes
}

fn block(name: &str, file_size: Option<u64>) -> ChunkedBlock {
    ChunkedBlock {
        v: 1,
        cipher: Cipher::None,
        key: None,
        nonce_base: None,
        chunk_size: 0,
        file_size,
        name: Some(name.into()),
        mimetype: Some("application/octet-stream".into()),
        sha256: None,
    }
}

#[tokio::test]
#[ignore = "needs a running wbfuwunel; see file header"]
async fn upload_download_seek_resume_stream_against_real_server() {
    let Some(target) = target() else {
        eprintln!("WBF_E2E_* not set; skipping");
        return;
    };
    let session = login_with_password(
        &target.server,
        &target.user,
        &target.password,
        "wbf-sdk e2e",
    )
    .await
    .expect("login");
    let who = whoami(&session).await.expect("whoami");
    assert_eq!(who.user_id, session.user_id);

    // 1. Hello：features 有 upload、download。
    let channel = Channel::connect(&session.server, &session.access_token, Transport::WebSocket)
        .await
        .expect("ws");
    let mut ws = WbfClient::new(channel);
    let hello = ws.hello("wbf-sdk e2e", &[]).await.expect("hello");
    assert!(
        hello.features.contains(&"upload".to_string())
            && hello.features.contains(&"download".to_string())
    );
    ws.ping().await.expect("ping");

    // 2. 固定大小上傳（三種 cipher）：server 的標準下載拿到的密文 = 本地密文逐 byte。
    let plaintext = random_bytes(300 * 1024 + 123);
    let expected_sha = hex::encode(Sha256::digest(&plaintext));
    for cipher in [Cipher::ChaCha20Poly1305, Cipher::Aes256Gcm, Cipher::None] {
        let file_cipher = FileCipher::generate(cipher, 64 * 1024).unwrap();
        let state = ws
            .create_upload(
                &session.server,
                &session.user_id,
                &file_cipher,
                &block("e2e.bin", Some(plaintext.len() as u64)),
            )
            .await
            .expect("create");
        let summary = ws
            .send_chunks(
                &state,
                &mut Cursor::new(&plaintext),
                0,
                true,
                &mut |_, _| {},
            )
            .await
            .expect("send");
        assert_eq!(summary.sha256.as_deref(), Some(expected_sha.as_str()));
        let mut final_block = state.block.clone();
        final_block.sha256 = summary.sha256.clone();
        let manifest = ws.seal_upload(&state, &final_block).await.expect("seal");

        let standard =
            standard_media_download(&session.server, &session.access_token, &manifest.mxc).await;
        let mut local_ciphertext = Vec::new();
        for (index, plain) in plaintext.chunks(64 * 1024).enumerate() {
            local_ciphertext.extend(file_cipher.seal_chunk(index as u32, plain).unwrap());
        }
        assert_eq!(
            standard, local_ciphertext,
            "{cipher:?}: standard download must be the raw chunk stream"
        );

        // 3. 下載（走 HTTP 通道，兩種通道都跑到）。
        let http = Channel::connect(&session.server, &session.access_token, Transport::Http)
            .await
            .expect("http");
        let mut http_client = WbfClient::new(http);
        let mut out = Vec::new();
        let report = http_client
            .download(&manifest, &mut out, &mut |_, _| {})
            .await
            .expect("download");
        assert_eq!(out, plaintext, "{cipher:?}");
        assert!(report.sha256_verified);

        // 4. seek：只讀一塊。
        let seek = ws
            .seek_read(&manifest, 150_000, Some(4096))
            .await
            .expect("seek");
        assert_eq!(seek.bytes, &plaintext[150_000..154_096]);
        assert_eq!(
            seek.chunks_read.len(),
            1,
            "seek must read exactly one chunk"
        );
        assert!(!seek.truncated);
    }

    // 5. 續傳：送一半、換一條連線問 Status、接著送。
    let file_cipher = FileCipher::generate(Cipher::ChaCha20Poly1305, 64 * 1024).unwrap();
    let state = ws
        .create_upload(
            &session.server,
            &session.user_id,
            &file_cipher,
            &block("resume.bin", Some(plaintext.len() as u64)),
        )
        .await
        .expect("create");
    // 「殺掉」：來源在第三塊讀到一半就報 IO 錯，send_chunks 帶著兩塊已送的狀態回 Err。
    let mut dying_source = FailAfter {
        inner: Cursor::new(&plaintext),
        remaining: 2 * 64 * 1024 + 100,
    };
    let partial = ws
        .send_chunks(&state, &mut dying_source, 0, false, &mut |_, _| {})
        .await;
    assert!(matches!(partial, Err(SdkError::Io(_))), "{partial:?}");

    let channel2 = Channel::connect(&session.server, &session.access_token, Transport::WebSocket)
        .await
        .expect("ws2");
    let mut ws2 = WbfClient::new(channel2);
    let status = ws2
        .upload_status(state.upload_id)
        .await
        .expect("status on a fresh connection");
    assert_eq!((status.received, status.finished), (2, false));
    let summary = ws2
        .send_chunks(
            &state,
            &mut Cursor::new(&plaintext),
            status.received,
            true,
            &mut |_, _| {},
        )
        .await
        .expect("resume");
    assert_eq!(summary.chunks_sent, 5);
    let mut final_block = state.block.clone();
    final_block.sha256 = summary.sha256;
    let manifest = ws2
        .seal_upload(&state, &final_block)
        .await
        .expect("seal after reconnect");
    let mut out = Vec::new();
    ws2.download(&manifest, &mut out, &mut |_, _| {})
        .await
        .expect("download");
    assert_eq!(out, plaintext);

    // 6. 串流：0/0 哨兵、IS_LAST、Seal 帶最終描述。
    let file_cipher = FileCipher::generate(Cipher::Aes256Gcm, 64 * 1024).unwrap();
    let state = ws2
        .create_upload(
            &session.server,
            &session.user_id,
            &file_cipher,
            &block("stream.bin", None),
        )
        .await
        .expect("create stream");
    let summary = ws2
        .send_stream(&state, &mut Cursor::new(&plaintext), &mut |_, _| {})
        .await
        .expect("stream");
    assert_eq!(summary.file_size, plaintext.len() as u64);
    let mut final_block = state.block.clone();
    final_block.file_size = Some(summary.file_size);
    final_block.sha256 = summary.sha256;
    let manifest = ws2
        .seal_upload(&state, &final_block)
        .await
        .expect("seal stream");
    let (info, _) = ws2.fetch_info(&manifest.mxc).await.expect("info");
    assert_eq!(info.file_size, None, "server does not know a stream's size");
    let mut out = Vec::new();
    ws2.download(&manifest, &mut out, &mut |_, _| {})
        .await
        .expect("download stream");
    assert_eq!(out, plaintext);

    // 7. 不存在的 mxc → Server NotFound。
    let mut missing = manifest.clone();
    missing.mxc = "mxc://localhost/0000000000000000".into();
    let error = ws2
        .download(&missing, &mut Vec::new(), &mut |_, _| {})
        .await
        .unwrap_err();
    assert!(matches!(error, SdkError::Server { .. }), "{error}");

    logout(&session).await.expect("logout");
    assert!(
        whoami(&session).await.is_err(),
        "token must be dead after logout"
    );
}

/// 標準 `GET /_matrix/client/v1/media/download/{server}/{id}`：分塊媒體整份給（wbfuwunel 的 /docs/design/chunked-upload-spec.md §4.2）。
async fn standard_media_download(server: &str, access_token: &str, mxc: &str) -> Vec<u8> {
    let path = mxc.strip_prefix("mxc://").expect("mxc");
    let url = format!(
        "{}/_matrix/client/v1/media/download/{path}",
        server.trim_end_matches('/')
    );
    let response = reqwest::Client::new()
        .get(url)
        .bearer_auth(access_token)
        .send()
        .await
        .expect("media download");
    assert!(
        response.status().is_success(),
        "standard download status {}",
        response.status()
    );
    response.bytes().await.expect("body").to_vec()
}

/// 讀到 `remaining` byte 之後就報 IO 錯，模擬上傳中途被殺。
struct FailAfter<R> {
    inner: R,
    remaining: usize,
}

impl<R: std::io::Read> std::io::Read for FailAfter<R> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        if self.remaining == 0 {
            return Err(std::io::Error::other("simulated kill"));
        }
        let want = buffer.len().min(self.remaining);
        let got = self.inner.read(&mut buffer[..want])?;
        self.remaining -= got;
        Ok(got)
    }
}

impl<R: std::io::Seek> std::io::Seek for FailAfter<R> {
    fn seek(&mut self, position: std::io::SeekFrom) -> std::io::Result<u64> {
        self.inner.seek(position)
    }
}

/// 走橋（#45 第 2 支）對真 server：`Hello` 宣告 `bridge` 與 `org.wbftw.device_versions`；開一個加密房，
/// `Members` 回來的房間版本號與自己的裝置版本號讀得到；發一則 to-device 給自己（走橋）被收下。
#[tokio::test]
#[ignore = "needs a running wbfuwunel; see file header"]
async fn bridge_members_and_send_to_device_against_real_server() {
    use wbf_sdk::protocol::{BRIDGE_FEATURE, DEVICE_VERSIONS_FEATURE};
    let Some(target) = target() else {
        eprintln!("WBF_E2E_* not set; skipping");
        return;
    };
    let session = login_with_password(
        &target.server,
        &target.user,
        &target.password,
        "wbf-sdk e2e bridge",
    )
    .await
    .expect("login");

    // 加密房走 HTTP 開（建房不在這一支的範圍）。
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
    let room_id = created["room_id"].as_str().expect("room_id").to_string();

    let channel = Channel::connect(&session.server, &session.access_token, Transport::WebSocket)
        .await
        .expect("ws");
    let mut ws = WbfClient::new(channel);
    let hello = ws.hello("wbf-sdk e2e bridge", &[]).await.expect("hello");
    assert!(
        hello
            .features
            .iter()
            .any(|feature| feature == BRIDGE_FEATURE),
        "{:?}",
        hello.features
    );
    assert!(
        hello
            .features
            .iter()
            .any(|feature| feature == DEVICE_VERSIONS_FEATURE),
        "{:?}",
        hello.features
    );

    let versions = ws
        .room_device_versions(&room_id)
        .await
        .expect("Members over the bridge");
    assert!(versions.room_version > 0, "{versions:?}");
    let mine = versions
        .members
        .get(&session.user_id)
        .expect("I am a joined member");
    assert!(mine.seq >= 1, "{mine:?}");
    // 再拿一次：同一個房間、沒人動 → 兩個號碼一樣（不是每次讀都跳）。
    let again = ws
        .room_device_versions(&room_id)
        .await
        .expect("Members again");
    assert_eq!(again, versions);

    // 發 to-device 給自己這台裝置；同一個 txn_id 重送也是 Ok（冪等）。
    let body = serde_json::json!({ "messages": { session.user_id.clone(): { session.device_id.clone(): { "body": "via bridge" } } } })
        .to_string()
        .into_bytes();
    ws.send_to_device("org.wbftw.e2e", "e2e-bridge-1", body.clone())
        .await
        .expect("SendToDevice");
    ws.send_to_device("org.wbftw.e2e", "e2e-bridge-1", body)
        .await
        .expect("SendToDevice again (idempotent)");

    // 不在房裡的房間：Matrix 的 403 帶 errcode 過來。
    let error = ws
        .room_device_versions("!nonexistent:localhost")
        .await
        .unwrap_err();
    assert!(
        error
            .matrix_status()
            .is_some_and(|status| status == 403 || status == 404),
        "{error:?}"
    );

    logout(&session).await.expect("logout");
}

/// 房間動作的那張端點表（`matrix_endpoint`，/docs/design/rooms/room-actions.md §2）對真 server：**同一份變數、同一份 body，走 HTTP 與走橋回的一樣**。
/// HTTP 那條是一般 Matrix 帳號的路，daemon 對本機 server 走不到（登入一定走 wbf），所以在這裡直接打：
/// 建房（`initial_state` 帶加密）→ 空的 `state_key`（URL 最後一段是空的）讀寫 → 帳號資料、標籤（path 有 `@`、`!`）→ 404 認得出來 → 退出、忘記。
#[tokio::test]
#[ignore = "needs a running wbfuwunel; see file header"]
async fn the_matrix_endpoint_table_answers_the_same_over_http_and_over_the_bridge() {
    use serde_json::{json, Map, Value};
    use wbf_sdk::matrix_endpoint::{self as endpoints, call_over_http};
    let Some(target) = target() else {
        eprintln!("WBF_E2E_* not set; skipping");
        return;
    };
    let session = login_with_password(
        &target.server,
        &target.user,
        &target.password,
        "wbf-sdk e2e endpoints",
    )
    .await
    .expect("login");
    let (server, token) = (session.server.as_str(), session.access_token.as_str());
    let vars = |value: Value| -> Map<String, Value> { value.as_object().unwrap().clone() };
    let channel = Channel::connect(server, token, Transport::WebSocket)
        .await
        .expect("ws");
    let mut bridge = WbfClient::new(channel);
    bridge
        .hello("wbf-sdk e2e endpoints", &[])
        .await
        .expect("hello");

    let body = wbf_sdk::room_state_edit::to_create_room_body(
        true,
        vars(json!({ "name": "endpoint table", "preset": "private_chat" })),
    )
    .unwrap();
    let created = call_over_http(
        server,
        token,
        &endpoints::CREATE_ROOM,
        &Map::new(),
        Some(&Value::Object(body)),
    )
    .await
    .expect("CreateRoom over http");
    let room = created["room_id"].as_str().expect("room_id").to_string();

    let encryption =
        vars(json!({ "room_id": room, "event_type": "m.room.encryption", "state_key": "" }));
    let over_http = call_over_http(
        server,
        token,
        &endpoints::GET_STATE_EVENT,
        &encryption,
        None,
    )
    .await
    .expect("GetStateEvent over http (an empty last segment)");
    let over_bridge = bridge
        .call_matrix_endpoint(&endpoints::GET_STATE_EVENT, &encryption, None)
        .await
        .expect("GetStateEvent over the bridge");
    assert_eq!(over_http, over_bridge);
    assert_eq!(
        over_http["algorithm"],
        wbf_sdk::room_state_edit::MEGOLM_ALGORITHM
    );

    let topic = vars(json!({ "room_id": room, "event_type": "m.room.topic", "state_key": "" }));
    let written = call_over_http(
        server,
        token,
        &endpoints::SET_STATE_EVENT,
        &topic,
        Some(&json!({ "topic": "hi" })),
    )
    .await
    .expect("SetStateEvent over http");
    assert!(written["event_id"].is_string(), "{written}");
    let read_back = bridge
        .call_matrix_endpoint(&endpoints::GET_STATE_EVENT, &topic, None)
        .await
        .expect("read back over the bridge");
    assert_eq!(read_back, json!({ "topic": "hi" }));

    let account_data =
        vars(json!({ "user_id": session.user_id, "room_id": room, "event_type": "org.wbftw.e2e" }));
    call_over_http(
        server,
        token,
        &endpoints::SET_ROOM_ACCOUNT_DATA,
        &account_data,
        Some(&json!({ "seen": 1 })),
    )
    .await
    .expect("SetRoomAccountData over http");
    assert_eq!(
        bridge
            .call_matrix_endpoint(&endpoints::GET_ROOM_ACCOUNT_DATA, &account_data, None)
            .await
            .expect("GetRoomAccountData over the bridge"),
        json!({ "seen": 1 })
    );
    let tag = vars(json!({ "user_id": session.user_id, "room_id": room, "tag": "u.e2e" }));
    call_over_http(
        server,
        token,
        &endpoints::SET_TAG,
        &tag,
        Some(&json!({ "order": 0.5 })),
    )
    .await
    .expect("SetTag over http");
    let tags_vars = vars(json!({ "user_id": session.user_id, "room_id": room }));
    let tags_http = call_over_http(server, token, &endpoints::GET_TAGS, &tags_vars, None)
        .await
        .expect("GetTags over http");
    let tags_bridge = bridge
        .call_matrix_endpoint(&endpoints::GET_TAGS, &tags_vars, None)
        .await
        .expect("GetTags over the bridge");
    assert_eq!(tags_http, tags_bridge);
    assert_eq!(tags_http["tags"]["u.e2e"]["order"], 0.5);

    // 沒有這一項：兩條路都是認得出來的 404。
    let avatar = vars(json!({ "room_id": room, "event_type": "m.room.avatar", "state_key": "" }));
    let missing = call_over_http(server, token, &endpoints::GET_STATE_EVENT, &avatar, None)
        .await
        .unwrap_err();
    assert!(missing.is_not_found(), "{missing:?}");
    assert!(bridge
        .call_matrix_endpoint(&endpoints::GET_STATE_EVENT, &avatar, None)
        .await
        .unwrap_err()
        .is_not_found());

    let room_only = vars(json!({ "room_id": room }));
    call_over_http(
        server,
        token,
        &endpoints::LEAVE,
        &room_only,
        Some(&json!({ "reason": "done" })),
    )
    .await
    .expect("Leave over http");
    call_over_http(server, token, &endpoints::FORGET, &room_only, None)
        .await
        .expect("Forget over http");
    let joined = call_over_http(server, token, &endpoints::JOINED_ROOMS, &Map::new(), None)
        .await
        .expect("JoinedRooms over http");
    assert!(
        joined["joined_rooms"]
            .as_array()
            .unwrap()
            .iter()
            .all(|listed| listed != room.as_str()),
        "{joined}"
    );
    logout(&session).await.expect("logout");
}

/// 同一支端點、同一份變數與 body，走 HTTP 一次、走橋一次。
async fn call_both_ways(
    session: &wbf_sdk::login::Session,
    bridge: &mut WbfClient<Channel>,
    endpoint: &wbf_sdk::matrix_endpoint::MatrixEndpoint,
    variables: &serde_json::Map<String, serde_json::Value>,
    body: Option<&serde_json::Value>,
) -> (
    Result<serde_json::Value, SdkError>,
    Result<serde_json::Value, SdkError>,
) {
    let over_http = wbf_sdk::matrix_endpoint::call_over_http(
        &session.server,
        &session.access_token,
        endpoint,
        variables,
        body,
    )
    .await;
    let over_bridge = bridge.call_matrix_endpoint(endpoint, variables, body).await;
    (over_http, over_bridge)
}

/// 兩條路的結果一樣：都成功就 body 一樣，都失敗就 HTTP 狀態一樣。回 HTTP 那份。
fn assert_same_outcome(
    name: &str,
    (over_http, over_bridge): (
        Result<serde_json::Value, SdkError>,
        Result<serde_json::Value, SdkError>,
    ),
) -> Result<serde_json::Value, SdkError> {
    match (&over_http, &over_bridge) {
        (Ok(http), Ok(bridge)) => assert_eq!(http, bridge, "{name}"),
        (Err(http), Err(bridge)) => assert_eq!(
            http.matrix_status(),
            bridge.matrix_status(),
            "{name}: {http:?} / {bridge:?}"
        ),
        _ => panic!("{name}: http {over_http:?} but bridge {over_bridge:?}"),
    }
    over_http
}

/// 批 3／批 4 其餘的 30 支端點（/docs/design/rpc-specs/matrix-relay.md §2）對真 server：**走 HTTP 與走橋回的一樣**，寫的那幾支一條路寫、另一條路讀回來。
/// 時間會動的欄位（在線狀態的 `last_active_ago`）只比不會動的那幾個。
#[tokio::test]
#[ignore = "needs a running wbfuwunel; see file header"]
async fn the_batch_3_and_4_endpoints_answer_the_same_over_http_and_over_the_bridge() {
    use serde_json::{json, Map, Value};
    use wbf_sdk::matrix_endpoint::{self as endpoints, call_over_http};
    let Some(target) = target() else {
        eprintln!("WBF_E2E_* not set; skipping");
        return;
    };
    let session = login_with_password(
        &target.server,
        &target.user,
        &target.password,
        "wbf-sdk e2e batch 3 and 4",
    )
    .await
    .expect("login");
    let (server, token) = (session.server.as_str(), session.access_token.as_str());
    let vars = |value: Value| -> Map<String, Value> { value.as_object().unwrap().clone() };
    let channel = Channel::connect(server, token, Transport::WebSocket)
        .await
        .expect("ws");
    let mut bridge = WbfClient::new(channel);
    bridge
        .hello("wbf-sdk e2e batch 3 and 4", &[])
        .await
        .expect("hello");
    let none = Map::new();

    // 能力、媒體設定、推播規則、pusher：純讀，契約是**成功**，兩條路一樣，而且長得像規格（兩條路一起失敗也要紅，PR #86 rumia）
    let mut read_both_ways = Vec::new();
    for endpoint in [
        &endpoints::CAPABILITIES,
        &endpoints::MEDIA_CONFIG,
        &endpoints::GET_PUSH_RULES,
        &endpoints::GET_GLOBAL_PUSH_RULES,
        &endpoints::GET_PUSHERS,
    ] {
        let outcome = call_both_ways(&session, &mut bridge, endpoint, &none, None).await;
        read_both_ways.push(assert_same_outcome(endpoint.name, outcome).expect(endpoint.name));
    }
    let [capabilities, media_config, rules, global_rules, pushers] = &read_both_ways[..] else {
        panic!("five reads");
    };
    assert!(capabilities["capabilities"].is_object(), "{capabilities}");
    assert!(media_config["m.upload.size"].is_u64(), "{media_config}");
    assert!(rules["global"]["override"].is_array(), "{rules}");
    assert_eq!(global_rules, &rules["global"]);
    assert!(pushers["pushers"].is_array(), "{pushers}");
    // TURN：這台沒設定是 404，兩條路一起 404 才是對的
    let outcome = call_both_ways(&session, &mut bridge, &endpoints::TURN_SERVER, &none, None).await;
    if let Err(error) = assert_same_outcome("TurnServer", outcome) {
        assert!(error.is_not_found(), "{error:?}");
    }
    let notifications = vars(json!({ "limit": 5 }));
    let outcome = call_both_ways(
        &session,
        &mut bridge,
        &endpoints::NOTIFICATIONS,
        &notifications,
        None,
    )
    .await;
    assert!(
        assert_same_outcome("Notifications", outcome).expect("Notifications")["notifications"]
            .is_array()
    );

    // 一間房、一則狀態事件，給關聯、討論串、跳到某天、檢舉用
    let created = call_over_http(
        server,
        token,
        &endpoints::CREATE_ROOM,
        &none,
        Some(&json!({ "name": "batch 3 and 4", "preset": "private_chat" })),
    )
    .await
    .expect("CreateRoom");
    let room = created["room_id"].as_str().expect("room_id").to_string();
    let topic = call_over_http(
        server,
        token,
        &endpoints::SET_STATE_EVENT,
        &vars(json!({ "room_id": room, "event_type": "m.room.topic", "state_key": "" })),
        Some(&json!({ "topic": "relations" })),
    )
    .await
    .expect("SetStateEvent");
    let event_id = topic["event_id"].as_str().expect("event_id").to_string();

    // 推播規則：HTTP 寫、橋讀；橋改、HTTP 讀；刪了兩條路都是 404
    let rule = vars(json!({ "kind": "room", "rule_id": room }));
    call_over_http(
        server,
        token,
        &endpoints::SET_PUSH_RULE,
        &rule,
        Some(&json!({ "actions": [] })),
    )
    .await
    .expect("SetPushRule over http");
    let outcome = call_both_ways(
        &session,
        &mut bridge,
        &endpoints::GET_PUSH_RULE,
        &rule,
        None,
    )
    .await;
    let read = assert_same_outcome("GetPushRule", outcome).expect("GetPushRule");
    assert_eq!(read["rule_id"], room.as_str(), "{read}");
    assert_eq!(read["actions"], json!([]), "{read}");
    bridge
        .call_matrix_endpoint(
            &endpoints::SET_PUSH_RULE_ENABLED,
            &rule,
            Some(&json!({ "enabled": false })),
        )
        .await
        .expect("SetPushRuleEnabled over the bridge");
    let outcome = call_both_ways(
        &session,
        &mut bridge,
        &endpoints::GET_PUSH_RULE_ENABLED,
        &rule,
        None,
    )
    .await;
    assert_eq!(
        assert_same_outcome("GetPushRuleEnabled", outcome).expect("GetPushRuleEnabled"),
        json!({ "enabled": false })
    );
    bridge
        .call_matrix_endpoint(
            &endpoints::SET_PUSH_RULE_ACTIONS,
            &rule,
            Some(&json!({ "actions": ["notify"] })),
        )
        .await
        .expect("SetPushRuleActions over the bridge");
    let outcome = call_both_ways(
        &session,
        &mut bridge,
        &endpoints::GET_PUSH_RULE_ACTIONS,
        &rule,
        None,
    )
    .await;
    assert_eq!(
        assert_same_outcome("GetPushRuleActions", outcome).expect("GetPushRuleActions"),
        json!({ "actions": ["notify"] })
    );
    bridge
        .call_matrix_endpoint(&endpoints::DELETE_PUSH_RULE, &rule, None)
        .await
        .expect("DeletePushRule over the bridge");
    let outcome = call_both_ways(
        &session,
        &mut bridge,
        &endpoints::GET_PUSH_RULE,
        &rule,
        None,
    )
    .await;
    let gone = assert_same_outcome("GetPushRule after delete", outcome).unwrap_err();
    assert!(gone.is_not_found(), "{gone:?}");

    // pusher：HTTP 加、橋看得到；`kind: null` 移除
    let pusher = json!({
        "pushkey": "e2e-pushkey", "app_id": "org.wbftw.e2e", "kind": "http",
        "app_display_name": "e2e", "device_display_name": "e2e", "lang": "en",
        "data": { "url": "https://push.example.org/_matrix/push/v1/notify" }
    });
    call_over_http(server, token, &endpoints::SET_PUSHER, &none, Some(&pusher))
        .await
        .expect("SetPusher over http");
    let listed = bridge
        .call_matrix_endpoint(&endpoints::GET_PUSHERS, &none, None)
        .await
        .expect("GetPushers over the bridge");
    assert!(
        listed["pushers"]
            .as_array()
            .unwrap()
            .iter()
            .any(|one| one["pushkey"] == "e2e-pushkey"),
        "{listed}"
    );
    bridge
        .call_matrix_endpoint(
            &endpoints::SET_PUSHER,
            &none,
            Some(&json!({ "pushkey": "e2e-pushkey", "app_id": "org.wbftw.e2e", "kind": null })),
        )
        .await
        .expect("SetPusher kind null over the bridge");
    let listed = call_over_http(server, token, &endpoints::GET_PUSHERS, &none, None)
        .await
        .expect("GetPushers over http");
    assert!(
        listed["pushers"]
            .as_array()
            .unwrap()
            .iter()
            .all(|one| one["pushkey"] != "e2e-pushkey"),
        "{listed}"
    );

    // 過濾器：路徑的 user_id 是自己；HTTP 建、兩條路讀回一樣
    let me = vars(json!({ "user_id": session.user_id }));
    let filter = call_over_http(
        server,
        token,
        &endpoints::CREATE_FILTER,
        &me,
        Some(&json!({ "room": { "timeline": { "limit": 7 } } })),
    )
    .await
    .expect("CreateFilter");
    let filter_id = filter["filter_id"].as_str().expect("filter_id").to_string();
    let filter_vars = vars(json!({ "user_id": session.user_id, "filter_id": filter_id }));
    let outcome = call_both_ways(
        &session,
        &mut bridge,
        &endpoints::GET_FILTER,
        &filter_vars,
        None,
    )
    .await;
    let read = assert_same_outcome("GetFilter", outcome).expect("GetFilter");
    assert_eq!(read["room"]["timeline"]["limit"], 7, "{read}");

    // 在線狀態：橋寫、兩條路讀（`last_active_ago` 會動，只比 presence 與 status_msg）
    bridge
        .call_matrix_endpoint(
            &endpoints::SET_PRESENCE,
            &me,
            Some(&json!({ "presence": "online", "status_msg": "e2e" })),
        )
        .await
        .expect("SetPresence over the bridge");
    let (over_http, over_bridge) =
        call_both_ways(&session, &mut bridge, &endpoints::GET_PRESENCE, &me, None).await;
    let (over_http, over_bridge) = (
        over_http.expect("GetPresence over http"),
        over_bridge.expect("GetPresence over the bridge"),
    );
    for field in ["presence", "status_msg"] {
        assert_eq!(over_http[field], over_bridge[field], "{field}");
    }
    assert_eq!(over_http["status_msg"], "e2e", "{over_http}");

    // 關聯、討論串、跳到某天、搜尋
    let relations = vars(json!({ "room_id": room, "event_id": event_id, "dir": "b", "limit": 5 }));
    let outcome = call_both_ways(
        &session,
        &mut bridge,
        &endpoints::RELATIONS,
        &relations,
        None,
    )
    .await;
    assert!(assert_same_outcome("Relations", outcome).expect("Relations")["chunk"].is_array());
    let by_type =
        vars(json!({ "room_id": room, "event_id": event_id, "rel_type": "m.annotation" }));
    let outcome = call_both_ways(
        &session,
        &mut bridge,
        &endpoints::RELATIONS_BY_REL_TYPE,
        &by_type,
        None,
    )
    .await;
    assert_same_outcome("RelationsByRelType", outcome).expect("RelationsByRelType");
    let by_both = vars(
        json!({ "room_id": room, "event_id": event_id, "rel_type": "m.annotation", "event_type": "m.reaction" }),
    );
    let outcome = call_both_ways(
        &session,
        &mut bridge,
        &endpoints::RELATIONS_BY_REL_TYPE_AND_EVENT_TYPE,
        &by_both,
        None,
    )
    .await;
    assert_same_outcome("RelationsByRelTypeAndEventType", outcome)
        .expect("RelationsByRelTypeAndEventType");
    let threads = vars(json!({ "room_id": room, "limit": 5 }));
    let outcome = call_both_ways(&session, &mut bridge, &endpoints::THREADS, &threads, None).await;
    assert!(assert_same_outcome("Threads", outcome).expect("Threads")["chunk"].is_array());
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let at = vars(json!({ "room_id": room, "ts": now_ms + 60_000, "dir": "b" }));
    let outcome = call_both_ways(
        &session,
        &mut bridge,
        &endpoints::TIMESTAMP_TO_EVENT,
        &at,
        None,
    )
    .await;
    let found = assert_same_outcome("TimestampToEvent", outcome).expect("TimestampToEvent");
    assert!(found["event_id"].is_string(), "{found}");
    let localpart = session
        .user_id
        .trim_start_matches('@')
        .split(':')
        .next()
        .unwrap()
        .to_string();
    let outcome = call_both_ways(
        &session,
        &mut bridge,
        &endpoints::SEARCH_USERS,
        &none,
        Some(&json!({ "search_term": localpart, "limit": 5 })),
    )
    .await;
    assert!(
        assert_same_outcome("SearchUsers", outcome).expect("SearchUsers")["results"].is_array()
    );
    let outcome = call_both_ways(
        &session,
        &mut bridge,
        &endpoints::SEARCH_EVENTS,
        &none,
        Some(&json!({ "search_categories": { "room_events": { "search_term": "relations", "filter": { "rooms": [room] } } } })),
    )
    .await;
    assert_same_outcome("SearchEvents", outcome).expect("SearchEvents");

    // 連結預覽：這台可能關著（403）或連不出去；兩條路要給同一個結果
    let preview = vars(json!({ "url": "http://127.0.0.1:9/" }));
    let outcome = call_both_ways(
        &session,
        &mut bridge,
        &endpoints::MEDIA_PREVIEW,
        &preview,
        None,
    )
    .await;
    assert_same_outcome("MediaPreview", outcome).ok();

    // 檢舉：兩條路各送一次，結果一樣
    let event = vars(json!({ "room_id": room, "event_id": event_id }));
    let outcome = call_both_ways(
        &session,
        &mut bridge,
        &endpoints::REPORT_EVENT,
        &event,
        Some(&json!({ "reason": "e2e" })),
    )
    .await;
    assert_same_outcome("ReportEvent", outcome).expect("ReportEvent");
    let room_only = vars(json!({ "room_id": room }));
    let outcome = call_both_ways(
        &session,
        &mut bridge,
        &endpoints::REPORT_ROOM,
        &room_only,
        Some(&json!({ "reason": "e2e" })),
    )
    .await;
    assert_same_outcome("ReportRoom", outcome).expect("ReportRoom");
    let nobody = vars(json!({ "user_id": "@nobody-e2e:localhost" }));
    let outcome = call_both_ways(
        &session,
        &mut bridge,
        &endpoints::REPORT_USER,
        &nobody,
        Some(&json!({ "reason": "e2e" })),
    )
    .await;
    assert_same_outcome("ReportUser", outcome).ok();

    call_over_http(server, token, &endpoints::LEAVE, &room_only, None)
        .await
        .expect("Leave");
    call_over_http(server, token, &endpoints::FORGET, &room_only, None)
        .await
        .expect("Forget");
    logout(&session).await.expect("logout");
}

/// 心跳對真 server（/docs/design/daemon/ws-receive-dispatch.md §5.1）：安靜的線每秒跳一次，Pong 經會話表回來、也過鉤子；三秒內至少兩個 Pong、線還開著、沒有無主。
/// 📎 server 的 60 秒 idle（wbfuwunel #103，2026-10-02）不在這裡驗（太久）；這條驗的是「Ping 送得出去、Pong 對得回來」那條路在真 server 上通。
#[tokio::test]
#[ignore = "needs a running wbfuwunel; see file header"]
async fn the_heartbeat_keeps_a_quiet_line_alive_against_the_real_server() {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;
    use wbf_sdk::channel::WsChannel;
    use wbf_sdk::{Heartbeat, Received};

    let Some(target) = target() else {
        eprintln!("WBF_E2E_* not set; skipping");
        return;
    };
    let session = login_with_password(
        &target.server,
        &target.user,
        &target.password,
        "wbf-sdk e2e heartbeat",
    )
    .await
    .expect("login");
    let pongs = Arc::new(AtomicU64::new(0));
    let counter = pongs.clone();
    let hook: wbf_sdk::ReceivedHook = Arc::new(move |received: &Received<'_>| {
        if received.pack.kind == wbf_wire::Kind::Control
            && received.pack.subtype == wbf_wire::pack::control::PONG
        {
            counter.fetch_add(1, Ordering::SeqCst);
        }
    });
    let channel = WsChannel::connect_with_heartbeat(
        &session.server,
        &session.access_token,
        hook,
        Heartbeat {
            interval: std::time::Duration::from_secs(1),
            reply_timeout: std::time::Duration::from_secs(5),
        },
    )
    .await
    .expect("ws");
    let mut client = WbfClient::new(Channel::WebSocket(Box::new(channel)));
    client
        .hello("wbf-sdk e2e heartbeat", &[])
        .await
        .expect("hello");
    tokio::time::sleep(std::time::Duration::from_millis(3500)).await;
    let pongs = pongs.load(Ordering::SeqCst);
    assert!(pongs >= 2, "3.5 秒、每秒一跳，至少兩個 Pong：{pongs}");
    assert!(!client.channel().is_closed(), "線還開著");
    assert_eq!(client.channel().unmatched(), Some(0), "每個 Pong 都有人等");
    // 線還能用。
    client.ping().await.expect("ping after heartbeats");
    logout(&session).await.expect("logout");
}

/// 一般 Matrix 帳號那條路（matrix-sdk 的 Client）只把 state 與 crypto 落地（維護者 2026-09-30，/docs/design/storage/local-cache-db.md §4.1）：
/// wbfuwunel 也是一台標準 Matrix server，所以直接用 `MatrixBackend::login` 走得到（登入流程本身會探活走 wbf、碰不到這條）。
/// 登入、同步、列房間都照常，store 目錄裡只有 `matrix-sdk-state` 與 `matrix-sdk-crypto` 兩個 sqlite，**沒有** event cache 與 media。
#[cfg(feature = "matrix")]
#[tokio::test]
#[ignore = "needs a running wbfuwunel; see file header"]
async fn the_matrix_sdk_client_keeps_only_state_and_crypto_on_disk() {
    use wbf_sdk::chat::ChatBackend;
    let Some(target) = target() else {
        eprintln!("skipped: WBF_E2E_SERVER / WBF_E2E_USER / WBF_E2E_PASSWORD_FILE not set");
        return;
    };
    let store_dir = std::env::temp_dir().join(format!("wbf-e2e-m-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&store_dir);
    let key = wbf_sdk::vault::Key32([5u8; 32]);
    let (backend, session) = wbf_sdk::backend::matrix_sdk::MatrixBackend::login(
        &target.server,
        &target.user,
        &target.password,
        "wbf-e2e-two-stores",
        &store_dir,
        &key,
        false,
    )
    .await
    .expect("matrix-sdk login");
    backend
        .sync_once(None, std::time::Duration::ZERO)
        .await
        .expect("sync_once");
    backend.conversations().await.expect("conversations");
    let mut stores: Vec<String> = std::fs::read_dir(&store_dir)
        .unwrap()
        .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
        .filter(|name| name.ends_with(".sqlite3"))
        .collect();
    stores.sort();
    assert_eq!(
        stores,
        vec![
            "matrix-sdk-crypto.sqlite3".to_string(),
            "matrix-sdk-state.sqlite3".to_string()
        ],
        "只有 state 與 crypto 落地"
    );
    drop(backend);
    logout(&session).await.expect("logout");
    let _ = std::fs::remove_dir_all(&store_dir);
}

/// 一般 Matrix 帳號的傳統上傳（/docs/design/rpc-specs/data-plane.md §7.2），走 matrix-sdk 的 Client 那條路（本機 wbfuwunel 登入一定走 wbf，
/// 所以直接 `MatrixBackend::login`）：明文房、加密房各傳一個（上限、`/create`、串流 `/upload`）、當標準附件送出（加密房 matrix-sdk 用 Megolm）、
/// 歷史裡讀回來是同一個檔，再照事件下載回來、整檔一樣（加密的 hash 對得上）。
/// 另要 `WBF_E2E_ROOM`（明文房）與 `WBF_E2E_ENCRYPTED_ROOM`（加密房），帳號都已經加入。
#[tokio::test]
#[ignore]
async fn a_traditional_upload_goes_into_plain_and_encrypted_rooms_and_downloads_back() {
    use wbf_sdk::chat::{ChatBackend, MatrixAttachment};
    use wbf_sdk::event_json::{matrix_file_message_content, messages_from_incoming};
    use wbf_sdk::matrix_media::{
        create_matrix_media, get_upload_size_limit, stream_matrix_media, upload_matrix_media,
    };
    use wbf_sdk::media_kind::{MediaKind, Verification};
    let (Some(target), Some(plain_room), Some(encrypted_room)) =
        (target(), env("WBF_E2E_ROOM"), env("WBF_E2E_ENCRYPTED_ROOM"))
    else {
        eprintln!("skipped: WBF_E2E_SERVER / USER / PASSWORD_FILE / ROOM / ENCRYPTED_ROOM not set");
        return;
    };
    let store_dir = std::env::temp_dir().join(format!("wbf-e2e-up-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&store_dir);
    let key = wbf_sdk::vault::Key32([6u8; 32]);
    let (backend, session) = wbf_sdk::backend::matrix_sdk::MatrixBackend::login(
        &target.server,
        &target.user,
        &target.password,
        "wbf-e2e-traditional-upload",
        &store_dir,
        &key,
        false,
    )
    .await
    .expect("matrix-sdk login");
    backend
        .sync_once(None, std::time::Duration::ZERO)
        .await
        .expect("sync_once");
    let limit = get_upload_size_limit(&session.server, &session.access_token)
        .await
        .expect("media config");
    eprintln!("m.upload.size = {limit:?}");
    let body: Vec<u8> = (0..150_003u32)
        .map(|position| (position % 249) as u8)
        .collect();
    for (room, encrypted) in [(&plain_room, false), (&encrypted_room, true)] {
        assert_eq!(
            backend
                .is_room_encrypted(room)
                .await
                .expect("encryption state"),
            encrypted,
            "{room}"
        );
        let mxc = create_matrix_media(&session.server, &session.access_token)
            .await
            .expect("media create");
        let upload = wbf_sdk::MatrixUpload {
            server: session.server.clone(),
            user_id: session.user_id.clone(),
            mxc: mxc.clone(),
            name: "traditional.bin".into(),
            mimetype: Some("application/octet-stream".into()),
            size: body.len() as u64,
            encrypted,
        };
        let manifest = upload_matrix_media(
            &session.server,
            &session.access_token,
            &upload,
            &mut &body[..],
        )
        .await
        .expect("upload");
        let content = matrix_file_message_content(&manifest, Some("e2e")).expect("content");
        let event_id = backend
            .send_event_content(room, "m.room.message", content, None)
            .await
            .expect("send");
        // 歷史裡讀回來：標準附件、同一個 mxc、同一種格式（加密房是 matrix-sdk 解開自己送的那則）。
        let page = backend.history(room, None, 10).await.expect("history");
        let messages = messages_from_incoming(room, &session.user_id, &page.events);
        let sent = messages
            .iter()
            .find(|message| message.id == event_id)
            .unwrap_or_else(|| panic!("{event_id} is in the history of {room}"));
        let attachment: MatrixAttachment = match &sent.kind {
            wbf_sdk::MessageKind::MatrixFile {
                attachment,
                caption,
            } => {
                assert_eq!(caption.as_deref(), Some("e2e"));
                attachment.clone()
            }
            other => panic!("{room}: {other:?}"),
        };
        let kind = match encrypted {
            true => MediaKind::MatrixEncrypted,
            false => MediaKind::MatrixPlain,
        };
        assert_eq!(
            (attachment.mxc.as_str(), attachment.kind),
            (mxc.as_str(), kind)
        );
        // 照事件下載回來。
        let (sink, mut pieces) = tokio::sync::mpsc::channel(4);
        let collecting = tokio::spawn(async move {
            let mut all = Vec::new();
            while let Some(piece) = pieces.recv().await {
                all.extend(piece);
            }
            all
        });
        let end = stream_matrix_media(&session.server, &session.access_token, &attachment, &sink)
            .await
            .expect("download");
        drop(sink);
        assert_eq!(
            collecting.await.unwrap(),
            body,
            "{room}: the same bytes came back"
        );
        let verified = match encrypted {
            true => Verification::Matched,
            false => Verification::Unknown,
        };
        assert_eq!(end.verified, verified, "{room}");
    }
    drop(backend);
    logout(&session).await.expect("logout");
    let _ = std::fs::remove_dir_all(&store_dir);
}

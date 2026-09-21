//! 對著真的 wbfuwunel 跑（plan-v1 §4 的驗收表，SDK 層那幾條）。平常 `#[ignore]`；要跑：
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
        let file_cipher = FileCipher::generate(cipher, 64 * 1024);
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
    let file_cipher = FileCipher::generate(Cipher::ChaCha20Poly1305, 64 * 1024);
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
    let file_cipher = FileCipher::generate(Cipher::Aes256Gcm, 64 * 1024);
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

/// 標準 `GET /_matrix/client/v1/media/download/{server}/{id}`：分塊媒體整份給（線上規格 §4.2）。
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

/// 心跳對真 server（ws-receive-dispatch.md §5.1）：安靜的線每秒跳一次，Pong 經會話表回來、也過鉤子；三秒內至少兩個 Pong、線還開著、沒有無主。
/// 📎 300 秒的 idle 不在這裡驗（太久）；這條驗的是「Ping 送得出去、Pong 對得回來」那條路在真 server 上通。
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
            quiet: std::time::Duration::from_millis(500),
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

//! 傳統 Matrix 附件的下載與串流解密（/docs/design/media/media-download.md §12），對著一個本機的假 HTTP server。
//! 加密用上游的 `AttachmentEncryptor`（就是 Element 這類 client 的格式），解密走我們的 `stream_matrix_media`。
#![cfg(feature = "matrix")]

use std::io::Read;
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use wbf_sdk::chat::MatrixAttachment;
use wbf_sdk::matrix_media::stream_matrix_media;
use wbf_sdk::media_kind::{MediaKind, Verification};
use wbf_sdk::SdkError;

/// 一個只回固定內容的 HTTP server：`v1` 的路徑可以設成回 404 M_UNRECOGNIZED（舊 server）。記下每個請求的路徑與 Authorization。
struct FakeMedia {
    base: String,
    seen: Arc<Mutex<Vec<(String, String)>>>,
}

async fn serve(body: Vec<u8>, v1_unrecognized: bool) -> FakeMedia {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let seen_here = seen.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let body = body.clone();
            let seen = seen_here.clone();
            tokio::spawn(async move {
                let mut head = Vec::new();
                let mut byte = [0u8; 1];
                while !head.ends_with(b"\r\n\r\n") {
                    if socket.read(&mut byte).await.unwrap_or(0) == 0 {
                        return;
                    }
                    head.push(byte[0]);
                }
                let head = String::from_utf8_lossy(&head).to_string();
                let path = head.split(' ').nth(1).unwrap_or("").to_string();
                let auth = head
                    .lines()
                    .find_map(|line| {
                        line.strip_prefix("authorization: ")
                            .or_else(|| line.strip_prefix("Authorization: "))
                    })
                    .unwrap_or("")
                    .to_string();
                seen.lock().unwrap().push((path.clone(), auth));
                let reply = if v1_unrecognized && path.contains("/_matrix/client/v1/") {
                    let error = br#"{"errcode":"M_UNRECOGNIZED","error":"Unrecognized request"}"#;
                    let mut reply = format!(
                        "HTTP/1.1 404 Not Found\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        error.len()
                    )
                    .into_bytes();
                    reply.extend_from_slice(error);
                    reply
                } else {
                    let mut reply = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    )
                    .into_bytes();
                    reply.extend_from_slice(&body);
                    reply
                };
                let _ = socket.write_all(&reply).await;
                let _ = socket.shutdown().await;
            });
        }
    });
    FakeMedia { base, seen }
}

fn sample(len: usize) -> Vec<u8> {
    (0..len).map(|index| (index * 7 + 3) as u8).collect()
}

/// 用上游的加密器加密，回（密文、事件裡的 `file`）。
fn encrypt(plain: &[u8], mxc: &str) -> (Vec<u8>, serde_json::Value) {
    let mut source = plain;
    let mut encryptor = matrix_sdk_crypto::AttachmentEncryptor::new(&mut source);
    let mut cipher = Vec::new();
    encryptor.read_to_end(&mut cipher).unwrap();
    let mut file = serde_json::to_value(encryptor.finish()).unwrap();
    file["url"] = serde_json::json!(mxc);
    (cipher, file)
}

fn attachment(
    kind: MediaKind,
    size: Option<u64>,
    file: Option<serde_json::Value>,
) -> MatrixAttachment {
    MatrixAttachment {
        msgtype: "m.file".into(),
        mxc: "mxc://localhost/AbCdEf".into(),
        kind,
        name: Some("a.bin".into()),
        mimetype: None,
        size,
        file,
    }
}

/// 全部收下來；channel 小，邊收邊讀才會有背壓。
async fn download(
    server: &FakeMedia,
    attachment: &MatrixAttachment,
) -> (
    Result<wbf_sdk::matrix_media::MatrixDownloadEnd, SdkError>,
    Vec<u8>,
) {
    let (sink, mut pieces) = mpsc::channel(2);
    let collecting = tokio::spawn(async move {
        let mut all = Vec::new();
        while let Some(piece) = pieces.recv().await {
            all.extend(piece);
        }
        all
    });
    let end = stream_matrix_media(&server.base, "secret-token", attachment, &sink).await;
    drop(sink);
    (end, collecting.await.unwrap())
}

#[tokio::test]
async fn an_encrypted_attachment_streams_out_decrypted_and_matches_its_hash() {
    let plain = sample(300_000);
    let (cipher, file) = encrypt(&plain, "mxc://localhost/AbCdEf");
    let server = serve(cipher, false).await;
    let attached = attachment(
        MediaKind::MatrixEncrypted,
        Some(plain.len() as u64),
        Some(file),
    );
    let (end, got) = download(&server, &attached).await;
    let end = end.unwrap();
    assert_eq!(got, plain);
    assert_eq!(
        (end.verified, end.plain_len),
        (Verification::Matched, plain.len() as u64)
    );
    let seen = server.seen.lock().unwrap().clone();
    assert_eq!(
        seen,
        vec![(
            "/_matrix/client/v1/media/download/localhost/AbCdEf".to_string(),
            "Bearer secret-token".to_string()
        )],
        "the authenticated endpoint, with the token"
    );
}

/// AES-CTR 擋不住竄改：翻一個密文 bit，明文同一個 bit 跟著翻、解密照樣「成功」，資料照交；只有讀完比 hash 才知道（Mismatched）。
#[tokio::test]
async fn a_flipped_bit_is_handed_out_and_caught_only_by_the_hash_at_the_end() {
    let plain = sample(100_000);
    let (mut cipher, file) = encrypt(&plain, "mxc://localhost/AbCdEf");
    cipher[5000] ^= 0x01;
    let server = serve(cipher, false).await;
    let attached = attachment(
        MediaKind::MatrixEncrypted,
        Some(plain.len() as u64),
        Some(file),
    );
    let (end, got) = download(&server, &attached).await;
    assert_eq!(end.unwrap().verified, Verification::Mismatched);
    assert_eq!(got.len(), plain.len(), "the data is given either way");
    assert_eq!(
        got[5000],
        plain[5000] ^ 0x01,
        "the same bit flipped in the plaintext"
    );
    assert_eq!(got[..5000], plain[..5000]);
}

#[tokio::test]
async fn a_plain_attachment_has_nothing_to_verify() {
    let plain = sample(70_000);
    let server = serve(plain.clone(), false).await;
    let (end, got) = download(&server, &attachment(MediaKind::MatrixPlain, None, None)).await;
    assert_eq!(end.unwrap().verified, Verification::Unknown);
    assert_eq!(got, plain);
}

#[tokio::test]
async fn a_size_that_is_not_what_the_event_said_is_an_integrity_error() {
    let plain = sample(10_000);
    let server = serve(plain.clone(), false).await;
    let (end, _) = download(
        &server,
        &attachment(MediaKind::MatrixPlain, Some(9_999), None),
    )
    .await;
    assert!(matches!(end, Err(SdkError::Integrity(_))), "{end:?}");
}

/// 舊 server 沒有驗證過的端點（回 404 M_UNRECOGNIZED）：退到 `/_matrix/media/v3/download`。
#[tokio::test]
async fn an_old_server_falls_back_to_the_legacy_endpoint() {
    let plain = sample(1_000);
    let server = serve(plain.clone(), true).await;
    let (end, got) = download(&server, &attachment(MediaKind::MatrixPlain, None, None)).await;
    end.unwrap();
    assert_eq!(got, plain);
    let paths: Vec<String> = server
        .seen
        .lock()
        .unwrap()
        .iter()
        .map(|(path, _)| path.clone())
        .collect();
    assert_eq!(
        paths,
        vec![
            "/_matrix/client/v1/media/download/localhost/AbCdEf".to_string(),
            "/_matrix/media/v3/download/localhost/AbCdEf".to_string()
        ]
    );
}

/// 加密描述不完整（不是 v2、少 hash）：🚫 試著解。
#[tokio::test]
async fn an_encrypted_attachment_whose_description_is_broken_is_refused() {
    let plain = sample(1_000);
    let (cipher, mut file) = encrypt(&plain, "mxc://localhost/AbCdEf");
    file["v"] = serde_json::json!("v1");
    let server = serve(cipher, false).await;
    let attached = attachment(MediaKind::MatrixEncrypted, None, Some(file));
    let (end, got) = download(&server, &attached).await;
    assert!(matches!(end, Err(SdkError::Integrity(_))), "{end:?}");
    assert!(got.is_empty(), "nothing is handed out");
}

// ── 上傳（/docs/design/rpc-specs/data-plane.md §7.2）──

/// 假 server 收到的一個請求。
#[derive(Clone, Debug)]
struct Received {
    method: String,
    path: String,
    /// 標頭名一律小寫
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Received {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }
}

/// 一台會收上傳的假 server：`config` 回 `m.upload.size`（`v1_unrecognized` 時 v1 回 404 M_UNRECOGNIZED）、`create` 回 `content_uri`、
/// `PUT /upload` 照 `Content-Length` 收 body；`upload_reply` 給了就用它回 PUT（例 409 M_CANNOT_OVERWRITE_MEDIA）。
struct FakeUpload {
    base: String,
    received: Arc<Mutex<Vec<Received>>>,
}

async fn serve_upload(
    limit: Option<u64>,
    content_uri: &'static str,
    v1_unrecognized: bool,
    upload_reply: Option<(u16, &'static str)>,
) -> FakeUpload {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let received = Arc::new(Mutex::new(Vec::new()));
    let received_here = received.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let received = received_here.clone();
            tokio::spawn(async move {
                let mut head = Vec::new();
                let mut byte = [0u8; 1];
                while !head.ends_with(b"\r\n\r\n") {
                    if socket.read(&mut byte).await.unwrap_or(0) == 0 {
                        return;
                    }
                    head.push(byte[0]);
                }
                let head = String::from_utf8_lossy(&head).to_string();
                let mut lines = head.lines();
                let mut first = lines.next().unwrap_or("").split(' ');
                let method = first.next().unwrap_or("").to_string();
                let path = first.next().unwrap_or("").to_string();
                let headers: Vec<(String, String)> = lines
                    .filter_map(|line| line.split_once(": "))
                    .map(|(key, value)| (key.to_ascii_lowercase(), value.to_string()))
                    .collect();
                let length: usize = headers
                    .iter()
                    .find(|(key, _)| key == "content-length")
                    .and_then(|(_, value)| value.parse().ok())
                    .unwrap_or(0);
                // 讀到 Content-Length 或斷線為止（client 中斷時收到的會比較短）。
                let mut body = vec![0u8; length];
                let mut filled = 0;
                while filled < length {
                    match socket.read(&mut body[filled..]).await {
                        Ok(0) | Err(_) => break,
                        Ok(got) => filled += got,
                    }
                }
                body.truncate(filled);
                received.lock().unwrap().push(Received {
                    method: method.clone(),
                    path: path.clone(),
                    headers,
                    body,
                });
                let (status, json) = if path.contains("/config") {
                    if v1_unrecognized && path.contains("/_matrix/client/v1/") {
                        (
                            404,
                            r#"{"errcode":"M_UNRECOGNIZED","error":"Unrecognized request"}"#
                                .to_string(),
                        )
                    } else {
                        let limit = limit
                            .map(|limit| format!(r#""m.upload.size":{limit}"#))
                            .unwrap_or_default();
                        (200, format!("{{{limit}}}"))
                    }
                } else if path.contains("/create") {
                    (
                        200,
                        format!(r#"{{"content_uri":"{content_uri}","unused_expires_at":1}}"#),
                    )
                } else {
                    match upload_reply {
                        Some((status, json)) => (status, json.to_string()),
                        None => (200, "{}".to_string()),
                    }
                };
                let reply = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{json}",
                    json.len()
                );
                let _ = socket.write_all(reply.as_bytes()).await;
                let _ = socket.shutdown().await;
            });
        }
    });
    FakeUpload { base, received }
}

fn new_upload(size: u64, encrypted: bool) -> wbf_sdk::MatrixUpload {
    wbf_sdk::MatrixUpload {
        server: "http://unused".into(),
        user_id: "@a:localhost".into(),
        mxc: "mxc://localhost/NewMedia".into(),
        name: "cat.png".into(),
        mimetype: Some("image/png".into()),
        size,
        encrypted,
    }
}

fn the_put(server: &FakeUpload) -> Received {
    server
        .received
        .lock()
        .unwrap()
        .iter()
        .find(|request| request.method == "PUT")
        .cloned()
        .expect("a PUT reached the server")
}

/// 加密上傳：server 收到的是密文、長度先講好（🚫 chunked）、型別是不透明的；manifest 的 `file` 用上游的解密器解得回原檔、hash 對得上。
#[tokio::test]
async fn an_encrypted_upload_streams_ciphertext_that_the_upstream_decryptor_opens() {
    let plain = sample(300_001);
    let server = serve_upload(None, "mxc://localhost/NewMedia", false, None).await;
    let manifest = wbf_sdk::matrix_media::upload_matrix_media(
        &server.base,
        "secret-token",
        &new_upload(plain.len() as u64, true),
        &mut &plain[..],
    )
    .await
    .unwrap();
    assert_eq!(
        (manifest.kind, manifest.size, manifest.name.as_str()),
        (MediaKind::MatrixEncrypted, plain.len() as u64, "cat.png")
    );
    let file = manifest.file.clone().unwrap();
    assert_eq!(file["url"], "mxc://localhost/NewMedia");
    let put = the_put(&server);
    assert_eq!(put.path, "/_matrix/media/v3/upload/localhost/NewMedia");
    assert_eq!(put.header("content-length"), Some("300001"));
    assert_eq!(
        put.header("transfer-encoding"),
        None,
        "length first, not chunked"
    );
    assert_eq!(
        put.header("content-type"),
        Some("application/octet-stream"),
        "the server does not learn it is an image"
    );
    assert_eq!(put.header("authorization"), Some("Bearer secret-token"));
    assert_ne!(put.body, plain, "the server got ciphertext");
    let info: matrix_sdk_crypto::MediaEncryptionInfo = serde_json::from_value(file).unwrap();
    let mut cipher = &put.body[..];
    let mut decryptor = matrix_sdk_crypto::AttachmentDecryptor::new(&mut cipher, info).unwrap();
    let mut opened = Vec::new();
    decryptor
        .read_to_end(&mut opened)
        .expect("the hash matches");
    assert_eq!(opened, plain);
}

/// 金鑰與 IV 每次 PUT 現產（同一組加密兩份 body 會洩漏 XOR）：同一個上傳 PUT 兩次，兩份 `file` 的 key、iv 都不一樣。
#[tokio::test]
async fn every_put_gets_its_own_key_and_iv() {
    let plain = sample(1_000);
    let server = serve_upload(None, "mxc://localhost/NewMedia", false, None).await;
    let upload = new_upload(plain.len() as u64, true);
    let mut files = Vec::new();
    for _ in 0..2 {
        let manifest =
            wbf_sdk::matrix_media::upload_matrix_media(&server.base, "t", &upload, &mut &plain[..])
                .await
                .unwrap();
        files.push(manifest.file.unwrap());
    }
    assert_ne!(files[0]["key"]["k"], files[1]["key"]["k"]);
    assert_ne!(files[0]["iv"], files[1]["iv"]);
}

#[tokio::test]
async fn a_plain_upload_goes_as_is_with_its_type() {
    let plain = sample(70_000);
    let server = serve_upload(None, "mxc://localhost/NewMedia", false, None).await;
    let manifest = wbf_sdk::matrix_media::upload_matrix_media(
        &server.base,
        "t",
        &new_upload(plain.len() as u64, false),
        &mut &plain[..],
    )
    .await
    .unwrap();
    assert_eq!(
        (manifest.kind, manifest.file),
        (MediaKind::MatrixPlain, None)
    );
    let put = the_put(&server);
    assert_eq!(put.body, plain);
    assert_eq!(put.header("content-type"), Some("image/png"));
}

/// body 比 `size` 短或長：中斷上游的請求，🚫 讓 server 收下一個長度不對的檔；回 Usage。
#[tokio::test]
async fn a_body_of_the_wrong_length_is_refused_and_never_completes_upstream() {
    for (size, body_len) in [(10_000u64, 9_000usize), (10_000, 11_000)] {
        let server = serve_upload(None, "mxc://localhost/NewMedia", false, None).await;
        let body = sample(body_len);
        let error = wbf_sdk::matrix_media::upload_matrix_media(
            &server.base,
            "t",
            &new_upload(size, false),
            &mut &body[..],
        )
        .await
        .unwrap_err();
        assert!(
            matches!(error, SdkError::Usage(_)),
            "{size}/{body_len}: {error:?}"
        );
        let complete = server
            .received
            .lock()
            .unwrap()
            .iter()
            .any(|request| request.method == "PUT" && request.body.len() as u64 == size);
        assert!(
            !complete,
            "{size}/{body_len}: the server never got a whole file"
        );
    }
}

/// server 說那個 mxc 已經有內容：回 Server、帶 errcode（daemon 回 502，UI 重新 `media.create`）。
#[tokio::test]
async fn a_media_id_that_already_has_content_is_a_server_error() {
    let plain = sample(1_000);
    let server = serve_upload(
        None,
        "mxc://localhost/NewMedia",
        false,
        Some((
            409,
            r#"{"errcode":"M_CANNOT_OVERWRITE_MEDIA","error":"exists"}"#,
        )),
    )
    .await;
    let error = wbf_sdk::matrix_media::upload_matrix_media(
        &server.base,
        "t",
        &new_upload(plain.len() as u64, true),
        &mut &plain[..],
    )
    .await
    .unwrap_err();
    assert_eq!(
        error.matrix_errcode(),
        Some("M_CANNOT_OVERWRITE_MEDIA"),
        "{error:?}"
    );
}

#[tokio::test]
async fn the_upload_limit_and_a_new_media_id_come_from_the_server() {
    let server = serve_upload(Some(50_000_000), "mxc://localhost/Fresh_1", true, None).await;
    let limit = wbf_sdk::matrix_media::get_upload_size_limit(&server.base, "t")
        .await
        .unwrap();
    assert_eq!(
        limit,
        Some(50_000_000),
        "fell back to the legacy config endpoint"
    );
    let mxc = wbf_sdk::matrix_media::create_matrix_media(&server.base, "t")
        .await
        .unwrap();
    assert_eq!(mxc, "mxc://localhost/Fresh_1");
    let paths: Vec<String> = server
        .received
        .lock()
        .unwrap()
        .iter()
        .map(|request| request.path.clone())
        .collect();
    assert_eq!(
        paths,
        vec![
            "/_matrix/client/v1/media/config".to_string(),
            "/_matrix/media/v3/config".to_string(),
            "/_matrix/media/v1/create".to_string()
        ]
    );
    let silent = serve_upload(None, "mxc://localhost/x", false, None).await;
    assert_eq!(
        wbf_sdk::matrix_media::get_upload_size_limit(&silent.base, "t")
            .await
            .unwrap(),
        None
    );
}

/// `create` 回的不是合法的 mxc（要拼進 PUT 的路徑）：🚫 拿來用。
#[tokio::test]
async fn a_created_media_id_that_is_not_a_valid_mxc_is_refused() {
    let server = serve_upload(None, "mxc://localhost/../../x", false, None).await;
    let error = wbf_sdk::matrix_media::create_matrix_media(&server.base, "t")
        .await
        .unwrap_err();
    assert!(matches!(error, SdkError::Integrity(_)), "{error:?}");
}

/// 收的那頭不要了（取消）：停下來、回 Usage。
#[tokio::test]
async fn a_closed_sink_stops_the_download() {
    let plain = sample(500_000);
    let server = serve(plain, false).await;
    let (sink, pieces) = mpsc::channel(1);
    drop(pieces);
    let end = stream_matrix_media(
        &server.base,
        "t",
        &attachment(MediaKind::MatrixPlain, None, None),
        &sink,
    )
    .await;
    assert!(matches!(end, Err(SdkError::Usage(_))), "{end:?}");
}

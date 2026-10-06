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

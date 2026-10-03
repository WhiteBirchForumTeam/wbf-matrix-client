//! 資料平面的 HTTP 那一層（/docs/design/rpc-specs/data-plane.md §3、§4）：真的開 port、用裸 TCP 送請求，驗路徑、Host 與狀態碼。
//! 整條上傳（建檔 → PUT → 送附件 → 下載比對）要真 server，在 `real_server.rs`。

use std::sync::Arc;

use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use wbf_daemon::connection::EncryptionPolicy;
use wbf_daemon::data_plane::{
    AccessKeys, DataServer, UploadMeta, MEDIA_PATH, UPLOAD_META_HEADER, UPLOAD_PATH,
};
use wbf_daemon::handle::Handle;
use wbf_daemon::settings::Settings;
use wbf_sdk::{Cipher, FileCipher, UploadState};

const TOKEN: [u8; 256] = [7u8; 256];

async fn start(dir: &std::path::Path) -> (Arc<Handle>, u16, tokio::task::JoinHandle<()>) {
    let handle = Handle::new(dir, EncryptionPolicy::enforced(), Settings::default());
    handle.set_access_keys(AccessKeys::from_token(&TOKEN));
    let server = DataServer::bind(0, handle.clone()).await.unwrap();
    let port = server.local_addr().unwrap().port();
    handle.set_ports(0, port).await;
    let task = tokio::spawn(server.run());
    (handle, port, task)
}

/// 一個請求、一個回應（`Connection: close`，讀到 EOF）。`host` 是 Host 標頭的值。
///
/// Return:
///     (u16, String, Vec<u8>)   (狀態碼, 標頭原文, body)
async fn send_as(
    host: &str,
    port: u16,
    method: &str,
    path: &str,
    headers: &str,
    body: &[u8],
) -> (u16, String, Vec<u8>) {
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    let head =
        format!("{method} {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n{headers}\r\n");
    stream.write_all(head.as_bytes()).await.unwrap();
    stream.write_all(body).await.unwrap();
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

async fn send(
    port: u16,
    method: &str,
    path: &str,
    headers: &str,
    body: &[u8],
) -> (u16, String, Vec<u8>) {
    send_as(
        &format!("127.0.0.1:{port}"),
        port,
        method,
        path,
        headers,
        body,
    )
    .await
}

/// 一個不存在的帳號的上傳：URL 鑄得出來，但 core 那邊找不到帳號。
fn upload_of_nobody() -> UploadState {
    let cipher = FileCipher::generate(Cipher::ChaCha20Poly1305, 16).unwrap();
    UploadState {
        server: "http://127.0.0.1:9".into(),
        user_id: "@nobody:localhost".into(),
        upload_id: 1,
        mxc: "mxc://localhost/0000000000000001".into(),
        chunk_max_bytes: 1 << 20,
        block: cipher.to_event_block(40),
    }
}

/// `media.create` 會給的那一對：PUT 的路徑、`Wbf-Upload-Meta` 那一行 header（含結尾的 CRLF）。
fn put_of(upload: &UploadState, encrypted: bool) -> (String, String) {
    let keys = AccessKeys::from_token(&TOKEN);
    let url_key = keys.to_upload_url_key(&upload.mxc, encrypted).unwrap();
    let meta = keys
        .to_upload_meta(
            &UploadMeta {
                upload: upload.clone(),
                source_uri: None,
            },
            encrypted,
        )
        .unwrap();
    (
        format!("{UPLOAD_PATH}{url_key}"),
        format!("{UPLOAD_META_HEADER}: {meta}\r\n"),
    )
}

/// 未解鎖一律 503（東西在，只是打不開）；解鎖之後：不是這個 daemon 發的 URL 404、方法不對 405、別的路徑 404。
#[tokio::test]
async fn locked_is_503_and_foreign_urls_paths_and_methods_are_told_apart() {
    let dir = tempfile::tempdir().unwrap();
    let (handle, port, _task) = start(dir.path()).await;
    let (path, meta) = put_of(&upload_of_nobody(), true);
    let headers = format!("{meta}Content-Length: 0\r\n");

    let (status, _, body) = send(port, "PUT", &path, &headers, b"").await;
    assert_eq!(status, 503);
    let body: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body["code"], 1001, "{body}");

    handle.core().await.create_vault(None).unwrap();
    let other_daemon = AccessKeys::from_token(&[8u8; 256])
        .to_upload_url_key(&upload_of_nobody().mxc, true)
        .unwrap();
    let (status, _, _) = send(
        port,
        "PUT",
        &format!("{UPLOAD_PATH}{other_daemon}"),
        &headers,
        b"",
    )
    .await;
    assert_eq!(status, 404, "別的 daemon（別的 token）發的");
    let (status, head, _) = send(port, "GET", &path, "", b"").await;
    assert_eq!(status, 405);
    assert!(head.to_ascii_lowercase().contains("allow: put"), "{head}");
    for path in [
        "/",
        "/upload/",
        "/upload/mxc/",
        "/upload/mxc/e-abc_def",
        &path.replace("/mxc/", "/"),
    ] {
        let (status, _, _) = send(port, "PUT", path, &headers, b"").await;
        assert_eq!(status, 404, "{path}");
    }
    let (status, head, _) = send(port, "PUT", "/media/mxc/e-abc_def", &headers, b"").await;
    assert_eq!(status, 405, "讀的路徑只收 GET／HEAD");
    assert!(
        head.to_ascii_lowercase().contains("allow: get, head"),
        "{head}"
    );
}

/// 讀的 URL（/docs/design/rpc-specs/data-plane.md §8）：未解鎖 503；別的 daemon 發的、用途不對（上傳的 URL）、本機沒有這個 mxc 的任何紀錄都是 404。
#[tokio::test]
async fn a_media_url_is_locked_foreign_or_unknown_before_anything_is_read() {
    let dir = tempfile::tempdir().unwrap();
    let (handle, port, _task) = start(dir.path()).await;
    let mxc = "mxc://localhost/nobody-has-this";
    let keys = AccessKeys::from_token(&TOKEN);
    let media_path = format!("{MEDIA_PATH}{}", keys.to_media_url_key(mxc, true).unwrap());
    let (status, _, body) = send(port, "GET", &media_path, "", b"").await;
    assert_eq!(status, 503);
    let body: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body["code"], 1001, "{body}");

    handle.core().await.create_vault(None).unwrap();
    let (status, _, _) = send(port, "GET", &media_path, "", b"").await;
    assert_eq!(status, 404, "本機沒有任何帳號有這個 mxc 的紀錄");
    let (status, _, _) = send(port, "HEAD", &media_path, "", b"").await;
    assert_eq!(status, 404);
    let other_daemon = AccessKeys::from_token(&[8u8; 256])
        .to_media_url_key(mxc, true)
        .unwrap();
    let (status, _, _) = send(port, "GET", &format!("{MEDIA_PATH}{other_daemon}"), "", b"").await;
    assert_eq!(status, 404, "別的 daemon（別的 token）發的");
    let upload_key = keys.to_upload_url_key(mxc, true).unwrap();
    let (status, _, _) = send(port, "GET", &format!("{MEDIA_PATH}{upload_key}"), "", b"").await;
    assert_eq!(status, 404, "上傳的 URL 拿來讀：用途不對");
    let media_key = keys.to_media_url_key(mxc, true).unwrap();
    let (status, _, _) = send(
        port,
        "PUT",
        &format!("{UPLOAD_PATH}{media_key}"),
        &format!("{UPLOAD_META_HEADER}: x\r\nContent-Length: 0\r\n"),
        b"",
    )
    .await;
    assert_eq!(status, 404, "讀的 URL 拿來上傳：用途不對");
    let (status, _, _) = send(port, "POST", &media_path, "Content-Length: 0\r\n", b"").await;
    assert_eq!(status, 405);
}

/// URL 只帶 mxc，上傳狀態在 `Wbf-Upload-Meta`：沒帶是 400（講清楚要帶什麼）；帶了別的上傳的 meta 跟不認得一樣 404。
#[tokio::test]
async fn the_meta_header_is_required_and_must_belong_to_the_url() {
    let dir = tempfile::tempdir().unwrap();
    let (handle, port, _task) = start(dir.path()).await;
    handle.core().await.create_vault(None).unwrap();
    let (path, meta) = put_of(&upload_of_nobody(), true);

    let (status, _, body) = send(port, "PUT", &path, "Content-Length: 40\r\n", &[0u8; 40]).await;
    assert_eq!(status, 400);
    let body: Value = serde_json::from_slice(&body).unwrap();
    assert!(
        body["msg"].as_str().unwrap().contains(UPLOAD_META_HEADER),
        "{body}"
    );

    let mut other = upload_of_nobody();
    other.upload_id = 2;
    other.mxc = "mxc://localhost/0000000000000002".into();
    let (_, meta_of_other) = put_of(&other, true);
    let headers = format!("{meta_of_other}Content-Length: 40\r\n");
    let (status, _, _) = send(port, "PUT", &path, &headers, &[0u8; 40]).await;
    assert_eq!(status, 404, "別的上傳的 meta 配不上這個 URL");

    let headers = format!("{meta}Content-Length: 40\r\n");
    let (status, _, _) = send(port, "PUT", &path, &headers, &[0u8; 40]).await;
    assert_eq!(status, 400, "配得上就交給 core（這個帳號不在）");
}

/// DNS rebinding：網頁把自己的網域指到 127.0.0.1，瀏覽器送的 Host 是那個網域——一律 403，🚫 不看 URL。
#[tokio::test]
async fn a_request_that_does_not_name_loopback_as_its_host_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let (handle, port, _task) = start(dir.path()).await;
    handle.core().await.create_vault(None).unwrap();
    let (path, meta) = put_of(&upload_of_nobody(), true);
    let headers = format!("{meta}Content-Length: 40\r\n");
    for host in ["evil.example", "evil.example:80", "127.0.0.1.evil.example"] {
        let (status, _, _) = send_as(host, port, "PUT", &path, &headers, &[0u8; 40]).await;
        assert_eq!(status, 403, "Host {host:?}");
    }
    for host in ["localhost", "LOCALHOST:1", "127.0.0.1", "[::1]:8080"] {
        let (status, _, _) = send_as(host, port, "PUT", &path, &headers, &[0u8; 40]).await;
        assert_ne!(status, 403, "Host {host:?}");
    }
}

/// 明文模式的 URL 與 meta（`c-`）：加密模式下一律不收；`daemon.set_encryption` 關掉之後才收。
#[tokio::test]
async fn a_plain_url_is_taken_only_while_encryption_is_off() {
    let dir = tempfile::tempdir().unwrap();
    let (handle, port, _task) = start(dir.path()).await;
    handle.core().await.create_vault(None).unwrap();
    let (path, meta) = put_of(&upload_of_nobody(), false);
    assert!(path.contains("/c-"), "{path}");
    let headers = format!("{meta}Content-Length: 40\r\n");
    let (status, _, _) = send(port, "PUT", &path, &headers, &[0u8; 40]).await;
    assert_eq!(status, 404, "加密模式下 c- 等於不存在");

    let reply = handle
        .call(wbf_daemon::message::Request {
            method: "daemon.set_encryption".into(),
            params: json!({ "enforced": false }),
            id: Some(1),
        })
        .await;
    assert_eq!(reply.code, 0, "{}", reply.msg);
    let (status, _, _) = send(port, "PUT", &path, &headers, &[0u8; 40]).await;
    assert_eq!(status, 400, "收了、交給 core（這個帳號不在）");
}

/// `Content-Length` 跟建檔時的大小對不上就 400、🚫 不讀 body；core 拒了（帳號不在，`Usage` → 400）之後，
/// 同一個 URL 可以再 PUT（🚫 不是 409）——PUT 結束的每一條路都要把「正在收」解除。
#[tokio::test]
async fn a_failed_put_leaves_the_url_usable_again() {
    let dir = tempfile::tempdir().unwrap();
    let (handle, port, _task) = start(dir.path()).await;
    handle.core().await.create_vault(None).unwrap();
    let (path, meta) = put_of(&upload_of_nobody(), true);

    let headers = format!("{meta}Content-Length: 39\r\n");
    let (status, _, body) = send(port, "PUT", &path, &headers, &[0u8; 39]).await;
    assert_eq!(status, 400);
    let body: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body["code"], 102, "{body}");
    assert!(
        body["msg"]
            .as_str()
            .unwrap()
            .contains("does not match the size 40"),
        "{body}"
    );

    let headers = format!("{meta}Content-Length: 40\r\n");
    for _ in 0..2 {
        let (status, _, body) = send(port, "PUT", &path, &headers, &[0u8; 40]).await;
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(status, 400, "{body}");
        assert!(
            body["msg"].as_str().unwrap().contains("no account"),
            "{body}"
        );
    }
}

/// `daemon.shutdown` 之後資料平面也停：port 不再收連線。
#[tokio::test]
async fn the_data_plane_stops_with_the_daemon() {
    let dir = tempfile::tempdir().unwrap();
    let (handle, port, task) = start(dir.path()).await;
    let reply = handle
        .call(wbf_daemon::message::Request {
            method: "daemon.shutdown".into(),
            params: json!({}),
            id: Some(1),
        })
        .await;
    assert_eq!(reply.code, 0);
    handle.begin_shutdown_if_requested();
    tokio::time::timeout(std::time::Duration::from_secs(5), task)
        .await
        .expect("the data plane stopped")
        .unwrap();
    assert!(tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .is_err());
}

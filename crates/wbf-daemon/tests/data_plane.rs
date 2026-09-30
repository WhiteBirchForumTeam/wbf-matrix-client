//! 資料平面的 HTTP 那一層（/docs/design/rpc-specs/data-plane.md §3、§4）：真的開 port、用裸 TCP 送請求，驗路徑與狀態碼。
//! 整條上傳（建檔 → 送附件 → PUT → 下載比對）要真 server，在 `real_server.rs`。

use std::sync::Arc;

use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use wbf_daemon::connection::EncryptionPolicy;
use wbf_daemon::data_plane::DataServer;
use wbf_daemon::handle::Handle;
use wbf_daemon::settings::Settings;
use wbf_sdk::{Cipher, FileCipher, UploadState};

async fn start(dir: &std::path::Path) -> (Arc<Handle>, u16, tokio::task::JoinHandle<()>) {
    let handle = Handle::new(dir, EncryptionPolicy::enforced(), Settings::default());
    let server = DataServer::bind(0, handle.clone()).await.unwrap();
    let port = server.local_addr().unwrap().port();
    handle.set_ports(0, port).await;
    let task = tokio::spawn(server.run());
    (handle, port, task)
}

/// 一個請求、一個回應（`Connection: close`，讀到 EOF）。
///
/// Return:
///     (u16, String, Vec<u8>)   (狀態碼, 標頭原文, body)
async fn send(
    port: u16,
    method: &str,
    path: &str,
    headers: &str,
    body: &[u8],
) -> (u16, String, Vec<u8>) {
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    let head = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n{headers}\r\n"
    );
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

fn put_path(token: &str) -> String {
    format!("/upload/{token}")
}

/// 一個不存在的帳號的上傳：鑄得出 token，但 core 那邊找不到帳號。
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

/// 未解鎖一律 503（東西在，只是打不開）；解鎖之後：認不得的 token 404、方法不對 405、別的路徑 404。
#[tokio::test]
async fn locked_is_503_and_unknown_tokens_paths_and_methods_are_told_apart() {
    let dir = tempfile::tempdir().unwrap();
    let (handle, port, _task) = start(dir.path()).await;
    let token = "0".repeat(64);

    let (status, _, body) =
        send(port, "PUT", &put_path(&token), "Content-Length: 0\r\n", b"").await;
    assert_eq!(status, 503);
    let body: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body["code"], 1001, "{body}");

    handle.core().await.create_vault(None).unwrap();
    let (status, _, _) = send(port, "PUT", &put_path(&token), "Content-Length: 0\r\n", b"").await;
    assert_eq!(status, 404, "認不得的 token");
    let (status, head, _) = send(port, "GET", &put_path(&token), "", b"").await;
    assert_eq!(status, 405);
    assert!(head.to_ascii_lowercase().contains("allow: put"), "{head}");
    for path in [
        "/",
        "/upload/",
        "/upload/abc",
        "/media/x",
        &format!("/upload/{token}/x"),
    ] {
        let (status, _, _) = send(port, "PUT", path, "Content-Length: 0\r\n", b"").await;
        assert_eq!(status, 404, "{path}");
    }
}

/// `Content-Length` 跟建檔時的大小對不上就 400、🚫 不讀 body；core 拒了（這裡是帳號不在，`Usage` → 400）之後 token 放回去，
/// 下一個 PUT 🚫 不是 409——PUT 結束的每一條路都要把「收著」解除。
#[tokio::test]
async fn a_failed_put_hands_the_token_back() {
    let dir = tempfile::tempdir().unwrap();
    let (handle, port, _task) = start(dir.path()).await;
    handle.core().await.create_vault(None).unwrap();
    let token = handle
        .capabilities()
        .issue_upload(upload_of_nobody())
        .unwrap();

    let (status, _, body) = send(
        port,
        "PUT",
        &put_path(&token),
        "Content-Length: 39\r\n",
        &[0u8; 39],
    )
    .await;
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

    for _ in 0..2 {
        let (status, _, body) = send(
            port,
            "PUT",
            &put_path(&token),
            "Content-Length: 40\r\n",
            &[0u8; 40],
        )
        .await;
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(status, 400, "{body}");
        assert!(
            body["msg"].as_str().unwrap().contains("no account"),
            "{body}"
        );
    }
    assert_eq!(
        handle
            .capabilities()
            .find_finished_upload(
                "mxc://localhost/0000000000000001",
                "http://127.0.0.1:9",
                "@nobody:localhost"
            )
            .err(),
        Some(wbf_daemon::data_plane::MissingUpload::NotFinished),
        "失敗不作廢固定大小的上傳（可以續傳），也🚫 不當成傳完了"
    );
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

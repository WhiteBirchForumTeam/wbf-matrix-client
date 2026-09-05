//! server 回 Matrix errcode 時要分到 `SdkError::Server`（CLI exit 2），不是 `Network`（exit 4）。
//! PR #9 審查 rumia 🟡1 用假 server 抓到的回歸：靠 Display 字串抓 `M_` 永遠抓不到。
//! 這裡起一個只會回 403 的迷你 HTTP server，不需要 wbfuwunel。要開 `--features matrix`。
#![cfg(feature = "matrix")]

use std::io::{Read, Write};
use std::net::TcpListener;

use wbf_sdk::backend::matrix_sdk::MatrixBackend;
use wbf_sdk::SdkError;

/// 只認兩條路：`/versions` 回一個版本（matrix-sdk 建 client 時會問），其他一律 403 M_FORBIDDEN。
fn spawn_forbidding_server() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let address = listener.local_addr().expect("addr");
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut buffer = [0u8; 4096];
            let read = stream.read(&mut buffer).unwrap_or(0);
            let request = String::from_utf8_lossy(&buffer[..read]);
            let (status, body) = if request.starts_with("GET /_matrix/client/versions") {
                ("200 OK", r#"{"versions":["v1.11"]}"#)
            } else {
                (
                    "403 Forbidden",
                    r#"{"errcode":"M_FORBIDDEN","error":"Invalid password"}"#,
                )
            };
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
        }
    });
    format!("http://{address}")
}

#[tokio::test]
async fn wrong_password_is_a_server_error_with_errcode() {
    let server = spawn_forbidding_server();
    let store = std::env::temp_dir().join(format!("wbf-sdk-test-{}", std::process::id()));
    let result = MatrixBackend::login(&server, "alice", "wrong", "test", &store).await;
    let _ = std::fs::remove_dir_all(&store);
    let error = result.err().expect("login must fail");
    match &error {
        SdkError::Server { code, meta, .. } => {
            assert_eq!(code, "M_FORBIDDEN");
            assert_eq!(meta["status"], 403);
        }
        other => panic!("expected Server, got {other}"),
    }
}

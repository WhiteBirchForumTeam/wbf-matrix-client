//! 真的把 daemon 這個**程序**跑起來（architecture-v2 §4.3 的第 2–5 步）。
//!
//! 上面兩個測試檔都是在同一個程序裡叫 `RpcServer::bind`，所以 `main.rs` 那一段
//! ——讀 token 檔、清掉舊的 `daemon.json`、綁好之後才寫、stdout 宣告 ready、結束時收拾——
//! 一行都沒被跑到（PR #31 審查 cirno 指出的最後一塊）。這個檔補的就是那一段。
//!
//! 🚫 不需要 homeserver：只到 `hello`／`daemon.info`／`daemon.shutdown`。

use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Child, Command, Stdio};

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::Message;
use wbf_daemon::pack::{self, PackType, RpcKeys, Side};

const TOKEN: [u8; 256] = [9u8; 256];

/// 起來之後 stdout 的第一行就是 ready；`main.rs` 保證 stdout 只有這一行。
struct Ready {
    rpc_port: u16,
}

fn spawn_daemon(data_dir: &Path, token_file: &Path) -> (Child, Ready) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_wbf-matrix-client-daemon"))
        .arg("-s")
        .arg("--data-dir")
        .arg(data_dir)
        .arg("--token-file")
        .arg(token_file)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("the daemon binary runs");
    let mut line = String::new();
    BufReader::new(child.stdout.as_mut().expect("piped stdout"))
        .read_line(&mut line)
        .expect("the daemon says ready on stdout");
    let ready: Value = serde_json::from_str(&line).unwrap_or_else(|error| {
        panic!("the ready line is not JSON ({error}): {line:?}");
    });
    assert_eq!(ready["ready"], true, "{ready}");
    let rpc_port = ready["rpc_port"].as_u64().expect("rpc_port") as u16;
    assert_ne!(rpc_port, 0);
    (child, Ready { rpc_port })
}

async fn call(port: u16, keys: &RpcKeys, requests: &[Value]) -> Vec<Value> {
    let (mut socket, _) = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}"))
        .await
        .expect("the daemon accepts connections once it says ready");
    let mut replies = Vec::new();
    for request in requests {
        let frame = pack::seal(
            keys,
            Side::Client,
            PackType::Cipher,
            request.to_string().as_bytes(),
        );
        socket.send(Message::Binary(frame.into())).await.unwrap();
        loop {
            match socket.next().await.unwrap().unwrap() {
                Message::Binary(bytes) => {
                    let (_, json) = pack::open(keys, Side::Client, &bytes).unwrap();
                    replies.push(serde_json::from_slice(&json).unwrap());
                    break;
                }
                Message::Ping(_) | Message::Pong(_) => continue,
                other => panic!("unexpected {other:?}"),
            }
        }
    }
    replies
}

/// 第 2–5 步走一遍真的程序：ready 的兩個管道、token 檔誰動、`daemon.json` 的進與出。
#[tokio::test]
async fn the_daemon_process_announces_ready_leaves_the_token_alone_and_cleans_up_on_shutdown() {
    // 短路徑：加密過的目錄名很長（handover §4 9b）。
    let dir = tempfile::Builder::new()
        .prefix("wp")
        .tempdir_in(std::env::temp_dir())
        .unwrap();
    let token_file = dir.path().join("daemon.token");
    std::fs::write(&token_file, TOKEN).unwrap();

    // ⚠️ 上一次留下的 `daemon.json`：daemon 必須在綁定之前清掉它，
    // 否則前端會把這份殘留當成「這一次」的 ready 然後連到別人的 port。
    let ready_path = dir.path().join("daemon.json");
    std::fs::write(&ready_path, br#"{"rpc_port":1,"data_port":2}"#).unwrap();

    let (mut child, ready) = spawn_daemon(dir.path(), &token_file);

    // 說 ready 的時候：port 真的在聽，而且 daemon.json 是**這一次**寫的。
    let written: Value =
        serde_json::from_slice(&std::fs::read(&ready_path).unwrap()).expect("daemon.json is JSON");
    assert_eq!(written["rpc_port"], ready.rpc_port);
    assert_ne!(written["rpc_port"], 1, "殘留的那份應該被蓋掉");
    assert_eq!(written["pid"], child.id());

    // 🚨 誰起的誰動：daemon 讀完 token 檔之後**不碰它**（不抹、不刪、不改）。
    assert_eq!(
        std::fs::read(&token_file).unwrap(),
        TOKEN,
        "daemon 不該動 token 檔"
    );

    let keys = RpcKeys::from_token(&TOKEN);
    let replies = call(
        ready.rpc_port,
        &keys,
        &[
            json!({ "method": "hello", "params": { "protocols": [1], "client": "wbf-matrix-rpc-cli process-test" }, "id": 0 }),
            json!({ "method": "daemon.info", "id": 1 }),
        ],
    )
    .await;
    assert_eq!(replies[0]["code"], 0, "{}", replies[0]);
    assert_eq!(replies[1]["result"]["rpc_port"], ready.rpc_port);

    // 前端做第 4 步：抹掉 token 檔。daemon 照樣服務（它不會回頭讀）。
    assert!(wbf_daemon::token::shred(&token_file).unwrap());
    let replies = call(
        ready.rpc_port,
        &keys,
        &[
            json!({ "method": "hello", "params": { "protocols": [1], "client": "wbf-matrix-rpc-cli process-test" }, "id": 0 }),
            json!({ "method": "daemon.shutdown", "id": 1 }),
        ],
    )
    .await;
    assert_eq!(replies[0]["code"], 0, "{}", replies[0]);
    assert_eq!(replies[1]["code"], 0, "{}", replies[1]);

    // 程序真的結束，而且收走了 daemon.json（🚫 不留給下一次當成 ready）。
    let status = wait_for_exit(&mut child);
    assert!(status.success(), "{status:?}");
    assert!(!ready_path.exists(), "daemon.json 應該被收掉");
}

/// 等程序結束，最多 10 秒。⚠️ 不用 `wait()` 直接擋著：卡住的時候要看得出是「沒結束」，
/// 而不是整個測試 hang 到 CI 逾時。
fn wait_for_exit(child: &mut Child) -> std::process::ExitStatus {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        match child.try_wait().expect("try_wait") {
            Some(status) => return status,
            None if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                panic!("the daemon did not exit within 10 s of daemon.shutdown");
            }
            None => std::thread::sleep(std::time::Duration::from_millis(50)),
        }
    }
}

/// token 檔別人讀得到就**拒絕啟動**（fail closed，architecture-v2 §4.3）。
/// Windows 靠目錄 ACL，那裡這條檢查一律過，所以只在 Unix 跑。
#[cfg(unix)]
#[test]
fn a_world_readable_token_file_stops_the_daemon_from_starting() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let token_file = dir.path().join("daemon.token");
    std::fs::write(&token_file, TOKEN).unwrap();
    std::fs::set_permissions(&token_file, std::fs::Permissions::from_mode(0o644)).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_wbf-matrix-client-daemon"))
        .arg("-s")
        .arg("--data-dir")
        .arg(dir.path())
        .arg("--token-file")
        .arg(&token_file)
        .output()
        .expect("the daemon binary runs");
    assert!(!output.status.success(), "0644 的 token 檔不該啟動得起來");
    assert!(!dir.path().join("daemon.json").exists(), "也不該寫 ready");
}

//! 真的把 daemon 這個**程序**跑起來（/docs/design/rpc-specs/local-interface.md §3 的三步）。
//!
//! 上面兩個測試檔都是在同一個程序裡叫 `RpcServer::bind`，所以 `main.rs` 那一段
//! ——從環境變數讀 token、清掉舊的 `daemon.json`、綁好之後才寫、stdout 宣告 ready、結束時收拾——
//! 一行都沒被跑到（PR #31 審查 cirno 指出的最後一塊）。這個檔補的就是那一段。
//!
//! **不**需要 homeserver：只到 `hello`／`daemon.info`／`daemon.shutdown`。

use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Child, Command, Stdio};

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::Message;
use wbf_daemon::pack::{self, PackType, RpcKeys, Side};
use wbf_daemon::protocol::CURRENT_PROTOCOL;
use wbf_daemon::token::{to_env_value, TOKEN_ENV};

const TOKEN: [u8; 256] = [9u8; 256];

/// 起來之後 stdout 的第一行就是 ready；`main.rs` 保證 stdout 只有這一行。
struct Ready {
    rpc_port: u16,
    instance: String,
    pid: u32,
}

/// 像前端那樣起 daemon：token 放在子程序的環境變數（/docs/design/rpc-specs/local-interface.md §3 的第 1 步）。
fn daemon_command(data_dir: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_wbf-matrix-client-daemon"));
    command
        .arg("-s")
        .arg("--data-dir")
        .arg(data_dir)
        .env(TOKEN_ENV, to_env_value(&TOKEN).as_str());
    command
}

fn spawn_daemon(data_dir: &Path) -> (Child, Ready) {
    let mut child = daemon_command(data_dir)
        .stdout(Stdio::piped())
        // ⚠️ 留著 stderr：起不來的時候，原因只在那裡。禁止丟到 null 然後對著空的 stdout 猜。
        .stderr(Stdio::piped())
        .spawn()
        .expect("the daemon binary runs");
    let mut line = String::new();
    BufReader::new(child.stdout.as_mut().expect("piped stdout"))
        .read_line(&mut line)
        .expect("讀 daemon 的 stdout");
    if line.trim().is_empty() {
        // 沒說 ready 就是根本沒起來 —— 把它的抱怨原樣印出來。
        panic!(
            "the daemon exited before saying ready: {}",
            complaint_of(child)
        );
    }
    let ready: Value = serde_json::from_str(&line).unwrap_or_else(|error| {
        panic!("the ready line is not JSON ({error}): {line:?}");
    });
    assert_eq!(ready["ready"], true, "{ready}");
    let rpc_port = ready["rpc_port"].as_u64().expect("rpc_port") as u16;
    assert_ne!(rpc_port, 0);
    let instance = ready["instance"].as_str().expect("instance").to_string();
    // UUID v4 的文字長相：8-4-4-4-12。不只檢查「有這個欄位」。
    assert_eq!(instance.len(), 36, "{instance}");
    assert_eq!(
        instance.chars().filter(|c| *c == '-').count(),
        4,
        "{instance}"
    );
    let pid = ready["pid"].as_u64().expect("pid") as u32;
    (
        child,
        Ready {
            rpc_port,
            instance,
            pid,
        },
    )
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
        )
        .unwrap();
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

/// 三步走一遍真的程序：ready 的兩個管道、`daemon.json` 的進與出、資料目錄裡沒有 token。
#[tokio::test]
async fn the_daemon_process_announces_ready_and_cleans_up_on_shutdown() {
    // 短路徑：加密過的目錄名很長（/docs/handover.md §4 9b）。
    let dir = tempfile::Builder::new()
        .prefix("wp")
        .tempdir_in(std::env::temp_dir())
        .unwrap();

    // ⚠️ 上一次留下的 `daemon.json`：daemon 必須在綁定之前清掉它，
    // 否則前端會把這份殘留當成「這一次」的 ready 然後連到別人的 port。
    let ready_path = dir.path().join("daemon.json");
    std::fs::write(&ready_path, br#"{"rpc_port":1,"data_port":2}"#).unwrap();

    let (mut child, ready) = spawn_daemon(dir.path());

    // 說 ready 的時候：port 真的在聽，而且 daemon.json 是**這一次**寫的。
    let written: Value =
        serde_json::from_slice(&std::fs::read(&ready_path).unwrap()).expect("daemon.json is JSON");
    assert_eq!(written["rpc_port"], ready.rpc_port);
    assert_ne!(written["rpc_port"], 1, "殘留的那份應該被蓋掉");
    assert_eq!(written["pid"], child.id());
    // 三個管道講的是同一個實例：stdout 的 ready、daemon.json、以及等一下的 hello／daemon.info。
    assert_eq!(ready.pid, child.id());
    assert_eq!(written["instance"], ready.instance);
    // 沒用 `--config`：資料目錄裡生成了一份起手式 conf（/docs/design/rpc-specs/rpc-cli.md §4）。
    let conf = std::fs::read_to_string(dir.path().join(wbf_core::conf::CONF_FILE_NAME))
        .expect("the daemon writes a starter wbf.conf");
    assert!(conf.contains("TRANSPORT"), "{conf}");

    let keys = RpcKeys::from_token(&TOKEN);
    let replies = call(
        ready.rpc_port,
        &keys,
        &[
            json!({ "method": "hello", "params": { "protocols": [CURRENT_PROTOCOL], "client": "wbf-matrix-rpc-cli process-test" }, "id": 0 }),
            json!({ "method": "daemon.info", "id": 1 }),
            json!({ "method": "daemon.shutdown", "id": 2 }),
        ],
    )
    .await;
    assert_eq!(replies[0]["code"], 0, "{}", replies[0]);
    assert_eq!(replies[0]["result"]["instance"], ready.instance, "hello");
    assert_eq!(replies[0]["result"]["pid"], child.id());
    assert_eq!(replies[1]["result"]["rpc_port"], ready.rpc_port);
    assert_eq!(
        replies[1]["result"]["instance"], ready.instance,
        "daemon.info"
    );
    assert!(replies[1]["result"]["uptime_seconds"].is_u64());
    assert_eq!(replies[2]["code"], 0, "{}", replies[2]);

    // 程序真的結束，而且收走了 daemon.json（不留給下一次當成 ready）。
    let status = wait_for_exit(&mut child);
    assert!(status.success(), "{status:?}");
    assert!(!ready_path.exists(), "daemon.json 應該被收掉");

    // 🔴 token 從頭到尾不落地：資料目錄裡沒有任何一個檔含它（原始 bytes 或環境變數那種 base64）。
    let encoded = to_env_value(&TOKEN);
    for entry in walk(dir.path()) {
        let bytes = std::fs::read(&entry).unwrap();
        assert!(
            !contains(&bytes, &TOKEN) && !contains(&bytes, encoded.as_bytes()),
            "the token ended up in {}",
            entry.display()
        );
    }
}

fn walk(dir: &Path) -> Vec<std::path::PathBuf> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            files.extend(walk(&path));
        } else {
            files.push(path);
        }
    }
    files
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

/// 起不來的時候它到底說了什麼。
fn complaint_of(mut child: Child) -> String {
    let _ = child.kill();
    match child.wait_with_output() {
        Ok(output) => String::from_utf8_lossy(&output.stderr).trim().to_string(),
        Err(error) => format!("(stderr 讀不到：{error})"),
    }
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

/// 資料目錄的獨佔（/docs/design/overview/architecture-v2.md §0.2）：第二個 daemon **起不來**，而且不會動到第一個的東西。
///
/// 🚨 這條在 2026-09-13 之前是不成立的：`cache.db` 開在 WAL（SQLite 本來就支援多程序），
/// 而 port 預設是隨機的，所以兩個 daemon 可以同時開著同一個資料目錄互相踩。
#[tokio::test]
async fn a_second_daemon_on_the_same_data_dir_refuses_to_start() {
    let dir = tempfile::Builder::new()
        .prefix("wl")
        .tempdir_in(std::env::temp_dir())
        .unwrap();

    let (mut first, ready) = spawn_daemon(dir.path());
    let ready_path = dir.path().join("daemon.json");
    let before = std::fs::read(&ready_path).unwrap();

    let second = daemon_command(dir.path())
        .output()
        .expect("the daemon binary runs");
    assert!(!second.status.success(), "第二個不該起得來");
    let complaint = String::from_utf8_lossy(&second.stderr);
    assert!(
        complaint.contains("is already using this data directory"),
        "{complaint}"
    );
    assert!(second.stdout.is_empty(), "起不來就不該印 ready");

    // 🔴 重點：第一個的 ready 檔**一個 byte 都沒被動到**（沒拿到鎖就不准碰這個目錄）。
    assert_eq!(std::fs::read(&ready_path).unwrap(), before);
    // 第一個也還活著、還在服務。
    let keys = RpcKeys::from_token(&TOKEN);
    let replies = call(
        ready.rpc_port,
        &keys,
        &[
            json!({ "method": "hello", "params": { "protocols": [CURRENT_PROTOCOL], "client": "wbf-matrix-rpc-cli process-test" }, "id": 0 }),
            json!({ "method": "daemon.info", "id": 1 }),
        ],
    )
    .await;
    assert_eq!(replies[1]["result"]["instance"], ready.instance);

    // 收攤：停掉第一個之後，同一個目錄就換得了人。
    let replies = call(
        ready.rpc_port,
        &keys,
        &[
            json!({ "method": "hello", "params": { "protocols": [CURRENT_PROTOCOL], "client": "wbf-matrix-rpc-cli process-test" }, "id": 0 }),
            json!({ "method": "daemon.shutdown", "id": 1 }),
        ],
    )
    .await;
    assert_eq!(replies[1]["code"], 0, "{}", replies[1]);
    assert!(wait_for_exit(&mut first).success());

    let (mut third, third_ready) = spawn_daemon(dir.path());
    assert_ne!(third_ready.instance, ready.instance, "是新的一個實例");
    let _ = third.kill();
    let _ = third.wait();
}

/// 只有 `-s` 一種起法；沒帶就講清楚、不碰資料目錄（/docs/design/rpc-specs/rpc-cli.md §4：單發模式不做）。
#[test]
fn without_serve_the_daemon_does_nothing_and_says_so() {
    let dir = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_wbf-matrix-client-daemon"))
        .arg("--data-dir")
        .arg(dir.path())
        .env(TOKEN_ENV, to_env_value(&TOKEN).as_str())
        .output()
        .expect("the daemon binary runs");
    assert!(!output.status.success(), "沒帶 -s 不該跑起來");
    assert!(output.stdout.is_empty(), "不該印 ready");
    let complaint = String::from_utf8_lossy(&output.stderr);
    assert!(complaint.contains("pass -s"), "{complaint}");
    // 連資料目錄都不該碰：沒有鎖檔、沒有 daemon.json、沒有 conf。
    assert!(!dir.path().join("daemon.json").exists());
    assert!(!dir.path().join(wbf_daemon::lock::LOCK_FILE_NAME).exists());
    assert!(!dir.path().join(wbf_core::conf::CONF_FILE_NAME).exists());
}

/// 沒帶 token 就不啟動，而且講得出要放在哪個環境變數（fail closed，/docs/design/rpc-specs/local-interface.md §3）。
#[test]
fn without_a_token_the_daemon_refuses_to_start_and_names_the_variable() {
    let dir = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_wbf-matrix-client-daemon"))
        .arg("-s")
        .arg("--data-dir")
        .arg(dir.path())
        .env_remove(TOKEN_ENV)
        .output()
        .expect("the daemon binary runs");
    assert!(!output.status.success(), "沒有 token 不該啟動得起來");
    assert!(output.stdout.is_empty(), "不該印 ready");
    let complaint = String::from_utf8_lossy(&output.stderr);
    assert!(complaint.contains(TOKEN_ENV), "{complaint}");
    assert!(!dir.path().join("daemon.json").exists());
    assert!(!dir.path().join(wbf_daemon::lock::LOCK_FILE_NAME).exists());
}

/// token 長度不對就不啟動，而且**講得出原因**（`TOKEN_LEN`，/docs/design/rpc-specs/local-interface.md §3）。
///
/// 📎 這條同時是 `spawn_daemon` 那個診斷路徑的實跑：daemon 起不來的時候，
/// 測試要看得到它的抱怨，不是對著一行空的 stdout 猜。
#[test]
fn a_token_of_the_wrong_length_stops_the_daemon_and_says_so() {
    use base64::Engine as _;
    let dir = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_wbf-matrix-client-daemon"))
        .arg("-s")
        .arg("--data-dir")
        .arg(dir.path())
        .env(
            TOKEN_ENV,
            base64::engine::general_purpose::STANDARD.encode([7u8; 255]),
        )
        .output()
        .expect("the daemon binary runs");
    assert!(!output.status.success(), "255 byte 的 token 不該啟動得起來");
    assert!(output.stdout.is_empty(), "不該印 ready");
    let complaint = String::from_utf8_lossy(&output.stderr);
    assert!(complaint.contains("255 bytes"), "{complaint}");
    assert!(complaint.contains("256"), "要說出正確的長度：{complaint}");
    // 起不來就不該在那個目錄留下東西。
    assert!(!dir.path().join("daemon.json").exists());
}

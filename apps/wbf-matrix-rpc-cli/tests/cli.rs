//! 真的跑 rpc-cli 這個執行檔，配真的 daemon（/docs/design/rpc-specs/wbf-cli-spec.md §0–§3；PR #89 審查：之前沒有任何測試跑過它，
//! 拿掉「第一個錯就停」整套照綠）。**不**需要 homeserver：只用 `daemon.info`、`vault.create`、認不得的 method。
//!
//! ⚠️ daemon 的執行檔要在 rpc-cli 旁邊（同一個 target 目錄）：`cargo test --workspace` 會一起編。
//! 只跑 `cargo test -p wbf-matrix-rpc-cli` 時要先 `cargo build -p wbf-daemon`，不然這裡講清楚、紅掉（**不**跳過）。

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use serde_json::Value;

fn rpc_cli() -> &'static str {
    env!("CARGO_BIN_EXE_wbf-matrix-rpc-cli")
}

fn daemon_binary() -> PathBuf {
    let path = Path::new(rpc_cli())
        .parent()
        .expect("the rpc-cli binary sits in a directory")
        .join(format!(
            "wbf-matrix-client-daemon{}",
            std::env::consts::EXE_SUFFIX
        ));
    assert!(
        path.exists(),
        "{} is missing: build it first (cargo build -p wbf-daemon) or run cargo test --workspace",
        path.display()
    );
    path
}

/// 短路徑：加密過的目錄名很長（/docs/handover.md §4 9b）。
fn data_dir(prefix: &str) -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix(prefix)
        .tempdir_in(std::env::temp_dir())
        .unwrap()
}

/// `batch` 一行一則：第一行送出去、stdin 還開著的時候就有回應；第一個錯之後的行**不**送、exit 1；跑完 daemon 收乾淨。
#[test]
fn batch_answers_each_line_before_the_next_and_stops_at_the_first_error() {
    let dir = data_dir("rb");
    let mut child = Command::new(rpc_cli())
        .arg("--data-dir")
        .arg(dir.path())
        .arg("--daemon")
        .arg(daemon_binary())
        .arg("-q")
        .arg("batch")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("rpc-cli runs");
    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let (lines_tx, lines_rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            if lines_tx.send(line.unwrap()).is_err() {
                break;
            }
        }
    });

    writeln!(stdin, r#"{{"method":"daemon.info"}}"#).unwrap();
    stdin.flush().unwrap();
    // stdin 還開著：先讀到 EOF 才送的話，這裡等不到。
    let first: Value = serde_json::from_str(
        &lines_rx
            .recv_timeout(Duration::from_secs(60))
            .expect("the first line is answered while stdin is still open"),
    )
    .unwrap();
    assert_eq!(first["code"], 0, "{first}");

    writeln!(stdin, r#"{{"method":"nope.nope"}}"#).unwrap();
    writeln!(stdin, r#"{{"method":"vault.create"}}"#).unwrap();
    drop(stdin);
    let output = child.wait_with_output().unwrap();
    let rest: Vec<String> = lines_rx.iter().collect();
    assert_eq!(rest.len(), 1, "only the failing line is answered: {rest:?}");
    let second: Value = serde_json::from_str(&rest[0]).unwrap();
    // 還沒建 vault：閘門先擋（1002），一樣是那一行的錯。
    assert_ne!(second["code"], 0, "{second}");
    assert_eq!(output.status.code(), Some(1));
    assert!(
        !dir.path().join("local.key").exists(),
        "the line after the error was not sent"
    );
    assert!(
        !dir.path().join("daemon.json").exists(),
        "the daemon this run started is gone"
    );
}

/// `--attach` 連資料目錄裡在跑的那支（`daemon.json` ＋ `daemon.token`），用完**不**關它。
#[tokio::test]
async fn attach_talks_to_the_running_daemon_and_leaves_it_running() {
    let dir = data_dir("ra");
    let token = wbf_daemon::token::new_token().unwrap();
    let daemon = wbf_daemon::client::spawn_daemon(
        &daemon_binary(),
        dir.path(),
        token.as_slice(),
        Stdio::null(),
    )
    .await
    .unwrap();

    for _ in 0..2 {
        let output = tokio::process::Command::new(rpc_cli())
            .arg("--data-dir")
            .arg(dir.path())
            .arg("--attach")
            .arg("call")
            .arg("daemon.info")
            .output()
            .await
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let info: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(info["instance"], daemon.ready.instance.as_str());
    }
    assert!(
        dir.path().join("daemon.json").exists(),
        "still running after two attached runs"
    );

    let (mut rpc, _) = wbf_daemon::client::RpcClient::connect(
        daemon.ready.rpc_port,
        token.as_slice(),
        "wbf-matrix-rpc-cli test",
    )
    .await
    .unwrap();
    assert_eq!(
        rpc.call("daemon.shutdown", Value::Null).await.unwrap()["code"],
        0
    );
    rpc.close().await;
    assert!(daemon
        .wait_for_exit(Duration::from_secs(30))
        .await
        .unwrap()
        .success());
}

/// 連哪一支要明講：`--attach` 與 `--daemon` 恰好一個，都沒給或都給是用法錯（exit 1，不是 clap 預設的 2）；
/// `--attach` 而沒有 daemon 在跑：講清楚、提示 `--daemon`。
#[test]
fn which_daemon_must_be_said_and_attaching_to_nothing_says_so() {
    let dir = data_dir("rn");
    let neither = Command::new(rpc_cli())
        .arg("--data-dir")
        .arg(dir.path())
        .arg("call")
        .arg("daemon.info")
        .env_remove("WBF_DAEMON")
        .output()
        .unwrap();
    assert_eq!(neither.status.code(), Some(1));

    let both = Command::new(rpc_cli())
        .arg("--data-dir")
        .arg(dir.path())
        .arg("--attach")
        .arg("--daemon")
        .arg(daemon_binary())
        .arg("call")
        .arg("daemon.info")
        .output()
        .unwrap();
    assert_eq!(both.status.code(), Some(1));

    let nothing_running = Command::new(rpc_cli())
        .arg("--data-dir")
        .arg(dir.path())
        .arg("--attach")
        .arg("call")
        .arg("daemon.info")
        .output()
        .unwrap();
    assert_eq!(nothing_running.status.code(), Some(1));
    let complaint = String::from_utf8_lossy(&nothing_running.stderr);
    assert!(complaint.contains("--daemon"), "{complaint}");
}

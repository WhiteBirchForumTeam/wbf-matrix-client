//! 一次執行的連線（/docs/design/rpc-specs/wbf-cli-spec.md §0、§1；維護者 2026-11-20 定兩種，由參數決定）：
//!
//! - `--attach`：連資料目錄裡在跑的那支 daemon（`daemon.json` 的 port ＋ `daemon.token`），用完只斷線，**不**關它。
//! - `--daemon <binary>`：起一支新的（產生 token、用環境變數指定給它），用完 `daemon.shutdown`、等它結束。
//!   起到一半出錯（ready 之後連不上、`hello` 被拒）時丟掉 `DaemonProcess` 就會殺掉它（`wbf_daemon::client`）。
//!
//! passphrase 與 password 從檔案或終端讀（daemon 不問終端，/docs/design/rpc-specs/local-interface.md §5）。

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use base64::Engine as _;
use serde_json::{json, Value};
use wbf_daemon::client::{self, ClientError, DaemonProcess, RpcClient};
use zeroize::Zeroizing;

/// daemon 收到 `daemon.shutdown` 之後最多等它多久結束：它自己的關機有上限（請求 10 秒＋取消後 5 秒＋各步 5 秒），這裡給寬一點。
const DAEMON_EXIT_WAIT: Duration = Duration::from_secs(60);

/// 這支程式報上的 client 名稱（`hello` 要 `wbf-matrix` 開頭）。
pub const CLIENT_NAME: &str = concat!("wbf-matrix-rpc-cli ", env!("CARGO_PKG_VERSION"));

/// 失敗：要印給人看的那句，與 exit code（/docs/design/rpc-specs/rpc-spec.md §5 的那一欄）。
#[derive(Debug)]
pub struct Failure {
    pub message: String,
    pub exit_code: u8,
}

impl Failure {
    pub fn usage(message: impl Into<String>) -> Failure {
        Failure {
            message: message.into(),
            exit_code: 1,
        }
    }

    /// 連線那一頭的錯（daemon 中途走了、連線斷了）：跟網路同一級。
    pub fn connection(error: ClientError) -> Failure {
        Failure {
            message: error.message,
            exit_code: 4,
        }
    }

    /// 一則 `code` 不是 0 的回應。
    pub fn of_response(response: &Value) -> Failure {
        let code = client::code_of(response);
        let message = response.get("msg").and_then(Value::as_str).unwrap_or("");
        let data = response
            .get("data")
            .filter(|data| !data.is_null())
            .map(|data| format!("\ndata: {data}"))
            .unwrap_or_default();
        Failure {
            message: format!("error {code}: {message}{data}"),
            exit_code: exit_code_of(code),
        }
    }
}

/// RPC 的 `code` → exit code（/docs/design/rpc-specs/rpc-spec.md §5.2 那一欄與它下面的兩條）。
///
/// Args:
///     code: example: 1402
/// Return:
///     u8  0 成功；1 用法（含所有 1xx、9001、認不得的碼）；2 server 拒絕；3 完整性；4 網路；5 逾時；130 被取消
pub fn exit_code_of(code: u64) -> u8 {
    match code {
        0 => 0,
        105 => 130,
        100..=199 => 1,
        9001 => 1,
        9000..=9999 => 4,
        1300 => 4,
        1400 | 1401 => 2,
        1402 | 1600 => 5,
        1500 | 1501 => 3,
        // 1001–1299 的其餘都是用法那一級；認不得的碼也落到 1（禁止當成成功）。
        _ => 1,
    }
}

/// 要連哪一支 daemon（`--attach` 與 `--daemon` 二選一）。
pub enum DaemonChoice {
    /// 資料目錄裡在跑的那支
    Attach,
    /// 起一支新的，example: "C:/wbf/wbf-matrix-client-daemon.exe"
    Spawn(PathBuf),
}

pub struct Session {
    pub client: RpcClient,
    /// 我們起的那支；`--attach` 時是 None（不是我們的，不關它）
    spawned: Option<DaemonProcess>,
    /// `hello` 的 result（`unlocked`、`key_mode`…）
    pub hello: Value,
}

impl Session {
    /// 連上（必要時先起 daemon）、`hello`。
    ///
    /// Args:
    ///     choice: `--attach` 或 `--daemon <binary>`
    ///     data_dir: example: "<data dir>"
    ///     quiet: true ＝ 起的 daemon 的 stderr 丟掉
    pub async fn start(
        choice: &DaemonChoice,
        data_dir: &Path,
        quiet: bool,
    ) -> Result<Session, Failure> {
        match choice {
            DaemonChoice::Attach => {
                let (running, token) = client::find_running_daemon(data_dir).map_err(|error| {
                    Failure::usage(format!(
                        "{error}\n(to start one for this run instead, pass --daemon <path to wbf-matrix-client-daemon>)"
                    ))
                })?;
                let (client, hello) = RpcClient::connect(running.rpc_port, &token, CLIENT_NAME)
                    .await
                    .map_err(|error| {
                        Failure::connection(ClientError {
                            message: format!(
                                "{}\n(the daemon in {} may have been killed without cleaning up; \
                                 if so, start a new one with --daemon)",
                                error.message,
                                data_dir.display()
                            ),
                            closing_code: error.closing_code,
                        })
                    })?;
                Ok(Session {
                    client,
                    spawned: None,
                    hello,
                })
            }
            DaemonChoice::Spawn(daemon) => {
                let token = client::new_token().map_err(|error| Failure::usage(error.message))?;
                let stderr = if quiet {
                    Stdio::null()
                } else {
                    Stdio::inherit()
                };
                let spawned = client::spawn_daemon(daemon, data_dir, token.as_slice(), stderr)
                    .await
                    .map_err(|error| {
                        Failure::usage(format!(
                            "{error}\n(if a daemon is already serving {}, connect to it with --attach)",
                            data_dir.display()
                        ))
                    })?;
                // 這裡出錯時 `spawned` 被丟掉，那支 daemon 就被殺掉（`DaemonProcess` 的 kill_on_drop）。
                let (client, hello) =
                    RpcClient::connect(spawned.ready.rpc_port, token.as_slice(), CLIENT_NAME)
                        .await
                        .map_err(Failure::connection)?;
                Ok(Session {
                    client,
                    spawned: Some(spawned),
                    hello,
                })
            }
        }
    }

    /// 解鎖（還沒解的話）。還沒有 `local.key` 就什麼都不做：要它的命令自己會拿到 1002。
    ///
    /// ⚠️ 抹記憶體是盡力而為：讀進來的 passphrase 與它的 base64 是 `Zeroizing`，但組進 JSON 請求的那份複本（`serde_json::Value`、送出的字串）**不**會被抹掉。
    ///
    /// Args:
    ///     passphrase_file: `--passphrase-file`；None 而 vault 要 passphrase 時問終端
    pub async fn unlock(&mut self, passphrase_file: Option<&Path>) -> Result<(), Failure> {
        if self.hello.get("unlocked") == Some(&Value::Bool(true)) {
            return Ok(());
        }
        let params = match self.hello.get("key_mode").and_then(Value::as_str) {
            None => return Ok(()),
            // 給了 passphrase 檔就照送，讓 daemon 用 1004 拒絕，**不**靜默忽略。
            Some("plain") => match passphrase_file {
                Some(path) => {
                    json!({ "passphrase_base64": base64_of(&read_passphrase_file(path)?).as_str() })
                }
                None => Value::Null,
            },
            Some(_) => {
                let passphrase = match passphrase_file {
                    Some(path) => read_passphrase_file(path)?,
                    None => prompt_on_terminal("passphrase: ")
                        .map(|text| Zeroizing::new(text.as_bytes().to_vec()))?,
                };
                json!({ "passphrase_base64": base64_of(&passphrase).as_str() })
            }
        };
        let response = self.call("vault.unlock", params).await?;
        require_success(&response)?;
        Ok(())
    }

    /// 送一則請求、拿回整則回應（`code` 不是 0 也回 Ok，由呼叫端決定）。
    pub async fn call(&mut self, method: &str, params: Value) -> Result<Value, Failure> {
        self.client
            .call(method, params)
            .await
            .map_err(Failure::connection)
    }

    /// 收尾。`--attach` 的：只關連線，daemon 照跑。我們起的：`daemon.shutdown`、關連線、等它結束。
    ///
    /// 送不出 `daemon.shutdown`（連線已經斷了）照樣等：終端按 Ctrl+C 時 daemon 跟我們在同一個 console、也收到了，
    /// 它自己在關（/docs/design/rpc-specs/wbf-cli-spec.md §3.4.2）。判準只有一個：daemon 結束了沒有。
    pub async fn finish(mut self) -> Result<(), Failure> {
        let Some(spawned) = self.spawned.take() else {
            self.client.close().await;
            return Ok(());
        };
        let asked = self.call("daemon.shutdown", Value::Null).await;
        self.client.close().await;
        spawned
            .wait_for_exit(DAEMON_EXIT_WAIT)
            .await
            .map_err(|error| Failure::usage(error.message))?;
        // 連線斷了但 daemon 已經結束：要的結果到了。daemon 回了錯（例 107 以外的拒絕）才講。
        if let Ok(response) = asked {
            if client::code_of(&response) != 0 && client::code_of(&response) != 107 {
                return Err(Failure::of_response(&response));
            }
        }
        Ok(())
    }
}

/// Return:
///     Ok(&Value)     `code` 是 0：它的 `result`
///     Err(Failure)   其他
pub fn require_success(response: &Value) -> Result<&Value, Failure> {
    if client::code_of(response) != 0 {
        return Err(Failure::of_response(response));
    }
    Ok(response.get("result").unwrap_or(&Value::Null))
}

/// `--passphrase-file`：**整檔原始 bytes**，不去尾換行、不驗 UTF-8（/docs/design/storage/vault-and-keys.md §3）。
fn read_passphrase_file(path: &Path) -> Result<Zeroizing<Vec<u8>>, Failure> {
    std::fs::read(path)
        .map(Zeroizing::new)
        .map_err(|error| Failure::usage(format!("cannot read {}: {error}", path.display())))
}

/// `--password-file`：整檔就是那句話，去掉結尾一個換行（它要送給 homeserver，Matrix 規定是字串，/docs/design/storage/vault-and-keys.md §3.4）。
pub fn read_password_file(path: &Path) -> Result<Zeroizing<String>, Failure> {
    let text = Zeroizing::new(
        std::fs::read_to_string(path)
            .map_err(|error| Failure::usage(format!("cannot read {}: {error}", path.display())))?,
    );
    let trimmed = text
        .strip_suffix('\n')
        .map(|stripped| stripped.strip_suffix('\r').unwrap_or(stripped))
        .unwrap_or(&text);
    Ok(Zeroizing::new(trimmed.to_string()))
}

/// 從終端不回顯地讀。stdin 不是終端（腳本、管線）就拒絕：rpassword 在 Windows 會繞過 stdin 直接開 console 等人打字，會掛住。
pub fn prompt_on_terminal(label: &str) -> Result<Zeroizing<String>, Failure> {
    use std::io::IsTerminal;
    if !std::io::stdin().is_terminal() {
        return Err(Failure::usage(format!(
            "{} needed but stdin is not a terminal; pass it with --password-file / --passphrase-file",
            label.trim_end_matches(": ")
        )));
    }
    rpassword::prompt_password(label)
        .map(Zeroizing::new)
        .map_err(|error| Failure::usage(format!("cannot read from the terminal: {error}")))
}

fn base64_of(bytes: &[u8]) -> Zeroizing<String> {
    Zeroizing::new(base64::engine::general_purpose::STANDARD.encode(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 對 /docs/design/rpc-specs/rpc-spec.md §5.2 那一欄與它下面兩條，一個一個對。
    #[test]
    fn exit_codes_follow_the_rpc_spec() {
        for (code, exit) in [
            (0, 0),
            (100, 1),
            (102, 1),
            (105, 130),
            (107, 1),
            (1001, 1),
            (1021, 1),
            (1100, 1),
            (1200, 1),
            (1300, 4),
            (1400, 2),
            (1401, 2),
            (1402, 5),
            (1500, 3),
            (1501, 3),
            (1600, 5),
            (9001, 1),
            (9006, 4),
            (4242, 1),
        ] {
            assert_eq!(exit_code_of(code), exit, "code {code}");
        }
    }

    #[test]
    fn a_password_file_loses_one_trailing_newline_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pw");
        std::fs::write(&path, "hunter2\r\n").unwrap();
        assert_eq!(read_password_file(&path).unwrap().as_str(), "hunter2");
        std::fs::write(&path, "hunter2\n\n").unwrap();
        assert_eq!(read_password_file(&path).unwrap().as_str(), "hunter2\n");
    }
}

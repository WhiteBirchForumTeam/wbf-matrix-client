//! 一次執行＝一支 daemon（/docs/design/rpc-specs/rpc-cli.md §1 的甲）：產生 token → 環境變數交給 daemon → 連線、`hello` → 解鎖；
//! 結束時 `daemon.shutdown`、等它自己走。別的前端起的 daemon 在跑時，daemon 自己拿不到資料目錄的鎖、不宣告 ready，這裡講清楚、不重試。
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
    fn connection(error: ClientError) -> Failure {
        Failure {
            message: error.0,
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

pub struct Session {
    pub client: RpcClient,
    daemon: DaemonProcess,
    /// `hello` 的 result（`unlocked`、`key_mode`…）
    pub hello: Value,
}

impl Session {
    /// 起 daemon、連上、`hello`。
    ///
    /// Args:
    ///     daemon: daemon 的執行檔, example: "<rpc-cli 同一個目錄>/wbf-matrix-client-daemon.exe"
    ///     data_dir: example: "<data dir>"
    ///     quiet: true ＝ daemon 的 stderr 丟掉
    pub async fn start(daemon: &Path, data_dir: &Path, quiet: bool) -> Result<Session, Failure> {
        let token = client::new_token().map_err(|error| Failure::usage(error.0))?;
        let stderr = if quiet {
            Stdio::null()
        } else {
            Stdio::inherit()
        };
        let daemon = client::spawn_daemon(daemon, data_dir, &token, stderr).map_err(|error| {
            Failure::usage(format!(
                "{error}\n(if another frontend's daemon is serving {}, use that frontend or stop it first: \
                 rpc-cli can only talk to a daemon it starts itself, /docs/design/rpc-specs/rpc-cli.md §1)",
                data_dir.display()
            ))
        })?;
        let (client, hello) = RpcClient::connect(daemon.ready.rpc_port, &token, CLIENT_NAME)
            .await
            .map_err(Failure::connection)?;
        drop(token);
        Ok(Session {
            client,
            daemon,
            hello,
        })
    }

    /// 解鎖（還沒解的話）。還沒有 `local.key` 就什麼都不做：要它的命令自己會拿到 1002。
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

    /// `daemon.shutdown`、關連線、等 daemon 自己結束。
    ///
    /// 送不出去（連線已經斷了）照樣等：終端按 Ctrl+C 時 daemon 跟我們在同一個 console、也收到了，
    /// 它自己在關（/docs/design/rpc-specs/wbf-cli-spec.md §3.4.2）。判準只有一個：daemon 結束了沒有。
    pub async fn finish(mut self) -> Result<(), Failure> {
        let asked = self.call("daemon.shutdown", Value::Null).await;
        self.client.close().await;
        let daemon = self.daemon;
        tokio::task::spawn_blocking(move || daemon.wait_for_exit(DAEMON_EXIT_WAIT))
            .await
            .map_err(|error| Failure::usage(format!("cannot wait for the daemon: {error}")))?
            .map_err(|error| Failure::usage(error.0))?;
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

/// daemon 的執行檔：`--daemon` 給了用它，沒給用 rpc-cli 同一個目錄的那支。
///
/// Return:
///     Ok(PathBuf)    example: "C:/wbf/wbf-matrix-client-daemon.exe"
///     Err(Failure)   找不到自己在哪
pub fn daemon_path(explicit: Option<PathBuf>) -> Result<PathBuf, Failure> {
    if let Some(path) = explicit {
        return Ok(path);
    }
    let me = std::env::current_exe().map_err(|error| {
        Failure::usage(format!(
            "cannot find where rpc-cli is: {error}; pass --daemon"
        ))
    })?;
    let dir = me
        .parent()
        .ok_or_else(|| Failure::usage("cannot find rpc-cli's directory; pass --daemon"))?;
    Ok(dir.join(format!(
        "wbf-matrix-client-daemon{}",
        std::env::consts::EXE_SUFFIX
    )))
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

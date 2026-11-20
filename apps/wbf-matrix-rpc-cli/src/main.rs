// 維護者 2026-09-23：正式碼不用會讓整支程式收掉的方法（unwrap／expect／panic／索引）——每個失敗要有去處；測試建置放行（測試要看到它炸）。
#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::unreachable,
        clippy::todo,
        clippy::unimplemented,
        clippy::indexing_slicing,
        clippy::string_slice
    )
)]
//! `wbf-matrix-rpc-cli`：從另一個程序丟 RPC 給 daemon（/docs/design/rpc-specs/rpc-cli.md）。
//!
//! 命令列只是 RPC 訊息的薄封裝（/docs/design/overview/architecture-v2.md §0.2）：一個通用的 `call`、一個 `batch`，
//! 加上五個「RPC 本身做不到、一定要在前端做」的便利命令（讀密碼、確認、讀寫本地檔、把推播印出來）。**不**長自己的邏輯：
//! 不重試、不改形、不串動作（例：加密房送訊息遇到 1401 要不要重送，是呼叫的人的事）。
//!
//! 連哪一支 daemon 由參數決定：`--attach` 連資料目錄裡在跑的那支、`--daemon <binary>` 起一支新的（`session.rs`）。
//! stdout 只放結果（JSON），訊息與錯誤走 stderr。

mod session;

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use clap::{ArgGroup, Parser, Subcommand};
use serde_json::{json, Map, Value};
use wbf_daemon::client;

use session::{require_success, DaemonChoice, Failure, Session};

#[derive(Parser)]
#[command(
    name = "wbf-matrix-rpc-cli",
    version,
    about = "從另一個程序丟 RPC 給 daemon：--attach 連在跑的那支，或 --daemon <binary> 起一支新的、做完關掉"
)]
#[command(group(ArgGroup::new("which_daemon").required(true).args(["attach", "daemon"])))]
struct Cli {
    /// 資料目錄（daemon 的）
    #[arg(long, env = "WBF_DATA_DIR")]
    data_dir: PathBuf,
    /// 解 `local.key` 的 passphrase 所在的檔（整檔原始 bytes）。沒給而需要時問終端
    #[arg(long, env = "WBF_PASSPHRASE_FILE")]
    passphrase_file: Option<PathBuf>,
    /// 連資料目錄裡在跑的那支 daemon（daemon.json 的 port、daemon.token）；用完不關它
    #[arg(long)]
    attach: bool,
    /// 起一支新的 daemon：它的執行檔。用完 daemon.shutdown、等它結束
    #[arg(long, env = "WBF_DAEMON")]
    daemon: Option<PathBuf>,
    /// 不顯示起的 daemon 自己的訊息（它的 stderr）
    #[arg(long, short)]
    quiet: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// 送一則 RPC，印它的 `result`（JSON）。例：call room.list '{"sync":"both"}'
    Call {
        /// method 名, example: room.list
        method: String,
        /// params，一個 JSON 物件；不給就不帶
        params: Option<String>,
    },
    /// 從 stdin 讀 JSON Lines（每行 {"method": …, "params": …}），每則回應印一行；遇到第一個錯就停
    Batch,
    /// 帳號裡要在前端做的那兩件（讀密碼、確認）；其餘用 call account.*
    #[command(subcommand)]
    Account(AccountCommand),
    /// 上傳一個檔（或 `-` 讀 stdin）到資料平面，印 manifest。發訊息另外 call room.send_attachment
    Put {
        /// 要上傳的檔；`-` 讀 stdin
        file: PathBuf,
        /// 給對方看的檔名；沒給用檔名（stdin 時必填）
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        mimetype: Option<String>,
        /// 要送去的房：照房間決定加不加密
        #[arg(long)]
        room: Option<String>,
        /// chacha20-poly1305、aes-256-gcm 或 none
        #[arg(long)]
        cipher: Option<String>,
        #[arg(long)]
        chunk_size: Option<u32>,
        #[arg(long)]
        user: Option<String>,
        #[arg(long)]
        server: Option<String>,
    },
    /// 從資料平面讀一個媒體，寫到檔案或 stdout
    Get {
        #[arg(long, conflicts_with_all = ["manifest", "room"])]
        mxc: Option<String>,
        /// manifest（`put` 印的那份 JSON）所在的檔
        #[arg(long, conflicts_with = "room")]
        manifest: Option<PathBuf>,
        /// 跟 --event-id 一起：房裡那則附件
        #[arg(long, requires = "event_id")]
        room: Option<String>,
        #[arg(long)]
        event_id: Option<String>,
        /// 寫到哪；`-` 或不給是 stdout
        #[arg(short, long)]
        output: Option<PathBuf>,
        /// HTTP Range，例：bytes=0-1023
        #[arg(long)]
        range: Option<String>,
        #[arg(long)]
        user: Option<String>,
        #[arg(long)]
        server: Option<String>,
    },
    /// 訂閱推播，一則一行印出來，直到 Ctrl+C、收滿 --count 則或 --timeout 秒
    Watch {
        /// 推播名，例：room.message；不給是全部
        events: Vec<String>,
        /// 只收這個帳號的（完整 mxid）
        #[arg(long)]
        user: Option<String>,
        #[arg(long)]
        count: Option<u64>,
        #[arg(long)]
        timeout: Option<u64>,
    },
}

#[derive(Subcommand)]
enum AccountCommand {
    /// 登入一個帳號（account.add）。還沒有 local.key 就先建（vault.create，有 --passphrase-file 就用它）
    Add {
        #[arg(long)]
        server: String,
        /// 帳號（localpart 或完整 mxid）
        #[arg(long)]
        user: String,
        /// 密碼所在的檔（去掉結尾一個換行）；沒給就問終端
        #[arg(long, env = "WBF_PASSWORD_FILE")]
        password_file: Option<PathBuf>,
        #[arg(long)]
        device_name: Option<String>,
    },
    /// 摧毀這台機器上一個帳號的一切（account.destroy）。不可逆：沒帶 --yes 就先問
    Destroy {
        /// 完整 mxid
        user: String,
        #[arg(long)]
        server: Option<String>,
        #[arg(long)]
        yes: bool,
        #[arg(long)]
        accept_history_loss: bool,
    },
}

fn main() -> ExitCode {
    // 用法錯是 exit 1（/docs/design/rpc-specs/wbf-cli-spec.md §4）；clap 預設的 2 在這裡是「server 拒絕」。`--help`／`--version` 是 0。
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error) => {
            let _ = error.print();
            return ExitCode::from(if error.use_stderr() { 1 } else { 0 });
        }
    };
    // 沒有 runtime 就沒有這支程式（「必須要開」的層級）——但停也要講清楚、給 exit code，禁止 panic。
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("cannot start the async runtime: {error}");
            return ExitCode::from(1);
        }
    };
    match runtime.block_on(run(cli)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(failure) => {
            eprintln!("{}", failure.message);
            ExitCode::from(failure.exit_code)
        }
    }
}

/// 要在起 daemon **之前**問人的：問完才起，免得 daemon 開著等人打字。
enum Asked {
    Nothing,
    Password(zeroize::Zeroizing<String>),
}

async fn run(cli: Cli) -> Result<(), Failure> {
    let asked = ask_before_starting(&cli.command)?;
    // clap 的群組已經保證兩個恰好給一個；這裡再問一次、沒有就當用法錯（不假定上游洗乾淨了）。
    let choice = match (cli.attach, cli.daemon) {
        (true, None) => DaemonChoice::Attach,
        (false, Some(path)) => DaemonChoice::Spawn(path),
        _ => {
            return Err(Failure::usage(
                "pass exactly one of --attach (the daemon already serving the data directory) or --daemon <path> (start a new one)",
            ))
        }
    };
    let mut session = Session::start(&choice, &cli.data_dir, cli.quiet).await?;
    let outcome = run_command(
        &mut session,
        cli.command,
        asked,
        cli.passphrase_file.as_deref(),
    )
    .await;
    // 不管命令成不成功都把 daemon 關掉；命令的錯優先講。
    let finished = session.finish().await;
    outcome?;
    finished
}

fn ask_before_starting(command: &Command) -> Result<Asked, Failure> {
    match command {
        Command::Account(AccountCommand::Add { password_file, .. }) => {
            let password = match password_file {
                Some(path) => session::read_password_file(path)?,
                None => session::prompt_on_terminal("password: ")?,
            };
            Ok(Asked::Password(password))
        }
        Command::Account(AccountCommand::Destroy { user, yes, .. }) => {
            if !yes {
                confirm_destroy(user)?;
            }
            Ok(Asked::Nothing)
        }
        _ => Ok(Asked::Nothing),
    }
}

async fn run_command(
    session: &mut Session,
    command: Command,
    asked: Asked,
    passphrase_file: Option<&Path>,
) -> Result<(), Failure> {
    if let Command::Account(AccountCommand::Add {
        server,
        user,
        device_name,
        ..
    }) = command
    {
        let Asked::Password(password) = asked else {
            return Err(Failure::usage("no password was read"));
        };
        return account_add(
            session,
            server,
            user,
            device_name,
            password,
            passphrase_file,
        )
        .await;
    }
    session.unlock(passphrase_file).await?;
    match command {
        Command::Call { method, params } => {
            let params = match params {
                Some(text) => serde_json::from_str(&text)
                    .map_err(|error| Failure::usage(format!("params is not JSON: {error}")))?,
                None => Value::Null,
            };
            let response = session.call(&method, params).await?;
            print_json(require_success(&response)?)
        }
        Command::Batch => batch(session).await,
        Command::Account(AccountCommand::Destroy {
            user,
            server,
            accept_history_loss,
            ..
        }) => {
            let response = session
                .call(
                    "account.destroy",
                    json!({ "user": user, "server": server, "accept_history_loss": accept_history_loss }),
                )
                .await?;
            print_json(require_success(&response)?)
        }
        Command::Account(AccountCommand::Add { .. }) => Ok(()),
        Command::Put {
            file,
            name,
            mimetype,
            room,
            cipher,
            chunk_size,
            user,
            server,
        } => {
            let target = Target { user, server };
            let upload = Upload {
                file,
                name,
                mimetype,
                room,
                cipher,
                chunk_size,
            };
            put(session, upload, target).await
        }
        Command::Get {
            mxc,
            manifest,
            room,
            event_id,
            output,
            range,
            user,
            server,
        } => {
            let media = media_ref(mxc, manifest, room, event_id)?;
            get(session, media, output, range, Target { user, server }).await
        }
        Command::Watch {
            events,
            user,
            count,
            timeout,
        } => watch(session, events, user, count, timeout).await,
    }
}

/// `user`／`server` 兩個共同欄位（/docs/design/rpc-specs/rpc-spec.md §2）。
struct Target {
    user: Option<String>,
    server: Option<String>,
}

impl Target {
    fn add_to(&self, params: &mut Map<String, Value>) {
        if let Some(user) = &self.user {
            params.insert("user".into(), json!(user));
        }
        if let Some(server) = &self.server {
            params.insert("server".into(), json!(server));
        }
    }
}

async fn account_add(
    session: &mut Session,
    server: String,
    user: String,
    device_name: Option<String>,
    password: zeroize::Zeroizing<String>,
    passphrase_file: Option<&Path>,
) -> Result<(), Failure> {
    if session.hello.get("key_mode").is_none_or(Value::is_null) {
        // 還沒有 `local.key`：先建。給了 passphrase 檔就用它鎖，否則是 plain（/docs/design/storage/vault-and-keys.md §3）。
        let params = match passphrase_file {
            Some(path) => {
                let bytes = std::fs::read(path)
                    .map(zeroize::Zeroizing::new)
                    .map_err(|error| {
                        Failure::usage(format!("cannot read {}: {error}", path.display()))
                    })?;
                use base64::Engine as _;
                json!({ "passphrase_base64": base64::engine::general_purpose::STANDARD.encode(bytes.as_slice()) })
            }
            None => Value::Null,
        };
        require_success(&session.call("vault.create", params).await?)?;
    } else {
        session.unlock(passphrase_file).await?;
    }
    // ⚠️ 抹記憶體是盡力而為：`password` 是 Zeroizing，組進 JSON 請求的那份複本**不**會被抹掉。
    let mut params = json!({ "server": server, "user": user, "password": password.as_str() });
    if let (Some(device_name), Some(fields)) = (device_name, params.as_object_mut()) {
        fields.insert("device_name".into(), json!(device_name));
    }
    let response = session.call("account.add", params).await?;
    print_json(require_success(&response)?)
}

/// 一行一則：讀一行、送、印回應原樣（含 `code`）、才讀下一行（PR #89 審查：先讀到 EOF 會讓「寫一行、等回應、再寫」的呼叫端互等）。
/// 第一個錯之後不再讀、不再送。
async fn batch(session: &mut Session) -> Result<(), Failure> {
    use tokio::io::AsyncBufReadExt as _;
    let mut lines = tokio::io::BufReader::new(tokio::io::stdin()).lines();
    while let Some(line) = lines
        .next_line()
        .await
        .map_err(|error| Failure::usage(format!("cannot read stdin: {error}")))?
    {
        // 去掉 UTF-8 BOM：Windows PowerShell 5.1 的管線會在第一行前面加一個。
        let line = line.trim_start_matches('\u{feff}');
        if line.trim().is_empty() {
            continue;
        }
        let request: Value = serde_json::from_str(line)
            .map_err(|error| Failure::usage(format!("not a JSON line ({error}): {line}")))?;
        let method = request
            .get("method")
            .and_then(Value::as_str)
            .ok_or_else(|| Failure::usage(format!("no method in: {line}")))?;
        let params = request.get("params").cloned().unwrap_or(Value::Null);
        let response = session.call(method, params).await?;
        print_json(&response)?;
        require_success(&response)?;
    }
    Ok(())
}

struct Upload {
    file: PathBuf,
    name: Option<String>,
    mimetype: Option<String>,
    room: Option<String>,
    cipher: Option<String>,
    chunk_size: Option<u32>,
}

async fn put(session: &mut Session, upload: Upload, target: Target) -> Result<(), Failure> {
    let from_stdin = upload.file.as_os_str() == "-";
    let name = match (&upload.name, from_stdin) {
        (Some(name), _) => name.clone(),
        (None, true) => {
            return Err(Failure::usage(
                "put - needs --name (stdin has no file name)",
            ))
        }
        (None, false) => upload
            .file
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .ok_or_else(|| {
                Failure::usage(format!(
                    "{} has no file name; pass --name",
                    upload.file.display()
                ))
            })?,
    };
    let mut params = Map::new();
    params.insert("name".into(), json!(name));
    for (key, value) in [
        ("mimetype", upload.mimetype.map(Value::from)),
        ("room", upload.room.map(Value::from)),
        ("cipher", upload.cipher.map(Value::from)),
        ("chunk_size", upload.chunk_size.map(Value::from)),
    ] {
        if let Some(value) = value {
            params.insert(key.into(), value);
        }
    }
    target.add_to(&mut params);
    let (reply, manifest) = if from_stdin {
        let created = create(session, params).await?;
        client::put(&created, tokio::io::stdin(), None).await
    } else {
        let file = tokio::fs::File::open(&upload.file).await.map_err(|error| {
            Failure::usage(format!("cannot open {}: {error}", upload.file.display()))
        })?;
        let size = file
            .metadata()
            .await
            .map_err(|error| {
                Failure::usage(format!("cannot read {}: {error}", upload.file.display()))
            })?
            .len();
        params.insert("size".into(), json!(size));
        let created = create(session, params).await?;
        client::put(&created, file, Some(size)).await
    }
    .map_err(Failure::connection)?;
    if reply.status != 200 {
        return Err(Failure {
            message: format!("the data plane answered {}: {manifest}", reply.status),
            exit_code: 2,
        });
    }
    print_json(&manifest)
}

async fn create(session: &mut Session, params: Map<String, Value>) -> Result<Value, Failure> {
    let response = session.call("media.create", Value::Object(params)).await?;
    Ok(require_success(&response)?.clone())
}

/// `media.open` 的那三種指法（/docs/design/rpc-specs/rpc-spec.md §3.6）。
fn media_ref(
    mxc: Option<String>,
    manifest: Option<PathBuf>,
    room: Option<String>,
    event_id: Option<String>,
) -> Result<Map<String, Value>, Failure> {
    let mut params = Map::new();
    match (mxc, manifest, room, event_id) {
        (Some(mxc), None, None, None) => {
            params.insert("mxc".into(), json!(mxc));
        }
        (None, Some(path), None, None) => {
            let text = std::fs::read_to_string(&path).map_err(|error| {
                Failure::usage(format!("cannot read {}: {error}", path.display()))
            })?;
            let manifest: Value = serde_json::from_str(&text).map_err(|error| {
                Failure::usage(format!("{} is not JSON: {error}", path.display()))
            })?;
            params.insert("manifest".into(), manifest);
        }
        (None, None, Some(room), Some(event_id)) => {
            params.insert("room".into(), json!(room));
            params.insert("event_id".into(), json!(event_id));
        }
        _ => {
            return Err(Failure::usage(
                "say which media: --mxc, --manifest, or --room with --event-id",
            ))
        }
    }
    Ok(params)
}

async fn get(
    session: &mut Session,
    mut params: Map<String, Value>,
    output: Option<PathBuf>,
    range: Option<String>,
    target: Target,
) -> Result<(), Failure> {
    target.add_to(&mut params);
    // 先訂閱再 open：下載完成的推播可能在 open 回來之前就到，晚訂閱會漏掉它。
    require_success(
        &session
            .call("subscribe", json!({ "events": ["media.download"] }))
            .await?,
    )?;
    let media = Value::Object(params);
    let response = session.call("media.open", media.clone()).await?;
    let opened = require_success(&response)?;
    let text_of = |name: &str| opened.get(name).and_then(Value::as_str).map(str::to_string);
    let (Some(url), Some(mxc)) = (text_of("url"), text_of("mxc")) else {
        return Err(Failure::usage(format!(
            "media.open gave no url or mxc: {opened}"
        )));
    };
    let state = text_of("state").unwrap_or_else(|| "unknown".to_string());
    let output = output.filter(|path| path.as_os_str() != "-");
    let mut reply = fetch(&url, range.as_deref(), output.as_deref()).await?;
    // 502 ＝ 還沒下載完、而這支 daemon 現在拉不了（沒有帳號看得到帶金鑰的事件，/docs/design/rpc-specs/data-plane.md §8）。
    // 它已經排進佇列了：等它下載完，從池裡讀就不需要事件（/docs/design/rpc-specs/wbf-cli-spec.md §3.3）。
    if reply.status == 502 && matches!(state.as_str(), "queued" | "downloading" | "verifying") {
        eprintln!("{mxc} cannot be streamed before it is downloaded; waiting for the download");
        wait_for_download(session, &mxc, &media).await?;
        reply = fetch(&url, range.as_deref(), output.as_deref()).await?;
    }
    eprintln!(
        "status {}{}",
        reply.status,
        reply
            .verified
            .as_ref()
            .map(|verified| format!(", verified {verified}"))
            .unwrap_or_default()
    );
    match reply.status {
        200 | 206 => Ok(()),
        // 資料照給，但它沒驗過或驗不過（/docs/design/rpc-specs/data-plane.md §8.2）：跟完整性同一級。
        412 => Err(Failure {
            message: "written, but not verified (or verification failed)".into(),
            exit_code: 3,
        }),
        status => Err(Failure {
            message: format!(
                "the data plane answered {status}: {}",
                reply.refusal.unwrap_or_default()
            ),
            exit_code: 2,
        }),
    }
}

/// 一次 GET，寫到檔案（每次重建）或 stdout。
async fn fetch(
    url: &str,
    range: Option<&str>,
    output: Option<&Path>,
) -> Result<client::DataReply, Failure> {
    match output {
        Some(path) => {
            let file = tokio::fs::File::create(path).await.map_err(|error| {
                Failure::usage(format!("cannot create {}: {error}", path.display()))
            })?;
            client::get(url, range, file).await
        }
        None => client::get(url, range, tokio::io::stdout()).await,
    }
    .map_err(Failure::connection)
}

/// 等 `media.download` 推這個 mxc 走到終點。沒有上限（下載多久要看檔多大），Ctrl+C 停。
///
/// 推播會掉（/docs/design/rpc-specs/rpc-spec.md §4）：收到 `desync` 就重叫一次 `media.open` 看狀態，完成了就不再等（PR #89 審查）。
///
/// Args:
///     media: 當初 `media.open` 的 params，重查用
/// Return:
///     Ok(())         `complete`（推播或重查）
///     Err(Failure)   `failed`／`cancelled`（exit 2）、連線斷了（exit 4）
async fn wait_for_download(session: &mut Session, mxc: &str, media: &Value) -> Result<(), Failure> {
    loop {
        let push = session
            .client
            .next_push()
            .await
            .map_err(Failure::connection)?;
        if push.get("method").and_then(Value::as_str) == Some("desync") {
            let response = session.call("media.open", media.clone()).await?;
            let state = require_success(&response)?
                .get("state")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            if matches!(state, "complete" | "local_source") {
                return Ok(());
            }
            continue;
        }
        let params = push.get("params").unwrap_or(&Value::Null);
        let is_this_file = push.get("method").and_then(Value::as_str) == Some("media.download")
            && params.get("mxc").and_then(Value::as_str) == Some(mxc);
        match params
            .get("state")
            .and_then(Value::as_str)
            .filter(|_| is_this_file)
        {
            Some("complete") => return Ok(()),
            Some(state @ ("failed" | "cancelled")) => {
                return Err(Failure {
                    message: format!("the download of {mxc} ended {state}: {params}"),
                    exit_code: 2,
                })
            }
            _ => {}
        }
    }
}

async fn watch(
    session: &mut Session,
    events: Vec<String>,
    user: Option<String>,
    count: Option<u64>,
    timeout: Option<u64>,
) -> Result<(), Failure> {
    let events = if events.is_empty() {
        vec!["*".to_string()]
    } else {
        events
    };
    let mut params = json!({ "events": events });
    if let (Some(user), Some(fields)) = (user, params.as_object_mut()) {
        fields.insert("user".into(), json!(user));
    }
    require_success(&session.call("subscribe", params).await?)?;
    let deadline =
        timeout.map(|seconds| tokio::time::Instant::now() + Duration::from_secs(seconds));
    let mut received = 0u64;
    // 建一次、整個迴圈共用：每圈重建的話，兩圈之間按的 Ctrl+C 會漏掉。
    // 優先看它（biased）：daemon 跟我們在同一個 console、也收到了，它關連線的通知可能同時到，那不是網路錯。
    let mut interrupted = std::pin::pin!(tokio::signal::ctrl_c());
    while count.is_none_or(|count| received < count) {
        let until_deadline = async {
            match deadline {
                Some(deadline) => tokio::time::sleep_until(deadline).await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            biased;
            _ = &mut interrupted => break,
            push = session.client.next_push() => {
                let push = match push {
                    Ok(push) => push,
                    // daemon 要關了（9006，例：終端的 Ctrl+C 也送到它那裡、它的通知先到）：沒有東西可看了，正常結束。
                    Err(error) if error.closing_code == Some(9006) => {
                        eprintln!("the daemon is shutting down; stopped watching");
                        break;
                    }
                    Err(error) => return Err(Failure::connection(error)),
                };
                print_json(&push)?;
                received += 1;
            }
            // 只給 --timeout 是「看這麼久」（時間到就是做完了）；也給了 --count 卻沒收滿才是逾時。
            _ = until_deadline => {
                return match count {
                    None => Ok(()),
                    Some(count) => Err(Failure {
                        message: format!("got {received} of {count} pushes within {}s", timeout.unwrap_or(0)),
                        exit_code: 5,
                    }),
                };
            }
        }
    }
    Ok(())
}

/// stdout 一行一個 JSON（腳本好接）。
fn print_json(value: &Value) -> Result<(), Failure> {
    let mut stdout = std::io::stdout().lock();
    writeln!(stdout, "{value}")
        .and_then(|()| stdout.flush())
        .map_err(|error| Failure::usage(format!("cannot write to stdout: {error}")))
}

/// 摧毀是不可逆的：要使用者把 mxid 再打一次。stdin 不是終端（腳本）就要 --yes，禁止默默當成同意。
fn confirm_destroy(user: &str) -> Result<(), Failure> {
    if !std::io::stdin().is_terminal() {
        return Err(Failure::usage(
            "account destroy cannot be undone; stdin is not a terminal, so pass --yes",
        ));
    }
    eprint!("this deletes everything about {user} on this machine and cannot be undone; type the mxid to confirm: ");
    let _ = std::io::stderr().flush();
    let mut typed = String::new();
    std::io::stdin()
        .read_line(&mut typed)
        .map_err(|error| Failure::usage(format!("cannot read the confirmation: {error}")))?;
    if typed.trim() != user {
        return Err(Failure::usage("not confirmed; nothing was destroyed"));
    }
    Ok(())
}

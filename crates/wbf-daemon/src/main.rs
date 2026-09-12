//! `wbf-matrix-client-daemon`：這一版只有 `-s`（常駐）。單發命令（`daemon <命令>`，architecture-v2 §0.2）
//! 與資料平面在下一支 PR。

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use clap::Parser;
use wbf_daemon::connection::EncryptionPolicy;
use wbf_daemon::handle::Handle;
use wbf_daemon::pack::RpcKeys;
use wbf_daemon::server::RpcServer;
use wbf_daemon::settings::Settings;
use zeroize::Zeroizing;

#[derive(Parser)]
#[command(
    name = "wbf-matrix-client-daemon",
    version,
    about = "wbfuwunel client 本體：常駐、持有本地資料庫、開 RPC 給前端"
)]
struct Cli {
    /// 常駐：開本地 RPC 的 WS，等前端來連
    #[arg(short = 's', long = "server")]
    serve: bool,
    /// 資料目錄
    #[arg(long, env = "WBF_DATA_DIR")]
    data_dir: PathBuf,
    /// `daemon.token` 的路徑（前端產生的 256 byte 隨機檔，architecture-v2 §4.3）。預設 <data dir>/daemon.token
    #[arg(long)]
    token_file: Option<PathBuf>,
    /// RPC 的 port；0 就隨機，寫進 <data dir>/daemon.json
    #[arg(long, default_value_t = 0)]
    rpc_port: u16,
    /// conf 檔在哪；沒給就找 <data dir>/wbf.conf。⚠️ 明指了卻不在就報錯，不 fallback（CLI 規格 §10.1）
    #[arg(long, env = "WBF_CONFIG")]
    config: Option<PathBuf>,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    if !cli.serve {
        eprintln!(
            "only `-s` (serve) is implemented in this version; single-shot commands come next"
        );
        return ExitCode::from(1);
    }
    let token_path = cli
        .token_file
        .unwrap_or_else(|| cli.data_dir.join("daemon.token"));
    let token = match std::fs::read(&token_path) {
        Ok(bytes) => Zeroizing::new(bytes),
        Err(error) => {
            eprintln!(
                "cannot read the daemon token at {}: {error}",
                token_path.display()
            );
            return ExitCode::from(1);
        }
    };
    let keys = match RpcKeys::from_token_file(&token) {
        Ok(keys) => Arc::new(keys),
        Err(actual) => {
            eprintln!(
                "the daemon token at {} is {actual} bytes; it must be exactly {} (architecture-v2 §4.3)",
                token_path.display(),
                wbf_daemon::pack::TOKEN_LEN
            );
            return ExitCode::from(1);
        }
    };
    drop(token);

    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    runtime.block_on(async move {
        let settings = match Settings::load(cli.config.as_deref(), &cli.data_dir) {
            Ok(settings) => settings,
            Err(error) => {
                eprintln!("{error}");
                return ExitCode::from(1);
            }
        };
        for warning in &settings.warnings {
            eprintln!("{warning}");
        }
        let policy = EncryptionPolicy::enforced();
        let handle = Handle::new(&cli.data_dir, policy.clone(), settings);
        let server = match RpcServer::bind(cli.rpc_port, keys, policy, handle.clone()).await {
            Ok(server) => server,
            Err(error) => {
                eprintln!("cannot bind the RPC port: {error}");
                return ExitCode::from(1);
            }
        };
        let rpc_port = server.local_addr().map(|addr| addr.port()).unwrap_or(0);
        // 資料平面還沒有：data_port 先 0。
        handle.set_ports(rpc_port, 0).await;
        let info = serde_json::json!({ "rpc_port": rpc_port, "data_port": 0 });
        if let Err(error) = std::fs::write(cli.data_dir.join("daemon.json"), info.to_string()) {
            eprintln!("cannot write daemon.json: {error}");
            return ExitCode::from(1);
        }
        eprintln!("listening on ws://127.0.0.1:{rpc_port}");
        server.run().await;
        let _ = std::fs::remove_file(cli.data_dir.join("daemon.json"));
        ExitCode::SUCCESS
    })
}

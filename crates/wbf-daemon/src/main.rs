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
        clippy::indexing_slicing
    )
)]
//! `wbf-matrix-client-daemon`：這一版只有 `-s`（常駐）。單發命令（`daemon <命令>`，architecture-v2 §0.2）
//! 與資料平面在下一支 PR。
//!
//! 起動的順序照 architecture-v2 §4.3 的五步（前端寫 token → spawn → **daemon 宣告 ready** →
//! 前端抹掉 token 檔 → 之後只在記憶體裡）。⭐ 這支負責的是第 3 步，而「宣告 ready」是**一個邊緣**，
//! 不是一個狀態：所以 `daemon.json` 在綁定**之前**先刪掉，綁好之後才 temp＋rename 寫進去 ——
//! 前端等的那個檔出現的瞬間，port 一定已經在聽了，而且它一定不是上一次留下來的。

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
    ///
    /// ⚠️ daemon 讀完就不再回頭讀它：前端該在 ready 之後把它抹掉（`wbf_daemon::token::shred`）
    #[arg(long)]
    token_file: Option<PathBuf>,
    /// RPC 的 port；0 就隨機，寫進 <data dir>/daemon.json
    #[arg(long, default_value_t = 0)]
    rpc_port: u16,
    /// conf 檔在哪；沒給就找 <data dir>/wbf.conf。⚠️ 明指了卻不在就報錯，不 fallback（CLI 規格 §10.1）
    #[arg(long, env = "WBF_CONFIG")]
    config: Option<PathBuf>,
    /// 單發命令：`daemon <命令> [參數…]`（architecture-v2 §0.2）。⚠️ 還沒實作
    #[arg(trailing_var_arg = true)]
    command: Vec<String>,
}

/// **啟動路徑只有兩種**（維護者 2026-09-13）：常駐，或單發。
///
/// ⭐ 它們是兩種**不同的起法**，不是一個旗標加一個選項 —— 所以兩個都帶不是「以某一邊為準」，
/// 是使用者搞錯了，🚫 我們不替他猜（A5：不確定就拒絕）。
#[derive(Debug)]
enum StartMode {
    /// `-s`：常駐、開 RPC、**會寫**（所以啟動時就要拿寫權）。
    Serve,
    /// 沒有 `-s`：跑一個命令就結束。⚠️ 還沒實作。
    OneShot(Vec<String>),
}

/// Args:
///     serve: 有沒有 `-s`, example: true
///     command: 尾巴的單發命令, example: vec!["account".into(), "list".into()]
/// Return:
///     Ok(StartMode)   兩種起法之一
///     Err(String)     兩個都帶、或兩個都沒帶；字串就是要印給使用者的那句
fn start_mode(serve: bool, command: Vec<String>) -> Result<StartMode, String> {
    match (serve, command.is_empty()) {
        (true, true) => Ok(StartMode::Serve),
        (false, false) => Ok(StartMode::OneShot(command)),
        (true, false) => Err(format!(
            "-s starts the daemon and a command runs one-shot;              these are the two ways to start it, so pass one or the other (got both: {})",
            command.join(" ")
        )),
        (false, true) => Err(
            "nothing to do: pass -s to serve, or a command to run one-shot".to_string(),
        ),
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match start_mode(cli.serve, cli.command) {
        Ok(StartMode::Serve) => {}
        Ok(StartMode::OneShot(command)) => {
            eprintln!(
                "single-shot commands are not implemented yet (got: {}); only `-s` works today",
                command.join(" ")
            );
            return ExitCode::from(1);
        }
        Err(complaint) => {
            eprintln!("{complaint}");
            return ExitCode::from(1);
        }
    }
    let token_path = cli
        .token_file
        .unwrap_or_else(|| cli.data_dir.join("daemon.token"));
    // Unix 上別人讀得到就拒絕跑：token ＝ 整個 RPC 的憑證，fail closed（🚫 不只印警告）。
    match wbf_daemon::token::is_private(&token_path) {
        Ok(true) => {}
        Ok(false) => {
            eprintln!(
                "the daemon token at {} is readable by other users; make it 0600",
                token_path.display()
            );
            return ExitCode::from(1);
        }
        Err(error) => {
            eprintln!(
                "cannot stat the daemon token at {}: {error}",
                token_path.display()
            );
            return ExitCode::from(1);
        }
    }
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

    let ready_path = cli.data_dir.join("daemon.json");
    // 沒有 runtime 就沒有這支 daemon（維護者的規矩裡「必須要開」的那個層級）——但停也要講清楚、給 exit code，🚫 不 panic。
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("cannot start the async runtime: {error}");
            return ExitCode::from(1);
        }
    };
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

        // 🚨 **`-s` 的第一件事：拿寫權**（architecture-v2 §0.2）。`-s` 就是「我要寫」的意思，
        // 所以🚫 不等到第一個寫請求才拿 —— 拿不到就不該啟動（fail closed）。
        // ⚠️ 寫權活在 `handle` 裡，所以它要一路拿到程序結束。
        if let Err(error) = handle.grant_write_access() {
            eprintln!("{error}");
            return ExitCode::from(1);
        }

        // ⚠️ 這一步**在拿到寫權之後**：舊的 `daemon.json` 也是這個目錄的檔，沒有寫權就去刪它，
        // 刪的可能是另一個 daemon 正在用的那份。
        // 刪不掉就不要跑 —— 那代表前端會拿到一份我們沒寫過的 port（fail closed）。
        match std::fs::remove_file(&ready_path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                eprintln!("cannot remove the stale {}: {error}", ready_path.display());
                return ExitCode::from(1);
            }
        }

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
        // `instance` 是這次啟動的 UUID：port 會被重複使用、pid 會被回收，**它不會** ——
        // 前端拿它回答「我現在講話的還是剛才那一個 daemon 嗎」（維護者 2026-09-13）。
        let info = serde_json::json!({
            "rpc_port": rpc_port,
            "data_port": 0,
            "pid": std::process::id(),
            "instance": handle.instance(),
        });
        // temp＋rename：前端可能正在 watch 這個檔，🚫 不讓它讀到寫一半的。
        if let Err(error) = wbf_sdk::vault::write_private(&ready_path, info.to_string().as_bytes())
        {
            eprintln!("cannot write daemon.json: {error}");
            return ExitCode::from(1);
        }
        // 第 3 步的 ready 訊號。stdout 一行 JSON 給 spawn 我們的那個程序（它拿得到 pipe，
        // 不必去 watch 檔案）；stderr 那行是給人看的。🚫 stdout 只有這一行，別的都走 stderr。
        println!(
            "{}",
            serde_json::json!({
                "ready": true,
                "rpc_port": rpc_port,
                "data_port": 0,
                "pid": std::process::id(),
                "instance": handle.instance(),
            })
        );
        eprintln!("listening on ws://127.0.0.1:{rpc_port}");
        eprintln!(
            "ready; now shred {} (the daemon will not read it again)",
            token_path.display()
        );
        server.run().await;
        let _ = std::fs::remove_file(&ready_path);
        ExitCode::SUCCESS
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 啟動路徑只有兩種，而且**互斥**（architecture-v2 §0.2）。
    #[test]
    fn there_are_exactly_two_ways_to_start_and_they_are_mutually_exclusive() {
        assert!(matches!(start_mode(true, Vec::new()), Ok(StartMode::Serve)));
        match start_mode(false, vec!["account".to_string(), "list".to_string()]) {
            Ok(StartMode::OneShot(command)) => assert_eq!(command, ["account", "list"]),
            other => panic!("沒有 -s 就是單發：{}", describe(&other)),
        }
        // 🚫 兩個都帶不猜：那是使用者搞錯了。
        let both = start_mode(true, vec!["account".to_string()]);
        assert!(both.is_err(), "-s 加命令應該報錯");
        assert!(
            both.unwrap_err().contains("one or the other"),
            "錯誤訊息要講得出怎麼改"
        );
        // 什麼都沒帶也不猜。
        assert!(start_mode(false, Vec::new()).is_err());
    }

    fn describe(mode: &Result<StartMode, String>) -> String {
        match mode {
            Ok(StartMode::Serve) => "serve".to_string(),
            Ok(StartMode::OneShot(command)) => format!("one-shot {command:?}"),
            Err(complaint) => complaint.clone(),
        }
    }
}

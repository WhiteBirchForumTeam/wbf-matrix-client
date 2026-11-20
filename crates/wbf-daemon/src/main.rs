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
//! `wbf-matrix-client-daemon`：只有 `-s`（常駐）：控制平面（RPC）與資料平面（/docs/design/rpc-specs/data-plane.md）兩個 port。
//! 單發命令（`daemon <命令>`）**不做**（維護者 2026-11-17，/docs/design/rpc-specs/rpc-cli.md §4）：「在腳本裡跑一條命令」由 rpc-cli 的 `call` 做。
//!
//! token 照 /docs/design/rpc-specs/local-interface.md §3：`WBF_DAEMON_TOKEN` 有指定就用（讀完清掉）、沒指定就自己產生，
//! 都寫進 `<data dir>/daemon.token` 給前端讀。⭐ 「宣告 ready」是**一個邊緣**，不是一個狀態：`daemon.json` 在綁定**之前**先刪掉，
//! 綁好、`daemon.token` 寫好之後才 temp＋rename 寫進去 —— 前端等的那個檔出現的瞬間，port 一定已經在聽、token 一定已經在，
//! 而且它一定不是上一次留下來的。

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use clap::Parser;
use wbf_daemon::connection::EncryptionPolicy;
use wbf_daemon::data_plane::{AccessKeys, DataServer};
use wbf_daemon::handle::Handle;
use wbf_daemon::pack::RpcKeys;
use wbf_daemon::server::RpcServer;
use wbf_daemon::settings::Settings;

#[derive(Parser)]
#[command(
    name = "wbf-matrix-client-daemon",
    version,
    about = "wbfuwunel client 本體：常駐、持有本地資料庫、開 RPC 給前端。token：環境變數 WBF_DAEMON_TOKEN 指定，沒指定就自己產生；都寫進 <data dir>/daemon.token"
)]
struct Cli {
    /// 常駐：開本地 RPC 的 WS，等前端來連（長旗標**不**叫 `--server`：那個字只指 homeserver，/docs/design/overview/architecture-v2.md §0.1）
    #[arg(short = 's', long = "serve")]
    serve: bool,
    /// 資料目錄
    #[arg(long, env = "WBF_DATA_DIR")]
    data_dir: PathBuf,
    /// RPC 的 port；0 就隨機，寫進 <data dir>/daemon.json
    #[arg(long, default_value_t = 0)]
    rpc_port: u16,
    /// 資料平面（媒體 bytes 的 HTTP）的 port；0 就隨機，寫進 <data dir>/daemon.json
    #[arg(long, default_value_t = 0)]
    data_port: u16,
    /// conf 檔在哪；沒給就找 <data dir>/wbf.conf（不在就生成一份）。⚠️ 明指了卻不在就報錯，不 fallback
    #[arg(long, env = "WBF_CONFIG")]
    config: Option<PathBuf>,
}

fn main() -> ExitCode {
    // 🚨 第一件事：讀指定的 token、從環境裡清掉。⚠️ 要在起 runtime、開任何執行緒之前（`wbf_daemon::token::take_token_from_env`）。
    // 也排在解析參數之前：參數錯了一樣要清，不能讓 token 留在環境裡。
    let given_token = wbf_daemon::token::take_token_from_env();
    let cli = Cli::parse();
    if !cli.serve {
        eprintln!("nothing to do: pass -s to serve (single-shot commands are not a thing; use wbf-matrix-rpc-cli)");
        return ExitCode::from(1);
    }
    // 沒指定就自己產生一把；指定了卻壞掉就不啟動（**不**默默改用自己產生的：那個前端會拿著對不上的 token 連不進來）。
    let token = match given_token {
        Ok(Some(token)) => token,
        Ok(None) => match wbf_daemon::token::new_token() {
            Ok(token) => zeroize::Zeroizing::new(token.to_vec()),
            Err(problem) => {
                eprintln!("{problem}");
                return ExitCode::from(1);
            }
        },
        Err(problem) => {
            eprintln!("{problem}");
            return ExitCode::from(1);
        }
    };
    // 長度在 `take_token_from_env` 已經驗過；這裡再驗一次（呼叫端不假定上游洗乾淨了）。
    let keys = match RpcKeys::from_token_checking_length(&token) {
        Ok(keys) => Arc::new(keys),
        Err(actual) => {
            eprintln!(
                "the daemon token is {actual} bytes; it must be exactly {} (/docs/design/rpc-specs/local-interface.md §3)",
                wbf_daemon::pack::TOKEN_LEN
            );
            return ExitCode::from(1);
        }
    };
    // 資料平面的 URL 也用同一份 token 導鑰（/docs/design/rpc-specs/data-plane.md §2）。token 本身留到寫進 daemon.token 為止。
    let access_keys = AccessKeys::from_token(&token);

    let ready_path = cli.data_dir.join("daemon.json");
    // 沒有 runtime 就沒有這支 daemon（維護者的規矩裡「必須要開」的那個層級）——但停也要講清楚、給 exit code，**不** panic。
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("cannot start the async runtime: {error}");
            return ExitCode::from(1);
        }
    };
    let exit = runtime.block_on(async move {
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
        // 沒用 `--config` 指別的檔時，資料目錄裡沒有 `wbf.conf` 就生成一份起手式（寫在拿到寫權之後）。
        let starter_conf = cli
            .config
            .is_none()
            .then(|| settings.starter_conf_entries());
        let policy = EncryptionPolicy::enforced();
        let handle = Handle::new(&cli.data_dir, policy.clone(), settings);
        handle.set_access_keys(access_keys);

        // 🚨 **`-s` 的第一件事：拿寫權**（/docs/design/overview/architecture-v2.md §0.2）。`-s` 就是「我要寫」的意思，
        // 所以**不**等到第一個寫請求才拿 —— 拿不到就不該啟動（fail closed）。
        // ⚠️ 寫權活在 `handle` 裡，所以它要一路拿到程序結束。
        if let Err(error) = handle.grant_write_access() {
            eprintln!("{error}");
            return ExitCode::from(1);
        }
        // 寫不成只講一聲：conf 是給人改的起手式，沒有它照樣用內建預設跑。已經存在的永遠不改寫。
        if let Some(entries) = starter_conf {
            match wbf_core::conf::write_if_absent(&cli.data_dir, &entries) {
                Ok(true) => eprintln!(
                    "wrote {} with the built-in defaults; edit it or delete it, it will not be rewritten",
                    cli.data_dir.join(wbf_core::conf::CONF_FILE_NAME).display()
                ),
                Ok(false) => {}
                Err(error) => eprintln!("cannot write the starter wbf.conf: {error}"),
            }
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
        let data_server = match DataServer::bind(cli.data_port, handle.clone()).await {
            Ok(data_server) => data_server,
            Err(error) => {
                eprintln!("cannot bind the data plane port: {error}");
                return ExitCode::from(1);
            }
        };
        let data_port = data_server.local_addr().map(|addr| addr.port()).unwrap_or(0);
        handle.set_ports(rpc_port, data_port).await;
        // `instance` 是這次啟動的 UUID：port 會被重複使用、pid 會被回收，**它不會** ——
        // 前端拿它回答「我現在講話的還是剛才那一個 daemon 嗎」（維護者 2026-09-13）。
        let info = serde_json::json!({
            "rpc_port": rpc_port,
            "data_port": data_port,
            "pid": std::process::id(),
            "instance": handle.instance(),
        });
        // token 先寫（蓋掉上一支留下的）、daemon.json 後寫：前端看到 daemon.json 時 token 一定已經在。寫不了就不宣告 ready（前端連不上）。
        if let Err(problem) = wbf_daemon::token::write_token_file(&cli.data_dir, &token) {
            eprintln!("{problem}");
            return ExitCode::from(1);
        }
        drop(token);
        // temp＋rename：前端可能正在 watch 這個檔，**不**讓它讀到寫一半的。
        if let Err(error) = wbf_sdk::vault::write_private(&ready_path, info.to_string().as_bytes())
        {
            eprintln!("cannot write daemon.json: {error}");
            return ExitCode::from(1);
        }
        // 第 3 步的 ready 訊號。stdout 一行 JSON 給 spawn 我們的那個程序（它拿得到 pipe，
        // 不必去 watch 檔案）；stderr 那行是給人看的。stdout **只有**這一行，別的都走 stderr。
        println!(
            "{}",
            serde_json::json!({
                "ready": true,
                "rpc_port": rpc_port,
                "data_port": data_port,
                "pid": std::process::id(),
                "instance": handle.instance(),
            })
        );
        eprintln!("listening on ws://127.0.0.1:{rpc_port} (RPC) and http://127.0.0.1:{data_port} (data plane)");
        // Ctrl+C（Windows 的 console 事件、Unix 的 SIGINT）與 SIGTERM 走同一條路：等於 `daemon.shutdown`。
        tokio::spawn(shutdown_on_signal(handle.clone()));
        tokio::join!(server.run(), data_server.run());
        // 關機（/docs/design/daemon/task-supervision.md §2.3）：每一步等完才下一步，等不到講一聲、往下走；禁止直接丟掉 runtime。
        handle.in_flight().finish_requests().await;
        handle.in_flight().finish_connections().await;
        handle.in_flight().finish_background().await;
        handle.core().await.shutdown().await;
        // 正常結束順便收掉；被強制殺掉時留著無所謂，下一支起來會蓋掉（維護者 2026-11-20）。
        let _ = std::fs::remove_file(&ready_path);
        let _ = std::fs::remove_file(wbf_daemon::token::token_file_path(&cli.data_dir));
        ExitCode::SUCCESS
    });
    // 到這裡該停的都停了；還沒停的（等不到、已經講過）給它們最後一段時間，之後才結束程序。
    runtime.shutdown_timeout(wbf_sdk::task_stop::STOP_TIMEOUT);
    exit
}

/// 收到 Ctrl+C／SIGTERM 就開始關機（跟 `daemon.shutdown` 同一條路，只是沒有回應要先送）。收訊號的接口開不起來就講一聲，照樣跑（`daemon.shutdown` 還在）。
/// 已經在關（`daemon.shutdown` 先來）就自己結束。
async fn shutdown_on_signal(handle: std::sync::Arc<Handle>) {
    let already_shutting_down = handle.wait_until_shutting_down();
    #[cfg(unix)]
    let terminated = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut terminate) => {
                terminate.recv().await;
            }
            Err(error) => {
                eprintln!("cannot listen for SIGTERM ({error}); use daemon.shutdown or Ctrl+C");
                std::future::pending::<()>().await;
            }
        }
    };
    #[cfg(not(unix))]
    let terminated = std::future::pending::<()>();
    tokio::select! {
        interrupted = tokio::signal::ctrl_c() => {
            if let Err(error) = interrupted {
                eprintln!("cannot listen for Ctrl+C ({error}); use daemon.shutdown");
                return;
            }
        }
        () = terminated => {}
        () = already_shutting_down => return,
    }
    eprintln!("shutting down (signal)");
    handle.request_shutdown();
}

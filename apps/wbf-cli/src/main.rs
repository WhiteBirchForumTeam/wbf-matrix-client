//! wbf-cli：介面照 `docs/design/wbf-cli-spec.md`。這個檔只有參數定義、分派、exit code；
//! 每個命令在 `commands.rs`，session 檔在 `session.rs`。

mod commands;
mod session;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};
use wbf_sdk::SdkError;

/// CLI 規格 §2 的全域參數。
#[derive(Parser)]
#[command(name = "wbf-cli", version, about = "wbfuwunel 的命令列")]
pub struct Cli {
    /// homeserver 的 base URL，example: http://localhost:6167；沒給用 session 檔的
    #[arg(long, global = true, env = "WBF_SERVER")]
    pub server: Option<String>,
    /// 直接給 access token，跳過 session 檔。不印、不寫進任何輸出
    #[arg(long, global = true, env = "WBF_ACCESS_TOKEN", hide_env_values = true)]
    pub token: Option<String>,
    /// session 檔位置
    #[arg(long, global = true, env = "WBF_SESSION")]
    pub session: Option<PathBuf>,
    /// stdout 只印 JSON（預設就是；現在是刻意的 no-op，留著是為了之後加人類可讀模式時介面不變，CLI 規格 §2）
    #[arg(long, global = true)]
    pub json: bool,
    /// stderr 不印進度
    #[arg(long, global = true)]
    pub quiet: bool,
    /// ws（預設）或 http
    #[arg(long, global = true, default_value = "ws", value_parser = ["ws", "http"])]
    pub transport: String,
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// 登入、寫 session 檔
    Login {
        /// mxid 或 localpart，example: @alice:localhost
        #[arg(long)]
        user: String,
        /// 整檔就是密碼；沒給就從終端讀（不回顯）
        #[arg(long)]
        password_file: Option<PathBuf>,
        #[arg(long, default_value = "wbf-cli")]
        device_name: String,
    },
    /// 讓 token 失效，刪 session 檔
    Logout,
    Whoami,
    /// Hello 加 Ping，印 server 的 features 與上限
    Ping,
    /// 固定大小上傳（可續傳），或 --stream 從 stdin 串流
    Upload(UploadArgs),
    /// 印 Status 的 Ack
    Status {
        upload_id: u64,
    },
    /// 送 Abort；給 --file 就順便刪它旁邊的狀態檔
    Abort {
        upload_id: u64,
        #[arg(long)]
        file: Option<PathBuf>,
    },
    /// 印 Info 的 Ack；有 manifest 就解描述並核對
    Info {
        mxc: String,
        #[arg(long)]
        manifest: Option<PathBuf>,
    },
    /// 整檔下載，全部檢查照約定 §3.1
    Download {
        #[arg(long)]
        manifest: PathBuf,
        #[arg(short, long)]
        out: Option<PathBuf>,
    },
    /// 只讀含 --at 的那一塊，明文寫到 stdout（CLI 規格 §3.3.1）
    Seek {
        #[arg(long)]
        manifest: PathBuf,
        #[arg(long)]
        at: u64,
        #[arg(long)]
        len: Option<u64>,
    },
}

#[derive(Args)]
pub struct UploadArgs {
    /// 要上傳的檔；--stream 時不給
    pub file: Option<PathBuf>,
    /// 從 stdin 讀、大小未知
    #[arg(long)]
    pub stream: bool,
    /// chacha20-poly1305、aes-256-gcm、none；預設依硬體
    #[arg(long)]
    pub cipher: Option<String>,
    /// 明文塊大小；沒給照約定 §2 的表
    #[arg(long)]
    pub chunk_size: Option<u32>,
    /// 串流的線路：mobile（預設）或 wifi
    #[arg(long, default_value = "mobile", value_parser = ["mobile", "wifi"])]
    pub link: String,
    /// manifest 寫到這裡；沒給印到 stdout
    #[arg(long)]
    pub manifest: Option<PathBuf>,
    /// 固定大小上傳也算整檔 SHA-256（串流一律算）
    #[arg(long)]
    pub sha256: bool,
    /// 描述的檔名（串流用；固定大小預設用檔名）
    #[arg(long)]
    pub name: Option<String>,
    #[arg(long)]
    pub mimetype: Option<String>,
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    match commands::run(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::from(exit_code(&error))
        }
    }
}

/// CLI 規格 §4。
///
/// Args:
///     error: example: SdkError::Integrity("...".into())
/// Return:
///     u8  1 用法／IO、2 server 拒絕或不講協議、3 完整性、4 網路
fn exit_code(error: &SdkError) -> u8 {
    match error {
        SdkError::Usage(_) | SdkError::Io(_) => 1,
        SdkError::Server { .. } | SdkError::Protocol(_) => 2,
        SdkError::Integrity(_) => 3,
        SdkError::Network(_) => 4,
    }
}

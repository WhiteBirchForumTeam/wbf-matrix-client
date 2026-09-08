//! wbf-cli：介面照 `docs/design/wbf-cli-spec.md`。這個檔只有參數定義、分派、exit code；
//! 每個命令在 `commands.rs`（第 2 步）、`rooms.rs`（第 3 步）、`recent.rs`（快取進料）；vault 怎麼解鎖在 `unlock.rs`，
//! 每個帳號的資料放哪在 `accounts.rs`。

mod accounts;
mod commands;
mod recent;
mod rooms;
mod unlock;

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
    /// 資料目錄（local.key、current、servers/<host>/cache.db、servers/<host>/accounts/<user>/…），預設見 CLI 規格 §7
    #[arg(long, global = true, env = "WBF_DATA_DIR")]
    pub data_dir: Option<PathBuf>,
    /// 用哪個帳號（mxid 或 localpart）；沒給就是最後一次 login 的那個。同名 localpart 在多個 server 時要配 --server
    #[arg(long, global = true, env = "WBF_ACCOUNT")]
    pub account: Option<String>,
    /// 整檔就是 passphrase（解 local.key 的那句話，不是 Matrix 帳號密碼）；沒給就看 unlock ticket，再沒有就從終端讀
    #[arg(long, global = true, env = "WBF_PASSPHRASE_FILE")]
    pub passphrase_file: Option<PathBuf>,
    /// passphrase 解鎖成功後 unlock ticket 的有效秒數；0 就不寫 ticket
    #[arg(long, global = true, default_value_t = 900)]
    pub unlock_ttl: u64,
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
    /// 讓 token 失效；刪這個帳號的 session.sealed 與 matrix/，cache.db 留著（這個 server 最後一個帳號登出時才刪）
    Logout,
    /// 列出資料目錄裡的帳號（不開 vault）
    Accounts,
    /// 摧毀一個帳號在 cache.db 裡的本機紀錄（它同步過的事件、房間清單、已讀位置；別的帳號也同步過的事件留著）
    ForgetAccount {
        /// 完整 mxid，example: @alice:localhost
        user: String,
        /// 不問確認（腳本用）
        #[arg(long)]
        yes: bool,
    },
    /// 刪 unlock ticket；下一個命令會再問 passphrase
    Lock,
    /// 給 local.key 設（或改）passphrase；沒給檔就從終端讀兩次
    SetPassphrase {
        #[arg(long)]
        new_passphrase_file: Option<PathBuf>,
    },
    /// 拿掉 passphrase，local.key 回到明文（plain）模式
    RemovePassphrase,
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
    /// 整檔下載，全部檢查照約定 §3.1。登入中就走媒體快取：池裡有就不連 server，沒有就邊下邊進池（local-cache-db §8）
    Download {
        #[arg(long)]
        manifest: PathBuf,
        #[arg(short, long)]
        out: Option<PathBuf>,
        /// 不經媒體快取，直接寫到 --out（--token 模式本來就這樣）
        #[arg(long)]
        no_cache: bool,
    },
    /// 媒體快取的狀態：池的大小、幾個檔、半成品（CLI 規格 §3.5）
    MediaStats,
    /// 媒體快取清理：超過配額就從最久沒用的刪，保護期內不刪（local-cache-db §8.5）；順便掃孤兒
    MediaGc {
        /// 配額，MiB；預設 2048
        #[arg(long, default_value_t = 2048)]
        quota_mib: u64,
        /// 保護期，天；預設 7
        #[arg(long, default_value_t = 7)]
        protect_days: u64,
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
    /// 列出加入的房間（CLI 規格 §3.4）
    Rooms,
    /// 送文字或檔案進房間；檔案先上傳再送約定 §5 的事件
    Send(SendArgs),
    /// 等新事件，來一則立刻印一則，JSON Lines（CLI 規格 §3.4.2）
    Watch(WatchArgs),
    /// Event/Recent：把 cache.db 水位線之後的事件跨房間拉回來寫進快取，直到追平或湊滿 --limit（CLI 規格 §3.5）
    Recent {
        /// 這一輪總共最多幾則（上層要的數量）；0 = 拉到追平為止
        #[arg(long, default_value_t = 10000)]
        limit: u64,
        /// 底層一次 Recent 要一窗幾則；server 上限 500（Hello 會說），超過先 clamp
        #[arg(long, default_value_t = 320)]
        window: u32,
        /// 每個 Batch 幾則；沒給用 server 預設（10），上限 100
        #[arg(long)]
        batch: Option<u32>,
        /// 不帶 cg_seq：從 server 最新的往回拉，不管快取裡已經有什麼
        #[arg(long)]
        from_scratch: bool,
    },
    /// 歷史，從最新往回（CLI 規格 §3.4.1）
    Read {
        room: String,
        #[arg(long, default_value_t = 50)]
        limit: u32,
        /// 接上一頁印的 next；--from-cache 時是 r_seq（只要比它小的）
        #[arg(long)]
        before: Option<String>,
        /// 不連 server，從 cache.db 讀（CLI 規格 §3.5）
        #[arg(long)]
        from_cache: bool,
        /// client 端過濾：事件 type，可多個
        #[arg(long = "type")]
        types: Vec<String>,
        #[arg(long)]
        sender: Option<String>,
    },
    /// 只留分塊檔事件，印 manifest；--save 一個事件存一個 <event_id>.json
    Files {
        room: String,
        #[arg(long, default_value_t = 50)]
        limit: u32,
        #[arg(long)]
        before: Option<String>,
        #[arg(long)]
        save: Option<PathBuf>,
        /// 不連 server，從 cache.db 讀（CLI 規格 §3.5）
        #[arg(long)]
        from_cache: bool,
    },
}

#[derive(Args)]
pub struct SendArgs {
    pub room: String,
    #[arg(long, conflicts_with = "file")]
    pub text: Option<String>,
    /// 先 upload 再送事件；upload 的參數照用
    #[arg(long)]
    pub file: Option<PathBuf>,
    /// 檔案訊息的說明文字
    #[arg(long)]
    pub caption: Option<String>,
    /// 非加密房間送檔案要確認；給了就跳過（腳本用）
    #[arg(long)]
    pub yes: bool,
    #[arg(long)]
    pub cipher: Option<String>,
    #[arg(long)]
    pub chunk_size: Option<u32>,
    #[arg(long)]
    pub sha256: bool,
    /// 上傳的 manifest 也寫一份到這裡（含 key，機密）
    #[arg(long)]
    pub manifest: Option<PathBuf>,
}

#[derive(Args)]
pub struct WatchArgs {
    pub room: String,
    /// tail（不結束）、wait（等幾秒）、once（印到第一則就停）
    #[arg(value_parser = ["tail", "wait", "once"])]
    pub mode: String,
    /// wait 的秒數
    pub seconds: Option<u64>,
    /// 接上次結束時 stderr 印的 since
    #[arg(long)]
    pub since: Option<String>,
    /// once 的上限秒數；到了還沒有 exit 5。不帶就等到有別人的訊息為止
    #[arg(long)]
    pub timeout: Option<u64>,
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

/// 主執行緒的棧在 Windows 只有 1 MB，debug build 裡 matrix-sdk 送訊息那條 async 路徑會爆棧（2026-09-07 實跑：`send` 直接
/// "overflowed its stack"）。所以 runtime 跑在自己開的執行緒上，棧給 64 MiB；release 不需要但一致比較不會踩到。
const MAIN_THREAD_STACK_BYTES: usize = 64 * 1024 * 1024;

fn main() -> ExitCode {
    let cli = Cli::parse();
    let worker = std::thread::Builder::new()
        .name("wbf-cli-main".into())
        .stack_size(MAIN_THREAD_STACK_BYTES)
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .thread_stack_size(MAIN_THREAD_STACK_BYTES)
                .build()
                .expect("tokio runtime");
            runtime.block_on(commands::run(cli))
        })
        .expect("spawn main thread");
    match worker.join() {
        Ok(Ok(())) => ExitCode::SUCCESS,
        Ok(Err(error)) => {
            eprintln!("error: {error}");
            ExitCode::from(exit_code(&error))
        }
        Err(_) => {
            eprintln!("error: wbf-cli main thread panicked");
            ExitCode::from(1)
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
        SdkError::Timeout(_) => 5,
    }
}

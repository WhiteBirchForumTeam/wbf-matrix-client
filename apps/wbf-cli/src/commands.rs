//! 每個命令一個函數，照 CLI 規格 §3。stdout 只有結果 JSON（`seek` 例外：明文 bytes），進度與警告在 stderr。

use std::io::Write;
use std::path::{Path, PathBuf};

use serde_json::json;

use wbf_core::conf::{Conf, Entry};
use wbf_core::Core;
use wbf_core::UploadRequest;
use wbf_core::{CoreError, CoreErrorKind};
use wbf_sdk::login::{logout, whoami};
use wbf_sdk::{Channel, Manifest, Session, Transport, WbfClient};

use crate::rooms::{json_value_of, print_value, set_field};
use crate::unlock::{
    default_data_dir, prompt_new_passphrase, prompt_password_on_terminal, read_passphrase_file,
    read_password_file, UnlockOptions,
};
use crate::{AccountAction, Cli, Command, KeyBackupAction, LoginArgs, RecoveryAction, UploadArgs};

pub const CLIENT_NAME: &str = concat!("wbf-cli/", env!("CARGO_PKG_VERSION"));

pub async fn run(cli: Cli) -> Result<(), CoreError> {
    let context = Context::from(&cli)?;
    let result = dispatch(&context, cli.command).await;
    if result.is_ok() {
        context.write_conf_if_asked_for()?;
    }
    result
}

/// ⚠️ 自動生成（§10.3）在**這裡之後**：三個條件之一是「這次命令成功結束」。
async fn dispatch(context: &Context, command: Command) -> Result<(), CoreError> {
    match command {
        Command::Login(args) => login_command(context, &args).await,
        Command::Logout {
            accept_history_loss,
        } => {
            // `--token` 模式沒有帳號目錄，只讓 server 端的 token 失效。
            if let Some(session) = context.token_session().await {
                logout(&session?).await?;
                return print_json(&json!({ "ok": true }));
            }
            // `logout` 就是 `account del <current 帳號>`（CLI 規格 §3.1）。
            let current = context.core()?.current_user_id()?;
            let result = log_out(context, &current, accept_history_loss).await?;
            print_json(&json!({ "ok": true, "user": result.user }))
        }
        Command::Account { action } => account_command(context, action).await,
        Command::KeyBackup { action } => key_backup_command(context, action).await,
        Command::Recovery { action } => recovery_command(context, action),
        Command::SetPassphrase {
            new_passphrase_file,
        } => {
            let passphrase = match new_passphrase_file {
                Some(path) => read_passphrase_file(&path)?,
                None => prompt_new_passphrase()?,
            };
            let mode = context.core()?.set_passphrase(Some(&passphrase))?;
            print_json(&json!({ "ok": true, "mode": mode }))
        }
        Command::RemovePassphrase => {
            let mode = context.core()?.set_passphrase(None)?;
            print_json(&json!({ "ok": true, "mode": mode }))
        }
        Command::Whoami => {
            let who = match context.token_session().await {
                Some(session) => {
                    let session = session?;
                    wbf_core::WhoAmI {
                        user_id: session.user_id,
                        device_id: session.device_id,
                        server: session.server,
                    }
                }
                None => context.core()?.whoami(&context.target()).await?,
            };
            print_value(&who)
        }
        Command::Ping => {
            let hello = context
                .core()?
                .ping(context.transport, CLIENT_NAME, &context.target())
                .await?;
            print_value(&hello)
        }
        Command::Upload(args) => upload_command(context, &args).await,
        Command::Status { upload_id } => {
            let status = context
                .core()?
                .upload_status(upload_id, context.transport, &context.target())
                .await?;
            print_value(&status)
        }
        Command::Abort { upload_id, file } => {
            context
                .core()?
                .abort_upload(
                    upload_id,
                    file.as_deref(),
                    context.transport,
                    &context.target(),
                )
                .await?;
            print_json(&json!({ "ok": true }))
        }
        Command::Info { mxc, manifest } => info_command(context, &mxc, manifest.as_deref()).await,
        Command::Download {
            manifest,
            out,
            no_cache,
        } => download_command(context, &manifest, out, no_cache).await,
        Command::MediaStats => media_stats_command(context).await,
        Command::MediaGc {
            quota_mib,
            protect_days,
        } => {
            let mut warnings = Vec::new();
            let quota_mib = quota_mib
                .unwrap_or_else(|| context.conf.get_number("QUOTA_MIB", 2048, &mut warnings));
            let protect_days = protect_days
                .unwrap_or_else(|| context.conf.get_number("PROTECT_DAYS", 7, &mut warnings));
            context.warn(&warnings);
            media_gc_command(context, quota_mib, protect_days).await
        }
        Command::Seek { manifest, at, len } => seek_command(context, &manifest, at, len).await,
        Command::Rooms => crate::rooms::rooms_command(context).await,
        Command::Send(args) => crate::rooms::send_command(context, &args).await,
        Command::Watch(args) => crate::rooms::watch_command(context, &args).await,
        Command::Recent {
            limit,
            window,
            batch,
            from_scratch,
        } => {
            let mut warnings = Vec::new();
            // ⚠️ conf 的鍵叫 MAX_EVENTS（跟 `RecentPlan` 的欄位同名），旗標叫 `--limit`。
            let limit = limit
                .unwrap_or_else(|| context.conf.get_number("MAX_EVENTS", 10_000, &mut warnings));
            let window =
                window.unwrap_or_else(|| context.conf.get_number("WINDOW", 320, &mut warnings));
            context.warn(&warnings);
            let plan = wbf_sdk::RecentPlan {
                max_events: (limit > 0).then_some(limit),
                window,
                batch,
            };
            crate::recent::recent_command(context, plan, from_scratch).await
        }
        Command::Read {
            room,
            limit,
            before,
            types,
            sender,
            from_cache,
        } => {
            crate::rooms::read_command(
                context,
                &room,
                limit,
                before.as_deref(),
                from_cache,
                &types,
                sender.as_deref(),
            )
            .await
        }
        Command::Files {
            room,
            limit,
            before,
            save,
            from_cache,
        } => {
            crate::rooms::files_command(
                context,
                &room,
                limit,
                before.as_deref(),
                from_cache,
                save.as_deref(),
            )
            .await
        }
    }
}

/// `login` 要讀哪個 password 檔：旗標沒給就用 conf 的 `PASSWORD_FILE`（CLI 規格 §10.5）。
///
/// ⚠️ 存進 conf 的是**路徑**，秘密是那個檔的**內容**——它從來不進 conf，自動生成也不寫這個鍵。
///
/// Args:
///     args: example: &LoginArgs { .. }
///     conf: example: &context.conf
/// Return:
///     Some(PathBuf)  旗標給的，或 conf 寫的
///     None           兩邊都沒有——從終端問
fn find_password_file(args: &LoginArgs, conf: &Conf) -> Option<PathBuf> {
    args.password_file
        .clone()
        .or_else(|| conf.find("PASSWORD_FILE").map(PathBuf::from))
}

/// 開關型的值寫回 conf 時長什麼樣（§10.4 只認得這兩個字）。
fn on_off(value: bool) -> String {
    if value { "on" } else { "off" }.to_string()
}

/// 全域參數解析完的樣子：server 與 token 從哪來，只在這裡決定一次。
pub struct Context {
    pub unlock: UnlockOptions,
    /// 常駐狀態（architecture-v2 §7）。⚠️ 現在一個命令建一個、命令結束就丟；
    /// daemon 接手之後它會活過整個程序，而這裡的程式碼不必改——這正是先做 `wbf-core` 的理由。
    ///
    /// 一個命令只解鎖一次：`session()` 與房間命令的 store 都從它拿，不然 Argon2 跑兩次。
    core: Core,
    pub account_override: Option<String>,
    pub server_override: Option<String>,
    pub token_override: Option<String>,
    pub quiet: bool,
    pub transport: Transport,
    /// 這次讀到的 conf（CLI 規格 §10）。命令自己的旗標沒給時從這裡拿預設。
    pub conf: Conf,
    /// `SERVER_BACKUP`：標準 Matrix key backup 開著嗎（local-cache-db §10.3）。
    /// ⚠️ 認不得的值落到 `true`——壞掉要壞在「備份還開著」那一邊。
    pub server_backup: bool,
    /// `LOCAL_ROOM_KEYS`：本地全量快照開著嗎（同 §10.4）。同樣落到 `true`。
    pub local_room_keys: bool,
    /// 這次實際生效的值，給自動生成用（§10.3）。🚫 裡面沒有秘密。
    effective: Vec<wbf_core::conf::Entry>,
    /// `--data-dir`／`WBF_DATA_DIR` 有給嗎——自動生成的三個條件之一。
    data_dir_was_given: bool,
    /// 備份關掉的警告一個命令只印一次（`rooms::backend` 可能被叫不只一次）。
    warned_about_backups: std::sync::OnceLock<()>,
}

/// conf 認得的鍵。⚠️ 加新鍵時要回來加一筆，不然它會被當成「認不得」印警告（§10.4）——
/// 那是**警告**不是錯誤，所以漏掉只會吵，不會讓命令壞掉。
const KNOWN_CONF_KEYS: &[&str] = &[
    "SERVER",
    "ACCOUNT",
    "TRANSPORT",
    "PASSPHRASE_FILE",
    "PASSWORD_FILE",
    "SERVER_BACKUP",
    "LOCAL_ROOM_KEYS",
    "QUOTA_MIB",
    "PROTECT_DAYS",
    "MAX_EVENTS",
    "WINDOW",
];

impl Context {
    /// 優先序：**旗標 > 環境變數 > conf > 內建預設**（CLI 規格 §10.2），每個值各自比一次。
    ///
    /// 旗標與環境變數由 clap 合在一起處理（`env = "WBF_…"`），所以這裡看到 `None` 就是
    /// 「兩者都沒給」——conf 接手。⚠️ 這也是那幾個旗標拿掉 clap 預設值的理由：留著預設值
    /// 就永遠不是 `None`，conf 會被一個「使用者根本沒打」的值蓋掉。
    fn from(cli: &Cli) -> Result<Context, CoreError> {
        // ⚠️ 資料目錄不能從 conf 來：conf 就在它裡面（§10.1）。
        let data_dir = match &cli.data_dir {
            Some(path) => path.clone(),
            None => default_data_dir()?,
        };
        let conf = wbf_core::conf::load(cli.config.as_deref(), &data_dir)?;
        let mut warnings = conf.warnings().to_vec();
        warnings.extend(conf.warn_about_unknown_keys(KNOWN_CONF_KEYS));
        let server = cli
            .server
            .clone()
            .or_else(|| conf.find("SERVER").map(str::to_string));
        let server_backup = conf.is_on("SERVER_BACKUP", true, &mut warnings);
        let local_room_keys = conf.is_on("LOCAL_ROOM_KEYS", true, &mut warnings);
        let transport_name = cli
            .transport
            .clone()
            .or_else(|| conf.find("TRANSPORT").map(str::to_string))
            .unwrap_or_else(|| "ws".to_string());
        let transport = Transport::from_name(&transport_name).ok_or_else(|| {
            CoreError::new(
                CoreErrorKind::Usage,
                format!("TRANSPORT={transport_name:?} is not `ws` or `http`"),
            )
        })?;
        if !cli.quiet {
            for warning in &warnings {
                eprintln!("{warning}");
            }
        }
        // 這次實際生效的值。🚫 不放 token、password、passphrase 的檔案路徑（§10.5）。
        let effective = vec![
            Entry {
                section: "general",
                key: "SERVER",
                value: server.clone().unwrap_or_default(),
                origin: wbf_core::conf::origin_of(
                    cli.server.is_some(),
                    conf.find("SERVER").is_some(),
                ),
            },
            Entry {
                section: "general",
                key: "TRANSPORT",
                value: transport_name.clone(),
                origin: wbf_core::conf::origin_of(
                    cli.transport.is_some(),
                    conf.find("TRANSPORT").is_some(),
                ),
            },
            Entry {
                section: "backup",
                key: "SERVER_BACKUP",
                value: on_off(server_backup),
                origin: wbf_core::conf::origin_of(false, conf.find("SERVER_BACKUP").is_some()),
            },
            Entry {
                section: "backup",
                key: "LOCAL_ROOM_KEYS",
                value: on_off(local_room_keys),
                origin: wbf_core::conf::origin_of(false, conf.find("LOCAL_ROOM_KEYS").is_some()),
            },
        ]
        .into_iter()
        // 沒有值的鍵不寫進去：`SERVER=` 讀回來是「沒寫」，寫它只是噪音。
        .filter(|entry: &Entry| !entry.value.is_empty())
        .collect();
        Ok(Context {
            warned_about_backups: std::sync::OnceLock::new(),
            core: Core::open(&data_dir),
            account_override: cli
                .account
                .clone()
                .or_else(|| conf.find("ACCOUNT").map(str::to_string)),
            unlock: UnlockOptions {
                data_dir,
                passphrase_file: cli
                    .passphrase_file
                    .clone()
                    .or_else(|| conf.find("PASSPHRASE_FILE").map(PathBuf::from)),
            },
            server_override: server.clone(),
            // 🚫 token 不從 conf 來（§10.5）：秘密不落地在明文檔裡。
            token_override: cli.token.clone(),
            quiet: cli.quiet,
            transport,
            conf,
            server_backup,
            local_room_keys,
            effective,
            data_dir_was_given: cli.data_dir.is_some(),
        })
    }

    /// `--token` 模式：**不碰 vault、不碰帳號目錄**，直接拿那個 token 講話。
    ///
    /// ⚠️ 這條路徑刻意**不經過 `Core`**：core 的世界是「一個有 vault 的資料目錄」，
    /// 而 `--token` 正是要繞過那整件事（除錯與腳本用，CLI 規格 §2）。
    /// 📎 它在 daemon 模型下是什麼意思還沒定（architecture-v2 §7.1 的開放項）。
    ///
    /// ⚠️ 打一次 `whoami` 填真的 `user_id`：續傳狀態檔的 `is_for` 要靠它分辨「不是你的
    /// 上傳」，填佔位值會讓兩把不同的 token 比成相等（PR #6 審查 rumia🟡2）。
    ///
    /// Return:
    ///     Some(Ok(Session))   有 `--token`，而且問到了它是誰
    ///     None                沒有 `--token`——走 core 那條路
    pub async fn token_session(&self) -> Option<Result<Session, CoreError>> {
        let token = self.token_override.as_ref()?;
        Some(self.token_session_inner(token).await)
    }

    async fn token_session_inner(&self, token: &str) -> Result<Session, CoreError> {
        let server = self.server_override.clone().ok_or_else(|| {
            CoreError::new(
                CoreErrorKind::Usage,
                "--token needs --server (or WBF_SERVER) too",
            )
        })?;
        let probe = Session {
            server,
            user_id: String::new(),
            device_id: String::new(),
            access_token: token.to_string(),
            store_dir: None,
            backend: None,
        };
        let who = whoami(&probe).await?;
        Ok(Session {
            user_id: who.user_id,
            device_id: who.device_id,
            ..probe
        })
    }

    /// 這個命令要對哪個帳號動作：`--account`（配 `--server` 消歧），沒給就是 `current`。
    ///
    /// 這次命令要對誰、哪台 server、備份開著嗎——core 幾乎每個方法都要這三件事。
    ///
    /// 📎 `server_backup` 是 conf 的值：**前端的決定**，core 不讀 conf（§3）。
    pub fn target(&self) -> wbf_core::Target {
        wbf_core::Target {
            user: self.account_override.clone(),
            server: self.server_override.clone(),
            server_backup: self.server_backup,
        }
    }

    /// 這個命令的常駐狀態，**保證已經解鎖**。
    ///
    /// ⚠️ 「怎麼拿到 passphrase」是 rpc-cli 這一側的責任（旗標的檔、問終端），
    /// 所以每次交出 `Core` 之前先在這裡把它解開——這樣底下的程式碼不必各自記得。
    /// daemon 那邊沒有這一步：解鎖是一次性的 RPC（`vault.unlock`），不是每個命令做一次。
    pub fn core(&self) -> Result<&Core, CoreError> {
        self.unlock.unlock(&self.core)?;
        Ok(&self.core)
    }

    /// 確保這個資料目錄有一把可用的 vault：沒有 `local.key` 就**建**一把。
    ///
    /// ⚠️ 「要不要設 passphrase」是這一側的決定（給了 `--passphrase-file` 就是
    /// passphrase 模式），core 只收 bytes。`login` 之前叫它。
    pub fn ensure_vault(&self) -> Result<(), CoreError> {
        if self.core.key_mode()?.is_some() {
            self.core()?;
            return Ok(());
        }
        let passphrase = match &self.unlock.passphrase_file {
            Some(path) => Some(crate::unlock::read_passphrase_file(path)?),
            None => None,
        };
        let mode = self
            .core
            .create_vault(passphrase.as_deref().map(|bytes| &bytes[..]))?;
        self.progress(format!(
            "created {} ({} mode)",
            self.unlock
                .data_dir
                .join(wbf_sdk::vault::KEY_FILE_NAME)
                .display(),
            match mode {
                wbf_sdk::vault::KeyMode::Plain => "plain",
                wbf_sdk::vault::KeyMode::Passphrase => "passphrase",
            }
        ));
        Ok(())
    }

    pub fn progress(&self, line: String) {
        if !self.quiet {
            eprintln!("{line}");
        }
    }

    /// 自動生成 `wbf.conf`（CLI 規格 §10.3）。三個條件都要成立，這裡管前兩個，
    /// 第三個（命令成功）由呼叫點決定——它在 `dispatch` 的結果是 `Ok` 之後才叫。
    ///
    /// 🚫 已經存在的永遠不改寫，連補鍵都不做：那是使用者的檔，不是我們的狀態檔。
    fn write_conf_if_asked_for(&self) -> Result<(), CoreError> {
        if !self.data_dir_was_given {
            return Ok(());
        }
        if wbf_core::conf::write_if_absent(&self.unlock.data_dir, &self.effective)? {
            self.progress(format!(
                "wrote {} with the values this run used; edit it or delete it, it will not be rewritten",
                self.unlock.data_dir.join(wbf_core::conf::CONF_FILE_NAME).display()
            ));
        }
        Ok(())
    }

    /// 備份被關掉時，任何會拿到房間金鑰的命令印一次（CLI 規格 §3.6）。
    ///
    /// ⚠️ 一個命令只印一次：`rooms::backend` 在同一個命令裡可能被叫不只一次，
    /// 而重複三次的警告等於沒有警告。
    ///
    /// 🚫 這不是「順便提醒」：關掉備份的後果是**歷史會消失**，而它是設定檔裡一行字造成的
    /// ——那行字可能是幾個月前寫的，也可能是別人寫的。
    pub fn warn_if_backups_are_off(&self) {
        if self.server_backup && self.local_room_keys {
            return;
        }
        if self.warned_about_backups.set(()).is_err() {
            return;
        }
        // 🚫 不去問 core 那個目錄在哪：這是一句警告，而為了印它去解鎖 vault
        // 是本末倒置（使用者可能只是打了 `--help`）。
        let keys_live_here = || "<account dir>/k/".to_string();
        match (self.server_backup, self.local_room_keys) {
            (false, false) => self.progress(
                "warning: both room key backups are disabled ([backup] in wbf.conf). If the crypto store is\n         \
                 deleted or breaks, your history becomes unreadable - there is no copy anywhere."
                    .into(),
            ),
            (false, true) => self.progress(format!(
                "warning: server-side room key backup is off ([backup] SERVER_BACKUP=off in wbf.conf).\n         \
                 Your room keys stay on this machine only:\n         {}",
                keys_live_here()
            )),
            (true, false) => self.progress(
                "warning: the local room key snapshot is off ([backup] LOCAL_ROOM_KEYS=off in wbf.conf);\n         \
                 only the server-side backup is keeping your room keys."
                    .into(),
            ),
            (true, true) => {}
        }
    }

    /// conf 解析出來的警告（認不得的值、parse 不出來的數字）。`--quiet` 就不印。
    pub fn warn(&self, warnings: &[String]) {
        for warning in warnings {
            self.progress(warning.clone());
        }
    }
}

/// `login`／`account add`（CLI 規格 §3.1）。
///
/// rpc-cli 在這裡只做三件前端的事：**把密碼生出來**、**決定要不要設 passphrase**、
/// 印出來。登入本身在 `Core`。
async fn login_command(context: &Context, args: &LoginArgs) -> Result<(), CoreError> {
    let (user, device_name) = (args.user.as_str(), args.device_name.as_str());
    let server = context.server_override.clone().ok_or_else(|| {
        CoreError::new(CoreErrorKind::Usage, "login needs --server (or WBF_SERVER)")
    })?;
    // conf 的 `PASSWORD_FILE` 補上旗標沒給的那格（CLI 規格 §10.5：它是**路徑**不是秘密）。
    let password_file = find_password_file(args, &context.conf);
    let password = match password_file.as_deref() {
        Some(path) => read_password_file(path)?,
        None => prompt_password_on_terminal("password: ")?,
    };
    // vault 先備好（沒有就建）：store 的金鑰與 `session.sealed` 都從它來。
    context.ensure_vault()?;
    context.warn_if_backups_are_off();
    let result = context
        .core()?
        .log_in(&server, user, &password, device_name, context.server_backup)
        .await?;
    print_value(&result)
}

/// `account <action>`（CLI 規格 §3.1）。多帳號是前提：一台機器上可以同時登入好幾個，
/// `current` 只回答「沒帶 `--account` 時用誰」。
async fn account_command(context: &Context, action: AccountAction) -> Result<(), CoreError> {
    match action {
        AccountAction::Add(args) => login_command(context, &args).await,
        AccountAction::Status => {
            // 目錄名是加密的（local-cache-db.md §11），所以列帳號要先解鎖。
            let status = context.core()?.account_status()?;
            // ⚠️ 「一個都解不開」的提示是 core 給的**資料**；要不要印是前端的決定。
            if let Some(hint) = &status.undecryptable_hint {
                context.progress(hint.clone());
            }
            print_value(&status.accounts)
        }
        AccountAction::Switch { user } => {
            let result = context
                .core()?
                .switch_current(&user, context.server_override.as_deref())?;
            if !result.logged_in {
                context.progress(format!(
                    "warning: {user} is not logged in; commands that need the server will fail until you run `login --user {user}`"
                ));
            }
            print_json(&json!({
                "ok": true,
                "current": result.current,
                "switched_from": result.switched_from,
            }))
        }
        AccountAction::Del {
            user,
            accept_history_loss,
        } => {
            let result = log_out(context, &user, accept_history_loss).await?;
            print_json(&json!({ "ok": true, "user": result.user }))
        }
        AccountAction::Destroy {
            user,
            yes,
            accept_history_loss,
        } => destroy_account_command(context, &user, yes, accept_history_loss).await,
    }
}

/// `logout`／`account del` 共用。
async fn log_out(
    context: &Context,
    user: &str,
    accept_history_loss: bool,
) -> Result<wbf_core::LogoutResult, CoreError> {
    let result = context
        .core()?
        .log_out(
            user,
            context.server_override.as_deref(),
            accept_history_loss,
            context.server_backup,
        )
        .await
        .map_err(with_recovery_hint)?;
    Ok(result)
}

/// core 的閘門訊息刻意**不提命令名字**（它不知道呼叫它的是誰）。rpc-cli 在這裡補上
/// 自己那幾句——這正是 `CoreErrorKind` 存在的理由。
fn with_recovery_hint(error: CoreError) -> CoreError {
    if error.kind != CoreErrorKind::HistoryWouldBeLost {
        return error;
    }
    CoreError::new(CoreErrorKind::HistoryWouldBeLost, format!(
        "{error}\n       \
         Run `wbf-cli key-backup recovery` to create one, or `wbf-cli recovery list` to see\n       \
         whose keys are kept here.\n       \
         If you only need the history on this machine, `wbf-cli key-backup save` writes a local\n       \
         snapshot - but note that logging out deletes that too.\n       \
         If you do not want that history at all, pass --accept-history-loss."
    ))
}

/// `recovery <action>`：這台機器保管著誰的 recovery key（local-cache-db.md §10.8）。
///
/// 🚫 不連 server：這些檔案是本機的東西，`list` 連內容都不解（只解檔名）。
fn recovery_command(context: &Context, action: RecoveryAction) -> Result<(), CoreError> {
    match action {
        RecoveryAction::List => {
            let users = context.core()?.list_recovery_key_users()?;
            print_json(&json!({ "users": users }))
        }
        RecoveryAction::Show { user } => {
            let key = context.core()?.find_recovery_key(&user)?.ok_or_else(|| {
                CoreError::new(CoreErrorKind::Usage, format!(
                    "no recovery key is kept here for {user}; run `key-backup recovery` while logged in as them"
                ))
            })?;
            // 會印秘密的第二個命令（另一個是 `key-backup recovery`）。CLI 規格 §3.6。
            print_json(&json!({ "user": user, "recovery_key": key.as_str() }))
        }
    }
}

/// `key-backup <action>`（CLI 規格 §3.6；local-cache-db.md §10）。
///
/// ⚠️ conf 的兩個開關在**這一層**判斷：core 被叫到就做，「要不要叫它」是前端的決定（§3）。
async fn key_backup_command(context: &Context, action: KeyBackupAction) -> Result<(), CoreError> {
    let core = context.core()?;
    let target = context.target();
    context.warn_if_backups_are_off();
    match action {
        KeyBackupAction::Status => {
            let status = core.backup_status(&target).await?;
            let mut output = json_value_of(&status)?;
            // conf 的兩個開關是**前端的值**，core 不知道有 conf 這種東西——所以在這裡加。
            set_field(
                &mut output,
                "server_backup_setting",
                json!(on_off(context.server_backup)),
            );
            set_field(
                &mut output,
                "local_room_keys_setting",
                json!(on_off(context.local_room_keys)),
            );
            print_json(&output)
        }
        KeyBackupAction::Upload => {
            if !context.server_backup {
                return Err(refuse_switched_off("SERVER_BACKUP", "upload to the server"));
            }
            if !context.local_room_keys {
                context
                    .progress("LOCAL_ROOM_KEYS=off, so the local snapshot was not updated".into());
            }
            let result = core
                .upload_room_keys(&target, context.local_room_keys)
                .await?;
            print_value(&result)
        }
        KeyBackupAction::Save => {
            // 🚫 明說要存卻被設定關掉：拒絕並說是誰關的，不要假裝存了。
            if !context.local_room_keys {
                return Err(refuse_switched_off(
                    "LOCAL_ROOM_KEYS",
                    "write a local snapshot",
                ));
            }
            let bytes = core.save_room_key_snapshot(&target).await?;
            print_json(&json!({ "ok": true, "bytes": bytes }))
        }
        KeyBackupAction::Import => {
            let result = core.import_room_key_snapshot(&target).await?;
            print_value(&result)
        }
        KeyBackupAction::Restore => {
            let result =
                core.restore_from_recovery_key(&target)
                    .await
                    .map_err(|error| match error.kind {
                        wbf_core::CoreErrorKind::NoRecoveryKeyHere => CoreError::new(
                            CoreErrorKind::Usage,
                            format!("{error}; run `wbf-cli key-backup recovery` first"),
                        ),
                        _ => error,
                    })?;
            let mut output = json_value_of(&result)?;
            set_field(&mut output, "ok", json!(true));
            print_json(&output)
        }
        KeyBackupAction::Recovery => {
            let recovery_key = core.create_recovery_key(&target).await?;
            context.progress(
                "this recovery key is now sealed under <data dir>/r/, which survives logout,\n       \
                 so you will not be asked to type it on this machine.\n       \
                 Still write it down: if this machine is lost, it is the only way back into the\n       \
                 server-side backup."
                    .into(),
            );
            print_json(&json!({ "recovery_key": recovery_key.as_str() }))
        }
    }
}

/// 使用者明說要做的事，被 conf 的開關關掉了（§10.4）。
///
/// ⚠️ 🚫 不靜默跳過：命令是他打的，回一句「好了」卻什麼都沒做，比拒絕更糟。
/// 訊息要說出**是哪個鍵**關的，不然他得自己翻檔案找。
fn refuse_switched_off(key: &str, what: &str) -> CoreError {
    CoreError::new(
        CoreErrorKind::Usage,
        format!(
            "{key}=off in the config file, so this command will not {what}; \
         set {key}=on (or remove the line) to allow it"
        ),
    )
}

/// `account destroy <mxid>`：裝置層加資料層。
///
/// rpc-cli 在這裡只做一件事：**問「你確定嗎」**。🚫 core 不做確認（§3）。
async fn destroy_account_command(
    context: &Context,
    user: &str,
    yes: bool,
    accept_history_loss: bool,
) -> Result<(), CoreError> {
    let server = context.server_override.as_deref();
    if !yes
        && !crate::rooms::confirm(&format!(
            "destroy {user}? this logs the device out and deletes its session, crypto store, recovery key, and the cached events only this account has"
        ))?
    {
        return Err(CoreError::new(CoreErrorKind::Usage, "cancelled"));
    }
    let result = context
        .core()?
        .destroy_account(user, server, accept_history_loss, context.server_backup)
        .await
        .map_err(with_recovery_hint)?;
    let mut output = json_value_of(&result)?;
    set_field(&mut output, "ok", json!(true));
    print_json(&output)
}

// ---- 上傳與媒體 ----
//
// ⚠️ 這一區有**兩條路**：
//
// | 模式 | 走哪 | 為什麼 |
// |---|---|---|
// | 登入中 | `Core` | 有 vault、有帳號目錄、有快取與媒體池 |
// | `--token` | 直接開通道 | 那個模式的定義就是「不碰 vault、不碰帳號目錄」（CLI 規格 §2） |
//
// 📎 `--token` 在 daemon 模型下是什麼意思還沒定（architecture-v2 §7.1 的開放項）。

/// `--token` 模式的通道：不碰 vault，直接拿那串 token 講話。
///
/// Return:
///     Some(Ok(client))   有 `--token`
///     None               沒有——呼叫端要走 core
async fn token_client(context: &Context) -> Option<Result<WbfClient<Channel>, CoreError>> {
    let session = match context.token_session().await? {
        Ok(session) => session,
        Err(error) => return Some(Err(error)),
    };
    let channel = Channel::connect(&session.server, &session.access_token, context.transport).await;
    Some(match channel {
        Ok(channel) => Ok(WbfClient::new(channel)),
        Err(error) => Err(error.into()),
    })
}

async fn upload_command(context: &Context, args: &UploadArgs) -> Result<(), CoreError> {
    let request = UploadRequest {
        file: args.file.clone().unwrap_or_default(),
        cipher: args.cipher.clone(),
        chunk_size: args.chunk_size,
        name: args.name.clone(),
        mimetype: args.mimetype.clone(),
        sha256: args.sha256,
    };
    if args.stream {
        if args.file.is_some() {
            return Err(CoreError::new(
                CoreErrorKind::Usage,
                "--stream reads stdin; do not pass a file",
            ));
        }
        let mut stdin = std::io::stdin().lock();
        let manifest = context
            .core()?
            .upload_stream(
                &mut stdin,
                &request,
                args.link == "wifi",
                context.transport,
                &context.target(),
            )
            .await?;
        return crate::rooms::emit_manifest(&manifest, args.manifest.as_deref());
    }
    if args.file.is_none() {
        return Err(CoreError::new(
            CoreErrorKind::Usage,
            "upload needs a file, or --stream",
        ));
    }
    let manifest = context
        .core()?
        .upload_file(&request, context.transport, &context.target())
        .await?;
    crate::rooms::emit_manifest(&manifest, args.manifest.as_deref())
}

/// manifest 是對方給的：它指的 server 要跟這次的 session 對得上，🚫 不然就是在對錯的
/// 地方要東西。
async fn read_manifest(context: &Context, path: &Path) -> Result<Manifest, CoreError> {
    let manifest = Manifest::from_json(&std::fs::read(path)?)?;
    let session_server = match context.token_session().await {
        Some(session) => session?.server,
        None => context.core()?.current_server(&context.target())?,
    };
    if manifest.server.trim_end_matches('/') != session_server.trim_end_matches('/') {
        return Err(CoreError::new(
            CoreErrorKind::Usage,
            format!(
                "manifest is for {}, but the session is on {session_server}",
                manifest.server
            ),
        ));
    }
    Ok(manifest)
}

/// 沒給 `-o` 時用描述的 `name`：⚠️ 它是**對方寫的**，帶路徑分隔符或是 `..` 就不能當檔名。
/// 🚫 不要自己「清理」那個名字——清出來的東西仍然是對方決定的，要求明給 `-o` 才安全。
fn output_path_from_name(name: Option<&str>) -> Result<PathBuf, CoreError> {
    let name = name
        .filter(|name| !name.is_empty())
        .unwrap_or("download.bin");
    let is_plain_file_name =
        !name.contains(['/', '\\']) && name != "." && name != ".." && !name.contains('\0');
    if !is_plain_file_name {
        return Err(CoreError::new(
            CoreErrorKind::Usage,
            format!("the file's name {name:?} is not a plain file name; pass -o"),
        ));
    }
    Ok(PathBuf::from(name))
}

async fn info_command(
    context: &Context,
    mxc: &str,
    manifest_path: Option<&Path>,
) -> Result<(), CoreError> {
    let manifest = match manifest_path {
        Some(path) => Some(read_manifest(context, path).await?),
        None => None,
    };
    // CLI 的 `info` 問的一直是「server 上那份長什麼樣」，所以它一律 `Server`
    // ——⚠️ 🚫 不要偷偷改成 `Local`：那會變成另一個問題的答案。
    let info = context
        .core()?
        .media_info(
            mxc,
            manifest.as_ref(),
            wbf_core::SyncMode::Server,
            context.transport,
            &context.target(),
        )
        .await?;
    print_value(&info)
}

async fn download_command(
    context: &Context,
    manifest_path: &Path,
    out: Option<PathBuf>,
    no_cache: bool,
) -> Result<(), CoreError> {
    let manifest = read_manifest(context, manifest_path).await?;
    let out = match out {
        Some(out) => out,
        None => output_path_from_name(manifest.block.name.as_deref())?,
    };
    let target = context.target();
    // 登入中且沒說 `--no-cache`：走媒體快取（池裡有就不連 server；沒有就邊下邊進池、
    // 可續傳），再從池複製到 `--out`（local-cache-db.md §8.7）。
    if !no_cache && context.token_override.is_none() {
        let result = context
            .core()?
            .download_to(&manifest, &out, context.transport, &target)
            .await?;
        return print_value(&result);
    }
    if let Some(client) = token_client(context).await {
        return download_with_raw_client(context, client?, &manifest, &out).await;
    }
    let result = context
        .core()?
        .download_direct(&manifest, &out, context.transport, &target)
        .await?;
    print_value(&result)
}

/// `--token` 模式的下載：沒有 vault、沒有池，逐塊寫進 `-o`。
async fn download_with_raw_client(
    context: &Context,
    mut client: WbfClient<Channel>,
    manifest: &Manifest,
    out: &Path,
) -> Result<(), CoreError> {
    let mut file = std::fs::File::create(out)?;
    let result = client
        .download(manifest, &mut file, &mut |done, total| {
            context.progress(format!("chunk {done}/{total}"))
        })
        .await;
    let report = match result {
        Ok(report) => report,
        Err(error) => {
            // 🚫 半成品不留：下次會被當成完整的用（CLI 規格 §4 exit 3 的語意）。
            drop(file);
            let _ = std::fs::remove_file(out);
            return Err(error.into());
        }
    };
    file.flush()?;
    print_json(&json!({
        "out": out.display().to_string(), "bytes": report.bytes, "chunks": report.chunks,
        "sha256_verified": report.sha256_verified,
    }))
}

async fn media_stats_command(context: &Context) -> Result<(), CoreError> {
    let stats = context.core()?.media_stats(&context.target())?;
    print_value(&stats)
}

async fn media_gc_command(
    context: &Context,
    quota_mib: u64,
    protect_days: u64,
) -> Result<(), CoreError> {
    let report =
        context
            .core()?
            .collect_media_garbage(quota_mib, protect_days, &context.target())?;
    print_value(&report)
}

async fn seek_command(
    context: &Context,
    manifest_path: &Path,
    at: u64,
    len: Option<u64>,
) -> Result<(), CoreError> {
    let manifest = read_manifest(context, manifest_path).await?;
    let result = context
        .core()?
        .seek_read(&manifest, at, len, context.transport, &context.target())
        .await?;
    let mut stdout = std::io::stdout().lock();
    stdout.write_all(&result.bytes)?;
    stdout.flush()?;
    // ⚠️ 摘要是**結果**不是進度，所以 `--quiet` 也印（CLI 規格 §3.3.1）。
    // bytes 已經寫出去了；摘要印不出來就用 exit code 講，🚫 不假裝成功。
    let summary = json_value_of(&result.summary(at, len))?;
    eprintln!("{summary}");
    Ok(())
}

// ---- 小工具 ----

fn print_json(value: &serde_json::Value) -> Result<(), CoreError> {
    crate::rooms::print_json(value)
}

#[cfg(test)]
mod conf_precedence_tests {
    //! 優先序（CLI 規格 §10.2）：**旗標 > 環境變數 > conf > 內建預設**，每個值各自比一次。
    //!
    //! 🚫 這裡不設環境變數：`std::env::set_var` 是行程全域的，跟同時跑的測試會互相汙染。
    //! 環境變數那一格由 clap 負責（`env = "WBF_…"`），它把旗標與環境合成同一個 `Option`——
    //! 所以這裡測得到的是「**沒給**就落到 conf、conf 沒有就落到內建預設」那兩格。

    use super::*;

    fn cli_with(data_dir: &std::path::Path) -> Cli {
        Cli {
            server: None,
            token: None,
            data_dir: Some(data_dir.to_path_buf()),
            account: None,
            config: None,
            passphrase_file: None,
            json: false,
            quiet: true,
            transport: None,
            command: Command::Whoami,
        }
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("wbf-prec-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn conf_fills_in_what_the_flags_did_not_give() {
        let dir = scratch("conf");
        std::fs::write(
            dir.join(wbf_core::conf::CONF_FILE_NAME),
            "[general]\nSERVER=http://from-conf:6167\nACCOUNT=@alice:localhost\nTRANSPORT=http\n[backup]\nSERVER_BACKUP=off\n",
        )
        .unwrap();
        let context = Context::from(&cli_with(&dir)).unwrap();
        assert_eq!(
            context.server_override.as_deref(),
            Some("http://from-conf:6167")
        );
        assert_eq!(
            context.account_override.as_deref(),
            Some("@alice:localhost")
        );
        assert_eq!(context.transport, Transport::Http);
        assert!(!context.server_backup);
        // 🚫 沒寫的鍵落到安全值，不是落到 false。
        assert!(context.local_room_keys);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_flag_beats_the_conf_file() {
        let dir = scratch("flag");
        std::fs::write(
            dir.join(wbf_core::conf::CONF_FILE_NAME),
            "[general]\nSERVER=http://from-conf:6167\nTRANSPORT=http\n",
        )
        .unwrap();
        let mut cli = cli_with(&dir);
        cli.server = Some("http://from-flag:6167".into());
        cli.transport = Some("ws".to_string());
        let context = Context::from(&cli).unwrap();
        assert_eq!(
            context.server_override.as_deref(),
            Some("http://from-flag:6167")
        );
        // conf 說 http、旗標說 ws → 旗標贏。
        assert_eq!(context.transport, Transport::WebSocket);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn no_conf_file_means_the_built_in_defaults() {
        let dir = scratch("none");
        let context = Context::from(&cli_with(&dir)).unwrap();
        assert_eq!(context.server_override, None);
        assert_eq!(context.transport, Transport::WebSocket);
        // 兩個開關的安全值都是「開著」。
        assert!(context.server_backup && context.local_room_keys);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn login_args(password_file: Option<&str>) -> LoginArgs {
        LoginArgs {
            user: "@alice:localhost".into(),
            password_file: password_file.map(std::path::PathBuf::from),
            device_name: "wbf-cli".into(),
        }
    }

    #[test]
    fn the_conf_file_supplies_the_password_file_path_when_the_flag_did_not() {
        // ⚠️ 進 conf 的是**路徑**不是秘密（§10.5）——秘密是那個檔的內容，它從來不進 conf。
        let dir = scratch("pwfile");
        std::fs::write(
            dir.join(wbf_core::conf::CONF_FILE_NAME),
            "[general]
PASSWORD_FILE=/tmp/from-conf
",
        )
        .unwrap();
        let context = Context::from(&cli_with(&dir)).unwrap();

        // 🚫 認得卻沒人讀 = 使用者寫了一行、login 照樣問密碼、一句話都不說
        //（PR #22 審查 rumia🔴1）。這一條就是在釘「有人讀」。
        assert_eq!(
            find_password_file(&login_args(None), &context.conf),
            Some(std::path::PathBuf::from("/tmp/from-conf"))
        );
        // 旗標照樣蓋過 conf。
        assert_eq!(
            find_password_file(&login_args(Some("/tmp/from-flag")), &context.conf),
            Some(std::path::PathBuf::from("/tmp/from-flag"))
        );
        // 兩邊都沒有就是 None（呼叫端會去問終端）。
        let empty = Context::from(&cli_with(&scratch("pwfile-empty"))).unwrap();
        assert_eq!(find_password_file(&login_args(None), &empty.conf), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_secret_in_the_conf_file_is_not_used() {
        let dir = scratch("secret");
        std::fs::write(
            dir.join(wbf_core::conf::CONF_FILE_NAME),
            "[general]\nACCESS_TOKEN=syt_nope\n",
        )
        .unwrap();
        let context = Context::from(&cli_with(&dir)).unwrap();
        assert_eq!(
            context.token_override, None,
            "🚫 token 不從 conf 來（§10.5）"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn generating_writes_what_this_run_used_and_reads_back_the_same() {
        let dir = scratch("gen");
        let mut cli = cli_with(&dir);
        cli.server = Some("http://from-flag:6167".into());
        let context = Context::from(&cli).unwrap();
        context.write_conf_if_asked_for().unwrap();

        let written = std::fs::read_to_string(dir.join(wbf_core::conf::CONF_FILE_NAME)).unwrap();
        assert!(
            written.contains("SERVER=http://from-flag:6167"),
            "{written}"
        );
        assert!(written.contains("; flag or env"), "{written}");
        assert!(written.contains("SERVER_BACKUP=on"), "{written}");
        // 🚫 秘密與「秘密在哪」的路徑都不寫（§10.5）。
        assert!(!written.contains("ACCESS_TOKEN") && !written.contains("PASSPHRASE_FILE"));

        // 讀回來就是同一組值。
        let again = Context::from(&cli_with(&dir)).unwrap();
        assert_eq!(
            again.server_override.as_deref(),
            Some("http://from-flag:6167")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_existing_conf_is_never_rewritten() {
        let dir = scratch("keep");
        let path = dir.join(wbf_core::conf::CONF_FILE_NAME);
        std::fs::write(&path, "[general]\nSERVER=http://mine:6167\n").unwrap();
        let context = Context::from(&cli_with(&dir)).unwrap();
        context.write_conf_if_asked_for().unwrap();
        // 使用者的檔，一個 byte 都不動——連補鍵都不做。
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "[general]\nSERVER=http://mine:6167\n"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn nothing_is_generated_without_an_explicit_data_dir() {
        let dir = scratch("nodatadir");
        let mut cli = cli_with(&dir);
        // 三個條件之一：`--data-dir`／`WBF_DATA_DIR` 有給。裝成沒給。
        cli.data_dir = None;
        let mut context = Context::from(&cli_with(&dir)).unwrap();
        context.data_dir_was_given = false;
        context.write_conf_if_asked_for().unwrap();
        assert!(!dir.join(wbf_core::conf::CONF_FILE_NAME).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}

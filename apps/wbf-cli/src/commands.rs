//! 每個命令一個函數，照 CLI 規格 §3。stdout 只有結果 JSON（`seek` 例外：明文 bytes），進度與警告在 stderr。

use std::io::Write;
use std::path::{Path, PathBuf};

use serde_json::json;
use wbf_sdk::backend::matrix_sdk::MatrixBackend;
use wbf_sdk::cache::{Cache, CacheIdentity, OpenOutcome};
use wbf_sdk::media::{self, FetchOutcome};
use wbf_sdk::media_pool::MediaPool;
use wbf_sdk::room_keys;

use crate::conf::{Conf, Entry};
use wbf_core::accounts::{self, AccountDir, DataDirMap};
use wbf_core::Core;
use wbf_sdk::chunk_crypto::{choose_chunk_size, choose_stream_chunk_size, DescriptionSlot, Link};
use wbf_sdk::login::{logout, whoami};
use wbf_sdk::{
    Channel, ChunkedBlock, Cipher, FileCipher, Manifest, SdkError, Session, Transport, UploadState,
    Vault, WbfClient,
};

use wbf_sdk::vault::write_private;

use crate::unlock::{
    default_data_dir, prompt_new_passphrase, prompt_password_on_terminal, read_passphrase_file,
    read_password_file, UnlockOptions,
};
use crate::{AccountAction, Cli, Command, KeyBackupAction, LoginArgs, RecoveryAction, UploadArgs};

pub const CLIENT_NAME: &str = concat!("wbf-cli/", env!("CARGO_PKG_VERSION"));

pub async fn run(cli: Cli) -> Result<(), SdkError> {
    let context = Context::from(&cli)?;
    let result = dispatch(&context, cli.command).await;
    if result.is_ok() {
        context.write_conf_if_asked_for()?;
    }
    result
}

/// ⚠️ 自動生成（§10.3）在**這裡之後**：三個條件之一是「這次命令成功結束」。
async fn dispatch(context: &Context, command: Command) -> Result<(), SdkError> {
    match command {
        Command::Login(args) => login_command(context, &args).await,
        Command::Logout {
            accept_history_loss,
        } => {
            // --token 模式沒有帳號目錄，只讓 server 端的 token 失效。
            if context.token_override.is_some() {
                logout(&context.session().await?).await?;
                return print_json(&json!({ "ok": true }));
            }
            let account = context.account()?.clone();
            let user = log_out_account(context, &account, accept_history_loss).await?;
            print_json(&json!({ "ok": true, "user": user }))
        }
        Command::Account { action } => account_command(context, action).await,
        Command::KeyBackup { action } => key_backup_command(context, action).await,
        Command::Recovery { action } => recovery_command(context, action),
        Command::Lock => {
            let removed = context.unlock.delete_ticket()?;
            print_json(&json!({ "ok": true, "had_ticket": removed }))
        }
        Command::SetPassphrase {
            new_passphrase_file,
        } => {
            let mut vault = context.unlock.open_vault()?;
            let passphrase = match new_passphrase_file {
                Some(path) => read_passphrase_file(&path)?,
                None => prompt_new_passphrase()?,
            };
            vault.set_unlock(&wbf_sdk::Unlock::Passphrase(passphrase))?;
            // 舊 ticket 是用舊 passphrase 換來的；換了就作廢，下一個命令要用新的。
            context.unlock.delete_ticket()?;
            print_json(&json!({ "ok": true, "mode": vault.mode() }))
        }
        Command::RemovePassphrase => {
            let mut vault = context.unlock.open_vault()?;
            vault.set_unlock(&wbf_sdk::Unlock::NoPassphrase)?;
            context.unlock.delete_ticket()?;
            print_json(&json!({ "ok": true, "mode": vault.mode() }))
        }
        Command::Whoami => {
            let who = whoami(&context.session().await?).await?;
            print_json(&json!({ "user_id": who.user_id, "device_id": who.device_id }))
        }
        Command::Ping => {
            let mut client = context.client().await?;
            let hello = client.hello(CLIENT_NAME).await?;
            client.ping().await?;
            print_json(&json!({
                "protocol": hello.protocol, "server": hello.server, "features": hello.features,
                "chunk_size_default": hello.chunk_size_default, "chunk_size_large": hello.chunk_size_large,
                "data_max_bytes": hello.data_max_bytes,
            }))
        }
        Command::Upload(args) => {
            if args.stream {
                upload_stream(context, &args).await
            } else {
                upload_file(context, &args).await
            }
        }
        Command::Status { upload_id } => {
            let status = context.client().await?.upload_status(upload_id).await?;
            print_json(&json!({
                "received": status.received, "chunk_count": status.chunk_count, "total_len": status.total_len,
                "finished": status.finished, "truncated": status.truncated, "chunk_size": status.chunk_size,
                "file_size": status.file_size,
            }))
        }
        Command::Abort { upload_id, file } => {
            context.client().await?.abort_upload(upload_id).await?;
            if let Some(file) = file {
                remove_if_exists(&state_path_for(&file))?;
            }
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
    /// 一個命令只解鎖一次：`session()` 與房間命令的 store 都從它拿，不然 Argon2 跑兩次、ticket 寫兩次。
    core: Core,
    /// 這個命令用的帳號目錄，也只解析一次。
    account: std::sync::OnceLock<AccountDir>,
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
    effective: Vec<crate::conf::Entry>,
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
    "UNLOCK_TTL",
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
    fn from(cli: &Cli) -> Result<Context, SdkError> {
        // ⚠️ 資料目錄不能從 conf 來：conf 就在它裡面（§10.1）。
        let data_dir = match &cli.data_dir {
            Some(path) => path.clone(),
            None => default_data_dir()?,
        };
        let conf = crate::conf::load(cli.config.as_deref(), &data_dir)?;
        let mut warnings = conf.warnings().to_vec();
        warnings.extend(conf.warn_about_unknown_keys(KNOWN_CONF_KEYS));
        let server = cli
            .server
            .clone()
            .or_else(|| conf.find("SERVER").map(str::to_string));
        let server_backup = conf.is_on("SERVER_BACKUP", true, &mut warnings);
        let local_room_keys = conf.is_on("LOCAL_ROOM_KEYS", true, &mut warnings);
        let unlock_ttl = match cli.unlock_ttl {
            Some(seconds) => seconds,
            None => conf.get_number("UNLOCK_TTL", 900, &mut warnings),
        };
        let transport_name = cli
            .transport
            .clone()
            .or_else(|| conf.find("TRANSPORT").map(str::to_string))
            .unwrap_or_else(|| "ws".to_string());
        let transport = Transport::from_name(&transport_name).ok_or_else(|| {
            SdkError::Usage(format!(
                "TRANSPORT={transport_name:?} is not `ws` or `http`"
            ))
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
                origin: crate::conf::origin_of(cli.server.is_some(), conf.find("SERVER").is_some()),
            },
            Entry {
                section: "general",
                key: "TRANSPORT",
                value: transport_name.clone(),
                origin: crate::conf::origin_of(
                    cli.transport.is_some(),
                    conf.find("TRANSPORT").is_some(),
                ),
            },
            Entry {
                section: "general",
                key: "UNLOCK_TTL",
                value: unlock_ttl.to_string(),
                origin: crate::conf::origin_of(
                    cli.unlock_ttl.is_some(),
                    conf.find("UNLOCK_TTL").is_some(),
                ),
            },
            Entry {
                section: "backup",
                key: "SERVER_BACKUP",
                value: on_off(server_backup),
                origin: crate::conf::origin_of(false, conf.find("SERVER_BACKUP").is_some()),
            },
            Entry {
                section: "backup",
                key: "LOCAL_ROOM_KEYS",
                value: on_off(local_room_keys),
                origin: crate::conf::origin_of(false, conf.find("LOCAL_ROOM_KEYS").is_some()),
            },
        ]
        .into_iter()
        // 沒有值的鍵不寫進去：`SERVER=` 讀回來是「沒寫」，寫它只是噪音。
        .filter(|entry: &Entry| !entry.value.is_empty())
        .collect();
        Ok(Context {
            warned_about_backups: std::sync::OnceLock::new(),
            core: Core::open(&data_dir),
            account: std::sync::OnceLock::new(),
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
                unlock_ttl: std::time::Duration::from_secs(unlock_ttl),
                quiet: cli.quiet,
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

    /// `--token` 加 `--server` 就不碰 vault；否則開 vault 讀 `session.sealed`，`--server` 可覆蓋。
    /// `--token` 模式打一次 `whoami` 填真的 user_id：續傳狀態檔的 `is_for` 要靠它分辨「不是你的上傳」，
    /// 填佔位值會讓兩把不同的 token 比成相等（PR #6 審查 rumia 🟡2）。
    pub async fn session(&self) -> Result<Session, SdkError> {
        if let Some(token) = &self.token_override {
            let server = self.server_override.clone().ok_or_else(|| {
                SdkError::Usage("--token needs --server (or WBF_SERVER) too".into())
            })?;
            let probe = Session {
                server,
                user_id: String::new(),
                device_id: String::new(),
                access_token: token.clone(),
                store_dir: None,
            };
            let who = whoami(&probe).await?;
            return Ok(Session {
                user_id: who.user_id,
                device_id: who.device_id,
                ..probe
            });
        }
        let mut session = self.stored_session()?;
        if let Some(server) = &self.server_override {
            session.server = server.clone();
        }
        Ok(session)
    }

    /// 帳號目錄裡封著的 session，原樣（沒套 `--server`）。快取的身份用它的 server。
    fn stored_session(&self) -> Result<Session, SdkError> {
        let account = self.account()?;
        self.vault()?
            .unseal_session(&account.session_path())?
            .ok_or_else(|| {
                SdkError::Usage(format!(
                    "{} is not logged in ({} missing); run `login` first",
                    account.label(),
                    account.session_path().display()
                ))
            })
    }

    /// 這個命令用哪個帳號：`--account`（配 `--server` 消歧）→ `current`。只解析一次。
    ///
    /// Return:
    ///     Ok(&AccountDir)
    ///     Err(Usage)   沒登入過、`--account` 找不到或有歧義、資料目錄還是舊的單一目錄佈局
    pub fn account(&self) -> Result<&AccountDir, SdkError> {
        if let Some(account) = self.account.get() {
            return Ok(account);
        }
        // 兩層目錄名都是加密的（local-cache-db.md §11），所以定位帳號一定要先解鎖。
        self.vault()?;
        let account = match &self.account_override {
            Some(user) => self
                .core
                .find_account(user, self.server_override.as_deref())?,
            // 🚫 訊息裡的「run `login`／pass --account」是 rpc-cli 的話，不是 core 的：
            // core 不知道呼叫它的人有沒有命令列。
            None => self.core.current_account().map_err(|error| {
                SdkError::Usage(format!("{error}; run `login` first (or pass --account)"))
            })?,
        };
        Ok(self.account.get_or_init(|| account))
    }

    /// 開一次、之後都拿同一個（唯讀）。要改鎖法的命令自己 `unlock.open_vault()` 拿可變的那份。
    /// ⚠️ rpc-cli 這一側負責**把 passphrase 生出來**（旗標的檔、ticket、問終端），
    /// 解鎖本身在 `Core`。daemon 那邊會換成 `core.unlock(RPC 進來的 bytes)`，
    /// 而底下所有用 `vault()` 的程式碼一行都不必改。
    pub fn vault(&self) -> Result<&Vault, SdkError> {
        if self.core.is_unlocked() {
            return self.core.vault();
        }
        Ok(self.core.adopt_unlocked_vault(self.unlock.open_vault()?))
    }

    /// 這個命令的常駐狀態，**保證已經解鎖**。
    ///
    /// ⚠️ 「怎麼拿到 passphrase」是 rpc-cli 這一側的責任（旗標的檔、ticket、問終端），
    /// 所以每次交出 `Core` 之前先在這裡把它解開——這樣底下的程式碼不必各自記得。
    /// daemon 那邊沒有這一步：解鎖是一次性的 RPC（`vault.unlock`），不是每個命令做一次。
    pub fn core(&self) -> Result<&Core, SdkError> {
        self.vault()?;
        Ok(&self.core)
    }

    /// 這個 server 的 `cache.db`（local-cache-db.md §6，所有帳號共用）與「我是誰」（mxid，讀寫快取都要帶）。
    /// 金鑰是 vault 的第一把子金鑰，身份是封著的 session 的 server。server 不符、解不開就重建（§1），重建時 stderr 說一聲。
    /// `--token` 模式沒有 vault 也沒有帳號目錄，不給快取。
    pub async fn cache(&self) -> Result<(Cache, String), SdkError> {
        if self.token_override.is_some() {
            return Err(SdkError::Usage(
                "the cache needs a logged-in account (local.key + session.sealed); --token mode has none".into(),
            ));
        }
        let stored = self.stored_session()?;
        let cache = self.open_cache(&stored.server)?;
        Ok((cache, stored.user_id))
    }

    /// 這個 server 的媒體儲存池（local-cache-db.md §8），跟 `cache.db` 同層；金鑰是第四把子金鑰。
    pub fn media_pool(&self) -> Result<MediaPool, SdkError> {
        MediaPool::open(
            &self.account()?.server_dir(),
            self.vault()?.media_store_key(),
        )
    }

    fn open_cache(&self, server: &str) -> Result<Cache, SdkError> {
        let identity = CacheIdentity {
            server: server.to_string(),
        };
        let (cache, outcome) = Cache::open(
            &self.account()?.server_dir(),
            &self.vault()?.cache_key(),
            &identity,
        )?;
        match outcome {
            OpenOutcome::Reused => {}
            OpenOutcome::Created => self.progress(format!("created {}", cache.path().display())),
            OpenOutcome::Rebuilt => self.progress(format!(
                "rebuilt {} (it belonged to another server, or could not be opened)",
                cache.path().display()
            )),
        }
        Ok(cache)
    }

    pub async fn client(&self) -> Result<WbfClient<Channel>, SdkError> {
        let session = self.session().await?;
        let channel =
            Channel::connect(&session.server, &session.access_token, self.transport).await?;
        Ok(WbfClient::new(channel))
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
    fn write_conf_if_asked_for(&self) -> Result<(), SdkError> {
        if !self.data_dir_was_given {
            return Ok(());
        }
        if crate::conf::write_if_absent(&self.unlock.data_dir, &self.effective)? {
            self.progress(format!(
                "wrote {} with the values this run used; edit it or delete it, it will not be rewritten",
                self.unlock.data_dir.join(crate::conf::CONF_FILE_NAME).display()
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
        let keys_live_here = || match self.account() {
            Ok(account) => room_keys::snapshot_path(&account.dir)
                .parent()
                .map(|dir| dir.display().to_string())
                .unwrap_or_else(|| "<account dir>/k/".to_string()),
            Err(_) => "<account dir>/k/".to_string(),
        };
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

/// `login`＝`account add`：登入、封 session、**自動切成 current** 並印一行 switch 提示（CLI 規格 §3.1.1）。
async fn login_command(context: &Context, args: &LoginArgs) -> Result<(), SdkError> {
    let (user, device_name) = (args.user.as_str(), args.device_name.as_str());
    let server = context
        .server_override
        .clone()
        .ok_or_else(|| SdkError::Usage("login needs --server (or WBF_SERVER)".into()))?;
    // conf 的 `PASSWORD_FILE` 補上旗標沒給的那格（CLI 規格 §10.5：它是**路徑**不是秘密，
    // 手寫進 conf 正是維護者要的「不用每次指定」）。
    // 🚫 它在 `KNOWN_CONF_KEYS` 裡卻沒人讀 = 使用者寫了一行、login 照樣問密碼、一句話都不說
    //（PR #22 審查 rumia🔴1／salvia🟡1／cirno）——那正是這個檔在 `refuse_switched_off`
    // 底下譴責的形狀。
    let password_file = find_password_file(args, &context.conf);
    let password = match password_file.as_deref() {
        Some(path) => read_password_file(path)?,
        None => prompt_password_on_terminal("password: ")?,
    };
    // vault 先開（沒有就建）：store 的金鑰與 session.sealed 都從它來。
    let vault = context.unlock.open_or_create_vault()?;
    // 帳號目錄由 server host 加 localpart 決定（store 在 login 前就要有路徑）。
    let dir_key = vault.account_dir_key();
    let account = AccountDir::locate(&context.unlock.data_dir, &dir_key, &server, user)?;
    // 沒有 session 卻留著 m/（crypto store）：上次沒走 logout（或舊版的 logout 沒刪），那個 store 綁著已經失效的裝置。消費端自己再清一次。
    if !account.is_logged_in() && account.matrix_store_dir().exists() {
        context.progress("removing a matrix store left over from a previous device".into());
        account.delete_matrix_store()?;
    }
    // 第 3 步起走 matrix-sdk 登入：拿到的是有裝置金鑰的 session，E2EE 房間才解得開。store 放帳號目錄的 m/。
    context.warn_if_backups_are_off();
    let (_backend, session) = MatrixBackend::login(
        &server,
        user,
        &password,
        device_name,
        &account.matrix_store_dir(),
        &vault.matrix_store_key(),
        context.server_backup,
    )
    .await?;
    // server 回的 user_id 才是權威（大小寫、localpart 正規化可能跟 --user 打的不一樣）：目錄名對不上就搬過去。
    let canonical = AccountDir::locate(
        &context.unlock.data_dir,
        &dir_key,
        &server,
        &session.user_id,
    )?;
    let account = if canonical.dir != account.dir {
        if canonical.dir.exists() {
            return Err(SdkError::Usage(format!(
                "server says you are {} but {} already exists; logout that account first",
                session.user_id,
                canonical.dir.display()
            )));
        }
        std::fs::create_dir_all(canonical.dir.parent().expect("account dir has a parent"))?;
        std::fs::rename(&account.dir, &canonical.dir)?;
        canonical
    } else {
        account
    };
    vault.seal_session(&account.session_path(), &session)?;
    let switched_from = switch_current_to(context, &dir_key, &account)?;
    print_json(
        &json!({ "user_id": session.user_id, "device_id": session.device_id, "server": session.server, "switched_from": switched_from }),
    )
}

/// `account <action>`（CLI 規格 §3.1）。多帳號是前提：一台機器上可以同時登入好幾個，
/// `current` 只回答「沒帶 `--account` 時用誰」。
async fn account_command(context: &Context, action: AccountAction) -> Result<(), SdkError> {
    match action {
        AccountAction::Add(args) => login_command(context, &args).await,
        AccountAction::Status => {
            // 目錄名是加密的（local-cache-db.md §11），所以列帳號要先解鎖——這跟 2026-09-09 之前不一樣。
            let vault = context.vault()?;
            let map = context.core()?.refresh_data_dir_map()?;
            if let Some(hint) = map.find_undecryptable_layout_hint() {
                context.progress(hint);
            }
            print_json(&serde_json::to_value(map.list_accounts(vault)?).expect("serializes"))
        }
        AccountAction::Switch { user } => {
            let map = context.core()?.refresh_data_dir_map()?;
            let account = find_account_by_full_mxid(context, &map, &user)?;
            if !account.is_logged_in() {
                context.progress(format!(
                    "warning: {user} is not logged in; commands that need the server will fail until you run `login --user {user}`"
                ));
            }
            let dir_key = context.vault()?.account_dir_key();
            let switched_from = switch_current_to(context, &dir_key, &account)?;
            print_json(&json!({
                "ok": true,
                "current": describe_account(context, &account),
                "switched_from": switched_from,
            }))
        }
        AccountAction::Del {
            user,
            accept_history_loss,
        } => {
            let map = context.core()?.refresh_data_dir_map()?;
            let account = find_account_by_full_mxid(context, &map, &user)?;
            let user = log_out_account(context, &account, accept_history_loss).await?;
            print_json(&json!({ "ok": true, "user": user }))
        }
        AccountAction::Destroy {
            user,
            yes,
            accept_history_loss,
        } => destroy_account_command(context, &user, yes, accept_history_loss).await,
    }
}

/// `account switch|del|destroy` 的 `<user>`：**一律完整 mxid**（維護者 2026-09-09）——
/// 這些命令會登出、會刪檔，變更的對象不該靠猜。只給 localpart 就報錯並列出本機的帳號，
/// 🚫 不推測、🚫 不拿唯一一個頂替。
///
/// Args:
///     user: example: "@bob:matrix.org"
/// Return:
///     Ok(AccountDir)
///     Err(Usage)   不是完整 mxid、找不到、或同名 localpart 有歧義
fn find_account_by_full_mxid(
    context: &Context,
    map: &DataDirMap,
    user: &str,
) -> Result<AccountDir, SdkError> {
    let vault = context.vault()?;
    if !user.starts_with('@') || !user.contains(':') {
        let known = map.list_accounts(vault)?;
        let names: Vec<String> = known
            .iter()
            .map(|summary| match &summary.user_id {
                Some(user_id) => user_id.clone(),
                None => format!("{} on {} (logged out)", summary.localpart, summary.server),
            })
            .collect();
        let names = if names.is_empty() {
            "none".to_string()
        } else {
            names.join(", ")
        };
        return Err(SdkError::Usage(format!(
            "expected a full Matrix ID like @bob:matrix.org, got \"{user}\"\n       accounts on this machine: {names}"
        )));
    }
    map.find_account_dir(user, context.server_override.as_deref())
}

/// 給人看的一句話。登入中的用 `session.sealed` 裡的權威 mxid 與 server URL；
/// 登出的只剩目錄名解出來的明文（localpart 與 host），組不出可靠的 mxid，所以老實說它登出了。
fn describe_account(context: &Context, account: &AccountDir) -> String {
    match context
        .vault()
        .and_then(|vault| vault.unseal_session(&account.session_path()))
    {
        Ok(Some(session)) => format!("{} on {}", session.user_id, session.server),
        _ => format!("{} (not logged in)", account.label()),
    }
}

/// 改 `current` 並印一行 switch 提示（CLI 規格 §3.1.1）。
///
/// Args:
///     dir_key: example: vault.account_dir_key()
/// Return:
///     Ok(Some(String))   換掉的是誰
///     Ok(None)           本來就沒有 current（第一次登入），或本來就是它
fn switch_current_to(
    context: &Context,
    dir_key: &wbf_sdk::Key32,
    account: &AccountDir,
) -> Result<Option<String>, SdkError> {
    let previous = accounts::read_current(&context.unlock.data_dir)?
        .and_then(|current| {
            accounts::find_account_of_current(&context.unlock.data_dir, dir_key, &current)
        })
        .filter(|previous| previous.dir != account.dir)
        .map(|previous| describe_account(context, &previous));
    accounts::write_current(&context.unlock.data_dir, account)?;
    let now = describe_account(context, account);
    context.progress(match &previous {
        Some(previous) => format!("switched to {now} (was {previous})"),
        None => format!("switched to {now} (no previous account)"),
    });
    Ok(previous)
}

/// `recovery <action>`：這台機器保管著誰的 recovery key（local-cache-db.md §10.8）。
///
/// 🚫 不連 server：這些檔案是本機的東西，`list` 連內容都不解（只解檔名）。
fn recovery_command(context: &Context, action: RecoveryAction) -> Result<(), SdkError> {
    let vault = context.vault()?;
    match action {
        RecoveryAction::List => {
            let users = context.core()?.list_recovery_key_users()?;
            print_json(&json!({ "users": users }))
        }
        RecoveryAction::Show { user } => {
            // 跟 `destroy` 走同一條：`list` 印得出 `@alice:localhost`，打 `@ALICE:LocalHost`
            // 卻說沒有，是同一個命令家族內的兩套規則（PR #21 審查 salvia🟢）。
            let map = context.core()?.refresh_data_dir_map()?;
            let missing = || {
                SdkError::Usage(format!(
                    "no recovery key is kept here for {user}; run `key-backup recovery` while logged in as them"
                ))
            };
            let user_id = map.find_recovery_key_user_id(&user)?.ok_or_else(missing)?;
            let key = wbf_core::recovery::find(&context.unlock.data_dir, vault, user_id)?
                .ok_or_else(missing)?;
            // 會印秘密的第二個命令（另一個是 `key-backup recovery`）。
            print_json(&json!({ "user": user_id, "recovery_key": key.as_str() }))
        }
    }
}

/// `key-backup <action>`（CLI 規格 §3.6；local-cache-db.md §10）。
async fn key_backup_command(context: &Context, action: KeyBackupAction) -> Result<(), SdkError> {
    let backend = crate::rooms::backend(context).await?;
    match action {
        KeyBackupAction::Status => {
            let status = backend.backup_status().await?;
            let snapshot = room_keys::get_snapshot_status(&context.account()?.dir);
            print_json(&json!({
                // conf 的兩個開關也印出來：`uploading_locally` 說的是上游現在的狀態，
                // 這兩個說的是「這台機器的設定叫它做什麼」，對不上時要看得出來（§10.4）。
                "server_backup_setting": on_off(context.server_backup),
                "local_room_keys_setting": on_off(context.local_room_keys),
                "server_backup_exists": status.exists_on_server,
                "uploading_locally": status.enabled_locally,
                "recovery_enabled": status.recovery_enabled,
                "recovery_state": status.recovery_state,
                "local_snapshot": snapshot.exists,
                "local_snapshot_bytes": snapshot.bytes,
                "local_snapshot_saved_at": snapshot.saved_at,
            }))
        }
        KeyBackupAction::Upload => {
            if !context.server_backup {
                return Err(refuse_switched_off("SERVER_BACKUP", "upload to the server"));
            }
            context.progress("uploading room keys to the server backup...".into());
            backend.upload_room_keys().await?;
            // 順手把本地那份也更新：兩份備份的用途不同（§10.2），但沒有理由讓使用者記得跑兩個命令。
            // ⚠️ 本地那份關掉時只跳過它，🚫 不讓整個 upload 失敗——使用者要的是 server 那份。
            let bytes = match context.local_room_keys {
                true => Some(save_room_key_snapshot(context, &backend).await?),
                false => {
                    context.progress(
                        "LOCAL_ROOM_KEYS=off, so the local snapshot was not updated".into(),
                    );
                    None
                }
            };
            let status = backend.backup_status().await?;
            print_json(&json!({
                "ok": true,
                "server_backup_exists": status.exists_on_server,
                "recovery_enabled": status.recovery_enabled,
                "local_snapshot_bytes": bytes,
            }))
        }
        KeyBackupAction::Save => {
            // 🚫 明說要存卻被設定關掉：拒絕並說是誰關的，不要假裝存了。
            if !context.local_room_keys {
                return Err(refuse_switched_off(
                    "LOCAL_ROOM_KEYS",
                    "write a local snapshot",
                ));
            }
            let bytes = save_room_key_snapshot(context, &backend).await?;
            print_json(&json!({ "ok": true, "bytes": bytes }))
        }
        KeyBackupAction::Import => {
            let account = context.account()?;
            let key = context.vault()?.room_key_backup_key();
            let (imported, total) = backend
                .import_room_key_snapshot(
                    &room_keys::snapshot_path(&account.dir),
                    &room_keys::snapshot_passphrase(&key),
                )
                .await?;
            print_json(&json!({ "ok": true, "imported": imported, "total": total }))
        }
        KeyBackupAction::Restore => {
            // logout 之後重新 login 是新裝置：crypto store 沒有 SSSS 的 secrets，
            // RecoveryState 是 Incomplete，server 上那份備份解不開。這條命令補上那一步
            // （2026-09-09 對真 server 驗證時發現的缺口）。
            let session = context.session().await?;
            let key = wbf_core::recovery::find(
                &context.unlock.data_dir,
                context.vault()?,
                &session.user_id,
            )?
            .ok_or_else(|| {
                SdkError::Usage(format!(
                    "no recovery key is kept here for {}; run `key-backup recovery` first, or restore it from wherever you wrote it down",
                    session.user_id
                ))
            })?;
            backend.recover_with(&key).await?;
            let status = backend.backup_status().await?;
            print_json(&json!({
                "ok": true,
                "recovery_enabled": status.recovery_enabled,
                "recovery_state": status.recovery_state,
            }))
        }
        KeyBackupAction::Recovery => {
            let recovery_key = backend.enable_recovery().await?;
            // 封進 <data dir>/r/（🚫 不是帳號目錄——`logout` 會把那裡清光，
            // 而 recovery key 正是清完之後唯一回得去的路；維護者 2026-09-09）。
            let session = context.session().await?;
            wbf_core::recovery::save(
                &context.unlock.data_dir,
                context.vault()?,
                &session.user_id,
                &recovery_key,
            )?;
            // 會印秘密的命令（另一個是 `recovery show`）。CLI 規格 §3.6。
            context.progress(
                "this recovery key is now sealed under <data dir>/r/, which survives logout,\n       \
                 so you will not be asked to type it on this machine.\n       \
                 Still write it down: if this machine is lost, it is the only way back into the\n       \
                 server-side backup."
                    .into(),
            );
            print_json(&json!({ "recovery_key": recovery_key }))
        }
    }
}

/// 把全部房間金鑰倒進本地快照（`key-backup save`；`key-backup upload` 也會順手叫一次）。
///
/// 🚫 `login` **不叫它**：剛登入的 crypto store 幾乎沒有金鑰，存了也是空的
/// （PR #19 審查 rumia🟡3／salvia🟡2：原本的 docstring 承諾了不存在的行為）。
///
/// Return:
///     Ok(u64)   快照有多少 byte
async fn save_room_key_snapshot(
    context: &Context,
    backend: &MatrixBackend,
) -> Result<u64, SdkError> {
    let account = context.account()?;
    let key = context.vault()?.room_key_backup_key();
    backend
        .save_room_key_snapshot(
            &room_keys::snapshot_path(&account.dir),
            &room_keys::snapshot_temp_path(&account.dir),
            &room_keys::snapshot_passphrase(&key),
        )
        .await
}

/// 使用者明說要做的事，被 conf 的開關關掉了（§10.4）。
///
/// ⚠️ 🚫 不靜默跳過：命令是他打的，回一句「好了」卻什麼都沒做，比拒絕更糟。
/// 訊息要說出**是哪個鍵**關的，不然他得自己翻檔案找。
///
/// Args:
///     key: example: "LOCAL_ROOM_KEYS"
///     what: example: "write a local snapshot"
fn refuse_switched_off(key: &str, what: &str) -> SdkError {
    SdkError::Usage(format!(
        "{key}=off in the config file, so this command will not {what}; \
         set {key}=on (or remove the line) to allow it"
    ))
}

/// 這個帳號的歷史**救得回來嗎**——只有正面認得才算數（local-cache-db.md §10.7）。
///
/// 🚫 不寫成「沒有 recovery key 才擋」：上游哪天多一種 `RecoveryState`，那種寫法會默默放行。
///
/// Args:
///     status: `find_backup_status_of` 的結果；`None` 是「問不到」
/// Return:
///     bool   true 只在「server 上有 backup ＆ recovery key 真的設好了」；問不到一律 false
fn is_history_recoverable(status: Option<&wbf_sdk::backend::matrix_sdk::BackupStatus>) -> bool {
    status.is_some_and(|status| status.exists_on_server && status.recovery_enabled)
}

/// 用**這個帳號自己的** session 與 store 開一個 backend（閘門要拿它問 server）。
///
/// ⚠️ 🚫 **不要用 `rooms::backend(context)`**：那條路走 `context.session()` → `context.account()`，
/// 解析的是 **current 帳號**（或 `--account` 覆蓋的那個），不是傳進來的 `account`。
/// `account del <user>` 的目標是 `<user>`，用 current 的狀態判斷會放行不該放行的刪除
/// （PR #19 審查 rumia／salvia 🔴1：current 有 recovery key 就把別的帳號的金鑰刪了）。
///
/// Args:
///     account: **目標**帳號, example: context.account()? 或 find_account_by_full_mxid(...)
/// Return:
///     Some(MatrixBackend)  開起來了，而且已經 sync 過一次
///     None                 沒 session、store 開不了、連不上 server——閘門會因此擋下來（fail closed）
async fn find_backend_of(context: &Context, account: &AccountDir) -> Option<MatrixBackend> {
    let vault = context.vault().ok()?;
    let session = vault.unseal_session(&account.session_path()).ok()??;
    let backend = MatrixBackend::restore(
        &session,
        &account.matrix_store_dir(),
        &vault.matrix_store_key(),
        context.server_backup,
    )
    .await
    .ok()?;
    // recovery 的狀態要 sync 過才是真的（它從 account data／secret storage 來）。
    let _ = backend.sync_once(None, std::time::Duration::ZERO).await;
    Some(backend)
}

/// `logout`／`account del`／`account destroy` 的閘門（local-cache-db.md §10.7）。
///
/// 這些命令會連 `m/`（crypto store）與 `k/`（本地快照）一起刪。在還沒有 recovery key 的
/// 預設狀態下，**server 端備份的私鑰就在那個 store 裡**——照樣登出的話歷史就回不來了。
///
/// 兩關，都要過（維護者 2026-09-09）：
///
/// 1. **server 那份救得回來嗎**：`exists_on_server && recovery_enabled`，正面認得才算
///    （`is_history_recoverable`）。
/// 2. **這台機器保管著這個帳號的 recovery key 嗎**：查 `<data dir>/r/`（`recovery::find`）。
///    ⚠️ 第 1 關只說得出「SSSS 設好了」——跑過 `key-backup recovery`、印出來、沒抄就關掉
///    終端的人也會通過第 1 關。第 2 關才確認得了「刪完之後這裡還有東西打得開那份備份」。
///
/// 🚫 第 2 關**不問使用者**（維護者 2026-09-09 定）：`key-backup recovery` 產生的當下就封進
/// `r/` 了，而那個目錄 `logout` 不碰。⚠️ 所以它證明的是「這台機器回得去」，不是「使用者手上有」
/// ——機器整台沒了就兩份都沒了，訊息裡因此仍然叫人抄下來。
///
/// Args:
///     accept_history_loss: `--accept-history-loss`，使用者明說接受失去它，兩關都跳過
/// Return:
///     Ok(())       放行（兩關都過、使用者明說接受、或這個帳號本來就登出了）
///     Err(Usage)   擋下來，訊息告訴他下一步
async fn refuse_if_history_would_be_lost(
    context: &Context,
    account: &AccountDir,
    accept_history_loss: bool,
) -> Result<(), SdkError> {
    if accept_history_loss || !account.is_logged_in() {
        // 已經登出的帳號沒有 session 可以問 server，也沒有 token 要失效；只是清本地殘留。
        return Ok(());
    }
    let Some(backend) = find_backend_of(context, account).await else {
        return Err(refusal(
            account,
            "the server could not be reached, so it is unknown whether\n       \
             the backup there can still be decrypted",
        ));
    };
    let status = backend.backup_status().await.ok();
    if !is_history_recoverable(status.as_ref()) {
        return Err(refusal(
            account,
            "the server-side backup cannot be decrypted yet - its key lives in the crypto store\n       \
             that is about to be deleted.\n       \
             Run `wbf-cli key-backup recovery` first to create a recovery key",
        ));
    }
    // 第 2 關：這台機器保管著這個帳號的 recovery key 嗎（維護者 2026-09-09）。
    // 🚫 不問使用者——它封在 <data dir>/r/，而那個目錄 `logout` 不碰，
    // 所以刪完 m/ 與 k/ 之後，它還在，歷史真的救得回來。
    // 🚫 不用使用者打的字串：封存時用的是 server 的權威 mxid。解不出 session 就是問不出
    // 這個帳號是誰——擋下來，不要拿佔位值去查（查不到會變成「沒保管」，方向剛好相反）。
    let Some(session) = context.vault()?.unseal_session(&account.session_path())? else {
        return Err(refusal(
            account,
            "its session could not be opened, so it is unknown which account this is",
        ));
    };
    if wbf_core::recovery::find(&context.unlock.data_dir, context.vault()?, &session.user_id)?
        .is_some()
    {
        return Ok(());
    }
    Err(refusal(
        account,
        "the server says secret storage is set up, but this machine is not keeping that account's\n       \
         recovery key, so nothing here could open the backup afterwards.\n       \
         Run `wbf-cli key-backup recovery` (it seals the key under <data dir>/r/, which\n       \
         survives logout), or `wbf-cli recovery list` to see whose keys are kept here",
    ))
}

/// 閘門擋下來時的訊息：`why` 是這一次為什麼擋，後面接一律相同的出路。
fn refusal(account: &AccountDir, why: &str) -> SdkError {
    SdkError::Usage(format!(
        "this would delete {}'s room keys on this machine (m/ and k/), and\n       \
         {why}.\n       \
         If you only need the history on this machine, `wbf-cli key-backup save` writes a local\n       \
         snapshot - but note that logging out deletes that too.\n       \
         If you do not want that history at all, pass --accept-history-loss.",
        account.label()
    ))
}

/// 裝置層的登出（CLI 規格 §3.1：`logout` 就是 `account del <current 帳號>`）：讓 token 失效，
/// 刪這個帳號的 `session.sealed` 與 `m/`。`cache.db` 裡的紀錄留著——要連那些一起清是 `account destroy`。
///
/// `m/` 不能留：Matrix 的 logout 讓裝置失效，下次 `login` 是新裝置，舊的 crypto store 會擋登入
/// （"account in the store doesn't match"，2026-09-07 實跑）。
///
/// 🚫 自己不印 stdout：`destroy` 會接在它後面再做資料層，兩邊都印就成了兩個 JSON 物件（CLI 規格 §4）。
///
/// Return:
///     Ok(String)   這次登出的是誰（給呼叫者印）, example: "@alice:localhost on http://localhost:6167"
async fn log_out_account(
    context: &Context,
    account: &AccountDir,
    accept_history_loss: bool,
) -> Result<String, SdkError> {
    let described = describe_account(context, account);
    refuse_if_history_would_be_lost(context, account, accept_history_loss).await?;
    let vault = context.vault()?;
    match vault.unseal_session(&account.session_path())? {
        Some(session) => {
            logout(&session).await?;
            vault.delete_sealed_session(&account.session_path())?;
        }
        // 已經登出但目錄還在（上次清到一半、或 del 一個登出中的帳號）：本地照樣清乾淨。
        None => context.progress(format!(
            "{} is already logged out; cleaning up the local files",
            account.label()
        )),
    }
    context.unlock.delete_ticket()?;
    account.delete_matrix_store()?;
    // 維護者 2026-09-09：離開這台機器就清乾淨——本地的房間金鑰備份跟著走（local-cache-db.md §10.7）。
    // 上面的閘門已經確認過「server 那份救得回來」，或使用者明說接受失去它。
    room_keys::del_snapshot(&account.dir)?;
    accounts::clear_current_if(&context.unlock.data_dir, account)?;
    // 這個 server 最後一個帳號登出：快取沒有主人了，整個丟（維護者：「除非所有帳號被登出」）。
    let server_dir = account.server_dir();
    if !accounts::has_any_logged_in_account(&server_dir)
        && wbf_sdk::cache::remove_cache(&server_dir)?
    {
        context.progress(format!(
            "removed {} (no account on this server is logged in any more)",
            server_dir.join(wbf_sdk::cache::CACHE_FILE_NAME).display()
        ));
    }
    Ok(described)
}

/// `account destroy <mxid>`：裝置層加資料層。先 `account del` 那一整套，再跑忘掉鏈
/// （local-cache-db.md §6）把這個帳號在 `cache.db` 裡**獨有**的東西清掉 —— 別的帳號也持有的一律不動
/// （維護者 2026-09-09：扣除別人帳號的持有）。
/// 要開哪個 server 的快取：`--server`，或那個帳號還登入著就用它封著的 session。
async fn destroy_account_command(
    context: &Context,
    user: &str,
    yes: bool,
    accept_history_loss: bool,
) -> Result<(), SdkError> {
    // 維護者 2026-09-10：會刪檔的命令，路徑當場刷新一次再比對——帳號目錄與 recovery key
    // 都從**同一份**快照來，中間不再掃第二次（掃兩次就有兩個不同時刻的答案）。
    let map = context.core()?.refresh_data_dir_map()?;
    let account = find_account_by_full_mxid(context, &map, user)?;
    // 🚫 在 logout 之前先問：`del` 只認精確的 mxid，而使用者打的那串大小寫可能跟
    // 封存時用的權威 mxid 不同（PR #19 審查 rumia🟡1／salvia）。
    let kept_recovery_key_user_id = map.find_recovery_key_user_id(user)?.map(str::to_string);
    let server = match &context.server_override {
        Some(server) => server.clone(),
        None => context
            .vault()?
            .unseal_session(&account.session_path())?
            .map(|session| session.server)
            .ok_or_else(|| {
                SdkError::Usage(format!(
                    "{} is logged out, so the server URL is unknown; pass --server",
                    account.label()
                ))
            })?,
    };
    if !yes
        && !crate::rooms::confirm(&format!(
            "destroy {user} on {server}? this logs the device out and deletes its session, crypto store, recovery key, and the cached events only this account has"
        ))?
    {
        return Err(SdkError::Usage("cancelled".into()));
    }
    // 先裝置層（logout、session.sealed、m/）再資料層：反過來的話 logout 要用的 session 已經被刪了。
    // 回顯用 logout 那邊的權威描述，🚫 不是使用者打進來的字串——`account del` 印的就是這個，
    // 兩個命令對同一個帳號要印同一件事（PR #21 審查 rumia🟢2）。
    let described = log_out_account(context, &account, accept_history_loss).await?;
    // ⚠️ destroy 的語意是「什麼都不留」，所以連 recovery key 也摧毀（維護者 2026-09-09）。
    // 🚫 `logout`／`account del` 不做這件事——它們留著它正是為了讓歷史救得回來。
    // 這一步之後，server 上那份備份就永遠解不開了。
    if let Some(kept_user_id) = &kept_recovery_key_user_id {
        wbf_core::recovery::del(&context.unlock.data_dir, context.vault()?, kept_user_id)?;
        context.progress(format!(
            "destroyed the recovery key kept here for {kept_user_id}; the server-side backup can no longer be opened"
        ));
    }
    // 快取在 server 層；用這個帳號的目錄定位它（不需要它是 current）。
    let identity = CacheIdentity { server };
    let (mut cache, _) = Cache::open(
        &account.server_dir(),
        &context.vault()?.cache_key(),
        &identity,
    )?;
    // 🚫 不拿使用者打的 `user` 去查 `users` 列：那裡存的是權威 mxid，精確比對差一個大小寫
    // 就查不到，然後 destroy 會印 `events_removed: 0`，看起來像「本來就沒有」，其實是全部
    // 殘留（PR #21 審查 salvia🔴——跟 recovery key 那條是同一個形狀）。
    let cached_mxids = cache.list_account_mxids()?;
    let report = match accounts::find_matching_plaintext(&cached_mxids, user, "cached account")? {
        Some(cached_mxid) => cache.forget_account(cached_mxid)?,
        // 真的沒有這個帳號的快取列（`login` 之後還沒 `recent` 過就是這樣）。
        None => Default::default(),
    };
    // DB 先、檔案後（local-cache-db.md §6 的忘掉鏈）：列已經刪了，現在刪池裡沒人指的檔。刪不掉只說一聲，下次 media-gc 的 sweep 會再收。
    let pool = MediaPool::open(&account.server_dir(), context.vault()?.media_store_key())?;
    let mut files_removed = 0u64;
    for file in &report.orphan_pool_files {
        match pool.remove(file) {
            Ok(()) => files_removed += 1,
            Err(error) => context.progress(format!("could not remove pool file {file}: {error}")),
        }
    }
    print_json(&json!({
        "ok": true, "user": described,
        "events_removed": report.events_removed, "media_removed": report.media_removed,
        "pool_files_removed": files_removed,
    }))
}

// ---- 上傳 ----

fn state_path_for(file: &Path) -> PathBuf {
    let mut name = file.file_name().unwrap_or_default().to_os_string();
    name.push(".wbf-upload.json");
    file.with_file_name(name)
}

fn parse_cipher(name: Option<&str>) -> Result<Cipher, SdkError> {
    match name {
        None => Ok(Cipher::default_for_this_machine()),
        Some(name) => Cipher::from_name(name).ok_or_else(|| {
            SdkError::Usage(format!(
                "--cipher {name}: use chacha20-poly1305, aes-256-gcm or none"
            ))
        }),
    }
}

async fn upload_file(context: &Context, args: &UploadArgs) -> Result<(), SdkError> {
    let manifest = upload_file_to_manifest(context, args).await?;
    emit_manifest(&manifest, args.manifest.as_deref())
}

/// 固定大小上傳的整條路（狀態檔、續傳、Seal），回 manifest；`upload` 與 `send --file` 共用。
pub async fn upload_file_to_manifest(
    context: &Context,
    args: &UploadArgs,
) -> Result<Manifest, SdkError> {
    let path = args
        .file
        .as_deref()
        .ok_or_else(|| SdkError::Usage("upload needs a file, or --stream".into()))?;
    let mut source = std::fs::File::open(path)?;
    let file_size = source.metadata()?.len();
    let session = context.session().await?;
    let mut client = context.client().await?;
    let state_path = state_path_for(path);

    let (state, from_chunk) = match std::fs::read(&state_path) {
        Ok(bytes) => {
            let state = UploadState::from_json(&bytes)?;
            if !state.is_for(&session.server, &session.user_id) {
                return Err(SdkError::Usage(format!(
                    "{} belongs to another server or user; delete it or use `abort`",
                    state_path.display()
                )));
            }
            if state.block.file_size != Some(file_size) {
                return Err(SdkError::Usage(format!(
                    "{} was written for a file of another size; delete it or use `abort`",
                    state_path.display()
                )));
            }
            let status = client.upload_status(state.upload_id).await?;
            if status.finished {
                // 上次在 Seal 前被殺：server 已經收齊，下面的 send_chunks 一塊也不會送（只在 --sha256 時重算雜湊），直接 Seal。
                context.progress("all chunks already on the server; sealing".to_string());
            } else {
                context.progress(format!("resume from chunk {}", status.received));
            }
            (state, status.received)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let cipher = parse_cipher(args.cipher.as_deref())?;
            let chunk_size = args
                .chunk_size
                .unwrap_or_else(|| choose_chunk_size(file_size));
            let file_cipher = FileCipher::generate(cipher, chunk_size);
            let mut block = file_cipher.to_event_block(file_size);
            block.name = Some(match &args.name {
                Some(name) => name.clone(),
                None => path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned(),
            });
            block.mimetype = args.mimetype.clone();
            let state = client
                .create_upload(&session.server, &session.user_id, &file_cipher, &block)
                .await?;
            write_private(&state_path, &state.to_json())?;
            (state, 0)
        }
        Err(error) => return Err(error.into()),
    };

    let summary = client
        .send_chunks(
            &state,
            &mut source,
            from_chunk,
            args.sha256,
            &mut |done, total| match total {
                Some(total) => context.progress(format!("chunk {done}/{total}")),
                None => context.progress(format!("chunk {done}")),
            },
        )
        .await?;
    let mut final_block = state.block.clone();
    final_block.sha256 = summary.sha256;
    let manifest = client.seal_upload(&state, &final_block).await?;
    remove_if_exists(&state_path)?;
    if summary.truncated {
        eprintln!("warning: server truncated this upload at its size limit");
    }
    Ok(manifest)
}

async fn upload_stream(context: &Context, args: &UploadArgs) -> Result<(), SdkError> {
    if args.file.is_some() {
        return Err(SdkError::Usage(
            "--stream reads stdin; do not pass a file".into(),
        ));
    }
    let session = context.session().await?;
    let mut client = context.client().await?;
    let cipher = parse_cipher(args.cipher.as_deref())?;
    let link = if args.link == "wifi" {
        Link::WifiOrWired
    } else {
        Link::MobileOrUnknown
    };
    let chunk_size = args
        .chunk_size
        .unwrap_or_else(|| choose_stream_chunk_size(link));
    let file_cipher = FileCipher::generate(cipher, chunk_size);
    let mut block = file_cipher.to_event_block(0);
    block.file_size = None;
    block.name = args.name.clone();
    block.mimetype = args.mimetype.clone();
    let state = client
        .create_upload(&session.server, &session.user_id, &file_cipher, &block)
        .await?;

    let mut stdin = std::io::stdin().lock();
    let summary = client
        .send_stream(&state, &mut stdin, &mut |done, _| {
            context.progress(format!("chunk {done}"))
        })
        .await?;
    let mut final_block = state.block.clone();
    final_block.file_size = Some(summary.file_size);
    final_block.sha256 = summary.sha256;
    let manifest = client.seal_upload(&state, &final_block).await?;
    if summary.truncated {
        eprintln!("warning: server truncated this upload at its size limit");
    }
    emit_manifest(&manifest, args.manifest.as_deref())
}

/// manifest 含 key：給了路徑就用私有權限寫檔，否則印到 stdout（CLI 規格 §5）。
fn emit_manifest(manifest: &Manifest, path: Option<&Path>) -> Result<(), SdkError> {
    match path {
        Some(path) => {
            write_private(path, &manifest.to_json())?;
            print_json(
                &json!({ "mxc": manifest.mxc, "manifest": path.display().to_string(),
                "file_size": manifest.block.file_size, "chunk_size": manifest.block.chunk_size }),
            )
        }
        None => {
            let value: serde_json::Value =
                serde_json::from_slice(&manifest.to_json()).expect("manifest is json");
            print_json(&value)
        }
    }
}

// ---- 下載 ----

/// 讀 manifest 並核對它是這個 session 的 server 的：顯式拒絕「拿 A server 的 manifest 去打 B server」，
/// 不讓使用者看到 NotFound 還要自己猜（與上傳狀態檔的 `is_for` 同一個規則）。
async fn read_manifest(context: &Context, path: &Path) -> Result<Manifest, SdkError> {
    let manifest = Manifest::from_json(&std::fs::read(path)?)?;
    let session = context.session().await?;
    if manifest.server.trim_end_matches('/') != session.server.trim_end_matches('/') {
        return Err(SdkError::Usage(format!(
            "manifest is for {}, but the session is on {}",
            manifest.server, session.server
        )));
    }
    Ok(manifest)
}

/// 沒給 `-o` 時用描述的 `name`：它是對方寫的，帶路徑分隔符或是 `..` 就不能當檔名，要求明給 `-o`。
fn output_path_from_name(name: Option<&str>) -> Result<PathBuf, SdkError> {
    let name = name
        .filter(|name| !name.is_empty())
        .unwrap_or("download.bin");
    let is_plain_file_name =
        !name.contains(['/', '\\']) && name != "." && name != ".." && !name.contains('\0');
    if !is_plain_file_name {
        return Err(SdkError::Usage(format!(
            "the file's name {name:?} is not a plain file name; pass -o"
        )));
    }
    Ok(PathBuf::from(name))
}

async fn info_command(
    context: &Context,
    mxc: &str,
    manifest: Option<&Path>,
) -> Result<(), SdkError> {
    let mut client = context.client().await?;
    let (info, description_data) = client.fetch_info(mxc).await?;
    let mut output = json!({
        "total_len": info.total_len, "file_size": info.file_size, "chunk_size": info.chunk_size,
        "chunk_count": info.chunk_count, "truncated": info.truncated, "content_type": info.content_type,
    });
    if let Some(path) = manifest {
        let manifest = read_manifest(context, path).await?;
        if manifest.mxc != mxc {
            return Err(SdkError::Usage(format!(
                "manifest is for {}, not {mxc}",
                manifest.mxc
            )));
        }
        // 約定 §3.1 第 2 條與描述交叉核對都在 verify_target 裡；過了再解一次描述給人看。
        let target = client.verify_target(&manifest).await?;
        let json = target
            .file_cipher
            .open_description(DescriptionSlot::Seal, &description_data)
            .or_else(|_| {
                target
                    .file_cipher
                    .open_description(DescriptionSlot::Create, &description_data)
            })?;
        let description =
            ChunkedBlock::from_description_json(&json).map_err(SdkError::Integrity)?;
        output["description"] = serde_json::to_value(&description).expect("block serializes");
        output["verified"] = json!(true);
    }
    print_json(&output)
}

async fn download_command(
    context: &Context,
    manifest_path: &Path,
    out: Option<PathBuf>,
    no_cache: bool,
) -> Result<(), SdkError> {
    let manifest = read_manifest(context, manifest_path).await?;
    let out = match out {
        Some(out) => out,
        None => output_path_from_name(manifest.block.name.as_deref())?,
    };
    // 登入中且沒說 --no-cache：走媒體快取（池裡有就不連 server；沒有就邊下邊進池、可續傳），再從池複製到 --out。
    if !no_cache && context.token_override.is_none() {
        return download_via_cache(context, &manifest, &out).await;
    }
    let mut client = context.client().await?;
    let mut file = std::fs::File::create(&out)?;
    let result = client
        .download(&manifest, &mut file, &mut |done, total| {
            context.progress(format!("chunk {done}/{total}"))
        })
        .await;
    let report = match result {
        Ok(report) => report,
        Err(error) => {
            // 半成品不留（CLI 規格 §4 exit 3 的語意）。
            drop(file);
            remove_if_exists(&out)?;
            return Err(error);
        }
    };
    file.flush()?;
    print_json(
        &json!({ "out": out.display().to_string(), "bytes": report.bytes, "chunks": report.chunks,
        "sha256_verified": report.sha256_verified }),
    )
}

/// `download` 的快取路徑（local-cache-db.md §8.7）：`media::fetch` 負責命中／續傳／進池，這裡只把池裡的檔複製出來。
async fn download_via_cache(
    context: &Context,
    manifest: &Manifest,
    out: &Path,
) -> Result<(), SdkError> {
    let (mut cache, _me) = context.cache().await?;
    let pool = context.media_pool()?;
    let mut client = context.client().await?;
    let fetched = media::fetch(
        &mut client,
        manifest,
        &mut cache,
        &pool,
        &mut |done, total| context.progress(format!("chunk {done}/{total}")),
    )
    .await?;
    let pool_file = fetched
        .entry
        .pool_file
        .as_deref()
        .ok_or_else(|| SdkError::Io(std::io::Error::other("fetched media has no pool file")))?;
    let mut reader = pool.open_read(pool_file)?;
    let mut file = std::fs::File::create(out)?;
    let bytes = std::io::copy(&mut reader, &mut file)?;
    file.flush()?;
    // sha256_verified 只在這次真的逐塊下載、整檔核對過才是 true；命中快取沒有重算，報 false，`hash` 給的是快取列記的校驗碼
    // （PR #14 審查 rumia 🟡1）。
    let (source, chunks, sha256_verified) = match fetched.outcome {
        FetchOutcome::CacheHit => ("cache", 0, false),
        FetchOutcome::Downloaded { chunks, .. } => {
            ("server", chunks, manifest.block.sha256.is_some())
        }
    };
    print_json(&json!({
        "out": out.display().to_string(), "bytes": bytes, "chunks": chunks, "source": source,
        "pool_file": pool_file, "hash": fetched.entry.hash, "sha256_verified": sha256_verified,
    }))
}

async fn media_stats_command(context: &Context) -> Result<(), SdkError> {
    let (cache, _me) = context.cache().await?;
    let pool = context.media_pool()?;
    let complete = cache.list_media_by_last_used()?;
    let incomplete = cache.list_media_incomplete()?;
    print_json(&json!({
        "pool_dir": pool.dir().display().to_string(),
        "bytes_on_disk": cache.media_bytes_on_disk()?,
        "complete_files": complete.len(),
        "incomplete_files": incomplete.len(),
        "pending_on_disk": pool.list_pending()?.len(),
        "oldest_last_used_at": complete.first().map(|entry| entry.last_used_at),
    }))
}

async fn media_gc_command(
    context: &Context,
    quota_mib: u64,
    protect_days: u64,
) -> Result<(), SdkError> {
    let (mut cache, _me) = context.cache().await?;
    let pool = context.media_pool()?;
    let protect = std::time::Duration::from_secs(protect_days * 24 * 3600);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0);
    let swept = media::sweep(&mut cache, &pool, protect, now)?;
    let report = media::collect_garbage(&mut cache, &pool, quota_mib * 1024 * 1024, protect, now)?;
    if report.still_over_quota {
        context.progress(format!(
            "media cache is still over quota ({} bytes > {} MiB); everything left is inside the {protect_days}-day protection window",
            report.bytes_after, quota_mib
        ));
    }
    print_json(&json!({
        "bytes_before": report.bytes_before, "bytes_after": report.bytes_after,
        "files_removed": report.files_removed, "still_over_quota": report.still_over_quota,
        "swept_missing_files": swept.reset_rows, "swept_pending": swept.removed_pending,
        "swept_orphan_files": swept.removed_orphan_files,
    }))
}

async fn seek_command(
    context: &Context,
    manifest_path: &Path,
    at: u64,
    len: Option<u64>,
) -> Result<(), SdkError> {
    let manifest = read_manifest(context, manifest_path).await?;
    let mut client = context.client().await?;
    let result = client.seek_read(&manifest, at, len).await?;
    let mut stdout = std::io::stdout().lock();
    stdout.write_all(&result.bytes)?;
    stdout.flush()?;
    // 摘要是結果不是進度，--quiet 也印（CLI 規格 §3.3.1）。
    eprintln!(
        "{}",
        json!({ "at": at, "len": len, "bytes": result.bytes.len(), "chunks_read": result.chunks_read,
            "truncated": result.truncated })
    );
    Ok(())
}

// ---- 小工具 ----

fn print_json(value: &serde_json::Value) -> Result<(), SdkError> {
    let mut stdout = std::io::stdout().lock();
    serde_json::to_writer(&mut stdout, value).map_err(|error| SdkError::Io(error.into()))?;
    stdout.write_all(b"\n")?;
    Ok(())
}

fn remove_if_exists(path: &Path) -> Result<(), SdkError> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
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
            unlock_ttl: None,
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
            dir.join(crate::conf::CONF_FILE_NAME),
            "[general]\nSERVER=http://from-conf:6167\nACCOUNT=@alice:localhost\nUNLOCK_TTL=60\nTRANSPORT=http\n[backup]\nSERVER_BACKUP=off\n",
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
        assert_eq!(
            context.unlock.unlock_ttl,
            std::time::Duration::from_secs(60)
        );
        assert!(!context.server_backup);
        // 🚫 沒寫的鍵落到安全值，不是落到 false。
        assert!(context.local_room_keys);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_flag_beats_the_conf_file() {
        let dir = scratch("flag");
        std::fs::write(
            dir.join(crate::conf::CONF_FILE_NAME),
            "[general]\nSERVER=http://from-conf:6167\nUNLOCK_TTL=60\n",
        )
        .unwrap();
        let mut cli = cli_with(&dir);
        cli.server = Some("http://from-flag:6167".into());
        cli.unlock_ttl = Some(5);
        let context = Context::from(&cli).unwrap();
        assert_eq!(
            context.server_override.as_deref(),
            Some("http://from-flag:6167")
        );
        assert_eq!(context.unlock.unlock_ttl, std::time::Duration::from_secs(5));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn no_conf_file_means_the_built_in_defaults() {
        let dir = scratch("none");
        let context = Context::from(&cli_with(&dir)).unwrap();
        assert_eq!(context.server_override, None);
        assert_eq!(
            context.unlock.unlock_ttl,
            std::time::Duration::from_secs(900)
        );
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
            dir.join(crate::conf::CONF_FILE_NAME),
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
            dir.join(crate::conf::CONF_FILE_NAME),
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

        let written = std::fs::read_to_string(dir.join(crate::conf::CONF_FILE_NAME)).unwrap();
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
        let path = dir.join(crate::conf::CONF_FILE_NAME);
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
        assert!(!dir.join(crate::conf::CONF_FILE_NAME).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wbf_sdk::backend::matrix_sdk::BackupStatus;

    fn status(exists_on_server: bool, recovery_enabled: bool) -> BackupStatus {
        BackupStatus {
            exists_on_server,
            enabled_locally: true,
            recovery_enabled,
            recovery_state: "Enabled".into(),
        }
    }

    /// `logout` 的閘門（local-cache-db.md §10.7）：只有正面認得「救得回來」才放行。
    /// 這裡失敗＝有人把判斷改成「不是 X 就放行」那種形狀，而那會在上游多一種狀態時默默開門。
    #[test]
    fn history_is_only_recoverable_when_both_halves_are_true() {
        assert!(is_history_recoverable(Some(&status(true, true))));

        assert!(
            !is_history_recoverable(Some(&status(true, false))),
            "server 上有 backup 但 SSSS 沒設好：那份備份換一台機器解不開"
        );
        assert!(
            !is_history_recoverable(Some(&status(false, true))),
            "SSSS 設好了但 server 上根本沒有 backup"
        );
        assert!(
            !is_history_recoverable(None),
            "問不到 server 就不是「正面認得救得回來」——fail closed"
        );
    }
}

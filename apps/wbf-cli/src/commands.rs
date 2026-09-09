//! 每個命令一個函數，照 CLI 規格 §3。stdout 只有結果 JSON（`seek` 例外：明文 bytes），進度與警告在 stderr。

use std::io::Write;
use std::path::{Path, PathBuf};

use serde_json::json;
use wbf_sdk::backend::matrix_sdk::MatrixBackend;
use wbf_sdk::cache::{Cache, CacheIdentity, OpenOutcome};
use wbf_sdk::media::{self, FetchOutcome};
use wbf_sdk::media_pool::MediaPool;

use crate::accounts::{self, AccountDir};
use wbf_sdk::chunk_crypto::{choose_chunk_size, choose_stream_chunk_size, DescriptionSlot, Link};
use wbf_sdk::login::{logout, whoami};
use wbf_sdk::{
    Channel, ChunkedBlock, Cipher, FileCipher, Manifest, SdkError, Session, Transport, UploadState,
    Vault, WbfClient,
};

use wbf_sdk::vault::write_private;

use crate::unlock::{
    default_data_dir, prompt_new_passphrase, prompt_password_on_terminal, read_password_file,
    UnlockOptions,
};
use crate::{AccountAction, Cli, Command, LoginArgs, UploadArgs};

pub const CLIENT_NAME: &str = concat!("wbf-cli/", env!("CARGO_PKG_VERSION"));

pub async fn run(cli: Cli) -> Result<(), SdkError> {
    let context = Context::from(&cli)?;
    match cli.command {
        Command::Login(args) => login_command(&context, &args).await,
        Command::Logout => {
            // --token 模式沒有帳號目錄，只讓 server 端的 token 失效。
            if context.token_override.is_some() {
                logout(&context.session().await?).await?;
                return print_json(&json!({ "ok": true }));
            }
            let account = context.account()?.clone();
            let user = log_out_account(&context, &account).await?;
            print_json(&json!({ "ok": true, "user": user }))
        }
        Command::Account { action } => account_command(&context, action).await,
        Command::Lock => {
            let removed = context.unlock.delete_ticket()?;
            print_json(&json!({ "ok": true, "had_ticket": removed }))
        }
        Command::SetPassphrase {
            new_passphrase_file,
        } => {
            let mut vault = context.unlock.open_vault()?;
            let passphrase = match new_passphrase_file {
                Some(path) => read_password_file(&path)?,
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
                upload_stream(&context, &args).await
            } else {
                upload_file(&context, &args).await
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
        Command::Info { mxc, manifest } => info_command(&context, &mxc, manifest.as_deref()).await,
        Command::Download {
            manifest,
            out,
            no_cache,
        } => download_command(&context, &manifest, out, no_cache).await,
        Command::MediaStats => media_stats_command(&context).await,
        Command::MediaGc {
            quota_mib,
            protect_days,
        } => media_gc_command(&context, quota_mib, protect_days).await,
        Command::Seek { manifest, at, len } => seek_command(&context, &manifest, at, len).await,
        Command::Rooms => crate::rooms::rooms_command(&context).await,
        Command::Send(args) => crate::rooms::send_command(&context, &args).await,
        Command::Watch(args) => crate::rooms::watch_command(&context, &args).await,
        Command::Recent {
            limit,
            window,
            batch,
            from_scratch,
        } => {
            let plan = wbf_sdk::RecentPlan {
                max_events: (limit > 0).then_some(limit),
                window,
                batch,
            };
            crate::recent::recent_command(&context, plan, from_scratch).await
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
                &context,
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
                &context,
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

/// 全域參數解析完的樣子：server 與 token 從哪來，只在這裡決定一次。
pub struct Context {
    pub unlock: UnlockOptions,
    /// 一個命令只解鎖一次：`session()` 與房間命令的 store 都從這裡拿，不然 Argon2 跑兩次、ticket 寫兩次。
    opened_vault: std::sync::OnceLock<Vault>,
    /// 這個命令用的帳號目錄，也只解析一次。
    account: std::sync::OnceLock<AccountDir>,
    pub account_override: Option<String>,
    pub server_override: Option<String>,
    pub token_override: Option<String>,
    pub quiet: bool,
    pub transport: Transport,
}

impl Context {
    fn from(cli: &Cli) -> Result<Context, SdkError> {
        let data_dir = match &cli.data_dir {
            Some(path) => path.clone(),
            None => default_data_dir()?,
        };
        Ok(Context {
            opened_vault: std::sync::OnceLock::new(),
            account: std::sync::OnceLock::new(),
            account_override: cli.account.clone(),
            unlock: UnlockOptions {
                data_dir,
                passphrase_file: cli.passphrase_file.clone(),
                unlock_ttl: std::time::Duration::from_secs(cli.unlock_ttl),
                quiet: cli.quiet,
            },
            server_override: cli.server.clone(),
            token_override: cli.token.clone(),
            quiet: cli.quiet,
            transport: Transport::from_name(&cli.transport).expect("clap restricts the values"),
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
        let vault = self.vault()?;
        let dir_key = vault.account_dir_key();
        let account = match &self.account_override {
            Some(user) => accounts::find_account(
                &self.unlock.data_dir,
                vault,
                user,
                self.server_override.as_deref(),
            )?,
            None => {
                let current = accounts::read_current(&self.unlock.data_dir)?.ok_or_else(|| {
                    SdkError::Usage(format!(
                        "no current account in {}; run `login` first (or pass --account)",
                        self.unlock.data_dir.display()
                    ))
                })?;
                accounts::find_account_of_current(&self.unlock.data_dir, &dir_key, &current)
                    .ok_or_else(|| {
                        SdkError::Usage(format!(
                            "the current account has no readable directory in {}; run `login` again",
                            self.unlock.data_dir.display()
                        ))
                    })?
            }
        };
        Ok(self.account.get_or_init(|| account))
    }

    /// 開一次、之後都拿同一個（唯讀）。要改鎖法的命令自己 `unlock.open_vault()` 拿可變的那份。
    pub fn vault(&self) -> Result<&Vault, SdkError> {
        if let Some(vault) = self.opened_vault.get() {
            return Ok(vault);
        }
        let vault = self.unlock.open_vault()?;
        Ok(self.opened_vault.get_or_init(|| vault))
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
}

/// `login`＝`account add`：登入、封 session、**自動切成 current** 並印一行 switch 提示（CLI 規格 §3.1.1）。
async fn login_command(context: &Context, args: &LoginArgs) -> Result<(), SdkError> {
    let (user, device_name) = (args.user.as_str(), args.device_name.as_str());
    let server = context
        .server_override
        .clone()
        .ok_or_else(|| SdkError::Usage("login needs --server (or WBF_SERVER)".into()))?;
    let password = match args.password_file.as_deref() {
        Some(path) => read_password_file(path)?,
        None => prompt_password_on_terminal("password: ")?,
    };
    // vault 先開（沒有就建）：store 的金鑰與 session.sealed 都從它來。
    let vault = context.unlock.open_or_create_vault()?;
    // 帳號目錄由 server host 加 localpart 決定（store 在 login 前就要有路徑）。
    let dir_key = vault.account_dir_key();
    let account = AccountDir::locate(&context.unlock.data_dir, &dir_key, &server, user)?;
    // 沒有 session 卻留著 matrix/：上次沒走 logout（或舊版的 logout 沒刪），那個 store 綁著已經失效的裝置。消費端自己再清一次。
    if !account.is_logged_in() && account.matrix_store_dir().exists() {
        context.progress("removing a matrix store left over from a previous device".into());
        account.delete_matrix_store()?;
    }
    // 第 3 步起走 matrix-sdk 登入：拿到的是有裝置金鑰的 session，E2EE 房間才解得開。store 放帳號目錄的 matrix/。
    let (_backend, session) = MatrixBackend::login(
        &server,
        user,
        &password,
        device_name,
        &account.matrix_store_dir(),
        &vault.matrix_store_key(),
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
            let listed = accounts::list_accounts(&context.unlock.data_dir, vault)?;
            if let Some(hint) = accounts::find_undecryptable_layout_hint(
                &context.unlock.data_dir,
                &vault.account_dir_key(),
            ) {
                context.progress(hint);
            }
            print_json(&serde_json::to_value(listed).expect("serializes"))
        }
        AccountAction::Switch { user } => {
            let account = find_account_by_full_mxid(context, &user)?;
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
        AccountAction::Del { user } => {
            let account = find_account_by_full_mxid(context, &user)?;
            let user = log_out_account(context, &account).await?;
            print_json(&json!({ "ok": true, "user": user }))
        }
        AccountAction::Destroy { user, yes } => destroy_account_command(context, &user, yes).await,
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
fn find_account_by_full_mxid(context: &Context, user: &str) -> Result<AccountDir, SdkError> {
    let vault = context.vault()?;
    if !user.starts_with('@') || !user.contains(':') {
        let known = accounts::list_accounts(&context.unlock.data_dir, vault)?;
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
    accounts::find_account(
        &context.unlock.data_dir,
        vault,
        user,
        context.server_override.as_deref(),
    )
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

/// 裝置層的登出（CLI 規格 §3.1：`logout` 就是 `account del <current 帳號>`）：讓 token 失效，
/// 刪這個帳號的 `session.sealed` 與 `matrix/`。`cache.db` 裡的紀錄留著——要連那些一起清是 `account destroy`。
///
/// `matrix/` 不能留：Matrix 的 logout 讓裝置失效，下次 `login` 是新裝置，舊的 crypto store 會擋登入
/// （"account in the store doesn't match"，2026-09-07 實跑）。
///
/// 🚫 自己不印 stdout：`destroy` 會接在它後面再做資料層，兩邊都印就成了兩個 JSON 物件（CLI 規格 §4）。
///
/// Return:
///     Ok(String)   這次登出的是誰（給呼叫者印）, example: "@alice:localhost on http://localhost:6167"
async fn log_out_account(context: &Context, account: &AccountDir) -> Result<String, SdkError> {
    let described = describe_account(context, account);
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
async fn destroy_account_command(context: &Context, user: &str, yes: bool) -> Result<(), SdkError> {
    let account = find_account_by_full_mxid(context, user)?;
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
            "destroy {user} on {server}? this logs the device out and deletes its session, crypto store, and the cached events only this account has"
        ))?
    {
        return Err(SdkError::Usage("cancelled".into()));
    }
    // 先裝置層（logout、session.sealed、matrix/）再資料層：反過來的話 logout 要用的 session 已經被刪了。
    log_out_account(context, &account).await?;
    // 快取在 server 層；用這個帳號的目錄定位它（不需要它是 current）。
    let identity = CacheIdentity { server };
    let (mut cache, _) = Cache::open(
        &account.server_dir(),
        &context.vault()?.cache_key(),
        &identity,
    )?;
    let report = cache.forget_account(user)?;
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
        "ok": true, "user": user,
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

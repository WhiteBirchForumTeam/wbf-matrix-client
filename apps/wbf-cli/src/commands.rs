//! 每個命令一個函數，照 CLI 規格 §3。stdout 只有結果 JSON（`seek` 例外：明文 bytes），進度與警告在 stderr。

use std::io::Write;
use std::path::{Path, PathBuf};

use serde_json::json;
use wbf_sdk::backend::matrix_sdk::MatrixBackend;
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
use crate::{Cli, Command, UploadArgs};

const CLIENT_NAME: &str = concat!("wbf-cli/", env!("CARGO_PKG_VERSION"));

pub async fn run(cli: Cli) -> Result<(), SdkError> {
    let context = Context::from(&cli)?;
    match cli.command {
        Command::Login {
            user,
            password_file,
            device_name,
        } => login_command(&context, &user, password_file.as_deref(), &device_name).await,
        Command::Logout => {
            let session = context.session().await?;
            logout(&session).await?;
            if context.token_override.is_none() {
                context.vault()?.delete_sealed_session()?;
                context.unlock.delete_ticket()?;
            }
            print_json(&json!({ "ok": true }))
        }
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
        Command::Download { manifest, out } => download_command(&context, &manifest, out).await,
        Command::Seek { manifest, at, len } => seek_command(&context, &manifest, at, len).await,
        Command::Rooms => crate::rooms::rooms_command(&context).await,
        Command::Send(args) => crate::rooms::send_command(&context, &args).await,
        Command::Watch(args) => crate::rooms::watch_command(&context, &args).await,
        Command::Read {
            room,
            limit,
            before,
            types,
            sender,
        } => {
            crate::rooms::read_command(
                &context,
                &room,
                limit,
                before.as_deref(),
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
        } => {
            crate::rooms::files_command(&context, &room, limit, before.as_deref(), save.as_deref())
                .await
        }
    }
}

/// 全域參數解析完的樣子：server 與 token 從哪來，只在這裡決定一次。
pub struct Context {
    pub unlock: UnlockOptions,
    /// 一個命令只解鎖一次：`session()` 與房間命令的 store 都從這裡拿，不然 Argon2 跑兩次、ticket 寫兩次。
    opened_vault: std::sync::OnceLock<Vault>,
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
        let mut session = self.vault()?.unseal_session()?.ok_or_else(|| {
            SdkError::Usage(format!(
                "no session in {}; run `login` first",
                self.unlock.data_dir.display()
            ))
        })?;
        if let Some(server) = &self.server_override {
            session.server = server.clone();
        }
        Ok(session)
    }

    /// 開一次、之後都拿同一個（唯讀）。要改鎖法的命令自己 `unlock.open_vault()` 拿可變的那份。
    pub fn vault(&self) -> Result<&Vault, SdkError> {
        if let Some(vault) = self.opened_vault.get() {
            return Ok(vault);
        }
        let vault = self.unlock.open_vault()?;
        Ok(self.opened_vault.get_or_init(|| vault))
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

async fn login_command(
    context: &Context,
    user: &str,
    password_file: Option<&Path>,
    device_name: &str,
) -> Result<(), SdkError> {
    let server = context
        .server_override
        .clone()
        .ok_or_else(|| SdkError::Usage("login needs --server (or WBF_SERVER)".into()))?;
    let password = match password_file {
        Some(path) => read_password_file(path)?,
        None => prompt_password_on_terminal("password: ")?,
    };
    // vault 先開（沒有就建）：store 的金鑰與 session.sealed 都從它來。
    let vault = context.unlock.open_or_create_vault()?;
    // 第 3 步起走 matrix-sdk 登入：拿到的是有裝置金鑰的 session，E2EE 房間才解得開。store 放 <data dir>/matrix/。
    let (_backend, session) = MatrixBackend::login(
        &server,
        user,
        &password,
        device_name,
        &context.unlock.matrix_store_dir(),
        &vault.matrix_store_key(),
    )
    .await?;
    vault.seal_session(&session)?;
    print_json(
        &json!({ "user_id": session.user_id, "device_id": session.device_id, "server": session.server }),
    )
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
) -> Result<(), SdkError> {
    let manifest = read_manifest(context, manifest_path).await?;
    let out = match out {
        Some(out) => out,
        None => output_path_from_name(manifest.block.name.as_deref())?,
    };
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

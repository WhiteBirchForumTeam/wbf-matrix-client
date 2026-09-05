//! 每個命令一個函數，照 CLI 規格 §3。stdout 只有結果 JSON（`seek` 例外：明文 bytes），進度與警告在 stderr。

use std::io::Write;
use std::path::{Path, PathBuf};

use serde_json::json;
use wbf_sdk::chunk_crypto::{choose_chunk_size, choose_stream_chunk_size, DescriptionSlot, Link};
use wbf_sdk::login::{login_with_password, logout, whoami};
use wbf_sdk::{
    Channel, ChunkedBlock, Cipher, FileCipher, Manifest, SdkError, Session, Transport, UploadState,
    WbfClient,
};

use crate::session::{
    default_session_path, delete_session, read_session, write_private, write_session,
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
            let session = context.session()?;
            logout(&session).await?;
            delete_session(&context.session_path)?;
            print_json(&json!({ "ok": true }))
        }
        Command::Whoami => {
            let who = whoami(&context.session()?).await?;
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
    }
}

/// 全域參數解析完的樣子：server 與 token 從哪來，只在這裡決定一次。
struct Context {
    session_path: PathBuf,
    server_override: Option<String>,
    token_override: Option<String>,
    quiet: bool,
    transport: Transport,
}

impl Context {
    fn from(cli: &Cli) -> Result<Context, SdkError> {
        let session_path = match &cli.session {
            Some(path) => path.clone(),
            None => default_session_path()?,
        };
        Ok(Context {
            session_path,
            server_override: cli.server.clone(),
            token_override: cli.token.clone(),
            quiet: cli.quiet,
            transport: Transport::from_name(&cli.transport).expect("clap restricts the values"),
        })
    }

    /// `--token` 加 `--server` 就不碰 session 檔；否則讀 session 檔，`--server` 可覆蓋。
    fn session(&self) -> Result<Session, SdkError> {
        if let Some(token) = &self.token_override {
            let server = self.server_override.clone().ok_or_else(|| {
                SdkError::Usage("--token needs --server (or WBF_SERVER) too".into())
            })?;
            return Ok(Session {
                server,
                user_id: "unknown".into(),
                device_id: "unknown".into(),
                access_token: token.clone(),
            });
        }
        let mut session = read_session(&self.session_path)?;
        if let Some(server) = &self.server_override {
            session.server = server.clone();
        }
        Ok(session)
    }

    async fn client(&self) -> Result<WbfClient<Channel>, SdkError> {
        let session = self.session()?;
        let channel =
            Channel::connect(&session.server, &session.access_token, self.transport).await?;
        Ok(WbfClient::new(channel))
    }

    fn progress(&self, line: String) {
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
        Some(path) => {
            let text = std::fs::read_to_string(path)?;
            text.strip_suffix('\n')
                .map(|stripped| stripped.strip_suffix('\r').unwrap_or(stripped))
                .unwrap_or(&text)
                .to_string()
        }
        None => rpassword::prompt_password("password: ")?,
    };
    let session = login_with_password(&server, user, &password, device_name).await?;
    drop(password);
    write_session(&context.session_path, &session)?;
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
    let path = args
        .file
        .as_deref()
        .ok_or_else(|| SdkError::Usage("upload needs a file, or --stream".into()))?;
    let mut source = std::fs::File::open(path)?;
    let file_size = source.metadata()?.len();
    let session = context.session()?;
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
            context.progress(format!("resume from chunk {}", status.received));
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
            &mut |done, total| {
                context.progress(format!("chunk {done}/{}", total.unwrap_or(0)));
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
    emit_manifest(&manifest, args.manifest.as_deref())
}

async fn upload_stream(context: &Context, args: &UploadArgs) -> Result<(), SdkError> {
    if args.file.is_some() {
        return Err(SdkError::Usage(
            "--stream reads stdin; do not pass a file".into(),
        ));
    }
    let session = context.session()?;
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

fn read_manifest(path: &Path) -> Result<Manifest, SdkError> {
    Manifest::from_json(&std::fs::read(path)?)
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
        let manifest = read_manifest(path)?;
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
    let manifest = read_manifest(manifest_path)?;
    let out = out.unwrap_or_else(|| {
        PathBuf::from(
            manifest
                .block
                .name
                .clone()
                .filter(|name| !name.is_empty())
                .unwrap_or_else(|| "download.bin".into()),
        )
    });
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
    let manifest = read_manifest(manifest_path)?;
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

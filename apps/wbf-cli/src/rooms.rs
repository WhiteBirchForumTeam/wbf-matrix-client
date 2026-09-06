//! 房間命令（CLI 規格 §3.4）：`rooms`、`send`、`watch`、`read`、`files`。
//! 只碰 `wbf-sdk` 的 `ChatBackend` 與模型型別；matrix-sdk 的東西在 SDK 的 adapter 裡（plan-v1 §7.2）。

use std::io::Write;
use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::json;
use wbf_sdk::backend::matrix_sdk::MatrixBackend;
use wbf_sdk::{
    Attachment, ChatBackend, Cipher, Manifest, Message, MessageKind, SdkError, Update, WatchControl,
};

use crate::commands::{upload_file_to_manifest, Context};
use crate::{SendArgs, UploadArgs, WatchArgs};
use wbf_sdk::vault::write_private;

/// 還原 backend 並做一次增量 sync（timeout 0）：房間列表與新事件到 store，之後的命令才看得到現況。
/// store 在 `<data dir>/matrix/`，金鑰是 vault 的第二把子金鑰（local-cache-db.md §5.3）。
async fn backend(context: &Context) -> Result<MatrixBackend, SdkError> {
    let session = context.session().await?;
    if session.store_dir.is_none() {
        return Err(SdkError::Usage(
            "this session has no matrix store (logged in with an older wbf-cli or --token); run `login` again".into(),
        ));
    }
    let backend = MatrixBackend::restore(
        &session,
        &context.unlock.matrix_store_dir(),
        &context.vault()?.matrix_store_key(),
    )
    .await?;
    backend.sync_once(None, Duration::ZERO).await?;
    Ok(backend)
}

pub async fn rooms_command(context: &Context) -> Result<(), SdkError> {
    let backend = backend(context).await?;
    let conversations = backend.conversations().await?;
    print_json(&serde_json::to_value(conversations).expect("serializes"))
}

pub async fn send_command(context: &Context, args: &SendArgs) -> Result<(), SdkError> {
    let backend = backend(context).await?;
    if let Some(text) = &args.text {
        let event_id = backend.send_text(&args.room, text).await?;
        return print_json(&json!({ "event_id": event_id }));
    }
    let Some(file) = &args.file else {
        return Err(SdkError::Usage("send needs --text or --file".into()));
    };

    let conversation = backend.conversation(&args.room).await?;
    // 約定 §5.1：沒 E2EE 的房間走明文模式，送之前警告並要求確認；🚫 永遠不在沒 E2EE 的房間送加密的區塊（key 會公開）。
    let cipher = if conversation.encrypted {
        args.cipher.clone()
    } else {
        eprintln!(
            "warning: room {} is NOT encrypted: the file will be stored in plaintext on the server and readable by every member and the server itself",
            args.room
        );
        if args
            .cipher
            .as_deref()
            .is_some_and(|cipher| cipher != Cipher::None.name())
        {
            return Err(SdkError::Usage(
                "an unencrypted room only takes --cipher none (an encrypted block's key would be public); drop --cipher".into(),
            ));
        }
        if !args.yes && !confirm("send it in plaintext anyway?")? {
            return Err(SdkError::Usage("cancelled".into()));
        }
        Some(Cipher::None.name().to_string())
    };

    let upload = UploadArgs {
        file: Some(file.clone()),
        stream: false,
        cipher,
        chunk_size: args.chunk_size,
        link: "mobile".into(),
        manifest: args.manifest.clone(),
        sha256: args.sha256,
        name: None,
        mimetype: None,
    };
    let manifest = upload_file_to_manifest(context, &upload).await?;
    if let Some(path) = &args.manifest {
        write_private(path, &manifest.to_json())?;
    }
    let attachment = Attachment {
        mxc: manifest.mxc.clone(),
        block: manifest.block.clone(),
    };
    // 約定 §5.2：附件宣告這一版帶不出去（matrix-sdk 不能加 header、server 的 Event/Send 還是提案）。講清楚，不裝作沒事。
    eprintln!(
        "warning: attachment {} is NOT declared to the server (no Event/Send yet); an unreferenced upload is swept after the server's grace period",
        manifest.mxc
    );
    let event_id = backend
        .send_file(&args.room, &attachment, args.caption.as_deref())
        .await?;
    print_json(&json!({ "event_id": event_id, "mxc": manifest.mxc }))
}

pub async fn watch_command(context: &Context, args: &WatchArgs) -> Result<(), SdkError> {
    let backend = backend(context).await?;
    let me = context.session().await?.user_id;
    let deadline = match args.mode.as_str() {
        "tail" => None,
        "wait" => Some(Duration::from_secs(args.seconds.ok_or_else(|| {
            SdkError::Usage("watch wait needs the seconds".into())
        })?)),
        "once" => args.timeout.map(Duration::from_secs),
        other => return Err(SdkError::Usage(format!("watch mode {other}"))),
    };
    let once = args.mode == "once";
    let room = args.room.clone();
    let mut on_update = |update: Update| -> WatchControl {
        let Update::NewMessage(message) = &update else {
            return WatchControl::Continue;
        };
        if message.conversation != room {
            return WatchControl::Continue;
        }
        // 自己送的也印（腳本自己濾），但 once 不把自己的算「第一則」（CLI 規格 §3.4.2）。
        let own = message.sender == me;
        print_line(message);
        if own {
            return WatchControl::Continue;
        }
        if once {
            WatchControl::Stop
        } else {
            WatchControl::Continue
        }
    };
    let started = Instant::now();
    let end = backend
        .watch(args.since.as_deref(), deadline, &mut on_update)
        .await?;
    eprintln!("since {}", end.since);
    if once && !end.stopped_by_callback {
        return Err(SdkError::Timeout(format!(
            "no event from another sender within {} seconds",
            started.elapsed().as_secs()
        )));
    }
    Ok(())
}

pub async fn read_command(
    context: &Context,
    room: &str,
    limit: u32,
    before: Option<&str>,
    types: &[String],
    sender: Option<&str>,
) -> Result<(), SdkError> {
    let backend = backend(context).await?;
    let page = backend.history(room, before, limit).await?;
    // 過濾在 client 端（CLI 規格 §3.4.1）；濾完可能是空的但 next 還在，呼叫者照 next 判斷。
    let events: Vec<&Message> = page
        .events
        .iter()
        .filter(|message| sender.is_none_or(|sender| message.sender == sender))
        .filter(|message| {
            types.is_empty() || types.iter().any(|wanted| kind_matches(message, wanted))
        })
        .collect();
    print_json(&json!({ "events": events, "next": page.next }))
}

pub async fn files_command(
    context: &Context,
    room: &str,
    limit: u32,
    before: Option<&str>,
    save: Option<&Path>,
) -> Result<(), SdkError> {
    let backend = backend(context).await?;
    let session = context.session().await?;
    let page = backend.history(room, before, limit).await?;
    let mut files = Vec::new();
    for message in &page.events {
        let MessageKind::File { attachment, .. } = &message.kind else {
            continue;
        };
        let manifest = Manifest {
            server: session.server.clone(),
            mxc: attachment.mxc.clone(),
            block: attachment.block.clone(),
        };
        if let Some(dir) = save {
            std::fs::create_dir_all(dir)?;
            let file_name = format!(
                "{}.json",
                message
                    .id
                    .trim_start_matches('$')
                    .replace(['/', '\\', ':'], "_")
            );
            write_private(&dir.join(file_name), &manifest.to_json())?;
        }
        files.push(json!({
            "event_id": message.id, "sender": message.sender, "ts": message.sent_at,
            "manifest": serde_json::from_slice::<serde_json::Value>(&manifest.to_json()).expect("json"),
        }));
    }
    print_json(&json!({ "files": files, "next": page.next }))
}

/// `--type` 對的是我們模型的 kind 名（text／file／deleted／system／unsupported），或 `Unsupported` 帶的原始 event type。
fn kind_matches(message: &Message, wanted: &str) -> bool {
    match &message.kind {
        MessageKind::Text { .. } => wanted == "text" || wanted == "m.room.message",
        MessageKind::File { .. } => wanted == "file" || wanted == "org.wbftw.wbfuwunel.file",
        MessageKind::Deleted { .. } => wanted == "deleted",
        MessageKind::System { event_type, .. } => wanted == "system" || wanted == event_type,
        MessageKind::Unsupported { event_type, .. } => {
            wanted == "unsupported" || event_type.starts_with(wanted)
        }
    }
}

fn confirm(question: &str) -> Result<bool, SdkError> {
    eprint!("{question} [y/N] ");
    std::io::stderr().flush()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    let answer = answer.trim();
    Ok(answer.eq_ignore_ascii_case("y") || answer.eq_ignore_ascii_case("yes"))
}

/// watch 的 JSON Lines：一事件一行、即時 flush（CLI 規格 §3.4.2）。
fn print_line(message: &Message) {
    let mut stdout = std::io::stdout().lock();
    let _ = serde_json::to_writer(&mut stdout, message);
    let _ = stdout.write_all(b"\n");
    let _ = stdout.flush();
}

fn print_json(value: &serde_json::Value) -> Result<(), SdkError> {
    let mut stdout = std::io::stdout().lock();
    serde_json::to_writer(&mut stdout, value).map_err(|error| SdkError::Io(error.into()))?;
    stdout.write_all(b"\n")?;
    Ok(())
}

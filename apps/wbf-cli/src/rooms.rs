//! 房間命令（CLI 規格 §3.4）：`rooms`、`send`、`watch`、`read`、`files`。
//!
//! ⚠️ 這一層**只做三件事**：把旗標翻成 core 的參數、問使用者（確認）、印出來。
//! 做什麼在 `wbf-core`（architecture-v2 §7）——包括寫穿快取、過濾、翻頁那些。

use std::io::Write;
use std::path::Path;

use serde_json::json;
use wbf_core::{
    cipher_for_plaintext_room, watch_mode_from_name, CoreError, CoreErrorKind, CoreEvent,
    HistoryQuery, HistorySource, UploadRequest,
};
use wbf_sdk::vault::write_private;
use wbf_sdk::Message;

use crate::commands::Context;
use crate::{SendArgs, WatchArgs};

pub async fn rooms_command(context: &Context) -> Result<(), CoreError> {
    context.warn_if_backups_are_off();
    let conversations = context
        .core()?
        .list_conversations(&context.target())
        .await?;
    print_json(&serde_json::to_value(conversations).expect("serializes"))
}

pub async fn send_command(context: &Context, args: &SendArgs) -> Result<(), CoreError> {
    context.warn_if_backups_are_off();
    let core = context.core()?;
    let target = context.target();
    if let Some(text) = &args.text {
        let event_id = core.send_text(&args.room, text, &target).await?;
        return print_json(&json!({ "event_id": event_id }));
    }
    let Some(file) = &args.file else {
        return Err(CoreError::new(
            CoreErrorKind::Usage,
            "send needs --text or --file",
        ));
    };

    // 約定 §5.1：沒 E2EE 的房間走明文模式，送之前**警告並要求確認**。
    // 🚫 這個確認是前端的事，core 不問（architecture-v2 §3）。
    let conversation = core.conversation(&args.room, &target).await?;
    let cipher = if conversation.encrypted {
        args.cipher.clone()
    } else {
        eprintln!(
            "warning: room {} is NOT encrypted: the file will be stored in plaintext on the server and readable by every member and the server itself",
            args.room
        );
        // 🚫 永遠不在沒 E2EE 的房間送加密的區塊（那個區塊的金鑰會公開）。
        let cipher = cipher_for_plaintext_room(args.cipher.as_deref())?;
        if !args.yes && !confirm("send it in plaintext anyway?")? {
            return Err(CoreError::new(CoreErrorKind::Usage, "cancelled"));
        }
        Some(cipher.name().to_string())
    };

    let request = UploadRequest {
        file: file.clone(),
        cipher,
        chunk_size: args.chunk_size,
        name: None,
        mimetype: None,
        sha256: args.sha256,
    };
    let result = core
        .send_file(
            &args.room,
            &request,
            args.caption.as_deref(),
            context.transport,
            &target,
        )
        .await?;
    // ⚠️ manifest 含金鑰：給了路徑就用**私有權限**寫（CLI 規格 §5）。
    if let Some(path) = &args.manifest {
        write_private(path, &result.manifest.to_json())?;
    }
    print_json(&json!({ "event_id": result.event_id, "mxc": result.mxc }))
}

pub async fn watch_command(context: &Context, args: &WatchArgs) -> Result<(), CoreError> {
    context.warn_if_backups_are_off();
    let mode = watch_mode_from_name(&args.mode, args.seconds, args.timeout)?;
    let once = args.mode == "once";
    // ⚠️ `watch` 是串流：訊息從事件來，一則印一行（CLI 規格 §3.4.2 的 JSON Lines）。
    // 所以要先訂閱再開始，🚫 不能等 `watch()` 回來才印。
    let mut events = context.core()?.subscribe();
    let printer = tokio::spawn(async move {
        while let Ok(event) = events.recv().await {
            if let CoreEvent::Message { message, .. } = event {
                print_line(&message);
            }
        }
    });
    let started = std::time::Instant::now();
    let summary = context
        .core()?
        .watch(&args.room, mode, args.since.as_deref(), &context.target())
        .await;
    printer.abort();
    let summary = summary?;
    eprintln!("since {}", summary.since);
    if once && !summary.stopped_by_message {
        return Err(CoreError::new(
            CoreErrorKind::Timeout,
            format!(
                "no event from another sender within {} seconds",
                started.elapsed().as_secs()
            ),
        ));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub async fn read_command(
    context: &Context,
    room: &str,
    limit: u32,
    before: Option<&str>,
    from_cache: bool,
    types: &[String],
    sender: Option<&str>,
) -> Result<(), CoreError> {
    let page = context
        .core()?
        .history(
            &HistoryQuery {
                room: room.to_string(),
                limit,
                before: before.map(str::to_string),
                source: source_of(from_cache),
                types: types.to_vec(),
                sender: sender.map(str::to_string),
            },
            &context.target(),
        )
        .await?;
    print_json(&serde_json::to_value(page).expect("serializes"))
}

pub async fn files_command(
    context: &Context,
    room: &str,
    limit: u32,
    before: Option<&str>,
    from_cache: bool,
    save: Option<&Path>,
) -> Result<(), CoreError> {
    let page = context
        .core()?
        .files(
            room,
            limit,
            before,
            source_of(from_cache),
            save,
            &context.target(),
        )
        .await?;
    print_json(&serde_json::to_value(page).expect("serializes"))
}

fn source_of(from_cache: bool) -> HistorySource {
    match from_cache {
        true => HistorySource::Cache,
        false => HistorySource::Server,
    }
}

/// manifest 含 key：給了路徑就用私有權限寫檔，否則印到 stdout（CLI 規格 §5）。
pub fn emit_manifest(manifest: &wbf_sdk::Manifest, path: Option<&Path>) -> Result<(), CoreError> {
    match path {
        Some(path) => {
            write_private(path, &manifest.to_json())?;
            print_json(&json!({ "manifest": path.display().to_string(), "mxc": manifest.mxc }))
        }
        None => {
            let value: serde_json::Value =
                serde_json::from_slice(&manifest.to_json()).expect("manifest is json");
            print_json(&value)
        }
    }
}

pub fn confirm(question: &str) -> Result<bool, CoreError> {
    eprint!("{question} [y/N] ");
    std::io::stderr()
        .flush()
        .map_err(|error| CoreError::new(CoreErrorKind::Io, format!("{error}")))?;
    let mut answer = String::new();
    std::io::stdin()
        .read_line(&mut answer)
        .map_err(|error| CoreError::new(CoreErrorKind::Io, format!("{error}")))?;
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

pub fn print_json(value: &serde_json::Value) -> Result<(), CoreError> {
    let mut stdout = std::io::stdout().lock();
    serde_json::to_writer(&mut stdout, value)
        .map_err(|error| CoreError::new(CoreErrorKind::Io, format!("{error}")))?;
    stdout
        .write_all(b"\n")
        .map_err(|error| CoreError::new(CoreErrorKind::Io, format!("{error}")))?;
    Ok(())
}

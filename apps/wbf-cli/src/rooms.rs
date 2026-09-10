//! 房間命令（CLI 規格 §3.4）：`rooms`、`send`、`watch`、`read`、`files`。
//! 只碰 `wbf-sdk` 的 `ChatBackend` 與模型型別；matrix-sdk 的東西在 SDK 的 adapter 裡（plan-v1 §7.2）。
//! 從 server 拿到的房間與事件順手寫進 `cache.db`（寫穿，local-cache-db.md §6）；`--from-cache` 不連 server 只讀快取。
//! 寫穿失敗只在 stderr 說一聲，不讓命令失敗：快取不是權威（§1）。

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
/// store 在帳號目錄的 `m/`，金鑰是 vault 的第二把子金鑰（local-cache-db.md §5.3）。
pub(crate) async fn backend(context: &Context) -> Result<MatrixBackend, SdkError> {
    let session = context.session().await?;
    if session.store_dir.is_none() {
        return Err(SdkError::Usage(
            "this session has no matrix store (logged in with an older wbf-cli or --token); run `login` again".into(),
        ));
    }
    let backend = MatrixBackend::restore(
        &session,
        &context.account()?.matrix_store_dir(),
        &context.vault()?.matrix_store_key(),
    )
    .await?;
    backend.sync_once(None, Duration::ZERO).await?;
    Ok(backend)
}

pub async fn rooms_command(context: &Context) -> Result<(), SdkError> {
    let backend = backend(context).await?;
    let conversations = backend.conversations().await?;
    match context.cache().await {
        Ok((mut cache, me)) => {
            write_through(context, cache.upsert_conversations(&me, &conversations))
        }
        Err(error) => context.progress(format!("cache: {error}")),
    }
    print_json(&serde_json::to_value(conversations).expect("serializes"))
}

/// 寫穿快取的錯誤只報不擋（§1：快取壞了的代價是重拉，不是命令失敗）。
fn write_through<T>(context: &Context, result: Result<T, SdkError>) {
    if let Err(error) = result {
        context.progress(format!("cache write failed (ignored): {error}"));
    }
}

/// `--before` 在 `--from-cache` 時是 r_seq 的數字，不是 server 的翻頁 token。
fn parse_before_r_seq(before: Option<&str>) -> Result<Option<i64>, SdkError> {
    before
        .map(|text| {
            text.parse::<i64>().map_err(|_| {
                SdkError::Usage(format!(
                    "--before {text}: with --from-cache it is an r_seq number (from the previous page's next)"
                ))
            })
        })
        .transpose()
}

/// 從快取印一頁：`next` 是這頁最小的 r_seq（下一頁 `--before` 用），沒有 r_seq 的房間翻不了頁（chat-model §4.3 的退化表）。
fn cached_page(messages: Vec<Message>) -> (Vec<Message>, Option<String>) {
    let next = messages
        .iter()
        .filter_map(|message| message.r_seq)
        .min()
        .map(|r_seq| r_seq.to_string());
    (messages, next)
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
    let mut seen: Vec<Message> = Vec::new();
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
        seen.push((**message).clone());
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
    // watch 印過的事件寫穿快取（收在 callback 外，callback 是同步的）。
    if !seen.is_empty() {
        match context.cache().await {
            Ok((mut cache, me)) => write_through(context, cache.upsert_messages(&me, &seen)),
            Err(error) => context.progress(format!("cache: {error}")),
        }
    }
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
    from_cache: bool,
    types: &[String],
    sender: Option<&str>,
) -> Result<(), SdkError> {
    let (events, next) = if from_cache {
        let (cache, me) = context.cache().await?;
        cached_page(cache.history(&me, room, parse_before_r_seq(before)?, limit)?)
    } else {
        let backend = backend(context).await?;
        let page = backend.history(room, before, limit).await?;
        match context.cache().await {
            Ok((mut cache, me)) => write_through(context, cache.upsert_messages(&me, &page.events)),
            Err(error) => context.progress(format!("cache: {error}")),
        }
        (page.events, page.next)
    };
    // 過濾在 client 端（CLI 規格 §3.4.1）；濾完可能是空的但 next 還在，呼叫者照 next 判斷。
    let events: Vec<&Message> = events
        .iter()
        .filter(|message| sender.is_none_or(|sender| message.sender == sender))
        .filter(|message| {
            types.is_empty() || types.iter().any(|wanted| kind_matches(message, wanted))
        })
        .collect();
    print_json(&json!({ "events": events, "next": next }))
}

pub async fn files_command(
    context: &Context,
    room: &str,
    limit: u32,
    before: Option<&str>,
    from_cache: bool,
    save: Option<&Path>,
) -> Result<(), SdkError> {
    let session = context.session().await?;
    let (events, next) = if from_cache {
        let (cache, me) = context.cache().await?;
        cached_page(cache.files(&me, room, parse_before_r_seq(before)?, limit)?)
    } else {
        let backend = backend(context).await?;
        let page = backend.history(room, before, limit).await?;
        match context.cache().await {
            Ok((mut cache, me)) => write_through(context, cache.upsert_messages(&me, &page.events)),
            Err(error) => context.progress(format!("cache: {error}")),
        }
        (page.events, page.next)
    };
    let mut files = Vec::new();
    for message in &events {
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
    print_json(&json!({ "files": files, "next": next }))
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

pub fn confirm(question: &str) -> Result<bool, SdkError> {
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

pub fn print_json(value: &serde_json::Value) -> Result<(), SdkError> {
    let mut stdout = std::io::stdout().lock();
    serde_json::to_writer(&mut stdout, value).map_err(|error| SdkError::Io(error.into()))?;
    stdout.write_all(b"\n")?;
    Ok(())
}

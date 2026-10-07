//! 房間命令（/docs/design/rpc-specs/wbf-cli-spec.md §3.4）：`rooms`、`send`、`watch`、`read`、`files`。
//!
//! ⚠️ 這一層**只做三件事**：把旗標翻成 core 的參數、問使用者（確認）、印出來。
//! 做什麼在 `wbf-core`（/docs/design/overview/architecture-v2.md §6）——包括寫穿快取、過濾、翻頁那些。

use std::io::Write;
use std::path::Path;

use serde_json::json;
use wbf_core::{
    cipher_for_plaintext_room, watch_mode_from_name, CoreError, CoreErrorKind, CoreEvent,
    HistoryQuery, SyncMode, UploadRequest,
};
use wbf_sdk::vault::write_private;
use wbf_sdk::Message;

use crate::commands::Context;
use crate::{SendArgs, WatchArgs};

pub async fn rooms_command(context: &Context) -> Result<(), CoreError> {
    context.warn_if_backups_are_off();
    // CLI 沒有常駐的上游會話可以依賴，所以它一律 `Both`：打上游、寫快取、回本地讀的那份。
    // wbf 帳號的列表只問加入了哪些房，沒拿過的那幾間除了 id 都是 null（/docs/design/rooms/chat-model.md §2.1）。
    let conversations = context
        .core()?
        .list_rooms(SyncMode::Both, &context.target())
        .await?;
    print_value(&conversations)
}

pub async fn send_command(context: &Context, args: &SendArgs) -> Result<(), CoreError> {
    context.warn_if_backups_are_off();
    let core = context.core()?;
    let target = context.target();
    if let Some(text) = &args.text {
        // 送文字只看本地的加密標記、本地不知道就回 Usage（/docs/design/keys/e2ee-rpc.md §7）；CLI 沒有常駐的快取，所以先拿一次房間。
        let conversation = core
            .conversation(&args.room, SyncMode::Both, &target)
            .await?;
        let options = send_options_for(core, &args.room, conversation.encrypted, &target).await?;
        let event_id = match core.send_text(&args.room, text, &options, &target).await {
            // 剛好在 refresh 與送出之間有人進出、換了裝置（1401）：照它帶回來的新狀態與同一個 txn_id 再送一次（只一次）。
            Err(error) if error.kind == CoreErrorKind::RoomDevicesChanged => {
                match options_after_room_devices_changed(&error) {
                    Some(again) => core.send_text(&args.room, text, &again, &target).await?,
                    None => return Err(error),
                }
            }
            sent => sent?,
        };
        return print_json(&json!({ "event_id": event_id }));
    }
    let Some(file) = &args.file else {
        return Err(CoreError::new(
            CoreErrorKind::Usage,
            "send needs --text or --file",
        ));
    };

    // /docs/design/media/wbf-client-convention-for-chunk.md §5.1：沒 E2EE 的房間走明文模式，送之前**警告並要求確認**。
    // 🚫 這個確認是前端的事，core 不問（/docs/design/overview/architecture-v2.md §3）。
    let conversation = core
        .conversation(&args.room, SyncMode::Both, &target)
        .await?;
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
    // 加密房一樣先確認房裡的人與裝置。
    let options = send_options_for(core, &args.room, conversation.encrypted, &target).await?;
    let sent = core
        .send_file(
            &args.room,
            &request,
            args.caption.as_deref(),
            &options,
            context.transport,
            &target,
        )
        .await;
    let (event_id, manifest) = match sent {
        Ok(result) => (result.event_id, result.manifest),
        // 傳完才被 1401 擋：檔案已經在 server 上、manifest 在 `data` 裡。用新狀態與同一個 txn_id 送一次附件（只一次，🚫 重傳檔案）。
        Err(error) if error.kind == CoreErrorKind::RoomDevicesChanged => {
            let manifest = error.data.as_ref().and_then(|data| {
                serde_json::from_value::<wbf_sdk::Manifest>(data.get("manifest")?.clone()).ok()
            });
            let (Some(again), Some(manifest)) =
                (options_after_room_devices_changed(&error), manifest)
            else {
                return Err(error);
            };
            let event_id = core
                .send_attachment(
                    &args.room,
                    &manifest,
                    args.caption.as_deref(),
                    &again,
                    &target,
                )
                .await?;
            (event_id, manifest)
        }
        Err(error) => return Err(error),
    };
    // ⚠️ manifest 含金鑰：給了路徑就用**私有權限**寫（/docs/design/rpc-specs/wbf-cli-spec.md §5）。
    if let Some(path) = &args.manifest {
        write_private(path, &manifest.to_json()?)?;
    }
    print_json(&json!({ "event_id": event_id, "mxc": manifest.mxc }))
}

/// 加密房要帶的房間狀態：CLI 自己就是前端，同一個命令裡先 `refresh_room_devices` 再送（/docs/design/keys/e2ee-rpc.md §2、§3）。
/// 一般 Matrix 帳號不用（matrix-sdk 自己管金鑰），明文房也不用。
///
/// Return:
///     Ok(SendOptions)   加密房的 wbf 帳號帶 `room_devices`；其他是預設（都不帶）
///     Err(...)          refresh 失敗（不在房裡、裝置雜湊對不上…）：🚫 送
async fn send_options_for(
    core: &wbf_core::Core,
    room: &str,
    encrypted: bool,
    target: &wbf_core::Target,
) -> Result<wbf_core::SendOptions, CoreError> {
    if !encrypted || !core.is_wbf_account_for(target)? {
        return Ok(wbf_core::SendOptions::default());
    }
    let refreshed = core.refresh_room_devices(room, None, target).await?;
    Ok(wbf_core::SendOptions {
        room_devices: Some(refreshed),
        txn_id: None,
    })
}

/// 1401 的 `data`：daemon 已經重拿了房間狀態（`{room_version, members, txn_id}`）→ 重送要帶的選項。
///
/// Return:
///     Some(SendOptions)   新的 `room_devices` 與同一個 `txn_id`
///     None                重拿也失敗了（`data` 沒有 `room_version`）、或形狀認不得：照原錯誤回
fn options_after_room_devices_changed(error: &CoreError) -> Option<wbf_core::SendOptions> {
    let data = error.data.as_ref()?;
    let room_devices: wbf_core::RoomDevices = serde_json::from_value(data.clone()).ok()?;
    let txn_id = data.get("txn_id")?.as_str()?.to_string();
    Some(wbf_core::SendOptions {
        room_devices: Some(room_devices),
        txn_id: Some(txn_id),
    })
}

pub async fn watch_command(context: &Context, args: &WatchArgs) -> Result<(), CoreError> {
    context.warn_if_backups_are_off();
    let mode = watch_mode_from_name(&args.mode, args.seconds, args.timeout)?;
    let once = args.mode == "once";
    // ⚠️ `watch` 是串流：訊息從事件來，一則印一行（/docs/design/rpc-specs/wbf-cli-spec.md §3.4.2 的 JSON Lines）。
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
                sync: sync_of(from_cache),
                types: types.to_vec(),
                sender: sender.map(str::to_string),
            },
            &context.target(),
        )
        .await?;
    print_value(&page)
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
            sync_of(from_cache),
            save,
            &context.target(),
        )
        .await?;
    print_value(&page)
}

/// CLI 的 `--from-cache` 對到新的三種 `sync`（/docs/design/daemon/daemon-runtime.md §3.1）。
///
/// Args:
///     from_cache: 有沒有帶 `--from-cache`, example: true
/// Return:
///     SyncMode  true → `Local`；false → **`Both`**
///
/// ⚠️ 沒帶旗標對的是 `Both` 而不是 `Server`：**CLI 本來就是「打上游＋寫穿快取」**，
/// 而 `Server` 是新的「看一眼不寫庫」語意 —— 🚫 不要悄悄改掉 CLI 的行為。
fn sync_of(from_cache: bool) -> SyncMode {
    match from_cache {
        true => SyncMode::Local,
        false => SyncMode::Both,
    }
}

/// manifest 含 key：給了路徑就用私有權限寫檔，否則印到 stdout（/docs/design/rpc-specs/wbf-cli-spec.md §5）。
pub fn emit_manifest(manifest: &wbf_sdk::Manifest, path: Option<&Path>) -> Result<(), CoreError> {
    match path {
        Some(path) => {
            write_private(path, &manifest.to_json()?)?;
            print_json(&json!({ "manifest": path.display().to_string(), "mxc": manifest.mxc }))
        }
        None => print_value(&manifest),
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

/// watch 的 JSON Lines：一事件一行、即時 flush（/docs/design/rpc-specs/wbf-cli-spec.md §3.4.2）。
fn print_line(message: &Message) {
    let mut stdout = std::io::stdout().lock();
    let _ = serde_json::to_writer(&mut stdout, message);
    let _ = stdout.write_all(b"\n");
    let _ = stdout.flush();
}

/// 把一個結果變成 JSON：這些型別都是純欄位，理論上不會失敗——但那不是 panic 的理由（維護者 2026-09-23：CLI 炸了就炸了不是理由）。
///
/// Return:
///     Ok(Value)
///     Err(Io)     序列化不了
pub fn json_value_of<T: serde::Serialize>(value: &T) -> Result<serde_json::Value, CoreError> {
    serde_json::to_value(value).map_err(|error| {
        CoreError::new(
            CoreErrorKind::Io,
            format!("the result could not be serialized: {error}"),
        )
    })
}

/// 序列化再印（大多數命令的最後一行）。
pub fn print_value<T: serde::Serialize>(value: &T) -> Result<(), CoreError> {
    print_json(&json_value_of(value)?)
}

/// 往一個 JSON 物件裡加一個欄位。`Value` 的 `[]=` 在不是物件時會 panic，這裡不會：不是物件就不加——
/// 呼叫端給的都是 struct `to_value` 出來的物件，「不是物件」到不了，所以靜默是設計，不是漏接。
///
/// Args:
///     output: example: json!({ "user": "@a:x" })
///     key: example: "ok"
///     value: example: json!(true)
pub fn set_field(output: &mut serde_json::Value, key: &str, value: serde_json::Value) {
    if let Some(fields) = output.as_object_mut() {
        fields.insert(key.to_string(), value);
    }
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

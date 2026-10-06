//! `room.*` 與 `sync.recent`（/docs/design/rpc-specs/rpc-spec.md §3.3、§3.4）。
//!
//! 🚫 沒有 `room.watch`：常駐之後新訊息走訂閱＋推播（下一支）。
//! 🚫 沒有確認：沒 E2EE 的房間送檔案，CLI 會問「送明文嗎」；daemon 不問，
//! 只守住那條不能破的規矩——**沒 E2EE 的房間永遠不送加密的區塊**（/docs/design/media/wbf-client-convention-for-chunk.md §5.1）。

use std::path::PathBuf;

use serde::Deserialize;
use serde_json::{json, Value};
use wbf_core::{
    cipher_for_plaintext_room, Core, HistoryQuery, RoomDevices, SendOptions, SyncMode,
    UploadRequest,
};
use wbf_sdk::{Manifest, RecentPlan};

use super::{parse_params, to_result, Handle, Outcome, TargetParams, TransportParam};

#[derive(Deserialize)]
struct RoomParams {
    room: String,
    /// 沒帶就是 `local`（/docs/design/rpc-specs/rpc-spec.md §2）。
    #[serde(default)]
    sync: SyncMode,
    #[serde(flatten)]
    target: TargetParams,
}

pub(super) async fn room_list(handle: &Handle, core: &Core, params: Value) -> Outcome {
    #[derive(Deserialize)]
    struct Params {
        #[serde(default)]
        sync: SyncMode,
        #[serde(flatten)]
        target: TargetParams,
    }
    let params: Params = parse_params(params)?;
    to_result(
        core.list_rooms(params.sync, &handle.target(&params.target))
            .await?,
    )
}

pub(super) async fn room_get(handle: &Handle, core: &Core, params: Value) -> Outcome {
    let params: RoomParams = parse_params(params)?;
    to_result(
        core.conversation(&params.room, params.sync, &handle.target(&params.target))
            .await?,
    )
}

/// 加密房要帶 `room_devices`（UI 存的那份，`room.refresh_devices` 回的）；被擋回 1401，`data` 是 daemon 自動重拿的房間狀態（/docs/design/keys/e2ee-rpc.md §3）。
pub(super) async fn room_send_text(handle: &Handle, core: &Core, params: Value) -> Outcome {
    #[derive(Deserialize)]
    struct Params {
        room: String,
        body: String,
        #[serde(default)]
        room_devices: Option<RoomDevices>,
        #[serde(default)]
        txn_id: Option<String>,
        #[serde(flatten)]
        target: TargetParams,
    }
    let params: Params = parse_params(params)?;
    let options = SendOptions {
        room_devices: params.room_devices,
        txn_id: params.txn_id,
    };
    let event_id = core
        .send_text(
            &params.room,
            &params.body,
            &options,
            &handle.target(&params.target),
        )
        .await?;
    Ok(json!({ "event_id": event_id }))
}

/// 資料平面版的送檔最後一步（/docs/design/rpc-specs/data-plane.md §5）：UI 打 HTTP 傳完、拿到 manifest 之後，把那份 manifest 帶回來叫這支發訊息。
/// daemon 🚫 不記傳完的上傳。加密房跟 `room.send_text` 一樣要 `room_devices`、被擋回 1401；重送用同一份 manifest 與 `txn_id`，檔案🚫 不必重傳。
pub(super) async fn room_send_attachment(handle: &Handle, core: &Core, params: Value) -> Outcome {
    #[derive(Deserialize)]
    struct Params {
        room: String,
        manifest: Manifest,
        #[serde(default)]
        caption: Option<String>,
        #[serde(default)]
        room_devices: Option<RoomDevices>,
        #[serde(default)]
        txn_id: Option<String>,
        #[serde(flatten)]
        target: TargetParams,
    }
    let params: Params = parse_params(params)?;
    let options = SendOptions {
        room_devices: params.room_devices,
        txn_id: params.txn_id,
    };
    let event_id = core
        .send_attachment(
            &params.room,
            &params.manifest,
            params.caption.as_deref(),
            &options,
            &handle.target(&params.target),
        )
        .await?;
    Ok(json!({ "event_id": event_id, "mxc": params.manifest.mxc, "attachment_declared": true }))
}

/// 確認這個房現在的人與裝置、把房間金鑰排給還沒有的裝置、交給後台送（UI 點進房、或自己發現版本號變了時叫；🚫 等送到）。
/// 回的 `{room_version, members, shared}` UI 存下來，送出時整份當 `room_devices` 帶回來（/docs/design/keys/e2ee-rpc.md §2）。
pub(super) async fn room_refresh_devices(handle: &Handle, core: &Core, params: Value) -> Outcome {
    #[derive(Deserialize)]
    struct Params {
        room: String,
        #[serde(default)]
        previous: Option<RoomDevices>,
        #[serde(flatten)]
        target: TargetParams,
    }
    let params: Params = parse_params(params)?;
    let refreshed = core
        .refresh_room_devices(
            &params.room,
            params.previous.as_ref(),
            &handle.target(&params.target),
        )
        .await?;
    Ok(json!(refreshed))
}

/// 路徑版（/docs/design/rpc-specs/rpc-spec.md §3.3）：daemon 自己讀檔、上傳、送事件，一則回應。
/// result 多帶 `manifest`（含金鑰）：前端要存就自己存，🚫 daemon 不落地。
pub(super) async fn room_send_file(handle: &Handle, core: &Core, params: Value) -> Outcome {
    #[derive(Deserialize)]
    struct Params {
        room: String,
        path: PathBuf,
        #[serde(default)]
        caption: Option<String>,
        #[serde(default)]
        cipher: Option<String>,
        #[serde(default)]
        chunk_size: Option<u32>,
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        mimetype: Option<String>,
        #[serde(default)]
        sha256: bool,
        #[serde(flatten)]
        transport: TransportParam,
        #[serde(flatten)]
        target: TargetParams,
    }
    let params: Params = parse_params(params)?;
    let transport = handle.transport(&params.transport)?;
    let target = handle.target(&params.target);
    // ⚠️ 送檔前要知道這個房間**現在**加不加密：`Both` 去上游確認過再回答。
    // 🚫 不能用 `Local` —— 快取裡的「沒加密」如果過期了，我們會把金鑰公開送出去。
    let conversation = core
        .conversation(&params.room, SyncMode::Both, &target)
        .await?;
    let cipher = if conversation.encrypted {
        params.cipher
    } else {
        // 沒 E2EE：只准 none；給了別的就拒（區塊的金鑰會公開）。
        Some(
            cipher_for_plaintext_room(params.cipher.as_deref())?
                .name()
                .to_string(),
        )
    };
    let request = UploadRequest {
        file: params.path,
        cipher,
        chunk_size: params.chunk_size,
        name: params.name,
        mimetype: params.mimetype,
        sha256: params.sha256,
    };
    let result = core
        .send_file(
            &params.room,
            &request,
            params.caption.as_deref(),
            transport,
            &target,
        )
        .await?;
    Ok(json!({
        "event_id": result.event_id,
        "mxc": result.mxc,
        "attachment_declared": result.attachment_declared,
        "manifest": serde_json::to_value(&result.manifest)?,
    }))
}

#[derive(Deserialize)]
struct PageParams {
    room: String,
    #[serde(default = "default_limit")]
    limit: u32,
    #[serde(default)]
    before: Option<String>,
    /// 沒帶就是 `local`（/docs/design/rpc-specs/rpc-spec.md §2）。📎 `before` 三種 `sync` 都是 `event_id`（上一頁的 `next`）。
    #[serde(default)]
    sync: SyncMode,
    #[serde(flatten)]
    target: TargetParams,
}

fn default_limit() -> u32 {
    50
}

pub(super) async fn room_history(handle: &Handle, core: &Core, params: Value) -> Outcome {
    #[derive(Deserialize)]
    struct Params {
        #[serde(flatten)]
        page: PageParams,
        #[serde(default)]
        types: Vec<String>,
        #[serde(default)]
        sender: Option<String>,
    }
    let params: Params = parse_params(params)?;
    let query = HistoryQuery {
        room: params.page.room,
        limit: params.page.limit,
        before: params.page.before,
        sync: params.page.sync,
        types: params.types,
        sender: params.sender,
    };
    to_result(
        core.history(&query, &handle.target(&params.page.target))
            .await?,
    )
}

/// CLI 的 `--save` 是前端的事：這裡只回 manifest，不寫檔。
pub(super) async fn room_files(handle: &Handle, core: &Core, params: Value) -> Outcome {
    let params: PageParams = parse_params(params)?;
    to_result(
        core.files(
            &params.room,
            params.limit,
            params.before.as_deref(),
            params.sync,
            None,
            &handle.target(&params.target),
        )
        .await?,
    )
}

pub(super) async fn sync_recent(handle: &Handle, core: &Core, params: Value) -> Outcome {
    #[derive(Deserialize)]
    struct Params {
        /// 這一輪總共要幾則；0 = 拉到追平（/docs/design/rpc-specs/wbf-cli-spec.md §3.5）。
        #[serde(default = "default_max_events")]
        max_events: u64,
        #[serde(default = "default_window")]
        window: u32,
        #[serde(default)]
        batch: Option<u32>,
        /// 從這個 `g_seq` 之後拿（UI 自己記的起點）；沒帶用 daemon 存的上一次水位（/docs/design/rpc-specs/rpc-spec.md §3.5）。
        #[serde(default)]
        since: Option<i64>,
        #[serde(default)]
        from_scratch: bool,
        #[serde(flatten)]
        transport: TransportParam,
        #[serde(flatten)]
        target: TargetParams,
    }
    fn default_max_events() -> u64 {
        10_000
    }
    fn default_window() -> u32 {
        320
    }
    let params: Params = parse_params(params)?;
    let plan = RecentPlan {
        max_events: (params.max_events != 0).then_some(params.max_events),
        window: params.window,
        batch: params.batch,
    };
    let transport = handle.transport(&params.transport)?;
    to_result(
        core.recent(
            plan,
            params.since,
            params.from_scratch,
            transport,
            &handle.target(&params.target),
        )
        .await?,
    )
}

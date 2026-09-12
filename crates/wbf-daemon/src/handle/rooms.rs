//! `room.*` 與 `sync.recent`（rpc-spec §3.3、§3.4）。
//!
//! 🚫 沒有 `room.watch`：常駐之後新訊息走訂閱＋推播（下一支）。
//! 🚫 沒有確認：沒 E2EE 的房間送檔案，CLI 會問「送明文嗎」；daemon 不問，
//! 只守住那條不能破的規矩——**沒 E2EE 的房間永遠不送加密的區塊**（約定 §5.1）。

use std::path::PathBuf;

use serde::Deserialize;
use serde_json::{json, Value};
use wbf_core::{cipher_for_plaintext_room, Core, HistoryQuery, HistorySource, UploadRequest};
use wbf_sdk::RecentPlan;

use super::{
    parse_params, to_result, Handle, Outcome, TargetParams, TransportParam, DAEMON_NAME,
    DAEMON_VERSION,
};

#[derive(Deserialize)]
struct RoomParams {
    room: String,
    #[serde(flatten)]
    target: TargetParams,
}

pub(super) async fn room_list(handle: &Handle, core: &Core, params: Value) -> Outcome {
    let target: TargetParams = parse_params(params)?;
    to_result(core.list_conversations(&handle.target(&target)).await?)
}

pub(super) async fn room_get(handle: &Handle, core: &Core, params: Value) -> Outcome {
    let params: RoomParams = parse_params(params)?;
    to_result(
        core.conversation(&params.room, &handle.target(&params.target))
            .await?,
    )
}

pub(super) async fn room_send_text(handle: &Handle, core: &Core, params: Value) -> Outcome {
    #[derive(Deserialize)]
    struct Params {
        room: String,
        body: String,
        #[serde(flatten)]
        target: TargetParams,
    }
    let params: Params = parse_params(params)?;
    let event_id = core
        .send_text(&params.room, &params.body, &handle.target(&params.target))
        .await?;
    Ok(json!({ "event_id": event_id }))
}

/// 路徑版（rpc-spec §3.3）：daemon 自己讀檔、上傳、送事件，一則回應。
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
    let conversation = core.conversation(&params.room, &target).await?;
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
        "manifest": serde_json::from_slice::<Value>(&result.manifest.to_json())?,
    }))
}

#[derive(Deserialize)]
struct PageParams {
    room: String,
    #[serde(default = "default_limit")]
    limit: u32,
    #[serde(default)]
    before: Option<String>,
    source: HistorySource,
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
        source: params.page.source,
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
            params.source,
            None,
            &handle.target(&params.target),
        )
        .await?,
    )
}

pub(super) async fn sync_recent(handle: &Handle, core: &Core, params: Value) -> Outcome {
    #[derive(Deserialize)]
    struct Params {
        /// 這一輪總共要幾則；0 = 拉到追平（CLI 規格 §3.5）。
        #[serde(default = "default_max_events")]
        max_events: u64,
        #[serde(default = "default_window")]
        window: u32,
        #[serde(default)]
        batch: Option<u32>,
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
            params.from_scratch,
            transport,
            &format!("{DAEMON_NAME} {DAEMON_VERSION}"),
            &handle.target(&params.target),
        )
        .await?,
    )
}

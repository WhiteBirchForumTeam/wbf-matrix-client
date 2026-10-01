//! `upload.*`、`media.*`、`server.ping`（/docs/design/rpc-specs/rpc-spec.md §3.5、§3.6、§3.8）。
//!
//! `media.create` 只建檔、鑄 URL：bytes 走資料平面的 PUT（`data_plane.rs`，/docs/design/rpc-specs/data-plane.md）。
//! 下載是每帳號一條佇列（/docs/design/media/media-download.md）：`media.download` 排、`media.open` 排並鑄讀的 URL（`GET /media`）、
//! `media.queue` 問、`media.cancel` 停；`media.save_to` 也排隊。

use std::path::PathBuf;

use serde::Deserialize;
use serde_json::{json, Value};
use wbf_core::{Core, MediaRef, NewUpload, SyncMode, UploadRequest};
use wbf_sdk::Manifest;

use super::{
    invalid_params, parse_params, to_result, Fail, Handle, Outcome, TargetParams, TransportParam,
    DAEMON_NAME, DAEMON_VERSION,
};
use crate::data_plane::{UploadMeta, MEDIA_PATH, UPLOAD_META_HEADER, UPLOAD_PATH};
use crate::message::code;

/// 資料平面的上傳第一步（/docs/design/rpc-specs/data-plane.md §4.1）：去 server 建檔、回一個 PUT 的 URL。
/// URL 只帶「用途 ‖ mxc」；整個上傳狀態（含檔案金鑰）放進 `Wbf-Upload-Meta` header，兩者都用共享 token 加密，daemon 🚫 不另外記。
/// 發訊息是 UI 在 PUT 回 manifest 之後另外叫的 `room.send_attachment`，🚫 不是這裡的事。
pub(super) async fn media_create(handle: &Handle, core: &Core, params: Value) -> Outcome {
    #[derive(Deserialize)]
    struct Params {
        #[serde(flatten)]
        upload: NewUpload,
        /// 原檔在這台機器的位置，URI（/docs/design/rpc-specs/data-plane.md §8.1）。意義由 UI 定，daemon 🚫 不驗、只原樣帶著。
        #[serde(default)]
        source_uri: Option<String>,
        #[serde(flatten)]
        target: TargetParams,
    }
    let params: Params = parse_params(params)?;
    // 沒有資料平面（單發命令）就不去 server 建檔：建了也沒有地方收 bytes。
    let (Some(data_port), Some(keys)) = (handle.data_port().await, handle.access_keys()) else {
        return Err(Fail::Rpc(
            code::BAD_REQUEST,
            "this daemon has no data plane (it was not started with -s), so nothing could receive the bytes".into(),
        ));
    };
    let upload = core
        .create_upload(&params.upload, &handle.target(&params.target))
        .await?;
    let encrypted = handle.is_encryption_enforced();
    let url_key = keys.to_upload_url_key(&upload.mxc, encrypted)?;
    let meta = keys.to_upload_meta(
        &UploadMeta {
            upload: upload.clone(),
            source_uri: params.source_uri,
        },
        encrypted,
    )?;
    Ok(json!({
        "upload_id": upload.upload_id,
        "mxc": upload.mxc,
        "url": format!("http://127.0.0.1:{data_port}{UPLOAD_PATH}{url_key}"),
        "headers": { UPLOAD_META_HEADER: meta },
    }))
}

pub(super) async fn upload_file(handle: &Handle, core: &Core, params: Value) -> Outcome {
    #[derive(Deserialize)]
    struct Params {
        path: PathBuf,
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
    let request = UploadRequest {
        file: params.path,
        cipher: params.cipher,
        chunk_size: params.chunk_size,
        name: params.name,
        mimetype: params.mimetype,
        sha256: params.sha256,
    };
    let manifest = core
        .upload_file(&request, transport, &handle.target(&params.target))
        .await?;
    Ok(serde_json::to_value(&manifest)?)
}

#[derive(Deserialize)]
struct UploadIdParams {
    upload_id: u64,
    #[serde(default)]
    state_file: Option<PathBuf>,
    #[serde(flatten)]
    transport: TransportParam,
    #[serde(flatten)]
    target: TargetParams,
}

pub(super) async fn upload_status(handle: &Handle, core: &Core, params: Value) -> Outcome {
    let params: UploadIdParams = parse_params(params)?;
    let transport = handle.transport(&params.transport)?;
    to_result(
        core.upload_status(params.upload_id, transport, &handle.target(&params.target))
            .await?,
    )
}

pub(super) async fn upload_abort(handle: &Handle, core: &Core, params: Value) -> Outcome {
    let params: UploadIdParams = parse_params(params)?;
    let transport = handle.transport(&params.transport)?;
    core.abort_upload(
        params.upload_id,
        params.state_file.as_deref(),
        transport,
        &handle.target(&params.target),
    )
    .await?;
    Ok(json!({ "ok": true }))
}

/// manifest 是對方給的：它指的 server 要跟這個帳號的 session 對得上，🚫 不然就是在對錯的地方要東西
/// （CLI 的 `read_manifest` 同一條）。
async fn manifest_for_this_session(
    handle: &Handle,
    core: &Core,
    manifest: Value,
    target: &TargetParams,
) -> Result<Manifest, Fail> {
    let manifest: Manifest = serde_json::from_value(manifest)
        .map_err(|error| invalid_params(format!("manifest: {error}")))?;
    let session_server = core.current_server(&handle.target(target))?;
    if manifest.server.trim_end_matches('/') != session_server.trim_end_matches('/') {
        return Err(invalid_params(format!(
            "manifest is for {}, but the session is on {session_server}",
            manifest.server
        )));
    }
    Ok(manifest)
}

pub(super) async fn media_info(handle: &Handle, core: &Core, params: Value) -> Outcome {
    #[derive(Deserialize)]
    struct Params {
        mxc: String,
        #[serde(default)]
        manifest: Option<Value>,
        /// 沒帶就是 `local`（/docs/design/rpc-specs/rpc-spec.md §2）。⭐ 媒體不可變，本地那份就是同一份事實。
        #[serde(default)]
        sync: SyncMode,
        #[serde(flatten)]
        transport: TransportParam,
        #[serde(flatten)]
        target: TargetParams,
    }
    let params: Params = parse_params(params)?;
    let transport = handle.transport(&params.transport)?;
    let manifest = match params.manifest {
        Some(value) => Some(manifest_for_this_session(handle, core, value, &params.target).await?),
        None => None,
    };
    to_result(
        core.media_info(
            &params.mxc,
            manifest.as_ref(),
            params.sync,
            transport,
            &handle.target(&params.target),
        )
        .await?,
    )
}

/// 要哪個檔：三種說法剛好給一種（/docs/design/media/media-download.md §7.1）。
#[derive(Deserialize)]
struct MediaRefParams {
    #[serde(default)]
    mxc: Option<String>,
    #[serde(default)]
    room: Option<String>,
    #[serde(default)]
    event_id: Option<String>,
    #[serde(default)]
    manifest: Option<Value>,
}

/// Return:
///     Ok(MediaRef)
///     Err(InvalidParams)   一種都沒給、給了不只一種（🚫 不猜哪個優先）、`room` 與 `event_id` 缺一個、manifest 是別台 server 的
async fn media_ref_of(
    handle: &Handle,
    core: &Core,
    params: MediaRefParams,
    target: &TargetParams,
) -> Result<MediaRef, Fail> {
    match (params.mxc, params.room, params.event_id, params.manifest) {
        (Some(mxc), None, None, None) => Ok(MediaRef::Mxc(mxc)),
        (None, Some(room), Some(event_id), None) => Ok(MediaRef::Event { room, event_id }),
        (None, None, None, Some(manifest)) => Ok(MediaRef::Manifest(
            manifest_for_this_session(handle, core, manifest, target).await?,
        )),
        _ => Err(invalid_params(
            "give exactly one of: mxc, room + event_id, manifest",
        )),
    }
}

/// 排一個檔進這個帳號的下載佇列；已經完整或有本機原檔就不排（/docs/design/media/media-download.md §5.3）。
pub(super) async fn media_download(handle: &Handle, core: &Core, params: Value) -> Outcome {
    #[derive(Deserialize)]
    struct Params {
        #[serde(flatten)]
        media: MediaRefParams,
        #[serde(flatten)]
        target: TargetParams,
    }
    let params: Params = parse_params(params)?;
    let media = media_ref_of(handle, core, params.media, &params.target).await?;
    to_result(
        core.media_download(&media, &handle.target(&params.target))
            .await?,
    )
}

/// 讀的 URL（`GET /media/mxc/…`，/docs/design/rpc-specs/data-plane.md §8）：不帶帳號、可以重用；不完整也沒原檔就順便排進佇列。
pub(super) async fn media_open(handle: &Handle, core: &Core, params: Value) -> Outcome {
    #[derive(Deserialize)]
    struct Params {
        #[serde(flatten)]
        media: MediaRefParams,
        #[serde(flatten)]
        target: TargetParams,
    }
    let params: Params = parse_params(params)?;
    let (Some(data_port), Some(keys)) = (handle.data_port().await, handle.access_keys()) else {
        return Err(Fail::Rpc(
            code::BAD_REQUEST,
            "this daemon has no data plane (it was not started with -s), so there is no URL to read from".into(),
        ));
    };
    let media = media_ref_of(handle, core, params.media, &params.target).await?;
    let opened = core
        .media_open(&media, &handle.target(&params.target))
        .await?;
    let url_key = keys.to_media_url_key(&opened.mxc, handle.is_encryption_enforced())?;
    let mut result = serde_json::to_value(&opened)?;
    if let Some(object) = result.as_object_mut() {
        object.insert(
            "url".into(),
            json!(format!("http://127.0.0.1:{data_port}{MEDIA_PATH}{url_key}")),
        );
    }
    Ok(result)
}

pub(super) async fn media_queue(handle: &Handle, core: &Core, params: Value) -> Outcome {
    let target: TargetParams = parse_params(params)?;
    Ok(json!({ "items": core.media_queue(&handle.target(&target)).await? }))
}

pub(super) async fn media_cancel(handle: &Handle, core: &Core, params: Value) -> Outcome {
    #[derive(Deserialize)]
    struct Params {
        mxc: String,
        #[serde(flatten)]
        target: TargetParams,
    }
    let params: Params = parse_params(params)?;
    let cancelled = core
        .media_cancel(&params.mxc, &handle.target(&params.target))
        .await?;
    Ok(json!({ "cancelled": cancelled }))
}

/// 明文落地是**使用者要的**（/docs/design/rpc-specs/local-interface.md §8）。排隊下載、等它完成、再從池（或本機原檔）複製；
/// `no_cache` 也排隊，只是這次下載的不留在池裡（/docs/design/media/media-download.md §5.5）。
pub(super) async fn media_save_to(handle: &Handle, core: &Core, params: Value) -> Outcome {
    #[derive(Deserialize)]
    struct Params {
        #[serde(flatten)]
        media: MediaRefParams,
        out: PathBuf,
        #[serde(default)]
        no_cache: bool,
        #[serde(flatten)]
        target: TargetParams,
    }
    let params: Params = parse_params(params)?;
    let media = media_ref_of(handle, core, params.media, &params.target).await?;
    to_result(
        core.save_media_to(
            &media,
            &params.out,
            params.no_cache,
            &handle.target(&params.target),
        )
        .await?,
    )
}

pub(super) async fn server_ping(handle: &Handle, core: &Core, params: Value) -> Outcome {
    #[derive(Deserialize)]
    struct Params {
        #[serde(flatten)]
        transport: TransportParam,
        #[serde(flatten)]
        target: TargetParams,
    }
    let params: Params = parse_params(params)?;
    let transport = handle.transport(&params.transport)?;
    to_result(
        core.ping(
            transport,
            &format!("{DAEMON_NAME} {DAEMON_VERSION}"),
            &handle.target(&params.target),
        )
        .await?,
    )
}

//! `upload.*`、`media.*`、`server.ping`（/docs/design/rpc-specs/rpc-spec.md §3.5、§3.6、§3.8）。
//!
//! `media.create` 只建檔、鑄 URL：bytes 走資料平面的 PUT（`data_plane.rs`，/docs/design/rpc-specs/data-plane.md）。
//! 下載是每帳號一個下載處理端（/docs/design/media/media-download.md）：`media.download` 排、`media.open` 排並鑄讀的 URL（`GET /media`）、
//! `media.queue` 問、`media.cancel` 停、`media.delete_local` 清；`media.export_to` 沒有就排、等完成才寫到 UI 給的 URI。

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use wbf_core::{
    Core, CoreErrorKind, CreatedUpload, ExportedMedia, MediaRef, NewUpload, SyncMode, UploadRequest,
};
use wbf_sdk::local_source::local_path_of_file_uri;
use wbf_sdk::Manifest;

use super::{
    invalid_params, parse_params, to_result, Fail, Handle, Outcome, TargetParams, TransportParam,
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
    // wbf 帳號建分塊上傳、一般 Matrix 帳號建傳統上傳（/docs/design/rpc-specs/data-plane.md §7.2）：core 照帳號分。
    let upload = core
        .create_media_upload(&params.upload, &handle.target(&params.target))
        .await?;
    let encrypted = handle.is_encryption_enforced();
    let url_key = keys.to_upload_url_key(upload.mxc(), encrypted)?;
    let mxc = upload.mxc().to_string();
    // 傳統上傳沒有上傳 id（server 那邊只有 mxc）。
    let upload_id = match &upload {
        CreatedUpload::Chunked(chunked) => Some(chunked.upload_id),
        CreatedUpload::Matrix(_) => None,
    };
    let meta = keys.to_upload_meta(
        &UploadMeta {
            upload,
            source_uri: params.source_uri,
        },
        encrypted,
    )?;
    let mut reply = json!({
        "mxc": mxc,
        "url": format!("http://127.0.0.1:{data_port}{UPLOAD_PATH}{url_key}"),
        "headers": { UPLOAD_META_HEADER: meta },
    });
    if let (Some(upload_id), Some(fields)) = (upload_id, reply.as_object_mut()) {
        fields.insert("upload_id".into(), json!(upload_id));
    }
    Ok(reply)
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

/// 要哪個檔：三種說法剛好給一種（/docs/design/media/media-download.md §7.1）；從訊息點下載的那種可以多帶 `mxc`（要是那則的附件）。
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
        (mxc, Some(room), Some(event_id), None) => Ok(MediaRef::Event {
            room,
            event_id,
            mxc,
        }),
        (None, None, None, Some(manifest)) => Ok(MediaRef::Manifest(
            manifest_for_this_session(handle, core, manifest, target).await?,
        )),
        _ => Err(invalid_params(
            "give exactly one of: mxc, room + event_id (mxc optional), manifest",
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

/// 清掉一個 mxc 在本地的一切（池檔、半成品、`media` 列），先取消這台 server 上所有正在下載它的（/docs/design/media/media-download.md §7.4）。
pub(super) async fn media_delete_local(handle: &Handle, core: &Core, params: Value) -> Outcome {
    #[derive(Deserialize)]
    struct Params {
        mxc: String,
        #[serde(flatten)]
        target: TargetParams,
    }
    let params: Params = parse_params(params)?;
    to_result(
        core.del_local_media(&params.mxc, &handle.target(&params.target))
            .await?,
    )
}

/// 匯出（/docs/design/media/media-download.md §7.3）：明文落地是**使用者要的**（/docs/design/rpc-specs/local-interface.md §8）。
/// `to` 是 URI，意義跟 `media.create` 的 `source_uri` 同一套（/docs/design/rpc-specs/data-plane.md §8.1）：現在只收 `file://`；
/// `http://`（daemon 用 PUT 丟給 UI，維護者 2026-10-02）之後才做。
pub(super) async fn media_export_to(handle: &Handle, core: &Core, params: Value) -> Outcome {
    #[derive(Deserialize)]
    struct Params {
        #[serde(flatten)]
        media: MediaRefParams,
        to: String,
        #[serde(default)]
        no_cache: bool,
        #[serde(flatten)]
        target: TargetParams,
    }
    #[derive(Serialize)]
    struct Exported<'a> {
        to: &'a str,
        #[serde(flatten)]
        media: ExportedMedia,
    }
    let params: Params = parse_params(params)?;
    let Some(path) = local_path_of_file_uri(&params.to) else {
        return Err(invalid_params(format!(
            "to must be a file:// URI (http:// is not supported yet): {}",
            params.to
        )));
    };
    let media = media_ref_of(handle, core, params.media, &params.target).await?;
    let exported = match core
        .export_media_to(
            &media,
            &path,
            params.no_cache,
            &handle.target(&params.target),
        )
        .await
    {
        Ok(exported) => exported,
        // 匯了、但沒驗過或驗不過（1501）：`data` 要跟成功時的 `result` 一字不差，補上 `to`（/docs/design/rpc-specs/rpc-spec.md §5.3）。
        Err(mut error) if error.kind == CoreErrorKind::Unverified => {
            if let Some(Value::Object(fields)) = error.data.as_mut() {
                fields.insert("to".to_string(), json!(params.to));
            }
            return Err(error.into());
        }
        Err(error) => return Err(error.into()),
    };
    to_result(Exported {
        to: &params.to,
        media: exported,
    })
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
    to_result(core.ping(transport, &handle.target(&params.target)).await?)
}

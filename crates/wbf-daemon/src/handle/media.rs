//! `upload.*`、`media.info`／`save_to`、`server.ping`（rpc-spec §3.5、§3.6、§3.8）。都有 `transport`。
//!
//! 還沒有：`media.open`（要 `PoolReader` 接 HTTP Range）、`media.create`（要 core 把建檔與送事件拆開）。

use std::path::PathBuf;

use serde::Deserialize;
use serde_json::{json, Value};
use wbf_core::{Core, SyncMode, UploadRequest};
use wbf_sdk::Manifest;

use super::{
    invalid_params, parse_params, to_result, Fail, Handle, Outcome, TargetParams, TransportParam,
    DAEMON_NAME, DAEMON_VERSION,
};

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
    Ok(serde_json::from_slice(&manifest.to_json())?)
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
        /// 沒帶就是 `local`（rpc-spec §2）。⭐ 媒體不可變，本地那份就是同一份事實。
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

/// 明文落地是**使用者要的**（architecture-v2 §4.8）。`no_cache` 不進池直接寫。
pub(super) async fn media_save_to(handle: &Handle, core: &Core, params: Value) -> Outcome {
    #[derive(Deserialize)]
    struct Params {
        manifest: Value,
        out: PathBuf,
        #[serde(default)]
        no_cache: bool,
        #[serde(flatten)]
        transport: TransportParam,
        #[serde(flatten)]
        target: TargetParams,
    }
    let params: Params = parse_params(params)?;
    let transport = handle.transport(&params.transport)?;
    let manifest = manifest_for_this_session(handle, core, params.manifest, &params.target).await?;
    let target = handle.target(&params.target);
    if params.no_cache {
        return to_result(
            core.download_direct(&manifest, &params.out, transport, &target)
                .await?,
        );
    }
    to_result(
        core.download_to(&manifest, &params.out, transport, &target)
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

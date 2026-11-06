//! `verify.*`、`devices.list`（/docs/design/keys/cross-signing-and-verification.md §5、§6）。
//! 要不要接受、表情對不對都是 UI 問使用者：這裡只照命令做，每一支回那個流程現在的樣子（跟 `verify.state` 推播同一個形狀）。

use serde::Deserialize;
use serde_json::{json, Value};
use wbf_core::Core;

use super::{parse_params, to_result, Handle, Outcome, TargetParams};

/// 一個流程的哪一步：`user_id` 是對方（自我驗證是自己）、`flow_id` 是 `verify.request` 回的或 `verify.state` 推來的。
#[derive(Deserialize)]
struct FlowParams {
    user_id: String,
    flow_id: String,
    #[serde(flatten)]
    target: TargetParams,
}

pub(super) async fn verify_request(handle: &Handle, core: &Core, params: Value) -> Outcome {
    #[derive(Deserialize)]
    struct Params {
        user_id: String,
        #[serde(default)]
        device_id: Option<String>,
        #[serde(flatten)]
        target: TargetParams,
    }
    let params: Params = parse_params(params)?;
    to_result(
        core.verify_request(
            &params.user_id,
            params.device_id.as_deref(),
            &handle.target(&params.target),
        )
        .await?,
    )
}

pub(super) async fn verify_accept(handle: &Handle, core: &Core, params: Value) -> Outcome {
    let params: FlowParams = parse_params(params)?;
    to_result(
        core.verify_accept(
            &params.user_id,
            &params.flow_id,
            &handle.target(&params.target),
        )
        .await?,
    )
}

pub(super) async fn verify_start_sas(handle: &Handle, core: &Core, params: Value) -> Outcome {
    let params: FlowParams = parse_params(params)?;
    to_result(
        core.verify_start_sas(
            &params.user_id,
            &params.flow_id,
            &handle.target(&params.target),
        )
        .await?,
    )
}

pub(super) async fn verify_confirm(handle: &Handle, core: &Core, params: Value) -> Outcome {
    let params: FlowParams = parse_params(params)?;
    to_result(
        core.verify_confirm(
            &params.user_id,
            &params.flow_id,
            &handle.target(&params.target),
        )
        .await?,
    )
}

pub(super) async fn verify_cancel(handle: &Handle, core: &Core, params: Value) -> Outcome {
    let params: FlowParams = parse_params(params)?;
    to_result(
        core.verify_cancel(
            &params.user_id,
            &params.flow_id,
            &handle.target(&params.target),
        )
        .await?,
    )
}

pub(super) async fn verify_list(handle: &Handle, core: &Core, params: Value) -> Outcome {
    let target: TargetParams = parse_params(params)?;
    let flows = core.verify_list(&handle.target(&target)).await?;
    Ok(json!({ "flows": flows }))
}

pub(super) async fn devices_list(handle: &Handle, core: &Core, params: Value) -> Outcome {
    #[derive(Deserialize)]
    struct Params {
        user_id: String,
        #[serde(flatten)]
        target: TargetParams,
    }
    let params: Params = parse_params(params)?;
    to_result(
        core.list_devices(&params.user_id, &handle.target(&params.target))
            .await?,
    )
}

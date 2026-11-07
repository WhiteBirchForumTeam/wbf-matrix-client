//! `backup.*`（/docs/design/rpc-specs/rpc-spec.md §3.7；room-key-backup）。
//!
//! conf 的兩個開關在這裡生效（/docs/design/rpc-specs/wbf-cli-spec.md §3.6 同一套規則）：
//! `SERVER_BACKUP=off` → `backup.upload` 整個拒絕；`LOCAL_ROOM_KEYS=off` → `backup.save` 拒絕、
//! `backup.upload` 只跳過 save 那一步。**不**靜默跳過：method 是呼叫的人叫的。

use serde::Deserialize;
use serde_json::{json, Value};
use wbf_core::Core;

use super::{on_off, parse_params, refuse_switched_off, to_result, Handle, Outcome, TargetParams};

pub(super) async fn backup_status(handle: &Handle, core: &Core, params: Value) -> Outcome {
    let target: TargetParams = parse_params(params)?;
    let status = core.backup_status(&handle.target(&target)).await?;
    let mut output = serde_json::to_value(status)?;
    // conf 的兩個開關是這台機器的設定，core 不知道有 conf——所以在這裡加。
    crate::push::insert_field(
        &mut output,
        "server_backup_setting",
        json!(on_off(handle.settings().server_backup)),
    );
    crate::push::insert_field(
        &mut output,
        "local_room_keys_setting",
        json!(on_off(handle.settings().local_room_keys)),
    );
    Ok(output)
}

pub(super) async fn backup_upload(handle: &Handle, core: &Core, params: Value) -> Outcome {
    let target: TargetParams = parse_params(params)?;
    if !handle.settings().server_backup {
        return Err(refuse_switched_off("SERVER_BACKUP", "upload to the server"));
    }
    to_result(
        core.upload_room_keys(&handle.target(&target), handle.settings().local_room_keys)
            .await?,
    )
}

pub(super) async fn backup_save(handle: &Handle, core: &Core, params: Value) -> Outcome {
    let target: TargetParams = parse_params(params)?;
    if !handle.settings().local_room_keys {
        return Err(refuse_switched_off(
            "LOCAL_ROOM_KEYS",
            "write a local snapshot",
        ));
    }
    let bytes = core.save_room_key_snapshot(&handle.target(&target)).await?;
    Ok(json!({ "bytes": bytes }))
}

pub(super) async fn backup_import(handle: &Handle, core: &Core, params: Value) -> Outcome {
    let target: TargetParams = parse_params(params)?;
    to_result(
        core.import_room_key_snapshot(&handle.target(&target))
            .await?,
    )
}

/// 沒帶 `recovery_key`、這台也沒保管 → `1020`，前端自己補「先叫 `backup.create_recovery_key`」那句。
/// 帶了就用帶的（換機器時這台沒保管）；打開成功才封進 `r/`（/docs/design/keys/cross-signing-and-verification.md §4.2）。
pub(super) async fn backup_restore(handle: &Handle, core: &Core, params: Value) -> Outcome {
    #[derive(Deserialize)]
    struct Params {
        #[serde(default)]
        recovery_key: Option<String>,
        #[serde(flatten)]
        target: TargetParams,
    }
    let params: Params = parse_params(params)?;
    to_result(
        core.restore_from_recovery_key(
            &handle.target(&params.target),
            params.recovery_key.as_deref(),
        )
        .await?,
    )
}

/// ⚠️ 秘密，只回這一次；同時封進 `<data dir>/r/`。
pub(super) async fn backup_create_recovery_key(
    handle: &Handle,
    core: &Core,
    params: Value,
) -> Outcome {
    let target: TargetParams = parse_params(params)?;
    let key = core.create_recovery_key(&handle.target(&target)).await?;
    Ok(json!({ "recovery_key": key.as_str() }))
}

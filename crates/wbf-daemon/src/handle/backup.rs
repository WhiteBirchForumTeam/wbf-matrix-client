//! `backup.*`（rpc-spec §3.7；local-cache-db §10）。
//!
//! conf 的兩個開關在這裡生效（CLI 規格 §3.6 同一套規則）：
//! `SERVER_BACKUP=off` → `backup.upload` 整個拒絕；`LOCAL_ROOM_KEYS=off` → `backup.save` 拒絕、
//! `backup.upload` 只跳過 save 那一步。🚫 不靜默跳過：method 是他叫的。

use serde_json::{json, Value};
use wbf_core::Core;

use super::{on_off, parse_params, refuse_switched_off, to_result, Handle, Outcome, TargetParams};

pub(super) async fn backup_status(handle: &Handle, core: &Core, params: Value) -> Outcome {
    let target: TargetParams = parse_params(params)?;
    let status = core.backup_status(&handle.target(&target)).await?;
    let mut output = serde_json::to_value(status)?;
    // conf 的兩個開關是這台機器的設定，core 不知道有 conf——所以在這裡加。
    output["server_backup_setting"] = json!(on_off(handle.settings().server_backup));
    output["local_room_keys_setting"] = json!(on_off(handle.settings().local_room_keys));
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

/// 沒保管 recovery key → `1020`，前端自己補「先叫 `backup.create_recovery_key`」那句。
pub(super) async fn backup_restore(handle: &Handle, core: &Core, params: Value) -> Outcome {
    let target: TargetParams = parse_params(params)?;
    to_result(
        core.restore_from_recovery_key(&handle.target(&target))
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

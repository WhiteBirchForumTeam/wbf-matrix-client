//! 不碰網路的 method：`vault.*`、`account.list`／`switch`、`media.stats`／`gc`、`recovery.*`。

use base64::Engine;
use serde::Deserialize;
use serde_json::{json, Value};
use wbf_core::{Core, CoreError, CoreErrorKind};
use zeroize::Zeroizing;

use super::{
    invalid_params, key_mode_json, parse_params, to_result, Fail, Handle, Outcome, TargetParams,
};

fn decode_passphrase(field: Option<String>) -> Result<Option<Zeroizing<Vec<u8>>>, Fail> {
    match field {
        None => Ok(None),
        Some(encoded) => base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map(|bytes| Some(Zeroizing::new(bytes)))
            .map_err(|error| invalid_params(format!("passphrase_base64: {error}"))),
    }
}

/// 建這個資料目錄的 `local.key`（rpc-spec §3.1）。**要 passphrase 模式就在這一步給**。
///
/// ⭐ 這條是 fresh 資料目錄唯一的起手式，🚫 `account.add` 不替前端偷建一把 plain 的 ——
/// 那會逼想要 passphrase 的前端「先落一份 plain 再重包」，中間那段磁碟上就是沒有 passphrase 保護的
/// （PR #31 審查 rumia🔴、salvia🔴）。已經有 `local.key` 就 `1100`，🚫 不覆蓋。
pub(super) fn vault_create(core: &Core, params: Value) -> Outcome {
    #[derive(Deserialize)]
    struct Params {
        #[serde(default)]
        passphrase_base64: Option<String>,
    }
    let params: Params = parse_params(params)?;
    let passphrase = decode_passphrase(params.passphrase_base64)?;
    let mode = core.create_vault(passphrase.as_deref().map(|bytes| bytes.as_slice()))?;
    Ok(json!({ "ok": true, "key_mode": mode }))
}

pub(super) fn vault_unlock(core: &Core, params: Value) -> Outcome {
    #[derive(Deserialize)]
    struct Params {
        #[serde(default)]
        passphrase_base64: Option<String>,
    }
    let params: Params = parse_params(params)?;
    let passphrase = decode_passphrase(params.passphrase_base64)?;
    core.unlock(passphrase.as_deref().map(|bytes| bytes.as_slice()))?;
    Ok(json!({ "ok": true, "key_mode": key_mode_json(core) }))
}

pub(super) fn vault_set_passphrase(core: &Core, params: Value) -> Outcome {
    #[derive(Deserialize)]
    struct Params {
        passphrase_base64: String,
    }
    let params: Params = parse_params(params)?;
    let passphrase = decode_passphrase(Some(params.passphrase_base64))?
        .ok_or_else(|| invalid_params("passphrase_base64 is required"))?;
    let mode = core.set_passphrase(Some(&passphrase))?;
    Ok(json!({ "ok": true, "key_mode": mode }))
}

pub(super) fn vault_remove_passphrase(core: &Core) -> Outcome {
    let mode = core.set_passphrase(None)?;
    Ok(json!({ "ok": true, "key_mode": mode }))
}

pub(super) fn account_list(core: &Core) -> Outcome {
    to_result(core.account_status()?)
}

pub(super) fn account_switch(core: &Core, params: Value) -> Outcome {
    #[derive(Deserialize)]
    struct Params {
        user: String,
        #[serde(default)]
        server: Option<String>,
    }
    let params: Params = parse_params(params)?;
    to_result(core.switch_current(&params.user, params.server.as_deref())?)
}

pub(super) fn media_stats(handle: &Handle, core: &Core, params: Value) -> Outcome {
    let target: TargetParams = parse_params(params)?;
    to_result(core.media_stats(&handle.target(&target))?)
}

pub(super) fn media_gc(handle: &Handle, core: &Core, params: Value) -> Outcome {
    #[derive(Deserialize)]
    struct Params {
        #[serde(default = "default_quota_mib")]
        quota_mib: u64,
        #[serde(default = "default_protect_days")]
        protect_days: u64,
        #[serde(flatten)]
        target: TargetParams,
    }
    fn default_quota_mib() -> u64 {
        2048
    }
    fn default_protect_days() -> u64 {
        7
    }
    let params: Params = parse_params(params)?;
    to_result(core.collect_media_garbage(
        params.quota_mib,
        params.protect_days,
        &handle.target(&params.target),
    )?)
}

pub(super) fn recovery_list(core: &Core) -> Outcome {
    Ok(json!({ "users": core.list_recovery_key_users()? }))
}

pub(super) fn recovery_show(core: &Core, params: Value) -> Outcome {
    #[derive(Deserialize)]
    struct Params {
        user: String,
    }
    let params: Params = parse_params(params)?;
    match core.find_recovery_key(&params.user)? {
        Some(key) => Ok(json!({ "user": params.user, "recovery_key": key.as_str() })),
        None => Err(Fail::Core(CoreError::new(
            CoreErrorKind::NoRecoveryKeyHere,
            format!("no recovery key kept for {}", params.user),
        ))),
    }
}

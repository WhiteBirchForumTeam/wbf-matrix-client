//! `account.add`／`whoami`／`del`／`destroy`（rpc-spec §3.2）。
//!
//! ⚠️ `password` 只活在這一則請求裡：不留、不進 log、不進任何推播。
//! 🚫 沒有「確認」：`account.destroy` 沒有 `--yes`，要問是前端的事（architecture-v2 §3）。

use serde::Deserialize;
use serde_json::Value;
use wbf_core::Core;
use zeroize::Zeroizing;

use super::{parse_params, to_result, Handle, Outcome, TargetParams};

pub(super) async fn account_add(handle: &Handle, core: &Core, params: Value) -> Outcome {
    #[derive(Deserialize)]
    struct Params {
        server: String,
        user: String,
        password: String,
        #[serde(default = "default_device_name")]
        device_name: String,
    }
    fn default_device_name() -> String {
        "wbf-matrix-client".to_string()
    }
    let params: Params = parse_params(params)?;
    let password = Zeroizing::new(params.password);
    // 第一次登入：還沒有 local.key 就建一把 plain 的（rpc-spec §3.2）。要 passphrase 模式的
    // 前端先叫 vault.set_passphrase。建了就是解鎖狀態。
    if core.key_mode()?.is_none() {
        core.create_vault(None)?;
    }
    to_result(
        core.log_in(
            &params.server,
            &params.user,
            &password,
            &params.device_name,
            handle.settings().server_backup,
        )
        .await?,
    )
}

pub(super) async fn account_whoami(handle: &Handle, core: &Core, params: Value) -> Outcome {
    let target: TargetParams = parse_params(params)?;
    to_result(core.whoami(&handle.target(&target)).await?)
}

#[derive(Deserialize)]
struct RemoveParams {
    user: String,
    #[serde(default)]
    server: Option<String>,
    #[serde(default)]
    accept_history_loss: bool,
}

pub(super) async fn account_del(handle: &Handle, core: &Core, params: Value) -> Outcome {
    let params: RemoveParams = parse_params(params)?;
    to_result(
        core.log_out(
            &params.user,
            params.server.as_deref(),
            params.accept_history_loss,
            handle.settings().server_backup,
        )
        .await?,
    )
}

pub(super) async fn account_destroy(handle: &Handle, core: &Core, params: Value) -> Outcome {
    let params: RemoveParams = parse_params(params)?;
    to_result(
        core.destroy_account(
            &params.user,
            params.server.as_deref(),
            params.accept_history_loss,
            handle.settings().server_backup,
        )
        .await?,
    )
}

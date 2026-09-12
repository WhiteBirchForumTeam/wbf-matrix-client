//! RPC 的 handle：一個 `Request` 進、一個 `Response` 出（rpc-spec §3）。
//!
//! 它是 architecture-v2 §4.9 閘門鏈裡「RPC 轉換 ⇔ daemon handle」那一格：把 JSON 的 `params`
//! 反序列化成 core 的型別、叫一個 core 方法、把回傳序列化回去。**命令列的 arg 之後也走這裡**
//! （§0.2：arg → RPC 訊息 → 同一個 `call`），🚫 不留第二套分派。
//!
//! 這一版只接**不碰網路**的 method（rpc-spec §10 標 ✅ 本機的那些）與 `daemon.*`；
//! 房間、上傳、備份那些在下一支 PR。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use base64::Engine;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::{watch, RwLock};
use wbf_core::{Core, CoreError, Target};
use zeroize::Zeroizing;

use crate::connection::{params_or_empty_object, EncryptionPolicy};
use crate::message::{code, Request, Response};
use crate::protocol::SUPPORTED_PROTOCOLS;

pub const DAEMON_NAME: &str = "wbf-matrix-client-daemon";
pub const DAEMON_VERSION: &str = env!("CARGO_PKG_VERSION");

/// 未解鎖時也接受的 method（architecture-v2 §4.5）。其他一律 `1001`。
fn is_allowed_while_locked(method: &str) -> bool {
    method == "hello" || method.starts_with("daemon.") || method.starts_with("vault.")
}

pub struct Handle {
    data_dir: PathBuf,
    /// `vault.lock` 會整個換掉：`Core` 解鎖一次就活著，沒有 lock；換一個新的就是鎖上。
    core: RwLock<Arc<Core>>,
    policy: EncryptionPolicy,
    started_at: Instant,
    shutdown: watch::Sender<bool>,
    ports: RwLock<Option<(u16, u16)>>,
}

impl Handle {
    pub fn new(data_dir: &Path, policy: EncryptionPolicy) -> Arc<Handle> {
        let (shutdown, _) = watch::channel(false);
        Arc::new(Handle {
            data_dir: data_dir.to_path_buf(),
            core: RwLock::new(Arc::new(Core::open(data_dir))),
            policy,
            started_at: Instant::now(),
            shutdown,
            ports: RwLock::new(None),
        })
    }

    /// server 起好 listener 之後回填，`daemon.info` 才報得出來。
    pub async fn set_ports(&self, rpc_port: u16, data_port: u16) {
        *self.ports.write().await = Some((rpc_port, data_port));
    }

    /// `daemon.shutdown` 之後變 `true`。server 用它停止 accept 並送 `SHUTTING_DOWN`。
    pub fn shutdown_signal(&self) -> watch::Receiver<bool> {
        self.shutdown.subscribe()
    }

    pub fn is_shutting_down(&self) -> bool {
        *self.shutdown.borrow()
    }

    pub async fn core(&self) -> Arc<Core> {
        self.core.read().await.clone()
    }

    /// `hello` 的 result（rpc-spec §1.3）。connection 談完版本後由這裡填 daemon 的狀態。
    pub async fn hello_result(&self, protocol: u32) -> Value {
        let core = self.core().await;
        json!({
            "protocol": protocol,
            "daemon": format!("{DAEMON_NAME} {DAEMON_VERSION}"),
            "data_dir": self.data_dir.display().to_string(),
            "unlocked": core.is_unlocked(),
            "key_mode": key_mode_json(&core),
            "encryption_enforced": self.policy.is_enforced(),
        })
    }

    /// 一個請求進、一個回應出。🚫 這裡不印任何東西。
    pub async fn call(&self, request: Request) -> Response {
        let id = request.id;
        if self.is_shutting_down() {
            return Response::error(
                id,
                code::DAEMON_SHUTTING_DOWN,
                "the daemon is shutting down",
            );
        }
        let core = self.core().await;
        if !core.is_unlocked() && !is_allowed_while_locked(&request.method) {
            return Response::from_core_error(id, &CoreError::locked(&self.data_dir));
        }
        let params = params_or_empty_object(&request.params);
        let outcome = match request.method.as_str() {
            "daemon.info" => self.daemon_info(&core).await,
            "daemon.set_encryption" => self.daemon_set_encryption(params),
            "daemon.shutdown" => self.daemon_shutdown(),
            "vault.unlock" => vault_unlock(&core, params),
            "vault.lock" => self.vault_lock().await,
            "vault.set_passphrase" => vault_set_passphrase(&core, params),
            "vault.remove_passphrase" => vault_remove_passphrase(&core),
            "account.list" => account_list(&core),
            "account.switch" => account_switch(&core, params),
            "media.stats" => media_stats(&core, params),
            "media.gc" => media_gc(&core, params),
            "recovery.list" => recovery_list(&core),
            "recovery.show" => recovery_show(&core, params),
            other => Err(Fail::Rpc(
                code::UNKNOWN_METHOD,
                format!("unknown method {other:?}"),
            )),
        };
        match outcome {
            Ok(result) => Response::ok(id, result),
            Err(Fail::Rpc(code, msg)) => Response::error(id, code, msg),
            Err(Fail::Core(error)) => Response::from_core_error(id, &error),
        }
    }

    async fn daemon_info(&self, core: &Core) -> Outcome {
        let ports = *self.ports.read().await;
        Ok(json!({
            "version": format!("{DAEMON_NAME} {DAEMON_VERSION}"),
            "data_dir": self.data_dir.display().to_string(),
            "unlocked": core.is_unlocked(),
            "key_mode": key_mode_json(core),
            "encryption_enforced": self.policy.is_enforced(),
            "protocols": SUPPORTED_PROTOCOLS,
            "rpc_port": ports.map(|(rpc, _)| rpc),
            "data_port": ports.map(|(_, data)| data),
            "uptime_seconds": self.started_at.elapsed().as_secs(),
        }))
    }

    fn daemon_set_encryption(&self, params: Value) -> Outcome {
        #[derive(Deserialize)]
        struct Params {
            enforced: bool,
        }
        let params: Params = parse_params(params)?;
        self.policy.set_enforced(params.enforced);
        Ok(json!({ "encryption_enforced": self.policy.is_enforced() }))
    }

    fn daemon_shutdown(&self) -> Outcome {
        let _ = self.shutdown.send(true);
        Ok(json!({ "ok": true }))
    }

    async fn vault_lock(&self) -> Outcome {
        *self.core.write().await = Arc::new(Core::open(&self.data_dir));
        Ok(json!({ "ok": true }))
    }
}

type Outcome = Result<Value, Fail>;

enum Fail {
    Rpc(u32, String),
    Core(CoreError),
}

impl From<CoreError> for Fail {
    fn from(error: CoreError) -> Fail {
        Fail::Core(error)
    }
}

fn parse_params<T: for<'de> Deserialize<'de>>(params: Value) -> Result<T, Fail> {
    serde_json::from_value(params)
        .map_err(|error| Fail::Rpc(code::INVALID_PARAMS, format!("params: {error}")))
}

fn key_mode_json(core: &Core) -> Value {
    match core.key_mode() {
        Ok(Some(mode)) => serde_json::to_value(mode).unwrap_or(Value::Null),
        _ => Value::Null,
    }
}

/// `user`／`server` 兩個共同欄位（rpc-spec §2）→ core 的 `Target`。
/// ⚠️ `server_backup` 這一版先 `false`：conf 還在 `apps/wbf-cli`，搬進 daemon 是下一支 PR。
#[derive(Deserialize, Default)]
struct TargetParams {
    #[serde(default)]
    user: Option<String>,
    #[serde(default)]
    server: Option<String>,
}

impl TargetParams {
    fn into_target(self) -> Target {
        Target {
            user: self.user,
            server: self.server,
            server_backup: false,
        }
    }
}

fn decode_passphrase(field: Option<String>) -> Result<Option<Zeroizing<Vec<u8>>>, Fail> {
    match field {
        None => Ok(None),
        Some(encoded) => base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map(|bytes| Some(Zeroizing::new(bytes)))
            .map_err(|error| {
                Fail::Rpc(code::INVALID_PARAMS, format!("passphrase_base64: {error}"))
            }),
    }
}

fn vault_unlock(core: &Core, params: Value) -> Outcome {
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

fn vault_set_passphrase(core: &Core, params: Value) -> Outcome {
    #[derive(Deserialize)]
    struct Params {
        passphrase_base64: String,
    }
    let params: Params = parse_params(params)?;
    let passphrase = decode_passphrase(Some(params.passphrase_base64))?.expect("Some in, Some out");
    let mode = core.set_passphrase(Some(&passphrase))?;
    Ok(json!({ "ok": true, "key_mode": mode }))
}

fn vault_remove_passphrase(core: &Core) -> Outcome {
    let mode = core.set_passphrase(None)?;
    Ok(json!({ "ok": true, "key_mode": mode }))
}

fn account_list(core: &Core) -> Outcome {
    Ok(serde_json::to_value(core.account_status()?)?)
}

fn account_switch(core: &Core, params: Value) -> Outcome {
    #[derive(Deserialize)]
    struct Params {
        user: String,
        #[serde(default)]
        server: Option<String>,
    }
    let params: Params = parse_params(params)?;
    Ok(serde_json::to_value(
        core.switch_current(&params.user, params.server.as_deref())?,
    )?)
}

fn media_stats(core: &Core, params: Value) -> Outcome {
    let target: TargetParams = parse_params(params)?;
    Ok(serde_json::to_value(
        core.media_stats(&target.into_target())?,
    )?)
}

fn media_gc(core: &Core, params: Value) -> Outcome {
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
    Ok(serde_json::to_value(core.collect_media_garbage(
        params.quota_mib,
        params.protect_days,
        &params.target.into_target(),
    )?)?)
}

fn recovery_list(core: &Core) -> Outcome {
    Ok(json!({ "users": core.list_recovery_key_users()? }))
}

fn recovery_show(core: &Core, params: Value) -> Outcome {
    #[derive(Deserialize)]
    struct Params {
        user: String,
    }
    let params: Params = parse_params(params)?;
    match core.find_recovery_key(&params.user)? {
        Some(key) => Ok(json!({ "user": params.user, "recovery_key": key.as_str() })),
        None => Err(Fail::Core(CoreError::new(
            wbf_core::CoreErrorKind::NoRecoveryKeyHere,
            format!("no recovery key kept for {}", params.user),
        ))),
    }
}

impl From<serde_json::Error> for Fail {
    fn from(error: serde_json::Error) -> Fail {
        Fail::Rpc(code::BAD_REQUEST, format!("serialise result: {error}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(method: &str, params: Value) -> Request {
        Request {
            method: method.to_string(),
            params,
            id: Some(1),
        }
    }

    #[tokio::test]
    async fn an_empty_data_dir_reports_no_key_file_on_unlock_and_locked_elsewhere() {
        let dir = tempfile::tempdir().unwrap();
        let handle = Handle::new(dir.path(), EncryptionPolicy::enforced());
        let response = handle.call(request("vault.unlock", json!({}))).await;
        assert_eq!(response.code, 1002, "{}", response.msg);
        // 沒解鎖：任何非 daemon.*／vault.* 的 method 都是 1001。
        let response = handle.call(request("account.list", json!({}))).await;
        assert_eq!(response.code, 1001);
        // daemon.info 未解鎖也接受。
        let response = handle.call(request("daemon.info", Value::Null)).await;
        assert_eq!(response.code, 0, "{}", response.msg);
        assert_eq!(response.result["unlocked"], false);
        assert_eq!(response.result["key_mode"], Value::Null);
        assert_eq!(response.result["encryption_enforced"], true);
    }

    #[tokio::test]
    async fn a_plain_vault_unlocks_and_lists_no_accounts() {
        let dir = tempfile::tempdir().unwrap();
        let handle = Handle::new(dir.path(), EncryptionPolicy::enforced());
        handle.core().await.create_vault(None).unwrap();
        let response = handle.call(request("vault.unlock", json!({}))).await;
        assert_eq!(response.code, 0, "{}", response.msg);
        assert_eq!(response.result["key_mode"], "plain");
        let response = handle.call(request("account.list", json!({}))).await;
        assert_eq!(response.code, 0, "{}", response.msg);
        assert_eq!(response.result["accounts"], json!([]));
        // lock 之後又回到 1001。
        let response = handle.call(request("vault.lock", Value::Null)).await;
        assert_eq!(response.code, 0);
        let response = handle.call(request("account.list", json!({}))).await;
        assert_eq!(response.code, 1001);
    }

    #[tokio::test]
    async fn bad_base64_and_unknown_methods_are_rpc_layer_errors() {
        let dir = tempfile::tempdir().unwrap();
        let handle = Handle::new(dir.path(), EncryptionPolicy::enforced());
        let response = handle
            .call(request(
                "vault.unlock",
                json!({ "passphrase_base64": "!!" }),
            ))
            .await;
        assert_eq!(response.code, code::INVALID_PARAMS);
        let response = handle.call(request("daemon.nope", Value::Null)).await;
        assert_eq!(response.code, code::UNKNOWN_METHOD);
        assert_eq!(response.result, Value::Null);
    }

    #[tokio::test]
    async fn set_encryption_flips_the_shared_policy_and_shutdown_refuses_what_comes_after() {
        let dir = tempfile::tempdir().unwrap();
        let policy = EncryptionPolicy::enforced();
        let handle = Handle::new(dir.path(), policy.clone());
        let response = handle
            .call(request(
                "daemon.set_encryption",
                json!({ "enforced": false }),
            ))
            .await;
        assert_eq!(response.result["encryption_enforced"], false);
        assert!(!policy.is_enforced());
        let mut signal = handle.shutdown_signal();
        handle.call(request("daemon.shutdown", Value::Null)).await;
        assert!(signal.changed().await.is_ok() || *signal.borrow());
        let response = handle.call(request("daemon.info", Value::Null)).await;
        assert_eq!(response.code, code::DAEMON_SHUTTING_DOWN);
    }
}

//! RPC 的 handle：一個 `Request` 進、一個 `Response` 出（rpc-spec §3）。
//!
//! 它是 architecture-v2 §4.9 閘門鏈裡「RPC 轉換 ⇔ daemon handle」那一格：把 JSON 的 `params`
//! 反序列化成 core 的型別、叫一個 core 方法、把回傳序列化回去。**命令列的 arg 之後也走這裡**
//! （§0.2：arg → RPC 訊息 → 同一個 `call`），🚫 不留第二套分派。
//!
//! | 子模組 | method |
//! |---|---|
//! | `local` | `daemon.*`、`vault.*`、`account.list`／`switch`、`media.stats`／`gc`、`recovery.*` |
//! | `accounts` | `account.add`／`whoami`／`del`／`destroy` |
//! | `rooms` | `room.*`、`sync.recent` |
//! | `media` | `upload.*`、`media.info`／`save_to`、`server.ping` |
//! | `backup` | `backup.*` |
//!
//! 還沒有：推播、`subscribe`／`cancel`、`media.open`／`create`、`room.send_attachment`（rpc-spec §10）。

mod accounts;
mod backup;
mod local;
mod media;
mod rooms;

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;

use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::{watch, RwLock};
use wbf_core::{Core, CoreError, CoreErrorKind, Target};
use wbf_sdk::Transport;

use crate::connection::{params_or_empty_object, EncryptionPolicy};
use crate::message::{code, Request, Response};
use crate::settings::Settings;

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
    settings: Settings,
    started_at: Instant,
    /// `daemon.shutdown` 只設這個；真正廣播（[`Handle::begin_shutdown_if_requested`]）由 server 在**送完那則回應之後**叫。
    /// 不然 close 通知可能排在 `{ ok: true }` 前面（PR #30 審查 cirno🔴3）。
    shutdown_requested: AtomicBool,
    shutdown: watch::Sender<bool>,
    ports: RwLock<Option<(u16, u16)>>,
    /// 現在活著的 RPC 連線數（`daemon.info` 的 `connections`）。
    connections: AtomicUsize,
}

impl Handle {
    pub fn new(data_dir: &Path, policy: EncryptionPolicy, settings: Settings) -> Arc<Handle> {
        let (shutdown, _) = watch::channel(false);
        Arc::new(Handle {
            data_dir: data_dir.to_path_buf(),
            core: RwLock::new(Arc::new(Core::open(data_dir))),
            policy,
            settings,
            started_at: Instant::now(),
            shutdown_requested: AtomicBool::new(false),
            shutdown,
            ports: RwLock::new(None),
            connections: AtomicUsize::new(0),
        })
    }

    /// 一條連線開了。回來的 guard 丟掉就是關了。
    pub fn connection_opened(self: &Arc<Handle>) -> ConnectionGuard {
        self.connections.fetch_add(1, Ordering::SeqCst);
        ConnectionGuard(self.clone())
    }

    pub fn connection_count(&self) -> usize {
        self.connections.load(Ordering::SeqCst)
    }

    /// `daemon.shutdown` 的回應已經送出去了嗎？是就真的開始關。server 在送完每一則回應後問一次。
    pub fn begin_shutdown_if_requested(&self) {
        if self.shutdown_requested.load(Ordering::SeqCst) {
            let _ = self.shutdown.send(true);
        }
    }

    /// server 起好 listener 之後回填，`daemon.info` 才報得出來。
    pub async fn set_ports(&self, rpc_port: u16, data_port: u16) {
        *self.ports.write().await = Some((rpc_port, data_port));
    }

    /// `daemon.shutdown` 之後變 `true`。server 用它停止 accept 並送 `SHUTTING_DOWN`。
    pub fn shutdown_signal(&self) -> watch::Receiver<bool> {
        self.shutdown.subscribe()
    }

    /// 要不要拒絕新請求：`daemon.shutdown` 一回完就拒，不等廣播。
    pub fn is_shutting_down(&self) -> bool {
        self.shutdown_requested.load(Ordering::SeqCst) || *self.shutdown.borrow()
    }

    pub async fn core(&self) -> Arc<Core> {
        self.core.read().await.clone()
    }

    pub fn settings(&self) -> &Settings {
        &self.settings
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
        if !core.is_unlocked()
            && !self.is_first_login(&core, &request.method)
            && !is_allowed_while_locked(&request.method)
        {
            return Response::from_core_error(id, &CoreError::locked(&self.data_dir));
        }
        let params = params_or_empty_object(&request.params);
        let outcome = self.dispatch(&core, &request.method, params).await;
        match outcome {
            Ok(result) => Response::ok(id, result),
            Err(Fail::Rpc(code, msg)) => Response::error(id, code, msg),
            Err(Fail::Core(error)) => Response::from_core_error(id, &error),
        }
    }

    /// 這個資料目錄還沒有 `local.key`：`account.add` 要能進來建它（rpc-spec §3.2），
    /// 不然第一次登入永遠被 `1001` 擋在外面。**有** `local.key` 但鎖著的時候不算。
    fn is_first_login(&self, core: &Core, method: &str) -> bool {
        method == "account.add" && matches!(core.key_mode(), Ok(None))
    }

    /// ⚠️ 每個分支都 `Box::pin`：matrix-sdk 的 future 很深，整個 match 當一個 future 讓編譯器推
    /// `Send` 會撞 E0275（遞迴上限）。裝箱把推導鏈在這裡切斷；代價是一次堆配置，可忽略。
    async fn dispatch(&self, core: &Core, method: &str, params: Value) -> Outcome {
        let future: Pin<Box<dyn Future<Output = Outcome> + Send + '_>> = match method {
            "daemon.info" => Box::pin(self.daemon_info(core)),
            "daemon.set_encryption" => Box::pin(async { self.daemon_set_encryption(params) }),
            "daemon.shutdown" => Box::pin(async { self.daemon_shutdown() }),
            "vault.unlock" => Box::pin(async { local::vault_unlock(core, params) }),
            "vault.lock" => Box::pin(self.vault_lock()),
            "vault.set_passphrase" => Box::pin(async { local::vault_set_passphrase(core, params) }),
            "vault.remove_passphrase" => Box::pin(async { local::vault_remove_passphrase(core) }),
            "account.list" => Box::pin(async { local::account_list(core) }),
            "account.switch" => Box::pin(async { local::account_switch(core, params) }),
            "media.stats" => Box::pin(async { local::media_stats(self, core, params) }),
            "media.gc" => Box::pin(async { local::media_gc(self, core, params) }),
            "recovery.list" => Box::pin(async { local::recovery_list(core) }),
            "recovery.show" => Box::pin(async { local::recovery_show(core, params) }),
            "account.add" => Box::pin(accounts::account_add(self, core, params)),
            "account.whoami" => Box::pin(accounts::account_whoami(self, core, params)),
            "account.del" => Box::pin(accounts::account_del(self, core, params)),
            "account.destroy" => Box::pin(accounts::account_destroy(self, core, params)),
            "room.list" => Box::pin(rooms::room_list(self, core, params)),
            "room.get" => Box::pin(rooms::room_get(self, core, params)),
            "room.send_text" => Box::pin(rooms::room_send_text(self, core, params)),
            "room.send_file" => Box::pin(rooms::room_send_file(self, core, params)),
            "room.history" => Box::pin(rooms::room_history(self, core, params)),
            "room.files" => Box::pin(rooms::room_files(self, core, params)),
            "sync.recent" => Box::pin(rooms::sync_recent(self, core, params)),
            "upload.file" => Box::pin(media::upload_file(self, core, params)),
            "upload.status" => Box::pin(media::upload_status(self, core, params)),
            "upload.abort" => Box::pin(media::upload_abort(self, core, params)),
            "media.info" => Box::pin(media::media_info(self, core, params)),
            "media.save_to" => Box::pin(media::media_save_to(self, core, params)),
            "server.ping" => Box::pin(media::server_ping(self, core, params)),
            "backup.status" => Box::pin(backup::backup_status(self, core, params)),
            "backup.upload" => Box::pin(backup::backup_upload(self, core, params)),
            "backup.save" => Box::pin(backup::backup_save(self, core, params)),
            "backup.import" => Box::pin(backup::backup_import(self, core, params)),
            "backup.restore" => Box::pin(backup::backup_restore(self, core, params)),
            "backup.create_recovery_key" => {
                Box::pin(backup::backup_create_recovery_key(self, core, params))
            }
            other => {
                return Err(Fail::Rpc(
                    code::UNKNOWN_METHOD,
                    format!("unknown method {other:?}"),
                ))
            }
        };
        future.await
    }

    async fn daemon_info(&self, core: &Core) -> Outcome {
        let ports = *self.ports.read().await;
        Ok(json!({
            "version": format!("{DAEMON_NAME} {DAEMON_VERSION}"),
            "data_dir": self.data_dir.display().to_string(),
            "unlocked": core.is_unlocked(),
            "key_mode": key_mode_json(core),
            "encryption_enforced": self.policy.is_enforced(),
            "protocols": crate::protocol::SUPPORTED_PROTOCOLS,
            "rpc_port": ports.map(|(rpc, _)| rpc),
            "data_port": ports.map(|(_, data)| data),
            "uptime_seconds": self.started_at.elapsed().as_secs(),
            "connections": self.connection_count(),
            "server_backup_setting": on_off(self.settings.server_backup),
            "local_room_keys_setting": on_off(self.settings.local_room_keys),
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

    /// 只記下「要關了」；廣播等回應送出去之後（見 [`Handle::begin_shutdown_if_requested`]）。
    fn daemon_shutdown(&self) -> Outcome {
        self.shutdown_requested.store(true, Ordering::SeqCst);
        Ok(json!({ "ok": true }))
    }

    async fn vault_lock(&self) -> Outcome {
        *self.core.write().await = Arc::new(Core::open(&self.data_dir));
        Ok(json!({ "ok": true }))
    }

    /// `user`／`server` 兩個共同欄位（rpc-spec §2）→ core 的 `Target`，`server_backup` 從 conf 來。
    fn target(&self, params: &TargetParams) -> Target {
        Target {
            user: params.user.clone(),
            server: params.server.clone(),
            server_backup: self.settings.server_backup,
        }
    }

    /// `transport` 欄位（rpc-spec §2）：沒帶用 conf 的；帶了認不得 → `102`。
    fn transport(&self, params: &TransportParam) -> Result<Transport, Fail> {
        match &params.transport {
            None => Ok(self.settings.transport),
            Some(name) => Transport::from_name(name).ok_or_else(|| {
                Fail::Rpc(
                    code::INVALID_PARAMS,
                    format!("transport must be \"ws\" or \"http\", not {name:?}"),
                )
            }),
        }
    }
}

/// 丟掉就把連線數減一。
pub struct ConnectionGuard(Arc<Handle>);

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        self.0.connections.fetch_sub(1, Ordering::SeqCst);
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

impl From<serde_json::Error> for Fail {
    fn from(error: serde_json::Error) -> Fail {
        Fail::Rpc(code::BAD_REQUEST, format!("serialise result: {error}"))
    }
}

fn invalid_params(msg: impl Into<String>) -> Fail {
    Fail::Rpc(code::INVALID_PARAMS, msg.into())
}

fn parse_params<T: for<'de> Deserialize<'de>>(params: Value) -> Result<T, Fail> {
    serde_json::from_value(params).map_err(|error| invalid_params(format!("params: {error}")))
}

/// 序列化 core 的 DTO 當 result。
fn to_result<T: serde::Serialize>(value: T) -> Outcome {
    Ok(serde_json::to_value(value)?)
}

fn key_mode_json(core: &Core) -> Value {
    match core.key_mode() {
        Ok(Some(mode)) => serde_json::to_value(mode).unwrap_or(Value::Null),
        _ => Value::Null,
    }
}

fn on_off(flag: bool) -> &'static str {
    if flag {
        "on"
    } else {
        "off"
    }
}

/// conf 的開關關著、而這個命令**只做**那件事：拒絕，並說是哪個鍵（CLI 規格 §3.6 同一條）。
fn refuse_switched_off(key: &str, what: &str) -> Fail {
    Fail::Core(CoreError::new(
        CoreErrorKind::Usage,
        format!("{key}=off in the config file, so this method will not {what}; set {key}=on (or remove the line) to allow it"),
    ))
}

/// `user`／`server`（rpc-spec §2）。用 `#[serde(flatten)]` 嵌進各 method 的 params。
#[derive(Deserialize, Default)]
struct TargetParams {
    #[serde(default)]
    user: Option<String>,
    #[serde(default)]
    server: Option<String>,
}

/// `transport`（rpc-spec §2）。同上，只有標了「有 transport」的 method 嵌它。
#[derive(Deserialize, Default)]
struct TransportParam {
    #[serde(default)]
    transport: Option<String>,
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

    fn handle(dir: &Path) -> Arc<Handle> {
        Handle::new(dir, EncryptionPolicy::enforced(), Settings::default())
    }

    #[tokio::test]
    async fn an_empty_data_dir_reports_no_key_file_on_unlock_and_locked_elsewhere() {
        let dir = tempfile::tempdir().unwrap();
        let handle = handle(dir.path());
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
        assert_eq!(response.result["server_backup_setting"], "on");
    }

    #[tokio::test]
    async fn the_first_login_gets_past_the_lock_but_a_locked_vault_does_not() {
        let dir = tempfile::tempdir().unwrap();
        let handle = handle(dir.path());
        // 沒 local.key：account.add 進得來（然後因為連不上 server 而失敗，不是 1001）。
        let response = handle
            .call(request(
                "account.add",
                json!({ "server": "http://127.0.0.1:9", "user": "@a:localhost", "password": "x" }),
            ))
            .await;
        assert_ne!(response.code, 1001, "{}", response.msg);
        assert!(response.code >= 1000, "{}", response.msg);
        // 現在 local.key 有了（account.add 建的）；鎖上之後 account.add 就是 1001。
        assert!(handle.core().await.key_mode().unwrap().is_some());
        handle.call(request("vault.lock", Value::Null)).await;
        let response = handle
            .call(request(
                "account.add",
                json!({ "server": "http://127.0.0.1:9", "user": "@a:localhost", "password": "x" }),
            ))
            .await;
        assert_eq!(response.code, 1001);
    }

    #[tokio::test]
    async fn a_plain_vault_unlocks_and_lists_no_accounts() {
        let dir = tempfile::tempdir().unwrap();
        let handle = handle(dir.path());
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
    async fn every_network_method_parses_its_params_and_reaches_core() {
        // 解鎖了但沒有帳號：每個 method 都該走到 core 然後被 core 拒絕（1000+），
        // 🚫 不是 101 unknown、🚫 不是 102 params。這張表是 rpc-spec §3 有 core 對應的全部。
        let dir = tempfile::tempdir().unwrap();
        let handle = handle(dir.path());
        handle.core().await.create_vault(None).unwrap();
        let manifest = json!({ "server": "http://127.0.0.1:9", "mxc": "mxc://x/y", "block": {
            "v": 1, "cipher": "none", "chunk_size": 65536, "size": 1, "name": "a" } });
        let cases = [
            ("account.whoami", json!({})),
            ("account.del", json!({ "user": "@a:localhost" })),
            ("account.destroy", json!({ "user": "@a:localhost" })),
            ("room.list", json!({})),
            ("room.get", json!({ "room": "!r:localhost" })),
            (
                "room.send_text",
                json!({ "room": "!r:localhost", "body": "hi" }),
            ),
            (
                "room.send_file",
                json!({ "room": "!r:localhost", "path": dir.path().join("nope").display().to_string() }),
            ),
            (
                "room.history",
                json!({ "room": "!r:localhost", "limit": 10, "source": "cache" }),
            ),
            (
                "room.files",
                json!({ "room": "!r:localhost", "limit": 10, "source": "cache" }),
            ),
            ("sync.recent", json!({})),
            (
                "upload.file",
                json!({ "path": dir.path().join("nope").display().to_string() }),
            ),
            ("upload.status", json!({ "upload_id": 1 })),
            ("upload.abort", json!({ "upload_id": 1 })),
            ("media.info", json!({ "mxc": "mxc://x/y" })),
            (
                "media.save_to",
                json!({ "manifest": manifest, "out": dir.path().join("o").display().to_string() }),
            ),
            ("server.ping", json!({})),
            ("backup.status", json!({})),
            ("backup.upload", json!({})),
            ("backup.save", json!({})),
            ("backup.import", json!({})),
            ("backup.restore", json!({})),
            ("backup.create_recovery_key", json!({})),
        ];
        for (method, params) in cases {
            let response = handle.call(request(method, params)).await;
            assert!(
                response.code >= 1000,
                "{method}: code {} msg {}",
                response.code,
                response.msg
            );
        }
    }

    #[tokio::test]
    async fn missing_or_wrong_params_are_102_before_core_is_touched() {
        let dir = tempfile::tempdir().unwrap();
        let handle = handle(dir.path());
        handle.core().await.create_vault(None).unwrap();
        let cases = [
            ("room.send_text", json!({ "room": "!r:localhost" })),
            (
                "room.history",
                json!({ "room": "!r:localhost", "limit": 10, "source": "elsewhere" }),
            ),
            ("server.ping", json!({ "transport": "carrier-pigeon" })),
            ("upload.status", json!({ "upload_id": "one" })),
            ("media.save_to", json!({ "manifest": {}, "out": "x" })),
            ("vault.unlock", json!({ "passphrase_base64": "!!" })),
        ];
        for (method, params) in cases {
            let response = handle.call(request(method, params)).await;
            assert_eq!(
                response.code,
                code::INVALID_PARAMS,
                "{method}: {}",
                response.msg
            );
        }
        let response = handle.call(request("daemon.nope", Value::Null)).await;
        assert_eq!(response.code, code::UNKNOWN_METHOD);
        assert_eq!(response.result, Value::Null);
    }

    #[tokio::test]
    async fn switched_off_backup_settings_refuse_the_matching_methods() {
        let dir = tempfile::tempdir().unwrap();
        let settings = Settings {
            server_backup: false,
            local_room_keys: false,
            ..Settings::default()
        };
        let handle = Handle::new(dir.path(), EncryptionPolicy::enforced(), settings);
        handle.core().await.create_vault(None).unwrap();
        let response = handle.call(request("backup.upload", json!({}))).await;
        assert_eq!(response.code, 1100, "{}", response.msg);
        assert!(response.msg.contains("SERVER_BACKUP=off"));
        let response = handle.call(request("backup.save", json!({}))).await;
        assert_eq!(response.code, 1100, "{}", response.msg);
        assert!(response.msg.contains("LOCAL_ROOM_KEYS=off"));
        let response = handle.call(request("daemon.info", Value::Null)).await;
        assert_eq!(response.result["server_backup_setting"], "off");
    }

    #[tokio::test]
    async fn set_encryption_flips_the_shared_policy_and_shutdown_refuses_what_comes_after() {
        let dir = tempfile::tempdir().unwrap();
        let policy = EncryptionPolicy::enforced();
        let handle = Handle::new(dir.path(), policy.clone(), Settings::default());
        let response = handle
            .call(request(
                "daemon.set_encryption",
                json!({ "enforced": false }),
            ))
            .await;
        assert_eq!(response.result["encryption_enforced"], false);
        assert!(!policy.is_enforced());
        let signal = handle.shutdown_signal();
        let response = handle.call(request("daemon.shutdown", Value::Null)).await;
        assert_eq!(response.code, 0);
        // 回完之前不廣播：server 送完回應才叫 begin_shutdown_if_requested。
        assert!(!*signal.borrow());
        assert!(handle.is_shutting_down());
        let response = handle.call(request("daemon.info", Value::Null)).await;
        assert_eq!(response.code, code::DAEMON_SHUTTING_DOWN);
        handle.begin_shutdown_if_requested();
        assert!(*signal.borrow());
    }

    #[tokio::test]
    async fn connections_are_counted_by_guards() {
        let dir = tempfile::tempdir().unwrap();
        let handle = handle(dir.path());
        let first = handle.connection_opened();
        let second = handle.connection_opened();
        assert_eq!(handle.connection_count(), 2);
        drop(first);
        let response = handle.call(request("daemon.info", Value::Null)).await;
        assert_eq!(response.result["connections"], 1);
        drop(second);
        assert_eq!(handle.connection_count(), 0);
    }
}

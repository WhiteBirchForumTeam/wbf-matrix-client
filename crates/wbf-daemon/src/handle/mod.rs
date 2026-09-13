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
use crate::lock::WriteAccess;
use crate::message::{code, Request, Response};
use crate::settings::Settings;

pub const DAEMON_NAME: &str = "wbf-matrix-client-daemon";

pub const DAEMON_VERSION: &str = env!("CARGO_PKG_VERSION");

/// 未解鎖時也接受的 method（architecture-v2 §4.5）。其他一律 `1001`。
/// 這個 method **保證不碰資料目錄**嗎？
///
/// Args:
///     method: example: "daemon.info"
/// Return:
///     bool  true 只給「正面認得、確定只在記憶體裡動」的那幾個；其餘一律當成會寫
///
/// 🚨 **判準是反過來寫的**：不是「列出會寫的」，而是「列出確定不寫的」，其餘落到**要寫權**那一邊
/// （A5 的 fail closed）。⭐ 這樣新加一個 method 而忘了想它的人，得到的是「被要求拿鎖」，
/// 🚫 不是「靜靜地寫進別人的資料庫」。
///
/// 📎 名單很短是刻意的：`vault.*`／`account.*`／`room.*` 這些**看起來像唯讀的也會寫** ——
/// `account.list` 要開 vault 解目錄名、`room.list` 會讓 matrix-sdk 的 store 寫東西。
/// 真正的唯讀命令（單發的 `--version` 那一類）根本不會走到這裡。
fn is_read_only(method: &str) -> bool {
    matches!(
        method,
        "hello" | "daemon.info" | "daemon.set_encryption" | "daemon.shutdown"
    )
}

fn is_allowed_while_locked(method: &str) -> bool {
    method == "hello" || method.starts_with("daemon.") || method.starts_with("vault.")
}

/// 鑄一個這次啟動的身分：UUID v4（`uuid` crate，隨機來自 OS）。
///
/// Return:
///     String  example: "3f2b1c4a-5d6e-4f80-9a1b-2c3d4e5f6071"
///
/// 📎 用現成的：`uuid` **本來就在依賴樹裡**（matrix-sdk → ruma），所以這裡沒有多一個相依，
/// 而版本／variant 位元與文字格式那些細節也不該由我們自己維護（維護者 2026-09-13）。
fn new_instance_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

pub struct Handle {
    data_dir: PathBuf,
    /// 解鎖一次就活著（`Core` 沒有 lock，daemon 也沒有 `vault.lock`——rpc-spec §3.1）。
    /// 還是 `RwLock`：`vault.create`／`unlock` 會換掉裡面的狀態，而讀的人是每一個請求。
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
    /// 「我有沒有寫這個資料目錄的能力」（維護者 2026-09-13）。🚨 預設**沒有**。
    write_access: WriteAccess,
    /// 這個 daemon 實例的身分（維護者 2026-09-13）：起來時鑄一次，活著的期間**不變**。
    /// 生產環境一個程序就是一個 `Handle`（`main.rs` 只建一個），所以它也就是那個程序的身分。
    instance: String,
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
            write_access: WriteAccess::none(),
            instance: new_instance_id(),
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

    /// 拿下這個資料目錄的寫權。**`-s` 起來的第一件事**（architecture-v2 §0.2）：拿不到就不要啟動。
    ///
    /// Return:
    ///     Ok(())      有寫的能力了
    ///     Err(...)    別人在用這個目錄；呼叫端該印出來然後結束
    pub fn grant_write_access(&self) -> Result<(), crate::lock::LockError> {
        self.write_access.grant(&self.data_dir)
    }

    pub fn can_write(&self) -> bool {
        self.write_access.is_granted()
    }

    /// 這個實例的 UUID。⭐ 前端用它回答「我現在講話的還是剛才那一個嗎」——
    /// port 會重複使用、pid 會被回收，**這個不會**。
    pub fn instance(&self) -> &str {
        &self.instance
    }

    pub fn uptime_seconds(&self) -> u64 {
        self.started_at.elapsed().as_secs()
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
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
            "instance": self.instance,
            "pid": std::process::id(),
            "uptime_seconds": self.uptime_seconds(),
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
        // 🚨 **要寫就要先有寫的能力**（architecture-v2 §0.2）。這是全 daemon 唯一檢查它的地方。
        // `-s` 在啟動時就拿到了，所以這裡是一個 atomic 讀；單發命令則是**第一個要寫的命令**
        // 觸發去拿鎖。拿不到就回「我沒有寫的權限」，🚫 不重試、🚫 不降級成唯讀跑一半。
        if !is_read_only(&request.method) {
            if let Err(error) = self.grant_write_access() {
                return Response::error(id, code::NO_WRITE_ACCESS, error.to_string());
            }
        }
        let core = self.core().await;
        if !core.is_unlocked() && !is_allowed_while_locked(&request.method) {
            return Response::from_core_error(id, &self.why_it_is_shut(&core));
        }
        let params = params_or_empty_object(&request.params);
        let outcome = self.dispatch(&core, &request.method, params).await;
        match outcome {
            Ok(result) => Response::ok(id, result),
            Err(Fail::Rpc(code, msg)) => Response::error(id, code, msg),
            Err(Fail::Core(error)) => Response::from_core_error(id, &error),
        }
    }

    /// 擋下來的時候是「還沒有 vault」還是「有但鎖著」？兩者的下一步完全不同，所以🚫 不共用一個
    /// `1001`：沒有 `local.key` 回 `1002` 並指向 `vault.create`（fresh 資料目錄的起手式，
    /// rpc-spec §3.1），有但鎖著才是 `1001`（去 `vault.unlock`）。
    /// 📎 讀不到 `local.key` 的狀態（IO 壞了）也當成鎖著：不確定就拒絕。
    fn why_it_is_shut(&self, core: &Core) -> CoreError {
        match core.key_mode() {
            Ok(None) => CoreError::new(
                CoreErrorKind::NoKeyFile,
                format!(
                    "{} has no key file yet; call vault.create first (with passphrase_base64 if you want passphrase mode)",
                    self.data_dir.display()
                ),
            ),
            _ => CoreError::locked(&self.data_dir),
        }
    }

    /// ⚠️ 每個分支都 `Box::pin`：matrix-sdk 的 future 很深，整個 match 當一個 future 讓編譯器推
    /// `Send` 會撞 E0275（遞迴上限）。裝箱把推導鏈在這裡切斷；代價是一次堆配置，可忽略。
    async fn dispatch(&self, core: &Core, method: &str, params: Value) -> Outcome {
        let future: Pin<Box<dyn Future<Output = Outcome> + Send + '_>> = match method {
            "daemon.info" => Box::pin(self.daemon_info(core)),
            "daemon.set_encryption" => Box::pin(async { self.daemon_set_encryption(params) }),
            "daemon.shutdown" => Box::pin(async { self.daemon_shutdown() }),
            "vault.create" => Box::pin(async { local::vault_create(core, params) }),
            "vault.unlock" => Box::pin(async { local::vault_unlock(core, params) }),
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
            "instance": self.instance,
            "pid": std::process::id(),
            "data_dir": self.data_dir.display().to_string(),
            "unlocked": core.is_unlocked(),
            "key_mode": key_mode_json(core),
            "encryption_enforced": self.policy.is_enforced(),
            "protocols": crate::protocol::SUPPORTED_PROTOCOLS,
            "rpc_port": ports.map(|(rpc, _)| rpc),
            "data_port": ports.map(|(_, data)| data),
            "uptime_seconds": self.uptime_seconds(),
            "connections": self.connection_count(),
            // 寫入者還有幾件在排隊（daemon-runtime §2.2）。⚠️ 一直漲＝寫得比收得慢。
            "cache_queue": core.cache_queue_len(),
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
        Fail::Rpc(code::INTERNAL, format!("serialise result: {error}"))
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
    async fn an_empty_data_dir_reports_no_key_file_everywhere_and_points_at_vault_create() {
        let dir = tempfile::tempdir().unwrap();
        let handle = handle(dir.path());
        let response = handle.call(request("vault.unlock", json!({}))).await;
        assert_eq!(response.code, 1002, "{}", response.msg);
        // 沒有 local.key：擋下來的理由是 1002（不是「鎖著」——還沒有東西可以鎖），
        // 而且訊息要指向下一步。
        let response = handle.call(request("account.list", json!({}))).await;
        assert_eq!(response.code, 1002, "{}", response.msg);
        assert!(response.msg.contains("vault.create"), "{}", response.msg);
        // daemon.info 未解鎖也接受。
        let response = handle.call(request("daemon.info", Value::Null)).await;
        assert_eq!(response.code, 0, "{}", response.msg);
        assert_eq!(response.result["unlocked"], false);
        assert_eq!(response.result["key_mode"], Value::Null);
        assert_eq!(response.result["encryption_enforced"], true);
        assert_eq!(response.result["server_backup_setting"], "on");
    }

    #[tokio::test]
    async fn a_fresh_data_dir_needs_vault_create_first_and_it_can_be_passphrase_mode() {
        let dir = tempfile::tempdir().unwrap();
        let handle = handle(dir.path());
        // 🚫 account.add 不替前端建 vault：fresh 資料目錄先 1002，訊息指向 vault.create。
        let add = request(
            "account.add",
            json!({ "server": "http://127.0.0.1:9", "user": "@a:localhost", "password": "x" }),
        );
        let response = handle.call(add).await;
        assert_eq!(response.code, 1002, "{}", response.msg);
        assert!(response.msg.contains("vault.create"), "{}", response.msg);

        // ⭐ 想要 passphrase 模式的前端**一步**就建得成，🚫 不必先落一份 plain 再重包。
        let response = handle
            .call(request(
                "vault.create",
                json!({ "passphrase_base64": "aHVudGVyMg==" }),
            ))
            .await;
        assert_eq!(response.code, 0, "{}", response.msg);
        assert_eq!(response.result["key_mode"], "passphrase");
        // 建完就是解鎖狀態：account.add 進得去（然後因為連不上 server 而失敗，不是 1001／1002）。
        let response = handle
            .call(request(
                "account.add",
                json!({ "server": "http://127.0.0.1:9", "user": "@a:localhost", "password": "x" }),
            ))
            .await;
        assert!(response.code >= 1300, "{}", response.msg);

        // 🚫 不覆蓋既有的 local.key（覆蓋＝把所有帳號鎖在門外）。
        let response = handle.call(request("vault.create", json!({}))).await;
        assert_eq!(response.code, 1100, "{}", response.msg);

        // daemon 重開 ＝ 回到未解鎖：這時 account.add 是 1001，🚫 不是「再建一把」。
        // ⚠️ **先把第一個丟掉**：重開的意思是舊的沒了。不丟的話新的拿不到寫權（109）——
        // 那是對的行為，但測的就不是「重開」了。
        drop(handle);
        let restarted = Handle::new(
            dir.path(),
            EncryptionPolicy::enforced(),
            Settings::default(),
        );
        let response = restarted
            .call(request(
                "account.add",
                json!({ "server": "http://127.0.0.1:9", "user": "@a:localhost", "password": "x" }),
            ))
            .await;
        assert_eq!(response.code, 1001, "{}", response.msg);
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
        // 🚫 沒有 vault.lock（維護者 2026-09-13：daemon 不需要這個 feature，rpc-spec §3.1）。
        // 真正的「鎖上」是 daemon.shutdown 再重開 —— 換一個 Handle 就是那條路。
        let response = handle.call(request("vault.lock", Value::Null)).await;
        assert_eq!(response.code, code::UNKNOWN_METHOD, "{}", response.msg);
        drop(handle);
        let restarted = Handle::new(
            dir.path(),
            EncryptionPolicy::enforced(),
            Settings::default(),
        );
        let response = restarted.call(request("account.list", json!({}))).await;
        assert_eq!(response.code, 1001);
    }

    /// 別人握著這個資料目錄的時候，**會寫的 method 一律 109**，而純 daemon 層的照常。
    #[tokio::test]
    async fn without_write_access_the_writing_methods_are_refused_but_daemon_info_still_answers() {
        let dir = tempfile::tempdir().unwrap();
        let squatter = crate::lock::lock_for_writing(dir.path()).unwrap();
        let handle = handle(dir.path());
        assert!(!handle.can_write(), "🚨 起手沒有寫的能力");

        for method in ["vault.create", "account.list", "room.list", "sync.recent"] {
            let response = handle.call(request(method, json!({}))).await;
            assert_eq!(
                response.code,
                code::NO_WRITE_ACCESS,
                "{method}: {}",
                response.msg
            );
        }
        // 🚫 失敗不會讓它以為自己有能力。
        assert!(!handle.can_write());

        // 不碰資料目錄的照常回答。
        let response = handle.call(request("daemon.info", Value::Null)).await;
        assert_eq!(response.code, 0, "{}", response.msg);

        // 對方放手之後，下一個要寫的請求自己就拿到了（單發命令就是這樣運作的）。
        drop(squatter);
        let response = handle.call(request("vault.create", json!({}))).await;
        assert_eq!(response.code, 0, "{}", response.msg);
        assert!(handle.can_write());
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

//! 資料平面（/docs/design/rpc-specs/data-plane.md）：`http://127.0.0.1:<data port>`，媒體的 bytes 只走這裡。
//!
//! | 這裡 | 別處 |
//! |---|---|
//! | capability 表（[`Capabilities`]）：token → 這一個上傳、有效期、現在有沒有 PUT 在收 | `media.create` 鑄 token、`room.send_attachment` 查上傳（`handle`） |
//! | HTTP listener（[`DataServer`]）：路徑、狀態碼、把 body 交給 core | 怎麼切塊、加密、上傳（`Core::receive_upload`） |
//!
//! 🚫 沒有全域 token：URL 本身就是 capability（/docs/design/rpc-specs/data-plane.md §2）。🚫 這裡不印東西。

use std::collections::HashMap;
use std::convert::Infallible;
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use bytes::Bytes;
use futures_util::TryStreamExt;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{header, Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use tokio::net::{TcpListener, TcpStream};
use tokio_util::io::StreamReader;
use wbf_core::{Core, CoreError, CoreErrorKind, Target};
use wbf_sdk::{Manifest, UploadState};

use crate::handle::Handle;
use crate::message::code;

/// 一張 token 活多久（/docs/design/rpc-specs/data-plane.md §2）。進行中的 PUT 過期也不打斷，過期只擋新的請求。
pub const CAPABILITY_TTL: Duration = Duration::from_secs(3600);

/// token 的隨機 bytes 數（URL 裡是它的 hex）。
const TOKEN_BYTES: usize = 32;

/// 所有有效的 capability。現在只有上傳；`media.open` 的讀取之後加在這裡。
#[derive(Default)]
pub struct Capabilities {
    uploads: Mutex<HashMap<String, UploadCapability>>,
}

struct UploadCapability {
    /// ⚠️ 含檔案金鑰：只在記憶體裡，🚫 不落地、不給前端。
    upload: UploadState,
    expires_at: Instant,
    phase: UploadPhase,
}

#[derive(Clone)]
enum UploadPhase {
    /// 沒有 PUT 在收（新的、或上一次斷了的固定大小上傳）。
    Idle,
    Receiving,
    /// 傳完了：再 PUT 直接回這份，`room.send_attachment` 也用它的區塊（多了 `sha256`，串流的多了 `file_size`）。
    Sealed(Manifest),
    /// 串流上傳斷了：server 不能續傳串流，這張 token 只能作廢。
    Broken,
}

/// `room.send_attachment` 照 mxc 找不到傳完的上傳時，是哪一種（[`Capabilities::find_finished_upload`]）。
#[derive(Debug, PartialEq, Eq)]
pub enum MissingUpload {
    /// 有這個上傳，但 PUT 還沒回 200
    NotFinished,
    /// 沒有、過期了、或是別的帳號的
    Unknown,
}

/// 一個 PUT 進來時，這張 token 的狀況。
enum PutStart {
    NotFound,
    Busy,
    Broken,
    Sealed(Manifest),
    Receive(UploadState),
}

impl Capabilities {
    fn uploads(&self) -> MutexGuard<'_, HashMap<String, UploadCapability>> {
        self.uploads
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// 替一個剛建好的上傳鑄一張 token。
    ///
    /// Args:
    ///     upload: `Core::create_upload` 回的
    /// Return:
    ///     Ok(String)       64 個 hex 字元，放進 `/upload/<token>`
    ///     Err(CoreError)   OS 給不出亂數（`Io`）
    pub fn issue_upload(&self, upload: UploadState) -> Result<String, CoreError> {
        let token = new_token()?;
        let now = Instant::now();
        let mut uploads = self.uploads();
        uploads.retain(|_, capability| {
            capability.expires_at > now || matches!(capability.phase, UploadPhase::Receiving)
        });
        uploads.insert(
            token.clone(),
            UploadCapability {
                upload,
                expires_at: now + CAPABILITY_TTL,
                phase: UploadPhase::Idle,
            },
        );
        Ok(token)
    }

    /// 這個帳號傳完的那個 mxc（`room.send_attachment`，/docs/design/rpc-specs/data-plane.md §5）。一定帶帳號一起找：
    /// 兩台 server 的 `server_name` 一樣時 mxc 也會撞，而上傳 id 本來就是各台自己發的。
    ///
    /// Args:
    ///     mxc: PUT 回的 manifest 裡那個, example: "mxc://localhost/000000000000004d"
    ///     server: 帳號的 homeserver, example: "http://127.0.0.1:6167"
    ///     user_id: example: "@alice:localhost"
    /// Return:
    ///     Ok((UploadState, Manifest))       傳完了：建檔那份（核對帳號用）與 manifest（事件用它的區塊）
    ///     Err(MissingUpload::NotFinished)   有這個上傳，但 PUT 還沒回 200
    ///     Err(MissingUpload::Unknown)       沒有、過期了、或是別的帳號的
    pub fn find_finished_upload(
        &self,
        mxc: &str,
        server: &str,
        user_id: &str,
    ) -> Result<(UploadState, Manifest), MissingUpload> {
        let now = Instant::now();
        let uploads = self.uploads();
        let found = uploads.values().find(|capability| {
            capability.expires_at > now
                && capability.upload.mxc == mxc
                && capability.upload.is_for(server, user_id)
        });
        match found {
            None => Err(MissingUpload::Unknown),
            Some(capability) => match &capability.phase {
                UploadPhase::Sealed(manifest) => Ok((capability.upload.clone(), manifest.clone())),
                _ => Err(MissingUpload::NotFinished),
            },
        }
    }

    /// 撤銷一個帳號的全部 token（`account.del`／`destroy` 之後，/docs/design/rpc-specs/data-plane.md §2）。
    /// 進行中的 PUT 不打斷（core 那邊沒有 session 就會失敗），之後的請求一律 404。
    pub fn revoke_account(&self, server: &str, user_id: &str) {
        self.uploads()
            .retain(|_, capability| !capability.upload.is_for(server, user_id));
    }

    fn begin_put(&self, token: &str) -> PutStart {
        let now = Instant::now();
        let mut uploads = self.uploads();
        let Some(capability) = uploads.get_mut(token) else {
            return PutStart::NotFound;
        };
        match &capability.phase {
            UploadPhase::Receiving => PutStart::Busy,
            // 過期跟不存在一樣 404：🚫 不分辨兩者（那會變成探測工具）。
            _ if capability.expires_at <= now => PutStart::NotFound,
            UploadPhase::Broken => PutStart::Broken,
            UploadPhase::Sealed(manifest) => PutStart::Sealed(manifest.clone()),
            UploadPhase::Idle => {
                capability.phase = UploadPhase::Receiving;
                PutStart::Receive(capability.upload.clone())
            }
        }
    }

    fn end_put(&self, token: &str, sealed: Option<&Manifest>) {
        let mut uploads = self.uploads();
        let Some(capability) = uploads.get_mut(token) else {
            return;
        };
        capability.phase = match sealed {
            Some(manifest) => UploadPhase::Sealed(manifest.clone()),
            None if capability.upload.block.file_size.is_some() => UploadPhase::Idle,
            None => UploadPhase::Broken,
        };
    }
}

/// 一個進行中的 PUT。🚨 丟掉（連線斷了、future 被 hyper 丟掉）就把狀態放回去——不然那張 token 永遠回 409。
struct PutInProgress {
    handle: Arc<Handle>,
    token: String,
    ended: bool,
}

impl PutInProgress {
    fn end(mut self, sealed: Option<&Manifest>) {
        self.handle.capabilities().end_put(&self.token, sealed);
        self.ended = true;
    }
}

impl Drop for PutInProgress {
    fn drop(&mut self) {
        if !self.ended {
            self.handle.capabilities().end_put(&self.token, None);
        }
    }
}

/// Return:
///     Ok(String)   `TOKEN_BYTES` 個 OS 亂數的 hex
///     Err(Io)      OS 給不出亂數
fn new_token() -> Result<String, CoreError> {
    let mut bytes = [0u8; TOKEN_BYTES];
    getrandom::getrandom(&mut bytes).map_err(|error| {
        CoreError::new(
            CoreErrorKind::Io,
            format!("no randomness for a data plane token: {error}"),
        )
    })?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

pub struct DataServer {
    listener: TcpListener,
    handle: Arc<Handle>,
}

impl DataServer {
    /// 綁 loopback。`port` 給 0 就是隨機 port（`local_addr` 才知道）。
    pub async fn bind(port: u16, handle: Arc<Handle>) -> std::io::Result<DataServer> {
        let listener = TcpListener::bind(("127.0.0.1", port)).await?;
        Ok(DataServer { listener, handle })
    }

    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    /// 一直 accept，直到 `daemon.shutdown`。
    pub async fn run(self) {
        let mut shutdown = self.handle.shutdown_signal();
        // ⚠️ 訂閱之前就關了的話 `changed()` 永遠等不到：先看一次現在的值。
        if *shutdown.borrow_and_update() {
            return;
        }
        loop {
            tokio::select! {
                accepted = self.listener.accept() => {
                    let Ok((stream, _)) = accepted else { continue };
                    tokio::spawn(serve_connection(stream, self.handle.clone()));
                }
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        return;
                    }
                }
            }
        }
    }
}

type Reply = Response<Full<Bytes>>;

async fn serve_connection(stream: TcpStream, handle: Arc<Handle>) {
    let service = service_fn(move |request| {
        let handle = handle.clone();
        async move { Ok::<Reply, Infallible>(route(handle, request).await) }
    });
    // 連線層的錯（對方亂送、半路斷）只影響這一條，🚫 不往上丟。
    let _ = http1::Builder::new()
        .serve_connection(TokioIo::new(stream), service)
        .await;
}

/// /docs/design/rpc-specs/data-plane.md §3 的路徑表。
async fn route(handle: Arc<Handle>, request: Request<Incoming>) -> Reply {
    let token = match request.uri().path().strip_prefix("/upload/") {
        Some(token) if is_token_shaped(token) => token.to_string(),
        _ => return not_found(),
    };
    if request.method() != Method::PUT {
        return method_not_allowed("PUT");
    }
    let core = handle.core().await;
    // 未解鎖是 503 不是 404：東西在，只是現在打不開（/docs/design/rpc-specs/local-interface.md §5）。
    if !core.is_unlocked() {
        return error_reply(
            StatusCode::SERVICE_UNAVAILABLE,
            CoreErrorKind::Locked.rpc_code(),
            "the vault is locked; call vault.unlock first",
        );
    }
    put_upload(handle, &core, token, request).await
}

/// 一個 PUT：body 是明文，一條連線送到底；回應在 `Seal` 之後才到，body 是 manifest（/docs/design/rpc-specs/data-plane.md §4）。
async fn put_upload(
    handle: Arc<Handle>,
    core: &Core,
    token: String,
    request: Request<Incoming>,
) -> Reply {
    let upload =
        match handle.capabilities().begin_put(&token) {
            PutStart::NotFound => return not_found(),
            PutStart::Busy => {
                return error_reply(
                    StatusCode::CONFLICT,
                    code::BUSY,
                    "this upload already has a PUT in progress",
                )
            }
            PutStart::Broken => return error_reply(
                StatusCode::CONFLICT,
                code::BUSY,
                "a streamed upload cannot resume after its PUT broke off: call media.create again",
            ),
            PutStart::Sealed(manifest) => return manifest_reply(&manifest),
            PutStart::Receive(upload) => upload,
        };
    let in_progress = PutInProgress {
        handle: handle.clone(),
        token,
        ended: false,
    };
    if let Err(message) = check_content_length(&request, upload.block.file_size) {
        return error_reply(StatusCode::BAD_REQUEST, code::INVALID_PARAMS, message);
    }
    // 上傳綁的是建它的那個帳號；core 會再核對一次（/docs/design/rpc-specs/data-plane.md §5）。
    let target = Target {
        user: Some(upload.user_id.clone()),
        server: Some(upload.server.clone()),
        server_backup: handle.settings().server_backup,
    };
    let frames = request
        .into_body()
        .into_data_stream()
        .map_err(std::io::Error::other);
    let mut body = StreamReader::new(frames);
    // ⚠️ 裝箱：matrix-sdk 的 future 很深，讓編譯器一路推 `Send` 會撞 E0275（遞迴上限）；handle 的 dispatch 也是這樣切的。
    let receiving: Pin<Box<dyn Future<Output = Result<Manifest, CoreError>> + Send + '_>> =
        Box::pin(core.receive_upload(&upload, &mut body, &target));
    match receiving.await {
        Ok(manifest) => {
            in_progress.end(Some(&manifest));
            manifest_reply(&manifest)
        }
        Err(error) => {
            in_progress.end(None);
            core_error_reply(&error)
        }
    }
}

/// `Content-Length` 有帶就要跟 `media.create` 的 `size` 一樣；沒帶（chunked）交給 core 數。
///
/// Return:
///     Ok(())         對得上、或沒帶、或這是串流上傳
///     Err(String)    帶了但不是數字、或跟 `size` 不一樣
fn check_content_length(request: &Request<Incoming>, size: Option<u64>) -> Result<(), String> {
    let Some(value) = request.headers().get(header::CONTENT_LENGTH) else {
        return Ok(());
    };
    let length = value
        .to_str()
        .ok()
        .and_then(|text| text.parse::<u64>().ok())
        .ok_or_else(|| "Content-Length is not a number".to_string())?;
    match size {
        Some(size) if size != length => Err(format!(
            "Content-Length {length} does not match the size {size} given to media.create"
        )),
        _ => Ok(()),
    }
}

/// token 的形狀：`TOKEN_BYTES` 個 byte 的小寫 hex。形狀不對就不去查表。
fn is_token_shaped(text: &str) -> bool {
    text.len() == TOKEN_BYTES * 2
        && text
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// core 的錯誤 → HTTP 狀態碼（/docs/design/rpc-specs/data-plane.md §4 的表）；body 是 `{ code, msg }`，code 同 RPC。
fn core_error_reply(error: &CoreError) -> Reply {
    let status = match error.kind {
        CoreErrorKind::Usage => StatusCode::BAD_REQUEST,
        // 帳號登出了：那張 token 形同撤銷。（找不到帳號的是 `Usage`，落在上面那格。）
        CoreErrorKind::NoSuchAccount | CoreErrorKind::NotLoggedIn => StatusCode::NOT_FOUND,
        CoreErrorKind::Locked | CoreErrorKind::NoKeyFile => StatusCode::SERVICE_UNAVAILABLE,
        CoreErrorKind::Network | CoreErrorKind::Server | CoreErrorKind::Timeout => {
            StatusCode::BAD_GATEWAY
        }
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    error_reply(status, error.kind.rpc_code(), &error.message)
}

fn manifest_reply(manifest: &Manifest) -> Reply {
    match manifest.to_json() {
        Ok(json) => json_reply(StatusCode::OK, json),
        Err(error) => error_reply(
            StatusCode::INTERNAL_SERVER_ERROR,
            code::INTERNAL,
            format!("{error}"),
        ),
    }
}

fn error_reply(status: StatusCode, code: u32, msg: impl AsRef<str>) -> Reply {
    let body = serde_json::json!({ "code": code, "msg": msg.as_ref() });
    json_reply(status, body.to_string().into_bytes())
}

fn not_found() -> Reply {
    empty_reply(StatusCode::NOT_FOUND)
}

fn method_not_allowed(allowed: &'static str) -> Reply {
    let mut reply = empty_reply(StatusCode::METHOD_NOT_ALLOWED);
    reply
        .headers_mut()
        .insert(header::ALLOW, header::HeaderValue::from_static(allowed));
    reply
}

fn json_reply(status: StatusCode, json: Vec<u8>) -> Reply {
    let mut reply = Response::new(Full::new(Bytes::from(json)));
    *reply.status_mut() = status;
    reply.headers_mut().insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("application/json"),
    );
    reply
}

fn empty_reply(status: StatusCode) -> Reply {
    let mut reply = Response::new(Full::new(Bytes::new()));
    *reply.status_mut() = status;
    reply
}

#[cfg(test)]
mod tests {
    use super::*;

    const SERVER: &str = "http://127.0.0.1:6167";
    const ALICE: &str = "@alice:localhost";

    fn upload(upload_id: u64, user_id: &str, size: Option<u64>) -> UploadState {
        let cipher = wbf_sdk::FileCipher::generate(wbf_sdk::Cipher::ChaCha20Poly1305, 16).unwrap();
        let mut block = cipher.to_event_block(size.unwrap_or(0));
        block.file_size = size;
        UploadState {
            server: SERVER.into(),
            user_id: user_id.into(),
            upload_id,
            mxc: format!("mxc://localhost/{upload_id:016x}"),
            chunk_max_bytes: 1 << 20,
            block,
        }
    }

    fn manifest_of(upload: &UploadState) -> Manifest {
        let mut block = upload.block.clone();
        block.file_size = Some(40);
        block.sha256 = Some("ab".repeat(32));
        Manifest {
            server: upload.server.clone(),
            mxc: upload.mxc.clone(),
            block,
        }
    }

    #[test]
    fn a_token_is_64_lowercase_hex_and_every_one_is_new() {
        let capabilities = Capabilities::default();
        let first = capabilities
            .issue_upload(upload(1, ALICE, Some(40)))
            .unwrap();
        let second = capabilities
            .issue_upload(upload(1, ALICE, Some(40)))
            .unwrap();
        assert!(is_token_shaped(&first), "{first}");
        assert_ne!(first, second, "同一個上傳鑄兩次也是兩張");
        assert!(!is_token_shaped(&first.to_uppercase()));
        assert!(!is_token_shaped("../etc/passwd"));
        assert!(!is_token_shaped(""));
    }

    /// hyper 可能直接丟掉處理 PUT 的 future（不走我們的錯誤路）：guard 被丟掉也要把「收著」解除，🚫 不留一張永遠 409 的 token。
    #[test]
    fn a_dropped_put_hands_the_token_back() {
        let dir = tempfile::tempdir().unwrap();
        let handle = Handle::new(
            dir.path(),
            crate::connection::EncryptionPolicy::enforced(),
            crate::settings::Settings::default(),
        );
        let token = handle
            .capabilities()
            .issue_upload(upload(1, ALICE, Some(40)))
            .unwrap();
        assert!(matches!(
            handle.capabilities().begin_put(&token),
            PutStart::Receive(_)
        ));
        drop(PutInProgress {
            handle: handle.clone(),
            token: token.clone(),
            ended: false,
        });
        assert!(
            matches!(handle.capabilities().begin_put(&token), PutStart::Receive(_)),
            "丟掉之後可以再 PUT"
        );
    }

    /// 一張 token 同時只收一個 PUT；PUT 斷了（guard 被丟掉）固定大小的放回去可以續傳，串流的作廢；傳完的再 PUT 直接回 manifest。
    #[test]
    fn a_put_holds_the_token_and_what_is_left_after_it_depends_on_how_it_ended() {
        let capabilities = Capabilities::default();
        let sized = capabilities
            .issue_upload(upload(1, ALICE, Some(40)))
            .unwrap();
        assert!(matches!(
            capabilities.begin_put(&sized),
            PutStart::Receive(_)
        ));
        assert!(
            matches!(capabilities.begin_put(&sized), PutStart::Busy),
            "409"
        );
        capabilities.end_put(&sized, None);
        assert!(
            matches!(capabilities.begin_put(&sized), PutStart::Receive(_)),
            "固定大小斷了可以再 PUT（續傳）"
        );
        let manifest = manifest_of(&upload(1, ALICE, Some(40)));
        capabilities.end_put(&sized, Some(&manifest));
        match capabilities.begin_put(&sized) {
            PutStart::Sealed(again) => assert_eq!(again, manifest),
            _ => panic!("傳完的再 PUT 要回同一份 manifest"),
        }

        let streamed = capabilities.issue_upload(upload(2, ALICE, None)).unwrap();
        assert!(matches!(
            capabilities.begin_put(&streamed),
            PutStart::Receive(_)
        ));
        capabilities.end_put(&streamed, None);
        assert!(
            matches!(capabilities.begin_put(&streamed), PutStart::Broken),
            "串流斷了 server 接不回去"
        );
        assert!(matches!(
            capabilities.begin_put(&"0".repeat(64)),
            PutStart::NotFound
        ));
    }

    /// 過期跟不存在一樣（404）；但進行中的 PUT 不被過期打斷，也不被清掉。
    #[test]
    fn an_expired_token_is_not_found_and_a_running_put_outlives_it() {
        let capabilities = Capabilities::default();
        let idle = capabilities
            .issue_upload(upload(1, ALICE, Some(40)))
            .unwrap();
        let running = capabilities
            .issue_upload(upload(2, ALICE, Some(40)))
            .unwrap();
        assert!(matches!(
            capabilities.begin_put(&running),
            PutStart::Receive(_)
        ));
        let past = Instant::now() - Duration::from_secs(1);
        for capability in capabilities.uploads().values_mut() {
            capability.expires_at = past;
        }
        assert!(matches!(capabilities.begin_put(&idle), PutStart::NotFound));
        assert_eq!(
            capabilities
                .find_finished_upload(&upload(1, ALICE, None).mxc, SERVER, ALICE)
                .err(),
            Some(MissingUpload::Unknown)
        );
        // 鑄新的會順手清掉過期的，但🚫 不清正在收的那張。
        capabilities
            .issue_upload(upload(3, ALICE, Some(40)))
            .unwrap();
        assert!(!capabilities.uploads().contains_key(&idle));
        assert!(capabilities.uploads().contains_key(&running));
    }

    /// `room.send_attachment` 只拿傳完的；一定照帳號找——兩台 server 的 `server_name` 一樣時 mxc 也會撞（A5：不是正面認得就不給）。
    #[test]
    fn only_a_finished_upload_of_the_same_account_is_found_by_its_mxc() {
        let capabilities = Capabilities::default();
        let alices = upload(7, ALICE, Some(40));
        let mxc = alices.mxc.clone();
        let token = capabilities.issue_upload(alices.clone()).unwrap();
        let bobs_token = capabilities
            .issue_upload(upload(7, "@bob:localhost", Some(40)))
            .unwrap();
        let find = |user_id: &str| capabilities.find_finished_upload(&mxc, SERVER, user_id);
        assert!(
            find(ALICE).err() == Some(MissingUpload::NotFinished),
            "還沒 PUT"
        );
        assert!(matches!(
            capabilities.begin_put(&token),
            PutStart::Receive(_)
        ));
        assert!(
            find(ALICE).err() == Some(MissingUpload::NotFinished),
            "PUT 進行中也還不算"
        );
        let manifest = manifest_of(&alices);
        capabilities.end_put(&token, Some(&manifest));
        let (found, found_manifest) = find(ALICE).expect("傳完了就找得到");
        assert_eq!(found.user_id, ALICE);
        assert_eq!(found_manifest, manifest);
        assert!(
            find("@bob:localhost").err() == Some(MissingUpload::NotFinished),
            "bob 那個同名 mxc 的還沒傳完——🚫 不是拿 alice 的給他"
        );
        assert_eq!(
            capabilities
                .find_finished_upload(&mxc, "http://other:6167", ALICE)
                .err(),
            Some(MissingUpload::Unknown)
        );
        assert_eq!(
            capabilities
                .find_finished_upload("mxc://localhost/0000000000000008", SERVER, ALICE)
                .err(),
            Some(MissingUpload::Unknown)
        );

        capabilities.revoke_account(SERVER, ALICE);
        assert!(find(ALICE).err() == Some(MissingUpload::Unknown));
        assert!(matches!(capabilities.begin_put(&token), PutStart::NotFound));
        assert!(
            matches!(capabilities.begin_put(&bobs_token), PutStart::Receive(_)),
            "只撤那一個帳號的"
        );
    }
}

//! 資料平面（/docs/design/rpc-specs/data-plane.md）：`http://127.0.0.1:<data port>`，媒體的 bytes 只走這裡。
//!
//! | 這裡 | 別處 |
//! |---|---|
//! | URL 與 meta（[`AccessKeys`]）：URL 裡那段是共享 token 加密的「用途 ‖ mxc」，上傳狀態在 `Wbf-Upload-Meta` header | `media.create` 鑄（`handle/media.rs`） |
//! | HTTP listener（[`DataServer`]）：路徑、Host、狀態碼、把 body 交給 core | 怎麼切塊、加密、上傳（`Core::receive_upload`） |
//!
//! 🚫 沒有 token 表、沒有 TTL（維護者 2026-09-30）：URL 與 header 帶著上傳的一切，daemon 解得開就是它發的；
//! 能活多久、要不要拒，是 server 的事。唯一的表是「正在收的上傳」，只活在那條連線的期間。🚫 這裡不印東西。

use std::collections::HashSet;
use std::convert::Infallible;
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use bytes::Bytes;
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
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
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

use crate::handle::Handle;
use crate::message::code;

/// 上傳的路徑前綴；後面接 URL key（/docs/design/rpc-specs/data-plane.md §3）。
pub const UPLOAD_PATH: &str = "/upload/mxc/";
/// PUT 時帶上傳狀態的 header（`media.create` 給，/docs/design/rpc-specs/data-plane.md §2）。
pub const UPLOAD_META_HEADER: &str = "Wbf-Upload-Meta";
/// 資料平面從共享 token 導鑰的 context（跟 RPC 的兩把分開，/docs/design/rpc-specs/local-interface.md §4）。
const ACCESS_CONTEXT: &str = "wbf-matrix-client data plane v1";
const URL_AAD: &[u8] = b"wbf-data url v1";
/// meta 的 AAD 後面再接 mxc：一份 meta 只配得上它那個 URL。
const META_AAD: &[u8] = b"wbf-data meta v1 ";
const NONCE_LEN: usize = 24;
/// 加密模式：`e_` ＋ 密文（URL 用 base58，header 用 base64url）。
const ENCRYPTED_PREFIX: &str = "e_";
/// 明文模式（`daemon.set_encryption` 關掉時）：`c_` ＋ 同一段明文、不加密。
const PLAIN_PREFIX: &str = "c_";
/// URL 明文的第一個 byte：這個 URL 是做什麼的。下載的 URL 拿來上傳要被拒。
const PURPOSE_UPLOAD: u8 = 0x01;

/// 從共享 token 導出的那一把，只拿來封／開資料平面的 URL 與 meta。
#[derive(Zeroize, ZeroizeOnDrop)]
pub struct AccessKeys {
    key: [u8; 32],
}

impl AccessKeys {
    /// Args:
    ///     token: `daemon.token` 的內容（跟 RPC 同一份）, example: 256 個隨機 byte
    pub fn from_token(token: &[u8]) -> AccessKeys {
        AccessKeys {
            key: blake3::derive_key(ACCESS_CONTEXT, token),
        }
    }

    /// 上傳 URL 裡 `/upload/mxc/` 後面那段：用途 ‖ mxc。
    ///
    /// Args:
    ///     mxc: example: "mxc://localhost/000000000000004d"
    ///     encrypted: daemon 現在是不是加密模式；是就 `e_`（token 加密），不是就 `c_`（明文、只給除錯）
    /// Return:
    ///     Ok(String)       example: "e_3mJr7AoUXx2Wqd…"（約 100 個字元）
    ///     Err(CoreError)   OS 給不出亂數（`Io`）
    pub fn to_upload_url_key(&self, mxc: &str, encrypted: bool) -> Result<String, CoreError> {
        let mut plain = vec![PURPOSE_UPLOAD];
        plain.extend_from_slice(mxc.as_bytes());
        let bytes = match encrypted {
            true => self.seal(&plain, URL_AAD)?,
            false => plain,
        };
        Ok(format!(
            "{}{}",
            prefix_of(encrypted),
            bs58::encode(bytes).into_string()
        ))
    }

    /// 開上傳 URL 那段。
    ///
    /// Args:
    ///     url_key: URL 裡那段, example: "e_3mJr7AoUXx2Wqd…"
    ///     encryption_enforced: daemon 現在是不是加密模式；是的話 `c_` 一律不收（fail closed）
    /// Return:
    ///     Some(String)   mxc
    ///     None           不是我們發的、被改過、別的 daemon（token 不同）發的、用途不對、形狀不對、加密模式下的 `c_`
    pub fn open_upload_url_key(&self, url_key: &str, encryption_enforced: bool) -> Option<String> {
        let plain = if let Some(encoded) = url_key.strip_prefix(ENCRYPTED_PREFIX) {
            self.open(&bs58::decode(encoded).into_vec().ok()?, URL_AAD)?
        } else if let Some(encoded) = url_key.strip_prefix(PLAIN_PREFIX) {
            if encryption_enforced {
                return None;
            }
            Zeroizing::new(bs58::decode(encoded).into_vec().ok()?)
        } else {
            return None;
        };
        let (purpose, mxc) = plain.split_first()?;
        if *purpose != PURPOSE_UPLOAD {
            return None;
        }
        String::from_utf8(mxc.to_vec()).ok()
    }

    /// `Wbf-Upload-Meta` header 的值：整份上傳狀態（含檔案金鑰），加密時綁定它的 mxc。
    ///
    /// Args:
    ///     upload: `Core::create_upload` 回的
    ///     encrypted: 同 [`AccessKeys::to_upload_url_key`]
    /// Return:
    ///     Ok(String)       example: "e_Qk3v…"（base64url，約 450 個字元）
    ///     Err(CoreError)   OS 給不出亂數、序列化不了（`Io`）
    pub fn to_upload_meta(
        &self,
        upload: &UploadState,
        encrypted: bool,
    ) -> Result<String, CoreError> {
        let json = Zeroizing::new(serde_json::to_vec(upload).map_err(|error| {
            CoreError::new(
                CoreErrorKind::Io,
                format!("cannot serialise the upload: {error}"),
            )
        })?);
        let encoded = match encrypted {
            true => URL_SAFE_NO_PAD.encode(self.seal(&json, &meta_aad(&upload.mxc))?),
            false => URL_SAFE_NO_PAD.encode(&*json),
        };
        Ok(format!("{}{encoded}", prefix_of(encrypted)))
    }

    /// 開 `Wbf-Upload-Meta`，而且它要是**這個 URL 的** meta。
    ///
    /// Args:
    ///     meta: header 的值, example: "e_Qk3v…"
    ///     mxc: 從 URL 開出來的那個, example: "mxc://localhost/000000000000004d"
    ///     encryption_enforced: 同 [`AccessKeys::open_upload_url_key`]
    /// Return:
    ///     Some(UploadState)   這個 daemon 為這個 mxc 發的
    ///     None                不是我們發的、被改過、是別的上傳的（mxc 對不上）、形狀不對、加密模式下的 `c_`
    pub fn open_upload_meta(
        &self,
        meta: &str,
        mxc: &str,
        encryption_enforced: bool,
    ) -> Option<UploadState> {
        let json = if let Some(encoded) = meta.strip_prefix(ENCRYPTED_PREFIX) {
            self.open(&URL_SAFE_NO_PAD.decode(encoded).ok()?, &meta_aad(mxc))?
        } else if let Some(encoded) = meta.strip_prefix(PLAIN_PREFIX) {
            if encryption_enforced {
                return None;
            }
            Zeroizing::new(URL_SAFE_NO_PAD.decode(encoded).ok()?)
        } else {
            return None;
        };
        let upload: UploadState = serde_json::from_slice(&json).ok()?;
        // 加密的已經被 AAD 綁住；明文的沒有，所以兩種都再對一次（A6：不靠上游記得綁）。
        (upload.mxc == mxc).then_some(upload)
    }

    /// Return:
    ///     Ok(Vec<u8>)      nonce(24) ‖ 密文
    ///     Err(CoreError)   OS 給不出亂數、AEAD 底層回錯（`Io`）
    fn seal(&self, plain: &[u8], aad: &[u8]) -> Result<Vec<u8>, CoreError> {
        let mut nonce = [0u8; NONCE_LEN];
        getrandom::getrandom(&mut nonce).map_err(|error| {
            CoreError::new(
                CoreErrorKind::Io,
                format!("no randomness for the data plane: {error}"),
            )
        })?;
        let sealed = XChaCha20Poly1305::new(&self.key.into())
            .encrypt(XNonce::from_slice(&nonce), Payload { msg: plain, aad })
            .map_err(|_| CoreError::new(CoreErrorKind::Io, "cannot seal for the data plane"))?;
        let mut bytes = nonce.to_vec();
        bytes.extend_from_slice(&sealed);
        Ok(bytes)
    }

    /// Return:
    ///     Some(明文)   解得開
    ///     None         太短、被改過、別的鑰匙、AAD 對不上
    fn open(&self, bytes: &[u8], aad: &[u8]) -> Option<Zeroizing<Vec<u8>>> {
        let nonce = bytes.get(..NONCE_LEN)?;
        let sealed = bytes.get(NONCE_LEN..)?;
        XChaCha20Poly1305::new(&self.key.into())
            .decrypt(XNonce::from_slice(nonce), Payload { msg: sealed, aad })
            .ok()
            .map(Zeroizing::new)
    }
}

fn prefix_of(encrypted: bool) -> &'static str {
    match encrypted {
        true => ENCRYPTED_PREFIX,
        false => PLAIN_PREFIX,
    }
}

fn meta_aad(mxc: &str) -> Vec<u8> {
    let mut aad = META_AAD.to_vec();
    aad.extend_from_slice(mxc.as_bytes());
    aad
}

/// 正在收的上傳（server、上傳 id）：同一個上傳同時只收一條 PUT（兩條交錯送，串流的塊會亂）。只活在連線期間。
#[derive(Default)]
struct UploadsReceiving {
    uploads: Mutex<HashSet<(String, u64)>>,
}

impl UploadsReceiving {
    fn uploads(&self) -> MutexGuard<'_, HashSet<(String, u64)>> {
        self.uploads
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Return:
    ///     Some(ReceivingGuard)   登記好了；丟掉 guard 就解除
    ///     None                   這個上傳已經有一條 PUT 在收
    fn begin(self: &Arc<Self>, upload: &UploadState) -> Option<ReceivingGuard> {
        let key = (upload.server.clone(), upload.upload_id);
        if !self.uploads().insert(key.clone()) {
            return None;
        }
        Some(ReceivingGuard {
            receiving: self.clone(),
            key,
        })
    }
}

/// 🚨 丟掉就解除登記——PUT 怎麼結束都一樣（回了、失敗、連線斷了、future 被 hyper 丟掉），🚫 不留一個永遠 409 的上傳。
struct ReceivingGuard {
    receiving: Arc<UploadsReceiving>,
    key: (String, u64),
}

impl Drop for ReceivingGuard {
    fn drop(&mut self) {
        self.receiving.uploads().remove(&self.key);
    }
}

pub struct DataServer {
    listener: TcpListener,
    handle: Arc<Handle>,
    receiving: Arc<UploadsReceiving>,
}

impl DataServer {
    /// 綁 loopback。`port` 給 0 就是隨機 port（`local_addr` 才知道）。
    pub async fn bind(port: u16, handle: Arc<Handle>) -> std::io::Result<DataServer> {
        let listener = TcpListener::bind(("127.0.0.1", port)).await?;
        Ok(DataServer {
            listener,
            handle,
            receiving: Arc::default(),
        })
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
                    tokio::spawn(serve_connection(stream, self.handle.clone(), self.receiving.clone()));
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

async fn serve_connection(
    stream: TcpStream,
    handle: Arc<Handle>,
    receiving: Arc<UploadsReceiving>,
) {
    let service = service_fn(move |request| {
        let (handle, receiving) = (handle.clone(), receiving.clone());
        async move { Ok::<Reply, Infallible>(route(handle, receiving, request).await) }
    });
    // 連線層的錯（對方亂送、半路斷）只影響這一條，🚫 不往上丟。
    let _ = http1::Builder::new()
        .serve_connection(TokioIo::new(stream), service)
        .await;
}

/// /docs/design/rpc-specs/data-plane.md §3 的路徑表。
async fn route(
    handle: Arc<Handle>,
    receiving: Arc<UploadsReceiving>,
    request: Request<Incoming>,
) -> Reply {
    if !is_loopback_host(&request) {
        return empty_reply(StatusCode::FORBIDDEN);
    }
    let Some(access) = request.uri().path().strip_prefix(UPLOAD_PATH) else {
        return not_found();
    };
    let access = access.to_string();
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
    let Some(keys) = handle.access_keys() else {
        return not_found();
    };
    let enforced = handle.is_encryption_enforced();
    let Some(mxc) = keys.open_upload_url_key(&access, enforced) else {
        return not_found();
    };
    let Some(meta) = request
        .headers()
        .get(UPLOAD_META_HEADER)
        .and_then(|value| value.to_str().ok())
    else {
        return error_reply(
            StatusCode::BAD_REQUEST,
            code::INVALID_PARAMS,
            format!(
                "missing the {UPLOAD_META_HEADER} header: send the headers media.create returned"
            ),
        );
    };
    // 開不開得了、是不是這個 URL 的：都不是就跟不認得的 URL 一樣 404（🚫 不分辨原因）。
    let Some(upload) = keys.open_upload_meta(meta, &mxc, enforced) else {
        return not_found();
    };
    let Some(_receiving) = receiving.begin(&upload) else {
        return error_reply(
            StatusCode::CONFLICT,
            code::BUSY,
            "this upload already has a PUT in progress",
        );
    };
    put_upload(&handle, &core, &upload, request).await
}

/// 一個 PUT：body 是明文，一條連線送到底；回應在 `Seal` 之後才到，body 是 manifest（/docs/design/rpc-specs/data-plane.md §4）。
async fn put_upload(
    handle: &Handle,
    core: &Core,
    upload: &UploadState,
    request: Request<Incoming>,
) -> Reply {
    if let Err(message) = check_content_length(&request, upload.block.file_size) {
        return error_reply(StatusCode::BAD_REQUEST, code::INVALID_PARAMS, message);
    }
    // 上傳綁的是建它的那個帳號；core 會再核對一次（/docs/design/rpc-specs/data-plane.md §1）。
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
        Box::pin(core.receive_upload(upload, &mut body, &target));
    match receiving.await {
        Ok(manifest) => manifest_reply(&manifest),
        Err(error) => core_error_reply(&error),
    }
}

/// `Host` 是 loopback 嗎（擋 DNS rebinding：網頁把自己的網域指到 127.0.0.1 之後，瀏覽器送的 Host 是那個網域）。
///
/// Return:
///     bool  1 ＝ `127.0.0.1`、`localhost`、`[::1]`（帶不帶 port 都可以）；沒有 Host 或其他 → 0
fn is_loopback_host(request: &Request<Incoming>) -> bool {
    let Some(host) = request
        .headers()
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
    else {
        return false;
    };
    let name = match host.strip_prefix("[::1]") {
        Some(rest) => return rest.is_empty() || rest.starts_with(':'),
        None => host.split(':').next().unwrap_or(host),
    };
    name == "127.0.0.1" || name.eq_ignore_ascii_case("localhost")
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

/// core 的錯誤 → HTTP 狀態碼（/docs/design/rpc-specs/data-plane.md §4.2 的表）；body 是 `{ code, msg }`，code 同 RPC。
fn core_error_reply(error: &CoreError) -> Reply {
    let status = match error.kind {
        CoreErrorKind::Usage => StatusCode::BAD_REQUEST,
        // 帳號登出了：那個 URL 形同作廢。（找不到帳號的是 `Usage`，落在上面那格。）
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

    fn upload(upload_id: u64, size: Option<u64>) -> UploadState {
        let cipher = wbf_sdk::FileCipher::generate(wbf_sdk::Cipher::ChaCha20Poly1305, 16).unwrap();
        let mut block = cipher.to_event_block(size.unwrap_or(0));
        block.file_size = size;
        block.name = Some("v.bin".into());
        UploadState {
            server: SERVER.into(),
            user_id: ALICE.into(),
            upload_id,
            mxc: format!("mxc://localhost/{upload_id:016x}"),
            chunk_max_bytes: 1 << 20,
            block,
        }
    }

    /// URL 只帶「用途 ‖ mxc」、短而且看不出是哪個檔；meta 開得回整份上傳（含檔案金鑰）。每次封都不一樣（nonce 隨機），都能重複開——URL 可以重用。
    #[test]
    fn the_url_carries_only_the_mxc_and_the_meta_carries_the_upload() {
        let keys = AccessKeys::from_token(&[7u8; 256]);
        let original = upload(7, Some(40));
        let first = keys.to_upload_url_key(&original.mxc, true).unwrap();
        let second = keys.to_upload_url_key(&original.mxc, true).unwrap();
        assert!(first.starts_with("e_") && first.len() < 120, "{first}");
        assert_ne!(first, second, "nonce 每次隨機");
        assert!(!first.contains("localhost"), "加密的 URL 看不出是哪個檔");
        for url_key in [&first, &first, &second] {
            assert_eq!(
                keys.open_upload_url_key(url_key, true),
                Some(original.mxc.clone())
            );
        }
        let meta = keys.to_upload_meta(&original, true).unwrap();
        assert!(meta.starts_with("e_"), "{meta}");
        assert_eq!(
            keys.open_upload_meta(&meta, &original.mxc, true),
            Some(original.clone())
        );
        assert_eq!(
            keys.open_upload_meta(&meta, &original.mxc, true),
            Some(original)
        );
    }

    /// 一份 meta 只配得上它那個 URL：A 檔的 meta 配 B 檔的 URL 一律拒（加密模式靠 AAD，明文模式靠再對一次 mxc）。
    #[test]
    fn a_meta_of_another_upload_does_not_fit_this_url() {
        let keys = AccessKeys::from_token(&[7u8; 256]);
        let (a, b) = (upload(1, Some(40)), upload(2, Some(40)));
        let meta_of_a = keys.to_upload_meta(&a, true).unwrap();
        assert_eq!(keys.open_upload_meta(&meta_of_a, &b.mxc, true), None);
        let plain_meta_of_a = keys.to_upload_meta(&a, false).unwrap();
        assert_eq!(keys.open_upload_meta(&plain_meta_of_a, &b.mxc, false), None);
        assert_eq!(
            keys.open_upload_meta(&plain_meta_of_a, &a.mxc, false),
            Some(a)
        );
    }

    /// 不是這個 daemon 發的一律不認（A5：不是正面認得就拒）：別的 token、改過一個字、截斷、亂寫、別的前綴、用途不對。
    #[test]
    fn what_this_daemon_did_not_issue_is_refused() {
        let keys = AccessKeys::from_token(&[7u8; 256]);
        let original = upload(7, Some(40));
        let url_key = keys.to_upload_url_key(&original.mxc, true).unwrap();
        let meta = keys.to_upload_meta(&original, true).unwrap();
        let other_daemon = AccessKeys::from_token(&[8u8; 256]);
        assert_eq!(other_daemon.open_upload_url_key(&url_key, true), None);
        assert_eq!(
            other_daemon.open_upload_meta(&meta, &original.mxc, true),
            None
        );

        let mut tampered = url_key.clone().into_bytes();
        let last = tampered.len() - 1;
        tampered[last] = if tampered[last] == b'2' { b'3' } else { b'2' };
        let tampered = String::from_utf8(tampered).unwrap();
        assert_eq!(keys.open_upload_url_key(&tampered, true), None);
        assert_eq!(
            keys.open_upload_url_key(&url_key[..url_key.len() - 5], true),
            None
        );
        assert_eq!(
            keys.open_upload_meta(&meta[..meta.len() - 5], &original.mxc, true),
            None
        );
        for junk in ["", "e_", "e_0OIl", "x_abc", "../../etc/passwd", "e_111"] {
            assert_eq!(keys.open_upload_url_key(junk, true), None, "{junk}");
            assert_eq!(
                keys.open_upload_meta(junk, &original.mxc, true),
                None,
                "{junk}"
            );
        }

        // 用途不對（例如之後的下載 URL）：解得開也不收。
        let mut plain = vec![0x02];
        plain.extend_from_slice(original.mxc.as_bytes());
        let wrong_purpose = format!("c_{}", bs58::encode(plain).into_string());
        assert_eq!(keys.open_upload_url_key(&wrong_purpose, false), None);
    }

    /// 明文模式（`c_`）只在 daemon 關掉加密時收；加密模式下拿 `c_` 來一律拒（fail closed）。
    #[test]
    fn plain_keys_are_taken_only_while_encryption_is_off() {
        let keys = AccessKeys::from_token(&[7u8; 256]);
        let original = upload(9, None);
        let url_key = keys.to_upload_url_key(&original.mxc, false).unwrap();
        let meta = keys.to_upload_meta(&original, false).unwrap();
        assert!(url_key.starts_with("c_") && meta.starts_with("c_"));
        assert_eq!(
            keys.open_upload_url_key(&url_key, false),
            Some(original.mxc.clone())
        );
        assert_eq!(keys.open_upload_url_key(&url_key, true), None);
        assert_eq!(
            keys.open_upload_meta(&meta, &original.mxc, false),
            Some(original.clone())
        );
        assert_eq!(keys.open_upload_meta(&meta, &original.mxc, true), None);
        // 加密的那種兩個模式都收。
        let encrypted = keys.to_upload_url_key(&original.mxc, true).unwrap();
        assert_eq!(
            keys.open_upload_url_key(&encrypted, false),
            Some(original.mxc)
        );
    }

    /// 同一個上傳同時只收一條 PUT；guard 丟掉（PUT 怎麼結束都一樣，含 future 被 hyper 丟掉）就解除。
    #[test]
    fn one_put_per_upload_at_a_time_and_dropping_the_guard_releases_it() {
        let receiving = Arc::new(UploadsReceiving::default());
        let first = receiving.begin(&upload(1, Some(40))).unwrap();
        assert!(receiving.begin(&upload(1, Some(40))).is_none(), "409");
        let other = receiving.begin(&upload(2, Some(40)));
        assert!(other.is_some(), "別的上傳不受影響");
        drop(first);
        assert!(
            receiving.begin(&upload(1, Some(40))).is_some(),
            "丟掉之後可以再 PUT"
        );
    }
}

//! 資料平面（/docs/design/rpc-specs/data-plane.md）：`http://127.0.0.1:<data port>`，媒體的 bytes 只走這裡。
//!
//! | 這裡 | 別處 |
//! |---|---|
//! | URL 與 meta（[`AccessKeys`]）：URL 裡那段是共享 token 加密的「用途 ‖ mxc」，上傳狀態在 `Wbf-Upload-Meta` header | `media.create`／`media.open` 鑄（`handle/media.rs`） |
//! | HTTP listener（[`DataServer`]）：路徑、Host、Range、狀態碼、把 body 交給 core、把 core 給的明文串流出去 | 怎麼切塊、加密、上傳（`Core::receive_upload`）；從哪讀（`Core::find_media_source`） |
//!
//! 🚫 沒有 token 表、沒有 TTL（維護者 2026-09-30）：URL 與 header 帶著上傳的一切，daemon 解得開就是它發的；
//! 能活多久、要不要拒，是 server 的事。唯一的表是「正在收的上傳」，只活在那條連線的期間。🚫 這裡不印東西。

use std::collections::HashSet;
use std::convert::Infallible;
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};

use bytes::Bytes;
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use futures_util::TryStreamExt;
use http_body_util::combinators::UnsyncBoxBody;
use http_body_util::{BodyExt, Full, StreamBody};
use hyper::body::Frame;
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{header, Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde::{Deserialize, Serialize};
use tokio::net::{TcpListener, TcpStream};
use tokio_util::io::StreamReader;
use wbf_core::{Core, CoreError, CoreErrorKind, MediaStream, Target};
use wbf_sdk::{Manifest, UploadState};
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

use crate::handle::Handle;
use crate::message::code;

/// 上傳的路徑前綴；後面接 URL key（/docs/design/rpc-specs/data-plane.md §3）。
pub const UPLOAD_PATH: &str = "/upload/mxc/";
/// 讀的路徑前綴；後面接 URL key（/docs/design/rpc-specs/data-plane.md §8）。
pub const MEDIA_PATH: &str = "/media/mxc/";
/// PUT 時帶上傳狀態的 header（`media.create` 給，/docs/design/rpc-specs/data-plane.md §2）。
pub const UPLOAD_META_HEADER: &str = "Wbf-Upload-Meta";
/// 讀的每個回應都帶：這個檔的格式（`media.kind`）與驗證結果（`media.verified`），讓前端知道 412 是哪一種（/docs/design/rpc-specs/data-plane.md §8.2）。
/// ⚠️ 要小寫：`HeaderMap::insert` 收 `&'static str` 時大寫的名字會 panic。
pub const MEDIA_KIND_HEADER: &str = "wbf-media-kind";
pub const MEDIA_VERIFIED_HEADER: &str = "wbf-media-verified";
/// 資料平面從共享 token 導鑰的 context（跟 RPC 的兩把分開，/docs/design/rpc-specs/local-interface.md §4）。
const ACCESS_CONTEXT: &str = "wbf-matrix-client data plane v1";
const URL_AAD: &[u8] = b"wbf-data url v1";
/// meta 的 AAD 後面再接 mxc：一份 meta 只配得上它那個 URL。
const META_AAD: &[u8] = b"wbf-data meta v1 ";
const NONCE_LEN: usize = 24;
/// 加密模式：`e-<B58(nonce)>_<B58(密文)>`——跟目錄名同一個長相（/docs/design/storage/vault-and-keys.md §2.2）。
const ENCRYPTED_PREFIX: &str = "e-";
/// 明文模式（`daemon.set_encryption` 關掉時）：`c-<B58(明文)>`，不加密。
const PLAIN_PREFIX: &str = "c-";
/// 加密模式裡分隔 nonce 與密文。Base58 的字母表沒有底線，所以它不會出現在兩段裡面。
const SEPARATOR: char = '_';
/// URL 明文的第一個 byte：這個 URL 是做什麼的。下載的 URL 拿來上傳要被拒，反之亦然。
const PURPOSE_UPLOAD: u8 = 0x01;
const PURPOSE_MEDIA: u8 = 0x02;

/// `Wbf-Upload-Meta` 裡封的東西：上傳狀態，加上 UI 給的原檔位置（/docs/design/rpc-specs/data-plane.md §8.1）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UploadMeta {
    pub upload: UploadState,
    /// `media.create` 的 `source_uri`，原樣；daemon 不解它，傳完記進 `media` 列。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_uri: Option<String>,
}

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
    ///     encrypted: daemon 現在是不是加密模式；是就 `e-`（token 加密），不是就 `c-`（明文、只給除錯）
    /// Return:
    ///     Ok(String)       example: "e-Hq3TbQ…_4kVn9s…"（約 100 個字元）
    ///     Err(CoreError)   OS 給不出亂數（`Io`）
    pub fn to_upload_url_key(&self, mxc: &str, encrypted: bool) -> Result<String, CoreError> {
        self.to_url_key(PURPOSE_UPLOAD, mxc, encrypted)
    }

    /// 讀的 URL 裡 `/media/mxc/` 後面那段：用途 ‖ mxc，🚫 不帶帳號（/docs/design/rpc-specs/data-plane.md §8）。
    ///
    /// Args:
    ///     mxc: example: "mxc://localhost/000000000000004d"
    ///     encrypted: 同 [`AccessKeys::to_upload_url_key`]
    /// Return:
    ///     Ok(String)       example: "e-Hq3TbQ…_4kVn9s…"
    ///     Err(CoreError)   OS 給不出亂數（`Io`）
    pub fn to_media_url_key(&self, mxc: &str, encrypted: bool) -> Result<String, CoreError> {
        self.to_url_key(PURPOSE_MEDIA, mxc, encrypted)
    }

    /// 開讀的 URL 那段。
    ///
    /// Return:
    ///     Some(String)   mxc
    ///     None           同 [`AccessKeys::open_upload_url_key`]（上傳的 URL 拿來讀也是 None）
    pub fn open_media_url_key(&self, url_key: &str, encryption_enforced: bool) -> Option<String> {
        self.open_url_key(PURPOSE_MEDIA, url_key, encryption_enforced)
    }

    fn to_url_key(&self, purpose: u8, mxc: &str, encrypted: bool) -> Result<String, CoreError> {
        let mut plain = vec![purpose];
        plain.extend_from_slice(mxc.as_bytes());
        self.to_text(&plain, URL_AAD, encrypted)
    }

    fn open_url_key(
        &self,
        purpose: u8,
        url_key: &str,
        encryption_enforced: bool,
    ) -> Option<String> {
        let plain = self.open_text(url_key, URL_AAD, encryption_enforced)?;
        let (found, mxc) = plain.split_first()?;
        if *found != purpose {
            return None;
        }
        String::from_utf8(mxc.to_vec()).ok()
    }

    /// 開上傳 URL 那段。
    ///
    /// Args:
    ///     url_key: URL 裡那段, example: "e-Hq3TbQ…_4kVn9s…"
    ///     encryption_enforced: daemon 現在是不是加密模式；是的話 `c-` 一律不收（fail closed）
    /// Return:
    ///     Some(String)   mxc
    ///     None           不是我們發的、被改過、別的 daemon（token 不同）發的、用途不對、形狀不對、加密模式下的 `c-`
    pub fn open_upload_url_key(&self, url_key: &str, encryption_enforced: bool) -> Option<String> {
        self.open_url_key(PURPOSE_UPLOAD, url_key, encryption_enforced)
    }

    /// `Wbf-Upload-Meta` header 的值：整份上傳狀態（含檔案金鑰）與原檔位置，加密時綁定它的 mxc。
    ///
    /// Args:
    ///     meta: 上傳狀態是 `Core::create_upload` 回的；`source_uri` 是 UI 給的
    ///     encrypted: 同 [`AccessKeys::to_upload_url_key`]
    /// Return:
    ///     Ok(String)       example: "e-Hq3TbQ…_8Pz2Lw…"（約 520 個字元）
    ///     Err(CoreError)   OS 給不出亂數、序列化不了（`Io`）
    pub fn to_upload_meta(&self, meta: &UploadMeta, encrypted: bool) -> Result<String, CoreError> {
        let json = Zeroizing::new(serde_json::to_vec(meta).map_err(|error| {
            CoreError::new(
                CoreErrorKind::Io,
                format!("cannot serialise the upload: {error}"),
            )
        })?);
        self.to_text(&json, &meta_aad(&meta.upload.mxc), encrypted)
    }

    /// 開 `Wbf-Upload-Meta`，而且它要是**這個 URL 的** meta。
    ///
    /// Args:
    ///     meta: header 的值, example: "e-Hq3TbQ…_8Pz2Lw…"
    ///     mxc: 從 URL 開出來的那個, example: "mxc://localhost/000000000000004d"
    ///     encryption_enforced: 同 [`AccessKeys::open_upload_url_key`]
    /// Return:
    ///     Some(UploadMeta)    這個 daemon 為這個 mxc 發的
    ///     None                不是我們發的、被改過、是別的上傳的（mxc 對不上）、形狀不對、加密模式下的 `c-`
    pub fn open_upload_meta(
        &self,
        meta: &str,
        mxc: &str,
        encryption_enforced: bool,
    ) -> Option<UploadMeta> {
        let json = self.open_text(meta, &meta_aad(mxc), encryption_enforced)?;
        let meta: UploadMeta = serde_json::from_slice(&json).ok()?;
        // 加密的已經被 AAD 綁住；明文的沒有，所以兩種都再對一次（A6：不靠上游記得綁）。
        (meta.upload.mxc == mxc).then_some(meta)
    }

    /// Return:
    ///     Ok(String)       加密：`e-<B58(nonce)>_<B58(密文)>`；明文：`c-<B58(明文)>`
    ///     Err(CoreError)   OS 給不出亂數、AEAD 底層回錯（`Io`）
    fn to_text(&self, plain: &[u8], aad: &[u8], encrypted: bool) -> Result<String, CoreError> {
        if !encrypted {
            return Ok(format!(
                "{PLAIN_PREFIX}{}",
                bs58::encode(plain).into_string()
            ));
        }
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
        Ok(format!(
            "{ENCRYPTED_PREFIX}{}{SEPARATOR}{}",
            bs58::encode(nonce).into_string(),
            bs58::encode(sealed).into_string()
        ))
    }

    /// Return:
    ///     Some(明文)   `e-` 解得開，或（加密模式關著時）`c-` 解碼得了
    ///     None         別的前綴、沒有分隔、Base58 壞、nonce 不是 24 byte、被改過、別的鑰匙、AAD 對不上、加密模式下的 `c-`
    fn open_text(
        &self,
        text: &str,
        aad: &[u8],
        encryption_enforced: bool,
    ) -> Option<Zeroizing<Vec<u8>>> {
        if let Some(encrypted) = text.strip_prefix(ENCRYPTED_PREFIX) {
            let (nonce, sealed) = encrypted.split_once(SEPARATOR)?;
            let nonce = bs58::decode(nonce).into_vec().ok()?;
            if nonce.len() != NONCE_LEN {
                return None;
            }
            let sealed = bs58::decode(sealed).into_vec().ok()?;
            return XChaCha20Poly1305::new(&self.key.into())
                .decrypt(XNonce::from_slice(&nonce), Payload { msg: &sealed, aad })
                .ok()
                .map(Zeroizing::new);
        }
        let plain = text.strip_prefix(PLAIN_PREFIX)?;
        if encryption_enforced {
            return None;
        }
        bs58::decode(plain).into_vec().ok().map(Zeroizing::new)
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

/// 回應的 body：小的一次給（`Full`），讀的是串流（`StreamBody`）。⚠️ 用 `Unsync`：串流裡等下載處理端的 future 不一定是 `Sync`。
type ReplyBody = UnsyncBoxBody<Bytes, std::io::Error>;
type Reply = Response<ReplyBody>;

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
    let path = request.uri().path().to_string();
    if let Some(access) = path.strip_prefix(UPLOAD_PATH) {
        if request.method() != Method::PUT {
            return method_not_allowed("PUT");
        }
        return route_upload(handle, receiving, access, request).await;
    }
    if let Some(access) = path.strip_prefix(MEDIA_PATH) {
        let is_head = request.method() == Method::HEAD;
        if request.method() != Method::GET && !is_head {
            return method_not_allowed("GET, HEAD");
        }
        return get_media(&handle, access, &request, is_head).await;
    }
    not_found()
}

/// `PUT /upload/mxc/<URL key>`：開 URL 與 meta、同一個上傳只收一條 PUT。
async fn route_upload(
    handle: Arc<Handle>,
    receiving: Arc<UploadsReceiving>,
    access: &str,
    request: Request<Incoming>,
) -> Reply {
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
    let Some(mxc) = keys.open_upload_url_key(access, enforced) else {
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
    let Some(meta) = keys.open_upload_meta(meta, &mxc, enforced) else {
        return not_found();
    };
    let Some(_receiving) = receiving.begin(&meta.upload) else {
        return error_reply(
            StatusCode::CONFLICT,
            code::BUSY,
            "this upload already has a PUT in progress",
        );
    };
    put_upload(&handle, &core, &meta, request).await
}

/// `GET`／`HEAD /media/mxc/<URL key>`（/docs/design/rpc-specs/data-plane.md §8）：開 URL → 找來源 → Range → 邊讀邊吐明文。
async fn get_media(
    handle: &Handle,
    access: &str,
    request: &Request<Incoming>,
    is_head: bool,
) -> Reply {
    let core = handle.core().await;
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
    let Some(mxc) = keys.open_media_url_key(access, handle.is_encryption_enforced()) else {
        return not_found();
    };
    let source = match core.find_media_source(&mxc).await {
        Ok(Some(source)) => source,
        Ok(None) => return not_found(),
        Err(error) if error.kind == CoreErrorKind::Locked => return core_error_reply(&error),
        // 有紀錄但沒有完整的檔、也拿不到（可信的）金鑰去拉——沒帳號看得到帶金鑰的事件，或看得到的描述都跟本地那一列對不上：上游這一段走不通（§8 的 502）。
        Err(error) => {
            return error_reply(
                StatusCode::BAD_GATEWAY,
                error.kind.rpc_code(),
                &error.message,
            )
        }
    };
    let size = source.size;
    let range = request
        .headers()
        .get(header::RANGE)
        .and_then(|value| value.to_str().ok());
    let (status, start, end) = match parse_range(range, size) {
        RangeAsked::Whole => (StatusCode::OK, 0, size),
        RangeAsked::Part(start, end) => (StatusCode::PARTIAL_CONTENT, start, end),
        RangeAsked::Unsatisfiable => {
            let mut reply = empty_reply(StatusCode::RANGE_NOT_SATISFIABLE);
            if let Ok(value) = header::HeaderValue::from_str(&format!("bytes */{size}")) {
                reply.headers_mut().insert(header::CONTENT_RANGE, value);
            }
            return reply;
        }
    };
    let content_type = source
        .mimetype
        .clone()
        .unwrap_or_else(|| "application/octet-stream".to_string());
    // 資料照給，狀態碼說它可不可信：沒驗過或驗不過的傳統加密檔是 412、body 跟 200／206 一模一樣（維護者 2026-10-06，/docs/design/rpc-specs/data-plane.md §8.2）。
    let (trusted, kind, verified) = (source.is_trusted(), source.kind, source.verified);
    let body = match is_head || start == end {
        true => full_body(Bytes::new()),
        false => stream_body(source.into_stream(start, end)),
    };
    let mut reply = Response::new(body);
    *reply.status_mut() = match trusted {
        true => status,
        false => StatusCode::PRECONDITION_FAILED,
    };
    let headers = reply.headers_mut();
    headers.insert(
        MEDIA_KIND_HEADER,
        header::HeaderValue::from(u16::from(kind.to_number())),
    );
    headers.insert(
        MEDIA_VERIFIED_HEADER,
        header::HeaderValue::from(u16::from(verified.to_number())),
    );
    headers.insert(
        header::ACCEPT_RANGES,
        header::HeaderValue::from_static("bytes"),
    );
    headers.insert(
        header::CONTENT_LENGTH,
        header::HeaderValue::from(end - start),
    );
    if let Ok(value) = header::HeaderValue::from_str(&content_type) {
        headers.insert(header::CONTENT_TYPE, value);
    }
    // 型別是寄件者填的：前端可能是瀏覽器，`text/html`、`image/svg+xml` 的附件被當頁面打開時🚫 讓它跑腳本、🚫 猜型別。
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        header::HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        header::HeaderValue::from_static("sandbox"),
    );
    if status == StatusCode::PARTIAL_CONTENT {
        let range = format!("bytes {start}-{}/{size}", end.saturating_sub(1));
        if let Ok(value) = header::HeaderValue::from_str(&range) {
            headers.insert(header::CONTENT_RANGE, value);
        }
    }
    reply
}

/// `Range` 要的是哪一段。
#[derive(Debug, PartialEq, Eq)]
enum RangeAsked {
    /// 沒帶、寫壞了、或不只一段：整檔（RFC 9110 §14.2：認不得的 Range 可以不理）
    Whole,
    /// `[start, end)`
    Part(u64, u64),
    /// 起點在檔尾或之後：416
    Unsatisfiable,
}

/// Args:
///     value: `Range` header, example: Some("bytes=100-199")
///     size: 明文總長, example: 1000
/// Return:
///     RangeAsked   `bytes=a-b`、`bytes=a-`、`bytes=-n`（最後 n byte）三種寫法；終點超過檔尾就截到檔尾
fn parse_range(value: Option<&str>, size: u64) -> RangeAsked {
    let Some(spec) = value.and_then(|value| value.trim().strip_prefix("bytes=")) else {
        return RangeAsked::Whole;
    };
    if spec.contains(',') {
        return RangeAsked::Whole;
    }
    let Some((first, last)) = spec.trim().split_once('-') else {
        return RangeAsked::Whole;
    };
    let (first, last) = (first.trim(), last.trim());
    if first.is_empty() {
        // 最後 n byte。
        let Ok(suffix) = last.parse::<u64>() else {
            return RangeAsked::Whole;
        };
        if suffix == 0 || size == 0 {
            return RangeAsked::Unsatisfiable;
        }
        return RangeAsked::Part(size.saturating_sub(suffix), size);
    }
    let Ok(start) = first.parse::<u64>() else {
        return RangeAsked::Whole;
    };
    let end = match last {
        "" => size,
        text => match text.parse::<u64>() {
            Ok(last) if last >= start => last.saturating_add(1).min(size),
            _ => return RangeAsked::Whole,
        },
    };
    if start >= size {
        return RangeAsked::Unsatisfiable;
    }
    RangeAsked::Part(start, end)
}

/// 把 core 給的明文一段一段吐出去。拿不到下一段就讓 body 出錯：hyper 斷線，播放器知道沒收完（🚫 不假裝結束）。
fn stream_body(stream: MediaStream) -> ReplyBody {
    let frames = futures_util::stream::unfold(Some(stream), |state| async move {
        let mut stream = state?;
        match stream.next_piece().await {
            Ok(Some(piece)) => Some((Ok(Frame::data(Bytes::from(piece))), Some(stream))),
            Ok(None) => None,
            Err(error) => Some((Err(std::io::Error::other(error.message)), None)),
        }
    });
    StreamBody::new(frames).boxed_unsync()
}

fn full_body(bytes: Bytes) -> ReplyBody {
    Full::new(bytes)
        .map_err(|never| match never {})
        .boxed_unsync()
}

/// 一個 PUT：body 是明文，一條連線送到底；回應在 `Seal` 之後才到，body 是 manifest（/docs/design/rpc-specs/data-plane.md §4）。
async fn put_upload(
    handle: &Handle,
    core: &Core,
    meta: &UploadMeta,
    request: Request<Incoming>,
) -> Reply {
    let upload = &meta.upload;
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
        Box::pin(core.receive_upload(upload, &mut body, meta.source_uri.as_deref(), &target));
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
    let mut reply = Response::new(full_body(Bytes::from(json)));
    *reply.status_mut() = status;
    reply.headers_mut().insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("application/json"),
    );
    reply
}

fn empty_reply(status: StatusCode) -> Reply {
    let mut reply = Response::new(full_body(Bytes::new()));
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

    fn meta_of(upload: &UploadState) -> UploadMeta {
        UploadMeta {
            upload: upload.clone(),
            source_uri: Some("file:///home/me/v.bin".into()),
        }
    }

    /// URL 只帶「用途 ‖ mxc」、短而且看不出是哪個檔；meta 開得回整份上傳（含檔案金鑰）。每次封都不一樣（nonce 隨機），都能重複開——URL 可以重用。
    #[test]
    fn the_url_carries_only_the_mxc_and_the_meta_carries_the_upload() {
        let keys = AccessKeys::from_token(&[7u8; 256]);
        let original = upload(7, Some(40));
        let first = keys.to_upload_url_key(&original.mxc, true).unwrap();
        let second = keys.to_upload_url_key(&original.mxc, true).unwrap();
        assert!(
            first.starts_with("e-") && first.contains('_') && first.len() < 120,
            "{first}"
        );
        assert_ne!(first, second, "nonce 每次隨機");
        assert!(!first.contains("localhost"), "加密的 URL 看不出是哪個檔");
        for url_key in [&first, &first, &second] {
            assert_eq!(
                keys.open_upload_url_key(url_key, true),
                Some(original.mxc.clone())
            );
        }
        let meta = keys.to_upload_meta(&meta_of(&original), true).unwrap();
        assert!(meta.starts_with("e-"), "{meta}");
        assert_eq!(
            keys.open_upload_meta(&meta, &original.mxc, true),
            Some(meta_of(&original))
        );
        assert_eq!(
            keys.open_upload_meta(&meta, &original.mxc, true),
            Some(meta_of(&original))
        );
    }

    /// 一份 meta 只配得上它那個 URL：A 檔的 meta 配 B 檔的 URL 一律拒（加密模式靠 AAD，明文模式靠再對一次 mxc）。
    #[test]
    fn a_meta_of_another_upload_does_not_fit_this_url() {
        let keys = AccessKeys::from_token(&[7u8; 256]);
        let (a, b) = (upload(1, Some(40)), upload(2, Some(40)));
        let meta_of_a = keys.to_upload_meta(&meta_of(&a), true).unwrap();
        assert_eq!(keys.open_upload_meta(&meta_of_a, &b.mxc, true), None);
        let plain_meta_of_a = keys.to_upload_meta(&meta_of(&a), false).unwrap();
        assert_eq!(keys.open_upload_meta(&plain_meta_of_a, &b.mxc, false), None);
        assert_eq!(
            keys.open_upload_meta(&plain_meta_of_a, &a.mxc, false),
            Some(meta_of(&a))
        );
    }

    /// 加密的 meta 靠 AAD 綁住 mxc，不只靠解開後那次比對：一份「JSON 寫 B、但封的時候綁 A」的 meta 拿去配 B 的 URL 也開不了。
    /// （daemon 自己發的 meta 兩邊一定一樣，所以只有直接用 `seal` 才造得出這種東西——這條就是單獨驗 AAD 那一層。）
    #[test]
    fn the_meta_is_bound_to_its_mxc_by_the_aad_not_only_by_the_json() {
        let keys = AccessKeys::from_token(&[7u8; 256]);
        let (a, b) = (upload(1, Some(40)), upload(2, Some(40)));
        let json_of_b = serde_json::to_vec(&meta_of(&b)).unwrap();
        let forged = keys.to_text(&json_of_b, &meta_aad(&a.mxc), true).unwrap();
        assert_eq!(keys.open_upload_meta(&forged, &b.mxc, true), None);
    }

    /// 不是這個 daemon 發的一律不認（A5：不是正面認得就拒）：別的 token、改過一個字、截斷、亂寫、別的前綴、用途不對。
    #[test]
    fn what_this_daemon_did_not_issue_is_refused() {
        let keys = AccessKeys::from_token(&[7u8; 256]);
        let original = upload(7, Some(40));
        let url_key = keys.to_upload_url_key(&original.mxc, true).unwrap();
        let meta = keys.to_upload_meta(&meta_of(&original), true).unwrap();
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
        for junk in [
            "",
            "e-",
            "e-0OIl_abc",
            "e-abc",
            "e_abc",
            "x-abc",
            "../../etc/passwd",
            "e-111_222",
        ] {
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
        let wrong_purpose = format!("c-{}", bs58::encode(plain).into_string());
        assert_eq!(keys.open_upload_url_key(&wrong_purpose, false), None);
    }

    /// 明文模式（`c-`）只在 daemon 關掉加密時收；加密模式下拿 `c-` 來一律拒（fail closed）。
    #[test]
    fn plain_keys_are_taken_only_while_encryption_is_off() {
        let keys = AccessKeys::from_token(&[7u8; 256]);
        let original = upload(9, None);
        let url_key = keys.to_upload_url_key(&original.mxc, false).unwrap();
        let meta = keys.to_upload_meta(&meta_of(&original), false).unwrap();
        assert!(url_key.starts_with("c-") && meta.starts_with("c-"));
        assert_eq!(
            keys.open_upload_url_key(&url_key, false),
            Some(original.mxc.clone())
        );
        assert_eq!(keys.open_upload_url_key(&url_key, true), None);
        assert_eq!(
            keys.open_upload_meta(&meta, &original.mxc, false),
            Some(meta_of(&original))
        );
        assert_eq!(keys.open_upload_meta(&meta, &original.mxc, true), None);
        // 加密的那種兩個模式都收。
        let encrypted = keys.to_upload_url_key(&original.mxc, true).unwrap();
        assert_eq!(
            keys.open_upload_url_key(&encrypted, false),
            Some(original.mxc)
        );
    }

    /// 讀的 URL 與上傳的 URL 只差用途那個 byte：拿來做另一件事一律不收。
    #[test]
    fn a_read_url_and_an_upload_url_are_not_interchangeable() {
        let keys = AccessKeys::from_token(&[7u8; 256]);
        let mxc = "mxc://localhost/0001";
        for encrypted in [true, false] {
            let read = keys.to_media_url_key(mxc, encrypted).unwrap();
            let upload = keys.to_upload_url_key(mxc, encrypted).unwrap();
            assert_eq!(keys.open_media_url_key(&read, false).as_deref(), Some(mxc));
            assert_eq!(keys.open_upload_url_key(&read, false), None);
            assert_eq!(keys.open_media_url_key(&upload, false), None);
        }
        // 加密模式下，明文的讀 URL 一律不收。
        let plain = keys.to_media_url_key(mxc, false).unwrap();
        assert_eq!(keys.open_media_url_key(&plain, true), None);
    }

    #[test]
    fn a_range_is_one_span_clamped_to_the_file() {
        assert_eq!(parse_range(None, 1000), RangeAsked::Whole);
        assert_eq!(
            parse_range(Some("bytes=100-199"), 1000),
            RangeAsked::Part(100, 200)
        );
        assert_eq!(
            parse_range(Some("bytes=100-"), 1000),
            RangeAsked::Part(100, 1000)
        );
        assert_eq!(
            parse_range(Some("bytes=-10"), 1000),
            RangeAsked::Part(990, 1000)
        );
        assert_eq!(
            parse_range(Some("bytes=-5000"), 1000),
            RangeAsked::Part(0, 1000)
        );
        assert_eq!(
            parse_range(Some("bytes=900-5000"), 1000),
            RangeAsked::Part(900, 1000)
        );
        assert_eq!(
            parse_range(Some("bytes=999-999"), 1000),
            RangeAsked::Part(999, 1000)
        );
        assert_eq!(
            parse_range(Some("bytes=1000-"), 1000),
            RangeAsked::Unsatisfiable
        );
        assert_eq!(
            parse_range(Some("bytes=5000-6000"), 1000),
            RangeAsked::Unsatisfiable
        );
        assert_eq!(
            parse_range(Some("bytes=-0"), 1000),
            RangeAsked::Unsatisfiable
        );
        // 寫壞的、倒過來的、不只一段的、別的單位：不理，給整檔。
        for ignored in [
            "bytes=abc",
            "bytes=200-100",
            "bytes=0-1,5-6",
            "items=0-1",
            "bytes=",
            "bytes=-",
        ] {
            assert_eq!(
                parse_range(Some(ignored), 1000),
                RangeAsked::Whole,
                "{ignored}"
            );
        }
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

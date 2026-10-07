//! 傳統 Matrix 媒體（標準的 `url`／`file`，🚫 分塊）的 HTTP 下載與邊收邊解（/docs/design/media/media-download.md §12）。
//!
//! 一個 GET 從頭讀到尾。`kind` 2 用上游的 `AttachmentDecryptor` 解（🚫 自己刻 AES-CTR）：它邊讀邊解、讀到結尾才比密文的 SHA-256，
//! 正好是「邊下載邊交、讀完才知道對不對」要的時機。明文一段一段交給呼叫者的 channel（有界：呼叫者寫得慢，這裡就晚一點讀下一段）。
//! 🚫 整檔進記憶體（🚫 用 matrix-sdk 的 `get_media_content`，它回 `Vec<u8>`）。

use std::collections::VecDeque;
use std::io::Read;
use std::sync::Mutex;
use std::time::Duration;

use matrix_sdk_crypto::{AttachmentDecryptor, AttachmentEncryptor, MediaEncryptionInfo};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::sync::mpsc;

use crate::chat::MatrixAttachment;
use crate::error::SdkError;
use crate::manifest::{MatrixManifest, MatrixUpload};
use crate::media_kind::{MediaKind, Verification};

/// 解密、加密一次處理最多這麼多。
const PIECE: usize = 64 * 1024;
/// 上傳時在路上（加密好、還沒送出去）最多幾段。
const PIECES_IN_FLIGHT: usize = 4;
/// 連線建不起來就放棄。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// 下載讀完了：驗證結果與明文總長。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MatrixDownloadEnd {
    /// `kind` 2：密文 SHA-256 對上 `Matched`、對不上 `Mismatched`；`kind` 3：`Unknown`（沒有 hash 可比）
    pub verified: Verification,
    pub plain_len: u64,
}

/// 把一個標準 Matrix 附件下載下來、（`kind` 2）邊收邊解，明文一段一段送進 `sink`。
///
/// Args:
///     server: 這個帳號的 homeserver, example: "https://matrix.org"
///     access_token: 這個帳號的 token（只進 `Authorization` 標頭）
///     attachment: 事件裡的附件（mxc、`file`、`info.size`）
///     sink: 明文段的去處；收的那頭關了就停
/// Return:
///     Ok(MatrixDownloadEnd)   讀完了（`kind` 2 驗不過也是 Ok，結果在 `verified`：資料已經交出去、留不留由呼叫者決定）
///     Err(Usage)              不是標準附件（`kind` 1）、mxc 不合法、收的那頭關了（被取消）
///     Err(Integrity)          `file` 解不成上游的加密描述（版本、hash 不對）、或下載到的大小跟 `info.size` 不一樣
///     Err(Server)             server 拒絕（meta 帶 `status`、`errcode`）
///     Err(Network)            連不上、斷線、整條線一段時間沒有回應
pub async fn stream_matrix_media(
    server: &str,
    access_token: &str,
    attachment: &MatrixAttachment,
    sink: &mpsc::Sender<Vec<u8>>,
) -> Result<MatrixDownloadEnd, SdkError> {
    let (server_name, media_id) = split_mxc(&attachment.mxc)?;
    let client = media_client()?;
    let base = server.trim_end_matches('/');
    let mut response = fetch(
        &client,
        &format!("{base}/_matrix/client/v1/media/download/{server_name}/{media_id}"),
        access_token,
    )
    .await?;
    // 舊 server 沒有驗證過的端點：退到舊的那條（一樣帶 token，server 不要就忽略）。
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        let error = matrix_error_of(response).await;
        if error.matrix_errcode() != Some("M_UNRECOGNIZED") {
            return Err(error);
        }
        response = fetch(
            &client,
            &format!("{base}/_matrix/media/v3/download/{server_name}/{media_id}"),
            access_token,
        )
        .await?;
    }
    if !response.status().is_success() {
        return Err(matrix_error_of(response).await);
    }
    let end = match attachment.kind {
        MediaKind::MatrixEncrypted => {
            let file = attachment.file.clone().ok_or_else(|| {
                SdkError::Integrity(format!(
                    "{}: an encrypted attachment without file",
                    attachment.mxc
                ))
            })?;
            let info: MediaEncryptionInfo = serde_json::from_value(file).map_err(|error| {
                SdkError::Integrity(format!("{}: the file description: {error}", attachment.mxc))
            })?;
            stream_decrypted(&mut response, info, sink).await?
        }
        MediaKind::MatrixPlain => stream_plain(&mut response, sink).await?,
        MediaKind::WbfChunked => {
            return Err(SdkError::Usage(format!(
                "{} is a chunked file: it is downloaded over the Download line",
                attachment.mxc
            )))
        }
    };
    if let Some(size) = attachment.size {
        if size != end.plain_len {
            return Err(SdkError::Integrity(format!(
                "{}: downloaded {} bytes, the event says {size}",
                attachment.mxc, end.plain_len
            )));
        }
    }
    Ok(end)
}

/// 這個 server 願意收多大的檔（`m.upload.size`，/docs/design/rpc-specs/data-plane.md §7.2）。
///
/// Args:
///     server: example: "https://matrix.org"
/// Return:
///     Ok(Some(u64))   上限（byte）
///     Ok(None)        server 沒講（兩個端點都回了、但沒有這個欄位）
///     Err(Server)     server 拒絕（meta 帶 `status`、`errcode`）
///     Err(Network)
pub async fn get_upload_size_limit(
    server: &str,
    access_token: &str,
) -> Result<Option<u64>, SdkError> {
    let client = media_client()?;
    let base = server.trim_end_matches('/');
    let mut response = fetch(
        &client,
        &format!("{base}/_matrix/client/v1/media/config"),
        access_token,
    )
    .await?;
    // 舊 server 沒有驗證過的端點：退到舊的那條（跟下載同一個規則）。
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        let error = matrix_error_of(response).await;
        if error.matrix_errcode() != Some("M_UNRECOGNIZED") {
            return Err(error);
        }
        response = fetch(
            &client,
            &format!("{base}/_matrix/media/v3/config"),
            access_token,
        )
        .await?;
    }
    if !response.status().is_success() {
        return Err(matrix_error_of(response).await);
    }
    let config: serde_json::Value = response
        .json()
        .await
        .map_err(|error| SdkError::Network(format!("media config: {error}")))?;
    Ok(config.get("m.upload.size").and_then(|size| size.as_u64()))
}

/// 先跟 server 拿一個 mxc（`POST /_matrix/media/v1/create`）：`media.create` 要回 mxc，bytes 之後才到。
///
/// Return:
///     Ok(String)      example: "mxc://matrix.org/AbCdEf"
///     Err(Server)     server 拒絕、或沒有這個端點（太舊的 server：🚫 退到「傳完才知道 mxc」的 `POST /upload`）
///     Err(Integrity)  回的不是一個合法的 mxc
///     Err(Network)
pub async fn create_matrix_media(server: &str, access_token: &str) -> Result<String, SdkError> {
    let client = media_client()?;
    let response = client
        .post(format!(
            "{}/_matrix/media/v1/create",
            server.trim_end_matches('/')
        ))
        .bearer_auth(access_token)
        .json(&serde_json::json!({}))
        .send()
        .await
        .map_err(|error| SdkError::Network(format!("media create: {error}")))?;
    if !response.status().is_success() {
        return Err(matrix_error_of(response).await);
    }
    let created: serde_json::Value = response
        .json()
        .await
        .map_err(|error| SdkError::Network(format!("media create: {error}")))?;
    let mxc = created
        .get("content_uri")
        .and_then(|uri| uri.as_str())
        .unwrap_or("")
        .to_string();
    // 它要拼進 PUT 的路徑：消費端自己再問一次。
    split_mxc(&mxc).map_err(|_| SdkError::Integrity(format!("media create returned {mxc:?}")))?;
    Ok(mxc)
}

/// 把 PUT 進來的明文串流成 `PUT /_matrix/media/v3/upload/{server}/{media_id}` 的 body（/docs/design/rpc-specs/data-plane.md §7.2）：
/// `encrypted` 就邊收邊用上游的 `AttachmentEncryptor` 加密（key／IV 這次現產）、邊算密文的 SHA-256。
/// 記憶體裡只有幾段：上游送不出去就不讀 `body`。
///
/// Args:
///     upload: `media.create` 建的（mxc、大小、加不加密）
///     body: PUT 的 body
/// Return:
///     Ok(MatrixManifest)   `kind` 2 帶 `file`（含金鑰）、`kind` 3 沒有
///     Err(Usage)           body 比 `size` 短或長（上游那個請求中斷了，🚫 送出一個長度不對的檔）、mxc 不合法
///     Err(Io)              body 讀到一半斷了
///     Err(Server)          server 拒絕（例 `M_CANNOT_OVERWRITE_MEDIA`：那個 mxc 已經有內容；`M_NOT_FOUND`：預先拿的 mxc 過期了）
///     Err(Network)
pub async fn upload_matrix_media<R: AsyncRead + Unpin>(
    server: &str,
    access_token: &str,
    upload: &MatrixUpload,
    body: &mut R,
) -> Result<MatrixManifest, SdkError> {
    let (server_name, media_id) = split_mxc(&upload.mxc)?;
    let client = media_client()?;
    // 加密的檔在 server 上是一坨不透明的 bytes：🚫 讓 server 看到它本來是什麼型別。
    let content_type = match upload.encrypted {
        true => "application/octet-stream",
        false => upload
            .mimetype
            .as_deref()
            .unwrap_or("application/octet-stream"),
    };
    let (sink, pieces) = mpsc::channel(PIECES_IN_FLIGHT);
    let request = client
        .put(format!(
            "{}/_matrix/media/v3/upload/{server_name}/{media_id}",
            server.trim_end_matches('/')
        ))
        .bearer_auth(access_token)
        .header(reqwest::header::CONTENT_TYPE, content_type)
        // 串流的 body 預設是 chunked：規格要先講長度，而密文長度就是明文長度。
        .header(reqwest::header::CONTENT_LENGTH, upload.size)
        .body(reqwest::Body::wrap_stream(Pieces { receiver: pieces }))
        .send();
    let feeding = feed_upload_body(body, upload, sink);
    let (fed, response) = tokio::join!(feeding, request);
    // 先看是不是「body 長度不對」（是我們中斷了請求）、再看 server 說什麼、最後才是「上游不收了」。
    let encryption = match (fed, response) {
        (Err(error @ (SdkError::Usage(_) | SdkError::Io(_))), _) => return Err(error),
        (_, Err(error)) => return Err(SdkError::Network(format!("media upload: {error}"))),
        (_, Ok(response)) if !response.status().is_success() => {
            return Err(matrix_error_of(response).await)
        }
        (Err(error), Ok(_)) => return Err(error),
        (Ok(encryption), Ok(_)) => encryption,
    };
    let file = match encryption {
        Some(info) => {
            let mut file = serde_json::to_value(info).map_err(|error| {
                SdkError::Integrity(format!("{}: the file description: {error}", upload.mxc))
            })?;
            if let Some(fields) = file.as_object_mut() {
                fields.insert("url".into(), serde_json::Value::String(upload.mxc.clone()));
            }
            Some(file)
        }
        None => None,
    };
    Ok(MatrixManifest {
        server: upload.server.clone(),
        mxc: upload.mxc.clone(),
        kind: match upload.encrypted {
            true => MediaKind::MatrixEncrypted,
            false => MediaKind::MatrixPlain,
        },
        name: upload.name.clone(),
        mimetype: upload.mimetype.clone(),
        size: upload.size,
        file,
    })
}

/// 讀 PUT 的 body、（加密的話）加密、一段一段送進 `sink`。長度不對就送一個錯進去：reqwest 中斷那個請求（🚫 讓 server 收下一個長度不對的檔）。
///
/// Return:
///     Ok(Some(info))   加密的：`key`、`iv`、密文的 `hashes.sha256`
///     Ok(None)         明文
///     Err(Usage)       body 比 `size` 短或長
///     Err(Io)          body 讀到一半斷了
///     Err(Network)     上游不收了（那個請求已經結束，原因在它的回應裡）
async fn feed_upload_body<R: AsyncRead + Unpin>(
    body: &mut R,
    upload: &MatrixUpload,
    sink: mpsc::Sender<std::io::Result<Vec<u8>>>,
) -> Result<Option<MediaEncryptionInfo>, SdkError> {
    let queue = Mutex::new(VecDeque::new());
    let mut source = Queued { bytes: &queue };
    // 佇列空著叫它只回 0、不收尾（跟解密器不同）：只在佇列有東西時叫。key／IV 在這裡現產。
    let mut encryptor = upload
        .encrypted
        .then(|| AttachmentEncryptor::new(&mut source));
    let mut buffer = vec![0u8; PIECE];
    let mut sent = 0u64;
    let refuse = |message: String| SdkError::Usage(message);
    loop {
        let read = match body.read(&mut buffer).await {
            Ok(read) => read,
            Err(error) => {
                let _ = sink
                    .send(Err(std::io::Error::other("the body broke")))
                    .await;
                return Err(SdkError::Io(error));
            }
        };
        if read == 0 {
            break;
        }
        sent += read as u64;
        if sent > upload.size {
            let _ = sink
                .send(Err(std::io::Error::other("the body is too long")))
                .await;
            return Err(refuse(format!(
                "the body is longer than the size {} the upload was created with",
                upload.size
            )));
        }
        let plain = buffer.get(..read).unwrap_or_default();
        let piece = match encryptor.as_mut() {
            Some(encryptor) => {
                queue
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .extend(plain.iter());
                let mut cipher = vec![0u8; read];
                let mut filled = 0;
                while let Some(space) = cipher.get_mut(filled..).filter(|space| !space.is_empty()) {
                    let got = encryptor.read(space).map_err(SdkError::Io)?;
                    if got == 0 {
                        break;
                    }
                    filled += got;
                }
                cipher.truncate(filled);
                cipher
            }
            None => plain.to_vec(),
        };
        if sink.send(Ok(piece)).await.is_err() {
            return Err(SdkError::Network(
                "the server stopped reading the upload".into(),
            ));
        }
    }
    if sent < upload.size {
        let _ = sink
            .send(Err(std::io::Error::other("the body is too short")))
            .await;
        return Err(refuse(format!(
            "the body ended after {sent} bytes but the upload was created with size {}",
            upload.size
        )));
    }
    Ok(encryptor.map(AttachmentEncryptor::finish))
}

/// 上傳的 body：從有界 channel 拿下一段（channel 滿了、讀 PUT 那邊就停著等 —— 背壓）。
struct Pieces {
    receiver: mpsc::Receiver<std::io::Result<Vec<u8>>>,
}

impl futures_core::Stream for Pieces {
    type Item = std::io::Result<Vec<u8>>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        self.receiver.poll_recv(context)
    }
}

fn media_client() -> Result<reqwest::Client, SdkError> {
    reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        // 逾時看整條線多久沒回應（/docs/design/daemon/link-requests.md §4 同一個數），🚫 整個檔限時：大檔在慢網路上可以很久。
        .read_timeout(crate::link::LINE_SILENCE)
        .build()
        .map_err(|error| SdkError::Network(format!("media client: {error}")))
}

/// `mxc://<server_name>/<media_id>` → 兩段。只收規格允許的字元（server_name 是主機名＋埠、media_id 是 `[A-Za-z0-9_-]`）：
/// 它們要拼進 URL 的路徑，🚫 讓 `/`、`..`、`?` 混進去。
///
/// Return:
///     Ok((server_name, media_id))
///     Err(Usage)   不是 mxc、少一段、有不允許的字元
fn split_mxc(mxc: &str) -> Result<(&str, &str), SdkError> {
    let bad = || SdkError::Usage(format!("{mxc:?} is not a valid mxc URI"));
    let rest = mxc.strip_prefix("mxc://").ok_or_else(bad)?;
    let (server_name, media_id) = rest.split_once('/').ok_or_else(bad)?;
    let server_ok = !server_name.is_empty()
        && server_name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b".-:[]".contains(&byte));
    let media_ok = !media_id.is_empty()
        && media_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte));
    match server_ok && media_ok && !server_name.contains("..") {
        true => Ok((server_name, media_id)),
        false => Err(bad()),
    }
}

async fn fetch(
    client: &reqwest::Client,
    url: &str,
    access_token: &str,
) -> Result<reqwest::Response, SdkError> {
    client
        .get(url)
        .bearer_auth(access_token)
        .send()
        .await
        .map_err(|error| SdkError::Network(format!("media download: {error}")))
}

/// 非 2xx → `Server`，meta 帶 `status` 與（有的話）`errcode`，讓 `is_not_found`、`matrix_errcode` 認得出來。
async fn matrix_error_of(response: reqwest::Response) -> SdkError {
    let status = response.status().as_u16();
    let body = response.bytes().await.unwrap_or_default();
    let parsed: serde_json::Value =
        serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null);
    let errcode = parsed
        .get("errcode")
        .and_then(|value| value.as_str())
        .unwrap_or("")
        .to_string();
    let message = parsed
        .get("error")
        .and_then(|value| value.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| String::from_utf8_lossy(&body).into_owned());
    SdkError::Server {
        code: match errcode.is_empty() {
            true => format!("HTTP_{status}"),
            false => errcode.clone(),
        },
        message,
        meta: serde_json::json!({ "status": status, "errcode": errcode }),
        code_id: None,
    }
}

fn body_error(error: reqwest::Error) -> SdkError {
    SdkError::Network(format!("media download: {error}"))
}

async fn send(sink: &mpsc::Sender<Vec<u8>>, piece: Vec<u8>) -> Result<(), SdkError> {
    sink.send(piece)
        .await
        .map_err(|_| SdkError::Usage("the download was stopped".into()))
}

async fn stream_plain(
    response: &mut reqwest::Response,
    sink: &mpsc::Sender<Vec<u8>>,
) -> Result<MatrixDownloadEnd, SdkError> {
    let mut plain_len = 0u64;
    while let Some(chunk) = response.chunk().await.map_err(body_error)? {
        plain_len += chunk.len() as u64;
        send(sink, chunk.to_vec()).await?;
    }
    Ok(MatrixDownloadEnd {
        verified: Verification::Unknown,
        plain_len,
    })
}

/// 上游的解密器吃同步的 `Read`：這個 `Read` 從一個佇列拿，佇列由下載那邊補。
/// ⚠️ 佇列空的時候回 0 ＝ 解密器當成讀到結尾、拿去比 hash：所以**只在佇列有東西、或真的讀完時**才叫它。
struct Queued<'a> {
    bytes: &'a Mutex<VecDeque<u8>>,
}

impl Read for Queued<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let mut bytes = self
            .bytes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let take = buffer.len().min(bytes.len());
        for (slot, byte) in buffer.iter_mut().zip(bytes.drain(..take)) {
            *slot = byte;
        }
        Ok(take)
    }
}

async fn stream_decrypted(
    response: &mut reqwest::Response,
    info: MediaEncryptionInfo,
    sink: &mpsc::Sender<Vec<u8>>,
) -> Result<MatrixDownloadEnd, SdkError> {
    let queue = Mutex::new(VecDeque::new());
    let mut source = Queued { bytes: &queue };
    let mut decryptor = AttachmentDecryptor::new(&mut source, info)
        .map_err(|error| SdkError::Integrity(format!("the file description: {error}")))?;
    let mut plain = vec![0u8; PIECE];
    let mut plain_len = 0u64;
    let is_empty = |queue: &Mutex<VecDeque<u8>>| {
        queue
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_empty()
    };
    while let Some(chunk) = response.chunk().await.map_err(body_error)? {
        queue
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .extend(chunk.iter());
        while !is_empty(&queue) {
            let read = decryptor
                .read(&mut plain)
                .map_err(|error| SdkError::Integrity(format!("decrypting: {error}")))?;
            let piece = plain.get(..read).unwrap_or_default().to_vec();
            plain_len += piece.len() as u64;
            send(sink, piece).await?;
        }
    }
    // 真的讀完：佇列空著叫一次 = 結尾，解密器在這時比 hash（對不上回錯）。
    let verified = match decryptor.read(&mut plain) {
        Ok(_) => Verification::Matched,
        Err(_) => Verification::Mismatched,
    };
    Ok(MatrixDownloadEnd {
        verified,
        plain_len,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_well_formed_mxc_is_split() {
        assert_eq!(
            split_mxc("mxc://matrix.org/AbC_d-9").unwrap(),
            ("matrix.org", "AbC_d-9")
        );
        assert_eq!(
            split_mxc("mxc://localhost:6167/x").unwrap(),
            ("localhost:6167", "x")
        );
        for bad in [
            "https://matrix.org/x",
            "mxc://matrix.org",
            "mxc://matrix.org/",
            "mxc:///x",
            "mxc://matrix.org/a/b",
            "mxc://matrix.org/a?b",
            "mxc://../x",
            "mxc://matrix.org/%2e%2e",
        ] {
            assert!(split_mxc(bad).is_err(), "{bad}");
        }
    }
}

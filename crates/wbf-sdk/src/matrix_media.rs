//! 傳統 Matrix 媒體（標準的 `url`／`file`，🚫 分塊）的 HTTP 下載與邊收邊解（/docs/design/media/media-download.md §12）。
//!
//! 一個 GET 從頭讀到尾。`kind` 2 用上游的 `AttachmentDecryptor` 解（🚫 自己刻 AES-CTR）：它邊讀邊解、讀到結尾才比密文的 SHA-256，
//! 正好是「邊下載邊交、讀完才知道對不對」要的時機。明文一段一段交給呼叫者的 channel（有界：呼叫者寫得慢，這裡就晚一點讀下一段）。
//! 🚫 整檔進記憶體（🚫 用 matrix-sdk 的 `get_media_content`，它回 `Vec<u8>`）。

use std::collections::VecDeque;
use std::io::Read;
use std::sync::Mutex;
use std::time::Duration;

use matrix_sdk_crypto::{AttachmentDecryptor, MediaEncryptionInfo};
use tokio::sync::mpsc;

use crate::chat::MatrixAttachment;
use crate::error::SdkError;
use crate::media_kind::{MediaKind, Verification};

/// 解密一次交出去最多這麼多明文。
const PIECE: usize = 64 * 1024;
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
    let client = reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        // 逾時看整條線多久沒回應（/docs/design/daemon/link-requests.md §4 同一個數），🚫 整個檔限時：大檔在慢網路上可以很久。
        .read_timeout(crate::link::LINE_SILENCE)
        .build()
        .map_err(|error| SdkError::Network(format!("media client: {error}")))?;
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

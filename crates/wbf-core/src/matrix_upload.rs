//! 一般 Matrix 帳號的傳統上傳（/docs/design/rpc-specs/data-plane.md §7.2）：建檔（問上限、預先拿 mxc）、收 bytes（串流進 `/upload`，
//! 加密房邊收邊 AES-256-CTR）、把傳完的檔當標準附件送出。HTTP 與加密在 sdk 的 `matrix_media`；這裡管帳號、房間加不加密、
//! 「這是不是這個帳號的上傳」。wbf 帳號一律走分塊（`attachment_ops.rs`），🚫 走這條。

use tokio::io::AsyncRead;
use wbf_sdk::event_json::matrix_file_message_content;
use wbf_sdk::matrix_media::{create_matrix_media, get_upload_size_limit, upload_matrix_media};
use wbf_sdk::media_kind::MediaKind;
use wbf_sdk::{Cipher, MatrixManifest, MatrixUpload};

use crate::accounts::AccountDir;
use crate::attachment_ops::NewUpload;
use crate::error::{CoreError, CoreErrorKind};
use crate::{Core, Target};

impl Core {
    /// `media.create` 的一般 Matrix 帳號那半：先問 `m.upload.size`（超過就拒、🚫 讀任何 bytes），再 `/create` 預先拿 mxc。
    ///
    /// Args:
    ///     request: example: &NewUpload { room: Some("!r:matrix.org".into()), name: "cat.png".into(), size: Some(81234), ..Default::default() }
    /// Return:
    ///     Ok(MatrixUpload)   🚫 金鑰（PUT 時現產）
    ///     Err(Usage)         沒給 `size` 或是 0（`/upload` 要先講長度）、超過 server 的上限、`cipher` 不是 `none`、加密房要 `none`
    ///     Err(Server)／Err(Network)   問房間、問上限、`/create` 失敗
    pub(crate) async fn create_matrix_upload(
        &self,
        account: &AccountDir,
        request: &NewUpload,
        target: &Target,
    ) -> Result<MatrixUpload, CoreError> {
        let usage = |message: String| CoreError::new(CoreErrorKind::Usage, message);
        let size = match request.size {
            Some(0) => return Err(usage("size 0: there is nothing to upload".into())),
            Some(size) => size,
            None => {
                return Err(usage(
                    "a traditional upload (general Matrix server) needs its size up front: /upload sends Content-Length first".into(),
                ))
            }
        };
        // 這條路只有 AES-256-CTR v2：wbf 的演算法名字在這裡沒有意義（`chunk_size` 也是，忽略）。
        let wants_plain = match request.cipher.as_deref() {
            None => false,
            Some(name) if name == Cipher::None.name() => true,
            Some(other) => {
                return Err(usage(format!(
                    "cipher {other:?}: a general Matrix server takes only standard attachments (AES-256-CTR in an encrypted room, plain otherwise)"
                )))
            }
        };
        let encrypted = match &request.room {
            Some(room) => {
                let encrypted = self
                    .synced_backend_of(account, target.server_backup)
                    .await?
                    .is_room_encrypted(room)
                    .await?;
                if encrypted && wants_plain {
                    return Err(usage(format!(
                        "{room} is encrypted: cipher `none` would leave the file readable on the server"
                    )));
                }
                encrypted
            }
            // 沒給房間：不知道要送去哪，金鑰沒有地方安全地放，只能明文（要加密就給 `room`）。
            None => false,
        };
        let session = self.session_of(account)?;
        if let Some(limit) = get_upload_size_limit(&session.server, &session.access_token).await? {
            if size > limit {
                return Err(usage(format!(
                    "{size} bytes is over this server's upload limit of {limit} bytes (m.upload.size)"
                )));
            }
        }
        let mxc = create_matrix_media(&session.server, &session.access_token).await?;
        Ok(MatrixUpload {
            server: session.server,
            user_id: session.user_id,
            mxc,
            name: request.name.clone(),
            mimetype: request.mimetype.clone(),
            size,
            encrypted,
        })
    }

    /// PUT 的 body 串流進 `/upload`（加密房邊收邊加密，key／IV 這次現產），回 manifest。🚫 續傳：斷了就再 PUT 一次整個 body（新的 key／IV）。
    ///
    /// Args:
    ///     upload: `media.create` 建的那份
    ///     body: PUT 的 body
    /// Return:
    ///     Ok(MatrixManifest)   `kind` 2 帶 `file`（含金鑰）、`kind` 3 沒有
    ///     Err(Usage)           不是這個帳號的上傳、這個帳號是 wbf 帳號、body 比 `size` 短或長
    ///     Err(Io)              body 讀到一半斷了
    ///     Err(Server)          server 拒絕（那個 mxc 已經有內容、預先拿的 mxc 過期了）：重新 `media.create`
    ///     Err(Network)
    pub async fn receive_matrix_upload<R: AsyncRead + Unpin>(
        &self,
        upload: &MatrixUpload,
        body: &mut R,
        target: &Target,
    ) -> Result<MatrixManifest, CoreError> {
        let account = self.account_or_current(target)?;
        self.refuse_unless_matrix_account(&account)?;
        let session = self.session_of(&account)?;
        if !upload.is_for(&session.server, &session.user_id) {
            return Err(CoreError::new(
                CoreErrorKind::Usage,
                format!("the upload of {} belongs to another account", upload.mxc),
            ));
        }
        Ok(upload_matrix_media(&session.server, &session.access_token, upload, body).await?)
    }

    /// 傳完的檔當標準附件送進房間（`room.send_attachment`，加密房由 matrix-sdk 用 Megolm 加密）。送出前問這一刻的房間：
    /// 加密房只收 `kind` 2、明文房只收 `kind` 3（明文房的事件帶金鑰等於公開它）。
    ///
    /// Args:
    ///     manifest: PUT 回、UI 帶回來的那份（🚫 信它：組事件時再驗一次）
    ///     caption: example: Some("看這個")
    /// Return:
    ///     Ok(String)   event_id
    ///     Err(Usage)   這個帳號是 wbf 帳號、manifest 是別台 server 的、格式跟房間對不上、manifest 自相矛盾
    ///     Err(Server)／Err(Network)
    pub async fn send_matrix_attachment(
        &self,
        room: &str,
        manifest: &MatrixManifest,
        caption: Option<&str>,
        target: &Target,
    ) -> Result<String, CoreError> {
        let account = self.account_or_current(target)?;
        self.refuse_unless_matrix_account(&account)?;
        let session = self.session_of(&account)?;
        if manifest.server.trim_end_matches('/') != session.server.trim_end_matches('/') {
            return Err(CoreError::new(
                CoreErrorKind::Usage,
                format!(
                    "the manifest is for {} on {}, but this account is on {}",
                    manifest.mxc, manifest.server, session.server
                ),
            ));
        }
        let backend = self
            .synced_backend_of(&account, target.server_backup)
            .await?;
        let encrypted = backend.is_room_encrypted(room).await?;
        let fits_the_room = match manifest.kind {
            MediaKind::MatrixEncrypted => encrypted,
            MediaKind::MatrixPlain => !encrypted,
            MediaKind::WbfChunked => false,
        };
        if !fits_the_room {
            let why = match encrypted {
                true => "is encrypted but this upload is not",
                false => "is not encrypted, so the file key in the event would be public",
            };
            return Err(CoreError::new(
                CoreErrorKind::Usage,
                format!("{room} {why}: create the upload again for this room"),
            ));
        }
        let content = matrix_file_message_content(manifest, caption)?;
        Ok(backend.send_message_content(room, content).await?)
    }

    /// Return:
    ///     Ok(())       一般 Matrix 帳號
    ///     Err(Usage)   wbf 帳號（它走分塊，`attachment_ops.rs`）
    fn refuse_unless_matrix_account(&self, account: &AccountDir) -> Result<(), CoreError> {
        match self.is_wbf_account(account)? {
            false => Ok(()),
            true => Err(CoreError::new(
                CoreErrorKind::Usage,
                "this is a wbf account: its uploads go in wbf chunks, not the traditional way",
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use wbf_sdk::login::SessionBackend;
    use wbf_sdk::media_kind::MediaKind;
    use wbf_sdk::{MatrixManifest, MatrixUpload};

    use crate::attachment_ops::NewUpload;
    use crate::error::CoreErrorKind;
    use crate::test_support::*;
    use crate::{CreatedUpload, Target};

    /// 假 server 收到的（路徑、body），照順序。
    type Seen = Arc<Mutex<Vec<(String, Vec<u8>)>>>;

    /// 一台假的一般 Matrix server（只有媒體那幾支）：記下每個請求的（路徑、body）。`config` 回 `limit`、`create` 回固定的 mxc、PUT 照 Content-Length 收。
    async fn media_server(limit: u64) -> (String, Seen) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let seen_here = seen.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let seen = seen_here.clone();
                tokio::spawn(async move {
                    let mut head = Vec::new();
                    let mut byte = [0u8; 1];
                    while !head.ends_with(b"\r\n\r\n") {
                        if socket.read(&mut byte).await.unwrap_or(0) == 0 {
                            return;
                        }
                        head.push(byte[0]);
                    }
                    let head = String::from_utf8_lossy(&head).to_string();
                    let path = head.split(' ').nth(1).unwrap_or("").to_string();
                    let length: usize = head
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length: ")
                                .map(str::to_string)
                        })
                        .and_then(|value| value.trim().parse().ok())
                        .unwrap_or(0);
                    let mut body = vec![0u8; length];
                    let mut filled = 0;
                    while filled < length {
                        match socket.read(&mut body[filled..]).await {
                            Ok(0) | Err(_) => break,
                            Ok(got) => filled += got,
                        }
                    }
                    body.truncate(filled);
                    seen.lock().unwrap().push((path.clone(), body));
                    let json = if path.contains("/config") {
                        format!(r#"{{"m.upload.size":{limit}}}"#)
                    } else if path.contains("/create") {
                        r#"{"content_uri":"mxc://localhost/Made_1"}"#.to_string()
                    } else {
                        "{}".to_string()
                    };
                    let reply = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{json}",
                        json.len()
                    );
                    let _ = socket.write_all(reply.as_bytes()).await;
                    let _ = socket.shutdown().await;
                });
            }
        });
        (base, seen)
    }

    fn asked(size: Option<u64>) -> NewUpload {
        NewUpload {
            name: "report.pdf".into(),
            size,
            mimetype: Some("application/pdf".into()),
            ..Default::default()
        }
    }

    fn paths(seen: &Seen) -> Vec<String> {
        seen.lock()
            .unwrap()
            .iter()
            .map(|(path, _)| path.clone())
            .collect()
    }

    /// 一般 Matrix 帳號、沒給房間：先問上限、再 `/create` 拿 mxc；PUT 的 body 原樣進 `/upload`，回 `kind` 3 的 manifest。
    #[tokio::test]
    async fn a_general_account_uploads_the_traditional_way() {
        let (base, seen) = media_server(1_000_000).await;
        let (core, _account) =
            core_with_account_on(&scratch("mu-plain"), &base, SessionBackend::MatrixSdkClient)
                .await;
        let target = Target::default();
        let body: Vec<u8> = (0..50_000u32)
            .map(|position| (position % 251) as u8)
            .collect();
        let created = core
            .create_media_upload(&asked(Some(body.len() as u64)), &target)
            .await
            .unwrap();
        let CreatedUpload::Matrix(upload) = created else {
            panic!("{created:?}");
        };
        assert_eq!(
            (
                upload.mxc.as_str(),
                upload.size,
                upload.encrypted,
                upload.user_id.as_str()
            ),
            ("mxc://localhost/Made_1", body.len() as u64, false, ME)
        );
        assert_eq!(
            paths(&seen),
            vec![
                "/_matrix/client/v1/media/config".to_string(),
                "/_matrix/media/v1/create".to_string()
            ],
            "the limit is asked before the media id"
        );
        let manifest = core
            .receive_matrix_upload(&upload, &mut &body[..], &target)
            .await
            .unwrap();
        assert_eq!(
            (
                manifest.kind,
                manifest.mxc.as_str(),
                manifest.file.is_none()
            ),
            (MediaKind::MatrixPlain, "mxc://localhost/Made_1", true)
        );
        let put = seen.lock().unwrap().last().cloned().unwrap();
        assert_eq!(
            put,
            (
                "/_matrix/media/v3/upload/localhost/Made_1".to_string(),
                body
            )
        );
    }

    /// 超過 `m.upload.size`：拒、🚫 去拿 mxc。沒給大小、大小 0、wbf 的演算法名字：拒、🚫 碰網路。
    #[tokio::test]
    async fn an_upload_over_the_limit_or_without_a_size_is_refused_before_anything_is_made() {
        let (base, seen) = media_server(10_000).await;
        let (core, _account) = core_with_account_on(
            &scratch("mu-refuse"),
            &base,
            SessionBackend::MatrixSdkClient,
        )
        .await;
        let target = Target::default();
        let error = core
            .create_media_upload(&asked(Some(10_001)), &target)
            .await
            .unwrap_err();
        assert_eq!(error.kind, CoreErrorKind::Usage, "{error:?}");
        assert_eq!(
            paths(&seen),
            vec!["/_matrix/client/v1/media/config".to_string()],
            "no media id was made"
        );
        seen.lock().unwrap().clear();
        let gcm = NewUpload {
            cipher: Some("aes-256-gcm".into()),
            ..asked(Some(10))
        };
        for request in [asked(None), asked(Some(0)), gcm] {
            let error = core
                .create_media_upload(&request, &target)
                .await
                .unwrap_err();
            assert_eq!(error.kind, CoreErrorKind::Usage, "{request:?}: {error:?}");
        }
        assert!(paths(&seen).is_empty(), "refused before any request");
    }

    /// 別人的上傳、wbf 帳號拿傳統的上傳或 manifest、別台 server 的 manifest：拒、🚫 碰網路。
    #[tokio::test]
    async fn an_upload_or_manifest_that_is_not_this_accounts_is_refused() {
        let (base, seen) = media_server(1_000_000).await;
        let (core, _account) =
            core_with_account_on(&scratch("mu-other"), &base, SessionBackend::MatrixSdkClient)
                .await;
        let target = Target::default();
        let upload = MatrixUpload {
            server: base.clone(),
            user_id: "@mallory:localhost".into(),
            mxc: "mxc://localhost/Made_1".into(),
            name: "a.bin".into(),
            mimetype: None,
            size: 3,
            encrypted: false,
        };
        let error = core
            .receive_matrix_upload(&upload, &mut &b"abc"[..], &target)
            .await
            .unwrap_err();
        assert_eq!(error.kind, CoreErrorKind::Usage, "{error:?}");
        let manifest = MatrixManifest {
            server: "https://elsewhere.example".into(),
            mxc: "mxc://elsewhere.example/X".into(),
            kind: MediaKind::MatrixPlain,
            name: "a.bin".into(),
            mimetype: None,
            size: 3,
            file: None,
        };
        let error = core
            .send_matrix_attachment(ROOM, &manifest, None, &target)
            .await
            .unwrap_err();
        assert_eq!(error.kind, CoreErrorKind::Usage, "{error:?}");
        assert!(paths(&seen).is_empty(), "nothing reached the server");

        let (wbf, _account) = core_with_wbf_account(&scratch("mu-wbf")).await;
        let mine = MatrixUpload {
            server: DEAD.into(),
            user_id: ME.into(),
            ..upload
        };
        let error = wbf
            .receive_matrix_upload(&mine, &mut &b"abc"[..], &target)
            .await
            .unwrap_err();
        assert_eq!(
            error.kind,
            CoreErrorKind::Usage,
            "a wbf account uploads in chunks: {error:?}"
        );
        let error = wbf
            .send_matrix_attachment(
                ROOM,
                &MatrixManifest {
                    server: DEAD.into(),
                    ..manifest
                },
                None,
                &target,
            )
            .await
            .unwrap_err();
        assert_eq!(error.kind, CoreErrorKind::Usage, "{error:?}");
    }
}

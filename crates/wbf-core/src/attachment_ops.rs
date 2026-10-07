//! 資料平面的上傳（/docs/design/rpc-specs/data-plane.md）：建檔、收 bytes、把傳完的檔當附件送出——三步分開，
//! 前兩步是 UI 打 HTTP（`media.create` 拿 URL、PUT bytes 拿回 manifest），第三步是 UI 另外叫的 RPC（`room.send_attachment`）。
//!
//! core 不知道 HTTP：bytes 從一個 `AsyncRead` 進來；建好的上傳（[`UploadState`]）由呼叫端帶著（daemon 把它加密進 PUT 的 URL），
//! 每一步再交回來。每一步都自己核對「這個上傳是不是這個帳號的」，🚫 不靠呼叫端記得查。
//!
//! 這裡是 wbf 帳號的分塊上傳；一般 Matrix 帳號走傳統上傳（`/_matrix/media`，`matrix_upload.rs`）。`media.create` 用的
//! [`Core::create_media_upload`] 照帳號的種類分流、回 [`CreatedUpload`]，PUT 時原樣交回來。

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncReadExt};

use wbf_sdk::chat::Attachment;
use wbf_sdk::chunk_crypto::{
    choose_chunk_size, choose_stream_chunk_size, chunk_count, expected_plain_len, Link,
};
use wbf_sdk::error_code::WbfErrorCode;
use wbf_sdk::manifest::Manifest;
use wbf_sdk::{Cipher, FileCipher, MatrixUpload, SdkError, Transport, UploadState};

use crate::accounts::AccountDir;
use crate::backend_choice::MethodHome;
use crate::error::{CoreError, CoreErrorKind};
use crate::link_pool::{LinkRole, PooledClient};
use crate::room_crypto::SendOptions;
use crate::rooms_ops::cipher_for_plaintext_room;
use crate::upload_ops::parse_cipher;
use crate::{Core, Target};

/// `media.create` 要建的上傳（/docs/design/rpc-specs/rpc-spec.md §3.6）。
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
pub struct NewUpload {
    /// 要送去哪個房：給了就照房間決定加不加密（/docs/design/media/wbf-client-convention-for-chunk.md §5.1）；沒給是裸上傳，照 `cipher`
    #[serde(default)]
    pub room: Option<String>,
    /// 給對方看的檔名, example: "video.mkv"
    pub name: String,
    /// 明文總長；None ＝ 串流（事先不知道多長）, example: Some(2147483648)
    #[serde(default)]
    pub size: Option<u64>,
    #[serde(default)]
    pub mimetype: Option<String>,
    /// None 用預設；有 `room` 時要跟房間對得上
    #[serde(default)]
    pub cipher: Option<String>,
    #[serde(default)]
    pub chunk_size: Option<u32>,
}

/// `media.create` 建好的上傳，照帳號的種類（/docs/design/rpc-specs/data-plane.md §4、§7.2）。PUT 時原樣交回來；daemon 把它封進 `Wbf-Upload-Meta`。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "format", rename_all = "snake_case")]
pub enum CreatedUpload {
    /// wbf 帳號：分塊（含檔案金鑰）
    Chunked(UploadState),
    /// 一般 Matrix 帳號：傳統 `/upload`（🚫 金鑰：PUT 時現產）
    Matrix(MatrixUpload),
}

impl CreatedUpload {
    /// Return:
    ///     &str  這個上傳的 mxc, example: "mxc://localhost/000000000000004d"
    pub fn mxc(&self) -> &str {
        match self {
            CreatedUpload::Chunked(upload) => &upload.mxc,
            CreatedUpload::Matrix(upload) => &upload.mxc,
        }
    }

    /// Return:
    ///     &str  建它的帳號在哪台 server, example: "https://matrix.org"
    pub fn server(&self) -> &str {
        match self {
            CreatedUpload::Chunked(upload) => &upload.server,
            CreatedUpload::Matrix(upload) => &upload.server,
        }
    }

    /// Return:
    ///     &str  建它的帳號, example: "@alice:matrix.org"
    pub fn user_id(&self) -> &str {
        match self {
            CreatedUpload::Chunked(upload) => &upload.user_id,
            CreatedUpload::Matrix(upload) => &upload.user_id,
        }
    }

    /// Return:
    ///     Some(u64)  明文總長（傳統上傳一定有）
    ///     None       分塊的串流上傳（事先不知道多長）
    pub fn size(&self) -> Option<u64> {
        match self {
            CreatedUpload::Chunked(upload) => upload.block.file_size,
            CreatedUpload::Matrix(upload) => Some(upload.size),
        }
    }
}

impl Core {
    /// `media.create`：wbf 帳號建分塊上傳（[`Core::create_upload`]）、一般 Matrix 帳號建傳統上傳（`matrix_upload.rs`）。
    ///
    /// Args:
    ///     request: example: &NewUpload { room: Some("!r:localhost".into()), name: "v.mkv".into(), size: Some(2147483648), ..Default::default() }
    /// Return:
    ///     Ok(CreatedUpload)   `Chunked` 含檔案金鑰：🚫 不落地、不給前端
    ///     Err(...)            同 [`Core::create_upload`]；傳統上傳另見 `Core::create_matrix_upload`
    pub async fn create_media_upload(
        &self,
        request: &NewUpload,
        target: &Target,
    ) -> Result<CreatedUpload, CoreError> {
        let account = self.account_or_current(target)?;
        match self.is_wbf_account(&account)? {
            true => self
                .create_upload(request, target)
                .await
                .map(CreatedUpload::Chunked),
            false => self
                .create_matrix_upload(&account, request, target)
                .await
                .map(CreatedUpload::Matrix),
        }
    }

    /// 去 server 建一個上傳（`Upload/Create`），回的 [`UploadState`] 由呼叫端記著，PUT 進來時交回 [`Core::receive_upload`]。
    ///
    /// Args:
    ///     request: example: &NewUpload { room: Some("!r:localhost".into()), name: "v.mkv".into(), size: Some(2147483648), ..Default::default() }
    /// Return:
    ///     Ok(UploadState)   含檔案金鑰：🚫 不落地、不給前端
    ///     Err(Usage)        不是 wbf 帳號、`size` 是 0、`cipher` 認不得或跟房間對不上
    ///     Err(Server)／Err(Network)
    pub async fn create_upload(
        &self,
        request: &NewUpload,
        target: &Target,
    ) -> Result<UploadState, CoreError> {
        let account = self.account_or_current(target)?;
        self.refuse_unless_wbf_upload(&account)?;
        if request.size == Some(0) {
            return Err(CoreError::new(
                CoreErrorKind::Usage,
                "size 0: the protocol has no zero-chunk upload",
            ));
        }
        let cipher = match &request.room {
            Some(room) => {
                let encrypted = self.wbf_is_room_encrypted(&account, room).await?;
                cipher_for_room(encrypted, request.cipher.as_deref())?
            }
            None => parse_cipher(request.cipher.as_deref())?,
        };
        // 串流不知道網路是什麼：用行動網路那一檔（小塊，斷了少重送）。
        let chunk_size = request.chunk_size.unwrap_or_else(|| match request.size {
            Some(size) => choose_chunk_size(size),
            None => choose_stream_chunk_size(Link::MobileOrUnknown),
        });
        let file_cipher = FileCipher::generate(cipher, chunk_size).map_err(SdkError::from)?;
        let mut block = file_cipher.to_event_block(request.size.unwrap_or(0));
        // ⚠️ 串流是 `None` 不是 0：「空檔」與「還不知道多大」是兩件事。
        block.file_size = request.size;
        block.name = Some(request.name.clone());
        block.mimetype = request.mimetype.clone();
        let session = self.session_of(&account)?;
        let mut client = self.upload_link(&account).await?;
        Ok(client
            .create_upload(&session.server, &session.user_id, &file_cipher, &block)
            .await?)
    }

    /// 把 PUT 進來的明文分塊加密、逐塊上傳、`Seal`，回 manifest（/docs/design/rpc-specs/data-plane.md §4.2）。
    ///
    /// 一塊借一次上傳線：整個檔握著線的話，另一個檔的 `create_upload` 就得等這個傳完。
    /// 固定大小可以續傳：同一個上傳再 PUT 一次整個 body，server 收過的塊照讀（整檔 SHA-256 要它們）但不再送。串流不能續傳。
    ///
    /// 傳完之後，UI 有給原檔位置（`source_uri`）就記進這個 mxc 的 `media` 列（/docs/design/rpc-specs/data-plane.md §8.1）：
    /// 之後讀這個檔，原檔在、大小對得上就直接讀它。🚫 不檢查 URI 解不解得了——意義是 UI 定的，讀的時候才猜。
    /// 記不起來只發一則提醒、照樣回 manifest：檔已經在 server 上了，讓 PUT 失敗只會逼 UI 重傳一個已經 `Seal` 的檔。
    ///
    /// Args:
    ///     upload: `create_upload` 回的那份
    ///     body: PUT 的 body
    ///     source_uri: `media.create` 時 UI 給的, example: Some("file:///home/me/v.mkv")
    /// Return:
    ///     Ok(Manifest)   已 `Seal`；`block.sha256` 一定有
    ///     Err(Usage)     不是這個帳號的上傳、body 比 `size` 短或長、串流的 body 是空的
    ///     Err(Io)        body 讀到一半斷了
    ///     Err(Server)／Err(Network)
    pub async fn receive_upload<R: AsyncRead + Unpin>(
        &self,
        upload: &UploadState,
        body: &mut R,
        source_uri: Option<&str>,
        target: &Target,
    ) -> Result<Manifest, CoreError> {
        let account = self.account_or_current(target)?;
        self.refuse_unless_upload_of(&account, upload)?;
        let file_cipher = upload.file_cipher()?;
        let mut hasher = Sha256::new();
        let (file_size, truncated) = match upload.block.file_size {
            Some(size) => {
                self.send_sized_body(&account, upload, &file_cipher, size, body, &mut hasher)
                    .await?
            }
            None => {
                self.send_streamed_body(&account, upload, &file_cipher, body, &mut hasher)
                    .await?
            }
        };
        let mut final_block = upload.block.clone();
        final_block.file_size = Some(file_size);
        final_block.sha256 = Some(hex::encode(hasher.finalize()));
        let manifest = self
            .upload_link(&account)
            .await?
            .seal_upload(upload, &final_block)
            .await?;
        if truncated {
            // server 那份比原檔短：🚫 不記原檔，不然讀的人拿到的是 server 上沒有的後半段。
            self.events.progress(format!(
                "warning: the server truncated {} at its size limit",
                manifest.mxc
            ));
        } else if let Some(source_uri) = source_uri {
            // 走單一寫入者（server_cache.rs），🚫 不另開一條寫入連線。
            let (manifest_here, source_uri) = (manifest.clone(), source_uri.to_string());
            let remembered = match self.server_cache_and_me(&account) {
                Ok((cache, _)) => {
                    cache
                        .run(move |cache| cache.media_remember_source(&manifest_here, &source_uri))
                        .await
                }
                Err(error) => Err(error),
            };
            if let Err(error) = remembered {
                self.events.progress(format!(
                    "warning: {} is uploaded, but its local source could not be remembered: {error}",
                    manifest.mxc
                ));
            }
        }
        Ok(manifest)
    }

    /// 把一個**傳完的**檔當附件送進房間（`room.send_attachment`，/docs/design/rpc-specs/data-plane.md §5）。
    /// 附件在同一個送訊息請求裡宣告（/docs/design/media/wbf-client-convention-for-chunk.md §5.2）；加密房的事件是密文，區塊（含檔案金鑰）在密文裡。
    ///
    /// ⚠️ 要傳完：server 只認 `Seal` 過的媒體，宣告一個還在傳的 mxc 會被整則拒送。所以要的是 manifest（`Seal` 之後才有），🚫 不是建檔時那份區塊。
    /// 「上傳者是不是 sender」由 server 驗（不是就 `Conflict`）；這裡先擋掉「不是這台 server 的 manifest」。
    ///
    /// Args:
    ///     manifest: PUT 回的那份（或 `upload.file` 的）；事件用它的區塊
    ///     caption: example: Some("看這個")
    ///     options: 加密房要 `room_devices`；`txn_id` 重送用
    /// Return:
    ///     Ok(String)                 event_id
    ///     Err(Usage)                 不是 wbf 帳號；manifest 是別台 server 的；區塊不能進事件；區塊跟房間對不上（明文房帶金鑰、加密房不加密）；加密房沒帶 `room_devices`
    ///     Err(RoomDevicesChanged)    加密房被 1506 擋；帶新的 `room_devices`、同一份 manifest 與 `txn_id` 重送，檔案🚫 不必重傳
    ///     Err(Server)／Err(Network)
    pub async fn send_attachment(
        &self,
        room: &str,
        manifest: &Manifest,
        caption: Option<&str>,
        options: &SendOptions,
        target: &Target,
    ) -> Result<String, CoreError> {
        let account = self.account_or_current(target)?;
        self.refuse_unless_wbf_upload(&account)?;
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
        let encrypted = self.wbf_is_room_encrypted(&account, room).await?;
        refuse_block_not_matching_room(room, encrypted, manifest.block.cipher)?;
        let attachment = Attachment {
            mxc: manifest.mxc.clone(),
            block: manifest.block.clone(),
        };
        let content = wbf_sdk::event_json::file_message_content(&attachment, caption)?;
        self.wbf_send_message(
            &account,
            room,
            encrypted,
            content,
            vec![manifest.mxc.clone()],
            options,
        )
        .await
    }

    /// 固定大小：讀剛好 `size` byte，逐塊送；server 已經有的塊只讀不送。
    ///
    /// Return:
    ///     Ok((u64, bool))   (明文總長, server 有沒有截斷)
    async fn send_sized_body<R: AsyncRead + Unpin>(
        &self,
        account: &AccountDir,
        upload: &UploadState,
        file_cipher: &FileCipher,
        size: u64,
        body: &mut R,
        hasher: &mut Sha256,
    ) -> Result<(u64, bool), CoreError> {
        let chunk_size = upload.block.chunk_size;
        let total = chunk_count(size, chunk_size).ok_or_else(|| {
            CoreError::new(CoreErrorKind::Usage, "file too large for u32 chunk indices")
        })?;
        // 續傳：問 server 收到第幾塊、之前有沒有截斷過（新的上傳是 0／沒有）。
        let status = self
            .upload_link(account)
            .await?
            .upload_status(upload.upload_id)
            .await?;
        let mut next_wanted = status.received;
        // 累加不覆寫：上一輪在被跳過的區段截斷過，這一輪的 Ack 不會再說一次（PR #67 審查 cirno 🟢5b）。
        let mut truncated = status.truncated;
        let mut buffer = vec![0u8; chunk_size as usize];
        let mut read_so_far = 0u64;
        for index in 0..total {
            let plain_len = expected_plain_len(size, chunk_size, index).ok_or_else(|| {
                CoreError::new(
                    CoreErrorKind::Usage,
                    format!("chunk {index} is past the end ({total} chunks)"),
                )
            })?;
            let plain = buffer.get_mut(..plain_len).ok_or_else(|| {
                CoreError::new(
                    CoreErrorKind::Usage,
                    format!("chunk {index} wants {plain_len} bytes but chunk_size is {chunk_size}"),
                )
            })?;
            let got = read_up_to(body, plain).await?;
            read_so_far += got as u64;
            if got < plain_len {
                return Err(CoreError::new(
                    CoreErrorKind::Usage,
                    format!("the body ended after {read_so_far} bytes but the upload was created with size {size}"),
                ));
            }
            hasher.update(&*plain);
            if index < next_wanted {
                continue;
            }
            let sealed = file_cipher
                .seal_chunk(index, plain)
                .map_err(SdkError::from)?;
            let sent = self
                .upload_link(account)
                .await?
                .send_chunk(upload.upload_id, index, sealed, index + 1 == total)
                .await;
            match sent {
                Ok(ack) => {
                    truncated |= ack.truncated;
                    next_wanted = ack.received;
                }
                // server 已經有更後面的塊（上一次 PUT 的 Ack 沒收到）：往後讀就好。要的是前面的就沒救了——body 倒不回去。
                Err(error) => match expected_seq_of(&error) {
                    Some(expected) if expected > u64::from(index) => {
                        next_wanted = u32::try_from(expected).unwrap_or(u32::MAX);
                    }
                    _ => return Err(error.into()),
                },
            }
        }
        let mut one_more = [0u8; 1];
        if body.read(&mut one_more).await? != 0 {
            return Err(CoreError::new(
                CoreErrorKind::Usage,
                format!("the body is longer than the size {size} the upload was created with"),
            ));
        }
        Ok((size, truncated))
    }

    /// 串流：讀到 EOF，每滿一塊送一塊，最後一塊帶 `IS_LAST`（/docs/design/media/wbf-client-convention-for-chunk.md §6）。
    /// 🚫 不能續傳：server 已經收過塊的串流再 PUT 一次，新 body 跟舊的塊對不上（例如現錄的串流），拒絕。
    ///
    /// Return:
    ///     Ok((u64, bool))   (明文總長, server 有沒有截斷)
    async fn send_streamed_body<R: AsyncRead + Unpin>(
        &self,
        account: &AccountDir,
        upload: &UploadState,
        file_cipher: &FileCipher,
        body: &mut R,
        hasher: &mut Sha256,
    ) -> Result<(u64, bool), CoreError> {
        let already = self
            .upload_link(account)
            .await?
            .upload_status(upload.upload_id)
            .await?
            .received;
        if already != 0 {
            return Err(CoreError::new(
                CoreErrorKind::Usage,
                format!(
                    "{} is a streamed upload that already has {already} chunks on the server, and a stream cannot resume: \
                     call media.create again",
                    upload.mxc
                ),
            ));
        }
        let chunk_size = upload.block.chunk_size as usize;
        // 要知道「這是最後一塊」得先看下一塊有沒有東西，所以永遠留一塊在手上。
        let mut pending = read_chunk(body, chunk_size).await?;
        if pending.is_empty() {
            return Err(CoreError::new(
                CoreErrorKind::Usage,
                "empty body: the protocol has no zero-chunk upload",
            ));
        }
        let mut file_size = 0u64;
        let mut index = 0u32;
        loop {
            let next = if pending.len() < chunk_size {
                Vec::new()
            } else {
                read_chunk(body, chunk_size).await?
            };
            let is_last = next.is_empty();
            hasher.update(&pending);
            file_size += pending.len() as u64;
            let sealed = file_cipher
                .seal_chunk(index, &pending)
                .map_err(SdkError::from)?;
            let ack = self
                .upload_link(account)
                .await?
                .send_chunk(upload.upload_id, index, sealed, is_last)
                .await?;
            if is_last || ack.finished {
                return Ok((file_size, ack.truncated));
            }
            index = index.checked_add(1).ok_or_else(|| {
                CoreError::new(CoreErrorKind::Usage, "too many chunks for u32 indices")
            })?;
            pending = next;
        }
    }

    /// Return:
    ///     Ok(())       wbf 帳號
    ///     Err(Usage)   一般 Matrix 帳號（它走傳統上傳，`matrix_upload.rs`）
    ///     Err(NotLoggedIn)
    fn refuse_unless_wbf_upload(&self, account: &AccountDir) -> Result<(), CoreError> {
        if self.is_wbf_account(account)? {
            return Ok(());
        }
        Err(CoreError::new(
            CoreErrorKind::Usage,
            "this account is on a general Matrix server: its uploads go the traditional way (/_matrix/media), not in wbf chunks",
        ))
    }

    /// 這個上傳是這個帳號（這台 server、這個人）建的嗎？🚫 不是就不碰：塊會進到別人的上傳裡、事件會宣告別人的媒體。
    ///
    /// Return:
    ///     Ok(())       是
    ///     Err(Usage)   不是，或這個帳號不是 wbf 帳號
    fn refuse_unless_upload_of(
        &self,
        account: &AccountDir,
        upload: &UploadState,
    ) -> Result<(), CoreError> {
        self.refuse_unless_wbf_upload(account)?;
        let session = self.session_of(account)?;
        if upload.is_for(&session.server, &session.user_id) {
            return Ok(());
        }
        Err(CoreError::new(
            CoreErrorKind::Usage,
            format!("upload {} belongs to another account", upload.upload_id),
        ))
    }

    async fn upload_link(&self, account: &AccountDir) -> Result<PooledClient, CoreError> {
        self.client_of(
            account,
            Transport::WebSocket,
            MethodHome::WbfSdkOnly,
            LinkRole::Upload,
        )
        .await
    }
}

/// 送去某個房的上傳該用哪種加密（/docs/design/media/wbf-client-convention-for-chunk.md §5.1）。
///
/// Args:
///     encrypted: 這個房現在加密了嗎
///     requested: 前端指定的, example: Some("aes-256-gcm")
/// Return:
///     Ok(Cipher)   加密房：指定的或預設的（🚫 不能是 `none`）；明文房：`none`
///     Err(Usage)   認不得的名字；加密房要 `none`（檔案會以明文存在 server 上）；明文房要加密（金鑰會公開在事件裡）
fn cipher_for_room(encrypted: bool, requested: Option<&str>) -> Result<Cipher, CoreError> {
    if !encrypted {
        return cipher_for_plaintext_room(requested);
    }
    match parse_cipher(requested)? {
        Cipher::None => Err(CoreError::new(
            CoreErrorKind::Usage,
            "an encrypted room only takes an encrypted attachment: cipher `none` would leave the file readable on the server",
        )),
        cipher => Ok(cipher),
    }
}

/// 送出前再對一次：區塊的加密跟這一刻的房間對得上嗎（建檔到送出之間房間可能變了，/docs/design/media/wbf-client-convention-for-chunk.md §5.1）。
///
/// Return:
///     Ok(())       加密房配加密的區塊、明文房配 `none`
///     Err(Usage)   其他（🚫 不送：明文房會公開金鑰，加密房會送一個 server 讀得到的檔）
pub(crate) fn refuse_block_not_matching_room(
    room: &str,
    encrypted: bool,
    cipher: Cipher,
) -> Result<(), CoreError> {
    match (encrypted, cipher == Cipher::None) {
        (true, false) | (false, true) => Ok(()),
        (true, true) => Err(CoreError::new(
            CoreErrorKind::Usage,
            format!("{room} is encrypted but this upload is not: create the upload again for this room"),
        )),
        (false, false) => Err(CoreError::new(
            CoreErrorKind::Usage,
            format!(
                "{room} is not encrypted, so the file key in the event would be public: create the upload again for this room"
            ),
        )),
    }
}

/// `OutOfOrder` 的 `expected_seq`；其他錯是 None。🚨 認碼只看 `code_id`（issue #29 第 2 項）。
fn expected_seq_of(error: &SdkError) -> Option<u64> {
    if error.wbf_code() != Some(WbfErrorCode::OutOfOrder) {
        return None;
    }
    match error {
        SdkError::Server { meta, .. } => meta.get("expected_seq").and_then(|value| value.as_u64()),
        _ => None,
    }
}

/// 讀到填滿 `buffer` 或 EOF。
///
/// Return:
///     Ok(usize)   讀到幾 byte；小於 `buffer.len()` ＝ 碰到 EOF
async fn read_up_to<R: AsyncRead + Unpin>(
    body: &mut R,
    buffer: &mut [u8],
) -> Result<usize, CoreError> {
    let mut filled = 0;
    while let Some(space) = buffer.get_mut(filled..).filter(|space| !space.is_empty()) {
        let got = body.read(space).await?;
        if got == 0 {
            break;
        }
        filled += got;
    }
    Ok(filled)
}

/// 讀一塊（最多 `chunk_size`）；空的 ＝ EOF。
async fn read_chunk<R: AsyncRead + Unpin>(
    body: &mut R,
    chunk_size: usize,
) -> Result<Vec<u8>, CoreError> {
    let mut buffer = vec![0u8; chunk_size];
    let got = read_up_to(body, &mut buffer).await?;
    buffer.truncate(got);
    Ok(buffer)
}

#[cfg(test)]
mod tests {
    use sha2::{Digest, Sha256};

    use super::*;
    use crate::test_support::*;

    fn body(len: usize) -> Vec<u8> {
        (0..len).map(|position| (position % 251) as u8).collect()
    }

    fn new_upload(room: Option<&str>, size: Option<u64>) -> NewUpload {
        NewUpload {
            room: room.map(str::to_string),
            name: "v.bin".to_string(),
            size,
            chunk_size: Some(16),
            ..NewUpload::default()
        }
    }

    /// 假 server 收到的密文，用 manifest 的區塊解回來。
    fn decrypted(upload: &FakeServer, manifest: &Manifest) -> Vec<u8> {
        let uploads = upload.uploads.lock().unwrap();
        let stored = uploads
            .values()
            .find(|stored| stored.mxc == manifest.mxc)
            .unwrap();
        assert!(stored.sealed);
        let cipher = FileCipher::from_event_block(&manifest.block).unwrap();
        let size = manifest.file_size();
        let mut plain = Vec::new();
        for (index, sealed) in stored.chunks.iter().enumerate() {
            let len = expected_plain_len(size, manifest.block.chunk_size, index as u32).unwrap();
            plain.extend(cipher.open_chunk(index as u32, sealed, len).unwrap());
        }
        plain
    }

    /// 建檔、收 bytes，回 (建檔那份, manifest)。
    async fn uploaded(
        core: &Core,
        request: &NewUpload,
        bytes: &[u8],
        target: &Target,
    ) -> (UploadState, Manifest) {
        let state = core.create_upload(request, target).await.unwrap();
        let manifest = core
            .receive_upload(&state, &mut &bytes[..], None, target)
            .await
            .unwrap();
        (state, manifest)
    }

    /// /docs/design/rpc-specs/data-plane.md 的整條（加密房）：建檔就有 mxc、body 進來分塊加密上傳、Seal；server 上的塊用 manifest 的區塊解得回原檔，
    /// manifest 帶整檔 SHA-256。傳完之後送附件：是密文、同一個請求宣告那個 mxc。
    #[tokio::test]
    async fn an_encrypted_attachment_is_uploaded_then_sent_as_ciphertext_with_its_mxc_declared() {
        let dir = scratch("attach-encrypted");
        let (core, account) = core_with_wbf_account(&dir).await;
        let (misc, upload) = misc_and_upload(&core, &account, true).await;
        // refresh 走 `Keys` 線（/docs/design/keys/e2ee-rpc.md §3.1）；假 server 丟掉線就斷，留到測試結束。
        let _keys = keys_line_with_room(&core, &account, 7).await;
        let target = Target::default();
        let original = body(40);

        let state = core
            .create_upload(&new_upload(Some(ROOM), Some(40)), &target)
            .await
            .unwrap();
        assert_ne!(state.block.cipher, Cipher::None, "加密房的附件要加密");
        assert!(state.block.key.is_some());
        let manifest = core
            .receive_upload(&state, &mut &original[..], None, &target)
            .await
            .unwrap();
        assert_eq!(manifest.mxc, state.mxc);
        assert_eq!(manifest.block.file_size, Some(40));
        assert_eq!(
            manifest.block.sha256,
            Some(hex::encode(Sha256::digest(&original)))
        );
        assert_eq!(
            manifest.block.key, state.block.key,
            "事件裡那把就是檔案的鑰"
        );
        assert_eq!(decrypted(&upload, &manifest), original);

        let refused = core
            .send_attachment(ROOM, &manifest, None, &SendOptions::default(), &target)
            .await
            .unwrap_err();
        assert!(refused.message.contains("room_devices"), "{refused:?}");
        let devices = core
            .refresh_room_devices(ROOM, None, &target)
            .await
            .unwrap();
        let options = SendOptions {
            room_devices: Some(devices),
            txn_id: Some("t1".into()),
        };
        let event_id = core
            .send_attachment(ROOM, &manifest, Some("看這個"), &options, &target)
            .await
            .unwrap();
        assert_eq!(event_id, "$sent-1");
        assert_eq!(
            misc.sent_events.lock().unwrap()[0].1,
            "m.room.encrypted",
            "🚫 加密房不送明文事件"
        );
        assert_eq!(
            *misc.sent_attachments.lock().unwrap(),
            vec![vec![state.mxc.clone()]],
            "附件跟密文同一個請求宣告"
        );
    }

    /// 模式由房間決定（/docs/design/media/wbf-client-convention-for-chunk.md §5.1），兩頭都擋：建檔時、送出時（房間可能在中間變了）。
    #[tokio::test]
    async fn the_cipher_has_to_match_the_room_when_created_and_when_sent() {
        let dir = scratch("attach-mode");
        let (core, account) = core_with_wbf_account(&dir).await;
        let (misc, _upload) = misc_and_upload(&core, &account, false).await;
        let target = Target::default();
        let original = body(40);

        let mut asked = new_upload(Some(ROOM), Some(40));
        asked.cipher = Some("chacha20-poly1305".into());
        let refused = core.create_upload(&asked, &target).await.unwrap_err();
        assert_eq!(
            refused.kind,
            CoreErrorKind::Usage,
            "明文房的區塊帶金鑰＝金鑰公開"
        );

        let (_, plain_manifest) =
            uploaded(&core, &new_upload(Some(ROOM), Some(40)), &original, &target).await;
        assert_eq!(plain_manifest.block.cipher, Cipher::None);
        assert!(plain_manifest.block.key.is_none());

        misc.room_is_encrypted
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let refused = core
            .send_attachment(
                ROOM,
                &plain_manifest,
                None,
                &SendOptions::default(),
                &target,
            )
            .await
            .unwrap_err();
        assert_eq!(refused.kind, CoreErrorKind::Usage);
        assert!(
            refused
                .message
                .contains("is encrypted but this upload is not"),
            "{refused:?}"
        );
        let mut asked = new_upload(Some(ROOM), Some(40));
        asked.cipher = Some("none".into());
        let refused = core.create_upload(&asked, &target).await.unwrap_err();
        assert_eq!(
            refused.kind,
            CoreErrorKind::Usage,
            "加密房的附件🚫 不能是明文"
        );

        let (_, encrypted_manifest) =
            uploaded(&core, &new_upload(Some(ROOM), Some(40)), &original, &target).await;
        misc.room_is_encrypted
            .store(false, std::sync::atomic::Ordering::SeqCst);
        let refused = core
            .send_attachment(
                ROOM,
                &encrypted_manifest,
                None,
                &SendOptions::default(),
                &target,
            )
            .await
            .unwrap_err();
        assert!(refused.message.contains("would be public"), "{refused:?}");
        assert!(
            misc.sent_events.lock().unwrap().is_empty(),
            "擋下來的都沒送"
        );

        let event_id = core
            .send_attachment(
                ROOM,
                &plain_manifest,
                None,
                &SendOptions::default(),
                &target,
            )
            .await
            .unwrap();
        assert_eq!(event_id, "$sent-1");
        assert_eq!(misc.sent_events.lock().unwrap()[0].1, "m.room.message");
        assert_eq!(
            *misc.sent_attachments.lock().unwrap(),
            vec![vec![plain_manifest.mxc.clone()]],
            "明文房也一律宣告"
        );
    }

    /// body 要剛好是建檔時說的大小；斷了之後再 PUT 一次整個 body，server 收過的塊只讀不送（整檔雜湊照樣對）。
    #[tokio::test]
    async fn a_sized_body_must_match_and_a_second_put_resumes() {
        let dir = scratch("attach-resume");
        let (core, account) = core_with_wbf_account(&dir).await;
        let (_misc, upload) = misc_and_upload(&core, &account, true).await;
        let target = Target::default();
        let original = body(40);

        let state = core
            .create_upload(&new_upload(None, Some(40)), &target)
            .await
            .unwrap();
        let short = core
            .receive_upload(&state, &mut &original[..20], None, &target)
            .await
            .unwrap_err();
        assert_eq!(short.kind, CoreErrorKind::Usage);
        assert!(short.message.contains("ended after 20 bytes"), "{short:?}");
        assert_eq!(
            upload.uploads.lock().unwrap()[&1].chunks.len(),
            1,
            "滿的第一塊已經送了"
        );

        let manifest = core
            .receive_upload(&state, &mut &original[..], None, &target)
            .await
            .unwrap();
        assert_eq!(
            manifest.block.sha256,
            Some(hex::encode(Sha256::digest(&original)))
        );
        assert_eq!(decrypted(&upload, &manifest), original);

        let other = core
            .create_upload(&new_upload(None, Some(40)), &target)
            .await
            .unwrap();
        let long = body(41);
        let refused = core
            .receive_upload(&other, &mut &long[..], None, &target)
            .await
            .unwrap_err();
        assert!(
            refused.message.contains("longer than the size 40"),
            "{refused:?}"
        );
    }

    /// 串流（沒給大小）：讀到 EOF，manifest 帶真的大小，送得出去；空的 body 拒（協議沒有零塊的上傳）。
    #[tokio::test]
    async fn a_streamed_upload_learns_its_size_at_the_end() {
        let dir = scratch("attach-stream");
        let (core, account) = core_with_wbf_account(&dir).await;
        let (misc, upload) = misc_and_upload(&core, &account, false).await;
        let target = Target::default();

        let state = core
            .create_upload(&new_upload(Some(ROOM), None), &target)
            .await
            .unwrap();
        assert_eq!(state.block.file_size, None);
        let empty: &[u8] = &[];
        let refused = core
            .receive_upload(&state, &mut &empty[..], None, &target)
            .await
            .unwrap_err();
        assert_eq!(refused.kind, CoreErrorKind::Usage, "空的串流沒有零塊的上傳");

        let (_, manifest) =
            uploaded(&core, &new_upload(Some(ROOM), None), &body(40), &target).await;
        assert_eq!(manifest.block.file_size, Some(40));
        assert_eq!(decrypted(&upload, &manifest), body(40));
        core.send_attachment(ROOM, &manifest, None, &SendOptions::default(), &target)
            .await
            .unwrap();
        assert_eq!(misc.sent_events.lock().unwrap().len(), 1);
    }

    /// 別的帳號的上傳一律不碰：塊會進到別人的上傳裡（A6：消費端自己再問一次）；別台 server 的 manifest 不送（附件宣告會指著別台的媒體）。
    #[tokio::test]
    async fn an_upload_of_another_account_or_a_manifest_of_another_server_is_refused() {
        let dir = scratch("attach-owner");
        let (core, account) = core_with_wbf_account(&dir).await;
        let (misc, upload) = misc_and_upload(&core, &account, false).await;
        let target = Target::default();
        let original = body(40);

        let (_, mut manifest) =
            uploaded(&core, &new_upload(Some(ROOM), Some(40)), &original, &target).await;
        manifest.server = "http://elsewhere:6167".into();
        let refused = core
            .send_attachment(ROOM, &manifest, None, &SendOptions::default(), &target)
            .await
            .unwrap_err();
        assert!(
            refused.message.contains("but this account is on"),
            "{refused:?}"
        );

        let mut stolen = core
            .create_upload(&new_upload(Some(ROOM), Some(40)), &target)
            .await
            .unwrap();
        stolen.user_id = "@b:localhost".into();
        let refused = core
            .receive_upload(&stolen, &mut &original[..], None, &target)
            .await
            .unwrap_err();
        assert!(
            refused.message.contains("belongs to another account"),
            "{refused:?}"
        );
        assert!(upload.uploads.lock().unwrap()[&2].chunks.is_empty());
        assert!(misc.sent_events.lock().unwrap().is_empty());
    }

    /// UI 給的原檔位置在傳完時記進 `media` 列（/docs/design/rpc-specs/data-plane.md §8.1）；🚫 不檢查它解不解得了——那是讀的時候的事。沒給就不建列。
    #[tokio::test]
    async fn the_local_source_is_remembered_once_the_upload_is_sealed() {
        let dir = scratch("attach-source");
        let (core, account) = core_with_wbf_account(&dir).await;
        let (_misc, _upload) = misc_and_upload(&core, &account, false).await;
        let target = Target::default();
        let original = body(40);

        let state = core
            .create_upload(&new_upload(Some(ROOM), Some(40)), &target)
            .await
            .unwrap();
        let manifest = core
            .receive_upload(
                &state,
                &mut &original[..],
                Some("file:///home/me/v.bin"),
                &target,
            )
            .await
            .unwrap();
        let (cache, _) = core.cache_and_me(&account).unwrap();
        let entry = cache
            .find_media(&manifest.mxc)
            .unwrap()
            .expect("傳完就有列");
        assert_eq!(entry.source_uri.as_deref(), Some("file:///home/me/v.bin"));
        assert_eq!(entry.file_size, Some(40));
        assert_eq!(entry.name.as_deref(), Some("v.bin"));
        assert!(!entry.complete, "池裡還沒有：只是記下原檔在哪");

        let (_, without) =
            uploaded(&core, &new_upload(Some(ROOM), Some(40)), &original, &target).await;
        assert!(
            cache.find_media(&without.mxc).unwrap().is_none(),
            "沒給 source_uri 就不建列"
        );
    }

    /// server 截斷過的上傳🚫 不記原檔——包括截斷發生在上一輪、這一輪續傳時被跳過的那段（Ack 不會再說一次，要從 `Status` 讀；PR #67 審查 cirno 🟢5b）。
    #[tokio::test]
    async fn a_truncated_upload_does_not_remember_its_source_even_after_a_resume() {
        let dir = scratch("attach-truncated");
        let (core, account) = core_with_wbf_account(&dir).await;
        let (_misc, upload) = misc_and_upload(&core, &account, false).await;
        let target = Target::default();
        let original = body(40);

        let state = core
            .create_upload(&new_upload(Some(ROOM), Some(40)), &target)
            .await
            .unwrap();
        core.receive_upload(&state, &mut &original[..20], None, &target)
            .await
            .unwrap_err();
        upload
            .uploads
            .lock()
            .unwrap()
            .get_mut(&1)
            .unwrap()
            .truncated = true;
        let manifest = core
            .receive_upload(
                &state,
                &mut &original[..],
                Some("file:///home/me/v.bin"),
                &target,
            )
            .await
            .unwrap();
        let (cache, _) = core.cache_and_me(&account).unwrap();
        assert!(
            cache.find_media(&manifest.mxc).unwrap().is_none(),
            "截斷過就不記原檔"
        );
    }

    /// 串流不能續傳：server 已經收過塊的串流再 PUT，新 body 對不上舊的塊，拒絕（要重新 `create_upload`）。
    #[tokio::test]
    async fn a_streamed_upload_is_not_put_twice() {
        let dir = scratch("attach-stream-twice");
        let (core, account) = core_with_wbf_account(&dir).await;
        let (_misc, upload) = misc_and_upload(&core, &account, false).await;
        let target = Target::default();

        let (state, _) = uploaded(&core, &new_upload(None, None), &body(40), &target).await;
        let refused = core
            .receive_upload(&state, &mut &body(40)[..], None, &target)
            .await
            .unwrap_err();
        assert_eq!(refused.kind, CoreErrorKind::Usage);
        assert!(refused.message.contains("cannot resume"), "{refused:?}");
        assert_eq!(
            upload.uploads.lock().unwrap()[&1].chunks.len(),
            3,
            "🚫 沒有多送"
        );
    }
}

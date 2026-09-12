//! 上傳：分塊、續傳、封存成 manifest，以及「上傳完當成附件送進房間」。
//!
//! ⚠️ **續傳狀態檔寫在使用者那個檔案旁邊**（`<file>.wbf-upload.json`）。那個位置是
//! rpc-cli 時代定的（CLI 規格 §3.3），在 daemon 模型下**不一定對**——daemon 可能根本
//! 沒有那個目錄的寫入權（Android 的 SAF 給的是 `content://`，連路徑都沒有）。
//! 🚫 這一輪**照原樣搬，不在這裡發明新答案**；記成 architecture-v2 §8 的一條開著的項目。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use wbf_sdk::chat::{Attachment, ChatBackend};
use wbf_sdk::chunk_crypto::choose_chunk_size;
use wbf_sdk::manifest::Manifest;
use wbf_sdk::vault::write_private;
use wbf_sdk::{Cipher, Transport};
use wbf_sdk::{FileCipher, UploadState};

use crate::accounts::AccountDir;
use crate::error::{CoreError, CoreErrorKind};
use crate::{Core, Target};

/// 要上傳什麼、怎麼切、怎麼加密。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UploadRequest {
    pub file: PathBuf,
    /// `None` 用預設的加密法；明文房間只准 `none`（見 [`crate::cipher_for_plaintext_room`]）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cipher: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chunk_size: Option<u32>,
    /// 給對方看的檔名；`None` 就用路徑上的那個。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mimetype: Option<String>,
    /// 邊傳邊算整檔 SHA-256，收檔端可以核對。
    #[serde(default)]
    pub sha256: bool,
}

/// `send --file` 的結果。
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SendFileResult {
    pub event_id: String,
    pub mxc: String,
    /// ⚠️ 附件**有沒有向 server 宣告**（約定 §5.2）。現在一律是 `false`：
    /// matrix-sdk 的 `Room::send` 不能加 header、server 的 `Event/Send` 還是提案。
    /// 🚫 沒宣告的上傳過了保護期會被掃掉——這個欄位就是讓前端講得出這件事。
    pub attachment_declared: bool,
}

impl Core {
    /// 把一個檔案分塊上傳並封存，回一份 manifest。**支援續傳**。
    pub async fn upload_file(
        &self,
        request: &UploadRequest,
        transport: Transport,
        target: &Target,
    ) -> Result<Manifest, CoreError> {
        let account = self.account_or_current(target)?;
        self.upload_with_account(&account, request, transport).await
    }

    /// 上傳一個檔案，然後把它當附件送進房間。
    ///
    /// 🚫 **不做「這個房間沒加密，你確定嗎」的確認**：那是前端的事（§3）。前端要先問
    /// [`Core::conversation`]，再用 [`crate::cipher_for_plaintext_room`] 決定 `cipher`。
    pub async fn send_file(
        &self,
        room: &str,
        request: &UploadRequest,
        caption: Option<&str>,
        transport: Transport,
        target: &Target,
    ) -> Result<SendFileResult, CoreError> {
        let account = self.account_or_current(target)?;
        let backend = self
            .synced_backend_of(&account, target.server_backup)
            .await?;
        let manifest = self
            .upload_with_account(&account, request, transport)
            .await?;
        let attachment = Attachment {
            mxc: manifest.mxc.clone(),
            block: manifest.block.clone(),
        };
        // 約定 §5.2：附件宣告這一版帶不出去。⚠️ 講清楚，🚫 不裝作沒事——
        // 沒被引用的上傳過了 server 的保護期就會被掃掉。
        self.events.progress(format!(
            "attachment {} is NOT declared to the server (no Event/Send yet); an unreferenced upload is swept after the server's grace period",
            manifest.mxc
        ));
        let event_id = backend.send_file(room, &attachment, caption).await?;
        Ok(SendFileResult {
            event_id,
            mxc: manifest.mxc,
            attachment_declared: false,
        })
    }

    async fn upload_with_account(
        &self,
        account: &AccountDir,
        request: &UploadRequest,
        transport: Transport,
    ) -> Result<Manifest, CoreError> {
        let path = request.file.as_path();
        let mut source = std::fs::File::open(path)
            .map_err(|error| CoreError::new(CoreErrorKind::Io, format!("{error}")))?;
        let file_size = source
            .metadata()
            .map_err(|error| CoreError::new(CoreErrorKind::Io, format!("{error}")))?
            .len();
        let session = self.session_of(account)?;
        let mut client = self.client_of(account, transport).await?;
        let state_path = state_path_for(path);

        let (state, from_chunk) = match std::fs::read(&state_path) {
            Ok(bytes) => {
                let state = UploadState::from_json(&bytes)?;
                // ⚠️ 續傳狀態檔是**綁 server 與帳號**的：拿別人的那份接下去傳，塊會進到
                // 錯的上傳裡。兩個條件都要正面對上才准續。
                if !state.is_for(&session.server, &session.user_id) {
                    return Err(CoreError::new(
                        CoreErrorKind::Usage,
                        format!(
                            "{} belongs to another server or user; delete it or abort the upload",
                            state_path.display()
                        ),
                    ));
                }
                if state.block.file_size != Some(file_size) {
                    return Err(CoreError::new(
                        CoreErrorKind::Usage,
                        format!(
                            "{} was written for a file of another size; delete it or abort the upload",
                            state_path.display()
                        ),
                    ));
                }
                let status = client.upload_status(state.upload_id).await?;
                if status.finished {
                    // 上次在 Seal 前被殺：server 已經收齊，下面一塊也不會送，直接 Seal。
                    self.events
                        .progress("all chunks already on the server; sealing");
                } else {
                    self.events
                        .progress(format!("resume from chunk {}", status.received));
                }
                (state, status.received)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let cipher = parse_cipher(request.cipher.as_deref())?;
                let chunk_size = request
                    .chunk_size
                    .unwrap_or_else(|| choose_chunk_size(file_size));
                let file_cipher = FileCipher::generate(cipher, chunk_size);
                let mut block = file_cipher.to_event_block(file_size);
                block.name = Some(match &request.name {
                    Some(name) => name.clone(),
                    None => path
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned(),
                });
                block.mimetype = request.mimetype.clone();
                let state = client
                    .create_upload(&session.server, &session.user_id, &file_cipher, &block)
                    .await?;
                write_private(&state_path, &state.to_json())?;
                (state, 0)
            }
            Err(error) => return Err(CoreError::new(CoreErrorKind::Io, format!("{error}"))),
        };

        let summary = client
            .send_chunks(
                &state,
                &mut source,
                from_chunk,
                request.sha256,
                &mut |done, total| match total {
                    Some(total) => self.events.progress(format!("chunk {done}/{total}")),
                    None => self.events.progress(format!("chunk {done}")),
                },
            )
            .await?;
        let mut final_block = state.block.clone();
        final_block.sha256 = summary.sha256;
        let manifest = client.seal_upload(&state, &final_block).await?;
        remove_if_exists(&state_path)?;
        if summary.truncated {
            self.events
                .progress("warning: server truncated this upload at its size limit");
        }
        Ok(manifest)
    }
}

/// 續傳狀態檔就放在那個檔案旁邊（CLI 規格 §3.3）。
pub(crate) fn remove_resume_state(file: &Path) -> Result<(), CoreError> {
    remove_if_exists(&state_path_for(file))
}

fn state_path_for(file: &Path) -> PathBuf {
    let mut name = file.file_name().unwrap_or_default().to_os_string();
    name.push(".wbf-upload.json");
    file.with_file_name(name)
}

pub(crate) fn parse_cipher(name: Option<&str>) -> Result<Cipher, CoreError> {
    match name {
        None => Ok(Cipher::default_for_this_machine()),
        Some(name) => Cipher::from_name(name).ok_or_else(|| {
            CoreError::new(
                CoreErrorKind::Usage,
                format!("unknown cipher {name:?}: use chacha20-poly1305, aes-256-gcm or none"),
            )
        }),
    }
}

fn remove_if_exists(path: &Path) -> Result<(), CoreError> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(CoreError::new(CoreErrorKind::Io, format!("{error}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_resume_state_sits_next_to_the_file_it_is_for() {
        // ⚠️ 這個位置在 daemon 模型下不一定對（見模組註解），但現在的語意要釘住：
        // 一個檔案一份狀態，🚫 不是全域一份。
        assert_eq!(
            state_path_for(Path::new("/tmp/video.mkv")),
            PathBuf::from("/tmp/video.mkv.wbf-upload.json")
        );
    }

    #[test]
    fn an_unknown_cipher_is_refused_rather_than_silently_defaulted() {
        assert!(parse_cipher(None).is_ok());
        // 🚫 打錯的加密法名字不能落到預設值：那會讓人以為用了他指定的那個。
        assert_eq!(
            parse_cipher(Some("rot13")).unwrap_err().kind,
            CoreErrorKind::Usage
        );
    }
}

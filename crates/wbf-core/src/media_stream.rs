//! `GET /media/mxc/…` 的那一段明文從哪來（/docs/design/media/media-download.md §7.2、/docs/design/rpc-specs/data-plane.md §8）。
//!
//! 由上往下：本機原檔 → 完整的池檔 → （下載處理端：主檔已封的段 → seek 暫存檔 → 現拉）。後三種都交給那個帳號的下載處理端，
//! 一塊一塊地要（每一塊是一個 seek 的 job）：它是主檔與 seek 暫存檔唯一的寫入者，讀也經過它，🚫 不另開把手跟它搶。
//!
//! URL 不帶帳號（維護者 2026-10-01：「只要匹配 mxc 就能看」）：mxc 屬於哪份 `cache.db`，照本機已登入的帳號一份一份找，
//! mxc 的 server_name 跟帳號網域一樣的先找。現拉要金鑰：從那個帳號看得到的事件裡拿，🚫 不跨帳號借。

use std::io::{Read, Seek, SeekFrom};
use std::sync::Arc;

use wbf_sdk::local_source::open_local_source;
use wbf_sdk::manifest::Manifest;
use wbf_sdk::media;
use wbf_sdk::media_kind::{MediaKind, Verification};
use wbf_sdk::media_pool::PoolReader;

use crate::accounts::AccountDir;
use crate::download_queue::Downloader;
use crate::error::{CoreError, CoreErrorKind};
use crate::event::DownloadState;
use crate::matrix_download::MatrixTransfer;
use crate::Core;

/// 本機原檔與池檔一次讀這麼多。
const PIECE: usize = 64 * 1024;

/// 一個找得到的媒體：多大、什麼型別、從哪讀、可不可信。
pub struct MediaSource {
    pub mxc: String,
    /// 明文總長
    pub size: u64,
    pub mimetype: Option<String>,
    /// 格式與驗證結果（`media` 列的）：資料平面拿來決定狀態碼（/docs/design/rpc-specs/data-plane.md §8.2）
    pub kind: MediaKind,
    pub verified: Verification,
    origin: Origin,
}

enum Origin {
    /// 本機原檔（/docs/design/rpc-specs/data-plane.md §8.1）：🚫 不碰池、不觸發下載。
    Local(std::fs::File),
    /// 池裡完整的主檔。
    Pool(PoolReader),
    /// 還不完整：一塊一塊跟下載處理端要。
    Chunks {
        downloader: Arc<Downloader>,
        manifest: Arc<Manifest>,
    },
    /// 還在下載的傳統檔（/docs/design/media/media-download.md §12.2）：跟它的 task 要已經寫進主檔的，還沒寫到就等。
    Matrix(Arc<MatrixTransfer>),
}

/// 一段 Range 的明文，邊讀邊吐：記憶體裡同時最多一塊（或一段）。
pub struct MediaStream {
    origin: Origin,
    position: u64,
    end: u64,
}

impl MediaSource {
    /// 這份資料可不可信（/docs/design/rpc-specs/data-plane.md §8.2）：本機原檔是這台機器自己傳的，可信；
    /// 其他的只有「有 hash 可比、而加密本身擋不住竄改」的格式（`kind` 2）要驗過且正確。
    ///
    /// Return:
    ///     bool   false ＝ GET 回 412、body 照給
    pub fn is_trusted(&self) -> bool {
        matches!(self.origin, Origin::Local(_))
            || !self.kind.is_trust_gated_on_hash()
            || self.verified == Verification::Matched
    }

    /// Args:
    ///     start: 明文起點, example: 0
    ///     end: 明文終點（不含），呼叫者已經確定 `start < end <= size`, example: 1048576
    pub fn into_stream(self, start: u64, end: u64) -> MediaStream {
        MediaStream {
            origin: self.origin,
            position: start,
            end: end.min(self.size),
        }
    }
}

impl MediaStream {
    /// 下一段明文。
    ///
    /// Return:
    ///     Ok(Some(bytes))   下一段（非空）
    ///     Ok(None)          這個 Range 吐完了
    ///     Err(Network)      現拉時線斷了（HTTP 那邊就斷線）
    ///     Err(Integrity)    那一塊驗不過
    ///     Err(Io)           本機檔讀不了
    pub async fn next_piece(&mut self) -> Result<Option<Vec<u8>>, CoreError> {
        if self.position >= self.end {
            return Ok(None);
        }
        let want = (self.end - self.position).min(PIECE as u64) as usize;
        let piece = match &mut self.origin {
            Origin::Local(file) => read_at(file, self.position, want)?,
            Origin::Pool(reader) => read_at(reader, self.position, want)?,
            Origin::Chunks { .. } | Origin::Matrix(_) => {
                // 只交 byte 位置：是哪一塊、從哪裡開始，照處理端手上那個檔驗過的切法，🚫 照描述自己算。
                let piece = match &self.origin {
                    Origin::Chunks {
                        downloader,
                        manifest,
                    } => {
                        downloader
                            .read_piece_at(manifest.clone(), self.position)
                            .await?
                    }
                    Origin::Matrix(transfer) => transfer.read_at(self.position).await?,
                    Origin::Local(_) | Origin::Pool(_) => {
                        return Err(CoreError::new(
                            CoreErrorKind::Io,
                            "the media source changed while reading",
                        ))
                    }
                };
                // 消費端自己再核一次：回來的那一塊真的涵蓋這個位置。
                let covers = self
                    .position
                    .checked_sub(piece.start)
                    .filter(|from| *from < piece.plain.len() as u64);
                let Some(from) = covers else {
                    return Err(CoreError::new(
                        CoreErrorKind::Integrity,
                        format!(
                            "the chunk at {} ({} bytes) does not cover position {}",
                            piece.start,
                            piece.plain.len(),
                            self.position
                        ),
                    ));
                };
                let to = (self.end - piece.start).min(piece.plain.len() as u64);
                piece
                    .plain
                    .get(from as usize..to as usize)
                    .unwrap_or_default()
                    .to_vec()
            }
        };
        if piece.is_empty() {
            return Err(CoreError::new(
                CoreErrorKind::Io,
                format!("the source ended at {} before {}", self.position, self.end),
            ));
        }
        self.position += piece.len() as u64;
        Ok(Some(piece))
    }
}

fn read_at<R: Read + Seek>(
    source: &mut R,
    position: u64,
    want: usize,
) -> Result<Vec<u8>, CoreError> {
    source.seek(SeekFrom::Start(position))?;
    let mut piece = vec![0u8; want];
    let mut filled = 0;
    while filled < want {
        let Some(rest) = piece.get_mut(filled..) else {
            break;
        };
        let read = source.read(rest)?;
        if read == 0 {
            break;
        }
        filled += read;
    }
    piece.truncate(filled);
    Ok(piece)
}

impl Core {
    /// 找這個 mxc 在本機哪裡讀得到（`GET /media`）。
    ///
    /// Args:
    ///     mxc: 從 URL 開出來的, example: "mxc://localhost/000000000000004d"
    /// Return:
    ///     Ok(Some(MediaSource))   讀得到（原檔、池、或可以現拉）
    ///     Ok(None)                本機沒有任何帳號有這個 mxc 的紀錄（404）
    ///     Err(Usage)              有紀錄，但沒有完整的檔，而且沒有帳號看得到跟那一列一致的事件（沒金鑰、或描述都對不上；502）
    ///     Err(Locked)             還沒解鎖
    pub async fn find_media_source(&self, mxc: &str) -> Result<Option<MediaSource>, CoreError> {
        let accounts = self.list_accounts_for_media(mxc)?;
        let mut has_record = false;
        for account in &accounts {
            let Ok((cache, me)) = self.server_cache_and_me(account) else {
                continue;
            };
            let Some(entry) = cache.read().await.find_media(mxc)? else {
                continue;
            };
            has_record = true;
            let mimetype = entry.mimetype.clone();
            // 開得起來就是大小對得上列的 `file_size`（`open_local_source` 比過）。
            if let Some((file, size)) = open_local_source(&entry)
                .and_then(|(file, _)| entry.file_size.map(|size| (file, size)))
            {
                return Ok(Some(MediaSource {
                    mxc: mxc.to_string(),
                    size,
                    mimetype,
                    kind: entry.kind,
                    verified: entry.verified,
                    origin: Origin::Local(file),
                }));
            }
            let pool = self.pool_of(account)?;
            if let Some(reader) = media::open_complete(&pool, &entry) {
                let mxc_here = mxc.to_string();
                cache.post(
                    move |cache| cache.touch_media(&mxc_here).map(|_| ()),
                    Vec::new(),
                );
                return Ok(Some(MediaSource {
                    mxc: mxc.to_string(),
                    size: reader.plain_len(),
                    mimetype,
                    kind: entry.kind,
                    verified: entry.verified,
                    origin: Origin::Pool(reader),
                }));
            }
            // 現拉的金鑰只能從描述來，而只有 mxc 的 GET 照本地那一列挑（/docs/design/media/media-download.md §5.3）：
            // 這個帳號看到的都跟列不是同一個檔（寫錯或偽造的事件）就換下一個帳號。
            let key = media::find_key_matching_record(&*cache.read().await, &me, &entry)?;
            let block = match key {
                // 傳統格式的檔（`kind` 2、3，兩種帳號都有）：走 HTTP 的 task（/docs/design/media/media-download.md §12.2）。
                Some(media::RecordedFileKey::Matrix(attachment)) => {
                    match self.open_matrix_source(account, mxc, attachment).await? {
                        Some(source) => return Ok(Some(source)),
                        None => continue,
                    }
                }
                Some(media::RecordedFileKey::Chunked(block))
                    if self.is_wbf_account(account).unwrap_or(false) =>
                {
                    block
                }
                _ => continue,
            };
            let manifest = Arc::new(Manifest {
                server: self.session_of(account)?.server,
                mxc: mxc.to_string(),
                block,
            });
            let reader_account = self.find_writer_of(account, mxc, &accounts);
            self.ensure_download_link(&reader_account).await;
            let downloader = self.downloader_of(&reader_account).await?;
            // 分塊的列一定有大小（CHECK）；沒有就不是能一塊一塊拉的檔。
            let Some(size) = entry.file_size else {
                continue;
            };
            return Ok(Some(MediaSource {
                mxc: mxc.to_string(),
                size,
                mimetype: mimetype.or_else(|| manifest.block.mimetype.clone()),
                kind: entry.kind,
                verified: entry.verified,
                origin: Origin::Chunks {
                    downloader,
                    manifest,
                },
            }));
        }
        match has_record {
            true => Err(CoreError::new(
                CoreErrorKind::Usage,
                format!("{mxc} is not complete here, and no account here can see a key that matches the local record"),
            )),
            false => Ok(None),
        }
    }

    /// 傳統檔的來源：還在下載就跟它的 task 要（沒在下載就起一個）、已經完整就從池給。
    /// 事件沒給大小：`Content-Length` 與 Range 都要總長，所以等它下載完、從池給（/docs/design/media/media-download.md §12.2）。
    ///
    /// Return:
    ///     Ok(Some(MediaSource))   讀得到
    ///     Ok(None)                下載了卻還是不在池裡（下一個帳號再試）
    ///     Err(...)                下載失敗、被取消
    async fn open_matrix_source(
        &self,
        account: &AccountDir,
        mxc: &str,
        attachment: wbf_sdk::chat::MatrixAttachment,
    ) -> Result<Option<MediaSource>, CoreError> {
        let size = attachment.size;
        let mimetype = attachment.mimetype.clone();
        let kind = attachment.kind;
        let (done, wait) = tokio::sync::oneshot::channel();
        let waiter = size.is_none().then_some(done);
        let job = self
            .ensure_matrix_download(account, attachment, waiter)
            .await?;
        let transfer = self.matrix_transfers.find(&account.server_dir(), mxc);
        match (job.state, size, transfer) {
            (DownloadState::Downloading, Some(size), Some(transfer)) => Ok(Some(MediaSource {
                mxc: mxc.to_string(),
                size,
                mimetype,
                kind,
                verified: Verification::Unknown,
                origin: Origin::Matrix(transfer),
            })),
            (DownloadState::Downloading, None, _) => {
                wait.await.map_err(|_| {
                    CoreError::new(
                        CoreErrorKind::Io,
                        format!("the download of {mxc} stopped before it was complete"),
                    )
                })??;
                self.complete_pool_source(account, mxc).await
            }
            // 已經完整、或剛好在這一刻完成（task 已經不在登記表裡）：從池給。
            _ => self.complete_pool_source(account, mxc).await,
        }
    }

    /// 池裡完整的那份。
    async fn complete_pool_source(
        &self,
        account: &AccountDir,
        mxc: &str,
    ) -> Result<Option<MediaSource>, CoreError> {
        let (cache, _me) = self.server_cache_and_me(account)?;
        let Some(entry) = cache.read().await.find_media(mxc)? else {
            return Ok(None);
        };
        let pool = self.pool_of(account)?;
        Ok(
            media::open_complete(&pool, &entry).map(|reader| MediaSource {
                mxc: mxc.to_string(),
                size: reader.plain_len(),
                mimetype: entry.mimetype.clone(),
                kind: entry.kind,
                verified: entry.verified,
                origin: Origin::Pool(reader),
            }),
        )
    }

    /// 本機已登入的帳號，mxc 的 server_name 跟帳號網域一樣的排前面。
    fn list_accounts_for_media(&self, mxc: &str) -> Result<Vec<AccountDir>, CoreError> {
        let server_name = mxc
            .strip_prefix("mxc://")
            .and_then(|rest| rest.split('/').next())
            .unwrap_or("");
        let mut accounts: Vec<AccountDir> = self
            .refresh_data_dir_map()?
            .list_account_dirs()
            .into_iter()
            .filter(|account| account.is_logged_in())
            .collect();
        accounts.sort_by_key(|account| {
            let same_domain = self
                .session_of(account)
                .map(|session| session.user_id.rsplit(':').next() == Some(server_name))
                .unwrap_or(false);
            !same_domain
        });
        Ok(accounts)
    }

    /// 這台 server 上如果有處理端正在下載這個 mxc，seek 交給它（它有主檔與 `m<id>.seek` 可以讀，/docs/design/media/media-download.md §6.1）；沒有就用 `account`（只是 seek）。
    fn find_writer_of(
        &self,
        account: &AccountDir,
        mxc: &str,
        accounts: &[AccountDir],
    ) -> AccountDir {
        let Some(holder) = self.media_claims.find_holder(&account.server_dir(), mxc) else {
            return account.clone();
        };
        accounts
            .iter()
            .find(|candidate| {
                candidate.server_dir() == account.server_dir()
                    && self
                        .session_of(candidate)
                        .is_ok_and(|session| session.user_id == holder)
            })
            .cloned()
            .unwrap_or_else(|| account.clone())
    }
}

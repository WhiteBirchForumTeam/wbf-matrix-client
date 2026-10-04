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
use wbf_sdk::media_pool::PoolReader;

use crate::accounts::AccountDir;
use crate::download_queue::Downloader;
use crate::error::{CoreError, CoreErrorKind};
use crate::Core;

/// 本機原檔與池檔一次讀這麼多。
const PIECE: usize = 64 * 1024;

/// 一個找得到的媒體：多大、什麼型別、從哪讀。
pub struct MediaSource {
    pub mxc: String,
    /// 明文總長
    pub size: u64,
    pub mimetype: Option<String>,
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
}

/// 一段 Range 的明文，邊讀邊吐：記憶體裡同時最多一塊（或一段）。
pub struct MediaStream {
    origin: Origin,
    position: u64,
    end: u64,
}

impl MediaSource {
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
            Origin::Chunks {
                downloader,
                manifest,
            } => {
                // 只交 byte 位置：是哪一塊、從哪裡開始，照處理端手上那個檔驗過的切法，🚫 照描述自己算。
                let piece = downloader
                    .read_piece_at(manifest.clone(), self.position)
                    .await?;
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
    ///     Err(Usage)              有紀錄，但沒有完整的檔、而且看得到它的帳號都沒有金鑰（502）
    ///     Err(Integrity)          有紀錄、沒有完整的檔，帳號看得到的描述跟那一列都對不上（502）
    ///     Err(Locked)             還沒解鎖
    pub async fn find_media_source(&self, mxc: &str) -> Result<Option<MediaSource>, CoreError> {
        let accounts = self.list_accounts_for_media(mxc)?;
        let mut has_record = false;
        let mut description_mismatched = false;
        for account in &accounts {
            let Ok((cache, me)) = self.server_cache_and_me(account) else {
                continue;
            };
            let Some(entry) = cache.read().await.find_media(mxc)? else {
                continue;
            };
            has_record = true;
            let mimetype = entry.mimetype.clone();
            if let Some((file, _)) = open_local_source(&entry) {
                return Ok(Some(MediaSource {
                    mxc: mxc.to_string(),
                    size: entry.file_size,
                    mimetype,
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
                    size: entry.file_size,
                    mimetype,
                    origin: Origin::Pool(reader),
                }));
            }
            if !self.is_wbf_account(account).unwrap_or(false) {
                continue;
            }
            let Some(block) = cache.read().await.find_media_block_for(&me, mxc)? else {
                continue;
            };
            let manifest = Arc::new(Manifest {
                server: self.session_of(account)?.server,
                mxc: mxc.to_string(),
                block,
            });
            // 現拉的金鑰只能從描述來：這個帳號看到的那則跟本地那一列不是同一個檔（寫錯或偽造的事件）就換下一個帳號。
            // 切片🚫 用它（處理端照驗過的切法算），只是 seek 也🚫 寫檔，所以這裡擋的是「拿一把不可信的金鑰去拉」。
            if !media::is_same_file(&entry, &manifest) {
                description_mismatched = true;
                continue;
            }
            let reader_account = self.find_writer_of(account, mxc, &accounts);
            self.ensure_download_link(&reader_account).await;
            let downloader = self.downloader_of(&reader_account).await?;
            return Ok(Some(MediaSource {
                mxc: mxc.to_string(),
                size: entry.file_size,
                mimetype: mimetype.or_else(|| manifest.block.mimetype.clone()),
                origin: Origin::Chunks {
                    downloader,
                    manifest,
                },
            }));
        }
        if description_mismatched {
            return Err(CoreError::new(
                CoreErrorKind::Integrity,
                format!("{mxc} is not complete here, and no account here has a description that matches the local record"),
            ));
        }
        match has_record {
            true => Err(CoreError::new(
                CoreErrorKind::Usage,
                format!("{mxc} is not in the local pool and no account here can see the key to fetch it"),
            )),
            false => Ok(None),
        }
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

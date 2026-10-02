//! 媒體池的對外操作：下載排隊（`media.download`／`media.open`／`media.queue`／`media.cancel`）、另存新檔、狀態、清理。
//! 設計在 /docs/design/media/media-download.md（下載）與 /docs/design/media/media-pool.md（池）。
//!
//! ⚠️ **池裡的東西是加密的**（/docs/design/media/media-pool.md §1）。所以「給前端一個路徑」是錯的：它讀到的是密文，
//! 而 core 先解密寫到某個路徑就是**明文落地**——整個加密池的意義就沒了（/docs/design/rpc-specs/local-interface.md §8）。
//! 播放與顯示走資料平面的 URL（`media.open` → `GET /media`，`media_stream.rs`）；這裡唯一的明文落地是使用者明說要放到自己選的位置（[`Core::save_media_to`]）。
//!
//! 下載本身由每帳號一個的下載處理端做（`download_queue.rs`）：這裡只負責「要哪個檔、本地已經有了沒、排進去、等它」。

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;

use serde::Serialize;

use wbf_sdk::local_source::open_local_source;
use wbf_sdk::manifest::Manifest;
use wbf_sdk::media;
use wbf_sdk::Transport;

use crate::accounts::AccountDir;
use crate::backend_choice::MethodHome;
use crate::download_queue::{cancelled_error, Downloader, DownloaderParts};
use crate::error::{CoreError, CoreErrorKind};
use crate::event::DownloadState;
use crate::link_pool::LinkRole;
use crate::{Core, Target};

/// `media-stats`。
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct MediaStats {
    pub pool_dir: String,
    pub bytes_on_disk: u64,
    pub complete_files: usize,
    pub incomplete_files: usize,
    pub pending_on_disk: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub oldest_last_used_at: Option<i64>,
}

/// `media-gc`：配額清理加孤兒掃除，兩件事一起報。
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct MediaGcReport {
    pub bytes_before: u64,
    pub bytes_after: u64,
    pub files_removed: u64,
    /// 該刪但還有把手開著、這一輪跳過的檔數（/docs/design/media/media-pool.md §5）。
    pub files_in_use: u64,
    /// ⚠️ `true` 表示**清完還是超過配額**：剩下的都在保護期內，沒有東西可以再刪。
    pub still_over_quota: bool,
    pub swept_missing_files: u64,
    pub swept_pending: u64,
    pub swept_orphan_files: u64,
}

/// `--no-cache` 的 CLI 下載結果（沒進池，所以沒有 `pool_file`／`source`）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DirectDownloadResult {
    pub out: String,
    pub bytes: u64,
    pub chunks: u32,
    pub sha256_verified: bool,
}

/// 要哪個檔（/docs/design/media/media-download.md §7.1）：三種說法，金鑰從哪來不同（§3.2）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MediaRef {
    /// 只給 mxc：金鑰從這個帳號看得到、引用它的事件裡找。
    Mxc(String),
    /// 某一則訊息的附件。
    Event { room: String, event_id: String },
    /// 直接給 manifest（含金鑰）。
    Manifest(Manifest),
}

/// `media.download` 的回答：這個檔現在的樣子。
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct MediaJob {
    pub mxc: String,
    pub state: DownloadState,
    /// 已落地的塊數（`complete`／`local_source` 時等於 `total`）
    pub done: u32,
    /// 總塊數；還不知道是 0
    pub total: u32,
}

/// `media.open` 的回答（URL 由 daemon 用共享 token 鑄，core 不知道）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct OpenedMedia {
    pub mxc: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mimetype: Option<String>,
    /// 明文總長
    pub size: u64,
    pub state: DownloadState,
}

/// `media.queue` 的一項。
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct QueuedMedia {
    pub mxc: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub state: DownloadState,
    pub done: u32,
    pub total: u32,
}

/// `media.save_to` 的結果。
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SavedMedia {
    pub out: String,
    pub bytes: u64,
    /// `"local_source"`（本機原檔）、`"cache"`（池裡本來就有）、`"server"`（這次排隊下載的）
    pub source: String,
    /// 快取列記的校驗碼（`sha256:…` 或 `blake3:…`）；本機原檔沒有
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hash: Option<String>,
}

impl Core {
    /// 媒體池現在多大、有幾個半成品。
    pub async fn media_stats(&self, target: &Target) -> Result<MediaStats, CoreError> {
        let account = self.account_or_current(target)?;
        let (cache, _me) = self.server_cache_and_me(&account)?;
        let pool = self.pool_of(&account)?;
        let reader = cache.read().await;
        let complete = reader.list_media_by_last_used()?;
        let incomplete = reader.list_media_incomplete()?;
        Ok(MediaStats {
            pool_dir: pool.dir().display().to_string(),
            bytes_on_disk: reader.media_bytes_on_disk()?,
            complete_files: complete.len(),
            incomplete_files: incomplete.len(),
            pending_on_disk: pool.list_pending()?.len(),
            oldest_last_used_at: complete.first().map(|entry| entry.last_used_at),
        })
    }

    /// 超過配額就從最久沒用的刪；保護期內的不刪。順便掃孤兒（/docs/design/media/media-pool.md §5）。
    ///
    /// Args:
    ///     quota_mib: example: 2048
    ///     protect_days: 保護期，這幾天內用過的一律不刪, example: 7
    pub async fn collect_media_garbage(
        &self,
        quota_mib: u64,
        protect_days: u64,
        target: &Target,
    ) -> Result<MediaGcReport, CoreError> {
        let account = self.account_or_current(target)?;
        let (cache, _me) = self.server_cache_and_me(&account)?;
        let pool = self.pool_of(&account)?;
        let protect = std::time::Duration::from_secs(protect_days * 24 * 3600);
        let in_use = self.list_media_in_use(&account.server_dir());
        let (swept, report) = cache
            .run(move |cache| {
                let swept =
                    media::sweep(cache, &pool, protect, std::time::SystemTime::now(), &in_use)?;
                let report = media::collect_garbage(
                    cache,
                    &pool,
                    quota_mib * 1024 * 1024,
                    protect,
                    now_millis(),
                )?;
                Ok((swept, report))
            })
            .await?;
        if report.still_over_quota {
            self.events.progress(format!(
                "media cache is still over quota ({} bytes > {quota_mib} MiB); everything left is inside the {protect_days}-day protection window",
                report.bytes_after
            ));
        }
        Ok(MediaGcReport {
            bytes_before: report.bytes_before,
            bytes_after: report.bytes_after,
            files_removed: report.files_removed,
            files_in_use: report.files_in_use,
            still_over_quota: report.still_over_quota,
            swept_missing_files: swept.reset_rows,
            swept_pending: swept.removed_pending,
            swept_orphan_files: swept.removed_orphan_files,
        })
    }

    /// 排一個檔進這個帳號的下載佇列（/docs/design/media/media-download.md §5.3）。已經完整、或本機有原檔就不排。
    ///
    /// Args:
    ///     media: example: &MediaRef::Mxc("mxc://localhost/000000000000004d".into())
    /// Return:
    ///     Ok(MediaJob)     `complete`／`local_source`（不排）、`queued`、`downloading`
    ///     Err(Usage)       找不到金鑰（這個帳號看不到引用它的事件）、manifest 是別台 server 的、一般 Matrix 帳號（傳統下載還沒接）
    pub async fn media_download(
        &self,
        media: &MediaRef,
        target: &Target,
    ) -> Result<MediaJob, CoreError> {
        let account = self.account_or_current(target)?;
        let manifest = self.resolve_media(&account, media).await?;
        self.ensure_queued(&account, manifest, None).await
    }

    /// `media.open`：這個檔能不能開來讀、多大、什麼型別；不完整也沒原檔就順便排進佇列（/docs/design/media/media-download.md §7.1）。
    ///
    /// Return:
    ///     Ok(OpenedMedia)  `state` 是 `local_source`、`complete`、`downloading`、`queued`
    ///     Err(Usage)       同 [`Core::media_download`]
    pub async fn media_open(
        &self,
        media: &MediaRef,
        target: &Target,
    ) -> Result<OpenedMedia, CoreError> {
        let account = self.account_or_current(target)?;
        let manifest = self.resolve_media(&account, media).await?;
        let (mxc, mimetype, size) = (
            manifest.mxc.clone(),
            manifest.block.mimetype.clone(),
            manifest.file_size(),
        );
        let job = self.ensure_queued(&account, manifest, None).await?;
        Ok(OpenedMedia {
            mxc,
            mimetype,
            size,
            state: job.state,
        })
    }

    /// `media.queue`：這個帳號的佇列，第一個是正在拉的。
    pub async fn media_queue(&self, target: &Target) -> Result<Vec<QueuedMedia>, CoreError> {
        let account = self.account_or_current(target)?;
        let downloader = self.downloader_of(&account).await?;
        Ok(downloader
            .list_jobs()
            .into_iter()
            .map(|(mxc, name, status)| QueuedMedia {
                mxc,
                name,
                state: status.state,
                done: status.done,
                total: status.total,
            })
            .collect())
    }

    /// `media.cancel`：正在拉就設旗標（處理完手上那一包就停）、排著就從佇列拿掉（/docs/design/media/media-download.md §5.4）。
    ///
    /// Return:
    ///     Ok(true)    在佇列或 `downloading` 表裡
    ///     Ok(false)   都不在
    pub async fn media_cancel(&self, mxc: &str, target: &Target) -> Result<bool, CoreError> {
        let account = self.account_or_current(target)?;
        let downloader = self.downloader_of(&account).await?;
        Ok(downloader.cancel(mxc))
    }

    /// 另存新檔（`media.save_to`）：**解密寫到 `out`**。本機原檔在就從它複製；池裡沒有就排進佇列、等它完成（/docs/design/media/media-download.md §5.5 最後）。
    ///
    /// ⚠️ 這裡明文落地是**使用者要的**（他指定了 `out`），不是我們偷偷做的——這條界線要守住（/docs/design/rpc-specs/local-interface.md §8）。
    ///
    /// Args:
    ///     no_cache: true ＝ 這次下載的不留在池裡（池裡本來就有的不動）
    /// Return:
    ///     Ok(SavedMedia)
    ///     Err(Usage)       同 [`Core::media_download`]；或下載被取消
    ///     Err(Integrity)   檔壞了（驗不過）
    ///     Err(Io)          寫不了 `out`
    pub async fn save_media_to(
        &self,
        media: &MediaRef,
        out: &Path,
        no_cache: bool,
        target: &Target,
    ) -> Result<SavedMedia, CoreError> {
        let account = self.account_or_current(target)?;
        let manifest = self.resolve_media(&account, media).await?;
        let mxc = manifest.mxc.clone();
        let (done, wait) = tokio::sync::oneshot::channel();
        let job = self.ensure_queued(&account, manifest, Some(done)).await?;
        let (cache, _me) = self.server_cache_and_me(&account)?;
        let pool = self.pool_of(&account)?;
        let source = match job.state {
            DownloadState::LocalSource => "local_source",
            DownloadState::Complete => "cache",
            _ => {
                wait.await.map_err(|_| cancelled_error(&mxc))??;
                "server"
            }
        };
        let entry = cache.read().await.find_media(&mxc)?.ok_or_else(|| {
            CoreError::new(
                CoreErrorKind::Io,
                format!("the media row of {mxc} vanished"),
            )
        })?;
        let out_path = out.to_path_buf();
        let bytes = match source {
            "local_source" => {
                let (file, _) = open_local_source(&entry).ok_or_else(|| {
                    CoreError::new(
                        CoreErrorKind::Io,
                        "the local original went away while copying",
                    )
                })?;
                copy_to(file, out_path).await?
            }
            _ => {
                let reader = media::open_complete(&pool, &entry).ok_or_else(|| {
                    CoreError::new(
                        CoreErrorKind::Io,
                        format!("{mxc} is not complete in the pool"),
                    )
                })?;
                let mxc_here = mxc.clone();
                cache.post(
                    move |cache| cache.touch_media(&mxc_here).map(|_| ()),
                    Vec::new(),
                );
                copy_to(reader, out_path).await?
            }
        };
        if no_cache && source == "server" {
            self.drop_from_pool(&account, &mxc).await?;
        }
        Ok(SavedMedia {
            out: out.display().to_string(),
            bytes,
            source: source.to_string(),
            hash: match source {
                "local_source" => None,
                _ => entry.hash,
            },
        })
    }

    /// 直接逐塊下載到檔案，**繞過媒體池與佇列**（CLI 的 `--no-cache`；daemon 的 `media.save_to` 走 [`Core::save_media_to`]）。
    ///
    /// 🚫 失敗時**刪掉半成品**：一個下載到一半的檔留在那裡，下次會被當成完整的用
    /// （/docs/design/rpc-specs/wbf-cli-spec.md §4 exit 3 的語意）。
    pub async fn download_direct(
        &self,
        manifest: &Manifest,
        out: &Path,
        transport: Transport,
        target: &Target,
    ) -> Result<DirectDownloadResult, CoreError> {
        let account = self.account_or_current(target)?;
        let mut client = self
            .client_of(
                &account,
                transport,
                MethodHome::WbfSdkOnly,
                LinkRole::Download,
            )
            .await?;
        let mut file = std::fs::File::create(out)?;
        let result = client
            .download(manifest, &mut file, &mut |done, total| {
                self.events.progress_of(
                    done as u64,
                    Some(total as u64),
                    format!("chunk {done}/{total}"),
                )
            })
            .await;
        let report = match result {
            Ok(report) => report,
            Err(error) => {
                drop(file);
                let _ = std::fs::remove_file(out);
                return Err(error.into());
            }
        };
        std::io::Write::flush(&mut file)?;
        Ok(DirectDownloadResult {
            out: out.display().to_string(),
            bytes: report.bytes,
            chunks: report.chunks,
            sha256_verified: report.sha256_verified,
        })
    }

    /// 三種說法 → manifest（/docs/design/media/media-download.md §3.2「金鑰從哪來」）。
    ///
    /// Return:
    ///     Ok(Manifest)   `server` 是這個帳號的 session 的 server
    ///     Err(Usage)     一般 Matrix 帳號；manifest 是別台 server 的；這個帳號看不到帶金鑰的事件
    pub(crate) async fn resolve_media(
        &self,
        account: &AccountDir,
        media: &MediaRef,
    ) -> Result<Manifest, CoreError> {
        if !self.is_wbf_account(account)? {
            return Err(CoreError::new(
                CoreErrorKind::Usage,
                "this account is on a general Matrix server: its media is downloaded the traditional way (/_matrix/media), which is not wired yet",
            ));
        }
        let session = self.session_of(account)?;
        let (cache, me) = self.server_cache_and_me(account)?;
        let manifest = match media {
            MediaRef::Manifest(manifest) => {
                // 消費端自己再問一次（A6）：daemon 也比過，但 manifest 是對方給的。
                if crate::accounts::server_host_of(&manifest.server)
                    != crate::accounts::server_host_of(&session.server)
                {
                    return Err(CoreError::new(
                        CoreErrorKind::Usage,
                        format!(
                            "the manifest is for {}, but this account is on {}",
                            manifest.server, session.server
                        ),
                    ));
                }
                manifest.clone()
            }
            MediaRef::Mxc(mxc) => {
                let block = cache.read().await.find_media_block_for(&me, mxc)?;
                let block = block.ok_or_else(|| {
                    CoreError::new(
                        CoreErrorKind::Usage,
                        format!("no event {me} can see carries the key of {mxc}; pass the manifest, or the room and event_id"),
                    )
                })?;
                Manifest {
                    server: session.server.clone(),
                    mxc: mxc.clone(),
                    block,
                }
            }
            MediaRef::Event { room, event_id } => {
                let attachment = cache
                    .read()
                    .await
                    .find_event_attachment(&me, room, event_id)?;
                let attachment = attachment.ok_or_else(|| {
                    CoreError::new(
                        CoreErrorKind::Usage,
                        format!("{event_id} in {room} is not a file {me} can see (or it is not decrypted yet)"),
                    )
                })?;
                Manifest {
                    server: session.server.clone(),
                    mxc: attachment.mxc,
                    block: attachment.block,
                }
            }
        };
        manifest
            .block
            .check_as_event_block()
            .map_err(|error| CoreError::new(CoreErrorKind::Integrity, error.to_string()))?;
        Ok(manifest)
    }

    /// 本地已經有了就回那個狀態；沒有就排進佇列（/docs/design/media/media-download.md §5.3 的表）。
    ///
    /// Args:
    ///     waiter: 要等它結束的話給一個；`complete`／`local_source` 時直接丟掉（呼叫者看 `state` 就知道不用等）
    pub(crate) async fn ensure_queued(
        &self,
        account: &AccountDir,
        manifest: Manifest,
        waiter: Option<tokio::sync::oneshot::Sender<Result<(), CoreError>>>,
    ) -> Result<MediaJob, CoreError> {
        let (cache, _me) = self.server_cache_and_me(account)?;
        let pool = self.pool_of(account)?;
        let entry = cache.read().await.find_media(&manifest.mxc)?;
        let mxc = manifest.mxc.clone();
        if let Some(entry) = &entry {
            let total =
                wbf_sdk::chunk_crypto::chunk_count(entry.file_size, entry.chunk_size).unwrap_or(0);
            if media::open_complete(&pool, entry).is_some() {
                return Ok(MediaJob {
                    mxc,
                    state: DownloadState::Complete,
                    done: total,
                    total,
                });
            }
            if open_local_source(entry).is_some() {
                return Ok(MediaJob {
                    mxc,
                    state: DownloadState::LocalSource,
                    done: total,
                    total,
                });
            }
        }
        // 線先開好：下載處理端只借開著的線（`LinkPool::find_ws_link`），開線是 core 入口與 `link_keeper` 的事。開不起來也照排，線回來就接著拉。
        self.ensure_download_link(account).await;
        let downloader = self.downloader_of(account).await?;
        let status = downloader.enqueue(Arc::new(manifest), waiter);
        Ok(MediaJob {
            mxc,
            state: status.state,
            done: status.done,
            total: status.total,
        })
    }

    /// 這個帳號的 `Download` 線開著嗎，沒開就開。開不起來只講一聲（job 照收、請求排著等線）。
    pub(crate) async fn ensure_download_link(&self, account: &AccountDir) {
        let opened = match self.pool_of_account(account) {
            Ok(links) => {
                links
                    .ensure_open(LinkRole::Download, || {
                        self.open_link(account, LinkRole::Download)
                    })
                    .await
            }
            Err(error) => Err(error),
        };
        if let Err(error) = opened {
            self.events.progress(format!(
                "media: the download link of {} could not be opened yet ({error}); queued anyway",
                account.label()
            ));
        }
    }

    /// 這個帳號的下載處理端；沒有就起一個。這台 server 在這個程序裡第一次起處理端時先掃一次孤兒（/docs/design/media/media-download.md §4.3、§11 第 1 條）。
    pub(crate) async fn downloader_of(
        &self,
        account: &AccountDir,
    ) -> Result<Arc<Downloader>, CoreError> {
        if let Some(existing) = self
            .downloaders
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&account.dir)
        {
            return Ok(existing.clone());
        }
        let server_dir = account.server_dir();
        let first_on_this_server = self
            .media_swept
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(server_dir.clone());
        let (cache, me) = self.server_cache_and_me(account)?;
        if first_on_this_server {
            let pool = self.pool_of(account)?;
            let in_use = self.list_media_in_use(&server_dir);
            let swept = cache
                .run(move |cache| {
                    media::sweep(
                        cache,
                        &pool,
                        media::DEFAULT_PROTECT,
                        std::time::SystemTime::now(),
                        &in_use,
                    )
                })
                .await;
            if let Err(error) = swept {
                self.events.progress(format!(
                    "media: sweeping the pool failed (ignored): {error}"
                ));
            }
        }
        let parts = DownloaderParts {
            user: me,
            server_dir,
            links: self.pool_of_account(account)?,
            media_pool: self.pool_of(account)?,
            cache,
            events: self.events.clone(),
            claims: self.media_claims.clone(),
        };
        let mut downloaders = self
            .downloaders
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // 起之前再看一次：兩個同時進來的，後到的用先到的那個（🚫 一個帳號兩個處理端）。
        if let Some(existing) = downloaders.get(&account.dir) {
            return Ok(existing.clone());
        }
        let downloader = Arc::new(Downloader::start(parts));
        downloaders.insert(account.dir.clone(), downloader.clone());
        Ok(downloader)
    }

    /// 登出、換 session：收掉這個帳號的下載處理端（開著的檔 fsync 留著，下次接著拉）。
    pub(crate) fn stop_downloader_of(&self, account: &AccountDir) {
        let removed = self
            .downloaders
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&account.dir);
        drop(removed);
    }

    /// 這台 server 上所有下載處理端正開著的暫存名（掃描不准碰）。
    fn list_media_in_use(&self, server_dir: &Path) -> HashSet<String> {
        let downloaders: Vec<Arc<Downloader>> = self
            .downloaders
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .filter(|(account_dir, _)| account_dir.starts_with(server_dir))
            .map(|(_, downloader)| downloader.clone())
            .collect();
        downloaders
            .iter()
            .flat_map(|downloader| downloader.list_open_names())
            .collect()
    }

    /// `no_cache`：這次為了另存才下載的，不留在池裡。還有別的 mxc 指著同一個池檔、或有人正在讀它，就只清這一列。
    async fn drop_from_pool(&self, account: &AccountDir, mxc: &str) -> Result<(), CoreError> {
        let (cache, _me) = self.server_cache_and_me(account)?;
        let pool = self.pool_of(account)?;
        let mxc_here = mxc.to_string();
        cache
            .run(move |cache| {
                let Some(pool_file) = cache
                    .find_media(&mxc_here)?
                    .and_then(|entry| entry.pool_file)
                else {
                    return Ok(());
                };
                if cache.media_references(&pool_file)? <= 1 && !pool.is_open(&pool_file)? {
                    pool.remove(&pool_file)?;
                }
                cache.media_reset(&mxc_here)
            })
            .await
    }
}

/// 把一個 `Read` 整個複製到新檔 `out`（在 blocking 執行緒上做：大檔會讀很久）。失敗刪掉半成品。
async fn copy_to<R: std::io::Read + Send + 'static>(
    mut source: R,
    out: std::path::PathBuf,
) -> Result<u64, CoreError> {
    tokio::task::spawn_blocking(move || {
        let copied = (|| {
            let mut file = std::fs::File::create(&out)?;
            let bytes = std::io::copy(&mut source, &mut file)?;
            std::io::Write::flush(&mut file)?;
            file.sync_all()?;
            Ok::<u64, std::io::Error>(bytes)
        })();
        if copied.is_err() {
            let _ = std::fs::remove_file(&out);
        }
        copied.map_err(CoreError::from)
    })
    .await
    .map_err(|error| CoreError::new(CoreErrorKind::Io, format!("the copy task died: {error}")))?
}

fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

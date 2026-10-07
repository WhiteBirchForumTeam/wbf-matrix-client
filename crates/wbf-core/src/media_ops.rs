//! 媒體池的對外操作：下載排隊（`media.download`／`media.open`／`media.queue`／`media.cancel`）、另存新檔、狀態、清理。
//! 設計在 /docs/design/media/media-download.md（下載）與 /docs/design/media/media-pool.md（池）。
//!
//! ⚠️ **池裡的東西是加密的**（/docs/design/media/media-pool.md §1）。所以「給前端一個路徑」是錯的：它讀到的是密文，
//! 而 core 先解密寫到某個路徑就是**明文落地**——整個加密池的意義就沒了（/docs/design/rpc-specs/local-interface.md §8）。
//! 播放與顯示走資料平面的 URL（`media.open` → `GET /media`，`media_stream.rs`）；這裡唯一的明文落地是使用者明說要匯出到自己選的位置（[`Core::export_media_to`]）。
//!
//! 下載本身由每帳號一個的下載處理端做（`download_queue.rs`）：這裡只負責「要哪個檔、本地已經有了沒、排進去、等它」。

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;

use serde::Serialize;

use wbf_sdk::chat::MatrixAttachment;
use wbf_sdk::local_source::open_local_source;
use wbf_sdk::manifest::Manifest;
use wbf_sdk::media;
use wbf_sdk::media_kind::{MediaKind, Verification};
use wbf_sdk::Transport;

use crate::accounts::AccountDir;
use crate::backend_choice::MethodHome;
use crate::download_queue::{Downloader, DownloaderParts};
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

/// 要哪個檔（/docs/design/media/media-download.md §7.1）：三種說法，金鑰從哪來不同（§3.2、§5.3）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MediaRef {
    /// 只給 mxc：照本地那一列的描述，金鑰從這個帳號看得到、跟列一致的事件裡找；沒有列就不能下載。
    Mxc(String),
    /// 某一則訊息的附件（從訊息點下載）：由這則事件確定本地那一列。`mxc` 給了就要是這則的附件。
    Event {
        room: String,
        event_id: String,
        mxc: Option<String>,
    },
    /// 直接給 manifest（含金鑰）。
    Manifest(Manifest),
}

/// `MediaRef` 解出來的：是哪一種檔、金鑰在哪（/docs/design/rpc-specs/data-plane.md §7.1）。
pub(crate) enum ResolvedMedia {
    /// wbf 分塊（`kind` 1）：走下載處理端的 `Download` 線
    Chunked(Manifest),
    /// 標準 Matrix 附件（`kind` 2、3）：走 HTTP（`matrix_download.rs`）
    Matrix(MatrixAttachment),
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
    /// `media` 列的格式與驗證結果（/docs/design/media/media-download.md §12.3）；列還沒建（剛排進去）就不在
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<MediaKind>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verified: Option<Verification>,
}

/// `media.open` 的回答（URL 由 daemon 用共享 token 鑄，core 不知道）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct OpenedMedia {
    pub mxc: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mimetype: Option<String>,
    /// 明文總長；傳統檔的事件沒給大小就不在（下載完才知道）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    pub state: DownloadState,
    /// `kind` 2 而 `verified` 不是 1 時，讀 URL 會是 412、body 照給（/docs/design/rpc-specs/data-plane.md §8.2）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<MediaKind>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verified: Option<Verification>,
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

/// `media.delete_local` 的結果。
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DeletedMedia {
    pub mxc: String,
    /// 本地有這一列、刪掉了（false ＝ 本來就沒有）
    pub removed: bool,
    /// 有正在下載（或排著）的被取消了
    pub cancelled: bool,
}

/// `media.export_to` 的結果（寫到哪由呼叫者自己知道：RPC 回它收到的 URI、CLI 回 `-o`）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ExportedMedia {
    pub bytes: u64,
    /// `"local_source"`（本機原檔）、`"cache"`（池裡本來就有）、`"server"`（這次排隊下載的）
    pub source: String,
    /// 哪一種格式（/docs/design/rpc-specs/data-plane.md §7.1）
    pub kind: MediaKind,
    /// 整檔 hash 的比對結果（/docs/design/media/media-download.md §12.3）
    pub verified: Verification,
    /// 快取列記的校驗碼（`sha256:…` 或 `blake3:…`）；列上沒記就沒有
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
    ///     Err(Usage)       找不到金鑰（這個帳號看不到引用它的事件）、manifest 是別台 server 的、一般 Matrix 帳號給了 manifest（它只有標準附件）
    ///     Err(Integrity)   同一個 mxc，這則事件的描述跟本地已經下載好、或正在下載的那份不一樣（/docs/design/media/media-download.md §12.1）
    pub async fn media_download(
        &self,
        media: &MediaRef,
        target: &Target,
    ) -> Result<MediaJob, CoreError> {
        let account = self.account_or_current(target)?;
        match self.resolve_media(&account, media).await? {
            ResolvedMedia::Chunked(manifest) => {
                self.ensure_queued(&account, manifest, None, true).await
            }
            ResolvedMedia::Matrix(attachment) => {
                self.ensure_matrix_download(&account, attachment, None)
                    .await
            }
        }
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
        let (mxc, mimetype, size, job) = match self.resolve_media(&account, media).await? {
            ResolvedMedia::Chunked(manifest) => {
                let (mxc, mimetype, size) = (
                    manifest.mxc.clone(),
                    manifest.block.mimetype.clone(),
                    manifest.block.file_size,
                );
                let job = self.ensure_queued(&account, manifest, None, true).await?;
                (mxc, mimetype, size, job)
            }
            ResolvedMedia::Matrix(attachment) => {
                let (mxc, mimetype, size) = (
                    attachment.mxc.clone(),
                    attachment.mimetype.clone(),
                    attachment.size,
                );
                let job = self
                    .ensure_matrix_download(&account, attachment, None)
                    .await?;
                (mxc, mimetype, size, job)
            }
        };
        Ok(OpenedMedia {
            mxc,
            mimetype,
            size,
            state: job.state,
            kind: job.kind,
            verified: job.verified,
        })
    }

    /// `media.queue`：這個帳號要過的分塊檔，先列自己的處理端（第一個是正在拉的），再列同 server 別的帳號替它下載的（維護者 2026-10-07）。
    pub async fn media_queue(&self, target: &Target) -> Result<Vec<QueuedMedia>, CoreError> {
        let account = self.account_or_current(target)?;
        let (_cache, me) = self.server_cache_and_me(&account)?;
        let own = self.downloader_of(&account).await?;
        let others = self
            .list_downloaders_on(&account.server_dir())
            .into_iter()
            .filter(|downloader| !Arc::ptr_eq(downloader, &own));
        Ok(std::iter::once(own.clone())
            .chain(others)
            .flat_map(|downloader| downloader.list_jobs_of(&me))
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
    /// 一個檔只有一個下載，要過它的帳號共用：誰叫都停，每個要過的帳號都收到 `cancelled`（維護者 2026-10-07）。
    ///
    /// Return:
    ///     Ok(true)    這台 server 上有它在下載或排著
    ///     Ok(false)   都不在
    pub async fn media_cancel(&self, mxc: &str, target: &Target) -> Result<bool, CoreError> {
        let account = self.account_or_current(target)?;
        Ok(self.cancel_downloads_on_server(&account, mxc))
    }

    /// 這台 server 上所有正在下載（或排著）這個 mxc 的都交取消：傳統的 task（登記表以 server 為鍵）與每個帳號已經起了的分塊處理端。
    /// 全部都交（🚫 第一個取消成功就停）：job 可能轉到別的帳號的處理端去了；🚫 為了取消新起一個處理端。
    ///
    /// Return:
    ///     bool   true ＝至少一個接了取消
    fn cancel_downloads_on_server(&self, account: &AccountDir, mxc: &str) -> bool {
        let mut cancelled = self.cancel_matrix_download(account, mxc);
        for downloader in self.list_downloaders_on(&account.server_dir()) {
            cancelled |= downloader.cancel(mxc);
        }
        cancelled
    }

    /// 匯出（`media.export_to`，/docs/design/media/media-download.md §7.3）：**解密寫到 `to`**。
    /// 本機原檔能驗（區塊帶 sha256）就從它匯出、整檔比 sha256（原檔在池外、沒有保護）；不能驗、或驗不過，就從池匯出——池裡沒有就排進下載、等它完成。
    /// 從池匯出🚫 再算 hash、只核大小（維護者 2026-10-06）：每段讀出時過了池的 AEAD，整檔 hash 下載完已經驗過、記在 `verified`。
    /// 傳統加密的檔（`kind` 2）沒驗過或驗不過：**照匯**、回 `Unverified`（1501），`data` 是成功時會給的那份（/docs/design/rpc-specs/data-plane.md §8.2 的約定）。
    ///
    /// ⚠️ 這裡明文落地是**使用者要的**（使用者指定了 `to`），不是我們偷偷做的——這條界線要守住（/docs/design/rpc-specs/local-interface.md §8）。
    ///
    /// Args:
    ///     to: 本機路徑（RPC 那邊先從 `file://` URI 解出來）, example: "C:/Users/me/v.mp4"
    ///     no_cache: true ＝ 這次為了匯出才下載的不留在池裡（池裡本來就有的不動）
    /// Return:
    ///     Ok(ExportedMedia)
    ///     Err(Usage)       同 [`Core::media_download`]；或下載被取消
    ///     Err(Integrity)   檔壞了（池裡那份大小對不上：池檔一起丟掉，下次重下）；`to` 🚫 被動過
    ///     Err(Unverified)  寫到 `to` 了，但它是沒驗過或驗不過的傳統加密檔；`data` 是 ExportedMedia
    ///     Err(Io)          寫不了 `to`
    pub async fn export_media_to(
        &self,
        media: &MediaRef,
        to: &Path,
        no_cache: bool,
        target: &Target,
    ) -> Result<ExportedMedia, CoreError> {
        let account = self.account_or_current(target)?;
        let resolved = self.resolve_media(&account, media).await?;
        let (cache, _me) = self.server_cache_and_me(&account)?;
        let (done, wait) = tokio::sync::oneshot::channel();
        let (mxc, job) = match resolved {
            ResolvedMedia::Chunked(manifest) => {
                let mxc = manifest.mxc.clone();
                if let Some(exported) = self.export_local_original(&cache, &manifest, to).await? {
                    return Ok(exported);
                }
                let job = self
                    .ensure_queued(&account, manifest, Some(done), false)
                    .await?;
                (mxc, job)
            }
            // 傳統檔還沒有「本機原檔」那條（一般 Matrix 帳號的上傳，/docs/design/rpc-specs/data-plane.md §7.2，還沒做）：一律從池匯出。
            ResolvedMedia::Matrix(attachment) => {
                let mxc = attachment.mxc.clone();
                let job = self
                    .ensure_matrix_download(&account, attachment, Some(done))
                    .await?;
                (mxc, job)
            }
        };
        let source = match job.state {
            DownloadState::Complete => "cache",
            _ => {
                wait.await.map_err(|_| {
                    CoreError::new(
                        CoreErrorKind::Io,
                        format!("the downloader stopped before {mxc} was complete (logged out?)"),
                    )
                })??;
                "server"
            }
        };
        let pool = self.pool_of(&account)?;
        let entry = cache.read().await.find_media(&mxc)?.ok_or_else(|| {
            CoreError::new(
                CoreErrorKind::Io,
                format!("the media row of {mxc} vanished"),
            )
        })?;
        let reader = media::open_complete(&pool, &entry).ok_or_else(|| {
            CoreError::new(
                CoreErrorKind::Io,
                format!("{mxc} is not complete in the pool"),
            )
        })?;
        // 🚫 再算 hash，只核大小（維護者 2026-10-06，/docs/design/media/media-download.md §7.3）。
        let expected = media::ExpectedContent {
            file_size: reader.plain_len(),
            sha256_hex: None,
        };
        let exported = export_blocking(reader, to, expected).await;
        if let Err(error) = &exported {
            if error.kind == CoreErrorKind::Integrity {
                // 池裡那份讀不滿自己的長度：它是壞的，丟掉，下次重下。
                self.drop_from_pool(&account, &mxc).await?;
            }
        }
        let bytes = exported?;
        let mxc_here = mxc.clone();
        cache.post(
            move |cache| cache.touch_media(&mxc_here).map(|_| ()),
            Vec::new(),
        );
        if no_cache && source == "server" {
            self.drop_from_pool(&account, &mxc).await?;
        }
        let exported = ExportedMedia {
            bytes,
            source: source.to_string(),
            kind: entry.kind,
            verified: entry.verified,
            hash: entry.hash,
        };
        // 資料照給（已經在 `to`），錯誤碼說它不可信（維護者 2026-10-06 的約定，/docs/design/rpc-specs/data-plane.md §8.2）。
        if exported.kind.is_trust_gated_on_hash() && exported.verified != Verification::Matched {
            return Err(CoreError::new(
                CoreErrorKind::Unverified,
                format!(
                    "{mxc} was exported, but its hash was {}: the content may not be what the sender sent",
                    match exported.verified {
                        Verification::Mismatched => "checked and does not match",
                        _ => "not checked",
                    }
                ),
            )
            .with_data(serde_json::to_value(&exported).map_err(|error| {
                CoreError::new(CoreErrorKind::Io, format!("export result: {error}"))
            })?));
        }
        Ok(exported)
    }

    /// 分塊的檔從本機原檔匯出（/docs/design/media/media-download.md §7.3）：區塊帶 sha256、原檔在、大小對得上才用它，整檔比 sha256（原檔在池外、沒有保護）。
    ///
    /// Return:
    ///     Ok(Some(ExportedMedia))   從原檔匯出了
    ///     Ok(None)                  沒有可用的原檔、或原檔上傳後被改過：呼叫者改從池匯出
    ///     Err(...)                  寫不了 `to`
    async fn export_local_original(
        &self,
        cache: &crate::server_cache::ServerCache,
        manifest: &Manifest,
        to: &Path,
    ) -> Result<Option<ExportedMedia>, CoreError> {
        let Some(sha256_hex) = manifest.block.sha256.clone() else {
            return Ok(None);
        };
        let mxc = &manifest.mxc;
        let entry = cache.read().await.find_media(mxc)?;
        let original = entry
            .filter(|entry| media::is_same_block(entry, &manifest.block))
            .and_then(|entry| {
                let file_size = entry.file_size?;
                open_local_source(&entry).map(|(file, _)| (file, file_size, entry))
            });
        let Some((file, file_size, entry)) = original else {
            return Ok(None);
        };
        let expected = media::ExpectedContent {
            file_size,
            sha256_hex: Some(sha256_hex),
        };
        match export_blocking(file, to, expected).await {
            // 這台機器自己傳的原檔、剛剛整檔比過 sha256：可信，🚫 看 `verified`（/docs/design/rpc-specs/data-plane.md §8.2）。
            Ok(bytes) => Ok(Some(ExportedMedia {
                bytes,
                source: "local_source".to_string(),
                kind: entry.kind,
                verified: entry.verified,
                hash: entry.hash,
            })),
            // 原檔在上傳之後被改過（大小沒變）：改從池匯出。
            Err(error) if error.kind == CoreErrorKind::Integrity => {
                self.events.progress(format!(
                    "export {mxc}: the local original no longer matches ({}); exporting from the pool",
                    error.message
                ));
                Ok(None)
            }
            Err(error) => Err(error),
        }
    }

    /// 直接逐塊下載到檔案，**繞過媒體池與佇列**（CLI 的 `--no-cache`；daemon 的 `media.export_to` 走 [`Core::export_media_to`]）。
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

    /// 三種說法 → 這個檔是哪一種、金鑰在哪（/docs/design/media/media-download.md §3.2「金鑰從哪來」、§12）。
    /// wbf 帳號兩種都可能（同一個房裡可以有 wbf 分塊檔與別的 client 送的標準附件）；一般 Matrix 帳號只有標準附件。
    ///
    /// Return:
    ///     Ok(ResolvedMedia::Chunked)   wbf 分塊（`manifest.server` 是這個帳號的 session 的 server）
    ///     Ok(ResolvedMedia::Matrix)    標準 Matrix 附件
    ///     Err(Usage)                   manifest 是別台 server 的、或一般 Matrix 帳號給了 manifest；只給 mxc 而本地沒有列、或這個帳號
    ///                                  看得到的事件都跟列對不上；那則事件不是這個帳號看得到的檔、或不是給的那個 mxc
    ///     Err(Integrity)               分塊的區塊不能拿來下載
    pub(crate) async fn resolve_media(
        &self,
        account: &AccountDir,
        media: &MediaRef,
    ) -> Result<ResolvedMedia, CoreError> {
        let is_wbf = self.is_wbf_account(account)?;
        let session = self.session_of(account)?;
        let (cache, me) = self.server_cache_and_me(account)?;
        let not_seen = |what: String| {
            CoreError::new(
                CoreErrorKind::Usage,
                format!("{what}; pass the manifest, or the room and event_id"),
            )
        };
        let manifest = match media {
            MediaRef::Manifest(manifest) => {
                if !is_wbf {
                    return Err(CoreError::new(
                        CoreErrorKind::Usage,
                        "a manifest is a chunked (wbf) file: this account is on a general Matrix server",
                    ));
                }
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
            // 只給 mxc：本地那一列說了算，挑跟它一致的事件拿金鑰（維護者 2026-10-07，/docs/design/media/media-download.md §5.3）。
            MediaRef::Mxc(mxc) => {
                let reader = cache.read().await;
                let entry = reader.find_media(mxc)?.ok_or_else(|| {
                    not_seen(format!(
                        "{mxc} has no local record (no event {me} has seen carries it)"
                    ))
                })?;
                match media::find_key_matching_record(&reader, &me, &entry)? {
                    Some(media::RecordedFileKey::Chunked(block)) if is_wbf => Manifest {
                        server: session.server.clone(),
                        mxc: mxc.clone(),
                        block,
                    },
                    Some(media::RecordedFileKey::Matrix(attachment)) => {
                        return Ok(ResolvedMedia::Matrix(attachment));
                    }
                    // 一般 Matrix 帳號下載不了分塊的檔；或看得到的事件都跟列對不上。
                    _ => {
                        return Err(not_seen(format!(
                            "no event {me} can see describes {mxc} the way the local record does"
                        )));
                    }
                }
            }
            // 從訊息點下載：先由這則事件確定列（列不在就照它建、補連結），描述跟列合不合由下載入口判斷、不合就回錯。
            MediaRef::Event {
                room,
                event_id,
                mxc,
            } => {
                let (me_here, room_here, event_here) = (me.clone(), room.clone(), event_id.clone());
                cache
                    .run(move |cache| cache.media_link_event(&me_here, &room_here, &event_here))
                    .await?;
                let refuse_other_mxc = |found: &str| match mxc.as_deref() {
                    Some(given) if given != found => Err(CoreError::new(
                        CoreErrorKind::Usage,
                        format!("{event_id} in {room} carries {found}, not {given}"),
                    )),
                    _ => Ok(()),
                };
                let reader = cache.read().await;
                let attachment = match is_wbf {
                    true => reader.find_event_attachment(&me, room, event_id)?,
                    false => None,
                };
                match attachment {
                    Some(attachment) => {
                        refuse_other_mxc(&attachment.mxc)?;
                        Manifest {
                            server: session.server.clone(),
                            mxc: attachment.mxc,
                            block: attachment.block,
                        }
                    }
                    None => {
                        let attachment = reader
                            .find_event_matrix_attachment(&me, room, event_id)?
                            .ok_or_else(|| {
                                CoreError::new(
                                    CoreErrorKind::Usage,
                                    format!("{event_id} in {room} is not a file {me} can see (or it is not decrypted yet)"),
                                )
                            })?;
                        refuse_other_mxc(&attachment.mxc)?;
                        return Ok(ResolvedMedia::Matrix(attachment));
                    }
                }
            }
        };
        manifest
            .block
            .check_as_event_block()
            .map_err(|error| CoreError::new(CoreErrorKind::Integrity, error.to_string()))?;
        Ok(ResolvedMedia::Chunked(manifest))
    }

    /// 本地已經有了就回那個狀態；沒有就排進佇列（/docs/design/media/media-download.md §5.3 的表）。
    /// 列說的跟這次的區塊對不上（大小、塊大小、sha256）一律回錯、列🚫 動（維護者 2026-10-07）：要換描述先 `media.delete_local`。
    ///
    /// Args:
    ///     waiter: 要等它結束的話給一個；`complete`／`local_source` 時直接丟掉（呼叫者看 `state` 就知道不用等）
    ///     local_source_counts: false ＝ 本機原檔不算「已經有了」（匯出時原檔驗不過，要池裡那份）
    /// Return:
    ///     Ok(MediaJob)
    ///     Err(Integrity)   本地這個 mxc 的列跟這次的描述對不上（不管下載完了沒）
    pub(crate) async fn ensure_queued(
        &self,
        account: &AccountDir,
        manifest: Manifest,
        waiter: Option<tokio::sync::oneshot::Sender<Result<(), CoreError>>>,
        local_source_counts: bool,
    ) -> Result<MediaJob, CoreError> {
        let (cache, _me) = self.server_cache_and_me(account)?;
        let pool = self.pool_of(account)?;
        let entry = cache.read().await.find_media(&manifest.mxc)?;
        let mxc = manifest.mxc.clone();
        if let Some(described) = entry
            .as_ref()
            .filter(|entry| !media::is_same_block(entry, &manifest.block))
        {
            return Err(other_description_error(&mxc, described));
        }
        if let Some(entry) = &entry {
            // 總塊數還不知道是 0（`MediaJob::total` 的約定）：不是分塊的檔沒有塊。
            let total = match (entry.file_size, entry.chunk_size) {
                (Some(file_size), Some(chunk_size)) => {
                    wbf_sdk::chunk_crypto::chunk_count(file_size, chunk_size).unwrap_or(0)
                }
                _ => 0,
            };
            if media::open_complete(&pool, entry).is_some() {
                return Ok(MediaJob {
                    mxc,
                    state: DownloadState::Complete,
                    done: total,
                    total,
                    kind: Some(entry.kind),
                    verified: Some(entry.verified),
                });
            }
            if local_source_counts && open_local_source(entry).is_some() {
                return Ok(MediaJob {
                    mxc,
                    state: DownloadState::LocalSource,
                    done: total,
                    total,
                    kind: Some(entry.kind),
                    verified: Some(entry.verified),
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
            kind: entry.as_ref().map(|entry| entry.kind),
            verified: entry.as_ref().map(|entry| entry.verified),
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
            line_silence: crate::download_queue::LINE_SILENCE,
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
        if let Some(downloader) = removed {
            downloader.stop();
        }
        // 傳統格式的下載也是這個帳號的 token 在拉：一起收（/docs/design/media/media-download.md §12.4）。
        self.stop_matrix_downloads_of(account);
    }

    /// 這台 server 上每個帳號已經起了的下載處理端（🚫 為了問而新起一個）。
    fn list_downloaders_on(&self, server_dir: &Path) -> Vec<Arc<Downloader>> {
        self.downloaders
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .filter(|(account_dir, _)| account_dir.starts_with(server_dir))
            .map(|(_, downloader)| downloader.clone())
            .collect()
    }

    /// 這台 server 上所有下載處理端正開著的暫存名（掃描不准碰）。
    fn list_media_in_use(&self, server_dir: &Path) -> HashSet<String> {
        let mut in_use: HashSet<String> = self
            .list_downloaders_on(server_dir)
            .iter()
            .flat_map(|downloader| downloader.list_open_names())
            .collect();
        in_use.extend(self.matrix_transfers.list_pending_names(server_dir));
        in_use
    }

    /// `media.delete_local`（/docs/design/media/media-download.md §7.4）：清掉這個 mxc 在本地的一切——池檔、半成品、seek 暫存檔、`media` 列。
    /// 先取消這台 server 上所有帳號正在下載它的（分塊與傳統兩條），等寫入者放手（最多 [`DELETE_WAITS_FOR_WRITER`]）才刪。
    /// 之後要再下載就從訊息點（`{ room, event_id }`）：列由那則事件重建（/docs/design/media/media-download.md §5.3）。
    ///
    /// Args:
    ///     mxc: example: "mxc://localhost/000000000000004d"
    /// Return:
    ///     Ok(DeletedMedia)   `removed` false ＝ 本地本來就沒有這一列
    ///     Err(Usage)         取消了，但等不到寫入者放手（例如 GET 還在讀分塊下載中的檔）：什麼都🚫 刪，晚點再來
    ///     Err(...)           DB、檔案動不了
    pub async fn del_local_media(
        &self,
        mxc: &str,
        target: &Target,
    ) -> Result<DeletedMedia, CoreError> {
        let account = self.account_or_current(target)?;
        let server_dir = account.server_dir();
        let (cache, _me) = self.server_cache_and_me(&account)?;
        let pool = self.pool_of(&account)?;
        let cancelled = self.cancel_downloads_on_server(&account, mxc);
        // 分塊的取消會把半成品留著續傳（/docs/design/media/media-download.md §5.4）：處理端關掉它之前🚫 刪。
        let pending_name = cache.read().await.media_pending_name(mxc)?;
        if let Some(pending_name) = pending_name {
            let started = std::time::Instant::now();
            while self.list_media_in_use(&server_dir).contains(&pending_name)
                && started.elapsed() < DELETE_WAITS_FOR_WRITER
            {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        }
        let in_use = self.list_media_in_use(&server_dir);
        let mxc_here = mxc.to_string();
        let removed = cache
            .run(move |cache| media::del_local_copy(cache, &pool, &mxc_here, &in_use))
            .await?;
        Ok(DeletedMedia {
            mxc: mxc.to_string(),
            removed,
            cancelled,
        })
    }

    /// `no_cache`：這次為了匯出才下載的，不留在池裡。還有別的 mxc 指著同一個池檔、或有人正在讀它，就只清這一列。
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

/// `media.delete_local` 取消下載之後，最多等寫入者放手這麼久（分塊的處理端等在途的那一塊落地才停）。
pub(crate) const DELETE_WAITS_FOR_WRITER: std::time::Duration = std::time::Duration::from_secs(5);

/// 同一個 mxc，這次的描述跟本地那一列說的不是同一個檔（/docs/design/media/media-download.md §5.3、§12.1）：拒這次、列🚫 動。
/// 先記下的那份說了算：寫錯或偽造的事件先到，真的那則也會被拒，UI 拿到這個錯就是警告；要換成這次的先 `media.delete_local`（維護者 2026-10-07）。
pub(crate) fn other_description_error(
    mxc: &str,
    recorded: &wbf_sdk::cache::MediaEntry,
) -> CoreError {
    CoreError::new(
        CoreErrorKind::Integrity,
        format!(
            "this description of {mxc} does not match the local record (kind {:?}, size {:?}, chunk size {:?}); refused, the local copy is kept. \
             To replace it with this one, delete the local copy (media.delete_local) and download it from the message",
            recorded.kind, recorded.file_size, recorded.chunk_size
        ),
    )
}

/// `media::export_to_path` 放到 blocking 執行緒上做（大檔會讀很久）。
async fn export_blocking<R: std::io::Read + Send + 'static>(
    source: R,
    to: &Path,
    expected: media::ExpectedContent,
) -> Result<u64, CoreError> {
    let to = to.to_path_buf();
    tokio::task::spawn_blocking(move || media::export_to_path(source, &to, &expected))
        .await
        .map_err(|error| {
            CoreError::new(CoreErrorKind::Io, format!("the export task died: {error}"))
        })?
        .map_err(CoreError::from)
}

fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

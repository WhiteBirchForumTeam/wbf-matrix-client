//! 媒體池：狀態、清理、下載到本機檔案。設計在 local-cache-db.md §8。
//!
//! ⚠️ **池裡的東西是加密的**（§8）。所以「給前端一個路徑」是錯的：它讀到的是密文，
//! 而 core 先解密寫到某個路徑就是**明文落地**——整個加密池的意義就沒了
//! （architecture-v2 §4.8 記了這是初稿的錯）。
//!
//! 這一層現在只提供「**使用者明說要把明文放到自己選的位置**」那條路（[`Core::download_to`]）。
//! 📎 daemon 落地時，播放與顯示會走資料平面的 capability URL（§4.8），🚫 不是這裡。

use std::io::Write;
use std::path::Path;

use serde::Serialize;

use wbf_sdk::channel::Channel;
use wbf_sdk::client::WbfClient;
use wbf_sdk::manifest::Manifest;
use wbf_sdk::media::{self, FetchOutcome};
use wbf_sdk::Transport;

use crate::accounts::AccountDir;
use crate::backend_choice::MethodHome;
use crate::error::{CoreError, CoreErrorKind};
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
    /// ⚠️ `true` 表示**清完還是超過配額**：剩下的都在保護期內，沒有東西可以再刪。
    pub still_over_quota: bool,
    pub swept_missing_files: u64,
    pub swept_pending: u64,
    pub swept_orphan_files: u64,
}

/// `--no-cache` 的下載結果（沒進池，所以沒有 `pool_file`／`source`）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DirectDownloadResult {
    pub out: String,
    pub bytes: u64,
    pub chunks: u32,
    pub sha256_verified: bool,
}

/// `download` 的結果。
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DownloadResult {
    pub out: String,
    pub bytes: u64,
    pub chunks: u32,
    /// `"cache"` 或 `"server"`。
    pub source: String,
    pub pool_file: String,
    /// 快取列記的校驗碼。⚠️ 半成品還沒有（下載到一半就沒算完），所以是 `Option`。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hash: Option<String>,
    /// ⚠️ 只有這次**真的逐塊下載並整檔核對過**才是 `true`。命中快取沒有重算，
    /// 報 `false`，而 `hash` 給的是快取列記的校驗碼（PR #14 審查 rumia🟡1）。
    pub sha256_verified: bool,
}

impl Core {
    /// 媒體池現在多大、有幾個半成品。
    pub fn media_stats(&self, target: &Target) -> Result<MediaStats, CoreError> {
        let account = self.account_or_current(target)?;
        let (cache, _me) = self.cache_and_me(&account)?;
        let pool = self.pool_of(&account)?;
        let complete = cache.list_media_by_last_used()?;
        let incomplete = cache.list_media_incomplete()?;
        Ok(MediaStats {
            pool_dir: pool.dir().display().to_string(),
            bytes_on_disk: cache.media_bytes_on_disk()?,
            complete_files: complete.len(),
            incomplete_files: incomplete.len(),
            pending_on_disk: pool.list_pending()?.len(),
            oldest_last_used_at: complete.first().map(|entry| entry.last_used_at),
        })
    }

    /// 超過配額就從最久沒用的刪；保護期內的不刪。順便掃孤兒（local-cache-db §8.5）。
    ///
    /// Args:
    ///     quota_mib: example: 2048
    ///     protect_days: 保護期，這幾天內用過的一律不刪, example: 7
    pub fn collect_media_garbage(
        &self,
        quota_mib: u64,
        protect_days: u64,
        target: &Target,
    ) -> Result<MediaGcReport, CoreError> {
        let account = self.account_or_current(target)?;
        let (mut cache, _me) = self.cache_and_me(&account)?;
        let pool = self.pool_of(&account)?;
        let protect = std::time::Duration::from_secs(protect_days * 24 * 3600);
        let now = now_millis();
        let swept = media::sweep(&mut cache, &pool, protect, now)?;
        let report =
            media::collect_garbage(&mut cache, &pool, quota_mib * 1024 * 1024, protect, now)?;
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
            still_over_quota: report.still_over_quota,
            swept_missing_files: swept.reset_rows,
            swept_pending: swept.removed_pending,
            swept_orphan_files: swept.removed_orphan_files,
        })
    }

    /// 把一份 manifest 指的東西下載下來，**解密寫到 `out`**。
    ///
    /// ⚠️ 這裡明文落地是**使用者要的**（他指定了 `out`），不是我們偷偷做的——
    /// 這條界線要守住（architecture-v2 §4.8）。
    ///
    /// 途中發 `Progress` 事件回報塊數。
    pub async fn download_to(
        &self,
        manifest: &Manifest,
        out: &Path,
        transport: Transport,
        target: &Target,
    ) -> Result<DownloadResult, CoreError> {
        let account = self.account_or_current(target)?;
        let (mut cache, _me) = self.cache_and_me(&account)?;
        let pool = self.pool_of(&account)?;
        let mut client = self
            .client_of(
                &account,
                transport,
                MethodHome::WbfSdkOnly,
                LinkRole::Download,
            )
            .await?;
        let fetched = media::fetch(
            &mut client,
            manifest,
            &mut cache,
            &pool,
            &mut |done, total| {
                self.events.progress_of(
                    done as u64,
                    Some(total as u64),
                    format!("chunk {done}/{total}"),
                )
            },
        )
        .await?;
        let pool_file =
            fetched.entry.pool_file.as_deref().ok_or_else(|| {
                CoreError::new(CoreErrorKind::Io, "fetched media has no pool file")
            })?;
        let mut reader = pool.open_read(pool_file)?;
        let mut file = std::fs::File::create(out)
            .map_err(|error| CoreError::new(CoreErrorKind::Io, format!("{error}")))?;
        let bytes = std::io::copy(&mut reader, &mut file)
            .map_err(|error| CoreError::new(CoreErrorKind::Io, format!("{error}")))?;
        file.flush()
            .map_err(|error| CoreError::new(CoreErrorKind::Io, format!("{error}")))?;
        let (source, chunks, sha256_verified) = match fetched.outcome {
            FetchOutcome::CacheHit => ("cache", 0, false),
            FetchOutcome::Downloaded { chunks, .. } => {
                ("server", chunks, manifest.block.sha256.is_some())
            }
        };
        Ok(DownloadResult {
            out: out.display().to_string(),
            bytes,
            chunks,
            source: source.to_string(),
            pool_file: pool_file.to_string(),
            hash: fetched.entry.hash,
            sha256_verified,
        })
    }

    /// 直接逐塊下載到檔案，**繞過媒體池**（`--no-cache`）。
    ///
    /// ⚠️ 「要不要走快取」是**使用者的選擇**，所以它是兩個方法而不是一個旗標：
    /// 走快取那條（[`Core::download_to`]）會把東西留在池裡，這條不會。
    ///
    /// 🚫 失敗時**刪掉半成品**：一個下載到一半的檔留在那裡，下次會被當成完整的用
    /// （CLI 規格 §4 exit 3 的語意）。
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
        file.flush()?;
        Ok(DirectDownloadResult {
            out: out.display().to_string(),
            bytes: report.bytes,
            chunks: report.chunks,
            sha256_verified: report.sha256_verified,
        })
    }

    /// 這個帳號跟 server 的 wbf 通道。🚨 **一律 WebSocket**：wbf 協議就是 WS
    /// （維護者 2026-09-13：「WBF 協議下總是用 WS」）。
    ///
    /// 🚫 **一般的呼叫端不要用這個，用 [`Core::client_of`]** —— 那裡才有「這台是不是 wbf」
    /// 與「這個方法住在哪一邊」的判斷（`backend_choice`）。這條是**底下那半**，
    /// 留給兩種人：閘門自己，以及**探測**（探測不能走閘門，不然它會叫到自己）。
    ///
    /// 📎 `Transport::Http`（pack over HTTP）🚫 **不從這裡走** —— 它只剩 debug 用途，
    /// 由 `wbf-sdk` 那一層自己的測試涵蓋。
    ///
    /// 🚫 **回傳值不准離開這個 crate**：它握著 `access_token`。
    pub(crate) async fn connect_wbf_client(
        &self,
        account: &AccountDir,
    ) -> Result<WbfClient<Channel>, CoreError> {
        let session = self.session_of(account)?;
        let channel =
            Channel::connect(&session.server, &session.access_token, Transport::WebSocket).await?;
        Ok(WbfClient::new(channel))
    }
}

fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

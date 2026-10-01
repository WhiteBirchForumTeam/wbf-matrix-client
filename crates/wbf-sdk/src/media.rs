//! 媒體快取的接法：下載的一個檔（[`MediaDownload`]）怎麼在主檔、seek 暫存檔與網路之間拿塊，加上配額清理與啟動掃描。
//! 設計在 /docs/design/media/media-download.md（下載）與 /docs/design/media/media-pool.md §5（清理）。
//!
//! - [`MediaDownload`]：一個 mxc 的主檔（`PoolWriter`，池格式 v2）、seek 暫存檔（`SeekStore`）與 `Info` 驗過的參數。
//!   主檔「往前一塊」先看 seek 暫存檔、沒有才上網拉；GET 要的塊先看本地（主檔已封的段、seek 暫存檔），沒有才現拉存進 seek 暫存檔。
//!   **誰排隊、誰先、何時取消、DB 怎麼寫**不在這裡：那是 core 的下載 worker（`wbf_core::download_queue`）。
//! - `collect_garbage`：配額 best effort、保護期內不刪、先刪檔再刪列、有人指著的池檔不刪（/docs/design/media/media-pool.md §5）。
//! - `sweep`：掃孤兒（DB 說有檔但打不開 → reset；`pending/` 裡沒人認領、過期、不是池格式 v2 → 刪；沒人指著的完成檔 → 刪）。
//!
//! 這裡是池、seek 暫存檔、`cache.db` 與下載管線唯一的交會點：`cache` 不知道池，`media_pool` 不知道 DB，下載管線不知道兩者。

use std::collections::HashSet;
use std::time::Duration;

use zeroize::Zeroizing;

use crate::cache::{Cache, MediaEntry};
use crate::channel::PackChannel;
use crate::chunk_crypto::{chunk_count, expected_plain_len};
use crate::client::WbfClient;
use crate::download::VerifiedTarget;
use crate::error::SdkError;
use crate::manifest::Manifest;
use crate::media_pool::{Finished, MediaPool, PoolReader, PoolWriter, SEEK_SUFFIX};
use crate::seek_store::SeekStore;

/// 進度快照的間隔（/docs/design/media/media-download.md §4.1：每 1.5 秒 fsync 一次並把段數寫回 DB）。
pub const PROGRESS_FLUSH: Duration = Duration::from_millis(1500);
/// /docs/design/media/media-pool.md §5 的預設。
pub const DEFAULT_QUOTA_BYTES: u64 = 2 * 1024 * 1024 * 1024;
pub const DEFAULT_PROTECT: Duration = Duration::from_secs(7 * 24 * 3600);

/// 一個正在下載（或正在被 seek）的 mxc。只有下載 worker 握著它：主檔與 seek 暫存檔都只有一個寫入者。
pub struct MediaDownload {
    manifest: Manifest,
    pending_name: String,
    writer: PoolWriter,
    /// seek 暫存檔：第一次 seek 才建；之前留下的（daemon 重開）在開檔時就接回來。
    seek: Option<SeekStore>,
    /// `Info` 驗過的參數：第一次要上網才問，之後每一塊都用它，🚫 不重問（/docs/design/media/media-download.md §3.2）。
    target: Option<VerifiedTarget>,
    file_size: u64,
    chunk_size: u32,
    chunk_count: u32,
}

impl MediaDownload {
    /// 開（續）這個 mxc 的主檔：有 pending 主檔就從檔案確定進度，沒有或不能續就從頭建（/docs/design/media/media-download.md §5.3）。
    ///
    /// Args:
    ///     pool: 這台 server 的池
    ///     pending_name: `cache.media_pending_name` 給的, example: "m12"
    ///     manifest: 含 mxc 與區塊（檔案金鑰、`file_size`、`chunk_size`）
    /// Return:
    ///     Ok(MediaDownload)
    ///     Err(Integrity)   區塊的 `file_size`／`chunk_size` 算不出塊數（0、或超過 u32）
    ///     Err(Io)          主檔建不了
    pub fn open(
        pool: &MediaPool,
        pending_name: &str,
        manifest: &Manifest,
    ) -> Result<MediaDownload, SdkError> {
        let file_size = manifest.file_size();
        let chunk_size = manifest.block.chunk_size;
        let count = chunk_count(file_size, chunk_size)
            .filter(|count| *count > 0)
            .ok_or_else(|| {
                SdkError::Integrity(format!(
                    "file_size {file_size} with chunk_size {chunk_size} gives no usable chunk count"
                ))
            })?;
        let writer = match pool.resume_pending(pending_name, &manifest.mxc) {
            Ok(writer) if writer.plain_len() <= file_size => writer,
            // 不在、不是 v2、別人的、比檔還長（壞了）：從頭來。
            _ => pool.create_pending(pending_name, &manifest.mxc)?,
        };
        let seek = match pool.seek_path(pending_name).exists() {
            true => SeekStore::open(pool, pending_name, &manifest.mxc, chunk_size, count).ok(),
            false => None,
        };
        Ok(MediaDownload {
            manifest: manifest.clone(),
            pending_name: pending_name.to_string(),
            writer,
            seek,
            target: None,
            file_size,
            chunk_size,
            chunk_count: count,
        })
    }

    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    pub fn chunk_count(&self) -> u32 {
        self.chunk_count
    }

    /// 主檔下一塊要拉第幾塊：涵蓋「已寫的明文長度」那一點的塊（那塊在它之前的部分會丟掉）。
    pub fn next_chunk(&self) -> u32 {
        u32::try_from(self.writer.plain_len() / u64::from(self.chunk_size)).unwrap_or(u32::MAX)
    }

    /// 主檔已經收齊整個檔（還沒收尾）。
    pub fn is_written(&self) -> bool {
        self.writer.plain_len() == self.file_size
    }

    pub fn segments_written(&self) -> u64 {
        self.writer.segments_written()
    }

    /// fsync 主檔（每 `PROGRESS_FLUSH` 一次）。
    pub fn sync(&mut self) -> Result<(), SdkError> {
        self.writer.sync()
    }

    /// 主檔往前一塊，**只用本地的**：下一塊在 seek 暫存檔就搬過來、不走網路（/docs/design/media/media-download.md §6.4）。
    ///
    /// Return:
    ///     Ok(true)    搬了一塊
    ///     Ok(false)   暫存檔沒有這一塊（或那一格解不開，已從位置表拿掉）：呼叫者上網拉
    ///     Err(Io)     主檔寫不了
    pub fn advance_from_seek_store(&mut self) -> Result<bool, SdkError> {
        let index = self.next_chunk();
        let Some(seek) = self.seek.as_mut() else {
            return Ok(false);
        };
        let Some(plain) = seek.read(index)? else {
            return Ok(false);
        };
        self.land(index, &plain)?;
        Ok(true)
    }

    /// 主檔往前一塊，從 server 拉（`Download/Read`）。驗長度、用檔案金鑰解開；解不開重拉一次，還是不行就是壞檔。
    ///
    /// Return:
    ///     Ok(())
    ///     Err(Integrity)   `Info` 對不上區塊、或這一塊連兩次驗不過：整個檔當壞檔（fail closed）
    ///     Err(Network)     線斷了：進度停在這裡，線回來之後從同一塊接著拉
    ///     Err(Server)      server 拒絕（NotFound 等）
    pub async fn advance_from_server<C: PackChannel>(
        &mut self,
        client: &mut WbfClient<C>,
    ) -> Result<(), SdkError> {
        let index = self.next_chunk();
        let plain = self.read_from_server(client, index).await?;
        self.land(index, &plain)
    }

    /// 第 `index` 塊，本地有就給：主檔已封的段、或 seek 暫存檔（/docs/design/media/media-download.md §7.2 第 3、4 列）。
    ///
    /// Return:
    ///     Ok(Some(明文))   本地有
    ///     Ok(None)         本地沒有：呼叫者現拉（`fetch_into_seek_store`）
    ///     Err(Integrity)   塊號超出 chunk_count
    ///     Err(Io)          主檔的段解不開
    pub fn read_local_chunk(&mut self, index: u32) -> Result<Option<Zeroizing<Vec<u8>>>, SdkError> {
        let (start, end) = self.chunk_range(index)?;
        if end <= self.writer.sealed_plain_len() {
            return Ok(Some(self.writer.read_sealed(start, end)?));
        }
        match self.seek.as_mut() {
            Some(seek) => seek.read(index),
            None => Ok(None),
        }
    }

    /// 現拉第 `index` 塊（seek）：驗過、append 進 seek 暫存檔、回明文（/docs/design/media/media-download.md §6.3）。
    ///
    /// Return:
    ///     Ok(明文)
    ///     Err(Integrity)／Err(Network)／Err(Server)   同 [`MediaDownload::advance_from_server`]
    ///     Err(Usage)       chunk_count 太大、不給 seek 暫存檔（`seek_store::MAX_SEEK_CHUNKS`）
    pub async fn fetch_into_seek_store<C: PackChannel>(
        &mut self,
        client: &mut WbfClient<C>,
        pool: &MediaPool,
        index: u32,
    ) -> Result<Zeroizing<Vec<u8>>, SdkError> {
        let plain = self.read_from_server(client, index).await?;
        if self.seek.is_none() {
            self.seek = Some(SeekStore::open(
                pool,
                &self.pending_name,
                &self.manifest.mxc,
                self.chunk_size,
                self.chunk_count,
            )?);
        }
        if let Some(seek) = self.seek.as_mut() {
            seek.append(index, &plain)?;
        }
        Ok(plain)
    }

    /// 收尾（/docs/design/media/media-download.md §4.1）：封最後一段、核對長度與區塊的 sha256、adopt 進池、刪 seek 暫存檔。
    ///
    /// Return:
    ///     Ok(Finished)     `hash_hex` 就是池檔名
    ///     Err(Integrity)   長度或 sha256 對不上：主檔與暫存檔都刪了，呼叫者把列 reset
    ///     Err(Io)          寫不了、搬不了
    pub fn finish(self, pool: &MediaPool) -> Result<Finished, SdkError> {
        let MediaDownload {
            manifest,
            pending_name,
            writer,
            seek,
            file_size,
            ..
        } = self;
        drop(seek);
        if writer.plain_len() != file_size {
            let plain_len = writer.plain_len();
            drop(writer);
            discard(pool, &pending_name);
            return Err(SdkError::Integrity(format!(
                "the pending file has {plain_len} bytes, file_size says {file_size}"
            )));
        }
        let finished = writer.finish()?;
        if let Some(expected) = manifest.block.sha256.as_deref() {
            if !expected.eq_ignore_ascii_case(&finished.sha256_hex) {
                discard(pool, &pending_name);
                return Err(SdkError::Integrity(format!(
                    "sha256 mismatch: block {expected}, file {}",
                    finished.sha256_hex
                )));
            }
        }
        pool.adopt(&pending_name, &finished.hash_hex)?;
        pool.discard_seek(&pending_name)?;
        Ok(finished)
    }

    /// 壞檔：主檔與 seek 暫存檔都刪（列由呼叫者 reset）。
    pub fn discard(self, pool: &MediaPool) {
        let pending_name = self.pending_name.clone();
        drop(self);
        discard(pool, &pending_name);
    }

    /// 第 `index` 塊的明文範圍 `[start, end)`。
    fn chunk_range(&self, index: u32) -> Result<(u64, u64), SdkError> {
        let len = expected_plain_len(self.file_size, self.chunk_size, index)
            .ok_or_else(|| SdkError::Integrity(format!("chunk {index} is beyond chunk_count")))?;
        let start = u64::from(index) * u64::from(self.chunk_size);
        Ok((start, start + len as u64))
    }

    /// 拉一塊：第一次先 `Info` 驗區塊；驗長度、解開；解不開重拉一次（傳輸錯），還是不行就 Integrity。
    async fn read_from_server<C: PackChannel>(
        &mut self,
        client: &mut WbfClient<C>,
        index: u32,
    ) -> Result<Zeroizing<Vec<u8>>, SdkError> {
        if self.target.is_none() {
            self.target = Some(client.verify_target(&self.manifest).await?);
        }
        let Some(target) = self.target.as_ref() else {
            return Err(SdkError::Integrity(
                "the download target was not verified".into(),
            ));
        };
        match client
            .read_and_open_chunk(&self.manifest, target, index)
            .await
        {
            Ok(plain) => Ok(Zeroizing::new(plain)),
            Err(SdkError::Integrity(_)) => Ok(Zeroizing::new(
                client
                    .read_and_open_chunk(&self.manifest, target, index)
                    .await?,
            )),
            Err(error) => Err(error),
        }
    }

    /// 一塊落地：長度要對；這一塊在「已寫長度」之前的部分丟掉（續傳點可以在塊中間），其餘餵進主檔。
    fn land(&mut self, index: u32, plain: &[u8]) -> Result<(), SdkError> {
        let (start, end) = self.chunk_range(index)?;
        if plain.len() as u64 != end - start {
            return Err(SdkError::Integrity(format!(
                "chunk {index} has {} bytes, expected {}",
                plain.len(),
                end - start
            )));
        }
        let already = self.writer.plain_len();
        if already < start || already >= end {
            return Err(SdkError::Usage(format!(
                "chunk {index} ({start}..{end}) does not continue the pending file at {already}"
            )));
        }
        let rest = plain
            .get((already - start) as usize..)
            .ok_or_else(|| SdkError::Usage(format!("chunk {index} is shorter than its skip")))?;
        std::io::Write::write_all(&mut self.writer, rest)?;
        Ok(())
    }
}

fn discard(pool: &MediaPool, pending_name: &str) {
    // 刪不掉只是佔空間，下次掃描再收；🚫 不蓋掉呼叫者要回的錯。
    let _ = pool.discard_pending(pending_name);
    let _ = pool.discard_seek(pending_name);
}

/// 快取裡完整的那份還能用嗎：檔在、開得起來（池格式 v2）、明文長度等於列的 `file_size`。
///
/// Return:
///     Some(PoolReader)   能用
///     None               列說沒完成、沒有檔名、檔不在、打不開、長度不對
pub fn open_complete(pool: &MediaPool, entry: &MediaEntry) -> Option<PoolReader> {
    if !entry.complete {
        return None;
    }
    let reader = pool.open_read(entry.pool_file.as_deref()?).ok()?;
    (reader.plain_len() == entry.file_size).then_some(reader)
}

/// `collect_garbage` 的結果。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GarbageReport {
    pub bytes_before: u64,
    pub bytes_after: u64,
    pub files_removed: u64,
    /// 該刪但還有把手開著、這一輪跳過的檔數（/docs/design/media/media-pool.md §5）。
    pub files_in_use: u64,
    /// 保護期外的候選都刪了還是超過配額（/docs/design/media/media-pool.md §5：不擋、UI 提示手動清理）。
    pub still_over_quota: bool,
}

/// /docs/design/media/media-pool.md §5：超過 `quota_bytes` 就從最舊的 `last_used_at` 開始刪，只刪保護期外的；有別的 mxc 指著的池檔不刪檔只清列。
/// 先刪檔再改 DB。
///
/// Args:
///     quota_bytes: example: 2 * 1024 * 1024 * 1024
///     protect: example: Duration::from_secs(7 * 24 * 3600)
///     now_millis: example: 1_788_000_000_000（測試好控制；正式用 `SystemTime::now()`）
pub fn collect_garbage(
    cache: &mut Cache,
    pool: &MediaPool,
    quota_bytes: u64,
    protect: Duration,
    now_millis: i64,
) -> Result<GarbageReport, SdkError> {
    let bytes_before = cache.media_bytes_on_disk()?;
    let mut report = GarbageReport {
        bytes_before,
        bytes_after: bytes_before,
        ..GarbageReport::default()
    };
    if bytes_before <= quota_bytes {
        return Ok(report);
    }
    let protect_millis = i64::try_from(protect.as_millis()).unwrap_or(i64::MAX);
    for entry in cache.list_media_by_last_used()? {
        if report.bytes_after <= quota_bytes {
            break;
        }
        if now_millis.saturating_sub(entry.last_used_at) < protect_millis {
            // 由舊到新排，第一個在保護期內的之後全都在保護期內。
            break;
        }
        let Some(pool_file) = entry.pool_file.as_deref() else {
            continue;
        };
        // 🚫 有把手開著的不刪，列也不動：正在看的東西不能從底下抽掉（Windows 上刪開著的檔還會失敗、整輪清理跟著中止）。
        if pool.is_open(pool_file)? {
            report.files_in_use += 1;
            continue;
        }
        // 這個池檔只有它一個 mxc 指著才能刪檔；否則只清這一列（空間沒省，但列要對）。
        if cache.media_references(pool_file)? <= 1 {
            pool.remove(pool_file)?;
            report.files_removed += 1;
            report.bytes_after = report.bytes_after.saturating_sub(entry.bytes_on_disk);
        }
        cache.media_reset(&entry.mxc)?;
    }
    // 遞減是估的（同一個檔的 bytes_on_disk 各列可能不同）；最後對一次 DB 才是報出去的數（PR #14 審查 cirno 💡1）。
    report.bytes_after = cache.media_bytes_on_disk()?;
    report.still_over_quota = report.bytes_after > quota_bytes;
    Ok(report)
}

/// `sweep` 的結果。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SweepReport {
    /// DB 說完整但檔不在（或不是池格式 v2）→ 列 reset 成「還沒下載」。
    pub reset_rows: u64,
    /// `pending/` 裡沒人認領、列已經完成、過了保護期、或不是池格式 v2 → 刪掉的暫存檔數（主檔與 `.seek` 都算）。
    pub removed_pending: u64,
    /// `media/<hh>/` 裡沒有任何 `media` 列指著的完成檔（`forget-account` 之後、DB 重建之後、v1 檔被 reset 之後）→ 刪掉的數。
    pub removed_orphan_files: u64,
}

/// 掃一次孤兒（/docs/design/media/media-pool.md §5 最後一條、/docs/design/media/media-download.md §4.3、§11 第 1 條），三個方向：
/// DB → 檔（說完整但打不開 → reset）、`pending/` → DB、`media/<hh>/` → DB（沒列指著 → 刪）。
///
/// Args:
///     protect: 暫存檔多久沒動過算過期, example: Duration::from_secs(7 * 24 * 3600)
///     now: 現在, example: SystemTime::now()
///     in_use: 正在下載（下載 worker 握著）的暫存名，🚫 不碰, example: {"m12"}
pub fn sweep(
    cache: &mut Cache,
    pool: &MediaPool,
    protect: Duration,
    now: std::time::SystemTime,
    in_use: &HashSet<String>,
) -> Result<SweepReport, SdkError> {
    let mut report = SweepReport::default();
    for entry in cache.list_media_by_last_used()? {
        if open_complete(pool, &entry).is_none() {
            cache.media_reset(&entry.mxc)?;
            report.reset_rows += 1;
        }
    }
    for name in pool.list_pending()? {
        let owner = name.strip_suffix(SEEK_SUFFIX).unwrap_or(&name).to_string();
        if in_use.contains(&owner) {
            continue;
        }
        let path = pool.pending_path(&name);
        let claimed = cache
            .find_media_by_pending_name(&owner)?
            .is_some_and(|entry| !entry.complete);
        let expired = std::fs::metadata(&path)
            .and_then(|metadata| metadata.modified())
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age >= protect);
        let is_old_format = !name.ends_with(SEEK_SUFFIX) && !pool.is_current_pending(&name);
        if !claimed || expired || is_old_format {
            if name.ends_with(SEEK_SUFFIX) {
                pool.discard_seek(&owner)?;
            } else {
                pool.discard_pending(&name)?;
            }
            report.removed_pending += 1;
        }
    }
    for pool_file in pool.list_files()? {
        // 沒人指著但還有把手開著（列剛被 reset、讀的人還沒關）：這一輪先留著，下次再收。
        if cache.media_references(&pool_file)? == 0 && !pool.is_open(&pool_file)? {
            pool.remove(&pool_file)?;
            report.removed_orphan_files += 1;
        }
    }
    Ok(report)
}

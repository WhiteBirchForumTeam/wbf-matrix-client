//! 媒體快取的接法：下載的一個檔（[`MediaDownload`]）的塊怎麼驗、怎麼落進主檔與 seek 暫存檔，加上配額清理與啟動掃描。
//! 設計在 /docs/design/media/media-download.md（下載）與 /docs/design/media/media-pool.md §5（清理）。
//!
//! - [`MediaDownload`]：一個 mxc 的主檔（`PoolWriter`，池格式 v2）、seek 暫存檔（`SeekStore`）與 `Info` 驗過的參數。
//!   🚫 不碰網路：`Info`／`Read` 的回覆由 core 的下載處理端（`wbf_core::download_queue`）收到後交進來（`accept_info`、`open_chunk`），
//!   這裡只管驗、解、落地（`land_chunk`、`store_seek_chunk`）與本地有沒有（`advance_from_seek_store`、`read_local_chunk`）。
//!   **誰排隊、誰先、何時取消、DB 怎麼寫**也是那邊的事。
//! - `collect_garbage`：配額 best effort、保護期內不刪、先刪檔再刪列、有人指著的池檔不刪（/docs/design/media/media-pool.md §5）。
//! - `sweep`：掃孤兒（DB 說有檔但打不開 → reset；`pending/` 裡沒人認領、過期、不是池格式 v2 → 刪；沒人指著的完成檔 → 刪）。
//!
//! 這裡是池、seek 暫存檔、`cache.db` 與下載管線唯一的交會點：`cache` 不知道池，`media_pool` 不知道 DB，下載管線不知道兩者。

use std::collections::HashSet;
use std::io::{Read, Write};
use std::path::Path;
use std::time::Duration;

use sha2::{Digest, Sha256};

use zeroize::Zeroizing;

use crate::cache::{Cache, MediaEntry};
use crate::chunk_crypto::{chunk_count, expected_plain_len};
use crate::download::{open_chunk_data, verify_info, VerifiedTarget};
use crate::error::SdkError;
use crate::manifest::Manifest;
use crate::media_pool::{Finished, MediaPool, PoolReader, PoolWriter, SEEK_SUFFIX};
use crate::protocol::InfoAck;
use crate::seek_store::SeekStore;

/// 進度快照的間隔（/docs/design/media/media-download.md §4.1：每 1.5 秒 fsync 一次並把段數寫回 DB）。
pub const PROGRESS_FLUSH: Duration = Duration::from_millis(1500);
/// /docs/design/media/media-pool.md §5 的預設。
pub const DEFAULT_QUOTA_BYTES: u64 = 2 * 1024 * 1024 * 1024;
pub const DEFAULT_PROTECT: Duration = Duration::from_secs(7 * 24 * 3600);

/// 一個正在下載（或正在被 seek）的 mxc。只有下載處理端握著它：主檔與 seek 暫存檔都只有一個寫入者。
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
        self.land_chunk(index, &plain)?;
        Ok(true)
    }

    /// Return:
    ///     bool  true ＝ 還沒用 `Info` 驗過區塊：上網拉任何一塊之前，先送 `Info`、把回覆交給 [`MediaDownload::accept_info`]（/docs/design/media/media-download.md §3.2）
    pub fn needs_info(&self) -> bool {
        self.target.is_none()
    }

    /// `Info` 的回覆到了：跟區塊核對，對得上就記下來，之後每一塊都用它、🚫 不重問。
    ///
    /// Args:
    ///     info: `protocol::info_reply` 解出來的 meta
    ///     description_data: 同一個回覆的 data
    /// Return:
    ///     Ok(())
    ///     Err(Integrity)   區塊對不上 `Info`、描述解不開：整個檔當壞檔（fail closed）
    pub fn accept_info(&mut self, info: &InfoAck, description_data: &[u8]) -> Result<(), SdkError> {
        self.target = Some(verify_info(&self.manifest, info, description_data)?);
        Ok(())
    }

    /// 第 `index` 塊的密文 → 明文（/docs/design/media/media-download.md §3.3）。
    ///
    /// Args:
    ///     index: example: 3
    ///     data: `protocol::read_reply` 給的密文
    /// Return:
    ///     Ok(明文)
    ///     Err(Integrity)   長度或標籤不對（呼叫者重拉一次，還是不行就是壞檔）
    ///     Err(Usage)       還沒 `accept_info`
    pub fn open_chunk(&self, index: u32, data: &[u8]) -> Result<Zeroizing<Vec<u8>>, SdkError> {
        let Some(target) = self.target.as_ref() else {
            return Err(SdkError::Usage(format!(
                "chunk {index} of {} arrived before Info verified the block",
                self.manifest.mxc
            )));
        };
        Ok(Zeroizing::new(open_chunk_data(
            &self.manifest,
            target,
            index,
            data,
        )?))
    }

    /// 第 `index` 塊，本地有就給：主檔已封的段、或 seek 暫存檔（/docs/design/media/media-download.md §7.2 第 3、4 列）。
    ///
    /// Return:
    ///     Ok(Some(明文))   本地有
    ///     Ok(None)         本地沒有：呼叫者現拉，拉到的交給 [`MediaDownload::store_seek_chunk`]
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

    /// seek 拉到的一塊：append 進 seek 暫存檔（第一次才建檔，/docs/design/media/media-download.md §6.3）。
    ///
    /// Return:
    ///     Ok(())
    ///     Err(Usage)       chunk_count 太大、不給 seek 暫存檔（`seek_store::MAX_SEEK_CHUNKS`）
    ///     Err(Io)          暫存檔寫不了
    pub fn store_seek_chunk(
        &mut self,
        pool: &MediaPool,
        index: u32,
        plain: &[u8],
    ) -> Result<(), SdkError> {
        if self.seek.is_none() {
            self.seek = Some(SeekStore::open(
                pool,
                &self.pending_name,
                &self.manifest.mxc,
                self.chunk_size,
                self.chunk_count,
            )?);
        }
        match self.seek.as_mut() {
            Some(seek) => seek.append(index, plain),
            None => Ok(()),
        }
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

    /// 主檔往前一塊：長度要對；這一塊在「已寫長度」之前的部分丟掉（續傳點可以在塊中間），其餘餵進主檔。
    ///
    /// Args:
    ///     index: 要是主檔的下一塊（`next_chunk()`）, example: 3
    ///     plain: 那一塊的明文（`open_chunk` 給的、或暫存檔裡的）
    /// Return:
    ///     Ok(())
    ///     Err(Integrity)   長度不對
    ///     Err(Usage)       不是主檔的下一塊
    ///     Err(Io)          主檔寫不了
    pub fn land_chunk(&mut self, index: u32, plain: &[u8]) -> Result<(), SdkError> {
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

/// 快取列跟這次的區塊說的是不是同一個檔（/docs/design/media/media-download.md §5.3，維護者 2026-10-02 照 PR #14 的規則）：
/// 大小、塊大小要一樣；兩邊都帶 sha256 就要一樣。🚫 算 hash，只比記下來的字。
///
/// Return:
///     bool  true ＝ 同一個檔，列可以照用；false ＝ 對不上，要丟掉重來（[`forget_for_new_description`]）
pub fn is_same_file(entry: &MediaEntry, manifest: &Manifest) -> bool {
    let same_size =
        entry.file_size == manifest.file_size() && entry.chunk_size == manifest.block.chunk_size;
    let recorded_sha256 = entry
        .hash
        .as_deref()
        .and_then(|hash| hash.strip_prefix("sha256:"));
    let same_sha256 = match (recorded_sha256, manifest.block.sha256.as_deref()) {
        (Some(recorded), Some(claimed)) => recorded.eq_ignore_ascii_case(claimed),
        _ => true,
    };
    same_size && same_sha256
}

/// 列說的跟這次的區塊不一樣（[`is_same_file`] 回 false）：丟掉這個 mxc 在本地的一切，列換成這次的描述、從頭來。
/// 池檔還有別的 mxc 指著、或有人正在讀，就只清這一列不刪檔；主檔與 seek 暫存檔刪掉。⚠️ 呼叫者先確定沒人正在下載它。
///
/// Return:
///     Ok(())
///     Err(Io)   DB 或檔案動不了
pub fn forget_for_new_description(
    cache: &mut Cache,
    pool: &MediaPool,
    manifest: &Manifest,
) -> Result<(), SdkError> {
    let Some(entry) = cache.find_media(&manifest.mxc)? else {
        return Ok(());
    };
    if let Some(pool_file) = entry.pool_file.as_deref() {
        if cache.media_references(pool_file)? <= 1 && !pool.is_open(pool_file)? {
            pool.remove(pool_file)?;
        }
    }
    if let Some(pending_name) = cache.media_pending_name(&manifest.mxc)? {
        discard(pool, &pending_name);
    }
    let block = &manifest.block;
    cache.media_redescribe(
        &manifest.mxc,
        block.name.as_deref(),
        block.mimetype.as_deref(),
        block.sha256.as_deref(),
        manifest.file_size(),
        block.chunk_size,
    )
}

/// 匯出時要對上的內容（/docs/design/media/media-download.md §7.1 的 `media.export_to`）。至少要有一個 hash：沒東西可比就🚫 匯出。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExpectedContent {
    pub file_size: u64,
    /// 明文的 BLAKE3（池檔的檔名就是它）
    pub blake3_hex: Option<String>,
    /// 區塊帶的 sha256（上傳者算的）
    pub sha256_hex: Option<String>,
}

/// 把明文整個寫到 `to`，邊寫邊算整檔的 BLAKE3／SHA-256（/docs/design/media/media-download.md §7.1）：先寫 `<to>.partial`、fsync，
/// 大小與每個給了的 hash 都對上才改名成 `to`；對不上就刪掉暫存、`to` 🚫 被動過。
///
/// Args:
///     source: 明文（池檔的 `PoolReader`，或本機原檔）
///     to: 使用者指定的位置, example: "C:/Users/me/v.mp4"
/// Return:
///     Ok(u64)          寫了幾 byte
///     Err(Integrity)   大小或 hash 對不上
///     Err(Usage)       一個 hash 都沒給、`to` 沒有檔名
///     Err(Io)          讀不了來源、寫不了、改不了名
pub fn export_verified<R: Read>(
    mut source: R,
    to: &Path,
    expected: &ExpectedContent,
) -> Result<u64, SdkError> {
    if expected.blake3_hex.is_none() && expected.sha256_hex.is_none() {
        return Err(SdkError::Usage(
            "nothing to verify the export against (no BLAKE3, no sha256)".into(),
        ));
    }
    let mut partial_name = to
        .file_name()
        .ok_or_else(|| SdkError::Usage(format!("{} has no file name", to.display())))?
        .to_os_string();
    partial_name.push(".partial");
    let partial = to.with_file_name(partial_name);
    let written = (|| {
        let mut file = std::fs::File::create(&partial)?;
        let mut blake3 = blake3::Hasher::new();
        let mut sha256 = Sha256::new();
        let mut buffer = Zeroizing::new(vec![0u8; 1 << 16]);
        let mut bytes = 0u64;
        loop {
            let read = source.read(&mut buffer)?;
            let Some(piece) = buffer.get(..read).filter(|piece| !piece.is_empty()) else {
                break;
            };
            blake3.update(piece);
            sha256.update(piece);
            file.write_all(piece)?;
            bytes += piece.len() as u64;
        }
        file.sync_all()?;
        if bytes != expected.file_size {
            return Err(SdkError::Integrity(format!(
                "exported {bytes} bytes, file_size says {}",
                expected.file_size
            )));
        }
        if let Some(wanted) = expected.blake3_hex.as_deref() {
            let actual = blake3.finalize().to_hex().to_string();
            if !wanted.eq_ignore_ascii_case(&actual) {
                return Err(SdkError::Integrity(format!(
                    "BLAKE3 mismatch: expected {wanted}, got {actual}"
                )));
            }
        }
        if let Some(wanted) = expected.sha256_hex.as_deref() {
            let actual = hex::encode(sha256.finalize());
            if !wanted.eq_ignore_ascii_case(&actual) {
                return Err(SdkError::Integrity(format!(
                    "sha256 mismatch: expected {wanted}, got {actual}"
                )));
            }
        }
        Ok(bytes)
    })();
    let renamed = written.and_then(|bytes| {
        std::fs::rename(&partial, to)?;
        Ok(bytes)
    });
    if renamed.is_err() {
        let _ = std::fs::remove_file(&partial);
    }
    renamed
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
///     in_use: 正在下載（下載處理端握著）的暫存名，🚫 不碰, example: {"m12"}
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

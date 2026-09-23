//! 媒體快取的接法（local-cache-db.md §8.3、§8.5、§8.7）：下載管線、儲存池（`media_pool`）與 `cache.db`（`cache`）三者怎麼一起動。
//!
//! - `fetch`：快取有完整檔就從池開；沒有就邊下邊 append 進池，進度在記憶體、每 `PROGRESS_FLUSH` 快照一次到 DB，
//!   中斷從上次快照的塊數續（檔截到那裡，之後的不信）。完成算 hash → adopt 進池（同 hash 去重）→ DB 寫齊。
//! - `collect_garbage`：配額 best effort、保護期內不刪、先刪檔再刪列、有人指著的池檔不刪（§8.5）。
//! - `sweep`：啟動掃孤兒（DB 說有檔不在 → reset；pending 沒對應列 → 刪）。
//!
//! 這裡是三個模組唯一的交會點：`cache` 不知道池，`media_pool` 不知道 DB，下載管線不知道兩者。

use std::io::Write;
use std::time::{Duration, Instant};

use crate::cache::{Cache, MediaEntry};
use crate::channel::PackChannel;
use crate::client::WbfClient;
use crate::error::SdkError;
use crate::manifest::Manifest;
use crate::media_pool::{MediaPool, PoolReader};

/// 進度快照的間隔（§8.3：每 1–2 秒）。
pub const PROGRESS_FLUSH: Duration = Duration::from_millis(1500);
/// §8.5 的預設。
pub const DEFAULT_QUOTA_BYTES: u64 = 2 * 1024 * 1024 * 1024;
pub const DEFAULT_PROTECT: Duration = Duration::from_secs(7 * 24 * 3600);

/// `fetch` 回的：檔在池裡了，加上這次做了什麼。
#[derive(Debug)]
pub struct Fetched {
    pub entry: MediaEntry,
    pub outcome: FetchOutcome,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FetchOutcome {
    /// 快取本來就有完整檔，沒碰網路。
    CacheHit,
    /// 這次下載的（`resumed_from` 是續傳起點的塊數，0 = 從頭）。
    Downloaded { chunks: u32, resumed_from: u64 },
}

/// 拿一個媒體：快取命中就直接回，沒有就下載進池。回來的 `entry.pool_file` 一定是 `Some`。
///
/// Args:
///     manifest: 含 mxc、block（key、chunk_size、file_size、name、mimetype）
///     on_progress: (已完成的塊, 總塊數)
/// Return:
///     Ok(Fetched)
///     Err(Integrity)   某塊驗不過：這次下載中止、檔截回上次快照，下次續
///     Err(Server／Network)  照下載管線的
pub async fn fetch<C: PackChannel>(
    client: &mut WbfClient<C>,
    manifest: &Manifest,
    cache: &mut Cache,
    pool: &MediaPool,
    on_progress: &mut (dyn FnMut(u32, u32) + Send),
) -> Result<Fetched, SdkError> {
    let block = &manifest.block;
    let entry = cache.media_begin(
        &manifest.mxc,
        block.name.as_deref(),
        block.mimetype.as_deref(),
        block.sha256.as_deref(),
        manifest.file_size(),
        block.chunk_size,
    )?;
    if entry.complete {
        // DB 說有：檔也要真的在、長度要對、校驗碼要跟這份 manifest 的區塊一致（§8.3「兩邊都問」；PR #14 審查 rumia 🟡1）。
        // 任一不符就當沒快取，reset 後重下。
        if cached_copy_matches(pool, &entry, manifest) {
            cache.touch_media(&manifest.mxc)?;
            return Ok(Fetched {
                entry,
                outcome: FetchOutcome::CacheHit,
            });
        }
        cache.media_reset(&manifest.mxc)?;
    }
    let pending_name = cache
        .media_pending_name(&manifest.mxc)?
        .ok_or_else(|| SdkError::Io(std::io::Error::other("media row vanished")))?;

    // 續傳點：上次快照的塊數，而且塊大小要一樣（不一樣就等於沒得續）。檔截到那裡；截不了就從頭。
    let chunk_size = block.chunk_size;
    let resume_chunks = if entry.chunks_written > 0 && entry.chunk_size == chunk_size {
        entry.chunks_written
    } else {
        0
    };
    let (mut writer, resumed_from) = match resume_chunks {
        0 => (pool.create_pending(&pending_name)?, 0),
        chunks => match pool.resume_pending(&pending_name, chunks * u64::from(chunk_size)) {
            Ok(writer) => (writer, chunks),
            Err(_) => (pool.create_pending(&pending_name)?, 0),
        },
    };
    if resumed_from == 0 {
        cache.media_progress(&manifest.mxc, 0, chunk_size)?;
    }

    let target = client.verify_target(manifest).await?;
    let mut progress = ProgressFlusher::new();
    let mut written_chunks = resumed_from;
    let result: Result<(), SdkError> = async {
        for index in (resumed_from as u32)..target.chunk_count {
            let plain = client.read_and_open_chunk(manifest, &target, index).await?;
            writer.write_all(&plain)?;
            written_chunks += 1;
            on_progress(index + 1, target.chunk_count);
            if progress.due() {
                writer.sync()?;
                cache.media_progress(&manifest.mxc, written_chunks, chunk_size)?;
                progress.flushed();
            }
        }
        Ok(())
    }
    .await;
    if let Err(error) = result {
        // 中止：DB 停在上次快照（不往前推），檔留著給下次續。呼叫者看到的錯誤就是下載管線的；
        // 這裡的 sync 失敗不能蓋掉它（PR #14 審查 rumia 🟢1）。
        let _ = writer.sync();
        drop(writer);
        return Err(error);
    }
    if writer.plain_len() != target.file_size {
        let plain_len = writer.plain_len();
        drop(writer);
        cache.media_progress(&manifest.mxc, 0, chunk_size)?;
        pool.discard_pending(&pending_name)?;
        return Err(SdkError::Integrity(format!(
            "pool file has {plain_len} bytes, file_size says {}",
            target.file_size
        )));
    }
    let finished = writer.finish()?;
    // 明文 sha256（約定 §3.1 第 5 條）由 verify 過的塊逐塊保證；這裡另有 BLAKE3 當檔名。
    pool.adopt(&pending_name, &finished.hash_hex)?;
    let bytes_on_disk = pool.bytes_on_disk(&finished.hash_hex)?;
    cache.media_finish(
        &manifest.mxc,
        &finished.hash_hex,
        u64::from(target.chunk_count),
        finished.plain_len,
        bytes_on_disk,
    )?;
    let entry = cache
        .find_media(&manifest.mxc)?
        .ok_or_else(|| SdkError::Io(std::io::Error::other("media row vanished")))?;
    Ok(Fetched {
        entry,
        outcome: FetchOutcome::Downloaded {
            chunks: target.chunk_count - resumed_from as u32,
            resumed_from,
        },
    })
}

/// 快取命中的三個條件：檔在池裡開得起來、明文長度等於區塊的 `file_size`、區塊帶 sha256 時要跟 `media.hash` 一致。
/// 檔名本身是明文 BLAKE3，所以「內容跟 hash 對不對」由 finish 時算的 hash 保證；這裡擋的是列與檔對不上、或同一個 mxc 換了區塊。
fn cached_copy_matches(pool: &MediaPool, entry: &MediaEntry, manifest: &Manifest) -> bool {
    let Some(pool_file) = entry.pool_file.as_deref() else {
        return false;
    };
    let Ok(reader) = pool.open_read(pool_file) else {
        return false;
    };
    if reader.plain_len() != manifest.file_size() || entry.file_size != manifest.file_size() {
        return false;
    }
    match (&entry.hash, &manifest.block.sha256) {
        // 存的不是 `sha256:` 開頭（別種雜湊）就不比；是就逐字比。
        (Some(stored), Some(sha256)) => stored
            .strip_prefix("sha256:")
            .is_none_or(|digest| digest.eq_ignore_ascii_case(sha256)),
        _ => true,
    }
}

/// 開快取裡的完整檔來讀；沒有或檔不在回 None（呼叫者去 `fetch`）。讀一次就 touch。
pub fn open_cached(
    cache: &mut Cache,
    pool: &MediaPool,
    mxc: &str,
) -> Result<Option<PoolReader>, SdkError> {
    let Some(entry) = cache.find_media(mxc)? else {
        return Ok(None);
    };
    if !entry.complete {
        return Ok(None);
    }
    let Some(pool_file) = entry.pool_file.as_deref() else {
        return Ok(None);
    };
    match pool.open_read(pool_file) {
        Ok(reader) => {
            cache.touch_media(mxc)?;
            Ok(Some(reader))
        }
        Err(_) => {
            cache.media_reset(mxc)?;
            Ok(None)
        }
    }
}

/// `collect_garbage` 的結果。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GarbageReport {
    pub bytes_before: u64,
    pub bytes_after: u64,
    pub files_removed: u64,
    /// 保護期外的候選都刪了還是超過配額（§8.5：不擋、UI 提示手動清理）。
    pub still_over_quota: bool,
}

/// §8.5：超過 `quota_bytes` 就從最舊的 `last_used_at` 開始刪，只刪保護期外的；有別的 mxc 指著的池檔不刪檔只清列。
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
    /// DB 說完整但檔不在 → 列 reset 成「還沒下載」。
    pub reset_rows: u64,
    /// `pending/` 裡沒人認領、或半成品過了保護期 → 刪掉的暫存檔數。
    pub removed_pending: u64,
    /// `media/<hh>/` 裡沒有任何 `media` 列指著的完成檔（`forget-account` 之後、或 DB 重建之後留下的）→ 刪掉的數。
    pub removed_orphan_files: u64,
}

/// 啟動時掃一次（§8.5 最後一條），三個方向：DB → 檔（說完整但檔不在 → reset）、`pending/` → DB（沒列認領或過保護期 → 刪）、
/// `media/<hh>/` → DB（沒列指著的完成檔 → 刪；`forget_account` 刪掉孤兒列之後就是這裡收檔，PR #14 審查 rumia／salvia 🟡）。
pub fn sweep(
    cache: &mut Cache,
    pool: &MediaPool,
    protect: Duration,
    now_millis: i64,
) -> Result<SweepReport, SdkError> {
    let mut reset_rows = 0;
    let mut removed_pending = 0;
    for entry in cache.list_media_by_last_used()? {
        let present = entry
            .pool_file
            .as_deref()
            .map(|pool_file| pool.open_read(pool_file).is_ok())
            .unwrap_or(false);
        if !present {
            cache.media_reset(&entry.mxc)?;
            reset_rows += 1;
        }
    }
    let protect_millis = i64::try_from(protect.as_millis()).unwrap_or(i64::MAX);
    let mut live_pending = std::collections::HashSet::new();
    for entry in cache.list_media_incomplete()? {
        let expired = now_millis.saturating_sub(entry.created_at) >= protect_millis;
        let name = cache.media_pending_name(&entry.mxc)?.unwrap_or_default();
        if expired {
            pool.discard_pending(&name)?;
            cache.media_reset(&entry.mxc)?;
            removed_pending += 1;
        } else {
            live_pending.insert(name);
        }
    }
    for name in pool.list_pending()? {
        if !live_pending.contains(&name) {
            pool.discard_pending(&name)?;
            removed_pending += 1;
        }
    }
    let mut removed_orphan_files = 0;
    for pool_file in pool.list_files()? {
        if cache.media_references(&pool_file)? == 0 {
            pool.remove(&pool_file)?;
            removed_orphan_files += 1;
        }
    }
    Ok(SweepReport {
        reset_rows,
        removed_pending,
        removed_orphan_files,
    })
}

/// 每 `PROGRESS_FLUSH` 才寫一次 DB 的小計時器。
struct ProgressFlusher {
    last_flush: Instant,
}

impl ProgressFlusher {
    fn new() -> ProgressFlusher {
        ProgressFlusher {
            last_flush: Instant::now(),
        }
    }

    fn due(&self) -> bool {
        self.last_flush.elapsed() >= PROGRESS_FLUSH
    }

    fn flushed(&mut self) {
        self.last_flush = Instant::now();
    }
}

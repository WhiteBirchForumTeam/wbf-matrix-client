//! 媒體儲存池（/docs/design/media/media-pool.md）：對上層是「開檔、順序 append、讀、刪」的完整明文檔；落地時整個池加密，一把金鑰
//! （`Vault::media_store_key()`）。
//!
//! 落地格式是池格式 v2（/docs/design/media/media-download.md §4.1）：
//!
//! ```text
//! 檔頭 32 byte：magic "WBFP"(4) ‖ version u8 = 2 ‖ 保留 3 byte ‖ segment_size u32 LE ‖ nonce_base 16 byte ‖ owner u32 LE
//! 之後：第 i 段 = XChaCha20-Poly1305(key, nonce = nonce_base ‖ u64_le(i), aad = "wbf-media-pool v2" ‖ nonce_base ‖ u64_le(i),
//!                                     明文 = u32_le(len) ‖ data(len) ‖ 0 × (segment_size − len))
//!       每段在磁碟上都是 R = 4 + segment_size + 16 byte：檔長一定是 32 + n × R，不是就是最後一段寫到一半。
//! ```
//!
//! - 順序 append：`PoolWriter` 湊滿一段就封一段；`finish()` 封最後一段（補滿、記真實長度）、fsync、回明文的 BLAKE3 與 SHA-256。
//! - 續傳：`MediaPool::resume_pending` 截掉寫到一半的段、從第 0 段起逐段解開，碰到第一個解不開的就截到它前面；完整的段就是進度。
//! - 每段只封一次：nonce 由段號決定，續傳時重封的那段明文一定一樣（就是檔案內容）。`owner` 是 mxc 的 BLAKE3 前 4 byte：
//!   同一個暫存名換了主人（`cache.db` 重建後 id 重新編號）就不續，🚫 不把別的檔的段接進來、🚫 不拿同一個 nonce 封別的明文。
//! - 檔名是明文 hash（/docs/design/media/media-pool.md §2），寫完才知道；寫的時候用呼叫者給的暫存名，`finish()` 回 hash 由呼叫者 rename（`adopt`）。
//!
//! 🚫 金鑰不進錯誤訊息、不 log。這裡沒有 SQL、沒有網路。

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

use crate::error::SdkError;
use crate::vault::Key32;

pub const POOL_DIR_NAME: &str = "media";
/// 下載中的暫存檔放這裡（/docs/design/media/media-pool.md §2：暫存名用 `media.id`）。
pub const PENDING_DIR_NAME: &str = "pending";
/// seek 暫存檔的副檔名：`pending/m<id>.seek`（/docs/design/media/media-download.md §4.3）。
pub const SEEK_SUFFIX: &str = ".seek";

const MAGIC: &[u8; 4] = b"WBFP";
pub const POOL_FORMAT_VERSION: u8 = 2;
const HEADER_LEN: u64 = 32;
const NONCE_BASE_LEN: usize = 16;
const TAG_LEN: u64 = 16;
const LEN_FIELD: u64 = 4;
const AAD_PREFIX: &[u8] = b"wbf-media-pool v2";
/// 明文段大小。64 KiB：跟最小的常見 chunk_size 一樣，每段多 20 byte（長度欄＋標籤），隨機讀一段只解 64 KiB。
pub const SEGMENT_SIZE: u32 = 65536;

/// 這個程序裡開著的讀把手：完成檔的路徑 → 幾個 `PoolReader` 開著它。
/// 🚫 清理（`collect_garbage`、`sweep`）不刪還有把手開著的檔（/docs/design/media/media-pool.md §5）。
/// 記在程序層級、不記在 `MediaPool` 上：池每次用都是新開一個值，把手卻活得比它久。
/// 只看這個程序就夠：資料目錄綁定 daemon，別的程序不准碰（/docs/design/overview/architecture-v2.md §0.2）。
static OPEN_READERS: std::sync::OnceLock<std::sync::Mutex<HashMap<PathBuf, usize>>> =
    std::sync::OnceLock::new();

fn open_readers() -> std::sync::MutexGuard<'static, HashMap<PathBuf, usize>> {
    OPEN_READERS
        .get_or_init(|| std::sync::Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// 一個池：目錄加金鑰。
pub struct MediaPool {
    dir: PathBuf,
    key: Key32,
}

/// 檔頭（開檔時讀一次）。
#[derive(Clone, Copy)]
struct Header {
    segment_size: u32,
    nonce_base: [u8; NONCE_BASE_LEN],
    owner: u32,
}

impl MediaPool {
    /// Args:
    ///     server_dir: example: "<data dir>/s/<b58>_<b58>"（池在它底下的 media/）
    ///     key: example: vault.media_store_key()
    pub fn open(server_dir: &Path, key: Key32) -> Result<MediaPool, SdkError> {
        let dir = server_dir.join(POOL_DIR_NAME);
        std::fs::create_dir_all(dir.join(PENDING_DIR_NAME))?;
        Ok(MediaPool { dir, key })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// 池金鑰（seek 暫存檔用同一把，/docs/design/media/media-download.md §4.2）。🚫 不出 sdk。
    pub(crate) fn key(&self) -> &Key32 {
        &self.key
    }

    /// 完成檔的位置：`media/<hash 前 2 hex>/<hash>`。
    pub fn path_of(&self, pool_file: &str) -> Result<PathBuf, SdkError> {
        if pool_file.len() < 4 || !pool_file.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(SdkError::Usage(format!(
                "pool file name {pool_file:?} is not a hex hash"
            )));
        }
        let shard = pool_file.get(..2).ok_or_else(|| {
            SdkError::Usage(format!("pool file name {pool_file:?} is shorter than 2"))
        })?;
        Ok(self.dir.join(shard).join(pool_file))
    }

    /// 下載中的主檔位置。
    pub fn pending_path(&self, pending_name: &str) -> PathBuf {
        self.dir.join(PENDING_DIR_NAME).join(pending_name)
    }

    /// 這個主檔旁邊的 seek 暫存檔位置。
    pub fn seek_path(&self, pending_name: &str) -> PathBuf {
        self.dir
            .join(PENDING_DIR_NAME)
            .join(format!("{pending_name}{SEEK_SUFFIX}"))
    }

    /// 開一個新的主檔從頭寫（已有同名的就覆蓋）。
    ///
    /// Args:
    ///     pending_name: example: "m12"
    ///     mxc: 這個主檔是誰的（進檔頭的 `owner`）, example: "mxc://localhost/000000000000004d"
    pub fn create_pending(&self, pending_name: &str, mxc: &str) -> Result<PoolWriter, SdkError> {
        let path = self.pending_path(pending_name);
        let header = Header {
            segment_size: SEGMENT_SIZE,
            nonce_base: random_nonce_base()?,
            owner: owner_of(mxc),
        };
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)?;
        file.write_all(&encode_header(&header))?;
        Ok(PoolWriter::new(self, header, file, path))
    }

    /// 接著寫一個主檔（/docs/design/media/media-download.md §4.1 的續傳）：截掉寫到一半的段、逐段解開，碰到第一個解不開的就截到它前面。
    /// 檔已經寫完（最後一段是短的）時，那一段解回記憶體，`plain_len()` 就是整個檔，呼叫者直接 `finish()`。
    ///
    /// Args:
    ///     pending_name: example: "m12"
    ///     mxc: 要續的是誰的；跟檔頭的 `owner` 對不上就不續, example: "mxc://localhost/000000000000004d"
    /// Return:
    ///     Ok(PoolWriter)   定位在最後一個完整段之後，BLAKE3／SHA-256 已算到那裡
    ///     Err(Io)          檔不在、不是池格式 v2（v1 一律不續，/docs/design/media/media-download.md §11 第 1 條）、
    ///                      段大小不是這一版的、owner 對不上——呼叫者從頭來（`create_pending`）
    pub fn resume_pending(&self, pending_name: &str, mxc: &str) -> Result<PoolWriter, SdkError> {
        let path = self.pending_path(pending_name);
        let mut file = OpenOptions::new().read(true).write(true).open(&path)?;
        let header = read_header(&mut file)?;
        if header.segment_size != SEGMENT_SIZE {
            return Err(pool_error(format!(
                "pending file has segment_size {}, this build writes {SEGMENT_SIZE}",
                header.segment_size
            )));
        }
        if header.owner != owner_of(mxc) {
            return Err(pool_error(
                "pending file belongs to another media (its owner tag differs)".into(),
            ));
        }
        let mut writer = PoolWriter::new(self, header, file, path);
        let record = record_len(&header);
        let on_disk = writer.file.metadata()?.len().saturating_sub(HEADER_LEN);
        let records = on_disk / record;
        for index in 0..records {
            let Ok(segment) = open_segment(&writer.cipher, &header, &mut writer.file, index) else {
                break;
            };
            let data = segment.data()?;
            writer.hash(data);
            if segment.len == u64::from(header.segment_size) {
                writer.segments_written = index + 1;
                continue;
            }
            // 短段只能是最後一段（finish 封的）：後面還有東西就是壞的，從它截掉。
            if index + 1 == records {
                writer.buffer.extend_from_slice(data);
                writer.holds_final_segment = true;
            } else {
                writer.rehash_up_to(index)?;
            }
            break;
        }
        writer.truncate_to_written()?;
        Ok(writer)
    }

    /// 主檔寫完了：搬到 `media/<hh>/<hash>`。已經有同 hash 的檔（別的 mxc 同內容）就丟掉主檔、用既有的（去重，/docs/design/media/media-pool.md §2）。
    ///
    /// Return:
    ///     Ok(bool)   true = 這次搬進去的；false = 池裡本來就有
    pub fn adopt(&self, pending_name: &str, hash_hex: &str) -> Result<bool, SdkError> {
        let target = self.path_of(hash_hex)?;
        let pending = self.pending_path(pending_name);
        if target.exists() && self.open_read(hash_hex).is_ok() {
            std::fs::remove_file(&pending)?;
            return Ok(false);
        }
        let pool_dir = target
            .parent()
            .ok_or_else(|| std::io::Error::other("pool file path has no parent directory"))?;
        std::fs::create_dir_all(pool_dir)?;
        std::fs::rename(&pending, &target)?;
        Ok(true)
    }

    /// 這個主檔是不是這一版的池格式（掃描用：v1 一律當壞檔，/docs/design/media/media-download.md §11 第 1 條）。
    ///
    /// Return:
    ///     bool  true ＝ 檔頭是池格式 v2；false ＝ 不在、太短、magic 不對、版本不是 2
    pub fn is_current_pending(&self, pending_name: &str) -> bool {
        File::open(self.pending_path(pending_name))
            .ok()
            .is_some_and(|mut file| read_header(&mut file).is_ok())
    }

    /// 刪主檔（不在也算成功）。seek 暫存檔另外刪（`discard_seek`）。
    pub fn discard_pending(&self, pending_name: &str) -> Result<(), SdkError> {
        remove_if_exists(&self.pending_path(pending_name))
    }

    /// 刪 seek 暫存檔（不在也算成功）。
    pub fn discard_seek(&self, pending_name: &str) -> Result<(), SdkError> {
        remove_if_exists(&self.seek_path(pending_name))
    }

    /// 讀一個完成檔：`Read + Seek` 的把手，位置是明文位置。
    ///
    /// Return:
    ///     Ok(PoolReader)
    ///     Err(Io)      檔不在、不是池格式 v2、檔長不是 32 + n × R、沒有段、最後一段解不開或長度不合
    pub fn open_read(&self, pool_file: &str) -> Result<PoolReader, SdkError> {
        let path = self.path_of(pool_file)?;
        let mut file = File::open(&path)?;
        let header = read_header(&mut file)?;
        let cipher = XChaCha20Poly1305::new(self.key.as_bytes().into());
        let body = file.metadata()?.len().saturating_sub(HEADER_LEN);
        let record = record_len(&header);
        if body == 0 || body % record != 0 {
            return Err(pool_error(format!(
                "a complete pool file must hold whole segments, this one has {body} bytes after the header"
            )));
        }
        let segments = body / record;
        let last = open_segment(&cipher, &header, &mut file, segments - 1)?;
        let plain_len = (segments - 1) * u64::from(header.segment_size) + last.len;
        *open_readers().entry(path.clone()).or_insert(0) += 1;
        Ok(PoolReader {
            path,
            cipher,
            header,
            file,
            segments,
            plain_len,
            position: 0,
            loaded_segment: None,
        })
    }

    /// 這個程序裡還有沒有 `PoolReader` 開著這個完成檔（清理要跳過它）。
    ///
    /// Args:
    ///     pool_file: 完成檔的 hash, example: "9f86d081884c7d65…"
    /// Return:
    ///     Ok(true)    有把手開著
    ///     Ok(false)   沒有
    ///     Err(Usage)  名字不是合法的 hash
    pub fn is_open(&self, pool_file: &str) -> Result<bool, SdkError> {
        let path = self.path_of(pool_file)?;
        Ok(open_readers().get(&path).is_some_and(|count| *count > 0))
    }

    /// 完成檔在磁碟上的大小（配額算 `bytes_on_disk` 用）。
    pub fn bytes_on_disk(&self, pool_file: &str) -> Result<u64, SdkError> {
        Ok(std::fs::metadata(self.path_of(pool_file)?)?.len())
    }

    /// 刪一個完成檔；不在也算成功。**先問 DB 有沒有別的 mxc 還指著它**（`media_by_pool_file`），這裡不問。
    pub fn remove(&self, pool_file: &str) -> Result<(), SdkError> {
        remove_if_exists(&self.path_of(pool_file)?)
    }

    /// 掃 `media/<hh>/`：所有完成檔的 hash（啟動時對照 DB 清沒人指的孤兒用）。
    pub fn list_files(&self) -> Result<Vec<String>, SdkError> {
        let mut names = Vec::new();
        for shard in std::fs::read_dir(&self.dir)? {
            let shard = shard?;
            let shard_name = shard.file_name().to_string_lossy().into_owned();
            if !shard.file_type()?.is_dir()
                || shard_name == PENDING_DIR_NAME
                || shard_name.len() != 2
            {
                continue;
            }
            for entry in std::fs::read_dir(shard.path())? {
                let entry = entry?;
                if entry.file_type()?.is_file() {
                    names.push(entry.file_name().to_string_lossy().into_owned());
                }
            }
        }
        names.sort();
        Ok(names)
    }

    /// 掃 `pending/`：所有暫存檔名（主檔 `m<id>` 與 seek 暫存檔 `m<id>.seek` 都在；啟動時對照 DB 清孤兒用）。
    pub fn list_pending(&self) -> Result<Vec<String>, SdkError> {
        let mut names = Vec::new();
        for entry in std::fs::read_dir(self.dir.join(PENDING_DIR_NAME))? {
            let entry = entry?;
            if entry.file_type()?.is_file() {
                names.push(entry.file_name().to_string_lossy().into_owned());
            }
        }
        names.sort();
        Ok(names)
    }
}

/// 順序 append 的把手。`Write` 收明文；`finish()` 才算完成。中途 drop 就是半成品（可以 `resume_pending`）。
pub struct PoolWriter {
    cipher: XChaCha20Poly1305,
    header: Header,
    file: File,
    path: PathBuf,
    /// 已經封好、落在檔裡的完整段數：這就是進度（/docs/design/media/media-download.md §4.1）。
    segments_written: u64,
    /// 還沒湊滿一段的明文。
    buffer: Zeroizing<Vec<u8>>,
    blake3: blake3::Hasher,
    sha256: Sha256,
    plain_len: u64,
    /// 續傳時載回的是 `finish` 封過的最後一段：檔已經完整，🚫 不准再寫（同一個段號會封到不同明文）。
    holds_final_segment: bool,
}

/// `finish()` 的結果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Finished {
    /// 明文的 BLAKE3，64 位小寫 hex；就是池裡的檔名。
    pub hash_hex: String,
    /// 明文的 SHA-256，64 位小寫 hex（跟區塊的 `sha256` 比，/docs/design/media/wbf-client-convention-for-chunk.md §3.1 第 5 條）。
    pub sha256_hex: String,
    pub plain_len: u64,
    /// 整個檔的段數（含最後一段）。
    pub segments: u64,
    pub bytes_on_disk: u64,
}

impl PoolWriter {
    fn new(pool: &MediaPool, header: Header, file: File, path: PathBuf) -> PoolWriter {
        PoolWriter {
            cipher: XChaCha20Poly1305::new(pool.key.as_bytes().into()),
            header,
            file,
            path,
            segments_written: 0,
            buffer: Zeroizing::new(Vec::with_capacity(header.segment_size as usize)),
            blake3: blake3::Hasher::new(),
            sha256: Sha256::new(),
            plain_len: 0,
            holds_final_segment: false,
        }
    }

    /// 目前已收下的明文長度（含還沒封的 buffer）。
    pub fn plain_len(&self) -> u64 {
        self.plain_len
    }

    /// 已經落地的完整段數。
    pub fn segments_written(&self) -> u64 {
        self.segments_written
    }

    /// 已經落地、讀得回來的明文長度（完整段 × segment_size）。
    pub fn sealed_plain_len(&self) -> u64 {
        self.segments_written * u64::from(self.header.segment_size)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn hash(&mut self, data: &[u8]) {
        self.blake3.update(data);
        self.sha256.update(data);
        self.plain_len += data.len() as u64;
    }

    /// 續傳時在第 `index` 段發現壞東西：雜湊得從頭重算到 `index` 之前（只有中間出現短段這種壞檔會走到）。
    fn rehash_up_to(&mut self, index: u64) -> Result<(), SdkError> {
        self.blake3 = blake3::Hasher::new();
        self.sha256 = Sha256::new();
        self.plain_len = 0;
        self.segments_written = index;
        for kept in 0..index {
            let segment = open_segment(&self.cipher, &self.header, &mut self.file, kept)?;
            let data = segment.data()?.to_vec();
            self.blake3.update(&data);
            self.sha256.update(&data);
            self.plain_len += data.len() as u64;
        }
        Ok(())
    }

    /// 檔截到已寫的完整段之後，寫入位置移到檔尾。
    fn truncate_to_written(&mut self) -> Result<(), SdkError> {
        let end = segment_offset(&self.header, self.segments_written);
        self.file.set_len(end)?;
        self.file.seek(SeekFrom::Start(end))?;
        Ok(())
    }

    /// 把 buffer 封成一段寫到檔尾（湊滿了、或 finish 時的最後一段）。
    fn seal_buffer(&mut self) -> Result<(), SdkError> {
        if self.buffer.is_empty() {
            return Ok(());
        }
        let sealed = seal_segment(
            &self.cipher,
            &self.header,
            self.segments_written,
            &self.buffer,
        )?;
        self.file.seek(SeekFrom::Start(segment_offset(
            &self.header,
            self.segments_written,
        )))?;
        self.file.write_all(&sealed)?;
        self.segments_written += 1;
        self.buffer.clear();
        Ok(())
    }

    /// 讀回已經落地的一段明文（GET 讀「主檔已寫的段」，/docs/design/media/media-download.md §7.2 第 3 列）。
    ///
    /// Args:
    ///     start: 明文起點, example: 65536
    ///     end: 明文終點（不含）；要在 `sealed_plain_len()` 之內, example: 131072
    /// Return:
    ///     Ok(Zeroizing<Vec<u8>>)   那一段明文
    ///     Err(Usage)               超出已落地的部分
    ///     Err(Io)                  某段解不開
    pub fn read_sealed(&mut self, start: u64, end: u64) -> Result<Zeroizing<Vec<u8>>, SdkError> {
        if start > end || end > self.sealed_plain_len() {
            return Err(SdkError::Usage(format!(
                "read {start}..{end} is past the {} sealed bytes",
                self.sealed_plain_len()
            )));
        }
        let segment_size = u64::from(self.header.segment_size);
        let mut out = Zeroizing::new(Vec::with_capacity((end - start) as usize));
        let mut position = start;
        while position < end {
            let index = position / segment_size;
            let segment = open_segment(&self.cipher, &self.header, &mut self.file, index)?;
            let data = segment.data()?;
            let from = (position - index * segment_size) as usize;
            let to = ((end - index * segment_size).min(segment.len)) as usize;
            let piece = data
                .get(from..to)
                .ok_or_else(|| pool_error(format!("segment {index} is shorter than {to} bytes")))?;
            out.extend_from_slice(piece);
            position = index * segment_size + to as u64;
        }
        // 讀的時候移了位置：下一次寫要回到檔尾。
        self.file.seek(SeekFrom::End(0))?;
        Ok(out)
    }

    /// 封最後一段（補滿、記真實長度）、fsync、回 hash。之後這個把手不能再寫。
    ///
    /// Return:
    ///     Ok(Finished)
    ///     Err(Usage)   一個 byte 都沒有（協議沒有零塊的上傳，池也不存零段的檔）
    ///     Err(Io)      寫不了
    pub fn finish(mut self) -> Result<Finished, SdkError> {
        if self.plain_len == 0 {
            return Err(SdkError::Usage(
                "a pool file needs at least one byte".into(),
            ));
        }
        self.seal_buffer()?;
        self.file.sync_all()?;
        let bytes_on_disk = self.file.metadata()?.len();
        Ok(Finished {
            hash_hex: self.blake3.finalize().to_hex().to_string(),
            sha256_hex: hex::encode(self.sha256.clone().finalize()),
            plain_len: self.plain_len,
            segments: self.segments_written,
            bytes_on_disk,
        })
    }

    /// fsync（/docs/design/media/media-download.md §4.1：每 1.5 秒一次，限制斷電時最多丟多少）。只有完整段在檔裡：buffer 不寫。
    pub fn sync(&mut self) -> Result<(), SdkError> {
        self.file.sync_data()?;
        Ok(())
    }
}

impl Write for PoolWriter {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        if self.holds_final_segment && !data.is_empty() {
            return Err(std::io::Error::other(
                "media pool: this file is already complete; nothing more can be written",
            ));
        }
        let mut remaining = data;
        while !remaining.is_empty() {
            let room = (self.header.segment_size as usize).saturating_sub(self.buffer.len());
            let take = room.min(remaining.len());
            let Some((piece, rest)) = remaining.split_at_checked(take) else {
                return Err(std::io::Error::other(
                    "media pool: write split past the end",
                ));
            };
            self.buffer.extend_from_slice(piece);
            self.blake3.update(piece);
            self.sha256.update(piece);
            self.plain_len += take as u64;
            remaining = rest;
            if self.buffer.len() == self.header.segment_size as usize {
                self.seal_buffer().map_err(io_error)?;
            }
        }
        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}

/// 讀的把手：明文位置的 `Read + Seek`，一次解一段、快取那一段。
pub struct PoolReader {
    /// 登記在 `OPEN_READERS` 的鍵；drop 時扣回去。
    path: PathBuf,
    cipher: XChaCha20Poly1305,
    header: Header,
    file: File,
    segments: u64,
    plain_len: u64,
    position: u64,
    loaded_segment: Option<(u64, Zeroizing<Vec<u8>>)>,
}

impl PoolReader {
    pub fn plain_len(&self) -> u64 {
        self.plain_len
    }

    fn load_segment(&mut self, index: u64) -> std::io::Result<&[u8]> {
        if self.loaded_segment.as_ref().map(|(loaded, _)| *loaded) != Some(index) {
            let segment = open_segment(&self.cipher, &self.header, &mut self.file, index)
                .map_err(io_error)?;
            // 除了最後一段，每段都要是滿的：不然明文位置就對不上了。
            if index + 1 < self.segments && segment.len != u64::from(self.header.segment_size) {
                return Err(std::io::Error::other(format!(
                    "media pool: segment {index} is short but is not the last"
                )));
            }
            let data = Zeroizing::new(segment.data().map_err(io_error)?.to_vec());
            self.loaded_segment = Some((index, data));
        }
        match &self.loaded_segment {
            Some((_, plain)) => Ok(plain),
            None => Err(std::io::Error::other("media pool: segment was not loaded")),
        }
    }
}

impl Drop for PoolReader {
    fn drop(&mut self) {
        let mut readers = open_readers();
        if let Some(count) = readers.get_mut(&self.path) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                readers.remove(&self.path);
            }
        }
    }
}

impl Read for PoolReader {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        if self.position >= self.plain_len || out.is_empty() {
            return Ok(0);
        }
        let segment_size = u64::from(self.header.segment_size);
        let index = self.position / segment_size;
        let offset = (self.position % segment_size) as usize;
        let segment = self.load_segment(index)?;
        if offset >= segment.len() {
            return Ok(0);
        }
        let take = (segment.len() - offset).min(out.len());
        let (Some(destination), Some(source)) =
            (out.get_mut(..take), segment.get(offset..offset + take))
        else {
            return Err(std::io::Error::other(
                "media pool: read window past the end",
            ));
        };
        destination.copy_from_slice(source);
        self.position += take as u64;
        Ok(take)
    }
}

impl Seek for PoolReader {
    fn seek(&mut self, target: SeekFrom) -> std::io::Result<u64> {
        let base = match target {
            SeekFrom::Start(offset) => offset as i128,
            SeekFrom::End(delta) => self.plain_len as i128 + delta as i128,
            SeekFrom::Current(delta) => self.position as i128 + delta as i128,
        };
        if base < 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "seek before start",
            ));
        }
        self.position = base as u64;
        Ok(self.position)
    }
}

// ---- 格式 ----

/// 檔頭的 `owner`：mxc 的 BLAKE3 前 4 byte。只用來認「這個暫存名是不是還是同一個檔的」，🚫 不是安全邊界（池金鑰才是）。
fn owner_of(mxc: &str) -> u32 {
    let hash = blake3::hash(mxc.as_bytes());
    let bytes = hash.as_bytes();
    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

pub(crate) fn random_nonce_base() -> Result<[u8; NONCE_BASE_LEN], SdkError> {
    let mut nonce_base = [0u8; NONCE_BASE_LEN];
    getrandom::getrandom(&mut nonce_base)
        .map_err(|error| SdkError::Io(std::io::Error::other(format!("csprng: {error}"))))?;
    Ok(nonce_base)
}

fn encode_header(header: &Header) -> [u8; HEADER_LEN as usize] {
    let mut bytes = [0u8; HEADER_LEN as usize];
    bytes[..4].copy_from_slice(MAGIC);
    bytes[4] = POOL_FORMAT_VERSION;
    bytes[8..12].copy_from_slice(&header.segment_size.to_le_bytes());
    bytes[12..12 + NONCE_BASE_LEN].copy_from_slice(&header.nonce_base);
    bytes[28..32].copy_from_slice(&header.owner.to_le_bytes());
    bytes
}

fn read_header(file: &mut File) -> Result<Header, SdkError> {
    let mut bytes = [0u8; HEADER_LEN as usize];
    file.seek(SeekFrom::Start(0))?;
    file.read_exact(&mut bytes)
        .map_err(|_| pool_error("file is shorter than the pool header".into()))?;
    if &bytes[..4] != MAGIC {
        return Err(pool_error("not a media pool file (bad magic)".into()));
    }
    if bytes[4] != POOL_FORMAT_VERSION {
        return Err(pool_error(format!(
            "media pool file version {} is not supported (this build reads {POOL_FORMAT_VERSION})",
            bytes[4]
        )));
    }
    let segment_size = u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]);
    if segment_size == 0 {
        return Err(pool_error("media pool file has segment_size 0".into()));
    }
    let mut nonce_base = [0u8; NONCE_BASE_LEN];
    nonce_base.copy_from_slice(&bytes[12..12 + NONCE_BASE_LEN]);
    Ok(Header {
        segment_size,
        nonce_base,
        owner: u32::from_le_bytes([bytes[28], bytes[29], bytes[30], bytes[31]]),
    })
}

/// 每段在磁碟上的固定大小 R。
fn record_len(header: &Header) -> u64 {
    LEN_FIELD + u64::from(header.segment_size) + TAG_LEN
}

fn segment_offset(header: &Header, index: u64) -> u64 {
    HEADER_LEN + index * record_len(header)
}

fn nonce_and_aad(header: &Header, index: u64) -> ([u8; 24], Vec<u8>) {
    let mut nonce = [0u8; 24];
    nonce[..NONCE_BASE_LEN].copy_from_slice(&header.nonce_base);
    nonce[NONCE_BASE_LEN..].copy_from_slice(&index.to_le_bytes());
    let mut aad = Vec::with_capacity(AAD_PREFIX.len() + NONCE_BASE_LEN + 8);
    aad.extend_from_slice(AAD_PREFIX);
    aad.extend_from_slice(&header.nonce_base);
    aad.extend_from_slice(&index.to_le_bytes());
    (nonce, aad)
}

fn seal_segment(
    cipher: &XChaCha20Poly1305,
    header: &Header,
    index: u64,
    data: &[u8],
) -> Result<Vec<u8>, SdkError> {
    let segment_size = header.segment_size as usize;
    if data.is_empty() || data.len() > segment_size {
        return Err(pool_error(format!(
            "segment {index} would hold {} bytes (1..={segment_size} allowed)",
            data.len()
        )));
    }
    let mut plain = Zeroizing::new(Vec::with_capacity(LEN_FIELD as usize + segment_size));
    plain.extend_from_slice(&(data.len() as u32).to_le_bytes());
    plain.extend_from_slice(data);
    plain.resize(LEN_FIELD as usize + segment_size, 0);
    let (nonce, aad) = nonce_and_aad(header, index);
    cipher
        .encrypt(
            XNonce::from_slice(&nonce),
            Payload {
                msg: &plain,
                aad: &aad,
            },
        )
        .map_err(|_| pool_error(format!("sealing segment {index} failed")))
}

/// 解開的一段：整段明文（含長度欄與補零）加上真實長度。
struct OpenedSegment {
    plain: Zeroizing<Vec<u8>>,
    len: u64,
}

impl OpenedSegment {
    fn data(&self) -> Result<&[u8], SdkError> {
        let end = LEN_FIELD as usize + self.len as usize;
        self.plain
            .get(LEN_FIELD as usize..end)
            .ok_or_else(|| pool_error("segment length runs past the segment".into()))
    }
}

/// 讀第 `index` 段並解開，驗長度欄是 1..=segment_size。
fn open_segment(
    cipher: &XChaCha20Poly1305,
    header: &Header,
    file: &mut File,
    index: u64,
) -> Result<OpenedSegment, SdkError> {
    let mut sealed = vec![0u8; record_len(header) as usize];
    file.seek(SeekFrom::Start(segment_offset(header, index)))?;
    file.read_exact(&mut sealed)
        .map_err(|_| pool_error(format!("segment {index} is not complete on disk")))?;
    let (nonce, aad) = nonce_and_aad(header, index);
    let plain = Zeroizing::new(
        cipher
            .decrypt(
                XNonce::from_slice(&nonce),
                Payload {
                    msg: &sealed,
                    aad: &aad,
                },
            )
            .map_err(|_| pool_error(format!("segment {index} does not authenticate")))?,
    );
    let len = plain
        .first_chunk::<4>()
        .map(|field| u64::from(u32::from_le_bytes(*field)))
        .ok_or_else(|| pool_error(format!("segment {index} has no length field")))?;
    if len == 0 || len > u64::from(header.segment_size) {
        return Err(pool_error(format!(
            "segment {index} claims {len} bytes (1..={} allowed)",
            header.segment_size
        )));
    }
    Ok(OpenedSegment { plain, len })
}

fn remove_if_exists(path: &Path) -> Result<(), SdkError> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

/// 池的錯誤一律當 Io（對呼叫者是「檔壞了、重拉」），字串裡沒有金鑰。
pub(crate) fn pool_error(message: String) -> SdkError {
    SdkError::Io(std::io::Error::other(format!("media pool: {message}")))
}

fn io_error(error: SdkError) -> std::io::Error {
    match error {
        SdkError::Io(error) => error,
        other => std::io::Error::other(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MXC: &str = "mxc://localhost/0001";
    const R: u64 = LEN_FIELD + SEGMENT_SIZE as u64 + TAG_LEN;

    fn scratch_pool(name: &str) -> (MediaPool, PathBuf) {
        let dir = std::env::temp_dir().join(format!("wbf-pool-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let pool = MediaPool::open(&dir, Key32([9u8; 32])).unwrap();
        (pool, dir)
    }

    fn pattern(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 251) as u8).collect()
    }

    fn write_and_adopt(pool: &MediaPool, name: &str, plain: &[u8]) -> Finished {
        let mut writer = pool.create_pending(name, MXC).unwrap();
        for piece in plain.chunks(7000) {
            writer.write_all(piece).unwrap();
        }
        let finished = writer.finish().unwrap();
        pool.adopt(name, &finished.hash_hex).unwrap();
        finished
    }

    fn read_all(pool: &MediaPool, hash: &str) -> Vec<u8> {
        let mut back = Vec::new();
        pool.open_read(hash)
            .unwrap()
            .read_to_end(&mut back)
            .unwrap();
        back
    }

    #[test]
    fn every_segment_is_written_full_and_the_file_length_is_whole_records() {
        let (pool, dir) = scratch_pool("full");
        for len in [
            1usize,
            SEGMENT_SIZE as usize,
            SEGMENT_SIZE as usize * 2 + 12345,
        ] {
            let plain = pattern(len);
            let finished = write_and_adopt(&pool, "w", &plain);
            let segments = (len as u64).div_ceil(u64::from(SEGMENT_SIZE));
            assert_eq!(
                finished.bytes_on_disk,
                HEADER_LEN + segments * R,
                "len {len}"
            );
            assert_eq!(finished.plain_len, len as u64);
            assert_eq!(finished.hash_hex, blake3::hash(&plain).to_hex().to_string());
            assert_eq!(finished.sha256_hex, hex::encode(Sha256::digest(&plain)));
            assert_eq!(read_all(&pool, &finished.hash_hex), plain);
            pool.remove(&finished.hash_hex).unwrap();
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_reader_seeks_across_segments_and_dedup_keeps_the_first_copy() {
        let (pool, dir) = scratch_pool("seek");
        let plain = pattern(SEGMENT_SIZE as usize * 2 + 12345);
        let finished = write_and_adopt(&pool, "1", &plain);
        assert!(!pool.pending_path("1").exists());
        let on_disk = std::fs::read(pool.path_of(&finished.hash_hex).unwrap()).unwrap();
        assert!(!on_disk.windows(64).any(|window| window == &plain[100..164]));
        let mut reader = pool.open_read(&finished.hash_hex).unwrap();
        reader
            .seek(SeekFrom::Start(SEGMENT_SIZE as u64 - 10))
            .unwrap();
        let mut window = [0u8; 20];
        reader.read_exact(&mut window).unwrap();
        assert_eq!(
            &window[..],
            &plain[SEGMENT_SIZE as usize - 10..SEGMENT_SIZE as usize + 10]
        );
        let mut writer = pool.create_pending("2", "mxc://localhost/other").unwrap();
        writer.write_all(&plain).unwrap();
        let second = writer.finish().unwrap();
        assert!(!pool.adopt("2", &second.hash_hex).unwrap());
        assert!(!pool.pending_path("2").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resume_keeps_whole_segments_and_drops_a_half_written_one() {
        let (pool, dir) = scratch_pool("resume");
        let plain = pattern(SEGMENT_SIZE as usize * 3 + 777);
        let mut writer = pool.create_pending("r", MXC).unwrap();
        writer
            .write_all(&plain[..SEGMENT_SIZE as usize * 2 + 5])
            .unwrap();
        assert_eq!(writer.segments_written(), 2);
        drop(writer);
        // 第三段寫到一半就斷：檔尾多出半筆。
        let path = pool.pending_path("r");
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(&[0xAB; 1000]).unwrap();
        drop(file);
        let mut resumed = pool.resume_pending("r", MXC).unwrap();
        assert_eq!(resumed.segments_written(), 2);
        assert_eq!(resumed.plain_len(), SEGMENT_SIZE as u64 * 2);
        assert_eq!(std::fs::metadata(&path).unwrap().len(), HEADER_LEN + 2 * R);
        resumed
            .write_all(&plain[SEGMENT_SIZE as usize * 2..])
            .unwrap();
        let finished = resumed.finish().unwrap();
        assert_eq!(finished.hash_hex, blake3::hash(&plain).to_hex().to_string());
        pool.adopt("r", &finished.hash_hex).unwrap();
        assert_eq!(read_all(&pool, &finished.hash_hex), plain);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resume_cuts_at_the_first_segment_that_does_not_open() {
        let (pool, dir) = scratch_pool("cut");
        let plain = pattern(SEGMENT_SIZE as usize * 3);
        let mut writer = pool.create_pending("c", MXC).unwrap();
        writer.write_all(&plain).unwrap();
        drop(writer);
        // 長度落地、內容沒落地：第二段變成零。
        let path = pool.pending_path("c");
        let mut bytes = std::fs::read(&path).unwrap();
        let second = (HEADER_LEN + R) as usize;
        bytes[second..second + R as usize].fill(0);
        std::fs::write(&path, &bytes).unwrap();
        let resumed = pool.resume_pending("c", MXC).unwrap();
        assert_eq!(resumed.segments_written(), 1);
        assert_eq!(std::fs::metadata(&path).unwrap().len(), HEADER_LEN + R);
        drop(resumed);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_finished_but_not_adopted_file_resumes_to_its_full_length() {
        let (pool, dir) = scratch_pool("done");
        let plain = pattern(SEGMENT_SIZE as usize + 300);
        let mut writer = pool.create_pending("d", MXC).unwrap();
        writer.write_all(&plain).unwrap();
        let first = writer.finish().unwrap();
        let resumed = pool.resume_pending("d", MXC).unwrap();
        assert_eq!(resumed.plain_len(), plain.len() as u64);
        assert_eq!(resumed.segments_written(), 1);
        let again = resumed.finish().unwrap();
        assert_eq!(again, first);
        // 完整的檔不准再接著寫。
        let mut resumed = pool.resume_pending("d", MXC).unwrap();
        assert!(resumed.write_all(&[1]).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_pending_file_of_another_media_or_version_is_not_resumed() {
        let (pool, dir) = scratch_pool("owner");
        let mut writer = pool.create_pending("o", MXC).unwrap();
        writer.write_all(&pattern(SEGMENT_SIZE as usize)).unwrap();
        drop(writer);
        assert!(pool
            .resume_pending("o", "mxc://localhost/someone-else")
            .is_err());
        assert!(pool.resume_pending("o", MXC).is_ok());
        // 池格式 v1 一律不續（/docs/design/media/media-download.md §11 第 1 條）。
        let path = pool.pending_path("o");
        let mut bytes = std::fs::read(&path).unwrap();
        bytes[4] = 1;
        std::fs::write(&path, &bytes).unwrap();
        assert!(pool.resume_pending("o", MXC).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tampering_moving_segments_and_a_wrong_key_are_rejected() {
        let (pool, dir) = scratch_pool("tamper");
        let plain = pattern(SEGMENT_SIZE as usize + 10);
        let finished = write_and_adopt(&pool, "t", &plain);
        let path = pool.path_of(&finished.hash_hex).unwrap();
        let original = std::fs::read(&path).unwrap();
        // 翻第一段的一個 byte。
        let mut bytes = original.clone();
        bytes[HEADER_LEN as usize + 3] ^= 1;
        std::fs::write(&path, &bytes).unwrap();
        let mut reader = pool.open_read(&finished.hash_hex).unwrap();
        assert!(reader.read_to_end(&mut Vec::new()).is_err());
        // 把第 1 段搬到第 0 段：AAD／nonce 帶段號，解不開。
        let mut bytes = original.clone();
        let (head, body) = bytes.split_at_mut(HEADER_LEN as usize);
        let _ = head;
        let (first, second) = body.split_at_mut(R as usize);
        first.swap_with_slice(&mut second[..R as usize]);
        std::fs::write(&path, &bytes).unwrap();
        assert!(pool.open_read(&finished.hash_hex).is_err());
        // 別把金鑰。
        std::fs::write(&path, &original).unwrap();
        let other = MediaPool::open(&dir, Key32([8u8; 32])).unwrap();
        assert!(other.open_read(&finished.hash_hex).is_err());
        // 檔長不是整數筆：拒。
        let mut bytes = original.clone();
        bytes.truncate(bytes.len() - 1);
        std::fs::write(&path, &bytes).unwrap();
        assert!(pool.open_read(&finished.hash_hex).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_length_field_over_the_segment_size_is_rejected() {
        let (_pool, dir) = scratch_pool("length");
        let cipher = XChaCha20Poly1305::new(Key32([9u8; 32]).as_bytes().into());
        let header = Header {
            segment_size: SEGMENT_SIZE,
            nonce_base: [3; NONCE_BASE_LEN],
            owner: owner_of(MXC),
        };
        let mut plain = vec![0u8; R as usize - TAG_LEN as usize];
        plain[..4].copy_from_slice(&(SEGMENT_SIZE + 1).to_le_bytes());
        let (nonce, aad) = nonce_and_aad(&header, 0);
        let sealed = cipher
            .encrypt(
                XNonce::from_slice(&nonce),
                Payload {
                    msg: &plain,
                    aad: &aad,
                },
            )
            .unwrap();
        let path = dir.join("forged");
        let mut bytes = encode_header(&header).to_vec();
        bytes.extend_from_slice(&sealed);
        std::fs::write(&path, &bytes).unwrap();
        let mut file = File::open(&path).unwrap();
        assert!(open_segment(&cipher, &header, &mut file, 0).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sealed_segments_read_back_while_writing() {
        let (pool, dir) = scratch_pool("sealed");
        let plain = pattern(SEGMENT_SIZE as usize * 2 + 50);
        let mut writer = pool.create_pending("s", MXC).unwrap();
        writer.write_all(&plain).unwrap();
        assert_eq!(writer.sealed_plain_len(), SEGMENT_SIZE as u64 * 2);
        let middle = writer.read_sealed(65000, 70000).unwrap();
        assert_eq!(&middle[..], &plain[65000..70000]);
        assert!(writer.read_sealed(0, SEGMENT_SIZE as u64 * 2 + 1).is_err());
        // 讀過之後照樣接著寫到正確的位置。
        writer.write_all(&[1, 2, 3]).unwrap();
        let finished = writer.finish().unwrap();
        pool.adopt("s", &finished.hash_hex).unwrap();
        let mut expected = plain.clone();
        expected.extend_from_slice(&[1, 2, 3]);
        assert_eq!(read_all(&pool, &finished.hash_hex), expected);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_empty_file_is_refused_and_bad_names_too() {
        let (pool, dir) = scratch_pool("empty");
        let writer = pool.create_pending("e", MXC).unwrap();
        assert!(writer.finish().is_err());
        assert!(pool.path_of("../x").is_err());
        assert!(pool.path_of("zz").is_err());
        assert_eq!(pool.list_pending().unwrap(), vec!["e".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

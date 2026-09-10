//! 媒體儲存池（local-cache-db.md §8）：對上層是「開檔、順序 append、讀、刪」的完整明文檔；落地時整個池加密，一把金鑰
//! （`Vault::media_store_key()`）。
//!
//! 落地格式（§8.1 說「池內部怎麼分段是實作細節」，就在這裡定）：
//!
//! ```text
//! 檔頭 32 byte：magic "WBFP"(4) ‖ version u8 = 1 ‖ 保留 3 byte ‖ segment_size u32 LE ‖ nonce_base 16 byte ‖ 保留 4 byte
//! 之後：第 i 段 = XChaCha20-Poly1305(key, nonce = nonce_base ‖ u64_le(i), aad = "wbf-media-pool v1" ‖ nonce_base ‖ u64_le(i), 明文段 i)
//!       每段明文固定 SEGMENT_SIZE（64 KiB），只有最後一段可以短；密文段 = 明文 + 16 byte 標籤，固定偏移，所以能隨機讀。
//! ```
//!
//! - 順序 append：`PoolWriter` 湊滿一段就封一段；`finish()` 封最後的短段、fsync、回明文的 BLAKE3。
//! - 續傳：`PoolWriter::resume()` 把最後那個不完整的段解回記憶體、檔截到該段起點，接著寫；BLAKE3 從頭重算（本機讀，便宜）。
//! - 檔名是明文 hash（§8.2），寫完才知道；寫的時候用呼叫者給的暫存名，`finish()` 回 hash 由呼叫者 rename（`adopt`）。
//! - 段索引進 nonce 與 AAD：把第 3 段搬到第 5 段解不開；nonce_base 每檔隨機：同內容的兩個暫存檔密文不同（去重靠 hash，不靠密文）。
//! - 暫定段（`sync()` 寫在檔尾、之後會被截掉重封）用段號最高位設 1 的 nonce：同段號的暫定段與正式段是兩個 nonce，每個 nonce 只封一次。
//!
//! 🚫 金鑰不進錯誤訊息、不 log。這裡沒有 SQL、沒有網路。

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};

use crate::error::SdkError;
use crate::vault::Key32;

pub const POOL_DIR_NAME: &str = "media";
/// 下載中的暫存檔放這裡（§8.2：暫存名用 `media.id`）。
pub const PENDING_DIR_NAME: &str = "pending";

const MAGIC: &[u8; 4] = b"WBFP";
const VERSION: u8 = 1;
const HEADER_LEN: u64 = 32;
const NONCE_BASE_LEN: usize = 16;
const TAG_LEN: u64 = 16;
const AAD_PREFIX: &[u8] = b"wbf-media-pool v1";
/// 明文段大小。64 KiB：跟最小的 chunk_size 一樣，每段 16 byte 標籤（0.02%），隨機讀一段只解 64 KiB。
pub const SEGMENT_SIZE: u32 = 65536;

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

    /// 完成檔的位置：`media/<hash 前 2 hex>/<hash>`。
    pub fn path_of(&self, pool_file: &str) -> Result<PathBuf, SdkError> {
        if pool_file.len() < 4 || !pool_file.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(SdkError::Usage(format!(
                "pool file name {pool_file:?} is not a hex hash"
            )));
        }
        Ok(self.dir.join(&pool_file[..2]).join(pool_file))
    }

    /// 下載中的暫存檔位置。
    pub fn pending_path(&self, pending_name: &str) -> PathBuf {
        self.dir.join(PENDING_DIR_NAME).join(pending_name)
    }

    /// 開一個新的暫存檔從頭寫（已有同名暫存檔就覆蓋）。
    pub fn create_pending(&self, pending_name: &str) -> Result<PoolWriter, SdkError> {
        let path = self.pending_path(pending_name);
        let mut nonce_base = [0u8; NONCE_BASE_LEN];
        getrandom::getrandom(&mut nonce_base)
            .map_err(|error| SdkError::Io(std::io::Error::other(format!("csprng: {error}"))))?;
        let header = Header {
            segment_size: SEGMENT_SIZE,
            nonce_base,
        };
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)?;
        file.write_all(&encode_header(&header))?;
        Ok(PoolWriter {
            cipher: XChaCha20Poly1305::new(self.key.as_bytes().into()),
            header,
            file,
            path,
            segment_index: 0,
            buffer: Vec::with_capacity(SEGMENT_SIZE as usize),
            provisional_on_disk: false,
            hasher: blake3::Hasher::new(),
            plain_len: 0,
        })
    }

    /// 接著寫一個暫存檔：明文只信任到 `trusted_plain_len`（§8.3 的「截到 chunks_written × chunk_size」），之後的丟掉。
    ///
    /// Args:
    ///     trusted_plain_len: example: 20 * 65536
    /// Return:
    ///     Ok(PoolWriter)   已定位到 trusted_plain_len，BLAKE3 已算到那裡
    ///     Err(Io)          暫存檔不在、比 trusted_plain_len 短、或某段解不開（呼叫者當成從頭來）
    pub fn resume_pending(
        &self,
        pending_name: &str,
        trusted_plain_len: u64,
    ) -> Result<PoolWriter, SdkError> {
        let path = self.pending_path(pending_name);
        let mut file = OpenOptions::new().read(true).write(true).open(&path)?;
        let header = read_header(&mut file)?;
        let cipher = XChaCha20Poly1305::new(self.key.as_bytes().into());
        let segment_size = u64::from(header.segment_size);
        let full_segments = trusted_plain_len / segment_size;
        let tail = (trusted_plain_len % segment_size) as usize;
        let mut hasher = blake3::Hasher::new();
        for index in 0..full_segments {
            let plain = read_segment(&cipher, &header, &mut file, index)?;
            if plain.len() as u64 != segment_size {
                return Err(pool_error(format!(
                    "segment {index} is short ({} bytes) below the trusted length",
                    plain.len()
                )));
            }
            hasher.update(&plain);
        }
        let mut buffer = Vec::with_capacity(header.segment_size as usize);
        if tail > 0 {
            // 尾巴那段可能是 sync() 留的暫定段（自己的 nonce），也可能是已經封好的正式整段（快照點在它中間）。
            let plain = match read_segment_as(&cipher, &header, &mut file, full_segments, true) {
                Ok(plain) => plain,
                Err(_) => read_segment(&cipher, &header, &mut file, full_segments)?,
            };
            if plain.len() < tail {
                return Err(pool_error(format!(
                    "last segment has {} bytes, trusted length needs {tail}",
                    plain.len()
                )));
            }
            buffer.extend_from_slice(&plain[..tail]);
            hasher.update(&buffer);
        }
        // 檔截到第 full_segments 段的起點；buffer 裡的短段等湊滿或 finish 再封。
        file.set_len(segment_offset(&header, full_segments))?;
        file.seek(SeekFrom::End(0))?;
        Ok(PoolWriter {
            cipher,
            header,
            file,
            path,
            segment_index: full_segments,
            buffer,
            provisional_on_disk: false,
            hasher,
            plain_len: trusted_plain_len,
        })
    }

    /// 暫存檔寫完了：搬到 `media/<hh>/<hash>`。已經有同 hash 的檔（別的 mxc 同內容）就丟掉暫存檔、用既有的（去重，§8.2）。
    ///
    /// Return:
    ///     Ok(bool)   true = 這次搬進去的；false = 池裡本來就有
    pub fn adopt(&self, pending_name: &str, hash_hex: &str) -> Result<bool, SdkError> {
        let target = self.path_of(hash_hex)?;
        let pending = self.pending_path(pending_name);
        if target.exists() {
            std::fs::remove_file(&pending)?;
            return Ok(false);
        }
        std::fs::create_dir_all(target.parent().expect("pool file has a parent"))?;
        std::fs::rename(&pending, &target)?;
        Ok(true)
    }

    pub fn discard_pending(&self, pending_name: &str) -> Result<(), SdkError> {
        remove_if_exists(&self.pending_path(pending_name))
    }

    /// 讀一個完成檔：`Read + Seek` 的把手，位置是明文位置。
    pub fn open_read(&self, pool_file: &str) -> Result<PoolReader, SdkError> {
        let path = self.path_of(pool_file)?;
        let mut file = File::open(&path)?;
        let header = read_header(&mut file)?;
        let cipher_len = file.metadata()?.len();
        let plain_len = plain_len_from_cipher_len(&header, cipher_len)?;
        Ok(PoolReader {
            cipher: XChaCha20Poly1305::new(self.key.as_bytes().into()),
            header,
            file,
            plain_len,
            position: 0,
            loaded_segment: None,
        })
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

    /// 掃 `pending/`：所有暫存檔名（啟動時對照 DB 清孤兒用）。
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
    segment_index: u64,
    buffer: Vec<u8>,
    /// `sync()` 把 buffer 先封成一個短的「暫定段」寫在檔尾，讓進度快照指到的資料真的在磁碟上；
    /// 下次要封正式的第 `segment_index` 段時先把檔截回它的起點。
    provisional_on_disk: bool,
    hasher: blake3::Hasher,
    plain_len: u64,
}

/// `finish()` 的結果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Finished {
    /// 明文的 BLAKE3，32 位小寫 hex；就是池裡的檔名。
    pub hash_hex: String,
    pub plain_len: u64,
    pub bytes_on_disk: u64,
}

impl PoolWriter {
    /// 目前已收下的明文長度（含還沒封的 buffer）。
    pub fn plain_len(&self) -> u64 {
        self.plain_len
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 有暫定段就先把檔截回第 `segment_index` 段的起點。
    fn drop_provisional(&mut self) -> Result<(), SdkError> {
        if self.provisional_on_disk {
            let start = segment_offset(&self.header, self.segment_index);
            self.file.set_len(start)?;
            self.file.seek(SeekFrom::Start(start))?;
            self.provisional_on_disk = false;
        }
        Ok(())
    }

    /// 把 buffer 封成正式的一段（湊滿了、或 finish 時的最後一段）。
    fn seal_buffer(&mut self) -> Result<(), SdkError> {
        if self.buffer.is_empty() {
            return Ok(());
        }
        self.drop_provisional()?;
        let sealed = seal_segment(
            &self.cipher,
            &self.header,
            self.segment_index,
            false,
            &self.buffer,
        )?;
        self.file.write_all(&sealed)?;
        self.segment_index += 1;
        self.buffer.clear();
        Ok(())
    }

    /// 封最後一段、fsync、回 hash。之後這個把手不能再寫。
    pub fn finish(mut self) -> Result<Finished, SdkError> {
        self.seal_buffer()?;
        self.file.sync_all()?;
        let bytes_on_disk = self.file.metadata()?.len();
        Ok(Finished {
            hash_hex: self.hasher.finalize().to_hex().to_string(),
            plain_len: self.plain_len,
            bytes_on_disk,
        })
    }

    /// 進度快照前呼叫：連湊不滿的那段也以「暫定段」寫到磁碟並 fsync，讓快照指到的每個 byte 都真的在檔裡
    /// （`resume_pending` 解得到）。下次湊滿時暫定段會被截掉重封。
    pub fn sync(&mut self) -> Result<(), SdkError> {
        if !self.buffer.is_empty() {
            self.drop_provisional()?;
            // 暫定段用自己的 nonce（PROVISIONAL_BIT）：同段號之後正式重封是另一個 nonce，不是 nonce 重用。
            let sealed = seal_segment(
                &self.cipher,
                &self.header,
                self.segment_index,
                true,
                &self.buffer,
            )?;
            self.file.write_all(&sealed)?;
            self.provisional_on_disk = true;
        }
        self.file.sync_data()?;
        Ok(())
    }
}

impl Write for PoolWriter {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        let mut remaining = data;
        while !remaining.is_empty() {
            let room = self.header.segment_size as usize - self.buffer.len();
            let take = room.min(remaining.len());
            self.buffer.extend_from_slice(&remaining[..take]);
            self.hasher.update(&remaining[..take]);
            self.plain_len += take as u64;
            remaining = &remaining[take..];
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
    cipher: XChaCha20Poly1305,
    header: Header,
    file: File,
    plain_len: u64,
    position: u64,
    loaded_segment: Option<(u64, Vec<u8>)>,
}

impl PoolReader {
    pub fn plain_len(&self) -> u64 {
        self.plain_len
    }

    fn load_segment(&mut self, index: u64) -> std::io::Result<&[u8]> {
        if self.loaded_segment.as_ref().map(|(loaded, _)| *loaded) != Some(index) {
            let plain = read_segment(&self.cipher, &self.header, &mut self.file, index)
                .map_err(io_error)?;
            self.loaded_segment = Some((index, plain));
        }
        Ok(&self.loaded_segment.as_ref().expect("just loaded").1)
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
        out[..take].copy_from_slice(&segment[offset..offset + take]);
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

fn encode_header(header: &Header) -> [u8; HEADER_LEN as usize] {
    let mut bytes = [0u8; HEADER_LEN as usize];
    bytes[..4].copy_from_slice(MAGIC);
    bytes[4] = VERSION;
    bytes[8..12].copy_from_slice(&header.segment_size.to_le_bytes());
    bytes[12..12 + NONCE_BASE_LEN].copy_from_slice(&header.nonce_base);
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
    if bytes[4] != VERSION {
        return Err(pool_error(format!(
            "media pool file version {} is not supported",
            bytes[4]
        )));
    }
    let segment_size = u32::from_le_bytes(bytes[8..12].try_into().expect("4 bytes"));
    if segment_size == 0 {
        return Err(pool_error("media pool file has segment_size 0".into()));
    }
    let mut nonce_base = [0u8; NONCE_BASE_LEN];
    nonce_base.copy_from_slice(&bytes[12..12 + NONCE_BASE_LEN]);
    Ok(Header {
        segment_size,
        nonce_base,
    })
}

fn segment_offset(header: &Header, index: u64) -> u64 {
    HEADER_LEN + index * (u64::from(header.segment_size) + TAG_LEN)
}

/// 暫定段（`sync()` 寫在檔尾、之後會被截掉重封的那段）用另一組 nonce：段號的最高位設 1。
/// 同一個 (key, nonce) 封兩份不同的明文是 AEAD 的大忌（Poly1305 的金鑰會漏），而暫定段與它之後的正式段就是「同段號、不同明文」；
/// 分開 nonce 之後兩者各自只封一次（PR #14 審查 rumia 🟡3）。段號只用 63 位，夠用（2^63 × 64 KiB）。
const PROVISIONAL_BIT: u64 = 1 << 63;

fn nonce_and_aad(header: &Header, index: u64, provisional: bool) -> ([u8; 24], Vec<u8>) {
    let tagged_index = if provisional {
        index | PROVISIONAL_BIT
    } else {
        index
    };
    let mut nonce = [0u8; 24];
    nonce[..NONCE_BASE_LEN].copy_from_slice(&header.nonce_base);
    nonce[NONCE_BASE_LEN..].copy_from_slice(&tagged_index.to_le_bytes());
    let mut aad = Vec::with_capacity(AAD_PREFIX.len() + NONCE_BASE_LEN + 8);
    aad.extend_from_slice(AAD_PREFIX);
    aad.extend_from_slice(&header.nonce_base);
    aad.extend_from_slice(&tagged_index.to_le_bytes());
    (nonce, aad)
}

fn seal_segment(
    cipher: &XChaCha20Poly1305,
    header: &Header,
    index: u64,
    provisional: bool,
    plain: &[u8],
) -> Result<Vec<u8>, SdkError> {
    if index & PROVISIONAL_BIT != 0 {
        return Err(pool_error(format!("segment index {index} is out of range")));
    }
    let (nonce, aad) = nonce_and_aad(header, index, provisional);
    cipher
        .encrypt(
            XNonce::from_slice(&nonce),
            Payload {
                msg: plain,
                aad: &aad,
            },
        )
        .map_err(|_| pool_error(format!("sealing segment {index} failed")))
}

/// 讀第 `index` 段並解開。最後一段可以短；超出檔尾回空。
fn read_segment(
    cipher: &XChaCha20Poly1305,
    header: &Header,
    file: &mut File,
    index: u64,
) -> Result<Vec<u8>, SdkError> {
    read_segment_as(cipher, header, file, index, false)
}

/// `provisional` = true 用暫定段的 nonce 解（只有 `resume_pending` 讀暫存檔的尾巴會用）。
fn read_segment_as(
    cipher: &XChaCha20Poly1305,
    header: &Header,
    file: &mut File,
    index: u64,
    provisional: bool,
) -> Result<Vec<u8>, SdkError> {
    let start = segment_offset(header, index);
    let total = file.metadata()?.len();
    if start >= total {
        return Ok(Vec::new());
    }
    let sealed_len = (total - start).min(u64::from(header.segment_size) + TAG_LEN) as usize;
    let mut sealed = vec![0u8; sealed_len];
    file.seek(SeekFrom::Start(start))?;
    file.read_exact(&mut sealed)?;
    let (nonce, aad) = nonce_and_aad(header, index, provisional);
    cipher
        .decrypt(
            XNonce::from_slice(&nonce),
            Payload {
                msg: &sealed,
                aad: &aad,
            },
        )
        .map_err(|_| pool_error(format!("segment {index} does not authenticate")))
}

/// 密文總長 → 明文總長（最後一段可短）。
fn plain_len_from_cipher_len(header: &Header, cipher_len: u64) -> Result<u64, SdkError> {
    if cipher_len < HEADER_LEN {
        return Err(pool_error("file is shorter than the pool header".into()));
    }
    let body = cipher_len - HEADER_LEN;
    let sealed_segment = u64::from(header.segment_size) + TAG_LEN;
    let full = body / sealed_segment;
    let rest = body % sealed_segment;
    let tail = if rest == 0 {
        0
    } else if rest > TAG_LEN {
        rest - TAG_LEN
    } else {
        return Err(pool_error("trailing bytes shorter than a tag".into()));
    };
    Ok(full * u64::from(header.segment_size) + tail)
}

fn remove_if_exists(path: &Path) -> Result<(), SdkError> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

/// 池的錯誤一律當 Io（對呼叫者是「檔壞了、重拉」），字串裡沒有金鑰。
fn pool_error(message: String) -> SdkError {
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

    fn scratch_pool(name: &str) -> (MediaPool, PathBuf) {
        let dir = std::env::temp_dir().join(format!("wbf-pool-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let pool = MediaPool::open(&dir, Key32([9u8; 32])).unwrap();
        (pool, dir)
    }

    fn pattern(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 251) as u8).collect()
    }

    #[test]
    fn write_finish_adopt_read_back_and_dedup() {
        let (pool, dir) = scratch_pool("roundtrip");
        // 兩段半：跨段、最後一段短。
        let plain = pattern(SEGMENT_SIZE as usize * 2 + 12345);
        let mut writer = pool.create_pending("1").unwrap();
        for piece in plain.chunks(7000) {
            writer.write_all(piece).unwrap();
        }
        let finished = writer.finish().unwrap();
        assert_eq!(finished.plain_len, plain.len() as u64);
        assert_eq!(finished.hash_hex, blake3::hash(&plain).to_hex().to_string());
        assert_eq!(
            finished.bytes_on_disk,
            HEADER_LEN + 3 * TAG_LEN + plain.len() as u64
        );
        assert!(pool.adopt("1", &finished.hash_hex).unwrap());
        assert!(!pool.pending_path("1").exists());
        // 落地不是明文。
        let on_disk = std::fs::read(pool.path_of(&finished.hash_hex).unwrap()).unwrap();
        assert!(!on_disk.windows(64).any(|window| window == &plain[100..164]));

        let mut reader = pool.open_read(&finished.hash_hex).unwrap();
        assert_eq!(reader.plain_len(), plain.len() as u64);
        let mut back = Vec::new();
        reader.read_to_end(&mut back).unwrap();
        assert_eq!(back, plain);
        // seek 到跨段的位置讀一小段。
        reader
            .seek(SeekFrom::Start(SEGMENT_SIZE as u64 - 10))
            .unwrap();
        let mut window = [0u8; 20];
        reader.read_exact(&mut window).unwrap();
        assert_eq!(
            &window[..],
            &plain[SEGMENT_SIZE as usize - 10..SEGMENT_SIZE as usize + 10]
        );

        // 同內容第二次：adopt 回 false、暫存檔被丟。
        let mut writer2 = pool.create_pending("2").unwrap();
        writer2.write_all(&plain).unwrap();
        let finished2 = writer2.finish().unwrap();
        assert_eq!(finished2.hash_hex, finished.hash_hex);
        assert!(!pool.adopt("2", &finished2.hash_hex).unwrap());
        assert!(!pool.pending_path("2").exists());
        assert_eq!(
            pool.bytes_on_disk(&finished.hash_hex).unwrap(),
            finished.bytes_on_disk
        );
        pool.remove(&finished.hash_hex).unwrap();
        assert!(pool.open_read(&finished.hash_hex).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resume_mid_segment_reproduces_the_same_hash() {
        let (pool, dir) = scratch_pool("resume");
        let plain = pattern(SEGMENT_SIZE as usize * 3 + 777);
        // 第一次：寫到 1.5 段加一點雜訊（模擬最後一次快照之後多寫的、不可信的部分），然後中斷。
        let trusted = SEGMENT_SIZE as u64 + 30000;
        let mut writer = pool.create_pending("r").unwrap();
        writer.write_all(&plain[..trusted as usize]).unwrap();
        writer.write_all(&[0xEE; 5000]).unwrap(); // 不可信的尾巴
        writer.sync().unwrap();
        drop(writer);
        // 續：只信任到 trusted，之後的丟掉。
        let mut resumed = pool.resume_pending("r", trusted).unwrap();
        assert_eq!(resumed.plain_len(), trusted);
        resumed.write_all(&plain[trusted as usize..]).unwrap();
        let finished = resumed.finish().unwrap();
        assert_eq!(finished.hash_hex, blake3::hash(&plain).to_hex().to_string());
        pool.adopt("r", &finished.hash_hex).unwrap();
        let mut back = Vec::new();
        pool.open_read(&finished.hash_hex)
            .unwrap()
            .read_to_end(&mut back)
            .unwrap();
        assert_eq!(back, plain);
        // 續傳點剛好在段邊界也行。
        let mut writer = pool.create_pending("r2").unwrap();
        writer
            .write_all(&plain[..SEGMENT_SIZE as usize * 2])
            .unwrap();
        drop(writer);
        let mut resumed = pool.resume_pending("r2", SEGMENT_SIZE as u64 * 2).unwrap();
        resumed
            .write_all(&plain[SEGMENT_SIZE as usize * 2..])
            .unwrap();
        assert_eq!(resumed.finish().unwrap().hash_hex, finished.hash_hex);
        // 信任長度比檔裡有的還長：拒絕。
        let mut writer = pool.create_pending("r3").unwrap();
        writer.write_all(&plain[..1000]).unwrap();
        drop(writer);
        assert!(pool.resume_pending("r3", 5000).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tampering_and_wrong_key_are_rejected() {
        let (pool, dir) = scratch_pool("tamper");
        let plain = pattern(SEGMENT_SIZE as usize + 10);
        let mut writer = pool.create_pending("t").unwrap();
        writer.write_all(&plain).unwrap();
        let finished = writer.finish().unwrap();
        pool.adopt("t", &finished.hash_hex).unwrap();
        let path = pool.path_of(&finished.hash_hex).unwrap();
        // 翻第二段的一個 byte。
        let mut bytes = std::fs::read(&path).unwrap();
        let second = segment_offset(
            &Header {
                segment_size: SEGMENT_SIZE,
                nonce_base: [0; NONCE_BASE_LEN],
            },
            1,
        ) as usize;
        bytes[second + 3] ^= 1;
        std::fs::write(&path, &bytes).unwrap();
        let mut reader = pool.open_read(&finished.hash_hex).unwrap();
        let mut first = vec![0u8; SEGMENT_SIZE as usize];
        reader.read_exact(&mut first).unwrap(); // 第一段還好
        let mut rest = Vec::new();
        assert!(reader.read_to_end(&mut rest).is_err()); // 第二段壞
                                                         // 別把金鑰。
        let other = MediaPool::open(&dir, Key32([8u8; 32])).unwrap();
        let mut reader = other.open_read(&finished.hash_hex).unwrap();
        assert!(reader.read_to_end(&mut Vec::new()).is_err());
        // 把第 0 段搬到第 1 段：AAD／nonce 帶段號，解不開。
        let mut bytes = std::fs::read(&path).unwrap();
        let seg = SEGMENT_SIZE as usize + TAG_LEN as usize;
        let first_segment: Vec<u8> = bytes[HEADER_LEN as usize..HEADER_LEN as usize + seg].to_vec();
        bytes.truncate(HEADER_LEN as usize + seg);
        bytes.extend_from_slice(&first_segment);
        std::fs::write(&path, &bytes).unwrap();
        let mut reader = pool.open_read(&finished.hash_hex).unwrap();
        reader.seek(SeekFrom::Start(SEGMENT_SIZE as u64)).unwrap();
        assert!(reader.read(&mut [0u8; 8]).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 暫定段與正式段是兩個 nonce：暫定段用正式 nonce 解不開（反之亦然），所以同段號重封不是 nonce 重用。
    #[test]
    fn provisional_segment_uses_its_own_nonce() {
        let (pool, dir) = scratch_pool("provisional");
        let plain = pattern(SEGMENT_SIZE as usize + 3000);
        let mut writer = pool.create_pending("p").unwrap();
        writer.write_all(&plain).unwrap();
        writer.sync().unwrap(); // 段 1 是 3000 byte 的暫定段
        let path = writer.path().to_path_buf();
        drop(writer);
        let cipher = XChaCha20Poly1305::new(Key32([9u8; 32]).as_bytes().into());
        let mut file = File::open(&path).unwrap();
        let header = read_header(&mut file).unwrap();
        assert!(
            read_segment(&cipher, &header, &mut file, 1).is_err(),
            "final nonce must not open a provisional segment"
        );
        assert_eq!(
            read_segment_as(&cipher, &header, &mut file, 1, true).unwrap(),
            &plain[SEGMENT_SIZE as usize..]
        );
        assert!(
            read_segment_as(&cipher, &header, &mut file, 0, true).is_err(),
            "provisional nonce must not open a final segment"
        );
        // 續上去寫完：尾段被截掉、用正式 nonce 重封，完成檔裡沒有暫定段。
        let mut resumed = pool.resume_pending("p", plain.len() as u64).unwrap();
        resumed.write_all(&[7u8; 10]).unwrap();
        let finished = resumed.finish().unwrap();
        pool.adopt("p", &finished.hash_hex).unwrap();
        let mut back = Vec::new();
        pool.open_read(&finished.hash_hex)
            .unwrap()
            .read_to_end(&mut back)
            .unwrap();
        assert_eq!(back.len(), plain.len() + 10);
        assert_eq!(pool.list_files().unwrap(), vec![finished.hash_hex.clone()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn empty_file_and_bad_names() {
        let (pool, dir) = scratch_pool("empty");
        let writer = pool.create_pending("e").unwrap();
        let finished = writer.finish().unwrap();
        assert_eq!(finished.plain_len, 0);
        assert_eq!(finished.hash_hex, blake3::hash(b"").to_hex().to_string());
        pool.adopt("e", &finished.hash_hex).unwrap();
        let mut reader = pool.open_read(&finished.hash_hex).unwrap();
        assert_eq!(reader.plain_len(), 0);
        assert_eq!(reader.read(&mut [0u8; 4]).unwrap(), 0);
        assert!(pool.path_of("../x").is_err());
        assert!(pool.path_of("zz").is_err());
        assert_eq!(pool.list_pending().unwrap(), Vec::<String>::new());
        let _ = std::fs::remove_dir_all(&dir);
    }
}

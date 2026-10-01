//! seek 暫存檔（/docs/design/media/media-download.md §4.2）：播放器 seek 到主檔還沒拉到的地方時，現拉的塊照到達順序 append 在這裡，
//! 一格一塊、每格固定大小；記憶體裡一張位置表（塊號 → 第幾格）讓查詢是 O(1)。主檔完成就整個刪掉。
//!
//! ```text
//! 檔頭 32 byte：magic "WBFS"(4) ‖ version u8 = 1 ‖ 保留 3 byte ‖ chunk_size u32 LE ‖ chunk_count u32 LE ‖ nonce_base 16 byte
//! 第 s 格（C = 4 + 4 + chunk_size + 16 byte）= u32_le(塊號 i)
//!     ‖ XChaCha20-Poly1305(池金鑰, nonce = nonce_base ‖ u64_le(s),
//!                          aad = "wbf-media-seek v1" ‖ nonce_base ‖ u64_le(s) ‖ u32_le(i) ‖ u32_le(chunk_size) ‖ mxc,
//!                          明文 = u32_le(len) ‖ 第 i 塊的明文(len) ‖ 0 × (chunk_size − len))
//! ```
//!
//! - 塊號放明文（重開不必解密就能重建位置表），但綁在 AAD 裡：改了就解不開。
//! - mxc 也在 AAD 裡：同一個暫存名換了主人（`cache.db` 重建後 id 重新編號），舊格一律解不開；開檔時試解第一格，解不開就整個重建。
//! - 寫入順序：整格寫完 → fsync → 才更新位置表。位置表永遠只指向已經落地的格。
//! - 每格只寫一次：格號只增不減，nonce 由格號決定。
//!
//! 🚫 金鑰不進錯誤訊息。這裡沒有 SQL、沒有網路。

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use zeroize::Zeroizing;

use crate::error::SdkError;
use crate::media_pool::{pool_error, random_nonce_base, MediaPool};

const MAGIC: &[u8; 4] = b"WBFS";
const VERSION: u8 = 1;
const HEADER_LEN: u64 = 32;
const NONCE_BASE_LEN: usize = 16;
const INDEX_FIELD: u64 = 4;
const LEN_FIELD: u64 = 4;
const TAG_LEN: u64 = 16;
const AAD_PREFIX: &[u8] = b"wbf-media-seek v1";
/// 位置表最多幾格：`4 byte × chunk_count` 在記憶體裡。64 Mi 塊是 256 MiB 的表——再多就不給 seek（主檔照常順序拉）。
pub const MAX_SEEK_CHUNKS: u32 = 1 << 26;

/// 一個 mxc 的 seek 暫存檔與它的位置表。
pub struct SeekStore {
    file: File,
    cipher: XChaCha20Poly1305,
    nonce_base: [u8; NONCE_BASE_LEN],
    chunk_size: u32,
    mxc: String,
    /// `slots[i] = s + 1`：第 i 塊在第 s 格；0 ＝ 沒有。
    slots: Vec<u32>,
    /// 檔裡已經有幾格（下一格的格號）。
    cells: u32,
}

impl SeekStore {
    /// 開這個 mxc 的 seek 暫存檔：在就驗檔頭、截掉寫到一半的格、重建位置表；不在（或對不上）就建新的。
    ///
    /// Args:
    ///     pool: 暫存檔放在池的 `pending/`，金鑰是池金鑰
    ///     pending_name: 主檔的暫存名, example: "m12"
    ///     mxc: example: "mxc://localhost/000000000000004d"
    ///     chunk_size: 事件區塊的, example: 65536
    ///     chunk_count: example: 160
    /// Return:
    ///     Ok(SeekStore)
    ///     Err(Usage)   `chunk_size` 是 0、`chunk_count` 是 0 或超過 `MAX_SEEK_CHUNKS`
    ///     Err(Io)      檔開不了、寫不了
    pub fn open(
        pool: &MediaPool,
        pending_name: &str,
        mxc: &str,
        chunk_size: u32,
        chunk_count: u32,
    ) -> Result<SeekStore, SdkError> {
        if chunk_size == 0 || chunk_count == 0 || chunk_count > MAX_SEEK_CHUNKS {
            return Err(SdkError::Usage(format!(
                "no seek file for chunk_size {chunk_size} × chunk_count {chunk_count}"
            )));
        }
        let path = pool.seek_path(pending_name);
        let cipher = XChaCha20Poly1305::new(pool.key().as_bytes().into());
        if let Ok(file) = OpenOptions::new().read(true).write(true).open(&path) {
            if let Some(store) =
                SeekStore::reopen(file, cipher.clone(), mxc, chunk_size, chunk_count)?
            {
                return Ok(store);
            }
        }
        let nonce_base = random_nonce_base()?;
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)?;
        file.write_all(&encode_header(chunk_size, chunk_count, &nonce_base))?;
        file.sync_data()?;
        Ok(SeekStore {
            file,
            cipher,
            nonce_base,
            chunk_size,
            mxc: mxc.to_string(),
            slots: vec![0; chunk_count as usize],
            cells: 0,
        })
    }

    /// 既有的檔：驗檔頭、截半格、重建位置表、試解第一格。
    ///
    /// Return:
    ///     Ok(Some(SeekStore))   可以接著用
    ///     Ok(None)              不是這個檔的（檔頭不對、chunk_size／chunk_count 不同、第一格解不開）：呼叫者建新的
    ///     Err(Io)               截檔失敗
    fn reopen(
        mut file: File,
        cipher: XChaCha20Poly1305,
        mxc: &str,
        chunk_size: u32,
        chunk_count: u32,
    ) -> Result<Option<SeekStore>, SdkError> {
        let mut header = [0u8; HEADER_LEN as usize];
        if file.read_exact(&mut header).is_err()
            || &header[..4] != MAGIC
            || header[4] != VERSION
            || u32::from_le_bytes([header[8], header[9], header[10], header[11]]) != chunk_size
            || u32::from_le_bytes([header[12], header[13], header[14], header[15]]) != chunk_count
        {
            return Ok(None);
        }
        let mut nonce_base = [0u8; NONCE_BASE_LEN];
        nonce_base.copy_from_slice(&header[16..16 + NONCE_BASE_LEN]);
        let mut store = SeekStore {
            file,
            cipher,
            nonce_base,
            chunk_size,
            mxc: mxc.to_string(),
            slots: vec![0; chunk_count as usize],
            cells: 0,
        };
        let cell = store.cell_len();
        let body = store.file.metadata()?.len().saturating_sub(HEADER_LEN);
        let whole_cells = u32::try_from(body / cell).unwrap_or(u32::MAX);
        let mut cells = 0u32;
        for slot in 0..whole_cells {
            let mut field = [0u8; INDEX_FIELD as usize];
            store.file.seek(SeekFrom::Start(store.cell_offset(slot)))?;
            store.file.read_exact(&mut field)?;
            let index = u32::from_le_bytes(field);
            // 塊號超出範圍：檔壞了，從這格起截掉。
            let Some(entry) = store.slots.get_mut(index as usize) else {
                break;
            };
            // 同一塊出現兩次：留第一個。
            if *entry == 0 {
                *entry = slot + 1;
            }
            cells = slot + 1;
        }
        store.cells = cells;
        store.file.set_len(store.cell_offset(cells))?;
        if cells > 0 && store.read_cell(0).is_err() {
            return Ok(None);
        }
        Ok(Some(store))
    }

    /// 每格在磁碟上的大小 C。
    fn cell_len(&self) -> u64 {
        INDEX_FIELD + LEN_FIELD + u64::from(self.chunk_size) + TAG_LEN
    }

    fn cell_offset(&self, slot: u32) -> u64 {
        HEADER_LEN + u64::from(slot) * self.cell_len()
    }

    fn nonce_and_aad(&self, slot: u32, index: u32) -> ([u8; 24], Vec<u8>) {
        let mut nonce = [0u8; 24];
        nonce[..NONCE_BASE_LEN].copy_from_slice(&self.nonce_base);
        nonce[NONCE_BASE_LEN..].copy_from_slice(&u64::from(slot).to_le_bytes());
        let mut aad = Vec::with_capacity(AAD_PREFIX.len() + NONCE_BASE_LEN + 16 + self.mxc.len());
        aad.extend_from_slice(AAD_PREFIX);
        aad.extend_from_slice(&self.nonce_base);
        aad.extend_from_slice(&u64::from(slot).to_le_bytes());
        aad.extend_from_slice(&index.to_le_bytes());
        aad.extend_from_slice(&self.chunk_size.to_le_bytes());
        aad.extend_from_slice(self.mxc.as_bytes());
        (nonce, aad)
    }

    /// 位置表裡有沒有第 `index` 塊。
    pub fn has(&self, index: u32) -> bool {
        self.slots
            .get(index as usize)
            .is_some_and(|slot| *slot != 0)
    }

    /// 已經有幾格。
    pub fn cells(&self) -> u32 {
        self.cells
    }

    /// 把第 `index` 塊的明文 append 成新的一格。已經有了就不寫（同一塊被兩個 GET 要，只存一次）。
    ///
    /// Args:
    ///     index: 塊號, example: 42
    ///     plain: 驗過的明文；最後一塊可以短, example: 65536 byte
    /// Return:
    ///     Ok(())
    ///     Err(Usage)   塊號超出 chunk_count、明文是空的或比 chunk_size 長
    ///     Err(Io)      寫不了（位置表沒動：寫失敗的格不會被指到）
    pub fn append(&mut self, index: u32, plain: &[u8]) -> Result<(), SdkError> {
        if self.slots.get(index as usize).is_none() {
            return Err(SdkError::Usage(format!(
                "chunk {index} is past chunk_count {}",
                self.slots.len()
            )));
        }
        if self.has(index) {
            return Ok(());
        }
        if plain.is_empty() || plain.len() > self.chunk_size as usize {
            return Err(SdkError::Usage(format!(
                "chunk {index} has {} bytes (1..={} allowed)",
                plain.len(),
                self.chunk_size
            )));
        }
        let slot = self.cells;
        let mut padded = Zeroizing::new(Vec::with_capacity(
            LEN_FIELD as usize + self.chunk_size as usize,
        ));
        padded.extend_from_slice(&(plain.len() as u32).to_le_bytes());
        padded.extend_from_slice(plain);
        padded.resize(LEN_FIELD as usize + self.chunk_size as usize, 0);
        let (nonce, aad) = self.nonce_and_aad(slot, index);
        let sealed = self
            .cipher
            .encrypt(
                XNonce::from_slice(&nonce),
                Payload {
                    msg: &padded,
                    aad: &aad,
                },
            )
            .map_err(|_| pool_error(format!("sealing seek cell {slot} failed")))?;
        let mut cell = Vec::with_capacity(self.cell_len() as usize);
        cell.extend_from_slice(&index.to_le_bytes());
        cell.extend_from_slice(&sealed);
        self.file.seek(SeekFrom::Start(self.cell_offset(slot)))?;
        self.file.write_all(&cell)?;
        self.file.sync_data()?;
        // 落地之後才指過去。
        self.cells = slot + 1;
        if let Some(entry) = self.slots.get_mut(index as usize) {
            *entry = slot + 1;
        }
        Ok(())
    }

    /// 讀第 `index` 塊。那一格解不開就從位置表拿掉、當成沒有（/docs/design/media/media-download.md §4.2 重開第 4 條）。
    ///
    /// Return:
    ///     Ok(Some(明文))   有，而且解得開
    ///     Ok(None)         沒有，或那一格壞了（已從位置表拿掉，呼叫者重拉）
    pub fn read(&mut self, index: u32) -> Result<Option<Zeroizing<Vec<u8>>>, SdkError> {
        let Some(slot) = self
            .slots
            .get(index as usize)
            .copied()
            .filter(|slot| *slot != 0)
        else {
            return Ok(None);
        };
        match self.read_cell(slot - 1) {
            Ok((stored_index, plain)) if stored_index == index => Ok(Some(plain)),
            _ => {
                if let Some(entry) = self.slots.get_mut(index as usize) {
                    *entry = 0;
                }
                Ok(None)
            }
        }
    }

    /// 解第 `slot` 格。
    ///
    /// Return:
    ///     Ok((塊號, 明文))
    ///     Err(Io)   讀不到、解不開、長度欄不合
    fn read_cell(&mut self, slot: u32) -> Result<(u32, Zeroizing<Vec<u8>>), SdkError> {
        let mut cell = vec![0u8; self.cell_len() as usize];
        self.file.seek(SeekFrom::Start(self.cell_offset(slot)))?;
        self.file.read_exact(&mut cell)?;
        let (index_field, sealed) = cell
            .split_first_chunk::<4>()
            .ok_or_else(|| pool_error(format!("seek cell {slot} is too short")))?;
        let index = u32::from_le_bytes(*index_field);
        let (nonce, aad) = self.nonce_and_aad(slot, index);
        let padded = Zeroizing::new(
            self.cipher
                .decrypt(
                    XNonce::from_slice(&nonce),
                    Payload {
                        msg: sealed,
                        aad: &aad,
                    },
                )
                .map_err(|_| pool_error(format!("seek cell {slot} does not authenticate")))?,
        );
        let len = padded
            .first_chunk::<4>()
            .map(|field| u32::from_le_bytes(*field) as usize)
            .ok_or_else(|| pool_error(format!("seek cell {slot} has no length field")))?;
        if len == 0 || len > self.chunk_size as usize {
            return Err(pool_error(format!(
                "seek cell {slot} claims {len} bytes (1..={} allowed)",
                self.chunk_size
            )));
        }
        let plain = padded
            .get(LEN_FIELD as usize..LEN_FIELD as usize + len)
            .ok_or_else(|| pool_error(format!("seek cell {slot} length runs past the cell")))?;
        Ok((index, Zeroizing::new(plain.to_vec())))
    }
}

fn encode_header(
    chunk_size: u32,
    chunk_count: u32,
    nonce_base: &[u8; NONCE_BASE_LEN],
) -> [u8; HEADER_LEN as usize] {
    let mut bytes = [0u8; HEADER_LEN as usize];
    bytes[..4].copy_from_slice(MAGIC);
    bytes[4] = VERSION;
    bytes[8..12].copy_from_slice(&chunk_size.to_le_bytes());
    bytes[12..16].copy_from_slice(&chunk_count.to_le_bytes());
    bytes[16..16 + NONCE_BASE_LEN].copy_from_slice(nonce_base);
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vault::Key32;

    const MXC: &str = "mxc://localhost/0001";
    const CHUNK: u32 = 4096;

    fn scratch_pool(name: &str) -> (MediaPool, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("wbf-seek-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        (MediaPool::open(&dir, Key32([5u8; 32])).unwrap(), dir)
    }

    fn chunk(index: u32, len: usize) -> Vec<u8> {
        (0..len).map(|i| (i as u32 ^ index) as u8).collect()
    }

    #[test]
    fn cells_append_in_arrival_order_and_the_table_finds_them() {
        let (pool, dir) = scratch_pool("order");
        let mut store = SeekStore::open(&pool, "m1", MXC, CHUNK, 10).unwrap();
        store.append(7, &chunk(7, CHUNK as usize)).unwrap();
        store.append(9, &chunk(9, 100)).unwrap(); // 最後一塊短，照樣寫滿一格
        store.append(7, &chunk(0, CHUNK as usize)).unwrap(); // 已經有了：不寫
        assert_eq!(store.cells(), 2);
        assert!(store.has(7) && store.has(9) && !store.has(0));
        assert_eq!(
            &store.read(7).unwrap().unwrap()[..],
            &chunk(7, CHUNK as usize)[..]
        );
        assert_eq!(&store.read(9).unwrap().unwrap()[..], &chunk(9, 100)[..]);
        assert!(store.read(3).unwrap().is_none());
        let cell = INDEX_FIELD + LEN_FIELD + u64::from(CHUNK) + TAG_LEN;
        assert_eq!(
            std::fs::metadata(pool.seek_path("m1")).unwrap().len(),
            HEADER_LEN + 2 * cell
        );
        assert!(store.append(10, &chunk(10, 10)).is_err());
        assert!(store.append(1, &[]).is_err());
        assert!(store.append(1, &chunk(1, CHUNK as usize + 1)).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reopening_cuts_a_half_cell_rebuilds_the_table_and_keeps_the_first_duplicate() {
        let (pool, dir) = scratch_pool("reopen");
        let mut store = SeekStore::open(&pool, "m1", MXC, CHUNK, 10).unwrap();
        store.append(4, &chunk(4, CHUNK as usize)).unwrap();
        store.append(2, &chunk(2, CHUNK as usize)).unwrap();
        drop(store);
        let path = pool.seek_path("m1");
        let cell = (INDEX_FIELD + LEN_FIELD + u64::from(CHUNK) + TAG_LEN) as usize;
        let mut bytes = std::fs::read(&path).unwrap();
        // 第 0 格複製成第 2 格（同塊號重複），再多半格。
        let first = bytes[HEADER_LEN as usize..HEADER_LEN as usize + cell].to_vec();
        bytes.extend_from_slice(&first);
        bytes.extend_from_slice(&[0xCD; 100]);
        std::fs::write(&path, &bytes).unwrap();
        let mut store = SeekStore::open(&pool, "m1", MXC, CHUNK, 10).unwrap();
        assert_eq!(store.cells(), 3);
        assert_eq!(
            std::fs::metadata(&path).unwrap().len(),
            HEADER_LEN + 3 * cell as u64
        );
        assert_eq!(
            &store.read(4).unwrap().unwrap()[..],
            &chunk(4, CHUNK as usize)[..]
        );
        assert_eq!(
            &store.read(2).unwrap().unwrap()[..],
            &chunk(2, CHUNK as usize)[..]
        );
        // 新的一格接在後面。
        store.append(0, &chunk(0, 1)).unwrap();
        assert_eq!(store.cells(), 4);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_index_past_the_count_cuts_the_file_there() {
        let (pool, dir) = scratch_pool("past");
        let mut store = SeekStore::open(&pool, "m1", MXC, CHUNK, 10).unwrap();
        store.append(1, &chunk(1, CHUNK as usize)).unwrap();
        store.append(2, &chunk(2, CHUNK as usize)).unwrap();
        drop(store);
        let path = pool.seek_path("m1");
        let cell = (INDEX_FIELD + LEN_FIELD + u64::from(CHUNK) + TAG_LEN) as usize;
        let mut bytes = std::fs::read(&path).unwrap();
        let second = HEADER_LEN as usize + cell;
        bytes[second..second + 4].copy_from_slice(&99u32.to_le_bytes());
        std::fs::write(&path, &bytes).unwrap();
        let mut store = SeekStore::open(&pool, "m1", MXC, CHUNK, 10).unwrap();
        assert_eq!(store.cells(), 1);
        assert!(store.has(1) && !store.has(2));
        assert!(store.read(1).unwrap().is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_changed_index_a_flipped_bit_or_another_media_does_not_open() {
        let (pool, dir) = scratch_pool("bound");
        let mut store = SeekStore::open(&pool, "m1", MXC, CHUNK, 10).unwrap();
        store.append(1, &chunk(1, CHUNK as usize)).unwrap();
        store.append(2, &chunk(2, CHUNK as usize)).unwrap();
        drop(store);
        let path = pool.seek_path("m1");
        let cell = (INDEX_FIELD + LEN_FIELD + u64::from(CHUNK) + TAG_LEN) as usize;
        let original = std::fs::read(&path).unwrap();
        // 第二格的塊號改成 3：AAD 綁塊號，解不開，讀時從表裡拿掉。
        let mut bytes = original.clone();
        let second = HEADER_LEN as usize + cell;
        bytes[second..second + 4].copy_from_slice(&3u32.to_le_bytes());
        std::fs::write(&path, &bytes).unwrap();
        let mut store = SeekStore::open(&pool, "m1", MXC, CHUNK, 10).unwrap();
        assert!(store.has(3));
        assert!(store.read(3).unwrap().is_none());
        assert!(!store.has(3));
        drop(store);
        // 第二格翻一個 bit。
        let mut bytes = original.clone();
        bytes[second + 40] ^= 1;
        std::fs::write(&path, &bytes).unwrap();
        let mut store = SeekStore::open(&pool, "m1", MXC, CHUNK, 10).unwrap();
        assert!(store.read(2).unwrap().is_none());
        assert!(store.read(1).unwrap().is_some());
        drop(store);
        // 別的 mxc 拿到同一個暫存名：第一格解不開，整個重建。
        std::fs::write(&path, &original).unwrap();
        let store = SeekStore::open(&pool, "m1", "mxc://localhost/other", CHUNK, 10).unwrap();
        assert_eq!(store.cells(), 0);
        assert!(!store.has(1));
        drop(store);
        // chunk_size 不同：整個重建。
        std::fs::write(&path, &original).unwrap();
        let store = SeekStore::open(&pool, "m1", MXC, CHUNK * 2, 10).unwrap();
        assert_eq!(store.cells(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_table_too_large_is_refused() {
        let (pool, dir) = scratch_pool("large");
        assert!(SeekStore::open(&pool, "m1", MXC, CHUNK, MAX_SEEK_CHUNKS + 1).is_err());
        assert!(SeekStore::open(&pool, "m1", MXC, 0, 1).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}

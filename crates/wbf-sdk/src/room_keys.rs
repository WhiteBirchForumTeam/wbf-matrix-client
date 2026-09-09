//! 本地的房間金鑰備份（local-cache-db.md §10.4）：一房一檔、順序 append、整份加密。
//!
//! 為什麼要有它：房間金鑰（Megolm inbound session）本來只活在 matrix-sdk 的 `crypto.db` 裡，
//! 那個目錄 `logout` 會刪、壞掉也叫人刪。server 端的標準 backup 是主力，但在使用者產生
//! recovery key 之前，**解開它的私鑰也只在本機的 crypto store 裡**——所以本地這一份是那段期間
//! 唯一救得回歷史的東西（§10.2）。
//!
//! 落地格式（`WBFRK1`）：
//!
//! ```text
//! 檔頭 32 byte：magic "WBFRK1\0\0"(8) ‖ version u16 LE = 1 ‖ 保留 6 byte ‖ file_id 16 byte（隨機）
//! 之後每筆：  u32 LE 密文長度 ‖ XChaCha20-Poly1305 密文（含 16 byte 標籤）
//!             nonce = file_id ‖ u64_le(這是第幾筆)   16 + 8 = 24 byte
//!             aad   = 整個 32 byte 檔頭               搬到別的檔就解不開
//! ```
//!
//! - **序號不寫進檔案**，它就是「這筆在檔案裡的順序」。存兩份遲早漂移，而讀的時候本來就要從頭數。
//! - **只 append，不改寫、不刪單筆**。同一個 `session_id` 可以出現多次（後來拿到 index 更小、
//!   更完整的那把）；import 時全部餵回上游，由它比 `first_known_index` 決定留哪把——🚫 我們不做取捨。
//! - 尾巴壞掉（寫到一半斷電）只截斷：前面成功的照樣可用，🚫 不因為最後一筆壞掉就丟掉整個檔。
//! - 一筆金鑰的明文就是上游 `ExportedRoomKey` 的 JSON，這個模組**當它是不透明字串**，只讀
//!   `session_id` 與 `first_known_index` 兩個欄位來去重——所以這裡沒有 matrix 型別、不吃 `matrix` feature。
//!
//! 🚫 金鑰不進錯誤訊息、不 log。這裡沒有 SQL、沒有網路。

use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};

use crate::error::SdkError;
use crate::vault::Key32;

pub const ROOM_KEYS_DIR_NAME: &str = "room-keys";

const MAGIC: &[u8; 8] = b"WBFRK1\0\0";
const VERSION: u16 = 1;
const HEADER_LEN: usize = 32;
const FILE_ID_LEN: usize = 16;
const TAG_LEN: usize = 16;
/// 一筆金鑰再大也不該到這個地步；擋住壞掉的長度前綴讓我們一次配置幾 GB。
const MAX_RECORD_LEN: u32 = 1 << 20;

/// 一個帳號的本地金鑰備份目錄。
pub struct RoomKeyStore {
    dir: PathBuf,
    key: Key32,
}

/// 讀一個房間的檔案讀到什麼。
#[derive(Debug, Default, PartialEq, Eq)]
pub struct RoomKeyRecords {
    /// 每筆金鑰的 JSON，原樣。
    pub records: Vec<String>,
    /// 從第幾筆開始讀不下去（尾巴壞掉）。`None` = 整個檔都好的。
    pub truncated_at: Option<usize>,
}

impl RoomKeyStore {
    /// Args:
    ///     account_dir: example: "<data dir>/servers/<b58>_<b58>/accounts/<b58>_<b58>"（池在它底下的 room-keys/）
    ///     key: example: vault.room_key_backup_key()
    pub fn open(account_dir: &Path, key: Key32) -> Result<RoomKeyStore, SdkError> {
        let dir = account_dir.join(ROOM_KEYS_DIR_NAME);
        std::fs::create_dir_all(&dir)?;
        Ok(RoomKeyStore { dir, key })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// 這個房間的檔案叫什麼。用 keyed hash 而不是 room_id：room_id 有 `!` 與 `:`（Windows 檔名不合法），
    /// 而且目錄名不該洩漏這個帳號在哪些房。
    fn path_of(&self, room_id: &str) -> PathBuf {
        let hash = blake3::keyed_hash(self.key.as_bytes(), room_id.as_bytes());
        self.dir
            .join(format!("{}.keys", hex::encode(&hash.as_bytes()[..16])))
    }

    /// 把還沒存過的那幾筆 append 進去。已經有的（同 `session_id` 同 `first_known_index`）跳過。
    ///
    /// Args:
    ///     room_id: example: "!abc:localhost"
    ///     records: 每筆是一把金鑰的 JSON, example: &["{\"session_id\":\"s1\",…}".to_string()]
    /// Return:
    ///     Ok(usize)   實際寫進去幾筆（已經有的不算）
    ///     Err(Io)     開不了檔、寫不進去
    pub fn append_new(&self, room_id: &str, records: &[String]) -> Result<usize, SdkError> {
        let path = self.path_of(room_id);
        let existing = self.read_file(&path)?;
        let mut seen: HashSet<(String, i64)> = existing
            .records
            .iter()
            .filter_map(|record| identity_of(record))
            .collect();
        let fresh: Vec<&String> = records
            .iter()
            .filter(|record| match identity_of(record) {
                Some(identity) => seen.insert(identity),
                // 認不出 session_id 的就照收：漏存一把金鑰比多存一把糟得多。
                None => true,
            })
            .collect();
        if fresh.is_empty() {
            return Ok(0);
        }
        let (mut file, header) = self.open_for_append(&path)?;
        // 序號接在既有那幾筆後面：它就是「這筆在檔案裡的順序」，也是 nonce 的一半。
        let first_index = existing.records.len() as u64;
        for (index, record) in (first_index..).zip(fresh.iter()) {
            let sealed = self.seal(&header, index, record.as_bytes())?;
            file.write_all(&(sealed.len() as u32).to_le_bytes())?;
            file.write_all(&sealed)?;
        }
        // fsync 過才算數：這是「不能漏」的那一份（§10.5）。
        file.sync_all()?;
        Ok(fresh.len())
    }

    /// 讀一個房間的全部金鑰。
    pub fn read_room(&self, room_id: &str) -> Result<RoomKeyRecords, SdkError> {
        self.read_file(&self.path_of(room_id))
    }

    /// 讀這個帳號所有房間的金鑰（`key-backup import` 用）。
    ///
    /// Return:
    ///     Ok(RoomKeyRecords)   所有檔案的金鑰接起來；`truncated_at` 是「有幾個檔的尾巴壞掉」
    pub fn read_all(&self) -> Result<RoomKeyRecords, SdkError> {
        let mut all = RoomKeyRecords::default();
        let mut damaged = 0usize;
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return Ok(all);
        };
        for entry in entries {
            let path = entry?.path();
            if path.extension().and_then(|extension| extension.to_str()) != Some("keys") {
                continue;
            }
            let read = self.read_file(&path)?;
            if read.truncated_at.is_some() {
                damaged += 1;
            }
            all.records.extend(read.records);
        }
        all.truncated_at = (damaged > 0).then_some(damaged);
        Ok(all)
    }

    /// 這個帳號本地存了幾把金鑰（`key-backup status` 用）。
    pub fn count_keys(&self) -> Result<usize, SdkError> {
        Ok(self.read_all()?.records.len())
    }

    fn read_file(&self, path: &Path) -> Result<RoomKeyRecords, SdkError> {
        let mut file = match File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(RoomKeyRecords::default())
            }
            Err(error) => return Err(error.into()),
        };
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        if bytes.len() < HEADER_LEN || &bytes[..MAGIC.len()] != MAGIC {
            // 不是我們的檔（或連檔頭都沒寫完）：當作沒有，🚫 不刪、🚫 不報錯（fail closed）。
            return Ok(RoomKeyRecords::default());
        }
        let mut header = [0u8; HEADER_LEN];
        header.copy_from_slice(&bytes[..HEADER_LEN]);
        let mut read = RoomKeyRecords::default();
        let mut offset = HEADER_LEN;
        let mut index = 0u64;
        while offset < bytes.len() {
            let Some(length) = read_u32(&bytes, offset) else {
                read.truncated_at = Some(read.records.len());
                break;
            };
            offset += 4;
            if length as usize > MAX_RECORD_LEN as usize
                || length as usize <= TAG_LEN
                || offset + length as usize > bytes.len()
            {
                read.truncated_at = Some(read.records.len());
                break;
            }
            let sealed = &bytes[offset..offset + length as usize];
            offset += length as usize;
            match self.open_record(&header, index, sealed) {
                Some(record) => read.records.push(record),
                None => {
                    read.truncated_at = Some(read.records.len());
                    break;
                }
            }
            index += 1;
        }
        Ok(read)
    }

    /// 開檔（沒有就建並寫檔頭），回傳「可以 append 的 handle」與檔頭。
    fn open_for_append(&self, path: &Path) -> Result<(File, [u8; HEADER_LEN]), SdkError> {
        if let Ok(mut existing) = File::open(path) {
            let mut header = [0u8; HEADER_LEN];
            if existing.read_exact(&mut header).is_ok() && &header[..MAGIC.len()] == MAGIC {
                let file = OpenOptions::new().append(true).open(path)?;
                return Ok((file, header));
            }
        }
        let mut header = [0u8; HEADER_LEN];
        header[..MAGIC.len()].copy_from_slice(MAGIC);
        header[8..10].copy_from_slice(&VERSION.to_le_bytes());
        let mut file_id = [0u8; FILE_ID_LEN];
        getrandom::getrandom(&mut file_id)
            .map_err(|error| SdkError::Usage(format!("no randomness available: {error}")))?;
        header[16..32].copy_from_slice(&file_id);
        let mut file = create_private(path)?;
        file.write_all(&header)?;
        Ok((file, header))
    }

    fn seal(
        &self,
        header: &[u8; HEADER_LEN],
        index: u64,
        plaintext: &[u8],
    ) -> Result<Vec<u8>, SdkError> {
        XChaCha20Poly1305::new(self.key.as_bytes().into())
            .encrypt(
                XNonce::from_slice(&nonce_of(header, index)),
                Payload {
                    msg: plaintext,
                    aad: header,
                },
            )
            .map_err(|_| SdkError::Usage("cannot encrypt a room key record".into()))
    }

    fn open_record(&self, header: &[u8; HEADER_LEN], index: u64, sealed: &[u8]) -> Option<String> {
        let plaintext = XChaCha20Poly1305::new(self.key.as_bytes().into())
            .decrypt(
                XNonce::from_slice(&nonce_of(header, index)),
                Payload {
                    msg: sealed,
                    aad: header,
                },
            )
            .ok()?;
        String::from_utf8(plaintext).ok()
    }
}

/// nonce = file_id(16) ‖ 這是第幾筆(8)。同一個檔裡每筆的序號都不同，跨檔 file_id 不同，
/// 所以同一把金鑰不會拿同一個 nonce 封兩份不同的明文。
fn nonce_of(header: &[u8; HEADER_LEN], index: u64) -> [u8; 24] {
    let mut nonce = [0u8; 24];
    nonce[..FILE_ID_LEN].copy_from_slice(&header[16..32]);
    nonce[FILE_ID_LEN..].copy_from_slice(&index.to_le_bytes());
    nonce
}

fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    let slice = bytes.get(offset..offset + 4)?;
    Some(u32::from_le_bytes(slice.try_into().ok()?))
}

/// 去重用的身份：同一把 Megolm session 可能被存好幾次，只有 `first_known_index` 更小的那次才有新東西。
///
/// Return:
///     Some((session_id, first_known_index))
///     None   不是物件、沒有 session_id（認不出來就照收，別漏存金鑰）
fn identity_of(record: &str) -> Option<(String, i64)> {
    let value: serde_json::Value = serde_json::from_str(record).ok()?;
    let session_id = value.get("session_id")?.as_str()?.to_string();
    let first_known_index = value
        .get("first_known_index")
        .and_then(|index| index.as_i64())
        .unwrap_or(-1);
    Some((session_id, first_known_index))
}

/// 建一個只有自己讀得到的新檔（Unix 0600）。
fn create_private(path: &Path) -> Result<File, SdkError> {
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    Ok(options.open(path)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(name: &str) -> (PathBuf, RoomKeyStore) {
        let dir = std::env::temp_dir().join(format!("wbf-room-keys-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let store = RoomKeyStore::open(&dir, Key32([7u8; 32])).unwrap();
        (dir, store)
    }

    fn record(session_id: &str, index: i64) -> String {
        format!(
            r#"{{"algorithm":"m.megolm.v1.aes-sha2","room_id":"!r:localhost","session_id":"{session_id}","first_known_index":{index},"session_key":"AAAA"}}"#
        )
    }

    #[test]
    fn append_then_read_round_trip() {
        let (dir, store) = store("round-trip");
        let written = store
            .append_new("!r:localhost", &[record("s1", 0), record("s2", 0)])
            .unwrap();
        assert_eq!(written, 2);
        let read = store.read_room("!r:localhost").unwrap();
        assert_eq!(read.records.len(), 2);
        assert_eq!(read.truncated_at, None);
        assert!(read.records[0].contains("\"s1\""));
        assert_eq!(store.count_keys().unwrap(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_same_key_twice_is_not_stored_twice() {
        let (dir, store) = store("dedupe");
        store
            .append_new("!r:localhost", &[record("s1", 0)])
            .unwrap();
        assert_eq!(
            store
                .append_new("!r:localhost", &[record("s1", 0), record("s2", 5)])
                .unwrap(),
            1,
            "s1 已經有了，只該寫 s2"
        );
        // 同一把 session 但 index 更小＝更完整，那是新東西，要收。
        assert_eq!(
            store
                .append_new("!r:localhost", &[record("s1", 3)])
                .unwrap(),
            1
        );
        assert_eq!(store.count_keys().unwrap(), 3);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_file_name_does_not_leak_the_room_id() {
        let (dir, store) = store("filename");
        store
            .append_new("!secret:localhost", &[record("s1", 0)])
            .unwrap();
        let names: Vec<String> = std::fs::read_dir(store.dir())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names.len(), 1);
        assert!(!names[0].contains("secret"), "{}", names[0]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_damaged_tail_truncates_instead_of_losing_the_file() {
        let (dir, store) = store("damaged");
        store
            .append_new("!r:localhost", &[record("s1", 0), record("s2", 0)])
            .unwrap();
        // 砍掉最後 5 個 byte：最後一筆讀不完整。
        let path = std::fs::read_dir(store.dir())
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let bytes = std::fs::read(&path).unwrap();
        std::fs::write(&path, &bytes[..bytes.len() - 5]).unwrap();

        let read = store.read_room("!r:localhost").unwrap();
        assert_eq!(read.records.len(), 1, "前面那筆照樣要讀得出來");
        assert_eq!(read.truncated_at, Some(1));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn another_key_reads_nothing() {
        let (dir, store) = store("otherkey");
        store
            .append_new("!r:localhost", &[record("s1", 0)])
            .unwrap();
        let other = RoomKeyStore::open(&dir, Key32([9u8; 32])).unwrap();
        // 檔名是 keyed hash，所以別把金鑰連檔案都對不上；就算對上了 AEAD 也解不開。
        assert!(other.read_room("!r:localhost").unwrap().records.is_empty());
        assert_eq!(
            other.read_all().unwrap().records.len(),
            0,
            "掃到檔案也該解不開"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn records_survive_a_reopen() {
        let (dir, store) = store("reopen");
        store
            .append_new("!r:localhost", &[record("s1", 0)])
            .unwrap();
        drop(store);
        let store = RoomKeyStore::open(&dir, Key32([7u8; 32])).unwrap();
        store
            .append_new("!r:localhost", &[record("s2", 0)])
            .unwrap();
        assert_eq!(store.read_room("!r:localhost").unwrap().records.len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

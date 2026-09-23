//! `daemon.token` 的生命週期（architecture-v2 §4.3，維護者 2026-09-13 定）。
//!
//! token 是**前端**產生的、前端與 daemon 之間唯一的憑證（§4.4 從它導兩把金鑰）。它落在磁碟上
//! 只是為了「把它交給一個還沒啟動的程序」，所以那段落地時間要**盡量短**：
//!
//! ```text
//! 1. 前端寫 256 byte 隨機檔                     （Unix 0600）
//! 2. 前端 spawn daemon -s --token-file <路徑>   （🚫 不用命令列傳內容：那會進 ps 輸出）
//! 3. daemon 讀它、導金鑰、開 port，宣告 ready   （stdout 一行 JSON ＋ <data dir>/daemon.json）
//! 4. 前端收到 ready（或自己 hello 成功）之後**抹掉**那個檔（[`shred`]）
//! 5. 此後 token 只活在兩個程序的記憶體裡，磁碟上沒有
//! ```
//!
//! 🚨 **誰起的誰動**（維護者 2026-09-13）：token 檔屬於叫起 daemon 的那個程序。daemon **默認完全
//! 不動它**——🚫 不抹、🚫 不刪、🚫 不改權限。它讀一次，然後把 bytes 丟掉（只留導出的金鑰），
//! 並且**之後永遠不再回頭讀那個路徑**，所以第 4 步刪掉它不會讓任何東西壞掉
//! （`loopback.rs` 有一條測這個）。
//!
//! 📎 [`shred`] 放在這個 crate 是因為**這裡是 token 格式的主人**（長度、導鑰、生命週期都在這裡定）：
//! 前端／rpc-cli 呼叫它來做第 4 步，🚫 daemon 自己不會去叫它。
//!
//! 🚫 **不要「用既有的那份」**：token 一旦抹掉就沒有既有的那份了，重連就重產生一把。
//! 一把 token 活得越久，它落在磁碟上的那幾秒就越不只是那幾秒（備份、快照、同步資料夾都會抄走它）。

use std::fs::File;
use std::io::{Seek, SeekFrom, Write};
use std::path::Path;

/// 覆蓋幾遍、每遍填什麼（維護者 2026-09-13 指定的順序）。
///
/// 隨機那遍先寫：它是唯一「連舊值的統計痕跡都蓋掉」的一遍；`0xFF`、`0x00` 兩遍讓人**看得出來
/// 這個檔被故意清過**（全 0 的檔比一堆亂數更像「已處理」）。
const PASSES: [Pass; 3] = [Pass::Random, Pass::Fill(0xFF), Pass::Fill(0x00)];

/// 一次一塊的大小。token 只有 256 byte，但這個函式🚫 不假設呼叫者遞給它的檔多大。
const CHUNK: usize = 64 * 1024;

#[derive(Clone, Copy)]
enum Pass {
    Random,
    Fill(u8),
}

/// 抹掉一個 token 檔：三遍覆蓋（隨機 → `0xFF` → `0x00`）之後才刪。
///
/// ⚠️ 覆蓋是**盡力**，不是保證：SSD 的抹寫層與 CoW 檔案系統（APFS、Btrfs、ZFS、VSS 快照）可能
/// 把舊內容留在別處，那不是使用者空間管得到的。🚫 所以不要把它當成「這個 token 從此不可能被
/// 撿回來」——真正的防線是**token 只有幾秒鐘在磁碟上**（`PASSES` 之前的那四步）與它 0600。
/// 📎 但盡力仍然值得：它擋掉最廉價的那一類（`undelete`、目錄項還在的救援工具、被抄走的備份）。
///
/// Args:
///     path: token 檔, example: "/tmp/wbf-daemon.token"
/// Return:
///     Ok(true)   本來有，覆蓋完刪掉了
///     Ok(false)  本來就沒有（🚫 不算錯：第 4 步重跑兩次是正常的）
///     Err(...)   打不開、寫不進去、或刪不掉——⚠️ 這時檔案**可能還在**，呼叫端要講出來
pub fn shred(path: &Path) -> std::io::Result<bool> {
    let mut file = match File::options().write(true).open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    let length = file.metadata()?.len();
    overwrite(&mut file, length)?;
    // 先確定覆蓋真的落地，再解除連結：反過來的話目錄項先消失，覆蓋寫進一個沒有名字的檔。
    file.sync_all()?;
    drop(file);
    std::fs::remove_file(path)?;
    Ok(true)
}

/// 把 `length` 個 byte 覆蓋 [`PASSES`] 遍。每遍寫完 flush＋sync，不然三遍只有最後一遍落地。
fn overwrite(file: &mut File, length: u64) -> std::io::Result<()> {
    let mut buffer = vec![0u8; CHUNK];
    for pass in PASSES {
        file.seek(SeekFrom::Start(0))?;
        let mut written = 0u64;
        while written < length {
            let piece = std::cmp::min(CHUNK as u64, length - written) as usize;
            let Some(window) = buffer.get_mut(..piece) else {
                return Err(std::io::Error::other(
                    "wipe window is larger than the buffer",
                ));
            };
            match pass {
                // 失敗就整個失敗：寫一段假的隨機（例如全 0）比不寫更糟——它看起來像做過了。
                Pass::Random => getrandom::getrandom(window)
                    .map_err(|error| std::io::Error::other(format!("no OS randomness: {error}")))?,
                Pass::Fill(byte) => window.fill(byte),
            }
            file.write_all(window)?;
            written += piece as u64;
        }
        file.flush()?;
        file.sync_all()?;
    }
    Ok(())
}

/// token 檔只有它的主人讀得到嗎？
///
/// Args:
///     path: example: "/tmp/wbf-daemon.token"
/// Return:
///     Ok(true)   Unix 上 group／other 一個位元都沒有；Windows 一律 true（靠目錄 ACL）
///     Ok(false)  Unix 上別人讀得到
///     Err(...)   讀不到 metadata
#[cfg(unix)]
pub fn is_private(path: &Path) -> std::io::Result<bool> {
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(path)?.permissions().mode();
    Ok(mode & 0o077 == 0)
}

#[cfg(not(unix))]
pub fn is_private(path: &Path) -> std::io::Result<bool> {
    if !path.exists() {
        return Err(std::io::Error::from(std::io::ErrorKind::NotFound));
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("wbf-token-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("daemon.token")
    }

    #[test]
    fn shredding_overwrites_then_removes_and_a_missing_file_is_not_an_error() {
        let path = scratch("shred");
        std::fs::write(&path, [7u8; crate::pack::TOKEN_LEN]).unwrap();
        assert!(shred(&path).unwrap(), "本來有");
        assert!(!path.exists(), "刪掉了");
        // 第 4 步重跑一次：沒有檔不是錯。
        assert!(!shred(&path).unwrap());
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// 覆蓋那幾遍真的寫進去了（🚫 不是只有 `remove_file`）：最後一遍是 `0x00`，
    /// 所以刪除之前檔案內容應該是全 0、長度不變。
    #[test]
    fn the_last_pass_leaves_zeroes_and_keeps_the_length() {
        let path = scratch("passes");
        let original = [0xABu8; crate::pack::TOKEN_LEN];
        std::fs::write(&path, original).unwrap();
        let mut file = File::options().write(true).open(&path).unwrap();
        overwrite(&mut file, original.len() as u64).unwrap();
        drop(file);
        let after = std::fs::read(&path).unwrap();
        assert_eq!(after.len(), original.len(), "長度不變");
        assert!(after.iter().all(|byte| *byte == 0), "最後一遍是 0x00");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// 比 token 長的檔也要整份蓋到（CHUNK 邊界）。
    #[test]
    fn a_file_larger_than_one_chunk_is_covered_end_to_end() {
        let path = scratch("big");
        let length = CHUNK + 1234;
        std::fs::write(&path, vec![0xABu8; length]).unwrap();
        let mut file = File::options().write(true).open(&path).unwrap();
        overwrite(&mut file, length as u64).unwrap();
        drop(file);
        assert!(std::fs::read(&path).unwrap().iter().all(|byte| *byte == 0));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn is_private_says_not_found_for_a_file_that_is_not_there() {
        let path = scratch("missing");
        let _ = std::fs::remove_file(&path);
        assert_eq!(
            is_private(&path).unwrap_err().kind(),
            std::io::ErrorKind::NotFound
        );
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}

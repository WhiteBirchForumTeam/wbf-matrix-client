//! daemon 對資料目錄的寫權（`WriteAccess`）。鎖本身在 `wbf_core::data_dir_lock`：daemon 與 `wbf-cli` 共用同一份，
//! 🚫 不各寫一份（/docs/design/overview/architecture-v2.md §0.2：前端不越過 daemon 寫資料目錄，維護者 2026-09-30）。

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

pub use wbf_core::data_dir_lock::{
    lock_for_reading, lock_for_writing, DataDirLock, LockError, LOCK_FILE_NAME,
};

/// 「我現在有沒有寫這個資料目錄的能力？」—— daemon 的全局狀態（維護者 2026-09-13）。
///
/// ⭐ 這是把「鎖」變成「**能力**」的那一層：鎖是一個檔案上的事實，能力是**每個寫操作都要先問**的
/// 前提。分開之後，忘記問的地方會被閘門擋下，而不是靜靜地寫進別人的資料庫。
///
/// ```text
/// 起手                 is_granted() == false（🚨 預設沒有寫的能力）
/// 要寫的時候  grant()   拿排他鎖 ── 成功 ─> true，之後每次都是 O(1) 的 atomic 讀
///                                └ 失敗 ─> 維持 false，呼叫端回「我沒有寫的權限」
/// ```
///
/// | 誰 | 什麼時候 grant |
/// |---|---|
/// | `-s` 常駐 | **啟動的第一件事**；拿不到就不啟動（/docs/design/overview/architecture-v2.md §0.2） |
/// | 單發命令 | 那個命令**真的要寫**的時候才拿；`--version` 這種連讀都不用的🚫 不拿 |
///
/// ⚠️ 能力一旦拿到就**持有到程序結束**（`DataDirLock` 活在這裡）：🚫 不做「寫完就放、下次再拿」——
/// 那等於在每兩個寫操作之間開一扇門讓別人進來寫，而我們手上的快取狀態會在那時失效。
#[derive(Debug)]
pub struct WriteAccess {
    /// 快路徑：每個請求都會問，🚫 不要每次都去搶 mutex。
    granted: AtomicBool,
    /// 拿到的鎖本體。`None` 就是還沒拿到。
    held: Mutex<Option<DataDirLock>>,
}

impl WriteAccess {
    /// 🚨 起手是**沒有**寫的能力。
    pub fn none() -> WriteAccess {
        WriteAccess {
            granted: AtomicBool::new(false),
            held: Mutex::new(None),
        }
    }

    /// Return:
    ///     bool  true 表示這個程序現在握著這個資料目錄的寫權
    pub fn is_granted(&self) -> bool {
        self.granted.load(Ordering::SeqCst)
    }

    /// 要到寫的能力。**冪等**：已經有了就直接回 `Ok`。
    ///
    /// Args:
    ///     data_dir: example: "<data dir>"
    /// Return:
    ///     Ok(())               有了（本來就有，或這次拿到）
    ///     Err(HeldByAnother)   別人在用這個目錄 —— 呼叫端要回「我沒有寫的權限」，🚫 不要自己重試
    ///     Err(Io)              鎖檔開不了
    pub fn grant(&self, data_dir: &Path) -> Result<(), LockError> {
        // ⚠️ 先拿 mutex 再檢查一次：兩個請求同時進來的時候，只有一個該去搶鎖。
        let mut held = self
            .held
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if held.is_some() {
            return Ok(());
        }
        let lock = lock_for_writing(data_dir)?;
        *held = Some(lock);
        // ⚠️ 順序：鎖先放進去、旗標最後開。反過來的話會有一瞬間「說有能力但鎖還沒到手」。
        self.granted.store(true, Ordering::SeqCst);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_second_daemon_on_the_same_data_dir_is_refused_and_the_first_keeps_the_lock() {
        let dir = tempfile::tempdir().unwrap();
        let first = lock_for_writing(dir.path()).expect("第一個拿得到");
        match lock_for_writing(dir.path()) {
            Err(LockError::HeldByAnother(path)) => assert_eq!(path, *first.path()),
            other => panic!("第二個不該拿得到：{other:?}"),
        }
        // 第一個放手之後，下一個拿得到（🚫 不留殘留鎖）。
        drop(first);
        let second = lock_for_writing(dir.path()).expect("放手之後拿得到");
        assert!(second.path().exists());
    }

    /// 不同的資料目錄互不相干：一台機器上開兩個 daemon 是合法的，只要它們各有各的目錄。
    #[test]
    fn two_different_data_dirs_do_not_block_each_other() {
        let one = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let _first = lock_for_writing(one.path()).unwrap();
        let _second = lock_for_writing(other.path()).expect("另一個目錄不該被擋");
    }

    #[test]
    fn write_access_starts_off_and_stays_off_when_someone_else_holds_the_directory() {
        let dir = tempfile::tempdir().unwrap();
        let access = WriteAccess::none();
        // 🚨 預設沒有寫的能力。
        assert!(!access.is_granted());

        let squatter = lock_for_writing(dir.path()).unwrap();
        assert!(matches!(
            access.grant(dir.path()),
            Err(LockError::HeldByAnother(_))
        ));
        // 失敗之後**還是 false**：🚫 不可以「試過了就當作有」。
        assert!(!access.is_granted());

        drop(squatter);
        access.grant(dir.path()).expect("沒人佔著就拿得到");
        assert!(access.is_granted());
        // 冪等：再要一次不會失敗、也不會換一把鎖。
        access.grant(dir.path()).expect("冪等");
        assert!(access.is_granted());
        // 拿著的期間別人拿不到。
        assert!(matches!(
            lock_for_writing(dir.path()),
            Err(LockError::HeldByAnother(_))
        ));
    }

    /// 共享＋共享可以：多個唯讀的程序同時看同一個資料目錄是允許的。
    #[test]
    fn many_readers_can_hold_the_lock_at_the_same_time() {
        let dir = tempfile::tempdir().unwrap();
        let _one = lock_for_reading(dir.path()).expect("第一個讀者");
        let _another = lock_for_reading(dir.path()).expect("第二個讀者也拿得到");
        let _third = lock_for_reading(dir.path()).expect("第三個也是");
    }

    /// 共享＋排他不行，**兩個方向都不行**。
    #[test]
    fn a_reader_and_a_writer_cannot_hold_it_at_the_same_time() {
        let dir = tempfile::tempdir().unwrap();
        // 有人在讀 → 寫不進來（🚫 不會「反正只是讀，讓他寫」）。
        let reader = lock_for_reading(dir.path()).unwrap();
        assert!(
            matches!(
                lock_for_writing(dir.path()),
                Err(LockError::HeldByAnother(_))
            ),
            "有讀者的時候不該讓人拿寫鎖"
        );
        drop(reader);

        // 有人在寫 → 讀不進來。⭐ 這正是 daemon 跑著的時候唯讀工具會遇到的情況，
        // 而那個拒絕就是答案：去走 RPC，不要動它的檔。
        let writer = lock_for_writing(dir.path()).unwrap();
        assert!(
            matches!(
                lock_for_reading(dir.path()),
                Err(LockError::HeldByAnother(_))
            ),
            "有寫者的時候不該讓人拿讀鎖"
        );
        drop(writer);

        let _after = lock_for_reading(dir.path()).expect("放手之後讀得了");
    }
}

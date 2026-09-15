//! core 內部的「把東西打開」那一層：session、matrix backend、`cache.db`、媒體池。
//!
//! ⚠️ 這整個模組是 **`pub(crate)`**，而且要一直是。`Session`（裡面有 `access_token`）、
//! `MatrixBackend`、`Cache`、`MediaPool` 全是程序內的 handle：序列化不了、跨不了 FFI，
//! 而 `Session` 還帶著秘密——**它一個欄位都不該離開這個 crate**
//!（architecture-v2 §7；PR #24 審查 cirno🔴）。
//!
//! 外面看得到的是 `Core` 上那些回**可序列化 DTO** 的方法；handle 活在這裡，被它們用。

use crate::error::{CoreError, CoreErrorKind};
use wbf_sdk::backend::matrix_sdk::MatrixBackend;
use wbf_sdk::cache::{Cache, CacheIdentity, OpenOutcome};
use wbf_sdk::login::Session;
use wbf_sdk::media_pool::MediaPool;

use crate::accounts::AccountDir;
use crate::Core;

impl Core {
    /// 這個帳號封著的 session。
    ///
    /// 🚫 **回傳值不准離開這個 crate**：`Session::access_token` 是秘密。要給外面看
    /// 「我是誰」用 [`crate::WhoAmI`]。
    ///
    /// Return:
    ///     Ok(Session)
    ///     Err(Usage)   沒登入（`session.sealed` 不在）、或解不開
    pub(crate) fn session_of(&self, account: &AccountDir) -> Result<Session, CoreError> {
        self.vault()?
            .unseal_session(&account.session_path())
            .map_err(CoreError::from)?
            .ok_or_else(|| {
                CoreError::new(
                    CoreErrorKind::NotLoggedIn,
                    format!(
                        "{} is not logged in ({} missing); log in first",
                        account.label(),
                        account.session_path().display()
                    ),
                )
            })
    }

    /// 這個帳號的 matrix-sdk backend（store 在帳號目錄的 `m/`，金鑰是第二把子金鑰）。
    ///
    /// Args:
    ///     server_backup: conf 的 `SERVER_BACKUP`（CLI 規格 §10）, example: true
    ///
    /// ⚠️ 這個旗標由**呼叫端**帶進來，🚫 core 自己不讀 conf——那是「代前端做決定」（§3）。
    pub(crate) async fn backend_of(
        &self,
        account: &AccountDir,
        server_backup: bool,
    ) -> Result<MatrixBackend, CoreError> {
        let session = self.session_of(account)?;
        if session.store_dir.is_none() {
            return Err(CoreError::new(
                CoreErrorKind::NotLoggedIn,
                "this session has no matrix store (logged in with an older build or --token); log in again",
            ));
        }
        Ok(MatrixBackend::restore(
            &session,
            &account.matrix_store_dir(),
            &self.vault()?.matrix_store_key(),
            server_backup,
        )
        .await?)
    }

    /// 這個帳號所屬 server 的 `cache.db`（local-cache-db.md §6，同 server 的帳號共用）。
    ///
    /// server 不符、解不開就重建（§1），重建時發一個 `Progress` 事件說一聲——
    /// 🚫 不是 `eprintln!`：core 不印東西（`event` 模組的模組註解寫了為什麼）。
    pub(crate) fn cache_of(&self, account: &AccountDir, server: &str) -> Result<Cache, CoreError> {
        #[cfg(test)]
        {
            *RAW_CACHE_OPENS
                .lock()
                .expect("the raw-open counter is never poisoned")
                .entry(account.server_dir())
                .or_insert(0) += 1;
        }
        let identity = CacheIdentity {
            server: server.to_string(),
        };
        let (cache, outcome) =
            Cache::open(&account.server_dir(), &self.vault()?.cache_key(), &identity)?;
        match outcome {
            OpenOutcome::Reused => {}
            OpenOutcome::Created => self
                .events
                .progress(format!("created {}", cache.path().display())),
            OpenOutcome::Rebuilt => self.events.progress(format!(
                "rebuilt {} (it was for another server, or could not be opened)",
                cache.path().display()
            )),
        }
        Ok(cache)
    }

    /// 這個帳號所屬 server 的 `cache.db` 的**寫入者＋讀連線**（daemon-runtime §2）。
    ///
    /// ⭐ **一個 server dir 一份，開了就留著**：多個寫入者就沒有順序可言（水位會倒退），
    /// 而且每次重開都要付一次 SQLCipher 導金鑰。
    ///
    /// ⚠️ **註冊表的鎖握滿「查、開、放進去」整段**，🚫 不是查完就放掉 —— 放掉的話兩個
    /// 呼叫端會同時看到 miss、同時 `Cache::open` 同一個檔（重建那條路會刪檔重造），
    /// 而且兩個寫入者是真的都起來了，只是後到的那個馬上被丟掉。⭐ **「一個 server dir
    /// 只有一個寫入者」要靠結構成立，不能靠時序恰好沒撞上**（PR #32 審查 cirno🔴）。
    ///
    /// 📎 代價是開庫期間別的呼叫端（含 `cache_queue_len`）會等 —— 那正是要的：
    /// 它們等的就是同一份東西。🚫 這中間沒有 `await`，所以不會跨 await 持鎖。
    ///
    /// Args:
    ///     account: 哪個帳號（它決定 server dir）
    ///     server: 這個庫是哪個 server 的, example: "http://localhost:6167"
    /// Return:
    ///     Ok(Arc<ServerCache>)   共用的那一份
    ///     Err(...)               開不了（磁碟、金鑰）
    pub(crate) fn server_cache_of(
        &self,
        account: &AccountDir,
        server: &str,
    ) -> Result<std::sync::Arc<crate::server_cache::ServerCache>, CoreError> {
        self.open_server_cache(account, server, OpenWhen::Always)?
            .ok_or_else(|| {
                CoreError::new(
                    crate::error::CoreErrorKind::Io,
                    "the server cache was not opened although it was asked to open always",
                )
            })
    }

    /// 同 [`Core::server_cache_of`]，但 **`cache.db` 不在就不開**（🚫 不建一個空的）。
    ///
    /// ⭐ 「在不在」跟「開」在**同一把註冊表的鎖裡**判斷（PR #40 審查 rumia🟡）：
    /// 用在「只是要處理既有資料」的路徑（`destroy` 的忘掉鏈），🚫 不先 `exists()` 再另外呼叫開庫。
    /// ⚠️ 還剩的窗口：`log_out` 的 `close_server_cache` 與刪檔之間沒有同一把鎖（既有行為，不在這裡）。
    ///
    /// Return:
    ///     Ok(Some(Arc<ServerCache>))  已經開著、或檔案在而開起來了
    ///     Ok(None)                    沒開著、而且沒有 `cache.db`
    ///     Err(...)                    開不了（磁碟、金鑰）
    pub(crate) fn find_server_cache_if_present(
        &self,
        account: &AccountDir,
        server: &str,
    ) -> Result<Option<std::sync::Arc<crate::server_cache::ServerCache>>, CoreError> {
        self.open_server_cache(account, server, OpenWhen::FileExists)
    }

    fn open_server_cache(
        &self,
        account: &AccountDir,
        server: &str,
        when: OpenWhen,
    ) -> Result<Option<std::sync::Arc<crate::server_cache::ServerCache>>, CoreError> {
        let dir = account.server_dir();
        let mut registry = self
            .server_caches
            .lock()
            .expect("the server-cache registry is never poisoned");
        if let Some(existing) = registry.get(&dir) {
            return Ok(Some(existing.clone()));
        }
        if when == OpenWhen::FileExists && !dir.join(wbf_sdk::cache::CACHE_FILE_NAME).exists() {
            return Ok(None);
        }
        let identity = CacheIdentity {
            server: server.to_string(),
        };
        let (cache, outcome) = crate::server_cache::ServerCache::open(
            &dir,
            &self.vault()?.cache_key(),
            &identity,
            self.events.clone(),
        )?;
        match outcome {
            OpenOutcome::Reused => {}
            OpenOutcome::Created => self
                .events
                .progress(format!("created {}", dir.join("cache.db").display())),
            OpenOutcome::Rebuilt => self.events.progress(format!(
                "rebuilt {} (it was for another server, or could not be opened)",
                dir.join("cache.db").display()
            )),
        }
        let cache = std::sync::Arc::new(cache);
        registry.insert(dir, cache.clone());
        Ok(Some(cache))
    }

    /// 關掉這個 server dir 的 `cache.db` 寫入者與讀連線，並從註冊表拿掉。
    ///
    /// 🚨 **刪 `cache.db` 之前一定要先叫它**（`log_out_account` 的「最後一個帳號登出就整個丟」）。
    /// 註冊表的 `ServerCache` 開了就一直握著那個檔：
    ///
    /// | | 不先關就刪 |
    /// |---|---|
    /// | Windows | 刪不掉（`os error 32`），登出失敗 |
    /// | Linux | 🚨 **刪得掉**，但寫入者與讀連線還握著已刪的檔；下次同一台 server 登入，註冊表交出的是那個舊 handle，寫進去的東西**消失** |
    ///
    /// ⚠️ **鎖握滿整段**（查、拿掉、關）：放掉的話，關的途中別人 `server_cache_of` 會開出第二個寫入者（#32 的教訓）。
    /// 📎 關會等 queue 裡剩的寫完；那段時間別的 server 的 `server_cache_of` 也在等 —— 登出很少，可以接受。
    ///
    /// Args:
    ///     server_dir: `<data dir>/s/<加密的 server 名>`
    /// Return:
    ///     Ok(true)    本來開著、現在關了
    ///     Ok(false)   本來就沒開
    ///     Err(Io)     還有別的請求拿著它 —— 🚫 不從它們底下抽掉，登出重試一次就好（清理是冪等的）
    pub(crate) fn close_server_cache(
        &self,
        server_dir: &std::path::Path,
    ) -> Result<bool, CoreError> {
        let mut registry = self
            .server_caches
            .lock()
            .expect("the server-cache registry is never poisoned");
        let Some(shared) = registry.remove(server_dir) else {
            return Ok(false);
        };
        match std::sync::Arc::try_unwrap(shared) {
            Ok(cache) => {
                cache.close();
                Ok(true)
            }
            Err(still_shared) => {
                registry.insert(server_dir.to_path_buf(), still_shared);
                Err(CoreError::new(
                    CoreErrorKind::Io,
                    "cache.db is still in use by another request, so it cannot be closed and removed yet; try again",
                ))
            }
        }
    }

    /// 這個帳號所屬 server 的媒體儲存池（local-cache-db.md §8），跟 `cache.db` 同層。
    pub(crate) fn pool_of(&self, account: &AccountDir) -> Result<MediaPool, CoreError> {
        Ok(MediaPool::open(
            &account.server_dir(),
            self.vault()?.media_store_key(),
        )?)
    }
}

/// [`Core::open_server_cache`] 什麼時候才開。
#[derive(Clone, Copy, PartialEq, Eq)]
enum OpenWhen {
    /// 沒有就建（一般的讀寫路徑）。
    Always,
    /// `cache.db` 在才開。
    FileExists,
}

/// 每個 server dir 上 [`Core::cache_of`] 開過幾次**繞過唯一寫入者**的連線。
///
/// 🚨 daemon 裡一台 server 只准一個寫入者（`ServerCache`，#32）；會刪東西的路徑（`destroy_account`）
/// 🚫 不准走這條。按目錄計數，理由跟 `server_cache::WRITERS_STARTED` 一樣：並行的測試不會互相抬高。
/// 📎 只在測試編譯：正式 build 不付每次開庫加鎖的成本（PR #40 審查 cirno🟡2）。
#[cfg(test)]
static RAW_CACHE_OPENS: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<std::path::PathBuf, usize>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

/// Return:
///     usize  這個 server dir 上 `cache_of` 開過的次數；0 = 從來沒有
#[cfg(test)]
pub(crate) fn get_raw_cache_opens_for(server_dir: &std::path::Path) -> usize {
    RAW_CACHE_OPENS
        .lock()
        .expect("the raw-open counter is never poisoned")
        .get(server_dir)
        .copied()
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use wbf_sdk::vault::Vault;
    use wbf_sdk::Unlock;

    const SERVER: &str = "http://localhost:6167";

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("wbf-core-h-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn unlocked(dir: &std::path::Path) -> Core {
        Vault::create(dir, &Unlock::NoPassphrase).unwrap();
        let core = Core::open(dir);
        core.unlock(None).unwrap();
        core
    }

    fn is_registered(core: &Core, server_dir: &std::path::Path) -> bool {
        core.server_caches
            .lock()
            .expect("registry")
            .contains_key(server_dir)
    }

    /// 關掉之前 queue 裡剩的**要寫完**；關完註冊表裡**沒有它**；別人還拿著就**拒絕**。
    #[test]
    fn closing_a_server_cache_drains_the_queue_and_refuses_while_someone_still_holds_it() {
        let dir = scratch("close");
        let core = unlocked(&dir);
        let key = core.vault().unwrap().account_dir_key();
        let account = AccountDir::locate(&dir, &key, SERVER, "@alice:localhost").unwrap();
        std::fs::create_dir_all(&account.dir).unwrap();
        let server_dir = account.server_dir();

        let cache = core.server_cache_of(&account, SERVER).unwrap();
        // ⚠️ 這件工作**故意慢**，寫完才立旗標。`close` 一回來就看旗標：有等寫入執行緒 → 一定立了；
        // 沒等 → `close` 幾微秒就回來、工作還在睡。🚫 不靠「重新開庫讀讀看」——那要花的時間
        // 可能比睡的還久，舊執行緒照樣來得及寫完，測試就驗不到「close 會等」（變異驗證時抓到兩次）。
        let finished = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let finished_in_job = finished.clone();
        cache.post(
            move |cache| {
                std::thread::sleep(std::time::Duration::from_millis(200));
                let written = cache.set_cg_seq("@alice:localhost", 42);
                finished_in_job.store(true, std::sync::atomic::Ordering::SeqCst);
                written
            },
            Vec::new(),
        );

        // 🚫 別人還拿著（這裡的 `cache`）：不准從它底下抽掉。
        assert!(core.close_server_cache(&server_dir).is_err());
        assert!(is_registered(&core, &server_dir), "拒絕時要原樣放回去");

        drop(cache);
        assert!(core.close_server_cache(&server_dir).unwrap(), "本來開著");
        assert!(
            finished.load(std::sync::atomic::Ordering::SeqCst),
            "🚨 close 回來時，queue 裡排著的那件要已經寫完"
        );
        assert!(!is_registered(&core, &server_dir), "關完註冊表裡沒有它");
        assert!(
            !core.close_server_cache(&server_dir).unwrap(),
            "再關一次：本來就沒開"
        );

        // queue 裡那件在關之前寫完了：重新開一份讀得到。
        let reopened = core.server_cache_of(&account, SERVER).unwrap();
        let seq = reopened
            .run_blocking(|cache| cache.get_cg_seq("@alice:localhost"))
            .unwrap();
        assert_eq!(seq, Some(42), "關之前排著的寫入要落地");
        drop(reopened);
        core.close_server_cache(&server_dir).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 🚨 **這台 server 的最後一個帳號登出：要先關 cache.db 再刪**（房間歷史那支的真 server 測試抓到）。
    ///
    /// 不先關的話：Windows 刪不掉（`os error 32`，登出失敗）；Linux 刪得掉，但註冊表還握著已刪的檔，
    /// 下次同一台 server 登入交出的是舊 handle —— 寫進去的東西消失。所以斷言兩件事，**兩種 OS 都會紅**：
    /// 登出成功、而且註冊表裡沒有它。
    #[tokio::test]
    async fn logging_out_the_last_account_closes_the_cache_before_removing_it() {
        let dir = scratch("logout-closes-cache");
        let core = unlocked(&dir);
        let key = core.vault().unwrap().account_dir_key();
        let account = AccountDir::locate(&dir, &key, SERVER, "@alice:localhost").unwrap();
        std::fs::create_dir_all(&account.dir).unwrap();
        let server_dir = account.server_dir();
        // 有人讀過快取（daemon 裡任何一個 room.* 都會），然後不再拿著。
        drop(core.server_cache_of(&account, SERVER).unwrap());
        assert!(server_dir.join(wbf_sdk::cache::CACHE_FILE_NAME).exists());

        // 🚫 不封 session：`is_logged_in()` 是 false，登出不必連網路就走到本地清理。
        core.log_out("@alice:localhost", None, true, false)
            .await
            .expect("最後一個帳號登出要成功");

        assert!(
            !is_registered(&core, &server_dir),
            "🚨 註冊表不准留著已刪的檔"
        );
        assert!(
            !server_dir.join(wbf_sdk::cache::CACHE_FILE_NAME).exists(),
            "cache.db 要真的刪掉"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 🚨 八條執行緒同時要同一個 server 的快取 —— **只准起一條寫入者**
    /// （PR #32 審查 cirno🔴）。
    ///
    /// ⚠️ 這條測試在「查完就放掉鎖、開完再 `or_insert`」的舊寫法上是**紅的**——
    /// 實測過三次三次都紅（2026-09-13），而且壞得比預期重：
    ///
    /// - 每個看到 miss 的呼叫端都真的開了一次庫、起了一條寫入者；
    /// - 🚨 更糟的是**大部分呼叫端直接失敗**：`io: cache.db: database is locked`。
    ///   八條同時第一次開同一個檔，建表那段是排他的，`busy_timeout` 也救不了。
    ///   ⭐ 所以那不只是「多一條孤兒寫入者」，是**開不起來**。
    ///
    /// 🚫 光比 `Arc::ptr_eq` 抓不到多開這件事：輸家拿到的就是贏家那一份，位址本來就相等。
    #[test]
    fn eight_threads_asking_for_one_server_cache_start_exactly_one_writer() {
        let dir = scratch("one-writer");
        let core = unlocked(&dir);
        let key = core.vault().unwrap().account_dir_key();
        let account = AccountDir::locate(&dir, &key, SERVER, "@alice:localhost").unwrap();
        std::fs::create_dir_all(&account.dir).unwrap();
        let server_dir = account.server_dir();
        assert_eq!(
            crate::server_cache::get_writers_started_for(&server_dir),
            0,
            "還沒有人要過"
        );

        let caches: Vec<_> = std::thread::scope(|scope| {
            let racers: Vec<_> = (0..8)
                .map(|_| scope.spawn(|| core.server_cache_of(&account, SERVER).unwrap()))
                .collect();
            racers
                .into_iter()
                .map(|racer| racer.join().expect("沒有一條該 panic"))
                .collect()
        });

        assert_eq!(
            crate::server_cache::get_writers_started_for(&server_dir),
            1,
            "一個 server dir 只准有一個寫入者，多起的那些會讓順序失去意義"
        );
        for cache in &caches {
            assert!(
                std::sync::Arc::ptr_eq(&caches[0], cache),
                "八條拿到的要是同一份"
            );
        }
        // 而且那一份是活的：寫進去、再讀回來。
        caches[0]
            .run_blocking(|cache| cache.set_cg_seq("@alice:localhost", 7))
            .expect("寫入者要收得到工作");
        let seq = caches[0]
            .run_blocking(|cache| cache.get_cg_seq("@alice:localhost"))
            .expect("讀得回來");
        assert_eq!(seq, Some(7), "寫進去的要真的落地");

        let _ = std::fs::remove_dir_all(&dir);
    }
}

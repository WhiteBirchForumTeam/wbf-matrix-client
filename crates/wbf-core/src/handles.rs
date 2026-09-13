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
        let dir = account.server_dir();
        let mut registry = self
            .server_caches
            .lock()
            .expect("the server-cache registry is never poisoned");
        if let Some(existing) = registry.get(&dir) {
            return Ok(existing.clone());
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
        Ok(cache)
    }

    /// 這個帳號所屬 server 的媒體儲存池（local-cache-db.md §8），跟 `cache.db` 同層。
    pub(crate) fn pool_of(&self, account: &AccountDir) -> Result<MediaPool, CoreError> {
        Ok(MediaPool::open(
            &account.server_dir(),
            self.vault()?.media_store_key(),
        )?)
    }
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

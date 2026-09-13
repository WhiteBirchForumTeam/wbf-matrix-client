//! 一個 server 的 `cache.db`：**唯一的寫入者**，加一條給讀的連線（daemon-runtime §2）。
//!
//! ## 為什麼要這一層
//!
//! `cache.db` 是**一個 server 一份、那台機器上這個 server 的所有帳號共用**（local-cache-db §6）。
//! daemon 常駐之後，兩個帳號的上游會話會**同時**往裡面寫，而 SQLite 的寫是排他的。
//!
//! ⚠️ **不是因為會馬上失敗**（2026-09-13 實測釘正）：`rusqlite` 開連線時就設了
//! `busy_timeout = 5000`，所以同時寫的結果是**等**，不是 `SQLITE_BUSY`
//! （`two_raw_connections_serialise_instead_of_failing` 量過）。真正的三個理由是：
//!
//! 1. 🚨 **等的時候是同步阻塞**：`upsert_*` 是 blocking 呼叫，卡在 async task 裡就是
//!    **卡住一條 tokio 工作執行緒**，最壞 5 秒。所以寫入者跑在**自己的 OS 執行緒**上，
//!    🚫 不在 runtime 的工作執行緒上。
//! 2. 🚨 **順序**：兩批事件誰先 commit 決定水位（`cg_seq`）落在哪。搶鎖的順序 ≠ 收到的順序，
//!    水位可能**倒退**。一條 queue 從根本解決，🚫 不必在每個寫入點做 `max()` 防禦。
//! 3. **成本**：每次操作都 `Cache::open` 要付一次 SQLCipher 導金鑰（PBKDF2）。連線留著重用。
//!
//! 📎 而 5 秒真的用完時仍然會失敗 —— 單一寫入者連那個尾巴也一起消掉了。
//!
//! ## 形狀
//!
//! ```text
//! 帳號 A 的上游會話 ──┐
//! 帳號 B 的上游會話 ──┼─post/run─> [ 無上限 queue ] ─> 寫入 task（唯一的寫連線）─> cache.db
//! RPC 觸發的寫（已讀…）┘                                         │
//!                                                    commit 成功之後才發事件
//! 讀（房間列表、歷史…）──────────────────> 另一條連線（WAL：一寫多讀）
//! ```
//!
//! - **queue 沒有上限**（維護者 2026-09-13）：丟掉一則已經收到的事件比塞住更糟。
//!   ⚠️ 代價是寫得比收得慢時會累積在記憶體裡，所以 [`ServerCache::queued`] 要能被看見
//!   （`daemon.info`）。
//! - **兩個入口**：[`ServerCache::post`]（丟進去就走）與 [`ServerCache::run`]（等它 commit）。
//!   ⭐ 規則一句話：**回應的內容取決於這次寫入的，就 `run`**；其餘 `post`。
//! - **事件由寫入者發**：`post` 帶著「commit 成功之後要發什麼」。🚫 呼叫端自己在 `post` 之後發
//!   會跑在 commit 前面 —— 前端收到推播去查，查到的是還沒有那筆的資料庫。

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use tokio::sync::{mpsc, oneshot, Mutex, MutexGuard};
use wbf_sdk::cache::{Cache, CacheIdentity, OpenOutcome};
use wbf_sdk::{Key32, SdkError};

use crate::event::EventSink;
use crate::{CoreError, CoreEvent};

/// 寫入 task 收到的一件事。事件與回執都包在閉包裡，所以這個型別不必有泛型。
type Job = Box<dyn FnOnce(&mut Cache, &EventSink) + Send>;

/// 一個 server 的快取：一個寫入者、一條讀連線。
pub(crate) struct ServerCache {
    to_writer: mpsc::UnboundedSender<Job>,
    /// 排隊中（還沒被寫入 task 拿走）的件數。
    queued: Arc<AtomicUsize>,
    /// 讀用的第二條連線。⚠️ WAL 允許一寫多讀，但**開一條連線要付 SQLCipher 導金鑰的成本**，
    /// 所以留著重用；🚫 不要每個請求開一次。
    /// 📎 用 `Mutex` 是因為 `Connection` 不是 `Sync`：讀因此是排隊的，
    /// 而我們的讀都是短查詢。哪天出現慢查詢再換成連線池。
    reader: Mutex<Cache>,
}

impl ServerCache {
    /// 開這個 server 的快取（寫一條、讀一條），並起動寫入 task。
    ///
    /// Args:
    ///     server_dir: `<data dir>/s/<加密的 server 名>`
    ///     key: vault 的第三把子金鑰
    ///     identity: 這個庫是哪個 server 的（對不上就重建）
    ///     events: commit 之後發事件用
    /// Return:
    ///     Ok(ServerCache)
    ///     Err(...)   開不了（磁碟、金鑰、目錄）
    ///
    /// 📎 🚫 **不需要 tokio runtime**：寫入者是一條自己的 OS 執行緒（見模組註解第 1 點），
    /// 所以 CLI 那種「一個命令一個程序」的用法也照樣能開。
    pub(crate) fn open(
        server_dir: &Path,
        key: &Key32,
        identity: &CacheIdentity,
        events: EventSink,
    ) -> Result<(ServerCache, OpenOutcome), CoreError> {
        // 寫連線先開：它可能要建檔或重建，🚫 不要讓兩條連線同時做那件事。
        let (writing, outcome) = Cache::open(server_dir, key, identity)?;
        let (reading, _) = Cache::open(server_dir, key, identity)?;

        let (to_writer, mut inbox) = mpsc::unbounded_channel::<Job>();
        let queued = Arc::new(AtomicUsize::new(0));
        let counter = queued.clone();
        // 🚨 自己的 OS 執行緒，🚫 不是 `tokio::spawn`：SQLite 的寫是**同步阻塞**的
        // （拿不到鎖會等，最壞 5 秒），擺在 runtime 的工作執行緒上就是卡住別人的 future。
        std::thread::Builder::new()
            .name("wbf-cache-writer".to_string())
            .spawn(move || {
                let mut writing = writing;
                // 送出端全部丟掉了（daemon 收攤）就跳出，執行緒跟著結束。
                while let Some(job) = inbox.blocking_recv() {
                    counter.fetch_sub(1, Ordering::SeqCst);
                    job(&mut writing, &events);
                }
            })
            .map_err(|error| {
                CoreError::new(
                    crate::CoreErrorKind::Io,
                    format!("cannot start the cache writer thread: {error}"),
                )
            })?;
        Ok((
            ServerCache {
                to_writer,
                queued,
                reader: Mutex::new(reading),
            },
            outcome,
        ))
    }

    /// 還有幾件在排隊。⚠️ 一直漲就是寫得比收得慢（daemon-runtime §2.2）。
    pub(crate) fn queued(&self) -> usize {
        self.queued.load(Ordering::SeqCst)
    }

    /// 讀。⚠️ 拿著這個 guard 的期間別的讀要等，所以🚫 不要抓著它做網路。
    pub(crate) async fn read(&self) -> MutexGuard<'_, Cache> {
        self.reader.lock().await
    }

    /// **丟進去就走**：不等它寫完。`emit_after_commit` 在 commit 成功之後才發。
    ///
    /// Args:
    ///     work: 對資料庫做的事，example: |cache| cache.upsert_messages(&me, &events).map(|_| ())
    ///     emit_after_commit: 成功才發的事件；失敗一則都不發
    ///
    /// ⚠️ 寫失敗**只發一則 `Note`，不擋任何人**（跟舊的 `write_through` 同一條政策：
    /// 快取壞了的代價是重拉，不是命令失敗）。
    pub(crate) fn post<F>(&self, work: F, emit_after_commit: Vec<CoreEvent>)
    where
        F: FnOnce(&mut Cache) -> Result<(), SdkError> + Send + 'static,
    {
        self.send(Box::new(move |cache, events| match work(cache) {
            Ok(()) => {
                for event in emit_after_commit {
                    events.emit(event);
                }
            }
            Err(error) => events.progress(format!("cache write failed (ignored): {error}")),
        }));
    }

    /// **等它 commit**，拿回結果。要拿這次寫入的數字去組回應的就走這條。
    ///
    /// Return:
    ///     Ok(T)      commit 成功了（WAL：程序當掉也不會丟）
    ///     Err(...)   寫失敗，或寫入 task 已經收攤
    pub(crate) async fn run<F, T>(&self, work: F) -> Result<T, CoreError>
    where
        F: FnOnce(&mut Cache) -> Result<T, SdkError> + Send + 'static,
        T: Send + 'static,
    {
        let (done, wait) = oneshot::channel();
        self.send(Box::new(move |cache, _events| {
            // 對方不等了（請求被取消）就沒人收——那不是錯誤，寫入照樣做完了。
            let _ = done.send(work(cache));
        }));
        match wait.await {
            Ok(result) => Ok(result?),
            Err(_) => Err(CoreError::new(
                crate::CoreErrorKind::Io,
                "the cache writer stopped before this write was applied",
            )),
        }
    }

    /// 同步脈絡裡的 `run`：**擋住當前執行緒**直到 commit。
    ///
    /// ⚠️ 只給「回呼是同步的」那些地方用（`recent` 的每批回呼就是），🚫 async 裡請用 [`Self::run`]。
    /// 📎 用 `std` 的 channel 而不是 `oneshot::blocking_recv`：後者在 tokio runtime 裡會 panic。
    /// ⭐ 這跟改成寫入者之前的行為一樣（本來就是在那個 task 上同步寫），
    /// 🚫 沒有變得更慢，只是換成排在同一條 queue 上。
    pub(crate) fn run_blocking<F, T>(&self, work: F) -> Result<T, CoreError>
    where
        F: FnOnce(&mut Cache) -> Result<T, SdkError> + Send + 'static,
        T: Send + 'static,
    {
        let (done, wait) = std::sync::mpsc::channel();
        self.send(Box::new(move |cache, _events| {
            let _ = done.send(work(cache));
        }));
        match wait.recv() {
            Ok(result) => Ok(result?),
            Err(_) => Err(CoreError::new(
                crate::CoreErrorKind::Io,
                "the cache writer stopped before this write was applied",
            )),
        }
    }

    fn send(&self, job: Job) {
        self.queued.fetch_add(1, Ordering::SeqCst);
        if self.to_writer.send(job).is_err() {
            // 寫入 task 沒了（收攤中）。計數要退回去，🚫 不然 `queued` 會永遠掛在那個數字。
            self.queued.fetch_sub(1, Ordering::SeqCst);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wbf_sdk::chat::{Message, MessageKind};

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("wbf-sc-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn open(dir: &Path, events: &EventSink) -> ServerCache {
        ServerCache::open(
            dir,
            &Key32([9u8; 32]),
            &CacheIdentity {
                server: "http://localhost:6167".to_string(),
            },
            events.clone(),
        )
        .unwrap()
        .0
    }

    fn message(room: &str, event_id: &str, r_seq: i64) -> Message {
        Message {
            id: event_id.to_string(),
            conversation: room.to_string(),
            sender: "@a:localhost".to_string(),
            sent_at: 1,
            kind: MessageKind::Text {
                body: "hi".to_string(),
                formatted_html: None,
            },
            reply_to: None,
            edited_by: None,
            reactions: Vec::new(),
            decrypted: None,
            undecryptable_reason: None,
            r_seq: Some(r_seq),
            g_seq: Some(r_seq),
        }
    }

    /// `run` 回來就代表**真的落地了**：另一條路徑立刻讀得到（daemon-runtime §2.5）。
    #[tokio::test]
    async fn run_returns_only_after_the_write_landed() {
        let dir = scratch("run");
        let events = EventSink::new();
        let cache = open(&dir, &events);

        let written = cache
            .run(move |cache| cache.upsert_messages("@a:localhost", &[message("!r", "$1", 1)]))
            .await
            .unwrap();
        assert_eq!(written, 1);

        // 讀連線是另一條連線：看得到就是真的 commit 了。
        let page = cache
            .read()
            .await
            .history("@a:localhost", "!r", None, 10)
            .unwrap();
        assert_eq!(page.len(), 1, "run 回來之後另一條連線就讀得到");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `post` 是丟進去就走，但**順序照丟的順序**，而且事件在 commit 之後才發。
    #[tokio::test]
    async fn post_keeps_the_order_and_emits_only_after_the_commit() {
        let dir = scratch("post");
        let events = EventSink::new();
        let cache = open(&dir, &events);
        let mut heard = events.subscribe();

        for index in 1..=20i64 {
            let note = CoreEvent::Note {
                job: None,
                text: format!("wrote {index}"),
            };
            cache.post(
                move |cache| {
                    cache
                        .upsert_messages(
                            "@a:localhost",
                            &[message("!r", &format!("${index}"), index)],
                        )
                        .map(|_| ())
                },
                vec![note],
            );
        }
        // 最後補一件 `run`：它回來的時候，前面 20 件一定都寫完了（同一條 queue、照順序）。
        cache.run(|_cache| Ok(())).await.unwrap();

        let page = cache
            .read()
            .await
            .history("@a:localhost", "!r", None, 100)
            .unwrap();
        assert_eq!(page.len(), 20, "20 件都寫進去了");

        // 事件的順序跟丟的順序一樣。
        for index in 1..=20 {
            match heard.try_recv().unwrap() {
                CoreEvent::Note { text, .. } => assert_eq!(text, format!("wrote {index}")),
                other => panic!("expected a Note, got {other:?}"),
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 🚨 這條就是 §2.5 那個「兩個帳號一起寫」的驗證：**同一個房間、同一批事件、兩個帳號同時灌**。
    ///
    /// ⭐ 事件本體只有一份（`events` 表），「誰看得到」是各自一份（`events_synced_log`）——
    /// 所以兩個帳號寫的是**同一批列**，正是最會撞的情況。
    /// 🚫 不能出現 `database is locked`，而且兩邊都要讀得到全部。
    #[tokio::test]
    async fn two_accounts_in_one_room_write_the_same_events_at_once_without_locking() {
        let dir = scratch("race");
        let events = EventSink::new();
        let cache = Arc::new(open(&dir, &events));
        let mut complaints = events.subscribe();

        let accounts = ["@a:localhost", "@b:localhost"];
        let mut writers = Vec::new();
        for account in accounts {
            let cache = cache.clone();
            writers.push(tokio::spawn(async move {
                for index in 1..=100i64 {
                    // ⚠️ 兩個帳號看到的是**同一則**事件（同一個 event_id、同一個 r_seq）——
                    // 這是真實情況，也是唯一索引 `(room, r_seq)` 會被兩邊同時碰的原因。
                    cache
                        .run(move |cache| {
                            cache.upsert_messages(
                                account,
                                &[message("!r", &format!("${index}"), index)],
                            )
                        })
                        .await
                        .expect("寫入不該失敗");
                }
            }));
        }
        for writer in writers {
            writer.await.unwrap();
        }

        // 🚫 一則「cache write failed」都不該有。
        while let Ok(event) = complaints.try_recv() {
            if let CoreEvent::Note { text, .. } = event {
                assert!(!text.contains("failed"), "{text}");
            }
        }
        // 混存但不混視野：兩個帳號各自都讀得到那 100 則。
        for account in accounts {
            let page = cache
                .read()
                .await
                .history(account, "!r", None, 500)
                .unwrap();
            assert_eq!(page.len(), 100, "{account} 應該看得到全部");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 🚨 **釘住那個假設**：兩條各自的連線同時寫，會**排隊**而不是失敗 ——
    /// 因為 `rusqlite` 開連線時就設了 `busy_timeout = 5000`。
    ///
    /// ⭐ 這條的價值在於它推翻了設計文件的第一版（我原本寫「沒有 busy_timeout，會立刻
    /// `SQLITE_BUSY`」）。哪天 rusqlite 改掉那個預設，這裡會紅，而那正是要停下來重想的時候。
    /// ⚠️ 所以「單一寫入者」的理由**不是**「不做會失敗」，是模組註解裡那三條
    /// （卡住 tokio 執行緒、順序、開檔成本）。
    #[test]
    fn two_raw_connections_serialise_instead_of_failing() {
        let dir = scratch("raw");
        let identity = CacheIdentity {
            server: "http://localhost:6167".to_string(),
        };
        let key = Key32([9u8; 32]);
        let (first, _) = Cache::open(&dir, &key, &identity).unwrap();
        let (second, _) = Cache::open(&dir, &key, &identity).unwrap();

        let locked = std::sync::Arc::new(AtomicUsize::new(0));
        let mut threads = Vec::new();
        for (index, mut cache) in [first, second].into_iter().enumerate() {
            let locked = locked.clone();
            threads.push(std::thread::spawn(move || {
                for round in 1..=200i64 {
                    let event_id = format!("${index}-{round}");
                    let seq = round + (index as i64) * 100_000;
                    if let Err(error) =
                        cache.upsert_messages("@a:localhost", &[message("!r", &event_id, seq)])
                    {
                        assert!(
                            error.to_string().contains("locked"),
                            "只預期鎖的錯，卻是 {error}"
                        );
                        locked.fetch_add(1, Ordering::SeqCst);
                    }
                }
            }));
        }
        for thread in threads {
            thread.join().unwrap();
        }
        assert_eq!(
            locked.load(Ordering::SeqCst),
            0,
            "rusqlite 的 busy_timeout 應該讓它們排隊；這裡不是 0 代表那個預設變了"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 排隊中的件數看得見（daemon.info 要用）。
    #[tokio::test]
    async fn the_queue_depth_is_visible_and_drains_back_to_zero() {
        let dir = scratch("queued");
        let events = EventSink::new();
        let cache = open(&dir, &events);
        for index in 1..=50i64 {
            cache.post(
                move |cache| {
                    cache
                        .upsert_messages(
                            "@a:localhost",
                            &[message("!r", &format!("${index}"), index)],
                        )
                        .map(|_| ())
                },
                Vec::new(),
            );
        }
        cache.run(|_cache| Ok(())).await.unwrap();
        assert_eq!(cache.queued(), 0, "跑完就歸零");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

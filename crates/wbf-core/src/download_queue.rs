//! 每帳號一個下載 worker（/docs/design/media/media-download.md §5、§6）：一條佇列、一次一個檔、一塊一步；seek 插隊同一條 `Download` 線。
//!
//! - **一塊一步**：worker 每次只做一塊（拉 → 落地 → 觸發下一步），做完回頭看收件匣，所以 seek 最多等一塊的傳輸時間（§5.1、§6.2）。
//! - **兩個收件匣**：seek 請求（GET 要的塊）先，佇列頭那個 job 的下一步後（§5.1）。
//! - **`downloading` 表**放取消旗標與進度（§5.2）：job 第一次開始時放進去、最後一包落地或看到旗標時由 worker 自己拿掉；`media.queue`／推播直接讀它。
//! - **線**：每塊借一次（`LinkPool::reuse`），🚫 不整個檔握著。線沒開或斷了就停一下再試，進度停在原地（§5.4「網路斷」）；開線是 core 的入口與 `link_keeper` 的事。
//! - **同一台 server 的同一個 mxc 只有一個寫入者**：主檔與 seek 暫存檔以 `m<media.id>` 命名、同 server 的帳號共用，所以開檔前先在 [`MediaClaims`] 認領；
//!   別的帳號正在寫它，這個 job 就等（它寫完，DB 會說完成），seek 交給認領的那個 worker（`Core::downloader_for_seek`）。
//!
//! 檔怎麼寫、塊從哪拿是 sdk 的 `MediaDownload`；這裡只管「誰、何時、做到哪」。DB 一律經 `ServerCache`（唯一寫入者）。

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use tokio::sync::{mpsc, oneshot, Notify};
use wbf_sdk::cache::MediaEntry;
use wbf_sdk::media::{self, MediaDownload, PROGRESS_FLUSH};
use wbf_sdk::media_pool::MediaPool;
use wbf_sdk::{Manifest, SdkError};
use zeroize::Zeroizing;

use crate::error::{CoreError, CoreErrorKind};
use crate::event::{DownloadState, EventSink};
use crate::link_pool::{LinkPool, LinkRole};
use crate::server_cache::ServerCache;
use crate::CoreEvent;

/// 推播進度的間隔：每個 job 最多每秒一則（/docs/design/media/media-download.md §5.5）。
const PUSH_EVERY: Duration = Duration::from_secs(1);
/// 線沒開、斷了、或別的帳號正在寫同一個檔：等這麼久再試。
const RETRY_AFTER: Duration = Duration::from_secs(1);
/// 沒事做時最久睡多久（有 seek、排進新 job、取消都會叫醒它）。
const IDLE_WAKE: Duration = Duration::from_secs(60);
/// 只為 seek 開著、不是 job 的檔最多留幾個（每個握著兩個檔案把手）。
const SEEK_ONLY_OPEN: usize = 4;

/// 一個正在下載的檔（/docs/design/media/media-download.md §5.2）：所有塊的請求共用這一份。
#[derive(Default)]
pub(crate) struct Downloading {
    cancelled: AtomicBool,
    done: AtomicU32,
    total: AtomicU32,
}

/// 一個 job 現在的樣子（`media.download`、`media.queue` 的回答）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct JobStatus {
    pub state: DownloadState,
    /// 已落地的塊數
    pub done: u32,
    /// 總塊數；還不知道是 0
    pub total: u32,
}

/// 佇列裡的一項。
struct QueuedJob {
    mxc: String,
    manifest: Arc<Manifest>,
}

/// 等某個 job 結束的人（`media.save_to`）。
type Waiter = oneshot::Sender<Result<(), CoreError>>;

#[derive(Default)]
struct QueueState {
    /// 第一個是正在拉的（如果它在 `downloading` 裡），其餘排著。
    queue: VecDeque<QueuedJob>,
    downloading: HashMap<String, Arc<Downloading>>,
    waiters: HashMap<String, Vec<Waiter>>,
    /// worker 現在開著的主檔暫存名（掃描不准碰，`media::sweep` 的 `in_use`）。
    open_names: HashSet<String>,
}

struct Shared {
    user: String,
    state: Mutex<QueueState>,
    wake: Notify,
}

impl Shared {
    fn state(&self) -> MutexGuard<'_, QueueState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// GET 要的一塊（/docs/design/media/media-download.md §6）。
struct SeekRequest {
    manifest: Arc<Manifest>,
    index: u32,
    reply: oneshot::Sender<Result<Zeroizing<Vec<u8>>, CoreError>>,
}

/// 同一台 server（server dir）的同一個 mxc 現在由哪個帳號的 worker 在寫。
#[derive(Default)]
pub(crate) struct MediaClaims {
    held: Mutex<HashMap<(PathBuf, String), String>>,
}

impl MediaClaims {
    fn held(&self) -> MutexGuard<'_, HashMap<(PathBuf, String), String>> {
        self.held
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Return:
    ///     bool  true ＝ 認領到了（本來沒人、或本來就是自己）；false ＝ 別的帳號正在寫
    fn claim(&self, server_dir: &Path, mxc: &str, user: &str) -> bool {
        let mut held = self.held();
        let holder = held
            .entry((server_dir.to_path_buf(), mxc.to_string()))
            .or_insert_with(|| user.to_string());
        holder == user
    }

    fn release(&self, server_dir: &Path, mxc: &str, user: &str) {
        let mut held = self.held();
        let key = (server_dir.to_path_buf(), mxc.to_string());
        if held.get(&key).is_some_and(|holder| holder == user) {
            held.remove(&key);
        }
    }

    /// Return:
    ///     Some(user)  正在寫它的帳號
    ///     None        沒人在寫
    pub(crate) fn find_holder(&self, server_dir: &Path, mxc: &str) -> Option<String> {
        self.held()
            .get(&(server_dir.to_path_buf(), mxc.to_string()))
            .cloned()
    }
}

/// 一個帳號的下載 worker。丟掉就 abort（登出、換 session：`Core::close_links`）。
pub(crate) struct Downloader {
    shared: Arc<Shared>,
    seeks: mpsc::UnboundedSender<SeekRequest>,
    task: tokio::task::JoinHandle<()>,
    events: EventSink,
}

impl Drop for Downloader {
    /// abort 之後 task 的 future 被丟掉，`Worker` 跟著 drop：它的 `Drop` 放掉認領（🚫 不靠 abort 剛好停在哪一行）。
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// worker 要的東西；🚫 不握 `Core`、不握線（線在池裡，每塊借一次）。
pub(crate) struct WorkerParts {
    pub user: String,
    pub server_dir: PathBuf,
    pub links: Arc<LinkPool>,
    pub media_pool: MediaPool,
    pub cache: Arc<ServerCache>,
    pub events: EventSink,
    pub claims: Arc<MediaClaims>,
}

impl Downloader {
    /// 起 worker。⚠️ 要在 tokio runtime 裡叫。
    pub(crate) fn start(parts: WorkerParts) -> Downloader {
        let shared = Arc::new(Shared {
            user: parts.user.clone(),
            state: Mutex::new(QueueState::default()),
            wake: Notify::new(),
        });
        let (seeks, inbox) = mpsc::unbounded_channel();
        let events = parts.events.clone();
        let worker = Worker {
            shared: shared.clone(),
            seeks: inbox,
            user: parts.user,
            server_dir: parts.server_dir,
            links: parts.links,
            media_pool: parts.media_pool,
            cache: parts.cache,
            events: parts.events,
            claims: parts.claims,
            open: HashMap::new(),
            timers: HashMap::new(),
            stalled_until: None,
        };
        Downloader {
            shared,
            seeks,
            task: tokio::spawn(worker.run()),
            events,
        }
    }

    /// 排一個 job（/docs/design/media/media-download.md §5.3）：已經在佇列或 `downloading` 表裡就不重複排。
    ///
    /// Args:
    ///     manifest: 要整檔的那個（含檔案金鑰）
    ///     waiter: 要等它結束的話給一個（`media.save_to`）；結束時收到 Ok 或那個錯
    /// Return:
    ///     JobStatus   排進去之後（或本來）的樣子
    pub(crate) fn enqueue(&self, manifest: Arc<Manifest>, waiter: Option<Waiter>) -> JobStatus {
        let mxc = manifest.mxc.clone();
        let (status, newly_queued) = {
            let mut state = self.shared.state();
            if let Some(waiter) = waiter {
                state.waiters.entry(mxc.clone()).or_default().push(waiter);
            }
            match status_in(&state, &mxc) {
                Some(status) => (status, false),
                None => {
                    state.queue.push_back(QueuedJob {
                        mxc: mxc.clone(),
                        manifest,
                    });
                    (
                        JobStatus {
                            state: DownloadState::Queued,
                            done: 0,
                            total: 0,
                        },
                        true,
                    )
                }
            }
        };
        if newly_queued {
            self.events.emit(CoreEvent::MediaDownload {
                user: self.shared.user.clone(),
                mxc,
                state: DownloadState::Queued,
                done: 0,
                total: 0,
                reason: None,
            });
        }
        self.shared.wake.notify_one();
        status
    }

    /// 佇列現在的樣子，第一個是正在拉的（`media.queue`）。
    ///
    /// Return:
    ///     Vec<(mxc, 區塊的檔名, JobStatus)>
    pub(crate) fn list_jobs(&self) -> Vec<(String, Option<String>, JobStatus)> {
        let state = self.shared.state();
        state
            .queue
            .iter()
            .filter_map(|job| {
                status_in(&state, &job.mxc)
                    .map(|status| (job.mxc.clone(), job.manifest.block.name.clone(), status))
            })
            .collect()
    }

    /// `media.cancel`（/docs/design/media/media-download.md §5.4 的表）：正在拉 → 設旗標，worker 處理完手上那一包就停；排著 → 從佇列拿掉。
    ///
    /// Return:
    ///     bool  true ＝ 在表裡或佇列裡；false ＝ 都不在
    pub(crate) fn cancel(&self, mxc: &str) -> bool {
        let removed_waiting = {
            let mut state = self.shared.state();
            if let Some(downloading) = state.downloading.get(mxc) {
                downloading.cancelled.store(true, Ordering::SeqCst);
                drop(state);
                self.shared.wake.notify_one();
                return true;
            }
            let before = state.queue.len();
            state.queue.retain(|job| job.mxc != mxc);
            if state.queue.len() == before {
                return false;
            }
            state.waiters.remove(mxc).unwrap_or_default()
        };
        for waiter in removed_waiting {
            let _ = waiter.send(Err(cancelled_error(mxc)));
        }
        self.events.emit(CoreEvent::MediaDownload {
            user: self.shared.user.clone(),
            mxc: mxc.to_string(),
            state: DownloadState::Cancelled,
            done: 0,
            total: 0,
            reason: None,
        });
        true
    }

    /// GET 要第 `index` 塊（seek，/docs/design/media/media-download.md §6）：丟進 seek 收件匣，worker 做完手上那一塊就處理。
    ///
    /// Return:
    ///     Ok(明文)
    ///     Err(Network)     線沒開或斷了
    ///     Err(Integrity)   這一塊驗不過
    ///     Err(Io)          worker 已經收了
    pub(crate) async fn read_chunk(
        &self,
        manifest: Arc<Manifest>,
        index: u32,
    ) -> Result<Zeroizing<Vec<u8>>, CoreError> {
        let (reply, answer) = oneshot::channel();
        self.seeks
            .send(SeekRequest {
                manifest,
                index,
                reply,
            })
            .map_err(|_| worker_gone())?;
        self.shared.wake.notify_one();
        answer.await.map_err(|_| worker_gone())?
    }

    /// worker 現在開著的主檔暫存名（掃描用）。
    pub(crate) fn list_open_names(&self) -> HashSet<String> {
        self.shared.state().open_names.clone()
    }
}

fn status_in(state: &QueueState, mxc: &str) -> Option<JobStatus> {
    if let Some(downloading) = state.downloading.get(mxc) {
        return Some(JobStatus {
            state: DownloadState::Downloading,
            done: downloading.done.load(Ordering::SeqCst),
            total: downloading.total.load(Ordering::SeqCst),
        });
    }
    state
        .queue
        .iter()
        .any(|job| job.mxc == mxc)
        .then_some(JobStatus {
            state: DownloadState::Queued,
            done: 0,
            total: 0,
        })
}

fn worker_gone() -> CoreError {
    CoreError::new(
        CoreErrorKind::Io,
        "the download worker of this account has stopped (logged out?)",
    )
}

pub(crate) fn cancelled_error(mxc: &str) -> CoreError {
    CoreError::new(
        CoreErrorKind::Usage,
        format!("the download of {mxc} was cancelled"),
    )
}

/// job 怎麼結束的。
enum JobEnd {
    Complete,
    Cancelled,
    Failed(CoreError),
}

/// 這一輪 worker 做了什麼。
#[derive(PartialEq, Eq)]
enum Stepped {
    /// 做了一塊（或結束了一個 job）：馬上回頭看收件匣。
    Worked,
    /// 佇列是空的。
    Idle,
    /// 卡住了（線沒開、別人在寫同一個檔）：等一下再試。
    Stalled,
}

/// 正在拉的 job 的計時器。
struct JobTimers {
    last_flush: Instant,
    last_push: Instant,
}

/// worker 開著的一個檔。
struct OpenMedia {
    /// 主檔的暫存名（`m<media.id>`），掃描不准碰
    pending_name: String,
    download: MediaDownload,
}

struct Worker {
    shared: Arc<Shared>,
    seeks: mpsc::UnboundedReceiver<SeekRequest>,
    user: String,
    server_dir: PathBuf,
    links: Arc<LinkPool>,
    media_pool: MediaPool,
    cache: Arc<ServerCache>,
    events: EventSink,
    claims: Arc<MediaClaims>,
    /// 開著的檔（job 的，與只為 seek 開的）。worker 是它們唯一的寫入者。
    open: HashMap<String, OpenMedia>,
    timers: HashMap<String, JobTimers>,
    stalled_until: Option<Instant>,
}

impl Worker {
    async fn run(mut self) {
        loop {
            // seek 先（§5.1）。
            match self.seeks.try_recv() {
                Ok(request) => {
                    self.serve_seek(request).await;
                    continue;
                }
                Err(mpsc::error::TryRecvError::Disconnected) => break,
                Err(mpsc::error::TryRecvError::Empty) => {}
            }
            let stalled = self
                .stalled_until
                .is_some_and(|until| Instant::now() < until);
            if !stalled {
                self.stalled_until = None;
                match self.step().await {
                    Stepped::Worked => continue,
                    Stepped::Stalled => self.stalled_until = Some(Instant::now() + RETRY_AFTER),
                    Stepped::Idle => {}
                }
            }
            let wait = self
                .stalled_until
                .map(|until| until.saturating_duration_since(Instant::now()))
                .unwrap_or(IDLE_WAKE);
            tokio::select! {
                request = self.seeks.recv() => match request {
                    Some(request) => self.serve_seek(request).await,
                    None => break,
                },
                _ = self.shared.wake.notified() => {}
                _ = tokio::time::sleep(wait) => {}
            }
        }
    }

    /// 佇列頭那個 job 的下一步（/docs/design/media/media-download.md §5.4）。
    async fn step(&mut self) -> Stepped {
        let Some((mxc, manifest)) = self
            .shared
            .state()
            .queue
            .front()
            .map(|job| (job.mxc.clone(), job.manifest.clone()))
        else {
            return Stepped::Idle;
        };
        let downloading = self.start_job(&mxc);
        if downloading.cancelled.load(Ordering::SeqCst) {
            self.end_job(&mxc, JobEnd::Cancelled);
            return Stepped::Worked;
        }
        if !self.open.contains_key(&mxc) {
            // 已經有完整的（別的帳號寫完了、或之前就有）：不下載。
            if self.find_complete_entry(&mxc).await.is_some() {
                self.end_job(&mxc, JobEnd::Complete);
                return Stepped::Worked;
            }
            match self.open_media(&manifest).await {
                Ok(true) => {}
                Ok(false) => return Stepped::Stalled,
                Err(error) => {
                    self.end_job(&mxc, JobEnd::Failed(error));
                    return Stepped::Worked;
                }
            }
        }
        let landed = match self.open.get_mut(&mxc) {
            Some(opened) => {
                downloading
                    .total
                    .store(opened.download.chunk_count(), Ordering::SeqCst);
                match opened.download.is_written() {
                    true => Ok(()),
                    false => advance(&mut opened.download, &self.links).await,
                }
            }
            None => return Stepped::Stalled,
        };
        match landed {
            Ok(()) => {}
            Err(Advance::LinkDown) => return Stepped::Stalled,
            Err(Advance::Failed(SdkError::Network(_) | SdkError::Timeout(_))) => {
                return Stepped::Stalled
            }
            Err(Advance::Failed(error)) => {
                // 壞檔（§3.2、§3.3）或 server 拒絕：主檔與暫存檔都刪、列 reset。
                if let Some(download) = self.take_open(&mxc) {
                    download.discard(&self.media_pool);
                }
                self.cache_reset(&mxc);
                self.end_job(&mxc, JobEnd::Failed(error.into()));
                return Stepped::Worked;
            }
        }
        let Some(opened) = self.open.get(&mxc) else {
            return Stepped::Worked;
        };
        if opened.download.is_written() {
            downloading
                .done
                .store(downloading.total.load(Ordering::SeqCst), Ordering::SeqCst);
            self.finish_job(&mxc).await;
            return Stepped::Worked;
        }
        let (segments, chunk_size) = (
            opened.download.segments_written(),
            opened.download.manifest().block.chunk_size,
        );
        downloading
            .done
            .store(opened.download.next_chunk(), Ordering::SeqCst);
        self.tick(&mxc, &downloading, segments, chunk_size);
        // 取消插在落地之後（§5.4 第 4 步）：手上那一包已經寫完。
        if downloading.cancelled.load(Ordering::SeqCst) {
            self.end_job(&mxc, JobEnd::Cancelled);
        }
        Stepped::Worked
    }

    /// job 第一次開始：放進 `downloading` 表、發 `downloading`。之後回同一份。
    fn start_job(&mut self, mxc: &str) -> Arc<Downloading> {
        let (downloading, is_new) = {
            let mut state = self.shared.state();
            match state.downloading.get(mxc) {
                Some(existing) => (existing.clone(), false),
                None => {
                    let created = Arc::new(Downloading::default());
                    state.downloading.insert(mxc.to_string(), created.clone());
                    (created, true)
                }
            }
        };
        if is_new {
            let now = Instant::now();
            self.timers.insert(
                mxc.to_string(),
                JobTimers {
                    last_flush: now,
                    last_push: now,
                },
            );
            self.push(mxc, DownloadState::Downloading, 0, 0, None);
        }
        downloading
    }

    /// 每 1.5 秒 fsync＋把段數寫回 DB（顯示用）；每秒最多推一則進度。
    fn tick(&mut self, mxc: &str, downloading: &Downloading, segments: u64, chunk_size: u32) {
        let now = Instant::now();
        let Some(timers) = self.timers.get_mut(mxc) else {
            return;
        };
        let flush_due = now.duration_since(timers.last_flush) >= PROGRESS_FLUSH;
        let push_due = now.duration_since(timers.last_push) >= PUSH_EVERY;
        if flush_due {
            timers.last_flush = now;
            if let Some(opened) = self.open.get_mut(mxc) {
                if let Err(error) = opened.download.sync() {
                    self.events.progress(format!(
                        "download {mxc}: fsync failed (kept going): {error}"
                    ));
                }
            }
            let mxc_here = mxc.to_string();
            self.cache.post(
                move |cache| cache.media_progress(&mxc_here, segments, chunk_size),
                Vec::new(),
            );
        }
        if push_due {
            if let Some(timers) = self.timers.get_mut(mxc) {
                timers.last_push = now;
            }
            self.push(
                mxc,
                DownloadState::Downloading,
                downloading.done.load(Ordering::SeqCst),
                downloading.total.load(Ordering::SeqCst),
                None,
            );
        }
    }

    /// 主檔收齊了：收尾、進池、DB 記完成，**之後**才放掉認領——反過來的話別的帳號會在 DB 記完成之前接手、把剛進池的檔重下一次。
    async fn finish_job(&mut self, mxc: &str) {
        let Some(opened) = self.open.remove(mxc) else {
            return;
        };
        self.shared.state().open_names.remove(&opened.pending_name);
        let end = match opened.download.finish(&self.media_pool) {
            Ok(finished) => match self.record_finished(mxc, &finished).await {
                Ok(()) => JobEnd::Complete,
                Err(error) => JobEnd::Failed(error),
            },
            Err(error) => {
                self.cache_reset(mxc);
                JobEnd::Failed(error.into())
            }
        };
        self.claims.release(&self.server_dir, mxc, &self.user);
        self.end_job(mxc, end);
    }

    async fn record_finished(
        &self,
        mxc: &str,
        finished: &wbf_sdk::media_pool::Finished,
    ) -> Result<(), CoreError> {
        let bytes_on_disk = self.media_pool.bytes_on_disk(&finished.hash_hex)?;
        let (mxc_here, hash, segments, plain_len) = (
            mxc.to_string(),
            finished.hash_hex.clone(),
            finished.segments,
            finished.plain_len,
        );
        self.cache
            .run(move |cache| {
                cache.media_finish(&mxc_here, &hash, segments, plain_len, bytes_on_disk)
            })
            .await
    }

    /// job 結束：拿出佇列與表、發推播、叫醒等它的人。取消的檔留著（再排一次從斷點接），只是關掉把手。
    fn end_job(&mut self, mxc: &str, end: JobEnd) {
        let (downloading, waiters) = {
            let mut state = self.shared.state();
            state.queue.retain(|job| job.mxc != mxc);
            (
                state.downloading.remove(mxc),
                state.waiters.remove(mxc).unwrap_or_default(),
            )
        };
        self.timers.remove(mxc);
        if let JobEnd::Cancelled = end {
            if let Some(mut download) = self.take_open(mxc) {
                let _ = download.sync();
            }
        }
        let (done, total) = downloading
            .map(|downloading| {
                (
                    downloading.done.load(Ordering::SeqCst),
                    downloading.total.load(Ordering::SeqCst),
                )
            })
            .unwrap_or((0, 0));
        let (state, reason, outcome) = match end {
            JobEnd::Complete => (DownloadState::Complete, None, Ok(())),
            JobEnd::Cancelled => (DownloadState::Cancelled, None, Err(cancelled_error(mxc))),
            JobEnd::Failed(error) => (
                DownloadState::Failed,
                Some(error.message.clone()),
                Err(error),
            ),
        };
        self.push(mxc, state, done, total, reason);
        for waiter in waiters {
            let _ = waiter.send(outcome.clone());
        }
    }

    /// GET 要的一塊：完整的池檔 → 主檔已封的段 → seek 暫存檔 → 現拉（/docs/design/media/media-download.md §7.2 第 2～5 列）。
    async fn serve_seek(&mut self, request: SeekRequest) {
        let answer = self.read_for_seek(&request.manifest, request.index).await;
        let _ = request.reply.send(answer);
        self.trim_seek_only();
    }

    async fn read_for_seek(
        &mut self,
        manifest: &Arc<Manifest>,
        index: u32,
    ) -> Result<Zeroizing<Vec<u8>>, CoreError> {
        let mxc = manifest.mxc.clone();
        if !self.open.contains_key(&mxc) {
            if let Some(entry) = self.find_complete_entry(&mxc).await {
                return read_complete_chunk(&self.media_pool, &entry, manifest, index);
            }
            if !self.open_media(manifest).await? {
                return Err(CoreError::new(
                    CoreErrorKind::AccountBusy,
                    format!("{mxc} is being written by another account's download; try again"),
                ));
            }
        }
        let Some(opened) = self.open.get_mut(&mxc) else {
            return Err(worker_gone());
        };
        if let Some(plain) = opened.download.read_local_chunk(index)? {
            return Ok(plain);
        }
        let Some(mut client) = self.links.reuse(LinkRole::Download).await else {
            return Err(link_down());
        };
        let fetched = opened
            .download
            .fetch_into_seek_store(&mut client, &self.media_pool, index)
            .await;
        drop(client);
        match fetched {
            Ok(plain) => Ok(plain),
            Err(error) => {
                // 只為 seek 開的檔不留著等下一次：線斷了、或這一塊壞了，下一個 GET 再開。
                if !self.is_queued(&mxc) {
                    if let Some(mut download) = self.take_open(&mxc) {
                        let _ = download.sync();
                    }
                }
                Err(error.into())
            }
        }
    }

    /// 開（續）一個 mxc 的檔：先在同 server 認領，再建列、拿暫存名、開檔。
    ///
    /// Return:
    ///     Ok(true)    開好了
    ///     Ok(false)   別的帳號正在寫它
    ///     Err(...)    列建不了、區塊算不出塊數、檔建不了
    async fn open_media(&mut self, manifest: &Arc<Manifest>) -> Result<bool, CoreError> {
        let mxc = manifest.mxc.clone();
        if !self.claims.claim(&self.server_dir, &mxc, &self.user) {
            return Ok(false);
        }
        let opened = async {
            let pending_name = self.begin_row(manifest).await?;
            let download = MediaDownload::open(&self.media_pool, &pending_name, manifest)?;
            Ok::<_, CoreError>(OpenMedia {
                pending_name,
                download,
            })
        }
        .await;
        match opened {
            Ok(opened) => {
                self.shared
                    .state()
                    .open_names
                    .insert(opened.pending_name.clone());
                self.open.insert(mxc, opened);
                Ok(true)
            }
            Err(error) => {
                self.claims.release(&self.server_dir, &mxc, &self.user);
                Err(error)
            }
        }
    }

    /// 關一個開著的檔：拿出來、掃描可以碰它了、放掉認領。檔怎麼處置（fsync 留著、刪）由呼叫者決定。
    fn take_open(&mut self, mxc: &str) -> Option<MediaDownload> {
        let opened = self.open.remove(mxc)?;
        self.shared.state().open_names.remove(&opened.pending_name);
        self.claims.release(&self.server_dir, mxc, &self.user);
        Some(opened.download)
    }

    /// `media_begin` ＋ 暫存名（/docs/design/media/media-download.md §4.4 的「job 建立」）。
    async fn begin_row(&self, manifest: &Manifest) -> Result<String, CoreError> {
        let block = manifest.block.clone();
        let (mxc, file_size) = (manifest.mxc.clone(), manifest.file_size());
        self.cache
            .run(move |cache| {
                cache.media_begin(
                    &mxc,
                    block.name.as_deref(),
                    block.mimetype.as_deref(),
                    block.sha256.as_deref(),
                    file_size,
                    block.chunk_size,
                )?;
                cache.media_pending_name(&mxc)?.ok_or_else(|| {
                    SdkError::Io(std::io::Error::other("the media row vanished after insert"))
                })
            })
            .await
    }

    /// DB 說完成、而且池檔真的能用（/docs/design/media/media-download.md §5.3 第一列）。
    async fn find_complete_entry(&self, mxc: &str) -> Option<MediaEntry> {
        let entry = self.cache.read().await.find_media(mxc).ok().flatten()?;
        media::open_complete(&self.media_pool, &entry).map(|_| entry)
    }

    fn is_queued(&self, mxc: &str) -> bool {
        self.shared.state().queue.iter().any(|job| job.mxc == mxc)
    }

    fn cache_reset(&self, mxc: &str) {
        let mxc_here = mxc.to_string();
        self.cache
            .post(move |cache| cache.media_reset(&mxc_here), Vec::new());
    }

    /// 只為 seek 開著的檔太多就關掉一些（排在佇列裡的不關）。
    fn trim_seek_only(&mut self) {
        let jobs: HashSet<String> = self
            .shared
            .state()
            .queue
            .iter()
            .map(|job| job.mxc.clone())
            .collect();
        let mut seek_only: Vec<String> = self
            .open
            .keys()
            .filter(|mxc| !jobs.contains(*mxc))
            .cloned()
            .collect();
        while seek_only.len() > SEEK_ONLY_OPEN {
            let Some(mxc) = seek_only.pop() else {
                break;
            };
            if let Some(mut download) = self.take_open(&mxc) {
                let _ = download.sync();
            }
        }
    }

    fn push(&self, mxc: &str, state: DownloadState, done: u32, total: u32, reason: Option<String>) {
        self.events.emit(CoreEvent::MediaDownload {
            user: self.user.clone(),
            mxc: mxc.to_string(),
            state,
            done,
            total,
            reason,
        });
    }

    /// worker 收攤：fsync 開著的檔、放掉認領。
    fn close_everything(&mut self) {
        let mxcs: Vec<String> = self.open.keys().cloned().collect();
        for mxc in mxcs {
            if let Some(mut download) = self.take_open(&mxc) {
                let _ = download.sync();
            }
        }
    }
}

impl Drop for Worker {
    /// 不管是跑完、還是被 abort（`Downloader` 丟掉）：開著的檔 fsync、認領放掉，🚫 不讓別的帳號的 job 永遠等。
    fn drop(&mut self) {
        self.close_everything();
    }
}

/// `advance` 為什麼沒落地。
enum Advance {
    /// `Download` 線沒開或死了（`link_keeper` 會重開）。
    LinkDown,
    Failed(SdkError),
}

/// 主檔往前一塊：暫存檔有就搬，沒有才借線拉（/docs/design/media/media-download.md §5.4 第 2 步）。
async fn advance(download: &mut MediaDownload, links: &LinkPool) -> Result<(), Advance> {
    match download.advance_from_seek_store() {
        Ok(true) => return Ok(()),
        Ok(false) => {}
        Err(error) => return Err(Advance::Failed(error)),
    }
    let Some(mut client) = links.reuse(LinkRole::Download).await else {
        return Err(Advance::LinkDown);
    };
    download
        .advance_from_server(&mut client)
        .await
        .map_err(Advance::Failed)
}

fn link_down() -> CoreError {
    CoreError::new(
        CoreErrorKind::Network,
        "the download link of this account is not open; it is reopened within a few seconds",
    )
}

/// 從完整的池檔讀第 `index` 塊（seek 進來時檔剛好完成了）。
fn read_complete_chunk(
    pool: &MediaPool,
    entry: &MediaEntry,
    manifest: &Manifest,
    index: u32,
) -> Result<Zeroizing<Vec<u8>>, CoreError> {
    use std::io::{Read, Seek, SeekFrom};
    let chunk_size = u64::from(manifest.block.chunk_size);
    let start = u64::from(index) * chunk_size;
    let end = (start + chunk_size).min(entry.file_size);
    if start >= end {
        return Err(CoreError::new(
            CoreErrorKind::Usage,
            format!("chunk {index} is past the end of {}", entry.mxc),
        ));
    }
    let mut reader = media::open_complete(pool, entry).ok_or_else(|| {
        CoreError::new(
            CoreErrorKind::Io,
            format!("{} is no longer in the pool", entry.mxc),
        )
    })?;
    reader.seek(SeekFrom::Start(start))?;
    let mut plain = Zeroizing::new(vec![0u8; (end - start) as usize]);
    reader.read_exact(&mut plain)?;
    Ok(plain)
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use tokio::sync::broadcast::Receiver;
    use wbf_sdk::Manifest;

    use super::*;
    use crate::accounts::AccountDir;
    use crate::test_support::*;
    use crate::{Core, MediaRef, NewUpload, Target};

    const SIZE: usize = 200;
    const CHUNK: u32 = 16;
    const CHUNKS: u32 = 13;

    fn body() -> Vec<u8> {
        (0..SIZE)
            .map(|position| (position * 7 % 251) as u8)
            .collect()
    }

    /// 一個帳號、`Upload` 與 `Download` 兩條記憶體線；傳一個 200 byte、16 byte 一塊的檔（13 塊），下載那台拿到同一份資料。
    async fn uploaded(name: &str) -> (Core, AccountDir, FakeServer, Manifest) {
        let dir = scratch(name);
        let (core, account) = core_with_wbf_account(&dir).await;
        let (upload_client, upload_server) =
            memory_client_with_hello(Arc::new(Mutex::new(Vec::new()))).await;
        let (download_client, download_server) =
            memory_client_with_hello(Arc::new(Mutex::new(Vec::new()))).await;
        let links = core.pool_of_account(&account).unwrap();
        drop(
            links
                .acquire(LinkRole::Upload, || async move { Ok(upload_client) })
                .await
                .unwrap(),
        );
        drop(
            links
                .acquire(LinkRole::Download, || async move { Ok(download_client) })
                .await
                .unwrap(),
        );
        let request = NewUpload {
            room: None,
            name: "v.bin".to_string(),
            size: Some(SIZE as u64),
            mimetype: Some("video/mp4".to_string()),
            chunk_size: Some(CHUNK),
            ..NewUpload::default()
        };
        let target = Target::default();
        let state = core.create_upload(&request, &target).await.unwrap();
        let plain = body();
        let manifest = core
            .receive_upload(&state, &mut &plain[..], None, &target)
            .await
            .unwrap();
        *download_server.uploads.lock().unwrap() = upload_server.uploads.lock().unwrap().clone();
        (core, account, download_server, manifest)
    }

    /// 收走所有 `Read` 的 permit：下載卡在下一個 `Read`，測試一個一個放。
    async fn hold_reads(server: &FakeServer) {
        server
            .read_permits
            .acquire_many(READ_PERMITS)
            .await
            .unwrap()
            .forget();
    }

    fn reads(server: &FakeServer) -> Vec<u32> {
        server.download_reads.lock().unwrap().clone()
    }

    /// 這個 mxc 的下一個「不是進度」的狀態。
    async fn next_state(events: &mut Receiver<CoreEvent>, mxc: &str) -> DownloadState {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Ok(CoreEvent::MediaDownload {
                    mxc: seen, state, ..
                }) = events.recv().await
                {
                    if seen == mxc && state != DownloadState::Downloading {
                        return state;
                    }
                }
            }
        })
        .await
        .expect("a media.download state")
    }

    async fn wait_until(events: &mut Receiver<CoreEvent>, mxc: &str, wanted: DownloadState) {
        loop {
            match next_state(events, mxc).await {
                state if state == wanted => return,
                DownloadState::Queued => continue,
                other => panic!("expected {wanted:?}, the download ended as {other:?}"),
            }
        }
    }

    async fn read_whole(core: &Core, mxc: &str) -> Vec<u8> {
        let source = core.find_media_source(mxc).await.unwrap().unwrap();
        let size = source.size;
        let mut stream = source.into_stream(0, size);
        let mut out = Vec::new();
        while let Some(piece) = stream.next_piece().await.unwrap() {
            out.extend_from_slice(&piece);
        }
        out
    }

    #[tokio::test]
    async fn a_queued_download_lands_in_the_pool_one_read_per_chunk() {
        let (core, account, server, manifest) = uploaded("dq-whole").await;
        let mut events = core.subscribe();
        let media = MediaRef::Manifest(manifest.clone());
        let job = core
            .media_download(&media, &Target::default())
            .await
            .unwrap();
        assert_eq!(job.state, DownloadState::Queued);
        assert_eq!(
            next_state(&mut events, &manifest.mxc).await,
            DownloadState::Queued
        );
        wait_until(&mut events, &manifest.mxc, DownloadState::Complete).await;
        assert_eq!(reads(&server), (0..CHUNKS).collect::<Vec<_>>());
        // 再要一次：已經完整，不排、不碰網路。
        let again = core
            .media_download(&media, &Target::default())
            .await
            .unwrap();
        assert_eq!(
            (again.state, again.done, again.total),
            (DownloadState::Complete, CHUNKS, CHUNKS)
        );
        assert!(core
            .media_queue(&Target::default())
            .await
            .unwrap()
            .is_empty());
        let (cache, _) = core.server_cache_and_me(&account).unwrap();
        let entry = cache
            .read()
            .await
            .find_media(&manifest.mxc)
            .unwrap()
            .unwrap();
        assert!(entry.complete);
        assert_eq!(entry.segments_written, 1);
        // 讀回來（GET 的完整池檔那條）：跟上傳的一樣，型別照區塊。
        let source = core
            .find_media_source(&manifest.mxc)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(source.mimetype.as_deref(), Some("video/mp4"));
        let mut stream = source.into_stream(30, 70);
        let mut window = Vec::new();
        while let Some(piece) = stream.next_piece().await.unwrap() {
            window.extend_from_slice(&piece);
        }
        assert_eq!(window, body()[30..70]);
        assert_eq!(read_whole(&core, &manifest.mxc).await, body());
        assert_eq!(
            reads(&server).len(),
            CHUNKS as usize,
            "reading the pool does not fetch"
        );
    }

    #[tokio::test]
    async fn the_same_media_is_queued_once() {
        let (core, _account, server, manifest) = uploaded("dq-once").await;
        hold_reads(&server).await;
        let media = MediaRef::Manifest(manifest.clone());
        core.media_download(&media, &Target::default())
            .await
            .unwrap();
        core.media_download(&media, &Target::default())
            .await
            .unwrap();
        let queue = core.media_queue(&Target::default()).await.unwrap();
        assert_eq!(queue.len(), 1, "{queue:?}");
        assert_eq!(queue[0].name.as_deref(), Some("v.bin"));
        server.read_permits.add_permits(READ_PERMITS as usize);
    }

    #[tokio::test]
    async fn a_cancel_lands_only_the_chunk_in_hand_and_a_new_request_resumes() {
        let (core, _account, server, manifest) = uploaded("dq-cancel").await;
        let mut events = core.subscribe();
        hold_reads(&server).await;
        let media = MediaRef::Manifest(manifest.clone());
        core.media_download(&media, &Target::default())
            .await
            .unwrap();
        // worker 卡在第 0 塊的 Read：它已經在 `downloading` 表裡了。
        wait_for_async(
            || async {
                core.media_queue(&Target::default())
                    .await
                    .unwrap()
                    .first()
                    .is_some_and(|item| item.state == DownloadState::Downloading)
            },
            "the job to start",
        )
        .await;
        assert!(core
            .media_cancel(&manifest.mxc, &Target::default())
            .await
            .unwrap());
        // 放行：手上那一塊照樣落地，之後就停。
        server.read_permits.add_permits(READ_PERMITS as usize);
        wait_until(&mut events, &manifest.mxc, DownloadState::Cancelled).await;
        assert_eq!(reads(&server), vec![0]);
        assert!(core
            .media_queue(&Target::default())
            .await
            .unwrap()
            .is_empty());
        assert!(!core
            .media_cancel(&manifest.mxc, &Target::default())
            .await
            .unwrap());
        // 再排一次就拉完（16 byte 的塊還不滿一段，所以從頭來——檔案本身就是進度）。
        core.media_download(&media, &Target::default())
            .await
            .unwrap();
        wait_until(&mut events, &manifest.mxc, DownloadState::Complete).await;
        assert_eq!(read_whole(&core, &manifest.mxc).await, body());
    }

    #[tokio::test]
    async fn a_queued_job_is_cancelled_without_touching_the_network() {
        let (core, account, server, manifest) = uploaded("dq-cancel-queued").await;
        hold_reads(&server).await;
        let downloader = core.downloader_of(&account).await.unwrap();
        // 兩個 job：第一個卡在 Read，第二個還排著。
        let mut second = manifest.clone();
        second.mxc = "mxc://fake/never".to_string();
        downloader.enqueue(Arc::new(manifest.clone()), None);
        let (done, wait) = tokio::sync::oneshot::channel();
        downloader.enqueue(Arc::new(second.clone()), Some(done));
        assert!(downloader.cancel(&second.mxc));
        assert!(
            wait.await.unwrap().is_err(),
            "the waiter hears it was cancelled"
        );
        let jobs = downloader.list_jobs();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].0, manifest.mxc);
        server.read_permits.add_permits(READ_PERMITS as usize);
    }

    #[tokio::test]
    async fn a_seek_jumps_the_queue_and_the_main_file_takes_it_from_the_seek_file() {
        let (core, account, server, manifest) = uploaded("dq-seek").await;
        let mut events = core.subscribe();
        hold_reads(&server).await;
        let downloader = core.downloader_of(&account).await.unwrap();
        downloader.enqueue(Arc::new(manifest.clone()), None);
        wait_for_async(
            || async {
                downloader
                    .list_jobs()
                    .first()
                    .is_some_and(|(_, _, status)| status.state == DownloadState::Downloading)
            },
            "the job to start",
        )
        .await;
        // 播放器 seek 到第 5 塊。佇列卡在第 0 塊的 Read：放一個，第 0 塊落地；worker 回頭先處理 seek。
        let seek = {
            let downloader = downloader.clone();
            let manifest = Arc::new(manifest.clone());
            tokio::spawn(async move { downloader.read_chunk(manifest, 5).await })
        };
        tokio::time::sleep(Duration::from_millis(50)).await;
        server.read_permits.add_permits(1);
        wait_for_async(|| async { reads(&server).len() == 1 }, "chunk 0").await;
        server.read_permits.add_permits(1);
        let chunk = seek.await.unwrap().unwrap();
        assert_eq!(&chunk[..], &body()[80..96]);
        assert_eq!(reads(&server), vec![0, 5]);
        // 放行（worker 一次只做一塊：佇列卡著的話，下一個 seek 也得等它）。同一塊再要一次：從暫存檔；
        // 主檔追到第 5 塊時也從暫存檔搬——第 5 塊整個下載只上網一次（最後的 reads 斷言）。
        server.read_permits.add_permits(READ_PERMITS as usize);
        let again = downloader
            .read_chunk(Arc::new(manifest.clone()), 5)
            .await
            .unwrap();
        assert_eq!(&again[..], &body()[80..96]);
        wait_until(&mut events, &manifest.mxc, DownloadState::Complete).await;
        let mut expected: Vec<u32> = vec![0, 5];
        expected.extend((1..CHUNKS).filter(|index| *index != 5));
        assert_eq!(reads(&server), expected);
        assert_eq!(read_whole(&core, &manifest.mxc).await, body());
        let pool = core.pool_of(&account).unwrap();
        assert!(
            pool.list_pending().unwrap().is_empty(),
            "the seek file goes with the job"
        );
    }

    #[tokio::test]
    async fn save_to_waits_for_the_queue_and_no_cache_leaves_nothing_in_the_pool() {
        let (core, account, _server, manifest) = uploaded("dq-save").await;
        let out = scratch("dq-save-out").join("v.bin");
        let media = MediaRef::Manifest(manifest.clone());
        let saved = core
            .save_media_to(&media, &out, true, &Target::default())
            .await
            .unwrap();
        assert_eq!(saved.source, "server");
        assert_eq!(saved.bytes, SIZE as u64);
        assert_eq!(std::fs::read(&out).unwrap(), body());
        let pool = core.pool_of(&account).unwrap();
        assert!(
            pool.list_files().unwrap().is_empty(),
            "no_cache keeps nothing"
        );
        // 沒有 no_cache：留在池裡，下一次另存直接從池拿。
        core.save_media_to(&media, &out, false, &Target::default())
            .await
            .unwrap();
        let again = core
            .save_media_to(&media, &out, false, &Target::default())
            .await
            .unwrap();
        assert_eq!(again.source, "cache");
        assert_eq!(pool.list_files().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn another_account_writing_the_same_file_makes_this_job_wait() {
        let (core, account, server, manifest) = uploaded("dq-claim").await;
        let mut events = core.subscribe();
        let server_dir = account.server_dir();
        assert!(core
            .media_claims
            .claim(&server_dir, &manifest.mxc, "@someone-else:localhost"));
        core.media_download(&MediaRef::Manifest(manifest.clone()), &Target::default())
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            reads(&server).is_empty(),
            "the job waits while another account writes the file"
        );
        core.media_claims
            .release(&server_dir, &manifest.mxc, "@someone-else:localhost");
        wait_until(&mut events, &manifest.mxc, DownloadState::Complete).await;
        assert_eq!(
            core.media_claims.find_holder(&server_dir, &manifest.mxc),
            None
        );
    }

    #[tokio::test]
    async fn stopping_the_worker_releases_what_it_was_writing() {
        let (core, account, server, manifest) = uploaded("dq-stop").await;
        hold_reads(&server).await;
        core.media_download(&MediaRef::Manifest(manifest.clone()), &Target::default())
            .await
            .unwrap();
        let server_dir = account.server_dir();
        wait_for_async(
            || async {
                core.media_claims
                    .find_holder(&server_dir, &manifest.mxc)
                    .is_some()
            },
            "the worker to claim the file",
        )
        .await;
        core.stop_downloader_of(&account);
        wait_for_async(
            || async {
                core.media_claims
                    .find_holder(&server_dir, &manifest.mxc)
                    .is_none()
            },
            "the aborted worker to release its claim",
        )
        .await;
        server.read_permits.add_permits(READ_PERMITS as usize);
    }

    #[test]
    fn a_claim_belongs_to_one_account_until_it_lets_go() {
        let claims = MediaClaims::default();
        let dir = std::path::PathBuf::from("s1");
        assert!(claims.claim(&dir, "mxc://a/1", "@a:x"));
        assert!(
            claims.claim(&dir, "mxc://a/1", "@a:x"),
            "claiming again is fine"
        );
        assert!(!claims.claim(&dir, "mxc://a/1", "@b:x"));
        assert!(
            claims.claim(&std::path::PathBuf::from("s2"), "mxc://a/1", "@b:x"),
            "another server dir is another file"
        );
        claims.release(&dir, "mxc://a/1", "@b:x");
        assert_eq!(
            claims.find_holder(&dir, "mxc://a/1").as_deref(),
            Some("@a:x"),
            "only the holder lets go"
        );
        claims.release(&dir, "mxc://a/1", "@a:x");
        assert!(claims.claim(&dir, "mxc://a/1", "@b:x"));
    }
}

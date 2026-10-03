//! 每帳號一個下載處理端（/docs/design/media/media-download.md §5、§6）：所有檔一起跑、每個檔一塊在途；`Download` 線送收分開（/docs/design/daemon/link-requests.md）。
//!
//! - **兩條 queue**：收件 queue 放 job（「下載這個檔」、「seek 要這一塊」），**處理一次就消耗掉**；發送 queue 是線的（[`RequestLine`]），放塊請求。
//!   處理「下載這個檔」只是建 `downloading` 項、開主檔、塞第一個請求，所以 jobA、jobB、jobC 進來就是三個檔一起跑，發送 queue 呈 `A B C、A B C`（§5.1）。
//! - **動作封在請求裡**：塊請求（[`DownloadRequest`]）就是它的動作。回覆到了，處理端照它與在途表裡掛著的人（主檔、在等的 GET）決定落到哪；
//!   主檔第 n 塊落地才塞第 n+1 塊。seek 的請求插到發送 queue 最前面（§6.2）。同一個請求🚫 送兩次：第二個要的人掛上去（§6.3）。
//! - **`downloading` 表**放取消旗標與進度（§5.2）：處理 job 時放進去，最後一塊落地、失敗、或看到旗標時由處理端自己拿掉；`media.queue`／推播直接讀它。
//! - **主檔與暫存檔只有一個寫入者**：落地都在這一個 task 裡做；同一台 server 的帳號共用池，所以開檔前先在 [`MediaClaims`] 認領（認的是處理端，不只帳號），
//!   別的處理端正在寫它，這個 job 放進「等認領」、留下記號，對方放掉時馬上再試（只為 seek 開著的會讓出來）；seek 交給認領的那個帳號（`media_stream.rs` 的 `find_writer_of`）。
//!
//! 塊怎麼驗、怎麼落地是 sdk 的 `MediaDownload`；這裡只管「誰、何時、做到哪」。DB 一律經 `ServerCache`（唯一寫入者）。

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use tokio::sync::{mpsc, oneshot, Notify};
use wbf_sdk::cache::MediaEntry;
use wbf_sdk::error_code::WbfErrorCode;
use wbf_sdk::media::{self, MediaDownload, PROGRESS_FLUSH};
use wbf_sdk::media_pool::MediaPool;
use wbf_sdk::{protocol, Manifest, SdkError};
use wbf_wire::Pack;
use zeroize::Zeroizing;

use crate::error::{CoreError, CoreErrorKind};
use crate::event::{DownloadState, EventSink};
use crate::link_pool::{LinkPool, LinkRole};
use crate::link_requests::{LineReply, LineRequest, RequestLine};
use crate::server_cache::ServerCache;
use crate::CoreEvent;

/// 推播進度的間隔：每個檔最多每秒一則（/docs/design/media/media-download.md §5.5）。
const PUSH_EVERY: Duration = Duration::from_secs(1);
/// 別的帳號正在寫同一個檔：等這麼久再試認領。
const CLAIM_RETRY: Duration = Duration::from_secs(1);
/// 沒事做時最久睡多久（收件、回覆、取消都會叫醒它）。
const IDLE_WAKE: Duration = Duration::from_secs(60);
/// 有塊請求在等時，`Download` 線最久可以多久沒有任何回應（/docs/design/daemon/link-requests.md §4）。跟 server 的 `wbf_ws_idle_timeout` 一樣是 60 秒（wbfuwunel #103，2026-10-02）；
/// 線真的死了，心跳 34 秒內就發現（`Network`），所以撞到這個的是「線活著、server 卻不回下載」。
pub(crate) const LINE_SILENCE: Duration = Duration::from_secs(60);
/// 同一個請求連續逾時幾次就當 server 那邊出事（/docs/design/daemon/link-requests.md §4）。
const TIMEOUT_ATTEMPTS: u32 = 3;
/// 只為 seek 開著、不是 job 的檔最多留幾個（每個握著兩個檔案把手）。
const SEEK_ONLY_OPEN: usize = 4;

/// 一個正在下載的檔（/docs/design/media/media-download.md §5.2）：它所有的塊請求共用這一份。
pub(crate) struct Downloading {
    cancelled: AtomicBool,
    /// 已落地的塊數
    done: AtomicU32,
    /// 總塊數
    total: AtomicU32,
    /// 區塊的檔名（`media.queue` 顯示用）
    name: Option<String>,
    /// 第幾個開始的（`media.queue` 照這個排）
    started: u64,
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

/// 收件 queue 裡「下載這個檔」的 job。
struct DownloadJob {
    mxc: String,
    manifest: Arc<Manifest>,
}

/// 收件 queue 裡「seek 要這一塊」的 job（GET，/docs/design/media/media-download.md §6.2）。
struct SeekJob {
    manifest: Arc<Manifest>,
    index: u32,
    reply: ChunkReply,
}

type ChunkReply = oneshot::Sender<Result<Zeroizing<Vec<u8>>, CoreError>>;

/// 等某個 job 結束的人（`media.export_to`）。
type Waiter = oneshot::Sender<Result<(), CoreError>>;

/// 從收件 queue 拿出來、還沒進 `downloading` 表的 job（處理它要等 DB 與開檔）。取消、重複排入、`media.queue` 都要認得它。
struct Preparing {
    name: Option<String>,
    cancelled: bool,
}

#[derive(Default)]
struct QueueState {
    /// 收件 queue 裡還沒處理的「下載這個檔」。
    inbox: VecDeque<DownloadJob>,
    /// 正在處理（建列、認領、開檔）的；處理完就拿掉（§5.3）。
    preparing: HashMap<String, Preparing>,
    /// 認領不到、等別的帳號寫完的（§5.1）。
    awaiting_claim: VecDeque<DownloadJob>,
    downloading: HashMap<String, Arc<Downloading>>,
    waiters: HashMap<String, Vec<Waiter>>,
    /// 處理端現在開著的主檔暫存名（掃描不准碰，`media::sweep` 的 `in_use`）。
    open_names: HashSet<String>,
    /// 下一個開始的檔拿第幾號（`Downloading::started`）。
    next_started: u64,
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

/// 認領的主人：哪個帳號（GET 照它找處理端，`find_writer_of`）、哪一個處理端。
/// 同一個帳號重登之後新舊兩個處理端可能同時活著（舊的被串流中的 GET 留住）：只認帳號的話兩個都能寫同一個檔、也會互相放掉對方的認領。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ClaimHolder {
    pub user: String,
    /// 每起一個處理端拿一個新號（[`MediaClaims::new_holder`]）
    pub handler: u64,
}

/// 開檔是為了什麼（/docs/design/media/media-download.md §5.1）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ClaimFor {
    /// job：認領不到就留下「在等」的記號，只為 seek 開著它的處理端看到就讓出來
    Download,
    /// 只為 GET 要的塊開：別的處理端的 job 在等它，就🚫 搶回來（不然讓出來的馬上又被播放器拿回去）
    Seek,
}

/// 「在等」的記號留多久：等的那個 job 每 `CLAIM_RETRY` 重試一次就延長；job 取消、帳號登出就不再延長，自己過期，🚫 要誰記得清。
const CLAIM_WANTED_FOR: Duration = Duration::from_secs(3);

#[derive(Default)]
struct ClaimTable {
    held: HashMap<(PathBuf, String), ClaimHolder>,
    /// 認領不到、在等的 job：哪個處理端、記號到什麼時候
    wanted: HashMap<(PathBuf, String), (u64, Instant)>,
}

/// 同一台 server（server dir）的同一個 mxc 現在由哪個處理端在寫。
#[derive(Default)]
pub(crate) struct MediaClaims {
    table: Mutex<ClaimTable>,
    next_handler: AtomicU64,
    /// 有人放掉認領：等認領的 job 馬上重試。
    released: Notify,
    /// 有 job 認領不到、留了記號：只為 seek 開著那個檔的處理端看要不要讓。
    /// 跟 `released` 分開：兩個處理端都在等別人的檔時，「我認領不到」去叫對方重試、對方又叫回來，會空轉。
    asked_to_yield: Notify,
}

impl MediaClaims {
    fn table(&self) -> MutexGuard<'_, ClaimTable> {
        self.table
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// 起一個處理端時拿它的認領身分。
    ///
    /// Args:
    ///     user: 這個處理端的帳號, example: "@alice:localhost"
    pub(crate) fn new_holder(&self, user: &str) -> ClaimHolder {
        ClaimHolder {
            user: user.to_string(),
            handler: self.next_handler.fetch_add(1, Ordering::SeqCst),
        }
    }

    /// Return:
    ///     bool  true ＝ 認領到了（本來沒人、或本來就是這個處理端）；
    ///           false ＝ 別的處理端正在寫（`Download` 就留下「在等」的記號），或（`Seek`）別的處理端的 job 在等它
    fn claim(&self, server_dir: &Path, mxc: &str, holder: &ClaimHolder, purpose: ClaimFor) -> bool {
        let key = (server_dir.to_path_buf(), mxc.to_string());
        let now = Instant::now();
        let mut table = self.table();
        if let Some(current) = table.held.get(&key) {
            let mine = current.handler == holder.handler;
            if !mine && purpose == ClaimFor::Download {
                table
                    .wanted
                    .insert(key, (holder.handler, now + CLAIM_WANTED_FOR));
                drop(table);
                self.asked_to_yield.notify_waiters();
            }
            return mine;
        }
        let wanted_by_another = table
            .wanted
            .get(&key)
            .is_some_and(|(handler, until)| *handler != holder.handler && *until > now);
        if purpose == ClaimFor::Seek && wanted_by_another {
            return false;
        }
        if !wanted_by_another {
            table.wanted.remove(&key);
        }
        table.held.insert(key, holder.clone());
        true
    }

    /// 只有認領它的那個處理端放得掉。
    fn release(&self, server_dir: &Path, mxc: &str, holder: &ClaimHolder) {
        let key = (server_dir.to_path_buf(), mxc.to_string());
        let released = {
            let mut table = self.table();
            let mine = table
                .held
                .get(&key)
                .is_some_and(|current| current.handler == holder.handler);
            if mine {
                table.held.remove(&key);
            }
            mine
        };
        if released {
            self.released.notify_waiters();
        }
    }

    /// 別的處理端的 job 在等這個檔嗎（記號還沒過期）。
    fn is_wanted_by_another(&self, server_dir: &Path, mxc: &str, holder: &ClaimHolder) -> bool {
        let key = (server_dir.to_path_buf(), mxc.to_string());
        self.table()
            .wanted
            .get(&key)
            .is_some_and(|(handler, until)| *handler != holder.handler && *until > Instant::now())
    }

    /// Return:
    ///     Some(user)  正在寫它的帳號
    ///     None        沒人在寫
    pub(crate) fn find_holder(&self, server_dir: &Path, mxc: &str) -> Option<String> {
        self.table()
            .held
            .get(&(server_dir.to_path_buf(), mxc.to_string()))
            .map(|holder| holder.user.clone())
    }
}

/// `Download` 線上的塊請求，也是它的動作（/docs/design/daemon/link-requests.md §3）：回覆到了，處理端照它處理。
/// 誰在等它（主檔、哪個 GET）不在這裡，在處理端的在途表（§6.3）：同一個請求只送一次，要的人掛上去。
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum DownloadRequest {
    /// `Download/Info`：驗區塊（/docs/design/media/media-download.md §3.2）。回來之後主檔走「下一塊」、掛著的 seek 送它們的 `Read`。
    Info { mxc: String },
    /// `Download/Read`：第 `index` 塊（§3.3）。回來之後主檔落地、走「下一塊」；seek 存進暫存檔、交給 GET。
    Chunk { mxc: String, index: u32 },
}

impl DownloadRequest {
    fn mxc(&self) -> &str {
        match self {
            DownloadRequest::Info { mxc } | DownloadRequest::Chunk { mxc, .. } => mxc,
        }
    }
}

impl LineRequest for DownloadRequest {
    fn to_pack(&self, seq: u32) -> Pack {
        match self {
            DownloadRequest::Info { mxc } => protocol::info(mxc, seq),
            DownloadRequest::Chunk { mxc, index } => protocol::read_chunk(mxc, *index, seq),
        }
    }
}

/// 在途表的一筆：等它的人，與它已經重送過幾次（只記在這裡：發送 queue 🚫 記，重排、插隊、取消主檔時都沿用這一份）。
struct InFlight {
    waiters: Vec<Waiting>,
    retries: Retries,
}

/// 一個請求重送過幾次。逾時與壞回覆**各算各的**（/docs/design/daemon/link-requests.md §4）：逾時過一次，壞回覆照樣還有一次重拉。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Retries {
    /// 線整段沒回應、重送過幾次（到 `TIMEOUT_ATTEMPTS` 就停下這個檔、檔案留著）
    timeouts: u32,
    /// 回覆壞了（塊驗不過、形狀不對、server 說解不開我們送的）重送過幾次（第二次就是壞檔）
    bad_replies: u32,
}

/// server 正面說「這個檔不在／不給你」（`NotFound`、`Forbidden`）：主檔拿不到了，刪（/docs/design/media/media-download.md §5.4）。
/// 其他的錯（token 過期、限流、server 內部錯、不認得的碼）🚫 刪：檔案留著，再要一次從斷點接著拉。
fn is_permanent_refusal(error: &SdkError) -> bool {
    matches!(
        error.wbf_code(),
        Some(WbfErrorCode::NotFound | WbfErrorCode::Forbidden)
    )
}

/// 回覆壞了：形狀不對，或 server 說解不開我們送的（`Corrupt`）。跟驗不過的塊一樣重送一次。
fn is_bad_reply(error: &SdkError) -> bool {
    matches!(error, SdkError::Protocol(_)) || error.wbf_code() == Some(WbfErrorCode::Corrupt)
}

/// 掛在一個塊請求上、等它回來的人。
enum Waiting {
    /// 主檔（正在跑的 job）。
    Main,
    /// GET 要的第 `index` 塊。掛在 `Info` 上的，`Info` 回來才送它自己的 `Read`。
    Seek { index: u32, reply: ChunkReply },
}

/// 一個帳號的下載處理端。丟掉就 abort（登出、換 session：`Core::close_links`）。
pub(crate) struct Downloader {
    shared: Arc<Shared>,
    seeks: mpsc::UnboundedSender<SeekJob>,
    task: tokio::task::JoinHandle<()>,
    events: EventSink,
}

impl Drop for Downloader {
    /// abort 之後 task 的 future 被丟掉，`DownloadHandler` 跟著 drop：它的 `Drop` 放掉認領（🚫 不靠 abort 剛好停在哪一行），它的線停掉發送端。
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// 處理端要的東西；🚫 不握 `Core`。線在池裡，發送端只借開著的分身。
pub(crate) struct DownloaderParts {
    pub user: String,
    pub server_dir: PathBuf,
    pub links: Arc<LinkPool>,
    pub media_pool: MediaPool,
    pub cache: Arc<ServerCache>,
    pub events: EventSink,
    pub claims: Arc<MediaClaims>,
    /// 有塊請求在等時，`Download` 線最久可以多久沒有任何回應（正式是 `LINE_SILENCE`；測試給短的）
    pub line_silence: Duration,
}

impl Downloader {
    /// 起處理端與 `Download` 線的發送端。⚠️ 要在 tokio runtime 裡叫。
    pub(crate) fn start(parts: DownloaderParts) -> Downloader {
        let shared = Arc::new(Shared {
            user: parts.user.clone(),
            state: Mutex::new(QueueState::default()),
            wake: Notify::new(),
        });
        let (seeks, seek_inbox) = mpsc::unbounded_channel();
        let (replies_tx, replies) = mpsc::unbounded_channel();
        let line = RequestLine::start(
            parts.links,
            LinkRole::Download,
            parts.line_silence,
            replies_tx,
        );
        let events = parts.events.clone();
        let handler = DownloadHandler {
            shared: shared.clone(),
            seeks: seek_inbox,
            replies,
            line,
            holder: parts.claims.new_holder(&parts.user),
            server_dir: parts.server_dir,
            media_pool: parts.media_pool,
            cache: parts.cache,
            events: parts.events,
            claims: parts.claims,
            open: HashMap::new(),
            opening: HashSet::new(),
            in_flight: HashMap::new(),
            timers: HashMap::new(),
            next_claim_retry: None,
        };
        Downloader {
            shared,
            seeks,
            task: tokio::spawn(handler.run()),
            events,
        }
    }

    /// 交一個「下載這個檔」的 job（/docs/design/media/media-download.md §5.3）：已經在跑、在收件 queue 或在等認領就不重複。
    ///
    /// Args:
    ///     manifest: 要整檔的那個（含檔案金鑰）
    ///     waiter: 要等它結束的話給一個（`media.export_to`）；結束時收到 Ok 或那個錯
    /// Return:
    ///     JobStatus   交進去之後（或本來）的樣子
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
                    state.inbox.push_back(DownloadJob {
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

    /// 現在的樣子（`media.queue`）：在跑的照開始的先後，再來是還沒開始的。
    ///
    /// Return:
    ///     Vec<(mxc, 區塊的檔名, JobStatus)>
    pub(crate) fn list_jobs(&self) -> Vec<(String, Option<String>, JobStatus)> {
        let state = self.shared.state();
        let mut running: Vec<(&String, &Arc<Downloading>)> = state.downloading.iter().collect();
        running.sort_by_key(|(_, downloading)| downloading.started);
        let running = running.into_iter().map(|(mxc, downloading)| {
            (
                mxc.clone(),
                downloading.name.clone(),
                JobStatus {
                    state: DownloadState::Downloading,
                    done: downloading.done.load(Ordering::SeqCst),
                    total: downloading.total.load(Ordering::SeqCst),
                },
            )
        });
        let preparing = state.preparing.iter().map(|(mxc, preparing)| {
            (
                mxc.clone(),
                preparing.name.clone(),
                JobStatus {
                    state: DownloadState::Queued,
                    done: 0,
                    total: 0,
                },
            )
        });
        let not_started = state
            .inbox
            .iter()
            .chain(state.awaiting_claim.iter())
            .map(|job| {
                (
                    job.mxc.clone(),
                    job.manifest.block.name.clone(),
                    JobStatus {
                        state: DownloadState::Queued,
                        done: 0,
                        total: 0,
                    },
                )
            });
        running.chain(preparing).chain(not_started).collect()
    }

    /// `media.cancel`（/docs/design/media/media-download.md §5.4 的表）：正在跑 → 設旗標，在途的那一塊落地就停；還沒開始 → 拿掉。
    ///
    /// Return:
    ///     bool  true ＝ 在跑或還沒開始；false ＝ 都不是
    pub(crate) fn cancel(&self, mxc: &str) -> bool {
        let removed_waiting = {
            let mut state = self.shared.state();
            if let Some(downloading) = state.downloading.get(mxc) {
                downloading.cancelled.store(true, Ordering::SeqCst);
                drop(state);
                self.shared.wake.notify_one();
                return true;
            }
            // 正在準備（還沒進表）：記下來，處理端開始它之前會看到。
            if let Some(preparing) = state.preparing.get_mut(mxc) {
                preparing.cancelled = true;
                return true;
            }
            let before = state.inbox.len() + state.awaiting_claim.len();
            state.inbox.retain(|job| job.mxc != mxc);
            state.awaiting_claim.retain(|job| job.mxc != mxc);
            if state.inbox.len() + state.awaiting_claim.len() == before {
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

    /// GET 要第 `index` 塊（seek，/docs/design/media/media-download.md §6）：交一個 seek 的 job，它的請求插到發送 queue 最前面。
    /// 🚫 自己限時：線斷了、線整段沒回應、這一塊驗不過，處理端都會回錯；線沒開時請求排著等線。播放器不要了（斷線）就丟掉這個 future。
    ///
    /// Return:
    ///     Ok(明文)
    ///     Err(Network)     線斷了
    ///     Err(Timeout)     線整段沒有回應（`LINE_SILENCE`）
    ///     Err(Integrity)   這一塊驗不過
    ///     Err(Io)          處理端已經收了
    pub(crate) async fn read_chunk(
        &self,
        manifest: Arc<Manifest>,
        index: u32,
    ) -> Result<Zeroizing<Vec<u8>>, CoreError> {
        let (reply, answer) = oneshot::channel();
        self.seeks
            .send(SeekJob {
                manifest,
                index,
                reply,
            })
            .map_err(|_| downloader_gone())?;
        answer.await.map_err(|_| downloader_gone())?
    }

    /// 收掉處理端（登出、換 session）。從表裡拿掉還不夠：串流中的 GET 握著另一個 `Arc`，只等 drop 的話舊處理端會一直活著——
    /// 它的線已經關了，GET 要的塊永遠等不到；它開著的檔也不在掃描的保護名單上。收掉之後 GET 立刻拿到錯、斷線，播放器重要時找到新的處理端。
    pub(crate) fn stop(&self) {
        self.task.abort();
    }

    /// 處理端現在開著的主檔暫存名（掃描用）。
    pub(crate) fn list_open_names(&self) -> HashSet<String> {
        self.shared.state().open_names.clone()
    }
}

fn mark_preparing(state: &mut QueueState, job: &DownloadJob) {
    state.preparing.insert(
        job.mxc.clone(),
        Preparing {
            name: job.manifest.block.name.clone(),
            cancelled: false,
        },
    );
}

fn status_in(state: &QueueState, mxc: &str) -> Option<JobStatus> {
    if let Some(downloading) = state.downloading.get(mxc) {
        return Some(JobStatus {
            state: DownloadState::Downloading,
            done: downloading.done.load(Ordering::SeqCst),
            total: downloading.total.load(Ordering::SeqCst),
        });
    }
    let not_started = state.preparing.contains_key(mxc)
        || state
            .inbox
            .iter()
            .chain(state.awaiting_claim.iter())
            .any(|job| job.mxc == mxc);
    not_started.then_some(JobStatus {
        state: DownloadState::Queued,
        done: 0,
        total: 0,
    })
}

fn downloader_gone() -> CoreError {
    CoreError::new(
        CoreErrorKind::Io,
        "the downloader of this account has stopped (logged out?)",
    )
}

/// GET 帶來的描述跟處理端手上那個檔的切法（大小、塊大小）不一樣。
fn description_mismatch(mxc: &str) -> CoreError {
    CoreError::new(
        CoreErrorKind::Integrity,
        format!("this description of {mxc} does not match the local copy (size or chunk size); the copy is kept"),
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

/// 正在跑的檔的計時器。
struct JobTimers {
    last_flush: Instant,
    last_push: Instant,
}

/// 處理端開著的一個檔。
struct OpenMedia {
    /// 主檔的暫存名（`m<media.id>`），掃描不准碰
    pending_name: String,
    download: MediaDownload,
}

struct DownloadHandler {
    shared: Arc<Shared>,
    seeks: mpsc::UnboundedReceiver<SeekJob>,
    /// 線交回來的回覆（每個送出的塊請求恰好一個）。
    replies: mpsc::UnboundedReceiver<LineReply<DownloadRequest>>,
    line: RequestLine<DownloadRequest>,
    /// 這個處理端的認領身分（`user` 就是它的帳號）
    holder: ClaimHolder,
    server_dir: PathBuf,
    media_pool: MediaPool,
    cache: Arc<ServerCache>,
    events: EventSink,
    claims: Arc<MediaClaims>,
    /// 開著的檔（在跑的，與只為 seek 開的）。處理端是它們唯一的寫入者。
    open: HashMap<String, OpenMedia>,
    /// 認領到了、還在開檔（等 DB 建列）的 mxc：還不在 `open` 裡，但處理端在這時被收掉也要放掉它的認領（`close_everything`）。
    opening: HashSet<String>,
    /// 在途表：排著或送出去的塊請求 → 等它的人（/docs/design/media/media-download.md §6.3）。
    in_flight: HashMap<DownloadRequest, InFlight>,
    timers: HashMap<String, JobTimers>,
    next_claim_retry: Option<Instant>,
}

impl DownloadHandler {
    async fn run(mut self) {
        loop {
            self.process_inbox().await;
            self.handle_cancellations();
            if self
                .next_claim_retry
                .is_some_and(|due| Instant::now() >= due)
            {
                self.retry_claims().await;
            }
            self.yield_wanted_seek_only();
            let wait = self
                .next_claim_retry
                .map(|due| due.saturating_duration_since(Instant::now()))
                .unwrap_or(IDLE_WAKE);
            tokio::select! {
                biased;
                seek = self.seeks.recv() => match seek {
                    Some(job) => self.process_seek(job).await,
                    None => break,
                },
                reply = self.replies.recv() => match reply {
                    Some(reply) => self.handle_reply(reply).await,
                    None => break,
                },
                _ = self.shared.wake.notified() => {}
                // 有人放掉認領：在等認領的 job 馬上重試，🚫 等到下一秒。
                _ = self.claims.released.notified() => {
                    if self.next_claim_retry.is_some() {
                        self.next_claim_retry = Some(Instant::now());
                    }
                }
                // 有 job 在等某個檔：醒來走一輪，上面的 `yield_wanted_seek_only` 看要不要讓。
                _ = self.claims.asked_to_yield.notified() => {}
                _ = tokio::time::sleep(wait) => {}
            }
        }
    }

    /// 收件 queue 裡的「下載這個檔」一個一個處理掉（§5.1）。
    async fn process_inbox(&mut self) {
        loop {
            let next = {
                let mut state = self.shared.state();
                let job = state.inbox.pop_front();
                if let Some(job) = &job {
                    mark_preparing(&mut state, job);
                }
                job
            };
            let Some(job) = next else {
                return;
            };
            self.process_download_job(job).await;
        }
    }

    /// 處理一個「下載這個檔」：它在處理的整段都算「準備中」（取消找得到），處理完拿掉。
    ///
    /// 處理端看過旗標之後、拿掉「準備中」之前來的取消（處理端與 `cancel` 在不同執行緒），旗標🚫 跟著條目一起丟：
    /// 拿掉跟轉交在同一把鎖裡——job 已經開始了就設到 `downloading` 那一份（下一輪 `handle_cancellations` 停它），
    /// 停在「等認領」就從那裡拿掉、發 `Cancelled`。
    async fn process_download_job(&mut self, job: DownloadJob) {
        let mxc = job.mxc.clone();
        self.prepare_download(job).await;
        let parked_and_cancelled = {
            let mut state = self.shared.state();
            let cancelled = state
                .preparing
                .remove(&mxc)
                .is_some_and(|preparing| preparing.cancelled);
            if !cancelled {
                false
            } else if let Some(downloading) = state.downloading.get(&mxc) {
                downloading.cancelled.store(true, Ordering::SeqCst);
                false
            } else {
                let before = state.awaiting_claim.len();
                state.awaiting_claim.retain(|parked| parked.mxc != mxc);
                state.awaiting_claim.len() != before
            }
        };
        if parked_and_cancelled {
            self.end_job(&mxc, JobEnd::Cancelled);
        }
    }

    /// 準備中被取消了嗎（`Downloader::cancel` 在它進 `downloading` 表之前來的）。
    fn is_cancelled_while_preparing(&self, mxc: &str) -> bool {
        self.shared
            .state()
            .preparing
            .get(mxc)
            .is_some_and(|preparing| preparing.cancelled)
    }

    /// 建 downloading 項、開主檔、塞第一個請求（/docs/design/media/media-download.md §5.3）。
    async fn prepare_download(&mut self, job: DownloadJob) {
        let mxc = job.mxc.clone();
        if self.find_downloading(&mxc).is_some() {
            return;
        }
        // 已經有完整的（別的帳號寫完了、或之前就有）：不下載。
        if !self.open.contains_key(&mxc) && self.find_complete_entry(&mxc).await.is_some() {
            self.end_job(&mxc, JobEnd::Complete);
            return;
        }
        // 只為 seek 開著、用的是另一則事件的描述：關掉（留著、🚫 刪）再用這個 job 自己那份開。
        // job 跑的檔一律用 job 的描述——驗不過才能確定是檔壞了（`is_job_running`）。
        let opened_with_another = self
            .open
            .get(&mxc)
            .is_some_and(|opened| opened.download.manifest() != job.manifest.as_ref());
        if opened_with_another {
            let error = CoreError::new(
                CoreErrorKind::AccountBusy,
                format!("{mxc} was reopened for a download; try again"),
            );
            self.fail_media(&mxc, error, false);
        }
        if !self.open.contains_key(&mxc) {
            match self.open_media(&job.manifest, ClaimFor::Download).await {
                Ok(true) => {}
                Ok(false) if self.is_cancelled_while_preparing(&mxc) => {
                    self.end_job(&mxc, JobEnd::Cancelled);
                    return;
                }
                Ok(false) => {
                    self.shared.state().awaiting_claim.push_back(job);
                    self.next_claim_retry
                        .get_or_insert_with(|| Instant::now() + CLAIM_RETRY);
                    return;
                }
                Err(error) => {
                    self.end_job(&mxc, JobEnd::Failed(error));
                    return;
                }
            }
        }
        let Some(opened) = self.open.get(&mxc) else {
            return;
        };
        let (done, total) = (opened.download.next_chunk(), opened.download.chunk_count());
        if self.is_cancelled_while_preparing(&mxc) {
            self.end_job(&mxc, JobEnd::Cancelled);
            return;
        }
        self.start_job(&mxc, job.manifest.block.name.clone(), done, total);
        self.continue_main(&mxc).await;
    }

    /// 等認領的 job 再處理一次（別的帳號寫完了，DB 會說完成）。
    async fn retry_claims(&mut self) {
        self.next_claim_retry = None;
        let waiting: Vec<DownloadJob> = {
            let mut state = self.shared.state();
            let jobs: Vec<DownloadJob> = state.awaiting_claim.drain(..).collect();
            for job in &jobs {
                mark_preparing(&mut state, job);
            }
            jobs
        };
        for job in waiting {
            self.process_download_job(job).await;
        }
    }

    /// 主檔的「下一塊」（/docs/design/media/media-download.md §5.4）：暫存檔有就搬、不走網路；沒有就塞它的請求，等回覆。
    async fn continue_main(&mut self, mxc: &str) {
        loop {
            let Some(downloading) = self.find_downloading(mxc) else {
                return;
            };
            // 取消在一塊落地之後生效（§5.4）：走到這裡，手上那一塊已經寫完了。
            if downloading.cancelled.load(Ordering::SeqCst) {
                self.end_job(mxc, JobEnd::Cancelled);
                return;
            }
            let Some(opened) = self.open.get_mut(mxc) else {
                self.end_job(mxc, JobEnd::Failed(downloader_gone()));
                return;
            };
            if opened.download.is_written() {
                downloading
                    .done
                    .store(opened.download.chunk_count(), Ordering::SeqCst);
                self.finish_job(mxc).await;
                return;
            }
            if opened.download.needs_info() {
                self.want(
                    DownloadRequest::Info {
                        mxc: mxc.to_string(),
                    },
                    Waiting::Main,
                    false,
                );
                return;
            }
            match opened.download.advance_from_seek_store() {
                Ok(true) => {
                    self.after_landing(mxc);
                    continue;
                }
                Ok(false) => {}
                Err(error) => {
                    self.fail_media(mxc, error.into(), true);
                    return;
                }
            }
            let index = opened.download.next_chunk();
            self.want(
                DownloadRequest::Chunk {
                    mxc: mxc.to_string(),
                    index,
                },
                Waiting::Main,
                false,
            );
            return;
        }
    }

    /// 要一個請求的結果：已經在途就掛上去（🚫 送兩次），沒有就排進發送 queue。
    ///
    /// Args:
    ///     at_front: seek 要的放最前面（§6.2）；本來排在後面的同一個請求一起提到前面
    fn want(&mut self, request: DownloadRequest, waiting: Waiting, at_front: bool) {
        if let Some(entry) = self.in_flight.get_mut(&request) {
            entry.waiters.push(waiting);
            if at_front && self.line.withdraw(&request) {
                self.line.push_front(request);
            }
            return;
        }
        self.in_flight.insert(
            request.clone(),
            InFlight {
                waiters: vec![waiting],
                retries: Retries::default(),
            },
        );
        match at_front {
            true => self.line.push_front(request),
            false => self.line.push_back(request),
        }
    }

    /// 一個塊請求的結果回來了（/docs/design/daemon/link-requests.md §4 的失敗處置）。
    async fn handle_reply(&mut self, reply: LineReply<DownloadRequest>) {
        let LineReply { action, result } = reply;
        // 沒人等了（取消、失敗、收尾時拿掉的）：丟掉。
        let Some(InFlight { waiters, retries }) = self.in_flight.remove(&action) else {
            return;
        };
        match result {
            Ok(ack) => match action {
                DownloadRequest::Info { mxc } => {
                    self.info_arrived(&mxc, ack, waiters, retries).await
                }
                DownloadRequest::Chunk { mxc, index } => {
                    self.chunk_arrived(&mxc, index, ack, waiters, retries).await
                }
            },
            // 線斷了：主檔的請求排回最前面，線重開之後接著送，進度停在原地；GET 🚫 等線，直接斷（播放器會再要）。
            Err(SdkError::Network(message)) => {
                let error = CoreError::new(CoreErrorKind::Network, message);
                let main = self.answer_seeks_only(waiters, &error);
                if main {
                    self.resend(action, vec![Waiting::Main], retries);
                }
            }
            Err(SdkError::Timeout(_)) if retries.timeouts + 1 < TIMEOUT_ATTEMPTS => {
                let retries = Retries {
                    timeouts: retries.timeouts + 1,
                    ..retries
                };
                self.resend(action, waiters, retries)
            }
            Err(error) if is_bad_reply(&error) && retries.bad_replies == 0 => {
                let retries = Retries {
                    bad_replies: 1,
                    ..retries
                };
                self.resend(action, waiters, retries)
            }
            // 逾時太多次、server 拒絕、暫時性的錯：照錯誤碼決定刪不刪（§5.4）。只有 GET 在等的請求失敗，只回 GET，🚫 連坐停掉主檔——
            // 除非 server 說這個檔不在／不給（主檔一樣拿不到）。
            Err(error) => {
                let discard = is_permanent_refusal(&error);
                let error = CoreError::from(error);
                let main = self.answer_seeks_only(waiters, &error);
                if main || discard {
                    self.fail_media(action.mxc(), error, discard);
                }
            }
        }
    }

    fn resend(&mut self, request: DownloadRequest, waiters: Vec<Waiting>, retries: Retries) {
        self.in_flight
            .insert(request.clone(), InFlight { waiters, retries });
        self.line.push_front(request);
    }

    /// 把錯交給掛著的 GET。
    ///
    /// Return:
    ///     bool  true ＝ 主檔也掛在上面（由呼叫者處置）
    fn answer_seeks_only(&mut self, waiters: Vec<Waiting>, error: &CoreError) -> bool {
        let mut main = false;
        for waiting in waiters {
            match waiting {
                Waiting::Main => main = true,
                Waiting::Seek { reply, .. } => {
                    let _ = reply.send(Err(error.clone()));
                }
            }
        }
        main
    }

    /// `Info` 回來（§3.2）：驗過就讓主檔走「下一塊」、掛著的 seek 送它們的 `Read`；驗不過就是壞檔。
    async fn info_arrived(
        &mut self,
        mxc: &str,
        ack: Pack,
        waiters: Vec<Waiting>,
        retries: Retries,
    ) {
        let Some(opened) = self.open.get_mut(mxc) else {
            self.answer_seeks_only(waiters, &downloader_gone());
            return;
        };
        let verified = protocol::info_reply(ack)
            .and_then(|(info, description)| opened.download.accept_info(&info, &description));
        match verified {
            Ok(()) => {}
            // 回覆的形狀不對：重送一次（續傳要先問 Info，一次怪回覆🚫 就刪掉之前的進度）。
            Err(error) if is_bad_reply(&error) && retries.bad_replies == 0 => {
                let request = DownloadRequest::Info {
                    mxc: mxc.to_string(),
                };
                let retries = Retries {
                    bad_replies: 1,
                    ..retries
                };
                self.resend(request, waiters, retries);
                return;
            }
            // 區塊跟 server 對不上、描述解不開：壞檔（§3.2）——但只為 seek 開的檔🚫 刪（下面 `is_job_running`）。
            Err(error) => {
                let error = CoreError::from(error);
                self.answer_seeks_only(waiters, &error);
                let discard = self.is_job_running(mxc);
                self.fail_media(mxc, error, discard);
                return;
            }
        }
        let mut main = false;
        for waiting in waiters {
            match waiting {
                Waiting::Main => main = true,
                Waiting::Seek { index, reply } => self.request_seek_chunk(mxc, index, reply),
            }
        }
        if main {
            self.continue_main(mxc).await;
        }
    }

    /// `Read` 回來（§3.3、§5.4、§6.3）：解開；主檔在等就落地、走「下一塊」，只有 GET 在等就存進 seek 暫存檔；交給每個在等的 GET。
    async fn chunk_arrived(
        &mut self,
        mxc: &str,
        index: u32,
        ack: Pack,
        waiters: Vec<Waiting>,
        retries: Retries,
    ) {
        let Some(opened) = self.open.get_mut(mxc) else {
            self.answer_seeks_only(waiters, &downloader_gone());
            return;
        };
        let opened_chunk = protocol::read_reply(ack, index)
            .and_then(|(_read, data)| opened.download.open_chunk(index, &data));
        let plain = match opened_chunk {
            Ok(plain) => plain,
            // 一塊壞了：重拉一次（傳輸錯）；還是壞 → 整個檔當壞檔（§3.3）。
            Err(SdkError::Integrity(_) | SdkError::Protocol(_)) if retries.bad_replies == 0 => {
                let request = DownloadRequest::Chunk {
                    mxc: mxc.to_string(),
                    index,
                };
                let retries = Retries {
                    bad_replies: 1,
                    ..retries
                };
                self.resend(request, waiters, retries);
                return;
            }
            Err(error) => {
                let error = CoreError::from(error);
                self.answer_seeks_only(waiters, &error);
                let discard = self.is_job_running(mxc);
                self.fail_media(mxc, error, discard);
                return;
            }
        };
        let main_waits = waiters
            .iter()
            .any(|waiting| matches!(waiting, Waiting::Main));
        let lands_in_main = main_waits && index == opened.download.next_chunk();
        let stored = match lands_in_main {
            true => opened.download.land_chunk(index, &plain),
            false => opened
                .download
                .store_seek_chunk(&self.media_pool, index, &plain),
        };
        for waiting in waiters {
            if let Waiting::Seek { reply, .. } = waiting {
                let _ = reply.send(Ok(plain.clone()));
            }
        }
        match stored {
            Ok(()) if lands_in_main => self.after_landing(mxc),
            Ok(()) => {}
            // 主檔寫不了是這個檔的事；暫存檔寫不了只是下次再拉（GET 已經拿到了）。
            Err(error) if lands_in_main => {
                self.fail_media(mxc, error.into(), false);
                return;
            }
            Err(error) => self.events.progress(format!(
                "download {mxc}: chunk {index} could not go into the seek file (kept going): {error}"
            )),
        }
        if main_waits {
            self.continue_main(mxc).await;
        }
        self.trim_seek_only();
    }

    /// 處理一個「seek 要這一塊」（/docs/design/media/media-download.md §6.2）：本地有就給，沒有就把請求插到最前面。
    async fn process_seek(&mut self, job: SeekJob) {
        let SeekJob {
            manifest,
            index,
            reply,
        } = job;
        let mxc = manifest.mxc.clone();
        // GET 照自己那份描述的塊大小切片：給它的塊一定要照同一個大小切，對不上就回錯，🚫 給位置錯的明文（/docs/design/media/media-download.md §7.2）。
        if let Some(opened) = self.open.get(&mxc) {
            let open_with = opened.download.manifest();
            if open_with.file_size() != manifest.file_size()
                || open_with.block.chunk_size != manifest.block.chunk_size
            {
                let _ = reply.send(Err(description_mismatch(&mxc)));
                return;
            }
        } else {
            if let Some(entry) = self.find_complete_entry(&mxc).await {
                let answer = match media::is_same_file(&entry, &manifest) {
                    true => read_complete_chunk(&self.media_pool, &entry, index),
                    false => Err(description_mismatch(&mxc)),
                };
                let _ = reply.send(answer);
                return;
            }
            match self.open_media(&manifest, ClaimFor::Seek).await {
                Ok(true) => {}
                Ok(false) => {
                    let _ = reply.send(Err(CoreError::new(
                        CoreErrorKind::AccountBusy,
                        format!("{mxc} is being written by another account's download; try again"),
                    )));
                    return;
                }
                Err(error) => {
                    let _ = reply.send(Err(error));
                    return;
                }
            }
        }
        self.request_seek_chunk(&mxc, index, reply);
        self.trim_seek_only();
    }

    /// GET 要的第 `index` 塊：本地有（主檔已封的段、暫存檔）就給；沒有就要它（還沒驗過區塊就先掛在 `Info` 上）。
    fn request_seek_chunk(&mut self, mxc: &str, index: u32, reply: ChunkReply) {
        let Some(opened) = self.open.get_mut(mxc) else {
            let _ = reply.send(Err(downloader_gone()));
            return;
        };
        match opened.download.read_local_chunk(index) {
            Ok(Some(plain)) => {
                let _ = reply.send(Ok(plain));
                return;
            }
            Ok(None) => {}
            Err(error) => {
                let _ = reply.send(Err(error.into()));
                return;
            }
        }
        let request = match opened.download.needs_info() {
            true => DownloadRequest::Info {
                mxc: mxc.to_string(),
            },
            false => DownloadRequest::Chunk {
                mxc: mxc.to_string(),
                index,
            },
        };
        self.want(request, Waiting::Seek { index, reply }, true);
    }

    /// 旗標設了的檔：主檔的請求還排著（沒送出去）就拿掉、當場停；已經送出去的等它回來落地再停（§5.4）。
    fn handle_cancellations(&mut self) {
        let cancelled: Vec<String> = self
            .shared
            .state()
            .downloading
            .iter()
            .filter(|(_, downloading)| downloading.cancelled.load(Ordering::SeqCst))
            .map(|(mxc, _)| mxc.clone())
            .collect();
        for mxc in cancelled {
            let main_request = self
                .in_flight
                .iter()
                .find(|(request, entry)| {
                    request.mxc() == mxc
                        && entry
                            .waiters
                            .iter()
                            .any(|waiting| matches!(waiting, Waiting::Main))
                })
                .map(|(request, _)| request.clone());
            let Some(request) = main_request else {
                self.end_job(&mxc, JobEnd::Cancelled);
                continue;
            };
            if !self.line.withdraw(&request) {
                continue;
            }
            let Some(InFlight { waiters, retries }) = self.in_flight.remove(&request) else {
                self.end_job(&mxc, JobEnd::Cancelled);
                continue;
            };
            let others: Vec<Waiting> = waiters
                .into_iter()
                .filter(|waiting| !matches!(waiting, Waiting::Main))
                .collect();
            // 同一個請求還有 GET 在等：放回去照送，重送次數沿用。
            if !others.is_empty() {
                self.resend(request, others, retries);
            }
            self.end_job(&mxc, JobEnd::Cancelled);
        }
    }

    /// 開始一個檔：放進 `downloading` 表、發 `downloading`。
    fn start_job(&mut self, mxc: &str, name: Option<String>, done: u32, total: u32) {
        {
            let mut state = self.shared.state();
            let started = state.next_started;
            state.next_started += 1;
            state.downloading.insert(
                mxc.to_string(),
                Arc::new(Downloading {
                    cancelled: AtomicBool::new(false),
                    done: AtomicU32::new(done),
                    total: AtomicU32::new(total),
                    name,
                    started,
                }),
            );
        }
        let now = Instant::now();
        self.timers.insert(
            mxc.to_string(),
            JobTimers {
                last_flush: now,
                last_push: now,
            },
        );
        self.push(mxc, DownloadState::Downloading, done, total, None);
    }

    fn find_downloading(&self, mxc: &str) -> Option<Arc<Downloading>> {
        self.shared.state().downloading.get(mxc).cloned()
    }

    /// 這個檔有 job 在跑嗎。有的話開著的檔用的就是 job 自己那份描述（`prepare_download` 保證），驗不過是檔壞了、刪；
    /// 沒有的話它只為 seek 開著、用的是 GET 帶來的描述：驗不過可疑的是那份描述（寫錯或偽造的事件），🚫 刪別人拉到一半的主檔。
    fn is_job_running(&self, mxc: &str) -> bool {
        self.shared.state().downloading.contains_key(mxc)
    }

    /// 主檔落了一塊：更新進度、到點就 fsync／寫 DB／推播。
    fn after_landing(&mut self, mxc: &str) {
        let (Some(downloading), Some(opened)) = (self.find_downloading(mxc), self.open.get(mxc))
        else {
            return;
        };
        let (next, segments, chunk_size) = (
            opened.download.next_chunk(),
            opened.download.segments_written(),
            opened.download.manifest().block.chunk_size,
        );
        downloading.done.store(next, Ordering::SeqCst);
        self.tick(mxc, &downloading, segments, chunk_size);
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
        self.claims.release(&self.server_dir, mxc, &self.holder);
        self.answer_seeks_from_pool(mxc).await;
        self.end_job(mxc, end);
    }

    /// 檔收尾了，還掛在它的請求上的 GET：從完整的池檔給（請求本身拿掉，回來也沒人要）。
    async fn answer_seeks_from_pool(&mut self, mxc: &str) {
        let requests: Vec<DownloadRequest> = self
            .in_flight
            .keys()
            .filter(|request| request.mxc() == mxc)
            .cloned()
            .collect();
        if requests.is_empty() {
            return;
        }
        let entry = self.find_complete_entry(mxc).await;
        for request in requests {
            self.line.withdraw(&request);
            let waiters = self
                .in_flight
                .remove(&request)
                .map(|entry| entry.waiters)
                .unwrap_or_default();
            for waiting in waiters {
                let Waiting::Seek { index, reply } = waiting else {
                    continue;
                };
                let answer = match &entry {
                    Some(entry) => read_complete_chunk(&self.media_pool, entry, index),
                    None => Err(downloader_gone()),
                };
                let _ = reply.send(answer);
            }
        }
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

    /// 這個檔停下來：它還排著的請求拿掉、掛著的 GET 收到錯；`discard` 就刪主檔與暫存檔（壞檔，§5.4），否則 fsync 留著（下次接著拉）。
    fn fail_media(&mut self, mxc: &str, error: CoreError, discard: bool) {
        let requests: Vec<DownloadRequest> = self
            .in_flight
            .keys()
            .filter(|request| request.mxc() == mxc)
            .cloned()
            .collect();
        for request in requests {
            self.line.withdraw(&request);
            let waiters = self
                .in_flight
                .remove(&request)
                .map(|entry| entry.waiters)
                .unwrap_or_default();
            self.answer_seeks_only(waiters, &error);
        }
        if let Some(mut download) = self.take_open(mxc) {
            match discard {
                true => {
                    download.discard(&self.media_pool);
                    self.cache_reset(mxc);
                }
                false => {
                    let _ = download.sync();
                }
            }
        }
        if self.find_downloading(mxc).is_some() {
            self.end_job(mxc, JobEnd::Failed(error));
        }
    }

    /// job 結束：拿出表、發推播、叫醒等它的人。取消的檔留著（再要一次從斷點接）；還有 GET 在等它的塊就先開著。
    fn end_job(&mut self, mxc: &str, end: JobEnd) {
        let (downloading, waiters) = {
            let mut state = self.shared.state();
            (
                state.downloading.remove(mxc),
                state.waiters.remove(mxc).unwrap_or_default(),
            )
        };
        self.timers.remove(mxc);
        let seeks_waiting = self.in_flight.keys().any(|request| request.mxc() == mxc);
        if matches!(end, JobEnd::Cancelled) && !seeks_waiting {
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

    /// 開（續）一個 mxc 的檔：先在同 server 認領，再建列、拿暫存名、開檔。
    ///
    /// Args:
    ///     purpose: job 要開的、還是只為 GET 要的塊開的（[`ClaimFor`]）
    /// Return:
    ///     Ok(true)    開好了
    ///     Ok(false)   別的處理端正在寫它（`Seek`：或別的處理端的 job 在等它）
    ///     Err(...)    列建不了、區塊算不出塊數、檔建不了
    async fn open_media(
        &mut self,
        manifest: &Arc<Manifest>,
        purpose: ClaimFor,
    ) -> Result<bool, CoreError> {
        let mxc = manifest.mxc.clone();
        if !self
            .claims
            .claim(&self.server_dir, &mxc, &self.holder, purpose)
        {
            return Ok(false);
        }
        // 下面要等 DB：在等的時候被 abort，`Drop` 從這裡知道要放掉這個認領。
        self.opening.insert(mxc.clone());
        let opened = async {
            let pending_name = self.begin_row(manifest).await?;
            let download = MediaDownload::open(&self.media_pool, &pending_name, manifest)?;
            Ok::<_, CoreError>(OpenMedia {
                pending_name,
                download,
            })
        }
        .await;
        self.opening.remove(&mxc);
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
                self.claims.release(&self.server_dir, &mxc, &self.holder);
                Err(error)
            }
        }
    }

    /// 關一個開著的檔：拿出來、掃描可以碰它了、放掉認領。檔怎麼處置（fsync 留著、刪）由呼叫者決定。
    fn take_open(&mut self, mxc: &str) -> Option<MediaDownload> {
        let opened = self.open.remove(mxc)?;
        self.shared.state().open_names.remove(&opened.pending_name);
        self.claims.release(&self.server_dir, mxc, &self.holder);
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

    fn cache_reset(&self, mxc: &str) {
        let mxc_here = mxc.to_string();
        self.cache
            .post(move |cache| cache.media_reset(&mxc_here), Vec::new());
    }

    /// 只為 seek 開著、現在沒事的檔（不是在跑的 job、沒有請求在途）：關掉它們🚫 影響任何人。
    fn list_idle_seek_only(&self) -> Vec<String> {
        let running: HashSet<String> = self.shared.state().downloading.keys().cloned().collect();
        let busy: HashSet<&str> = self.in_flight.keys().map(DownloadRequest::mxc).collect();
        self.open
            .keys()
            .filter(|mxc| !running.contains(*mxc) && !busy.contains(mxc.as_str()))
            .cloned()
            .collect()
    }

    /// 只為 seek 開著的檔太多就關掉一些。
    fn trim_seek_only(&mut self) {
        let mut seek_only = self.list_idle_seek_only();
        while seek_only.len() > SEEK_ONLY_OPEN {
            let Some(mxc) = seek_only.pop() else {
                break;
            };
            if let Some(mut download) = self.take_open(&mxc) {
                let _ = download.sync();
            }
        }
    }

    /// 別的處理端的 job 在等的檔，這裡只為 seek 開著：關掉、放掉認領讓它寫（/docs/design/media/media-download.md §5.1）。
    /// 🚫 等它湊滿 `SEEK_ONLY_OPEN` 才關——只為 seek 開的檔永遠不會寫完，對方會一直等。
    fn yield_wanted_seek_only(&mut self) {
        for mxc in self.list_idle_seek_only() {
            if !self
                .claims
                .is_wanted_by_another(&self.server_dir, &mxc, &self.holder)
            {
                continue;
            }
            if let Some(mut download) = self.take_open(&mxc) {
                let _ = download.sync();
            }
        }
    }

    fn push(&self, mxc: &str, state: DownloadState, done: u32, total: u32, reason: Option<String>) {
        self.events.emit(CoreEvent::MediaDownload {
            user: self.holder.user.clone(),
            mxc: mxc.to_string(),
            state,
            done,
            total,
            reason,
        });
    }

    /// 處理端收攤：fsync 開著的檔、放掉認領。
    fn close_everything(&mut self) {
        let mxcs: Vec<String> = self.open.keys().cloned().collect();
        for mxc in mxcs {
            if let Some(mut download) = self.take_open(&mxc) {
                let _ = download.sync();
            }
        }
        for mxc in std::mem::take(&mut self.opening) {
            self.claims.release(&self.server_dir, &mxc, &self.holder);
        }
    }
}

impl Drop for DownloadHandler {
    /// 不管是跑完、還是被 abort（`Downloader` 丟掉）：開著的檔 fsync、認領放掉，🚫 不讓別的帳號的 job 永遠等。
    fn drop(&mut self) {
        self.close_everything();
    }
}

/// 從完整的池檔讀第 `index` 塊（seek 進來時檔剛好完成了）。塊大小照 DB 那一列（`media_begin` 從區塊抄的）。
fn read_complete_chunk(
    pool: &MediaPool,
    entry: &MediaEntry,
    index: u32,
) -> Result<Zeroizing<Vec<u8>>, CoreError> {
    use std::io::{Read, Seek, SeekFrom};
    let chunk_size = u64::from(entry.chunk_size);
    let start = u64::from(index) * chunk_size;
    let end = (start + chunk_size).min(entry.file_size);
    if chunk_size == 0 || start >= end {
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

    /// 第 `file` 個檔的內容（每個檔不一樣，池裡🚫 去重成同一個）。
    fn body_of(file: usize) -> Vec<u8> {
        (0..SIZE)
            .map(|position| ((position * 7 + file * 13) % 251) as u8)
            .collect()
    }

    fn body() -> Vec<u8> {
        body_of(0)
    }

    /// 一個帳號、`Upload` 與 `Download` 兩條記憶體線；傳一個 200 byte、16 byte 一塊的檔（13 塊），下載那台拿到同一份資料。
    async fn uploaded(name: &str) -> (Core, AccountDir, FakeServer, Manifest) {
        let (core, account, server, mut manifests, _upload_server) = uploaded_many(name, 1).await;
        let manifest = manifests.remove(0);
        (core, account, server, manifest)
    }

    /// 同上，傳 `count` 個檔（第 0 個叫 `v.bin`，其他 `v<n>.bin`）。最後一個是上傳那台假 server（之後再傳的檔要自己抄到下載那台）。
    async fn uploaded_many(
        name: &str,
        count: usize,
    ) -> (Core, AccountDir, FakeServer, Vec<Manifest>, FakeServer) {
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
        let target = Target::default();
        let mut manifests = Vec::new();
        for file in 0..count {
            let request = NewUpload {
                room: None,
                name: match file {
                    0 => "v.bin".to_string(),
                    _ => format!("v{file}.bin"),
                },
                size: Some(SIZE as u64),
                mimetype: Some("video/mp4".to_string()),
                chunk_size: Some(CHUNK),
                ..NewUpload::default()
            };
            let state = core.create_upload(&request, &target).await.unwrap();
            let plain = body_of(file);
            manifests.push(
                core.receive_upload(&state, &mut &plain[..], None, &target)
                    .await
                    .unwrap(),
            );
        }
        *download_server.uploads.lock().unwrap() = upload_server.uploads.lock().unwrap().clone();
        (core, account, download_server, manifests, upload_server)
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

    /// server 處理過的 `Read` 的塊號（照順序；只有一個檔的測試用）。
    fn reads(server: &FakeServer) -> Vec<u32> {
        server
            .download_reads
            .lock()
            .unwrap()
            .iter()
            .map(|(_, index)| *index)
            .collect()
    }

    /// `to` 旁邊沒有任何匯出暫存檔（`<to>.partial.<pid>-<n>`）。
    fn no_partials_left(to: &std::path::Path) -> bool {
        let Some(name) = to
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
        else {
            return false;
        };
        let prefix = format!("{name}.partial");
        std::fs::read_dir(to.parent().unwrap())
            .unwrap()
            .all(|entry| {
                !entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(&prefix)
            })
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
        // 第 0 塊的 Read 卡在 server：這個檔已經在 `downloading` 表裡了。
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
        // 播放器 seek 到第 5 塊：它的 Read 插到發送 queue 最前面、馬上送出（🚫 等第 0 塊回來）。
        // server 照到達順序一次處理一個，所以放一個是第 0 塊、再放一個就是第 5 塊——主檔的第 1 塊排在它後面。
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
        // 放行。同一塊再要一次：從暫存檔；主檔追到第 5 塊時也從暫存檔搬——第 5 塊整個下載只上網一次（最後的 reads 斷言）。
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
    async fn export_waits_for_the_queue_and_no_cache_leaves_nothing_in_the_pool() {
        let (core, account, _server, manifest) = uploaded("dq-export").await;
        let out = scratch("dq-export-out").join("v.bin");
        let media = MediaRef::Manifest(manifest.clone());
        let exported = core
            .export_media_to(&media, &out, true, &Target::default())
            .await
            .unwrap();
        assert_eq!(exported.source, "server");
        assert_eq!(exported.bytes, SIZE as u64);
        assert_eq!(std::fs::read(&out).unwrap(), body());
        assert!(
            no_partials_left(&out),
            "the partial file is renamed into place"
        );
        let pool = core.pool_of(&account).unwrap();
        assert!(
            pool.list_files().unwrap().is_empty(),
            "no_cache keeps nothing"
        );
        // 沒有 no_cache：留在池裡，下一次匯出直接從池拿。
        core.export_media_to(&media, &out, false, &Target::default())
            .await
            .unwrap();
        let again = core
            .export_media_to(&media, &out, false, &Target::default())
            .await
            .unwrap();
        assert_eq!(again.source, "cache");
        assert_eq!(pool.list_files().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn an_export_checks_the_whole_file_and_drops_a_pool_copy_that_does_not_match() {
        let (core, account, _server, manifests, _upload_server) =
            uploaded_many("dq-export-bad", 2).await;
        let (first, second) = (manifests[0].clone(), manifests[1].clone());
        let target = Target::default();
        let out = scratch("dq-export-bad-out").join("v.bin");
        for manifest in [&first, &second] {
            core.export_media_to(&MediaRef::Manifest(manifest.clone()), &out, false, &target)
                .await
                .unwrap();
        }
        // 第一個檔的列改指第二個檔的池檔（同樣大小、能打開、內容不對）：串流照用，匯出要擋下。
        let (cache, _) = core.server_cache_and_me(&account).unwrap();
        let reader = cache.read().await;
        let second_entry = reader.find_media(&second.mxc).unwrap().unwrap();
        drop(reader);
        let (mxc, pool_file, segments, bytes) = (
            first.mxc.clone(),
            second_entry.pool_file.clone().unwrap(),
            second_entry.segments_written,
            second_entry.bytes_on_disk,
        );
        cache
            .run(move |cache| cache.media_finish(&mxc, &pool_file, segments, SIZE as u64, bytes))
            .await
            .unwrap();
        let _ = std::fs::remove_file(&out);
        let error = core
            .export_media_to(&MediaRef::Manifest(first.clone()), &out, false, &target)
            .await
            .unwrap_err();
        assert_eq!(error.kind, CoreErrorKind::Integrity, "{error:?}");
        assert!(!out.exists(), "nothing lands at the destination");
        assert!(no_partials_left(&out));
        let entry = cache.read().await.find_media(&first.mxc).unwrap().unwrap();
        assert!(!entry.complete, "the bad copy is dropped from the cache");
        // 下一次就重下，拿到對的。
        let exported = core
            .export_media_to(&MediaRef::Manifest(first.clone()), &out, false, &target)
            .await
            .unwrap();
        assert_eq!(exported.source, "server");
        assert_eq!(std::fs::read(&out).unwrap(), body_of(0));
    }

    #[tokio::test]
    async fn an_export_uses_the_local_original_only_while_it_still_matches() {
        let (core, _account, download_server, _manifests, upload_server) =
            uploaded_many("dq-export-local", 1).await;
        // 再傳一個檔，這次 UI 給了原檔的位置。
        let original = scratch("dq-export-local-original").join("o.bin");
        let plain = body_of(5);
        std::fs::write(&original, &plain).unwrap();
        let uri = format!(
            "file:///{}",
            original
                .display()
                .to_string()
                .replace('\\', "/")
                .trim_start_matches('/')
        );
        let target = Target::default();
        let request = NewUpload {
            room: None,
            name: "o.bin".to_string(),
            size: Some(SIZE as u64),
            mimetype: None,
            chunk_size: Some(CHUNK),
            ..NewUpload::default()
        };
        let state = core.create_upload(&request, &target).await.unwrap();
        let manifest = core
            .receive_upload(&state, &mut &plain[..], Some(&uri), &target)
            .await
            .unwrap();
        // 下載那台的假 server 也要有這個檔（原檔驗不過時要從它拉）。
        *download_server.uploads.lock().unwrap() = upload_server.uploads.lock().unwrap().clone();
        let out = scratch("dq-export-local-out").join("o.bin");
        let media = MediaRef::Manifest(manifest.clone());
        let exported = core
            .export_media_to(&media, &out, false, &target)
            .await
            .unwrap();
        assert_eq!(exported.source, "local_source");
        assert_eq!(std::fs::read(&out).unwrap(), plain);
        // 原檔被改了、大小沒變：sha256 對不上，改從池（這裡是 server）匯出，內容還是對的。
        let mut changed = plain.clone();
        changed[0] ^= 1;
        std::fs::write(&original, &changed).unwrap();
        let exported = core
            .export_media_to(&media, &out, false, &target)
            .await
            .unwrap();
        assert_eq!(exported.source, "server");
        assert_eq!(std::fs::read(&out).unwrap(), plain);
    }

    #[tokio::test]
    async fn a_wrong_description_never_deletes_a_complete_verified_copy() {
        let (core, account, server, manifest) = uploaded("dq-liar-complete").await;
        let mut events = core.subscribe();
        let target = Target::default();
        let media = MediaRef::Manifest(manifest.clone());
        core.media_download(&media, &target).await.unwrap();
        wait_until(&mut events, &manifest.mxc, DownloadState::Complete).await;
        // 另一則訊息引用同一個 mxc，卻說大小不一樣（寫錯或偽造）：錯的是它，快取🚫 動（維護者 2026-10-03）。
        let mut liar = manifest.clone();
        liar.block.file_size = Some(SIZE as u64 + 16);
        let liar = MediaRef::Manifest(liar);
        for result in [
            core.media_download(&liar, &target).await.map(|_| ()),
            core.media_open(&liar, &target).await.map(|_| ()),
            core.export_media_to(&liar, &scratch("dq-liar-out").join("x"), false, &target)
                .await
                .map(|_| ()),
        ] {
            assert_eq!(
                result.unwrap_err().kind,
                CoreErrorKind::Integrity,
                "the wrong description is refused"
            );
        }
        let (cache, _) = core.server_cache_and_me(&account).unwrap();
        let entry = cache
            .read()
            .await
            .find_media(&manifest.mxc)
            .unwrap()
            .unwrap();
        assert!(
            entry.complete && entry.file_size == SIZE as u64,
            "{entry:?}"
        );
        assert_eq!(read_whole(&core, &manifest.mxc).await, body());
        assert_eq!(
            reads(&server).len(),
            CHUNKS as usize,
            "nothing was downloaded again"
        );
    }

    #[tokio::test]
    async fn a_different_size_drops_an_unfinished_copy_and_starts_over() {
        let (core, account, server, manifest) = uploaded("dq-liar-unfinished").await;
        let mut events = core.subscribe();
        let target = Target::default();
        let media = MediaRef::Manifest(manifest.clone());
        // 下載到一半就取消：列還沒完成、主檔半成品留著。
        hold_reads(&server).await;
        let downloader = core.downloader_of(&account).await.unwrap();
        downloader.enqueue(Arc::new(manifest.clone()), None);
        wait_until_the_first_read_is_out(&downloader).await;
        assert!(downloader.cancel(&manifest.mxc));
        server.read_permits.add_permits(READ_PERMITS as usize);
        wait_until(&mut events, &manifest.mxc, DownloadState::Cancelled).await;
        // 兩份描述都還沒被整檔驗過：照 PR #14 的規則丟掉、照這份從頭來（這份是假的，所以下載失敗）。
        let mut liar = manifest.clone();
        liar.block.file_size = Some(SIZE as u64 + 16);
        let job = core
            .media_download(&MediaRef::Manifest(liar), &target)
            .await
            .unwrap();
        assert_ne!(job.state, DownloadState::Complete);
        wait_until(&mut events, &manifest.mxc, DownloadState::Failed).await;
        // 原本那份再來一次：又對不上（列現在是假的那份）、也還沒完成，再丟一次、重下，拿到對的。
        core.media_download(&media, &target).await.unwrap();
        wait_until(&mut events, &manifest.mxc, DownloadState::Complete).await;
        assert_eq!(read_whole(&core, &manifest.mxc).await, body());
    }

    #[tokio::test]
    async fn a_server_error_that_is_not_a_refusal_keeps_the_unfinished_file() {
        let (core, account, server, manifest) = uploaded("dq-server-internal").await;
        let mut events = core.subscribe();
        let target = Target::default();
        let media = MediaRef::Manifest(manifest.clone());
        // server 內部錯（1901）：這個檔停下、標失敗，但主檔半成品🚫 刪（§5.4）；再要一次就接著拉完。
        *server.fail_next_read.lock().unwrap() = Some(("Internal", 1901));
        core.media_download(&media, &target).await.unwrap();
        wait_until(&mut events, &manifest.mxc, DownloadState::Failed).await;
        let pool = core.pool_of(&account).unwrap();
        assert_eq!(
            pool.list_pending().unwrap().len(),
            1,
            "the unfinished file stays"
        );
        core.media_download(&media, &target).await.unwrap();
        wait_until(&mut events, &manifest.mxc, DownloadState::Complete).await;
        assert_eq!(read_whole(&core, &manifest.mxc).await, body());
    }

    #[tokio::test]
    async fn not_found_from_the_server_deletes_the_unfinished_file() {
        let (core, account, server, manifest) = uploaded("dq-server-notfound").await;
        let mut events = core.subscribe();
        // server 說這個檔不在（1501）：主檔拿不到了，半成品刪掉、列 reset。
        *server.fail_next_read.lock().unwrap() = Some(("NotFound", 1501));
        core.media_download(&MediaRef::Manifest(manifest.clone()), &Target::default())
            .await
            .unwrap();
        wait_until(&mut events, &manifest.mxc, DownloadState::Failed).await;
        let pool = core.pool_of(&account).unwrap();
        wait_for_async(
            || async { pool.list_pending().unwrap().is_empty() },
            "the unfinished file to be deleted",
        )
        .await;
    }

    #[tokio::test]
    async fn a_timeout_does_not_use_up_the_retry_for_a_bad_chunk() {
        let (core, account, server, manifest) = uploaded("dq-timeout-then-bad").await;
        let mut events = core.subscribe();
        hold_reads(&server).await;
        // 自己起一個線上沉默時限很短的處理端（正式是 60 秒）。
        let (cache, me) = core.server_cache_and_me(&account).unwrap();
        let downloader = Downloader::start(DownloaderParts {
            user: me,
            server_dir: account.server_dir(),
            links: core.pool_of_account(&account).unwrap(),
            media_pool: core.pool_of(&account).unwrap(),
            cache,
            events: core.events.clone(),
            claims: core.media_claims.clone(),
            line_silence: Duration::from_millis(500),
        });
        downloader.enqueue(Arc::new(manifest.clone()), None);
        // 第 0 塊卡在 server：線整段沒回應 → 逾時一次、重送。
        tokio::time::sleep(Duration::from_millis(750)).await;
        // 放行：原本那份（已經沒人等）與重送那份的回覆都弄壞。重送那份是這一塊**第一次**壞：要再拉一次，🚫 當壞檔。
        server
            .corrupt_reads
            .store(2, std::sync::atomic::Ordering::SeqCst);
        server.read_permits.add_permits(READ_PERMITS as usize);
        wait_until(&mut events, &manifest.mxc, DownloadState::Complete).await;
        assert_eq!(reads(&server)[..3], [0, 0, 0]);
        assert_eq!(read_whole(&core, &manifest.mxc).await, body());
    }

    #[tokio::test]
    async fn another_account_writing_the_same_file_makes_this_job_wait() {
        let (core, account, server, manifest) = uploaded("dq-claim").await;
        let mut events = core.subscribe();
        let server_dir = account.server_dir();
        let someone_else = core.media_claims.new_holder("@someone-else:localhost");
        assert!(core.media_claims.claim(
            &server_dir,
            &manifest.mxc,
            &someone_else,
            ClaimFor::Download
        ));
        core.media_download(&MediaRef::Manifest(manifest.clone()), &Target::default())
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            reads(&server).is_empty(),
            "the job waits while another account writes the file"
        );
        core.media_claims
            .release(&server_dir, &manifest.mxc, &someone_else);
        wait_until(&mut events, &manifest.mxc, DownloadState::Complete).await;
        assert_eq!(
            core.media_claims.find_holder(&server_dir, &manifest.mxc),
            None
        );
    }

    #[tokio::test]
    async fn stopping_the_downloader_releases_what_it_was_writing() {
        let (core, account, server, manifest) = uploaded("dq-stop").await;
        hold_reads(&server).await;
        // 處理端先起好（第一次起會用唯一寫入者掃一次池），再讓唯一寫入者忙一下：
        // 處理端認領之後卡在建列那一步（還不在 `open` 裡），這時收掉它也要放掉認領。
        core.downloader_of(&account).await.unwrap();
        let (cache, _) = core.server_cache_and_me(&account).unwrap();
        cache.post(
            |_| {
                std::thread::sleep(Duration::from_millis(500));
                Ok(())
            },
            Vec::new(),
        );
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
            "the downloader to claim the file",
        )
        .await;
        core.stop_downloader_of(&account);
        wait_for_async(
            || async {
                core.media_claims
                    .find_holder(&server_dir, &manifest.mxc)
                    .is_none()
            },
            "the aborted downloader to release its claim",
        )
        .await;
        server.read_permits.add_permits(READ_PERMITS as usize);
    }

    #[tokio::test]
    async fn three_files_run_together_with_one_chunk_each_in_flight() {
        let (core, account, server, manifests, _upload_server) = uploaded_many("dq-abc", 3).await;
        let downloader = core.downloader_of(&account).await.unwrap();
        // 三個 job 一起交進去（中間🚫 await）：處理端一次處理掉，三個檔一起跑。
        for manifest in &manifests {
            downloader.enqueue(Arc::new(manifest.clone()), None);
        }
        wait_for_async(
            || async { downloader.list_jobs().is_empty() },
            "all three files to finish",
        )
        .await;
        // 每個檔同時一塊在途：第 n 塊落地才要 n+1，所以 server 看到的是 A0 B0 C0、A1 B1 C1、…（/docs/design/media/media-download.md §5.1）。
        let expected: Vec<(String, u32)> = (0..CHUNKS)
            .flat_map(|index| {
                manifests
                    .iter()
                    .map(move |manifest| (manifest.mxc.clone(), index))
            })
            .collect();
        assert_eq!(*server.download_reads.lock().unwrap(), expected);
        for (file, manifest) in manifests.iter().enumerate() {
            assert_eq!(read_whole(&core, &manifest.mxc).await, body_of(file));
        }
    }

    #[tokio::test]
    async fn a_dropped_link_keeps_the_progress_and_the_download_goes_on_when_it_is_back() {
        let (core, account, server, manifest) = uploaded("dq-relink").await;
        let mut events = core.subscribe();
        hold_reads(&server).await;
        core.media_download(&MediaRef::Manifest(manifest.clone()), &Target::default())
            .await
            .unwrap();
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
        // 第 0 塊的 Read 送出去了（server 卡著），然後線斷了：在途的請求拿到 Network，排回發送 queue 等線。
        tokio::time::sleep(Duration::from_millis(100)).await;
        let links = core.pool_of_account(&account).unwrap();
        assert!(links.close(LinkRole::Download, "test").await);
        tokio::time::sleep(Duration::from_millis(300)).await;
        let queue = core.media_queue(&Target::default()).await.unwrap();
        assert_eq!(queue.len(), 1, "{queue:?}");
        assert_eq!(
            (queue[0].state, queue[0].done),
            (DownloadState::Downloading, 0),
            "the progress waits where it was"
        );
        // 線回來（新的連線、同一份資料）：從第 0 塊接著拉完，每一塊只在新線上要一次。
        let (client, new_server) = memory_client_with_hello(Arc::new(Mutex::new(Vec::new()))).await;
        *new_server.uploads.lock().unwrap() = server.uploads.lock().unwrap().clone();
        drop(
            links
                .acquire(LinkRole::Download, || async move { Ok(client) })
                .await
                .unwrap(),
        );
        wait_until(&mut events, &manifest.mxc, DownloadState::Complete).await;
        assert_eq!(reads(&new_server), (0..CHUNKS).collect::<Vec<_>>());
        assert_eq!(read_whole(&core, &manifest.mxc).await, body());
        server.read_permits.add_permits(READ_PERMITS as usize);
    }

    /// 第 0 塊的 Read 已經送到 server（卡著）的時候，等 `downloading` 出現之後再等一下。
    async fn wait_until_the_first_read_is_out(downloader: &Downloader) {
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
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    #[tokio::test]
    async fn a_seek_for_the_chunk_on_the_wire_rides_on_it() {
        let (core, account, server, manifest) = uploaded("dq-ride").await;
        let mut events = core.subscribe();
        hold_reads(&server).await;
        let downloader = core.downloader_of(&account).await.unwrap();
        downloader.enqueue(Arc::new(manifest.clone()), None);
        wait_until_the_first_read_is_out(&downloader).await;
        // 主檔的第 0 塊在途，GET 也要第 0 塊：掛上去，🚫 再送一次（/docs/design/media/media-download.md §6.3）。
        let seek = {
            let downloader = downloader.clone();
            let manifest = Arc::new(manifest.clone());
            tokio::spawn(async move { downloader.read_chunk(manifest, 0).await })
        };
        tokio::time::sleep(Duration::from_millis(50)).await;
        server.read_permits.add_permits(READ_PERMITS as usize);
        assert_eq!(&seek.await.unwrap().unwrap()[..], &body()[..16]);
        wait_until(&mut events, &manifest.mxc, DownloadState::Complete).await;
        assert_eq!(reads(&server), (0..CHUNKS).collect::<Vec<_>>());
        assert_eq!(read_whole(&core, &manifest.mxc).await, body());
    }

    #[tokio::test]
    async fn a_cancel_while_the_link_is_down_ends_at_once() {
        let (core, account, server, manifest) = uploaded("dq-cancel-down").await;
        let mut events = core.subscribe();
        let links = core.pool_of_account(&account).unwrap();
        assert!(links.close(LinkRole::Download, "test").await);
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
        // 它的請求還排在發送 queue 裡（沒有線）：取消就當場拿掉、當場停，🚫 等線回來。
        assert!(downloader.cancel(&manifest.mxc));
        wait_until(&mut events, &manifest.mxc, DownloadState::Cancelled).await;
        assert!(downloader.list_jobs().is_empty());
        assert!(reads(&server).is_empty());
    }

    #[tokio::test]
    async fn a_seek_made_while_the_link_is_down_goes_out_first_when_it_is_back() {
        let (core, account, server, manifest) = uploaded("dq-seek-relink").await;
        let mut events = core.subscribe();
        hold_reads(&server).await;
        let downloader = core.downloader_of(&account).await.unwrap();
        downloader.enqueue(Arc::new(manifest.clone()), None);
        wait_until_the_first_read_is_out(&downloader).await;
        // 線斷了：主檔的第 0 塊排回發送 queue。這時播放器 seek 到第 5 塊：它排在第 0 塊前面。
        let links = core.pool_of_account(&account).unwrap();
        assert!(links.close(LinkRole::Download, "test").await);
        tokio::time::sleep(Duration::from_millis(100)).await;
        let seek = {
            let downloader = downloader.clone();
            let manifest = Arc::new(manifest.clone());
            tokio::spawn(async move { downloader.read_chunk(manifest, 5).await })
        };
        tokio::time::sleep(Duration::from_millis(100)).await;
        let (client, new_server) = memory_client_with_hello(Arc::new(Mutex::new(Vec::new()))).await;
        *new_server.uploads.lock().unwrap() = server.uploads.lock().unwrap().clone();
        drop(
            links
                .acquire(LinkRole::Download, || async move { Ok(client) })
                .await
                .unwrap(),
        );
        assert_eq!(&seek.await.unwrap().unwrap()[..], &body()[80..96]);
        wait_until(&mut events, &manifest.mxc, DownloadState::Complete).await;
        let mut expected: Vec<u32> = vec![5];
        expected.extend((0..CHUNKS).filter(|index| *index != 5));
        assert_eq!(reads(&new_server), expected);
        server.read_permits.add_permits(READ_PERMITS as usize);
    }

    #[tokio::test]
    async fn a_chunk_that_does_not_open_is_fetched_once_more() {
        let (core, _account, server, manifest) = uploaded("dq-corrupt-once").await;
        let mut events = core.subscribe();
        server
            .corrupt_reads
            .store(1, std::sync::atomic::Ordering::SeqCst);
        core.media_download(&MediaRef::Manifest(manifest.clone()), &Target::default())
            .await
            .unwrap();
        wait_until(&mut events, &manifest.mxc, DownloadState::Complete).await;
        let mut expected: Vec<u32> = vec![0];
        expected.extend(0..CHUNKS);
        assert_eq!(reads(&server), expected);
        assert_eq!(read_whole(&core, &manifest.mxc).await, body());
    }

    #[tokio::test]
    async fn a_chunk_that_does_not_open_twice_makes_the_file_bad_and_leaves_nothing() {
        let (core, account, server, manifest) = uploaded("dq-corrupt-twice").await;
        let mut events = core.subscribe();
        server
            .corrupt_reads
            .store(2, std::sync::atomic::Ordering::SeqCst);
        core.media_download(&MediaRef::Manifest(manifest.clone()), &Target::default())
            .await
            .unwrap();
        wait_until(&mut events, &manifest.mxc, DownloadState::Failed).await;
        assert_eq!(reads(&server), vec![0, 0]);
        let pool = core.pool_of(&account).unwrap();
        assert!(
            pool.list_pending().unwrap().is_empty(),
            "a bad file leaves no pending file behind"
        );
    }

    #[tokio::test]
    async fn a_job_being_prepared_can_be_cancelled() {
        let (core, account, server, manifest) = uploaded("dq-cancel-preparing").await;
        let mut events = core.subscribe();
        let downloader = core.downloader_of(&account).await.unwrap();
        // 讓唯一寫入者忙 500 ms：處理端建列那一步排在它後面，這個 job 停在「準備中」（已經不在收件 queue、還沒進 downloading 表）。
        let (cache, _) = core.server_cache_and_me(&account).unwrap();
        cache.post(
            |_| {
                std::thread::sleep(Duration::from_millis(500));
                Ok(())
            },
            Vec::new(),
        );
        downloader.enqueue(Arc::new(manifest.clone()), None);
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            downloader
                .list_jobs()
                .first()
                .map(|(mxc, _, status)| (mxc.clone(), status.state)),
            Some((manifest.mxc.clone(), DownloadState::Queued)),
            "the job being prepared still shows up"
        );
        assert!(downloader.cancel(&manifest.mxc), "and cancel finds it");
        wait_until(&mut events, &manifest.mxc, DownloadState::Cancelled).await;
        assert!(downloader.list_jobs().is_empty());
        assert!(reads(&server).is_empty());
    }

    /// 同一台 server 上「另一個帳號」的處理端：自己的線（另一台假 server，資料抄同一份）、同一個池與 cache.db、同一張認領表。
    async fn start_another_account(
        core: &Core,
        account: &AccountDir,
        server: &FakeServer,
        user: &str,
    ) -> (Downloader, FakeServer) {
        let links = Arc::new(LinkPool::new(user, core.events.clone()));
        let (client, other_server) =
            memory_client_with_hello(Arc::new(Mutex::new(Vec::new()))).await;
        *other_server.uploads.lock().unwrap() = server.uploads.lock().unwrap().clone();
        drop(
            links
                .acquire(LinkRole::Download, || async move { Ok(client) })
                .await
                .unwrap(),
        );
        let (cache, _) = core.server_cache_and_me(account).unwrap();
        let downloader = Downloader::start(DownloaderParts {
            user: user.to_string(),
            server_dir: account.server_dir(),
            links,
            media_pool: core.pool_of(account).unwrap(),
            cache,
            events: core.events.clone(),
            claims: core.media_claims.clone(),
            line_silence: LINE_SILENCE,
        });
        (downloader, other_server)
    }

    /// 把一則帶附件的訊息當成 `user` 同步進來的存進快取（GET 從這裡拿描述）。
    async fn store_file_event(
        core: &Core,
        account: &AccountDir,
        user: &str,
        event_id: &str,
        mxc: &str,
        block: &wbf_sdk::ChunkedBlock,
    ) {
        let (cache, _) = core.server_cache_and_me(account).unwrap();
        let event = serde_json::json!({
            "type": "m.room.message", "event_id": event_id, "room_id": "!r:localhost",
            "sender": "@carol:localhost", "origin_server_ts": 1000,
            "content": {
                "msgtype": wbf_sdk::event_json::FILE_MSGTYPE, "body": "v.bin", "url": mxc,
                wbf_sdk::event_json::CHUNKED_BLOCK_KEY: block,
            },
        });
        let user = user.to_string();
        cache
            .run(move |cache| {
                cache
                    .upsert_events(
                        &user,
                        "!r:localhost",
                        &[wbf_sdk::incoming::IncomingEvent::Plain { event }],
                    )
                    .map(|_| ())
            })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn a_get_uses_only_a_description_that_matches_the_local_record() {
        let (core, account, _server, manifests, _upload_server) =
            uploaded_many("dq-get-describe", 2).await;
        let (_, me) = core.server_cache_and_me(&account).unwrap();
        let (first, second) = (&manifests[0], &manifests[1]);
        // 第一個檔：本地那一列由同 server 另一個帳號的事件建（塊 16 byte）；這個帳號看到的那則把塊大小寫成 32。
        store_file_event(
            &core,
            &account,
            "@bob:localhost",
            "$right",
            &first.mxc,
            &first.block,
        )
        .await;
        let mut cut_wrong = first.block.clone();
        cut_wrong.chunk_size = CHUNK * 2;
        store_file_event(&core, &account, &me, "$wrong", &first.mxc, &cut_wrong).await;
        let error = core
            .find_media_source(&first.mxc)
            .await
            .err()
            .expect("a description that does not match the record is not used");
        assert_eq!(error.kind, CoreErrorKind::Integrity, "{error:?}");
        // 第二個檔：描述對得上，照常一塊一塊跟處理端拿。
        store_file_event(&core, &account, &me, "$second", &second.mxc, &second.block).await;
        assert_eq!(read_whole(&core, &second.mxc).await, body_of(1));
    }

    #[tokio::test]
    async fn a_seek_with_a_description_cut_differently_gets_an_error_not_other_bytes() {
        let (core, account, server, manifest) = uploaded("dq-seek-cut").await;
        let mut events = core.subscribe();
        let downloader = core.downloader_of(&account).await.unwrap();
        let mut cut_wrong = manifest.clone();
        cut_wrong.block.chunk_size = CHUNK * 2;
        let cut_wrong = Arc::new(cut_wrong);
        // 檔開著（下載中，第 0 塊卡在 server）：開著那份的切法跟 GET 那份不一樣，回錯，🚫 掛上去拿一塊別的大小的。
        hold_reads(&server).await;
        downloader.enqueue(Arc::new(manifest.clone()), None);
        wait_until_the_first_read_is_out(&downloader).await;
        let error = tokio::time::timeout(
            Duration::from_secs(5),
            downloader.read_chunk(cut_wrong.clone(), 0),
        )
        .await
        .expect("answered at once")
        .unwrap_err();
        assert_eq!(error.kind, CoreErrorKind::Integrity, "{error:?}");
        server.read_permits.add_permits(READ_PERMITS as usize);
        wait_until(&mut events, &manifest.mxc, DownloadState::Complete).await;
        // 檔完整了：從池檔給也一樣看切法。
        let error = downloader.read_chunk(cut_wrong, 1).await.unwrap_err();
        assert_eq!(error.kind, CoreErrorKind::Integrity, "{error:?}");
        let right = downloader
            .read_chunk(Arc::new(manifest.clone()), 1)
            .await
            .unwrap();
        assert_eq!(&right[..], &body()[16..32]);
    }

    #[tokio::test]
    async fn a_seek_whose_description_fails_does_not_delete_the_unfinished_file() {
        let (core, account, server, manifest) = uploaded("dq-seek-bad-key").await;
        let mut events = core.subscribe();
        // 先拉一塊就取消：主檔留著（再要一次從斷點接）。
        hold_reads(&server).await;
        let downloader = core.downloader_of(&account).await.unwrap();
        downloader.enqueue(Arc::new(manifest.clone()), None);
        wait_until_the_first_read_is_out(&downloader).await;
        assert!(downloader.cancel(&manifest.mxc));
        server.read_permits.add_permits(READ_PERMITS as usize);
        wait_until(&mut events, &manifest.mxc, DownloadState::Cancelled).await;
        let pool = core.pool_of(&account).unwrap();
        assert_eq!(pool.list_pending().unwrap().len(), 1);
        // 一則金鑰寫錯（或偽造）的事件，播放器拿它 seek：`Info` 驗不過。可疑的是那份描述，🚫 刪別人拉到一半的主檔。
        let mut wrong_key = manifest.clone();
        wrong_key.block.key = Some([7u8; 32]);
        let error = downloader
            .read_chunk(Arc::new(wrong_key), 5)
            .await
            .unwrap_err();
        assert_eq!(error.kind, CoreErrorKind::Integrity, "{error:?}");
        assert_eq!(
            pool.list_pending().unwrap().len(),
            1,
            "the unfinished file is kept"
        );
        downloader.enqueue(Arc::new(manifest.clone()), None);
        wait_until(&mut events, &manifest.mxc, DownloadState::Complete).await;
        assert_eq!(read_whole(&core, &manifest.mxc).await, body());
    }

    #[tokio::test]
    async fn a_job_reopens_a_file_opened_for_seek_with_another_description() {
        let (core, account, server, manifest) = uploaded("dq-reopen").await;
        let mut events = core.subscribe();
        // 線斷著：seek 的 `Info` 排著送不出去。這個 seek 帶的是金鑰寫錯的描述，檔只為它開著。
        let links = core.pool_of_account(&account).unwrap();
        assert!(links.close(LinkRole::Download, "test").await);
        let downloader = core.downloader_of(&account).await.unwrap();
        let mut wrong_key = manifest.clone();
        wrong_key.block.key = Some([7u8; 32]);
        let seek = {
            let downloader = downloader.clone();
            tokio::spawn(async move { downloader.read_chunk(Arc::new(wrong_key), 5).await })
        };
        let server_dir = account.server_dir();
        wait_for_async(
            || async {
                core.media_claims
                    .find_holder(&server_dir, &manifest.mxc)
                    .is_some()
            },
            "the seek to open the file",
        )
        .await;
        // job 來了：用它自己那份描述重開，🚫 接著用那份寫錯的（不然 `Info` 驗不過時會被當成壞檔、整個刪掉）。
        downloader.enqueue(Arc::new(manifest.clone()), None);
        let answer = tokio::time::timeout(Duration::from_secs(5), seek)
            .await
            .expect("the seek is told the file was reopened")
            .unwrap();
        assert!(answer.is_err());
        let (client, new_server) = memory_client_with_hello(Arc::new(Mutex::new(Vec::new()))).await;
        *new_server.uploads.lock().unwrap() = server.uploads.lock().unwrap().clone();
        drop(
            links
                .acquire(LinkRole::Download, || async move { Ok(client) })
                .await
                .unwrap(),
        );
        wait_until(&mut events, &manifest.mxc, DownloadState::Complete).await;
        assert_eq!(read_whole(&core, &manifest.mxc).await, body());
    }

    #[tokio::test]
    async fn a_file_open_only_for_seek_lets_go_when_another_account_downloads_it() {
        let (core, account, server, manifest) = uploaded("dq-yield").await;
        let mut events = core.subscribe();
        let alice = core.downloader_of(&account).await.unwrap();
        // 播放器 seek 了一塊：這個處理端只為 seek 開著這個檔、握著認領。
        let chunk = alice
            .read_chunk(Arc::new(manifest.clone()), 5)
            .await
            .unwrap();
        assert_eq!(&chunk[..], &body()[80..96]);
        let server_dir = account.server_dir();
        assert!(core
            .media_claims
            .find_holder(&server_dir, &manifest.mxc)
            .is_some());
        // 同一台 server 的另一個帳號要整檔：它認領不到、留下記號，只為 seek 開著的那邊就讓出來——🚫 等它湊滿 `SEEK_ONLY_OPEN` 個才關。
        let (bob, bob_server) =
            start_another_account(&core, &account, &server, "@bob:localhost").await;
        bob.enqueue(Arc::new(manifest.clone()), None);
        wait_until(&mut events, &manifest.mxc, DownloadState::Complete).await;
        // 它接手同一個主檔與 seek 暫存檔：第 5 塊從暫存檔搬，🚫 再拉。
        let bob_reads: Vec<u32> = bob_server
            .download_reads
            .lock()
            .unwrap()
            .iter()
            .map(|(_, index)| *index)
            .collect();
        assert_eq!(
            bob_reads,
            (0..CHUNKS).filter(|index| *index != 5).collect::<Vec<_>>()
        );
        // 讓出來之後 seek 照樣拿得到（現在從完整的池檔）。
        let chunk = alice
            .read_chunk(Arc::new(manifest.clone()), 6)
            .await
            .unwrap();
        assert_eq!(&chunk[..], &body()[96..112]);
    }

    #[tokio::test]
    async fn a_stopped_downloader_ends_the_read_a_stream_was_waiting_for() {
        let (core, account, server, manifest) = uploaded("dq-stop-stream").await;
        hold_reads(&server).await;
        let downloader = core.downloader_of(&account).await.unwrap();
        // 串流中的 GET 握著處理端、等一塊（server 卡著）。登出時收掉處理端：GET 要馬上拿到錯，🚫 跟著舊處理端一直等。
        let read = {
            let downloader = downloader.clone();
            let manifest = Arc::new(manifest.clone());
            tokio::spawn(async move { downloader.read_chunk(manifest, 3).await })
        };
        let server_dir = account.server_dir();
        wait_for_async(
            || async {
                core.media_claims
                    .find_holder(&server_dir, &manifest.mxc)
                    .is_some()
            },
            "the seek to open the file",
        )
        .await;
        core.stop_downloader_of(&account);
        let answer = tokio::time::timeout(Duration::from_secs(5), read)
            .await
            .expect("the read ends when the downloader is stopped")
            .unwrap();
        assert!(answer.is_err());
        wait_for_async(
            || async {
                core.media_claims
                    .find_holder(&server_dir, &manifest.mxc)
                    .is_none()
            },
            "the stopped downloader to let go of the file",
        )
        .await;
        server.read_permits.add_permits(READ_PERMITS as usize);
    }

    #[test]
    fn a_claim_belongs_to_one_handler_until_it_lets_go() {
        let claims = MediaClaims::default();
        let dir = std::path::PathBuf::from("s1");
        let mxc = "mxc://a/1";
        let alice = claims.new_holder("@a:x");
        // 同一個帳號重登之後的新處理端（舊的還被串流中的 GET 留著）
        let alice_again = claims.new_holder("@a:x");
        let bob = claims.new_holder("@b:x");
        assert!(claims.claim(&dir, mxc, &alice, ClaimFor::Seek));
        assert!(
            claims.claim(&dir, mxc, &alice, ClaimFor::Seek),
            "claiming again is fine"
        );
        assert!(
            !claims.claim(&dir, mxc, &alice_again, ClaimFor::Download),
            "the same account's other handler does not share it"
        );
        assert!(
            claims.claim(
                &std::path::PathBuf::from("s2"),
                mxc,
                &bob,
                ClaimFor::Download
            ),
            "another server dir is another file"
        );
        claims.release(&dir, mxc, &alice_again);
        assert_eq!(
            claims.find_holder(&dir, mxc).as_deref(),
            Some("@a:x"),
            "only the handler that holds it lets go"
        );
        assert!(
            claims.is_wanted_by_another(&dir, mxc, &alice),
            "the job that could not claim it left a mark"
        );
        assert!(!claims.is_wanted_by_another(&dir, mxc, &alice_again));
        claims.release(&dir, mxc, &alice);
        assert!(
            !claims.claim(&dir, mxc, &bob, ClaimFor::Seek),
            "a seek does not take what a job is waiting for"
        );
        assert!(
            claims.claim(&dir, mxc, &alice_again, ClaimFor::Download),
            "the job that waited gets it"
        );
        assert!(
            !claims.is_wanted_by_another(&dir, mxc, &bob),
            "and its mark is gone"
        );
        claims.release(&dir, mxc, &alice);
        assert!(
            !claims.claim(&dir, mxc, &bob, ClaimFor::Seek),
            "the old handler cannot let go of the new one's claim"
        );
    }
}

//! 一條連線：一個讀取 task ＋ 一個送出 task ＋ 一張會話表（ws-receive-dispatch.md §0、§5–§7）。
//!
//! 送與收是兩件事：`send` 只等佇列有位子，永遠不等回覆；回覆由讀取 task 查表交付。
//! 兩個 task 哪一個死了、或呼叫端 `close()`，都走同一條路 `shut_down`：`closed` → `fail_all` → 兩個 task 都 abort。
//! 這層🚫 不重連（第 8 階段監督者的事）：關了之後每個請求立刻回錯。

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use wbf_wire::Pack;

use crate::error::SdkError;
use crate::sessions::{
    take_end_reason, OneshotSink, PackSink, Received, ReceivedHook, SessionGeneration, SessionKey,
    SessionTable, StreamSink, SubscriptionSink,
};
use crate::transport::{FrameSink, FrameSource};

/// 送出佇列最多積幾個 pack；滿了 `send` 就等（背壓），🚫 不丟。
pub const SEND_QUEUE_PACKS: usize = 16;
/// 連續幾個 frame 解不開就關線（跟 server 的 `wbf_ws_corrupt_budget` 同一個想法：一個壞 frame 不值得斷線，一連串就是這條線只剩壞的）。
pub const CORRUPT_FRAME_BUDGET: u32 = 8;
/// 送出（進佇列）最多等多久：送出 task 卡在死掉的 socket 上時，呼叫端不該永遠掛著。
pub const SEND_TIMEOUT: Duration = Duration::from_secs(300);

/// 心跳（ws-receive-dispatch.md §5.1，維護者 2026-09-21：照 WireGuard 的 persistent keepalive 那個概念）：每條線自己一個，
/// 每 `interval` 醒一次；最近 `quiet` 之內這條線有任何送或收就跳過這次，否則送一個 `Ping` 等 `Pong`；
/// `reply_timeout` 內沒回就當這條線死了（`shut_down`）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Heartbeat {
    pub interval: Duration,
    pub quiet: Duration,
    pub reply_timeout: Duration,
}

impl Heartbeat {
    /// 預設：24 秒一次、最近 20 秒有通訊就跳過、Pong 等 10 秒。
    /// 24 秒 ≪ server 的 `wbf_ws_idle_timeout`（300 秒），閘著的線不會被 server 當黑洞收掉；也能在半分鐘內發現對方已經不在。
    pub const DEFAULT: Heartbeat = Heartbeat {
        interval: Duration::from_secs(24),
        quiet: Duration::from_secs(20),
        reply_timeout: Duration::from_secs(10),
    };

    /// 不跳（只給測試別的事情時用）。
    pub const OFF: Heartbeat = Heartbeat {
        interval: Duration::MAX,
        quiet: Duration::ZERO,
        reply_timeout: Duration::from_secs(10),
    };
}

/// 心跳的 `Ping` 用的請求號從這裡往下數：`WbfClient` 的計數器從 1 往上，兩邊碰到要幾十億個請求；真的撞到（`register` 回 Usage）就跳過這次。
const HEARTBEAT_FIRST_SEQ: u32 = u32::MAX;

static NEXT_CONNECTION_ID: AtomicU64 = AtomicU64::new(0);

/// 一個要 Ack 的請求怎麼等（§6）。🚨 預設 `attempts = 1`：重送是冪等的呼叫點自己開的。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AckPolicy {
    /// 總共送幾次（含第一次）。0 當 1。
    pub attempts: u32,
    /// 每一次等回覆的上限。
    pub timeout: Duration,
}

impl AckPolicy {
    /// 送一次、等 `timeout`。
    pub fn once(timeout: Duration) -> AckPolicy {
        AckPolicy {
            attempts: 1,
            timeout,
        }
    }
}

/// 兩個 task 與每個 handle 共用的那一份：表、關了沒、兩個 task 的把手、鉤子。
struct Shared {
    connection_id: u64,
    table: Mutex<SessionTable>,
    closed: AtomicBool,
    tasks: Mutex<Vec<JoinHandle<()>>>,
    hook: ReceivedHook,
    /// 這條線起來的時間；`last_activity_ms` 從這裡算。
    started: std::time::Instant,
    /// 上次送或收（任何 frame）是起來之後第幾毫秒。心跳拿它決定要不要跳過。
    last_activity_ms: AtomicU64,
}

impl Shared {
    fn table(&self) -> MutexGuard<'_, SessionTable> {
        self.table
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// 送了或收了一個 frame。
    fn touch(&self) {
        let elapsed = self.started.elapsed().as_millis() as u64;
        self.last_activity_ms.store(elapsed, Ordering::Relaxed);
    }

    /// Return:
    ///     Duration   距離上次送或收多久
    fn idle_for(&self) -> Duration {
        let now = self.started.elapsed().as_millis() as u64;
        Duration::from_millis(now.saturating_sub(self.last_activity_ms.load(Ordering::Relaxed)))
    }

    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    /// 唯一的關線路徑（§7）：第一個叫到的人做事，之後的都是 no-op。
    /// 順序是先 `fail_all` 再 abort：在等的人先拿到錯，task 才停（abort 自己那個 task 也可以：它在下一個 await 點才停）。
    fn shut_down(&self, reason: &str) {
        if self.closed.swap(true, Ordering::SeqCst) {
            return;
        }
        self.table().fail_all(reason);
        let tasks: Vec<JoinHandle<()>> = self
            .tasks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .drain(..)
            .collect();
        for task in tasks {
            task.abort();
        }
    }
}

pub struct WsLink {
    shared: Arc<Shared>,
    outgoing: mpsc::Sender<Vec<u8>>,
    /// 只有交給呼叫端的那一份是 true：它被丟掉才是「不要這條線了」。心跳 task 手上那份分身是 false，被 abort 時 drop 不關線。
    owner: bool,
}

impl WsLink {
    /// 起兩個 task 加預設的心跳（`Heartbeat::DEFAULT`）。`hook` 每收一個 pack 叫一次（§4，在表鎖之外）；沒接 UI 就給 `sessions::no_hook()`。
    pub fn start<S: FrameSource, K: FrameSink>(source: S, sink: K, hook: ReceivedHook) -> WsLink {
        WsLink::start_with_heartbeat(source, sink, hook, Heartbeat::DEFAULT)
    }

    /// 同上，心跳的間隔自己給（測試用短的；`Heartbeat::OFF` 不跳）。
    pub fn start_with_heartbeat<S: FrameSource, K: FrameSink>(
        source: S,
        sink: K,
        hook: ReceivedHook,
        heartbeat: Heartbeat,
    ) -> WsLink {
        let connection_id = NEXT_CONNECTION_ID.fetch_add(1, Ordering::Relaxed) + 1;
        let shared = Arc::new(Shared {
            connection_id,
            table: Mutex::new(SessionTable::new(connection_id)),
            closed: AtomicBool::new(false),
            tasks: Mutex::new(Vec::new()),
            hook,
            started: std::time::Instant::now(),
            last_activity_ms: AtomicU64::new(0),
        });
        let (outgoing, queued) = mpsc::channel::<Vec<u8>>(SEND_QUEUE_PACKS);

        let writer = tokio::spawn(write_queued(sink, queued, shared.clone()));
        let reader = tokio::spawn(read_and_dispatch(source, shared.clone()));
        let pulse = tokio::spawn(beat(
            WsLink {
                shared: shared.clone(),
                outgoing: outgoing.clone(),
                owner: false,
            },
            heartbeat,
        ));
        shared
            .tasks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .extend([writer, reader, pulse]);
        // 某個 task 在把手登記進去之前就死了（多執行緒 runtime 做得到）：它叫的 shut_down 沒東西可 abort，這裡補上。
        if shared.is_closed() {
            for task in shared
                .tasks
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .drain(..)
            {
                task.abort();
            }
        }
        WsLink {
            shared,
            outgoing,
            owner: true,
        }
    }

    pub fn connection_id(&self) -> u64 {
        self.shared.connection_id
    }

    /// Return:
    ///     bool  1 = 關了（表已清空、之後的請求都會回錯）
    pub fn is_closed(&self) -> bool {
        self.shared.is_closed()
    }

    /// Return:
    ///     u64   這條連線至今收到幾個沒人等的 pack。診斷數字，不是錯（呼叫端喊停之後 server 還在送的同 id Batch 也算）
    pub fn unmatched(&self) -> u64 {
        self.shared.table().unmatched()
    }

    /// 診斷用：無主的 pack 也交一份到這裡。
    pub fn set_orphan_sink(&self, sender: mpsc::Sender<Pack>) {
        self.shared.table().set_orphan_sink(sender);
    }

    /// 只送，不等回覆。有序類由呼叫端按 `seq` 送；單一送出 task 寫 sink，順序不會亂。
    ///
    /// Return:
    ///     Ok(())          進佇列了
    ///     Err(Network)    連線已經關了、或佇列 `SEND_TIMEOUT` 內都排不進去
    pub async fn send(&self, pack: &Pack) -> Result<(), SdkError> {
        if self.shared.is_closed() {
            return Err(SdkError::Network(format!(
                "connection {}: closed, nothing more goes out",
                self.shared.connection_id
            )));
        }
        let bytes = pack.encode()?;
        match tokio::time::timeout(SEND_TIMEOUT, self.outgoing.send(bytes)).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(_)) => Err(SdkError::Network(format!(
                "connection {}: send queue closed, the connection is gone",
                self.shared.connection_id
            ))),
            Err(_) => Err(SdkError::Network(format!(
                "connection {}: send queue did not drain within {SEND_TIMEOUT:?}",
                self.shared.connection_id
            ))),
        }
    }

    /// 一問一答：登記 `Reply { id, seq }` → 送 → 等一個。逾時就把項目拿掉（晚到的回覆變無主）。
    ///
    /// Return:
    ///     Ok(Pack)        回來的那個（可能是 Error pack，這裡不解讀）
    ///     Err(Timeout)    連線還活著、只是沒回
    ///     Err(Network)    連線沒了
    ///     Err(Usage)      同一個 (id, seq) 已經有人在等
    pub async fn request(&self, pack: Pack, timeout: Duration) -> Result<Pack, SdkError> {
        self.request_with_policy(pack, AckPolicy::once(timeout))
            .await
    }

    /// 同上，逾時而且還有次數就**原樣**重送（同 id、同 seq、同 bytes）。第一次的回覆晚到與第二次的回覆同鍵：先到的交付，後到的無主。
    pub async fn request_with_policy(
        &self,
        pack: Pack,
        policy: AckPolicy,
    ) -> Result<Pack, SdkError> {
        let key = SessionKey::Reply {
            id: pack.id,
            seq: pack.seq,
        };
        let (sink, mut receiver) = OneshotSink::new();
        let generation = self.shared.table().register(key, Box::new(sink))?;
        let attempts = policy.attempts.max(1);
        let mut attempt = 0u32;
        let outcome = loop {
            attempt += 1;
            if let Err(error) = self.send(&pack).await {
                break Err(error);
            }
            match tokio::time::timeout(policy.timeout, &mut receiver).await {
                Ok(Ok(delivered)) => break delivered,
                Ok(Err(_dropped)) => {
                    break Err(SdkError::Network(format!(
                        "connection {}: the reply slot for {key:?} was dropped",
                        self.shared.connection_id
                    )))
                }
                Err(_elapsed) if attempt < attempts => continue,
                Err(_elapsed) => break Err(self.no_reply_error(key, attempt, policy.timeout)),
            }
        };
        self.shared.table().remove_if(key, generation);
        outcome
    }

    /// 逾時分兩種（PR #52 審查 cirno 🟡3）：連線還活著是 `Timeout`（可以重試），連線沒了是 `Network`（要重連）。
    fn no_reply_error(&self, key: SessionKey, attempts: u32, timeout: Duration) -> SdkError {
        let message = format!(
            "connection {}: no reply to {key:?} within {timeout:?} after {attempts} attempt(s)",
            self.shared.connection_id
        );
        if self.shared.is_closed() {
            SdkError::Network(format!("{message}; the connection is closed"))
        } else {
            SdkError::Timeout(message)
        }
    }

    /// 一問多答：登記 `Session(id)` → 送。回來的每一個抄這個 id 的 pack 都進 handle；handle 丟掉就從表裡拿掉。
    ///
    /// Return:
    ///     Ok(StreamHandle)
    ///     Err(Usage)      `id` 是 0（具名會話要有名字）、或這個 id 已經有會話
    ///     Err(Network)    送不出去
    pub async fn open_stream(&self, pack: Pack) -> Result<StreamHandle, SdkError> {
        let key = named_key(self.shared.connection_id, &pack)?;
        let (sink, receiver, end) = StreamSink::new();
        let generation = self.register_and_send(key, Box::new(sink), &pack).await?;
        Ok(StreamHandle {
            inbox: SessionInbox {
                key,
                generation,
                receiver,
                end,
                shared: self.shared.clone(),
            },
        })
    }

    /// 訂閱：登記長活的 `Session(id)` → 送。到 handle 被丟掉、或收到帶這個 id 的 `Control/Error`（`Superseded`）為止。
    /// 📎 線上的 `Unsubscribe` 是呼叫端的事；丟掉 handle 只是不再收。
    pub async fn subscribe(&self, pack: Pack) -> Result<Subscription, SdkError> {
        let key = named_key(self.shared.connection_id, &pack)?;
        let (sink, receiver, gap, end) = SubscriptionSink::new();
        let generation = self.register_and_send(key, Box::new(sink), &pack).await?;
        Ok(Subscription {
            inbox: SessionInbox {
                key,
                generation,
                receiver,
                end,
                shared: self.shared.clone(),
            },
            gap,
        })
    }

    async fn register_and_send(
        &self,
        key: SessionKey,
        sink: Box<dyn PackSink>,
        pack: &Pack,
    ) -> Result<SessionGeneration, SdkError> {
        let generation = self.shared.table().register(key, sink)?;
        if let Err(error) = self.send(pack).await {
            self.shared.table().remove_if(key, generation);
            return Err(error);
        }
        Ok(generation)
    }

    /// 主動關：兩個 task 停、表清空、之後 `send` 立刻回錯。
    pub fn close(&self) {
        self.shared.shut_down("closed by this side");
    }
}

impl Drop for WsLink {
    fn drop(&mut self) {
        if self.owner {
            self.close();
        }
    }
}

fn named_key(connection_id: u64, pack: &Pack) -> Result<SessionKey, SdkError> {
    if pack.id == 0 {
        return Err(SdkError::Usage(format!(
            "connection {connection_id}: a named session needs a non-zero id (kind {:?} subtype {:#04x})",
            pack.kind, pack.subtype
        )));
    }
    Ok(SessionKey::Session(pack.id))
}

/// 送出 task。sink 寫不進去就是連線死了：走 `shut_down`，🚫 不能只是自己停下來（PR #52 審查 rumia／cirno／salvia 🔴）。
async fn write_queued<K: FrameSink>(
    mut sink: K,
    mut queued: mpsc::Receiver<Vec<u8>>,
    shared: Arc<Shared>,
) {
    while let Some(bytes) = queued.recv().await {
        if let Err(error) = sink.send(bytes).await {
            shared.shut_down(&format!("send failed: {error}"));
            return;
        }
        shared.touch();
    }
}

/// 心跳 task（`Heartbeat`）。拿一份 `WsLink` 的分身（同一個 `Shared`，`owner: false`）送 `Ping`：走的是一般的 `request`，所以 `Pong` 也經會話表、也過鉤子。
async fn beat(link: WsLink, heartbeat: Heartbeat) {
    if heartbeat.interval == Duration::MAX {
        return;
    }
    let mut seq = HEARTBEAT_FIRST_SEQ;
    loop {
        tokio::time::sleep(heartbeat.interval).await;
        if link.shared.is_closed() {
            return;
        }
        // 最近有通訊：線是活的、server 那邊的 idle 也沒在走，這次不用跳。
        if link.shared.idle_for() < heartbeat.quiet {
            continue;
        }
        let ping = Pack {
            kind: wbf_wire::Kind::Control,
            subtype: wbf_wire::pack::control::PING,
            flags: wbf_wire::pack::flags::WANT_ACK,
            id: 0,
            seq,
            meta: Vec::new(),
            data: Vec::new(),
        };
        seq = seq.wrapping_sub(1);
        match link.request(ping, heartbeat.reply_timeout).await {
            Ok(_pong) => {}
            // 這個號剛好有人在用：那條線顯然活著，下次再說。
            Err(SdkError::Usage(_)) => {}
            Err(error) => {
                link.shared.shut_down(&format!("heartbeat: {error}"));
                return;
            }
        }
    }
}

/// 讀取 task：收 → decode → 查表（鎖內）→ 鉤子（鎖外）→ 交付（鎖內）。
async fn read_and_dispatch<S: FrameSource>(mut source: S, shared: Arc<Shared>) {
    let mut undecodable_in_a_row = 0u32;
    let reason = loop {
        match source.receive().await {
            Ok(Some(bytes)) => match Pack::decode(&bytes) {
                Ok(pack) => {
                    shared.touch();
                    undecodable_in_a_row = 0;
                    let (session, route) = shared.table().classify(&pack);
                    (shared.hook)(&Received {
                        connection_id: shared.connection_id,
                        session,
                        route,
                        pack: &pack,
                    });
                    shared.table().dispatch(pack);
                }
                Err(error) => {
                    undecodable_in_a_row += 1;
                    if undecodable_in_a_row >= CORRUPT_FRAME_BUDGET {
                        break format!(
                            "{undecodable_in_a_row} frames in a row did not decode, last: {error}"
                        );
                    }
                }
            },
            Ok(None) => break "websocket closed by the peer".to_string(),
            Err(error) => break error.to_string(),
        }
    };
    shared.shut_down(&reason);
}

/// 一段具名會話的收件匣；丟掉就從表裡拿掉（只拿自己那一代；之後抄這個 id 的 pack 變無主）。
struct SessionInbox {
    key: SessionKey,
    generation: SessionGeneration,
    receiver: mpsc::Receiver<Pack>,
    end: Arc<Mutex<Option<SdkError>>>,
    shared: Arc<Shared>,
}

impl SessionInbox {
    async fn next(&mut self, timeout: Duration) -> Result<Option<Pack>, SdkError> {
        match tokio::time::timeout(timeout, self.receiver.recv()).await {
            Ok(Some(pack)) => Ok(Some(pack)),
            Ok(None) => match take_end_reason(&self.end) {
                Some(reason) => Err(reason),
                None => Ok(None),
            },
            Err(_elapsed) => {
                let message = format!("session {:?}: no pack within {timeout:?}", self.key);
                if self.shared.is_closed() {
                    Err(SdkError::Network(format!(
                        "{message}; the connection is closed"
                    )))
                } else {
                    Err(SdkError::Timeout(message))
                }
            }
        }
    }
}

impl Drop for SessionInbox {
    fn drop(&mut self) {
        self.shared.table().remove_if(self.key, self.generation);
    }
}

pub struct StreamHandle {
    inbox: SessionInbox,
}

impl StreamHandle {
    /// Return:
    ///     Ok(Some(pack))   下一個
    ///     Ok(None)         會話結束了（`IS_LAST`／`Control/Error` 已經交付過）
    ///     Err(Timeout)     連線還活著、只是這段時間沒有下一個
    ///     Err(Network)     連線沒了
    ///     Err(Protocol)    收件匣塞滿（消費端沒在讀）
    pub async fn next(&mut self, timeout: Duration) -> Result<Option<Pack>, SdkError> {
        self.inbox.next(timeout).await
    }

    pub fn key(&self) -> SessionKey {
        self.inbox.key
    }
}

pub struct Subscription {
    inbox: SessionInbox,
    gap: Arc<AtomicBool>,
}

impl Subscription {
    /// Return:
    ///     Ok(Some(pack))   下一個（Ack、CryptoState、Push、DeviceChanged、Superseded 都從這裡來，🚫 這裡不解讀）
    ///     Ok(None)         訂閱結束了（`Control/Error` 已經交付過）
    ///     Err(Timeout)     連線還活著、只是這段時間沒推
    ///     Err(Network)     連線沒了
    pub async fn next(&mut self, timeout: Duration) -> Result<Option<Pack>, SdkError> {
        self.inbox.next(timeout).await
    }

    /// 讀一次就清掉：true ＝ 上次問到現在收件匣滿過、丟過推播（跟 server 的 `gap` 同義，用 `Fetch`／`Recent` 補）。
    pub fn take_gap(&self) -> bool {
        self.gap.swap(false, Ordering::SeqCst)
    }

    pub fn key(&self) -> SessionKey {
        self.inbox.key
    }
}

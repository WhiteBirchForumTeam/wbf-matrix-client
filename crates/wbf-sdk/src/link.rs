//! 一條連線：一個讀取 task ＋ 一個送出 task ＋ 一張會話表（ws-receive-dispatch.md §0、§5–§7）。
//!
//! 送與收是兩件事：`send` 只等佇列有位子，永遠不等回覆；回覆由讀取 task 查表交付。
//! 這層🚫 不重連（第 8 階段監督者的事）：讀取 task 結束就把表清掉，之後每個請求立刻回錯。

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use wbf_wire::Pack;

use crate::error::SdkError;
use crate::sessions::{
    take_end_reason, OneshotSink, PackSink, ReceivedHook, SessionKey, SessionTable, StreamSink,
    SubscriptionSink,
};
use crate::transport::{FrameSink, FrameSource};

/// 送出佇列最多積幾個 pack；滿了 `send` 就等（背壓），🚫 不丟。
pub const SEND_QUEUE_PACKS: usize = 16;
/// 連續幾個 frame 解不開就關線（跟 server 的 `wbf_ws_corrupt_budget` 同一個想法：一個壞 frame 不值得斷線，一連串就是這條線只剩壞的）。
pub const CORRUPT_FRAME_BUDGET: u32 = 8;
/// 送出（進佇列）最多等多久：送出 task 卡在死掉的 socket 上時，呼叫端不該永遠掛著。
pub const SEND_TIMEOUT: Duration = Duration::from_secs(300);

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

pub struct WsLink {
    connection_id: u64,
    table: Arc<Mutex<SessionTable>>,
    outgoing: mpsc::Sender<Vec<u8>>,
    closed: Arc<AtomicBool>,
    reader: JoinHandle<()>,
}

impl WsLink {
    /// 起兩個 task。`hook` 每收一個 pack 叫一次（§4）；沒接 UI 就給 `sessions::no_hook()`。
    pub fn start<S: FrameSource, K: FrameSink>(source: S, sink: K, hook: ReceivedHook) -> WsLink {
        let connection_id = NEXT_CONNECTION_ID.fetch_add(1, Ordering::Relaxed) + 1;
        let table = Arc::new(Mutex::new(SessionTable::new(connection_id, hook)));
        let closed = Arc::new(AtomicBool::new(false));
        let (outgoing, queued) = mpsc::channel::<Vec<u8>>(SEND_QUEUE_PACKS);

        let writer = tokio::spawn(write_queued(sink, queued));
        let reader = tokio::spawn(read_and_dispatch(
            source,
            table.clone(),
            closed.clone(),
            writer,
        ));
        WsLink {
            connection_id,
            table,
            outgoing,
            closed,
            reader,
        }
    }

    pub fn connection_id(&self) -> u64 {
        self.connection_id
    }

    /// Return:
    ///     bool  1 = 讀取 task 已經結束（表已清空、之後的請求都會回錯）
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    /// Return:
    ///     u64   這條連線至今收到幾個沒人等的 pack
    pub fn unmatched(&self) -> u64 {
        self.table().unmatched()
    }

    /// 診斷用：無主的 pack 也交一份到這裡。
    pub fn set_orphan_sink(&self, sender: mpsc::Sender<Pack>) {
        self.table().set_orphan_sink(sender);
    }

    fn table(&self) -> MutexGuard<'_, SessionTable> {
        self.table
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// 只送，不等回覆。有序類由呼叫端按 `seq` 送；單一送出 task 寫 sink，順序不會亂。
    ///
    /// Return:
    ///     Ok(())          進佇列了
    ///     Err(Network)    連線已經沒了、或佇列 `SEND_TIMEOUT` 內都排不進去
    pub async fn send(&self, pack: &Pack) -> Result<(), SdkError> {
        let bytes = pack.encode()?;
        match tokio::time::timeout(SEND_TIMEOUT, self.outgoing.send(bytes)).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(_)) => Err(SdkError::Network(format!(
                "connection {}: send queue closed, the connection is gone",
                self.connection_id
            ))),
            Err(_) => Err(SdkError::Network(format!(
                "connection {}: send queue did not drain within {SEND_TIMEOUT:?}",
                self.connection_id
            ))),
        }
    }

    /// 一問一答：登記 `Reply { id, seq }` → 送 → 等一個。逾時就把項目拿掉（晚到的回覆變無主）。
    ///
    /// Return:
    ///     Ok(Pack)        回來的那個（可能是 Error pack，這裡不解讀）
    ///     Err(Network)    逾時、連線沒了
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
        self.table().register(key, Box::new(sink))?;
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
                        self.connection_id
                    )))
                }
                Err(_elapsed) if attempt < attempts => continue,
                Err(_elapsed) => {
                    break Err(SdkError::Network(format!(
                        "connection {}: no reply to {key:?} within {:?} after {attempt} attempt(s)",
                        self.connection_id, policy.timeout
                    )))
                }
            }
        };
        self.table().remove(key);
        outcome
    }

    /// 一問多答：登記 `Session(id)` → 送。回來的每一個抄這個 id 的 pack 都進 handle；handle 丟掉就從表裡拿掉。
    ///
    /// Return:
    ///     Ok(StreamHandle)
    ///     Err(Usage)      `id` 是 0（具名會話要有名字）、或這個 id 已經有會話
    ///     Err(Network)    送不出去
    pub async fn open_stream(&self, pack: Pack) -> Result<StreamHandle, SdkError> {
        let key = named_key(self.connection_id, &pack)?;
        let (sink, receiver, end) = StreamSink::new();
        self.register_and_send(key, Box::new(sink), &pack).await?;
        Ok(StreamHandle {
            inbox: SessionInbox {
                key,
                receiver,
                end,
                table: self.table.clone(),
            },
        })
    }

    /// 訂閱：登記長活的 `Session(id)` → 送。到 handle 被丟掉、或收到帶這個 id 的 `Control/Error`（`Superseded`）為止。
    /// 📎 線上的 `Unsubscribe` 是呼叫端的事；丟掉 handle 只是不再收。
    pub async fn subscribe(&self, pack: Pack) -> Result<Subscription, SdkError> {
        let key = named_key(self.connection_id, &pack)?;
        let (sink, receiver, gap, end) = SubscriptionSink::new();
        self.register_and_send(key, Box::new(sink), &pack).await?;
        Ok(Subscription {
            inbox: SessionInbox {
                key,
                receiver,
                end,
                table: self.table.clone(),
            },
            gap,
        })
    }

    async fn register_and_send(
        &self,
        key: SessionKey,
        sink: Box<dyn PackSink>,
        pack: &Pack,
    ) -> Result<(), SdkError> {
        self.table().register(key, sink)?;
        if let Err(error) = self.send(pack).await {
            self.table().remove(key);
            return Err(error);
        }
        Ok(())
    }

    /// 主動關：兩個 task 停、表清空。
    pub fn close(&self) {
        self.reader.abort();
        self.closed.store(true, Ordering::SeqCst);
        self.table().fail_all("closed by this side");
    }
}

impl Drop for WsLink {
    fn drop(&mut self) {
        self.close();
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

async fn write_queued<K: FrameSink>(mut sink: K, mut queued: mpsc::Receiver<Vec<u8>>) {
    while let Some(bytes) = queued.recv().await {
        if sink.send(bytes).await.is_err() {
            break;
        }
    }
}

async fn read_and_dispatch<S: FrameSource>(
    mut source: S,
    table: Arc<Mutex<SessionTable>>,
    closed: Arc<AtomicBool>,
    writer: JoinHandle<()>,
) {
    let mut undecodable_in_a_row = 0u32;
    let reason = loop {
        match source.receive().await {
            Ok(Some(bytes)) => match Pack::decode(&bytes) {
                Ok(pack) => {
                    undecodable_in_a_row = 0;
                    table
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .dispatch(pack);
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
    closed.store(true, Ordering::SeqCst);
    writer.abort();
    table
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .fail_all(&reason);
}

/// 一段具名會話的收件匣；丟掉就從表裡拿掉（之後抄這個 id 的 pack 變無主）。
struct SessionInbox {
    key: SessionKey,
    receiver: mpsc::Receiver<Pack>,
    end: Arc<Mutex<Option<SdkError>>>,
    table: Arc<Mutex<SessionTable>>,
}

impl SessionInbox {
    async fn next(&mut self, timeout: Duration) -> Result<Option<Pack>, SdkError> {
        match tokio::time::timeout(timeout, self.receiver.recv()).await {
            Ok(Some(pack)) => Ok(Some(pack)),
            Ok(None) => match take_end_reason(&self.end) {
                Some(reason) => Err(reason),
                None => Ok(None),
            },
            Err(_elapsed) => Err(SdkError::Network(format!(
                "session {:?}: no pack within {timeout:?}",
                self.key
            ))),
        }
    }
}

impl Drop for SessionInbox {
    fn drop(&mut self) {
        self.table
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(self.key);
    }
}

pub struct StreamHandle {
    inbox: SessionInbox,
}

impl StreamHandle {
    /// Return:
    ///     Ok(Some(pack))   下一個
    ///     Ok(None)         會話結束了（`IS_LAST`／`Control/Error` 已經交付過）
    ///     Err(Network)     逾時、連線沒了
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
    ///     Err(Network)     逾時、連線沒了
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

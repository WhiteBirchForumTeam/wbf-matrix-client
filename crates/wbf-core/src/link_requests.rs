//! 一條線上的請求：送收分開、動作封在請求裡（/docs/design/daemon/link-requests.md）。
//!
//! [`RequestLine`] 是一條線的**發送 queue ＋ 發送端**：請求（就是它的動作，[`LineRequest`]）放進 queue，發送端一個一個取出、配 seq、
//! 交給 `WsLink` 送出，🚫 不等回覆。每個回覆（或逾時、斷線）連同它的動作交回擁有者的收件匣（[`LineReply`]），由擁有者的處理端照到達順序執行。
//!
//! - 線從連線池借**分身**（`LinkPool::find_ws_link`），🚫 不握池那格的鎖。沒開或死了就停一下再看，queue 留著（§2 最後一條）：開線是 `link_keeper` 的事。
//! - 發送端一次只交一個給 `WsLink`、交完才取下一個，所以插到最前面的請求就是下一個送出去的（§2）。
//! - 逾時看整條線：有請求在等、而一段時間內一個回應都沒收到才算（§4）；線還在回應，排得再後面也🚫 逾時。
//! - 失敗怎麼處置🚫 不在這裡決定（§4）：擁有者收到 `Network`／`Timeout` 自己決定要不要 `push_front` 重送。

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use tokio::sync::{mpsc, Notify};
use tokio::task::{JoinHandle, JoinSet};
use wbf_sdk::link::WsLink;
use wbf_sdk::{protocol, SdkError};
use wbf_wire::Pack;

use crate::link_pool::{LinkPool, LinkRole};

/// 一問一答的 seq 從這裡往上數（/docs/design/daemon/link-requests.md §7）。同一條 `WsLink` 上還有兩個發號的：
/// 開線時的 `WbfClient`（`hello`，還沒搬過來的 CLI 路徑也是）從 1 往上、心跳從 `u32::MAX` 往下，三邊要撞到都得先發二十億個請求。
pub(crate) const FIRST_SEQ: u32 = 1 << 31;

/// 線沒開或死了：等這麼久再向池要一次。
const LINK_RETRY: Duration = Duration::from_secs(1);

/// 一條線上的一種請求。它同時是**動作**：回覆到了，擁有者照它決定做什麼（/docs/design/daemon/link-requests.md §3）。
pub(crate) trait LineRequest: Clone + PartialEq + Send + 'static {
    /// Args:
    ///     seq: 線發的請求號, example: 2147483648
    /// Return:
    ///     Pack  這個請求的封包
    fn to_pack(&self, seq: u32) -> Pack;
}

/// 一個請求的結果，交回擁有者的收件匣。
pub(crate) struct LineReply<A> {
    pub action: A,
    /// `Ok`：`expect_ack` 驗過的 Ack。`Err`：`Network`（線斷了、送不出去）、`Timeout`（線活著、server 沒聲）、`Server`（server 拒絕）、`Protocol`
    pub result: Result<Pack, SdkError>,
}

struct LineQueue<A> {
    queued: Mutex<VecDeque<A>>,
    wake: Notify,
}

impl<A> LineQueue<A> {
    fn queued(&self) -> MutexGuard<'_, VecDeque<A>> {
        self.queued
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// 一條線的發送 queue ＋ 發送端。丟掉就停發送端，送出去還沒回的也一起丟（回覆變無主）。
pub(crate) struct RequestLine<A> {
    queue: Arc<LineQueue<A>>,
    sender: JoinHandle<()>,
}

impl<A> Drop for RequestLine<A> {
    fn drop(&mut self) {
        self.sender.abort();
    }
}

impl<A: LineRequest> RequestLine<A> {
    /// 起發送端。⚠️ 要在 tokio runtime 裡叫。
    ///
    /// Args:
    ///     links: 這個帳號的連線池（只借開著的線）
    ///     role: example: LinkRole::Download
    ///     silence_timeout: 有請求在等時，這條線最久可以多久沒有任何回應, example: Duration::from_secs(60)
    ///     replies: 擁有者的收件匣；每個送出的請求恰好交回一個 `LineReply`（擁有者已經不在了就丟掉）
    pub(crate) fn start(
        links: Arc<LinkPool>,
        role: LinkRole,
        silence_timeout: Duration,
        replies: mpsc::UnboundedSender<LineReply<A>>,
    ) -> RequestLine<A> {
        let queue = Arc::new(LineQueue {
            queued: Mutex::new(VecDeque::new()),
            wake: Notify::new(),
        });
        let sender = tokio::spawn(send_queued(
            queue.clone(),
            links,
            role,
            silence_timeout,
            replies,
        ));
        RequestLine { queue, sender }
    }

    /// 排在尾巴（一般的請求）。
    pub(crate) fn push_back(&self, action: A) {
        self.queue.queued().push_back(action);
        self.queue.wake.notify_one();
    }

    /// 插到最前面：下一個送出去的就是它（seek，/docs/design/media/media-download.md §6.2；或重送，/docs/design/daemon/link-requests.md §4）。
    /// 送了第幾次🚫 記在這裡：那是擁有者的事（它決定要不要重送）。
    pub(crate) fn push_front(&self, action: A) {
        self.queue.queued().push_front(action);
        self.queue.wake.notify_one();
    }

    /// 還沒送出去的就拿掉（取消）。已經送出去的拿不回來：它的回覆照樣會交回收件匣。
    ///
    /// Return:
    ///     bool  true ＝ 還在 queue 裡、拿掉了；false ＝ 不在 queue 裡（送出去了，或本來就沒有）
    pub(crate) fn withdraw(&self, action: &A) -> bool {
        let mut queued = self.queue.queued();
        let before = queued.len();
        queued.retain(|item| item != action);
        queued.len() != before
    }
}

/// 送出去、還沒回的請求，鍵是 seq。等回覆的小 task 回來時從這裡拿走自己那一筆才交回去；線整段沒回應時發送端一次拿走全部——
/// 誰先拿到誰交，所以每個請求恰好交回一次。
struct OnWire<A> {
    requests: HashMap<u32, A>,
    /// 上一次收到回應的時間；線從「沒有請求在等」變成「有」的那一刻也算（之前多久沒事都不算沉默）。
    heard_at: Instant,
}

type SharedOnWire<A> = Arc<Mutex<OnWire<A>>>;

fn lock<A>(on_wire: &SharedOnWire<A>) -> MutexGuard<'_, OnWire<A>> {
    on_wire
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// 發送端：取出 → 配 seq → 登記並送出（`WsLink::send_request`）→ 回覆交給一個小 task 等，🚫 這裡不等。
/// 逾時看的是**整條線**（/docs/design/daemon/link-requests.md §4）：有請求在等、而 `silence_timeout` 內一個回應都沒收到，在途的全部交回 `Timeout`；
/// 線還在回應，就🚫 有人逾時。
async fn send_queued<A: LineRequest>(
    queue: Arc<LineQueue<A>>,
    links: Arc<LinkPool>,
    role: LinkRole,
    silence_timeout: Duration,
    replies: mpsc::UnboundedSender<LineReply<A>>,
) {
    let mut link: Option<WsLink> = None;
    let mut next_seq = FIRST_SEQ;
    let on_wire: SharedOnWire<A> = Arc::new(Mutex::new(OnWire {
        requests: HashMap::new(),
        heard_at: Instant::now(),
    }));
    let check_every =
        (silence_timeout / 10).clamp(Duration::from_millis(10), Duration::from_secs(5));
    // 等回覆的小 task 都在這裡：發送端停了（擁有者丟掉 `RequestLine`），它們跟著停。
    let mut waiting = JoinSet::new();
    loop {
        while waiting.try_join_next().is_some() {}
        time_out_if_silent(&on_wire, silence_timeout, &replies, &mut waiting);
        let next = queue.queued().pop_front();
        let Some(action) = next else {
            tokio::select! {
                _ = queue.wake.notified() => {}
                _ = tokio::time::sleep(check_every) => {}
            }
            continue;
        };
        let current = match link.take() {
            Some(open) if !open.is_closed() => Some(open),
            _ => links.find_ws_link(role).await,
        };
        let Some(current) = current else {
            // 線沒開：放回去，進度停在原地，等一下再看（/docs/design/daemon/link-requests.md §2）。
            queue.queued().push_front(action);
            tokio::time::sleep(LINK_RETRY).await;
            continue;
        };
        let seq = next_seq;
        next_seq = next_seq.wrapping_add(1).max(FIRST_SEQ);
        let pack = action.to_pack(seq);
        match current.send_request(pack.clone()).await {
            Ok(pending) => {
                {
                    let mut wire = lock(&on_wire);
                    if wire.requests.is_empty() {
                        wire.heard_at = Instant::now();
                    }
                    wire.requests.insert(seq, action);
                }
                let (on_wire, replies) = (on_wire.clone(), replies.clone());
                waiting.spawn(async move {
                    let result = pending
                        .wait()
                        .await
                        .and_then(|response| protocol::expect_ack(&pack, response));
                    let answered = {
                        let mut wire = lock(&on_wire);
                        wire.heard_at = Instant::now();
                        wire.requests.remove(&seq)
                    };
                    if let Some(action) = answered {
                        let _ = replies.send(LineReply { action, result });
                    }
                });
            }
            // 這個號剛好有人在等（換一圈回來撞到還沒回的）：放回去，下一輪換下一個號。
            Err(SdkError::Usage(_)) => queue.queued().push_front(action),
            Err(error) => {
                let _ = replies.send(LineReply {
                    action,
                    result: Err(error),
                });
            }
        }
        link = Some(current);
    }
}

/// 有請求在等、而線 `silence_timeout` 內沒有任何回應：在途的全部交回 `Timeout`，等它們的小 task 停掉（晚到的回覆變無主）。
fn time_out_if_silent<A>(
    on_wire: &SharedOnWire<A>,
    silence_timeout: Duration,
    replies: &mpsc::UnboundedSender<LineReply<A>>,
    waiting: &mut JoinSet<()>,
) {
    let silent: Vec<A> = {
        let mut wire = lock(on_wire);
        if wire.requests.is_empty() || wire.heard_at.elapsed() < silence_timeout {
            return;
        }
        wire.requests.drain().map(|(_, action)| action).collect()
    };
    waiting.abort_all();
    for action in silent {
        let _ = replies.send(LineReply {
            action,
            result: Err(SdkError::Timeout(format!(
                "no response on this line for {silence_timeout:?}"
            ))),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use wbf_sdk::channel::{Channel, WsChannel};
    use wbf_sdk::client::WbfClient;
    use wbf_sdk::link::Heartbeat;
    use wbf_sdk::sessions::no_hook;
    use wbf_sdk::transport::{memory_pair, FrameSink, FrameSource};
    use wbf_wire::pack::{control, flags};
    use wbf_wire::Kind;

    use crate::event::EventSink;

    /// 測試用的請求：一個名字，pack 的 meta 就是那個名字。
    #[derive(Clone, Debug, PartialEq)]
    struct Named(&'static str);

    impl LineRequest for Named {
        fn to_pack(&self, seq: u32) -> Pack {
            Pack {
                kind: Kind::Download,
                subtype: wbf_wire::pack::download::INFO,
                flags: 0,
                id: 0,
                seq,
                meta: self.0.as_bytes().to_vec(),
                data: Vec::new(),
            }
        }
    }

    fn ack_for(request: &Pack) -> Pack {
        Pack {
            kind: Kind::Control,
            subtype: control::ACK,
            flags: flags::IS_RESPONSE,
            id: request.id,
            seq: request.seq,
            meta: request.meta.clone(),
            data: Vec::new(),
        }
    }

    /// 一條記憶體線放進池的 Download 那格。回 (池, server 那端收到的請求, server 那端送回覆的出口)。
    async fn pool_with_link() -> (
        Arc<LinkPool>,
        mpsc::UnboundedReceiver<Pack>,
        mpsc::UnboundedSender<Pack>,
    ) {
        let (client_end, server_end) = memory_pair(64);
        let link = WsLink::start_with_heartbeat(
            client_end.source,
            client_end.sink,
            no_hook(),
            Heartbeat::OFF,
        );
        let client = WbfClient::new(Channel::WebSocket(Box::new(WsChannel::from_link(link))));
        let pool = Arc::new(LinkPool::new("@alice:fake", EventSink::default()));
        let opened = pool
            .acquire(LinkRole::Download, || async move { Ok(client) })
            .await;
        assert!(opened.is_ok());
        drop(opened);

        let (mut server_source, mut server_sink) = (server_end.source, server_end.sink);
        let (requests_tx, requests) = mpsc::unbounded_channel();
        let (answers, mut answers_rx) = mpsc::unbounded_channel::<Pack>();
        tokio::spawn(async move {
            while let Ok(Some(bytes)) = server_source.receive().await {
                let _ = requests_tx.send(Pack::decode(&bytes).unwrap());
            }
        });
        tokio::spawn(async move {
            while let Some(pack) = answers_rx.recv().await {
                if server_sink.send(pack.encode().unwrap()).await.is_err() {
                    return;
                }
            }
        });
        (pool, requests, answers)
    }

    #[tokio::test]
    async fn requests_go_out_without_waiting_for_replies_and_replies_come_back_in_any_order() {
        let (pool, mut requests, answers) = pool_with_link().await;
        let (replies_tx, mut replies) = mpsc::unbounded_channel();
        let line = RequestLine::start(pool, LinkRole::Download, Duration::from_secs(5), replies_tx);
        line.push_back(Named("A"));
        line.push_back(Named("B"));
        line.push_back(Named("C"));
        // 三個都送出去了，一個回覆都還沒有：發送端🚫 不等回覆。
        let mut sent = Vec::new();
        for _ in 0..3 {
            sent.push(requests.recv().await.unwrap());
        }
        assert_eq!(
            sent.iter()
                .map(|pack| pack.meta.clone())
                .collect::<Vec<_>>(),
            vec![b"A".to_vec(), b"B".to_vec(), b"C".to_vec()]
        );
        assert_eq!(sent[0].seq, FIRST_SEQ);
        // 倒著回：每個回覆照 (id, seq) 找回自己的動作。
        for pack in sent.iter().rev() {
            answers.send(ack_for(pack)).unwrap();
        }
        let mut got = Vec::new();
        for _ in 0..3 {
            let reply = replies.recv().await.unwrap();
            let ack = reply.result.unwrap();
            assert_eq!(ack.meta, reply.action.0.as_bytes());
            got.push(reply.action.0);
        }
        assert_eq!(got, vec!["C", "B", "A"]);
    }

    #[tokio::test]
    async fn a_request_pushed_to_the_front_goes_out_next() {
        let (pool, mut requests, _answers) = pool_with_link().await;
        let (replies_tx, _replies) = mpsc::unbounded_channel();
        let line = RequestLine::start(pool, LinkRole::Download, Duration::from_secs(5), replies_tx);
        // 單執行緒的 runtime：下一個 await 之前發送端跑不到，所以三個是一起排進去的。
        line.push_back(Named("A1"));
        line.push_back(Named("B1"));
        line.push_front(Named("seek"));
        let order: Vec<Vec<u8>> = [
            requests.recv().await.unwrap(),
            requests.recv().await.unwrap(),
            requests.recv().await.unwrap(),
        ]
        .into_iter()
        .map(|pack| pack.meta)
        .collect();
        assert_eq!(
            order,
            vec![b"seek".to_vec(), b"A1".to_vec(), b"B1".to_vec()]
        );
    }

    #[tokio::test]
    async fn a_withdrawn_request_never_goes_out() {
        let (replies_tx, _replies) = mpsc::unbounded_channel();
        // 池裡沒有線：發送端拿到的請求會放回去、等線。
        let line = RequestLine::start(
            Arc::new(LinkPool::new("@nobody:fake", EventSink::default())),
            LinkRole::Download,
            Duration::from_secs(5),
            replies_tx,
        );
        line.push_back(Named("A"));
        line.push_back(Named("B"));
        tokio::task::yield_now().await;
        assert!(line.withdraw(&Named("A")));
        assert!(!line.withdraw(&Named("A")), "only once");
        assert_eq!(line.queue.queued().len(), 1);
    }

    #[tokio::test]
    async fn a_closed_link_hands_every_request_on_the_wire_back_as_network() {
        let (pool, mut requests, _answers) = pool_with_link().await;
        let (replies_tx, mut replies) = mpsc::unbounded_channel();
        let line = RequestLine::start(
            pool.clone(),
            LinkRole::Download,
            Duration::from_secs(5),
            replies_tx,
        );
        line.push_back(Named("A"));
        line.push_back(Named("B"));
        requests.recv().await.unwrap();
        requests.recv().await.unwrap();
        pool.close_all("test").await;
        for _ in 0..2 {
            let reply = replies.recv().await.unwrap();
            assert!(
                matches!(reply.result, Err(SdkError::Network(_))),
                "{:?}",
                reply.result.err()
            );
        }
    }

    #[tokio::test]
    async fn a_line_that_keeps_answering_times_out_nobody() {
        let (pool, mut requests, answers) = pool_with_link().await;
        let (replies_tx, mut replies) = mpsc::unbounded_channel();
        let line = RequestLine::start(
            pool,
            LinkRole::Download,
            Duration::from_millis(300),
            replies_tx,
        );
        line.push_back(Named("A"));
        line.push_back(Named("B"));
        line.push_back(Named("C"));
        let sent = [
            requests.recv().await.unwrap(),
            requests.recv().await.unwrap(),
            requests.recv().await.unwrap(),
        ];
        // 每 200 ms 回一個：C 從送出到回覆超過 600 ms，可是線一直在回應，誰都🚫 逾時。
        for pack in &sent {
            tokio::time::sleep(Duration::from_millis(200)).await;
            answers.send(ack_for(pack)).unwrap();
        }
        for _ in 0..3 {
            let reply = tokio::time::timeout(Duration::from_secs(2), replies.recv())
                .await
                .unwrap()
                .unwrap();
            assert!(reply.result.is_ok(), "{:?}", reply.result.err());
        }
    }

    #[tokio::test]
    async fn a_line_that_answers_nothing_times_out_every_waiting_request_once() {
        let (pool, mut requests, answers) = pool_with_link().await;
        let (replies_tx, mut replies) = mpsc::unbounded_channel();
        let line = RequestLine::start(
            pool,
            LinkRole::Download,
            Duration::from_millis(200),
            replies_tx,
        );
        line.push_back(Named("A"));
        line.push_back(Named("B"));
        let first = requests.recv().await.unwrap();
        requests.recv().await.unwrap();
        let mut timed_out = Vec::new();
        for _ in 0..2 {
            let reply = tokio::time::timeout(Duration::from_secs(2), replies.recv())
                .await
                .unwrap()
                .unwrap();
            assert!(
                matches!(reply.result, Err(SdkError::Timeout(_))),
                "{:?}",
                reply.result.err()
            );
            timed_out.push(reply.action.0);
        }
        timed_out.sort();
        assert_eq!(timed_out, vec!["A", "B"]);
        // 晚到的回覆🚫 再交一次。
        answers.send(ack_for(&first)).unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(200), replies.recv())
                .await
                .is_err(),
            "each request comes back exactly once"
        );
    }
}

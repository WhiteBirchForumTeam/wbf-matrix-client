//! 會話表：這個 pack 是誰的（ws-receive-dispatch.md §2–§4）。
//!
//! 純資料結構、同步、沒有網路：`SessionKey` → `PackSink`。讀取 task 每收一個 pack 先 `classify`（查表、不動表）、
//! 放鎖之後叫鉤子、再 `dispatch`（交付）；沒人等就是「無主」，計數、🚫 不交給剛好在等的人。
//! 鉤子（`ReceivedHook`，§4）的型別定義在這裡，但表**不持有、不呼叫**它：它在表鎖之外由 `link.rs` 叫，才不會重入死鎖。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::{mpsc, oneshot};
use wbf_wire::pack::{control, flags};
use wbf_wire::{Kind, Pack};

use crate::error::SdkError;

/// `id` 是會話的名字，`seq` 是會話內的計數（wire-format §4.1）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SessionKey {
    /// 具名會話：所有抄這個 `id` 的 pack 都是它的（含推播與 `Superseded`）。`id` 不是 0。
    Session(u64),
    /// 一問一答：`id` 與 `seq` 都抄回的那一個 Control 回應。
    Reply { id: u64, seq: u32 },
}

/// 這個 pack 走了哪條路。只給鉤子與 log 看，表自己不用它做決定。
/// 有 serde：它會跟著 `CoreEvent::Received` 變成 RPC 的推播（link-pool.md §4）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Route {
    Oneshot,
    Stream,
    Subscription,
    Unmatched,
}

/// 鉤子看到的一個 pack：哪條連線、哪段會話、走哪條路、pack 本身。
/// `session`／`route` 是**查表那一刻**的答案；交付在放鎖之後，中間項目被拿掉的話交付會算成無主（差一個 pack、只影響計數）。
pub struct Received<'a> {
    pub connection_id: u64,
    /// None ＝ 無主。
    pub session: Option<SessionKey>,
    pub route: Route,
    pub pack: &'a Pack,
}

/// 同步、不可等待：要做慢事就自己丟進自己的佇列，讀取 task 不被它拖住。
/// 在表鎖**之外**叫（§4），所以裡面可以讀同一條 link 的同步狀態（`unmatched()` 之類）；🚫 不能在裡面 block 等這條 link 的回覆——那是等自己。
pub type ReceivedHook = Arc<dyn Fn(&Received<'_>) + Send + Sync>;

/// 什麼都不做的鉤子（沒接 UI 的時候）。
pub fn no_hook() -> ReceivedHook {
    Arc::new(|_| {})
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Delivery {
    /// 項目留在表裡。
    Kept,
    /// 這段會話結束了，表把它拿掉。
    Finished,
}

/// 會話表裡的一項：一個 handler 加一個結束規則。內建三種（`OneshotSink`、`StreamSink`、`SubscriptionSink`）；
/// 特規的 spec 寫一個新的登記進表，讀取 task 與表一個字不改。
pub trait PackSink: Send {
    fn route(&self) -> Route;
    fn deliver(&mut self, pack: Pack) -> Delivery;
    /// 關線時每一項都會被叫到：在等的人要收到錯，不能無聲掛著。
    fn fail(self: Box<Self>, reason: SdkError);
}

/// 一段會話為什麼結束。消費端的 channel 收到 None 之後來這裡問：有理由就是錯，沒有就是正常結束。
type EndReason = Arc<Mutex<Option<SdkError>>>;

pub(crate) fn take_end_reason(end: &EndReason) -> Option<SdkError> {
    end.lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .take()
}

fn set_end_reason(end: &EndReason, reason: SdkError) {
    let mut slot = end.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if slot.is_none() {
        *slot = Some(reason);
    }
}

// ---- 內建的三種 sink ----

/// 收到一個就結束。
pub struct OneshotSink {
    sender: Option<oneshot::Sender<Result<Pack, SdkError>>>,
}

impl OneshotSink {
    pub fn new() -> (OneshotSink, oneshot::Receiver<Result<Pack, SdkError>>) {
        let (sender, receiver) = oneshot::channel();
        (
            OneshotSink {
                sender: Some(sender),
            },
            receiver,
        )
    }
}

impl PackSink for OneshotSink {
    fn route(&self) -> Route {
        Route::Oneshot
    }

    fn deliver(&mut self, pack: Pack) -> Delivery {
        if let Some(sender) = self.sender.take() {
            let _ = sender.send(Ok(pack));
        }
        Delivery::Finished
    }

    fn fail(mut self: Box<Self>, reason: SdkError) {
        if let Some(sender) = self.sender.take() {
            let _ = sender.send(Err(reason));
        }
    }
}

/// 串流的收件匣一次最多積幾個 pack。串流的窗有 server 的則數與位元組上限（`Recent` 一窗 ≤ 500 則、每 Batch 10 則），
/// 一個正常的消費者不會塞滿它；塞滿是消費端的 bug，這段會話**失敗**（🚫 不丟、🚫 不擋讀取 task，§3）。
pub const STREAM_QUEUE_PACKS: usize = 256;

/// 一問多答：`Recent`、`Fetch`、`ItemsDestroy`、`Subscribe` 的 Ack＋`CryptoState`。`IS_LAST` 或 `Control/Error` 之後結束。
pub struct StreamSink {
    sender: Option<mpsc::Sender<Pack>>,
    end: EndReason,
}

impl StreamSink {
    pub fn new() -> (StreamSink, mpsc::Receiver<Pack>, EndReason) {
        let (sender, receiver) = mpsc::channel(STREAM_QUEUE_PACKS);
        let end: EndReason = Arc::new(Mutex::new(None));
        (
            StreamSink {
                sender: Some(sender),
                end: end.clone(),
            },
            receiver,
            end,
        )
    }
}

fn is_session_end(pack: &Pack) -> bool {
    pack.flags & flags::IS_LAST != 0
        || (pack.kind == Kind::Control && pack.subtype == control::ERROR)
}

impl PackSink for StreamSink {
    fn route(&self) -> Route {
        Route::Stream
    }

    fn deliver(&mut self, pack: Pack) -> Delivery {
        let Some(sender) = self.sender.as_ref() else {
            return Delivery::Finished;
        };
        let ends = is_session_end(&pack);
        match sender.try_send(pack) {
            Ok(()) if ends => Delivery::Finished,
            Ok(()) => Delivery::Kept,
            Err(mpsc::error::TrySendError::Full(_)) => {
                set_end_reason(
                    &self.end,
                    SdkError::Protocol(format!(
                        "stream session fell {STREAM_QUEUE_PACKS} packs behind; the consumer is not reading"
                    )),
                );
                self.sender = None;
                Delivery::Finished
            }
            Err(mpsc::error::TrySendError::Closed(_)) => Delivery::Finished,
        }
    }

    fn fail(self: Box<Self>, reason: SdkError) {
        set_end_reason(&self.end, reason);
    }
}

/// 訂閱的收件匣一次最多積幾個推播。滿了就丟那個 pack、標 `gap`（跟 server 的 `gap` 同義：正確性由 `Recent`／`Fetch`／1506 守）。
pub const SUBSCRIPTION_QUEUE_PACKS: usize = 64;

/// 長活：到呼叫端丟掉 handle（表把它拿掉），或收到帶這個 id 的 `Control/Error`（`Superseded`；交給消費端當終點）。
pub struct SubscriptionSink {
    sender: Option<mpsc::Sender<Pack>>,
    gap: Arc<AtomicBool>,
    end: EndReason,
}

impl SubscriptionSink {
    #[allow(clippy::type_complexity)]
    pub fn new() -> (
        SubscriptionSink,
        mpsc::Receiver<Pack>,
        Arc<AtomicBool>,
        EndReason,
    ) {
        let (sender, receiver) = mpsc::channel(SUBSCRIPTION_QUEUE_PACKS);
        let gap = Arc::new(AtomicBool::new(false));
        let end: EndReason = Arc::new(Mutex::new(None));
        (
            SubscriptionSink {
                sender: Some(sender),
                gap: gap.clone(),
                end: end.clone(),
            },
            receiver,
            gap,
            end,
        )
    }
}

impl PackSink for SubscriptionSink {
    fn route(&self) -> Route {
        Route::Subscription
    }

    fn deliver(&mut self, pack: Pack) -> Delivery {
        let Some(sender) = self.sender.as_ref() else {
            return Delivery::Finished;
        };
        let ends = pack.kind == Kind::Control && pack.subtype == control::ERROR;
        match sender.try_send(pack) {
            Ok(()) if ends => Delivery::Finished,
            Ok(()) => Delivery::Kept,
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.gap.store(true, Ordering::SeqCst);
                if ends {
                    // 終點那個 pack 擠不進去：消費端拿不到 Error 本身，至少要知道會話是被 server 收掉的。
                    set_end_reason(
                        &self.end,
                        SdkError::Protocol(
                            "subscription ended by a Control/Error the full inbox could not hold"
                                .into(),
                        ),
                    );
                    Delivery::Finished
                } else {
                    Delivery::Kept
                }
            }
            Err(mpsc::error::TrySendError::Closed(_)) => Delivery::Finished,
        }
    }

    fn fail(self: Box<Self>, reason: SdkError) {
        set_end_reason(&self.end, reason);
    }
}

// ---- 表 ----

/// 每次 `register` 發一個新的世代號。同一個鍵先後兩段會話（`IS_LAST` 收掉舊的、同 id 再開新的）靠它分得開：
/// 舊 handle 晚一點才 drop 時，`remove_if` 看世代不對就不動——🚫 不能把新會話從表裡拿掉（PR #52 審查 salvia 🟡3）。
pub type SessionGeneration = u64;

struct Entry {
    generation: SessionGeneration,
    sink: Box<dyn PackSink>,
}

pub struct SessionTable {
    connection_id: u64,
    entries: HashMap<SessionKey, Entry>,
    next_generation: SessionGeneration,
    unmatched: u64,
    /// 診斷用：無主的 pack 交到這裡（測試看「它真的被算成無主」用）。滿了就丟。
    orphans: Option<mpsc::Sender<Pack>>,
}

impl SessionTable {
    pub fn new(connection_id: u64) -> SessionTable {
        SessionTable {
            connection_id,
            entries: HashMap::new(),
            next_generation: 0,
            unmatched: 0,
            orphans: None,
        }
    }

    pub fn connection_id(&self) -> u64 {
        self.connection_id
    }

    /// 🚨 登記一定在送出之前（§2.1）。同一個鍵已經有人在等就是呼叫端的 bug：拒絕，🚫 不蓋掉。
    ///
    /// Return:
    ///     Ok(generation)  登記了；拿掉自己時用 `remove_if(key, generation)`
    ///     Err(Usage)      鍵已經被佔
    pub fn register(
        &mut self,
        key: SessionKey,
        sink: Box<dyn PackSink>,
    ) -> Result<SessionGeneration, SdkError> {
        if self.entries.contains_key(&key) {
            return Err(SdkError::Usage(format!(
                "connection {}: session {key:?} is already registered",
                self.connection_id
            )));
        }
        self.next_generation = self.next_generation.wrapping_add(1);
        let generation = self.next_generation;
        self.entries.insert(key, Entry { generation, sink });
        Ok(generation)
    }

    /// 只在表裡那一項還是**自己那一代**時才拿掉。
    ///
    /// Return:
    ///     Some(sink)    拿掉了（呼叫端自己決定要不要 `fail` 它）
    ///     None          本來就沒有、或已經是別人（新一代）的
    pub fn remove_if(
        &mut self,
        key: SessionKey,
        generation: SessionGeneration,
    ) -> Option<Box<dyn PackSink>> {
        let is_mine = self
            .entries
            .get(&key)
            .is_some_and(|entry| entry.generation == generation);
        if !is_mine {
            return None;
        }
        self.entries.remove(&key).map(|entry| entry.sink)
    }

    pub fn set_orphan_sink(&mut self, sender: mpsc::Sender<Pack>) {
        self.orphans = Some(sender);
    }

    /// Return:
    ///     u64   這條連線至今收到幾個沒人等的 pack。診斷數字，不是錯：呼叫端喊停之後 server 還在送的同 id Batch 也算在裡面
    pub fn unmatched(&self) -> u64 {
        self.unmatched
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// §2.1 的四條規則，依序。
    fn find_key(&self, pack: &Pack) -> Option<SessionKey> {
        if pack.id != 0 && self.entries.contains_key(&SessionKey::Session(pack.id)) {
            return Some(SessionKey::Session(pack.id));
        }
        let exact = SessionKey::Reply {
            id: pack.id,
            seq: pack.seq,
        };
        if self.entries.contains_key(&exact) {
            return Some(exact);
        }
        // `Upload/Create` 的例外：server 把新發的上傳 id 放在標頭。這裡只負責送到，id 該不該是 0 由 `expect_ack` 判。
        let is_control_response =
            pack.kind == Kind::Control && pack.flags & flags::IS_RESPONSE != 0;
        let create_shaped = SessionKey::Reply {
            id: 0,
            seq: pack.seq,
        };
        if is_control_response && self.entries.contains_key(&create_shaped) {
            return Some(create_shaped);
        }
        None
    }

    /// 查表、不動表：這個 pack 現在會交給誰。給鉤子用（在鎖外叫鉤子之前先問一次）。
    ///
    /// Return:
    ///     (Some(key), route)   有人在等
    ///     (None, Unmatched)    無主
    pub fn classify(&self, pack: &Pack) -> (Option<SessionKey>, Route) {
        match self.find_key(pack) {
            Some(key) => {
                let route = self
                    .entries
                    .get(&key)
                    .map(|entry| entry.sink.route())
                    .unwrap_or(Route::Unmatched);
                (Some(key), route)
            }
            None => (None, Route::Unmatched),
        }
    }

    /// 交付。讀取 task 每收一個 pack 叫一次（在鉤子之後）。
    pub fn dispatch(&mut self, pack: Pack) {
        match self.find_key(&pack) {
            Some(key) => {
                let finished = self
                    .entries
                    .get_mut(&key)
                    .is_some_and(|entry| entry.sink.deliver(pack) == Delivery::Finished);
                if finished {
                    self.entries.remove(&key);
                }
            }
            None => {
                self.unmatched = self.unmatched.saturating_add(1);
                if let Some(orphans) = &self.orphans {
                    let _ = orphans.try_send(pack);
                }
            }
        }
    }

    /// 關線：每一項都收到 `Network(reason)`，表清空。
    pub fn fail_all(&mut self, reason: &str) {
        for (key, entry) in self.entries.drain() {
            entry.sink.fail(SdkError::Network(format!(
                "connection {}: {reason} (session {key:?})",
                self.connection_id
            )));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pack(kind: Kind, subtype: u8, flags: u8, id: u64, seq: u32) -> Pack {
        Pack {
            kind,
            subtype,
            flags,
            id,
            seq,
            meta: Vec::new(),
            data: Vec::new(),
        }
    }

    fn ack(id: u64, seq: u32) -> Pack {
        pack(Kind::Control, control::ACK, flags::IS_RESPONSE, id, seq)
    }

    fn error(id: u64, seq: u32) -> Pack {
        pack(Kind::Control, control::ERROR, flags::IS_RESPONSE, id, seq)
    }

    fn push(id: u64, seq: u32) -> Pack {
        pack(
            Kind::Device,
            wbf_wire::pack::device::PUSH,
            flags::IS_RESPONSE,
            id,
            seq,
        )
    }

    fn table() -> SessionTable {
        SessionTable::new(7)
    }

    #[test]
    fn a_reply_goes_to_the_request_that_echoes_both_id_and_seq() {
        let mut table = table();
        let (sink, mut receiver) = OneshotSink::new();
        table
            .register(SessionKey::Reply { id: 0, seq: 5 }, Box::new(sink))
            .unwrap();
        table.dispatch(ack(0, 6));
        assert_eq!(table.unmatched(), 1, "seq 6 沒人等");
        assert!(receiver.try_recv().is_err());
        table.dispatch(ack(0, 5));
        assert_eq!(receiver.try_recv().unwrap().unwrap().seq, 5);
        assert!(table.is_empty(), "單發收到一個就從表裡拿掉");
    }

    /// `Upload/Create` 的回應標頭 id 是新發的上傳 id，不是抄請求的 0。
    #[test]
    fn a_control_response_with_a_foreign_id_still_reaches_the_id_zero_request() {
        let mut table = table();
        let (sink, mut receiver) = OneshotSink::new();
        table
            .register(SessionKey::Reply { id: 0, seq: 9 }, Box::new(sink))
            .unwrap();
        table.dispatch(ack(4242, 9));
        assert_eq!(receiver.try_recv().unwrap().unwrap().id, 4242);
        assert_eq!(table.unmatched(), 0);
    }

    /// 🚨 不是 Control 回應的 pack 不走 Create 例外：一個 id 對不上的 Batch 不能被當成回覆。
    #[test]
    fn the_create_exception_is_only_for_control_responses() {
        let mut table = table();
        let (sink, mut receiver) = OneshotSink::new();
        table
            .register(SessionKey::Reply { id: 0, seq: 9 }, Box::new(sink))
            .unwrap();
        table.dispatch(pack(
            Kind::Event,
            wbf_wire::pack::event::BATCH,
            flags::IS_RESPONSE,
            4242,
            9,
        ));
        assert_eq!(table.unmatched(), 1);
        assert!(receiver.try_recv().is_err());
        assert_eq!(table.len(), 1, "請求還在等");
    }

    /// ⭐ 活著的會話擁有它 id 底下的每一個 pack：推播、Ack、Error 都進它，順序無所謂。
    #[test]
    fn a_live_session_owns_every_pack_with_its_id_in_any_order() {
        let mut table = table();
        let (sink, mut receiver, _gap, end) = SubscriptionSink::new();
        let id = 0x0100_0000_0000_0001;
        table
            .register(SessionKey::Session(id), Box::new(sink))
            .unwrap();
        table.dispatch(push(id, 2));
        table.dispatch(ack(id, 3));
        table.dispatch(push(id, 0));
        let seqs: Vec<u32> = (0..3).map(|_| receiver.try_recv().unwrap().seq).collect();
        assert_eq!(seqs, vec![2, 3, 0], "照到達順序交付，不重排");
        assert_eq!(table.unmatched(), 0);
        table.dispatch(error(id, 4));
        assert_eq!(receiver.try_recv().unwrap().subtype, control::ERROR);
        assert!(table.is_empty(), "Control/Error 收掉訂閱");
        assert!(receiver.try_recv().is_err(), "sender 已丟掉");
        assert!(take_end_reason(&end).is_none(), "正常結束沒有理由");
    }

    #[test]
    fn a_stream_ends_on_is_last_or_error_and_fails_when_the_consumer_stops_reading() {
        let mut table = table();
        let (sink, mut receiver, end) = StreamSink::new();
        table
            .register(SessionKey::Session(3), Box::new(sink))
            .unwrap();
        table.dispatch(push(3, 0));
        table.dispatch(pack(
            Kind::Device,
            0x02,
            flags::IS_RESPONSE | flags::IS_LAST,
            3,
            1,
        ));
        assert!(table.is_empty(), "IS_LAST 結束會話");
        assert_eq!(receiver.try_recv().unwrap().seq, 0);
        assert_eq!(receiver.try_recv().unwrap().seq, 1);
        assert!(take_end_reason(&end).is_none());

        let (sink, mut receiver, end) = StreamSink::new();
        table
            .register(SessionKey::Session(4), Box::new(sink))
            .unwrap();
        for seq in 0..=(STREAM_QUEUE_PACKS as u32) {
            table.dispatch(push(4, seq));
        }
        assert!(table.is_empty(), "塞滿就失敗、從表裡拿掉");
        assert!(matches!(take_end_reason(&end), Some(SdkError::Protocol(_))));
        let _ = receiver.try_recv();
    }

    #[test]
    fn a_full_subscription_drops_the_pack_and_flags_a_gap_but_stays_alive() {
        let mut table = table();
        let (sink, mut receiver, gap, _end) = SubscriptionSink::new();
        table
            .register(SessionKey::Session(5), Box::new(sink))
            .unwrap();
        for seq in 0..=(SUBSCRIPTION_QUEUE_PACKS as u32) {
            table.dispatch(push(5, seq));
        }
        assert!(gap.load(Ordering::SeqCst));
        assert_eq!(table.len(), 1, "訂閱還活著");
        let mut got = 0;
        while receiver.try_recv().is_ok() {
            got += 1;
        }
        assert_eq!(got, SUBSCRIPTION_QUEUE_PACKS, "多的那一個丟了");
    }

    #[test]
    fn registering_the_same_key_twice_is_refused() {
        let mut table = table();
        let (first, _) = OneshotSink::new();
        let (second, _) = OneshotSink::new();
        table
            .register(SessionKey::Reply { id: 0, seq: 1 }, Box::new(first))
            .unwrap();
        assert!(matches!(
            table.register(SessionKey::Reply { id: 0, seq: 1 }, Box::new(second)),
            Err(SdkError::Usage(_))
        ));
    }

    /// 🚨 同一個鍵兩代會話：舊的那代拿不掉新的（PR #52 審查 salvia 🟡3）。
    #[test]
    fn an_old_generation_cannot_remove_the_new_session_under_the_same_key() {
        let mut table = table();
        let (first, _receiver_1, _end_1) = StreamSink::new();
        let old_generation = table
            .register(SessionKey::Session(9), Box::new(first))
            .unwrap();
        table.dispatch(pack(
            Kind::Device,
            0x02,
            flags::IS_RESPONSE | flags::IS_LAST,
            9,
            0,
        ));
        assert!(table.is_empty(), "IS_LAST 收掉第一代");
        let (second, mut receiver_2, _end_2) = StreamSink::new();
        let new_generation = table
            .register(SessionKey::Session(9), Box::new(second))
            .unwrap();
        assert_ne!(old_generation, new_generation);
        assert!(
            table
                .remove_if(SessionKey::Session(9), old_generation)
                .is_none(),
            "舊 handle 晚一點才 drop：不能動新的"
        );
        table.dispatch(push(9, 0));
        assert_eq!(receiver_2.try_recv().unwrap().seq, 0, "新會話照收");
        assert!(table
            .remove_if(SessionKey::Session(9), new_generation)
            .is_some());
        assert!(table.is_empty());
    }

    #[test]
    fn closing_the_connection_fails_every_waiter_and_empties_the_table() {
        let mut table = table();
        let (oneshot_sink, mut oneshot_receiver) = OneshotSink::new();
        let (stream_sink, _stream_receiver, stream_end) = StreamSink::new();
        let (subscription_sink, _subscription_receiver, _gap, subscription_end) =
            SubscriptionSink::new();
        table
            .register(SessionKey::Reply { id: 0, seq: 1 }, Box::new(oneshot_sink))
            .unwrap();
        table
            .register(SessionKey::Session(2), Box::new(stream_sink))
            .unwrap();
        table
            .register(SessionKey::Session(3), Box::new(subscription_sink))
            .unwrap();
        table.fail_all("socket went away");
        assert!(table.is_empty());
        assert!(matches!(
            oneshot_receiver.try_recv().unwrap(),
            Err(SdkError::Network(reason)) if reason.contains("socket went away")
        ));
        assert!(matches!(
            take_end_reason(&stream_end),
            Some(SdkError::Network(_))
        ));
        assert!(matches!(
            take_end_reason(&subscription_end),
            Some(SdkError::Network(_))
        ));
    }

    /// `classify` 不動表，答案跟 `dispatch` 會走的路一致。
    #[test]
    fn classify_names_the_route_without_touching_the_table() {
        let mut table = table();
        let (sink, _receiver, _gap, _end) = SubscriptionSink::new();
        table
            .register(SessionKey::Session(9), Box::new(sink))
            .unwrap();
        assert_eq!(
            table.classify(&push(9, 0)),
            (Some(SessionKey::Session(9)), Route::Subscription)
        );
        assert_eq!(table.classify(&ack(0, 77)), (None, Route::Unmatched));
        assert_eq!(table.len(), 1);
        assert_eq!(table.unmatched(), 0, "classify 不計數");
    }
}

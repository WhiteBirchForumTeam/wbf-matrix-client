//! 收包分派（ws-receive-dispatch.md §8）：傳輸用記憶體對接，測試扮 server 從另一頭送 bytes，順序故意亂。
//! 表本身的規則在 `sessions.rs` 的單元測試；這裡驗的是 link 把 task、表、handle 接起來之後的行為。

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::mpsc;
use wbf_sdk::transport::{memory_pair, FrameSink, FrameSource, MemoryEnd};
use wbf_sdk::{AckPolicy, Received, Route, SdkError, SessionKey, WsLink};
use wbf_wire::pack::{control, device, flags};
use wbf_wire::{Kind, Pack};

const SHORT: Duration = Duration::from_millis(200);
const LONG: Duration = Duration::from_secs(5);

fn pack(kind: Kind, subtype: u8, flags: u8, id: u64, seq: u32) -> Pack {
    Pack {
        kind,
        subtype,
        flags,
        id,
        seq,
        meta: b"{}".to_vec(),
        data: Vec::new(),
    }
}

fn ack(id: u64, seq: u32) -> Pack {
    pack(Kind::Control, control::ACK, flags::IS_RESPONSE, id, seq)
}

fn error_superseded(id: u64, seq: u32) -> Pack {
    let mut pack = pack(
        Kind::Control,
        control::ERROR,
        flags::IS_RESPONSE | flags::IS_LAST,
        id,
        seq,
    );
    pack.meta = br#"{"code":"Superseded","code_id":1505,"message":"taken over"}"#.to_vec();
    pack
}

fn device_push(id: u64, seq: u32) -> Pack {
    pack(Kind::Device, device::PUSH, flags::IS_RESPONSE, id, seq)
}

fn device_batch(id: u64, seq: u32, last: bool) -> Pack {
    let mut flags = flags::IS_RESPONSE;
    if last {
        flags |= flags::IS_LAST;
    }
    pack(Kind::Device, device::BATCH, flags, id, seq)
}

fn ping(seq: u32) -> Pack {
    pack(Kind::Control, control::PING, flags::WANT_ACK, 0, seq)
}

fn fetch(id: u64, seq: u32) -> Pack {
    pack(Kind::Device, device::FETCH, 0, id, seq)
}

/// 測試扮的 server 那一端：送 pack、收 client 送來的 pack。
struct Peer {
    end: MemoryEnd,
}

impl Peer {
    async fn send(&mut self, pack: Pack) {
        self.end.sink.send(pack.encode().unwrap()).await.unwrap();
    }

    async fn receive(&mut self) -> Pack {
        let bytes = tokio::time::timeout(LONG, self.end.source.receive())
            .await
            .expect("client sends within the timeout")
            .unwrap()
            .expect("client still connected");
        Pack::decode(&bytes).unwrap()
    }
}

fn connect() -> (WsLink, Peer) {
    let (client_end, server_end) = memory_pair(64);
    let link = WsLink::start(client_end.source, client_end.sink, wbf_sdk::no_hook());
    (link, Peer { end: server_end })
}

#[tokio::test]
async fn a_reply_that_arrives_before_anyone_waits_is_unmatched_not_delivered_to_the_next_request() {
    let (link, mut peer) = connect();
    let (orphans, mut orphan_inbox) = mpsc::channel(8);
    link.set_orphan_sink(orphans);
    // seq 1 的回覆先到，沒人等。
    peer.send(ack(0, 1)).await;
    let orphan = tokio::time::timeout(LONG, orphan_inbox.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(orphan.seq, 1);
    assert_eq!(link.unmatched(), 1);
    // 之後才發 seq 1 的請求：🚫 不能拿剛才那個回覆，要等新的。
    let request = tokio::spawn({
        let link_ping = ping(1);
        async move { link_ping }
    });
    let request_pack = request.await.unwrap();
    let waiting = link.request(request_pack, SHORT);
    let outcome = waiting.await;
    assert!(
        matches!(outcome, Err(SdkError::Network(ref reason)) if reason.contains("no reply")),
        "{outcome:?}"
    );
    assert_eq!(peer.receive().await.seq, 1, "請求真的送出去了");
}

#[tokio::test]
async fn pushes_between_the_batches_of_a_stream_go_to_the_subscription_not_the_stream() {
    let (link, mut peer) = connect();
    let subscription_id = 0x0100_0000_0000_0001;
    let fetch_id = 0x0100_0000_0000_0002;
    let mut subscription = link
        .subscribe(pack(Kind::Device, device::SUBSCRIBE, 0, subscription_id, 1))
        .await
        .unwrap();
    assert_eq!(peer.receive().await.subtype, device::SUBSCRIBE);
    let mut stream = link.open_stream(fetch(fetch_id, 2)).await.unwrap();
    assert_eq!(peer.receive().await.subtype, device::FETCH);

    // server 的順序：Batch 0、Push、Ack（訂閱的）、CryptoState、Batch 1(last)、Push。
    peer.send(device_batch(fetch_id, 0, false)).await;
    peer.send(device_push(subscription_id, 0)).await;
    peer.send(ack(subscription_id, 1)).await;
    peer.send(pack(
        Kind::Device,
        device::CRYPTO_STATE,
        flags::IS_RESPONSE,
        subscription_id,
        1,
    ))
    .await;
    peer.send(device_batch(fetch_id, 1, true)).await;
    peer.send(device_push(subscription_id, 2)).await;

    let first = stream.next(LONG).await.unwrap().unwrap();
    let second = stream.next(LONG).await.unwrap().unwrap();
    assert_eq!((first.subtype, first.seq), (device::BATCH, 0));
    assert_eq!((second.subtype, second.seq), (device::BATCH, 1));
    assert!(
        stream.next(LONG).await.unwrap().is_none(),
        "IS_LAST 之後串流結束"
    );

    let mut got = Vec::new();
    for _ in 0..4 {
        let pack = subscription.next(LONG).await.unwrap().unwrap();
        got.push((pack.kind, pack.subtype, pack.seq));
    }
    assert_eq!(
        got,
        vec![
            (Kind::Device, device::PUSH, 0),
            (Kind::Control, control::ACK, 1),
            (Kind::Device, device::CRYPTO_STATE, 1),
            (Kind::Device, device::PUSH, 2),
        ]
    );
    assert_eq!(link.unmatched(), 0, "每一個都有主");
    assert!(!subscription.take_gap());
}

#[tokio::test]
async fn superseded_ends_the_subscription_it_names_and_touches_nothing_else() {
    let (link, mut peer) = connect();
    let mine = 0x0100_0000_0000_0007;
    let other = 0x0100_0000_0000_0008;
    let mut superseded = link
        .subscribe(pack(Kind::Device, device::SUBSCRIBE, 0, mine, 1))
        .await
        .unwrap();
    let mut survivor = link
        .subscribe(pack(Kind::Event, 0x04, 0, other, 2))
        .await
        .unwrap();
    peer.receive().await;
    peer.receive().await;
    peer.send(error_superseded(mine, 5)).await;
    peer.send(device_push(other, 0)).await;

    let terminal = superseded.next(LONG).await.unwrap().unwrap();
    assert_eq!(terminal.subtype, control::ERROR);
    assert!(
        superseded.next(LONG).await.unwrap().is_none(),
        "Superseded 之後這個訂閱結束，不是錯"
    );
    assert_eq!(survivor.next(LONG).await.unwrap().unwrap().seq, 0);
    // 被收掉的 id 之後再來的推播是無主的。
    peer.send(device_push(mine, 6)).await;
    peer.send(device_push(other, 1)).await;
    assert_eq!(survivor.next(LONG).await.unwrap().unwrap().seq, 1);
    assert_eq!(link.unmatched(), 1);
}

#[tokio::test]
async fn closing_the_connection_fails_every_waiter_with_the_reason() {
    let (link, mut peer) = connect();
    let mut subscription = link
        .subscribe(pack(
            Kind::Device,
            device::SUBSCRIBE,
            0,
            0x0100_0000_0000_0001,
            1,
        ))
        .await
        .unwrap();
    let mut stream = link
        .open_stream(fetch(0x0100_0000_0000_0002, 2))
        .await
        .unwrap();
    let pending = tokio::spawn({
        let link = Arc::new(link);
        let link_for_request = link.clone();
        async move {
            let outcome = link_for_request.request(ping(3), LONG).await;
            (link, outcome)
        }
    });
    for _ in 0..3 {
        peer.receive().await;
    }
    drop(peer);

    let (link, outcome) = pending.await.unwrap();
    assert!(
        matches!(outcome, Err(SdkError::Network(ref reason)) if reason.contains("closed by the peer")),
        "{outcome:?}"
    );
    assert!(matches!(stream.next(LONG).await, Err(SdkError::Network(_))));
    assert!(matches!(
        subscription.next(LONG).await,
        Err(SdkError::Network(_))
    ));
    assert!(link.is_closed());
    assert!(matches!(
        link.request(ping(4), SHORT).await,
        Err(SdkError::Network(_))
    ));
}

#[tokio::test]
async fn the_hook_sees_every_pack_including_the_unmatched_ones() {
    type Seen = Arc<Mutex<Vec<(Option<SessionKey>, Route, u8)>>>;
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let recorder = seen.clone();
    let (client_end, server_end) = memory_pair(64);
    let link = WsLink::start(
        client_end.source,
        client_end.sink,
        Arc::new(move |received: &Received<'_>| {
            recorder.lock().unwrap().push((
                received.session,
                received.route,
                received.pack.subtype,
            ));
        }),
    );
    let mut peer = Peer { end: server_end };
    let reply = tokio::spawn({
        let link = Arc::new(link);
        async move {
            let outcome = link.request(ping(1), LONG).await;
            (link, outcome)
        }
    });
    assert_eq!(peer.receive().await.subtype, control::PING);
    peer.send(device_push(99, 0)).await;
    peer.send(pack(Kind::Control, control::PONG, flags::IS_RESPONSE, 0, 1))
        .await;
    let (_link, outcome) = reply.await.unwrap();
    assert_eq!(outcome.unwrap().subtype, control::PONG);
    let seen = seen.lock().unwrap().clone();
    assert_eq!(
        seen,
        vec![
            (None, Route::Unmatched, device::PUSH),
            (
                Some(SessionKey::Reply { id: 0, seq: 1 }),
                Route::Oneshot,
                control::PONG
            ),
        ]
    );
}

#[tokio::test]
async fn an_ack_policy_resends_the_same_bytes_and_the_late_first_reply_is_unmatched() {
    let (link, mut peer) = connect();
    let policy = AckPolicy {
        attempts: 3,
        timeout: SHORT,
    };
    let request = tokio::spawn({
        let link = Arc::new(link);
        async move {
            let outcome = link.request_with_policy(ping(7), policy).await;
            (link, outcome)
        }
    });
    let first = peer.receive().await;
    let second = peer.receive().await;
    assert_eq!(first, second, "原樣重送：同 id、同 seq、同內容");
    peer.send(pack(Kind::Control, control::PONG, flags::IS_RESPONSE, 0, 7))
        .await;
    let (link, outcome) = request.await.unwrap();
    assert_eq!(outcome.unwrap().seq, 7);
    // 第一次的回覆現在才到：沒人等了。
    peer.send(pack(Kind::Control, control::PONG, flags::IS_RESPONSE, 0, 7))
        .await;
    tokio::time::sleep(SHORT).await;
    assert_eq!(link.unmatched(), 1);
    // 第三次不會送：拿到回覆就停。
    assert!(
        tokio::time::timeout(SHORT, peer.end.source.receive())
            .await
            .is_err(),
        "沒有第三次"
    );
}

#[tokio::test]
async fn dropping_a_stream_handle_makes_its_later_packs_unmatched() {
    let (link, mut peer) = connect();
    let id = 0x0100_0000_0000_0003;
    let stream = link.open_stream(fetch(id, 1)).await.unwrap();
    peer.receive().await;
    drop(stream);
    peer.send(device_batch(id, 0, false)).await;
    tokio::time::sleep(SHORT).await;
    assert_eq!(link.unmatched(), 1);
    assert!(
        link.open_stream(fetch(id, 2)).await.is_ok(),
        "同一個 id 可以再開：舊的已經從表裡拿掉"
    );
}

#[tokio::test]
async fn a_named_session_refuses_id_zero_and_a_duplicate_id() {
    let (link, mut peer) = connect();
    assert!(matches!(
        link.open_stream(fetch(0, 1)).await,
        Err(SdkError::Usage(_))
    ));
    let _first = link.open_stream(fetch(5, 2)).await.unwrap();
    peer.receive().await;
    assert!(matches!(
        link.open_stream(fetch(5, 3)).await,
        Err(SdkError::Usage(_))
    ));
}

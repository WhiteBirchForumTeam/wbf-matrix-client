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
        matches!(outcome, Err(SdkError::Timeout(ref reason)) if reason.contains("no reply")),
        "連線還活著，是逾時不是斷線：{outcome:?}"
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

// ---- PR #52 審查補的（rumia／cirno／salvia 🔴：writer 死了沒人知道、close 不停 writer；salvia 🟡3：舊 handle 誤刪新會話）----

/// 送出那半死了（對面不收了、但還在送）：在等的人要在有限時間內收到 Network，不是等到各自的 300 秒。
#[tokio::test]
async fn a_dead_sink_fails_every_waiter_promptly_and_closes_the_link() {
    let (client_end, server_end) = memory_pair(64);
    let link = Arc::new(WsLink::start(
        client_end.source,
        client_end.sink,
        wbf_sdk::no_hook(),
    ));
    let MemoryEnd {
        source: peer_source,
        sink: peer_sink,
    } = server_end;
    // 對面不再讀 client 送的東西（丟掉 source），但它自己那條送的線還在：讀取 task 看不出任何異狀。
    drop(peer_source);
    let mut subscription = link
        .subscribe(pack(
            Kind::Device,
            device::SUBSCRIBE,
            0,
            0x0100_0000_0000_0001,
            1,
        ))
        .await
        .expect("registered and queued; the failure surfaces in the writer");
    let pending = tokio::spawn({
        let link = link.clone();
        async move { link.request(ping(2), LONG).await }
    });
    let outcome = tokio::time::timeout(LONG, pending)
        .await
        .expect("failed within bounded time, not after 300 s")
        .unwrap();
    // 請求登記在 writer 死掉之前或之後都可能（兩個 task 的競賽）：前者拿到 fail_all 的理由、後者在 send 開頭就被擋。兩種都是 Network。
    assert!(matches!(outcome, Err(SdkError::Network(_))), "{outcome:?}");
    assert!(link.is_closed());
    // 訂閱一定在失敗之前就登記了：它拿到的理由就是 writer 死掉那個。
    let subscription_outcome = subscription.next(LONG).await;
    assert!(
        matches!(subscription_outcome, Err(SdkError::Network(ref reason)) if reason.contains("send failed")),
        "{subscription_outcome:?}"
    );
    assert!(matches!(
        link.send(&ping(3)).await,
        Err(SdkError::Network(_))
    ));
    drop(peer_sink);
}

/// `close()` 之後：`send` 立刻回錯、對面再也收不到任何 bytes、在等的人拿到 Network。
#[tokio::test]
async fn close_stops_both_tasks_and_nothing_goes_out_afterwards() {
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
    assert_eq!(peer.receive().await.subtype, device::SUBSCRIBE);
    link.close();
    assert!(link.is_closed());
    assert!(
        matches!(link.send(&ping(2)).await, Err(SdkError::Network(ref reason)) if reason.contains("closed")),
        "close 之後 send 立刻回錯"
    );
    assert!(matches!(
        subscription.next(LONG).await,
        Err(SdkError::Network(_))
    ));
    // 送出 task 停了 → client 那頭的 sink 丟掉 → 對面的 source 收到「對方關了」，而不是任何 bytes。
    let peer_saw = tokio::time::timeout(LONG, peer.end.source.receive())
        .await
        .expect("the peer learns the link closed within bounded time");
    assert!(matches!(peer_saw, Ok(None)), "{peer_saw:?}");
}

/// 同一個 id 兩代會話：舊 handle 晚一點才 drop，不能把新會話從表裡拿掉。
#[tokio::test]
async fn a_late_drop_of_an_old_handle_does_not_remove_the_new_session_with_the_same_id() {
    let (link, mut peer) = connect();
    let id = 0x0100_0000_0000_0009;
    let mut old = link.open_stream(fetch(id, 1)).await.unwrap();
    peer.receive().await;
    peer.send(device_batch(id, 0, true)).await;
    assert_eq!(old.next(LONG).await.unwrap().unwrap().seq, 0);
    assert!(
        old.next(LONG).await.unwrap().is_none(),
        "IS_LAST 收掉第一代"
    );
    let mut new = link.open_stream(fetch(id, 2)).await.unwrap();
    peer.receive().await;
    drop(old);
    peer.send(device_batch(id, 0, false)).await;
    assert_eq!(
        new.next(LONG).await.unwrap().unwrap().seq,
        0,
        "新會話照收，沒被舊 handle 的 Drop 拿掉"
    );
    assert_eq!(link.unmatched(), 0);
}

/// 逾時分兩種：連線活著是 Timeout，連線沒了是 Network。
#[tokio::test]
async fn a_quiet_server_is_a_timeout_but_a_dead_connection_is_a_network_error() {
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
    peer.receive().await;
    assert!(matches!(
        subscription.next(SHORT).await,
        Err(SdkError::Timeout(_))
    ));
    drop(peer);
    assert!(matches!(
        subscription.next(LONG).await,
        Err(SdkError::Network(_))
    ));
    assert!(matches!(
        link.request(ping(2), SHORT).await,
        Err(SdkError::Network(_))
    ));
}

// ---- 心跳（ws-receive-dispatch.md §5.1；維護者 2026-09-21：照 WireGuard，安靜才跳、每條線自己一個）----

use wbf_sdk::link::Heartbeat;

fn connect_with_heartbeat(heartbeat: Heartbeat) -> (WsLink, Peer) {
    let (client_end, server_end) = memory_pair(64);
    let link = WsLink::start_with_heartbeat(
        client_end.source,
        client_end.sink,
        wbf_sdk::no_hook(),
        heartbeat,
    );
    (link, Peer { end: server_end })
}

fn pong_for(ping: &Pack) -> Pack {
    pack(
        Kind::Control,
        control::PONG,
        flags::IS_RESPONSE,
        0,
        ping.seq,
    )
}

/// 安靜的線：時間到就送 Ping；對方回 Pong 線就活著、下一個間隔再跳一次。
#[tokio::test]
async fn a_quiet_link_pings_and_stays_open_when_the_peer_answers() {
    let (link, mut peer) = connect_with_heartbeat(Heartbeat {
        interval: Duration::from_millis(100),
        quiet: Duration::from_millis(50),
        reply_timeout: Duration::from_millis(500),
    });
    let first = peer.receive().await;
    assert_eq!(
        (first.kind, first.subtype, first.id),
        (Kind::Control, control::PING, 0)
    );
    assert!(first.flags & flags::WANT_ACK != 0);
    peer.send(pong_for(&first)).await;
    let second = peer.receive().await;
    assert_eq!(second.subtype, control::PING, "下一個間隔再跳一次");
    assert_eq!(
        second.seq,
        first.seq.wrapping_sub(1),
        "心跳的請求號從上往下數"
    );
    peer.send(pong_for(&second)).await;
    assert!(!link.is_closed());
    assert_eq!(link.unmatched(), 0, "Pong 都有人等");
}

/// 忙的線不跳：最近 `quiet` 之內有收到東西就跳過（這裡對面一直送無主的 pack）。
#[tokio::test]
async fn a_busy_link_skips_its_heartbeat() {
    let (link, mut peer) = connect_with_heartbeat(Heartbeat {
        interval: Duration::from_millis(60),
        quiet: Duration::from_millis(200),
        reply_timeout: Duration::from_millis(500),
    });
    // 600 ms 裡每 40 ms 送一個 pack 給 client：線一直是「最近有通訊」。
    for _ in 0..15 {
        peer.send(device_push(0x0100_0000_0000_0009, 0)).await;
        tokio::time::sleep(Duration::from_millis(40)).await;
    }
    assert!(
        tokio::time::timeout(Duration::from_millis(50), peer.end.source.receive())
            .await
            .is_err(),
        "整段時間對面沒收到任何 Ping"
    );
    assert!(!link.is_closed());
    // 安靜下來之後才跳。
    let ping = peer.receive().await;
    assert_eq!(ping.subtype, control::PING);
}

/// Ping 沒人回：這條線死了（shut_down），在等的人立刻收到 Network、理由說是心跳。
#[tokio::test]
async fn an_unanswered_heartbeat_closes_the_link_and_fails_the_waiters() {
    let (link, mut peer) = connect_with_heartbeat(Heartbeat {
        interval: Duration::from_millis(50),
        quiet: Duration::from_millis(10),
        reply_timeout: Duration::from_millis(100),
    });
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
    assert_eq!(peer.receive().await.subtype, device::SUBSCRIBE);
    // 訂閱那一送算通訊；等它安靜下來，Ping 來了、不回。
    let ping = peer.receive().await;
    assert_eq!(ping.subtype, control::PING);
    let outcome = subscription.next(Duration::from_secs(5)).await;
    assert!(
        matches!(outcome, Err(SdkError::Network(ref reason)) if reason.contains("heartbeat")),
        "{outcome:?}"
    );
    assert!(link.is_closed());
}

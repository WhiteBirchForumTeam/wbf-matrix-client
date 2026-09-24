//! 訂閱線的測試共用品（`room_sync.rs`／`key_sync.rs` 的測試）：記憶體對接的假 server（答 Hello、房間的 Subscribe／Recent／Unsubscribe、
//! 金鑰的 Subscribe／Fetch／ItemsDestroy／Unsubscribe）、開好帳號的 `Core`、等待與讀快取的小工具。🚫 只在測試建置。

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use wbf_sdk::channel::{Channel, WsChannel};
use wbf_sdk::client::WbfClient;
use wbf_sdk::login::{Session, SessionBackend};
use wbf_sdk::transport::{memory_pair, FrameSink, FrameSource, MemoryEnd};
use wbf_sdk::WsLink;
use wbf_wire::pack::{control, device, event, flags};
use wbf_wire::{Kind, Pack};

use crate::accounts::AccountDir;
use crate::link_pool::LinkRole;
use crate::{Core, CoreEvent};

pub(crate) const DEAD: &str = "http://127.0.0.1:1";
pub(crate) const ME: &str = "@a:localhost";
pub(crate) const ROOM: &str = "!r:localhost";

pub(crate) fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("wbf-core-roomsync-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// 開好一個 wbf 帳號（session 封好、`m/` 的 crypto store 建好，跟 login 一樣）。
pub(crate) async fn core_with_wbf_account(dir: &std::path::Path) -> (Core, AccountDir) {
    wbf_sdk::vault::Vault::create(dir, &wbf_sdk::Unlock::NoPassphrase).unwrap();
    let core = Core::open(dir);
    core.unlock(None).unwrap();
    let vault = core.vault().unwrap();
    let account = AccountDir::locate(dir, &vault.account_dir_key(), DEAD, ME).unwrap();
    std::fs::create_dir_all(&account.dir).unwrap();
    vault
        .seal_session(
            &account.session_path(),
            &Session {
                server: DEAD.to_string(),
                user_id: ME.to_string(),
                device_id: "DEV".to_string(),
                access_token: "syt_memory".to_string(),
                store_dir: None,
                backend: Some(SessionBackend::WbfSdk),
            },
        )
        .unwrap();
    crate::accounts::write_current(dir, &account).unwrap();
    // wbf 帳號登入時就有 crypto store（login_ops::create_crypto_store_for）；引擎本身丟掉，`olm_engine_of` 要用再開。
    wbf_sdk::crypto_engine::OlmEngine::open(
        &account.matrix_store_dir(),
        &vault.matrix_store_key(),
        ME,
        "DEV",
    )
    .await
    .unwrap();
    (core, account)
}

/// 一則 to-device（`m.dummy`：OlmMachine 認得、吃了不留痕），`count` 就是它在佇列裡的號。
pub(crate) fn to_device_item(count: u64) -> (u64, Value) {
    (
        count,
        json!({ "type": "m.dummy", "sender": "@b:localhost", "content": {} }),
    )
}

/// 一包 `Device/Push`（server → client）：`(count, 事件)` 舊→新。
pub(crate) fn device_push(
    subscription_id: u64,
    seq: u32,
    gap: bool,
    items: &[(u64, Value)],
) -> Pack {
    let counts: Vec<u64> = items.iter().map(|(count, _)| *count).collect();
    let events: Vec<Value> = items.iter().map(|(_, event)| event.clone()).collect();
    response(
        Kind::Device,
        device::PUSH,
        subscription_id,
        seq,
        json!({ "bc": items.len(), "ot": counts.first().copied().unwrap_or(0), "nt": counts.last().copied().unwrap_or(0),
                "counts": counts, "gap": gap }),
        length_prefixed(&events),
    )
}

/// `Device/Fetch` 的一窗：一個 Batch 就送完（`r: 0`、`more: false`）。
pub(crate) fn device_batch(fetch_id: u64, items: &[(u64, Value)]) -> Pack {
    let counts: Vec<u64> = items.iter().map(|(count, _)| *count).collect();
    let events: Vec<Value> = items.iter().map(|(_, event)| event.clone()).collect();
    response(
        Kind::Device,
        device::BATCH,
        fetch_id,
        0,
        json!({ "tc": items.len(), "bc": items.len(), "ot": counts.first().copied().unwrap_or(0), "nt": counts.last().copied().unwrap_or(0),
                "counts": counts, "r": 0, "more": false }),
        length_prefixed(&events),
    )
}

/// 一則文字事件，`g_seq` 就是它的號碼（`r_seq` 同號，夠用）。
pub(crate) fn text_event(g_seq: i64) -> Value {
    json!({
        "type": "m.room.message", "event_id": format!("${g_seq}"), "room_id": ROOM, "sender": "@b:localhost",
        "origin_server_ts": g_seq, "content": { "msgtype": "m.text", "body": format!("event {g_seq}") },
        "unsigned": { wbf_sdk::protocol::R_SEQ_KEY: g_seq, wbf_sdk::protocol::G_SEQ_KEY: g_seq },
    })
}

pub(crate) fn g_seq_of(event: &Value) -> i64 {
    event["unsigned"][wbf_sdk::protocol::G_SEQ_KEY]
        .as_i64()
        .unwrap()
}

pub(crate) fn length_prefixed(events: &[Value]) -> Vec<u8> {
    let mut data = Vec::new();
    for event in events {
        let bytes = serde_json::to_vec(event).unwrap();
        data.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
        data.extend_from_slice(&bytes);
    }
    data
}

pub(crate) fn response(
    kind: Kind,
    subtype: u8,
    id: u64,
    seq: u32,
    meta: Value,
    data: Vec<u8>,
) -> Pack {
    Pack {
        kind,
        subtype,
        flags: flags::IS_RESPONSE,
        id,
        seq,
        meta: meta.to_string().into_bytes(),
        data,
    }
}

/// 一包推播（server → client）：`fs`＝最新那則的 `g_seq`。
pub(crate) fn push(subscription_id: u64, seq: u32, gap: bool, events: &[Value]) -> Pack {
    let seqs: Vec<i64> = events.iter().map(g_seq_of).collect();
    response(
        Kind::Event,
        event::PUSH,
        subscription_id,
        seq,
        json!({ "bc": events.len(), "fs": seqs.iter().max().copied().unwrap_or(0),
                "ls": seqs.iter().min().copied().unwrap_or(0), "gap": gap }),
        length_prefixed(events),
    )
}

/// 假 server 在回 `Subscribe` 的 Ack 之前先推的那一包：(seq, gap, 事件)。
pub(crate) type EarlyPush = Arc<Mutex<Option<(u32, bool, Vec<Value>)>>>;

/// 假的 server 端：答 Hello、Subscribe、Recent（照 `cg_seq` 給比它新的，一窗一個 Batch）、Unsubscribe；
/// 測試從 `outbound` 塞 server 主動推的包。記下每次 `Recent` 帶的 `cg_seq` 與訂閱的 id。事件清單可以跟別條線共用。
pub(crate) struct FakeServer {
    pub(crate) recent_requests: Arc<Mutex<Vec<Option<i64>>>>,
    /// 每個 `Subscribe` 的 id（照順序）。
    pub(crate) subscription_ids: Arc<Mutex<Vec<u64>>>,
    /// 設了就在回 `Subscribe` 的 Ack 之前先推這一包（seq、gap、事件）：造 Ack 之前就到的推播。
    pub(crate) early_push: EarlyPush,
    pub(crate) outbound: tokio::sync::mpsc::Sender<Pack>,

    pub(crate) task: tokio::task::JoinHandle<()>,
    /// 每個 `Device/Subscribe` 的 id（照順序）。
    pub(crate) device_subscription_ids: Arc<Mutex<Vec<u64>>>,
    /// 這台裝置的 to-device 佇列：`Fetch` 從這裡給、`ItemsDestroy` 從這裡刪。測試自己塞。
    pub(crate) to_device: Arc<Mutex<Vec<(u64, Value)>>>,
    /// `ItemsDestroy` 銷毀過的 count（照順序）。
    pub(crate) destroyed: Arc<Mutex<Vec<u64>>>,
}

pub(crate) fn start_fake_server(mut peer: MemoryEnd, events: Arc<Mutex<Vec<Value>>>) -> FakeServer {
    let recent_requests = Arc::new(Mutex::new(Vec::new()));
    let subscription_ids = Arc::new(Mutex::new(Vec::new()));
    let early_push: EarlyPush = Arc::new(Mutex::new(None));
    let (outbound, mut outbound_rx) = tokio::sync::mpsc::channel::<Pack>(16);
    let device_subscription_ids = Arc::new(Mutex::new(Vec::new()));
    let to_device: Arc<Mutex<Vec<(u64, Value)>>> = Arc::new(Mutex::new(Vec::new()));
    let destroyed = Arc::new(Mutex::new(Vec::new()));
    let (recents_t, sub_t, early_t) = (
        recent_requests.clone(),
        subscription_ids.clone(),
        early_push.clone(),
    );
    let (device_subs_t, to_device_t, destroyed_t) = (
        device_subscription_ids.clone(),
        to_device.clone(),
        destroyed.clone(),
    );
    let task = tokio::spawn(async move {
        loop {
            let pack = tokio::select! {
                frame = peer.source.receive() => match frame {
                    Ok(Some(bytes)) => Pack::decode(&bytes).unwrap(),
                    _ => return,
                },
                pushed = outbound_rx.recv() => match pushed {
                    Some(pack) => {
                        peer.sink.send(pack.encode().unwrap()).await.unwrap();
                        continue;
                    }
                    None => return,
                },
            };
            let reply = match (pack.kind, pack.subtype) {
                (Kind::Control, control::HELLO) => response(
                    Kind::Control,
                    control::ACK,
                    0,
                    pack.seq,
                    json!({ "protocol": wbf_sdk::protocol::PROTOCOL_VERSION, "server": "fake", "features": ["recent", "device"],
                            "chunk_size_default": 16, "chunk_size_large": 16, "data_max_bytes": 1048576 }),
                    Vec::new(),
                ),
                (Kind::Control, control::PING) => response(
                    Kind::Control,
                    control::PONG,
                    0,
                    pack.seq,
                    json!({}),
                    Vec::new(),
                ),
                (Kind::Event, event::SUBSCRIBE) => {
                    sub_t.lock().unwrap().push(pack.id);
                    let early = early_t.lock().unwrap().take();
                    if let Some((seq, gap, early_events)) = early {
                        let pushed = push(pack.id, seq, gap, &early_events);
                        peer.sink.send(pushed.encode().unwrap()).await.unwrap();
                    }
                    let latest = events
                        .lock()
                        .unwrap()
                        .iter()
                        .map(g_seq_of)
                        .max()
                        .unwrap_or(0);
                    response(
                        Kind::Control,
                        control::ACK,
                        pack.id,
                        pack.seq,
                        json!({ "latest_g_seq": latest, "joined": 1, "skipped": [] }),
                        Vec::new(),
                    )
                }
                (Kind::Event, event::UNSUBSCRIBE) => response(
                    Kind::Control,
                    control::ACK,
                    pack.id,
                    pack.seq,
                    json!({}),
                    Vec::new(),
                ),
                (Kind::Event, event::RECENT) => {
                    let meta: Value = serde_json::from_slice(&pack.meta).unwrap();
                    let cg_seq = meta["cg_seq"].as_i64();
                    let before = meta["before"].as_i64();
                    let limit = meta["limit"].as_u64().unwrap_or(320) as usize;
                    recents_t.lock().unwrap().push(cg_seq);
                    let mut window: Vec<Value> = events
                        .lock()
                        .unwrap()
                        .iter()
                        .filter(|event| {
                            let g_seq = g_seq_of(event);
                            cg_seq.is_none_or(|cg_seq| g_seq > cg_seq)
                                && before.is_none_or(|before| g_seq < before)
                        })
                        .cloned()
                        .collect();
                    // 新到舊，最多 `limit` 則。
                    window.sort_by_key(|event| std::cmp::Reverse(g_seq_of(event)));
                    let more = window.len() > limit;
                    window.truncate(limit);
                    let seqs: Vec<i64> = window.iter().map(g_seq_of).collect();
                    response(
                        Kind::Event,
                        event::BATCH,
                        pack.id,
                        0,
                        json!({ "tc": window.len(), "bc": window.len(), "fs": seqs.first().copied().unwrap_or(0),
                                "ls": seqs.last().copied().unwrap_or(0), "r": 0, "more": more }),
                        length_prefixed(&window),
                    )
                }
                // 金鑰那半（wbf-to-device.md）：訂閱回 Ack 再 CryptoState；Fetch 給佇列裡比 cd_seq 新的；ItemsDestroy 從佇列刪、回 Ack 再 ItemsDestroyed。
                (Kind::Device, device::SUBSCRIBE) => {
                    device_subs_t.lock().unwrap().push(pack.id);
                    let ack = response(
                        Kind::Control,
                        control::ACK,
                        pack.id,
                        pack.seq,
                        json!({ "latest_cd_seq": 0 }),
                        Vec::new(),
                    );
                    peer.sink.send(ack.encode().unwrap()).await.unwrap();
                    response(
                        Kind::Device,
                        device::CRYPTO_STATE,
                        pack.id,
                        0,
                        json!({ "otk_counts": { "signed_curve25519": 50 }, "unused_fallback_key_types": [], "gap": false }),
                        Vec::new(),
                    )
                }
                (Kind::Device, device::FETCH) => {
                    let meta: Value = serde_json::from_slice(&pack.meta).unwrap();
                    let cd_seq = meta["cd_seq"].as_u64();
                    let window: Vec<(u64, Value)> = to_device_t
                        .lock()
                        .unwrap()
                        .iter()
                        .filter(|(count, _)| cd_seq.is_none_or(|cd_seq| *count > cd_seq))
                        .cloned()
                        .collect();
                    device_batch(pack.id, &window)
                }
                (Kind::Device, device::ITEMS_DESTROY) => {
                    let counts: Vec<u64> = pack
                        .data
                        .chunks_exact(8)
                        .map(|chunk| u64::from_be_bytes(chunk.try_into().unwrap()))
                        .collect();
                    to_device_t
                        .lock()
                        .unwrap()
                        .retain(|(count, _)| !counts.contains(count));
                    destroyed_t.lock().unwrap().extend(counts.iter().copied());
                    let ack = response(
                        Kind::Control,
                        control::ACK,
                        pack.id,
                        pack.seq,
                        json!({}),
                        Vec::new(),
                    );
                    peer.sink.send(ack.encode().unwrap()).await.unwrap();
                    let mut data = Vec::new();
                    for count in &counts {
                        data.extend_from_slice(&count.to_be_bytes());
                    }
                    response(
                        Kind::Device,
                        device::ITEMS_DESTROYED,
                        pack.id,
                        0,
                        json!({ "tc": counts.len(), "bc": counts.len() }),
                        data,
                    )
                }
                (Kind::Device, device::UNSUBSCRIBE) => response(
                    Kind::Control,
                    control::ACK,
                    pack.id,
                    pack.seq,
                    json!({}),
                    Vec::new(),
                ),
                other => panic!("fake server got {other:?}"),
            };
            peer.sink.send(reply.encode().unwrap()).await.unwrap();
        }
    });
    FakeServer {
        recent_requests,
        subscription_ids,
        early_push,
        outbound,
        task,
        device_subscription_ids,
        to_device,
        destroyed,
    }
}

/// 記憶體對接的線，已經 hello 過（生產路徑的 `open_link` 也是開完就 hello、再 `init_connection`）。
pub(crate) async fn memory_client_with_hello(
    events: Arc<Mutex<Vec<Value>>>,
) -> (WbfClient<Channel>, FakeServer) {
    let (client_end, server_end) = memory_pair(64);
    let fake = start_fake_server(server_end, events);
    let link = WsLink::start(client_end.source, client_end.sink, wbf_sdk::no_hook());
    let mut client = WbfClient::new(Channel::WebSocket(Box::new(WsChannel::from_link(link))));
    client.hello("room-sync test", &[]).await.unwrap();
    (client, fake)
}

/// 訂好、線放回池裡；回訂閱的 id 與池。
pub(crate) async fn subscribed(
    core: &Core,
    account: &AccountDir,
    events: &Arc<Mutex<Vec<Value>>>,
) -> (u64, FakeServer, Arc<crate::link_pool::LinkPool>) {
    let (mut client, fake) = memory_client_with_hello(events.clone()).await;
    core.init_connection(account, LinkRole::Subscriptions, &mut client)
        .await
        .expect("subscribe");
    let subscription_id = fake
        .subscription_ids
        .lock()
        .unwrap()
        .last()
        .copied()
        .expect("server got a Subscribe");
    let pool = core.pool_of_account(account).unwrap();
    drop(
        pool.acquire(LinkRole::Subscriptions, || async move { Ok(client) })
            .await
            .unwrap(),
    );
    (subscription_id, fake, pool)
}

pub(crate) async fn wait_for_async<F, Fut>(mut condition: F, what: &str)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    tokio::time::timeout(Duration::from_secs(10), async {
        while !condition().await {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for: {what}"));
}

pub(crate) async fn cg_seq_of(core: &Core, account: &AccountDir) -> Option<i64> {
    let (cache, me) = core.server_cache_and_me(account).unwrap();
    let cg_seq = cache.read().await.get_cg_seq(&me).unwrap();
    cg_seq
}

pub(crate) async fn cached_ids(core: &Core, account: &AccountDir) -> Vec<String> {
    let (cache, me) = core.server_cache_and_me(account).unwrap();
    let messages = cache.read().await.history(&me, ROOM, None, 100).unwrap();
    let mut ids: Vec<String> = messages.into_iter().map(|message| message.id).collect();
    ids.sort();
    ids
}

pub(crate) async fn next_message(seen: &mut tokio::sync::broadcast::Receiver<CoreEvent>) -> String {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let CoreEvent::Message { message, .. } = seen.recv().await.unwrap() {
                return message.id;
            }
        }
    })
    .await
    .expect("a room.message arrives")
}

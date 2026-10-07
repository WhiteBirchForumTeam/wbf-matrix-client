//! 訂閱線的測試共用品（`room_sync.rs`／`key_sync.rs` 的測試）：記憶體對接的假 server（答 Hello、房間的 Subscribe／Recent／Unsubscribe、
//! 金鑰的 Subscribe／Fetch／ItemsDestroy／Unsubscribe）、開好帳號的 `Core`、等待與讀快取的小工具。🚫 只在測試建置。

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use wbf_sdk::channel::{Channel, WsChannel};
use wbf_sdk::client::WbfClient;
use wbf_sdk::login::{Session, SessionBackend};
use wbf_sdk::protocol::{
    BridgedEndpoint, BRIDGE_ACCOUNT_DATA, BRIDGE_JOINED_ROOMS, BRIDGE_KEYS_CLAIM,
    BRIDGE_KEYS_QUERY, BRIDGE_KEYS_UPLOAD, BRIDGE_MEMBERS, BRIDGE_ROOM_STATE,
    BRIDGE_SEND_TO_DEVICE, BRIDGE_STATE_EVENT,
};
use wbf_sdk::transport::{memory_pair, FrameSink, FrameSource, MemoryEnd};
use wbf_sdk::WsLink;
use wbf_wire::pack::{control, device, download, event, flags, upload};
use wbf_wire::{Kind, Pack};

use crate::accounts::AccountDir;
use crate::link_pool::LinkRole;
use crate::{Core, CoreEvent};

/// 本地記住一間房（`room.get` 拿過的樣子）：送文字只看本地記的「加不加密」（wbf_rooms.rs），測試要先「拿過房間」。
pub(crate) async fn remember_room(core: &Core, account: &AccountDir, room: &str, encrypted: bool) {
    let (cache, me) = core.server_cache_and_me(account).unwrap();
    let conversation = wbf_sdk::chat::Conversation {
        id: room.to_string(),
        kind: wbf_sdk::chat::ConversationKind::Group,
        name: None,
        topic: None,
        encrypted,
        member_count: 2,
        my_power_level: 0,
        can_send_message: true,
        direct_peer: None,
    };
    cache
        .run(move |cache| cache.upsert_conversations(&me, &[conversation]))
        .await
        .unwrap();
}

pub(crate) const DEAD: &str = "http://127.0.0.1:1";
pub(crate) const ME: &str = "@a:localhost";
pub(crate) const ROOM: &str = "!r:localhost";
/// 假 server 的 `JoinedRooms` 回 `ROOM` 跟這一間。
pub(crate) const OTHER_ROOM: &str = "!other:localhost";
/// 假 server 的 `GetState` 給的房名。
pub(crate) const FAKE_ROOM_NAME: &str = "fake room";

pub(crate) fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("wbf-core-roomsync-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// 開好一個 wbf 帳號（session 封好、`m/` 的 crypto store 建好，跟 login 一樣）。
pub(crate) async fn core_with_wbf_account(dir: &std::path::Path) -> (Core, AccountDir) {
    core_with_account_on(dir, DEAD, SessionBackend::WbfSdk).await
}

/// 同上，server 與 backend 自己給（例：傳統下載要一台真的回 HTTP 的假 server；一般 Matrix 帳號是 `MatrixSdkClient`）。
pub(crate) async fn core_with_account_on(
    dir: &std::path::Path,
    server: &str,
    backend: SessionBackend,
) -> (Core, AccountDir) {
    wbf_sdk::vault::Vault::create(dir, &wbf_sdk::Unlock::NoPassphrase).unwrap();
    let core = Core::open(dir);
    core.unlock(None).unwrap();
    let account = add_account_on(&core, dir, server, ME, backend).await;
    crate::accounts::write_current(dir, &account).unwrap();
    (core, account)
}

/// 在已經解鎖的 core 裡多開一個帳號（同一台 server 就共用 `cache.db` 與池）；🚫 改 current。
pub(crate) async fn add_account_on(
    core: &Core,
    dir: &std::path::Path,
    server: &str,
    user: &str,
    backend: SessionBackend,
) -> AccountDir {
    let vault = core.vault().unwrap();
    let account = AccountDir::locate(dir, &vault.account_dir_key(), server, user).unwrap();
    std::fs::create_dir_all(&account.dir).unwrap();
    vault
        .seal_session(
            &account.session_path(),
            &Session {
                server: server.to_string(),
                user_id: user.to_string(),
                device_id: "DEV".to_string(),
                access_token: "syt_memory".to_string(),
                store_dir: None,
                backend: Some(backend),
            },
        )
        .unwrap();
    // wbf 帳號登入時就有 crypto store（login_ops::create_crypto_store_for）；引擎本身丟掉，`olm_engine_of` 要用再開。
    wbf_sdk::crypto_engine::OlmEngine::open(
        &account.matrix_store_dir(),
        &vault.matrix_store_key(),
        user,
        "DEV",
    )
    .await
    .unwrap();
    account
}

/// 橋 `Members` 的 body：房裡只有自己。假 server 的橋不存金鑰（查什麼都回空的），所以裝置雜湊是「沒有任何金鑰」的那個值。
pub(crate) fn members_body(room_version: u64) -> Value {
    let hash = wbf_sdk::device_version::compute_device_keys_hash(ME, &json!({})).unwrap();
    json!({
        "chunk": [{
            "type": "m.room.member", "state_key": ME, "content": { "membership": "join" },
            "unsigned": { "org.wbftw.device_version": format!("1-{hash}") }
        }],
        "org.wbftw.room_version": room_version
    })
}

/// 兩條記憶體對接的線放進池裡：`Misc`（房間狀態、送事件）與 `Upload`（上傳）。各是一台假 server，狀態不共用——
/// 送事件那台不驗 mxc，所以附件宣告只看它帶了什麼。房間版本號 7、成員只有自己。
pub(crate) async fn misc_and_upload(
    core: &Core,
    account: &AccountDir,
    encrypted: bool,
) -> (FakeServer, FakeServer) {
    let (misc_client, misc) = memory_client_with_hello(Arc::new(Mutex::new(Vec::new()))).await;
    let (upload_client, upload) = memory_client_with_hello(Arc::new(Mutex::new(Vec::new()))).await;
    let pool = core.pool_of_account(account).unwrap();
    drop(
        pool.acquire(LinkRole::Misc, || async move { Ok(misc_client) })
            .await
            .unwrap(),
    );
    drop(
        pool.acquire(LinkRole::Upload, || async move { Ok(upload_client) })
            .await
            .unwrap(),
    );
    misc.room_is_encrypted
        .store(encrypted, std::sync::atomic::Ordering::SeqCst);
    *misc.members.lock().unwrap() = Some(members_body(7));
    *misc.current_room_version.lock().unwrap() = Some(7);
    (misc, upload)
}

/// 一條放進池裡的 `Keys` 線（記憶體對接的假 server）：refresh 走這條（/docs/design/keys/e2ee-rpc.md §3.1），
/// 它的 `Members` 回房間版本號 `room_version`、成員只有自己。測試要讓房間「變了」就改回傳那個的 `members`。
pub(crate) async fn keys_line_with_room(
    core: &Core,
    account: &AccountDir,
    room_version: u64,
) -> FakeServer {
    let (client, fake) = memory_client_with_hello(Arc::new(Mutex::new(Vec::new()))).await;
    drop(
        core.pool_of_account(account)
            .unwrap()
            .acquire(LinkRole::Keys, || async move { Ok(client) })
            .await
            .unwrap(),
    );
    *fake.members.lock().unwrap() = Some(members_body(room_version));
    fake
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
/// 假 server 在回 `Device/Subscribe` 的 Ack 之前先推的那一包：(seq, gap, items)。
pub(crate) type DeviceEarlyPush = Arc<Mutex<Option<(u32, bool, Vec<(u64, Value)>)>>>;

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
    /// 每次 `Device/Fetch` 帶的 `cd_seq`（照順序；被注入失敗的那次也算）。
    pub(crate) fetch_requests: Arc<Mutex<Vec<Option<u64>>>>,
    /// 設了就在回 `Device/Subscribe` 的 Ack 之前先推這一包（seq、gap、items）：造 Ack 之前就到的金鑰推播。
    pub(crate) device_early_push: DeviceEarlyPush,
    /// 下一次 `Device/Fetch` 回 Error（一次性）。
    pub(crate) fail_next_fetch: Arc<std::sync::atomic::AtomicBool>,
    /// 走橋的呼叫：(kind, subtype)，照收到的順序。
    pub(crate) bridge_calls: Arc<Mutex<Vec<(Kind, u8)>>>,
    /// 收過幾個 `Hello`（同一條線只該在開線時 hello 一次，/docs/design/daemon/link-requests.md §7）。
    pub(crate) hellos: Arc<std::sync::atomic::AtomicU32>,
    /// 橋 `Members`（`BRIDGE_MEMBERS`）回的 body；None → `Forbidden`。
    pub(crate) members: Arc<Mutex<Option<Value>>>,
    /// 橋 `GetStateEvent` 問 `m.room.encryption` 時：true 回 megolm 的 content，false 回 404（沒加密）。
    pub(crate) room_is_encrypted: Arc<std::sync::atomic::AtomicBool>,
    /// 加密時 `m.room.encryption` 的 content（預設只有 megolm 的 `algorithm`；測換金鑰期限時加 `rotation_period_*`）。
    pub(crate) encryption_content: Arc<Mutex<Value>>,
    /// `Event/Send`：學 server 的 F4——設了就是這個房目前的房間版本號，加密事件帶的號碼對不上就 1506（帶目前的號碼）。
    pub(crate) current_room_version: Arc<Mutex<Option<u64>>>,
    /// `Event/Send` 收下的：(room_id, type, room_version, txn_id, content)。
    pub(crate) sent_events: SentEvents,
    /// `Event/Send` 每則宣告的附件（跟 `sent_events` 同順序）。
    pub(crate) sent_attachments: Arc<Mutex<Vec<Vec<String>>>>,
    /// `Upload/*` 建的上傳，key 是上傳 id（從 1 開始）。`Download/*` 也從這裡給（測試可以把上傳那台的複製過來）。
    pub(crate) uploads: FakeUploads,
    /// 每次 `Download/Read` 要的 (mxc, 塊號)，照 server 處理的順序。
    pub(crate) download_reads: Arc<Mutex<Vec<(String, u32)>>>,
    /// 每個 `Download/Read` 回答前先拿一個 permit：測試把 permit 收走就卡住下載、一次放一個（`add_permits(1)`）。預設多到用不完。
    pub(crate) read_permits: Arc<tokio::sync::Semaphore>,
    /// 接下來幾個 `Download/Read` 的回覆要弄壞（密文翻一個 bit）：測「一塊壞了重拉一次」。
    pub(crate) corrupt_reads: Arc<std::sync::atomic::AtomicU32>,
    /// 下一個 `Download/Read` 改回這個錯誤（`(名字, code_id)`，例 `("Internal", 1901)`），回一次就清掉：測錯誤碼分流。
    pub(crate) fail_next_read: Arc<Mutex<Option<(&'static str, u64)>>>,
    /// 接下來幾個走橋的呼叫回 `Control/Error`（仍記在 `bridge_calls`）：測「後台送金鑰失敗就隔一段時間重試」。
    pub(crate) fail_bridged: Arc<std::sync::atomic::AtomicU32>,
}

/// `read_permits` 一開始有幾個（測試要卡住就 `acquire_many(READ_PERMITS)` 收走）。
pub(crate) const READ_PERMITS: u32 = 1 << 20;

/// 假 server 上的一個上傳：收到的密文塊照索引排、`Seal` 帶的描述。
#[derive(Clone, Debug, Default)]
pub(crate) struct FakeUpload {
    pub(crate) mxc: String,
    /// 串流是 None。
    pub(crate) chunk_count: Option<u32>,
    pub(crate) chunks: Vec<Vec<u8>>,
    pub(crate) finished: bool,
    pub(crate) sealed: bool,
    /// 測試設：server 說這個上傳**之前**被截斷過——只有 `Status` 的 Ack 帶（模擬截斷發生在被跳過的上一輪，這一輪的 `Chunk` Ack 不會再說）。
    pub(crate) truncated: bool,
    pub(crate) chunk_size: u32,
    /// 串流是 None。
    pub(crate) file_size: Option<u64>,
    /// 加密的描述：`Seal` 帶的，沒有就用 `Create` 帶的（`Download/Info` 回這個）。
    pub(crate) description: Vec<u8>,
}

pub(crate) type FakeUploads = Arc<Mutex<std::collections::BTreeMap<u64, FakeUpload>>>;

pub(crate) type SentEvents = Arc<Mutex<Vec<(String, String, Option<u64>, String, Vec<u8>)>>>;

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
    let fetch_requests: Arc<Mutex<Vec<Option<u64>>>> = Arc::new(Mutex::new(Vec::new()));
    let device_early_push: DeviceEarlyPush = Arc::new(Mutex::new(None));
    let fail_next_fetch = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (fetches_t, device_early_t, fail_fetch_t) = (
        fetch_requests.clone(),
        device_early_push.clone(),
        fail_next_fetch.clone(),
    );
    let bridge_calls: Arc<Mutex<Vec<(Kind, u8)>>> = Arc::new(Mutex::new(Vec::new()));
    let hellos = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let hellos_t = hellos.clone();
    let members: Arc<Mutex<Option<Value>>> = Arc::new(Mutex::new(None));
    let room_is_encrypted = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let encryption_content = Arc::new(Mutex::new(json!({ "algorithm": "m.megolm.v1.aes-sha2" })));
    let encryption_content_t = encryption_content.clone();
    let current_room_version: Arc<Mutex<Option<u64>>> = Arc::new(Mutex::new(None));
    let sent_events: SentEvents = Arc::new(Mutex::new(Vec::new()));
    let sent_attachments: Arc<Mutex<Vec<Vec<String>>>> = Arc::new(Mutex::new(Vec::new()));
    let uploads: FakeUploads = Arc::new(Mutex::new(std::collections::BTreeMap::new()));
    let (attachments_t, uploads_t) = (sent_attachments.clone(), uploads.clone());
    let download_reads: Arc<Mutex<Vec<(String, u32)>>> = Arc::new(Mutex::new(Vec::new()));
    let read_permits = Arc::new(tokio::sync::Semaphore::new(READ_PERMITS as usize));
    let (reads_t, permits_t) = (download_reads.clone(), read_permits.clone());
    let corrupt_reads = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let corrupt_t = corrupt_reads.clone();
    let fail_next_read: Arc<Mutex<Option<(&'static str, u64)>>> = Arc::new(Mutex::new(None));
    let fail_read_t = fail_next_read.clone();
    let fail_bridged = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let fail_bridged_t = fail_bridged.clone();
    let (bridge_t, members_t, encrypted_t, room_version_t, sent_t) = (
        bridge_calls.clone(),
        members.clone(),
        room_is_encrypted.clone(),
        current_room_version.clone(),
        sent_events.clone(),
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
            if pack.flags & flags::IS_BRIDGED != 0 {
                bridge_t.lock().unwrap().push((pack.kind, pack.subtype));
                let fail = fail_bridged_t
                    .fetch_update(
                        std::sync::atomic::Ordering::SeqCst,
                        std::sync::atomic::Ordering::SeqCst,
                        |left| left.checked_sub(1),
                    )
                    .is_ok();
                let reply = if fail {
                    let mut error = injected_error(pack.id, pack.seq);
                    error.flags |= flags::IS_BRIDGED;
                    error
                } else {
                    bridged_reply(&pack, &members_t, &encrypted_t, &encryption_content_t)
                };
                peer.sink.send(reply.encode().unwrap()).await.unwrap();
                continue;
            }
            let reply = match (pack.kind, pack.subtype) {
                (Kind::Event, event::SEND) => {
                    let meta: Value = serde_json::from_slice(&pack.meta).unwrap();
                    let room_id = meta["room_id"].as_str().unwrap_or_default().to_string();
                    let event_type = meta["type"].as_str().unwrap_or_default().to_string();
                    let txn_id = meta["txn_id"].as_str().unwrap_or_default().to_string();
                    let room_version = meta["room_version"].as_u64();
                    let current = *room_version_t.lock().unwrap();
                    match current {
                        Some(current)
                            if event_type == "m.room.encrypted"
                                && room_version != Some(current) =>
                        {
                            response(
                                Kind::Control,
                                control::ERROR,
                                pack.id,
                                pack.seq,
                                json!({ "code": "RoomDevicesChanged", "code_id": 1506,
                                        "message": "the room devices changed", "room_version": current }),
                                Vec::new(),
                            )
                        }
                        _ => {
                            let declared = meta["attachments"]
                                .as_array()
                                .map(|mxcs| {
                                    mxcs.iter()
                                        .filter_map(|mxc| mxc.as_str().map(str::to_string))
                                        .collect()
                                })
                                .unwrap_or_default();
                            attachments_t.lock().unwrap().push(declared);
                            let mut sent = sent_t.lock().unwrap();
                            sent.push((
                                room_id,
                                event_type,
                                room_version,
                                txn_id,
                                pack.data.to_vec(),
                            ));
                            response(
                                Kind::Control,
                                control::ACK,
                                pack.id,
                                pack.seq,
                                json!({ "event_id": format!("$sent-{}", sent.len()) }),
                                Vec::new(),
                            )
                        }
                    }
                }
                (Kind::Upload, _) => upload_reply(&pack, &uploads_t),
                (Kind::Download, download::READ) => {
                    if let Ok(permit) = permits_t.acquire().await {
                        permit.forget();
                    }
                    if let Some((code, code_id)) = fail_read_t.lock().unwrap().take() {
                        reads_t
                            .lock()
                            .unwrap()
                            .push(("<failed>".to_string(), u32::MAX));
                        response(
                            Kind::Control,
                            control::ERROR,
                            pack.id,
                            pack.seq,
                            json!({ "code": code, "code_id": code_id, "message": "fake failure" }),
                            Vec::new(),
                        )
                    } else {
                        let mut reply = download_reply(&pack, &uploads_t, &reads_t);
                        let corrupt = corrupt_t
                            .fetch_update(
                                std::sync::atomic::Ordering::SeqCst,
                                std::sync::atomic::Ordering::SeqCst,
                                |left| left.checked_sub(1),
                            )
                            .is_ok();
                        if corrupt && !reply.data.is_empty() {
                            reply.data[0] ^= 1;
                        }
                        reply
                    }
                }
                (Kind::Download, _) => download_reply(&pack, &uploads_t, &reads_t),
                (Kind::Control, control::HELLO) => {
                    hellos_t.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    response(
                        Kind::Control,
                        control::ACK,
                        0,
                        pack.seq,
                        json!({ "protocol": wbf_sdk::protocol::PROTOCOL_VERSION, "server": "fake", "features": ["recent", "device", "bridge", "attachments"],
                            "chunk_size_default": 16, "chunk_size_large": 16, "data_max_bytes": 1048576 }),
                        Vec::new(),
                    )
                }
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
                    let early = device_early_t.lock().unwrap().take();
                    if let Some((seq, gap, items)) = early {
                        let pushed = device_push(pack.id, seq, gap, &items);
                        peer.sink.send(pushed.encode().unwrap()).await.unwrap();
                    }
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
                    fetches_t.lock().unwrap().push(cd_seq);
                    if fail_fetch_t.swap(false, std::sync::atomic::Ordering::SeqCst) {
                        peer.sink
                            .send(injected_error(pack.id, pack.seq).encode().unwrap())
                            .await
                            .unwrap();
                        continue;
                    }
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
        fetch_requests,
        device_early_push,
        fail_next_fetch,
        bridge_calls,
        hellos,
        members,
        room_is_encrypted,
        encryption_content,
        current_room_version,
        sent_events,
        sent_attachments,
        uploads,
        download_reads,
        read_permits,
        corrupt_reads,
        fail_next_read,
        fail_bridged,
    }
}

/// `Download/*` 的回答（wbfuwunel 的 /docs/design/chunked-upload-spec.md §4）：從上傳收下的塊給。
fn download_reply(
    pack: &Pack,
    uploads: &FakeUploads,
    reads: &Arc<Mutex<Vec<(String, u32)>>>,
) -> Pack {
    let uploads = uploads.lock().unwrap();
    let meta: Value = serde_json::from_slice(&pack.meta).unwrap();
    let mxc = meta["mxc"].as_str().unwrap_or_default();
    let Some(upload) = uploads
        .values()
        .find(|upload| upload.mxc == mxc && upload.sealed)
    else {
        return response(
            Kind::Control,
            control::ERROR,
            pack.id,
            pack.seq,
            json!({ "code": "NotFound", "code_id": 1404, "message": "no such media" }),
            Vec::new(),
        );
    };
    let total_len: usize = upload.chunks.iter().map(Vec::len).sum();
    match pack.subtype {
        download::INFO => response(
            Kind::Control,
            control::ACK,
            pack.id,
            pack.seq,
            json!({ "total_len": total_len, "file_size": upload.file_size, "chunk_size": upload.chunk_size,
                    "chunk_count": upload.chunks.len(), "truncated": false, "content_type": null,
                    "read_len": 65536, "chunk_size_large": 1048576 }),
            upload.description.clone(),
        ),
        download::READ => {
            let index = meta["chunk"].as_u64().unwrap() as usize;
            reads.lock().unwrap().push((mxc.to_string(), index as u32));
            let data = upload.chunks[index].clone();
            response(
                Kind::Control,
                control::ACK,
                pack.id,
                pack.seq,
                json!({ "chunk": index, "pos": index as u64 * u64::from(upload.chunk_size), "len": data.len(),
                        "chunk_size": upload.chunk_size, "chunk_count": upload.chunks.len(), "total_len": total_len }),
                data,
            )
        }
        other => panic!("fake server got Download subtype {other}"),
    }
}

/// `Upload/*` 的回答（wbfuwunel 的 /docs/design/chunked-upload-spec.md §3）：只照順序收塊、不驗密文。
fn upload_reply(pack: &Pack, uploads: &FakeUploads) -> Pack {
    let mut uploads = uploads.lock().unwrap();
    let ack = |meta: Value| {
        response(
            Kind::Control,
            control::ACK,
            pack.id,
            pack.seq,
            meta,
            Vec::new(),
        )
    };
    let chunk_ack = |upload: &FakeUpload| {
        json!({ "received": upload.chunks.len(), "chunk_count": upload.chunk_count,
                "total_len": upload.chunks.iter().map(Vec::len).sum::<usize>(),
                "finished": upload.finished, "truncated": false })
    };
    match pack.subtype {
        upload::CREATE => {
            let info = wbf_wire::EncryptedFileInfo::from_bytes(&pack.meta).unwrap();
            let id = uploads.len() as u64 + 1;
            let mxc = format!("mxc://fake/{id:016x}");
            uploads.insert(
                id,
                FakeUpload {
                    mxc: mxc.clone(),
                    chunk_count: (info.file_size != 0).then_some(info.chunk_count),
                    chunk_size: info.chunk_size,
                    file_size: (info.file_size != 0).then_some(info.file_size),
                    description: pack.data.to_vec(),
                    ..FakeUpload::default()
                },
            );
            ack(json!({ "id": id, "mxc": mxc, "chunk_size": info.chunk_size,
                        "chunk_max_bytes": 1048576, "expires_at": 0 }))
        }
        upload::CHUNK => {
            let upload = uploads.get_mut(&pack.id).unwrap();
            let received = upload.chunks.len() as u32;
            if pack.seq > received {
                return response(
                    Kind::Control,
                    control::ERROR,
                    pack.id,
                    pack.seq,
                    json!({ "code": "OutOfOrder", "code_id": 1503, "message": "gap", "expected_seq": received }),
                    Vec::new(),
                );
            }
            if pack.seq == received && !upload.finished {
                upload.chunks.push(pack.data.to_vec());
                let count = upload.chunks.len() as u32;
                upload.finished =
                    pack.flags & flags::IS_LAST != 0 || upload.chunk_count == Some(count);
            }
            ack(chunk_ack(upload))
        }
        upload::STATUS => {
            let upload = uploads.get(&pack.id).unwrap();
            let mut meta = chunk_ack(upload);
            meta["chunk_size"] = json!(16);
            meta["truncated"] = json!(upload.truncated);
            meta["file_size"] = Value::Null;
            ack(meta)
        }
        upload::SEAL => {
            let upload = uploads.get_mut(&pack.id).unwrap();
            assert!(upload.finished, "Seal before the last chunk");
            upload.sealed = true;
            if !pack.data.is_empty() {
                upload.description = pack.data.to_vec();
            }
            ack(json!({ "mxc": upload.mxc }))
        }
        other => panic!("fake server got Upload subtype {other}"),
    }
}

/// 走橋的回答（bridge-specs index.md §1.2）：成功 `Control/Ack`、失敗 `Control/Error`，都帶 bit4。只認加密那條路用到的幾支，
/// 回的是「形狀對」的答案讓狀態機往下走：不存金鑰，查金鑰一律回空的、claim 一律回沒有。
fn bridged_reply(
    pack: &Pack,
    members: &Arc<Mutex<Option<Value>>>,
    room_is_encrypted: &Arc<std::sync::atomic::AtomicBool>,
    encryption_content: &Arc<Mutex<Value>>,
) -> Pack {
    let reply = |subtype: u8, meta: Value, data: Vec<u8>| Pack {
        kind: Kind::Control,
        subtype,
        flags: flags::IS_RESPONSE | flags::IS_BRIDGED,
        id: pack.id,
        seq: pack.seq,
        meta: meta.to_string().into_bytes(),
        data,
    };
    let ok = |body: Value| {
        reply(
            control::ACK,
            json!({ "headers": { "content-type": "application/json" }, "status": 200 }),
            body.to_string().into_bytes(),
        )
    };
    let rejected = |code: &str, code_id: u64, status: u16, errcode: &str| {
        reply(
            control::ERROR,
            json!({ "code": code, "code_id": code_id, "errcode": errcode, "message": errcode, "status": status }),
            json!({ "errcode": errcode, "error": errcode })
                .to_string()
                .into_bytes(),
        )
    };
    // 編號用 sdk 的具名常數（wbfuwunel bridge-specs），🚫 不手寫 hex：常數改了這裡跟著走（PR #62 審查 rumia／cirno 🟡2）。
    let is =
        |endpoint: BridgedEndpoint| (pack.kind, pack.subtype) == (endpoint.kind, endpoint.subtype);
    if is(BRIDGE_MEMBERS) {
        match members.lock().unwrap().clone() {
            Some(body) => ok(body),
            None => rejected("Forbidden", 1302, 403, "M_FORBIDDEN"),
        }
    } else if is(BRIDGE_STATE_EVENT) {
        if room_is_encrypted.load(std::sync::atomic::Ordering::SeqCst) {
            ok(encryption_content.lock().unwrap().clone())
        } else {
            rejected("NotFound", 1501, 404, "M_NOT_FOUND")
        }
    } else if is(BRIDGE_JOINED_ROOMS) {
        ok(json!({ "joined_rooms": [ROOM, OTHER_ROOM] }))
    } else if is(BRIDGE_ROOM_STATE) {
        let mut state = vec![
            json!({ "type": "m.room.create", "state_key": "", "sender": ME, "content": { "room_version": "10" } }),
            json!({ "type": "m.room.member", "state_key": ME, "sender": ME, "content": { "membership": "join" } }),
            json!({ "type": "m.room.name", "state_key": "", "sender": ME, "content": { "name": FAKE_ROOM_NAME } }),
        ];
        if room_is_encrypted.load(std::sync::atomic::Ordering::SeqCst) {
            state.push(
                json!({ "type": "m.room.encryption", "state_key": "", "sender": ME,
                               "content": encryption_content.lock().unwrap().clone() }),
            );
        }
        ok(Value::Array(state))
    } else if is(BRIDGE_ACCOUNT_DATA) {
        rejected("NotFound", 1501, 404, "M_NOT_FOUND")
    } else if is(BRIDGE_KEYS_UPLOAD) {
        ok(json!({ "one_time_key_counts": { "signed_curve25519": 50 } }))
    } else if is(BRIDGE_KEYS_QUERY) {
        // 問到的每個人都要有一個條目（哪怕是空的）：上游對沒回答的人會一直重查。
        let asked: Value = serde_json::from_slice(&pack.data).unwrap_or_default();
        let device_keys: serde_json::Map<String, Value> = asked["device_keys"]
            .as_object()
            .map(|users| users.keys().map(|user| (user.clone(), json!({}))).collect())
            .unwrap_or_default();
        ok(
            json!({ "device_keys": device_keys, "failures": {}, "master_keys": {},
                   "self_signing_keys": {}, "user_signing_keys": {} }),
        )
    } else if is(BRIDGE_KEYS_CLAIM) {
        ok(json!({ "one_time_keys": {}, "failures": {} }))
    } else if is(BRIDGE_SEND_TO_DEVICE) {
        ok(json!({}))
    } else {
        rejected("UnknownKind", 1101, 400, "M_UNRECOGNIZED")
    }
}

/// 假 server 注入的失敗：一則 `Control/Error`（`Internal`）。
fn injected_error(id: u64, seq: u32) -> Pack {
    response(
        Kind::Control,
        control::ERROR,
        id,
        seq,
        json!({ "code": "Internal", "code_id": 1000, "message": "injected failure" }),
        Vec::new(),
    )
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

/// 房間那條線訂好、線放回池裡；回 `Event/Subscribe` 的 id 與池。
pub(crate) async fn subscribed(
    core: &Core,
    account: &AccountDir,
    events: &Arc<Mutex<Vec<Value>>>,
) -> (u64, FakeServer, Arc<crate::link_pool::LinkPool>) {
    let (mut client, fake) = memory_client_with_hello(events.clone()).await;
    core.init_connection(account, LinkRole::Rooms, &mut client)
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
        pool.acquire(LinkRole::Rooms, || async move { Ok(client) })
            .await
            .unwrap(),
    );
    (subscription_id, fake, pool)
}

/// 金鑰那條線訂好（含上線追平）、線放進池裡；回 `Device/Subscribe` 的 id 與池。
/// ⚠️ 跟正式路徑（`open_link`）同一個順序：`init_connection` 在 `acquire` 的開線閉包裡跑、握著那一格——
/// 收金鑰的 task 一起來就 `reuse` 那格時會等到線放進去，🚫 不會先看到空格（先 init 再放的話，第一個 `CryptoState` 的補上傳會落空）。
pub(crate) async fn subscribed_keys(
    core: &Core,
    account: &AccountDir,
    events: &Arc<Mutex<Vec<Value>>>,
) -> (u64, FakeServer, Arc<crate::link_pool::LinkPool>) {
    let (mut client, fake) = memory_client_with_hello(events.clone()).await;
    let pool = core.pool_of_account(account).unwrap();
    drop(
        pool.acquire(LinkRole::Keys, || async move {
            core.init_connection(account, LinkRole::Keys, &mut client)
                .await?;
            Ok(client)
        })
        .await
        .expect("subscribe keys"),
    );
    let device_subscription_id = fake
        .device_subscription_ids
        .lock()
        .unwrap()
        .last()
        .copied()
        .expect("server got a Device/Subscribe");
    (device_subscription_id, fake, pool)
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

/// 這個 mxc 的 `media.download` 推播照帳號分好，等到 `users` 每個都收到結束的那一則（`complete`／`cancelled`／`failed`），最多 10 秒。
pub(crate) async fn states_by_user_until_each_ends(
    events: &mut tokio::sync::broadcast::Receiver<CoreEvent>,
    mxc: &str,
    users: &[&str],
) -> std::collections::HashMap<String, Vec<crate::event::DownloadState>> {
    use crate::event::DownloadState;
    tokio::time::timeout(Duration::from_secs(10), async {
        let mut seen: std::collections::HashMap<String, Vec<DownloadState>> =
            std::collections::HashMap::new();
        let ended = |states: &Vec<DownloadState>| {
            states.last().is_some_and(|state| {
                matches!(
                    state,
                    DownloadState::Complete | DownloadState::Cancelled | DownloadState::Failed
                )
            })
        };
        loop {
            if users.iter().all(|user| seen.get(*user).is_some_and(&ended)) {
                return seen;
            }
            if let Ok(CoreEvent::MediaDownload {
                user,
                mxc: event_mxc,
                state,
                ..
            }) = events.recv().await
            {
                if event_mxc == mxc {
                    seen.entry(user).or_default().push(state);
                }
            }
        }
    })
    .await
    .expect("every account hears how the download ended")
}

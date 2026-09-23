//! 訂閱線的內容：房間事件的訂閱與推播寫進 `cache.db`（daemon-runtime 第 6／7 階段；server 的語意在 wbfuwunel `wbf-event-push.md`）。
//!
//! 維護者 2026-09-22／23 定的形狀（room-sync.md §0）：
//!
//! - 池開線走一支通用的 [`Core::init_connection`]：hello 之後看角色。`Subscriptions` 就送 `Event/Subscribe`、起一個收推播的 task；其他角色不做事。
//!   線死了、下次要用重開時自然重訂（link-pool.md §3）。訂閱會話結束（server 送 `Error`）時 socket 可能還活著，池看不出來——
//!   task 收攤時自己把那格關掉，「重開就重訂」在這條路才成立；🚫 關線不是重訂（to-device-client.md §5.1：被接手的不重訂），重訂要等下一個 `open_subscriptions`。
//! - **daemon 只管訂閱當下**：一包來寫一包、commit 之後發 `room.message`。**🚫 不碰水位、不記洞、不補窗**。
//! - 水位（`cg_seq`）只由 UI 叫的 `sync.recent` 動；推播漏掉的（server 的 `gap`、本地丟包、一包解不開、寫失敗）**都不管**：
//!   UI 下次叫 `Recent` 會從它自己決定的起點重拉那一段（冪等），UI 不叫就不補，永遠拿不到也不管。誰記有沒有漏是 UI 層的事。
//!
//! 事件跟 `Recent` 那條路一樣**原樣**寫（密文不解，local-cache-db.md §7.2）；訂閱是純的，`seq` 跳號不管、`Subscribe` 不帶 `cg_seq`。

use std::sync::Arc;
use std::time::Duration;

use wbf_sdk::channel::Channel;
use wbf_sdk::client::{RoomSubscription, WbfClient};
use wbf_sdk::event_json::messages_from_incoming;
use wbf_sdk::protocol::{EventSubscribeReply, PushMeta};
use wbf_sdk::{IncomingEvent, Transport};

use crate::accounts::AccountDir;
use crate::backend_choice::MethodHome;
use crate::error::CoreError;
use crate::event::{EventSink, LinkState};
use crate::link_pool::{LinkPool, LinkRole};
use crate::server_cache::ServerCache;
use crate::{Core, CoreEvent, Target};

/// 等 `Subscribe` 的 Ack 最久多久。
const ACK_TIMEOUT: Duration = Duration::from_secs(30);
/// 訂閱 task 每次等推播最久多久：到了只是「這段時間沒事」，繼續等（心跳另外在線上跑）。
const PUSH_IDLE_TIMEOUT: Duration = Duration::from_secs(60);
/// 關訂閱線時等 task 收攤最久多久；不肯就 abort。
const STOP_TIMEOUT: Duration = Duration::from_secs(5);

/// 一個帳號的收推播 task。丟掉就 abort（`Core` 丟掉、或線重開換新的一個）。
pub(crate) struct RoomSyncHandle {
    task: tokio::task::JoinHandle<()>,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
}

impl Drop for RoomSyncHandle {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// task 自己握著的東西——🚫 不握 `Core`（task 是 `'static`），也不握線（線在池裡；訂閱是線上的一個會話，guard 放掉之後線照樣能用）。
struct RoomSyncTask {
    me: String,
    cache: Arc<ServerCache>,
    events: EventSink,
    /// 這個帳號的池：只為了收攤時把訂閱那格關掉（模組註解）。
    pool: Arc<LinkPool>,
}

impl Core {
    /// 池開完一條線的通用初始化（維護者 2026-09-22）：看角色決定還要做什麼。
    ///
    /// | 角色 | 做什麼 |
    /// |---|---|
    /// | `Subscriptions` | `Event/Subscribe`（帳號層、不帶 `cg_seq`）→ Ack 之後起收推播的 task（之前有的話換掉：它的線已經死了）；訂閱結束時 task 關這格線 |
    /// | 其他 | 不做事 |
    ///
    /// Args:
    ///     role: 這條線的角色, example: LinkRole::Subscriptions
    ///     client: 已經 hello 過的線
    /// Return:
    ///     Ok(())
    ///     Err(Network)    Ack 沒等到、線死了（池就當這條沒開成）
    ///     Err(Server)     server 拒
    ///     Err(AccountBusy) 正在登出：不替它訂
    pub(crate) async fn init_connection(
        &self,
        account: &AccountDir,
        role: LinkRole,
        client: &mut WbfClient<Channel>,
    ) -> Result<(), CoreError> {
        if role != LinkRole::Subscriptions {
            return Ok(());
        }
        let (cache, me) = self.server_cache_and_me(account)?;
        let pool = self.pool_of_account(account)?;
        let subscription = client.room_subscription(None, ACK_TIMEOUT).await?;
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let task = RoomSyncTask {
            me,
            cache,
            events: self.events.clone(),
            pool,
        };
        let handle = RoomSyncHandle {
            task: tokio::spawn(task.run(subscription, stopped)),
            stop: Some(stop),
        };
        // 舊的（線死了留下來的）在這裡被 Drop、abort。
        let _previous = self
            .room_syncs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(account.dir.clone(), handle);
        Ok(())
    }

    /// 開訂閱線（開的時候 `init_connection` 就訂了）。已經開著就是 no-op。
    ///
    /// Return:
    ///     Ok(())
    ///     Err(Usage)      沒登入、不是 wbf server（訂閱只有 wbf 講得出來）
    ///     Err(Network)    線開不起來、Ack 沒等到
    pub async fn open_subscriptions(&self, target: &Target) -> Result<(), CoreError> {
        let account = self.account_or_current(target)?;
        let _line = self
            .client_of(
                &account,
                Transport::WebSocket,
                MethodHome::WbfSdkOnly,
                LinkRole::Subscriptions,
            )
            .await?;
        Ok(())
    }

    /// 關訂閱線：先收 task，再關線（斷線 server 自動退訂）。
    ///
    /// Return:
    ///     Ok(true)    本來開著，關了
    ///     Ok(false)   本來就沒開
    pub async fn close_subscriptions(&self, target: &Target) -> Result<bool, CoreError> {
        let account = self.account_or_current(target)?;
        self.stop_room_sync_of(&account).await;
        let pool = self
            .link_pools
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&account.dir)
            .cloned();
        Ok(match pool {
            Some(pool) => {
                pool.close(LinkRole::Subscriptions, "closed on request")
                    .await
            }
            None => false,
        })
    }

    /// 收掉這個帳號的收推播 task（關線、登出用）。
    ///
    /// Return:
    ///     bool  true ＝ 本來在跑
    pub(crate) async fn stop_room_sync_of(&self, account: &AccountDir) -> bool {
        let handle = self
            .room_syncs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&account.dir);
        let Some(mut handle) = handle else {
            return false;
        };
        if handle.task.is_finished() {
            return false;
        }
        if let Some(stop) = handle.stop.take() {
            let _ = stop.send(());
        }
        let _ = tokio::time::timeout(STOP_TIMEOUT, &mut handle.task).await;
        true
    }

    /// Return:
    ///     bool  true ＝ 這個帳號的收推播 task 還在（只給測試斷言用；生產路徑看 `link.state`）
    #[cfg(test)]
    pub(crate) fn is_room_syncing(&self, account: &AccountDir) -> bool {
        self.room_syncs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&account.dir)
            .is_some_and(|handle| !handle.task.is_finished())
    }
}

impl RoomSyncTask {
    async fn run(
        self,
        mut subscription: RoomSubscription,
        mut stopped: tokio::sync::oneshot::Receiver<()>,
    ) {
        // Ack 之前就推來的：跟之後的一樣處理。
        let early: Vec<_> = subscription.early_pushes.drain(..).collect();
        for (meta, events) in early {
            self.on_push(meta, events).await;
        }
        loop {
            // 本地收件匣滿過＝丟過包：講一聲就好——補不補是 UI 的事（模組註解）。
            if subscription.take_gap() {
                self.events
                    .progress("room sync: the subscription inbox overflowed; some pushes were dropped (sync.recent refetches them)");
            }
            let next = tokio::select! {
                _ = &mut stopped => return,
                next = subscription.next(PUSH_IDLE_TIMEOUT) => next,
            };
            match next {
                Ok(Some(EventSubscribeReply::Push { meta, events })) => {
                    self.on_push(meta, events).await
                }
                // 金鑰那支才消費；這裡只確認它不會把 task 弄死。
                Ok(Some(EventSubscribeReply::DeviceChanged(_)))
                | Ok(Some(EventSubscribeReply::Acknowledged(_)))
                | Ok(Some(EventSubscribeReply::Unsubscribed { .. })) => {}
                Err(wbf_sdk::SdkError::Timeout(_)) => {}
                // 一包壞了就是漏一包：講一聲，繼續收。
                Err(wbf_sdk::SdkError::Protocol(why)) => {
                    self.events.progress(format!(
                        "room sync: a push could not be read and is dropped ({why}); sync.recent refetches it"
                    ));
                }
                Ok(None) | Err(_) => {
                    let why = match next {
                        Ok(None) => "the server ended the subscription".to_string(),
                        Err(error) => error.to_string(),
                        Ok(Some(_)) => unreachable!("handled above"),
                    };
                    let reason = format!("the room subscription ended: {why}");
                    // 訂閱會話結束了 socket 可能還活著（server 送 Error，例如被另一台裝置接手），池的殞死偵測看不出來：
                    // 這裡把那格關掉、池發 closed；下次 open_subscriptions 才重開、重訂（🚫 不在這裡重訂）。
                    // 線不在池裡（已經被別人關了）就只講一聲。
                    if !self.pool.close(LinkRole::Subscriptions, &reason).await {
                        self.events.emit(CoreEvent::Link {
                            user: self.me.clone(),
                            role: LinkRole::Subscriptions,
                            state: LinkState::Closed,
                            reason: Some(reason),
                        });
                    }
                    return;
                }
            }
        }
    }

    /// 一包（Ack 之前推來的也走這裡）：`gap` 只講一聲；原樣寫進 cache（照房分組）、commit 之後發 `room.message`。🚫 不碰水位（模組註解）。
    ///
    /// 存不了的（沒 `room_id`：不猜房間；沒 `event_id`／`sender`：`upsert_events` 不寫）在分組時就擋掉——
    /// 🚫 不進 `by_room`，所以不會替一則不在庫裡的事件發 `room.message`；數出來、講出來（`Note`）。規則只有一份：`storable_identity`。
    async fn on_push(&self, meta: PushMeta, events: Vec<serde_json::Value>) {
        if meta.gap {
            self.events.progress(
                "room sync: the server dropped pushes before this one (sync.recent refetches them)",
            );
        }
        let mut by_room: std::collections::BTreeMap<String, Vec<IncomingEvent>> =
            std::collections::BTreeMap::new();
        let mut unstorable = 0usize;
        for raw in events {
            let room = raw
                .get("room_id")
                .and_then(|value| value.as_str())
                .map(str::to_string);
            let incoming = IncomingEvent::from_ws_json(raw);
            match room {
                Some(room) if incoming.storable_identity().is_some() => {
                    by_room.entry(room).or_default().push(incoming)
                }
                _ => unstorable += 1,
            }
        }
        // 通知給折好的訊息（自己送的也發，收的人自己濾）；庫裡存原樣。
        let notices: Vec<CoreEvent> = by_room
            .iter()
            .flat_map(|(room, events)| {
                messages_from_incoming(room, events)
                    .into_iter()
                    .map(|message| CoreEvent::Message {
                        user: self.me.clone(),
                        message: Box::new(message),
                    })
            })
            .collect();
        let me = self.me.clone();
        let written = self
            .cache
            .run(move |cache| {
                let mut unstorable = 0usize;
                for (room, events) in &by_room {
                    unstorable += cache.upsert_events_counted(&me, room, events)?.unstorable;
                }
                Ok(unstorable)
            })
            .await;
        match written {
            Ok(unstorable_in_cache) => {
                // commit 之後才發（PR #32 的規矩）。
                for notice in notices {
                    self.events.emit(notice);
                }
                let dropped = unstorable + unstorable_in_cache;
                if dropped > 0 {
                    self.events.progress(format!(
                        "room sync: {dropped} pushed event(s) could not be stored (no room_id, event_id or sender) and are dropped"
                    ));
                }
            }
            // 寫失敗只報不擋（快取壞了的代價是重拉：UI 的 sync.recent）；沒落地的就不通知。
            Err(error) => self
                .events
                .progress(format!("room sync: cache write failed (ignored): {error}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use serde_json::{json, Value};
    use wbf_sdk::channel::{Channel, WsChannel};
    use wbf_sdk::client::WbfClient;
    use wbf_sdk::login::{Session, SessionBackend};
    use wbf_sdk::transport::{memory_pair, FrameSink, FrameSource, MemoryEnd};
    use wbf_sdk::{RecentPlan, Transport, WsLink};
    use wbf_wire::pack::{control, event, flags};
    use wbf_wire::{Kind, Pack};

    use crate::accounts::AccountDir;
    use crate::event::LinkState;
    use crate::link_pool::LinkRole;
    use crate::{Core, CoreEvent, Target};

    const DEAD: &str = "http://127.0.0.1:1";
    const ME: &str = "@a:localhost";
    const ROOM: &str = "!r:localhost";

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("wbf-core-roomsync-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn core_with_wbf_account(dir: &std::path::Path) -> (Core, AccountDir) {
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
        (core, account)
    }

    /// 一則文字事件，`g_seq` 就是它的號碼（`r_seq` 同號，夠用）。
    fn text_event(g_seq: i64) -> Value {
        json!({
            "type": "m.room.message", "event_id": format!("${g_seq}"), "room_id": ROOM, "sender": "@b:localhost",
            "origin_server_ts": g_seq, "content": { "msgtype": "m.text", "body": format!("event {g_seq}") },
            "unsigned": { wbf_sdk::protocol::R_SEQ_KEY: g_seq, wbf_sdk::protocol::G_SEQ_KEY: g_seq },
        })
    }

    fn g_seq_of(event: &Value) -> i64 {
        event["unsigned"][wbf_sdk::protocol::G_SEQ_KEY]
            .as_i64()
            .unwrap()
    }

    fn length_prefixed(events: &[Value]) -> Vec<u8> {
        let mut data = Vec::new();
        for event in events {
            let bytes = serde_json::to_vec(event).unwrap();
            data.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
            data.extend_from_slice(&bytes);
        }
        data
    }

    fn response(kind: Kind, subtype: u8, id: u64, seq: u32, meta: Value, data: Vec<u8>) -> Pack {
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
    fn push(subscription_id: u64, seq: u32, gap: bool, events: &[Value]) -> Pack {
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
    type EarlyPush = Arc<Mutex<Option<(u32, bool, Vec<Value>)>>>;

    /// 假的 server 端：答 Hello、Subscribe、Recent（照 `cg_seq` 給比它新的，一窗一個 Batch）、Unsubscribe；
    /// 測試從 `outbound` 塞 server 主動推的包。記下每次 `Recent` 帶的 `cg_seq` 與訂閱的 id。事件清單可以跟別條線共用。
    struct FakeServer {
        recent_requests: Arc<Mutex<Vec<Option<i64>>>>,
        /// 每個 `Subscribe` 的 id（照順序）。
        subscription_ids: Arc<Mutex<Vec<u64>>>,
        /// 設了就在回 `Subscribe` 的 Ack 之前先推這一包（seq、gap、事件）：造 Ack 之前就到的推播。
        early_push: EarlyPush,
        outbound: tokio::sync::mpsc::Sender<Pack>,
        task: tokio::task::JoinHandle<()>,
    }

    fn start_fake_server(mut peer: MemoryEnd, events: Arc<Mutex<Vec<Value>>>) -> FakeServer {
        let recent_requests = Arc::new(Mutex::new(Vec::new()));
        let subscription_ids = Arc::new(Mutex::new(Vec::new()));
        let early_push: EarlyPush = Arc::new(Mutex::new(None));
        let (outbound, mut outbound_rx) = tokio::sync::mpsc::channel::<Pack>(16);
        let (recents_t, sub_t, early_t) = (
            recent_requests.clone(),
            subscription_ids.clone(),
            early_push.clone(),
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
                        json!({ "protocol": wbf_sdk::protocol::PROTOCOL_VERSION, "server": "fake", "features": ["recent"],
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
        }
    }

    /// 記憶體對接的線，已經 hello 過（生產路徑的 `open_link` 也是開完就 hello、再 `init_connection`）。
    async fn memory_client_with_hello(
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
    async fn subscribed(
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

    async fn wait_for_async<F, Fut>(mut condition: F, what: &str)
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

    async fn cg_seq_of(core: &Core, account: &AccountDir) -> Option<i64> {
        let (cache, me) = core.server_cache_and_me(account).unwrap();
        let cg_seq = cache.read().await.get_cg_seq(&me).unwrap();
        cg_seq
    }

    async fn cached_ids(core: &Core, account: &AccountDir) -> Vec<String> {
        let (cache, me) = core.server_cache_and_me(account).unwrap();
        let messages = cache.read().await.history(&me, ROOM, None, 100).unwrap();
        let mut ids: Vec<String> = messages.into_iter().map(|message| message.id).collect();
        ids.sort();
        ids
    }

    async fn next_message(seen: &mut tokio::sync::broadcast::Receiver<CoreEvent>) -> String {
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

    /// 模組註解的形狀：`init_connection` 訂了、🚫 沒叫 Recent → 推一包寫進去、`room.message` 在 commit 之後、**水位不動** →
    /// 帶 gap 的包、壞包（`bc` 對不上）、下一包都一樣：寫得了的寫、講一聲、水位還是不動 → UI 叫 `sync.recent`（帶 `since`）才動水位、才補回漏的 →
    /// 關訂閱線只關那一條。
    #[tokio::test]
    async fn the_task_only_writes_pushes_and_never_touches_the_watermark() {
        let dir = scratch("pure");
        let (core, account) = core_with_wbf_account(&dir);
        let mut seen = core.subscribe();
        let events: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
        let (subscription_id, fake, pool) = subscribed(&core, &account, &events).await;
        assert!(core.is_room_syncing(&account));
        assert!(
            fake.recent_requests.lock().unwrap().is_empty(),
            "補窗不是 daemon 的事：開線不叫 Recent"
        );

        // 一包正常的推播：寫進去、commit 之後才有 room.message、水位不動。
        events.lock().unwrap().push(text_event(4801));
        fake.outbound
            .send(push(subscription_id, 0, false, &[text_event(4801)]))
            .await
            .unwrap();
        assert_eq!(next_message(&mut seen).await, "$4801");
        assert!(cached_ids(&core, &account)
            .await
            .contains(&"$4801".to_string()));
        assert_eq!(cg_seq_of(&core, &account).await, None, "🚫 推播不碰水位");

        // server 說有洞（4850 掉了）、一包壞掉、再一包正常：都寫得了的寫、水位還是不動。
        events
            .lock()
            .unwrap()
            .extend([text_event(4850), text_event(4900), text_event(4950)]);
        fake.outbound
            .send(push(subscription_id, 1, true, &[text_event(4900)]))
            .await
            .unwrap();
        assert_eq!(next_message(&mut seen).await, "$4900");
        let mut broken = push(subscription_id, 2, false, &[text_event(4925)]);
        broken.meta = br#"{"bc":2,"fs":4925,"ls":4925,"gap":false}"#.to_vec();
        fake.outbound.send(broken).await.unwrap();
        fake.outbound
            .send(push(subscription_id, 3, false, &[text_event(4950)]))
            .await
            .unwrap();
        assert_eq!(next_message(&mut seen).await, "$4950");
        assert_eq!(
            cg_seq_of(&core, &account).await,
            None,
            "🚫 有洞、壞包、之後的包：水位一樣不動"
        );
        let ids = cached_ids(&core, &account).await;
        assert!(!ids.contains(&"$4850".to_string()), "漏的 daemon 不補");
        assert!(!ids.contains(&"$4925".to_string()), "壞包丟掉");

        // UI 補：`sync.recent` 帶自己的起點（它手上 room.message 最後一則的 g_seq 之前也行，這裡從頭）→ 4850 回來、水位到 4950。
        let (misc, _fake_misc) = memory_client_with_hello(events.clone()).await;
        drop(
            pool.acquire(LinkRole::Misc, || async move { Ok(misc) })
                .await
                .unwrap(),
        );
        let summary = core
            .recent(
                RecentPlan {
                    max_events: None,
                    ..RecentPlan::default()
                },
                Some(4801),
                false,
                Transport::WebSocket,
                "ui refill",
                &Target::default(),
            )
            .await
            .expect("sync.recent");
        assert_eq!(
            (summary.pulled, summary.cg_seq_before, summary.cg_seq_after),
            (3, Some(4801), Some(4950)),
            "UI 給的起點就是起點"
        );
        assert!(cached_ids(&core, &account)
            .await
            .contains(&"$4850".to_string()));
        assert_eq!(
            cg_seq_of(&core, &account).await,
            Some(4950),
            "水位只由 Recent 動"
        );
        // 之後的推播照樣不碰它。
        events.lock().unwrap().push(text_event(5000));
        fake.outbound
            .send(push(subscription_id, 4, false, &[text_event(5000)]))
            .await
            .unwrap();
        assert_eq!(next_message(&mut seen).await, "$5000");
        assert_eq!(cg_seq_of(&core, &account).await, Some(4950));
        // 沒帶 since：從 daemon 存的水位（4950）起。
        let summary = core
            .recent(
                RecentPlan {
                    max_events: None,
                    ..RecentPlan::default()
                },
                None,
                false,
                Transport::WebSocket,
                "ui refill",
                &Target::default(),
            )
            .await
            .unwrap();
        assert_eq!(
            (summary.cg_seq_before, summary.cg_seq_after),
            (Some(4950), Some(5000))
        );

        // 關訂閱線：task 收掉、那一格 Idle、發 Closed。
        assert!(core.close_subscriptions(&Target::default()).await.unwrap());
        assert!(!core.is_room_syncing(&account));
        assert_eq!(pool.open_count(), 1, "只關訂閱那條，misc 還開著");
        let closed = std::iter::from_fn(|| seen.try_recv().ok()).any(|event| {
            matches!(
                event,
                CoreEvent::Link {
                    role: LinkRole::Subscriptions,
                    state: LinkState::Closed,
                    ..
                }
            )
        });
        assert!(closed, "關線要發 link.state closed");
        assert!(
            !core.close_subscriptions(&Target::default()).await.unwrap(),
            "再關一次：本來就沒開"
        );
        fake.task.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 存不了的事件（沒 `sender`）：寫得了的照寫、有 `Note`、🚫 不替它發 `room.message`。
    #[tokio::test]
    async fn an_event_that_can_never_be_stored_is_reported_and_not_announced() {
        let dir = scratch("unstorable");
        let (core, account) = core_with_wbf_account(&dir);
        let mut seen = core.subscribe();
        let events: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
        let (subscription_id, fake, _pool) = subscribed(&core, &account, &events).await;
        let mut no_sender = text_event(401);
        no_sender.as_object_mut().unwrap().remove("sender");
        fake.outbound
            .send(push(
                subscription_id,
                0,
                false,
                &[no_sender, text_event(400)],
            ))
            .await
            .unwrap();
        assert_eq!(
            next_message(&mut seen).await,
            "$400",
            "沒 sender 的那則不通知"
        );
        let ids = cached_ids(&core, &account).await;
        assert!(ids.contains(&"$400".to_string()));
        assert!(!ids.contains(&"$401".to_string()), "沒 sender 的不寫");
        let reported = std::iter::from_fn(|| seen.try_recv().ok()).any(|event| {
            matches!(event, CoreEvent::Note { text, .. } if text.contains("1 pushed event(s) could not be stored"))
        });
        assert!(reported, "存不了的要講出來");
        fake.task.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 線死了：task 結束、先發一次 `link.state: closed`（不等池下次取用才發現）、`is_room_syncing` 變 false。🚫 沒有背景重連。
    #[tokio::test]
    async fn a_dead_line_ends_the_task_and_says_so() {
        let dir = scratch("dead");
        let (core, account) = core_with_wbf_account(&dir);
        let mut seen = core.subscribe();
        let (mut client, fake) = memory_client_with_hello(Arc::new(Mutex::new(Vec::new()))).await;
        core.init_connection(&account, LinkRole::Subscriptions, &mut client)
            .await
            .expect("subscribe");
        // 對方收攤。
        fake.task.abort();
        drop(fake.outbound);
        wait_for_async(
            || async { !core.is_room_syncing(&account) },
            "the task ends",
        )
        .await;
        let closed = std::iter::from_fn(|| seen.try_recv().ok()).find_map(|event| match event {
            CoreEvent::Link {
                role: LinkRole::Subscriptions,
                state: LinkState::Closed,
                reason,
                ..
            } => Some(reason),
            _ => None,
        });
        assert!(
            closed
                .clone()
                .flatten()
                .is_some_and(|reason| reason.contains("subscription ended")),
            "線死了要講：{closed:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// server 收掉訂閱會話、socket 還活著（例如被另一台裝置接手的 `Error`）：task 把那格關掉（池發 `closed` 帶原因）、🚫 不自己重訂；
    /// 下一次開訂閱線走 `open` → `init_connection` → 第二個 `Subscribe`、新訂閱收得到（PR #58 審查 rumia #655／cirno #658：
    /// 之前 task 只發事件不關線，池看 socket 還活著就把舊線交回去，永遠不再訂）。
    #[tokio::test]
    async fn an_ended_subscription_closes_the_line_so_the_next_open_subscribes_again() {
        let dir = scratch("resubscribe");
        let (core, account) = core_with_wbf_account(&dir);
        let mut seen = core.subscribe();
        let events: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
        let (subscription_id, fake, pool) = subscribed(&core, &account, &events).await;
        assert_eq!(pool.open_count(), 1);

        fake.outbound
            .send(response(
                Kind::Control,
                control::ERROR,
                subscription_id,
                0,
                json!({ "code": "M_UNKNOWN", "message": "subscription superseded by another device" }),
                Vec::new(),
            ))
            .await
            .unwrap();
        wait_for_async(
            || async { !core.is_room_syncing(&account) },
            "the task ends",
        )
        .await;
        assert_eq!(
            pool.open_count(),
            0,
            "訂閱會話死了就把那條線關掉，socket 活著也一樣"
        );
        let closed = std::iter::from_fn(|| seen.try_recv().ok()).find_map(|event| match event {
            CoreEvent::Link {
                role: LinkRole::Subscriptions,
                state: LinkState::Closed,
                reason,
                ..
            } => Some(reason),
            _ => None,
        });
        assert!(
            closed
                .clone()
                .flatten()
                .is_some_and(|reason| reason.contains("subscription ended")),
            "關線要帶原因：{closed:?}"
        );

        // 下一次開：那格是空的 → `open`（生產路徑的 open_link：hello 之後 init_connection）→ 第二個 Subscribe、新的 task。
        let (mut client, fake_again) = memory_client_with_hello(events.clone()).await;
        let (core_ref, account_ref) = (&core, &account);
        drop(
            pool.acquire(LinkRole::Subscriptions, || async move {
                core_ref
                    .init_connection(account_ref, LinkRole::Subscriptions, &mut client)
                    .await?;
                Ok(client)
            })
            .await
            .unwrap(),
        );
        let again = fake_again.subscription_ids.lock().unwrap().last().copied();
        let again = again.expect("重開就重訂：第二個 Subscribe");
        assert!(core.is_room_syncing(&account));
        events.lock().unwrap().push(text_event(9));
        fake_again
            .outbound
            .send(push(again, 0, false, &[text_event(9)]))
            .await
            .unwrap();
        assert_eq!(next_message(&mut seen).await, "$9", "新訂閱收得到");
        fake.task.abort();
        fake_again.task.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Ack 之前就推來的包（server 先登記再回 Ack）跟之後的一樣：帶 `gap` 也要講一聲（PR #58 審查 salvia #656／cirno #658 🟢）。
    #[tokio::test]
    async fn a_gap_in_a_push_before_the_ack_is_reported_too() {
        let dir = scratch("early-gap");
        let (core, account) = core_with_wbf_account(&dir);
        let mut seen = core.subscribe();
        let events: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(vec![text_event(7)]));
        let (mut client, fake) = memory_client_with_hello(events).await;
        *fake.early_push.lock().unwrap() = Some((0, true, vec![text_event(7)]));
        core.init_connection(&account, LinkRole::Subscriptions, &mut client)
            .await
            .expect("subscribe");
        let mut gap_reported = false;
        let first = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                match seen.recv().await.unwrap() {
                    CoreEvent::Note { text, .. } if text.contains("the server dropped pushes") => {
                        gap_reported = true
                    }
                    CoreEvent::Message { message, .. } => return message.id,
                    _ => {}
                }
            }
        })
        .await
        .expect("the early push arrives");
        assert_eq!(first, "$7", "Ack 之前推來的照寫、照通知");
        assert!(gap_reported, "Ack 之前的 gap 也要講");
        fake.task.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 對真的 wbfuwunel：alice 登入、開訂閱線；bob（另一個 `Core`、另一個資料目錄）用 `Event/Send` 送一則；alice 的 `room.message` 在時限內到、
    /// cache 有它、水位不動；關訂閱線；兩邊登出。
    ///
    /// `--ignored`；環境變數：`WBF_E2E_SERVER`、`WBF_E2E_USER`（完整 mxid）、`WBF_E2E_PASSWORD_FILE`、`WBF_E2E_USER_B`、`WBF_E2E_PASSWORD_B_FILE`、
    /// `WBF_E2E_ROOM`（兩人都在的明文房）。
    #[tokio::test]
    #[ignore = "needs a running wbfuwunel: WBF_E2E_SERVER, WBF_E2E_USER, WBF_E2E_PASSWORD_FILE, WBF_E2E_USER_B, WBF_E2E_PASSWORD_B_FILE, WBF_E2E_ROOM"]
    async fn a_subscription_receives_what_another_account_sends_over_the_real_server() {
        let env = |name: &str| std::env::var(name).unwrap_or_else(|_| panic!("{name}"));
        let password_of = |file: &str| {
            let text = std::fs::read_to_string(file).unwrap();
            text.strip_suffix('\n').unwrap_or(&text).to_string()
        };
        let server = env("WBF_E2E_SERVER");
        let room = env("WBF_E2E_ROOM");
        let (user_a, password_a) = (
            env("WBF_E2E_USER"),
            password_of(&env("WBF_E2E_PASSWORD_FILE")),
        );
        let (user_b, password_b) = (
            env("WBF_E2E_USER_B"),
            password_of(&env("WBF_E2E_PASSWORD_B_FILE")),
        );
        let (dir_a, dir_b) = (scratch("real-a"), scratch("real-b"));
        let core_a = Core::open(&dir_a);
        core_a.create_vault(None).unwrap();
        core_a
            .log_in(&server, &user_a, &password_a, "room-sync e2e A", true)
            .await
            .expect("alice logs in");
        let core_b = Core::open(&dir_b);
        core_b.create_vault(None).unwrap();
        core_b
            .log_in(&server, &user_b, &password_b, "room-sync e2e B", true)
            .await
            .expect("bob logs in");
        let account_a = core_a.current_account().unwrap();
        let mut seen = core_a.subscribe();

        core_a
            .open_subscriptions(&Target::default())
            .await
            .expect("subscribe over the real server");
        assert!(core_a.is_room_syncing(&account_a));

        let body = format!("room sync e2e {}", std::process::id());
        let event_id = core_b
            .send_text(&room, &body, &Target::default())
            .await
            .expect("bob sends over Event/Send");
        let arrived = tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                if let CoreEvent::Message { user, message } = seen.recv().await.unwrap() {
                    if message.id == event_id {
                        assert_eq!(user, user_a);
                        return message;
                    }
                }
            }
        })
        .await
        .expect("alice gets bob's message as a push within 20 s");
        assert_eq!(arrived.conversation, room);
        // 讀完就放：登出要關 cache.db，還有人握著它會被拒。
        let cached = {
            let (cache, me) = core_a.server_cache_and_me(&account_a).unwrap();
            let reader = cache.read().await;
            reader
                .list_messages_by_event_ids(&me, &room, std::slice::from_ref(&event_id))
                .unwrap()
        };
        assert_eq!(cached.len(), 1, "commit 之後才發事件，所以此刻庫裡一定有");
        assert_eq!(cg_seq_of(&core_a, &account_a).await, None, "推播不碰水位");

        assert!(core_a
            .close_subscriptions(&Target::default())
            .await
            .unwrap());
        assert!(!core_a.is_room_syncing(&account_a));
        core_a
            .log_out(&user_a, None, true, true)
            .await
            .expect("alice logs out");
        core_b
            .log_out(&user_b, None, true, true)
            .await
            .expect("bob logs out");
        let _ = std::fs::remove_dir_all(&dir_a);
        let _ = std::fs::remove_dir_all(&dir_b);
    }
}

//! 訂閱線的內容：房間事件的訂閱與推播寫進 `cache.db`（daemon-runtime 第 6／7 階段；server 的語意在 wbfuwunel `wbf-event-push.md`）。
//!
//! 維護者 2026-09-22 定的形狀（room-sync.md §0）：
//!
//! - 池開線走一支通用的 [`Core::init_connection`]：hello 之後看角色。`Subscriptions` 就送 `Event/Subscribe`、起一個收推播的 task；其他角色不做事。
//!   線死了、下次要用重開時自然重訂（link-pool.md §3）。
//! - **daemon 只管訂閱當下**：一包來寫一包、commit 之後發 `room.message`、水位往前推到 `fs`。
//! - **補窗全由 UI 叫 `sync.recent`**（冪等、隨時可叫）：UI 看回應的 `caught_up` 決定要不要再叫。daemon 沒有補窗 job、不發任何「有洞」的訊號。
//! - daemon 唯一要守的：**水位不能跨過洞**（§2）——不然 UI 下次從水位起 `Recent`，洞就永遠補不回來。
//!
//! 事件跟 `Recent` 那條路一樣**原樣**寫（密文不解，local-cache-db.md §7.2）；訂閱是純的，`seq` 跳號不管、`Subscribe` 不帶 `cg_seq`。

use std::sync::Arc;
use std::time::Duration;

use wbf_sdk::channel::Channel;
use wbf_sdk::client::{RoomSubscription, WbfClient};
use wbf_sdk::event_json::messages_from_incoming;
use wbf_sdk::protocol::EventSubscribeReply;
use wbf_sdk::{IncomingEvent, Transport};

use crate::accounts::AccountDir;
use crate::backend_choice::MethodHome;
use crate::error::CoreError;
use crate::event::{EventSink, LinkState};
use crate::link_pool::LinkRole;
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
    /// §2：有洞時是那包的 `fs`——水位凍在洞之前，直到看到水位被 `Recent` 推過它。
    hole_before: Option<i64>,
}

impl Core {
    /// 池開完一條線的通用初始化（維護者 2026-09-22）：看角色決定還要做什麼。
    ///
    /// | 角色 | 做什麼 |
    /// |---|---|
    /// | `Subscriptions` | `Event/Subscribe`（帳號層、不帶 `cg_seq`）→ Ack 之後起收推播的 task（之前有的話換掉：它的線已經死了） |
    /// | 其他 | 不做事 |
    ///
    /// Args:
    ///     role: 這條線的角色, example: LinkRole::Subscriptions
    ///     client: 已經 hello 過的線
    /// Return:
    ///     Ok(())
    ///     Err(Network)    Ack 沒等到、線死了（池就當這條沒開成）
    ///     Err(Server)     server 拒
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
        let subscription = client.room_subscription(None, ACK_TIMEOUT).await?;
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let task = RoomSyncTask {
            me,
            cache,
            events: self.events.clone(),
            hole_before: None,
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
        mut self,
        mut subscription: RoomSubscription,
        mut stopped: tokio::sync::oneshot::Receiver<()>,
    ) {
        // Ack 之前就推來的：跟之後的一樣處理。
        let early: Vec<_> = subscription.early_pushes.drain(..).collect();
        for (meta, events) in early {
            self.on_push(meta.fs, meta.gap, events).await;
        }
        loop {
            // 本地收件匣滿過＝丟過包：跟 server 的 gap 一樣，洞在現在的水位之後、還沒處理的包之前。
            if subscription.take_gap() {
                self.freeze_before_next_push().await;
            }
            let next = tokio::select! {
                _ = &mut stopped => return,
                next = subscription.next(PUSH_IDLE_TIMEOUT) => next,
            };
            match next {
                Ok(Some(EventSubscribeReply::Push { meta, events })) => {
                    self.on_push(meta.fs, meta.gap, events).await;
                }
                // 金鑰那支才消費；這裡只確認它不會把 task 弄死。
                Ok(Some(EventSubscribeReply::DeviceChanged(_)))
                | Ok(Some(EventSubscribeReply::Acknowledged(_)))
                | Ok(Some(EventSubscribeReply::Unsubscribed { .. })) => {}
                Err(wbf_sdk::SdkError::Timeout(_)) => {}
                // 一包壞了就是漏一包：當成洞，水位凍住到 UI 補過為止。
                Err(wbf_sdk::SdkError::Protocol(why)) => {
                    self.events.progress(format!(
                        "room sync: a push could not be read ({why}); the watermark is frozen until the next sync.recent"
                    ));
                    self.freeze_before_next_push().await;
                }
                Ok(None) | Err(_) => {
                    let why = match next {
                        Ok(None) => "the server ended the subscription".to_string(),
                        Err(error) => error.to_string(),
                        Ok(Some(_)) => unreachable!("handled above"),
                    };
                    // 線死了池要到下次取用才發現；這裡先講，UI 不必等。（下次取用時池會再發一次 Closed→Opened。）
                    self.events.emit(CoreEvent::Link {
                        user: self.me.clone(),
                        role: LinkRole::Subscriptions,
                        state: LinkState::Closed,
                        reason: Some(format!("the room subscription ended: {why}")),
                    });
                    return;
                }
            }
        }
    }

    /// 洞在「現在的水位」之後：把凍結點設成現在的水位（之後的包不推，直到 UI 的 `Recent` 推過它）。
    async fn freeze_before_next_push(&mut self) {
        let me = self.me.clone();
        match self.cache.run(move |cache| cache.get_cg_seq(&me)).await {
            // 沒有水位就沒有洞可跨（UI 的第一次 Recent 從最新拿）。
            Ok(Some(cg_seq)) => {
                self.hole_before = Some(self.hole_before.map_or(cg_seq, |hole| hole.max(cg_seq)));
            }
            Ok(None) => {}
            Err(error) => self
                .events
                .progress(format!("room sync: cannot read the watermark: {error}")),
        }
    }

    /// 一包：原樣寫進 cache（照房分組）、水位往前推到 `fs`、commit 之後發 `room.message`。
    ///
    /// 🚨 §2 **水位不能跨過洞**：帶 `gap` 的包把凍結點設成它的 `fs`（洞在舊水位跟它之間），這包與**之後的**包都不推水位，
    /// 直到看到水位已經 ≥ 凍結點——那只可能是 UI 叫的 `sync.recent` 推的（它從舊水位起、推到那一刻最新的 `fs`），洞補過了才解凍。
    /// 判斷跟寫入在同一個 cache 工作裡，跟 `Recent` 的寫入排同一條 queue，🚫 不靠時序。
    async fn on_push(&mut self, fs: i64, gap: bool, events: Vec<serde_json::Value>) {
        let mut by_room: std::collections::BTreeMap<String, Vec<IncomingEvent>> =
            std::collections::BTreeMap::new();
        let mut skipped = 0usize;
        for raw in events {
            // 沒帶 room_id 的不猜、不寫（跟 `recent` 同一條規矩）。
            match raw.get("room_id").and_then(|value| value.as_str()) {
                Some(room) => by_room
                    .entry(room.to_string())
                    .or_default()
                    .push(IncomingEvent::from_ws_json(raw)),
                None => skipped += 1,
            }
        }
        if skipped > 0 {
            self.events.progress(format!(
                "room sync: skipped {skipped} pushed event(s) without room_id"
            ));
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
        let hole_before = self.hole_before;
        let written = self
            .cache
            .run(move |cache| {
                for (room, events) in &by_room {
                    cache.upsert_events(&me, room, events)?;
                }
                let hole_before = match (gap, hole_before) {
                    (true, hole) => Some(hole.map_or(fs, |hole| hole.max(fs))),
                    (false, Some(hole)) => {
                        // 水位到了凍結點以上，只可能是 Recent 推的：洞補過了，解凍。
                        if cache.get_cg_seq(&me)?.is_some_and(|cg_seq| cg_seq >= hole) {
                            None
                        } else {
                            Some(hole)
                        }
                    }
                    (false, None) => None,
                };
                if hole_before.is_none() && fs > 0 {
                    cache.advance_cg_seq(&me, fs)?;
                }
                Ok(hole_before)
            })
            .await;
        match written {
            Ok(hole_before) => {
                self.hole_before = hole_before;
                // commit 之後才發（PR #32 的規矩）。
                for notice in notices {
                    self.events.emit(notice);
                }
            }
            // 寫失敗只報不擋（快取壞了的代價是重拉）；沒落地的就不通知。
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

    /// 假的 server 端：答 Hello、Subscribe、Recent（照 `cg_seq` 給比它新的，一窗一個 Batch）、Unsubscribe；
    /// 測試從 `outbound` 塞 server 主動推的包。記下每次 `Recent` 帶的 `cg_seq` 與訂閱的 id。事件清單可以跟別條線共用。
    struct FakeServer {
        recent_requests: Arc<Mutex<Vec<Option<i64>>>>,
        subscription_id: Arc<Mutex<Option<u64>>>,
        outbound: tokio::sync::mpsc::Sender<Pack>,
        task: tokio::task::JoinHandle<()>,
    }

    fn start_fake_server(mut peer: MemoryEnd, events: Arc<Mutex<Vec<Value>>>) -> FakeServer {
        let recent_requests = Arc::new(Mutex::new(Vec::new()));
        let subscription_id = Arc::new(Mutex::new(None));
        let (outbound, mut outbound_rx) = tokio::sync::mpsc::channel::<Pack>(16);
        let (recents_t, sub_t) = (recent_requests.clone(), subscription_id.clone());
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
                        *sub_t.lock().unwrap() = Some(pack.id);
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
            subscription_id,
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

    /// 模組註解的形狀，對著假 server 走一遍：`init_connection` 訂了、🚫 沒補窗 → 推一包寫進去、水位前進、`room.message` 在 commit 之後 →
    /// 帶 gap 的包：寫、水位凍住；**之後正常的包也不推**（不然跨過洞）→ UI 叫 `sync.recent`（從洞之前的水位起）補回洞、推水位 →
    /// 再來一包才恢復推 → 關訂閱線。
    #[tokio::test]
    async fn the_watermark_never_crosses_a_hole_and_refilling_is_the_uis_job() {
        let dir = scratch("hole");
        let (core, account) = core_with_wbf_account(&dir);
        let mut seen = core.subscribe();
        let events: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
        let (mut client, fake) = memory_client_with_hello(events.clone()).await;

        // 池開線的最後一步。
        core.init_connection(&account, LinkRole::Subscriptions, &mut client)
            .await
            .expect("subscribe");
        let subscription_id = fake
            .subscription_id
            .lock()
            .unwrap()
            .expect("server got a Subscribe");
        assert!(core.is_room_syncing(&account));
        let pool = core.pool_of_account(&account).unwrap();
        drop(
            pool.acquire(LinkRole::Subscriptions, || async move { Ok(client) })
                .await
                .unwrap(),
        );
        assert!(
            fake.recent_requests.lock().unwrap().is_empty(),
            "補窗不是 daemon 的事：開線不叫 Recent"
        );
        assert_eq!(cg_seq_of(&core, &account).await, None);

        // 一包正常的推播：寫進去、水位前進、commit 之後才有 room.message。
        events.lock().unwrap().push(text_event(4801));
        fake.outbound
            .send(push(subscription_id, 0, false, &[text_event(4801)]))
            .await
            .unwrap();
        assert_eq!(next_message(&mut seen).await, "$4801");
        assert!(cached_ids(&core, &account)
            .await
            .contains(&"$4801".to_string()));
        assert_eq!(cg_seq_of(&core, &account).await, Some(4801));

        // 有洞：4850 掉了，server 推 4900 帶 gap → 寫、水位不動。
        events
            .lock()
            .unwrap()
            .extend([text_event(4850), text_event(4900), text_event(4950)]);
        fake.outbound
            .send(push(subscription_id, 1, true, &[text_event(4900)]))
            .await
            .unwrap();
        assert_eq!(next_message(&mut seen).await, "$4900");
        assert_eq!(
            cg_seq_of(&core, &account).await,
            Some(4801),
            "🚨 帶 gap 的包不推水位"
        );
        // 之後正常的一包也不推：推了就跨過 4850。
        fake.outbound
            .send(push(subscription_id, 2, false, &[text_event(4950)]))
            .await
            .unwrap();
        assert_eq!(next_message(&mut seen).await, "$4950");
        assert_eq!(
            cg_seq_of(&core, &account).await,
            Some(4801),
            "🚨 洞還沒補之前，後面的包也不能推水位"
        );

        // UI 補窗：`sync.recent`（走 Misc 線；這裡先把一條記憶體對接的線放進那一格）。從 4801 起拿到 4850／4900／4950，水位到 4950。
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
                false,
                Transport::WebSocket,
                "ui refill",
                &Target::default(),
            )
            .await
            .expect("sync.recent");
        assert_eq!(
            (summary.pulled, summary.cg_seq_before, summary.cg_seq_after),
            (3, Some(4801), Some(4950))
        );
        assert!(summary.caught_up);
        assert!(
            cached_ids(&core, &account)
                .await
                .contains(&"$4850".to_string()),
            "洞補回來了"
        );
        assert_eq!(cg_seq_of(&core, &account).await, Some(4950));

        // 洞補過了：下一包恢復推水位。
        events.lock().unwrap().push(text_event(5000));
        fake.outbound
            .send(push(subscription_id, 3, false, &[text_event(5000)]))
            .await
            .unwrap();
        assert_eq!(next_message(&mut seen).await, "$5000");
        wait_for_async(
            || async { cg_seq_of(&core, &account).await == Some(5000) },
            "the watermark moves again once the hole is filled",
        )
        .await;

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

    /// 對真的 wbfuwunel：alice 登入、開訂閱線；bob（另一個 `Core`、另一個資料目錄）用 `Event/Send` 送一則；alice 的 `room.message` 在時限內到、
    /// cache 有它、水位前進；關訂閱線；兩邊登出。
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
        let cg_seq_before = cg_seq_of(&core_a, &account_a).await;

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
        assert!(
            cg_seq_of(&core_a, &account_a).await > cg_seq_before,
            "水位往前推"
        );

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

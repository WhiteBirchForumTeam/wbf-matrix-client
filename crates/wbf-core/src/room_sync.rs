//! 跟上游：一個帳號一個背景 task，在訂閱線上收 `Event/Push` 寫進 `cache.db`（daemon-runtime 第 6／7 階段；
//! server 的語意在 wbfuwunel `wbf-event-push.md`）。
//!
//! 維護者 2026-09-22 定的順序（先純一點，🚫 不接 RPC）：
//!
//! 1. 讀本地水位 `cg_seq`。
//! 2. 送 `Event/Subscribe`（帳號層、**不帶 `cg_seq`**）。收到 Ack 就登記完成——server 先登記再回 Ack，之後的每則新事件都會推來。
//! 3. **補窗是一個 job**：同一條線、同一個 guard，`Recent{cg_seq}` 一窗一窗翻到追平或到起始總量（預設 1000；server 一窗預設 320、上限 500）。
//! 4. 放掉 guard，背景 task 讀訂閱：**訂閱是純的**——一包來寫一包、水位只往前推到 `fs`、跳號不管。`gap` 就再跑一次同一個補窗 job。
//! 5. 線死了 task 結束，發 `sync.state: disconnected`；🚫 不背景重連（link-pool.md §3：下一次 `start_room_sync` 重開重訂）。
//!
//! 事件跟 `Recent` 那條路一樣**原樣**寫（密文不解，local-cache-db.md §7.2），commit 之後才發 `room.message`（PR #32 的規矩）。

use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;

use wbf_sdk::client::RoomSubscription;
use wbf_sdk::event_json::messages_from_incoming;
use wbf_sdk::protocol::EventSubscribeReply;
use wbf_sdk::{IncomingEvent, RecentPlan, Transport};

use crate::accounts::AccountDir;
use crate::backend_choice::MethodHome;
use crate::error::{CoreError, CoreErrorKind};
use crate::event::{EventSink, SyncState};
use crate::link_pool::{LinkPool, LinkRole};
use crate::server_cache::ServerCache;
use crate::sync_ops::{pull_recent, RecentSummary};
use crate::{Core, CoreEvent, Target};

/// 起始同步最多拿幾則（維護者 2026-09-22：「先建議短一點，可能設一千試試」）。到了還沒追平就停，水位仍推到最新；更舊的留給翻歷史。
pub const STARTUP_SYNC_MAX_EVENTS: u64 = 1000;

/// 等 `Subscribe` 的 Ack、等 `Unsubscribe` 的 Ack 最久多久。
const ACK_TIMEOUT: Duration = Duration::from_secs(30);
/// 訂閱 task 每次等推播最久多久：到了只是「這段時間沒事」，繼續等（心跳另外在線上跑）。
const PUSH_IDLE_TIMEOUT: Duration = Duration::from_secs(60);

/// `start_room_sync` 的結果：訂到了什麼、補窗補了多少。
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RoomSyncStart {
    pub user: String,
    /// server 在 Ack 那一刻最新的 `g_seq`。
    pub latest_g_seq: i64,
    /// 訂閱進了幾房、跳過哪些（不是成員的）。
    pub joined: u32,
    pub skipped: Vec<String>,
    /// 補窗 job 的摘要（`caught_up: false` ＝ 到了起始總量還沒追平）。
    pub recent: RecentSummary,
}

/// 一個帳號的跟上游 task。丟掉就 abort（`Core` 丟掉時）。
pub(crate) struct RoomSyncHandle {
    task: tokio::task::JoinHandle<()>,
    /// 叫它說出口地退訂再結束（`stop_room_sync`）。
    stop: Option<tokio::sync::oneshot::Sender<()>>,
}

impl Drop for RoomSyncHandle {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// task 自己握著的東西——🚫 不握 `Core`（task 是 `'static` 的）。補 gap 時要線就跟池要；線死了池會叫 opener，
/// 而這裡的 opener 一律回錯：線死了訂閱也死了，task 本來就該結束（重開是下一次 `start_room_sync` 的事）。
struct RoomSyncTask {
    me: String,
    pool: Arc<LinkPool>,
    cache: Arc<ServerCache>,
    events: EventSink,
    plan: RecentPlan,
}

impl Core {
    /// 跟上游（模組註解的 1–4 步），回來時補窗 job 已經做完、背景 task 已經在收推播。
    ///
    /// Args:
    ///     max_events: 起始同步最多拿幾則；None 用 [`STARTUP_SYNC_MAX_EVENTS`], example: Some(1000)
    /// Return:
    ///     Ok(RoomSyncStart)
    ///     Err(AccountBusy)   這個帳號已經在跟了
    ///     Err(Usage)         沒登入、不是 wbf server（訂閱只有 wbf 講得出來）
    ///     Err(Network)       線開不起來、Ack 沒等到
    pub async fn start_room_sync(
        &self,
        max_events: Option<u64>,
        target: &Target,
    ) -> Result<RoomSyncStart, CoreError> {
        let account = self.account_or_current(target)?;
        self.start_room_sync_with(&account, max_events, || {
            self.open_link(&account, LinkRole::Subscriptions)
        })
        .await
    }

    /// 同上，開線的方式由呼叫端給（測試用記憶體對接；生產路徑就是 `open_link`）。
    pub(crate) async fn start_room_sync_with<F, Fut>(
        &self,
        account: &AccountDir,
        max_events: Option<u64>,
        open: F,
    ) -> Result<RoomSyncStart, CoreError>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<
            Output = Result<wbf_sdk::client::WbfClient<wbf_sdk::channel::Channel>, CoreError>,
        >,
    {
        if self.is_room_syncing(account) {
            return Err(CoreError::new(
                CoreErrorKind::AccountBusy,
                format!("{} is already following its homeserver", account.label()),
            ));
        }
        // 訂閱只有 wbf 講得出來：這一步替我們做「這台講不講 wbf」的判斷（不講就是 Usage）。
        let speaks_wbf = self.get_backend_kind(account).await == crate::BackendKind::WbfSdk;
        crate::get_backend_for(Transport::WebSocket, speaks_wbf, MethodHome::WbfSdkOnly)?;
        let (cache, me) = self.server_cache_and_me(account)?;
        let pool = self.pool_of_account(account)?;
        let plan = RecentPlan {
            max_events: Some(max_events.unwrap_or(STARTUP_SYNC_MAX_EVENTS)),
            ..RecentPlan::default()
        };

        // 1. 水位。
        let cg_seq = cache.read().await.get_cg_seq(&me)?;
        self.events.emit(CoreEvent::SyncState {
            user: me.clone(),
            state: SyncState::CatchingUp,
            cg_seq,
        });
        // 2. 訂閱（先登記，之後的新事件都會推來）。
        let mut client = pool.acquire(LinkRole::Subscriptions, open).await?;
        let subscription = client.room_subscription(None, ACK_TIMEOUT).await?;
        // 3. 補窗 job：同一條線、同一個 guard。
        let recent = pull_recent(&mut client, &cache, &self.events, &me, cg_seq, plan).await?;
        drop(client);
        self.events.emit(CoreEvent::SyncState {
            user: me.clone(),
            state: SyncState::CaughtUp,
            cg_seq: recent.cg_seq_after,
        });

        let start = RoomSyncStart {
            user: me.clone(),
            latest_g_seq: subscription.ack.latest_g_seq,
            joined: subscription.ack.joined,
            skipped: subscription.ack.skipped.clone(),
            recent,
        };
        // 4. 放手：背景 task 收推播。
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let task = RoomSyncTask {
            me,
            pool,
            cache,
            events: self.events.clone(),
            plan,
        };
        let handle = RoomSyncHandle {
            task: tokio::spawn(task.run(subscription, stopped)),
            stop: Some(stop),
        };
        let previous = self
            .room_syncs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(account.dir.clone(), handle);
        // 上面已經擋過「在跟」；還是有一個的話（兩個 start 同時進來）收掉舊的，🚫 不讓兩個 task 寫同一份水位。
        drop(previous);
        Ok(start)
    }

    /// 停止跟上游：叫 task 說出口地退訂（`Event/Unsubscribe`）再結束。線本身留在池裡。
    ///
    /// Return:
    ///     Ok(true)    本來在跟，停了
    ///     Ok(false)   本來就沒在跟
    pub async fn stop_room_sync(&self, target: &Target) -> Result<bool, CoreError> {
        let account = self.account_or_current(target)?;
        Ok(self.stop_room_sync_of(&account).await)
    }

    /// 同上，給登出用（登出接著會關線，退不退訂都一樣；這裡只是把 task 收乾淨）。
    pub(crate) async fn stop_room_sync_of(&self, account: &AccountDir) -> bool {
        let handle = self
            .room_syncs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&account.dir);
        let Some(mut handle) = handle else {
            return false;
        };
        // task 自己已經結束（線死了）：只是把 handle 收掉，🚫 不算「本來在跟」。
        if handle.task.is_finished() {
            return false;
        }
        if let Some(stop) = handle.stop.take() {
            let _ = stop.send(());
        }
        // 給它 ACK_TIMEOUT 說再見；不肯就 abort（Drop）。
        let _ = tokio::time::timeout(ACK_TIMEOUT, &mut handle.task).await;
        true
    }

    /// Return:
    ///     bool  true ＝ 這個帳號的跟上游 task 還在
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
        let mut need_recent = false;
        for (meta, events) in early {
            need_recent |= self.on_push(meta.fs, meta.gap, events);
        }
        loop {
            if need_recent || subscription.take_gap() {
                need_recent = false;
                self.fill_gap().await;
            }
            let next = tokio::select! {
                _ = &mut stopped => {
                    self.unsubscribe(subscription).await;
                    return;
                }
                next = subscription.next(PUSH_IDLE_TIMEOUT) => next,
            };
            match next {
                Ok(Some(EventSubscribeReply::Push { meta, events })) => {
                    need_recent = self.on_push(meta.fs, meta.gap, events);
                }
                // 金鑰那支才消費；這裡只確認它不會把 task 弄死。
                Ok(Some(EventSubscribeReply::DeviceChanged(_))) => {}
                Ok(Some(EventSubscribeReply::Acknowledged(_)))
                | Ok(Some(EventSubscribeReply::Unsubscribed { .. })) => {}
                Err(wbf_sdk::SdkError::Timeout(_)) => {}
                // 一包壞了就是漏一包：補一次。
                Err(wbf_sdk::SdkError::Protocol(why)) => {
                    self.events.progress(format!(
                        "room sync: a push could not be read ({why}); refilling"
                    ));
                    need_recent = true;
                }
                Ok(None) | Err(_) => {
                    self.events.emit(CoreEvent::SyncState {
                        user: self.me.clone(),
                        state: SyncState::Disconnected,
                        cg_seq: None,
                    });
                    return;
                }
            }
        }
    }

    /// 一包：原樣寫進 cache（照房分組）、水位只往前推到 `fs`、commit 之後發 `room.message`。
    ///
    /// 🚨 **帶 `gap` 的那包不推水位**：洞在舊水位跟這包之間，先推到 `fs` 再 `Recent(cg_seq)` 就會跨過它（server 的 `Recent` 只給比 `cg_seq` 新的）。
    /// 事件照樣寫（冪等），水位由補窗 job 推（它拿舊水位起、推到第一窗的 `fs`，這包也在那窗裡）。
    ///
    /// Return:
    ///     bool  true ＝ 這包說有洞（`gap`），呼叫端補窗
    fn on_push(&self, fs: i64, gap: bool, events: Vec<serde_json::Value>) -> bool {
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
        // 通知給折好的訊息（自己送的也發，呼叫端自己濾）；庫裡存原樣。
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
        self.cache.post(
            move |cache| {
                for (room, events) in &by_room {
                    cache.upsert_events(&me, room, events)?;
                }
                // 🚨 水位跟事件同一條 queue、同一個順序：事件還沒落地水位就前進的事不會發生。有洞的那包不推（上面的註解）。
                if fs > 0 && !gap {
                    cache.advance_cg_seq(&me, fs)?;
                }
                Ok(())
            },
            notices,
        );
        gap
    }

    /// 補窗：跟池要線（死了就不補——訂閱也死了，`run` 下一輪會結束）。
    async fn fill_gap(&self) {
        let line = self
            .pool
            .acquire(LinkRole::Subscriptions, || async {
                Err(CoreError::new(
                    CoreErrorKind::Network,
                    "the subscription line is gone; the room sync ends and must be started again",
                ))
            })
            .await;
        let mut client = match line {
            Ok(client) => client,
            Err(error) => {
                self.events
                    .progress(format!("room sync: cannot refill after a gap: {error}"));
                return;
            }
        };
        let cg_seq = match self.cache.read().await.get_cg_seq(&self.me) {
            Ok(cg_seq) => cg_seq,
            Err(error) => {
                self.events
                    .progress(format!("room sync: cannot read the watermark: {error}"));
                return;
            }
        };
        self.events.emit(CoreEvent::SyncState {
            user: self.me.clone(),
            state: SyncState::CatchingUp,
            cg_seq,
        });
        let outcome = pull_recent(
            &mut client,
            &self.cache,
            &self.events,
            &self.me,
            cg_seq,
            self.plan,
        )
        .await;
        match outcome {
            Ok(summary) => self.events.emit(CoreEvent::SyncState {
                user: self.me.clone(),
                state: SyncState::CaughtUp,
                cg_seq: summary.cg_seq_after,
            }),
            Err(error) => self
                .events
                .progress(format!("room sync: refill after a gap failed: {error}")),
        }
    }

    async fn unsubscribe(&self, subscription: RoomSubscription) {
        let line = self
            .pool
            .acquire(LinkRole::Subscriptions, || async {
                Err(CoreError::new(
                    CoreErrorKind::Network,
                    "the subscription line is gone (nothing to unsubscribe from)",
                ))
            })
            .await;
        if let Ok(mut client) = line {
            if let Err(error) = client.room_unsubscribe(subscription, ACK_TIMEOUT).await {
                self.events
                    .progress(format!("room sync: unsubscribe did not complete: {error}"));
            }
        }
        self.events.emit(CoreEvent::SyncState {
            user: self.me.clone(),
            state: SyncState::Disconnected,
            cg_seq: None,
        });
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
    use wbf_sdk::WsLink;
    use wbf_wire::pack::{control, event, flags};
    use wbf_wire::{Kind, Pack};

    use crate::accounts::AccountDir;
    use crate::error::CoreErrorKind;
    use crate::event::SyncState;
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
        let seqs: Vec<i64> = events
            .iter()
            .map(|event| {
                event["unsigned"][wbf_sdk::protocol::G_SEQ_KEY]
                    .as_i64()
                    .unwrap()
            })
            .collect();
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
    /// 測試從 `outbound` 塞 server 主動推的包。記下每次 `Recent` 帶的 `cg_seq` 與訂閱的 id。
    struct FakeServer {
        events: Arc<Mutex<Vec<Value>>>,
        recent_requests: Arc<Mutex<Vec<Option<i64>>>>,
        subscription_id: Arc<Mutex<Option<u64>>>,
        unsubscribed: Arc<Mutex<bool>>,
        outbound: tokio::sync::mpsc::Sender<Pack>,
        task: tokio::task::JoinHandle<()>,
    }

    fn start_fake_server(mut peer: MemoryEnd) -> FakeServer {
        let events: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
        let recent_requests = Arc::new(Mutex::new(Vec::new()));
        let subscription_id = Arc::new(Mutex::new(None));
        let unsubscribed = Arc::new(Mutex::new(false));
        let (outbound, mut outbound_rx) = tokio::sync::mpsc::channel::<Pack>(16);
        let (events_t, recents_t, sub_t, unsub_t) = (
            events.clone(),
            recent_requests.clone(),
            subscription_id.clone(),
            unsubscribed.clone(),
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
                        *sub_t.lock().unwrap() = Some(pack.id);
                        let latest = events_t
                            .lock()
                            .unwrap()
                            .iter()
                            .filter_map(|event| {
                                event["unsigned"][wbf_sdk::protocol::G_SEQ_KEY].as_i64()
                            })
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
                    (Kind::Event, event::UNSUBSCRIBE) => {
                        *unsub_t.lock().unwrap() = true;
                        response(
                            Kind::Control,
                            control::ACK,
                            pack.id,
                            pack.seq,
                            json!({}),
                            Vec::new(),
                        )
                    }
                    (Kind::Event, event::RECENT) => {
                        let meta: Value = serde_json::from_slice(&pack.meta).unwrap();
                        let cg_seq = meta["cg_seq"].as_i64();
                        let before = meta["before"].as_i64();
                        let limit = meta["limit"].as_u64().unwrap_or(320) as usize;
                        recents_t.lock().unwrap().push(cg_seq);
                        let mut window: Vec<Value> = events_t
                            .lock()
                            .unwrap()
                            .iter()
                            .filter(|event| {
                                let g_seq = event["unsigned"][wbf_sdk::protocol::G_SEQ_KEY]
                                    .as_i64()
                                    .unwrap();
                                cg_seq.is_none_or(|cg_seq| g_seq > cg_seq)
                                    && before.is_none_or(|before| g_seq < before)
                            })
                            .cloned()
                            .collect();
                        // 新到舊，最多 `limit` 則。
                        window.sort_by_key(|event| {
                            std::cmp::Reverse(
                                event["unsigned"][wbf_sdk::protocol::G_SEQ_KEY]
                                    .as_i64()
                                    .unwrap(),
                            )
                        });
                        let more = window.len() > limit;
                        window.truncate(limit);
                        let seqs: Vec<i64> = window
                            .iter()
                            .map(|event| {
                                event["unsigned"][wbf_sdk::protocol::G_SEQ_KEY]
                                    .as_i64()
                                    .unwrap()
                            })
                            .collect();
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
            events,
            recent_requests,
            subscription_id,
            unsubscribed,
            outbound,
            task,
        }
    }

    /// 記憶體對接的線，已經 hello 過（生產路徑的 `open_link` 也是開完就 hello）。
    async fn memory_client_with_hello() -> (WbfClient<Channel>, FakeServer) {
        let (client_end, server_end) = memory_pair(64);
        let fake = start_fake_server(server_end);
        let link = WsLink::start(client_end.source, client_end.sink, wbf_sdk::no_hook());
        let mut client = WbfClient::new(Channel::WebSocket(Box::new(WsChannel::from_link(link))));
        client.hello("room-sync test", &[]).await.unwrap();
        (client, fake)
    }

    async fn wait_for<F: FnMut() -> bool>(mut condition: F, what: &str) {
        tokio::time::timeout(Duration::from_secs(10), async {
            while !condition() {
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

    /// 模組註解的 1–5 步，對著假 server 走一遍：訂閱 → 補窗（拿到 5 則、水位到最新）→ 推播寫進去、水位前進、`room.message` 在 commit 之後 →
    /// 帶 `gap` 的包觸發 `Recent(舊水位)` 把洞補回來 → 說出口地退訂。
    #[tokio::test]
    async fn a_room_sync_subscribes_fills_the_window_then_follows_pushes_and_gaps() {
        let dir = scratch("follow");
        let (core, account) = core_with_wbf_account(&dir);
        let mut seen = core.subscribe();
        let (client, fake) = memory_client_with_hello().await;
        fake.events
            .lock()
            .unwrap()
            .extend((4701..=4705).map(text_event));

        let start = core
            .start_room_sync_with(&account, Some(1000), || async move { Ok(client) })
            .await
            .expect("subscribe + refill");
        assert_eq!(start.latest_g_seq, 4705);
        assert_eq!(start.recent.pulled, 5, "{start:?}");
        assert!(start.recent.caught_up);
        assert_eq!(start.recent.cg_seq_after, Some(4705));
        assert_eq!(cg_seq_of(&core, &account).await, Some(4705));
        assert_eq!(cached_ids(&core, &account).await.len(), 5);
        assert_eq!(
            *fake.recent_requests.lock().unwrap(),
            vec![None],
            "第一次沒有水位：從最新拿"
        );
        assert!(core.is_room_syncing(&account));
        // 第二次 start 被擋：一個帳號一個 task。
        let (another, _fake2) = memory_client_with_hello().await;
        let error = core
            .start_room_sync_with(&account, None, || async move { Ok(another) })
            .await
            .expect_err("already syncing");
        assert_eq!(error.kind, CoreErrorKind::AccountBusy);

        // 一包正常的推播：寫進去、水位前進、commit 之後才有 room.message。
        let subscription_id = fake.subscription_id.lock().unwrap().unwrap();
        fake.outbound
            .send(push(subscription_id, 0, false, &[text_event(4801)]))
            .await
            .unwrap();
        let message = loop {
            match tokio::time::timeout(Duration::from_secs(10), seen.recv())
                .await
                .expect("a room.message arrives")
                .unwrap()
            {
                CoreEvent::Message { user, message } => break (user, message),
                _ => continue,
            }
        };
        assert_eq!(message.0, ME);
        assert_eq!(message.1.id, "$4801");
        // 事件在 commit 之後才發，所以此刻庫裡一定有它、水位也已經到了。
        assert!(cached_ids(&core, &account)
            .await
            .contains(&"$4801".to_string()));
        assert_eq!(cg_seq_of(&core, &account).await, Some(4801));

        // 有洞：server 只推了 4900，4850 掉了 → 那包帶 gap → 拿**舊水位** 4801 去 Recent，4850 才補得回來。
        fake.events
            .lock()
            .unwrap()
            .extend([text_event(4850), text_event(4900)]);
        fake.outbound
            .send(push(subscription_id, 1, true, &[text_event(4900)]))
            .await
            .unwrap();
        wait_for(
            || fake.recent_requests.lock().unwrap().len() == 2,
            "the gap triggers a Recent",
        )
        .await;
        assert_eq!(
            fake.recent_requests.lock().unwrap()[1],
            Some(4801),
            "🚨 補窗要從洞之前的水位起，不是這包的 fs"
        );
        tokio::time::timeout(Duration::from_secs(10), async {
            while cg_seq_of(&core, &account).await != Some(4900) {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the watermark reaches 4900 after the refill");
        let ids = cached_ids(&core, &account).await;
        assert!(ids.contains(&"$4850".to_string()), "洞補回來了：{ids:?}");
        assert!(ids.contains(&"$4900".to_string()));

        // 說出口地退訂。
        assert!(core.stop_room_sync(&Target::default()).await.unwrap());
        assert!(
            *fake.unsubscribed.lock().unwrap(),
            "server 收到 Unsubscribe"
        );
        assert!(!core.is_room_syncing(&account));
        let states: Vec<SyncState> = std::iter::from_fn(|| seen.try_recv().ok())
            .filter_map(|event| match event {
                CoreEvent::SyncState { state, .. } => Some(state),
                _ => None,
            })
            .collect();
        assert_eq!(
            states.last(),
            Some(&SyncState::Disconnected),
            "退訂之後告訴 UI 斷了：{states:?}"
        );
        assert!(
            states.contains(&SyncState::CatchingUp) && states.contains(&SyncState::CaughtUp),
            "補窗前後各發一次：{states:?}"
        );
        fake.task.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 對真的 wbfuwunel：alice 登入、跟上游；bob（另一個 `Core`、另一個資料目錄）用 `Event/Send` 送一則；alice 的 `room.message` 在時限內到、
    /// cache 有它、水位前進；說出口地退訂；兩邊登出。
    ///
    /// `--ignored`；環境變數：`WBF_E2E_SERVER`、`WBF_E2E_USER`（完整 mxid）、`WBF_E2E_PASSWORD_FILE`、`WBF_E2E_USER_B`、`WBF_E2E_PASSWORD_B_FILE`、
    /// `WBF_E2E_ROOM`（兩人都在的明文房）。
    #[tokio::test]
    #[ignore = "needs a running wbfuwunel: WBF_E2E_SERVER, WBF_E2E_USER, WBF_E2E_PASSWORD_FILE, WBF_E2E_USER_B, WBF_E2E_PASSWORD_B_FILE, WBF_E2E_ROOM"]
    async fn a_room_sync_receives_what_another_account_sends_over_the_real_server() {
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

        let start = core_a
            .start_room_sync(Some(1000), &Target::default())
            .await
            .expect("subscribe + refill over the real server");
        assert!(start.joined >= 1, "{start:?}");
        assert!(core_a.is_room_syncing(&account_a));
        let cg_seq_before = cg_seq_of(&core_a, &account_a).await;

        // bob 送一則（wbf 帳號：Event/Send）；alice 的訂閱線要推來。
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
        // 讀完就放：登出要關 cache.db，還有人握著它會被拒（那正是它該有的行為）。
        let cached = {
            let (cache, me) = core_a.server_cache_and_me(&account_a).unwrap();
            let reader = cache.read().await;
            reader
                .list_messages_by_event_ids(&me, &room, std::slice::from_ref(&event_id))
                .unwrap()
        };
        assert_eq!(cached.len(), 1, "commit 之後才發事件，所以此刻庫裡一定有");
        let cg_seq_after = cg_seq_of(&core_a, &account_a).await;
        assert!(
            cg_seq_after > cg_seq_before,
            "水位往前推：{cg_seq_before:?} → {cg_seq_after:?}"
        );

        assert!(core_a.stop_room_sync(&Target::default()).await.unwrap());
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

    /// 線死了：task 結束、發 disconnected、`is_room_syncing` 變 false。🚫 沒有背景重連。
    #[tokio::test]
    async fn a_dead_line_ends_the_room_sync_and_says_so() {
        let dir = scratch("dead");
        let (core, account) = core_with_wbf_account(&dir);
        let mut seen = core.subscribe();
        let (client, fake) = memory_client_with_hello().await;
        core.start_room_sync_with(&account, None, || async move { Ok(client) })
            .await
            .expect("subscribe");
        // 對方收攤（丟掉它那端）。
        fake.task.abort();
        drop(fake.outbound);
        wait_for(|| !core.is_room_syncing(&account), "the task ends").await;
        let disconnected = std::iter::from_fn(|| seen.try_recv().ok()).any(|event| {
            matches!(
                event,
                CoreEvent::SyncState {
                    state: SyncState::Disconnected,
                    ..
                }
            )
        });
        assert!(disconnected, "線死了要講");
        // 沒在跟：stop 是 no-op。
        assert!(!core.stop_room_sync(&Target::default()).await.unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }
}

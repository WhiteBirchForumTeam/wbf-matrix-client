//! 連線池（link-pool.md）：一個帳號五條線，各司其職、要用才開、斷了下次要用再開。
//!
//! 池只管 socket：哪條線開了、關了、要不要重開。**怎麼開**是呼叫端交進來的（`acquire` 的 `open` 閉包，§7 的接縫）——
//! 正式的在 `Core::open_link`（session → `Channel::connect` → `hello`），測試的用記憶體對接。
//! 🚫 沒有背景重連（第 8 階段的監督者）；🚫 不知道訂閱的內容（那是用線的人的事）。

use std::collections::HashMap;
use std::future::Future;
use std::ops::{Deref, DerefMut};
use std::sync::{Arc, Mutex};

use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};
use wbf_sdk::channel::Channel;
use wbf_sdk::client::WbfClient;
use wbf_sdk::sessions::Received;

use crate::error::CoreError;
use crate::event::{CoreEvent, EventSink, LinkState};

/// 四條線的角色（link-pool.md §1）。分界是「誰會塞爆佇列」與「掉了救不救得回來」，🚫 不是照 kind。
/// 📌 房間事件與金鑰事件的訂閱**暫時共用一條**（維護者 2026-09-21：server 每台裝置預設 4 條 WS，先不動 server）；將來要分就是多一個角色。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LinkRole {
    /// 一問一答：Hello／Ping、Info、橋、Event/Send、Recent（拉窗）、Device/Fetch／ItemsDestroy。
    Misc,
    /// Upload/*。
    Upload,
    /// Download/*。
    Download,
    /// 訂閱線：全局房間事件（Event/Subscribe／Push／DeviceChanged）與全局金鑰事件（Device/Subscribe／Push／CryptoState）。只有訂閱命令會開它。
    Subscriptions,
}

impl LinkRole {
    pub const ALL: [LinkRole; 4] = [
        LinkRole::Misc,
        LinkRole::Upload,
        LinkRole::Download,
        LinkRole::Subscriptions,
    ];

    /// Return:
    ///     &str  example: "misc"
    pub fn name(&self) -> &'static str {
        match self {
            LinkRole::Misc => "misc",
            LinkRole::Upload => "upload",
            LinkRole::Download => "download",
            LinkRole::Subscriptions => "subscriptions",
        }
    }

    /// 這條線是不是訂閱線（預設不開、只有訂閱命令會開，§3）。
    pub fn is_subscription(&self) -> bool {
        matches!(self, LinkRole::Subscriptions)
    }
}

/// 一格：那條線的 client，`None` ＝ 沒開（Idle）。`Option` 在 async mutex 裡，所以「開」與「用」是同一把鎖下的事：
/// 同一條線同時進來的兩個命令，一個開、另一個等它開好就用（🚫 不會各開一條）。
type Slot = Arc<AsyncMutex<Option<WbfClient<Channel>>>>;

/// 一個帳號的五條線。
pub struct LinkPool {
    user: String,
    events: EventSink,
    slots: Mutex<HashMap<LinkRole, Slot>>,
}

impl LinkPool {
    /// Args:
    ///     user: 這個池是誰的（事件裡的 `user`）, example: "@alice:localhost"
    pub(crate) fn new(user: &str, events: EventSink) -> LinkPool {
        LinkPool {
            user: user.to_string(),
            events,
            slots: Mutex::new(HashMap::new()),
        }
    }

    pub fn user(&self) -> &str {
        &self.user
    }

    fn slot(&self, role: LinkRole) -> Slot {
        self.slots
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .entry(role)
            .or_default()
            .clone()
    }

    /// 拿那條線來用：沒開就開、發現死了就重開，然後把 guard 交出去（一條線一次一個命令，§5）。
    ///
    /// Args:
    ///     role: example: LinkRole::Misc
    ///     open: 怎麼開一條這個角色的線, example: || core.open_link(&account, role)
    /// Return:
    ///     Ok(PooledClient)   開著的線；丟掉 guard 就是把線還回池裡（線本身還開著）
    ///     Err(...)           `open` 的錯原樣（沒 session 是 Usage、連不上是 Network）；池裡那格維持沒開，🚫 不發事件
    ///
    /// 📌 登出跟這裡的競賽由 server 決定，不由池猜（link-pool.md §3）：登出先撤 token 再 `close_all`，之後才開的線在 hello 就被 server 拒；
    /// 正在開的那一條由 `close_all` 等它的鎖、開完、命令做完再收。
    pub async fn acquire<F, Fut>(&self, role: LinkRole, open: F) -> Result<PooledClient, CoreError>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<WbfClient<Channel>, CoreError>>,
    {
        let slot = self.slot(role);
        let mut guard = slot.lock_owned().await;
        let found_dead = guard
            .as_ref()
            .is_some_and(|client| client.channel().is_closed());
        if found_dead {
            *guard = None;
            self.emit_link(
                role,
                LinkState::Closed,
                Some("found closed when it was next needed".to_string()),
            );
        }
        if guard.is_none() {
            *guard = Some(open().await?);
            self.emit_link(role, LinkState::Opened, None);
        }
        Ok(PooledClient::Pooled(guard))
    }

    /// 登出、destroy、換 session：全關（token 撤了，留著也是死的）。**等正在用線的命令做完**才關那條（維護者 2026-09-21：還在處理的要處理完）；
    /// 正在開的那一條也一樣（開的人握著鎖）。關完這個池還能再用，但 `Core` 會把它從註冊表拿掉，下一個命令用新 session 建新池。
    ///
    /// Args:
    ///     reason: example: "logged out"
    /// Return:
    ///     usize   關掉了幾條開著的
    pub async fn close_all(&self, reason: &str) -> usize {
        let slots: Vec<(LinkRole, Slot)> = self
            .slots
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .drain()
            .collect();
        let mut closed = 0;
        for (role, slot) in slots {
            let mut guard = slot.lock().await;
            if let Some(client) = guard.take() {
                drop(client);
                closed += 1;
                self.emit_link(role, LinkState::Closed, Some(reason.to_string()));
            }
        }
        closed
    }

    /// Return:
    ///     usize   現在開著的線數（正在被用的也算開著）。給 `daemon.info`
    pub fn open_count(&self) -> usize {
        let slots: Vec<Slot> = self
            .slots
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values()
            .cloned()
            .collect();
        slots
            .iter()
            .filter(|slot| match slot.try_lock() {
                Ok(guard) => guard
                    .as_ref()
                    .is_some_and(|client| !client.channel().is_closed()),
                // 鎖在別人手上 ＝ 有命令正在用它 ＝ 它是開的。
                Err(_) => true,
            })
            .count()
    }

    fn emit_link(&self, role: LinkRole, state: LinkState, reason: Option<String>) {
        self.events.emit(CoreEvent::Link {
            user: self.user.clone(),
            role,
            state,
            reason,
        });
    }
}

/// `ReceivedHook` 的那一頭（link-pool.md §4）：每個收到的 pack 變一則 `CoreEvent::Received`，**只有標頭**、🚫 不帶 meta／data。
/// 讀取 task 上叫的，表鎖之外；`EventSink::emit` 是 broadcast 的 try_send，不擋。
pub(crate) fn received_hook(
    events: EventSink,
    user: String,
    role: LinkRole,
) -> wbf_sdk::ReceivedHook {
    Arc::new(move |received: &Received<'_>| {
        events.emit(CoreEvent::Received {
            user: user.clone(),
            role,
            kind: received.pack.kind as u8,
            subtype: received.pack.subtype,
            id: received.pack.id,
            seq: received.pack.seq,
            route: received.route,
        });
    })
}

/// 「登出中」的範圍（account-session.md §4 第 1 與第 5 步）：活著就封池，丟掉就解封。
pub(crate) struct LoggingOutGuard<'a> {
    core: &'a crate::Core,
    account: &'a crate::accounts::AccountDir,
}

impl Drop for LoggingOutGuard<'_> {
    fn drop(&mut self) {
        self.core.end_logging_out(self.account);
    }
}

/// 池裡拿出來的一條線。丟掉就是還回去（線不關）。
pub enum PooledClient {
    /// 池裡那格的 guard：同一條線的下一個命令等它被丟掉。
    Pooled(OwnedMutexGuard<Option<WbfClient<Channel>>>),
    /// 不進池的（`Transport::Http` 那種一次性的）。
    Own(WbfClient<Channel>),
}

impl Deref for PooledClient {
    type Target = WbfClient<Channel>;

    fn deref(&self) -> &WbfClient<Channel> {
        match self {
            PooledClient::Pooled(guard) => guard
                .as_ref()
                .expect("a pooled slot is filled before it is handed out"),
            PooledClient::Own(client) => client,
        }
    }
}

impl DerefMut for PooledClient {
    fn deref_mut(&mut self) -> &mut WbfClient<Channel> {
        match self {
            PooledClient::Pooled(guard) => guard
                .as_mut()
                .expect("a pooled slot is filled before it is handed out"),
            PooledClient::Own(client) => client,
        }
    }
}

/// 池開線時 `Hello` 報的名字。
pub const LINK_CLIENT_NAME: &str = "wbf-core/0.1";

/// 開一條某個角色的線要向 server 宣告哪些 feature（link-pool.md §7）。
/// ⚠️ `Keys` 之後宣告 `org.wbftw.device_versions`（那時 `Event/Send` 也一起接上，宣告了就得帶 `room_version`）；現在全部是空的。
pub fn features_of(role: LinkRole) -> &'static [&'static str] {
    match role {
        LinkRole::Misc | LinkRole::Upload | LinkRole::Download | LinkRole::Subscriptions => &[],
    }
}

impl crate::Core {
    /// 這個帳號的池；沒有就建一個。⚠️ 要有 session（池以它的 `user_id` 命名事件）——沒登入的帳號沒有池，也開不了線。
    ///
    /// Return:
    ///     Ok(Arc<LinkPool>)
    ///     Err(Usage)         這個帳號沒登入
    pub(crate) fn pool_of_account(
        &self,
        account: &crate::accounts::AccountDir,
    ) -> Result<Arc<LinkPool>, CoreError> {
        // 登出中：封池（account-session.md §4 第 1 步）。正在跑的命令握著自己的 guard，不從這裡進來，不受影響。
        if self.is_logging_out(account) {
            return Err(CoreError::new(
                crate::error::CoreErrorKind::AccountBusy,
                format!(
                    "{} is logging out; nothing more is sent on its links",
                    account.label()
                ),
            ));
        }
        let user = self.session_of(account)?.user_id;
        Ok(self
            .link_pools
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .entry(account.dir.clone())
            .or_insert_with(|| Arc::new(LinkPool::new(&user, self.events.clone())))
            .clone())
    }

    /// 正式的「開一條線」（link-pool.md §3、§7）：session → `Channel::connect`（Bearer 升級，這就是登入）→ `hello`。
    /// 每個收到的 pack 經鉤子變 `CoreEvent::Received`。
    ///
    /// Return:
    ///     Ok(WbfClient)   開好、hello 過了
    ///     Err(Usage)      沒登入
    ///     Err(Network)    連不上、hello 沒回
    pub(crate) async fn open_link(
        &self,
        account: &crate::accounts::AccountDir,
        role: LinkRole,
    ) -> Result<WbfClient<Channel>, CoreError> {
        let session = self.session_of(account)?;
        let hook = received_hook(self.events.clone(), session.user_id.clone(), role);
        let channel = wbf_sdk::channel::WsChannel::connect_with_hook(
            &session.server,
            &session.access_token,
            hook,
        )
        .await?;
        let mut client = WbfClient::new(Channel::WebSocket(Box::new(channel)));
        client.hello(LINK_CLIENT_NAME, features_of(role)).await?;
        Ok(client)
    }

    /// 登出的封池（account-session.md §4）：guard 活著的期間 `pool_of_account` 一律 `AccountBusy`，guard 丟掉就解封——
    /// 成功、失敗、提前 return、future 被 drop 都走同一條（PR #54 審查 cirno／salvia／rumia 🔴：第一版只在失敗分支解封，成功登出後重登入會卡 AccountBusy 到 daemon 重開）。
    pub(crate) fn logging_out_guard<'a>(
        &'a self,
        account: &'a crate::accounts::AccountDir,
    ) -> LoggingOutGuard<'a> {
        self.begin_logging_out(account);
        LoggingOutGuard {
            core: self,
            account,
        }
    }

    /// 登出的封池：放進去之後 `pool_of_account` 一律 `AccountBusy`。🚫 生產路徑用 [`Core::logging_out_guard`]，不要手動配對。
    pub(crate) fn begin_logging_out(&self, account: &crate::accounts::AccountDir) {
        self.logging_out
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(account.dir.clone());
    }

    /// 解封：HTTP 登出失敗（no-op、連線照常）或本地清完（之後 `session_of` 自己會回 NotLoggedIn）。
    pub(crate) fn end_logging_out(&self, account: &crate::accounts::AccountDir) {
        self.logging_out
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&account.dir);
    }

    pub(crate) fn is_logging_out(&self, account: &crate::accounts::AccountDir) -> bool {
        self.logging_out
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .contains(&account.dir)
    }

    /// 登出、destroy：這個帳號的池整個拿掉、開著的線全關。沒有池就什麼都不做。
    ///
    /// Return:
    ///     usize   關掉了幾條
    pub(crate) async fn close_links(
        &self,
        account: &crate::accounts::AccountDir,
        reason: &str,
    ) -> usize {
        let pool = self
            .link_pools
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&account.dir);
        match pool {
            Some(pool) => pool.close_all(reason).await,
            None => 0,
        }
    }

    /// Return:
    ///     usize   所有帳號加起來現在開著幾條線（`daemon.info`）
    pub fn open_link_count(&self) -> usize {
        let pools: Vec<Arc<LinkPool>> = self
            .link_pools
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values()
            .cloned()
            .collect();
        pools.iter().map(|pool| pool.open_count()).sum()
    }

    /// 從外面塞一則事件進 core 的廣播。**測試用**（daemon 的推播測試要一則可控的事件）；正式程式碼🚫 不叫它——
    /// 事件是 core 發生的事，不是別人替它說的。
    #[doc(hidden)]
    pub fn emit_event(&self, event: CoreEvent) {
        self.events.emit(event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::CoreErrorKind;
    use wbf_sdk::channel::WsChannel;
    use wbf_sdk::transport::{memory_pair, MemoryEnd};
    use wbf_sdk::WsLink;

    /// 一個假的「開連線」：記憶體對接，另一端交給測試握著（丟掉它的 sink 就是對方關線）。
    fn memory_client(hook: wbf_sdk::ReceivedHook) -> (WbfClient<Channel>, MemoryEnd) {
        let (client_end, server_end) = memory_pair(8);
        let link = WsLink::start(client_end.source, client_end.sink, hook);
        let channel = Channel::WebSocket(Box::new(WsChannel::from_link(link)));
        (WbfClient::new(channel), server_end)
    }

    fn connection_id_of(client: &WbfClient<Channel>) -> u64 {
        match client.channel() {
            Channel::WebSocket(channel) => channel.link().connection_id(),
            Channel::Http(_) => unreachable!("tests only pool websocket links"),
        }
    }

    fn drain(events: &mut tokio::sync::broadcast::Receiver<CoreEvent>) -> Vec<CoreEvent> {
        let mut seen = Vec::new();
        while let Ok(event) = events.try_recv() {
            seen.push(event);
        }
        seen
    }

    #[tokio::test]
    async fn the_same_role_reuses_the_open_line_and_different_roles_get_different_lines() {
        let events = EventSink::new();
        let mut seen = events.subscribe();
        let pool = LinkPool::new("@alice:localhost", events);
        let mut peers = Vec::new();
        let mut opened = 0;
        let mut open = || {
            opened += 1;
            let (client, peer) = memory_client(wbf_sdk::no_hook());
            peers.push(peer);
            async move { Ok(client) }
        };
        let first = connection_id_of(&pool.acquire(LinkRole::Misc, &mut open).await.unwrap());
        let again = connection_id_of(&pool.acquire(LinkRole::Misc, &mut open).await.unwrap());
        let upload = connection_id_of(&pool.acquire(LinkRole::Upload, &mut open).await.unwrap());
        assert_eq!(first, again, "同一角色第二次拿到同一條");
        assert_ne!(first, upload, "不同角色是不同條");
        assert_eq!(opened, 2, "只開了兩條");
        assert_eq!(pool.open_count(), 2);
        assert_eq!(
            drain(&mut seen),
            vec![
                CoreEvent::Link {
                    user: "@alice:localhost".into(),
                    role: LinkRole::Misc,
                    state: LinkState::Opened,
                    reason: None
                },
                CoreEvent::Link {
                    user: "@alice:localhost".into(),
                    role: LinkRole::Upload,
                    state: LinkState::Opened,
                    reason: None
                },
            ]
        );
    }

    /// 對方關了線：下一次要用才發現，發 Closed、重開、發 Opened；中間🚫 沒有人在背景做事。
    #[tokio::test]
    async fn a_dead_line_is_reopened_the_next_time_it_is_needed() {
        let events = EventSink::new();
        let mut seen = events.subscribe();
        let pool = LinkPool::new("@alice:localhost", events);
        let (client, peer) = memory_client(wbf_sdk::no_hook());
        let first = connection_id_of(
            &pool
                .acquire(LinkRole::Subscriptions, || async move { Ok(client) })
                .await
                .unwrap(),
        );
        drop(peer);
        // 讓讀取 task 看到「對方關了」。
        for _ in 0..50 {
            tokio::task::yield_now().await;
        }
        assert_eq!(pool.open_count(), 0, "死了就不算開著");
        drain(&mut seen);
        let (client, _peer) = memory_client(wbf_sdk::no_hook());
        let second = connection_id_of(
            &pool
                .acquire(LinkRole::Subscriptions, || async move { Ok(client) })
                .await
                .unwrap(),
        );
        assert_ne!(first, second, "重開是新的一條");
        let states: Vec<(LinkState, bool)> = drain(&mut seen)
            .into_iter()
            .map(|event| match event {
                CoreEvent::Link { state, reason, .. } => (state, reason.is_some()),
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(
            states,
            vec![(LinkState::Closed, true), (LinkState::Opened, false)]
        );
    }

    /// 開不起來（沒 session、連不上）：錯原樣回、那格維持沒開、🚫 不發事件；下一次再試。
    #[tokio::test]
    async fn a_failed_open_leaves_the_slot_empty_and_emits_nothing() {
        let events = EventSink::new();
        let mut seen = events.subscribe();
        let pool = LinkPool::new("@alice:localhost", events);
        let outcome = pool
            .acquire(LinkRole::Misc, || async {
                Err(CoreError::new(CoreErrorKind::Usage, "not logged in"))
            })
            .await;
        assert!(matches!(outcome, Err(error) if error.kind == CoreErrorKind::Usage));
        assert_eq!(pool.open_count(), 0);
        assert!(drain(&mut seen).is_empty());
        let (client, _peer) = memory_client(wbf_sdk::no_hook());
        assert!(pool
            .acquire(LinkRole::Misc, || async move { Ok(client) })
            .await
            .is_ok());
    }

    /// 登出撞上正在開線的 acquire（PR #53 審查 rumia 🔴1；維護者 2026-09-21 定：還在處理的要處理完）：
    /// `close_all` 等那一格的鎖——開完、命令用完、guard 丟掉——才收那條；收完池裡沒有線。順序由 oneshot 控制，不靠運氣。
    /// 📌 「之後不能再開」不是池的事：server 撤了 token，hello 就拒；本地刪了 session，`open_link` 連試都不試。
    #[tokio::test]
    async fn a_logout_waits_for_the_line_being_opened_and_then_closes_it() {
        let events = EventSink::new();
        let mut seen = events.subscribe();
        let pool = Arc::new(LinkPool::new("@alice:localhost", events));
        let (release, released) = tokio::sync::oneshot::channel::<()>();
        let (client, _peer) = memory_client(wbf_sdk::no_hook());
        let acquiring = tokio::spawn({
            let pool = pool.clone();
            async move {
                pool.acquire(LinkRole::Misc, || async move {
                    released.await.unwrap();
                    Ok(client)
                })
                .await
                .map(|line| connection_id_of(&line))
            }
        });
        // 讓 acquire 走到 `open().await` 裡面等（它握著那一格的鎖）。
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
        // 登出：等那一格的鎖——鎖在 acquire 手上，要等 open 回來、命令做完才放。
        let closing = tokio::spawn({
            let pool = pool.clone();
            async move { pool.close_all("logged out").await }
        });
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
        assert!(!closing.is_finished(), "登出在等正在開的那條");
        release.send(()).unwrap();
        let outcome = acquiring.await.unwrap();
        assert!(outcome.is_ok(), "開好的那條交給命令用：{outcome:?}");
        // 命令做完（guard 在 spawn 裡就丟了），登出才收得到它。
        assert_eq!(closing.await.unwrap(), 1, "收了那條剛開好、用完的線");
        assert_eq!(pool.open_count(), 0);
        let states: Vec<LinkState> = drain(&mut seen)
            .into_iter()
            .map(|event| match event {
                CoreEvent::Link { state, .. } => state,
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(states, vec![LinkState::Opened, LinkState::Closed]);
    }

    #[tokio::test]
    async fn close_all_closes_every_open_line_and_says_why() {
        let events = EventSink::new();
        let mut seen = events.subscribe();
        let pool = LinkPool::new("@alice:localhost", events);
        let mut peers = Vec::new();
        for role in [LinkRole::Misc, LinkRole::Download, LinkRole::Subscriptions] {
            let (client, peer) = memory_client(wbf_sdk::no_hook());
            peers.push(peer);
            pool.acquire(role, || async move { Ok(client) })
                .await
                .unwrap();
        }
        drain(&mut seen);
        assert_eq!(pool.close_all("logged out").await, 3);
        assert_eq!(pool.open_count(), 0);
        let closed: Vec<LinkRole> = drain(&mut seen)
            .into_iter()
            .map(|event| match event {
                CoreEvent::Link {
                    role,
                    state: LinkState::Closed,
                    reason: Some(reason),
                    ..
                } if reason == "logged out" => role,
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(closed.len(), 3);
        assert!(closed.contains(&LinkRole::Subscriptions));
        // 關過之後再要就是重開，不是拿到一條死的（池不記「登出了沒」：那是 session 與 server 的事）。
        let (client, _peer) = memory_client(wbf_sdk::no_hook());
        pool.acquire(LinkRole::Misc, || async move { Ok(client) })
            .await
            .unwrap();
        assert_eq!(pool.open_count(), 1);
    }

    fn sample_push() -> wbf_wire::Pack {
        wbf_wire::Pack {
            kind: wbf_wire::Kind::Device,
            subtype: wbf_wire::pack::device::PUSH,
            flags: wbf_wire::pack::flags::IS_RESPONSE,
            id: 0x0100_0000_0000_0001,
            seq: 3,
            meta: b"{\"secret\":\"not copied\"}".to_vec(),
            data: vec![7; 64],
        }
    }

    /// 鉤子：一個 pack 變一則 `Received`，只有標頭（meta／data 不在事件裡）、帶那條線的角色。
    #[test]
    fn the_hook_turns_a_pack_into_a_header_only_event_with_the_role() {
        let sink = EventSink::new();
        let mut seen = sink.subscribe();
        let hook = received_hook(sink, "@alice:localhost".into(), LinkRole::Subscriptions);
        hook(&Received {
            connection_id: 9,
            session: None,
            route: wbf_sdk::Route::Unmatched,
            pack: &sample_push(),
        });
        assert_eq!(
            seen.try_recv().unwrap(),
            CoreEvent::Received {
                user: "@alice:localhost".into(),
                role: LinkRole::Subscriptions,
                kind: 0x16,
                subtype: wbf_wire::pack::device::PUSH,
                id: 0x0100_0000_0000_0001,
                seq: 3,
                route: wbf_sdk::Route::Unmatched,
            }
        );
    }

    /// 整條路：對面送一個 pack 進池裡的線 → 讀取 task → 鉤子 → `Received` 事件。
    #[tokio::test]
    async fn a_pack_arriving_on_a_pooled_line_becomes_a_received_event() {
        let events = EventSink::new();
        let mut seen = events.subscribe();
        let hook = received_hook(
            events.clone(),
            "@alice:localhost".into(),
            LinkRole::Subscriptions,
        );
        let (client, mut peer) = memory_client(hook);
        let pool = LinkPool::new("@alice:localhost", events);
        let _line = pool
            .acquire(LinkRole::Subscriptions, || async move { Ok(client) })
            .await
            .unwrap();
        use wbf_sdk::transport::FrameSink;
        peer.sink
            .send(sample_push().encode().unwrap())
            .await
            .unwrap();
        let received = loop {
            match tokio::time::timeout(std::time::Duration::from_secs(5), seen.recv())
                .await
                .expect("the event arrives")
                .unwrap()
            {
                event @ CoreEvent::Received { .. } => break event,
                _ => continue,
            }
        };
        assert!(
            matches!(
                received,
                CoreEvent::Received {
                    role: LinkRole::Subscriptions,
                    kind: 0x16,
                    seq: 3,
                    ..
                }
            ),
            "{received:?}"
        );
    }
}

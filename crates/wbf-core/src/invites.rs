//! 邀請（/docs/design/rooms/room-actions.md §3.3）：房間線上推來的 `Invite/Push`／`Gone` 寫進本地、發 `room.membership`；
//! UI 叫的 `room.invites` 補一輪完整快照。
//!
//! 維護者 2026-10-14 定的形狀：開線只 `Invite/Subscribe`（`room_sync.rs`）；補拿一輪是 UI 叫的 RPC，daemon 🚫 自己補、🚫 存水位
//! （server 定了每次一輪完整快照，wbfuwunel 的 /docs/design/events/invites-on-the-wire.md §3.3）；推播的 `gap` 原樣轉給 UI。
//! 一輪跑的期間被推播碰過的房記在 [`InviteRounds`]（只在記憶體、那一輪回了就丟），補完時以比較新的為準（同一份 §8 ②'）。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use wbf_sdk::chat::{Invite, Membership, RoomInvite};
use wbf_sdk::protocol::{InviteItem, InviteSubscribeReply};

use crate::error::CoreError;
use crate::event::EventSink;
use crate::server_cache::ServerCache;
use crate::{Core, CoreEvent, Target};

/// 一輪 `Fetch` 的每個 `Batch` 之間最多等多久。
const BATCH_TIMEOUT: Duration = Duration::from_secs(30);

/// 正在跑的那幾輪：帳號 → 這一輪開始之後被推播碰過的房。
/// 一個帳號可能同時有兩支 `room.invites` 在跑：各自數，最後一支回了才丟。
#[derive(Default)]
pub(crate) struct InviteRounds {
    open: Mutex<HashMap<String, OpenRound>>,
}

#[derive(Default)]
struct OpenRound {
    callers: usize,
    /// 房 → 碰過它的推播裡最大的 `is`
    touched: HashMap<String, u64>,
}

/// 一輪開著的證明；丟掉就算這一輪結束了（成功、失敗、RPC 被取消都一樣）。
struct RoundGuard {
    rounds: Arc<InviteRounds>,
    me: String,
}

impl InviteRounds {
    /// 一輪開始（要在送 `Fetch` **之前**：之後推來的才記得到）。
    fn begin(self: &Arc<Self>, me: &str) -> RoundGuard {
        let mut open = self
            .open
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        open.entry(me.to_string()).or_default().callers += 1;
        RoundGuard {
            rounds: self.clone(),
            me: me.to_string(),
        }
    }

    /// 推播碰到一間房：有一輪開著才記（沒開就沒人要讀）。🚨 要在推播寫進本地**之前**叫——
    /// 那一輪在寫入者裡讀它，所以「記了還沒寫」的推播會讓快照讓開、之後推播自己寫。
    ///
    /// Args:
    ///     me: 被邀請的帳號, example: "@bob:localhost"
    ///     room: example: "!r:localhost"
    ///     is: 那則推播的位置, example: 4711
    pub(crate) fn mark_touched(&self, me: &str, room: &str, is: u64) {
        let mut open = self
            .open
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(round) = open.get_mut(me) {
            let newest = round.touched.entry(room.to_string()).or_insert(is);
            *newest = (*newest).max(is);
        }
    }

    /// Return:
    ///     HashMap<String, u64>  這個帳號開著的那一輪裡碰過的房；沒開就是空的
    fn list_touched(&self, me: &str) -> HashMap<String, u64> {
        self.open
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(me)
            .map(|round| round.touched.clone())
            .unwrap_or_default()
    }
}

impl Drop for RoundGuard {
    fn drop(&mut self) {
        let mut open = self
            .rounds
            .open
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let finished = match open.get_mut(&self.me) {
            Some(round) => {
                round.callers = round.callers.saturating_sub(1);
                round.callers == 0
            }
            None => false,
        };
        if finished {
            open.remove(&self.me);
        }
    }
}

/// 房間線上推來的一則邀請推播（`room_sync.rs` 的 task 叫）：先記「這一輪碰過」、再寫本地、寫成了才發 `room.membership`（`gap` 原樣）。
/// 寫不進去只講一聲、🚫 發（沒落地的就不通知，跟 `Event/Push` 同一條）；`Gone` 的身分不認得也只講一聲、🚫 猜。
///
/// Args:
///     me: 這條線的帳號, example: "@bob:localhost"
///     reply: 訂閱上收到的那一則
///     local_gap: 本地在這之前丟過邀請推播（收件匣滿、壞包）：併進 server 的 `gap` 一起給 UI, example: false
pub(crate) async fn record_invite_push(
    me: &str,
    cache: &ServerCache,
    events: &EventSink,
    rounds: &InviteRounds,
    reply: InviteSubscribeReply,
    local_gap: bool,
) {
    match reply {
        InviteSubscribeReply::Push { invite, is, gap } => {
            rounds.mark_touched(me, &invite.room_id, is);
            let room = invite.room_id.clone();
            let me_here = me.to_string();
            match cache
                .run(move |cache| cache.record_invite(&me_here, &invite))
                .await
            {
                Ok(written) => events.emit(CoreEvent::Membership {
                    user: me.to_string(),
                    room,
                    membership: Membership::Invite,
                    invite: Some(Invite::from_item(written, me)),
                    gap: gap || local_gap,
                }),
                Err(error) => events.progress(format!(
                    "invites: an invite to {room} could not be stored (ignored; room.invites refetches it): {error}"
                )),
            }
        }
        InviteSubscribeReply::Gone {
            room_id,
            membership,
            is,
            gap,
        } => {
            let Some(membership) = Membership::find_by_name(&membership) else {
                events.progress(format!(
                    "invites: the invite to {room_id} ended as {membership:?}, which is not a membership; ignored (room.invites refetches the list)"
                ));
                return;
            };
            rounds.mark_touched(me, &room_id, is);
            let room = room_id.clone();
            let me_here = me.to_string();
            match cache
                .run(move |cache| cache.record_membership(&me_here, &room_id, membership, None))
                .await
            {
                Ok(_) => events.emit(CoreEvent::Membership {
                    user: me.to_string(),
                    room,
                    membership,
                    invite: None,
                    gap: gap || local_gap,
                }),
                Err(error) => events.progress(format!(
                    "invites: the end of the invite to {room} could not be stored (ignored; room.invites refetches the list): {error}"
                )),
            }
        }
        InviteSubscribeReply::Acknowledged(_) | InviteSubscribeReply::Unsubscribed { .. } => {}
    }
}

impl Core {
    /// `room.invites`：主動補一輪邀請（/docs/design/rooms/room-actions.md §3.3.4）——UI 首次啟動叫一次、看到推播帶 `gap` 時再叫。
    /// wbf 帳號跑一輪 `Invite/Fetch` 到 `r == 0`；一般 Matrix 帳號跑一次 `sync_once`、讀 store 裡的被邀請清單（§3.3.5）。
    /// 補完照 §3.3.3 寫進本地（一個 transaction）、有變的列發 `room.membership`（`gap: false`），再從本地讀回去。
    ///
    /// Return:
    ///     Ok(Vec<RoomInvite>)  現在所有待處理的邀請（本地讀的，照房間 id 排）
    ///     Err(Server)          server 拒（`Unsupported`：server 還不認得 `Invite`）
    ///     Err(Network)         一輪中途斷了：🚫 寫任何一列，再叫一次就是新的一輪
    ///     Err(Protocol)        Batch 形狀錯
    pub async fn fetch_invites(&self, target: &Target) -> Result<Vec<RoomInvite>, CoreError> {
        let account = self.account_or_current(target)?;
        let (cache, me) = self.server_cache_and_me(&account)?;
        // 一輪開著的期間（到寫完為止）推來的才算「碰過」；一般 Matrix 帳號沒有推播，開了也只是空的。
        let round_open = self.invite_rounds.begin(&me);
        let (round, departed): (Vec<(u64, InviteItem)>, HashMap<String, Membership>) =
            if self.is_wbf_account(&account)? {
                let fetched = self
                    .misc_client(&account)
                    .await?
                    .invite_fetch_round(BATCH_TIMEOUT)
                    .await?;
                (fetched.items, HashMap::new())
            } else {
                let backend = self
                    .synced_backend_of(&account, target.server_backup)
                    .await?;
                let invites = backend.list_invites().await?;
                let locally_invited: Vec<String> = cache
                    .read()
                    .await
                    .list_room_entries(&me, &[Membership::Invite])?
                    .into_iter()
                    .map(|entry| entry.id)
                    .collect();
                // 沒有 count：一般 Matrix 帳號沒有推播，`touched` 永遠是空的，用不到。
                let round = invites.into_iter().map(|invite| (0, invite)).collect();
                (round, backend.find_memberships(&locally_invited))
            };
        let rounds = self.invite_rounds.clone();
        let me_here = me.clone();
        let changes = cache
            .run(move |cache| {
                // 在寫入者裡讀：推播「先記、再寫」，所以這裡看得到每一則還沒寫的（`InviteRounds::mark_touched`）。
                let touched = rounds.list_touched(&me_here);
                cache.apply_invite_round(&me_here, &round, &touched, &departed)
            })
            .await?;
        drop(round_open);
        for change in changes {
            self.events.emit(CoreEvent::Membership {
                user: me.clone(),
                room: change.room_id,
                membership: change.membership,
                invite: change.invite.map(|invite| Invite::from_item(invite, &me)),
                gap: false,
            });
        }
        let mut invites: Vec<RoomInvite> = cache
            .read()
            .await
            .list_room_entries(&me, &[Membership::Invite])?
            .into_iter()
            .filter_map(|entry| {
                entry.invite.map(|invite| RoomInvite {
                    room: entry.id,
                    invite,
                })
            })
            .collect();
        invites.sort_by(|left, right| left.room.cmp(&right.room));
        Ok(invites)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use serde_json::{json, Value};
    use wbf_sdk::chat::{Invite, Membership};

    use super::InviteRounds;
    use crate::accounts::AccountDir;
    use crate::link_pool::LinkRole;
    use crate::test_support::*;
    use crate::{Core, CoreEvent, Target};

    /// 下一則 `room.membership`（最多 10 秒）：(房, 身分, 邀請, gap)。
    async fn next_membership(
        seen: &mut tokio::sync::broadcast::Receiver<CoreEvent>,
    ) -> (String, Membership, Option<Invite>, bool) {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let CoreEvent::Membership {
                    room,
                    membership,
                    invite,
                    gap,
                    ..
                } = seen.recv().await.unwrap()
                {
                    return (room, membership, invite, gap);
                }
            }
        })
        .await
        .expect("a room.membership arrives")
    }

    async fn invited_ids(core: &Core) -> Vec<String> {
        core.list_rooms(
            crate::SyncMode::Local,
            &[Membership::Invite],
            &Target::default(),
        )
        .await
        .unwrap()
        .into_iter()
        .map(|entry| entry.id)
        .collect()
    }

    /// 給這個帳號的池放一條 Misc 線（`room.invites` 的 `Fetch` 走它），回那條線的假 server。
    async fn with_misc_line(core: &Core, account: &AccountDir) -> FakeServer {
        let (misc, fake_misc) = memory_client_with_hello(Arc::new(Mutex::new(Vec::new()))).await;
        drop(
            core.pool_of_account(account)
                .unwrap()
                .acquire(LinkRole::Misc, || async move { Ok(misc) })
                .await
                .unwrap(),
        );
        fake_misc
    }

    fn member_event() -> Value {
        json!({ "type": "m.room.member", "state_key": "@a:localhost", "sender": "@alice:localhost",
                "content": { "membership": "invite" } })
    }

    /// /docs/design/rooms/room-actions.md §3.3：開線只訂閱（🚫 Fetch）→ 推來的 `Push`／`Gone` 寫進本地、各發一則 `room.membership`（`gap` 原樣）→
    /// UI 叫 `room.invites` 才補一輪：分好幾個 Batch、中途有一間被跳過也照樣到 `r == 0`；本地有、快照裡沒有、這一輪沒碰過的標 `leave`；有變的才推。
    #[tokio::test]
    async fn pushes_land_as_they_come_and_a_round_replaces_the_list() {
        let dir = scratch("invites-round");
        let (core, account) = core_with_wbf_account(&dir).await;
        let mut seen = core.subscribe();
        let (_room_subscription, fake, _pool) =
            subscribed(&core, &account, &Arc::new(Mutex::new(Vec::new()))).await;
        let invite_subscription = *fake
            .invites
            .lock()
            .unwrap()
            .subscription_ids
            .last()
            .expect("Invite/Subscribe");
        assert_eq!(
            fake.invites.lock().unwrap().fetches,
            0,
            "開線只訂閱，補一輪是 UI 的事"
        );

        fake.outbound
            .send(invite_push(
                invite_subscription,
                0,
                "!a:localhost",
                7,
                false,
                &[member_event()],
            ))
            .await
            .unwrap();
        let (room, membership, invite, gap) = next_membership(&mut seen).await;
        assert_eq!(
            (room.as_str(), membership, gap),
            ("!a:localhost", Membership::Invite, false)
        );
        let invite = invite.unwrap();
        assert_eq!(invite.inviter.as_deref(), Some("@alice:localhost"));
        assert!(invite.is_from_this_server);
        assert_eq!(invited_ids(&core).await, ["!a:localhost"]);

        fake.outbound
            .send(invite_gone(
                invite_subscription,
                1,
                "!a:localhost",
                "leave",
                8,
            ))
            .await
            .unwrap();
        let (room, membership, invite, _) = next_membership(&mut seen).await;
        assert_eq!(
            (room.as_str(), membership, invite),
            ("!a:localhost", Membership::Leave, None)
        );
        assert!(invited_ids(&core).await.is_empty());

        // 太大被降級（sc 0）、server 說之前掉過推播：照寫、`gap` 原樣給 UI（它就知道該叫 room.invites）
        fake.outbound
            .send(invite_push(
                invite_subscription,
                2,
                "!stale:localhost",
                9,
                true,
                &[],
            ))
            .await
            .unwrap();
        let (room, _, invite, gap) = next_membership(&mut seen).await;
        assert_eq!(room, "!stale:localhost");
        assert!(gap);
        assert_eq!(invite.unwrap().state, None);

        // UI 叫的一輪：兩個 Batch、一間在途中結束被跳過（bc 加總 < tc）、!stale 已經不在 server 上
        let fake_misc = with_misc_line(&core, &account).await;
        {
            let mut invites = fake_misc.invites.lock().unwrap();
            invites.pending = vec![
                (10, invite_entry("!b:localhost", &[member_event()])),
                (11, invite_entry("!c:localhost", &[member_event()])),
            ];
            invites.per_batch = 1;
            invites.skipped = 1;
        }
        let invites = core.fetch_invites(&Target::default()).await.unwrap();
        let rooms: Vec<&str> = invites.iter().map(|invite| invite.room.as_str()).collect();
        assert_eq!(rooms, ["!b:localhost", "!c:localhost"]);
        assert_eq!(fake_misc.invites.lock().unwrap().fetches, 1);
        let mut announced = Vec::new();
        for _ in 0..3 {
            let (room, membership, _, gap) = next_membership(&mut seen).await;
            assert!(!gap, "補一輪寫出來的🚫 帶 gap");
            announced.push((room, membership));
        }
        announced.sort_by(|left, right| left.0.cmp(&right.0));
        assert_eq!(
            announced,
            [
                ("!b:localhost".to_string(), Membership::Invite),
                ("!c:localhost".to_string(), Membership::Invite),
                ("!stale:localhost".to_string(), Membership::Leave),
            ]
        );
        assert_eq!(invited_ids(&core).await, ["!b:localhost", "!c:localhost"]);
        assert!(
            core.invite_rounds.list_touched("@a:localhost").is_empty(),
            "一輪回了，碰過的房就丟掉"
        );

        // 同一份再叫一次：什麼都沒變，🚫 推
        core.fetch_invites(&Target::default()).await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(300), next_membership(&mut seen))
                .await
                .is_err(),
            "沒變就🚫 推"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// server §8 ②'：一輪已經開始（`Fetch` 送出了）、快照還沒到之前推來的，比快照新——
    /// 新的邀請（快照的上界之後，不在快照裡）🚫 被當成「不在快照裡」刪掉；結束了的（`Gone` 的 `is` 比快照那筆大）🚫 被快照寫回來。
    #[tokio::test]
    async fn pushes_during_a_round_win_over_its_snapshot() {
        let dir = scratch("invites-during");
        let (core, account) = core_with_wbf_account(&dir).await;
        let mut seen = core.subscribe();
        let (_room_subscription, fake, _pool) =
            subscribed(&core, &account, &Arc::new(Mutex::new(Vec::new()))).await;
        let invite_subscription = *fake
            .invites
            .lock()
            .unwrap()
            .subscription_ids
            .last()
            .unwrap();
        let fake_misc = with_misc_line(&core, &account).await;
        let gate = Arc::new(tokio::sync::Notify::new());
        {
            let mut invites = fake_misc.invites.lock().unwrap();
            invites.pending = vec![(10, invite_entry("!b:localhost", &[member_event()]))];
            invites.hold_batches = Some(gate.clone());
        }
        let during = async {
            while fake_misc.invites.lock().unwrap().fetches == 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            fake.outbound
                .send(invite_push(
                    invite_subscription,
                    0,
                    "!new:localhost",
                    200,
                    false,
                    &[member_event()],
                ))
                .await
                .unwrap();
            fake.outbound
                .send(invite_gone(
                    invite_subscription,
                    1,
                    "!b:localhost",
                    "leave",
                    201,
                ))
                .await
                .unwrap();
            // 兩則都寫進本地了，才讓快照過來
            for _ in 0..2 {
                next_membership(&mut seen).await;
            }
            gate.notify_one();
        };
        let target = Target::default();
        let (round, ()) = tokio::join!(core.fetch_invites(&target), during);
        round.unwrap();
        assert_eq!(invited_ids(&core).await, ["!new:localhost"]);
        let left: Vec<String> = core
            .list_rooms(
                crate::SyncMode::Local,
                &[Membership::Leave],
                &Target::default(),
            )
            .await
            .unwrap()
            .into_iter()
            .map(|entry| entry.id)
            .collect();
        assert_eq!(
            left,
            ["!b:localhost"],
            "Gone 的 is 比快照那筆大：🚫 被寫回 invite"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 一輪中途斷了（server 送完第一個 Batch 就回 Error）：回錯、🚫 寫任何一列、🚫 刪本地的（補到一半的不是快照）。
    #[tokio::test]
    async fn a_round_cut_short_changes_nothing() {
        let dir = scratch("invites-cut");
        let (core, account) = core_with_wbf_account(&dir).await;
        let (_room_subscription, fake, _pool) =
            subscribed(&core, &account, &Arc::new(Mutex::new(Vec::new()))).await;
        let invite_subscription = *fake
            .invites
            .lock()
            .unwrap()
            .subscription_ids
            .last()
            .unwrap();
        let mut seen = core.subscribe();
        fake.outbound
            .send(invite_push(
                invite_subscription,
                0,
                "!old:localhost",
                3,
                false,
                &[member_event()],
            ))
            .await
            .unwrap();
        next_membership(&mut seen).await;
        let fake_misc = with_misc_line(&core, &account).await;
        {
            let mut invites = fake_misc.invites.lock().unwrap();
            invites.pending = vec![
                (10, invite_entry("!b:localhost", &[])),
                (11, invite_entry("!c:localhost", &[])),
            ];
            invites.per_batch = 1;
            invites.cut_after_first_batch = true;
        }
        assert!(core.fetch_invites(&Target::default()).await.is_err());
        assert_eq!(
            invited_ids(&core).await,
            ["!old:localhost"],
            "🚫 寫 !b、🚫 刪 !old"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// server 還不認得 `Invite`（`Subscribe` 被拒）：房間的訂閱照跑，講一聲。
    #[tokio::test]
    async fn a_server_without_invites_still_syncs_rooms() {
        let dir = scratch("invites-refused");
        let (core, account) = core_with_wbf_account(&dir).await;
        let mut seen = core.subscribe();
        let (mut client, fake) = memory_client_with_hello(Arc::new(Mutex::new(Vec::new()))).await;
        fake.invites.lock().unwrap().refuse_subscribe = true;
        core.init_connection(&account, LinkRole::Rooms, &mut client)
            .await
            .expect("the room subscription does not depend on invites");
        assert!(core.is_room_syncing(&account));
        let said = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let CoreEvent::Note { text, .. } = seen.recv().await.unwrap() {
                    if text.contains("Invite/Subscribe") {
                        return text;
                    }
                }
            }
        })
        .await
        .expect("it says so");
        assert!(said.contains("room.invites"), "{said}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 碰過的房只在有一輪開著時記；兩支同時跑的，最後一支回了才丟；同一間取最大的 `is`。
    #[test]
    fn touched_rooms_are_kept_only_while_a_round_is_open() {
        let rounds = Arc::new(InviteRounds::default());
        rounds.mark_touched("@a:x", "!r", 5);
        assert!(
            rounds.list_touched("@a:x").is_empty(),
            "沒有一輪開著：🚫 記"
        );
        let first = rounds.begin("@a:x");
        let second = rounds.begin("@a:x");
        rounds.mark_touched("@a:x", "!r", 9);
        rounds.mark_touched("@a:x", "!r", 7);
        rounds.mark_touched("@b:x", "!r", 1);
        assert_eq!(rounds.list_touched("@a:x").get("!r"), Some(&9));
        assert!(rounds.list_touched("@b:x").is_empty(), "別的帳號沒開");
        drop(first);
        assert_eq!(
            rounds.list_touched("@a:x").get("!r"),
            Some(&9),
            "還有一支在跑"
        );
        drop(second);
        assert!(rounds.list_touched("@a:x").is_empty());
    }
}

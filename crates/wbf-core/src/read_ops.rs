//! 已讀：標已讀（`room.read`）與未讀數（`room.get` 的 `unread`）（/docs/design/messages/read-receipts.md）。
//! 本地水位只往前；上游照 UI 給的送，收不收由 server 決定。私人還是公開由呼叫者定好再傳進來（conf 的預設是 daemon 的事）。

use serde_json::{json, Map};

use wbf_sdk::cache::{ReadAt, Unread};
use wbf_sdk::matrix_endpoint;

use crate::error::{CoreError, CoreErrorKind};
use crate::rooms_ops::SyncMode;
use crate::{Core, Target};

/// [`Core::mark_read`] 的結果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MarkedRead {
    /// `local`／`both` 是寫完之後本地的樣子；`server` 是本地沒動的樣子
    pub unread: Unread,
    /// 送上游的是哪一種回條；`local` 是 None, example: Some("m.read.private")
    pub sent: Option<&'static str>,
}

impl Core {
    /// 這個帳號在這間房的未讀數，一律從本地算（/docs/design/messages/read-receipts.md §4）。
    ///
    /// Args:
    ///     room: example: "!r:localhost"
    /// Return:
    ///     Ok(Unread)   這間房本地沒有是 0、`read_upto` None
    pub async fn unread(&self, room: &str, target: &Target) -> Result<Unread, CoreError> {
        let account = self.account_or_current(target)?;
        let (cache, me) = self.server_cache_and_me(&account)?;
        let unread = cache.read().await.get_unread(&me, room)?;
        Ok(unread)
    }

    /// 標已讀（`room.read`，/docs/design/messages/read-receipts.md §3）。拒絕時什麼都不寫、什麼都不送。
    /// `both` 先送上游、成功了再寫本地。
    ///
    /// Args:
    ///     at: example: &ReadAt::EventId("$abc".into())
    ///     sync: `local` 只寫本地、`server` 只送上游、`both` 兩個都做, example: SyncMode::Both
    ///     visible_to_others: true 送 `m.read`（同房間的人看得到）、false 送 `m.read.private`, example: false
    /// Return:
    ///     Ok(MarkedRead)
    ///     Err(Usage)    給的是 seq、或 `sync` 是 `local`／`both`，而這個帳號的快取裡沒有那一則
    ///     Err(Server)   上游拒了；`data` 帶 `status`／`errcode`
    pub async fn mark_read(
        &self,
        room: &str,
        at: &ReadAt,
        sync: SyncMode,
        visible_to_others: bool,
        target: &Target,
    ) -> Result<MarkedRead, CoreError> {
        let account = self.account_or_current(target)?;
        let (cache, me) = self.server_cache_and_me(&account)?;
        let writes_local = sync != SyncMode::Server;
        // Matrix 的回條吃 `event_id`：給 seq 的從本地換；要寫本地的也先確認有這一則，免得送了上游才發現寫不進來。
        let event_id = match (at, writes_local) {
            (ReadAt::EventId(event_id), false) => event_id.clone(),
            _ => match cache.read().await.find_readable_event(&me, room, at)? {
                Some(found) => found.event_id,
                None => {
                    let asked = match at {
                        ReadAt::EventId(event_id) => event_id.clone(),
                        ReadAt::GSeq(g_seq) => format!("g_seq {g_seq}"),
                        ReadAt::RSeq(r_seq) => format!("r_seq {r_seq}"),
                    };
                    return Err(CoreError::new(
                        CoreErrorKind::Usage,
                        format!("{asked} in {room} is not in the local cache for {me}: sync it first (room.history with sync=both)"),
                    ));
                }
            },
        };
        let sent = match sync {
            SyncMode::Local => None,
            SyncMode::Server | SyncMode::Both => {
                let receipt_type = if visible_to_others {
                    "m.read"
                } else {
                    "m.read.private"
                };
                let variables = Map::from_iter([
                    ("room_id".to_string(), json!(room)),
                    ("receipt_type".to_string(), json!(receipt_type)),
                    ("event_id".to_string(), json!(event_id)),
                ]);
                self.call_matrix_endpoint(&matrix_endpoint::RECEIPT, variables, None, target)
                    .await?;
                Some(receipt_type)
            }
        };
        if writes_local {
            let (me_here, room_here) = (me.clone(), room.to_string());
            cache
                .run(move |cache| cache.set_read_position(&me_here, &room_here, &event_id))
                .await?;
        }
        let unread = cache.read().await.get_unread(&me, room)?;
        Ok(MarkedRead { unread, sent })
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{json, Value};
    use wbf_sdk::cache::{ReadAt, ReadPosition};
    use wbf_sdk::IncomingEvent;
    use wbf_wire::Kind;

    use crate::error::CoreErrorKind;
    use crate::rooms_ops::SyncMode;
    use crate::test_support::{
        core_with_wbf_account, misc_and_upload, remember_room, scratch, FakeServer, ROOM,
    };
    use crate::{Core, Target};

    const BOB: &str = "@bob:localhost";

    fn text(id: &str, sender: &str, r_seq: i64) -> IncomingEvent {
        IncomingEvent::Plain {
            event: json!({
                "type": "m.room.message", "event_id": id, "room_id": ROOM, "sender": sender,
                "origin_server_ts": r_seq, "content": { "msgtype": "m.text", "body": "hi" },
                "unsigned": { wbf_sdk::protocol::R_SEQ_KEY: r_seq, wbf_sdk::protocol::G_SEQ_KEY: r_seq * 10 },
            }),
        }
    }

    /// 本地有 Bob 的 $1 $2 $3；`Misc` 線接上假 server。
    async fn three_unread(name: &str) -> (Core, FakeServer) {
        let (core, account) = core_with_wbf_account(&scratch(name)).await;
        let (misc, _upload) = misc_and_upload(&core, &account, false).await;
        remember_room(&core, &account, ROOM, false).await;
        let (cache, me) = core.server_cache_and_me(&account).unwrap();
        let events = vec![text("$1", BOB, 1), text("$2", BOB, 2), text("$3", BOB, 3)];
        cache
            .run(move |cache| cache.upsert_events(&me, ROOM, &events).map(|_| ()))
            .await
            .unwrap();
        (core, misc)
    }

    fn receipts(misc: &FakeServer) -> Vec<(Kind, u8, Value, Value)> {
        misc.rooms.lock().unwrap().requests.clone()
    }

    /// §3：`local` 只寫本地、不送；給 seq 從本地換成 event_id；回寫完之後的未讀數。
    #[tokio::test]
    async fn local_marks_the_watermark_without_sending() {
        let (core, misc) = three_unread("read-local").await;
        let target = Target::default();
        assert_eq!(core.unread(ROOM, &target).await.unwrap().count, 3);
        let marked = core
            .mark_read(ROOM, &ReadAt::RSeq(2), SyncMode::Local, false, &target)
            .await
            .unwrap();
        assert_eq!(marked.sent, None);
        assert_eq!(marked.unread.count, 1);
        assert_eq!(
            marked.unread.read_upto,
            Some(ReadPosition {
                event_id: "$2".into(),
                r_seq: Some(2)
            })
        );
        assert!(receipts(&misc).is_empty(), "local sends nothing");
    }

    /// §3、§5：`both` 走橋 `0x15 0x22`，私人的送 `m.read.private`、公開的送 `m.read`；body 是 `{}`。
    #[tokio::test]
    async fn both_sends_the_receipt_over_the_bridge_then_writes_local() {
        let (core, misc) = three_unread("read-both").await;
        let target = Target::default();
        core.mark_read(ROOM, &ReadAt::GSeq(10), SyncMode::Both, false, &target)
            .await
            .unwrap();
        let marked = core
            .mark_read(
                ROOM,
                &ReadAt::EventId("$3".into()),
                SyncMode::Both,
                true,
                &target,
            )
            .await
            .unwrap();
        assert_eq!(marked.sent, Some("m.read"));
        assert_eq!(marked.unread.count, 0);
        assert_eq!(
            receipts(&misc),
            vec![
                (
                    Kind::Receipt,
                    0x22,
                    json!({ "room_id": ROOM, "receipt_type": "m.read.private", "event_id": "$1" }),
                    json!({})
                ),
                (
                    Kind::Receipt,
                    0x22,
                    json!({ "room_id": ROOM, "receipt_type": "m.read", "event_id": "$3" }),
                    json!({})
                ),
            ]
        );
    }

    /// §3 的拒絕：本地沒有那一則 → 1100、什麼都不送；上游拒了 → 本地不寫。`server` 給 event_id 不必本地有。
    #[tokio::test]
    async fn a_refusal_writes_nothing_and_sends_nothing() {
        let (core, misc) = three_unread("read-refused").await;
        let target = Target::default();
        for (at, sync) in [
            (ReadAt::EventId("$missing".into()), SyncMode::Both),
            (ReadAt::EventId("$missing".into()), SyncMode::Local),
            (ReadAt::RSeq(9), SyncMode::Server),
        ] {
            let error = core
                .mark_read(ROOM, &at, sync, false, &target)
                .await
                .expect_err("not in the cache");
            assert_eq!(
                error.kind,
                CoreErrorKind::Usage,
                "{at:?} {sync:?}: {error:?}"
            );
        }
        assert!(receipts(&misc).is_empty(), "refused before sending");

        misc.rooms.lock().unwrap().forbid_next = true;
        let error = core
            .mark_read(
                ROOM,
                &ReadAt::EventId("$2".into()),
                SyncMode::Both,
                false,
                &target,
            )
            .await
            .expect_err("forbidden");
        assert_eq!(error.kind, CoreErrorKind::Server, "{error:?}");
        assert_eq!(core.unread(ROOM, &target).await.unwrap().read_upto, None);

        let marked = core
            .mark_read(
                ROOM,
                &ReadAt::EventId("$elsewhere".into()),
                SyncMode::Server,
                false,
                &target,
            )
            .await
            .unwrap();
        assert_eq!(marked.sent, Some("m.read.private"));
        assert_eq!(marked.unread.count, 3, "server leaves local alone");
    }
}

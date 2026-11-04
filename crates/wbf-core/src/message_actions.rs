//! 訊息動作的送出那半：回覆、編輯、表情、收回（/docs/design/messages/message-actions.md）。
//! 收的那半在 cache（/docs/design/messages/edits-and-redactions.md §4–§6）。送完**不**寫本地：結果跟別人送的一樣從 Rooms 線推回來，照收的規則折進目標。
//! 本地只擋「送出去一定錯」的請求（`room.edit` 的目標，§4.2）；**不**替 UI 做選擇（表情的切換是 UI 的事，§1 第 5 條）。

use serde_json::{json, Map, Value};

use wbf_sdk::cache::StoredTarget;
use wbf_sdk::incoming::EventClass;
use wbf_sdk::matrix_endpoint;
use wbf_sdk::message_content::{
    add_reply_and_mentions, list_mentioned_users, to_edit_content, to_reaction_content,
    RepliedMessage, MESSAGE_EVENT_TYPE, REACTION_EVENT_TYPE,
};

use crate::accounts::AccountDir;
use crate::error::{CoreError, CoreErrorKind};
use crate::room_crypto::SendOptions;
use crate::{Core, Target};

impl Core {
    /// 改自己的一則文字（`room.edit`，/docs/design/messages/message-actions.md §3.3、§4.2）。
    ///
    /// Args:
    ///     event_id: 被改的那則, example: "$abc"
    ///     body: 新的字, example: "改過的字"
    ///     options: `mentions` 是新版本提到誰（None ＝ 改字不通知任何人）；加密房要 `room_devices`；`reply_to` 要是 None, example: &SendOptions::default()
    /// Return:
    ///     Ok(String)                 edit 那則自己的 event_id
    ///     Err(Usage)                 目標不合 §4.2 那張表（`message` 寫哪一條）、帶了 `reply_to`、本地不知道這間房加不加密、加密房沒帶 `room_devices`
    ///     Err(RoomDevicesChanged)／Err(RoomKeyNotReady)   同 [`Core::send_text`]
    pub async fn edit_text(
        &self,
        room: &str,
        event_id: &str,
        body: &str,
        options: &SendOptions,
        target: &Target,
    ) -> Result<String, CoreError> {
        if options.reply_to.is_some() {
            return Err(usage(
                "room.edit takes no reply_to: a reply stays on the original message",
            ));
        }
        let account = self.account_or_current(target)?;
        let (cache, me) = self.server_cache_and_me(&account)?;
        let stored = cache.read().await.find_stored_target(&me, room, event_id)?;
        let previous_mentions = list_mentions_of_editable_target(event_id, &me, stored.as_ref())?;
        // 🚨 規格：加密的訊息只能用加密的 edit 改。wbf 帳號照本地的加密標記送，所以先對一次；一般 Matrix 帳號由 matrix-sdk 照房間加密。
        let target_was_encrypted = stored.as_ref().is_some_and(|stored| stored.is_encrypted);
        if target_was_encrypted
            && self.is_wbf_account(&account)?
            && !self.find_local_room_encryption(&account, room).await?
        {
            return Err(usage(format!(
                "{event_id} is encrypted but {room} is marked unencrypted here: an encrypted message can only be edited by an encrypted edit; fetch the room (room.get with sync=both) first"
            )));
        }
        let content = to_edit_content(
            event_id,
            body,
            options.mentions.as_deref(),
            &previous_mentions,
        );
        self.send_room_event(&account, room, MESSAGE_EVENT_TYPE, content, options, target)
            .await
    }

    /// 對一則訊息按表情（`room.react`，/docs/design/messages/message-actions.md §3.4、§4.3）：只會「加」。
    /// 冪等：本地看得到這個帳號已經按過同一個 `key` 就回那則、**不**再送；只查本地、**不**問 server。
    ///
    /// Args:
    ///     event_id: 被按的那則, example: "$abc"
    ///     key: 原樣送, example: "👍"
    ///     options: 加密房要 `room_devices`；`reply_to`、`mentions` 要是 None, example: &SendOptions::default()
    /// Return:
    ///     Ok(String)                 那則反應的 event_id（冪等時是本地已有的那則）
    ///     Err(Usage)                 `key` 是空字串、帶了 `reply_to`／`mentions`、本地不知道這間房加不加密、加密房沒帶 `room_devices`
    ///     Err(RoomDevicesChanged)／Err(RoomKeyNotReady)   同 [`Core::send_text`]
    pub async fn react(
        &self,
        room: &str,
        event_id: &str,
        key: &str,
        options: &SendOptions,
        target: &Target,
    ) -> Result<String, CoreError> {
        if options.reply_to.is_some() || options.mentions.is_some() {
            return Err(usage("room.react takes no reply_to and no mentions"));
        }
        if key.is_empty() {
            return Err(usage("room.react: key is empty"));
        }
        let account = self.account_or_current(target)?;
        let (cache, me) = self.server_cache_and_me(&account)?;
        let existing = cache
            .read()
            .await
            .find_own_reaction(&me, room, event_id, key)?;
        if let Some(existing) = existing {
            return Ok(existing);
        }
        let content = to_reaction_content(event_id, key);
        self.send_room_event(
            &account,
            room,
            REACTION_EVENT_TYPE,
            content,
            options,
            target,
        )
        .await
    }

    /// 收回一則（`room.redact`，/docs/design/messages/message-actions.md §3.5、§4.4）：訊息、自己的表情都是它。
    /// 一律明文（Matrix 的 redaction 不加密），走端點表：wbf 帳號走橋 `0x14 0x24`、一般 Matrix 帳號走 HTTP。
    /// **不**檢查目標是誰的、在不在本地：收回別人的要權限，由 server 照 power level 擋。
    ///
    /// Args:
    ///     event_id: 被收回的那則, example: "$react1"
    ///     reason: example: Some("打錯字")
    ///     txn_id: 重送用同一個（server 只收回一次）；None ＝ daemon 產一個, example: None
    /// Return:
    ///     Ok(String)     收回動作本身那則的 event_id
    ///     Err(Server)    server 拒（權限不夠是 403）；`data` 帶 `status`／`errcode`；或回應沒有 `event_id`
    ///     Err(Usage)／Err(Network)
    pub async fn redact(
        &self,
        room: &str,
        event_id: &str,
        reason: Option<&str>,
        txn_id: Option<&str>,
        target: &Target,
    ) -> Result<String, CoreError> {
        let txn_id = match txn_id {
            Some(txn_id) => txn_id.to_string(),
            None => wbf_sdk::protocol::new_txn_id()?,
        };
        let variables = Map::from_iter([
            ("room_id".to_string(), json!(room)),
            ("event_id".to_string(), json!(event_id)),
            ("txn_id".to_string(), json!(txn_id)),
        ]);
        let body = match reason {
            Some(reason) => json!({ "reason": reason }),
            None => json!({}),
        };
        let reply = self
            .call_matrix_endpoint(&matrix_endpoint::REDACT, variables, Some(body), target)
            .await?;
        match reply.get("event_id").and_then(Value::as_str) {
            Some(redaction_event_id) => Ok(redaction_event_id.to_string()),
            None => Err(CoreError::new(
                CoreErrorKind::Server,
                format!("Redact of {event_id} did not answer an event_id: {reply}"),
            )),
        }
    }

    /// 在 content 上加 `options` 的回覆與提到（送訊息的三支共用，/docs/design/messages/message-actions.md §3.1、§3.2）。
    /// 被回覆的人從本地查：查不到就不加，也**不**報錯。
    ///
    /// Args:
    ///     content: 原本的 content, example: json!({"msgtype":"m.text","body":"好"})
    ///     options: 看 `reply_to`、`mentions`, example: &SendOptions::default()
    /// Return:
    ///     Ok(Value)   沒有回覆也沒有提到：原樣
    ///     Err(...)    讀本地失敗
    pub(crate) async fn add_reply_and_mentions_of(
        &self,
        account: &AccountDir,
        room: &str,
        content: Value,
        options: &SendOptions,
    ) -> Result<Value, CoreError> {
        if options.reply_to.is_none() && options.mentions.is_none() {
            return Ok(content);
        }
        let (cache, me) = self.server_cache_and_me(account)?;
        let reply = match &options.reply_to {
            None => None,
            Some(event_id) => {
                let stored = cache.read().await.find_stored_target(&me, room, event_id)?;
                Some(RepliedMessage {
                    event_id: event_id.clone(),
                    sender: stored.map(|stored| stored.sender),
                })
            }
        };
        Ok(add_reply_and_mentions(
            content,
            reply.as_ref(),
            options.mentions.as_deref(),
            &me,
        ))
    }

    /// 送一則沒有附件的事件（文字、編輯、表情）：wbf 帳號照本地的加密標記走 `Event/Send`；一般 Matrix 帳號走 matrix-sdk（加密房它自己 Megolm）。
    ///
    /// Args:
    ///     event_type: example: "m.room.message"
    ///     content: 組好的 content, example: json!({"msgtype":"m.text","body":"hi"})
    /// Return:
    ///     Ok(String)   event_id
    ///     Err(...)     同 [`Core::send_text`]
    pub(crate) async fn send_room_event(
        &self,
        account: &AccountDir,
        room: &str,
        event_type: &str,
        content: Value,
        options: &SendOptions,
        target: &Target,
    ) -> Result<String, CoreError> {
        if self.is_wbf_account(account)? {
            return self
                .wbf_send_by_local_encryption(account, room, event_type, content, options)
                .await;
        }
        let backend = self
            .synced_backend_of(account, target.server_backup)
            .await?;
        Ok(backend
            .send_event_content(room, event_type, content, options.txn_id.as_deref())
            .await?)
    }
}

/// `room.edit` 的目標要**同時**滿足 /docs/design/messages/message-actions.md §4.2 那張表的每一條。
///
/// Args:
///     event_id: 只拿來寫錯誤訊息, example: "$abc"
///     me: 送的這個帳號, example: "@alice:localhost"
///     stored: 本地查到的（[`wbf_sdk::cache::Cache::find_stored_target`]）
/// Return:
///     Ok(Vec<String>)   目標目前那一版提到誰（給外層 `m.mentions` 算「新加的」）
///     Err(Usage)        不合哪一條，`message` 寫出來
fn list_mentions_of_editable_target(
    event_id: &str,
    me: &str,
    stored: Option<&StoredTarget>,
) -> Result<Vec<String>, CoreError> {
    let Some(stored) = stored else {
        return Err(usage(format!(
            "{event_id} is not in the local cache for this account: fetch it first (room.history with sync=both)"
        )));
    };
    if stored.sender != me {
        return Err(usage(format!(
            "{event_id} was sent by {}: only your own messages can be edited",
            stored.sender
        )));
    }
    match stored.class {
        Some(EventClass::Msg) => {}
        Some(EventClass::General) => {
            return Err(usage(format!(
                "{event_id} is not decrypted here, so it cannot be checked or edited"
            )))
        }
        Some(EventClass::Edit | EventClass::Reaction | EventClass::Redact) => {
            return Err(usage(format!(
                "{event_id} is not an original message (an edit, a reaction or a redaction): edit the original"
            )))
        }
        None => {
            return Err(usage(format!(
                "{event_id} has an unreadable class in the local cache, so it cannot be checked or edited"
            )))
        }
    }
    if stored.is_redacted {
        return Err(usage(format!(
            "{event_id} was deleted: a deleted message cannot be edited"
        )));
    }
    if stored.event_type.as_deref() != Some(MESSAGE_EVENT_TYPE) {
        return Err(usage(format!(
            "{event_id} is not a message: only text can be edited"
        )));
    }
    let Some(current_content) = &stored.current_content else {
        return Err(usage(format!(
            "the current version of {event_id} is not readable here (its latest edit is not synced, or was deleted): sync and try again"
        )));
    };
    if current_content.get("msgtype").and_then(Value::as_str) != Some("m.text") {
        return Err(usage(format!(
            "{event_id} is not text (msgtype m.text): only text can be edited"
        )));
    }
    Ok(list_mentioned_users(current_content))
}

fn usage(message: impl Into<String>) -> CoreError {
    CoreError::new(CoreErrorKind::Usage, message)
}

#[cfg(test)]
mod tests {
    use serde_json::{json, Value};
    use wbf_sdk::IncomingEvent;
    use wbf_wire::Kind;

    use crate::error::CoreErrorKind;
    use crate::rooms_ops::{HistoryQuery, SyncMode};
    use crate::test_support::{
        core_with_wbf_account, misc_and_upload, remember_room, scratch, FakeServer, ME, ROOM,
    };
    use crate::{Core, SendOptions, Target};

    const ALICE: &str = "@alice:localhost";
    const BOB: &str = "@bob:localhost";

    fn event(id: &str, sender: &str, event_type: &str, content: Value, r_seq: i64) -> Value {
        json!({
            "type": event_type, "event_id": id, "room_id": ROOM, "sender": sender,
            "origin_server_ts": r_seq, "content": content,
            "unsigned": { wbf_sdk::protocol::R_SEQ_KEY: r_seq, wbf_sdk::protocol::G_SEQ_KEY: r_seq * 10 },
        })
    }

    fn text(id: &str, sender: &str, body: &str, r_seq: i64) -> IncomingEvent {
        IncomingEvent::Plain {
            event: event(
                id,
                sender,
                "m.room.message",
                json!({ "msgtype": "m.text", "body": body }),
                r_seq,
            ),
        }
    }

    fn reaction(id: &str, sender: &str, target: &str, key: &str, r_seq: i64) -> IncomingEvent {
        IncomingEvent::Plain {
            event: event(
                id,
                sender,
                "m.reaction",
                json!({ "m.relates_to": { "rel_type": "m.annotation", "event_id": target, "key": key } }),
                r_seq,
            ),
        }
    }

    async fn seed(core: &Core, account: &crate::accounts::AccountDir, events: Vec<IncomingEvent>) {
        let (cache, me) = core.server_cache_and_me(account).unwrap();
        cache
            .run(move |cache| cache.upsert_events(&me, ROOM, &events).map(|_| ()))
            .await
            .unwrap();
    }

    /// 開好帳號、本地記住一間明文房、`Misc` 線接上假 server。
    async fn plaintext_room(name: &str) -> (Core, crate::accounts::AccountDir, FakeServer) {
        let (core, account) = core_with_wbf_account(&scratch(name)).await;
        let (misc, _upload) = misc_and_upload(&core, &account, false).await;
        remember_room(&core, &account, ROOM, false).await;
        (core, account, misc)
    }

    /// `Event/Send` 收下的 (type, content)。
    fn sent(misc: &FakeServer) -> Vec<(String, Value)> {
        misc.sent_events
            .lock()
            .unwrap()
            .iter()
            .map(|(_, event_type, _, _, content)| {
                (event_type.clone(), serde_json::from_slice(content).unwrap())
            })
            .collect()
    }

    /// /docs/design/messages/message-actions.md §3.1：回覆帶 `m.in_reply_to`，被回覆的人從本地查、放進 `m.mentions`。
    #[tokio::test]
    async fn a_reply_carries_in_reply_to_and_mentions_the_locally_known_sender() {
        let (core, account, misc) = plaintext_room("reply").await;
        seed(&core, &account, vec![text("$abc", ALICE, "要吃什麼", 1)]).await;
        let options = SendOptions {
            reply_to: Some("$abc".into()),
            ..SendOptions::default()
        };
        core.send_text(ROOM, "好", &options, &Target::default())
            .await
            .unwrap();
        assert_eq!(
            sent(&misc),
            vec![(
                "m.room.message".to_string(),
                json!({ "msgtype": "m.text", "body": "好",
                        "m.relates_to": { "m.in_reply_to": { "event_id": "$abc" } },
                        "m.mentions": { "user_ids": [ALICE] } })
            )]
        );
    }

    /// §3.3：外層只通知這次新加的人；新版本放完整名單。
    #[tokio::test]
    async fn an_edit_sends_the_replacement_and_mentions_only_the_newly_mentioned() {
        let (core, account, misc) = plaintext_room("edit").await;
        let mut mine = event(
            "$mine",
            ME,
            "m.room.message",
            json!({ "msgtype": "m.text", "body": "hi bob", "m.mentions": { "user_ids": [BOB] } }),
            1,
        );
        mine["content"]["m.mentions"] = json!({ "user_ids": [BOB] });
        seed(&core, &account, vec![IncomingEvent::Plain { event: mine }]).await;
        let options = SendOptions {
            mentions: Some(vec![BOB.into(), ALICE.into()]),
            ..SendOptions::default()
        };
        core.edit_text(
            ROOM,
            "$mine",
            "hi bob and alice",
            &options,
            &Target::default(),
        )
        .await
        .unwrap();
        assert_eq!(
            sent(&misc),
            vec![(
                "m.room.message".to_string(),
                json!({ "msgtype": "m.text", "body": "* hi bob and alice",
                        "m.new_content": { "msgtype": "m.text", "body": "hi bob and alice",
                                           "m.mentions": { "user_ids": [BOB, ALICE] } },
                        "m.mentions": { "user_ids": [ALICE] },
                        "m.relates_to": { "rel_type": "m.replace", "event_id": "$mine" } })
            )]
        );
    }

    /// §4.2 那張表，一條一個目標：每個都回 1100、什麼都不送（這些 edit server 照收，但每個 client 都會丟掉它）。
    #[tokio::test]
    async fn an_edit_whose_target_every_client_would_drop_is_refused_before_sending() {
        let (core, account, misc) = plaintext_room("edit-refused").await;
        seed(
            &core,
            &account,
            vec![
                text("$mine", ME, "hi", 1),
                text("$theirs", BOB, "hi", 2),
                IncomingEvent::Plain {
                    event: event(
                        "$edit",
                        ME,
                        "m.room.message",
                        json!({ "msgtype": "m.text", "body": "* hey",
                                "m.new_content": { "msgtype": "m.text", "body": "hey" },
                                "m.relates_to": { "rel_type": "m.replace", "event_id": "$mine" } }),
                        3,
                    ),
                },
                text("$gone", ME, "oops", 4),
                IncomingEvent::Plain {
                    event: event(
                        "$redaction",
                        ME,
                        "m.room.redaction",
                        json!({ "redacts": "$gone" }),
                        5,
                    ),
                },
                IncomingEvent::Plain {
                    event: {
                        let mut topic =
                            event("$topic", ME, "m.room.topic", json!({ "topic": "t" }), 6);
                        topic["state_key"] = json!("");
                        topic
                    },
                },
                IncomingEvent::Plain {
                    event: event(
                        "$notice",
                        ME,
                        "m.room.message",
                        json!({ "msgtype": "m.notice", "body": "bot says" }),
                        7,
                    ),
                },
                IncomingEvent::Undecrypted {
                    ciphertext: event(
                        "$sealed",
                        ME,
                        "m.room.encrypted",
                        json!({ "algorithm": "m.megolm.v1.aes-sha2", "session_id": "S", "ciphertext": "x" }),
                        8,
                    ),
                    reason: "no key".into(),
                },
            ],
        )
        .await;
        for (target, why) in [
            ("$missing", "not in the local cache"),
            ("$theirs", "only your own messages"),
            ("$edit", "not an original message"),
            ("$gone", "was deleted"),
            ("$topic", "not a message"),
            ("$notice", "not text"),
            ("$sealed", "not decrypted here"),
        ] {
            let error = core
                .edit_text(
                    ROOM,
                    target,
                    "new",
                    &SendOptions::default(),
                    &Target::default(),
                )
                .await
                .expect_err(target);
            assert_eq!(error.kind, CoreErrorKind::Usage, "{target}: {error:?}");
            assert!(error.message.contains(why), "{target}: {}", error.message);
        }
        let with_reply = SendOptions {
            reply_to: Some("$theirs".into()),
            ..SendOptions::default()
        };
        let error = core
            .edit_text(ROOM, "$mine", "new", &with_reply, &Target::default())
            .await
            .expect_err("reply_to");
        assert_eq!(error.kind, CoreErrorKind::Usage, "{error:?}");
        assert!(
            sent(&misc).is_empty(),
            "nothing was sent: {:?}",
            sent(&misc)
        );
    }

    /// §4.3：本地看得到這個帳號已經按過同一個 `key` → 回那則、不送；別的 `key` 照送（§3.4 的形狀）。
    /// 別人按的不算；收回過的不算。
    #[tokio::test]
    async fn a_reaction_already_pressed_here_is_answered_without_sending_again() {
        let (core, account, misc) = plaintext_room("react").await;
        seed(
            &core,
            &account,
            vec![
                text("$abc", BOB, "hi", 1),
                reaction("$bobs", BOB, "$abc", "🎉", 2),
                reaction("$old", ME, "$abc", "❤", 3),
                IncomingEvent::Plain {
                    event: event(
                        "$undo",
                        ME,
                        "m.room.redaction",
                        json!({ "redacts": "$old" }),
                        4,
                    ),
                },
                reaction("$mine", ME, "$abc", "👍", 5),
            ],
        )
        .await;
        let options = SendOptions::default();
        let target = Target::default();
        let answered = core
            .react(ROOM, "$abc", "👍", &options, &target)
            .await
            .unwrap();
        assert_eq!(answered, "$mine");
        assert!(sent(&misc).is_empty(), "pressed already: nothing is sent");

        for key in ["🎉", "❤"] {
            core.react(ROOM, "$abc", key, &options, &target)
                .await
                .unwrap();
        }
        assert_eq!(
            sent(&misc),
            ["🎉", "❤"]
                .iter()
                .map(|key| (
                    "m.reaction".to_string(),
                    json!({ "m.relates_to": { "rel_type": "m.annotation", "event_id": "$abc", "key": key } })
                ))
                .collect::<Vec<_>>(),
            "someone else's 🎉 and my redacted ❤ do not count"
        );

        let with_mentions = SendOptions {
            mentions: Some(vec![BOB.into()]),
            ..SendOptions::default()
        };
        for (key, options) in [("", &options), ("👍", &with_mentions)] {
            let error = core
                .react(ROOM, "$abc", key, options, &target)
                .await
                .expect_err(key);
            assert_eq!(error.kind, CoreErrorKind::Usage, "{key:?}: {error:?}");
        }
    }

    /// §3.5：走橋 `0x14 0x24`，變數與 body 照 Matrix；回的是收回動作本身那則的 id。
    #[tokio::test]
    async fn a_redaction_goes_over_the_bridge_and_answers_its_own_event_id() {
        let (core, _account, misc) = plaintext_room("redact").await;
        let redaction = core
            .redact(
                ROOM,
                "$react1",
                Some("打錯字"),
                Some("t-1"),
                &Target::default(),
            )
            .await
            .unwrap();
        assert_eq!(redaction, "$redaction1");
        let requests = misc.rooms.lock().unwrap().requests.clone();
        assert_eq!(
            requests,
            vec![(
                Kind::Event,
                0x24,
                json!({ "room_id": ROOM, "event_id": "$react1", "txn_id": "t-1" }),
                json!({ "reason": "打錯字" })
            )]
        );
        assert!(sent(&misc).is_empty(), "a redaction is not an Event/Send");
    }

    /// /docs/design/messages/message-actions.md §5：本地讀的 `reactions` 照人去重，`mine` 是讀者自己最早的那則。
    #[tokio::test]
    async fn local_history_counts_a_person_once_and_marks_mine() {
        let (core, account, _misc) = plaintext_room("mine").await;
        seed(
            &core,
            &account,
            vec![
                text("$abc", BOB, "hi", 1),
                reaction("$first", ME, "$abc", "👍", 2),
                reaction("$bobs", BOB, "$abc", "👍", 3),
                reaction("$again", ME, "$abc", "👍", 4),
            ],
        )
        .await;
        let query = HistoryQuery {
            room: ROOM.into(),
            limit: 10,
            before: None,
            sync: SyncMode::Local,
            types: Vec::new(),
            sender: None,
        };
        let page = core.history(&query, &Target::default()).await.unwrap();
        let reactions = &page.events[0].reactions;
        assert_eq!(reactions.len(), 1, "{reactions:?}");
        assert_eq!(reactions[0].by, vec![ME, BOB]);
        assert_eq!(reactions[0].mine.as_deref(), Some("$first"));
        let shown = serde_json::to_value(&page.events[0]).unwrap();
        assert_eq!(
            shown["reactions"],
            json!([{ "key": "👍", "by": [ME, BOB], "mine": "$first" }])
        );
    }
}

//! 送出的那半：回覆、提到、編輯、表情的 content 怎麼組（/docs/design/messages/message-actions.md §3）。
//! 純函式：不碰網路、不碰本地。被回覆的人是誰、目標目前提到誰，由呼叫端從本地查好再給。
//! wbf 帳號（`Event/Send`）與一般 Matrix 帳號（matrix-sdk 的 `send_raw`）送的是這裡組出來的同一份 content。

use serde_json::{json, Value};

/// 一般訊息的 event type。
pub const MESSAGE_EVENT_TYPE: &str = "m.room.message";
/// 表情的 event type（/docs/design/messages/message-actions.md §3.4）。
pub const REACTION_EVENT_TYPE: &str = "m.reaction";

/// 被回覆的那則。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepliedMessage {
    /// example: "$abc"
    pub event_id: String,
    /// 本地查得到才有（/docs/design/messages/message-actions.md §3.2）, example: Some("@alice:localhost".into())
    pub sender: Option<String>,
}

/// 在一則 `m.room.message` 的 content 上加回覆與 `m.mentions`（/docs/design/messages/message-actions.md §3.1、§3.2）。
/// 禁止在 `body` 塞引文：規格已廢 fallback 引文。
///
/// Args:
///     content: 原本的 content, example: json!({"msgtype":"m.text","body":"好"})
///     reply: 有回覆時給, example: Some(&RepliedMessage { event_id: "$abc".into(), sender: Some("@alice:localhost".into()) })
///     mentions: UI 給的 mxid, example: Some(&["@bob:localhost".to_string()])
///     me: 送的這個帳號（被回覆的人是自己就不加）, example: "@carol:localhost"
/// Return:
///     Value  沒有回覆也沒有 `mentions`：原樣（整欄不帶 `m.mentions`）；
///            否則多 `m.mentions: {user_ids}`（`mentions` 的順序、被回覆的人放最後、去重），有回覆再多 `m.relates_to.m.in_reply_to`
pub fn add_reply_and_mentions(
    mut content: Value,
    reply: Option<&RepliedMessage>,
    mentions: Option<&[String]>,
    me: &str,
) -> Value {
    if reply.is_none() && mentions.is_none() {
        return content;
    }
    let mut user_ids = list_unique(mentions.unwrap_or_default());
    if let Some(sender) = reply.and_then(|reply| reply.sender.as_deref()) {
        if sender != me && !user_ids.iter().any(|user_id| user_id == sender) {
            user_ids.push(sender.to_string());
        }
    }
    if let Some(fields) = content.as_object_mut() {
        fields.insert("m.mentions".into(), json!({ "user_ids": user_ids }));
        if let Some(reply) = reply {
            fields.insert(
                "m.relates_to".into(),
                json!({ "m.in_reply_to": { "event_id": reply.event_id } }),
            );
        }
    }
    content
}

/// 改一則文字的 content（/docs/design/messages/message-actions.md §3.3）。
///
/// Args:
///     target_event_id: 被改的那則, example: "$abc"
///     body: 新的字, example: "改過的字"
///     mentions: 新版本提到誰；None ＝ 兩層都 `{}`（改字不通知任何人）, example: Some(&["@bob:localhost".to_string()])
///     previous_mentions: 目標目前那一版的 `m.mentions.user_ids`（[`list_mentioned_users`]）, example: &["@alice:localhost".to_string()]
/// Return:
///     Value  外層 `body` 是 `* <新的字>`；`m.new_content` 是完整的新內容、裡面**不**放 `m.relates_to`；
///            外層 `m.mentions` 只放這次新加的人（`mentions` 減掉 `previous_mentions`）
pub fn to_edit_content(
    target_event_id: &str,
    body: &str,
    mentions: Option<&[String]>,
    previous_mentions: &[String],
) -> Value {
    let (new_mentions, added_mentions) = match mentions {
        None => (json!({}), json!({})),
        Some(mentions) => {
            let user_ids = list_unique(mentions);
            let added: Vec<&String> = user_ids
                .iter()
                .filter(|user_id| !previous_mentions.contains(user_id))
                .collect();
            (
                json!({ "user_ids": user_ids }),
                json!({ "user_ids": added }),
            )
        }
    };
    json!({
        "msgtype": "m.text",
        "body": format!("* {body}"),
        "m.new_content": { "msgtype": "m.text", "body": body, "m.mentions": new_mentions },
        "m.mentions": added_mentions,
        "m.relates_to": { "rel_type": "m.replace", "event_id": target_event_id },
    })
}

/// 表情的 content（/docs/design/messages/message-actions.md §3.4）；event type 是 [`REACTION_EVENT_TYPE`]。
///
/// Args:
///     target_event_id: example: "$abc"
///     key: 原樣送，**不**檢查是不是 emoji, example: "👍"
/// Return:
///     Value  `{"m.relates_to": {"rel_type": "m.annotation", "event_id", "key"}}`
pub fn to_reaction_content(target_event_id: &str, key: &str) -> Value {
    json!({
        "m.relates_to": { "rel_type": "m.annotation", "event_id": target_event_id, "key": key },
    })
}

/// Args:
///     content: 一則訊息目前那一版的 content, example: json!({"body":"hi","m.mentions":{"user_ids":["@bob:localhost"]}})
/// Return:
///     Vec<String>  `m.mentions.user_ids` 裡的字串；沒有這欄、或不是字串的就略過（空的就是沒提到誰）
pub fn list_mentioned_users(content: &Value) -> Vec<String> {
    content
        .get("m.mentions")
        .and_then(|mentions| mentions.get("user_ids"))
        .and_then(|user_ids| user_ids.as_array())
        .map(|user_ids| {
            user_ids
                .iter()
                .filter_map(|user_id| user_id.as_str())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// 照第一次出現的順序去重。
fn list_unique(user_ids: &[String]) -> Vec<String> {
    let mut unique: Vec<String> = Vec::with_capacity(user_ids.len());
    for user_id in user_ids {
        if !unique.contains(user_id) {
            unique.push(user_id.clone());
        }
    }
    unique
}

#[cfg(test)]
mod tests {
    use super::*;

    const ME: &str = "@carol:localhost";

    fn text(body: &str) -> Value {
        json!({ "msgtype": "m.text", "body": body })
    }

    fn reply_to(sender: Option<&str>) -> RepliedMessage {
        RepliedMessage {
            event_id: "$abc".into(),
            sender: sender.map(str::to_string),
        }
    }

    fn owned(user_ids: &[&str]) -> Vec<String> {
        user_ids.iter().map(|user_id| user_id.to_string()).collect()
    }

    /// /docs/design/messages/message-actions.md §3.1 的例子，逐字。
    #[test]
    fn a_reply_carries_in_reply_to_and_mentions_the_replied_sender() {
        let content = add_reply_and_mentions(
            text("好"),
            Some(&reply_to(Some("@alice:localhost"))),
            None,
            ME,
        );
        assert_eq!(
            content,
            json!({ "msgtype": "m.text", "body": "好",
                    "m.relates_to": { "m.in_reply_to": { "event_id": "$abc" } },
                    "m.mentions": { "user_ids": ["@alice:localhost"] } })
        );
    }

    /// §3.2 那張表，一列一條。
    #[test]
    fn no_reply_and_no_mentions_leave_the_content_as_it_was() {
        assert_eq!(
            add_reply_and_mentions(text("hi"), None, None, ME),
            text("hi")
        );
    }

    #[test]
    fn mentions_without_a_reply_are_sent_as_given() {
        let mentions = owned(&["@bob:localhost", "@dave:localhost", "@bob:localhost"]);
        let content = add_reply_and_mentions(text("hi"), None, Some(&mentions), ME);
        assert_eq!(
            content,
            json!({ "msgtype": "m.text", "body": "hi",
                    "m.mentions": { "user_ids": ["@bob:localhost", "@dave:localhost"] } })
        );
    }

    #[test]
    fn a_reply_adds_the_replied_sender_last_and_once() {
        let mentions = owned(&["@bob:localhost", "@alice:localhost"]);
        let content = add_reply_and_mentions(
            text("hi"),
            Some(&reply_to(Some("@alice:localhost"))),
            Some(&mentions),
            ME,
        );
        assert_eq!(
            content["m.mentions"],
            json!({ "user_ids": ["@bob:localhost", "@alice:localhost"] })
        );
        let content = add_reply_and_mentions(
            text("hi"),
            Some(&reply_to(Some("@alice:localhost"))),
            Some(&owned(&["@bob:localhost"])),
            ME,
        );
        assert_eq!(
            content["m.mentions"],
            json!({ "user_ids": ["@bob:localhost", "@alice:localhost"] })
        );
    }

    /// 回覆自己不提到自己；被回覆的人本地查不到也照送，`m.mentions` 照樣帶（對方才不會照 body 找名字）。
    #[test]
    fn a_reply_to_myself_or_to_an_unknown_sender_still_carries_empty_mentions() {
        for reply in [reply_to(Some(ME)), reply_to(None)] {
            let content = add_reply_and_mentions(text("hi"), Some(&reply), None, ME);
            assert_eq!(
                content["m.mentions"],
                json!({ "user_ids": [] }),
                "{reply:?}"
            );
            assert_eq!(content["m.relates_to"]["m.in_reply_to"]["event_id"], "$abc");
        }
    }

    /// §3.3 的例子，逐字：沒帶 `mentions` → 兩層都 `{}`。
    #[test]
    fn an_edit_without_mentions_notifies_nobody() {
        assert_eq!(
            to_edit_content("$abc", "改過的字", None, &owned(&["@bob:localhost"])),
            json!({ "msgtype": "m.text", "body": "* 改過的字",
                    "m.new_content": { "msgtype": "m.text", "body": "改過的字", "m.mentions": {} },
                    "m.mentions": {},
                    "m.relates_to": { "rel_type": "m.replace", "event_id": "$abc" } })
        );
    }

    /// 帶了 `mentions`：新版本放完整名單，外層只放這次新加的。
    #[test]
    fn an_edit_with_mentions_notifies_only_the_newly_mentioned() {
        let content = to_edit_content(
            "$abc",
            "hi",
            Some(&owned(&["@bob:localhost", "@dave:localhost"])),
            &owned(&["@bob:localhost"]),
        );
        assert_eq!(
            content["m.new_content"]["m.mentions"],
            json!({ "user_ids": ["@bob:localhost", "@dave:localhost"] })
        );
        assert_eq!(
            content["m.mentions"],
            json!({ "user_ids": ["@dave:localhost"] })
        );
        assert!(content["m.new_content"].get("m.relates_to").is_none());
    }

    /// §3.4 的例子，逐字。
    #[test]
    fn a_reaction_is_an_annotation_with_the_key_as_given() {
        assert_eq!(
            to_reaction_content("$abc", "👍"),
            json!({ "m.relates_to": { "rel_type": "m.annotation", "event_id": "$abc", "key": "👍" } })
        );
    }

    #[test]
    fn mentioned_users_are_read_from_m_mentions_and_skip_what_is_not_text() {
        let content =
            json!({ "m.mentions": { "user_ids": ["@bob:localhost", 7, "@dave:localhost"] } });
        assert_eq!(
            list_mentioned_users(&content),
            owned(&["@bob:localhost", "@dave:localhost"])
        );
        assert!(list_mentioned_users(&text("hi")).is_empty());
    }
}

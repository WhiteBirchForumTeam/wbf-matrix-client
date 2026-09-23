//! wbf 帳號的房間描述：從橋拿到的狀態事件（`GetState`）與 `m.direct` 組出 `Conversation`（chat-model §2、§3）。
//!
//! 純函數、沒有 matrix-sdk。規則跟 `backend/matrix_sdk.rs` 的 `describe` 是同一套（那邊讀的是 Client 的 `Room`），
//! 兩邊算出來的 `kind`／`can_send_message` 要一樣——這裡的測試就是在釘那件事。

use serde_json::Value;

use crate::chat::{Conversation, ConversationKind};
use crate::error::SdkError;

/// `m.direct` 裡把這個房間列成私訊的那些人（matrix-sdk 的 `direct_targets`）。
///
/// Args:
///     m_direct: `GET /user/{me}/account_data/m.direct` 的內容；None ＝ 沒寫過, example: Some(&json!({"@bob:x": ["!r:x"]}))
///     room_id: example: "!r:x"
/// Return:
///     Vec<String>  排好序；沒人列它就是空的
pub fn direct_peers_of_room(m_direct: Option<&Value>, room_id: &str) -> Vec<String> {
    let Some(map) = m_direct.and_then(Value::as_object) else {
        return Vec::new();
    };
    let mut peers: Vec<String> = map
        .iter()
        .filter(|(_, rooms)| {
            rooms
                .as_array()
                .is_some_and(|rooms| rooms.iter().any(|room| room.as_str() == Some(room_id)))
        })
        .map(|(user, _)| user.clone())
        .collect();
    peers.sort();
    peers
}

/// 一個房間的狀態 → `Conversation`。
///
/// Args:
///     room_id: example: "!r:x"
///     me: 自己的 mxid, example: "@alice:x"
///     state: `GetState` 回的狀態事件（`{type, state_key, content, sender}`）
///     direct_peers: [`direct_peers_of_room`] 的結果
/// Return:
///     Ok(Conversation)
///     Err(Protocol)     沒有 `m.room.create`（不是一個房間的狀態）
pub fn conversation_from_state(
    room_id: &str,
    me: &str,
    state: &[Value],
    direct_peers: &[String],
) -> Result<Conversation, SdkError> {
    let create = find_state(state, "m.room.create", "").ok_or_else(|| {
        SdkError::Protocol(format!("{room_id}: the room state has no m.room.create"))
    })?;
    let power_levels = find_state(state, "m.room.power_levels", "").map(|event| &event["content"]);
    let my_power_level = my_power_level(create, power_levels, me);
    let needed_to_send = power_levels
        .map(|content| {
            read_level(content.pointer("/events/m.room.message"))
                .or_else(|| read_level(content.get("events_default")))
                .unwrap_or(0)
        })
        .unwrap_or(0);
    let can_send_message = my_power_level >= needed_to_send;

    let joined: Vec<&Value> = state
        .iter()
        .filter(|event| {
            event["type"].as_str() == Some("m.room.member")
                && event.pointer("/content/membership").and_then(Value::as_str) == Some("join")
        })
        .collect();
    let member_count = joined.len() as u64;

    // chat-model §3.1：m.direct 有它且成員剛好兩個才是 Direct；§3.2：發訊息的門檻只有 owner（100）達得到才是 Channel。
    let (kind, direct_peer) = if !direct_peers.is_empty() && member_count == 2 {
        (ConversationKind::Direct, direct_peers.first().cloned())
    } else if needed_to_send >= 100 {
        (ConversationKind::Channel, None)
    } else {
        (ConversationKind::Group, None)
    };

    let name = find_state(state, "m.room.name", "")
        .and_then(|event| event.pointer("/content/name"))
        .and_then(non_empty_string)
        .or_else(|| {
            find_state(state, "m.room.canonical_alias", "")
                .and_then(|event| event.pointer("/content/alias"))
                .and_then(non_empty_string)
        })
        .or_else(|| name_from_members(state, me));
    let topic = find_state(state, "m.room.topic", "")
        .and_then(|event| event.pointer("/content/topic"))
        .and_then(non_empty_string);
    let encrypted = find_state(state, "m.room.encryption", "").is_some_and(|event| {
        is_encryption_content(event.get("content").unwrap_or(&serde_json::Value::Null))
    });

    Ok(Conversation {
        id: room_id.to_string(),
        kind,
        name,
        topic,
        encrypted,
        member_count,
        my_power_level,
        can_send_message,
        direct_peer,
    })
}

/// `m.room.encryption` 的 content 算不算「這房加密了」。
/// 🚨 只有帶著非空的 `algorithm` 才算：空 content 的 encryption 事件不算（fail closed 的方向是「當成沒加密」——
/// 送檔前的確認會把它當明文房、拒絕帶金鑰的 cipher，而不是把明文當密文送出去）。
/// `conversation_from_state`（全量狀態）與送訊息前的單項確認（`GetStateEvent`）共用這一句，兩邊不會漂。
///
/// Args:
///     content: example: &json!({"algorithm": "m.megolm.v1.aes-sha2"})
/// Return:
///     bool  true 只在 `algorithm` 是非空字串
pub fn is_encryption_content(content: &Value) -> bool {
    non_empty_string(&content["algorithm"]).is_some()
}

fn find_state<'a>(state: &'a [Value], event_type: &str, state_key: &str) -> Option<&'a Value> {
    // 同型別同 key 只該有一個；有多個就取最後一個（server 給的順序後面的比較新）。
    state.iter().rfind(|event| {
        event["type"].as_str() == Some(event_type) && event["state_key"].as_str() == Some(state_key)
    })
}

/// 自己的 power level（跟 `backend/matrix_sdk.rs` 的 `describe` 同一套）：
/// room v12 起建房者是「無限」（`i64::MAX`）；沒有 `m.room.power_levels` 時建房者 100、其他人 0（Matrix 規格的預設）。
fn my_power_level(create: &Value, power_levels: Option<&Value>, me: &str) -> i64 {
    let is_creator = create["sender"].as_str() == Some(me)
        || create
            .pointer("/content/additional_creators")
            .and_then(Value::as_array)
            .is_some_and(|creators| creators.iter().any(|creator| creator.as_str() == Some(me)));
    let room_version: u64 = create
        .pointer("/content/room_version")
        .and_then(Value::as_str)
        .unwrap_or("1")
        .parse()
        // 認不得的版本字串（實驗版）當舊版：沒有「無限」，照 power_levels 算（往低的那邊倒）。
        .unwrap_or(0);
    if is_creator && room_version >= 12 {
        return i64::MAX;
    }
    match power_levels {
        Some(content) => read_level(content.pointer(&format!("/users/{}", escape_pointer(me))))
            .or_else(|| read_level(content.get("users_default")))
            .unwrap_or(0),
        None if is_creator => 100,
        None => 0,
    }
}

/// power level 的值：規格是整數，但有的 server 存成字串（ruma 也收）。
fn read_level(value: Option<&Value>) -> Option<i64> {
    let value = value?;
    value
        .as_i64()
        .or_else(|| value.as_str().and_then(|text| text.parse().ok()))
}

/// JSON Pointer 的跳脫（RFC 6901）：user_id 的 localpart 可含 `/` 與 `~`。
fn escape_pointer(key: &str) -> String {
    key.replace('~', "~0").replace('/', "~1")
}

fn non_empty_string(value: &Value) -> Option<String> {
    value
        .as_str()
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

/// 沒有名字也沒有別名時，照 Matrix 規格「算出來的名字」的精神用成員湊：除了自己以外加入中／被邀請的成員，
/// 取 displayname（沒有就 mxid），排好序，最多列三個，其餘用「and N others」。一個都沒有就 None。
fn name_from_members(state: &[Value], me: &str) -> Option<String> {
    let mut names: Vec<String> = state
        .iter()
        .filter(|event| event["type"].as_str() == Some("m.room.member"))
        .filter(|event| {
            matches!(
                event.pointer("/content/membership").and_then(Value::as_str),
                Some("join") | Some("invite")
            )
        })
        .filter_map(|event| {
            let user = event["state_key"].as_str()?;
            if user == me {
                return None;
            }
            Some(
                event
                    .pointer("/content/displayname")
                    .and_then(non_empty_string)
                    .unwrap_or_else(|| user.to_string()),
            )
        })
        .collect();
    if names.is_empty() {
        return None;
    }
    names.sort();
    let others = names.len().saturating_sub(3);
    let mut name = names.iter().take(3).cloned().collect::<Vec<_>>().join(", ");
    if others > 0 {
        name.push_str(&format!(" and {others} others"));
    }
    Some(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn member(user: &str, membership: &str, displayname: Option<&str>) -> Value {
        let mut content = json!({ "membership": membership });
        if let Some(displayname) = displayname {
            content["displayname"] = json!(displayname);
        }
        json!({ "type": "m.room.member", "state_key": user, "sender": user, "content": content })
    }

    fn create(sender: &str, room_version: &str) -> Value {
        json!({ "type": "m.room.create", "state_key": "", "sender": sender,
                "content": { "creator": sender, "room_version": room_version } })
    }

    #[test]
    fn a_group_with_a_name_topic_and_encryption() {
        let state = vec![
            create("@alice:x", "10"),
            json!({ "type": "m.room.name", "state_key": "", "content": { "name": "ops" } }),
            json!({ "type": "m.room.topic", "state_key": "", "content": { "topic": "on call" } }),
            json!({ "type": "m.room.encryption", "state_key": "", "content": { "algorithm": "m.megolm.v1.aes-sha2" } }),
            json!({ "type": "m.room.power_levels", "state_key": "", "content": {
                "users": { "@alice:x": 100 }, "users_default": 0, "events": { "m.room.message": 0 } } }),
            member("@alice:x", "join", Some("Alice")),
            member("@bob:x", "join", Some("Bob")),
            member("@carol:x", "join", None),
            member("@dave:x", "leave", None),
        ];
        let conversation = conversation_from_state("!r:x", "@bob:x", &state, &[]).unwrap();
        assert_eq!(
            conversation,
            Conversation {
                id: "!r:x".into(),
                kind: ConversationKind::Group,
                name: Some("ops".into()),
                topic: Some("on call".into()),
                encrypted: true,
                member_count: 3,
                my_power_level: 0,
                can_send_message: true,
                direct_peer: None,
            }
        );
    }

    #[test]
    fn a_direct_room_needs_m_direct_and_exactly_two_members() {
        let state = vec![
            create("@alice:x", "10"),
            member("@alice:x", "join", Some("Alice")),
            member("@bob:x", "join", Some("Bob")),
        ];
        let m_direct = json!({ "@bob:x": ["!other:x", "!r:x"], "@carol:x": ["!other:x"] });
        let peers = direct_peers_of_room(Some(&m_direct), "!r:x");
        assert_eq!(peers, vec!["@bob:x".to_string()]);
        let conversation = conversation_from_state("!r:x", "@alice:x", &state, &peers).unwrap();
        assert_eq!(conversation.kind, ConversationKind::Direct);
        assert_eq!(conversation.direct_peer.as_deref(), Some("@bob:x"));
        // 沒名字、沒別名：用對方的名字。
        assert_eq!(conversation.name.as_deref(), Some("Bob"));
        // 沒有 power_levels 事件：建房者 100，其他人 0；門檻 0，都能發。
        assert_eq!(conversation.my_power_level, 100);
        assert!(conversation.can_send_message);
        assert!(!conversation.encrypted);

        // m.direct 沒列它 → 不是 Direct，就算只有兩個人。
        let group = conversation_from_state("!r:x", "@alice:x", &state, &[]).unwrap();
        assert_eq!(group.kind, ConversationKind::Group);
        assert_eq!(group.direct_peer, None);
        // 沒寫過 m.direct（404）→ 空的。
        assert!(direct_peers_of_room(None, "!r:x").is_empty());
    }

    #[test]
    fn a_channel_is_where_only_owners_can_post() {
        let state = vec![
            create("@alice:x", "10"),
            json!({ "type": "m.room.power_levels", "state_key": "", "content": {
                "users": { "@alice:x": 100 }, "events_default": "100" } }),
            member("@alice:x", "join", None),
            member("@bob:x", "join", None),
            member("@carol:x", "join", None),
        ];
        let owner = conversation_from_state("!r:x", "@alice:x", &state, &[]).unwrap();
        assert_eq!(owner.kind, ConversationKind::Channel);
        assert!(owner.can_send_message);
        let reader = conversation_from_state("!r:x", "@bob:x", &state, &[]).unwrap();
        assert_eq!(reader.kind, ConversationKind::Channel);
        assert!(!reader.can_send_message, "門檻是字串 \"100\" 也要讀得出來");
        assert_eq!(reader.my_power_level, 0);
    }

    #[test]
    fn a_v12_creator_has_infinite_power_and_names_fall_back_to_members() {
        let state = vec![
            create("@alice:x", "12"),
            json!({ "type": "m.room.power_levels", "state_key": "", "content": { "users": {}, "events_default": 50 } }),
            member("@alice:x", "join", None),
            member("@bob:x", "join", Some("Bob")),
            member("@carol:x", "invite", None),
            member("@dave:x", "join", Some("Dave")),
            member("@erin:x", "join", Some("Erin")),
        ];
        let creator = conversation_from_state("!r:x", "@alice:x", &state, &[]).unwrap();
        assert_eq!(creator.my_power_level, i64::MAX);
        assert!(creator.can_send_message);
        assert_eq!(
            creator.name.as_deref(),
            Some("@carol:x, Bob, Dave and 1 others"),
            "排序後前三個，其餘算人數"
        );
        // 實驗版本字串認不得 → 不是無限，照 power_levels。
        let mut experimental = state.clone();
        experimental[0] = create("@alice:x", "org.matrix.experimental");
        let creator = conversation_from_state("!r:x", "@alice:x", &experimental, &[]).unwrap();
        assert_eq!(creator.my_power_level, 0);
        assert!(!creator.can_send_message);
    }

    #[test]
    fn encryption_needs_an_algorithm_and_state_needs_a_create_event() {
        let state = vec![
            create("@alice:x", "10"),
            json!({ "type": "m.room.encryption", "state_key": "", "content": {} }),
            member("@alice:x", "join", None),
        ];
        let conversation = conversation_from_state("!r:x", "@alice:x", &state, &[]).unwrap();
        assert!(
            !conversation.encrypted,
            "沒有 algorithm 的 encryption 事件不算加密"
        );
        assert_eq!(conversation.name, None, "只有自己：沒有名字可湊");
        assert!(matches!(
            conversation_from_state("!r:x", "@alice:x", &[], &[]),
            Err(SdkError::Protocol(_))
        ));
    }
}

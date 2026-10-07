//! 改房間狀態與 `m.direct` 的純函數（/docs/design/rooms/room-actions.md §2.1、§2.5、§4、§5）：daemon 讀出目前那一份、照這裡的規則改、整份寫回。
//!
//! 只擋「格式不是 Matrix 能接受的」（型別、mxid 的形狀）；值合不合理（負數、比自己高）是 server 照 auth rules 擋的，🚫 在這裡判斷（/docs/design/rooms/room-actions.md §1 第 4 條）。
//! 純函數、沒有網路。

use serde_json::{Map, Value};

use crate::error::SdkError;

/// `m.room.encryption` 的演算法（Megolm），建加密房與 `room.enable_encryption` 都寫它。
pub const MEGOLM_ALGORITHM: &str = "m.megolm.v1.aes-sha2";

/// `m.room.power_levels` 裡的數字欄（給了就換）。
const POWER_LEVEL_NUMBERS: [&str; 7] = [
    "users_default",
    "events_default",
    "state_default",
    "invite",
    "kick",
    "ban",
    "redact",
];
/// `m.room.power_levels` 裡的對照表（逐個 key 合併、`null` 拿掉）。
const POWER_LEVEL_MAPS: [&str; 3] = ["users", "events", "notifications"];

/// Return:
///     Value  `m.room.encryption` 的 content, example: {"algorithm": "m.megolm.v1.aes-sha2"}
pub fn to_encryption_content() -> Value {
    serde_json::json!({ "algorithm": MEGOLM_ALGORITHM })
}

/// `room.create` 的 body：UI 給的欄位原樣，`encrypted: true` 時在 `initial_state` 加 `m.room.encryption`（/docs/design/rooms/room-actions.md §2.1）。
///
/// Args:
///     encrypted: 必填的那個, example: true
///     fields: CreateRoom 的其他欄位原樣（名字、格式、預設值都是 Matrix 的）, example: {"name": "週末聚餐", "preset": "private_chat"}
/// Return:
///     Ok(Map)      要送的 body
///     Err(Usage)   `initial_state` 不是陣列；UI 自己在 `initial_state` 放了 `m.room.encryption`、`encrypted` 卻給 false（兩邊說的相反）；body 裡有 `encrypted`
pub fn to_create_room_body(
    encrypted: bool,
    mut fields: Map<String, Value>,
) -> Result<Map<String, Value>, SdkError> {
    if fields.contains_key("encrypted") {
        return Err(SdkError::Usage(
            "room.create: `encrypted` is ours, not a CreateRoom field".into(),
        ));
    }
    let initial_state = match fields.remove("initial_state") {
        None => Vec::new(),
        Some(Value::Array(events)) => events,
        Some(other) => {
            return Err(SdkError::Usage(format!(
                "room.create: initial_state must be a list of state events, not {other}"
            )))
        }
    };
    let already_encrypted = initial_state
        .iter()
        .any(|event| event["type"].as_str() == Some("m.room.encryption"));
    let mut initial_state = initial_state;
    match (encrypted, already_encrypted) {
        (false, true) => {
            return Err(SdkError::Usage(
                "room.create: initial_state has m.room.encryption but encrypted is false; say the same thing in both places"
                    .into(),
            ))
        }
        (true, false) => initial_state.push(serde_json::json!({
            "type": "m.room.encryption",
            "state_key": "",
            "content": to_encryption_content(),
        })),
        (true, true) | (false, false) => {}
    }
    if !initial_state.is_empty() {
        fields.insert("initial_state".into(), Value::Array(initial_state));
    }
    Ok(fields)
}

/// `room.set_power_levels` 的合併（/docs/design/rooms/room-actions.md §4.2）：數字欄給了就換；`users`／`events`／`notifications` 逐個 key 合併，key 給 `null` 拿掉。
///
/// Args:
///     current: 目前的 `m.room.power_levels` content；None ＝ 這間房沒有那一項, example: Some(&json!({"users": {"@alice:x": 100}}))
///     changes: 要改的欄位（跟 content 同名同格式）, example: {"users": {"@bob:x": 50}, "events_default": 100}
/// Return:
///     Ok(Value)    整份新的 content
///     Err(Usage)   都沒給；有表上沒有的欄位；數字欄不是整數；對照表不是物件、值不是整數或 null；`users` 的 key 不是 mxid；
///                  房間沒有 `m.room.power_levels`（🚫 從空的開始寫：省掉的欄位會落到「事件在」的預設，建房者會丟掉 100，改用 `room.set_state` 寫整份）
pub fn merge_power_levels(
    current: Option<&Value>,
    changes: &Map<String, Value>,
) -> Result<Value, SdkError> {
    let bad = |why: String| SdkError::Usage(format!("room.set_power_levels: {why}"));
    if changes.is_empty() {
        return Err(bad("nothing to change".into()));
    }
    let Some(current) = current else {
        return Err(bad(
            "this room has no m.room.power_levels to merge into; write the whole content with room.set_state".into(),
        ));
    };
    let mut merged = match current {
        Value::Object(content) => content.clone(),
        other => {
            return Err(bad(format!(
                "the current power levels are not an object: {other}"
            )))
        }
    };
    for (field, change) in changes {
        if POWER_LEVEL_NUMBERS.contains(&field.as_str()) {
            if !change.is_i64() {
                return Err(bad(format!("`{field}` must be an integer, not {change}")));
            }
            merged.insert(field.clone(), change.clone());
            continue;
        }
        if !POWER_LEVEL_MAPS.contains(&field.as_str()) {
            return Err(bad(format!(
                "`{field}` is not a field of m.room.power_levels"
            )));
        }
        let Value::Object(entries) = change else {
            return Err(bad(format!("`{field}` must be an object, not {change}")));
        };
        let mut table = match merged.remove(field) {
            Some(Value::Object(table)) => table,
            _ => Map::new(),
        };
        for (key, level) in entries {
            if field == "users" && !is_mxid(key) {
                return Err(bad(format!("`users` key {key:?} is not a user id")));
            }
            match level {
                Value::Null => {
                    table.remove(key);
                }
                level if level.is_i64() => {
                    table.insert(key.clone(), level.clone());
                }
                other => {
                    return Err(bad(format!(
                        "`{field}.{key}` must be an integer or null, not {other}"
                    )))
                }
            }
        }
        merged.insert(field.clone(), Value::Object(table));
    }
    Ok(Value::Object(merged))
}

/// `@localpart:server` 的形狀（只看形狀，🚫 驗這個人存不存在）。
fn is_mxid(text: &str) -> bool {
    text.strip_prefix('@')
        .and_then(|rest| rest.split_once(':'))
        .is_some_and(|(localpart, server)| !localpart.is_empty() && !server.is_empty())
}

/// 置頂或取消置頂一則（/docs/design/rooms/room-actions.md §5）：`pinned` 是照順序的清單，加到最後。
///
/// Args:
///     current: 目前的 `m.room.pinned_events` content；None ＝ 沒有那一項, example: Some(&json!({"pinned": ["$a"]}))
///     event_id: example: "$b"
///     pinned: true 置頂、false 取消
/// Return:
///     Ok(Some(Value))  新的 content（content 裡的其他欄位照舊）
///     Ok(None)         已經是那個狀態：🚫 寫
///     Err(Usage)       目前的 content 不是物件、`pinned` 不是字串的清單
pub fn to_pinned_content(
    current: Option<&Value>,
    event_id: &str,
    pinned: bool,
) -> Result<Option<Value>, SdkError> {
    let mut content = match current {
        None => Map::new(),
        Some(Value::Object(content)) => content.clone(),
        Some(other) => {
            return Err(SdkError::Usage(format!(
                "room.pin: the current m.room.pinned_events is not an object: {other}"
            )))
        }
    };
    let mut list: Vec<String> = match content.get("pinned") {
        None => Vec::new(),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| item.as_str().map(str::to_string))
            .collect::<Option<Vec<String>>>()
            .ok_or_else(|| {
                SdkError::Usage("room.pin: the current pinned list has a non-string".into())
            })?,
        Some(other) => {
            return Err(SdkError::Usage(format!(
                "room.pin: the current pinned list is not a list: {other}"
            )))
        }
    };
    let is_pinned = list.iter().any(|pinned_id| pinned_id == event_id);
    match (pinned, is_pinned) {
        (true, true) | (false, false) => return Ok(None),
        (true, false) => list.push(event_id.to_string()),
        (false, true) => list.retain(|pinned_id| pinned_id != event_id),
    }
    content.insert(
        "pinned".into(),
        Value::Array(list.into_iter().map(Value::String).collect()),
    );
    Ok(Some(Value::Object(content)))
}

/// 在自己的 `m.direct`（`{ "@對方": ["!房", …] }`，整份覆蓋寫）加上或拿掉「`user` → `room`」（/docs/design/rooms/room-actions.md §2.5）。
/// 對方的清單空了就把對方拿掉。
///
/// Args:
///     current: 目前的 `m.direct`；None ＝ 沒寫過, example: Some(&json!({"@bob:x": ["!a:x"]}))
///     user: 對方, example: "@bob:x"
///     room: example: "!r:x"
///     direct: true 加、false 拿掉
/// Return:
///     Ok(Some(Value))  新的整份
///     Ok(None)         已經是那個狀態：🚫 寫
///     Err(Usage)       目前的 `m.direct` 不是物件、某個人的清單不是字串的清單；`user` 不是 mxid
pub fn to_direct_content(
    current: Option<&Value>,
    user: &str,
    room: &str,
    direct: bool,
) -> Result<Option<Value>, SdkError> {
    if !is_mxid(user) {
        return Err(SdkError::Usage(format!(
            "m.direct: {user:?} is not a user id"
        )));
    }
    let mut content = match current {
        None => Map::new(),
        Some(Value::Object(content)) => content.clone(),
        Some(other) => {
            return Err(SdkError::Usage(format!(
                "m.direct: the current content is not an object: {other}"
            )))
        }
    };
    let mut rooms: Vec<String> = match content.get(user) {
        None => Vec::new(),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| item.as_str().map(str::to_string))
            .collect::<Option<Vec<String>>>()
            .ok_or_else(|| SdkError::Usage(format!("m.direct: {user}'s list has a non-string")))?,
        Some(other) => {
            return Err(SdkError::Usage(format!(
                "m.direct: {user}'s list is not a list: {other}"
            )))
        }
    };
    let is_listed = rooms.iter().any(|listed| listed == room);
    match (direct, is_listed) {
        (true, true) | (false, false) => return Ok(None),
        (true, false) => rooms.push(room.to_string()),
        (false, true) => rooms.retain(|listed| listed != room),
    }
    if rooms.is_empty() {
        content.remove(user);
    } else {
        content.insert(
            user.to_string(),
            Value::Array(rooms.into_iter().map(Value::String).collect()),
        );
    }
    Ok(Some(Value::Object(content)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn map(value: Value) -> Map<String, Value> {
        value.as_object().unwrap().clone()
    }

    #[test]
    fn create_room_adds_encryption_to_initial_state_and_refuses_a_contradiction() {
        let body = to_create_room_body(
            true,
            map(json!({ "name": "ops", "preset": "private_chat" })),
        )
        .unwrap();
        assert_eq!(body["name"], "ops");
        assert_eq!(
            body["initial_state"],
            json!([{ "type": "m.room.encryption", "state_key": "", "content": { "algorithm": MEGOLM_ALGORITHM } }])
        );
        // 明文房：🚫 加 initial_state
        let body = to_create_room_body(false, map(json!({ "name": "ops" }))).unwrap();
        assert!(!body.contains_key("initial_state"));
        // UI 自己放了、也說要加密：🚫 放兩份
        let own = json!([{ "type": "m.room.encryption", "state_key": "", "content": { "algorithm": MEGOLM_ALGORITHM } },
                         { "type": "m.room.history_visibility", "state_key": "", "content": { "history_visibility": "joined" } }]);
        let body = to_create_room_body(true, map(json!({ "initial_state": own.clone() }))).unwrap();
        assert_eq!(body["initial_state"], own);
        // 兩邊說的相反、initial_state 不是陣列、多帶 encrypted：都是用法錯
        for (encrypted, fields) in [
            (false, json!({ "initial_state": own })),
            (true, json!({ "initial_state": {} })),
            (true, json!({ "encrypted": true })),
        ] {
            assert!(matches!(
                to_create_room_body(encrypted, map(fields)),
                Err(SdkError::Usage(_))
            ));
        }
    }

    /// /docs/design/rooms/room-actions.md §4.2 的表：數字欄換、對照表逐個 key 合、`null` 拿掉、其他照舊。
    #[test]
    fn power_levels_merge_field_by_field_and_key_by_key() {
        let current = json!({ "users": { "@alice:x": 100, "@carol:x": 50 }, "events": { "m.room.name": 50 }, "ban": 50, "kick": 50 });
        let merged = merge_power_levels(
            Some(&current),
            &map(json!({ "users": { "@bob:x": 50, "@carol:x": null }, "events_default": 100, "kick": 75 })),
        )
        .unwrap();
        assert_eq!(
            merged,
            json!({ "users": { "@alice:x": 100, "@bob:x": 50 }, "events": { "m.room.name": 50 },
                    "ban": 50, "kick": 75, "events_default": 100 })
        );
        // notifications 也逐個 key
        let merged = merge_power_levels(
            Some(&current),
            &map(json!({ "notifications": { "room": 0 } })),
        )
        .unwrap();
        assert_eq!(merged["notifications"], json!({ "room": 0 }));
        assert_eq!(merged["users"], current["users"], "沒給的照舊");
    }

    #[test]
    fn power_levels_refuse_what_matrix_would_not_accept_but_not_values() {
        let current = json!({ "users": { "@alice:x": 100 } });
        for wrong in [
            json!({}),
            json!({ "kick": "50" }),
            json!({ "kick": 1.5 }),
            json!({ "users": [] }),
            json!({ "users": { "bob": 50 } }),
            json!({ "users": { "@bob:x": "50" } }),
            json!({ "creator": "@alice:x" }),
        ] {
            assert!(
                matches!(
                    merge_power_levels(Some(&current), &map(wrong.clone())),
                    Err(SdkError::Usage(_))
                ),
                "{wrong}"
            );
        }
        // 值合不合理是 server 擋：負數、比自己高都照寫
        assert!(merge_power_levels(
            Some(&current),
            &map(json!({ "users": { "@bob:x": 9000 }, "ban": -1 }))
        )
        .is_ok());
        // 沒有 power_levels 的房：🚫 從空的開始（建房者會丟掉 100）
        assert!(matches!(
            merge_power_levels(None, &map(json!({ "kick": 50 }))),
            Err(SdkError::Usage(_))
        ));
    }

    #[test]
    fn pinning_adds_to_the_end_unpinning_removes_and_no_change_writes_nothing() {
        let current = json!({ "pinned": ["$a", "$b"] });
        assert_eq!(
            to_pinned_content(Some(&current), "$c", true).unwrap(),
            Some(json!({ "pinned": ["$a", "$b", "$c"] }))
        );
        assert_eq!(
            to_pinned_content(Some(&current), "$a", false).unwrap(),
            Some(json!({ "pinned": ["$b"] }))
        );
        assert_eq!(to_pinned_content(Some(&current), "$a", true).unwrap(), None);
        assert_eq!(
            to_pinned_content(Some(&current), "$z", false).unwrap(),
            None
        );
        assert_eq!(
            to_pinned_content(None, "$a", true).unwrap(),
            Some(json!({ "pinned": ["$a"] }))
        );
        assert!(to_pinned_content(Some(&json!({ "pinned": [1] })), "$a", true).is_err());
    }

    #[test]
    fn m_direct_adds_and_removes_one_room_for_one_person() {
        let current = json!({ "@bob:x": ["!a:x"], "@carol:x": ["!c:x"] });
        assert_eq!(
            to_direct_content(Some(&current), "@bob:x", "!r:x", true).unwrap(),
            Some(json!({ "@bob:x": ["!a:x", "!r:x"], "@carol:x": ["!c:x"] }))
        );
        assert_eq!(
            to_direct_content(Some(&current), "@carol:x", "!c:x", false).unwrap(),
            Some(json!({ "@bob:x": ["!a:x"] })),
            "清單空了就把那個人拿掉"
        );
        assert_eq!(
            to_direct_content(Some(&current), "@bob:x", "!a:x", true).unwrap(),
            None
        );
        assert_eq!(
            to_direct_content(None, "@bob:x", "!r:x", true).unwrap(),
            Some(json!({ "@bob:x": ["!r:x"] }))
        );
        assert!(to_direct_content(None, "bob", "!r:x", true).is_err());
    }
}

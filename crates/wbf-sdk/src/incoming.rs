//! 上游給的事件原樣（`IncomingEvent`）、一頁的游標（`EventPage`），與事件的分類規則（local-cache-db.md §7）。
//!
//! ⭐ 「收到了什麼」與「最後該顯示什麼」是兩件事：這裡只回答前者與「它屬於哪一類、參照誰」；
//! 顯示用的 `Message` 在 `event_json`，存法在 `cache`。這裡只有 serde_json，沒有 matrix-sdk、沒有 SQL。

use serde::{Deserialize, Serialize};

use crate::protocol::event_seqs;

/// 解不開的事件沒有更細的原因時用的字（WS 那條路根本不解密）。
pub const NOT_DECRYPTED_HERE: &str = "NotDecryptedHere";

/// 上游給的一則事件，照**拿到的樣子**分三種。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum IncomingEvent {
    /// 本來就沒加密的事件。
    Plain { event: serde_json::Value },
    /// 解開了的加密事件。
    Decrypted {
        /// 收到的密文事件。⚠️ matrix-sdk 解開之後**不給密文**（`DecryptedRoomEvent` 只有明文），那條路是 None
        /// （維護者 2026-09-14：拿不到就放 NULL）；自己撈、自己解的才有。
        ciphertext: Option<serde_json::Value>,
        /// 解開後的完整事件（有 `event_id`／`sender`／`origin_server_ts`，`type` 與 `content` 是明文的）。
        cleartext: serde_json::Value,
    },
    /// 還沒解（或解不開）的加密事件：密文照存。
    Undecrypted {
        ciphertext: serde_json::Value,
        reason: String,
    },
}

impl IncomingEvent {
    /// WS（`Event/Recent`）拿到的原始 JSON：這條路不解密，`m.room.encrypted` 就是還沒解的。
    ///
    /// Args:
    ///     event: example: {"type":"m.room.message","event_id":"$a","sender":"@a:x","origin_server_ts":1,"content":{"body":"hi"}}
    /// Return:
    ///     IncomingEvent  `type` 是 `m.room.encrypted` → Undecrypted（原因 `NotDecryptedHere`）；其他 → Plain
    pub fn from_ws_json(event: serde_json::Value) -> IncomingEvent {
        match event.get("type").and_then(|value| value.as_str()) {
            Some("m.room.encrypted") => IncomingEvent::Undecrypted {
                ciphertext: event,
                reason: NOT_DECRYPTED_HERE.to_string(),
            },
            _ => IncomingEvent::Plain { event },
        }
    }

    /// 原樣收到的事件（存進 `raw_event` 的那份）。
    ///
    /// Return:
    ///     Some(&Value)  Plain 的事件、Undecrypted 的密文、Decrypted 有帶的密文
    ///     None          matrix-sdk 解開的事件：密文拿不到
    pub fn raw_event(&self) -> Option<&serde_json::Value> {
        match self {
            IncomingEvent::Plain { event } => Some(event),
            IncomingEvent::Decrypted { ciphertext, .. } => ciphertext.as_ref(),
            IncomingEvent::Undecrypted { ciphertext, .. } => Some(ciphertext),
        }
    }

    /// 看得懂的那一份（`type`／`content` 是明文）。
    ///
    /// Return:
    ///     Some(&Value)  Plain 的事件、Decrypted 的明文
    ///     None          Undecrypted
    pub fn cleartext(&self) -> Option<&serde_json::Value> {
        match self {
            IncomingEvent::Plain { event } => Some(event),
            IncomingEvent::Decrypted { cleartext, .. } => Some(cleartext),
            IncomingEvent::Undecrypted { .. } => None,
        }
    }

    /// 帶著 server 蓋的欄位（`event_id`、`sender`、`origin_server_ts`、`unsigned` 裡的序號與 `redacted_because`）那一份：
    /// 有原樣就用原樣，否則用明文（matrix-sdk 解開時把這些欄位併進明文了）。
    pub fn envelope(&self) -> &serde_json::Value {
        match self {
            IncomingEvent::Plain { event } => event,
            IncomingEvent::Decrypted {
                ciphertext: Some(ciphertext),
                ..
            } => ciphertext,
            IncomingEvent::Decrypted {
                ciphertext: None,
                cleartext,
            } => cleartext,
            IncomingEvent::Undecrypted { ciphertext, .. } => ciphertext,
        }
    }

    /// Return:
    ///     Some(true)   解開了的加密事件
    ///     Some(false)  沒解開的加密事件
    ///     None         本來就不是加密事件
    pub fn decrypted(&self) -> Option<bool> {
        match self {
            IncomingEvent::Plain { .. } => None,
            IncomingEvent::Decrypted { .. } => Some(true),
            IncomingEvent::Undecrypted { .. } => Some(false),
        }
    }

    /// Return:
    ///     Some(event_id)  envelope 有字串的 `event_id`
    ///     None            沒有 —— 這種事件不能被參照、不能當游標，呼叫端不存
    pub fn find_event_id(&self) -> Option<&str> {
        self.envelope()
            .get("event_id")
            .and_then(|value| value.as_str())
    }

    /// Return:
    ///     (r_seq, g_seq)  從 envelope 的 `unsigned` 讀；非 fork server 兩個都是 None
    pub fn seqs(&self) -> (Option<i64>, Option<i64>) {
        event_seqs(self.envelope())
    }

    /// server 送來的時候就已經是 redact 過的樣子（`unsigned.redacted_because`）。
    pub fn is_redacted_by_server(&self) -> bool {
        self.envelope()
            .get("unsigned")
            .and_then(|unsigned| unsigned.get("redacted_because"))
            .is_some()
    }
}

/// 上游的一頁，**照上游給的順序**（新到舊）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventPage {
    pub events: Vec<IncomingEvent>,
    /// 下一頁的錨（`event_id`）；None ＝到頭了。
    pub next: Option<String>,
}

impl EventPage {
    /// 🚨 **`next` 從上游的原始順序取**（PR #36 審查 rumia🔴）：原始順序裡最舊、有 `event_id` 的那則。
    /// 🚫 不從折疊或讀回之後的結果推 —— 折疊會拿掉關係事件、讀回會濾掉 `hidden`，最後一則都可能比較新，
    /// 拿它往回問就會重複同一段。游標要的是「上游這一頁到哪裡」，不是「這一頁顯示了什麼」。
    ///
    /// Args:
    ///     events: 上游那一頁，新到舊
    /// Return:
    ///     EventPage  空頁（或整頁都沒有 `event_id`）的 `next` 才是 None
    pub fn from_upstream_order(events: Vec<IncomingEvent>) -> EventPage {
        let next = events
            .iter()
            .rev()
            .find_map(IncomingEvent::find_event_id)
            .map(str::to_string);
        EventPage { events, next }
    }
}

/// 事件的類別（`events.class`，local-cache-db.md §7.3）。字串是格式的一部分。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EventClass {
    /// 還看不懂的（沒解開的密文）。
    General,
    /// 自己就是要顯示的一則（訊息、狀態事件、認不得的 type 都算）。
    Msg,
    Edit,
    Redact,
    Reaction,
}

impl EventClass {
    pub fn to_column(self) -> &'static str {
        match self {
            EventClass::General => "general",
            EventClass::Msg => "msg",
            EventClass::Edit => "edit",
            EventClass::Redact => "redact",
            EventClass::Reaction => "reaction",
        }
    }

    /// Return:
    ///     Some(EventClass)  認得的字
    ///     None              其他 —— 呼叫端當壞資料
    pub fn from_column(column: &str) -> Option<EventClass> {
        match column {
            "general" => Some(EventClass::General),
            "msg" => Some(EventClass::Msg),
            "edit" => Some(EventClass::Edit),
            "redact" => Some(EventClass::Redact),
            "reaction" => Some(EventClass::Reaction),
            _ => None,
        }
    }
}

/// 一則事件分類完的結果：寫進 `events` 的那幾欄。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Classified {
    pub class: EventClass,
    /// 明文的 `type`；沒解開的是 None。
    pub event_type: Option<String>,
    /// `final_body` 的初值：msg 是自己的 `content`、edit 是 `m.new_content`、redact／reaction 是自己的 `content`；
    /// 沒解開的是 None（＝還沒處理）。
    pub final_body: Option<serde_json::Value>,
    /// edit／redact／reaction 指向的目標。
    pub ref_event_id: Option<String>,
}

/// 照明文分類（local-cache-db.md §7.4）。
///
/// ⚠️ 狀態事件（有 `state_key`）一律是 msg，🚫 不會被當成 edit —— spec 規定 edit 不能是狀態事件。
/// ⚠️ 缺 `m.new_content` 的 `m.replace` 不是有效的 edit（spec），當 msg 顯示它自己的 `body`（fallback）。
///
/// Args:
///     incoming: example: IncomingEvent::Plain { event: {"type":"m.reaction", "content":{"m.relates_to":{"rel_type":"m.annotation","event_id":"$t","key":"👍"}}, …} }
/// Return:
///     Classified  Undecrypted → General、其他照明文的 type 與 `m.relates_to` 分
pub fn classify(incoming: &IncomingEvent) -> Classified {
    let Some(cleartext) = incoming.cleartext() else {
        return Classified {
            class: EventClass::General,
            event_type: None,
            final_body: None,
            ref_event_id: None,
        };
    };
    let event_type = cleartext
        .get("type")
        .and_then(|value| value.as_str())
        .unwrap_or("unknown")
        .to_string();
    let content = cleartext
        .get("content")
        .cloned()
        .unwrap_or_else(|| serde_json::Value::Object(Default::default()));
    let is_state = cleartext.get("state_key").is_some();
    let relates = content.get("m.relates_to");
    let relates_text = |key: &str| {
        relates
            .and_then(|relates| relates.get(key))
            .and_then(|value| value.as_str())
            .map(str::to_string)
    };
    let (class, ref_event_id, final_body) = match event_type.as_str() {
        "m.room.redaction" => {
            // room v11 起 `redacts` 在 content 裡；之前在事件頂層。兩邊都看。
            let target = content
                .get("redacts")
                .or_else(|| cleartext.get("redacts"))
                .and_then(|value| value.as_str())
                .map(str::to_string);
            match target {
                Some(target) => (EventClass::Redact, Some(target), content),
                None => (EventClass::Msg, None, content),
            }
        }
        "m.reaction" => match (
            relates_text("rel_type").as_deref(),
            relates_text("event_id"),
        ) {
            (Some("m.annotation"), Some(target)) if relates_text("key").is_some() => {
                (EventClass::Reaction, Some(target), content)
            }
            _ => (EventClass::Msg, None, content),
        },
        _ => match (
            is_state,
            relates_text("rel_type").as_deref(),
            relates_text("event_id"),
            content.get("m.new_content"),
        ) {
            (false, Some("m.replace"), Some(target), Some(new_content)) => {
                (EventClass::Edit, Some(target), new_content.clone())
            }
            _ => (EventClass::Msg, None, content),
        },
    };
    Classified {
        class,
        event_type: Some(event_type),
        final_body: Some(final_body),
        ref_event_id,
    }
}

/// 一則 edit 或它的目標在「edit 有不有效」這件事上需要的事實。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReplacementSide<'a> {
    pub sender: &'a str,
    pub class: EventClass,
    /// 明文的 `type`。
    pub event_type: Option<&'a str>,
    pub is_state: bool,
    /// 原本是不是加密事件（`decrypted` 不是 NULL）。
    pub was_encrypted: bool,
}

/// spec「validity of replacement events」（local-cache-db.md §7.5 的表）。
///
/// 🚨 **不是正面認得有效，就是無效**：任何一條說不出來（例如 type 缺）都拒絕。兩條是資安相關：
/// sender 不同（別人改你的訊息）、加密的目標配明文的 edit（用明文蓋掉加密訊息）。
///
/// Args:
///     target: 被改的那則
///     edit: 那個 edit
/// Return:
///     Ok(())               有效
///     Err(&'static str)    無效的理由（寫 log 用）
pub fn check_replacement(
    target: &ReplacementSide<'_>,
    edit: &ReplacementSide<'_>,
) -> Result<(), &'static str> {
    if edit.class != EventClass::Edit {
        return Err("not an edit");
    }
    if target.class != EventClass::Msg {
        // 目標本身是 edit（不能 edit 一個 edit）、redact、reaction、還沒解開的，都不改。
        return Err("the target is not a message");
    }
    if target.sender != edit.sender {
        return Err("the edit comes from a different sender");
    }
    if target.is_state || edit.is_state {
        return Err("state events cannot be replaced or be replacements");
    }
    match (target.event_type, edit.event_type) {
        (Some(target_type), Some(edit_type)) if target_type == edit_type => {}
        _ => return Err("the edit changes the event type"),
    }
    if target.was_encrypted && !edit.was_encrypted {
        return Err("a plaintext edit cannot replace an encrypted event");
    }
    Ok(())
}

/// 套一個有效的 edit 之後，目標的 `final_body`：`m.new_content` 整份取代，
/// 但 **`m.relates_to` 留目標原本的**（spec：`m.new_content` 裡的 `m.relates_to` 不算數）—— 否則回覆關係被 edit 洗掉。
///
/// Args:
///     target_final_body: 目標現在的 final_body, example: {"msgtype":"m.text","body":"hi","m.relates_to":{"m.in_reply_to":{"event_id":"$q"}}}
///     new_content: edit 的 final_body（`m.new_content`）, example: {"msgtype":"m.text","body":"hello"}
/// Return:
///     Value  new_content 去掉它自己的 `m.relates_to`、補上目標的（目標沒有就不補）
pub fn to_replaced_body(
    target_final_body: &serde_json::Value,
    new_content: &serde_json::Value,
) -> serde_json::Value {
    let mut replaced = match new_content {
        serde_json::Value::Object(object) => object.clone(),
        _ => serde_json::Map::new(),
    };
    replaced.remove("m.relates_to");
    if let Some(relates) = target_final_body.get("m.relates_to") {
        replaced.insert("m.relates_to".into(), relates.clone());
    }
    serde_json::Value::Object(replaced)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn plain(event: serde_json::Value) -> IncomingEvent {
        IncomingEvent::Plain { event }
    }

    #[test]
    fn next_comes_from_the_oldest_upstream_event_that_has_an_id() {
        let page = EventPage::from_upstream_order(vec![
            plain(json!({"event_id": "$new"})),
            plain(json!({"event_id": "$old"})),
            plain(json!({"no_id": true})),
        ]);
        assert_eq!(page.next.as_deref(), Some("$old"));
        assert_eq!(EventPage::from_upstream_order(vec![]).next, None);
    }

    #[test]
    fn ws_json_is_undecrypted_only_when_it_is_an_encrypted_event() {
        assert!(matches!(
            IncomingEvent::from_ws_json(json!({"type": "m.room.encrypted"})),
            IncomingEvent::Undecrypted { ref reason, .. } if reason == NOT_DECRYPTED_HERE
        ));
        assert!(matches!(
            IncomingEvent::from_ws_json(json!({"type": "m.room.message"})),
            IncomingEvent::Plain { .. }
        ));
    }

    #[test]
    fn sdk_decrypted_events_have_no_raw_event_and_use_the_cleartext_envelope() {
        let incoming = IncomingEvent::Decrypted {
            ciphertext: None,
            cleartext: json!({"event_id": "$e", "unsigned": {"org.wbftw.wbfuwunel.g_seq": 7}}),
        };
        assert_eq!(incoming.raw_event(), None);
        assert_eq!(incoming.find_event_id(), Some("$e"));
        assert_eq!(incoming.decrypted(), Some(true));
    }

    #[test]
    fn classify_each_kind() {
        let undecrypted = IncomingEvent::Undecrypted {
            ciphertext: json!({"type": "m.room.encrypted"}),
            reason: "x".into(),
        };
        assert_eq!(classify(&undecrypted).class, EventClass::General);
        assert_eq!(classify(&undecrypted).final_body, None);

        let message = classify(&plain(
            json!({"type": "m.room.message", "content": {"body": "hi"}}),
        ));
        assert_eq!(message.class, EventClass::Msg);
        assert_eq!(message.final_body, Some(json!({"body": "hi"})));

        let edit = classify(&plain(json!({"type": "m.room.message", "content": {
            "body": "* hello", "m.new_content": {"body": "hello"},
            "m.relates_to": {"rel_type": "m.replace", "event_id": "$t"}}})));
        assert_eq!(edit.class, EventClass::Edit);
        assert_eq!(edit.ref_event_id.as_deref(), Some("$t"));
        assert_eq!(
            edit.final_body,
            Some(json!({"body": "hello"})),
            "edit 的 final_body 是 new_content"
        );

        let redact_v11 = classify(&plain(
            json!({"type": "m.room.redaction", "content": {"redacts": "$t"}}),
        ));
        let redact_old = classify(&plain(
            json!({"type": "m.room.redaction", "redacts": "$t", "content": {}}),
        ));
        assert_eq!(
            (redact_v11.class, redact_v11.ref_event_id.as_deref()),
            (EventClass::Redact, Some("$t"))
        );
        assert_eq!(
            (redact_old.class, redact_old.ref_event_id.as_deref()),
            (EventClass::Redact, Some("$t"))
        );

        let reaction = classify(&plain(json!({"type": "m.reaction", "content": {
            "m.relates_to": {"rel_type": "m.annotation", "event_id": "$t", "key": "👍"}}})));
        assert_eq!(
            (reaction.class, reaction.ref_event_id.as_deref()),
            (EventClass::Reaction, Some("$t"))
        );
    }

    #[test]
    fn a_replace_without_new_content_or_on_a_state_event_is_not_an_edit() {
        let no_new_content = classify(&plain(json!({"type": "m.room.message", "content": {
            "body": "* x", "m.relates_to": {"rel_type": "m.replace", "event_id": "$t"}}})));
        assert_eq!(no_new_content.class, EventClass::Msg);
        let state = classify(&plain(
            json!({"type": "m.room.topic", "state_key": "", "content": {
            "topic": "x", "m.new_content": {"topic": "y"},
            "m.relates_to": {"rel_type": "m.replace", "event_id": "$t"}}}),
        ));
        assert_eq!(state.class, EventClass::Msg);
    }

    fn side<'a>(sender: &'a str, class: EventClass, encrypted: bool) -> ReplacementSide<'a> {
        ReplacementSide {
            sender,
            class,
            event_type: Some("m.room.message"),
            is_state: false,
            was_encrypted: encrypted,
        }
    }

    /// 🚨 資安相關的兩條：別人不能改你的訊息；明文 edit 不能蓋加密訊息。
    #[test]
    fn replacement_rules_fail_closed() {
        let target = side("@a:x", EventClass::Msg, true);
        assert_eq!(
            check_replacement(&target, &side("@a:x", EventClass::Edit, true)),
            Ok(())
        );
        assert!(check_replacement(&target, &side("@mallory:x", EventClass::Edit, true)).is_err());
        assert!(check_replacement(&target, &side("@a:x", EventClass::Edit, false)).is_err());
        assert!(check_replacement(
            &side("@a:x", EventClass::Edit, true),
            &side("@a:x", EventClass::Edit, true)
        )
        .is_err());
        let mut other_type = side("@a:x", EventClass::Edit, true);
        other_type.event_type = Some("m.sticker");
        assert!(check_replacement(&target, &other_type).is_err());
        let mut unknown_type = side("@a:x", EventClass::Edit, true);
        unknown_type.event_type = None;
        assert!(check_replacement(&target, &unknown_type).is_err());
        let mut state_target = side("@a:x", EventClass::Msg, false);
        state_target.is_state = true;
        assert!(check_replacement(&state_target, &side("@a:x", EventClass::Edit, false)).is_err());
    }

    #[test]
    fn a_replaced_body_keeps_the_targets_relation_and_drops_the_edits() {
        let target = json!({"body": "hi", "m.relates_to": {"m.in_reply_to": {"event_id": "$q"}}});
        let new_content =
            json!({"body": "hello", "m.relates_to": {"m.in_reply_to": {"event_id": "$evil"}}});
        assert_eq!(
            to_replaced_body(&target, &new_content),
            json!({"body": "hello", "m.relates_to": {"m.in_reply_to": {"event_id": "$q"}}})
        );
        assert_eq!(
            to_replaced_body(&json!({"body": "hi"}), &json!({"body": "yo"})),
            json!({"body": "yo"})
        );
    }
}

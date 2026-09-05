//! `backend/matrix_sdk.rs` 的事件轉換：JSON → `Message`、關係事件的聚合。不需要 server；要開 `--features matrix`。
#![cfg(feature = "matrix")]

use serde_json::json;
use wbf_sdk::backend::matrix_sdk::message_from_json;
use wbf_sdk::MessageKind;

fn text_event(id: &str, body: &str) -> serde_json::Value {
    json!({
        "type": "m.room.message", "event_id": id, "sender": "@a:localhost", "origin_server_ts": 1,
        "content": { "msgtype": "m.text", "body": body },
        "unsigned": { "org.wbftw.wbfuwunel.r_seq": 7, "org.wbftw.wbfuwunel.g_seq": 70 }
    })
}

#[test]
fn text_message_carries_seqs_and_body() {
    let message = message_from_json(&text_event("$a", "hi"));
    assert_eq!(message.id, "$a");
    assert_eq!(message.r_seq, Some(7));
    assert_eq!(message.g_seq, Some(70));
    assert!(matches!(&message.kind, MessageKind::Text { body, .. } if body == "hi"));
    assert_eq!(
        message.decrypted, None,
        "decryption state is set by the adapter, not the parser"
    );
}

#[test]
fn file_message_needs_a_valid_block_else_unsupported() {
    let good = json!({
        "type": "m.room.message", "event_id": "$f", "sender": "@a:localhost", "origin_server_ts": 1,
        "content": {
            "msgtype": "org.wbftw.wbfuwunel.file", "body": "x", "url": "mxc://localhost/1",
            "org.wbftw.wbfuwunel.chunked": { "v": 1, "cipher": "none", "chunk_size": 16, "file_size": 40 }
        }
    });
    let message = message_from_json(&good);
    assert!(
        matches!(&message.kind, MessageKind::File { attachment, .. } if attachment.mxc == "mxc://localhost/1")
    );

    let mut bad = good.clone();
    bad["content"]["org.wbftw.wbfuwunel.chunked"]["v"] = json!(2);
    let message = message_from_json(&bad);
    assert!(
        matches!(&message.kind, MessageKind::Unsupported { event_type, .. } if event_type.contains("unknown convention version"))
    );

    let mut no_block = good.clone();
    no_block["content"]
        .as_object_mut()
        .unwrap()
        .remove("org.wbftw.wbfuwunel.chunked");
    assert!(matches!(
        message_from_json(&no_block).kind,
        MessageKind::Unsupported { .. }
    ));
}

#[test]
fn redacted_and_unknown_events_are_not_dropped() {
    let redacted = json!({
        "type": "m.room.message", "event_id": "$r", "sender": "@a:localhost", "origin_server_ts": 1,
        "content": {}, "unsigned": { "redacted_because": { "content": { "reason": "spam" } } }
    });
    assert!(
        matches!(message_from_json(&redacted).kind, MessageKind::Deleted { reason: Some(ref r) } if r == "spam")
    );

    let unknown = json!({ "type": "org.example.custom", "event_id": "$u", "sender": "@a:localhost", "origin_server_ts": 1, "content": {} });
    assert!(
        matches!(message_from_json(&unknown).kind, MessageKind::Unsupported { ref event_type, .. } if event_type == "org.example.custom")
    );

    let member = json!({ "type": "m.room.member", "state_key": "@b:localhost", "event_id": "$m", "sender": "@b:localhost",
        "origin_server_ts": 1, "content": { "membership": "join" } });
    assert!(
        matches!(message_from_json(&member).kind, MessageKind::System { ref line, .. } if line == "@b:localhost join")
    );
}

#[test]
fn reply_to_is_read_from_relates_to() {
    let mut event = text_event("$b", "re");
    event["content"]["m.relates_to"] = json!({ "m.in_reply_to": { "event_id": "$a" } });
    assert_eq!(message_from_json(&event).reply_to.as_deref(), Some("$a"));
    assert_eq!(message_from_json(&text_event("$c", "no")).reply_to, None);
}

#[test]
fn missing_fields_become_unknown_not_empty() {
    let bare = json!({ "type": "m.room.message", "content": { "msgtype": "m.text", "body": "x" } });
    let message = message_from_json(&bare);
    assert_eq!(message.id, "unknown");
    assert_eq!(message.sender, "unknown");
    assert_eq!(message.conversation, "unknown");
    assert_eq!((message.r_seq, message.g_seq), (None, None));
}

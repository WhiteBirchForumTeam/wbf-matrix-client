//! `event_json.rs` 的事件轉換：JSON → `Message`、關係事件的聚合。不需要 server、不需要 matrix feature。

use serde_json::json;
use wbf_sdk::event_json::message_from_json;
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
/// 標準 Matrix 附件（/docs/design/rpc-specs/data-plane.md §7.1）：有 `file` 是加密的（kind 2）、只有 `url` 是明文的（kind 3）；
/// `file` 不完整**不**退成明文、**不**當成一個檔；`filename` 跟 `body` 不一樣時 `body` 是說明。
fn standard_attachments_are_recognised_and_a_broken_encrypted_one_is_not_taken_as_plain() {
    use wbf_sdk::media_kind::MediaKind;
    let encrypted = json!({
        "type": "m.room.message", "event_id": "$e", "sender": "@a:localhost", "origin_server_ts": 1,
        "content": {
            "msgtype": "m.image", "body": "看這張", "filename": "cat.jpg",
            "info": { "mimetype": "image/jpeg", "size": 1234 },
            "file": { "url": "mxc://matrix.org/AbC", "v": "v2", "iv": "w+sE15fzSc0AAAAAAAAAAA",
                      "key": { "kty": "oct", "alg": "A256CTR", "ext": true, "k": "qcHVMSgYg-71CauWBezXI5qkaRb0LuIy-Wx5kIaHMIA", "key_ops": ["encrypt", "decrypt"] },
                      "hashes": { "sha256": "fdSLu/YkRx3Wyh3KQabP3rd6+SFiKg5lsJZQHtkSAYA" } }
        }
    });
    let message = message_from_json(&encrypted);
    let MessageKind::MatrixFile {
        attachment,
        caption,
    } = &message.kind
    else {
        panic!("{:?}", message.kind)
    };
    assert_eq!(
        (
            attachment.mxc.as_str(),
            attachment.kind,
            attachment.name.as_deref(),
            attachment.mimetype.as_deref(),
            attachment.size,
            caption.as_deref()
        ),
        (
            "mxc://matrix.org/AbC",
            MediaKind::MatrixEncrypted,
            Some("cat.jpg"),
            Some("image/jpeg"),
            Some(1234),
            Some("看這張")
        )
    );
    assert!(attachment.file.is_some());

    let plain = json!({
        "type": "m.room.message", "event_id": "$p", "sender": "@a:localhost", "origin_server_ts": 1,
        "content": { "msgtype": "m.file", "body": "a.pdf", "url": "mxc://matrix.org/Pdf" }
    });
    let message = message_from_json(&plain);
    let MessageKind::MatrixFile {
        attachment,
        caption,
    } = &message.kind
    else {
        panic!("{:?}", message.kind)
    };
    assert_eq!(
        (
            attachment.kind,
            attachment.name.as_deref(),
            caption.as_deref()
        ),
        (MediaKind::MatrixPlain, Some("a.pdf"), None)
    );
    assert!(attachment.file.is_none());

    // `file` 少了 hash：**不**退回外層的 `url` 當明文（那會把密文當明文給出去）。
    let mut broken = encrypted.clone();
    broken["content"]["file"]["hashes"] = json!({});
    broken["content"]["url"] = json!("mxc://matrix.org/AbC");
    assert!(matches!(
        message_from_json(&broken).kind,
        MessageKind::Unsupported { .. }
    ));
    // 不是 mxc：**不**當成檔。
    let mut not_mxc = plain.clone();
    not_mxc["content"]["url"] = json!("https://example.org/a.pdf");
    assert!(matches!(
        message_from_json(&not_mxc).kind,
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

// ---- aggregate：同一頁內的關係事件折進目標（PR #9 審查 cirno 💡1、salvia）----

use wbf_sdk::event_json::messages_from_json;

/// 讀這一頁的帳號（算 `Reaction.mine`）。
const READER: &str = "@b:localhost";

fn relation(
    id: &str,
    sender: &str,
    event_type: &str,
    content: serde_json::Value,
) -> serde_json::Value {
    json!({ "type": event_type, "event_id": id, "sender": sender, "origin_server_ts": 2, "content": content })
}

#[test]
fn reactions_fold_into_target_and_disappear_as_events() {
    let page = vec![
        text_event("$a", "hi"),
        relation(
            "$r1",
            "@b:localhost",
            "m.reaction",
            json!({ "m.relates_to": { "rel_type": "m.annotation", "event_id": "$a", "key": "👍" } }),
        ),
        relation(
            "$r2",
            "@c:localhost",
            "m.reaction",
            json!({ "m.relates_to": { "rel_type": "m.annotation", "event_id": "$a", "key": "👍" } }),
        ),
        relation(
            "$r3",
            "@b:localhost",
            "m.reaction",
            json!({ "m.relates_to": { "rel_type": "m.annotation", "event_id": "$a", "key": "❤" } }),
        ),
    ];
    let messages = messages_from_json("!r:localhost", READER, &page);
    assert_eq!(messages.len(), 1, "reaction events are consumed");
    let reactions = &messages[0].reactions;
    assert_eq!(reactions.len(), 2);
    let thumbs = reactions
        .iter()
        .find(|reaction| reaction.key == "👍")
        .unwrap();
    assert_eq!(thumbs.by, vec!["@b:localhost", "@c:localhost"]);
    assert_eq!(
        thumbs.mine.as_deref(),
        Some("$r1"),
        "the reader pressed 👍 as $r1"
    );
    assert_eq!(
        messages[0].conversation, "!r:localhost",
        "sync events have no room_id; the page fills it in"
    );
}

/// 同一個人對同一個 key 按了兩則：`by` 只算一次，`mine` 是最早的那則（頁是新到舊也一樣，/docs/design/messages/message-actions.md §5）。
/// 別人按的不是 `mine`。
#[test]
fn the_same_person_pressing_twice_counts_once_and_mine_is_the_earliest() {
    let pressed = |id: &str, sender: &str, ts: u64| {
        let mut event = relation(
            id,
            sender,
            "m.reaction",
            json!({ "m.relates_to": { "rel_type": "m.annotation", "event_id": "$a", "key": "👍" } }),
        );
        event["origin_server_ts"] = json!(ts);
        event
    };
    // 新到舊（`room.history` 的順序）。
    let page = vec![
        pressed("$late", READER, 30),
        pressed("$other", "@c:localhost", 20),
        pressed("$early", READER, 10),
        text_event("$a", "hi"),
    ];
    let messages = messages_from_json("!r:localhost", READER, &page);
    assert_eq!(messages.len(), 1);
    let thumbs = &messages[0].reactions[0];
    assert_eq!(thumbs.by, vec![READER, "@c:localhost"]);
    assert_eq!(thumbs.mine.as_deref(), Some("$early"));

    let messages = messages_from_json("!r:localhost", "@d:localhost", &page);
    assert_eq!(messages[0].reactions[0].mine, None, "nobody else's is mine");
}

#[test]
fn edit_replaces_content_and_marks_editor() {
    let page = vec![
        text_event("$a", "first"),
        relation(
            "$e",
            "@a:localhost",
            "m.room.message",
            json!({
                "msgtype": "m.text", "body": "* second",
                "m.relates_to": { "rel_type": "m.replace", "event_id": "$a" },
                "m.new_content": { "msgtype": "m.text", "body": "second" }
            }),
        ),
    ];
    let messages = messages_from_json("!r:localhost", READER, &page);
    assert_eq!(messages.len(), 1);
    assert!(matches!(&messages[0].kind, MessageKind::Text { body, .. } if body == "second"));
    assert_eq!(messages[0].edited_by.as_deref(), Some("@a:localhost"));
}

#[test]
fn redaction_marks_target_deleted() {
    let page = vec![
        text_event("$a", "oops"),
        relation(
            "$d",
            "@a:localhost",
            "m.room.redaction",
            json!({ "redacts": "$a", "reason": "typo" }),
        ),
    ];
    let messages = messages_from_json("!r:localhost", READER, &page);
    assert_eq!(messages.len(), 1);
    assert!(
        matches!(&messages[0].kind, MessageKind::Deleted { reason: Some(reason) } if reason == "typo")
    );

    // room v11 之前 redacts 在頂層
    let mut old_style = relation("$d2", "@a:localhost", "m.room.redaction", json!({}));
    old_style["redacts"] = json!("$a");
    let messages = messages_from_json("!r:localhost", READER, &[text_event("$a", "x"), old_style]);
    assert!(matches!(messages[0].kind, MessageKind::Deleted { .. }));
}

fn at(mut event: serde_json::Value, ts: u64) -> serde_json::Value {
    event["origin_server_ts"] = json!(ts);
    event
}

fn edit_of(id: &str, sender: &str, target: &str, body: &str) -> serde_json::Value {
    relation(
        id,
        sender,
        "m.room.message",
        json!({
            "msgtype": "m.text", "body": format!("* {body}"),
            "m.relates_to": { "rel_type": "m.replace", "event_id": target },
            "m.new_content": { "msgtype": "m.text", "body": body }
        }),
    )
}

fn redaction_of(id: &str, sender: &str, target: &str) -> serde_json::Value {
    relation(id, sender, "m.room.redaction", json!({ "redacts": target }))
}

/// 收回比 edit 早、兩則在同一頁：還是收回贏，跟寫庫那條一樣（/docs/design/messages/edits-and-redactions.md §5）。
/// 新到舊（`room.history`）與舊到新（watch）兩種順序都一樣。
#[test]
fn a_redaction_older_than_an_edit_on_the_same_page_still_wins() {
    let newest_first = vec![
        at(edit_of("$e", "@a:localhost", "$a", "resurrected"), 20),
        at(redaction_of("$d", "@a:localhost", "$a"), 10),
        at(text_event("$a", "first"), 5),
    ];
    let mut oldest_first = newest_first.clone();
    oldest_first.reverse();
    for page in [newest_first, oldest_first] {
        let messages = messages_from_json("!r:localhost", READER, &page);
        assert_eq!(messages.len(), 1, "the edit and the redaction fold away");
        assert!(
            matches!(messages[0].kind, MessageKind::Deleted { .. }),
            "{:?}",
            messages[0].kind
        );
        assert_eq!(messages[0].edited_by, None);
    }
}

/// 別人送的 `m.replace` 指向你的訊息：折掉、不套（`check_replacement`，/docs/design/messages/edits-and-redactions.md §5）。
#[test]
fn an_edit_from_someone_else_is_folded_but_not_applied() {
    let page = vec![
        text_event("$a", "first"),
        edit_of("$e", "@mallory:localhost", "$a", "forged"),
    ];
    let messages = messages_from_json("!r:localhost", READER, &page);
    assert_eq!(
        messages.len(),
        1,
        "an edit never shows as a message of its own"
    );
    assert!(matches!(&messages[0].kind, MessageKind::Text { body, .. } if body == "first"));
    assert_eq!(messages[0].edited_by, None);
}

/// 加密的訊息只能用加密的 edit 改：明文的 edit 折掉、不套；加密的照套。
#[test]
fn a_plaintext_edit_cannot_replace_an_encrypted_message() {
    use wbf_sdk::event_json::messages_from_incoming;
    use wbf_sdk::incoming::IncomingEvent;
    let target = IncomingEvent::Decrypted {
        ciphertext: None,
        cleartext: text_event("$a", "secret"),
    };
    let edit = edit_of("$e", "@a:localhost", "$a", "plain");

    let page = vec![
        target.clone(),
        IncomingEvent::Plain {
            event: edit.clone(),
        },
    ];
    let messages = messages_from_incoming("!r:localhost", READER, &page);
    assert_eq!(messages.len(), 1);
    assert!(matches!(&messages[0].kind, MessageKind::Text { body, .. } if body == "secret"));
    assert_eq!(messages[0].edited_by, None);

    let page = vec![
        target,
        IncomingEvent::Decrypted {
            ciphertext: None,
            cleartext: edit,
        },
    ];
    let messages = messages_from_incoming("!r:localhost", READER, &page);
    assert!(matches!(&messages[0].kind, MessageKind::Text { body, .. } if body == "plain"));
    assert_eq!(messages[0].edited_by.as_deref(), Some("@a:localhost"));
}

/// 表情或 edit 跟收回它的那則在同一頁：不算數，`mine` 也不指著已收回的那則（/docs/design/messages/message-actions.md §5）。
#[test]
fn a_reaction_or_edit_withdrawn_on_the_same_page_does_not_count() {
    let pressed = |id: &str, sender: &str| {
        relation(
            id,
            sender,
            "m.reaction",
            json!({ "m.relates_to": { "rel_type": "m.annotation", "event_id": "$a", "key": "👍" } }),
        )
    };
    let page = vec![
        at(redaction_of("$undo_edit", "@a:localhost", "$e"), 40),
        at(redaction_of("$undo_press", READER, "$press"), 30),
        at(edit_of("$e", "@a:localhost", "$a", "second"), 20),
        at(pressed("$other", "@c:localhost"), 15),
        at(pressed("$press", READER), 10),
        at(text_event("$a", "first"), 5),
    ];
    let messages = messages_from_json("!r:localhost", READER, &page);
    assert_eq!(messages.len(), 1, "every relation folds into $a");
    assert!(matches!(&messages[0].kind, MessageKind::Text { body, .. } if body == "first"));
    assert_eq!(messages[0].edited_by, None);
    let thumbs = &messages[0].reactions[0];
    assert_eq!(thumbs.by, vec!["@c:localhost"]);
    assert_eq!(thumbs.mine, None);
}

#[test]
fn relation_whose_target_is_not_on_the_page_is_kept() {
    let page = vec![relation(
        "$r",
        "@b:localhost",
        "m.reaction",
        json!({ "m.relates_to": { "rel_type": "m.annotation", "event_id": "$elsewhere", "key": "👍" } }),
    )];
    let messages = messages_from_json("!r:localhost", READER, &page);
    assert_eq!(messages.len(), 1, "not dropped");
    assert!(
        matches!(&messages[0].kind, MessageKind::Unsupported { event_type, .. } if event_type == "m.reaction")
    );
}

/// 我們傳統上傳的檔送出去的事件（/docs/design/rpc-specs/data-plane.md §7.2）：收的那邊（同一支解析）讀回來是同一個檔、同一種格式、同一個說明。
#[test]
fn a_traditional_upload_is_sent_as_a_standard_attachment_that_reads_back_the_same() {
    use wbf_sdk::event_json::matrix_file_message_content;
    use wbf_sdk::media_kind::MediaKind;
    use wbf_sdk::MatrixManifest;
    let file = json!({ "v": "v2", "url": "mxc://matrix.org/AbCdEf", "iv": "<iv>",
                       "key": { "kty": "oct", "alg": "A256CTR", "k": "<key>", "ext": true, "key_ops": ["encrypt", "decrypt"] },
                       "hashes": { "sha256": "<sha256>" } });
    let encrypted = MatrixManifest {
        server: "https://matrix.org".into(),
        mxc: "mxc://matrix.org/AbCdEf".into(),
        kind: MediaKind::MatrixEncrypted,
        name: "cat.png".into(),
        mimetype: Some("image/png".into()),
        size: 81234,
        file: Some(file.clone()),
    };
    let content = matrix_file_message_content(&encrypted, Some("look")).unwrap();
    assert_eq!(
        content,
        json!({ "msgtype": "m.image", "body": "look", "filename": "cat.png",
                "info": { "size": 81234, "mimetype": "image/png" }, "file": file })
    );
    let event = json!({ "type": "m.room.message", "event_id": "$f", "sender": "@a:matrix.org", "origin_server_ts": 1, "content": content });
    match message_from_json(&event).kind {
        MessageKind::MatrixFile {
            attachment,
            caption,
        } => {
            assert_eq!(
                (
                    attachment.mxc.as_str(),
                    attachment.kind,
                    attachment.name.as_deref(),
                    attachment.size
                ),
                (
                    "mxc://matrix.org/AbCdEf",
                    MediaKind::MatrixEncrypted,
                    Some("cat.png"),
                    Some(81234)
                )
            );
            assert_eq!(caption.as_deref(), Some("look"));
        }
        other => panic!("{other:?}"),
    }
    let plain = MatrixManifest {
        mimetype: Some("application/pdf".into()),
        name: "report.pdf".into(),
        kind: MediaKind::MatrixPlain,
        file: None,
        ..encrypted.clone()
    };
    assert_eq!(
        matrix_file_message_content(&plain, None).unwrap(),
        json!({ "msgtype": "m.file", "body": "report.pdf", "url": "mxc://matrix.org/AbCdEf",
                "info": { "size": 81234, "mimetype": "application/pdf" } })
    );
}

/// 帶回來的 manifest 說的跟它帶的對不上：**不**送（加密房的檔沒 `file`、明文的帶了金鑰、`file` 指著另一個 mxc、`file` 不齊）。
#[test]
fn a_traditional_manifest_that_contradicts_itself_is_not_sent() {
    use wbf_sdk::event_json::matrix_file_message_content;
    use wbf_sdk::media_kind::MediaKind;
    use wbf_sdk::MatrixManifest;
    let file = json!({ "v": "v2", "url": "mxc://matrix.org/AbCdEf", "iv": "<iv>", "key": { "k": "<key>" }, "hashes": { "sha256": "<sha256>" } });
    let base = MatrixManifest {
        server: "https://matrix.org".into(),
        mxc: "mxc://matrix.org/AbCdEf".into(),
        kind: MediaKind::MatrixEncrypted,
        name: "a.bin".into(),
        mimetype: None,
        size: 10,
        file: Some(file.clone()),
    };
    let mut other_mxc = file.clone();
    other_mxc["url"] = json!("mxc://matrix.org/Other");
    let mut no_hash = file.clone();
    no_hash["hashes"] = json!({});
    for broken in [
        MatrixManifest {
            file: None,
            ..base.clone()
        },
        MatrixManifest {
            kind: MediaKind::MatrixPlain,
            ..base.clone()
        },
        MatrixManifest {
            kind: MediaKind::WbfChunked,
            ..base.clone()
        },
        MatrixManifest {
            file: Some(other_mxc),
            ..base.clone()
        },
        MatrixManifest {
            file: Some(no_hash),
            ..base.clone()
        },
    ] {
        assert!(
            matches!(
                matrix_file_message_content(&broken, None),
                Err(wbf_sdk::SdkError::Usage(_))
            ),
            "{broken:?}"
        );
    }
    assert!(matrix_file_message_content(&base, None).is_ok());
}

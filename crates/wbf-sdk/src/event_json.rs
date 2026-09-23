//! 原始 Matrix 事件 JSON → 我們的 `Message`（chat-model §2.3、§3.4）。
//!
//! 這裡只有 serde_json 與聊天模型，沒有 matrix-sdk：`/sync`、`/messages`、`Event/Recent` 回來的事件都長這樣，
//! 所以 `backend/matrix_sdk`（feature `matrix`）與 `recent`（純 WS）共用同一份轉換，不會漂移。
//! 解密狀態（`decrypted`）這裡一律 None：只有 matrix-sdk 的 `TimelineEvent` 知道，由它的 adapter 補。

use crate::chat::{Attachment, Message, MessageKind, Reaction};
use crate::incoming::IncomingEvent;
use crate::protocol::event_seqs;

/// 約定 §5 的 msgtype 與區塊 key。
pub const FILE_MSGTYPE: &str = "org.wbftw.wbfuwunel.file";
pub const CHUNKED_BLOCK_KEY: &str = "org.wbftw.wbfuwunel.chunked";

/// 約定 §5 的檔案事件 content（`m.room.message`，msgtype 是 [`FILE_MSGTYPE`]）。
/// matrix-sdk 那條路（`Room::send`）與 wbf 那條路（`Event/Send`）共用這一份，送出去的事件才不會漂。
///
/// Args:
///     attachment: example: &Attachment { mxc: "mxc://localhost/1122334455667788".into(), block: <約定 §5 的區塊> }
///     caption: example: Some("看這個")
/// Return:
///     Ok(Value)     `{"msgtype": FILE_MSGTYPE, "body": "<caption>\n<name>（WBF 分塊檔，需要 WBF client 才能開）", "url": mxc, CHUNKED_BLOCK_KEY: block, "caption"?: caption}`
///     Err(Usage)    區塊不能進事件（`ChunkedBlock::check_as_event_block`）
pub fn file_message_content(
    attachment: &Attachment,
    caption: Option<&str>,
) -> Result<serde_json::Value, crate::error::SdkError> {
    attachment.block.check_as_event_block()?;
    let name = attachment
        .block
        .name
        .clone()
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "file".to_string());
    let body = match caption {
        Some(caption) => format!("{caption}\n{name}（WBF 分塊檔，需要 WBF client 才能開）"),
        None => format!("{name}（WBF 分塊檔，需要 WBF client 才能開）"),
    };
    let mut content = serde_json::json!({
        "msgtype": FILE_MSGTYPE,
        "body": body,
        "url": attachment.mxc,
        CHUNKED_BLOCK_KEY: attachment.block,
    });
    if let (Some(caption), Some(fields)) = (caption, content.as_object_mut()) {
        fields.insert(
            "caption".to_string(),
            serde_json::Value::String(caption.to_string()),
        );
    }
    Ok(content)
}

/// 一頁事件 JSON → `Message` 陣列，關係事件折進目標（`aggregate`）。給沒有 `TimelineEvent` 的呼叫者與測試用；
/// `decrypted` 一律 None（解密狀態只有 matrix-sdk 的 `TimelineEvent` 知道）。
///
/// Args:
///     conversation: room_id，sync 的事件沒帶時補上
///     raws: 事件 JSON，照 server 給的順序
/// Return:
///     Vec<Message>  已折進去的 reaction／edit／redaction 事件不在裡面；目標不在這一頁的照原樣留著
pub fn messages_from_json(conversation: &str, raws: &[serde_json::Value]) -> Vec<Message> {
    aggregate(
        conversation,
        raws.iter()
            .map(|raw| (message_from_json(raw), relation_of(raw)))
            .collect(),
    )
}

/// 上游一頁（`IncomingEvent`，照上游順序）→ 顯示用的 `Message`，關係事件折進**同一頁**的目標（`aggregate`）。
/// 給不寫庫的路（`sync=server`、watch 的通知）；寫庫的路讀回本地處理過的樣子（local-cache-db.md §7）。
///
/// Args:
///     conversation: room_id，sync 的事件沒帶時補上
///     events: 上游那一頁
/// Return:
///     Vec<Message>  沒解開的是 `Unsupported`、`decrypted: Some(false)` 帶原因；已折進去的關係事件不在裡面
pub fn messages_from_incoming(conversation: &str, events: &[IncomingEvent]) -> Vec<Message> {
    aggregate(
        conversation,
        events
            .iter()
            .map(|incoming| {
                let envelope = incoming.envelope();
                let mut message = message_from_json(incoming.cleartext().unwrap_or(envelope));
                // server 蓋的欄位一律從 envelope 拿：自己解的明文不一定帶著它們。
                let text = |key: &str| envelope.get(key).and_then(|value| value.as_str());
                message.id = text("event_id").unwrap_or("unknown").to_string();
                message.sender = text("sender").unwrap_or("unknown").to_string();
                message.sent_at = envelope
                    .get("origin_server_ts")
                    .and_then(|ts| ts.as_u64())
                    .unwrap_or(0);
                (message.r_seq, message.g_seq) = incoming.seqs();
                message.decrypted = incoming.decrypted();
                message.undecryptable_reason = match incoming {
                    IncomingEvent::Undecrypted { reason, .. } => Some(reason.clone()),
                    _ => None,
                };
                (message, incoming.cleartext().and_then(relation_of))
            })
            .collect(),
    )
}

/// 從事件的 JSON（sync 或 messages 回來的原樣）組 `Message`。conversation 由呼叫者填（sync 的事件沒有 room_id）。
///
/// Args:
///     raw: 一則事件的 JSON, example: {"type":"m.room.message","event_id":"$a","sender":"@a:x","origin_server_ts":1,"content":{"msgtype":"m.text","body":"hi"}}
/// Return:
///     Message  認不得的 type 是 `Unsupported`，不丟
pub fn message_from_json(raw: &serde_json::Value) -> Message {
    let text = |key: &str| {
        raw.get(key)
            .and_then(|value| value.as_str())
            .map(str::to_string)
    };
    let event_type = text("type").unwrap_or_else(|| "unknown".into());
    let content = raw
        .get("content")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let unsigned = raw.get("unsigned");
    let redacted = unsigned
        .and_then(|unsigned| unsigned.get("redacted_because"))
        .is_some();
    let (r_seq, g_seq) = event_seqs(raw);

    let reply_to = content
        .get("m.relates_to")
        .and_then(|relates| relates.get("m.in_reply_to"))
        .and_then(|reply| reply.get("event_id"))
        .and_then(|id| id.as_str())
        .map(str::to_string);

    let kind = if redacted {
        MessageKind::Deleted {
            reason: unsigned
                .and_then(|unsigned| unsigned.get("redacted_because"))
                .and_then(|because| because.get("content"))
                .and_then(|content| content.get("reason"))
                .and_then(|reason| reason.as_str())
                .map(str::to_string),
        }
    } else {
        kind_from_content(&event_type, &content, raw)
    };

    Message {
        id: text("event_id").unwrap_or_else(|| "unknown".into()),
        conversation: text("room_id").unwrap_or_else(|| "unknown".into()),
        sender: text("sender").unwrap_or_else(|| "unknown".into()),
        sent_at: raw
            .get("origin_server_ts")
            .and_then(|ts| ts.as_u64())
            .unwrap_or(0),
        kind,
        reply_to,
        edited_by: None,
        reactions: Vec::new(),
        // 原始 JSON 裡的 `m.room.encrypted` 就是還沒解的加密事件：`Some(false)` 帶原因，不是 None（None 是「本來就不是加密事件」，
        // chat-model §2.3）。matrix-sdk 的 adapter 解得開會覆蓋成 `Some(true)`。
        decrypted: if event_type == "m.room.encrypted" {
            Some(false)
        } else {
            None
        },
        undecryptable_reason: if event_type == "m.room.encrypted" {
            Some("NotDecryptedHere".into())
        } else {
            None
        },
        r_seq,
        g_seq,
    }
}

pub(crate) fn kind_from_content(
    event_type: &str,
    content: &serde_json::Value,
    raw: &serde_json::Value,
) -> MessageKind {
    let body = content
        .get("body")
        .and_then(|body| body.as_str())
        .map(str::to_string);
    match event_type {
        "m.room.message" => {
            let msgtype = content
                .get("msgtype")
                .and_then(|value| value.as_str())
                .unwrap_or("");
            match msgtype {
                "m.text" | "m.notice" | "m.emote" => MessageKind::Text {
                    body: body.unwrap_or_default(),
                    formatted_html: content
                        .get("formatted_body")
                        .and_then(|value| value.as_str())
                        .map(str::to_string),
                },
                FILE_MSGTYPE => match file_attachment(content) {
                    Ok(attachment) => MessageKind::File {
                        attachment,
                        caption: content
                            .get("caption")
                            .and_then(|value| value.as_str())
                            .map(str::to_string),
                    },
                    // 約定 §5：區塊缺、v 不認得、cipher 不認得 → 當成解不開的檔，顯示 body。
                    Err(reason) => MessageKind::Unsupported {
                        event_type: format!("{event_type}/{msgtype} ({reason})"),
                        body,
                    },
                },
                other => MessageKind::Unsupported {
                    event_type: format!("{event_type}/{other}"),
                    body,
                },
            }
        }
        "m.room.encrypted" => MessageKind::Undecryptable,
        "m.room.member"
        | "m.room.name"
        | "m.room.topic"
        | "m.room.power_levels"
        | "m.room.encryption"
        | "m.room.pinned_events"
        | "m.room.create"
        | "m.room.join_rules"
        | "m.room.history_visibility"
        | "m.room.avatar"
        | "m.room.canonical_alias"
        | "m.room.guest_access" => MessageKind::System {
            event_type: event_type.to_string(),
            line: system_line(event_type, content, raw),
        },
        other => MessageKind::Unsupported {
            event_type: other.to_string(),
            body,
        },
    }
}

fn file_attachment(content: &serde_json::Value) -> Result<Attachment, String> {
    let mxc = content
        .get("url")
        .and_then(|value| value.as_str())
        .ok_or("no url")?
        .to_string();
    let block_json = content.get(CHUNKED_BLOCK_KEY).ok_or("no chunked block")?;
    let block: crate::chunk_block::ChunkedBlock =
        serde_json::from_value(block_json.clone()).map_err(|error| format!("block: {error}"))?;
    block
        .check_as_event_block()
        .map_err(|error| error.to_string())?;
    Ok(Attachment { mxc, block })
}

fn system_line(event_type: &str, content: &serde_json::Value, raw: &serde_json::Value) -> String {
    let sender = raw
        .get("sender")
        .and_then(|value| value.as_str())
        .unwrap_or("?");
    let state_key = raw
        .get("state_key")
        .and_then(|value| value.as_str())
        .unwrap_or("");
    let field = |key: &str| {
        content
            .get(key)
            .and_then(|value| value.as_str())
            .unwrap_or("")
            .to_string()
    };
    match event_type {
        "m.room.member" => format!("{state_key} {}", field("membership")),
        "m.room.name" => format!("{sender} named the room {:?}", field("name")),
        "m.room.topic" => format!("{sender} set the topic {:?}", field("topic")),
        "m.room.encryption" => format!("{sender} enabled encryption"),
        "m.room.create" => format!("{sender} created the room"),
        other => format!("{sender} changed {other}"),
    }
}

/// 一則事件指向另一則的關係：reaction、edit、redaction。從原始 JSON 讀，不進 `Message`。
pub(crate) enum Relation {
    Reaction {
        target: String,
        key: String,
    },
    Replace {
        target: String,
        new_content: serde_json::Value,
    },
    Redaction {
        target: String,
        reason: Option<String>,
    },
}

pub(crate) fn relation_of(raw: &serde_json::Value) -> Option<Relation> {
    let event_type = raw.get("type")?.as_str()?;
    let content = raw.get("content")?;
    match event_type {
        "m.reaction" => {
            let relates = content.get("m.relates_to")?;
            if relates.get("rel_type")?.as_str()? != "m.annotation" {
                return None;
            }
            Some(Relation::Reaction {
                target: relates.get("event_id")?.as_str()?.to_string(),
                key: relates.get("key")?.as_str()?.to_string(),
            })
        }
        "m.room.redaction" => {
            // room v11 起 `redacts` 在 content 裡；之前在事件頂層。兩邊都看。
            let target = content
                .get("redacts")
                .or_else(|| raw.get("redacts"))?
                .as_str()?
                .to_string();
            let reason = content
                .get("reason")
                .and_then(|value| value.as_str())
                .map(str::to_string);
            Some(Relation::Redaction { target, reason })
        }
        _ => {
            let relates = content.get("m.relates_to")?;
            if relates.get("rel_type")?.as_str()? != "m.replace" {
                return None;
            }
            Some(Relation::Replace {
                target: relates.get("event_id")?.as_str()?.to_string(),
                new_content: content.get("m.new_content")?.clone(),
            })
        }
    }
}

/// 一頁裡的關係事件折進目標：`m.reaction` 聚合成 `reactions`、`m.replace` 覆蓋內容並標 `edited_by`、redaction 標 `Deleted`。
/// 目標不在這一頁的關係事件照原樣留著（`Unsupported`），不丟。範圍只有這一頁（chat-model §3.4）。
pub(crate) fn aggregate(
    conversation: &str,
    items: Vec<(Message, Option<Relation>)>,
) -> Vec<Message> {
    let mut messages: Vec<Message> = Vec::with_capacity(items.len());
    let mut relations: Vec<(usize, Relation, String)> = Vec::new();
    for (mut message, relation) in items {
        if message.conversation == "unknown" {
            message.conversation = conversation.to_string();
        }
        let sender = message.sender.clone();
        let index = messages.len();
        messages.push(message);
        if let Some(relation) = relation {
            relations.push((index, relation, sender));
        }
    }
    let mut consumed = vec![false; messages.len()];
    for (index, relation, sender) in relations {
        let target = match &relation {
            Relation::Reaction { target, .. }
            | Relation::Replace { target, .. }
            | Relation::Redaction { target, .. } => target.clone(),
        };
        let Some(target_message) = messages.iter_mut().find(|message| message.id == target) else {
            continue;
        };
        match relation {
            Relation::Reaction { key, .. } => {
                let reactions = &mut target_message.reactions;
                match reactions.iter_mut().find(|reaction| reaction.key == key) {
                    Some(reaction) => reaction.by.push(sender),
                    None => reactions.push(Reaction {
                        key,
                        by: vec![sender],
                    }),
                }
            }
            Relation::Replace { new_content, .. } => {
                target_message.kind =
                    kind_from_content("m.room.message", &new_content, &serde_json::Value::Null);
                target_message.edited_by = Some(sender);
            }
            Relation::Redaction { reason, .. } => {
                target_message.kind = MessageKind::Deleted { reason };
            }
        }
        // `index` 是上面 push 進 `messages` 時的位置，`consumed` 跟它等長：`None` 到不了，靜默跳過是設計。
        if let Some(flag) = consumed.get_mut(index) {
            *flag = true;
        }
    }
    messages
        .into_iter()
        .zip(consumed)
        .filter(|(_, consumed)| !consumed)
        .map(|(message, _)| message)
        .collect()
}

#[cfg(test)]
mod file_message_content_tests {
    use super::*;
    use crate::chunk_block::ChunkedBlock;
    use crate::Cipher;

    fn plain_block(name: Option<&str>) -> ChunkedBlock {
        ChunkedBlock {
            v: 1,
            cipher: Cipher::None,
            key: None,
            nonce_base: None,
            chunk_size: 16,
            file_size: Some(3),
            name: name.map(str::to_string),
            mimetype: None,
            sha256: None,
        }
    }

    /// Client 那條（`Room::send`）與 wbf 那條（`Event/Send`）送的是同一份 content：這裡釘它的形狀。
    #[test]
    fn file_message_content_carries_url_block_body_and_optional_caption() {
        let attachment = Attachment {
            mxc: "mxc://localhost/1122334455667788".into(),
            block: plain_block(Some("notes.txt")),
        };
        let content = file_message_content(&attachment, Some("看這個")).unwrap();
        assert_eq!(content["msgtype"], FILE_MSGTYPE);
        assert_eq!(content["url"], "mxc://localhost/1122334455667788");
        assert_eq!(content["caption"], "看這個");
        assert_eq!(
            content["body"],
            "看這個\nnotes.txt（WBF 分塊檔，需要 WBF client 才能開）"
        );
        assert_eq!(content[CHUNKED_BLOCK_KEY]["cipher"], "none");
        assert_eq!(content[CHUNKED_BLOCK_KEY]["file_size"], 3);
        // 沒 caption 就沒有那個欄位；沒名字用 "file"。
        let content = file_message_content(
            &Attachment {
                mxc: "mxc://localhost/1".into(),
                block: plain_block(None),
            },
            None,
        )
        .unwrap();
        assert!(content.get("caption").is_none());
        assert_eq!(
            content["body"],
            "file（WBF 分塊檔，需要 WBF client 才能開）"
        );
        // 進不了事件的區塊（明文模式帶 key）→ Usage，🚫 不送出去。
        let mut bad = plain_block(None);
        bad.key = Some([7u8; 32]);
        assert!(file_message_content(
            &Attachment {
                mxc: "mxc://localhost/1".into(),
                block: bad,
            },
            None,
        )
        .is_err());
    }
}

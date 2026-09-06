//! `ChatBackend` 的第一個實作：包上游 `matrix-sdk`。**這是整個 crate 唯一 `use matrix_sdk` 的檔**（plan-v1 §7.2）。
//!
//! 對上游的依賴，逐條列（每個 PR 要寫的）：
//! - `Client`（builder、sqlite store、`restore_session`、`sync_once`、`joined_rooms`）
//! - `MatrixAuth::login_username`（登入拿 device 與 token）
//! - `Room`（`messages`、`send`、`is_direct`、`power_levels`、`display_name`、`is_encrypted`、成員數）
//! - `TimelineEvent`（解密結果：解得開就給明文事件，解不開給原事件加原因）
//! - ruma 的 `RoomMessageEventContent`／`MessageType::new`（組 `m.room.message`）
//!
//! 加密在這一版**完全在 matrix-sdk 裡**（`Room::send` 自己 Megolm、`TimelineEvent` 自己解）：我們沒有直接碰 `OlmMachine`，
//! 所以 plan-v1 §7.2 的 `RoomCrypto` trait 這一版還沒有東西可包；接管送訊息（附件宣告需要，見 `send_file`）那一版才會出現。

use std::path::Path;
use std::time::{Duration, Instant};

use matrix_sdk::authentication::matrix::MatrixSession;
use matrix_sdk::authentication::SessionTokens;
use matrix_sdk::config::{RequestConfig, SyncSettings};
use matrix_sdk::deserialized_responses::{TimelineEvent, TimelineEventKind};
use matrix_sdk::room::MessagesOptions;
use matrix_sdk::ruma::events::room::message::{MessageType, RoomMessageEventContent};
use matrix_sdk::ruma::events::room::power_levels::UserPowerLevel;
use matrix_sdk::ruma::events::MessageLikeEventType;
use matrix_sdk::ruma::{OwnedRoomId, RoomId, UInt, UserId};
use matrix_sdk::SqliteStoreConfig;
use matrix_sdk::{Client, Room, SessionMeta};

use crate::chat::{
    Attachment, ChatBackend, Conversation, ConversationKind, Message, MessageKind, Page, Reaction,
    Update, WatchControl, WatchEnd,
};
use crate::error::SdkError;
use crate::login::Session;
use crate::protocol::event_seqs;
use crate::vault::Key32;

/// 約定 §5 的 msgtype 與區塊 key。
pub const FILE_MSGTYPE: &str = "org.wbftw.wbfuwunel.file";
pub const CHUNKED_BLOCK_KEY: &str = "org.wbftw.wbfuwunel.chunked";

/// 每次 `/sync` 最多等多久（server 端長輪詢）。
const SYNC_POLL: Duration = Duration::from_secs(30);

pub struct MatrixBackend {
    client: Client,
    me: String,
}

impl MatrixBackend {
    /// 登入：拿到裝置與 token，store 落在 `store_dir`（crypto 與 state 兩個 sqlite，plan-v1 §7.1 說的「非存不可」）。
    /// store 用 `store_key` 包住它自己的 `StoreCipher`（local-cache-db.md §5.3）：這把是 `Vault::matrix_store_key()`。
    ///
    /// Args:
    ///     server: example: "http://localhost:6167"
    ///     user: mxid 或 localpart
    ///     password: 🚫 不印、不 log
    ///     store_dir: example: "<data dir>/wbf-cli/matrix"
    ///     store_key: example: vault.matrix_store_key()
    /// Return:
    ///     Ok((MatrixBackend, Session))   Session 給呼叫者寫 session 檔
    ///     Err(Server)                    登入被拒（M_FORBIDDEN…）
    pub async fn login(
        server: &str,
        user: &str,
        password: &str,
        device_name: &str,
        store_dir: &Path,
        store_key: &Key32,
    ) -> Result<(MatrixBackend, Session), SdkError> {
        let client = build_client(server, store_dir, store_key).await?;
        let response = client
            .matrix_auth()
            .login_username(user, password)
            .initial_device_display_name(device_name)
            .send()
            .await
            .map_err(matrix_error)?;
        let session = Session {
            server: server.trim_end_matches('/').to_string(),
            user_id: response.user_id.to_string(),
            device_id: response.device_id.to_string(),
            access_token: response.access_token,
            store_dir: Some(store_dir.display().to_string()),
        };
        let me = session.user_id.clone();
        Ok((MatrixBackend { client, me }, session))
    }

    /// 用 session 檔還原：同一個裝置、同一個 store。
    pub async fn restore(
        session: &Session,
        store_dir: &Path,
        store_key: &Key32,
    ) -> Result<MatrixBackend, SdkError> {
        let client = build_client(&session.server, store_dir, store_key).await?;
        let user_id = UserId::parse(&session.user_id)
            .map_err(|error| SdkError::Usage(format!("session user_id: {error}")))?;
        client
            .restore_session(MatrixSession {
                meta: SessionMeta {
                    user_id,
                    device_id: session.device_id.clone().into(),
                },
                tokens: SessionTokens {
                    access_token: session.access_token.clone(),
                    refresh_token: None,
                },
            })
            .await
            .map_err(matrix_error)?;
        Ok(MatrixBackend {
            client,
            me: session.user_id.clone(),
        })
    }

    /// 一次 sync，把房間列表與金鑰狀態拉到 store；`conversations` 前要有一次。回下次的 `since`。
    pub async fn sync_once(&self, since: Option<&str>, poll: Duration) -> Result<String, SdkError> {
        let mut settings = SyncSettings::new().timeout(poll);
        if let Some(since) = since {
            settings = settings.token(since);
        }
        let response = self
            .client
            .sync_once(settings)
            .await
            .map_err(matrix_error)?;
        Ok(response.next_batch)
    }

    fn room(&self, id: &str) -> Result<Room, SdkError> {
        let room_id =
            RoomId::parse(id).map_err(|error| SdkError::Usage(format!("room id {id}: {error}")))?;
        self.client
            .get_room(&room_id)
            .ok_or_else(|| SdkError::Usage(format!("not in room {id} (or not synced yet)")))
    }

    async fn describe(&self, room: &Room) -> Result<Conversation, SdkError> {
        let power_levels = room.power_levels_or_default().await;
        let me = UserId::parse(&self.me).map_err(|error| SdkError::Usage(error.to_string()))?;
        // room v12 起建房者是「無限」權限；對我們就是「比任何門檻都大」，用 i64::MAX 表示。
        let my_power_level: i64 = match power_levels.for_user(&me) {
            UserPowerLevel::Infinite => i64::MAX,
            UserPowerLevel::Int(level) => i64::from(level),
            // non_exhaustive：不認得的變體當最低，fail closed（會被算成不能發）。
            _ => i64::MIN,
        };
        let needed_to_send: i64 =
            i64::from(power_levels.for_message(MessageLikeEventType::RoomMessage));
        let can_send_message = my_power_level >= needed_to_send;
        let member_count = room.joined_members_count();
        let is_direct = room.is_direct().await.unwrap_or(false);

        // chat-model §3.1：m.direct 有它且成員剛好兩個才是 Direct；§3.2：發訊息的門檻只有 owner（100）達得到才是 Channel。
        let (kind, direct_peer) = if is_direct && member_count == 2 {
            let peer = room
                .direct_targets()
                .into_iter()
                .next()
                .map(|target| target.to_string());
            (ConversationKind::Direct, peer)
        } else if needed_to_send >= 100 {
            (ConversationKind::Channel, None)
        } else {
            (ConversationKind::Group, None)
        };

        let name = match room.display_name().await {
            Ok(display) => Some(display.to_string()),
            Err(_) => room.name(),
        };
        Ok(Conversation {
            id: room.room_id().to_string(),
            kind,
            name,
            topic: room.topic(),
            encrypted: room.encryption_state().is_encrypted(),
            member_count,
            my_power_level,
            can_send_message,
            direct_peer,
        })
    }
}

impl ChatBackend for MatrixBackend {
    async fn conversations(&self) -> Result<Vec<Conversation>, SdkError> {
        let mut out = Vec::new();
        for room in self.client.joined_rooms() {
            out.push(self.describe(&room).await?);
        }
        Ok(out)
    }

    async fn conversation(&self, id: &str) -> Result<Conversation, SdkError> {
        let room = self.room(id)?;
        self.describe(&room).await
    }

    async fn history(&self, id: &str, before: Option<&str>, limit: u32) -> Result<Page, SdkError> {
        if limit == 0 {
            return Err(SdkError::Usage("history limit must be at least 1".into()));
        }
        let room = self.room(id)?;
        let mut options = MessagesOptions::backward();
        options.limit = UInt::from(limit);
        options.from = before.map(str::to_string);
        let messages = room.messages(options).await.map_err(matrix_error)?;
        let events = aggregate(id, messages.chunk.iter().map(to_message).collect());
        Ok(Page {
            events,
            next: messages.end,
        })
    }

    async fn send_text(&self, id: &str, body: &str) -> Result<String, SdkError> {
        let room = self.room(id)?;
        let response = room
            .send(RoomMessageEventContent::text_plain(body))
            .await
            .map_err(matrix_error)?;
        Ok(response.response.event_id.to_string())
    }

    /// ⚠️ 附件宣告（約定 §5.2）這一版帶不出去：`Room::send` 不能加 header，`Event/Send` 在 server 端還是提案。
    /// 所以 server 的媒體計數不會 +1，這則的附件過保護期會被清（等 server 定案；到時這裡改走 `WbfClient::send_event`，
    /// 那需要自己 Megolm 加密，也就是 `RoomCrypto` 出現的時候）。呼叫者要知道這件事，CLI 會印警告。
    async fn send_file(
        &self,
        id: &str,
        attachment: &Attachment,
        caption: Option<&str>,
    ) -> Result<String, SdkError> {
        let room = self.room(id)?;
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
        let mut data = serde_json::Map::new();
        data.insert(
            "url".into(),
            serde_json::Value::String(attachment.mxc.clone()),
        );
        data.insert(
            CHUNKED_BLOCK_KEY.into(),
            serde_json::to_value(&attachment.block).expect("ChunkedBlock serializes"),
        );
        if let Some(caption) = caption {
            data.insert(
                "caption".into(),
                serde_json::Value::String(caption.to_string()),
            );
        }
        let message_type = MessageType::new(FILE_MSGTYPE, body, data)
            .map_err(|error| SdkError::Usage(format!("file event content: {error}")))?;
        let response = room
            .send(RoomMessageEventContent::new(message_type))
            .await
            .map_err(matrix_error)?;
        Ok(response.response.event_id.to_string())
    }

    async fn watch(
        &self,
        since: Option<&str>,
        deadline: Option<Duration>,
        on_update: &mut dyn FnMut(Update) -> WatchControl,
    ) -> Result<WatchEnd, SdkError> {
        // 沒給 since 就先對齊到「現在」：timeout 0 的一次 sync，事件全部丟掉。
        let mut token = match since {
            Some(since) => since.to_string(),
            None => self.sync_once(None, Duration::ZERO).await?,
        };
        let started = Instant::now();
        loop {
            let poll = match deadline {
                Some(deadline) => {
                    let remaining = deadline.saturating_sub(started.elapsed());
                    if remaining.is_zero() {
                        return Ok(WatchEnd {
                            since: token,
                            stopped_by_callback: false,
                        });
                    }
                    remaining.min(SYNC_POLL)
                }
                None => SYNC_POLL,
            };
            let settings = SyncSettings::new().timeout(poll).token(token.clone());
            let response = self
                .client
                .sync_once(settings)
                .await
                .map_err(matrix_error)?;
            token = response.next_batch;

            let mut control = WatchControl::Continue;
            for (room_id, update) in &response.rooms.joined {
                let room_id: &OwnedRoomId = room_id;
                let messages = aggregate(
                    room_id.as_str(),
                    update.timeline.events.iter().map(to_message).collect(),
                );
                for message in messages {
                    if on_update(Update::NewMessage(Box::new(message))) == WatchControl::Stop {
                        control = WatchControl::Stop;
                        break;
                    }
                }
                if control == WatchControl::Stop {
                    break;
                }
            }
            if control == WatchControl::Continue {
                for room_id in response.rooms.left.keys() {
                    if on_update(Update::ConversationLeft {
                        id: room_id.to_string(),
                    }) == WatchControl::Stop
                    {
                        control = WatchControl::Stop;
                        break;
                    }
                }
            }
            if control == WatchControl::Stop {
                return Ok(WatchEnd {
                    since: token,
                    stopped_by_callback: true,
                });
            }
        }
    }
}

async fn build_client(
    server: &str,
    store_dir: &Path,
    store_key: &Key32,
) -> Result<Client, SdkError> {
    std::fs::create_dir_all(store_dir)?;
    // 與 channel::REQUEST_TIMEOUT 同一個數：server 黑洞了就回錯，不讓 CLI 掛死（PR #9 審查 rumia 🟢3）。
    // `key(...)` 走 `StoreCipher::open_with_key`：沒有 PBKDF2，密碼那一層在 Vault 做過了（local-cache-db.md §5.3）。
    let store_config = SqliteStoreConfig::new(store_dir).key(Some(store_key.as_bytes()));
    Client::builder()
        .homeserver_url(server)
        .request_config(RequestConfig::new().timeout(crate::channel::REQUEST_TIMEOUT))
        .sqlite_store_with_config_and_cache_path(store_config, None::<&Path>)
        .build()
        .await
        .map_err(|error| match error {
            // 開不了 store 多半是既有的 store 不是這把金鑰包的（舊版沒有金鑰、或 local.key 換過）。
            // store 只是「非存不可」的裝置狀態，刪掉重新 login 就好；不做遷移（local-cache-db.md §1）。
            matrix_sdk::ClientBuildError::SqliteStore(error) => SdkError::Usage(format!(
                "cannot open the matrix store at {}: {error}; it was made with another key file — delete that directory and run `login` again",
                store_dir.display()
            )),
            other => SdkError::Network(format!("matrix client: {other}")),
        })
}

/// matrix-sdk 的錯誤分類到我們的：server 回了 Matrix 的 `errcode`（M_FORBIDDEN…）→ `Server`（code 就是 errcode），其他 → `Network`。
/// 用型別化的入口，不 parse Display 字串（PR #9 審查 rumia 🟡1：Display 是 `[403 / M_FORBIDDEN] …`，字串抓不到）。
fn matrix_error(error: matrix_sdk::Error) -> SdkError {
    if let Some(kind) = error.client_api_error_kind() {
        let status = error
            .as_client_api_error()
            .map(|api| api.status_code.as_u16())
            .unwrap_or(0);
        return SdkError::Server {
            code: kind.errcode().to_string(),
            message: error.to_string(),
            meta: serde_json::json!({ "status": status }),
        };
    }
    SdkError::Network(format!("matrix: {error}"))
}

// ---- 事件 → Message ----

/// `TimelineEvent` 是 matrix-sdk 解密過的結果：解得開就是明文事件，解不開是原事件加原因。這裡把它變成我們的 `Message`。
fn to_message(event: &TimelineEvent) -> (Message, Option<Relation>) {
    let (raw, decrypted, reason) = match &event.kind {
        TimelineEventKind::Decrypted(decrypted) => (
            serde_json::to_value(&decrypted.event).unwrap_or(serde_json::Value::Null),
            Some(true),
            None,
        ),
        TimelineEventKind::UnableToDecrypt { event, utd_info } => (
            serde_json::to_value(event).unwrap_or(serde_json::Value::Null),
            Some(false),
            Some(format!("{:?}", utd_info.reason)),
        ),
        TimelineEventKind::PlainText { event } => (
            serde_json::to_value(event).unwrap_or(serde_json::Value::Null),
            None,
            None,
        ),
    };
    let relation = relation_of(&raw);
    let mut message = message_from_json(&raw);
    message.decrypted = decrypted;
    message.undecryptable_reason = reason;
    (message, relation)
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
        decrypted: None,
        undecryptable_reason: None,
        r_seq,
        g_seq,
    }
}

fn kind_from_content(
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
        "m.room.encrypted" => MessageKind::Unsupported {
            event_type: event_type.to_string(),
            body: None,
        },
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
enum Relation {
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

fn relation_of(raw: &serde_json::Value) -> Option<Relation> {
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
fn aggregate(conversation: &str, items: Vec<(Message, Option<Relation>)>) -> Vec<Message> {
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
        let Some(target_index) = messages.iter().position(|message| message.id == target) else {
            continue;
        };
        match relation {
            Relation::Reaction { key, .. } => {
                let reactions = &mut messages[target_index].reactions;
                match reactions.iter_mut().find(|reaction| reaction.key == key) {
                    Some(reaction) => reaction.by.push(sender),
                    None => reactions.push(Reaction {
                        key,
                        by: vec![sender],
                    }),
                }
            }
            Relation::Replace { new_content, .. } => {
                let target_message = &mut messages[target_index];
                target_message.kind =
                    kind_from_content("m.room.message", &new_content, &serde_json::Value::Null);
                target_message.edited_by = Some(sender);
            }
            Relation::Redaction { reason, .. } => {
                messages[target_index].kind = MessageKind::Deleted { reason };
            }
        }
        consumed[index] = true;
    }
    messages
        .into_iter()
        .zip(consumed)
        .filter(|(_, consumed)| !consumed)
        .map(|(message, _)| message)
        .collect()
}

//! `cache.db`：本地快取（local-cache-db.md §1、§3、§6）。SQLCipher 整檔加密，raw key 是 `Vault::cache_key()`。
//!
//! **一個 server 一個檔、多帳號混存**（維護者 2026-09-07 定）：事件只存一份；誰看得到哪一則由 `events_synced_log` 逐則記
//! （server 經任一條路給過這個 user 的才算），沒有列就看不到——fail closed，不用 r_seq 下界去猜可見性。
//! **快取不是權威**（§1）：server 不符、schema 版本不對、解不開，一律刪檔重建，不寫遷移；讀到壞資料當成沒有快取。
//!
//! schema 風格：實體表 `INTEGER PRIMARY KEY` 加識別碼的 UNIQUE 索引；關聯表整數複合主鍵 `WITHOUT ROWID`；
//! 字串識別碼（mxid、room_id、event_id）各只存一次，其餘全走整數外鍵。整數 id 不出這個檔。
//! 這裡只有 SQL 與我們的聊天模型（`Message`、`Conversation`），沒有 matrix-sdk、沒有網路。
//! 🚫 金鑰不進錯誤訊息、不 log。

use std::path::{Path, PathBuf};

use rusqlite::{params, Connection, OptionalExtension, Transaction};

use crate::chat::{Conversation, Message, MessageKind};
use crate::error::SdkError;
use crate::vault::Key32;

pub const CACHE_FILE_NAME: &str = "cache.db";
/// 換 schema 就加一，舊檔整個重建（§1）。
const SCHEMA_VERSION: i64 = 3;

/// `events.kind` 的整數碼。順序是格式的一部分，只能往後加。
const KIND_TEXT: i64 = 0;
const KIND_FILE: i64 = 1;
const KIND_DELETED: i64 = 2;
const KIND_SYSTEM: i64 = 3;
const KIND_UNSUPPORTED: i64 = 4;

/// `message_json` 不存這些鍵：它們各有自己的欄位，讀出時組回去（`split_message`／`join_message`）。
const COLUMN_BACKED_KEYS: [&str; 7] = [
    "id",
    "conversation",
    "sender",
    "sent_at",
    "r_seq",
    "g_seq",
    "decrypted",
];

/// 快取屬於哪個 server；不符就不是這份快取（§6 `meta`）。帳號不在身份裡：同一個 server 的帳號共用。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CacheIdentity {
    pub server: String,
}

/// 開檔時做了什麼，給呼叫者印在 stderr（CLI）或 log（UI）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpenOutcome {
    /// 既有的檔、server 與版本都對。
    Reused,
    /// 沒有檔，新建。
    Created,
    /// 有檔但解不開／server 不符／版本不對，刪掉重建。
    Rebuilt,
}

pub struct Cache {
    connection: Connection,
    path: PathBuf,
}

/// 本地閱讀位置（§6 `read_positions`）：`event_id` 是權威，`r_seq` 給算術用。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReadPosition {
    pub event_id: String,
    pub r_seq: Option<i64>,
    pub ts: i64,
}

/// `media` 的一列（§6）。池與下載管線還沒有，這一版只有指針與時間。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MediaEntry {
    pub mxc: String,
    /// 明文的 BLAKE3 hex；None = 還沒下載完。
    pub pool_file: Option<String>,
    pub name: Option<String>,
    pub mimetype: Option<String>,
    /// 明文的校驗碼，`<algo>:<hex>`：事件區塊有帶就是 `sha256:…`（上傳者算的），沒帶就下載完填 `blake3:…`（我們算的，同 `pool_file`）。
    /// 還沒下載完而區塊也沒帶時是 None。
    pub hash: Option<String>,
    pub file_size: u64,
    pub chunk_size: u32,
    pub chunks_written: u64,
    pub complete: bool,
    pub bytes_on_disk: u64,
    pub created_at: i64,
    pub last_used_at: i64,
}

/// `forget_account` 的結果：清了什麼、哪些池檔已經沒人指、可以刪。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ForgetReport {
    pub events_removed: u64,
    pub media_removed: u64,
    /// 只列已經沒有任何 `media` 列指著的檔（同 hash 去重過的檔可能還被別的 mxc 用）。呼叫者拿去刪池裡的檔，DB 先、檔案後。
    pub orphan_pool_files: Vec<String>,
}

impl Cache {
    /// Args:
    ///     dir: example: "<data dir>/s/<b58 nonce>_<b58 密文>"
    ///     key: example: vault.cache_key()
    ///     identity: example: CacheIdentity { server: "http://localhost:6167".into() }
    /// Return:
    ///     Ok((Cache, OpenOutcome))
    ///     Err(Usage)   這個 build 沒有 SQLCipher（PRAGMA cipher_version 是空的）：拒絕，不寫明文快取
    ///     Err(Io)      目錄建不起來、檔刪不掉
    pub fn open(
        dir: &Path,
        key: &Key32,
        identity: &CacheIdentity,
    ) -> Result<(Cache, OpenOutcome), SdkError> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join(CACHE_FILE_NAME);
        let existed = path.exists();
        if existed {
            if let Some(cache) = try_open_existing(&path, key, identity)? {
                return Ok((cache, OpenOutcome::Reused));
            }
            remove_database_files(&path)?;
        }
        let connection = open_with_key(&path, key)?;
        create_schema(&connection, identity)?;
        Ok((
            Cache { connection, path },
            if existed {
                OpenOutcome::Rebuilt
            } else {
                OpenOutcome::Created
            },
        ))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    // ---- sync_state ----

    /// 這個帳號快取裡最新的全域序號（水位線），餵 `Event/Recent` 的 `cg_seq`。
    ///
    /// Return:
    ///     Ok(Some(i64))   有
    ///     Ok(None)        這個帳號還沒同步過
    pub fn get_cg_seq(&self, user_id: &str) -> Result<Option<i64>, SdkError> {
        self.connection
            .query_row(
                "SELECT s.cg_seq FROM sync_state s JOIN users u ON u.id = s.user WHERE u.mxid = ?1",
                params![user_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(db_error)
    }

    pub fn set_cg_seq(&mut self, user_id: &str, cg_seq: i64) -> Result<(), SdkError> {
        let transaction = self.connection.transaction().map_err(db_error)?;
        let user = user_row_id(&transaction, user_id)?;
        transaction
            .execute(
                "INSERT INTO sync_state (user, cg_seq, updated_at) VALUES (?1, ?2, ?3)
                 ON CONFLICT(user) DO UPDATE SET cg_seq = excluded.cg_seq, updated_at = excluded.updated_at",
                params![user, cg_seq, now_millis()],
            )
            .map_err(db_error)?;
        transaction.commit().map_err(db_error)
    }

    // ---- room_list ----

    /// 這個帳號的房間清單（一人一列；`Conversation` 是他看到的樣子）。
    pub fn upsert_conversations(
        &mut self,
        user_id: &str,
        conversations: &[Conversation],
    ) -> Result<(), SdkError> {
        let transaction = self.connection.transaction().map_err(db_error)?;
        let user = user_row_id(&transaction, user_id)?;
        let now = now_millis();
        {
            let mut insert = transaction
                .prepare_cached(
                    "INSERT INTO room_list (room, user, conversation_json, refreshed_at) VALUES (?1, ?2, ?3, ?4)
                     ON CONFLICT(user, room) DO UPDATE SET conversation_json = excluded.conversation_json,
                       refreshed_at = excluded.refreshed_at",
                )
                .map_err(db_error)?;
            for conversation in conversations {
                let room = room_row_id(&transaction, &conversation.id)?;
                insert
                    .execute(params![
                        room,
                        user,
                        serde_json::to_string(conversation).expect("Conversation serializes"),
                        now,
                    ])
                    .map_err(db_error)?;
            }
        }
        transaction.commit().map_err(db_error)
    }

    /// Return:
    ///     Ok(Vec<Conversation>)   這個帳號的，照名稱排；解不開的列跳過（§1：壞資料當沒有）
    pub fn list_conversations(&self, user_id: &str) -> Result<Vec<Conversation>, SdkError> {
        let mut statement = self
            .connection
            .prepare_cached(
                "SELECT l.conversation_json FROM room_list l JOIN users u ON u.id = l.user WHERE u.mxid = ?1",
            )
            .map_err(db_error)?;
        let rows = statement
            .query_map(params![user_id], |row| row.get::<_, String>(0))
            .map_err(db_error)?;
        let mut conversations: Vec<Conversation> = Vec::new();
        for row in rows {
            let json = row.map_err(db_error)?;
            if let Ok(conversation) = serde_json::from_str(&json) {
                conversations.push(conversation);
            }
        }
        conversations
            .sort_by(|left, right| left.name.cmp(&right.name).then(left.id.cmp(&right.id)));
        Ok(conversations)
    }

    // ---- events ----

    /// 這個帳號從 server 拿到的一批事件。事件本身一份（同一則再寫就覆蓋，server 說的算，§1）；
    /// 每則替 `user_id` 記一列 `events_synced_log`，已有就只更新 `last_synced_at`，`hidden` 不動。
    /// 唯一不覆蓋的情況：快取裡已經是解開的（`decrypted = 1`）、來的是還沒解的（`decrypted = 0`）——明文不會被密文蓋回去。
    /// file 事件順手建 `media`（只填事件區塊裡有的欄，已有就不動）與 `event_media`。
    ///
    /// Return:
    ///     Ok(usize)   處理的則數
    pub fn upsert_messages(
        &mut self,
        user_id: &str,
        messages: &[Message],
    ) -> Result<usize, SdkError> {
        let transaction = self.connection.transaction().map_err(db_error)?;
        let reader = user_row_id(&transaction, user_id)?;
        let now = now_millis();
        {
            let mut upsert_event = transaction
                .prepare_cached(
                    "INSERT INTO events (room, event_id, sender, r_seq, g_seq, origin_server_ts, kind, decrypted, message_json)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                     ON CONFLICT(room, event_id) DO UPDATE SET sender = excluded.sender, r_seq = excluded.r_seq,
                       g_seq = excluded.g_seq, origin_server_ts = excluded.origin_server_ts, kind = excluded.kind,
                       decrypted = excluded.decrypted, message_json = excluded.message_json
                     WHERE NOT (events.decrypted IS 1 AND excluded.decrypted IS 0)",
                    // ↑ 用 null-safe 的 `IS`（PR #13 審查 salvia／cirno）：decrypted 是 NULL（本來就不是加密事件）時
                    //   `NULL = 1` 是 NULL，整個 WHERE 變 NULL，NULL→NULL 的覆蓋會被靜默跳掉；`NULL IS 1` 是 FALSE，就不會。
                    //   語意就是「只有『明文被密文蓋』這一種要擋」，其他一律覆蓋。有測試 null_decrypted_rows_still_get_overwritten。
                )
                .map_err(db_error)?;
            let mut event_row_id = transaction
                .prepare_cached("SELECT id FROM events WHERE room = ?1 AND event_id = ?2")
                .map_err(db_error)?;
            let mut log = transaction
                .prepare_cached(
                    "INSERT INTO events_synced_log (event, user, first_synced_at, last_synced_at, hidden) VALUES (?1, ?2, ?3, ?3, 0)
                     ON CONFLICT(event, user) DO UPDATE SET last_synced_at = excluded.last_synced_at",
                )
                .map_err(db_error)?;
            let mut link_media = transaction
                .prepare_cached("INSERT OR IGNORE INTO event_media (event, media) VALUES (?1, ?2)")
                .map_err(db_error)?;
            for message in messages {
                let room = room_row_id(&transaction, &message.conversation)?;
                let sender = user_row_id(&transaction, &message.sender)?;
                upsert_event
                    .execute(params![
                        room,
                        message.id,
                        sender,
                        message.r_seq,
                        message.g_seq,
                        message.sent_at as i64,
                        kind_code(&message.kind),
                        message.decrypted.map(|flag| flag as i64),
                        split_message(message),
                    ])
                    .map_err(db_error)?;
                // upsert 在「不蓋明文」時不回列，所以 id 另外查，不靠 RETURNING。
                let event: i64 = event_row_id
                    .query_row(params![room, message.id], |row| row.get(0))
                    .map_err(db_error)?;
                log.execute(params![event, reader, now]).map_err(db_error)?;
                if let MessageKind::File { attachment, .. } = &message.kind {
                    let media = media_row_id(
                        &transaction,
                        &attachment.mxc,
                        attachment.block.name.as_deref(),
                        attachment.block.mimetype.as_deref(),
                        block_hash(attachment.block.sha256.as_deref()).as_deref(),
                        attachment.block.file_size.unwrap_or(0),
                        attachment.block.chunk_size,
                    )?;
                    link_media
                        .execute(params![event, media])
                        .map_err(db_error)?;
                }
            }
        }
        transaction.commit().map_err(db_error)?;
        Ok(messages.len())
    }

    /// 歷史，從最新往回（CLI 規格 §3.4.1 的 `read`，只是來源是快取）。只回這個帳號同步過、而且沒藏的。
    /// 有 `r_seq` 的房間照 `r_seq` 排；沒有的退到 `origin_server_ts`（chat-model §4.3 的退化表）。
    ///
    /// Args:
    ///     user_id: example: "@alice:localhost"
    ///     room_id: example: "!abc:localhost"
    ///     before_r_seq: example: Some(120)  只要 r_seq 小於它的；None 從最新開始
    ///     limit: example: 50
    /// Return:
    ///     Ok(Vec<Message>)   新到舊；解不開的列跳過
    pub fn history(
        &self,
        user_id: &str,
        room_id: &str,
        before_r_seq: Option<i64>,
        limit: u32,
    ) -> Result<Vec<Message>, SdkError> {
        self.select_messages(user_id, room_id, before_r_seq, limit, None)
    }

    /// 只要分塊檔事件（`files` 命令）。
    pub fn files(
        &self,
        user_id: &str,
        room_id: &str,
        before_r_seq: Option<i64>,
        limit: u32,
    ) -> Result<Vec<Message>, SdkError> {
        self.select_messages(user_id, room_id, before_r_seq, limit, Some(KIND_FILE))
    }

    fn select_messages(
        &self,
        user_id: &str,
        room_id: &str,
        before_r_seq: Option<i64>,
        limit: u32,
        kind: Option<i64>,
    ) -> Result<Vec<Message>, SdkError> {
        let mut statement = self
            .connection
            .prepare_cached(
                "SELECT e.event_id, r.room_id, s.mxid, e.origin_server_ts, e.r_seq, e.g_seq, e.decrypted, e.message_json
                 FROM events e
                 JOIN events_synced_log l ON l.event = e.id
                 JOIN users reader ON reader.id = l.user
                 JOIN users s ON s.id = e.sender
                 JOIN rooms r ON r.id = e.room
                 WHERE reader.mxid = ?1 AND r.room_id = ?2 AND l.hidden = 0
                   AND (?3 IS NULL OR (e.r_seq IS NOT NULL AND e.r_seq < ?3))
                   AND (?4 IS NULL OR e.kind = ?4)
                 ORDER BY e.r_seq IS NULL, e.r_seq DESC, e.origin_server_ts DESC
                 LIMIT ?5",
            )
            .map_err(db_error)?;
        let rows = statement
            .query_map(
                params![user_id, room_id, before_r_seq, kind, limit],
                |row| {
                    Ok(EventRow {
                        event_id: row.get(0)?,
                        room_id: row.get(1)?,
                        sender: row.get(2)?,
                        origin_server_ts: row.get(3)?,
                        r_seq: row.get(4)?,
                        g_seq: row.get(5)?,
                        decrypted: row.get(6)?,
                        message_json: row.get(7)?,
                    })
                },
            )
            .map_err(db_error)?;
        let mut messages = Vec::new();
        for row in rows {
            if let Some(message) = join_message(&row.map_err(db_error)?) {
                messages.push(message);
            }
        }
        Ok(messages)
    }

    /// 這個房間快取裡有幾則、最大 r_seq 是多少（不分帳號；判洞與顯示用）。
    ///
    /// Return:
    ///     Ok((count, max_r_seq))   沒有 r_seq 的房間 max 是 None
    pub fn room_stats(&self, room_id: &str) -> Result<(u64, Option<i64>), SdkError> {
        self.connection
            .query_row(
                "SELECT COUNT(*), MAX(e.r_seq) FROM events e JOIN rooms r ON r.id = e.room WHERE r.room_id = ?1",
                params![room_id],
                |row| Ok((row.get::<_, i64>(0)? as u64, row.get::<_, Option<i64>>(1)?)),
            )
            .map_err(db_error)
    }

    // ---- read position、delete for me ----

    /// 已讀位置指向快取裡的事件列；那則還沒進快取就 `Err(Usage)`（先同步再標）。
    pub fn set_read_position(
        &mut self,
        user_id: &str,
        room_id: &str,
        event_id: &str,
        ts: i64,
    ) -> Result<(), SdkError> {
        let transaction = self.connection.transaction().map_err(db_error)?;
        let user = user_row_id(&transaction, user_id)?;
        let room = room_row_id(&transaction, room_id)?;
        let event = find_event_row_id(&transaction, room, event_id)?.ok_or_else(|| {
            SdkError::Usage(format!(
                "{event_id} is not in the cache; sync it before marking it read"
            ))
        })?;
        transaction
            .execute(
                "INSERT INTO read_positions (room, user, event, ts) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(user, room) DO UPDATE SET event = excluded.event, ts = excluded.ts",
                params![room, user, event, ts],
            )
            .map_err(db_error)?;
        transaction.commit().map_err(db_error)
    }

    pub fn get_read_position(
        &self,
        user_id: &str,
        room_id: &str,
    ) -> Result<Option<ReadPosition>, SdkError> {
        self.connection
            .query_row(
                "SELECT e.event_id, e.r_seq, p.ts FROM read_positions p
                 JOIN users u ON u.id = p.user JOIN rooms r ON r.id = p.room JOIN events e ON e.id = p.event
                 WHERE u.mxid = ?1 AND r.room_id = ?2",
                params![user_id, room_id],
                |row| {
                    Ok(ReadPosition {
                        event_id: row.get(0)?,
                        r_seq: row.get(1)?,
                        ts: row.get(2)?,
                    })
                },
            )
            .optional()
            .map_err(db_error)
    }

    /// Delete for me（chat-model §5）：只對這個帳號藏；再同步同一則也不會跑回來（`hidden` 不被 sync 動）。
    /// 那則不在這個帳號的同步紀錄裡就什麼都不做（沒看過的東西沒有可藏的）。
    ///
    /// Return:
    ///     Ok(bool)   true = 真的藏了一列
    pub fn hide_message(
        &mut self,
        user_id: &str,
        room_id: &str,
        event_id: &str,
    ) -> Result<bool, SdkError> {
        let changed = self
            .connection
            .execute(
                "UPDATE events_synced_log SET hidden = 1
                 WHERE user = (SELECT id FROM users WHERE mxid = ?1)
                   AND event = (SELECT e.id FROM events e JOIN rooms r ON r.id = e.room WHERE r.room_id = ?2 AND e.event_id = ?3)",
                params![user_id, room_id, event_id],
            )
            .map_err(db_error)?;
        Ok(changed > 0)
    }

    // ---- media ----

    pub fn find_media(&self, mxc: &str) -> Result<Option<MediaEntry>, SdkError> {
        self.connection
            .query_row(
                "SELECT mxc, pool_file, name, mimetype, hash, file_size, chunk_size, chunks_written, complete, bytes_on_disk, created_at, last_used_at
                 FROM media WHERE mxc = ?1",
                params![mxc],
                media_entry_from_row,
            )
            .optional()
            .map_err(db_error)
    }

    /// 下載前叫：沒有這個 mxc 的列就建（`download --manifest` 可能沒有對應的事件），有就原樣回。
    ///
    /// Return:
    ///     Ok(MediaEntry)   `complete` 是 true 就不用下載了
    pub fn media_begin(
        &mut self,
        mxc: &str,
        name: Option<&str>,
        mimetype: Option<&str>,
        block_sha256_hex: Option<&str>,
        file_size: u64,
        chunk_size: u32,
    ) -> Result<MediaEntry, SdkError> {
        let transaction = self.connection.transaction().map_err(db_error)?;
        media_row_id(
            &transaction,
            mxc,
            name,
            mimetype,
            block_hash(block_sha256_hex).as_deref(),
            file_size,
            chunk_size,
        )?;
        transaction.commit().map_err(db_error)?;
        self.find_media(mxc)?
            .ok_or_else(|| SdkError::Io(std::io::Error::other("media row vanished after insert")))
    }

    /// `media.id`：池裡暫存檔的名字（§8.2）。
    pub fn media_pending_name(&self, mxc: &str) -> Result<Option<String>, SdkError> {
        self.connection
            .query_row("SELECT id FROM media WHERE mxc = ?1", params![mxc], |row| {
                row.get::<_, i64>(0)
            })
            .optional()
            .map_err(db_error)
            .map(|id| id.map(|id| format!("m{id}")))
    }

    /// 進度快照（§8.3：記憶體每 1–2 秒 flush 一次）。`chunk_size` 也一起寫：續傳截檔用的是下載時的塊大小。
    pub fn media_progress(
        &mut self,
        mxc: &str,
        chunks_written: u64,
        chunk_size: u32,
    ) -> Result<(), SdkError> {
        self.connection
            .execute(
                "UPDATE media SET chunks_written = ?2, chunk_size = ?3, complete = 0 WHERE mxc = ?1",
                params![mxc, chunks_written as i64, chunk_size as i64],
            )
            .map_err(db_error)?;
        Ok(())
    }

    /// 下載完、檔已 adopt 進池：寫齊 `pool_file`、`complete`、`file_size`（下載到的長度是事實，蓋掉區塊說的）、`bytes_on_disk`。
    pub fn media_finish(
        &mut self,
        mxc: &str,
        pool_file: &str,
        chunks_written: u64,
        file_size: u64,
        bytes_on_disk: u64,
    ) -> Result<(), SdkError> {
        let now = now_millis();
        self.connection
            .execute(
                "UPDATE media SET pool_file = ?2, complete = 1, chunks_written = ?3, file_size = ?4, bytes_on_disk = ?5, last_used_at = ?6,
                   hash = COALESCE(hash, 'blake3:' || ?2) WHERE mxc = ?1",
                params![mxc, pool_file, chunks_written as i64, file_size as i64, bytes_on_disk as i64, now],
            )
            .map_err(db_error)?;
        Ok(())
    }

    /// 這個檔的本地副本不算數了（壞掉、被刪）：回到「還沒下載」。列留著（事件還指著它）。
    pub fn media_reset(&mut self, mxc: &str) -> Result<(), SdkError> {
        self.connection
            .execute(
                "UPDATE media SET pool_file = NULL, complete = 0, chunks_written = 0, bytes_on_disk = 0 WHERE mxc = ?1",
                params![mxc],
            )
            .map_err(db_error)?;
        Ok(())
    }

    /// 還有幾個 `media` 列指著這個池檔（去重過的檔刪之前要問）。
    pub fn media_references(&self, pool_file: &str) -> Result<u64, SdkError> {
        self.connection
            .query_row(
                "SELECT COUNT(*) FROM media WHERE pool_file = ?1",
                params![pool_file],
                |row| row.get::<_, i64>(0),
            )
            .map(|count| count as u64)
            .map_err(db_error)
    }

    /// 配額用：完成檔的 `bytes_on_disk` 加總（同一個池檔被多個 mxc 指著只算一次）。
    pub fn media_bytes_on_disk(&self) -> Result<u64, SdkError> {
        self.connection
            .query_row(
                "SELECT COALESCE(SUM(bytes), 0) FROM (SELECT MAX(bytes_on_disk) AS bytes FROM media WHERE complete = 1 GROUP BY pool_file)",
                [],
                |row| row.get::<_, i64>(0),
            )
            .map(|sum| sum as u64)
            .map_err(db_error)
    }

    /// 完成檔，照 `last_used_at` 由舊到新（LRU 清理用，§8.5）。
    pub fn list_media_by_last_used(&self) -> Result<Vec<MediaEntry>, SdkError> {
        let mut statement = self
            .connection
            .prepare_cached(
                "SELECT mxc, pool_file, name, mimetype, hash, file_size, chunk_size, chunks_written, complete, bytes_on_disk, created_at, last_used_at
                 FROM media WHERE complete = 1 ORDER BY last_used_at ASC, mxc",
            )
            .map_err(db_error)?;
        let rows = statement
            .query_map([], media_entry_from_row)
            .map_err(db_error)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(db_error)
    }

    /// 半成品（`complete = 0` 但 `chunks_written > 0`），啟動時掃孤兒用。
    pub fn list_media_incomplete(&self) -> Result<Vec<MediaEntry>, SdkError> {
        let mut statement = self
            .connection
            .prepare_cached(
                "SELECT mxc, pool_file, name, mimetype, hash, file_size, chunk_size, chunks_written, complete, bytes_on_disk, created_at, last_used_at
                 FROM media WHERE complete = 0 AND chunks_written > 0 ORDER BY mxc",
            )
            .map_err(db_error)?;
        let rows = statement
            .query_map([], media_entry_from_row)
            .map_err(db_error)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(db_error)
    }

    /// 看過一次：更新 `last_used_at`（配額的保護期靠它，§8.5）。
    pub fn touch_media(&mut self, mxc: &str) -> Result<bool, SdkError> {
        let changed = self
            .connection
            .execute(
                "UPDATE media SET last_used_at = ?2 WHERE mxc = ?1",
                params![mxc, now_millis()],
            )
            .map_err(db_error)?;
        Ok(changed > 0)
    }

    // ---- forget ----

    /// 摧毀這個帳號的本機紀錄（UI 的選項；`logout` 不叫它）。🚫 不刪 `users` 列：他可能是別人事件的 sender。
    /// 一個 transaction：刪他的關聯列 → 沒人同步過的事件 → 沒事件指的媒體 → 沒事件也沒清單的房間。
    ///
    /// Return:
    ///     Ok(ForgetReport)   `orphan_pool_files` 給呼叫者去刪池裡的檔（DB 先、檔案後）；沒見過的帳號回全零
    pub fn forget_account(&mut self, user_id: &str) -> Result<ForgetReport, SdkError> {
        let transaction = self.connection.transaction().map_err(db_error)?;
        let Some(user) = find_user_row_id(&transaction, user_id)? else {
            return Ok(ForgetReport::default());
        };
        for sql in [
            "DELETE FROM events_synced_log WHERE user = ?1",
            "DELETE FROM room_list WHERE user = ?1",
            "DELETE FROM sync_state WHERE user = ?1",
            "DELETE FROM read_positions WHERE user = ?1",
        ] {
            transaction.execute(sql, params![user]).map_err(db_error)?;
        }
        let events_removed = transaction
            .execute(
                "DELETE FROM events WHERE id NOT IN (SELECT event FROM events_synced_log)",
                [],
            )
            .map_err(db_error)? as u64;
        // 孤兒媒體：沒有任何事件指它。先把 pool_file 記下來，再刪列；最後只回那些已經沒有別的 media 列共用的檔。
        let orphan_files: Vec<Option<String>> = {
            let mut statement = transaction
                .prepare(
                    "SELECT pool_file FROM media WHERE id NOT IN (SELECT media FROM event_media)",
                )
                .map_err(db_error)?;
            let rows = statement
                .query_map([], |row| row.get::<_, Option<String>>(0))
                .map_err(db_error)?;
            rows.collect::<Result<_, _>>().map_err(db_error)?
        };
        let media_removed = transaction
            .execute(
                "DELETE FROM media WHERE id NOT IN (SELECT media FROM event_media)",
                [],
            )
            .map_err(db_error)? as u64;
        let mut orphan_pool_files = Vec::new();
        {
            let mut still_referenced = transaction
                .prepare("SELECT 1 FROM media WHERE pool_file = ?1")
                .map_err(db_error)?;
            for file in orphan_files.into_iter().flatten() {
                if !still_referenced.exists(params![file]).map_err(db_error)? {
                    orphan_pool_files.push(file);
                }
            }
        }
        orphan_pool_files.sort();
        orphan_pool_files.dedup();
        transaction
            .execute(
                "DELETE FROM rooms WHERE id NOT IN (SELECT room FROM events UNION SELECT room FROM room_list)",
                [],
            )
            .map_err(db_error)?;
        transaction.commit().map_err(db_error)?;
        Ok(ForgetReport {
            events_removed,
            media_removed,
            orphan_pool_files,
        })
    }

    /// 測試用後門：跑一句 UPDATE／DELETE（例如把 `last_used_at` 撥到很久以前）。🚫 正式碼不用；參數只收字串。
    #[doc(hidden)]
    pub fn debug_execute(&mut self, sql: &str, params: &[&String]) -> Result<usize, SdkError> {
        // 只准 UPDATE（PR #14 審查 rumia 🟢2）：測試要的只是撥時間戳，不給它 DROP／DELETE 的能力。
        if !sql.trim_start().to_ascii_uppercase().starts_with("UPDATE ") {
            return Err(SdkError::Usage(
                "debug_execute only runs UPDATE statements".into(),
            ));
        }
        self.connection
            .execute(sql, rusqlite::params_from_iter(params.iter()))
            .map_err(db_error)
    }

    /// 除錯與測試用：一張表有幾列。
    pub fn count_rows(&self, table: &str) -> Result<u64, SdkError> {
        const TABLES: [&str; 9] = [
            "users",
            "rooms",
            "events",
            "events_synced_log",
            "room_list",
            "sync_state",
            "read_positions",
            "media",
            "event_media",
        ];
        if !TABLES.contains(&table) {
            return Err(SdkError::Usage(format!("no table {table} in the cache")));
        }
        self.connection
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get::<_, i64>(0)
            })
            .map(|count| count as u64)
            .map_err(db_error)
    }
}

fn media_entry_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<MediaEntry> {
    Ok(MediaEntry {
        mxc: row.get(0)?,
        pool_file: row.get(1)?,
        name: row.get(2)?,
        mimetype: row.get(3)?,
        hash: row.get(4)?,
        file_size: row.get::<_, i64>(5)? as u64,
        chunk_size: row.get::<_, i64>(6)? as u32,
        chunks_written: row.get::<_, i64>(7)? as u64,
        complete: row.get::<_, i64>(8)? == 1,
        bytes_on_disk: row.get::<_, i64>(9)? as u64,
        created_at: row.get(10)?,
        last_used_at: row.get(11)?,
    })
}

struct EventRow {
    event_id: String,
    room_id: String,
    sender: String,
    origin_server_ts: i64,
    r_seq: Option<i64>,
    g_seq: Option<i64>,
    decrypted: Option<i64>,
    message_json: String,
}

/// `Message` 去掉有自己欄位的鍵之後的 JSON（`COLUMN_BACKED_KEYS`）。
fn split_message(message: &Message) -> String {
    let mut value = serde_json::to_value(message).expect("Message serializes");
    if let Some(object) = value.as_object_mut() {
        for key in COLUMN_BACKED_KEYS {
            object.remove(key);
        }
    }
    value.to_string()
}

/// `split_message` 的反向：欄位塞回 JSON 再反序列化。壞列回 None（§1：壞資料當沒有）。
fn join_message(row: &EventRow) -> Option<Message> {
    let mut value: serde_json::Value = serde_json::from_str(&row.message_json).ok()?;
    let object = value.as_object_mut()?;
    object.insert("id".into(), row.event_id.clone().into());
    object.insert("conversation".into(), row.room_id.clone().into());
    object.insert("sender".into(), row.sender.clone().into());
    object.insert(
        "sent_at".into(),
        (row.origin_server_ts.max(0) as u64).into(),
    );
    if let Some(r_seq) = row.r_seq {
        object.insert("r_seq".into(), r_seq.into());
    }
    if let Some(g_seq) = row.g_seq {
        object.insert("g_seq".into(), g_seq.into());
    }
    object.insert(
        "decrypted".into(),
        match row.decrypted {
            Some(1) => serde_json::Value::Bool(true),
            Some(_) => serde_json::Value::Bool(false),
            None => serde_json::Value::Null,
        },
    );
    serde_json::from_value(value).ok()
}

fn kind_code(kind: &MessageKind) -> i64 {
    match kind {
        MessageKind::Text { .. } => KIND_TEXT,
        MessageKind::File { .. } => KIND_FILE,
        MessageKind::Deleted { .. } => KIND_DELETED,
        MessageKind::System { .. } => KIND_SYSTEM,
        MessageKind::Unsupported { .. } => KIND_UNSUPPORTED,
    }
}

// ---- 識別碼 → 整數 id（整數不出這個檔） ----

fn user_row_id(transaction: &Transaction<'_>, mxid: &str) -> Result<i64, SdkError> {
    transaction
        .execute(
            "INSERT OR IGNORE INTO users (mxid, first_seen_at) VALUES (?1, ?2)",
            params![mxid, now_millis()],
        )
        .map_err(db_error)?;
    transaction
        .query_row(
            "SELECT id FROM users WHERE mxid = ?1",
            params![mxid],
            |row| row.get(0),
        )
        .map_err(db_error)
}

fn find_user_row_id(transaction: &Transaction<'_>, mxid: &str) -> Result<Option<i64>, SdkError> {
    transaction
        .query_row(
            "SELECT id FROM users WHERE mxid = ?1",
            params![mxid],
            |row| row.get(0),
        )
        .optional()
        .map_err(db_error)
}

fn room_row_id(transaction: &Transaction<'_>, room_id: &str) -> Result<i64, SdkError> {
    transaction
        .execute(
            "INSERT OR IGNORE INTO rooms (room_id, first_seen_at) VALUES (?1, ?2)",
            params![room_id, now_millis()],
        )
        .map_err(db_error)?;
    transaction
        .query_row(
            "SELECT id FROM rooms WHERE room_id = ?1",
            params![room_id],
            |row| row.get(0),
        )
        .map_err(db_error)
}

fn find_event_row_id(
    transaction: &Transaction<'_>,
    room: i64,
    event_id: &str,
) -> Result<Option<i64>, SdkError> {
    transaction
        .query_row(
            "SELECT id FROM events WHERE room = ?1 AND event_id = ?2",
            params![room, event_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(db_error)
}

/// `media` 有這個 mxc 就用它（不動既有欄），沒有就建一列「還沒下載」。
/// 事件區塊的 `sha256`（hex）→ `media.hash` 的形式 `sha256:<hex>`；沒帶就 None。
fn block_hash(sha256_hex: Option<&str>) -> Option<String> {
    sha256_hex
        .filter(|hex| !hex.is_empty())
        .map(|hex| format!("sha256:{}", hex.to_ascii_lowercase()))
}

fn media_row_id(
    transaction: &Transaction<'_>,
    mxc: &str,
    name: Option<&str>,
    mimetype: Option<&str>,
    hash: Option<&str>,
    file_size: u64,
    chunk_size: u32,
) -> Result<i64, SdkError> {
    let now = now_millis();
    transaction
        .execute(
            "INSERT OR IGNORE INTO media (mxc, pool_file, name, mimetype, hash, file_size, chunk_size, chunks_written, complete, bytes_on_disk, created_at, last_used_at)
             VALUES (?1, NULL, ?2, ?3, ?4, ?5, ?6, 0, 0, 0, ?7, ?7)",
            params![mxc, name, mimetype, hash, file_size as i64, chunk_size as i64, now],
        )
        .map_err(db_error)?;
    // 舊列沒有 hash、這次的區塊有帶：補上（同一個 mxc 的區塊 sha256 不會變，只會從沒有變成有）。
    if let Some(hash) = hash {
        transaction
            .execute(
                "UPDATE media SET hash = ?2 WHERE mxc = ?1 AND hash IS NULL",
                params![mxc, hash],
            )
            .map_err(db_error)?;
    }
    transaction
        .query_row("SELECT id FROM media WHERE mxc = ?1", params![mxc], |row| {
            row.get(0)
        })
        .map_err(db_error)
}

// ---- 開檔 ----

/// Return:
///     Ok(Some(Cache))   開得起來、server 與版本都對
///     Ok(None)          解不開、不是我們的 schema、server 或版本不符 → 呼叫者刪檔重建
fn try_open_existing(
    path: &Path,
    key: &Key32,
    identity: &CacheIdentity,
) -> Result<Option<Cache>, SdkError> {
    // 錯的金鑰在第一次讀頁就是「file is not a database」：那是快取層的 Io 錯，走重建。
    // 「這個 build 沒有 SQLCipher」是 Usage 錯，要往上丟，不能被當成重建的理由。
    let connection = match open_with_key(path, key) {
        Ok(connection) => connection,
        Err(SdkError::Io(_)) => return Ok(None),
        Err(error) => return Err(error),
    };
    let read_meta = |key: &str| -> Option<String> {
        connection
            .query_row(
                "SELECT value FROM meta WHERE key = ?1",
                params![key],
                |row| row.get(0),
            )
            .optional()
            .ok()
            .flatten()
    };
    let matches = read_meta("schema_version").as_deref() == Some(&SCHEMA_VERSION.to_string())
        && read_meta("server").as_deref() == Some(identity.server.as_str());
    if !matches {
        return Ok(None);
    }
    Ok(Some(Cache {
        connection,
        path: path.to_path_buf(),
    }))
}

fn open_with_key(path: &Path, key: &Key32) -> Result<Connection, SdkError> {
    let connection = Connection::open(path).map_err(db_error)?;
    // raw key：跳過 SQLCipher 自己的 KDF，導出在 Vault 做過了（§3）。
    let key_pragma = format!("\"x'{}'\"", hex::encode(key.as_bytes()));
    connection
        .pragma_update(None, "key", &key_pragma)
        .map_err(db_error)?;
    // fail closed：這個 build 沒有 SQLCipher 時 PRAGMA key 是 no-op，會靜默寫出明文檔。問 cipher_version，空的就拒絕。
    let cipher_version: Option<String> = connection
        .query_row("PRAGMA cipher_version", [], |row| row.get(0))
        .optional()
        .map_err(db_error)?;
    if cipher_version.as_deref().unwrap_or("").is_empty() {
        return Err(SdkError::Usage(
            "this build of wbf-sdk has no SQLCipher (PRAGMA cipher_version is empty); refusing to write an unencrypted cache".into(),
        ));
    }
    // 外鍵預設是關的；CASCADE 全靠它，每條連線都要開，開完再驗一次（消費端自己問）。
    connection
        .execute_batch("PRAGMA journal_mode = WAL; PRAGMA foreign_keys = ON;")
        .map_err(db_error)?;
    let foreign_keys_on: i64 = connection
        .query_row("PRAGMA foreign_keys", [], |row| row.get(0))
        .map_err(db_error)?;
    if foreign_keys_on != 1 {
        return Err(SdkError::Io(std::io::Error::other(
            "cache.db: PRAGMA foreign_keys could not be enabled",
        )));
    }
    Ok(connection)
}

fn create_schema(connection: &Connection, identity: &CacheIdentity) -> Result<(), SdkError> {
    connection
        .execute_batch(
            "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             CREATE TABLE users (id INTEGER PRIMARY KEY, mxid TEXT NOT NULL UNIQUE, first_seen_at INTEGER NOT NULL);
             CREATE TABLE rooms (id INTEGER PRIMARY KEY, room_id TEXT NOT NULL UNIQUE, first_seen_at INTEGER NOT NULL);
             CREATE TABLE events (
               id INTEGER PRIMARY KEY,
               room INTEGER NOT NULL REFERENCES rooms(id) ON DELETE CASCADE,
               event_id TEXT NOT NULL,
               sender INTEGER NOT NULL REFERENCES users(id),
               r_seq INTEGER, g_seq INTEGER, origin_server_ts INTEGER NOT NULL,
               kind INTEGER NOT NULL, decrypted INTEGER,
               message_json TEXT NOT NULL);
             CREATE UNIQUE INDEX events_by_event_id ON events (room, event_id);
             CREATE UNIQUE INDEX events_by_seq ON events (room, r_seq) WHERE r_seq IS NOT NULL;
             CREATE INDEX events_by_time ON events (room, origin_server_ts);
             CREATE INDEX events_files ON events (room) WHERE kind = 1;
             CREATE TABLE events_synced_log (
               event INTEGER NOT NULL REFERENCES events(id) ON DELETE CASCADE,
               user INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
               first_synced_at INTEGER NOT NULL, last_synced_at INTEGER NOT NULL,
               hidden INTEGER NOT NULL DEFAULT 0,
               PRIMARY KEY (event, user)) WITHOUT ROWID;
             CREATE INDEX events_synced_log_by_user ON events_synced_log (user, event);
             CREATE TABLE room_list (
               room INTEGER NOT NULL REFERENCES rooms(id) ON DELETE CASCADE,
               user INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
               conversation_json TEXT NOT NULL, refreshed_at INTEGER NOT NULL,
               PRIMARY KEY (user, room)) WITHOUT ROWID;
             CREATE TABLE sync_state (
               user INTEGER PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
               cg_seq INTEGER NOT NULL, updated_at INTEGER NOT NULL) WITHOUT ROWID;
             CREATE TABLE read_positions (
               room INTEGER NOT NULL REFERENCES rooms(id) ON DELETE CASCADE,
               user INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
               event INTEGER NOT NULL REFERENCES events(id) ON DELETE CASCADE,
               ts INTEGER NOT NULL,
               PRIMARY KEY (user, room)) WITHOUT ROWID;
             CREATE TABLE media (
               id INTEGER PRIMARY KEY,
               mxc TEXT NOT NULL UNIQUE,
               pool_file TEXT,
               name TEXT, mimetype TEXT,
               hash TEXT,
               file_size INTEGER NOT NULL, chunk_size INTEGER NOT NULL,
               chunks_written INTEGER NOT NULL, complete INTEGER NOT NULL, bytes_on_disk INTEGER NOT NULL,
               created_at INTEGER NOT NULL, last_used_at INTEGER NOT NULL);
             CREATE INDEX media_lru ON media (last_used_at);
             CREATE INDEX media_by_pool_file ON media (pool_file) WHERE pool_file IS NOT NULL;
             CREATE TABLE event_media (
               event INTEGER NOT NULL REFERENCES events(id) ON DELETE CASCADE,
               media INTEGER NOT NULL REFERENCES media(id) ON DELETE CASCADE,
               PRIMARY KEY (event, media)) WITHOUT ROWID;
             CREATE INDEX event_media_by_media ON event_media (media);",
        )
        .map_err(db_error)?;
    let mut insert = connection
        .prepare("INSERT INTO meta (key, value) VALUES (?1, ?2)")
        .map_err(db_error)?;
    for (key, value) in [
        ("schema_version", SCHEMA_VERSION.to_string()),
        ("server", identity.server.clone()),
        ("created_at", now_millis().to_string()),
    ] {
        insert.execute(params![key, value]).map_err(db_error)?;
    }
    Ok(())
}

/// 這個 server 的快取整個丟掉（所有帳號都登出時；§1 說快取可以整個刪）。沒有檔也算成功。
pub fn remove_cache(dir: &Path) -> Result<bool, SdkError> {
    let path = dir.join(CACHE_FILE_NAME);
    if !path.exists() {
        return Ok(false);
    }
    remove_database_files(&path)?;
    Ok(true)
}

/// 刪 `cache.db` 與 SQLite 的旁檔（不然新檔會撿到舊的 journal）。
fn remove_database_files(path: &Path) -> Result<(), SdkError> {
    std::fs::remove_file(path)?;
    for suffix in ["-journal", "-wal", "-shm"] {
        let _ = std::fs::remove_file(path.with_file_name(format!("{CACHE_FILE_NAME}{suffix}")));
    }
    Ok(())
}

fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

/// rusqlite 的錯誤一律當 Io：對呼叫者來說是「快取壞了」，不是用法錯。錯誤字串裡沒有金鑰。
fn db_error(error: rusqlite::Error) -> SdkError {
    SdkError::Io(std::io::Error::other(format!("cache.db: {error}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::{Attachment, ConversationKind};
    use crate::chunk_block::ChunkedBlock;

    const ALICE: &str = "@alice:localhost";
    const BOB: &str = "@bob:localhost";

    fn scratch_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("wbf-cache-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn identity() -> CacheIdentity {
        CacheIdentity {
            server: "http://localhost:6167".into(),
        }
    }

    fn open(name: &str) -> (Cache, PathBuf) {
        let dir = scratch_dir(name);
        let (cache, _) = Cache::open(&dir, &Key32([1u8; 32]), &identity()).unwrap();
        (cache, dir)
    }

    fn text(room: &str, id: &str, r_seq: Option<i64>, ts: u64) -> Message {
        Message {
            id: id.into(),
            conversation: room.into(),
            sender: "@carol:localhost".into(),
            sent_at: ts,
            kind: MessageKind::Text {
                body: format!("body {id}"),
                formatted_html: None,
            },
            reply_to: None,
            edited_by: None,
            reactions: Vec::new(),
            decrypted: Some(true),
            undecryptable_reason: None,
            r_seq,
            g_seq: r_seq.map(|seq| seq * 10),
        }
    }

    fn file(room: &str, id: &str, r_seq: i64, mxc: &str) -> Message {
        let block: ChunkedBlock = serde_json::from_value(serde_json::json!({
            "v": 1, "cipher": "none", "chunk_size": 65536, "file_size": 10, "name": "a.txt"
        }))
        .unwrap_or_else(|_| panic!("ChunkedBlock fixture"));
        Message {
            kind: MessageKind::File {
                attachment: Attachment {
                    mxc: mxc.into(),
                    block,
                },
                caption: None,
            },
            ..text(room, id, Some(r_seq), 1000 + r_seq as u64)
        }
    }

    fn ids(messages: &[Message]) -> Vec<String> {
        messages.iter().map(|message| message.id.clone()).collect()
    }

    #[test]
    fn open_creates_reuses_and_rebuilds_on_server_change_or_wrong_key() {
        let dir = scratch_dir("open");
        let key = Key32([1u8; 32]);
        let (mut cache, outcome) = Cache::open(&dir, &key, &identity()).unwrap();
        assert_eq!(outcome, OpenOutcome::Created);
        cache
            .upsert_messages(ALICE, &[text("!r", "$1", Some(1), 1)])
            .unwrap();
        let path = cache.path().to_path_buf();
        drop(cache);
        let bytes = std::fs::read(&path).unwrap();
        assert!(
            !bytes.starts_with(b"SQLite format 3"),
            "cache.db is not encrypted"
        );

        let (cache, outcome) = Cache::open(&dir, &key, &identity()).unwrap();
        assert_eq!(outcome, OpenOutcome::Reused);
        assert_eq!(cache.history(ALICE, "!r", None, 10).unwrap().len(), 1);
        drop(cache);

        let other_server = CacheIdentity {
            server: "http://other:6167".into(),
        };
        let (cache, outcome) = Cache::open(&dir, &key, &other_server).unwrap();
        assert_eq!(outcome, OpenOutcome::Rebuilt);
        assert!(cache.history(ALICE, "!r", None, 10).unwrap().is_empty());
        drop(cache);

        let (cache, outcome) = Cache::open(&dir, &Key32([2u8; 32]), &identity()).unwrap();
        assert_eq!(outcome, OpenOutcome::Rebuilt);
        assert!(cache.history(ALICE, "!r", None, 10).unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn foreign_keys_are_on() {
        let (cache, dir) = open("fk");
        let on: i64 = cache
            .connection
            .query_row("PRAGMA foreign_keys", [], |row| row.get(0))
            .unwrap();
        assert_eq!(on, 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn each_user_only_sees_what_they_synced_and_events_are_stored_once() {
        let (mut cache, dir) = open("visibility");
        // alice 拿到 1、2、3；bob 拿到 2、3、4。同一個 room。
        cache
            .upsert_messages(
                ALICE,
                &[
                    text("!r", "$1", Some(1), 10),
                    text("!r", "$2", Some(2), 20),
                    text("!r", "$3", Some(3), 30),
                ],
            )
            .unwrap();
        cache
            .upsert_messages(
                BOB,
                &[
                    text("!r", "$2", Some(2), 20),
                    text("!r", "$3", Some(3), 30),
                    text("!r", "$4", Some(4), 40),
                ],
            )
            .unwrap();
        assert_eq!(
            ids(&cache.history(ALICE, "!r", None, 10).unwrap()),
            ["$3", "$2", "$1"]
        );
        assert_eq!(
            ids(&cache.history(BOB, "!r", None, 10).unwrap()),
            ["$4", "$3", "$2"]
        );
        assert!(cache
            .history("@nobody:localhost", "!r", None, 10)
            .unwrap()
            .is_empty());
        assert_eq!(cache.count_rows("events").unwrap(), 4);
        assert_eq!(cache.count_rows("events_synced_log").unwrap(), 6);
        // 字串識別碼各只存一次：sender carol、讀者 alice／bob → users 三列；room 一列。
        assert_eq!(cache.count_rows("users").unwrap(), 3);
        assert_eq!(cache.count_rows("rooms").unwrap(), 1);
        assert_eq!(cache.room_stats("!r").unwrap(), (4, Some(4)));
        // 翻頁：--before 是 r_seq。
        assert_eq!(
            ids(&cache.history(ALICE, "!r", Some(3), 1).unwrap()),
            ["$2"]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rooms_without_r_seq_fall_back_to_time_order() {
        let (mut cache, dir) = open("plain");
        cache
            .upsert_messages(
                ALICE,
                &[text("!p", "$p1", None, 100), text("!p", "$p2", None, 200)],
            )
            .unwrap();
        assert_eq!(
            ids(&cache.history(ALICE, "!p", None, 10).unwrap()),
            ["$p2", "$p1"]
        );
        assert_eq!(cache.room_stats("!p").unwrap(), (2, None));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn split_and_join_message_roundtrip_every_field() {
        let (mut cache, dir) = open("roundtrip");
        let mut message = text("!r", "$m", Some(7), 777);
        message.reply_to = Some("$parent".into());
        message.edited_by = Some(BOB.into());
        message.reactions = vec![crate::chat::Reaction {
            key: "👍".into(),
            by: vec![BOB.into()],
        }];
        message.decrypted = Some(false);
        message.undecryptable_reason = Some("MissingMegolmSession".into());
        cache
            .upsert_messages(ALICE, std::slice::from_ref(&message))
            .unwrap();
        let stored = cache.history(ALICE, "!r", None, 1).unwrap().remove(0);
        assert_eq!(stored, message);
        // 剝掉的鍵真的不在 message_json 裡。
        let raw: String = cache
            .connection
            .query_row("SELECT message_json FROM events", [], |row| row.get(0))
            .unwrap();
        for key in COLUMN_BACKED_KEYS {
            assert!(
                !raw.contains(&format!("\"{key}\"")),
                "{key} is still in message_json: {raw}"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn undecrypted_copy_does_not_overwrite_a_decrypted_row_but_the_reverse_does() {
        let (mut cache, dir) = open("nodowngrade");
        let plain = text("!r", "$e", Some(1), 1);
        cache
            .upsert_messages(ALICE, std::slice::from_ref(&plain))
            .unwrap();
        let mut raw = text("!r", "$e", Some(1), 1);
        raw.kind = MessageKind::Unsupported {
            event_type: "m.room.encrypted".into(),
            body: None,
        };
        raw.decrypted = Some(false);
        // bob 拿到的是密文：他也拿到一列 synced_log，但事件內容不被降級。
        cache
            .upsert_messages(BOB, std::slice::from_ref(&raw))
            .unwrap();
        let alice_view = cache.history(ALICE, "!r", None, 1).unwrap().remove(0);
        assert_eq!(alice_view.decrypted, Some(true));
        assert!(matches!(alice_view.kind, MessageKind::Text { .. }));
        assert_eq!(
            cache.history(BOB, "!r", None, 1).unwrap()[0].decrypted,
            Some(true)
        );
        // 反過來：先密文後明文要蓋。
        let mut raw2 = raw.clone();
        raw2.id = "$f".into();
        raw2.r_seq = Some(2);
        cache
            .upsert_messages(BOB, std::slice::from_ref(&raw2))
            .unwrap();
        let mut plain2 = plain.clone();
        plain2.id = "$f".into();
        plain2.r_seq = Some(2);
        cache
            .upsert_messages(ALICE, std::slice::from_ref(&plain2))
            .unwrap();
        assert_eq!(
            cache.history(BOB, "!r", None, 1).unwrap()[0].decrypted,
            Some(true)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// decrypted 是 NULL（非加密事件）的列再寫一次也要覆蓋：三值邏輯不能讓 NULL→NULL 靜默跳過（PR #13 審查）。
    #[test]
    fn null_decrypted_rows_still_get_overwritten() {
        let (mut cache, dir) = open("nullnull");
        let mut first = text("!r", "$n", Some(1), 1);
        first.decrypted = None;
        cache
            .upsert_messages(ALICE, std::slice::from_ref(&first))
            .unwrap();
        let mut edited = first.clone();
        edited.kind = MessageKind::Text {
            body: "edited".into(),
            formatted_html: None,
        };
        cache
            .upsert_messages(ALICE, std::slice::from_ref(&edited))
            .unwrap();
        let stored = cache.history(ALICE, "!r", None, 1).unwrap().remove(0);
        assert!(matches!(&stored.kind, MessageKind::Text { body, .. } if body == "edited"));
        assert_eq!(stored.decrypted, None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn hide_is_per_user_and_survives_resync() {
        let (mut cache, dir) = open("hidden");
        let message = text("!r", "$1", Some(1), 1);
        cache
            .upsert_messages(ALICE, std::slice::from_ref(&message))
            .unwrap();
        cache
            .upsert_messages(BOB, std::slice::from_ref(&message))
            .unwrap();
        assert!(cache.hide_message(ALICE, "!r", "$1").unwrap());
        assert!(cache.history(ALICE, "!r", None, 10).unwrap().is_empty());
        assert_eq!(cache.history(BOB, "!r", None, 10).unwrap().len(), 1);
        // 再同步：只更新 last_synced_at，hidden 不動。
        cache
            .upsert_messages(ALICE, std::slice::from_ref(&message))
            .unwrap();
        assert!(cache.history(ALICE, "!r", None, 10).unwrap().is_empty());
        // 沒看過的東西沒有可藏的。
        assert!(!cache.hide_message("@nobody:localhost", "!r", "$1").unwrap());
        assert!(!cache.hide_message(ALICE, "!r", "$missing").unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn file_events_create_media_once_and_forget_walks_the_chain() {
        let (mut cache, dir) = open("media");
        let shared = file("!r", "$f1", 1, "mxc://localhost/aaaa");
        let alice_only = file("!r", "$f2", 2, "mxc://localhost/bbbb");
        cache
            .upsert_messages(ALICE, &[shared.clone(), alice_only.clone()])
            .unwrap();
        cache
            .upsert_messages(BOB, std::slice::from_ref(&shared))
            .unwrap();
        assert_eq!(cache.count_rows("media").unwrap(), 2);
        assert_eq!(cache.count_rows("event_media").unwrap(), 2);
        let entry = cache.find_media("mxc://localhost/aaaa").unwrap().unwrap();
        assert_eq!(entry.name.as_deref(), Some("a.txt"));
        assert_eq!(entry.file_size, 10);
        assert!(!entry.complete);
        assert!(cache.touch_media("mxc://localhost/aaaa").unwrap());
        assert!(!cache.touch_media("mxc://localhost/none").unwrap());
        assert_eq!(
            ids(&cache.files(ALICE, "!r", None, 10).unwrap()),
            ["$f2", "$f1"]
        );
        assert_eq!(ids(&cache.files(BOB, "!r", None, 10).unwrap()), ["$f1"]);

        // 模擬兩個都下載完：池檔 hash-a、hash-b。
        cache
            .connection
            .execute(
                "UPDATE media SET pool_file = 'hash-b', complete = 1 WHERE mxc = 'mxc://localhost/bbbb'",
                [],
            )
            .unwrap();
        cache
            .connection
            .execute(
                "UPDATE media SET pool_file = 'hash-a', complete = 1 WHERE mxc = 'mxc://localhost/aaaa'",
                [],
            )
            .unwrap();

        // 忘掉 alice：$f2 沒人同步過 → 清；bbbb 沒事件指 → 清，池檔 hash-b 沒人用 → 回傳。$f1／aaaa 還有 bob。
        let report = cache.forget_account(ALICE).unwrap();
        assert_eq!(report.events_removed, 1);
        assert_eq!(report.media_removed, 1);
        assert_eq!(report.orphan_pool_files, vec!["hash-b".to_string()]);
        assert_eq!(cache.count_rows("events").unwrap(), 1);
        assert_eq!(cache.count_rows("media").unwrap(), 1);
        assert_eq!(cache.count_rows("event_media").unwrap(), 1);
        assert_eq!(ids(&cache.files(BOB, "!r", None, 10).unwrap()), ["$f1"]);
        assert!(cache.history(ALICE, "!r", None, 10).unwrap().is_empty());
        // users 列不清：alice 可能是別人事件的 sender。
        assert!(cache.count_rows("users").unwrap() >= 3);
        // 忘掉沒見過的帳號：什麼都不動。
        assert_eq!(
            cache.forget_account("@nobody:localhost").unwrap(),
            ForgetReport::default()
        );

        // 同 hash 兩個 mxc：只 forget 到其中一個 mxc 的事件時，池檔不回傳。
        let dup1 = file("!s", "$d1", 1, "mxc://localhost/c1");
        let dup2 = file("!s", "$d2", 2, "mxc://localhost/c2");
        cache
            .upsert_messages(ALICE, std::slice::from_ref(&dup1))
            .unwrap();
        cache
            .upsert_messages(BOB, std::slice::from_ref(&dup2))
            .unwrap();
        cache
            .connection
            .execute(
                "UPDATE media SET pool_file = 'hash-same', complete = 1 WHERE mxc IN ('mxc://localhost/c1', 'mxc://localhost/c2')",
                [],
            )
            .unwrap();
        let report = cache.forget_account(ALICE).unwrap();
        assert_eq!(report.media_removed, 1);
        assert!(report.orphan_pool_files.is_empty(), "{report:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn media_hash_comes_from_the_block_or_is_filled_with_blake3() {
        let (mut cache, dir) = open("hash");
        // 區塊沒帶 sha256、還沒下載完：None。
        cache
            .upsert_messages(ALICE, &[file("!h", "$n", 1, "mxc://localhost/nohash")])
            .unwrap();
        assert_eq!(
            cache
                .find_media("mxc://localhost/nohash")
                .unwrap()
                .unwrap()
                .hash,
            None
        );
        // 區塊帶 sha256：進來就是 sha256:<hex>（小寫）。
        let mut with_sha = file("!h", "$s", 2, "mxc://localhost/withhash");
        if let MessageKind::File { attachment, .. } = &mut with_sha.kind {
            attachment.block.sha256 = Some("ABCDEF".into());
        }
        cache
            .upsert_messages(ALICE, std::slice::from_ref(&with_sha))
            .unwrap();
        assert_eq!(
            cache
                .find_media("mxc://localhost/withhash")
                .unwrap()
                .unwrap()
                .hash
                .as_deref(),
            Some("sha256:abcdef")
        );
        // 沒帶的下載完：用 blake3 補；之後 reset 也不清（校驗碼是內容的事實，不是本地副本的）。
        cache
            .media_finish("mxc://localhost/nohash", "hash-n", 1, 10, 100)
            .unwrap();
        cache.media_reset("mxc://localhost/nohash").unwrap();
        assert_eq!(
            cache
                .find_media("mxc://localhost/nohash")
                .unwrap()
                .unwrap()
                .hash
                .as_deref(),
            Some("blake3:hash-n")
        );
        // 帶 sha256 的下載完：不被 blake3 蓋掉。
        cache
            .media_finish("mxc://localhost/withhash", "hash-s", 1, 10, 100)
            .unwrap();
        assert_eq!(
            cache
                .find_media("mxc://localhost/withhash")
                .unwrap()
                .unwrap()
                .hash
                .as_deref(),
            Some("sha256:abcdef")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn forget_removes_rooms_nobody_references_and_keeps_others() {
        let (mut cache, dir) = open("forget-rooms");
        cache
            .upsert_messages(ALICE, &[text("!only-alice", "$1", Some(1), 1)])
            .unwrap();
        cache
            .upsert_messages(ALICE, &[text("!shared", "$2", Some(1), 1)])
            .unwrap();
        cache
            .upsert_messages(BOB, &[text("!shared", "$2", Some(1), 1)])
            .unwrap();
        cache.set_cg_seq(ALICE, 50).unwrap();
        cache.set_cg_seq(BOB, 60).unwrap();
        let report = cache.forget_account(ALICE).unwrap();
        assert_eq!(report.events_removed, 1);
        assert_eq!(cache.count_rows("rooms").unwrap(), 1);
        assert_eq!(cache.get_cg_seq(ALICE).unwrap(), None);
        assert_eq!(cache.get_cg_seq(BOB).unwrap(), Some(60));
        assert_eq!(cache.history(BOB, "!shared", None, 10).unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sync_state_room_list_and_read_position_are_per_user() {
        let (mut cache, dir) = open("peruser");
        assert_eq!(cache.get_cg_seq(ALICE).unwrap(), None);
        cache.set_cg_seq(ALICE, 4700).unwrap();
        cache.set_cg_seq(ALICE, 4800).unwrap();
        assert_eq!(cache.get_cg_seq(ALICE).unwrap(), Some(4800));
        assert_eq!(cache.get_cg_seq(BOB).unwrap(), None);

        let conversation = Conversation {
            id: "!r".into(),
            kind: ConversationKind::Group,
            name: Some("room".into()),
            topic: None,
            encrypted: true,
            member_count: 2,
            my_power_level: 100,
            can_send_message: true,
            direct_peer: None,
        };
        cache
            .upsert_conversations(ALICE, std::slice::from_ref(&conversation))
            .unwrap();
        cache
            .upsert_conversations(ALICE, std::slice::from_ref(&conversation))
            .unwrap();
        assert_eq!(cache.list_conversations(ALICE).unwrap(), vec![conversation]);
        assert!(cache.list_conversations(BOB).unwrap().is_empty());
        assert_eq!(cache.count_rows("room_list").unwrap(), 1);

        cache
            .upsert_messages(ALICE, &[text("!r", "$1", Some(1), 5)])
            .unwrap();
        assert_eq!(cache.get_read_position(ALICE, "!r").unwrap(), None);
        assert!(cache.set_read_position(ALICE, "!r", "$missing", 5).is_err());
        cache.set_read_position(ALICE, "!r", "$1", 5).unwrap();
        assert_eq!(
            cache.get_read_position(ALICE, "!r").unwrap(),
            Some(ReadPosition {
                event_id: "$1".into(),
                r_seq: Some(1),
                ts: 5
            })
        );
        assert_eq!(cache.get_read_position(BOB, "!r").unwrap(), None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

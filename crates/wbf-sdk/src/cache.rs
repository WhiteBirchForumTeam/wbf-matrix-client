//! `cache.db`：本地快取（local-cache-db.md §1、§3、§6）。SQLCipher 整檔加密，raw key 是 `Vault::cache_key()`。
//!
//! **一個 server 一個檔、多帳號混存**（維護者 2026-09-07 定）：事件只存一份；誰看得到哪一則由 `events_synced_log` 逐則記
//! （server 經任一條路給過這個 user 的才算），沒有列就看不到——fail closed，不用 r_seq 下界去猜可見性。
//! **快取不是權威**（§1）：server 不符、schema 版本不對、解不開，一律刪檔重建，不寫遷移；讀到壞資料當成沒有快取。
//!
//! schema 風格：實體表 `INTEGER PRIMARY KEY` 加識別碼的 UNIQUE 索引；關聯表整數複合主鍵 `WITHOUT ROWID`；
//! 字串識別碼（mxid、room_id、event_id）各只存一次，其餘全走整數外鍵。整數 id 不出這個檔。
//! **事件存原樣、顯示另存**（§7，維護者 2026-09-14）：`raw_event` 第一次寫入之後永遠不動；解密結果與套過 edit 的內容在
//! `content_json`，**兩者寫進去就不改**。edit／redact／reaction 的 `ref_event_id` 指目標；訊息自己的 `ref_event_id` 指
//! **目前要顯示的 edit**，寫入時比 `modified_timestamp` 決定換不換（§7.5）。redact 在目標那列打勾 `is_redacted`。
//! 這裡只有 SQL 與我們的聊天模型（`Message`、`Conversation`），沒有 matrix-sdk、沒有網路。
//! 🚫 金鑰不進錯誤訊息、不 log。

use std::path::{Path, PathBuf};

use rusqlite::{params, Connection, OptionalExtension, Transaction};

use crate::chat::{Conversation, Message, MessageKind, Reaction};
use crate::error::SdkError;
use crate::event_json::{kind_from_content, FILE_MSGTYPE};
use crate::incoming::{
    check_replacement, classify, to_replaced_body, EventClass, IncomingEvent, ReplacementSide,
    NOT_DECRYPTED_HERE,
};
use crate::vault::Key32;

pub const CACHE_FILE_NAME: &str = "cache.db";
/// 換 schema 就加一，舊檔整個重建（§1）。v5：`events` 照 §7 改。
const SCHEMA_VERSION: i64 = 5;

/// 快取屬於哪個 server；不符就不是這份快取（§6 `meta`）。帳號不在身份裡：同一個 server 的帳號共用。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CacheIdentity {
    pub server: String,
}

/// [`Cache::upsert_events_counted`] 的結果。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UpsertOutcome {
    /// 寫進去（或已經在、記了同步紀錄）的則數。
    pub written: usize,
    /// 永遠存不了的則數：沒有 `event_id`／`sender`，或 `room_id` 跟參數不同。呼叫端要講出來。
    pub unstorable: usize,
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

/// 一則事件在房間裡的位置（[`Cache::find_event_position`]）。
///
/// ⚠️ 兩個號各司其職（wbfuwunel room-seq-and-recent.md §2）：**`g_seq` 翻頁**（server 的游標），
/// **`r_seq` 判洞**（房內連續）。非 fork server 的事件兩個都是 `None`。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EventPosition {
    pub r_seq: Option<i64>,
    pub g_seq: Option<i64>,
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

    /// 水位**只往前推**：`cg_seq` 比現在的大才寫（推播一包推一次，包會亂序、會重複；`Recent` 的窗用 `set_cg_seq`）。
    ///
    /// Args:
    ///     user_id: example: "@alice:localhost"
    ///     cg_seq: 這一包最新那則的 `g_seq`, example: 4712
    /// Return:
    ///     Ok(true)    推進了（之前沒有、或比較舊）
    ///     Ok(false)   現在的已經 ≥ 它，沒動
    pub fn advance_cg_seq(&mut self, user_id: &str, cg_seq: i64) -> Result<bool, SdkError> {
        match self.get_cg_seq(user_id)? {
            Some(current) if current >= cg_seq => Ok(false),
            _ => {
                self.set_cg_seq(user_id, cg_seq)?;
                Ok(true)
            }
        }
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
            // 🚨 **只准 0 → 1，🚫 不准 1 → 0**：Matrix 房間一開加密就關不掉，所以任何一份
            // 「沒加密」—— 過期的、別的帳號舊的、有 bug 的 —— 都🚫 不准把已知加密的房間蓋回明文。
            // ⭐ 蓋回去的下一步就是送檔時用 `cipher: none` 把區塊金鑰公開出去（約定 §5.1）。
            let mut mark_encryption = transaction
                .prepare_cached(
                    "UPDATE rooms SET encrypted = CASE WHEN encrypted = 1 THEN 1 ELSE ?2 END WHERE id = ?1",
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
                mark_encryption
                    .execute(params![room, conversation.encrypted])
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
                "SELECT l.conversation_json, r.encrypted FROM room_list l
                   JOIN users u ON u.id = l.user JOIN rooms r ON r.id = l.room
                 WHERE u.mxid = ?1",
            )
            .map_err(db_error)?;
        let rows = statement
            .query_map(params![user_id], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, Option<bool>>(1)?))
            })
            .map_err(db_error)?;
        let mut conversations: Vec<Conversation> = Vec::new();
        for row in rows {
            let (json, room_is_encrypted) = row.map_err(db_error)?;
            if let Ok(mut conversation) = serde_json::from_str::<Conversation>(&json) {
                // 🚨 `rooms.encrypted` 是**房間的**事實，`conversation_json` 只是**這個帳號上次看到的樣子**。
                // 房間說加密就是加密 —— 🚫 不讓一份舊的「沒加密」從讀的這一側漏出去。
                if room_is_encrypted == Some(true) {
                    conversation.encrypted = true;
                }
                conversations.push(conversation);
            }
        }
        conversations
            .sort_by(|left, right| left.name.cmp(&right.name).then(left.id.cmp(&right.id)));
        Ok(conversations)
    }

    // ---- events ----

    /// 這個帳號從 server 拿到的一批事件（同一個房間），照 §7 存與處理，一個 transaction。
    ///
    /// - 事件一份：**`raw_event` 第一次寫入之後永遠不動**（只從 NULL 補上）；同一則再來只補缺的序號、
    ///   server 已經 redact 過就把 `is_redacted` 打勾（只升不降）。
    /// - 之前沒解開（`class = general`）的，這次帶著明文來：照明文分類，當作第一次處理。
    /// - 每則替 `user_id` 記一列 `events_synced_log`，已有就只更新 `last_synced_at`，`hidden` 不動。
    ///
    /// 🚫 **不寫**：沒有 `event_id` 或 `sender` 的（不能被參照、不能比對誰改了誰的訊息）；
    /// 事件自己帶的 `room_id` 跟 `room_id` 參數不一樣的（不猜是哪一邊錯）。
    ///
    /// Args:
    ///     user_id: 從 server 拿到這批的帳號, example: "@alice:localhost"
    ///     room_id: 這批事件的房間, example: "!abc:localhost"
    ///     events: 上游給的原樣
    /// Return:
    ///     Ok(usize)   寫進去（或已經在、記了同步紀錄）的則數；不寫的不算（要知道不寫了幾則用 [`Cache::upsert_events_counted`]）
    pub fn upsert_events(
        &mut self,
        user_id: &str,
        room_id: &str,
        events: &[IncomingEvent],
    ) -> Result<usize, SdkError> {
        Ok(self
            .upsert_events_counted(user_id, room_id, events)?
            .written)
    }

    /// 同 [`Cache::upsert_events`]，但連「不寫的幾則」一起回：進料口（推播、`Recent`）要把這個數字講出來，🚫 不靜默跳過（PR #58 審查 rumia 🔴）。
    ///
    /// Return:
    ///     Ok(UpsertOutcome)   `written` 寫進去的；`unstorable` 沒有 `event_id`／`sender`、或 `room_id` 跟參數不同的（永遠存不了，不是這次失敗）
    pub fn upsert_events_counted(
        &mut self,
        user_id: &str,
        room_id: &str,
        events: &[IncomingEvent],
    ) -> Result<UpsertOutcome, SdkError> {
        let transaction = self.connection.transaction().map_err(db_error)?;
        let reader = user_row_id(&transaction, user_id)?;
        let room = room_row_id(&transaction, room_id)?;
        let now = now_millis();
        let mut written = 0usize;
        let mut unstorable = 0usize;
        for incoming in events {
            let envelope = incoming.envelope();
            let text = |key: &str| envelope.get(key).and_then(|value| value.as_str());
            let Some((event_id, sender_mxid)) = incoming.storable_identity() else {
                unstorable += 1;
                continue;
            };
            if text("room_id").is_some_and(|own_room| own_room != room_id) {
                unstorable += 1;
                continue;
            }
            let sender = user_row_id(&transaction, sender_mxid)?;
            let (r_seq, g_seq) = incoming.seqs();
            let origin_server_ts = envelope
                .get("origin_server_ts")
                .and_then(|ts| ts.as_i64())
                .unwrap_or(0);
            let classified = classify(incoming);
            // edit／redact 要等目標（改目標那一列）；msg、reaction 分完類就處理完了。
            let is_processed_on_arrival =
                matches!(classified.class, EventClass::Msg | EventClass::Reaction);
            let raw_event = incoming.raw_event().map(|raw| raw.to_string());
            let content_json = classified
                .content_json
                .as_ref()
                .map(|body| body.to_string());
            let is_redacted_by_server = incoming.is_redacted_by_server();
            let inserted = transaction
                .prepare_cached(
                    "INSERT INTO events (room, event_id, sender, r_seq, g_seq, origin_server_ts, decrypted, raw_event,
                       event_type, content_json, is_processed, is_redacted, class, ref_event_id, modified_timestamp)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, 0)
                     ON CONFLICT(room, event_id) DO NOTHING",
                )
                .map_err(db_error)?
                .execute(params![
                    room,
                    event_id,
                    sender,
                    r_seq,
                    g_seq,
                    origin_server_ts,
                    incoming.decrypted().map(|flag| flag as i64),
                    raw_event,
                    classified.event_type,
                    content_json,
                    is_processed_on_arrival as i64,
                    is_redacted_by_server as i64,
                    classified.class.to_column(),
                    classified.ref_event_id,
                ])
                .map_err(db_error)?
                == 1;
            let event = find_event_row_id(&transaction, room, event_id)?.ok_or_else(|| {
                SdkError::Usage(format!(
                    "cache: {event_id} vanished right after it was written"
                ))
            })?;
            let mut is_new_to_process = inserted;
            if !inserted {
                // 🚫 raw_event、content_json 不覆蓋：只從 NULL 補。is_redacted 只升不降。
                transaction
                    .prepare_cached(
                        "UPDATE events SET raw_event = COALESCE(raw_event, ?2), r_seq = COALESCE(r_seq, ?3),
                           g_seq = COALESCE(g_seq, ?4), is_redacted = MAX(is_redacted, ?5)
                         WHERE id = ?1",
                    )
                    .map_err(db_error)?
                    .execute(params![event, raw_event, r_seq, g_seq, is_redacted_by_server as i64])
                    .map_err(db_error)?;
                if classified.class != EventClass::General {
                    is_new_to_process = transaction
                        .prepare_cached(
                            "UPDATE events SET decrypted = ?2, event_type = ?3, content_json = ?4, class = ?5,
                               ref_event_id = ?6, is_processed = ?7
                             WHERE id = ?1 AND class = 'general'",
                        )
                        .map_err(db_error)?
                        .execute(params![
                            event,
                            incoming.decrypted().map(|flag| flag as i64),
                            classified.event_type,
                            content_json,
                            classified.class.to_column(),
                            classified.ref_event_id,
                            is_processed_on_arrival as i64,
                        ])
                        .map_err(db_error)?
                        == 1;
                }
            }
            if is_new_to_process {
                process_event(&transaction, room, event)?;
            }
            transaction
                .prepare_cached(
                    "INSERT INTO events_synced_log (event, user, first_synced_at, last_synced_at, hidden) VALUES (?1, ?2, ?3, ?3, 0)
                     ON CONFLICT(event, user) DO UPDATE SET last_synced_at = excluded.last_synced_at",
                )
                .map_err(db_error)?
                .execute(params![event, reader, now])
                .map_err(db_error)?;
            written += 1;
        }
        transaction.commit().map_err(db_error)?;
        Ok(UpsertOutcome {
            written,
            unstorable,
        })
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
        self.select_messages(user_id, room_id, before_r_seq, limit, false)
    }

    /// 只要分塊檔事件（`files` 命令）。
    pub fn files(
        &self,
        user_id: &str,
        room_id: &str,
        before_r_seq: Option<i64>,
        limit: u32,
    ) -> Result<Vec<Message>, SdkError> {
        self.select_messages(user_id, room_id, before_r_seq, limit, true)
    }

    fn select_messages(
        &self,
        user_id: &str,
        room_id: &str,
        before_r_seq: Option<i64>,
        limit: u32,
        only_files: bool,
    ) -> Result<Vec<Message>, SdkError> {
        // 🚨 只出「自己要顯示」的列（msg、還沒解開的 general）：edit／redact／reaction 的效果在目標上。
        let rows: Vec<EventRow> = {
            let mut statement = self
                .connection
                .prepare_cached(&format!(
                    "SELECT {EVENT_ROW_COLUMNS}
                     FROM events e
                     JOIN events_synced_log l ON l.event = e.id
                     JOIN users reader ON reader.id = l.user
                     JOIN users s ON s.id = e.sender
                     JOIN rooms r ON r.id = e.room
                     WHERE reader.mxid = ?1 AND r.room_id = ?2 AND l.hidden = 0
                       AND e.class IN ('msg', 'general')
                       AND (?3 IS NULL OR (e.r_seq IS NOT NULL AND e.r_seq < ?3))
                       AND (?4 = 0 OR (e.class = 'msg' AND e.is_redacted = 0
                                       AND json_extract(e.content_json, '$.msgtype') = ?6))
                     ORDER BY e.r_seq IS NULL, e.r_seq DESC, e.origin_server_ts DESC
                     LIMIT ?5"
                ))
                .map_err(db_error)?;
            let rows = statement
                .query_map(
                    params![
                        user_id,
                        room_id,
                        before_r_seq,
                        only_files as i64,
                        limit,
                        FILE_MSGTYPE
                    ],
                    event_row_from_sql,
                )
                .map_err(db_error)?;
            let collected: Vec<EventRow> =
                rows.collect::<rusqlite::Result<_>>().map_err(db_error)?;
            collected
        };
        let mut messages = Vec::with_capacity(rows.len());
        for row in &rows {
            if let Some(message) = self.to_message(user_id, row)? {
                messages.push(message);
            }
        }
        Ok(messages)
    }

    /// 一則事件在房間裡的**位置**：翻頁時把 UI 給的 `event_id` 換成上游聽得懂的座標。
    ///
    /// 🚨 **只看這個帳號看得到的**（JOIN `events_synced_log`）：別的帳號同步進來、這個帳號從沒看過的事件，
    /// 🚫 不准拿來當翻頁的錨 —— 那等於用 B 的可見範圍替 A 定位，而且洩漏「那則事件存在」。
    /// ⚠️ `hidden` 的也算（刪給自己看的只是不顯示，位置照樣是真的）。
    ///
    /// Args:
    ///     user_id: example: "@alice:localhost"
    ///     room_id: example: "!abc:localhost"
    ///     event_id: example: "$e1"
    /// Return:
    ///     Ok(Some(EventPosition))  找到了；`r_seq`／`g_seq` 在非 fork server 上是 None
    ///     Ok(None)                 這個帳號在這個房間沒有這則（沒同步過、別人的、不存在）
    pub fn find_event_position(
        &self,
        user_id: &str,
        room_id: &str,
        event_id: &str,
    ) -> Result<Option<EventPosition>, SdkError> {
        self.connection
            .query_row(
                "SELECT e.r_seq, e.g_seq FROM events e
                   JOIN events_synced_log l ON l.event = e.id
                   JOIN users reader ON reader.id = l.user
                   JOIN rooms r ON r.id = e.room
                 WHERE reader.mxid = ?1 AND r.room_id = ?2 AND e.event_id = ?3",
                params![user_id, room_id, event_id],
                |row| {
                    Ok(EventPosition {
                        r_seq: row.get(0)?,
                        g_seq: row.get(1)?,
                    })
                },
            )
            .optional()
            .map_err(db_error)
    }

    /// 按給的 `event_id` 從本地讀回訊息，**順序照給的**。
    ///
    /// ⭐ 用在「問完上游、寫進去、再從本地讀回這一頁」：上游決定**哪幾則、什麼順序**
    /// （`Recent` 照 `g_seq`、`/messages` 照拓樸序），本地決定**每一則長什麼樣**
    /// （已解密的明文不會被密文蓋掉、`hidden` 的不出來、別的帳號的看不到）。
    /// 🚫 **不靠 `r_seq` 排**：非 fork server 的事件沒有它，而拿時間戳排是錯的（chat-model §4.3）。
    ///
    /// Args:
    ///     user_id: example: "@alice:localhost"
    ///     room_id: example: "!abc:localhost"
    ///     event_ids: 上游那一頁的順序, example: &["$new".to_string(), "$old".to_string()]
    /// Return:
    ///     Ok(Vec<Message>)  照 `event_ids` 的順序；本地沒有、`hidden`、這個帳號看不到的就不在裡面
    pub fn list_messages_by_event_ids(
        &self,
        user_id: &str,
        room_id: &str,
        event_ids: &[String],
    ) -> Result<Vec<Message>, SdkError> {
        let mut messages = Vec::with_capacity(event_ids.len());
        for event_id in event_ids {
            let row = self
                .connection
                .prepare_cached(&format!(
                    "SELECT {EVENT_ROW_COLUMNS}
                     FROM events e
                     JOIN events_synced_log l ON l.event = e.id
                     JOIN users reader ON reader.id = l.user
                     JOIN users s ON s.id = e.sender
                     JOIN rooms r ON r.id = e.room
                     WHERE reader.mxid = ?1 AND r.room_id = ?2 AND e.event_id = ?3 AND l.hidden = 0
                       AND e.class IN ('msg', 'general')"
                ))
                .map_err(db_error)?
                .query_row(params![user_id, room_id, event_id], event_row_from_sql)
                .optional()
                .map_err(db_error)?;
            if let Some(message) = match &row {
                Some(row) => self.to_message(user_id, row)?,
                None => None,
            } {
                messages.push(message);
            }
        }
        Ok(messages)
    }

    /// 一列 → 顯示用的 `Message`（§7.5 的讀取順序）：
    ///
    /// 1. `is_redacted` → 已刪除
    /// 2. 還沒解開（general）→ 解不開的記號
    /// 3. 自己的 `ref_event_id` 是 NULL → 自己的 `content_json`；不是 → **以被指的那個 edit 為準**：
    ///    讀者還沒同步到 → 過時；讀者 hide 了它 → 整則不出現；被 redact → 已刪除；否則換成它的 `m.new_content`
    ///
    /// Return:
    ///     Ok(Some(Message))
    ///     Ok(None)            不出現：目前的 edit 被這個讀者 hide 了；或壞列（class 認不得、msg 沒有 content_json 或解不開 JSON，§1）
    fn to_message(&self, user_id: &str, row: &EventRow) -> Result<Option<Message>, SdkError> {
        let Some(class) = EventClass::from_column(&row.class) else {
            return Ok(None);
        };
        let raw_event: Option<serde_json::Value> = row
            .raw_event
            .as_deref()
            .and_then(|raw| serde_json::from_str(raw).ok());
        let mut message = Message {
            id: row.event_id.clone(),
            conversation: row.room_id.clone(),
            sender: row.sender.clone(),
            sent_at: row.origin_server_ts.max(0) as u64,
            kind: MessageKind::Undecryptable,
            reply_to: None,
            edited_by: None,
            reactions: Vec::new(),
            decrypted: row.decrypted.map(|flag| flag == 1),
            undecryptable_reason: None,
            r_seq: row.r_seq,
            g_seq: row.g_seq,
        };
        if row.is_redacted {
            message.kind = MessageKind::Deleted {
                reason: self.find_redaction_reason(row.room, &row.event_id, raw_event.as_ref())?,
            };
            return Ok(Some(message));
        }
        if class != EventClass::Msg {
            // general：還沒解開的密文。
            message.decrypted = Some(false);
            message.undecryptable_reason = Some(NOT_DECRYPTED_HERE.into());
            return Ok(Some(message));
        }
        let (Some(event_type), Some(own_content)) = (
            row.event_type.as_deref(),
            row.content_json
                .as_deref()
                .and_then(|body| serde_json::from_str::<serde_json::Value>(body).ok()),
        ) else {
            return Ok(None);
        };
        // ⭐ 這則自己的 content_json 永遠不改；被 edit 過就在這裡換成目前那個 edit 的（§7.5）。
        let content = match self.find_current_edit(user_id, row)? {
            CurrentEdit::NotEdited => own_content,
            CurrentEdit::Visible {
                new_content,
                editor,
            } => {
                message.edited_by = Some(editor);
                to_replaced_body(&own_content, &new_content)
            }
            CurrentEdit::NotSynced => {
                message.kind = MessageKind::Outdated;
                return Ok(Some(message));
            }
            CurrentEdit::Hidden => return Ok(None),
            CurrentEdit::Redacted { reason } => {
                message.kind = MessageKind::Deleted { reason };
                return Ok(Some(message));
            }
        };
        // system_line 要 sender 與 state_key：狀態事件從不加密，所以 raw_event 一定在；沒有就只給 sender。
        let envelope = raw_event.unwrap_or_else(|| serde_json::json!({ "sender": row.sender }));
        message.kind = kind_from_content(event_type, &content, &envelope);
        message.reply_to = content
            .get("m.relates_to")
            .and_then(|relates| relates.get("m.in_reply_to"))
            .and_then(|reply| reply.get("event_id"))
            .and_then(|event_id| event_id.as_str())
            .map(str::to_string);
        message.reactions = self.list_reactions(user_id, row)?;
        Ok(Some(message))
    }

    /// 訊息自己的 `ref_event_id` 指的那個 edit（寫入時已經驗過、選過最新的）。**有參照就以參照物為準**（維護者 2026-09-14）。
    ///
    /// 🚨 消費端再問一次（A6）：那一列必須**真的是指回這則的 edit、同一個 sender**，
    /// 對不上就當沒被 edit 過，顯示原文 —— 🚫 不因為寫入端的一個 bug 就把別人的內容顯示成這則。
    /// 對得上之後照這個順序看那一列：
    ///
    /// 1. 這個帳號沒同步過 → 還沒同步到，版本過時（可見性跟 events 表完全一致）
    /// 2. 這個帳號 hide 了它（Delete for me）→ 這則跟著隱藏
    /// 3. 被 redact → 已刪除。📎 正常流程裡 redact 掉目前的 edit 會重設指標（`apply_redaction`），這條是防線
    ///
    /// Args:
    ///     user_id: 讀者, example: "@alice:localhost"
    ///     row: 要顯示的那則訊息
    /// Return:
    ///     Ok(CurrentEdit::NotEdited)          沒被 edit 過，或指到的那列對不上
    ///     Ok(CurrentEdit::NotSynced)          讀者沒同步過那個 edit
    ///     Ok(CurrentEdit::Hidden)             讀者 hide 了那個 edit
    ///     Ok(CurrentEdit::Redacted { reason }) 那個 edit 被 redact 了
    ///     Ok(CurrentEdit::Visible { .. })     照常換成它的內容
    fn find_current_edit(&self, user_id: &str, row: &EventRow) -> Result<CurrentEdit, SdkError> {
        let Some(edit_event_id) = row.ref_event_id.as_deref() else {
            return Ok(CurrentEdit::NotEdited);
        };
        // hidden：NULL ＝ 讀者沒有同步紀錄；0／1 ＝ 有紀錄、hide 了沒。
        let found: Option<PointedEditRow> = self
            .connection
            .prepare_cached(
                "SELECT x.content_json, x.raw_event, x.is_redacted,
                        (SELECT l.hidden FROM events_synced_log l JOIN users reader ON reader.id = l.user
                         WHERE l.event = x.id AND reader.mxid = ?5)
                 FROM events x
                 WHERE x.room = ?1 AND x.event_id = ?2 AND x.class = 'edit' AND x.ref_event_id = ?3
                   AND x.sender = ?4",
            )
            .map_err(db_error)?
            .query_row(
                params![row.room, edit_event_id, row.event_id, row.sender_row, user_id],
                |sql_row| {
                    Ok(PointedEditRow {
                        content_json: sql_row.get(0)?,
                        raw_event: sql_row.get(1)?,
                        is_redacted: sql_row.get::<_, i64>(2)? == 1,
                        reader_hidden: sql_row.get(3)?,
                    })
                },
            )
            .optional()
            .map_err(db_error)?;
        let Some(PointedEditRow {
            content_json,
            raw_event: edit_raw_event,
            is_redacted,
            reader_hidden: hidden,
        }) = found
        else {
            return Ok(CurrentEdit::NotEdited);
        };
        match hidden {
            None => return Ok(CurrentEdit::NotSynced),
            Some(0) => {}
            // 0 以外的值都不是正面認得「沒 hide」：當 hide 了（fail closed）。
            Some(_) => return Ok(CurrentEdit::Hidden),
        }
        if is_redacted {
            let edit_raw_event: Option<serde_json::Value> = edit_raw_event
                .as_deref()
                .and_then(|raw| serde_json::from_str(raw).ok());
            return Ok(CurrentEdit::Redacted {
                reason: self.find_redaction_reason(
                    row.room,
                    edit_event_id,
                    edit_raw_event.as_ref(),
                )?,
            });
        }
        Ok(
            match content_json
                .as_deref()
                .and_then(|body| serde_json::from_str::<serde_json::Value>(body).ok())
            {
                Some(new_content) => CurrentEdit::Visible {
                    new_content,
                    editor: row.sender.clone(),
                },
                None => CurrentEdit::NotEdited,
            },
        )
    }

    /// 參照這則的 reaction，🚨 **只算這個帳號同步過、沒 hide 的**（跟事件本身同一條可見性規則），被 redact 的不算。
    fn list_reactions(&self, user_id: &str, row: &EventRow) -> Result<Vec<Reaction>, SdkError> {
        let mut statement = self
            .connection
            .prepare_cached(
                "SELECT x.content_json, u.mxid FROM events x
                 JOIN users u ON u.id = x.sender
                 JOIN events_synced_log l ON l.event = x.id
                 JOIN users reader ON reader.id = l.user
                 WHERE x.room = ?1 AND x.ref_event_id = ?2 AND x.class = 'reaction' AND x.is_redacted = 0
                   AND reader.mxid = ?3 AND l.hidden = 0
                 ORDER BY x.id",
            )
            .map_err(db_error)?;
        let rows = statement
            .query_map(params![row.room, row.event_id, user_id], |sql_row| {
                Ok((
                    sql_row.get::<_, Option<String>>(0)?,
                    sql_row.get::<_, String>(1)?,
                ))
            })
            .map_err(db_error)?;
        let mut reactions: Vec<Reaction> = Vec::new();
        for reaction_row in rows {
            let (body, by) = reaction_row.map_err(db_error)?;
            let Some(key) = body
                .as_deref()
                .and_then(|body| serde_json::from_str::<serde_json::Value>(body).ok())
                .and_then(|body| {
                    body.get("m.relates_to")
                        .and_then(|relates| relates.get("key"))
                        .and_then(|key| key.as_str())
                        .map(str::to_string)
                })
            else {
                continue;
            };
            match reactions.iter_mut().find(|reaction| reaction.key == key) {
                Some(reaction) => reaction.by.push(by),
                None => reactions.push(Reaction { key, by: vec![by] }),
            }
        }
        Ok(reactions)
    }

    /// 刪除的理由：本地有 redact 事件就用它的 `reason`，否則看 server 蓋在原樣上的 `redacted_because`。
    fn find_redaction_reason(
        &self,
        room: i64,
        event_id: &str,
        raw_event: Option<&serde_json::Value>,
    ) -> Result<Option<String>, SdkError> {
        let redaction_body: Option<Option<String>> = self
            .connection
            .prepare_cached(
                "SELECT content_json FROM events WHERE room = ?1 AND ref_event_id = ?2 AND class = 'redact'
                 ORDER BY id LIMIT 1",
            )
            .map_err(db_error)?
            .query_row(params![room, event_id], |sql_row| sql_row.get(0))
            .optional()
            .map_err(db_error)?;
        let reason_in = |content: Option<&serde_json::Value>| {
            content
                .and_then(|content| content.get("reason"))
                .and_then(|reason| reason.as_str())
                .map(str::to_string)
        };
        let from_redaction = redaction_body
            .flatten()
            .and_then(|body| serde_json::from_str::<serde_json::Value>(&body).ok());
        Ok(reason_in(from_redaction.as_ref()).or_else(|| {
            reason_in(
                raw_event
                    .and_then(|raw| raw.get("unsigned"))
                    .and_then(|unsigned| unsigned.get("redacted_because"))
                    .and_then(|because| because.get("content")),
            )
        }))
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

    /// 這份 `cache.db` 認得哪些帳號（`users` 列裡的權威 mxid）。
    ///
    /// 🚫 這裡只給資料，比對規則不在這一層：使用者打的字串怎麼對到這些值（大小寫、歧義
    /// 怎麼辦）是 CLI 的政策，不是快取的（PR #21 審查 salvia🔴）。
    ///
    /// Return:
    ///     Ok(Vec<String>)   完整 mxid，排序過；空的 DB 就是空的
    pub fn list_account_mxids(&self) -> Result<Vec<String>, SdkError> {
        let mut statement = self
            .connection
            .prepare("SELECT mxid FROM users ORDER BY mxid")
            .map_err(db_error)?;
        let rows = statement
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(db_error)?;
        rows.collect::<Result<Vec<String>, _>>().map_err(db_error)
    }

    /// 摧毀這個帳號的本機紀錄（UI 的選項；`logout` 不叫它）。🚫 不刪 `users` 列：他可能是別人事件的 sender。
    /// 一個 transaction：刪他的關聯列 → 沒人同步過的事件 → 沒事件指的媒體 → 沒事件也沒清單的房間。
    ///
    /// ⚠️ `user_id` 要是 `users` 列裡那串**權威** mxid（精確比對）。使用者打進來的字串
    /// 先過 `list_account_mxids` 加 `accounts::find_matching_plaintext`——不然大小寫差一個字
    /// 就查不到，這個函數會老實回全零，而呼叫者會以為「本來就沒有」（PR #21 審查 salvia🔴）。
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

/// `SELECT` 一列事件時的欄位順序；跟 `event_row_from_sql` 一起改。
const EVENT_ROW_COLUMNS: &str =
    "e.id, e.room, e.event_id, r.room_id, e.sender, s.mxid, e.origin_server_ts, e.r_seq, e.g_seq,
     e.decrypted, e.raw_event, e.event_type, e.content_json, e.is_redacted, e.class, e.ref_event_id";

/// [`Cache::find_current_edit`] 讀到的那個 edit 列。
struct PointedEditRow {
    content_json: Option<String>,
    raw_event: Option<String>,
    is_redacted: bool,
    /// None ＝ 讀者沒有同步紀錄；Some(0／1) ＝ 有紀錄、hide 了沒。
    reader_hidden: Option<i64>,
}

/// [`Cache::find_current_edit`] 的答案。
enum CurrentEdit {
    NotEdited,
    Visible {
        new_content: serde_json::Value,
        editor: String,
    },
    NotSynced,
    Hidden,
    Redacted {
        reason: Option<String>,
    },
}

struct EventRow {
    room: i64,
    event_id: String,
    room_id: String,
    sender_row: i64,
    sender: String,
    origin_server_ts: i64,
    r_seq: Option<i64>,
    g_seq: Option<i64>,
    decrypted: Option<i64>,
    raw_event: Option<String>,
    event_type: Option<String>,
    content_json: Option<String>,
    is_redacted: bool,
    class: String,
    /// msg：目前要顯示的 edit；關係事件：目標。
    ref_event_id: Option<String>,
}

fn event_row_from_sql(row: &rusqlite::Row<'_>) -> rusqlite::Result<EventRow> {
    Ok(EventRow {
        room: row.get(1)?,
        event_id: row.get(2)?,
        room_id: row.get(3)?,
        sender_row: row.get(4)?,
        sender: row.get(5)?,
        origin_server_ts: row.get(6)?,
        r_seq: row.get(7)?,
        g_seq: row.get(8)?,
        decrypted: row.get(9)?,
        raw_event: row.get(10)?,
        event_type: row.get(11)?,
        content_json: row.get(12)?,
        is_redacted: row.get::<_, i64>(13)? == 1,
        class: row.get(14)?,
        ref_event_id: row.get(15)?,
    })
}

// ---- §7 的處理（都在寫入的 transaction 裡） ----

/// 狀態事件（有 `state_key`）。狀態事件從不加密，所以看原樣就夠；原樣是 NULL（matrix-sdk 解開的）就一定不是。
fn is_state_event(raw_event: Option<&serde_json::Value>) -> bool {
    raw_event.is_some_and(|raw| raw.get("state_key").is_some())
}

/// 驗 edit、比新舊時要知道的一列。
struct EventFacts {
    id: i64,
    event_id: String,
    sender: String,
    origin_server_ts: i64,
    decrypted: Option<i64>,
    is_state: bool,
    event_type: Option<String>,
    class: Option<EventClass>,
    is_redacted: bool,
    modified_timestamp: i64,
    ref_event_id: Option<String>,
}

impl EventFacts {
    fn replacement_side(&self) -> ReplacementSide<'_> {
        ReplacementSide {
            sender: &self.sender,
            // 認不得的 class 當 General：不是 msg 也不是 edit，check_replacement 一律拒絕。
            class: self.class.unwrap_or(EventClass::General),
            event_type: self.event_type.as_deref(),
            is_state: self.is_state,
            was_encrypted: self.decrypted.is_some(),
        }
    }
}

fn list_event_facts(
    transaction: &Transaction<'_>,
    condition: &str,
    parameters: impl rusqlite::Params,
) -> Result<Vec<EventFacts>, SdkError> {
    let mut statement = transaction
        .prepare_cached(&format!(
            "SELECT e.id, e.event_id, u.mxid, e.origin_server_ts, e.decrypted, e.raw_event, e.event_type, e.class,
                    e.is_redacted, e.modified_timestamp, e.ref_event_id
             FROM events e JOIN users u ON u.id = e.sender WHERE {condition}"
        ))
        .map_err(db_error)?;
    let rows = statement
        .query_map(parameters, |row| {
            let raw_event: Option<String> = row.get(5)?;
            Ok(EventFacts {
                id: row.get(0)?,
                event_id: row.get(1)?,
                sender: row.get(2)?,
                origin_server_ts: row.get(3)?,
                decrypted: row.get(4)?,
                is_state: is_state_event(
                    raw_event
                        .as_deref()
                        .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
                        .as_ref(),
                ),
                event_type: row.get(6)?,
                class: EventClass::from_column(&row.get::<_, String>(7)?),
                is_redacted: row.get::<_, i64>(8)? == 1,
                modified_timestamp: row.get(9)?,
                ref_event_id: row.get(10)?,
            })
        })
        .map_err(db_error)?;
    let collected: Vec<EventFacts> = rows.collect::<rusqlite::Result<_>>().map_err(db_error)?;
    Ok(collected)
}

fn find_event_facts(
    transaction: &Transaction<'_>,
    condition: &str,
    parameters: impl rusqlite::Params,
) -> Result<Option<EventFacts>, SdkError> {
    Ok(list_event_facts(transaction, condition, parameters)?
        .into_iter()
        .next())
}

fn list_waiting_relations(
    transaction: &Transaction<'_>,
    room: i64,
    target_event_id: &str,
    class: EventClass,
) -> Result<Vec<i64>, SdkError> {
    let mut statement = transaction
        .prepare_cached(
            "SELECT id FROM events WHERE room = ?1 AND ref_event_id = ?2 AND class = ?3 AND is_processed = 0",
        )
        .map_err(db_error)?;
    let rows = statement
        .query_map(params![room, target_event_id, class.to_column()], |row| {
            row.get(0)
        })
        .map_err(db_error)?;
    let collected: Vec<i64> = rows.collect::<rusqlite::Result<_>>().map_err(db_error)?;
    Ok(collected)
}

fn set_processed(transaction: &Transaction<'_>, event: i64) -> Result<(), SdkError> {
    transaction
        .prepare_cached("UPDATE events SET is_processed = 1 WHERE id = ?1")
        .map_err(db_error)?
        .execute(params![event])
        .map_err(db_error)?;
    Ok(())
}

fn set_current_edit(
    transaction: &Transaction<'_>,
    target: i64,
    edit_event_id: Option<&str>,
    modified_timestamp: i64,
) -> Result<(), SdkError> {
    transaction
        .prepare_cached("UPDATE events SET ref_event_id = ?2, modified_timestamp = ?3 WHERE id = ?1 AND class = 'msg'")
        .map_err(db_error)?
        .execute(params![target, edit_event_id, modified_timestamp])
        .map_err(db_error)?;
    Ok(())
}

/// 剛寫進來（或剛解開）的一則（§7.4）。🚫 **不改任何一列的 `content_json`**。
///
/// - msg／edit 的內容是約定 §5 的檔 → 建 `media` 與 `event_media`（edit 的檔掛在 edit 自己那列）
/// - 等著這則的 redact、edit（先到的）→ 現在處理
/// - 自己是 edit／redact → 目標在就處理，不在就等
fn process_event(transaction: &Transaction<'_>, room: i64, event: i64) -> Result<(), SdkError> {
    let Some((event_id, class, event_type, content_json)) = transaction
        .prepare_cached(
            "SELECT event_id, class, event_type, content_json FROM events WHERE id = ?1",
        )
        .map_err(db_error)?
        .query_row(params![event], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
            ))
        })
        .optional()
        .map_err(db_error)?
    else {
        return Ok(());
    };
    let class = EventClass::from_column(&class);
    if matches!(class, Some(EventClass::Msg) | Some(EventClass::Edit)) {
        link_media_of(
            transaction,
            event,
            event_type.as_deref(),
            content_json.as_deref(),
        )?;
    }
    // redact 先（§7.6）：打勾之後，等著的 edit 都會跳過。
    for redaction in list_waiting_relations(transaction, room, &event_id, EventClass::Redact)? {
        apply_redaction(transaction, room, redaction)?;
    }
    for edit in list_waiting_relations(transaction, room, &event_id, EventClass::Edit)? {
        apply_edit(transaction, room, edit)?;
    }
    match class {
        Some(EventClass::Redact) => apply_redaction(transaction, room, event)?,
        Some(EventClass::Edit) => apply_edit(transaction, room, event)?,
        _ => {}
    }
    Ok(())
}

/// edit（§7.5）：目標在本地、沒被 redact、這個 edit 有效、**比目前的新** → 目標的 `ref_event_id` 換成它、
/// `modified_timestamp` 換成它的 server 時間；否則跳過。不管換不換，這個 edit 都標處理過（目標不在或還沒解開除外）。
///
/// 「比目前的新」一律看 `modified_timestamp`：`(edit 的 origin_server_ts, event_id) > (modified_timestamp, ref_event_id)`。
/// 訊息寫入時 `modified_timestamp = 0`，所以第一個 edit 不管時間戳多少都會贏（維護者 2026-09-14）。
fn apply_edit(transaction: &Transaction<'_>, room: i64, edit: i64) -> Result<(), SdkError> {
    let Some(edit) = find_event_facts(
        transaction,
        "e.id = ?1 AND e.class = 'edit' AND e.is_processed = 0",
        params![edit],
    )?
    else {
        return Ok(());
    };
    let Some(target_event_id) = edit.ref_event_id.as_deref() else {
        return set_processed(transaction, edit.id);
    };
    let Some(target) = find_event_facts(
        transaction,
        "e.room = ?1 AND e.event_id = ?2",
        params![room, target_event_id],
    )?
    else {
        return Ok(());
    };
    if target.class == Some(EventClass::General) {
        // 目標還沒解開：驗不了 type／加密，等它解開（process_event 會再叫）。
        return Ok(());
    }
    if target.is_redacted
        || edit.is_redacted
        || check_replacement(&target.replacement_side(), &edit.replacement_side()).is_err()
    {
        return set_processed(transaction, edit.id);
    }
    // 一律看 modified_timestamp（寫入時是 0，所以第一個 edit 一定贏）；平手比 event_id，目前沒有 edit 的 None 最小。
    let is_newer = (edit.origin_server_ts, Some(edit.event_id.as_str()))
        > (target.modified_timestamp, target.ref_event_id.as_deref());
    if is_newer {
        set_current_edit(
            transaction,
            target.id,
            Some(&edit.event_id),
            edit.origin_server_ts,
        )?;
    }
    set_processed(transaction, edit.id)
}

/// redact（§7.6）：目標在本地就打勾 `is_redacted`、`modified_timestamp` 換成 redact 的 server 時間，這個 redact 標處理過；
/// 目標還不在就留著 `is_processed = 0` 等它到。
/// 🚫 不動目標的 `raw_event`、`content_json`：密文與明文都還在，可以復原（§7.2）。
/// ⭐ 目標還沒解開（general）也照打勾：刪不刪跟看不看得懂無關。
/// ⭐ 被 redact 的是**某則訊息目前顯示的 edit** → 那則訊息退回剩下的有效 edit 裡最新的（沒有就顯示原文）。
fn apply_redaction(
    transaction: &Transaction<'_>,
    room: i64,
    redaction: i64,
) -> Result<(), SdkError> {
    let Some(redaction) = find_event_facts(
        transaction,
        "e.id = ?1 AND e.class = 'redact' AND e.is_processed = 0",
        params![redaction],
    )?
    else {
        return Ok(());
    };
    let Some(target_event_id) = redaction.ref_event_id.as_deref() else {
        return set_processed(transaction, redaction.id);
    };
    let Some(target) = find_event_facts(
        transaction,
        "e.room = ?1 AND e.event_id = ?2",
        params![room, target_event_id],
    )?
    else {
        return Ok(());
    };
    transaction
        .prepare_cached("UPDATE events SET is_redacted = 1, modified_timestamp = ?2 WHERE id = ?1")
        .map_err(db_error)?
        .execute(params![target.id, redaction.origin_server_ts])
        .map_err(db_error)?;
    set_processed(transaction, redaction.id)?;
    if target.class != Some(EventClass::Edit) {
        return Ok(());
    }
    let Some(edited_event_id) = target.ref_event_id.as_deref() else {
        return Ok(());
    };
    let Some(edited) = find_event_facts(
        transaction,
        "e.room = ?1 AND e.event_id = ?2 AND e.class = 'msg' AND e.ref_event_id = ?3",
        params![room, edited_event_id, target.event_id],
    )?
    else {
        return Ok(());
    };
    let candidates = list_event_facts(
        transaction,
        "e.room = ?1 AND e.ref_event_id = ?2 AND e.class = 'edit' AND e.is_redacted = 0
         ORDER BY e.origin_server_ts DESC, e.event_id DESC",
        params![room, edited.event_id],
    )?;
    let fallback = candidates.iter().find(|candidate| {
        check_replacement(&edited.replacement_side(), &candidate.replacement_side()).is_ok()
    });
    match fallback {
        Some(previous) => set_current_edit(
            transaction,
            edited.id,
            Some(&previous.event_id),
            previous.origin_server_ts,
        ),
        // 一個都不剩：回到沒被 edit 過的樣子（modified_timestamp 回 0），之後晚到的 edit 照樣能贏。
        None => set_current_edit(transaction, edited.id, None, 0),
    }
}

/// 內容是約定 §5 的檔：建 `media`（已有就不動）與 `event_media`。
fn link_media_of(
    transaction: &Transaction<'_>,
    event: i64,
    event_type: Option<&str>,
    content_json: Option<&str>,
) -> Result<(), SdkError> {
    let (Some(event_type), Some(body)) = (
        event_type,
        content_json.and_then(|body| serde_json::from_str::<serde_json::Value>(body).ok()),
    ) else {
        return Ok(());
    };
    let MessageKind::File { attachment, .. } =
        kind_from_content(event_type, &body, &serde_json::Value::Null)
    else {
        return Ok(());
    };
    let media = media_row_id(
        transaction,
        &attachment.mxc,
        attachment.block.name.as_deref(),
        attachment.block.mimetype.as_deref(),
        block_hash(attachment.block.sha256.as_deref()).as_deref(),
        attachment.block.file_size.unwrap_or(0),
        attachment.block.chunk_size,
    )?;
    transaction
        .prepare_cached("INSERT OR IGNORE INTO event_media (event, media) VALUES (?1, ?2)")
        .map_err(db_error)?
        .execute(params![event, media])
        .map_err(db_error)?;
    Ok(())
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
             CREATE TABLE rooms (id INTEGER PRIMARY KEY, room_id TEXT NOT NULL UNIQUE, first_seen_at INTEGER NOT NULL,
               encrypted INTEGER CHECK (encrypted IN (0, 1)));
             CREATE TABLE events (
               id INTEGER PRIMARY KEY,
               room INTEGER NOT NULL REFERENCES rooms(id) ON DELETE CASCADE,
               event_id TEXT NOT NULL,
               sender INTEGER NOT NULL REFERENCES users(id),
               r_seq INTEGER, g_seq INTEGER, origin_server_ts INTEGER NOT NULL,
               decrypted INTEGER CHECK (decrypted IN (0, 1)),
               raw_event TEXT,
               event_type TEXT,
               content_json TEXT,
               is_processed INTEGER NOT NULL DEFAULT 0 CHECK (is_processed IN (0, 1)),
               is_redacted INTEGER NOT NULL DEFAULT 0 CHECK (is_redacted IN (0, 1)),
               class TEXT NOT NULL DEFAULT 'general' CHECK (class IN ('general', 'msg', 'edit', 'redact', 'reaction')),
               ref_event_id TEXT,
               modified_timestamp INTEGER NOT NULL DEFAULT 0);
             CREATE UNIQUE INDEX events_by_event_id ON events (room, event_id);
             CREATE UNIQUE INDEX events_by_seq ON events (room, r_seq) WHERE r_seq IS NOT NULL;
             CREATE INDEX events_by_time ON events (room, origin_server_ts);
             CREATE INDEX events_by_ref ON events (room, ref_event_id) WHERE ref_event_id IS NOT NULL;
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
    use crate::chat::ConversationKind;

    const ALICE: &str = "@alice:localhost";
    const BOB: &str = "@bob:localhost";
    const CAROL: &str = "@carol:localhost";

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

    /// 一則要寫進去的事件與它的房間（`upsert_events` 一次一個房間）。
    #[derive(Clone)]
    struct Fixture {
        room: String,
        event: IncomingEvent,
    }

    fn put(cache: &mut Cache, user: &str, fixtures: &[Fixture]) {
        for fixture in fixtures {
            cache
                .upsert_events(user, &fixture.room, std::slice::from_ref(&fixture.event))
                .unwrap();
        }
    }

    fn event_json(
        room: &str,
        id: &str,
        sender: &str,
        r_seq: Option<i64>,
        ts: u64,
        content: serde_json::Value,
    ) -> serde_json::Value {
        let mut event = serde_json::json!({
            "type": "m.room.message", "event_id": id, "room_id": room, "sender": sender,
            "origin_server_ts": ts, "content": content,
        });
        if let Some(r_seq) = r_seq {
            event["unsigned"] = serde_json::json!({
                crate::protocol::R_SEQ_KEY: r_seq, crate::protocol::G_SEQ_KEY: r_seq * 10,
            });
        }
        event
    }

    fn text(room: &str, id: &str, r_seq: Option<i64>, ts: u64) -> Fixture {
        Fixture {
            room: room.into(),
            event: IncomingEvent::Plain {
                event: event_json(
                    room,
                    id,
                    CAROL,
                    r_seq,
                    ts,
                    serde_json::json!({ "msgtype": "m.text", "body": format!("body {id}") }),
                ),
            },
        }
    }

    fn file(room: &str, id: &str, r_seq: i64, mxc: &str) -> Fixture {
        Fixture {
            room: room.into(),
            event: IncomingEvent::Plain {
                event: event_json(
                    room,
                    id,
                    CAROL,
                    Some(r_seq),
                    1000 + r_seq as u64,
                    serde_json::json!({
                        "msgtype": FILE_MSGTYPE, "body": "a.txt", "url": mxc,
                        crate::event_json::CHUNKED_BLOCK_KEY: {
                            "v": 1, "cipher": "none", "chunk_size": 65536, "file_size": 10, "name": "a.txt"
                        }
                    }),
                ),
            },
        }
    }

    fn ids(messages: &[Message]) -> Vec<String> {
        messages.iter().map(|message| message.id.clone()).collect()
    }

    /// 🚨 翻頁的錨**只認這個帳號看得到的事件**：BOB 同步進來、ALICE 從沒看過的，
    /// 🚫 不准給 ALICE 當錨 —— 那等於用 BOB 的可見範圍替 ALICE 定位，還洩漏那則事件存在。
    #[test]
    fn an_event_only_another_account_synced_cannot_be_used_as_a_paging_anchor() {
        let (mut cache, dir) = open("position-visibility");
        put(&mut cache, BOB, &[text("!r", "$bobs", Some(7), 7)]);

        assert_eq!(
            cache.find_event_position(BOB, "!r", "$bobs").unwrap(),
            Some(EventPosition {
                r_seq: Some(7),
                g_seq: Some(70),
            })
        );
        assert_eq!(
            cache.find_event_position(ALICE, "!r", "$bobs").unwrap(),
            None,
            "🚫 ALICE 沒看過就沒有這個錨"
        );
        assert_eq!(
            cache.find_event_position(BOB, "!other", "$bobs").unwrap(),
            None,
            "🚫 別的房間的同名 id 也不算"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ⭐ 從本地讀回「上游那一頁」：**順序照給的**（🚫 不靠 `r_seq`、🚫 不靠時間戳），
    /// 本地沒有的、`hidden` 的、別的帳號的都不出來。
    #[test]
    fn messages_read_back_by_event_id_keep_the_upstream_order_and_local_rules() {
        let (mut cache, dir) = open("by-event-ids");
        // 非 fork server 的事件：沒有 r_seq，而且時間戳故意跟上游順序相反。
        put(
            &mut cache,
            ALICE,
            &[
                text("!r", "$older", None, 900),
                text("!r", "$newer", None, 100),
                text("!r", "$hidden", None, 500),
            ],
        );
        cache.hide_message(ALICE, "!r", "$hidden").unwrap();
        put(&mut cache, BOB, &[text("!r", "$bob-only", None, 1)]);

        let upstream_order =
            ["$newer", "$hidden", "$missing", "$bob-only", "$older"].map(str::to_string);
        let read_back = cache
            .list_messages_by_event_ids(ALICE, "!r", &upstream_order)
            .unwrap();
        assert_eq!(
            ids(&read_back),
            ["$newer", "$older"],
            "照上游的順序；hidden、本地沒有、別的帳號的都不在"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn room(id: &str, encrypted: bool) -> Conversation {
        Conversation {
            id: id.into(),
            kind: ConversationKind::Group,
            name: Some(id.into()),
            topic: None,
            encrypted,
            member_count: 2,
            my_power_level: 0,
            can_send_message: true,
            direct_peer: None,
        }
    }

    fn column_encrypted(cache: &Cache, room_id: &str) -> Option<bool> {
        cache
            .connection
            .query_row(
                "SELECT encrypted FROM rooms WHERE room_id = ?1",
                params![room_id],
                |row| row.get(0),
            )
            .unwrap()
    }

    /// 🚨 **加密只准升、不准降**：Matrix 房間一開加密就關不掉，所以一份過期的「沒加密」
    /// 🚫 不准把已知加密的房間蓋回明文 —— 蓋回去的下一步是用 `cipher: none` 把區塊金鑰公開送出。
    #[test]
    fn a_room_once_known_encrypted_is_never_downgraded_to_plaintext() {
        let (mut cache, dir) = open("encrypted-ratchet");
        cache
            .upsert_conversations(ALICE, &[room("!r", true)])
            .unwrap();
        assert_eq!(column_encrypted(&cache, "!r"), Some(true));

        // 同一個帳號之後又拿到一份說「沒加密」的（過期、或有 bug）。
        cache
            .upsert_conversations(ALICE, &[room("!r", false)])
            .unwrap();
        assert_eq!(
            column_encrypted(&cache, "!r"),
            Some(true),
            "🚫 不准降回明文"
        );
        assert!(
            cache.list_conversations(ALICE).unwrap()[0].encrypted,
            "讀出來也要是加密"
        );

        // 明文房間升級成加密：可以。
        cache
            .upsert_conversations(ALICE, &[room("!plain", false)])
            .unwrap();
        assert_eq!(column_encrypted(&cache, "!plain"), Some(false));
        cache
            .upsert_conversations(ALICE, &[room("!plain", true)])
            .unwrap();
        assert_eq!(column_encrypted(&cache, "!plain"), Some(true));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ⭐ 加密是**房間的**事實、不是「某個帳號上次看到的」—— 所以 B 帳號那份舊的
    /// `conversation_json` 說沒加密，讀出來仍然要是加密（A 已經確認過了）。
    #[test]
    fn another_accounts_stale_view_cannot_hide_that_a_room_is_encrypted() {
        let (mut cache, dir) = open("encrypted-across-accounts");
        cache
            .upsert_conversations(BOB, &[room("!r", false)])
            .unwrap();
        cache
            .upsert_conversations(ALICE, &[room("!r", true)])
            .unwrap();

        let bob_sees = cache.list_conversations(BOB).unwrap();
        assert!(
            bob_sees[0].encrypted,
            "🚨 BOB 的 JSON 是舊的，但房間已知加密 —— 讀出來不准是明文"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 只因為**收到事件**才建出來的房間列：**不知道**加不加密（NULL），🚫 不是明文（0）。
    #[test]
    fn a_room_seen_only_through_events_has_unknown_encryption_not_plaintext() {
        let (mut cache, dir) = open("encrypted-unknown");
        put(&mut cache, ALICE, &[text("!only-events", "$1", Some(1), 1)]);
        assert_eq!(
            column_encrypted(&cache, "!only-events"),
            None,
            "🚫 不知道就是不知道——預設成 0 等於預設明文"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn open_creates_reuses_and_rebuilds_on_server_change_or_wrong_key() {
        let dir = scratch_dir("open");
        let key = Key32([1u8; 32]);
        let (mut cache, outcome) = Cache::open(&dir, &key, &identity()).unwrap();
        assert_eq!(outcome, OpenOutcome::Created);
        put(&mut cache, ALICE, &[text("!r", "$1", Some(1), 1)]);
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
        put(
            &mut cache,
            ALICE,
            &[
                text("!r", "$1", Some(1), 10),
                text("!r", "$2", Some(2), 20),
                text("!r", "$3", Some(3), 30),
            ],
        );
        put(
            &mut cache,
            BOB,
            &[
                text("!r", "$2", Some(2), 20),
                text("!r", "$3", Some(3), 30),
                text("!r", "$4", Some(4), 40),
            ],
        );
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
        put(
            &mut cache,
            ALICE,
            &[text("!p", "$p1", None, 100), text("!p", "$p2", None, 200)],
        );
        assert_eq!(
            ids(&cache.history(ALICE, "!p", None, 10).unwrap()),
            ["$p2", "$p1"]
        );
        assert_eq!(cache.room_stats("!p").unwrap(), (2, None));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn hide_is_per_user_and_survives_resync() {
        let (mut cache, dir) = open("hidden");
        let message = text("!r", "$1", Some(1), 1);
        put(&mut cache, ALICE, std::slice::from_ref(&message));
        put(&mut cache, BOB, std::slice::from_ref(&message));
        assert!(cache.hide_message(ALICE, "!r", "$1").unwrap());
        assert!(cache.history(ALICE, "!r", None, 10).unwrap().is_empty());
        assert_eq!(cache.history(BOB, "!r", None, 10).unwrap().len(), 1);
        // 再同步：只更新 last_synced_at，hidden 不動。
        put(&mut cache, ALICE, std::slice::from_ref(&message));
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
        put(&mut cache, ALICE, &[shared.clone(), alice_only.clone()]);
        put(&mut cache, BOB, std::slice::from_ref(&shared));
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
        // ⚠️ 大小寫差一個字也是「沒見過」——這裡是精確比對，所以呼叫端要先把使用者打的
        // 字串換成 users 列裡那串（`list_account_mxids` 加 `accounts::find_matching_plaintext`）。
        // 少了那一步，`account destroy` 會回全零、看起來像「本來就沒有」（PR #21 審查 salvia🔴）。
        assert_eq!(
            cache.forget_account("@ALICE:LocalHost").unwrap(),
            ForgetReport::default()
        );
        assert!(cache
            .list_account_mxids()
            .unwrap()
            .contains(&ALICE.to_string()));

        // 同 hash 兩個 mxc：只 forget 到其中一個 mxc 的事件時，池檔不回傳。
        let dup1 = file("!s", "$d1", 1, "mxc://localhost/c1");
        let dup2 = file("!s", "$d2", 2, "mxc://localhost/c2");
        put(&mut cache, ALICE, std::slice::from_ref(&dup1));
        put(&mut cache, BOB, std::slice::from_ref(&dup2));
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
        put(
            &mut cache,
            ALICE,
            &[file("!h", "$n", 1, "mxc://localhost/nohash")],
        );
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
        if let IncomingEvent::Plain { event } = &mut with_sha.event {
            event["content"][crate::event_json::CHUNKED_BLOCK_KEY]["sha256"] =
                serde_json::json!("ABCDEF");
        }
        put(&mut cache, ALICE, std::slice::from_ref(&with_sha));
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
        put(&mut cache, ALICE, &[text("!only-alice", "$1", Some(1), 1)]);
        put(&mut cache, ALICE, &[text("!shared", "$2", Some(1), 1)]);
        put(&mut cache, BOB, &[text("!shared", "$2", Some(1), 1)]);
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

        put(&mut cache, ALICE, &[text("!r", "$1", Some(1), 5)]);
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

    // ---- §7：原始事件不動、最終內容另存、ref_event_id 參照 ----

    fn plain(room: &str, event: serde_json::Value) -> Fixture {
        Fixture {
            room: room.into(),
            event: IncomingEvent::Plain { event },
        }
    }

    fn edit_of(
        room: &str,
        id: &str,
        sender: &str,
        target: &str,
        origin_server_ts: u64,
        body: &str,
    ) -> Fixture {
        let event = event_json(
            room,
            id,
            sender,
            None,
            origin_server_ts,
            serde_json::json!({
                "msgtype": "m.text", "body": format!("* {body}"),
                "m.new_content": { "msgtype": "m.text", "body": body },
                "m.relates_to": { "rel_type": "m.replace", "event_id": target },
            }),
        );
        plain(room, event)
    }

    fn redaction_of(room: &str, id: &str, target: &str) -> Fixture {
        plain(
            room,
            serde_json::json!({
                "type": "m.room.redaction", "event_id": id, "room_id": room, "sender": CAROL,
                "origin_server_ts": 9, "content": { "redacts": target, "reason": "spam" },
            }),
        )
    }

    fn reaction_of(room: &str, id: &str, sender: &str, target: &str, key: &str) -> Fixture {
        plain(
            room,
            serde_json::json!({
                "type": "m.reaction", "event_id": id, "room_id": room, "sender": sender, "origin_server_ts": 6,
                "content": { "m.relates_to": { "rel_type": "m.annotation", "event_id": target, "key": key } },
            }),
        )
    }

    fn body_of(cache: &Cache, user: &str, room: &str, id: &str) -> Option<String> {
        cache
            .list_messages_by_event_ids(user, room, &[id.to_string()])
            .unwrap()
            .into_iter()
            .next()
            .and_then(|message| match message.kind {
                MessageKind::Text { body, .. } => Some(body),
                _ => None,
            })
    }

    fn column_of<T: rusqlite::types::FromSql>(cache: &Cache, column: &str, id: &str) -> T {
        cache
            .connection
            .query_row(
                &format!("SELECT {column} FROM events WHERE event_id = ?1"),
                params![id],
                |row| row.get(0),
            )
            .unwrap()
    }

    /// 🚫 **raw_event 第一次寫入之後永遠不動**：同一則再來（內容不同也一樣）不覆蓋原樣、也不覆蓋顯示的內容。
    #[test]
    fn the_raw_event_is_written_once_and_never_overwritten() {
        let (mut cache, dir) = open("raw-once");
        let first = text("!r", "$e", Some(1), 1);
        put(&mut cache, ALICE, std::slice::from_ref(&first));
        let mut changed = first.clone();
        if let IncomingEvent::Plain { event } = &mut changed.event {
            event["content"]["body"] = serde_json::json!("tampered");
        }
        put(&mut cache, ALICE, &[changed]);
        let IncomingEvent::Plain { event } = &first.event else {
            unreachable!()
        };
        assert_eq!(
            column_of::<String>(&cache, "raw_event", "$e"),
            event.to_string()
        );
        assert_eq!(
            body_of(&cache, ALICE, "!r", "$e").as_deref(),
            Some("body $e")
        );
        assert_eq!(column_of::<String>(&cache, "class", "$e"), "msg");
        assert_eq!(column_of::<i64>(&cache, "is_processed", "$e"), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// matrix-sdk 解開的事件拿不到密文 → raw_event 是 NULL（維護者 2026-09-14），內容照樣在 content_json。
    #[test]
    fn sdk_decrypted_events_store_no_raw_event_but_show_their_cleartext() {
        let (mut cache, dir) = open("sdk-decrypted");
        let cleartext = event_json(
            "!r",
            "$d",
            CAROL,
            Some(1),
            1,
            serde_json::json!({ "msgtype": "m.text", "body": "secret" }),
        );
        cache
            .upsert_events(
                ALICE,
                "!r",
                &[IncomingEvent::Decrypted {
                    ciphertext: None,
                    cleartext,
                }],
            )
            .unwrap();
        assert_eq!(column_of::<Option<String>>(&cache, "raw_event", "$d"), None);
        let message = cache.history(ALICE, "!r", None, 1).unwrap().remove(0);
        assert_eq!(message.decrypted, Some(true));
        assert!(matches!(&message.kind, MessageKind::Text { body, .. } if body == "secret"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 還沒解開的：class general、content_json NULL、is_processed 0，顯示成解不開。之後明文到了才處理，
    /// 🚫 密文照樣留在 raw_event；🚫 再來一次密文不會把明文蓋回去。
    #[test]
    fn an_undecrypted_event_is_processed_when_its_cleartext_arrives_and_the_ciphertext_stays() {
        let (mut cache, dir) = open("general");
        let ciphertext = serde_json::json!({
            "type": "m.room.encrypted", "event_id": "$c", "room_id": "!r", "sender": CAROL,
            "origin_server_ts": 1, "content": { "algorithm": "m.megolm.v1.aes-sha2", "ciphertext": "AAA" },
        });
        let undecrypted = IncomingEvent::Undecrypted {
            ciphertext: ciphertext.clone(),
            reason: "MissingRoomKey".into(),
        };
        cache
            .upsert_events(BOB, "!r", std::slice::from_ref(&undecrypted))
            .unwrap();
        assert_eq!(column_of::<String>(&cache, "class", "$c"), "general");
        assert_eq!(
            column_of::<Option<String>>(&cache, "content_json", "$c"),
            None
        );
        assert_eq!(column_of::<i64>(&cache, "is_processed", "$c"), 0);
        let shown = cache.history(BOB, "!r", None, 1).unwrap().remove(0);
        assert_eq!(shown.decrypted, Some(false));
        assert!(matches!(shown.kind, MessageKind::Undecryptable));

        let cleartext = event_json(
            "!r",
            "$c",
            CAROL,
            None,
            1,
            serde_json::json!({ "msgtype": "m.text", "body": "hi" }),
        );
        cache
            .upsert_events(
                ALICE,
                "!r",
                &[IncomingEvent::Decrypted {
                    ciphertext: None,
                    cleartext,
                }],
            )
            .unwrap();
        cache.upsert_events(BOB, "!r", &[undecrypted]).unwrap();
        assert_eq!(
            column_of::<String>(&cache, "raw_event", "$c"),
            ciphertext.to_string(),
            "密文還在"
        );
        assert_eq!(
            body_of(&cache, BOB, "!r", "$c").as_deref(),
            Some("hi"),
            "明文不被密文蓋回去"
        );
        assert_eq!(column_of::<Option<i64>>(&cache, "decrypted", "$c"), Some(1));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// edit 後到：顯示的內容換成 new_content，🚨 回覆關係留原本的；edit 自己不出現在歷史裡。
    /// 🚫 目標的 raw_event 與 content_json 都不動。
    #[test]
    fn an_edit_changes_what_is_shown_but_not_the_targets_row() {
        let (mut cache, dir) = open("edit-after");
        let target = plain(
            "!r",
            event_json(
                "!r",
                "$t",
                CAROL,
                Some(1),
                1,
                serde_json::json!({
                    "msgtype": "m.text", "body": "hi", "m.relates_to": { "m.in_reply_to": { "event_id": "$q" } },
                }),
            ),
        );
        let target_content: String = {
            put(&mut cache, ALICE, std::slice::from_ref(&target));
            column_of(&cache, "content_json", "$t")
        };
        put(
            &mut cache,
            ALICE,
            &[edit_of("!r", "$e", CAROL, "$t", 20, "hello")],
        );
        let history = cache.history(ALICE, "!r", None, 10).unwrap();
        assert_eq!(ids(&history), ["$t"], "edit 不是一則獨立的訊息");
        assert!(matches!(&history[0].kind, MessageKind::Text { body, .. } if body == "hello"));
        assert_eq!(history[0].reply_to.as_deref(), Some("$q"));
        assert_eq!(history[0].edited_by.as_deref(), Some(CAROL));
        assert_eq!(
            column_of::<String>(&cache, "content_json", "$t"),
            target_content,
            "目標那列的明文不動"
        );
        assert!(column_of::<String>(&cache, "raw_event", "$t").contains("\"hi\""));
        assert_eq!(
            column_of::<Option<String>>(&cache, "ref_event_id", "$e").as_deref(),
            Some("$t")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ⭐ edit 先到、目標後到：什麼都不用補 —— 讀取時一查就有（§7.5）。
    #[test]
    fn an_edit_that_arrives_before_its_target_shows_once_the_target_lands() {
        let (mut cache, dir) = open("edit-before");
        put(
            &mut cache,
            ALICE,
            &[edit_of("!r", "$e", CAROL, "$t", 20, "hello")],
        );
        assert!(cache.history(ALICE, "!r", None, 10).unwrap().is_empty());
        put(&mut cache, ALICE, &[text("!r", "$t", Some(1), 1)]);
        assert_eq!(body_of(&cache, ALICE, "!r", "$t").as_deref(), Some("hello"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 🚨 資安：別人不能改你的訊息；明文 edit 不能蓋加密訊息（spec 的有效性規則）。
    /// 而且無效的 edit 🚫 不會讓訊息被標成「改過」。
    #[test]
    fn invalid_edits_are_ignored() {
        let (mut cache, dir) = open("edit-invalid");
        put(
            &mut cache,
            ALICE,
            &[
                text("!r", "$t", Some(1), 1),
                edit_of("!r", "$m", "@mallory:localhost", "$t", 20, "pwned"),
            ],
        );
        assert_eq!(
            body_of(&cache, ALICE, "!r", "$t").as_deref(),
            Some("body $t")
        );
        assert_eq!(
            cache.history(ALICE, "!r", None, 1).unwrap()[0].edited_by,
            None
        );

        let cleartext = event_json(
            "!r",
            "$secret",
            CAROL,
            Some(2),
            2,
            serde_json::json!({ "msgtype": "m.text", "body": "secret" }),
        );
        cache
            .upsert_events(
                ALICE,
                "!r",
                &[IncomingEvent::Decrypted {
                    ciphertext: None,
                    cleartext,
                }],
            )
            .unwrap();
        put(
            &mut cache,
            ALICE,
            &[edit_of(
                "!r",
                "$plain-edit",
                CAROL,
                "$secret",
                30,
                "overwritten",
            )],
        );
        assert_eq!(
            body_of(&cache, ALICE, "!r", "$secret").as_deref(),
            Some("secret")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ⭐ 最新的贏：比 edit 自己的 `origin_server_ts`，不管到的順序、也不管有沒有 g_seq（這些 edit 都沒有）。
    #[test]
    fn the_newest_edit_by_server_time_wins_whatever_the_arrival_order() {
        let (mut cache, dir) = open("edit-newest");
        put(
            &mut cache,
            ALICE,
            &[
                text("!r", "$t", None, 1),
                edit_of("!r", "$new", CAROL, "$t", 30, "newest"),
                edit_of("!r", "$old", CAROL, "$t", 20, "older"),
            ],
        );
        assert_eq!(
            body_of(&cache, ALICE, "!r", "$t").as_deref(),
            Some("newest")
        );

        put(
            &mut cache,
            ALICE,
            &[
                text("!r", "$u", None, 2),
                edit_of("!r", "$old2", CAROL, "$u", 20, "older"),
                edit_of("!r", "$new2", CAROL, "$u", 30, "newest"),
            ],
        );
        assert_eq!(
            body_of(&cache, ALICE, "!r", "$u").as_deref(),
            Some("newest")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 同一毫秒：`event_id` 字典序大的算新 —— 不管寫入順序，答案固定。
    #[test]
    fn edits_with_the_same_server_time_are_ordered_by_event_id() {
        let (mut cache, dir) = open("edit-tie");
        put(
            &mut cache,
            ALICE,
            &[
                text("!r", "$t", None, 1),
                edit_of("!r", "$b", CAROL, "$t", 20, "from b"),
                edit_of("!r", "$a", CAROL, "$t", 20, "from a"),
            ],
        );
        assert_eq!(
            body_of(&cache, ALICE, "!r", "$t").as_deref(),
            Some("from b")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ⭐ redact 掉最新的 edit，自然退回上一個；全部 redact 掉就是原文（目標那列從沒被改過）。
    #[test]
    fn redacting_the_newest_edit_falls_back_to_the_previous_one() {
        let (mut cache, dir) = open("edit-redacted");
        put(
            &mut cache,
            ALICE,
            &[
                text("!r", "$t", None, 1),
                edit_of("!r", "$e1", CAROL, "$t", 20, "first"),
                edit_of("!r", "$e2", CAROL, "$t", 30, "second"),
            ],
        );
        put(&mut cache, ALICE, &[redaction_of("!r", "$x2", "$e2")]);
        assert_eq!(body_of(&cache, ALICE, "!r", "$t").as_deref(), Some("first"));
        put(&mut cache, ALICE, &[redaction_of("!r", "$x1", "$e1")]);
        assert_eq!(
            body_of(&cache, ALICE, "!r", "$t").as_deref(),
            Some("body $t")
        );
        assert_eq!(
            cache.history(ALICE, "!r", None, 1).unwrap()[0].edited_by,
            None
        );
        assert_eq!(
            column_of::<i64>(&cache, "modified_timestamp", "$t"),
            0,
            "一個都不剩：回到沒被 edit 過"
        );
        // 回到 0 之後，晚到的 edit（時間比那兩個 redact 都早）照樣能贏。
        put(
            &mut cache,
            ALICE,
            &[edit_of("!r", "$late", CAROL, "$t", 5, "late")],
        );
        assert_eq!(body_of(&cache, ALICE, "!r", "$t").as_deref(), Some("late"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 🚨 可見性跟 events 表完全一致：目前的 edit 讀者還沒同步到 → 過時的記號，🚫 原文與 edit 內容都不給。
    #[test]
    fn a_current_edit_the_reader_has_not_synced_is_outdated() {
        let (mut cache, dir) = open("edit-outdated");
        let target = text("!r", "$t", Some(1), 1);
        put(&mut cache, ALICE, std::slice::from_ref(&target));
        put(
            &mut cache,
            BOB,
            &[target, edit_of("!r", "$e", CAROL, "$t", 20, "bob saw this")],
        );
        let alice_view = cache.history(ALICE, "!r", None, 1).unwrap().remove(0);
        assert_eq!(alice_view.kind, MessageKind::Outdated);
        assert_eq!(alice_view.edited_by, None);
        assert_eq!(
            body_of(&cache, BOB, "!r", "$t").as_deref(),
            Some("bob saw this")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ⭐ 目標那一列記著「目前顯示哪個 edit」與它的時間（§7.5）；讀取只照那一列走。
    #[test]
    fn the_target_row_points_at_the_current_edit_with_its_server_time() {
        let (mut cache, dir) = open("edit-pointer");
        put(&mut cache, ALICE, &[text("!r", "$t", None, 1)]);
        assert_eq!(
            column_of::<Option<String>>(&cache, "ref_event_id", "$t"),
            None
        );
        assert_eq!(
            column_of::<i64>(&cache, "modified_timestamp", "$t"),
            0,
            "寫入時是 0：第一個 edit 一定贏"
        );
        put(
            &mut cache,
            ALICE,
            &[
                edit_of("!r", "$new", CAROL, "$t", 30, "newest"),
                edit_of("!r", "$old", CAROL, "$t", 20, "older"),
            ],
        );
        assert_eq!(
            column_of::<Option<String>>(&cache, "ref_event_id", "$t").as_deref(),
            Some("$new")
        );
        assert_eq!(column_of::<i64>(&cache, "modified_timestamp", "$t"), 30);
        assert_eq!(
            column_of::<i64>(&cache, "is_processed", "$old"),
            1,
            "比較舊的跳過，也算處理過"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 第一個 edit 🚫 不跟目標自己的時間比：發送端時鐘偏了，edit 的時間比原文早，也照樣生效。
    #[test]
    fn the_first_edit_applies_even_if_its_clock_is_behind_the_original() {
        let (mut cache, dir) = open("edit-skew");
        put(
            &mut cache,
            ALICE,
            &[
                text("!r", "$t", None, 100),
                edit_of("!r", "$e", CAROL, "$t", 50, "skewed"),
            ],
        );
        assert_eq!(
            body_of(&cache, ALICE, "!r", "$t").as_deref(),
            Some("skewed")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// edit 先到、目標還沒解開：等；目標解開之後才驗、才生效。
    #[test]
    fn an_edit_waits_while_its_target_is_still_undecrypted() {
        let (mut cache, dir) = open("edit-general");
        let ciphertext = serde_json::json!({
            "type": "m.room.encrypted", "event_id": "$t", "room_id": "!r", "sender": CAROL,
            "origin_server_ts": 1, "content": { "ciphertext": "AAA" },
        });
        cache
            .upsert_events(
                ALICE,
                "!r",
                &[IncomingEvent::Undecrypted {
                    ciphertext,
                    reason: "x".into(),
                }],
            )
            .unwrap();
        let edit_cleartext = event_json(
            "!r",
            "$e",
            CAROL,
            None,
            20,
            serde_json::json!({
                "msgtype": "m.text", "body": "* hello",
                "m.new_content": { "msgtype": "m.text", "body": "hello" },
                "m.relates_to": { "rel_type": "m.replace", "event_id": "$t" },
            }),
        );
        cache
            .upsert_events(
                ALICE,
                "!r",
                &[IncomingEvent::Decrypted {
                    ciphertext: None,
                    cleartext: edit_cleartext,
                }],
            )
            .unwrap();
        assert_eq!(column_of::<i64>(&cache, "is_processed", "$e"), 0);
        let target_cleartext = event_json(
            "!r",
            "$t",
            CAROL,
            None,
            1,
            serde_json::json!({ "msgtype": "m.text", "body": "hi" }),
        );
        cache
            .upsert_events(
                ALICE,
                "!r",
                &[IncomingEvent::Decrypted {
                    ciphertext: None,
                    cleartext: target_cleartext,
                }],
            )
            .unwrap();
        assert_eq!(body_of(&cache, ALICE, "!r", "$t").as_deref(), Some("hello"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// redact：目標打勾、`modified_timestamp` 換成 redact 的時間；還沒解開的訊息被 redact 也顯示「已刪除」。
    #[test]
    fn a_redaction_moves_the_modified_timestamp_and_wins_over_undecryptable() {
        let (mut cache, dir) = open("redact-time");
        put(
            &mut cache,
            ALICE,
            &[text("!r", "$t", None, 1), redaction_of("!r", "$x", "$t")],
        );
        assert_eq!(column_of::<i64>(&cache, "modified_timestamp", "$t"), 9);
        let ciphertext = serde_json::json!({
            "type": "m.room.encrypted", "event_id": "$c", "room_id": "!r", "sender": CAROL,
            "origin_server_ts": 2, "content": { "ciphertext": "AAA" },
        });
        cache
            .upsert_events(
                ALICE,
                "!r",
                &[IncomingEvent::Undecrypted {
                    ciphertext,
                    reason: "x".into(),
                }],
            )
            .unwrap();
        put(&mut cache, ALICE, &[redaction_of("!r", "$y", "$c")]);
        assert!(matches!(
            cache
                .list_messages_by_event_ids(ALICE, "!r", &["$c".to_string()])
                .unwrap()[0]
                .kind,
            MessageKind::Deleted { .. }
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 🚨 讀取端再驗一次：訊息那列的 `ref_event_id` 被弄成指向別人的事件，🚫 不顯示那個內容。
    #[test]
    fn a_pointer_to_something_that_is_not_its_edit_shows_the_original() {
        let (mut cache, dir) = open("edit-pointer-guard");
        put(
            &mut cache,
            ALICE,
            &[
                text("!r", "$t", None, 1),
                edit_of("!r", "$m", "@mallory:localhost", "$other", 20, "pwned"),
            ],
        );
        cache
            .connection
            .execute(
                "UPDATE events SET ref_event_id = '$m' WHERE event_id = '$t'",
                [],
            )
            .unwrap();
        assert_eq!(
            body_of(&cache, ALICE, "!r", "$t").as_deref(),
            Some("body $t")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// redact 優先：打勾之後任何 edit 都不改它；🚫 原樣不動（可以復原）。redact 先到也一樣。
    #[test]
    fn a_redaction_wins_over_edits_and_keeps_the_raw_event() {
        let (mut cache, dir) = open("redact");
        put(
            &mut cache,
            ALICE,
            &[text("!r", "$t", Some(1), 1), redaction_of("!r", "$x", "$t")],
        );
        put(
            &mut cache,
            ALICE,
            &[edit_of("!r", "$e", CAROL, "$t", 99, "after")],
        );
        let shown = cache.history(ALICE, "!r", None, 10).unwrap();
        assert_eq!(ids(&shown), ["$t"]);
        assert!(
            matches!(&shown[0].kind, MessageKind::Deleted { reason: Some(reason) } if reason == "spam")
        );
        assert_eq!(column_of::<i64>(&cache, "is_redacted", "$t"), 1);
        assert!(column_of::<String>(&cache, "raw_event", "$t").contains("body $t"));
        assert!(
            column_of::<String>(&cache, "content_json", "$t").contains("body $t"),
            "content_json 也沒被改"
        );

        put(
            &mut cache,
            ALICE,
            &[
                redaction_of("!r", "$y", "$later"),
                text("!r", "$later", Some(2), 2),
            ],
        );
        assert!(matches!(
            cache
                .list_messages_by_event_ids(ALICE, "!r", &["$later".to_string()])
                .unwrap()[0]
                .kind,
            MessageKind::Deleted { .. }
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// reaction 讀取時聚合；🚨 只算這個帳號同步過的；被 redact 的不算。
    #[test]
    fn reactions_count_only_what_the_reader_synced_and_not_redacted() {
        let (mut cache, dir) = open("reactions");
        let target = text("!r", "$t", Some(1), 1);
        put(
            &mut cache,
            ALICE,
            &[target.clone(), reaction_of("!r", "$r1", BOB, "$t", "👍")],
        );
        put(
            &mut cache,
            BOB,
            &[
                target,
                reaction_of("!r", "$r2", CAROL, "$t", "👍"),
                reaction_of("!r", "$r3", CAROL, "$t", "🎉"),
            ],
        );
        let alice_view = cache.history(ALICE, "!r", None, 1).unwrap().remove(0);
        assert_eq!(
            alice_view.reactions,
            vec![Reaction {
                key: "👍".into(),
                by: vec![BOB.into()]
            }]
        );
        put(&mut cache, BOB, &[redaction_of("!r", "$x", "$r3")]);
        let bob_view = cache.history(BOB, "!r", None, 1).unwrap().remove(0);
        assert_eq!(
            bob_view.reactions,
            vec![Reaction {
                key: "👍".into(),
                by: vec![CAROL.into()]
            }]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 🚫 不寫：沒有 sender 的（比對 edit 的 sender 會變成佔位值相等）、自己帶的 room_id 跟參數不一樣的。
    #[test]
    fn events_without_a_sender_or_from_another_room_are_not_written() {
        let (mut cache, dir) = open("refuse");
        let mut no_sender = event_json(
            "!r",
            "$n",
            CAROL,
            Some(1),
            1,
            serde_json::json!({ "body": "x" }),
        );
        no_sender.as_object_mut().unwrap().remove("sender");
        let other_room = event_json(
            "!other",
            "$o",
            CAROL,
            Some(2),
            2,
            serde_json::json!({ "body": "x" }),
        );
        let written = cache
            .upsert_events(
                ALICE,
                "!r",
                &[
                    IncomingEvent::Plain { event: no_sender },
                    IncomingEvent::Plain { event: other_room },
                ],
            )
            .unwrap();
        assert_eq!(written, 0);
        assert_eq!(cache.count_rows("events").unwrap(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ⭐ 有參照就以參照物為準：讀者 hide 了目前的 edit → 這則跟著隱藏（維護者 2026-09-14）。別的帳號不受影響。
    #[test]
    fn hiding_the_current_edit_hides_the_message_for_that_reader() {
        let (mut cache, dir) = open("edit-hidden");
        let fixtures = [
            text("!r", "$t", Some(1), 1),
            edit_of("!r", "$e", CAROL, "$t", 20, "hello"),
        ];
        put(&mut cache, ALICE, &fixtures);
        put(&mut cache, BOB, &fixtures);
        assert!(cache.hide_message(ALICE, "!r", "$e").unwrap());
        assert!(cache.history(ALICE, "!r", None, 10).unwrap().is_empty());
        assert!(cache
            .list_messages_by_event_ids(ALICE, "!r", &["$t".to_string()])
            .unwrap()
            .is_empty());
        assert_eq!(body_of(&cache, BOB, "!r", "$t").as_deref(), Some("hello"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 防線：指標還指著一個被 redact 的 edit（正常流程會重設，這裡用 SQL 做出來）→ 已刪除，🚫 不顯示那個 edit 的內容。
    #[test]
    fn a_current_edit_that_is_redacted_shows_deleted() {
        let (mut cache, dir) = open("edit-pointer-redacted");
        put(
            &mut cache,
            ALICE,
            &[
                text("!r", "$t", Some(1), 1),
                edit_of("!r", "$e", CAROL, "$t", 20, "hello"),
            ],
        );
        cache
            .connection
            .execute(
                "UPDATE events SET is_redacted = 1 WHERE event_id = '$e'",
                [],
            )
            .unwrap();
        let shown = cache.history(ALICE, "!r", None, 1).unwrap().remove(0);
        assert!(
            matches!(shown.kind, MessageKind::Deleted { .. }),
            "{shown:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// reaction 被讀者 hide 了就不算。
    #[test]
    fn a_hidden_reaction_is_not_counted() {
        let (mut cache, dir) = open("reaction-hidden");
        put(
            &mut cache,
            ALICE,
            &[
                text("!r", "$t", Some(1), 1),
                reaction_of("!r", "$r1", BOB, "$t", "👍"),
                reaction_of("!r", "$r2", CAROL, "$t", "👍"),
            ],
        );
        assert!(cache.hide_message(ALICE, "!r", "$r1").unwrap());
        let shown = cache.history(ALICE, "!r", None, 1).unwrap().remove(0);
        assert_eq!(
            shown.reactions,
            vec![Reaction {
                key: "👍".into(),
                by: vec![CAROL.into()]
            }]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

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
use matrix_sdk::encryption::recovery::RecoveryState;
use matrix_sdk::encryption::{BackupDownloadStrategy, EncryptionSettings};
use matrix_sdk::room::MessagesOptions;
use matrix_sdk::ruma::events::room::message::{MessageType, RoomMessageEventContent};
use matrix_sdk::ruma::events::room::power_levels::UserPowerLevel;
use matrix_sdk::ruma::events::MessageLikeEventType;
use matrix_sdk::ruma::{OwnedRoomId, RoomId, UInt, UserId};
use matrix_sdk::SqliteStoreConfig;
use matrix_sdk::{Client, Room, SessionMeta};

use crate::chat::{
    Attachment, ChatBackend, Conversation, ConversationKind, Message, Page, Update, WatchControl,
    WatchEnd,
};
use crate::error::SdkError;
use crate::event_json::{
    aggregate, message_from_json, relation_of, Relation, CHUNKED_BLOCK_KEY, FILE_MSGTYPE,
};
use crate::login::Session;
use crate::vault::Key32;

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
        server_backup: bool,
    ) -> Result<(MatrixBackend, Session), SdkError> {
        let client = build_client(server, store_dir, store_key, server_backup).await?;
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
        server_backup: bool,
    ) -> Result<MatrixBackend, SdkError> {
        let client = build_client(&session.server, store_dir, store_key, server_backup).await?;
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

    /// ⚠️ `before` 是 **`event_id`**，🚫 不是 server 的翻頁 token（chat-model §4.3：`event_id` 是可攜的權威）。
    /// 標準 Matrix 沒有「從某則事件往前翻」的 API，所以分兩步：
    ///
    /// 1. `/context/{event_id}` 拿到**那一則之前**的 token（`prev_batch_token`）；
    /// 2. `/messages` 從那個 token 往回。
    ///
    /// ⭐ 所以**不必暫存 token**：任何一則事件都拿得到它的位置，代價是每頁多一次請求。
    /// ⚠️ `/context` 的 `limit` 給 0，但有的 server 會把 0 當預設值、照樣回 `events_before` ——
    /// 那些就是緊鄰錨點的更舊事件，🚫 不能丟，所以接在這一頁的最前面。
    async fn history(&self, id: &str, before: Option<&str>, limit: u32) -> Result<Page, SdkError> {
        if limit == 0 {
            return Err(SdkError::Usage("history limit must be at least 1".into()));
        }
        let room = self.room(id)?;
        let (adjacent, from, remaining) = match before {
            None => (Vec::new(), None, limit),
            Some(anchor) => {
                let anchor = matrix_sdk::ruma::EventId::parse(anchor).map_err(|error| {
                    SdkError::Usage(format!("`before` must be an event id: {error}"))
                })?;
                let context = room
                    .event_with_context(&anchor, false, UInt::from(0u32), None)
                    .await
                    .map_err(matrix_error)?;
                let has_prev_token = context.prev_batch_token.is_some();
                match plan_after_context(context.events_before, has_prev_token, limit) {
                    ContextPlan::Done(events_before) => return Ok(page_from_timeline(id, &events_before)),
                    ContextPlan::Continue {
                        adjacent,
                        remaining,
                    } => (adjacent, context.prev_batch_token, remaining),
                }
            }
        };
        let mut timeline: Vec<TimelineEvent> = adjacent;
        if remaining > 0 {
            let mut options = MessagesOptions::backward();
            options.limit = UInt::from(remaining);
            options.from = from;
            let messages = room.messages(options).await.map_err(matrix_error)?;
            timeline.extend(messages.chunk);
        }
        Ok(page_from_timeline(id, &timeline))
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
        on_update: &mut (dyn FnMut(Update) -> WatchControl + Send),
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

/// server 端房間金鑰備份的設定（local-cache-db.md §10.3）。
///
/// `auto_enable_backups`：`login` 之後 server 上沒有 backup version 就建一個，並開始上傳。
/// ⚠️ 建 version 時 **backup 的私鑰只存在本機 crypto store**，沒進 SSSS —— 所以在使用者跑
/// `key-backup recovery` 之前，server 上那份**換一台機器也解不開**（它防的是本機 crypto.db 壞掉，
/// 不是換裝置）。這句話要出現在警告與 `logout` 的閘門裡。
///
/// `backup_download_strategy`：解不開某則訊息時才去 backup 拿那把金鑰，🚫 不一開機就全下載。
///
/// Args:
///     server_backup: conf 的 `SERVER_BACKUP`（CLI 規格 §10）, example: true
/// Return:
///     EncryptionSettings   `server_backup` 是 false 時只有 `auto_enable_backups` 關掉
fn backup_encryption_settings(server_backup: bool) -> EncryptionSettings {
    EncryptionSettings {
        auto_enable_backups: server_backup,
        // ⚠️ cross-signing 不跟著關：`SERVER_BACKUP=off` 說的是「不要上傳房間金鑰」，
        // 不是「不要驗裝置」。把兩件事綁在一起會讓關掉備份的人連裝置驗證都沒了。
        auto_enable_cross_signing: true,
        // 🚫 下載策略也不跟著關：server 上**已經有**的 backup（之前開著時傳的、或別台傳的）
        // 仍然該在解不開訊息時派上用場。關掉的是「往上傳」，不是「往下拿」。
        backup_download_strategy: BackupDownloadStrategy::AfterDecryptionFailure,
    }
}

/// `key-backup status` 印的東西（CLI 規格 §3.6）。🚫 不含任何金鑰內容。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BackupStatus {
    /// server 上有沒有 backup version。
    pub exists_on_server: bool,
    /// 本機的 backup 有沒有啟用（有金鑰、會上傳）。
    pub enabled_locally: bool,
    /// SSSS（secret storage）設好了、而且本機有全部的 secrets——上游 `RecoveryState::Enabled`
    /// 的原話是 "Secret storage is set up and we have all the secrets locally"。
    ///
    /// ⚠️ **它不代表使用者手上真的有那串 recovery key**：跑過 `key-backup recovery`、
    /// 印出來、然後沒抄下來就關掉終端，這個欄位一樣是 true。技術上沒有辦法驗證那件事
    /// （維護者 2026-09-09 問到這點）。所以欄位叫 `recovery_enabled` 而不是
    /// `has_recovery_key`——🚫 名字不要承諾我們驗不到的事。
    ///
    /// **只有 `Enabled` 算數**：`Unknown`、`Incomplete`、`Disabled` 一律當作沒有
    /// （fail closed，§10.7 的閘門靠這個判斷）。
    pub recovery_enabled: bool,
    /// 上游 `RecoveryState` 的名字，給人看的。
    pub recovery_state: String,
}

impl MatrixBackend {
    /// server 端備份現在是什麼狀態（`key-backup status`、`logout` 的閘門都用它）。
    ///
    /// Return:
    ///     Ok(BackupStatus)
    ///     Err(Network)   問不到 server（閘門會因此擋下來——問不到就不是「正面認得救得回來」）
    pub async fn backup_status(&self) -> Result<BackupStatus, SdkError> {
        let backups = self.client.encryption().backups();
        let exists_on_server = backups
            .fetch_exists_on_server()
            .await
            .map_err(|error| SdkError::Network(format!("key backup: {error}")))?;
        let recovery_state = self.client.encryption().recovery().state();
        Ok(BackupStatus {
            exists_on_server,
            enabled_locally: backups.are_enabled().await,
            // 🚫 不寫成「不是 Disabled 就算有」：新增一種狀態就會默默放行（local-cache-db.md §10.7）。
            recovery_enabled: matches!(recovery_state, RecoveryState::Enabled),
            recovery_state: format!("{recovery_state:?}"),
        })
    }

    /// 把 crypto store 裡的房間金鑰推上 server，**傳完才回來**（`key-backup upload`）。
    ///
    /// 為什麼要有這個命令：上游的上傳是背景 task，而 `BackupUploadingTask` 的 `Drop` 直接
    /// `abort()`——CLI 一個命令跑完就 exit，那個 task 可能一筆都還沒送出去（local-cache-db.md §10.6）。
    ///
    /// Return:
    ///     Ok(())        追平了
    ///     Err(Network)  server 不收、或中途斷線
    pub async fn upload_room_keys(&self) -> Result<(), SdkError> {
        self.client
            .encryption()
            .backups()
            .wait_for_steady_state()
            .await
            .map_err(|error| SdkError::Network(format!("key backup upload: {error}")))
    }

    /// 把 crypto store 裡的**全部**房間金鑰倒進本地快照（`key-backup save`；local-cache-db.md §10.4）。
    ///
    /// 上游直接寫檔，金鑰不經過我們的記憶體。先寫 `temp_path` 再 rename 到 `path`：
    /// 寫到一半斷電不會把上一份好的蓋成半個檔。
    ///
    /// 匯出是**全量**的（上游只給這條路，拿不到逐把金鑰），所以每次都覆蓋整份——
    /// crypto store 只增不減，新的快照一定含得下舊的，不必去重也不會愈積愈多。
    /// 代價是每次一輪 PBKDF2 500,000（約半秒），所以這是命令觸發的，不是每個命令都做。
    ///
    /// Args:
    ///     path: example: room_keys::snapshot_path(&account.dir)
    ///     temp_path: example: room_keys::snapshot_temp_path(&account.dir)
    ///     passphrase: 🚫 不印、不 log, example: room_keys::snapshot_passphrase(&key)
    /// Return:
    ///     Ok(u64)      快照有多少 byte
    ///     Err(Usage)   store 開不了、寫不進去
    pub async fn save_room_key_snapshot(
        &self,
        path: &Path,
        temp_path: &Path,
        passphrase: &str,
    ) -> Result<u64, SdkError> {
        if let Some(parent) = path.parent() {
            // 目錄 0700、檔案 0600：上游的 export 走 umask 預設，而這是全部房間金鑰的密文
            // （PR #19 審查 rumia🟡2／salvia🟡4）。
            crate::room_keys::prepare_dir(parent)?;
        }
        self.client
            .encryption()
            .export_room_keys(temp_path.to_path_buf(), passphrase, |_| true)
            .await
            .map_err(|error| SdkError::Usage(format!("cannot export room keys: {error}")))?;
        // rename 保留來源檔的權限，所以要在 rename 之前收。
        crate::room_keys::set_snapshot_permissions(temp_path)?;
        std::fs::rename(temp_path, path)?;
        Ok(std::fs::metadata(path)?.len())
    }

    /// 把本地快照餵回 crypto store（`key-backup import`）。重新 `login`、或刪過 `m/` 之後用。
    ///
    /// Return:
    ///     Ok((imported, total))   這次新匯入幾把、快照裡總共幾把
    ///     Err(Usage)              沒有快照、解不開（不是這把 local.key 存的）、格式壞掉
    pub async fn import_room_key_snapshot(
        &self,
        path: &Path,
        passphrase: &str,
    ) -> Result<(usize, usize), SdkError> {
        if !path.exists() {
            return Err(SdkError::Usage(format!(
                "no local room key snapshot at {}; run `key-backup save` while logged in",
                path.display()
            )));
        }
        let result = self
            .client
            .encryption()
            .import_room_keys(path.to_path_buf(), passphrase)
            .await
            .map_err(|error| SdkError::Usage(format!("cannot import room keys: {error}")))?;
        Ok((result.imported_count, result.total_count))
    }

    /// 拿 recovery key 把**這台裝置**恢復：解開 SSSS、把 backup 的解密金鑰收進 crypto store。
    ///
    /// 什麼時候要：`logout` 之後重新 `login` 是**新裝置**，它的 crypto store 沒有 SSSS 的 secrets，
    /// 所以 `RecoveryState` 會是 `Incomplete`，server 上那份備份也解不開——直到跑過這個
    /// （2026-09-09 對真 server 驗證時發現這個缺口：recovery key 保管著卻沒有入口用它）。
    ///
    /// 跑完之後 `backup_download_strategy` 才有東西可以下載：解不開的訊息會自動去 backup 拿金鑰。
    ///
    /// Args:
    ///     recovery_key: 🚫 不印、不 log
    /// Return:
    ///     Ok(())        恢復了
    ///     Err(Network)  key 不對、或問不到 server
    pub async fn recover_with(&self, recovery_key: &str) -> Result<(), SdkError> {
        self.client
            .encryption()
            .recovery()
            .recover(recovery_key)
            .await
            .map_err(|error| SdkError::Network(format!("recover: {error}")))
    }

    /// 產生 recovery key（`key-backup recovery`）。設好之後 server 上那份備份**換裝置也解得開**。
    ///
    /// Return:
    ///     Ok(String)   recovery key，🚫 只印一次、不寫檔、不進 log
    ///     Err(Network) server 不收
    pub async fn enable_recovery(&self) -> Result<String, SdkError> {
        self.client
            .encryption()
            .recovery()
            .enable()
            .await
            .map_err(|error| SdkError::Network(format!("recovery: {error}")))
    }
}

async fn build_client(
    server: &str,
    store_dir: &Path,
    store_key: &Key32,
    server_backup: bool,
) -> Result<Client, SdkError> {
    std::fs::create_dir_all(store_dir)?;
    // 與 channel::REQUEST_TIMEOUT 同一個數：server 黑洞了就回錯，不讓 CLI 掛死（PR #9 審查 rumia 🟢3）。
    // `key(...)` 走 `StoreCipher::open_with_key`：沒有 PBKDF2，密碼那一層在 Vault 做過了（local-cache-db.md §5.3）。
    let store_config = SqliteStoreConfig::new(store_dir).key(Some(store_key.as_bytes()));
    Client::builder()
        .homeserver_url(server)
        .request_config(RequestConfig::new().timeout(crate::channel::REQUEST_TIMEOUT))
        .with_encryption_settings(backup_encryption_settings(server_backup))
        .sqlite_store_with_config_and_cache_path(store_config, None::<&Path>)
        .build()
        .await
        .map_err(|error| match error {
            // 開不了 store 通常是兩個原因之一，而它們的處置完全不同——所以先分辨再報。
            matrix_sdk::ClientBuildError::SqliteStore(error) => {
                let path = store_dir.display().to_string();
                // ⚠️ 路徑太長時 sqlite 也回「開不了」，2026-09-09 實測被誤報成「金鑰不對」，
                // 害人去刪一個其實沒問題的目錄。Windows 的 MAX_PATH 是 260，
                // 上游的 store 檔名最長是 matrix-sdk-event-cache.sqlite3（30 字元）。
                if cfg!(windows) && path.chars().count() + 31 > 250 {
                    SdkError::Usage(format!(
                        "cannot open the matrix store at {path}: the path is {} characters and Windows \
                         refuses paths over 260 - move the data dir somewhere shorter (--data-dir)",
                        path.chars().count()
                    ))
                } else {
                    // store 只是「非存不可」的裝置狀態，刪掉重新 login 就好；不做遷移（local-cache-db.md §1）。
                    SdkError::Usage(format!(
                        "cannot open the matrix store at {path}: {error}; it was made with another key file - delete that directory and run `login` again"
                    ))
                }
            }
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
            // 🚫 不是 wbf `Error` pack 來的：沒有 `code_id`（`wbf_code()` 因此是 None）。
            code_id: None,
        };
    }
    SdkError::Network(format!("matrix: {error}"))
}

// ---- 事件 → Message ----

/// `TimelineEvent` 是 matrix-sdk 解密過的結果：解得開就是明文事件，解不開是原事件加原因。這裡把它變成我們的 `Message`。
/// `/context` 回來之後，這一頁還要不要再問 `/messages`。
#[derive(Debug, PartialEq, Eq)]
enum ContextPlan<T> {
    /// 不用再問：緊鄰的已經湊滿一頁，或那一則之前已經沒有東西（房間的開頭）。
    Done(Vec<T>),
    /// 還差幾則：從 `prev_batch_token` 往回問 `/messages`。
    Continue { adjacent: Vec<T>, remaining: u32 },
}

/// 🚨 **這一頁最多 `limit` 則** —— `/context` 回多少都一樣（PR #36 審查 rumia🔴）。
///
/// `/context` 的 `limit` 我們給 0，但有的 server 把 0 當「預設值」、照樣回一串 `events_before`；
/// 整串塞進這一頁，`limit = 1` 就可能回 10 則。所以先截到 `limit`。
///
/// ⭐ **截掉的不會被跳過**：下一頁的錨是**這一頁最舊那則的 `event_id`**（`next_anchor_of`），
/// 下一次會對它重新 `/context` —— 被截掉的那幾則比它舊，下一頁自然拿到。
/// 🚫 所以截掉之後**不能**再從 `prev_batch_token` 接 `/messages`：那個 token 在**整串** `events_before` 之前，
/// 接下去會跳過被截掉的那段。
///
/// 🚨 沒有 token ＝ 那一則之前已經沒有東西（房間的開頭）：🚫 不再用 `from: None` 去問，那會拿到最新一頁、看起來像成功。
///
/// Args:
///     events_before: `/context` 回的緊鄰更舊事件，新到舊, example: vec![e9, e8, e7]
///     has_prev_token: `/context` 有沒有給 `prev_batch_token`, example: true
///     limit: 這一頁最多幾則（呼叫端已擋掉 0）, example: 1
/// Return:
///     Done(events)                 截到 `limit` 的緊鄰事件就是整頁
///     Continue { adjacent, remaining }  緊鄰的不到 `limit`、而且還有 token：再問 `remaining` 則
fn plan_after_context<T>(mut events_before: Vec<T>, has_prev_token: bool, limit: u32) -> ContextPlan<T> {
    let limit = usize::try_from(limit).unwrap_or(usize::MAX);
    if events_before.len() >= limit {
        events_before.truncate(limit);
        return ContextPlan::Done(events_before);
    }
    if !has_prev_token {
        return ContextPlan::Done(events_before);
    }
    let remaining = u32::try_from(limit - events_before.len()).unwrap_or(u32::MAX);
    ContextPlan::Continue {
        adjacent: events_before,
        remaining,
    }
}

/// 上游一頁（新到舊）→ `Page`：訊息照常折疊，**`next` 從折疊之前的原始順序取**。
///
/// 🚨 **游標不准從 `aggregate` 之後的輸出推**（PR #36 審查 rumia🔴）：`aggregate` 會把 reaction／edit／redaction
/// 折進目標、從輸出拿掉。要是**最舊那則原始事件**被折掉（例：聯邦補回的歷史順序錯亂，關係事件排在目標後面），
/// 折疊後的最後一則會比較新，拿它往回問就重複同一段 —— 游標要的是「上游這一頁到哪裡」，🚫 不是「這一頁顯示了什麼」。
///
/// Args:
///     id: room_id，sync 的事件沒帶時補上
///     timeline: 上游給的這一頁，新到舊
/// Return:
///     Page  `next` ＝ 原始順序裡最舊、而且有合法 id 的那則；整頁都沒有 id（或空頁）才是 None ＝到頭了
fn page_from_timeline(id: &str, timeline: &[TimelineEvent]) -> Page {
    let next = timeline
        .iter()
        .rev()
        .find_map(|event| event.event_id())
        .map(|event_id| event_id.to_string());
    let events = aggregate(id, timeline.iter().map(to_message).collect());
    Page { events, next }
}

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

#[cfg(test)]
mod context_plan_tests {
    use super::*;

    fn plaintext(json: serde_json::Value) -> TimelineEvent {
        TimelineEvent::from_plaintext(
            matrix_sdk::ruma::serde::Raw::from_json_string(json.to_string()).unwrap(),
        )
    }

    /// 🚨 **`next` 從原始順序取，不從折疊後的輸出取**（PR #36 審查 rumia🔴）。
    /// 這一頁新到舊是 `[$t, $r]`，而最舊的 `$r` 是**對 `$t` 的 reaction**（順序錯亂的歷史才會這樣）：
    /// 折疊後只剩 `$t`，但下一頁要從 `$r` 之前問 —— 從 `$t` 問會把 `$r` 再拿一次。
    #[test]
    fn the_next_anchor_is_the_oldest_raw_event_even_when_aggregation_folds_it_away() {
        let target = plaintext(serde_json::json!({
            "type": "m.room.message", "event_id": "$t", "sender": "@a:x", "origin_server_ts": 2,
            "content": { "msgtype": "m.text", "body": "hi" }
        }));
        let reaction = plaintext(serde_json::json!({
            "type": "m.reaction", "event_id": "$r", "sender": "@b:x", "origin_server_ts": 1,
            "content": { "m.relates_to": { "rel_type": "m.annotation", "event_id": "$t", "key": "👍" } }
        }));
        let page = page_from_timeline("!room:x", &[target, reaction]);
        let ids: Vec<&str> = page.events.iter().map(|message| message.id.as_str()).collect();
        assert_eq!(ids, ["$t"], "reaction 折進了目標");
        assert_eq!(page.next.as_deref(), Some("$r"), "🚨 游標是上游最舊那則，不是折疊後的最後一則");
    }

    #[test]
    fn an_empty_page_has_no_next_anchor() {
        assert_eq!(page_from_timeline("!room:x", &[]).next, None);
    }

    /// 🚨 **`/context` 回的比 `limit` 多：截到 `limit`，而且不再接 `/messages`**（PR #36 審查 rumia🔴）。
    /// 有的 server 把 `limit=0` 當預設值；整串塞進來，`limit = 1` 就回 10 則。
    #[test]
    fn a_context_that_returns_more_than_the_limit_is_cut_to_the_limit() {
        let events_before: Vec<u32> = (0..10).rev().collect(); // 9, 8, …, 0：新到舊
        assert_eq!(
            plan_after_context(events_before, true, 1),
            ContextPlan::Done(vec![9]),
            "最多 1 則，而且是最新（緊鄰錨點）的那則；🚫 不從 token 接，那會跳過被截掉的 8…0"
        );
    }

    #[test]
    fn exactly_the_limit_needs_no_messages_call() {
        assert_eq!(plan_after_context(vec![3, 2], true, 2), ContextPlan::Done(vec![3, 2]));
    }

    #[test]
    fn fewer_than_the_limit_continues_from_the_token_for_the_rest() {
        assert_eq!(
            plan_after_context(vec![3, 2], true, 5),
            ContextPlan::Continue {
                adjacent: vec![3, 2],
                remaining: 3
            }
        );
    }

    /// 沒有 token ＝房間的開頭：🚫 不再問（`from: None` 會拿到最新一頁），也🚫 不超過 limit。
    #[test]
    fn no_token_means_the_start_of_the_room_and_still_respects_the_limit() {
        assert_eq!(plan_after_context(vec![3, 2], false, 5), ContextPlan::Done(vec![3, 2]));
        assert_eq!(plan_after_context(vec![3, 2, 1], false, 2), ContextPlan::Done(vec![3, 2]));
        assert_eq!(plan_after_context(Vec::<u32>::new(), false, 5), ContextPlan::Done(vec![]));
    }
}

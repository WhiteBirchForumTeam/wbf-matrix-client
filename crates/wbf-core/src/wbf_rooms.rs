//! wbf 帳號的房間（account-session.md §6）：沒有 matrix-sdk 的 Client，房間清單走橋（`JoinedRooms` ＋ 每房 `GetState` ＋ `m.direct`），
//! 送事件走 `Event/Send`（附件宣告終於帶得出去，wbf-client-convention-for-chunk.md §5.2）。
//!
//! 加密房的文字走 `room_crypto.rs`（先分金鑰、加密、帶 UI 給的房間版本號）；加密房的**檔案**還送不了（加密附件沒接，e2ee-rpc.md §6）。
//! ⚠️ 「加不加密」問的是**這一刻的狀態**（`GetState`），🚫 不用快取：過期的「沒加密」會把明文送進已經加密的房。

use serde_json::Value;

use wbf_sdk::chat::Conversation;
use wbf_sdk::protocol::SendRequest;
use wbf_sdk::room_state::{conversation_from_state, direct_peers_of_room, is_encryption_content};
use wbf_sdk::Transport;

use crate::accounts::AccountDir;
use crate::backend_choice::MethodHome;
use crate::error::{CoreError, CoreErrorKind};
use crate::link_pool::LinkRole;
use crate::room_crypto::SendOptions;
use crate::Core;

impl Core {
    /// 加入的房間，每一間都問一次狀態。⚠️ N 間房就是 N＋2 次橋的往返（一次 `JoinedRooms`、一次 `m.direct`）；
    /// 大房間的狀態超過 2 MiB 會被 server 以 `TooLarge` 拒，那一間就讓整個呼叫失敗——講出來比少列一間好。
    pub(crate) async fn wbf_conversations(
        &self,
        account: &AccountDir,
    ) -> Result<Vec<Conversation>, CoreError> {
        let me = self.session_of(account)?.user_id;
        let mut client = self
            .client_of(
                account,
                Transport::WebSocket,
                MethodHome::WbfSdkOnly,
                LinkRole::Misc,
            )
            .await?;
        let rooms = client.joined_rooms().await?;
        let m_direct = client.account_data(&me, "m.direct").await?;
        let mut conversations = Vec::with_capacity(rooms.len());
        for room in rooms {
            let state = client.room_state(&room).await?;
            let peers = direct_peers_of_room(m_direct.as_ref(), &room);
            conversations.push(conversation_from_state(&room, &me, &state, &peers)?);
        }
        Ok(conversations)
    }

    /// 一個房間現在的樣子（走橋：`GetState` ＋ `m.direct`）。
    ///
    /// Return:
    ///     Ok(Conversation)
    ///     Err(Server)       不在房裡（`Forbidden`）
    pub(crate) async fn wbf_conversation(
        &self,
        account: &AccountDir,
        room: &str,
    ) -> Result<Conversation, CoreError> {
        let me = self.session_of(account)?.user_id;
        let mut client = self
            .client_of(
                account,
                Transport::WebSocket,
                MethodHome::WbfSdkOnly,
                LinkRole::Misc,
            )
            .await?;
        let state = client.room_state(room).await?;
        let m_direct = client.account_data(&me, "m.direct").await?;
        let peers = direct_peers_of_room(m_direct.as_ref(), room);
        Ok(conversation_from_state(room, &me, &state, &peers)?)
    }

    /// 送一則文字（`m.room.message`／`m.text`）：明文房直接送；加密房先分金鑰、加密、帶 UI 給的房間版本號送（room_crypto.rs）。
    ///
    /// Args:
    ///     options: 加密房要 `room_devices`；`txn_id` 重送用, example: &SendOptions::default()
    /// Return:
    ///     Ok(String)                 event_id
    ///     Err(Usage)                 加密房但沒帶 `room_devices`
    ///     Err(RoomDevicesChanged)    加密房被 1506 擋；`data` 是 daemon 自動重拿的房間狀態（room_crypto.rs）
    pub(crate) async fn wbf_send_text(
        &self,
        account: &AccountDir,
        room: &str,
        body: &str,
        options: &SendOptions,
    ) -> Result<String, CoreError> {
        let content = serde_json::json!({ "msgtype": "m.text", "body": body });
        let txn_id = match &options.txn_id {
            Some(txn_id) => txn_id.clone(),
            None => wbf_sdk::protocol::new_txn_id()?,
        };
        if !self.wbf_is_room_encrypted(account, room).await? {
            return self
                .wbf_send_event(account, room, "m.room.message", content, Vec::new(), txn_id)
                .await;
        }
        let Some(devices) = &options.room_devices else {
            return Err(CoreError::new(
                CoreErrorKind::Usage,
                format!(
                    "{room} is encrypted: pass room_devices (what room.refresh_devices returned for this room) \
                     so the message carries the room version the server checks"
                ),
            ));
        };
        self.wbf_send_encrypted(account, room, "m.room.message", content, devices, txn_id)
            .await
    }

    /// 這個房間現在加密了就拒絕：送檔還沒有加密那條（加密附件是另一件事），明文的 `Event/Send` 不該進加密房。
    ///
    /// Return:
    ///     Ok(())       沒加密
    ///     Err(Usage)   加密了
    pub(crate) async fn wbf_refuse_if_encrypted(
        &self,
        account: &AccountDir,
        room: &str,
    ) -> Result<(), CoreError> {
        if self.wbf_is_room_encrypted(account, room).await? {
            return Err(CoreError::new(
                CoreErrorKind::Usage,
                format!(
                    "{room} is encrypted, and wbf accounts cannot send files there yet: this path sends a plaintext \
                     attachment over Event/Send, and encrypted attachments are not wired (e2ee-rpc.md §6)"
                ),
            ));
        }
        Ok(())
    }

    /// 這個房間現在加密了嗎。
    ///
    /// Return:
    ///     Ok(bool)   true ＝ 有 `m.room.encryption` 而且形狀認得
    ///     Err(...)   問不到（線開不起來、server 拒）——🚫 不當成沒加密：不確定就不送明文
    async fn wbf_is_room_encrypted(
        &self,
        account: &AccountDir,
        room: &str,
    ) -> Result<bool, CoreError> {
        // 只問 `m.room.encryption` 這一項（`GetStateEvent`）：全量 `GetState` 在大房間會被 server `TooLarge` 擋，
        // 送訊息就跟著送不了（PR #56 審查 cirno 🟡1）。沒有這一項（404）＝沒加密。
        let mut client = self
            .client_of(
                account,
                Transport::WebSocket,
                MethodHome::WbfSdkOnly,
                LinkRole::Misc,
            )
            .await?;
        Ok(client
            .state_event(room, "m.room.encryption", "")
            .await?
            .is_some_and(|content| is_encryption_content(&content)))
    }

    /// `Event/Send` 一則事件（明文 content），附件在 meta 裡宣告（wbf-client-convention-for-chunk.md §5.2）。
    /// 🚫 不檢查加不加密：呼叫端先過 [`Core::wbf_refuse_if_encrypted`]（送檔那條在上傳**之前**就要問，不然白傳）。
    ///
    /// Args:
    ///     event_type: example: "m.room.message"
    ///     content: 事件 content
    ///     attachments: 這則用到的 mxc, example: vec!["mxc://localhost/1122334455667788".into()]
    ///     txn_id: 重送用同一個, example: "wbf-1727600000-3"
    /// Return:
    ///     Ok(String)   server 收下的 event_id
    ///     Err(Server)  `Conflict`：某個 mxc 不是本站的、找不到、不是自己傳的、或有墓碑；整則沒送
    pub(crate) async fn wbf_send_event(
        &self,
        account: &AccountDir,
        room: &str,
        event_type: &str,
        content: Value,
        attachments: Vec<String>,
        txn_id: String,
    ) -> Result<String, CoreError> {
        let mut client = self
            .client_of(
                account,
                Transport::WebSocket,
                MethodHome::WbfSdkOnly,
                LinkRole::Misc,
            )
            .await?;
        let request = SendRequest {
            room_id: room.to_string(),
            event_type: event_type.to_string(),
            txn_id,
            attachments,
            room_version: None,
        };
        let content = serde_json::to_vec(&content).map_err(|error| {
            CoreError::new(CoreErrorKind::Usage, format!("event content: {error}"))
        })?;
        let ack = client.send_event(&request, content).await?;
        Ok(ack.event_id)
    }
}

#[cfg(test)]
mod tests {
    use wbf_sdk::login::{Session, SessionBackend};

    use crate::accounts::AccountDir;
    use crate::error::CoreErrorKind;
    use crate::rooms_ops::{HistoryQuery, SyncMode};
    use crate::sync_ops::WatchMode;
    use crate::{Core, Target};

    /// 沒人在聽的位址：wbf 那條路一開線就是 `Network`，Client 那條路是別的錯——兩條路分得開。
    const DEAD: &str = "http://127.0.0.1:1";
    const ME: &str = "@a:localhost";

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("wbf-core-wbfrooms-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 解鎖好、有一個登入時走了 wbf 那條的帳號。
    fn core_with_wbf_account(dir: &std::path::Path) -> (Core, AccountDir) {
        wbf_sdk::vault::Vault::create(dir, &wbf_sdk::Unlock::NoPassphrase).unwrap();
        let core = Core::open(dir);
        core.unlock(None).unwrap();
        let vault = core.vault().unwrap();
        let account = AccountDir::locate(dir, &vault.account_dir_key(), DEAD, ME).unwrap();
        std::fs::create_dir_all(&account.dir).unwrap();
        vault
            .seal_session(
                &account.session_path(),
                &Session {
                    server: DEAD.to_string(),
                    user_id: ME.to_string(),
                    device_id: "DEV".to_string(),
                    access_token: "syt_nobody_is_listening".to_string(),
                    store_dir: None,
                    backend: Some(SessionBackend::WbfSdk),
                },
            )
            .unwrap();
        crate::accounts::write_current(dir, &account).unwrap();
        (core, account)
    }

    /// account-session.md §6：wbf 帳號的房間命令走 WS（開線失敗是 `Network`），還掛在 Client 上的那幾支明講拒絕（`Usage`）。
    /// 🚫 兩種都不該是「log in again」（`backend_of` 對 `store_dir: None` 的那句）——那是把 wbf 帳號誤認成舊版 session。
    #[tokio::test]
    async fn a_wbf_account_routes_rooms_over_ws_and_refuses_what_still_needs_the_client() {
        let dir = scratch("routing");
        let (core, _account) = core_with_wbf_account(&dir);
        let target = Target::default();

        // 走 WS 的：清單（server／both）、單一房間、送文字——到 `client_of` 開線那一步才失敗，而且是 Network。
        for outcome in [
            core.list_conversations(SyncMode::Server, &target)
                .await
                .map(|_| ()),
            core.conversation("!r:localhost", SyncMode::Server, &target)
                .await
                .map(|_| ()),
            core.send_text(
                "!r:localhost",
                "hi",
                &crate::SendOptions::default(),
                &target,
            )
            .await
            .map(|_| ()),
        ] {
            let error = outcome.expect_err("nobody is listening");
            assert_eq!(error.kind, CoreErrorKind::Network, "{error:?}");
        }

        // 還掛在 Client 上的：備份、watch——`Usage`，訊息講出是 wbf 帳號。
        let error = core.backup_status(&target).await.expect_err("refused");
        assert_eq!(error.kind, CoreErrorKind::Usage, "{error:?}");
        assert!(error.message.contains("wbf server"), "{}", error.message);
        let error = core
            .watch(
                "!r:localhost",
                WatchMode::Once {
                    timeout_seconds: Some(1),
                },
                None,
                &target,
            )
            .await
            .expect_err("refused");
        assert_eq!(error.kind, CoreErrorKind::Usage, "{error:?}");
        assert!(error.message.contains("watch"), "{}", error.message);

        // 歷史：錨點不在本地、`sync=server` → 以前改走 `/context`，wbf 帳號沒有 Client → 明講拒絕（等 wbfuwunel #64）。
        let error = core
            .history(
                &HistoryQuery {
                    room: "!r:localhost".into(),
                    limit: 3,
                    before: Some("$not-cached".into()),
                    sync: SyncMode::Server,
                    types: vec![],
                    sender: None,
                },
                &target,
            )
            .await
            .expect_err("refused");
        assert_eq!(error.kind, CoreErrorKind::Usage, "{error:?}");
        assert!(error.message.contains("#64"), "{}", error.message);

        // 登出閘門：問不到 server 那份備份、這台也沒有 recovery key → 擋（fail closed）；accept_history_loss 才過（那條走真的登出，這裡不跑）。
        let error = core.log_out(ME, None, false, true).await.expect_err("gate");
        assert_eq!(error.kind, CoreErrorKind::HistoryWouldBeLost, "{error:?}");
        assert!(error.message.contains("wbf server"), "{}", error.message);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

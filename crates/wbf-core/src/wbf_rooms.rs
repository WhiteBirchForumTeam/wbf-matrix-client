//! wbf 帳號的房間（account-session.md §6）：沒有 matrix-sdk 的 Client，房間清單走橋（`JoinedRooms` ＋ 每房 `GetState` ＋ `m.direct`），
//! 送事件走 `Event/Send`（附件宣告終於帶得出去，約定 §5.2）。
//!
//! 🚫 **加密房這裡不送**：這條路送的是明文 content，E2EE 的 RPC 面接上 `encrypt_and_send` 之前，加密房一律拒絕。
//! ⚠️ 「加不加密」問的是**這一刻的狀態**（`GetState`），🚫 不用快取：過期的「沒加密」會把明文送進已經加密的房。

use serde_json::Value;

use wbf_sdk::chat::Conversation;
use wbf_sdk::protocol::SendRequest;
use wbf_sdk::room_state::{conversation_from_state, direct_peers_of_room};
use wbf_sdk::Transport;

use crate::accounts::AccountDir;
use crate::backend_choice::MethodHome;
use crate::error::{CoreError, CoreErrorKind};
use crate::link_pool::LinkRole;
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

    /// 送一則明文文字（`m.room.message`／`m.text`）。加密房拒絕（模組註解）。
    ///
    /// Return:
    ///     Ok(String)   event_id
    ///     Err(Usage)   房間加密了
    pub(crate) async fn wbf_send_text(
        &self,
        account: &AccountDir,
        room: &str,
        body: &str,
    ) -> Result<String, CoreError> {
        self.wbf_refuse_if_encrypted(account, room).await?;
        self.wbf_send_event(
            account,
            room,
            "m.room.message",
            serde_json::json!({ "msgtype": "m.text", "body": body }),
            Vec::new(),
        )
        .await
    }

    /// 這個房間現在加密了就拒絕：明文的 `Event/Send` 不該進加密房。
    ///
    /// Return:
    ///     Ok(())       沒加密
    ///     Err(Usage)   加密了（E2EE 的 RPC 面接 `encrypt_and_send` 之後才送得了）
    pub(crate) async fn wbf_refuse_if_encrypted(
        &self,
        account: &AccountDir,
        room: &str,
    ) -> Result<(), CoreError> {
        let conversation = self.wbf_conversation(account, room).await?;
        if conversation.encrypted {
            return Err(CoreError::new(
                CoreErrorKind::Usage,
                format!(
                    "{room} is encrypted, and wbf accounts cannot send encrypted messages yet: this path sends \
                     plaintext over Event/Send, and encrypting first arrives with the E2EE work (e2ee-walkthrough §16.6)"
                ),
            ));
        }
        Ok(())
    }

    /// `Event/Send` 一則事件（明文 content），附件在 meta 裡宣告（約定 §5.2）。
    /// 🚫 不檢查加不加密：呼叫端先過 [`Core::wbf_refuse_if_encrypted`]（送檔那條在上傳**之前**就要問，不然白傳）。
    ///
    /// Args:
    ///     event_type: example: "m.room.message"
    ///     content: 事件 content
    ///     attachments: 這則用到的 mxc, example: vec!["mxc://localhost/1122334455667788".into()]
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
            txn_id: wbf_sdk::protocol::new_txn_id()?,
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
            core.send_text("!r:localhost", "hi", &target)
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

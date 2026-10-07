//! 房間動作（/docs/design/rooms/room-actions.md）：UI 叫一個 RPC，daemon 做一個房間動作，回 server 給的 body。
//!
//! daemon 是中轉站（維護者 2026-10-07）：參數照 Matrix 的名字與格式，結果原樣；wbf 帳號走橋、一般 Matrix 帳號照同一個 HTTP 端點送
//! （`wbf_sdk::matrix_endpoint`，一張表兩條路）。daemon 自己加的只有：`room.create` 的 `encrypted`（轉成 `initial_state`）與 `is_direct` 時寫 `m.direct`；
//! 權限、置頂、`m.direct` 讀出目前那一份、改給了的部分、整份寫回（`wbf_sdk::room_state_edit`）。
//!
//! 會改到本地房間列的帶 [`WriteSync`]：`Both`（預設）收到 server 的 ACK 才寫本地；**寫不進去照樣回成功、另推一則 note**——
//! 成功以 server 的 ACK 為準，回錯 UI 會以為沒做而重做一次（維護者 2026-10-07）。`Server` 🚫 碰本地。
//! 收邀請（被別人邀）等 wbfuwunel #111（/docs/design/rooms/room-actions.md §3.3），🚫 在這裡。

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use wbf_sdk::cache::Cache;
use wbf_sdk::chat::Membership;
use wbf_sdk::login::SessionBackend;
use wbf_sdk::matrix_endpoint::{self, MatrixEndpoint};
use wbf_sdk::room_state_edit;
use wbf_sdk::SdkError;

use crate::accounts::AccountDir;
use crate::error::{CoreError, CoreErrorKind};
use crate::{Core, Target};

/// 寫本地的房間動作要不要也寫本地（/docs/design/rooms/room-actions.md §1 第 7 條）。
/// 🚫 沒有 `local`：動作一定要打到 server。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WriteSync {
    /// 收到 server 的 ACK 之後把本地的房間列一起改掉
    #[default]
    Both,
    /// 只打遠端。⚠️ `room.enable_encryption` 用它很危險（本地還記著明文，`room.send_text` 會送明文），只給除錯（§3.2）。
    Server,
}

/// 進一間房的兩種方法。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EnterRoom {
    Join,
    Knock,
}

/// 對另一個人做的成員動作。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemberAction {
    Invite,
    Kick,
    Ban,
    Unban,
}

impl MemberAction {
    fn endpoint(self) -> &'static MatrixEndpoint {
        match self {
            MemberAction::Invite => &matrix_endpoint::INVITE,
            MemberAction::Kick => &matrix_endpoint::KICK,
            MemberAction::Ban => &matrix_endpoint::BAN,
            MemberAction::Unban => &matrix_endpoint::UNBAN,
        }
    }

    /// 這個人如果是本機登入的帳號，它那一列變成什麼（/docs/design/rooms/room-actions.md §3 的表）：
    /// (新的身分, 只在目前是它時才改)。
    fn local_membership(self) -> (Membership, Option<Membership>) {
        match self {
            MemberAction::Invite => (Membership::Invite, None),
            // Matrix 裡被踢之後就是 `leave`
            MemberAction::Kick => (Membership::Leave, None),
            MemberAction::Ban => (Membership::Ban, None),
            // 解除封鎖之後是 `leave`（要再邀請或自己加入才回來）；🚫 把不是 `ban` 的列改掉
            MemberAction::Unban => (Membership::Leave, Some(Membership::Ban)),
        }
    }

    fn name(self) -> &'static str {
        match self {
            MemberAction::Invite => "room.invite",
            MemberAction::Kick => "room.kick",
            MemberAction::Ban => "room.ban",
            MemberAction::Unban => "room.unban",
        }
    }
}

/// Args 的 `{ "room_id": … }` 這種變數表。
fn to_variables(pairs: &[(&str, Value)]) -> Map<String, Value> {
    pairs
        .iter()
        .map(|(name, value)| (name.to_string(), value.clone()))
        .collect()
}

/// `reason` 給了才放進 body（Matrix 的選填欄位：🚫 送 `null`）。
fn to_reason_body(reason: Option<&str>) -> Map<String, Value> {
    reason
        .map(|reason| ("reason".to_string(), json!(reason)))
        .into_iter()
        .collect()
}

/// server 拒絕：照原樣往上帶（/docs/design/rooms/room-actions.md §2）——`data` 是 Matrix 的 `status`／`errcode`（例 403 `M_FORBIDDEN`），UI 拿它判斷、🚫 parse 那句話。
fn to_core_error(error: SdkError) -> CoreError {
    let meta = match &error {
        SdkError::Server { meta, .. } => Some(meta.clone()),
        _ => None,
    };
    let core_error = CoreError::from(error);
    match meta {
        Some(meta) => core_error.with_data(meta),
        None => core_error,
    }
}

impl Core {
    /// 叫一支房間相關的 Matrix 端點、原樣回它的 body：wbf 帳號走橋（`Misc` 線），一般 Matrix 帳號照同一個 HTTP 端點送。
    /// 純讀、或本地沒存的那幾支（成員、簡介、目錄、標籤、帳號資料…）daemon 直接用它。
    ///
    /// Args:
    ///     endpoint: example: &matrix_endpoint::SUMMARY
    ///     variables: path 與 query 變數, example: {"room_id_or_alias": "#lobby:localhost"}
    ///     body: POST／PUT 的 body, example: None
    /// Return:
    ///     Ok(Value)      server 的 body
    ///     Err(Usage)     變數或 body 不合規則（橋的規則，wbfuwunel 的 /docs/bridge-specs/index.md §1.3）
    ///     Err(Server)    server 拒絕；`data` 帶 `status`／`errcode`
    ///     Err(Network)
    pub async fn call_matrix_endpoint(
        &self,
        endpoint: &MatrixEndpoint,
        variables: Map<String, Value>,
        body: Option<Value>,
        target: &Target,
    ) -> Result<Value, CoreError> {
        let account = self.account_or_current(target)?;
        self.call_endpoint_as(&account, endpoint, &variables, body.as_ref())
            .await
    }

    /// 同 [`Core::call_matrix_endpoint`]，404（沒有這一項、沒寫過）回 `None`：`room.get_state`、帳號資料的讀。
    ///
    /// Return:
    ///     Ok(Some(Value))  server 的 body
    ///     Ok(None)         404
    ///     Err(...)         同 [`Core::call_matrix_endpoint`]
    pub async fn find_via_matrix_endpoint(
        &self,
        endpoint: &MatrixEndpoint,
        variables: Map<String, Value>,
        target: &Target,
    ) -> Result<Option<Value>, CoreError> {
        let account = self.account_or_current(target)?;
        self.find_endpoint_as(&account, endpoint, &variables).await
    }

    /// 這個帳號是誰（帳號資料、標籤的路徑要自己的 mxid）。
    ///
    /// Return:
    ///     Ok(String)   example: "@alice:localhost"
    ///     Err(...)     沒這個帳號、沒登入
    pub fn get_my_user_id(&self, target: &Target) -> Result<String, CoreError> {
        let account = self.account_or_current(target)?;
        Ok(self.session_of(&account)?.user_id)
    }

    /// 建房（/docs/design/rooms/room-actions.md §2.1）：`fields` 原樣進 CreateRoom 的 body，`encrypted: true` 加 `m.room.encryption`。
    /// `is_direct: true` 時順便把每個 `invite` 的人寫進自己的 `m.direct`；那一步失敗🚫 回錯（房已經建好了），推一則 note。
    /// `both`：建房者那一列 `join`（樣子空著，UI 要就叫 `room.get`）、`rooms.encrypted` 寫建房時給的值（`room.send_text` 靠它）。
    ///
    /// Args:
    ///     encrypted: 必填, example: true
    ///     fields: CreateRoom 的其他欄位, example: {"name": "週末聚餐", "preset": "private_chat", "invite": ["@bob:localhost"], "is_direct": true}
    /// Return:
    ///     Ok(Value)      server 的 body, example: {"room_id": "!abc:localhost"}
    ///     Err(Usage)     `initial_state` 與 `encrypted` 說的相反、`initial_state` 不是陣列
    ///     Err(Server)    server 拒絕（`data` 帶 `status`／`errcode`）；server 的 body 沒有 `room_id`
    pub async fn create_room(
        &self,
        encrypted: bool,
        fields: Map<String, Value>,
        sync: WriteSync,
        target: &Target,
    ) -> Result<Value, CoreError> {
        let account = self.account_or_current(target)?;
        let me = self.session_of(&account)?.user_id;
        let is_direct = fields.get("is_direct") == Some(&Value::Bool(true));
        let invited: Vec<String> = fields
            .get("invite")
            .and_then(Value::as_array)
            .map(|users| {
                users
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        let body = room_state_edit::to_create_room_body(encrypted, fields)?;
        let created = self
            .call_endpoint_as(
                &account,
                &matrix_endpoint::CREATE_ROOM,
                &Map::new(),
                Some(&Value::Object(body)),
            )
            .await?;
        let Some(room) = created
            .get("room_id")
            .and_then(Value::as_str)
            .map(str::to_string)
        else {
            return Err(CoreError::new(
                CoreErrorKind::Server,
                format!("CreateRoom answered without a room_id: {created}"),
            ));
        };
        if sync == WriteSync::Both {
            let (room_here, me_here) = (room.clone(), me.clone());
            self.record_locally(&account, "room.create", &room, move |cache| {
                cache.record_membership(&me_here, &room_here, Membership::Join, None)?;
                cache.record_room_encryption(&room_here, encrypted)
            })
            .await;
        }
        if is_direct {
            for user in &invited {
                if let Err(error) = self.write_direct(&account, &me, &room, user, true).await {
                    self.events.progress(format!(
                        "room.create: {room} was created, but m.direct could not be updated for {user} ({error}); \
                         call room.set_direct"
                    ));
                }
            }
        }
        Ok(created)
    }

    /// 加入或敲門（`#別名` 也收；`via` 是去哪幾台 server 找）。`both`：自己那一列 `join`／`knock`（列不在就加，樣子🚫 動）。
    ///
    /// Args:
    ///     room: example: "#lobby:localhost"
    ///     via: example: vec!["localhost".to_string()]
    ///     reason: example: Some("我是 alice 的朋友")
    /// Return:
    ///     Ok(Value)   server 的 body, example: {"room_id": "!abc:localhost"}
    pub async fn enter_room(
        &self,
        how: EnterRoom,
        room: &str,
        via: Vec<String>,
        reason: Option<&str>,
        sync: WriteSync,
        target: &Target,
    ) -> Result<Value, CoreError> {
        let account = self.account_or_current(target)?;
        let (endpoint, membership, name) = match how {
            EnterRoom::Join => (&matrix_endpoint::JOIN, Membership::Join, "room.join"),
            EnterRoom::Knock => (&matrix_endpoint::KNOCK, Membership::Knock, "room.knock"),
        };
        let mut variables = to_variables(&[("room_id_or_alias", json!(room))]);
        if !via.is_empty() {
            variables.insert("via".into(), json!(via));
        }
        let entered = self
            .call_endpoint_as(
                &account,
                endpoint,
                &variables,
                Some(&Value::Object(to_reason_body(reason))),
            )
            .await?;
        // `#別名` 進去的：本地記 server 給的房間 id（列以 id 為主鍵）。
        let room_id = entered
            .get("room_id")
            .and_then(Value::as_str)
            .unwrap_or(room)
            .to_string();
        if sync == WriteSync::Both && room_id.starts_with('!') {
            let me = self.session_of(&account)?.user_id;
            let room_here = room_id.clone();
            self.record_locally(&account, name, &room_id, move |cache| {
                cache
                    .record_membership(&me, &room_here, membership, None)
                    .map(|_| ())
            })
            .await;
        }
        Ok(entered)
    }

    /// 退出。`both`：自己那一列 `leave`，🚫 刪、樣子留著（要刪用 [`Core::forget_room`]）。
    ///
    /// Return:
    ///     Ok(Value)   server 的 body（`{}`）
    pub async fn leave_room(
        &self,
        room: &str,
        reason: Option<&str>,
        sync: WriteSync,
        target: &Target,
    ) -> Result<Value, CoreError> {
        let account = self.account_or_current(target)?;
        let left = self
            .call_endpoint_as(
                &account,
                &matrix_endpoint::LEAVE,
                &to_variables(&[("room_id", json!(room))]),
                Some(&Value::Object(to_reason_body(reason))),
            )
            .await?;
        if sync == WriteSync::Both {
            let me = self.session_of(&account)?.user_id;
            let room_here = room.to_string();
            self.record_locally(&account, "room.leave", room, move |cache| {
                cache
                    .record_membership(&me, &room_here, Membership::Leave, None)
                    .map(|_| ())
            })
            .await;
        }
        Ok(left)
    }

    /// 忘記（/docs/design/rooms/room-actions.md §3.1）：server 要先退出（不然 400）。`both`：刪自己那一列，本機沒有別的帳號看得到這間房才清整間房的紀錄。
    ///
    /// Return:
    ///     Ok(Value)   `{"history_cleared": bool}`（daemon 加的欄位）：true ＝ 本地這間房的紀錄清光了；`server`、或本地寫不進去是 false
    pub async fn forget_room(
        &self,
        room: &str,
        sync: WriteSync,
        target: &Target,
    ) -> Result<Value, CoreError> {
        let account = self.account_or_current(target)?;
        self.call_endpoint_as(
            &account,
            &matrix_endpoint::FORGET,
            &to_variables(&[("room_id", json!(room))]),
            None,
        )
        .await?;
        let mut history_cleared = false;
        if sync == WriteSync::Both {
            let me = self.session_of(&account)?.user_id;
            let room_here = room.to_string();
            history_cleared = self
                .record_locally(&account, "room.forget", room, move |cache| {
                    cache.forget_room(&me, &room_here)
                })
                .await
                .unwrap_or(false);
        }
        Ok(json!({ "history_cleared": history_cleared }))
    }

    /// 邀請、踢人、封鎖、解除封鎖。`both`：被動到的人**是這台機器登入的帳號**才改它那一列（/docs/design/rooms/room-actions.md §3 的表）；不是就🚫 寫。
    ///
    /// Args:
    ///     user: 被動到的人, example: "@bob:localhost"
    ///     reason: example: Some("spam")
    /// Return:
    ///     Ok(Value)   server 的 body（`{}`）
    pub async fn act_on_member(
        &self,
        action: MemberAction,
        room: &str,
        user: &str,
        reason: Option<&str>,
        sync: WriteSync,
        target: &Target,
    ) -> Result<Value, CoreError> {
        let account = self.account_or_current(target)?;
        let mut body = to_reason_body(reason);
        body.insert("user_id".into(), json!(user));
        let body = Value::Object(body);
        let done = self
            .call_endpoint_as(
                &account,
                action.endpoint(),
                &to_variables(&[("room_id", json!(room))]),
                Some(&body),
            )
            .await?;
        if sync == WriteSync::Both && self.is_logged_in_here(&account, user) {
            let (membership, only_if_now) = action.local_membership();
            let (user_here, room_here) = (user.to_string(), room.to_string());
            self.record_locally(&account, action.name(), room, move |cache| {
                cache
                    .record_membership(&user_here, &room_here, membership, only_if_now)
                    .map(|_| ())
            })
            .await;
        }
        Ok(done)
    }

    /// 升級房間版本。`both`：新房（`replacement_room`）加一列 `join`、樣子空著；舊房那一列🚫 動。
    ///
    /// Args:
    ///     new_version: example: "11"
    /// Return:
    ///     Ok(Value)   server 的 body, example: {"replacement_room": "!new:localhost"}
    pub async fn upgrade_room(
        &self,
        room: &str,
        new_version: &str,
        sync: WriteSync,
        target: &Target,
    ) -> Result<Value, CoreError> {
        let account = self.account_or_current(target)?;
        let upgraded = self
            .call_endpoint_as(
                &account,
                &matrix_endpoint::UPGRADE,
                &to_variables(&[("room_id", json!(room))]),
                Some(&json!({ "new_version": new_version })),
            )
            .await?;
        let replacement = upgraded
            .get("replacement_room")
            .and_then(Value::as_str)
            .map(str::to_string);
        if let (WriteSync::Both, Some(replacement)) = (sync, replacement) {
            let me = self.session_of(&account)?.user_id;
            let replacement_here = replacement.clone();
            self.record_locally(&account, "room.upgrade", &replacement, move |cache| {
                cache
                    .record_membership(&me, &replacement_here, Membership::Join, None)
                    .map(|_| ())
            })
            .await;
        }
        Ok(upgraded)
    }

    /// 寫一項房間狀態（`room.set_state` 與它所有的窄版都走這裡）。`both`：自己那一列拿過狀態就換掉那一項、重算；
    /// 寫的是 `m.room.encryption` 就 `rooms.encrypted = 1`（/docs/design/rooms/room-actions.md §3、§3.2）。
    ///
    /// Args:
    ///     event_type: example: "m.room.name"
    ///     state_key: 空字串是一個值, example: ""
    ///     content: example: json!({"name": "週末聚餐"})
    /// Return:
    ///     Ok(Value)      server 的 body, example: {"event_id": "$abc"}
    ///     Err(Server)    權限不夠（403 `M_FORBIDDEN`，`data` 帶 `status`／`errcode`）
    pub async fn set_room_state(
        &self,
        room: &str,
        event_type: &str,
        state_key: &str,
        content: Value,
        sync: WriteSync,
        target: &Target,
    ) -> Result<Value, CoreError> {
        let account = self.account_or_current(target)?;
        self.set_state_as(&account, room, event_type, state_key, content, sync)
            .await
    }

    /// 改權限（/docs/design/rooms/room-actions.md §4.2）：讀出目前的 `m.room.power_levels`、照合併規則改、整份寫回。
    ///
    /// Args:
    ///     changes: 要改的欄位（跟 content 同名同格式）, example: {"users": {"@bob:localhost": 50}, "events_default": 100}
    /// Return:
    ///     Ok(Value)      server 的 body, example: {"event_id": "$pl"}
    ///     Err(Usage)     都沒給、格式不是 Matrix 能接受的、房間沒有 `m.room.power_levels`（`room_state_edit::merge_power_levels`）
    ///     Err(Server)    server 照 auth rules 擋（403）
    pub async fn set_power_levels(
        &self,
        room: &str,
        changes: Map<String, Value>,
        sync: WriteSync,
        target: &Target,
    ) -> Result<Value, CoreError> {
        let account = self.account_or_current(target)?;
        let current = self
            .find_state_as(&account, room, "m.room.power_levels")
            .await?;
        let merged = room_state_edit::merge_power_levels(current.as_ref(), &changes)?;
        self.set_state_as(&account, room, "m.room.power_levels", "", merged, sync)
            .await
    }

    /// 置頂或取消置頂一則（/docs/design/rooms/room-actions.md §5）。已經是那個狀態就🚫 寫。
    ///
    /// Args:
    ///     event_id: example: "$abc"
    ///     pinned: true 置頂、false 取消
    /// Return:
    ///     Ok(Value)   寫了：server 的 body（`{"event_id": …}`）；沒寫：`{}`
    pub async fn pin_event(
        &self,
        room: &str,
        event_id: &str,
        pinned: bool,
        sync: WriteSync,
        target: &Target,
    ) -> Result<Value, CoreError> {
        let account = self.account_or_current(target)?;
        let current = self
            .find_state_as(&account, room, "m.room.pinned_events")
            .await?;
        match room_state_edit::to_pinned_content(current.as_ref(), event_id, pinned)? {
            None => Ok(json!({})),
            Some(content) => {
                self.set_state_as(&account, room, "m.room.pinned_events", "", content, sync)
                    .await
            }
        }
    }

    /// 在自己的 `m.direct` 加上或拿掉「`user` → `room`」（/docs/design/rooms/room-actions.md §2.5）。帳號資料本地沒存，🚫 帶 `sync`。
    ///
    /// Args:
    ///     user: 對方, example: "@bob:localhost"
    ///     direct: true 加、false 拿掉
    /// Return:
    ///     Ok(Value)   `{}`（已經是那個狀態也是：🚫 寫）
    pub async fn set_direct(
        &self,
        room: &str,
        user: &str,
        direct: bool,
        target: &Target,
    ) -> Result<Value, CoreError> {
        let account = self.account_or_current(target)?;
        let me = self.session_of(&account)?.user_id;
        self.write_direct(&account, &me, room, user, direct).await?;
        Ok(json!({}))
    }

    async fn write_direct(
        &self,
        account: &AccountDir,
        me: &str,
        room: &str,
        user: &str,
        direct: bool,
    ) -> Result<(), CoreError> {
        let variables = to_variables(&[("user_id", json!(me)), ("event_type", json!("m.direct"))]);
        let current = self
            .find_endpoint_as(account, &matrix_endpoint::GET_ACCOUNT_DATA, &variables)
            .await?;
        let Some(content) =
            room_state_edit::to_direct_content(current.as_ref(), user, room, direct)?
        else {
            return Ok(());
        };
        self.call_endpoint_as(
            account,
            &matrix_endpoint::SET_ACCOUNT_DATA,
            &variables,
            Some(&content),
        )
        .await?;
        Ok(())
    }

    async fn set_state_as(
        &self,
        account: &AccountDir,
        room: &str,
        event_type: &str,
        state_key: &str,
        content: Value,
        sync: WriteSync,
    ) -> Result<Value, CoreError> {
        let written = self
            .call_endpoint_as(
                account,
                &matrix_endpoint::SET_STATE_EVENT,
                &to_variables(&[
                    ("room_id", json!(room)),
                    ("event_type", json!(event_type)),
                    ("state_key", json!(state_key)),
                ]),
                Some(&content),
            )
            .await?;
        if sync == WriteSync::Both {
            let me = self.session_of(account)?.user_id;
            let event = json!({
                "type": event_type, "state_key": state_key, "content": content,
                "sender": me, "event_id": written.get("event_id"),
            });
            let room_here = room.to_string();
            self.record_locally(account, "room.set_state", room, move |cache| {
                cache
                    .record_written_state(&me, &room_here, &event)
                    .map(|_| ())
            })
            .await;
        }
        Ok(written)
    }

    /// 房間的某一項狀態（`state_key` 是空字串的那種）；沒有這一項是 None。
    async fn find_state_as(
        &self,
        account: &AccountDir,
        room: &str,
        event_type: &str,
    ) -> Result<Option<Value>, CoreError> {
        self.find_endpoint_as(
            account,
            &matrix_endpoint::GET_STATE_EVENT,
            &to_variables(&[
                ("room_id", json!(room)),
                ("event_type", json!(event_type)),
                ("state_key", json!("")),
            ]),
        )
        .await
    }

    async fn find_endpoint_as(
        &self,
        account: &AccountDir,
        endpoint: &MatrixEndpoint,
        variables: &Map<String, Value>,
    ) -> Result<Option<Value>, CoreError> {
        match self
            .call_endpoint_raw(account, endpoint, variables, None)
            .await?
        {
            Ok(body) => Ok(Some(body)),
            Err(error) if error.is_not_found() => Ok(None),
            Err(error) => Err(to_core_error(error)),
        }
    }

    async fn call_endpoint_as(
        &self,
        account: &AccountDir,
        endpoint: &MatrixEndpoint,
        variables: &Map<String, Value>,
        body: Option<&Value>,
    ) -> Result<Value, CoreError> {
        self.call_endpoint_raw(account, endpoint, variables, body)
            .await?
            .map_err(to_core_error)
    }

    /// 一張表兩條路的分岔點（/docs/design/rooms/room-actions.md §2）：🚫 別處再判一次帳號種類。
    ///
    /// Return:
    ///     Err(CoreError)       還沒叫到端點：沒登入、線開不起來
    ///     Ok(Err(SdkError))    端點拒了（呼叫端要分 404 就看這個）
    ///     Ok(Ok(Value))        server 的 body
    async fn call_endpoint_raw(
        &self,
        account: &AccountDir,
        endpoint: &MatrixEndpoint,
        variables: &Map<String, Value>,
        body: Option<&Value>,
    ) -> Result<Result<Value, SdkError>, CoreError> {
        let session = self.session_of(account)?;
        if session.backend == Some(SessionBackend::WbfSdk) {
            let mut client = self.misc_client(account).await?;
            return Ok(client.call_matrix_endpoint(endpoint, variables, body).await);
        }
        Ok(matrix_endpoint::call_over_http(
            &session.server,
            &session.access_token,
            endpoint,
            variables,
            body,
        )
        .await)
    }

    /// `user` 是不是這台機器上登入的帳號（同一台 server）：被邀請、被踢的人是本機帳號，它那一列才跟著改。
    /// 🚫 只看 localpart：目錄是用 localpart 算的，`@bob:別台` 會算到同一個目錄——所以解開 session、mxid 逐字比對，對不上就當不是（fail closed）。
    fn is_logged_in_here(&self, my_account: &AccountDir, user: &str) -> bool {
        let (Ok(session), Ok(vault)) = (self.session_of(my_account), self.vault()) else {
            return false;
        };
        let Ok(dir) = AccountDir::locate(
            &self.data_dir,
            &vault.account_dir_key(),
            &session.server,
            user,
        ) else {
            return false;
        };
        matches!(self.session_of(&dir), Ok(other) if other.user_id == user)
    }

    /// 寫本地；寫不進去推一則 note、回 None（動作在 server 上已經成功了，🚫 回錯）。
    async fn record_locally<T, F>(
        &self,
        account: &AccountDir,
        what: &str,
        room: &str,
        work: F,
    ) -> Option<T>
    where
        F: FnOnce(&mut Cache) -> Result<T, SdkError> + Send + 'static,
        T: Send + 'static,
    {
        let outcome = match self.server_cache_and_me(account) {
            Ok((cache, _me)) => cache.run(work).await,
            Err(error) => Err(error),
        };
        match outcome {
            Ok(value) => Some(value),
            Err(error) => {
                self.events.progress(format!(
                    "{what}: {room} was done on the server, but the local room list could not be updated ({error}); \
                     call room.list with sync=both"
                ));
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use serde_json::{json, Map, Value};
    use wbf_sdk::chat::{ConversationKind, Membership};
    use wbf_sdk::login::SessionBackend;
    use wbf_sdk::matrix_endpoint as endpoints;

    use super::*;
    use crate::link_pool::LinkRole;
    use crate::test_support::{
        add_account_on, core_with_wbf_account, memory_client_with_hello, scratch, FakeServer,
        CREATED_ROOM, DEAD, ME, ROOM,
    };
    use crate::{CoreEvent, SyncMode};

    /// 本機的第二個帳號（同一台 server，共用 `cache.db`）。
    const LOCAL_FRIEND: &str = "@b:localhost";

    /// wbf 帳號、`Misc` 線是記憶體對接的假 server。
    async fn open(name: &str) -> (Core, AccountDir, FakeServer, std::path::PathBuf) {
        let dir = scratch(&format!("room-actions-{name}"));
        let (core, account) = core_with_wbf_account(&dir).await;
        let (client, fake) = memory_client_with_hello(Arc::new(Mutex::new(Vec::new()))).await;
        drop(
            core.pool_of_account(&account)
                .unwrap()
                .acquire(LinkRole::Misc, || async move { Ok(client) })
                .await
                .unwrap(),
        );
        (core, account, fake, dir)
    }

    fn requests_to(fake: &FakeServer, endpoint: &MatrixEndpoint) -> Vec<(Value, Value)> {
        fake.rooms
            .lock()
            .unwrap()
            .requests
            .iter()
            .filter(|(kind, subtype, _, _)| {
                (*kind, *subtype) == (endpoint.bridge.kind, endpoint.bridge.subtype)
            })
            .map(|(_, _, meta, body)| (meta.clone(), body.clone()))
            .collect()
    }

    async fn membership_of(
        core: &Core,
        account: &AccountDir,
        user: &str,
        room: &str,
    ) -> Option<Membership> {
        let (cache, _me) = core.server_cache_and_me(account).unwrap();
        let all = [
            Membership::Join,
            Membership::Invite,
            Membership::Knock,
            Membership::Leave,
            Membership::Ban,
        ];
        let entries = cache.read().await.list_room_entries(user, &all).unwrap();
        entries
            .into_iter()
            .find(|entry| entry.id == room)
            .map(|entry| entry.membership)
    }

    fn fields(value: Value) -> Map<String, Value> {
        value.as_object().unwrap().clone()
    }

    /// 建房（/docs/design/rooms/room-actions.md §2.1、§3）：參數原樣進 body、`encrypted` 變成 `initial_state`、`is_direct` 寫 `m.direct`；
    /// `both` 記建房者 `join` 與 `rooms.encrypted`，`server` 🚫 碰本地。
    #[tokio::test]
    async fn creating_a_room_sends_matrix_fields_as_given_and_records_it_locally() {
        let (core, account, fake, dir) = open("create").await;
        let target = Target::default();
        let created = core
            .create_room(
                true,
                fields(json!({ "name": "週末聚餐", "preset": "trusted_private_chat", "invite": [LOCAL_FRIEND], "is_direct": true })),
                WriteSync::Both,
                &target,
            )
            .await
            .unwrap();
        assert_eq!(
            created,
            json!({ "room_id": CREATED_ROOM }),
            "server 的 body 原樣"
        );
        let (_, body) = requests_to(&fake, &endpoints::CREATE_ROOM).remove(0);
        assert_eq!(body["name"], "週末聚餐");
        assert_eq!(body["preset"], "trusted_private_chat");
        assert_eq!(body["is_direct"], true);
        assert_eq!(body["initial_state"][0]["type"], "m.room.encryption");
        assert!(
            body.get("encrypted").is_none(),
            "encrypted 是我們的，🚫 送上去"
        );
        assert_eq!(
            fake.rooms.lock().unwrap().account_data.get("m.direct"),
            Some(&json!({ LOCAL_FRIEND: [CREATED_ROOM] })),
            "is_direct：對方 → 這間房寫進自己的 m.direct"
        );
        assert_eq!(
            membership_of(&core, &account, ME, CREATED_ROOM).await,
            Some(Membership::Join)
        );
        let (cache, _) = core.server_cache_and_me(&account).unwrap();
        assert_eq!(
            cache
                .read()
                .await
                .find_room_encrypted(CREATED_ROOM)
                .unwrap(),
            Some(true),
            "room.send_text 靠這格"
        );
        assert_eq!(
            core.conversation(CREATED_ROOM, SyncMode::Local, &target)
                .await
                .unwrap_err()
                .kind,
            CoreErrorKind::NoSuchAccount,
            "樣子空著：daemon 🚫 自己補拿（UI 要就叫 room.get）"
        );

        // `server`：只打遠端。
        let dir2 = scratch("room-actions-create-server");
        let (core2, account2, _fake2, _) = open("create-server").await;
        core2
            .create_room(false, Map::new(), WriteSync::Server, &target)
            .await
            .unwrap();
        assert_eq!(
            membership_of(&core2, &account2, ME, CREATED_ROOM).await,
            None
        );
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&dir2);
    }

    /// server 拒了：錯照原樣往上帶（`data` 是 Matrix 的 `status`／`errcode`），本地🚫 寫。
    #[tokio::test]
    async fn a_refused_action_carries_the_matrix_error_and_writes_nothing() {
        let (core, account, fake, dir) = open("forbidden").await;
        fake.rooms.lock().unwrap().forbid_next = true;
        let error = core
            .enter_room(
                EnterRoom::Join,
                ROOM,
                vec![],
                None,
                WriteSync::Both,
                &Target::default(),
            )
            .await
            .unwrap_err();
        assert_eq!(error.kind, CoreErrorKind::Server, "{error:?}");
        let data = error.data.expect("the Matrix error rides along");
        assert_eq!(
            (data["status"].as_u64(), data["errcode"].as_str()),
            (Some(403), Some("M_FORBIDDEN"))
        );
        assert_eq!(membership_of(&core, &account, ME, ROOM).await, None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 加入 → 退出（`leave`、🚫 刪列）→ 忘記（刪列、只剩自己就清紀錄）（/docs/design/rooms/room-actions.md §3、§3.1）。
    #[tokio::test]
    async fn join_leave_and_forget_walk_the_local_row() {
        let (core, account, fake, dir) = open("join-leave-forget").await;
        let target = Target::default();
        let joined = core
            .enter_room(
                EnterRoom::Join,
                "#lobby:localhost",
                vec!["localhost".into()],
                Some("hi"),
                WriteSync::Both,
                &target,
            )
            .await
            .unwrap();
        assert_eq!(joined["room_id"], ROOM);
        let (meta, body) = requests_to(&fake, &endpoints::JOIN).remove(0);
        assert_eq!(
            meta,
            json!({ "room_id_or_alias": "#lobby:localhost", "via": ["localhost"] })
        );
        assert_eq!(body, json!({ "reason": "hi" }));
        assert_eq!(
            membership_of(&core, &account, ME, ROOM).await,
            Some(Membership::Join),
            "記的是 server 給的房間 id"
        );

        core.leave_room(ROOM, None, WriteSync::Both, &target)
            .await
            .unwrap();
        assert_eq!(
            membership_of(&core, &account, ME, ROOM).await,
            Some(Membership::Leave)
        );
        let forgot = core
            .forget_room(ROOM, WriteSync::Both, &target)
            .await
            .unwrap();
        assert_eq!(forgot, json!({ "history_cleared": true }));
        assert_eq!(membership_of(&core, &account, ME, ROOM).await, None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 邀請、封鎖、解除封鎖：被動到的人是**本機登入的帳號**才改它那一列；別人🚫 寫（/docs/design/rooms/room-actions.md §3）。
    #[tokio::test]
    async fn member_actions_touch_only_rows_of_accounts_logged_in_here() {
        let (core, account, fake, dir) = open("members").await;
        add_account_on(&core, &dir, DEAD, LOCAL_FRIEND, SessionBackend::WbfSdk).await;
        let target = Target::default();
        core.act_on_member(
            MemberAction::Invite,
            ROOM,
            LOCAL_FRIEND,
            Some("來吃飯"),
            WriteSync::Both,
            &target,
        )
        .await
        .unwrap();
        let (meta, body) = requests_to(&fake, &endpoints::INVITE).remove(0);
        assert_eq!(meta, json!({ "room_id": ROOM }));
        assert_eq!(
            body,
            json!({ "user_id": LOCAL_FRIEND, "reason": "來吃飯" }),
            "被邀的人在 body，🚫 meta"
        );
        assert_eq!(
            membership_of(&core, &account, LOCAL_FRIEND, ROOM).await,
            Some(Membership::Invite)
        );

        // 同一個 localpart、別台 server：目錄算到同一個，但 mxid 對不上 → 🚫 當成本機帳號。
        core.act_on_member(
            MemberAction::Invite,
            ROOM,
            "@b:elsewhere",
            None,
            WriteSync::Both,
            &target,
        )
        .await
        .unwrap();
        assert_eq!(
            membership_of(&core, &account, "@b:elsewhere", ROOM).await,
            None
        );
        core.act_on_member(
            MemberAction::Invite,
            ROOM,
            "@stranger:localhost",
            None,
            WriteSync::Both,
            &target,
        )
        .await
        .unwrap();
        assert_eq!(
            membership_of(&core, &account, "@stranger:localhost", ROOM).await,
            None
        );

        // 解除封鎖只在目前是 `ban` 時改：還在邀請中的🚫 被改成 `leave`。
        core.act_on_member(
            MemberAction::Unban,
            ROOM,
            LOCAL_FRIEND,
            None,
            WriteSync::Both,
            &target,
        )
        .await
        .unwrap();
        assert_eq!(
            membership_of(&core, &account, LOCAL_FRIEND, ROOM).await,
            Some(Membership::Invite)
        );

        core.act_on_member(
            MemberAction::Ban,
            ROOM,
            LOCAL_FRIEND,
            None,
            WriteSync::Both,
            &target,
        )
        .await
        .unwrap();
        assert_eq!(
            membership_of(&core, &account, LOCAL_FRIEND, ROOM).await,
            Some(Membership::Ban)
        );
        core.act_on_member(
            MemberAction::Unban,
            ROOM,
            LOCAL_FRIEND,
            None,
            WriteSync::Both,
            &target,
        )
        .await
        .unwrap();
        assert_eq!(
            membership_of(&core, &account, LOCAL_FRIEND, ROOM).await,
            Some(Membership::Leave)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 改名、改權限、置頂：讀出目前那一項、改給了的部分、整份寫回；`both` 把本地那份 `state` 換掉、重算（/docs/design/rooms/room-actions.md §3、§4、§5）。
    #[tokio::test]
    async fn state_writes_merge_upstream_and_recompute_the_local_view() {
        let (core, _account, fake, dir) = open("state").await;
        let target = Target::default();
        fake.rooms.lock().unwrap().state.insert(
            ("m.room.power_levels".into(), String::new()),
            json!({ "users": { ME: 100, "@old:localhost": 50 }, "kick": 50 }),
        );
        let fetched = core
            .conversation(ROOM, SyncMode::Both, &target)
            .await
            .unwrap();
        assert!(
            fetched
                .state
                .as_ref()
                .is_some_and(|state| !state.is_empty()),
            "room.get 帶整份狀態"
        );

        let written = core
            .set_power_levels(
                ROOM,
                fields(json!({ "users": { "@bob:localhost": 50, "@old:localhost": null }, "events_default": 100 })),
                WriteSync::Both,
                &target,
            )
            .await
            .unwrap();
        assert!(written["event_id"].is_string(), "{written}");
        let (meta, merged) = requests_to(&fake, &endpoints::SET_STATE_EVENT).remove(0);
        assert_eq!(
            meta,
            json!({ "room_id": ROOM, "event_type": "m.room.power_levels", "state_key": "" })
        );
        assert_eq!(
            merged,
            json!({ "users": { ME: 100, "@bob:localhost": 50 }, "kick": 50, "events_default": 100 }),
            "給了的換、null 拿掉、其他照舊"
        );
        let local = core
            .conversation(ROOM, SyncMode::Local, &target)
            .await
            .unwrap();
        assert_eq!(
            local.kind,
            ConversationKind::Channel,
            "發言門檻 100：本地跟著重算"
        );
        assert!(local.can_send_message);

        core.set_room_state(
            ROOM,
            "m.room.name",
            "",
            json!({ "name": "ops" }),
            WriteSync::Both,
            &target,
        )
        .await
        .unwrap();
        assert_eq!(
            core.conversation(ROOM, SyncMode::Local, &target)
                .await
                .unwrap()
                .name
                .as_deref(),
            Some("ops")
        );

        // 置頂：第一次寫、第二次已經是那個狀態 → 🚫 寫。
        let pinned = core
            .pin_event(ROOM, "$a", true, WriteSync::Both, &target)
            .await
            .unwrap();
        assert!(pinned["event_id"].is_string());
        assert_eq!(
            core.pin_event(ROOM, "$a", true, WriteSync::Both, &target)
                .await
                .unwrap(),
            json!({})
        );
        let pins: Vec<Value> = requests_to(&fake, &endpoints::SET_STATE_EVENT)
            .into_iter()
            .filter(|(meta, _)| meta["event_type"] == "m.room.pinned_events")
            .map(|(_, body)| body)
            .collect();
        assert_eq!(pins, vec![json!({ "pinned": ["$a"] })]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 明文房升級加密（/docs/design/rooms/room-actions.md §3.2）：`both` 之後本地標加密，`room.send_text` 就走加密那條（要 `room_devices`）。
    #[tokio::test]
    async fn enabling_encryption_marks_the_room_so_text_goes_encrypted() {
        let (core, _account, _fake, dir) = open("enable-encryption").await;
        let target = Target::default();
        core.conversation(ROOM, SyncMode::Both, &target)
            .await
            .unwrap();
        core.set_room_state(
            ROOM,
            "m.room.encryption",
            "",
            room_state_edit::to_encryption_content(),
            WriteSync::Both,
            &target,
        )
        .await
        .unwrap();
        assert!(
            core.conversation(ROOM, SyncMode::Local, &target)
                .await
                .unwrap()
                .encrypted
        );
        let error = core
            .send_text(ROOM, "hi", &crate::SendOptions::default(), &target)
            .await
            .unwrap_err();
        assert_eq!(error.kind, CoreErrorKind::Usage);
        assert!(error.message.contains("room_devices"), "{}", error.message);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 本地寫不進去：照樣回成功（server 已經 ACK），另推一則 note（維護者 2026-10-07）。
    #[tokio::test]
    async fn a_local_write_that_fails_still_succeeds_and_says_so() {
        let (core, account, _fake, dir) = open("note").await;
        let mut notes = core.subscribe();
        let done = core
            .record_locally(
                &account,
                "room.leave",
                ROOM,
                |_cache| -> Result<(), SdkError> {
                    Err(SdkError::Io(std::io::Error::other("disk is gone")))
                },
            )
            .await;
        assert!(done.is_none());
        // 開快取、線開關之類的事件也走同一條廣播：等到講 room.leave 的那則。
        let text = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if let CoreEvent::Note { text, .. } = notes.recv().await.unwrap() {
                    if text.contains("room.leave") {
                        break text;
                    }
                }
            }
        })
        .await
        .expect("a note about room.leave");
        assert!(
            text.contains(ROOM) && text.contains("disk is gone"),
            "{text}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 純讀的直接過：還沒加入也能問的簡介。
    #[tokio::test]
    async fn read_only_endpoints_pass_through() {
        let (core, _account, fake, dir) = open("summary").await;
        let summary = core
            .call_matrix_endpoint(
                &endpoints::SUMMARY,
                to_variables(&[
                    ("room_id_or_alias", json!("#lobby:localhost")),
                    ("via", json!(["localhost"])),
                ]),
                None,
                &Target::default(),
            )
            .await
            .unwrap();
        assert_eq!(summary["room_id"], ROOM);
        assert_eq!(
            requests_to(&fake, &endpoints::SUMMARY)[0].0,
            json!({ "room_id_or_alias": "#lobby:localhost", "via": ["localhost"] })
        );
        // 變數不合橋的規則：送出之前就擋。
        let error = core
            .call_matrix_endpoint(&endpoints::LEAVE, Map::new(), None, &Target::default())
            .await
            .unwrap_err();
        assert_eq!(error.kind, CoreErrorKind::Usage);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

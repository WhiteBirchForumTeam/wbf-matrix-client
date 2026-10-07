//! 房間動作的 RPC（/docs/design/rooms/room-actions.md §2）：參數照 Matrix 的名字與格式、結果是 server 回的 body 原樣。
//! 這一層只把 params 翻成 core 的參數（型別不對是 `102`）；做什麼在 `wbf_core::room_actions`。
//!
//! ⚠️ `user`／`server` 已經是「哪個帳號」（/docs/design/rpc-specs/rpc-spec.md §2），所以被動到的那個人叫 `user_id`（Matrix body 的名字），
//! 公開目錄問哪一台叫 `directory_server`。

use serde::Deserialize;
use serde_json::{json, Map, Value};
use wbf_core::{Core, EnterRoom, MemberAction, WriteSync};
use wbf_sdk::matrix_endpoint::{self, MatrixEndpoint};

use super::{invalid_params, parse_params, Handle, Outcome, TargetParams};

/// 變數表：給了的才放（query 變數沒給就整個省掉，wbfuwunel 的 /docs/bridge-specs/index.md §1.3）。
fn to_variables(pairs: Vec<(&str, Option<Value>)>) -> Map<String, Value> {
    pairs
        .into_iter()
        .filter_map(|(name, value)| Some((name.to_string(), value?)))
        .collect()
}

fn to_json<T: serde::Serialize>(value: Option<T>) -> Option<Value> {
    value.and_then(|value| serde_json::to_value(value).ok())
}

async fn pass_through(
    handle: &Handle,
    core: &Core,
    endpoint: &MatrixEndpoint,
    variables: Map<String, Value>,
    body: Option<Value>,
    target: &TargetParams,
) -> Outcome {
    Ok(core
        .call_matrix_endpoint(endpoint, variables, body, &handle.target(target))
        .await?)
}

/// 沒有這一項／沒寫過是 `null`。
async fn find_through(
    handle: &Handle,
    core: &Core,
    endpoint: &MatrixEndpoint,
    variables: Map<String, Value>,
    target: &TargetParams,
) -> Outcome {
    Ok(core
        .find_via_matrix_endpoint(endpoint, variables, &handle.target(target))
        .await?
        .unwrap_or(Value::Null))
}

type JsonObject = Map<String, Value>;

/// params 的物件拆成：daemon 自己的欄位（`sync`、`user`、`server`、`room`…）之外剩下的，原樣交給 Matrix。
///
/// Return:
///     Ok((剩下的, 拿出來的))   拿出來的是 `taken` 列的那幾個（沒有就不在裡面）
///     Err(102)                params 不是物件
fn split_object(params: Value, taken: &[&str]) -> Result<(JsonObject, JsonObject), super::Fail> {
    let Value::Object(mut rest) = params else {
        return Err(invalid_params("params must be an object"));
    };
    let mut ours = Map::new();
    for name in taken {
        if let Some(value) = rest.remove(*name) {
            ours.insert(name.to_string(), value);
        }
    }
    Ok((rest, ours))
}

// ---- 成員與房間本身（§2.1）----

/// `{ encrypted: bool（必填）, …CreateRoom 的欄位原樣 }`
pub(super) async fn room_create(handle: &Handle, core: &Core, params: Value) -> Outcome {
    #[derive(Deserialize)]
    struct Ours {
        encrypted: bool,
        #[serde(default)]
        sync: WriteSync,
        #[serde(flatten)]
        target: TargetParams,
    }
    let (fields, ours) = split_object(params, &["encrypted", "sync", "user", "server"])?;
    if !ours.contains_key("encrypted") {
        return Err(invalid_params(
            "room.create needs `encrypted` (true or false): whether the room is end-to-end encrypted is chosen when it is made",
        ));
    }
    let ours: Ours = parse_params(Value::Object(ours))?;
    Ok(core
        .create_room(
            ours.encrypted,
            fields,
            ours.sync,
            &handle.target(&ours.target),
        )
        .await?)
}

#[derive(Deserialize)]
struct EnterParams {
    room: String,
    #[serde(default)]
    via: Vec<String>,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    sync: WriteSync,
    #[serde(flatten)]
    target: TargetParams,
}

pub(super) async fn room_join(handle: &Handle, core: &Core, params: Value) -> Outcome {
    enter(handle, core, params, EnterRoom::Join).await
}

pub(super) async fn room_knock(handle: &Handle, core: &Core, params: Value) -> Outcome {
    enter(handle, core, params, EnterRoom::Knock).await
}

async fn enter(handle: &Handle, core: &Core, params: Value, how: EnterRoom) -> Outcome {
    let params: EnterParams = parse_params(params)?;
    Ok(core
        .enter_room(
            how,
            &params.room,
            params.via,
            params.reason.as_deref(),
            params.sync,
            &handle.target(&params.target),
        )
        .await?)
}

pub(super) async fn room_leave(handle: &Handle, core: &Core, params: Value) -> Outcome {
    #[derive(Deserialize)]
    struct Params {
        room: String,
        #[serde(default)]
        reason: Option<String>,
        #[serde(default)]
        sync: WriteSync,
        #[serde(flatten)]
        target: TargetParams,
    }
    let params: Params = parse_params(params)?;
    Ok(core
        .leave_room(
            &params.room,
            params.reason.as_deref(),
            params.sync,
            &handle.target(&params.target),
        )
        .await?)
}

pub(super) async fn room_forget(handle: &Handle, core: &Core, params: Value) -> Outcome {
    #[derive(Deserialize)]
    struct Params {
        room: String,
        #[serde(default)]
        sync: WriteSync,
        #[serde(flatten)]
        target: TargetParams,
    }
    let params: Params = parse_params(params)?;
    Ok(core
        .forget_room(&params.room, params.sync, &handle.target(&params.target))
        .await?)
}

pub(super) async fn room_invite(handle: &Handle, core: &Core, params: Value) -> Outcome {
    act_on_member(handle, core, params, MemberAction::Invite).await
}

pub(super) async fn room_kick(handle: &Handle, core: &Core, params: Value) -> Outcome {
    act_on_member(handle, core, params, MemberAction::Kick).await
}

pub(super) async fn room_ban(handle: &Handle, core: &Core, params: Value) -> Outcome {
    act_on_member(handle, core, params, MemberAction::Ban).await
}

pub(super) async fn room_unban(handle: &Handle, core: &Core, params: Value) -> Outcome {
    act_on_member(handle, core, params, MemberAction::Unban).await
}

async fn act_on_member(
    handle: &Handle,
    core: &Core,
    params: Value,
    action: MemberAction,
) -> Outcome {
    #[derive(Deserialize)]
    struct Params {
        room: String,
        user_id: String,
        #[serde(default)]
        reason: Option<String>,
        #[serde(default)]
        sync: WriteSync,
        #[serde(flatten)]
        target: TargetParams,
    }
    let params: Params = parse_params(params)?;
    Ok(core
        .act_on_member(
            action,
            &params.room,
            &params.user_id,
            params.reason.as_deref(),
            params.sync,
            &handle.target(&params.target),
        )
        .await?)
}

pub(super) async fn room_upgrade(handle: &Handle, core: &Core, params: Value) -> Outcome {
    #[derive(Deserialize)]
    struct Params {
        room: String,
        new_version: String,
        #[serde(default)]
        sync: WriteSync,
        #[serde(flatten)]
        target: TargetParams,
    }
    let params: Params = parse_params(params)?;
    Ok(core
        .upgrade_room(
            &params.room,
            &params.new_version,
            params.sync,
            &handle.target(&params.target),
        )
        .await?)
}

pub(super) async fn room_members(handle: &Handle, core: &Core, params: Value) -> Outcome {
    #[derive(Deserialize)]
    struct Params {
        room: String,
        #[serde(default)]
        membership: Option<String>,
        #[serde(default)]
        not_membership: Option<String>,
        #[serde(flatten)]
        target: TargetParams,
    }
    let params: Params = parse_params(params)?;
    let variables = to_variables(vec![
        ("room_id", Some(json!(params.room))),
        ("membership", to_json(params.membership)),
        ("not_membership", to_json(params.not_membership)),
    ]);
    pass_through(
        handle,
        core,
        &matrix_endpoint::MEMBERS,
        variables,
        None,
        &params.target,
    )
    .await
}

#[derive(Deserialize)]
struct RoomParams {
    room: String,
    #[serde(flatten)]
    target: TargetParams,
}

pub(super) async fn room_joined_members(handle: &Handle, core: &Core, params: Value) -> Outcome {
    let params: RoomParams = parse_params(params)?;
    let variables = to_variables(vec![("room_id", Some(json!(params.room)))]);
    pass_through(
        handle,
        core,
        &matrix_endpoint::JOINED_MEMBERS,
        variables,
        None,
        &params.target,
    )
    .await
}

pub(super) async fn room_aliases(handle: &Handle, core: &Core, params: Value) -> Outcome {
    let params: RoomParams = parse_params(params)?;
    let variables = to_variables(vec![("room_id", Some(json!(params.room)))]);
    pass_through(
        handle,
        core,
        &matrix_endpoint::ROOM_ALIASES,
        variables,
        None,
        &params.target,
    )
    .await
}

pub(super) async fn room_get_visibility(handle: &Handle, core: &Core, params: Value) -> Outcome {
    let params: RoomParams = parse_params(params)?;
    let variables = to_variables(vec![("room_id", Some(json!(params.room)))]);
    pass_through(
        handle,
        core,
        &matrix_endpoint::GET_VISIBILITY,
        variables,
        None,
        &params.target,
    )
    .await
}

pub(super) async fn room_set_visibility(handle: &Handle, core: &Core, params: Value) -> Outcome {
    #[derive(Deserialize)]
    struct Params {
        room: String,
        visibility: String,
        #[serde(flatten)]
        target: TargetParams,
    }
    let params: Params = parse_params(params)?;
    let variables = to_variables(vec![("room_id", Some(json!(params.room)))]);
    let body = json!({ "visibility": params.visibility });
    pass_through(
        handle,
        core,
        &matrix_endpoint::SET_VISIBILITY,
        variables,
        Some(body),
        &params.target,
    )
    .await
}

/// 還沒加入也能問（被邀請時拿房名的另一條路）。
pub(super) async fn room_summary(handle: &Handle, core: &Core, params: Value) -> Outcome {
    #[derive(Deserialize)]
    struct Params {
        room: String,
        #[serde(default)]
        via: Vec<String>,
        #[serde(flatten)]
        target: TargetParams,
    }
    let params: Params = parse_params(params)?;
    let variables = to_variables(vec![
        ("room_id_or_alias", Some(json!(params.room))),
        ("via", (!params.via.is_empty()).then(|| json!(params.via))),
    ]);
    pass_through(
        handle,
        core,
        &matrix_endpoint::SUMMARY,
        variables,
        None,
        &params.target,
    )
    .await
}

pub(super) async fn room_hierarchy(handle: &Handle, core: &Core, params: Value) -> Outcome {
    #[derive(Deserialize)]
    struct Params {
        room: String,
        #[serde(default)]
        from: Option<String>,
        #[serde(default)]
        limit: Option<u64>,
        #[serde(default)]
        max_depth: Option<u64>,
        #[serde(default)]
        suggested_only: Option<bool>,
        #[serde(flatten)]
        target: TargetParams,
    }
    let params: Params = parse_params(params)?;
    let variables = to_variables(vec![
        ("room_id", Some(json!(params.room))),
        ("from", to_json(params.from)),
        ("limit", to_json(params.limit)),
        ("max_depth", to_json(params.max_depth)),
        ("suggested_only", to_json(params.suggested_only)),
    ]);
    pass_through(
        handle,
        core,
        &matrix_endpoint::HIERARCHY,
        variables,
        None,
        &params.target,
    )
    .await
}

pub(super) async fn room_mutual_rooms(handle: &Handle, core: &Core, params: Value) -> Outcome {
    #[derive(Deserialize)]
    struct Params {
        user_id: String,
        #[serde(default)]
        from: Option<String>,
        #[serde(flatten)]
        target: TargetParams,
    }
    let params: Params = parse_params(params)?;
    let variables = to_variables(vec![
        ("user_id", Some(json!(params.user_id))),
        ("from", to_json(params.from)),
    ]);
    pass_through(
        handle,
        core,
        &matrix_endpoint::MUTUAL_ROOMS,
        variables,
        None,
        &params.target,
    )
    .await
}

/// 公開目錄：只翻頁走 `GET`；給了 `filter`／`include_all_networks`／`third_party_instance_id`／`room_types` 任一個就走 `POST`（它們在 body 裡，`limit`／`since` 也一起放進 body）。
pub(super) async fn room_public_rooms(handle: &Handle, core: &Core, params: Value) -> Outcome {
    #[derive(Deserialize)]
    struct Params {
        /// 問哪一台的目錄（Matrix 的 `server`；`server` 這個名字已經是「哪個帳號」）
        #[serde(default)]
        directory_server: Option<String>,
        #[serde(default)]
        limit: Option<u64>,
        #[serde(default)]
        since: Option<String>,
        #[serde(default)]
        filter: Option<Value>,
        #[serde(default)]
        include_all_networks: Option<bool>,
        #[serde(default)]
        third_party_instance_id: Option<String>,
        #[serde(default)]
        room_types: Option<Value>,
        #[serde(flatten)]
        target: TargetParams,
    }
    let params: Params = parse_params(params)?;
    let is_filtered = params.filter.is_some()
        || params.include_all_networks.is_some()
        || params.third_party_instance_id.is_some()
        || params.room_types.is_some();
    if !is_filtered {
        let variables = to_variables(vec![
            ("server", to_json(params.directory_server)),
            ("limit", to_json(params.limit)),
            ("since", to_json(params.since)),
        ]);
        return pass_through(
            handle,
            core,
            &matrix_endpoint::PUBLIC_ROOMS,
            variables,
            None,
            &params.target,
        )
        .await;
    }
    let variables = to_variables(vec![("server", to_json(params.directory_server))]);
    let body = Value::Object(to_variables(vec![
        ("filter", params.filter),
        ("include_all_networks", to_json(params.include_all_networks)),
        (
            "third_party_instance_id",
            to_json(params.third_party_instance_id),
        ),
        ("room_types", params.room_types),
        ("limit", to_json(params.limit)),
        ("since", to_json(params.since)),
    ]));
    pass_through(
        handle,
        core,
        &matrix_endpoint::PUBLIC_ROOMS_FILTERED,
        variables,
        Some(body),
        &params.target,
    )
    .await
}

pub(super) async fn room_resolve_alias(handle: &Handle, core: &Core, params: Value) -> Outcome {
    #[derive(Deserialize)]
    struct Params {
        alias: String,
        #[serde(flatten)]
        target: TargetParams,
    }
    let params: Params = parse_params(params)?;
    let variables = to_variables(vec![("room_alias", Some(json!(params.alias)))]);
    pass_through(
        handle,
        core,
        &matrix_endpoint::GET_ALIAS,
        variables,
        None,
        &params.target,
    )
    .await
}

pub(super) async fn room_set_alias(handle: &Handle, core: &Core, params: Value) -> Outcome {
    #[derive(Deserialize)]
    struct Params {
        alias: String,
        room: String,
        #[serde(flatten)]
        target: TargetParams,
    }
    let params: Params = parse_params(params)?;
    let variables = to_variables(vec![("room_alias", Some(json!(params.alias)))]);
    let body = json!({ "room_id": params.room });
    pass_through(
        handle,
        core,
        &matrix_endpoint::SET_ALIAS,
        variables,
        Some(body),
        &params.target,
    )
    .await
}

pub(super) async fn room_delete_alias(handle: &Handle, core: &Core, params: Value) -> Outcome {
    #[derive(Deserialize)]
    struct Params {
        alias: String,
        #[serde(flatten)]
        target: TargetParams,
    }
    let params: Params = parse_params(params)?;
    let variables = to_variables(vec![("room_alias", Some(json!(params.alias)))]);
    pass_through(
        handle,
        core,
        &matrix_endpoint::DELETE_ALIAS,
        variables,
        None,
        &params.target,
    )
    .await
}

// ---- 房間狀態（§2.2）----

fn empty_state_key() -> String {
    String::new()
}

/// 那一項的 content；沒有這一項是 `null`。
pub(super) async fn room_get_state(handle: &Handle, core: &Core, params: Value) -> Outcome {
    #[derive(Deserialize)]
    struct Params {
        room: String,
        event_type: String,
        #[serde(default = "empty_state_key")]
        state_key: String,
        #[serde(flatten)]
        target: TargetParams,
    }
    let params: Params = parse_params(params)?;
    let variables = to_variables(vec![
        ("room_id", Some(json!(params.room))),
        ("event_type", Some(json!(params.event_type))),
        ("state_key", Some(json!(params.state_key))),
    ]);
    find_through(
        handle,
        core,
        &matrix_endpoint::GET_STATE_EVENT,
        variables,
        &params.target,
    )
    .await
}

pub(super) async fn room_set_state(handle: &Handle, core: &Core, params: Value) -> Outcome {
    #[derive(Deserialize)]
    struct Params {
        room: String,
        event_type: String,
        #[serde(default = "empty_state_key")]
        state_key: String,
        content: Map<String, Value>,
        #[serde(default)]
        sync: WriteSync,
        #[serde(flatten)]
        target: TargetParams,
    }
    let params: Params = parse_params(params)?;
    Ok(core
        .set_room_state(
            &params.room,
            &params.event_type,
            &params.state_key,
            Value::Object(params.content),
            params.sync,
            &handle.target(&params.target),
        )
        .await?)
}

/// 窄版：`state_key` 是空字串的那一項，content 由呼叫端組好。
async fn set_named_state(
    handle: &Handle,
    core: &Core,
    room: &str,
    event_type: &str,
    content: Value,
    sync: WriteSync,
    target: &TargetParams,
) -> Outcome {
    Ok(core
        .set_room_state(room, event_type, "", content, sync, &handle.target(target))
        .await?)
}

/// 窄版共用的 params：`room`、`sync`、帳號，其餘欄位各自定。
#[derive(Deserialize)]
struct StateParams<T> {
    room: String,
    #[serde(default)]
    sync: WriteSync,
    #[serde(flatten)]
    target: TargetParams,
    #[serde(flatten)]
    fields: T,
}

pub(super) async fn room_set_name(handle: &Handle, core: &Core, params: Value) -> Outcome {
    #[derive(Deserialize)]
    struct Fields {
        name: String,
    }
    let params: StateParams<Fields> = parse_params(params)?;
    let content = json!({ "name": params.fields.name });
    set_named_state(
        handle,
        core,
        &params.room,
        "m.room.name",
        content,
        params.sync,
        &params.target,
    )
    .await
}

pub(super) async fn room_set_topic(handle: &Handle, core: &Core, params: Value) -> Outcome {
    #[derive(Deserialize)]
    struct Fields {
        topic: String,
    }
    let params: StateParams<Fields> = parse_params(params)?;
    let content = json!({ "topic": params.fields.topic });
    set_named_state(
        handle,
        core,
        &params.room,
        "m.room.topic",
        content,
        params.sync,
        &params.target,
    )
    .await
}

pub(super) async fn room_set_avatar(handle: &Handle, core: &Core, params: Value) -> Outcome {
    #[derive(Deserialize)]
    struct Fields {
        /// mxc
        url: String,
    }
    let params: StateParams<Fields> = parse_params(params)?;
    let content = json!({ "url": params.fields.url });
    set_named_state(
        handle,
        core,
        &params.room,
        "m.room.avatar",
        content,
        params.sync,
        &params.target,
    )
    .await
}

/// ⚠️ 照 Matrix：整份覆蓋——沒給 `alt_aliases` 就是拿掉它們。
pub(super) async fn room_set_canonical_alias(
    handle: &Handle,
    core: &Core,
    params: Value,
) -> Outcome {
    #[derive(Deserialize)]
    struct Fields {
        #[serde(default)]
        alias: Option<String>,
        #[serde(default)]
        alt_aliases: Option<Vec<String>>,
    }
    let params: StateParams<Fields> = parse_params(params)?;
    let content = Value::Object(to_variables(vec![
        ("alias", to_json(params.fields.alias)),
        ("alt_aliases", to_json(params.fields.alt_aliases)),
    ]));
    set_named_state(
        handle,
        core,
        &params.room,
        "m.room.canonical_alias",
        content,
        params.sync,
        &params.target,
    )
    .await
}

pub(super) async fn room_set_history_visibility(
    handle: &Handle,
    core: &Core,
    params: Value,
) -> Outcome {
    #[derive(Deserialize)]
    struct Fields {
        history_visibility: String,
    }
    let params: StateParams<Fields> = parse_params(params)?;
    let content = json!({ "history_visibility": params.fields.history_visibility });
    set_named_state(
        handle,
        core,
        &params.room,
        "m.room.history_visibility",
        content,
        params.sync,
        &params.target,
    )
    .await
}

pub(super) async fn room_set_join_rule(handle: &Handle, core: &Core, params: Value) -> Outcome {
    #[derive(Deserialize)]
    struct Fields {
        join_rule: String,
        #[serde(default)]
        allow: Option<Vec<Value>>,
    }
    let params: StateParams<Fields> = parse_params(params)?;
    let content = Value::Object(to_variables(vec![
        ("join_rule", Some(json!(params.fields.join_rule))),
        ("allow", to_json(params.fields.allow)),
    ]));
    set_named_state(
        handle,
        core,
        &params.room,
        "m.room.join_rules",
        content,
        params.sync,
        &params.target,
    )
    .await
}

pub(super) async fn room_set_guest_access(handle: &Handle, core: &Core, params: Value) -> Outcome {
    #[derive(Deserialize)]
    struct Fields {
        guest_access: String,
    }
    let params: StateParams<Fields> = parse_params(params)?;
    let content = json!({ "guest_access": params.fields.guest_access });
    set_named_state(
        handle,
        core,
        &params.room,
        "m.room.guest_access",
        content,
        params.sync,
        &params.target,
    )
    .await
}

/// 明文房改成加密（/docs/design/rooms/room-actions.md §3.2）。開了就關不掉；`sync=server` 只給除錯。
pub(super) async fn room_enable_encryption(handle: &Handle, core: &Core, params: Value) -> Outcome {
    #[derive(Deserialize)]
    struct NoFields {}
    let params: StateParams<NoFields> = parse_params(params)?;
    let content = wbf_sdk::room_state_edit::to_encryption_content();
    set_named_state(
        handle,
        core,
        &params.room,
        "m.room.encryption",
        content,
        params.sync,
        &params.target,
    )
    .await
}

/// 欄位跟 `m.room.power_levels` 的 content 同名同格式（§4.2 的合併規則）。
pub(super) async fn room_set_power_levels(handle: &Handle, core: &Core, params: Value) -> Outcome {
    #[derive(Deserialize)]
    struct Ours {
        room: String,
        #[serde(default)]
        sync: WriteSync,
        #[serde(flatten)]
        target: TargetParams,
    }
    let (changes, ours) = split_object(params, &["room", "sync", "user", "server"])?;
    let ours: Ours = parse_params(Value::Object(ours))?;
    Ok(core
        .set_power_levels(&ours.room, changes, ours.sync, &handle.target(&ours.target))
        .await?)
}

pub(super) async fn room_pin(handle: &Handle, core: &Core, params: Value) -> Outcome {
    #[derive(Deserialize)]
    struct Fields {
        event_id: String,
        pinned: bool,
    }
    let params: StateParams<Fields> = parse_params(params)?;
    Ok(core
        .pin_event(
            &params.room,
            &params.fields.event_id,
            params.fields.pinned,
            params.sync,
            &handle.target(&params.target),
        )
        .await?)
}

// ---- 標籤與帳號資料（§2.3）：本地沒存，🚫 帶 `sync` ----

/// 自己的 mxid 與這間房：標籤、房間的帳號資料的路徑都要。
fn me_and_room(
    handle: &Handle,
    core: &Core,
    room: &str,
    target: &TargetParams,
) -> Result<Vec<(&'static str, Option<Value>)>, super::Fail> {
    let me = core.get_my_user_id(&handle.target(target))?;
    Ok(vec![
        ("user_id", Some(json!(me))),
        ("room_id", Some(json!(room))),
    ])
}

pub(super) async fn room_get_tags(handle: &Handle, core: &Core, params: Value) -> Outcome {
    let params: RoomParams = parse_params(params)?;
    let variables = to_variables(me_and_room(handle, core, &params.room, &params.target)?);
    pass_through(
        handle,
        core,
        &matrix_endpoint::GET_TAGS,
        variables,
        None,
        &params.target,
    )
    .await
}

pub(super) async fn room_set_tag(handle: &Handle, core: &Core, params: Value) -> Outcome {
    #[derive(Deserialize)]
    struct Params {
        room: String,
        tag: String,
        #[serde(default)]
        order: Option<f64>,
        #[serde(flatten)]
        target: TargetParams,
    }
    let params: Params = parse_params(params)?;
    let mut pairs = me_and_room(handle, core, &params.room, &params.target)?;
    pairs.push(("tag", Some(json!(params.tag))));
    let body = Value::Object(to_variables(vec![("order", to_json(params.order))]));
    pass_through(
        handle,
        core,
        &matrix_endpoint::SET_TAG,
        to_variables(pairs),
        Some(body),
        &params.target,
    )
    .await
}

pub(super) async fn room_delete_tag(handle: &Handle, core: &Core, params: Value) -> Outcome {
    #[derive(Deserialize)]
    struct Params {
        room: String,
        tag: String,
        #[serde(flatten)]
        target: TargetParams,
    }
    let params: Params = parse_params(params)?;
    let mut pairs = me_and_room(handle, core, &params.room, &params.target)?;
    pairs.push(("tag", Some(json!(params.tag))));
    pass_through(
        handle,
        core,
        &matrix_endpoint::DELETE_TAG,
        to_variables(pairs),
        None,
        &params.target,
    )
    .await
}

/// 沒寫過是 `null`。
pub(super) async fn room_get_account_data(handle: &Handle, core: &Core, params: Value) -> Outcome {
    #[derive(Deserialize)]
    struct Params {
        room: String,
        event_type: String,
        #[serde(flatten)]
        target: TargetParams,
    }
    let params: Params = parse_params(params)?;
    let mut pairs = me_and_room(handle, core, &params.room, &params.target)?;
    pairs.push(("event_type", Some(json!(params.event_type))));
    find_through(
        handle,
        core,
        &matrix_endpoint::GET_ROOM_ACCOUNT_DATA,
        to_variables(pairs),
        &params.target,
    )
    .await
}

pub(super) async fn room_set_account_data(handle: &Handle, core: &Core, params: Value) -> Outcome {
    #[derive(Deserialize)]
    struct Params {
        room: String,
        event_type: String,
        content: Map<String, Value>,
        #[serde(flatten)]
        target: TargetParams,
    }
    let params: Params = parse_params(params)?;
    let mut pairs = me_and_room(handle, core, &params.room, &params.target)?;
    pairs.push(("event_type", Some(json!(params.event_type))));
    pass_through(
        handle,
        core,
        &matrix_endpoint::SET_ROOM_ACCOUNT_DATA,
        to_variables(pairs),
        Some(Value::Object(params.content)),
        &params.target,
    )
    .await
}

/// 帳號層（例 `m.direct`）；沒寫過是 `null`。
pub(super) async fn account_get_data(handle: &Handle, core: &Core, params: Value) -> Outcome {
    #[derive(Deserialize)]
    struct Params {
        event_type: String,
        #[serde(flatten)]
        target: TargetParams,
    }
    let params: Params = parse_params(params)?;
    let me = core.get_my_user_id(&handle.target(&params.target))?;
    let variables = to_variables(vec![
        ("user_id", Some(json!(me))),
        ("event_type", Some(json!(params.event_type))),
    ]);
    find_through(
        handle,
        core,
        &matrix_endpoint::GET_ACCOUNT_DATA,
        variables,
        &params.target,
    )
    .await
}

/// ⚠️ 整份覆蓋（Matrix 的帳號資料沒有合併）。
pub(super) async fn account_set_data(handle: &Handle, core: &Core, params: Value) -> Outcome {
    #[derive(Deserialize)]
    struct Params {
        event_type: String,
        content: Map<String, Value>,
        #[serde(flatten)]
        target: TargetParams,
    }
    let params: Params = parse_params(params)?;
    let me = core.get_my_user_id(&handle.target(&params.target))?;
    let variables = to_variables(vec![
        ("user_id", Some(json!(me))),
        ("event_type", Some(json!(params.event_type))),
    ]);
    pass_through(
        handle,
        core,
        &matrix_endpoint::SET_ACCOUNT_DATA,
        variables,
        Some(Value::Object(params.content)),
        &params.target,
    )
    .await
}

/// 自己的 `m.direct` 加上或拿掉「`user_id` → `room`」（§2.5）。
pub(super) async fn room_set_direct(handle: &Handle, core: &Core, params: Value) -> Outcome {
    #[derive(Deserialize)]
    struct Params {
        room: String,
        user_id: String,
        direct: bool,
        #[serde(flatten)]
        target: TargetParams,
    }
    let params: Params = parse_params(params)?;
    Ok(core
        .set_direct(
            &params.room,
            &params.user_id,
            params.direct,
            &handle.target(&params.target),
        )
        .await?)
}

//! 批 3／批 4 其餘的 Matrix 端點（/docs/design/rpc-specs/matrix-relay.md §2）：推播規則、搜尋、關聯、在線狀態、過濾器、檢舉、server 的能力、媒體設定、TURN。
//! 跟房間動作同一套：params 照 Matrix 的名字填進端點的變數與 body，結果是 server 回的 body 原樣；本地**不**存，所以**不**帶 `sync`。
//! 中轉函式在 `room_actions.rs`（`pass_through`：server 拒了是 1400；`find_through`：404 是 `null`）。

use serde::Deserialize;
use serde_json::{json, Map, Value};
use wbf_core::Core;
use wbf_sdk::matrix_endpoint::{self, MatrixEndpoint};

use super::room_actions::{find_through, pass_through, split_object, to_json, to_variables};
use super::{invalid_params, parse_params, Fail, Handle, Outcome, TargetParams};

#[derive(Deserialize)]
struct OnlyTarget {
    #[serde(flatten)]
    target: TargetParams,
}

/// 推播規則的路徑變數：`kind` 是規則種類（`override`／`content`／`room`／`sender`／`underride`），**不是** pack 的 kind。
#[derive(Deserialize)]
struct RuleParams {
    kind: String,
    rule_id: String,
    #[serde(flatten)]
    target: TargetParams,
}

impl RuleParams {
    fn to_variables(&self) -> Map<String, Value> {
        to_variables(vec![
            ("kind", Some(json!(self.kind))),
            ("rule_id", Some(json!(self.rule_id))),
        ])
    }
}

/// 沒有任何參數的那幾支：`push.get_rules`、`push.get_global_rules`、`push.get_pushers`、`server.capabilities`、`media.config`。
async fn call_without_variables(
    handle: &Handle,
    core: &Core,
    endpoint: &MatrixEndpoint,
    params: Value,
) -> Outcome {
    let params: OnlyTarget = parse_params(params)?;
    pass_through(handle, core, endpoint, Map::new(), None, &params.target).await
}

// ---- 推播規則、pusher、通知（§2.1）----

pub(super) async fn push_get_rules(handle: &Handle, core: &Core, params: Value) -> Outcome {
    call_without_variables(handle, core, &matrix_endpoint::GET_PUSH_RULES, params).await
}

pub(super) async fn push_get_global_rules(handle: &Handle, core: &Core, params: Value) -> Outcome {
    call_without_variables(
        handle,
        core,
        &matrix_endpoint::GET_GLOBAL_PUSH_RULES,
        params,
    )
    .await
}

/// 沒有這條規則是 `null`（`kind` 打錯也是，server 兩種都回 404）。
pub(super) async fn push_get_rule(handle: &Handle, core: &Core, params: Value) -> Outcome {
    let params: RuleParams = parse_params(params)?;
    find_through(
        handle,
        core,
        &matrix_endpoint::GET_PUSH_RULE,
        params.to_variables(),
        &params.target,
    )
    .await
}

/// `{ kind, rule_id, before?, after?, …規則本身 }`：路徑與 query 變數以外的欄位原樣當 body（`actions`、`conditions`、`pattern`）。
pub(super) async fn push_set_rule(handle: &Handle, core: &Core, params: Value) -> Outcome {
    #[derive(Deserialize)]
    struct Ours {
        kind: String,
        rule_id: String,
        #[serde(default)]
        before: Option<String>,
        #[serde(default)]
        after: Option<String>,
        #[serde(flatten)]
        target: TargetParams,
    }
    let (rule, ours) = split_object(
        params,
        &["kind", "rule_id", "before", "after", "user", "server"],
    )?;
    let ours: Ours = parse_params(Value::Object(ours))?;
    let variables = to_variables(vec![
        ("kind", Some(json!(ours.kind))),
        ("rule_id", Some(json!(ours.rule_id))),
        ("before", to_json(ours.before)),
        ("after", to_json(ours.after)),
    ]);
    pass_through(
        handle,
        core,
        &matrix_endpoint::SET_PUSH_RULE,
        variables,
        Some(Value::Object(rule)),
        &ours.target,
    )
    .await
}

pub(super) async fn push_delete_rule(handle: &Handle, core: &Core, params: Value) -> Outcome {
    let params: RuleParams = parse_params(params)?;
    pass_through(
        handle,
        core,
        &matrix_endpoint::DELETE_PUSH_RULE,
        params.to_variables(),
        None,
        &params.target,
    )
    .await
}

pub(super) async fn push_get_rule_enabled(handle: &Handle, core: &Core, params: Value) -> Outcome {
    let params: RuleParams = parse_params(params)?;
    pass_through(
        handle,
        core,
        &matrix_endpoint::GET_PUSH_RULE_ENABLED,
        params.to_variables(),
        None,
        &params.target,
    )
    .await
}

pub(super) async fn push_set_rule_enabled(handle: &Handle, core: &Core, params: Value) -> Outcome {
    #[derive(Deserialize)]
    struct Params {
        enabled: bool,
        #[serde(flatten)]
        rule: RuleParams,
    }
    let params: Params = parse_params(params)?;
    pass_through(
        handle,
        core,
        &matrix_endpoint::SET_PUSH_RULE_ENABLED,
        params.rule.to_variables(),
        Some(json!({ "enabled": params.enabled })),
        &params.rule.target,
    )
    .await
}

pub(super) async fn push_get_rule_actions(handle: &Handle, core: &Core, params: Value) -> Outcome {
    let params: RuleParams = parse_params(params)?;
    pass_through(
        handle,
        core,
        &matrix_endpoint::GET_PUSH_RULE_ACTIONS,
        params.to_variables(),
        None,
        &params.target,
    )
    .await
}

/// `actions: []` 就是「這條規則什麼都不做」（靜音一個房間：`kind: "room"`、`rule_id` 用房間 id）。
pub(super) async fn push_set_rule_actions(handle: &Handle, core: &Core, params: Value) -> Outcome {
    #[derive(Deserialize)]
    struct Params {
        actions: Vec<Value>,
        #[serde(flatten)]
        rule: RuleParams,
    }
    let params: Params = parse_params(params)?;
    pass_through(
        handle,
        core,
        &matrix_endpoint::SET_PUSH_RULE_ACTIONS,
        params.rule.to_variables(),
        Some(json!({ "actions": params.actions })),
        &params.rule.target,
    )
    .await
}

pub(super) async fn push_get_pushers(handle: &Handle, core: &Core, params: Value) -> Outcome {
    call_without_variables(handle, core, &matrix_endpoint::GET_PUSHERS, params).await
}

/// 整份原樣當 body；`kind: null` 是**移除**那個 pusher，所以 `null` 照送、**不**拿掉。
pub(super) async fn push_set_pusher(handle: &Handle, core: &Core, params: Value) -> Outcome {
    let (pusher, ours) = split_object(params, &["user", "server"])?;
    let ours: OnlyTarget = parse_params(Value::Object(ours))?;
    pass_through(
        handle,
        core,
        &matrix_endpoint::SET_PUSHER,
        Map::new(),
        Some(Value::Object(pusher)),
        &ours.target,
    )
    .await
}

pub(super) async fn push_notifications(handle: &Handle, core: &Core, params: Value) -> Outcome {
    #[derive(Deserialize)]
    struct Params {
        #[serde(default)]
        from: Option<String>,
        #[serde(default)]
        limit: Option<u64>,
        #[serde(default)]
        only: Option<String>,
        #[serde(flatten)]
        target: TargetParams,
    }
    let params: Params = parse_params(params)?;
    let variables = to_variables(vec![
        ("from", to_json(params.from)),
        ("limit", to_json(params.limit)),
        ("only", to_json(params.only)),
    ]);
    pass_through(
        handle,
        core,
        &matrix_endpoint::NOTIFICATIONS,
        variables,
        None,
        &params.target,
    )
    .await
}

// ---- 搜尋與使用者目錄（§2.2）----

/// `next_batch` 從上一次回覆的 body（`search_categories.room_events.next_batch`）拿，這裡放進 query。
pub(super) async fn search_events(handle: &Handle, core: &Core, params: Value) -> Outcome {
    #[derive(Deserialize)]
    struct Params {
        search_categories: Map<String, Value>,
        #[serde(default)]
        next_batch: Option<String>,
        #[serde(flatten)]
        target: TargetParams,
    }
    let params: Params = parse_params(params)?;
    let variables = to_variables(vec![("next_batch", to_json(params.next_batch))]);
    let body = json!({ "search_categories": params.search_categories });
    pass_through(
        handle,
        core,
        &matrix_endpoint::SEARCH_EVENTS,
        variables,
        Some(body),
        &params.target,
    )
    .await
}

pub(super) async fn search_users(handle: &Handle, core: &Core, params: Value) -> Outcome {
    #[derive(Deserialize)]
    struct Params {
        search_term: String,
        #[serde(default)]
        limit: Option<u64>,
        #[serde(flatten)]
        target: TargetParams,
    }
    let params: Params = parse_params(params)?;
    let body = Value::Object(to_variables(vec![
        ("search_term", Some(json!(params.search_term))),
        ("limit", to_json(params.limit)),
    ]));
    pass_through(
        handle,
        core,
        &matrix_endpoint::SEARCH_USERS,
        Map::new(),
        Some(body),
        &params.target,
    )
    .await
}

// ---- 關聯、討論串、跳到某天（§2.3）----

/// 三支關聯端點選哪一支：給了 `rel_type` 才能再限 `event_type`（路徑一段接一段）。
///
/// Args:
///     has_rel_type: example: true
///     has_event_type: example: true
/// Return:
///     Ok(&MatrixEndpoint)   兩個都沒有 → `RELATIONS`；只有 `rel_type` → `RELATIONS_BY_REL_TYPE`；兩個都有 → `RELATIONS_BY_REL_TYPE_AND_EVENT_TYPE`
///     Err(102)              只有 `event_type`
fn pick_relations_endpoint(
    has_rel_type: bool,
    has_event_type: bool,
) -> Result<&'static MatrixEndpoint, Fail> {
    match (has_rel_type, has_event_type) {
        (false, false) => Ok(&matrix_endpoint::RELATIONS),
        (true, false) => Ok(&matrix_endpoint::RELATIONS_BY_REL_TYPE),
        (true, true) => Ok(&matrix_endpoint::RELATIONS_BY_REL_TYPE_AND_EVENT_TYPE),
        (false, true) => Err(invalid_params(
            "room.relations: `event_type` narrows a `rel_type`, so it needs `rel_type` too",
        )),
    }
}

pub(super) async fn room_relations(handle: &Handle, core: &Core, params: Value) -> Outcome {
    #[derive(Deserialize)]
    struct Params {
        room: String,
        event_id: String,
        #[serde(default)]
        rel_type: Option<String>,
        #[serde(default)]
        event_type: Option<String>,
        #[serde(default)]
        from: Option<String>,
        #[serde(default)]
        to: Option<String>,
        #[serde(default)]
        dir: Option<String>,
        #[serde(default)]
        limit: Option<u64>,
        #[serde(default)]
        recurse: Option<bool>,
        #[serde(flatten)]
        target: TargetParams,
    }
    let params: Params = parse_params(params)?;
    let endpoint = pick_relations_endpoint(params.rel_type.is_some(), params.event_type.is_some())?;
    let variables = to_variables(vec![
        ("room_id", Some(json!(params.room))),
        ("event_id", Some(json!(params.event_id))),
        ("rel_type", to_json(params.rel_type)),
        ("event_type", to_json(params.event_type)),
        ("from", to_json(params.from)),
        ("to", to_json(params.to)),
        ("dir", to_json(params.dir)),
        ("limit", to_json(params.limit)),
        ("recurse", to_json(params.recurse)),
    ]);
    pass_through(handle, core, endpoint, variables, None, &params.target).await
}

pub(super) async fn room_threads(handle: &Handle, core: &Core, params: Value) -> Outcome {
    #[derive(Deserialize)]
    struct Params {
        room: String,
        #[serde(default)]
        from: Option<String>,
        #[serde(default)]
        include: Option<String>,
        #[serde(default)]
        limit: Option<u64>,
        #[serde(flatten)]
        target: TargetParams,
    }
    let params: Params = parse_params(params)?;
    let variables = to_variables(vec![
        ("room_id", Some(json!(params.room))),
        ("from", to_json(params.from)),
        ("include", to_json(params.include)),
        ("limit", to_json(params.limit)),
    ]);
    pass_through(
        handle,
        core,
        &matrix_endpoint::THREADS,
        variables,
        None,
        &params.target,
    )
    .await
}

/// `ts`（毫秒）與 `dir`（`"f"`／`"b"`）Matrix 都必填，所以這裡也必填。
pub(super) async fn room_timestamp_to_event(
    handle: &Handle,
    core: &Core,
    params: Value,
) -> Outcome {
    #[derive(Deserialize)]
    struct Params {
        room: String,
        ts: u64,
        dir: String,
        #[serde(flatten)]
        target: TargetParams,
    }
    let params: Params = parse_params(params)?;
    let variables = to_variables(vec![
        ("room_id", Some(json!(params.room))),
        ("ts", Some(json!(params.ts))),
        ("dir", Some(json!(params.dir))),
    ]);
    pass_through(
        handle,
        core,
        &matrix_endpoint::TIMESTAMP_TO_EVENT,
        variables,
        None,
        &params.target,
    )
    .await
}

// ---- 在線狀態（§2.4）----

pub(super) async fn presence_get(handle: &Handle, core: &Core, params: Value) -> Outcome {
    #[derive(Deserialize)]
    struct Params {
        user_id: String,
        #[serde(flatten)]
        target: TargetParams,
    }
    let params: Params = parse_params(params)?;
    let variables = to_variables(vec![("user_id", Some(json!(params.user_id)))]);
    pass_through(
        handle,
        core,
        &matrix_endpoint::GET_PRESENCE,
        variables,
        None,
        &params.target,
    )
    .await
}

/// 路徑的 `user_id` 只能是自己：daemon 填目前帳號的 mxid。
pub(super) async fn presence_set(handle: &Handle, core: &Core, params: Value) -> Outcome {
    #[derive(Deserialize)]
    struct Params {
        presence: String,
        #[serde(default)]
        status_msg: Option<String>,
        #[serde(flatten)]
        target: TargetParams,
    }
    let params: Params = parse_params(params)?;
    let me = core.get_my_user_id(&handle.target(&params.target))?;
    let variables = to_variables(vec![("user_id", Some(json!(me)))]);
    let body = Value::Object(to_variables(vec![
        ("presence", Some(json!(params.presence))),
        ("status_msg", to_json(params.status_msg)),
    ]));
    pass_through(
        handle,
        core,
        &matrix_endpoint::SET_PRESENCE,
        variables,
        Some(body),
        &params.target,
    )
    .await
}

// ---- 過濾器（§2.5）：路徑的 `user_id` 一律是自己 ----

pub(super) async fn filter_create(handle: &Handle, core: &Core, params: Value) -> Outcome {
    #[derive(Deserialize)]
    struct Params {
        filter: Map<String, Value>,
        #[serde(flatten)]
        target: TargetParams,
    }
    let params: Params = parse_params(params)?;
    let me = core.get_my_user_id(&handle.target(&params.target))?;
    let variables = to_variables(vec![("user_id", Some(json!(me)))]);
    pass_through(
        handle,
        core,
        &matrix_endpoint::CREATE_FILTER,
        variables,
        Some(Value::Object(params.filter)),
        &params.target,
    )
    .await
}

/// 沒有這個過濾器是 `null`。
pub(super) async fn filter_get(handle: &Handle, core: &Core, params: Value) -> Outcome {
    #[derive(Deserialize)]
    struct Params {
        filter_id: String,
        #[serde(flatten)]
        target: TargetParams,
    }
    let params: Params = parse_params(params)?;
    let me = core.get_my_user_id(&handle.target(&params.target))?;
    let variables = to_variables(vec![
        ("user_id", Some(json!(me))),
        ("filter_id", Some(json!(params.filter_id))),
    ]);
    find_through(
        handle,
        core,
        &matrix_endpoint::GET_FILTER,
        variables,
        &params.target,
    )
    .await
}

// ---- 檢舉（§2.6）：`reason` 規格上有的必填，daemon **不**擋（交 server）----

fn to_reason_body(reason: Option<String>) -> Value {
    Value::Object(to_variables(vec![("reason", to_json(reason))]))
}

pub(super) async fn report_event(handle: &Handle, core: &Core, params: Value) -> Outcome {
    #[derive(Deserialize)]
    struct Params {
        room: String,
        event_id: String,
        #[serde(default)]
        reason: Option<String>,
        #[serde(default)]
        score: Option<i64>,
        #[serde(flatten)]
        target: TargetParams,
    }
    let params: Params = parse_params(params)?;
    let variables = to_variables(vec![
        ("room_id", Some(json!(params.room))),
        ("event_id", Some(json!(params.event_id))),
    ]);
    let body = Value::Object(to_variables(vec![
        ("reason", to_json(params.reason)),
        ("score", to_json(params.score)),
    ]));
    pass_through(
        handle,
        core,
        &matrix_endpoint::REPORT_EVENT,
        variables,
        Some(body),
        &params.target,
    )
    .await
}

pub(super) async fn report_room(handle: &Handle, core: &Core, params: Value) -> Outcome {
    #[derive(Deserialize)]
    struct Params {
        room: String,
        #[serde(default)]
        reason: Option<String>,
        #[serde(flatten)]
        target: TargetParams,
    }
    let params: Params = parse_params(params)?;
    let variables = to_variables(vec![("room_id", Some(json!(params.room)))]);
    pass_through(
        handle,
        core,
        &matrix_endpoint::REPORT_ROOM,
        variables,
        Some(to_reason_body(params.reason)),
        &params.target,
    )
    .await
}

pub(super) async fn report_user(handle: &Handle, core: &Core, params: Value) -> Outcome {
    #[derive(Deserialize)]
    struct Params {
        user_id: String,
        #[serde(default)]
        reason: Option<String>,
        #[serde(flatten)]
        target: TargetParams,
    }
    let params: Params = parse_params(params)?;
    let variables = to_variables(vec![("user_id", Some(json!(params.user_id)))]);
    pass_through(
        handle,
        core,
        &matrix_endpoint::REPORT_USER,
        variables,
        Some(to_reason_body(params.reason)),
        &params.target,
    )
    .await
}

// ---- server 的能力、媒體設定、連結預覽、TURN（§2.7）----

pub(super) async fn server_capabilities(handle: &Handle, core: &Core, params: Value) -> Outcome {
    call_without_variables(handle, core, &matrix_endpoint::CAPABILITIES, params).await
}

pub(super) async fn media_config(handle: &Handle, core: &Core, params: Value) -> Outcome {
    call_without_variables(handle, core, &matrix_endpoint::MEDIA_CONFIG, params).await
}

/// 被 server 關掉是 403（`1400`）：「這台不給預覽」，**不是**連結壞了。
pub(super) async fn media_preview_url(handle: &Handle, core: &Core, params: Value) -> Outcome {
    #[derive(Deserialize)]
    struct Params {
        url: String,
        #[serde(default)]
        ts: Option<u64>,
        #[serde(flatten)]
        target: TargetParams,
    }
    let params: Params = parse_params(params)?;
    let variables = to_variables(vec![
        ("url", Some(json!(params.url))),
        ("ts", to_json(params.ts)),
    ]);
    pass_through(
        handle,
        core,
        &matrix_endpoint::MEDIA_PREVIEW,
        variables,
        None,
        &params.target,
    )
    .await
}

/// 這台沒有 TURN（MSC4166 的 404）是 `null`：通話只能直連。
pub(super) async fn voip_turn_server(handle: &Handle, core: &Core, params: Value) -> Outcome {
    let params: OnlyTarget = parse_params(params)?;
    find_through(
        handle,
        core,
        &matrix_endpoint::TURN_SERVER,
        Map::new(),
        &params.target,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relations_pick_the_endpoint_by_what_was_given() {
        let name_of = |has_rel_type, has_event_type| {
            pick_relations_endpoint(has_rel_type, has_event_type)
                .ok()
                .map(|endpoint| endpoint.name)
        };
        assert_eq!(name_of(false, false), Some("Relations"));
        assert_eq!(name_of(true, false), Some("RelationsByRelType"));
        assert_eq!(name_of(true, true), Some("RelationsByRelTypeAndEventType"));
        assert_eq!(name_of(false, true), None);
    }
}

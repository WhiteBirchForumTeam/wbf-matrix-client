//! 房間相關的 Matrix 端點怎麼叫（/docs/design/rooms/room-actions.md §2）：**一張表，兩條路**。
//! wbf 帳號走橋（[`crate::WbfClient::call_matrix_endpoint`]）、一般 Matrix 帳號照同一個 HTTP 端點送（[`call_over_http`]）；
//! 兩條路吃同一份變數、同一份 body，回同一份 Matrix 的 body（server 的 e2e 釘過「走橋與走 HTTP 回的一樣」，本 repo issue #55）。
//!
//! 變數的規則照橋（wbfuwunel 的 /docs/bridge-specs/index.md §1.3）：path 變數是字串、🚫 `.`／`..`；query 變數是字串、數字、布林或它們的陣列；
//! 表上沒有的變數拒絕。兩條路送出前都過 [`check_variables`]，同一個錯在兩條路上長一樣。
//! 號碼的權威在 server 的 /docs/bridge-specs/index.md；已經在 `protocol.rs` 的那幾個（`Members`、`JoinedRooms`、`GetState`、`GetStateEvent`、`GetAccountData`）引用那裡的常數，🚫 再抄一次。

use serde_json::{Map, Value};

use crate::error::SdkError;
use crate::protocol::{self, BridgedEndpoint};
use wbf_wire::Kind;

/// HTTP 的方法。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HttpMethod {
    Get,
    Post,
    Put,
    Delete,
}

/// 一支 Matrix 端點：橋的號碼與它對到的 HTTP 端點。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MatrixEndpoint {
    /// 橋的規格裡的名字，給錯誤訊息用, example: "CreateRoom"
    pub name: &'static str,
    pub bridge: BridgedEndpoint,
    pub method: HttpMethod,
    /// `{變數}` 是 path 變數, example: "/_matrix/client/v3/rooms/{room_id}/leave"
    pub path: &'static str,
    /// query 變數的名字
    pub query: &'static [&'static str],
}

const fn room(subtype: u8) -> BridgedEndpoint {
    BridgedEndpoint {
        kind: Kind::Room,
        subtype,
    }
}

const fn event(subtype: u8) -> BridgedEndpoint {
    BridgedEndpoint {
        kind: Kind::Event,
        subtype,
    }
}

const fn account(subtype: u8) -> BridgedEndpoint {
    BridgedEndpoint {
        kind: Kind::Account,
        subtype,
    }
}

// ---- 0x13 Room（wbfuwunel 的 /docs/bridge-specs/0x13-room.md）----

pub const CREATE_ROOM: MatrixEndpoint = MatrixEndpoint {
    name: "CreateRoom",
    bridge: room(0x20),
    method: HttpMethod::Post,
    path: "/_matrix/client/v3/createRoom",
    query: &[],
};
pub const JOIN: MatrixEndpoint = MatrixEndpoint {
    name: "Join",
    bridge: room(0x21),
    method: HttpMethod::Post,
    path: "/_matrix/client/v3/join/{room_id_or_alias}",
    query: &["via", "server_name"],
};
pub const LEAVE: MatrixEndpoint = MatrixEndpoint {
    name: "Leave",
    bridge: room(0x22),
    method: HttpMethod::Post,
    path: "/_matrix/client/v3/rooms/{room_id}/leave",
    query: &[],
};
/// 還在房裡是 400：要先 Leave。
pub const FORGET: MatrixEndpoint = MatrixEndpoint {
    name: "Forget",
    bridge: room(0x23),
    method: HttpMethod::Post,
    path: "/_matrix/client/v3/rooms/{room_id}/forget",
    query: &[],
};
/// 被邀請的人在 **body** 的 `user_id`，🚫 meta。
pub const INVITE: MatrixEndpoint = MatrixEndpoint {
    name: "Invite",
    bridge: room(0x24),
    method: HttpMethod::Post,
    path: "/_matrix/client/v3/rooms/{room_id}/invite",
    query: &[],
};
pub const KICK: MatrixEndpoint = MatrixEndpoint {
    name: "Kick",
    bridge: room(0x25),
    method: HttpMethod::Post,
    path: "/_matrix/client/v3/rooms/{room_id}/kick",
    query: &[],
};
pub const BAN: MatrixEndpoint = MatrixEndpoint {
    name: "Ban",
    bridge: room(0x26),
    method: HttpMethod::Post,
    path: "/_matrix/client/v3/rooms/{room_id}/ban",
    query: &[],
};
pub const UNBAN: MatrixEndpoint = MatrixEndpoint {
    name: "Unban",
    bridge: room(0x27),
    method: HttpMethod::Post,
    path: "/_matrix/client/v3/rooms/{room_id}/unban",
    query: &[],
};
pub const JOINED_ROOMS: MatrixEndpoint = MatrixEndpoint {
    name: "JoinedRooms",
    bridge: protocol::BRIDGE_JOINED_ROOMS,
    method: HttpMethod::Get,
    path: "/_matrix/client/v3/joined_rooms",
    query: &[],
};
/// wbf server 的回應多帶房間版本號與裝置版本號（`protocol::BRIDGE_MEMBERS`）；一般 Matrix server 沒有。
pub const MEMBERS: MatrixEndpoint = MatrixEndpoint {
    name: "Members",
    bridge: protocol::BRIDGE_MEMBERS,
    method: HttpMethod::Get,
    path: "/_matrix/client/v3/rooms/{room_id}/members",
    query: &["membership", "not_membership"],
};
pub const GET_ALIAS: MatrixEndpoint = MatrixEndpoint {
    name: "GetAlias",
    bridge: room(0x2A),
    method: HttpMethod::Get,
    path: "/_matrix/client/v3/directory/room/{room_alias}",
    query: &[],
};
pub const SET_ALIAS: MatrixEndpoint = MatrixEndpoint {
    name: "SetAlias",
    bridge: room(0x2B),
    method: HttpMethod::Put,
    path: "/_matrix/client/v3/directory/room/{room_alias}",
    query: &[],
};
pub const DELETE_ALIAS: MatrixEndpoint = MatrixEndpoint {
    name: "DeleteAlias",
    bridge: room(0x2C),
    method: HttpMethod::Delete,
    path: "/_matrix/client/v3/directory/room/{room_alias}",
    query: &[],
};
pub const UPGRADE: MatrixEndpoint = MatrixEndpoint {
    name: "Upgrade",
    bridge: room(0x2D),
    method: HttpMethod::Post,
    path: "/_matrix/client/v3/rooms/{room_id}/upgrade",
    query: &[],
};
pub const KNOCK: MatrixEndpoint = MatrixEndpoint {
    name: "Knock",
    bridge: room(0x2E),
    method: HttpMethod::Post,
    path: "/_matrix/client/v3/knock/{room_id_or_alias}",
    query: &["via", "server_name"],
};
pub const JOINED_MEMBERS: MatrixEndpoint = MatrixEndpoint {
    name: "JoinedMembers",
    bridge: room(0x2F),
    method: HttpMethod::Get,
    path: "/_matrix/client/v3/rooms/{room_id}/joined_members",
    query: &[],
};
pub const GET_VISIBILITY: MatrixEndpoint = MatrixEndpoint {
    name: "GetVisibility",
    bridge: room(0x30),
    method: HttpMethod::Get,
    path: "/_matrix/client/v3/directory/list/room/{room_id}",
    query: &[],
};
pub const SET_VISIBILITY: MatrixEndpoint = MatrixEndpoint {
    name: "SetVisibility",
    bridge: room(0x31),
    method: HttpMethod::Put,
    path: "/_matrix/client/v3/directory/list/room/{room_id}",
    query: &[],
};
/// 還沒加入也能問。
pub const SUMMARY: MatrixEndpoint = MatrixEndpoint {
    name: "Summary",
    bridge: room(0x32),
    method: HttpMethod::Get,
    path: "/_matrix/client/v1/room_summary/{room_id_or_alias}",
    query: &["via"],
};
pub const HIERARCHY: MatrixEndpoint = MatrixEndpoint {
    name: "Hierarchy",
    bridge: room(0x33),
    method: HttpMethod::Get,
    path: "/_matrix/client/v1/rooms/{room_id}/hierarchy",
    query: &["from", "limit", "max_depth", "suggested_only"],
};
/// ⚠️ `user_id` 是 query 變數，🚫 路徑的一段。
pub const MUTUAL_ROOMS: MatrixEndpoint = MatrixEndpoint {
    name: "MutualRooms",
    bridge: room(0x34),
    method: HttpMethod::Get,
    path: "/_matrix/client/v1/mutual_rooms",
    query: &["user_id", "from"],
};
pub const PUBLIC_ROOMS: MatrixEndpoint = MatrixEndpoint {
    name: "PublicRooms",
    bridge: room(0x35),
    method: HttpMethod::Get,
    path: "/_matrix/client/v3/publicRooms",
    query: &["limit", "since", "server"],
};
/// 搜尋與過濾條件在 body。
pub const PUBLIC_ROOMS_FILTERED: MatrixEndpoint = MatrixEndpoint {
    name: "PublicRoomsFiltered",
    bridge: room(0x36),
    method: HttpMethod::Post,
    path: "/_matrix/client/v3/publicRooms",
    query: &["server"],
};
pub const ROOM_ALIASES: MatrixEndpoint = MatrixEndpoint {
    name: "RoomAliases",
    bridge: room(0x37),
    method: HttpMethod::Get,
    path: "/_matrix/client/v3/rooms/{room_id}/aliases",
    query: &[],
};

// ---- 0x14 Event 的狀態那三支（wbfuwunel 的 /docs/bridge-specs/0x14-event.md）----

/// ⚠️ 走橋時超過 2 MiB 是 `TooLarge`（`protocol::BRIDGE_ROOM_STATE`）。
pub const GET_STATE: MatrixEndpoint = MatrixEndpoint {
    name: "GetState",
    bridge: protocol::BRIDGE_ROOM_STATE,
    method: HttpMethod::Get,
    path: "/_matrix/client/v3/rooms/{room_id}/state",
    query: &[],
};
/// 回**只有 content**；沒有這一項是 404。`state_key` 空字串是一個值，🚫 省。
pub const GET_STATE_EVENT: MatrixEndpoint = MatrixEndpoint {
    name: "GetStateEvent",
    bridge: protocol::BRIDGE_STATE_EVENT,
    method: HttpMethod::Get,
    path: "/_matrix/client/v3/rooms/{room_id}/state/{event_type}/{state_key}",
    query: &[],
};
pub const SET_STATE_EVENT: MatrixEndpoint = MatrixEndpoint {
    name: "SetStateEvent",
    bridge: event(0x23),
    method: HttpMethod::Put,
    path: "/_matrix/client/v3/rooms/{room_id}/state/{event_type}/{state_key}",
    query: &[],
};

// ---- 0x11 Account 的帳號資料與標籤（wbfuwunel 的 /docs/bridge-specs/0x11-account.md）----

/// 沒寫過是 404。
pub const GET_ACCOUNT_DATA: MatrixEndpoint = MatrixEndpoint {
    name: "GetAccountData",
    bridge: protocol::BRIDGE_ACCOUNT_DATA,
    method: HttpMethod::Get,
    path: "/_matrix/client/v3/user/{user_id}/account_data/{event_type}",
    query: &[],
};
pub const SET_ACCOUNT_DATA: MatrixEndpoint = MatrixEndpoint {
    name: "SetAccountData",
    bridge: account(0x26),
    method: HttpMethod::Put,
    path: "/_matrix/client/v3/user/{user_id}/account_data/{event_type}",
    query: &[],
};
pub const GET_ROOM_ACCOUNT_DATA: MatrixEndpoint = MatrixEndpoint {
    name: "GetRoomAccountData",
    bridge: account(0x27),
    method: HttpMethod::Get,
    path: "/_matrix/client/v3/user/{user_id}/rooms/{room_id}/account_data/{event_type}",
    query: &[],
};
pub const SET_ROOM_ACCOUNT_DATA: MatrixEndpoint = MatrixEndpoint {
    name: "SetRoomAccountData",
    bridge: account(0x28),
    method: HttpMethod::Put,
    path: "/_matrix/client/v3/user/{user_id}/rooms/{room_id}/account_data/{event_type}",
    query: &[],
};
pub const GET_TAGS: MatrixEndpoint = MatrixEndpoint {
    name: "GetTags",
    bridge: account(0x29),
    method: HttpMethod::Get,
    path: "/_matrix/client/v3/user/{user_id}/rooms/{room_id}/tags",
    query: &[],
};
pub const SET_TAG: MatrixEndpoint = MatrixEndpoint {
    name: "SetTag",
    bridge: account(0x2A),
    method: HttpMethod::Put,
    path: "/_matrix/client/v3/user/{user_id}/rooms/{room_id}/tags/{tag}",
    query: &[],
};
pub const DELETE_TAG: MatrixEndpoint = MatrixEndpoint {
    name: "DeleteTag",
    bridge: account(0x2B),
    method: HttpMethod::Delete,
    path: "/_matrix/client/v3/user/{user_id}/rooms/{room_id}/tags/{tag}",
    query: &[],
};

impl MatrixEndpoint {
    /// Return:
    ///     Vec<&str>  path 模板裡的變數名，照出現順序, example: vec!["room_id"]
    pub fn list_path_variables(&self) -> Vec<&'static str> {
        self.path
            .split('/')
            .filter_map(|segment| segment.strip_prefix('{')?.strip_suffix('}'))
            .collect()
    }
}

/// 送出前驗變數（同橋的規則，wbfuwunel 的 /docs/bridge-specs/index.md §1.3）。
///
/// Args:
///     endpoint: example: LEAVE
///     variables: example: {"room_id": "!r:localhost"}
/// Return:
///     Ok(())
///     Err(Usage)   少了 path 變數、path 變數不是字串或是 `.`／`..`、query 變數的型別不對、多了表上沒有的變數
pub fn check_variables(
    endpoint: &MatrixEndpoint,
    variables: &Map<String, Value>,
) -> Result<(), SdkError> {
    let path_variables = endpoint.list_path_variables();
    let bad = |why: String| SdkError::Usage(format!("{}: {why}", endpoint.name));
    for name in &path_variables {
        match variables.get(*name) {
            Some(Value::String(value)) if value == "." || value == ".." => {
                return Err(bad(format!(
                    "`{name}` cannot be {value:?} (the URL would drop that segment)"
                )))
            }
            Some(Value::String(_)) => {}
            Some(other) => return Err(bad(format!("`{name}` must be a string, not {other}"))),
            None => return Err(bad(format!("`{name}` is missing"))),
        }
    }
    for (name, value) in variables {
        if path_variables.contains(&name.as_str()) {
            continue;
        }
        if !endpoint.query.contains(&name.as_str()) {
            return Err(bad(format!("`{name}` is not a variable of this endpoint")));
        }
        let is_scalar =
            |value: &Value| matches!(value, Value::String(_) | Value::Number(_) | Value::Bool(_));
        let fits = match value {
            Value::Array(items) => items.iter().all(is_scalar),
            other => is_scalar(other),
        };
        if !fits {
            return Err(bad(format!(
                "`{name}` must be a string, a number, a boolean, or a list of those"
            )));
        }
    }
    Ok(())
}

/// HTTP body 的 bytes：給了就原樣；POST／PUT 沒給送 `{}`（Matrix 這幾支的 body 都是物件）；GET／DELETE 沒有 body。
///
/// Return:
///     Ok(Vec<u8>)
///     Err(Usage)   給了 body 的 GET／DELETE（橋與 HTTP 都不會帶它，🚫 默默丟掉）
pub fn encode_body(endpoint: &MatrixEndpoint, body: Option<&Value>) -> Result<Vec<u8>, SdkError> {
    match (endpoint.method, body) {
        (HttpMethod::Get | HttpMethod::Delete, Some(_)) => Err(SdkError::Usage(format!(
            "{}: this endpoint takes no body",
            endpoint.name
        ))),
        (HttpMethod::Get | HttpMethod::Delete, None) => Ok(Vec::new()),
        (HttpMethod::Post | HttpMethod::Put, None) => Ok(b"{}".to_vec()),
        (HttpMethod::Post | HttpMethod::Put, Some(body)) => serde_json::to_vec(body)
            .map_err(|error| crate::error::cannot_serialize(endpoint.name, error)),
    }
}

/// 回應的 body → JSON。空的 body 當 `{}`（有的端點成功時不給 body）。
///
/// Return:
///     Ok(Value)
///     Err(Protocol)   不是 JSON
pub fn decode_reply_body(endpoint: &MatrixEndpoint, body: &[u8]) -> Result<Value, SdkError> {
    if body.is_empty() {
        return Ok(Value::Object(Map::new()));
    }
    serde_json::from_slice(body)
        .map_err(|error| SdkError::Protocol(format!("{} body is not JSON: {error}", endpoint.name)))
}

/// 照 HTTP 送一支端點（一般 Matrix 帳號）。
///
/// Args:
///     server: 這個帳號的 homeserver, example: "https://matrix.org"
///     access_token: 只進 `Authorization` 標頭
///     endpoint: example: LEAVE
///     variables: example: {"room_id": "!r:localhost"}
///     body: 給 POST／PUT, example: Some(&json!({"reason": "bye"}))
/// Return:
///     Ok(Value)       2xx 的 body
///     Err(Usage)      變數或 body 不合規則（[`check_variables`]、[`encode_body`]）、server 不是 URL
///     Err(Server)     非 2xx：meta 帶 `status` 與 `errcode`（[`crate::SdkError::is_not_found`] 認得出 404）
///     Err(Protocol)   2xx 但 body 不是 JSON
///     Err(Network)
pub async fn call_over_http(
    server: &str,
    access_token: &str,
    endpoint: &MatrixEndpoint,
    variables: &Map<String, Value>,
    body: Option<&Value>,
) -> Result<Value, SdkError> {
    check_variables(endpoint, variables)?;
    let url = to_url(server, endpoint, variables)?;
    let body = encode_body(endpoint, body)?;
    let client = reqwest::Client::builder()
        .connect_timeout(crate::link::LINE_SILENCE)
        .read_timeout(crate::link::LINE_SILENCE)
        .build()
        .map_err(|error| SdkError::Network(format!("{}: {error}", endpoint.name)))?;
    let request = match endpoint.method {
        HttpMethod::Get => client.get(url),
        HttpMethod::Post => client.post(url),
        HttpMethod::Put => client.put(url),
        HttpMethod::Delete => client.delete(url),
    };
    let request = request.bearer_auth(access_token);
    let request = match endpoint.method {
        HttpMethod::Post | HttpMethod::Put => request
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body),
        HttpMethod::Get | HttpMethod::Delete => request,
    };
    let response = request
        .send()
        .await
        .map_err(|error| SdkError::Network(format!("{}: {error}", endpoint.name)))?;
    if !response.status().is_success() {
        return Err(server_error_of(response).await);
    }
    let bytes = response
        .bytes()
        .await
        .map_err(|error| SdkError::Network(format!("{}: {error}", endpoint.name)))?;
    decode_reply_body(endpoint, &bytes)
}

/// 模板 → URL：path 變數每一段各自 percent-encode（`#別名` 變 `%23`、event id 的 `/` 變 `%2F`），query 變數照順序附上，陣列是重複的參數。
/// 呼叫前要先過 [`check_variables`]。
///
/// Return:
///     Ok(Url)
///     Err(Usage)   server 不是能接路徑的 URL
fn to_url(
    server: &str,
    endpoint: &MatrixEndpoint,
    variables: &Map<String, Value>,
) -> Result<reqwest::Url, SdkError> {
    let mut url = reqwest::Url::parse(server)
        .map_err(|error| SdkError::Usage(format!("server {server:?} is not a URL: {error}")))?;
    {
        let mut segments = url
            .path_segments_mut()
            .map_err(|()| SdkError::Usage(format!("server {server:?} cannot take a path")))?;
        segments.pop_if_empty();
        for segment in endpoint.path.trim_start_matches('/').split('/') {
            let variable = segment
                .strip_prefix('{')
                .and_then(|rest| rest.strip_suffix('}'));
            match variable {
                Some(name) => {
                    let value = variables.get(name).and_then(Value::as_str).ok_or_else(|| {
                        SdkError::Usage(format!("{}: `{name}` is missing", endpoint.name))
                    })?;
                    segments.push(value);
                }
                None => {
                    segments.push(segment);
                }
            }
        }
    }
    let query: Vec<(&str, String)> = variables
        .iter()
        .filter(|(name, _)| endpoint.query.contains(&name.as_str()))
        .flat_map(|(name, value)| {
            let values = match value {
                Value::Array(items) => items.iter().map(to_query_text).collect(),
                other => vec![to_query_text(other)],
            };
            values.into_iter().map(move |text| (name.as_str(), text))
        })
        .collect();
    if !query.is_empty() {
        let mut pairs = url.query_pairs_mut();
        for (name, text) in query {
            pairs.append_pair(name, &text);
        }
    }
    Ok(url)
}

fn to_query_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

/// 非 2xx → `Server`，meta 帶 `status` 與（有的話）`errcode`，讓 `is_not_found`、`matrix_errcode` 認得出來。
pub(crate) async fn server_error_of(response: reqwest::Response) -> SdkError {
    let status = response.status().as_u16();
    let body = response.bytes().await.unwrap_or_default();
    let parsed: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let errcode = parsed
        .get("errcode")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let message = parsed
        .get("error")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| String::from_utf8_lossy(&body).into_owned());
    SdkError::Server {
        code: match errcode.is_empty() {
            true => format!("HTTP_{status}"),
            false => errcode.clone(),
        },
        message,
        meta: serde_json::json!({ "status": status, "errcode": errcode }),
        code_id: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn vars(value: Value) -> Map<String, Value> {
        value.as_object().unwrap().clone()
    }

    #[test]
    fn path_variables_are_read_from_the_template() {
        assert_eq!(
            GET_STATE_EVENT.list_path_variables(),
            vec!["room_id", "event_type", "state_key"]
        );
        assert!(CREATE_ROOM.list_path_variables().is_empty());
    }

    /// 同橋的規則（wbfuwunel 的 /docs/bridge-specs/index.md §1.3）：缺、型別錯、`.`／`..`、多出來的都擋。
    #[test]
    fn variables_follow_the_bridge_rules() {
        assert!(check_variables(&LEAVE, &vars(json!({ "room_id": "!r:x" }))).is_ok());
        // 空字串是一個值（state_key）
        assert!(check_variables(
            &GET_STATE_EVENT,
            &vars(json!({ "room_id": "!r:x", "event_type": "m.room.name", "state_key": "" }))
        )
        .is_ok());
        assert!(check_variables(
            &JOIN,
            &vars(json!({ "room_id_or_alias": "#a:x", "via": ["x", "y"] }))
        )
        .is_ok());
        assert!(check_variables(
            &HIERARCHY,
            &vars(json!({ "room_id": "!r:x", "limit": 5, "suggested_only": true }))
        )
        .is_ok());
        for wrong in [
            json!({}),
            json!({ "room_id": 1 }),
            json!({ "room_id": ".." }),
            json!({ "room_id": "." }),
            json!({ "room_id": "!r:x", "user_id": "@b:x" }),
        ] {
            assert!(
                matches!(
                    check_variables(&LEAVE, &vars(wrong.clone())),
                    Err(SdkError::Usage(_))
                ),
                "{wrong}"
            );
        }
        assert!(matches!(
            check_variables(
                &JOIN,
                &vars(json!({ "room_id_or_alias": "#a:x", "via": [{ "x": 1 }] }))
            ),
            Err(SdkError::Usage(_))
        ));
    }

    #[test]
    fn a_body_goes_only_with_post_and_put() {
        assert_eq!(encode_body(&LEAVE, None).unwrap(), b"{}".to_vec());
        assert_eq!(encode_body(&GET_STATE, None).unwrap(), Vec::<u8>::new());
        assert!(encode_body(&GET_STATE, Some(&json!({}))).is_err());
        assert_eq!(
            encode_body(&INVITE, Some(&json!({ "user_id": "@b:x" }))).unwrap(),
            br#"{"user_id":"@b:x"}"#.to_vec()
        );
    }

    /// 每段各自編碼：`#` → `%23`、event id 的 `/` → `%2F`、空的 state_key 是最後一段空的；陣列是重複的參數。
    #[test]
    fn the_url_encodes_each_segment_and_repeats_lists() {
        let url = to_url(
            "http://localhost:6167/",
            &JOIN,
            &vars(json!({ "room_id_or_alias": "#lobby:localhost", "via": ["a.example", "b.example"] })),
        )
        .unwrap();
        assert_eq!(
            url.as_str(),
            "http://localhost:6167/_matrix/client/v3/join/%23lobby:localhost?via=a.example&via=b.example"
        );
        let url = to_url(
            "http://localhost:6167",
            &SET_STATE_EVENT,
            &vars(json!({ "room_id": "!r/x:localhost", "event_type": "m.room.name", "state_key": "" })),
        )
        .unwrap();
        assert_eq!(
            url.as_str(),
            "http://localhost:6167/_matrix/client/v3/rooms/!r%2Fx:localhost/state/m.room.name/"
        );
        let url = to_url(
            "http://localhost:6167",
            &HIERARCHY,
            &vars(json!({ "room_id": "!s:localhost", "limit": 20, "suggested_only": false })),
        )
        .unwrap();
        assert_eq!(
            url.as_str(),
            "http://localhost:6167/_matrix/client/v1/rooms/!s:localhost/hierarchy?limit=20&suggested_only=false"
        );
    }

    #[test]
    fn an_empty_reply_body_is_an_empty_object() {
        assert_eq!(decode_reply_body(&LEAVE, b"").unwrap(), json!({}));
        assert_eq!(
            decode_reply_body(&CREATE_ROOM, br#"{"room_id":"!r"}"#).unwrap(),
            json!({ "room_id": "!r" })
        );
        assert!(matches!(
            decode_reply_body(&LEAVE, b"nope"),
            Err(SdkError::Protocol(_))
        ));
    }
}

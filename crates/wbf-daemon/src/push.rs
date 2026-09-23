//! 推播（rpc-spec §4、link-pool.md §6）：core 的每一則事件變成一則沒有 `id` 的請求，**這裡決定要不要送**——
//! 訂閱集合（每條 RPC 連線一份）＋「自己發的長工作的進度不用訂」。維護者：「rpc 發送到 UI 的 function 裡面判斷這個包要不要過去」。

use std::collections::HashSet;

use serde_json::{json, Value};
use wbf_core::CoreEvent;

use crate::message::Request;

/// 一條 RPC 連線訂了什麼（rpc-spec §3.9）。連線關了就沒了。
#[derive(Default, Debug)]
pub struct Subscriptions {
    events: HashSet<String>,
    /// 只收這個帳號的；`None` ＝ 全部帳號。
    user: Option<String>,
}

impl Subscriptions {
    /// Args:
    ///     names: example: &["room.message".into(), "*".into()]
    ///     user: example: Some("@alice:localhost".into())；None 不改原本的
    /// Return:
    ///     Vec<String>  訂完之後的集合（排好序）
    pub fn subscribe(&mut self, names: &[String], user: Option<String>) -> Vec<String> {
        self.events.extend(names.iter().cloned());
        if user.is_some() {
            self.user = user;
        }
        self.list()
    }

    /// Return:
    ///     Vec<String>  退完之後剩下的
    pub fn unsubscribe(&mut self, names: &[String]) -> Vec<String> {
        for name in names {
            self.events.remove(name);
        }
        self.list()
    }

    pub fn list(&self) -> Vec<String> {
        let mut names: Vec<String> = self.events.iter().cloned().collect();
        names.sort();
        names
    }

    /// Return:
    ///     bool  1 = 什麼都沒訂（`desync` 也不該給它：它本來就收不到任何推播，漏了也沒有東西可漏；PR #53 審查 rumia 🔴2）
    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    /// 這則推播要不要給這條連線。
    ///
    /// Args:
    ///     method: 推播名, example: "room.message"
    ///     user: 這則是哪個帳號的；沒有帳號的（`desync`、`progress`）是 None, example: Some("@alice:localhost")
    /// Return:
    ///     bool  1 = 訂了這個名字（或 `"*"`），而且帳號對得上（沒指定帳號、或這則沒有帳號、或一樣）
    pub fn wants(&self, method: &str, user: Option<&str>) -> bool {
        let named = self.events.contains("*") || self.events.contains(method);
        let user_matches = match (&self.user, user) {
            (Some(wanted), Some(actual)) => wanted == actual,
            _ => true,
        };
        named && user_matches
    }
}

/// 一則要送的推播，加上「判斷要不要送」需要的兩個欄位。
pub struct Push {
    pub request: Request,
    /// 這則是哪個帳號的。
    pub user: Option<String>,
    /// 這則屬於哪個長工作（`progress`／`note`）：發那個請求的連線不用訂也收得到（rpc-spec §4）。
    pub job: Option<u64>,
}

/// 往一個 JSON 物件裡加一個欄位。`Value` 的 `[]=` 在不是物件時會 panic，這裡不會：不是物件就不加（這裡的 params 都是 `json!({...})`）。
///
/// Args:
///     params: example: json!({ "note": "hi" })
///     key: example: "id"
///     value: example: json!(7)
pub(crate) fn insert_field(params: &mut serde_json::Value, key: &str, value: serde_json::Value) {
    if let Some(fields) = params.as_object_mut() {
        fields.insert(key.to_string(), value);
    }
}

/// core 的事件 → 推播。名字與欄位照 rpc-spec §4。
pub fn push_of(event: &CoreEvent) -> Push {
    match event {
        // `id` 沒有就不帶（rpc-spec §4 的 `id?`），🚫 不送 `null`（PR #53 審查 cirno 🟡2）。
        CoreEvent::Note { job, text } => {
            let mut params = json!({ "note": text });
            if let Some(job) = job {
                insert_field(&mut params, "id", json!(job));
            }
            Push {
                request: Request::push("note", params),
                user: None,
                job: *job,
            }
        }
        CoreEvent::Progress {
            job,
            done,
            total,
            text,
        } => {
            let mut params = json!({ "done": done, "note": text });
            if let Some(job) = job {
                insert_field(&mut params, "id", json!(job));
            }
            if let Some(total) = total {
                insert_field(&mut params, "total", json!(total));
            }
            Push {
                request: Request::push("progress", params),
                user: None,
                job: *job,
            }
        }
        CoreEvent::Message { user, message } => Push {
            request: Request::push(
                "room.message",
                json!({ "user": user, "room": message.conversation, "message": message }),
            ),
            user: Some(user.clone()),
            job: None,
        },
        CoreEvent::SyncState {
            user,
            state,
            cg_seq,
        } => {
            let mut params = json!({ "user": user, "state": state });
            if let Some(cg_seq) = cg_seq {
                insert_field(&mut params, "cg_seq", json!(cg_seq));
            }
            Push {
                request: Request::push("sync.state", params),
                user: Some(user.clone()),
                job: None,
            }
        }
        CoreEvent::Link {
            user,
            role,
            state,
            reason,
        } => {
            let mut params = json!({ "user": user, "role": role, "state": state });
            if let Some(reason) = reason {
                insert_field(&mut params, "reason", json!(reason));
            }
            Push {
                request: Request::push("link.state", params),
                user: Some(user.clone()),
                job: None,
            }
        }
        CoreEvent::Received {
            user,
            role,
            kind,
            subtype,
            id,
            seq,
            route,
        } => Push {
            request: Request::push(
                "pack.received",
                json!({
                    "user": user, "role": role, "kind": kind, "subtype": subtype,
                    "id": id, "seq": seq, "route": route,
                }),
            ),
            user: Some(user.clone()),
            job: None,
        },
    }
}

/// 這條連線漏掉了 `missed` 則（daemon-runtime §5.3）。🚫 不重播、🚫 不假裝沒事。
pub fn desync(missed: u64) -> Request {
    Request::push("desync", json!({ "missed": missed }))
}

/// `subscribe`／`unsubscribe` 的 params：`events` 是字串陣列、`user` 是選填字串。
///
/// Return:
///     Ok((names, user))
///     Err(String)          給 `102` 的訊息
pub fn parse_subscription_params(params: &Value) -> Result<(Vec<String>, Option<String>), String> {
    let names = params
        .get("events")
        .and_then(Value::as_array)
        .ok_or_else(|| "events must be an array of push names".to_string())?
        .iter()
        .map(|name| {
            name.as_str()
                .map(str::to_string)
                .ok_or_else(|| "events must be an array of push names".to_string())
        })
        .collect::<Result<Vec<String>, String>>()?;
    let user = match params.get("user") {
        None | Some(Value::Null) => None,
        Some(Value::String(user)) => Some(user.clone()),
        Some(_) => return Err("user must be a string".to_string()),
    };
    Ok((names, user))
}

#[cfg(test)]
mod tests {
    use super::*;
    use wbf_core::{LinkRole, LinkState};

    #[test]
    fn a_subscription_filters_by_name_star_and_user() {
        let mut subscriptions = Subscriptions::default();
        assert!(
            !subscriptions.wants("link.state", Some("@a:x")),
            "沒訂就不推"
        );
        assert_eq!(
            subscriptions.subscribe(&["link.state".into()], Some("@a:x".into())),
            vec!["link.state".to_string()]
        );
        assert!(subscriptions.wants("link.state", Some("@a:x")));
        assert!(
            !subscriptions.wants("link.state", Some("@b:x")),
            "別的帳號不推"
        );
        assert!(
            subscriptions.wants("link.state", None),
            "沒有帳號的事件不受帳號過濾"
        );
        assert!(
            !subscriptions.wants("room.message", Some("@a:x")),
            "沒訂的名字不推"
        );
        subscriptions.subscribe(&["*".into()], None);
        assert!(
            subscriptions.wants("room.message", Some("@a:x")),
            "`*` 全訂"
        );
        assert_eq!(
            subscriptions.unsubscribe(&["*".into(), "nothing".into()]),
            vec!["link.state".to_string()]
        );
        assert!(!subscriptions.wants("room.message", Some("@a:x")));
    }

    #[test]
    fn every_core_event_has_a_push_name_from_the_spec() {
        let link = push_of(&CoreEvent::Link {
            user: "@a:x".into(),
            role: LinkRole::Subscriptions,
            state: LinkState::Closed,
            reason: Some("logged out".into()),
        });
        assert_eq!(link.request.method, "link.state");
        assert_eq!(link.request.id, None, "推播沒有 id");
        assert_eq!(link.user.as_deref(), Some("@a:x"));
        assert_eq!(
            link.request.params,
            json!({ "user": "@a:x", "role": "subscriptions", "state": "closed", "reason": "logged out" })
        );
        let progress = push_of(&CoreEvent::Progress {
            job: Some(7),
            done: 3,
            total: None,
            text: "chunk 3".into(),
        });
        assert_eq!(progress.request.method, "progress");
        assert_eq!(progress.job, Some(7));
        assert_eq!(progress.request.params["id"], json!(7));
        assert!(
            progress.request.params.get("total").is_none(),
            "不知道總數就不帶"
        );
        let orphan_note = push_of(&CoreEvent::Note {
            job: None,
            text: "hi".into(),
        });
        assert!(
            orphan_note.request.params.get("id").is_none(),
            "不在任何工作裡就不帶 id，不送 null：{}",
            orphan_note.request.params
        );
        let mut none = Subscriptions::default();
        assert!(none.is_empty());
        none.subscribe(&["*".into()], None);
        assert!(!none.is_empty());
        let received = push_of(&CoreEvent::Received {
            user: "@a:x".into(),
            role: LinkRole::Misc,
            kind: 0x16,
            subtype: 0x06,
            id: 1,
            seq: 2,
            route: wbf_sdk::Route::Subscription,
        });
        assert_eq!(received.request.method, "pack.received");
        assert_eq!(received.request.params["route"], json!("subscription"));
        assert_eq!(desync(44).params, json!({ "missed": 44 }));
    }

    #[test]
    fn subscription_params_are_checked() {
        assert_eq!(
            parse_subscription_params(&json!({ "events": ["a", "*"], "user": "@a:x" })).unwrap(),
            (
                vec!["a".to_string(), "*".to_string()],
                Some("@a:x".to_string())
            )
        );
        assert!(parse_subscription_params(&json!({})).is_err());
        assert!(parse_subscription_params(&json!({ "events": "a" })).is_err());
        assert!(parse_subscription_params(&json!({ "events": [1] })).is_err());
        assert!(parse_subscription_params(&json!({ "events": [], "user": 5 })).is_err());
    }
}

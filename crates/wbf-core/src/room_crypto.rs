//! 房間的加解密（/docs/design/keys/e2ee-rpc.md，維護者 2026-09-29 定的形狀）：refresh、加密送出、被 1506 擋之後自動重拿房間狀態、
//! 收到時解密、金鑰到了補解。
//!
//! - **狀態放 UI**：房間版本號與每個成員的裝置版本號（[`RoomDevices`]）由 UI 存，送出時帶回來；daemon 🚫 不存每房的快照。
//! - **送訊息🚫 綁發金鑰**（維護者 2026-10-05，/docs/design/keys/e2ee-rpc.md §3）：送出只在本機備好房間金鑰（sdk `encrypt_and_send` 裡），
//!   送完、refresh 完把這個房交給後台（`key_share.rs`）送金鑰，🚫 等它。
//! - **被 1506 擋**：daemon 自動 refresh（重拿成員、只重查變了的人、金鑰交給後台），把新的 [`RoomDevices`] 放進錯誤的 `data` 一起回；
//!   🚫 不自動重送（重送是 UI 的事，用同一個 `txn_id`）。
//! - **解密**：收到時有金鑰就解，密文明文一起存；沒金鑰只存密文。金鑰到了，找出那把 session 還沒解的立刻解、補存明文，
//!   並照收訊息那條路發 `room.message`（同 `event_id`，UI 當更新）。

use std::collections::BTreeMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use wbf_sdk::cache::UndecryptedFilter;
use wbf_sdk::crypto_engine::{OlmEngine, OutgoingRoomEvent, RoomRefresh, SendOutcome};
use wbf_sdk::device_version::{DeviceVersion, RoomDeviceVersions};
use wbf_sdk::event_json::messages_from_incoming;
use wbf_sdk::{IncomingEvent, Transport};

use crate::accounts::AccountDir;
use crate::backend_choice::MethodHome;
use crate::error::{CoreError, CoreErrorKind};
use crate::event::EventSink;
use crate::link_pool::LinkRole;
use crate::server_cache::ServerCache;
use crate::{Core, CoreEvent, Target};

/// 一個房間的房間版本號與每個 `join` 成員的裝置版本號：**UI 存**，送出時原樣帶回來（維護者 2026-09-29）。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoomDevices {
    /// example: 81234
    pub room_version: u64,
    /// 成員 → 裝置版本號, example: {"@bob:localhost": "3-810b7c3be4"}
    pub members: BTreeMap<String, String>,
}

/// `room.refresh_devices` 的結果，也是 1506 那則錯誤的 `data`：這一刻的房間狀態、這輪排給後台送的 to-device 數。
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RoomDevicesRefresh {
    #[serde(flatten)]
    pub devices: RoomDevices,
    /// 這輪在本機排好、交給後台送的 to-device 數（維護者 2026-10-05：欄位名留著、意思改了，🚫 代表已經送到）
    pub shared: usize,
}

/// 送一則文字的選項（`room.send_text`）。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SendOptions {
    /// 加密房必帶：UI 手上那份（`room.refresh_devices` 回的、或上一次 1506 的 `data`）。明文房不看它。
    pub room_devices: Option<RoomDevices>,
    /// 重送用同一個（server 冪等）；None ＝ daemon 產一個新的，被 1506 擋時放在 `data.txn_id` 回給 UI
    pub txn_id: Option<String>,
}

/// cache 裡要拿去補解的那一批。
pub(crate) enum StoredToDecrypt {
    /// 用這把房間金鑰加密的（收到那把金鑰時）, example: StoredToDecrypt::Session("SESSIONID".into())
    Session(String),
    /// 這幾則（`Recent` 剛寫進去的密文）, example: StoredToDecrypt::EventIds(vec!["$e".into()])
    EventIds(Vec<String>),
}

impl RoomDevices {
    /// Return:
    ///     Ok(RoomDeviceVersions)
    ///     Err(Usage)   某個成員的裝置版本號不是 `序號-雜湊`（UI 帶錯了；🚫 不猜）
    fn to_versions(&self) -> Result<RoomDeviceVersions, CoreError> {
        let mut members = BTreeMap::new();
        for (user_id, text) in &self.members {
            let version = DeviceVersion::parse(text).ok_or_else(|| {
                CoreError::new(
                    CoreErrorKind::Usage,
                    format!(
                        "room_devices.members[{user_id}] is {text:?}, not a device version like \"3-810b7c3be4\": \
                         pass back what room.refresh_devices returned"
                    ),
                )
            })?;
            members.insert(user_id.clone(), version);
        }
        Ok(RoomDeviceVersions {
            room_version: self.room_version,
            members,
        })
    }

    fn from_versions(versions: &RoomDeviceVersions) -> RoomDevices {
        RoomDevices {
            room_version: versions.room_version,
            members: versions
                .members
                .iter()
                .map(|(user_id, version)| (user_id.clone(), version.to_text()))
                .collect(),
        }
    }
}

impl RoomDevicesRefresh {
    fn from_refresh(refresh: &RoomRefresh) -> RoomDevicesRefresh {
        RoomDevicesRefresh {
            devices: RoomDevices::from_versions(&refresh.versions),
            shared: refresh.queued_to_device_requests,
        }
    }
}

impl Core {
    /// 確認這個房現在的人與裝置、把房間金鑰交給後台補給還沒有的裝置（UI 點進房、或自己發現版本號變了時叫）；🚫 等後台送完。
    ///
    /// Args:
    ///     room: example: "!r:localhost"
    ///     previous: UI 手上的上一份；帶了只重查裝置版本號變了的人，None ＝ 每個人都查（只查得多，送金鑰照樣只送缺的）
    /// Return:
    ///     Ok(RoomDevicesRefresh)   UI 存下 `devices`，下次送出帶回來
    ///     Err(Usage)               不是 wbf 帳號、或 `previous` 的裝置版本號形狀不對
    ///     Err(Server)              不在房裡（`Forbidden`）、server 沒給號碼、某人的裝置雜湊重查後仍對不上（fail closed，房間金鑰不發）
    ///     Err(Network)             線開不起來
    pub async fn refresh_room_devices(
        &self,
        room: &str,
        previous: Option<&RoomDevices>,
        target: &Target,
    ) -> Result<RoomDevicesRefresh, CoreError> {
        let account = self.account_or_current(target)?;
        let engine = self.olm_engine_of(&account).await?;
        let previous = previous.map(RoomDevices::to_versions).transpose()?;
        let mut client = self
            .client_of(
                &account,
                Transport::WebSocket,
                MethodHome::WbfSdkOnly,
                LinkRole::Misc,
            )
            .await?;
        let refresh = engine
            .refresh_room_devices(&mut client, room, previous.as_ref())
            .await?;
        drop(client);
        self.queue_room_key_share(
            &account,
            room,
            refresh.versions.members.keys().cloned().collect(),
        )
        .await;
        Ok(RoomDevicesRefresh::from_refresh(&refresh))
    }

    /// wbf 帳號在加密房送一則事件：在本機備好房間金鑰、加密、帶 UI 給的房間版本號送（sdk `encrypt_and_send`，🚫 上網分金鑰），
    /// 送完把這個房交給後台送金鑰（/docs/design/keys/e2ee-rpc.md §3.1）。
    /// 被 1506 擋 → 自動 refresh（`previous` 就是 UI 帶來的那份，只重查變了的人）→ 金鑰交給後台 → 回 `RoomDevicesChanged`，`data` 是新狀態加 `txn_id`。
    ///
    /// Args:
    ///     message: 明文的事件；`attachments` 跟密文同一個 `Event/Send` 宣告, example: OutgoingRoomEvent { event_type: "m.room.message".into(), content: json!({"msgtype":"m.text","body":"hi"}), txn_id: "wbf-1727600000-3".into(), attachments: vec![] }
    ///     devices: UI 手上那份
    /// Return:
    ///     Ok(String)                 event_id
    ///     Err(RoomDevicesChanged)    訊息沒送；`data`：`{room_version, members, shared, txn_id}`（重拿成功）或 `{txn_id, current_room_version}`（重拿也失敗，UI 自己叫 `room.refresh_devices`）
    ///     Err(Usage)                 `devices` 的形狀不對
    ///     Err(Server)／Err(Network)  其他拒絕、線開不起來
    pub(crate) async fn wbf_send_encrypted(
        &self,
        account: &AccountDir,
        room: &str,
        message: OutgoingRoomEvent,
        devices: &RoomDevices,
    ) -> Result<String, CoreError> {
        let engine = self.olm_engine_of(account).await?;
        let previous = devices.to_versions()?;
        let members: Vec<String> = devices.members.keys().cloned().collect();
        let mut client = self
            .client_of(
                account,
                Transport::WebSocket,
                MethodHome::WbfSdkOnly,
                LinkRole::Misc,
            )
            .await?;
        let txn_id = message.txn_id.clone();
        let outcome = engine
            .encrypt_and_send(&mut client, room, devices.room_version, &members, &message)
            .await?;
        let (current_room_version, blocked) = match outcome {
            SendOutcome::Sent { event_id } => {
                drop(client);
                self.queue_room_key_share(account, room, members).await;
                return Ok(event_id);
            }
            SendOutcome::RoomDevicesChanged {
                current_room_version,
                error,
            } => (current_room_version, error),
        };
        // 被擋了：daemon 自動做下一步（重拿房間狀態、金鑰交給後台），連同錯誤一起回；🚫 不自動重送。
        let refreshed = engine
            .refresh_room_devices(&mut client, room, Some(&previous))
            .await;
        drop(client);
        match refreshed {
            Ok(refresh) => {
                self.queue_room_key_share(
                    account,
                    room,
                    refresh.versions.members.keys().cloned().collect(),
                )
                .await;
                let fresh = RoomDevicesRefresh::from_refresh(&refresh);
                Err(CoreError::new(
                    CoreErrorKind::RoomDevicesChanged,
                    format!(
                        "{room}: {blocked}; the room was fetched again and its keys were queued for the new devices (room_version {}): \
                         send again with the room_devices in data and the same txn_id",
                        fresh.devices.room_version
                    ),
                )
                .with_data(json!({
                    "room_version": fresh.devices.room_version,
                    "members": fresh.devices.members,
                    "shared": fresh.shared,
                    "txn_id": txn_id,
                })))
            }
            Err(error) => Err(CoreError::new(
                CoreErrorKind::RoomDevicesChanged,
                format!(
                    "{room}: {blocked}; fetching the room again failed too ({error}): call room.refresh_devices, then send again with the same txn_id"
                ),
            )
            .with_data(json!({
                "txn_id": txn_id,
                "current_room_version": current_room_version,
            }))),
        }
    }
}

/// 一則 WS 收到的事件 → 要寫進 cache 的樣子：有引擎就讓它試著解（解得開密文明文一起存），沒有就原樣（密文標成沒解）。
///
/// Args:
///     engine: 這個帳號的 crypto 引擎；None ＝ 開不起來或不是 wbf 帳號
///     room: example: "!r:localhost"
///     raw: `Recent`／`Push` 給的原樣事件
pub(crate) async fn to_incoming(
    engine: Option<&OlmEngine>,
    room: &str,
    raw: Value,
) -> IncomingEvent {
    match engine {
        Some(engine) => engine.to_incoming(room, raw).await,
        None => IncomingEvent::from_ws_json(raw),
    }
}

/// 把 cache 裡這個讀者還沒解開的那一批拿去解，解開的補存明文（密文照留）；`announce` 就在 commit 之後照收訊息那條路發 `room.message`。
/// 解不開的不動（金鑰還沒到，下次還有機會）。
///
/// Args:
///     me: 讀者, example: "@alice:localhost"
///     room: example: "!r:localhost"
///     which: 哪一批
///     announce: true ＝ 發 `room.message`（金鑰到了補解時）；false ＝ 不發（`Recent` 剛拉的，UI 本來就會讀）
/// Return:
///     Ok(usize)   這次解開、寫進去的則數
///     Err(...)    cache 讀或寫失敗
pub(crate) async fn decrypt_stored(
    engine: &OlmEngine,
    cache: &Arc<ServerCache>,
    events: &EventSink,
    me: &str,
    room: &str,
    which: StoredToDecrypt,
    announce: bool,
) -> Result<usize, CoreError> {
    let ciphertexts = {
        let reader = cache.read().await;
        let filter = match &which {
            StoredToDecrypt::Session(session_id) => UndecryptedFilter::BySession(session_id),
            StoredToDecrypt::EventIds(event_ids) => UndecryptedFilter::ByEventIds(event_ids),
        };
        reader.list_undecrypted_ciphertexts(me, room, &filter)?
    };
    let mut opened = Vec::new();
    for ciphertext in ciphertexts {
        let incoming = engine.to_incoming(room, ciphertext).await;
        if matches!(incoming, IncomingEvent::Decrypted { .. }) {
            opened.push(incoming);
        }
    }
    if opened.is_empty() {
        return Ok(0);
    }
    let notices: Vec<CoreEvent> = if announce {
        messages_from_incoming(room, &opened)
            .into_iter()
            .map(|message| CoreEvent::Message {
                user: me.to_string(),
                message: Box::new(message),
            })
            .collect()
    } else {
        Vec::new()
    };
    let count = opened.len();
    let (me_here, room_here) = (me.to_string(), room.to_string());
    // 寫失敗：這批解開的明文沒存進去、也不發 `room.message`（沒落地的不通知）。密文還在 cache，錯誤帶上則數回給呼叫端講出來（PR #62 審查 rumia 🟡1）。
    cache
        .run(move |cache| cache.upsert_events(&me_here, &room_here, &opened))
        .await
        .map_err(|error| {
            CoreError::new(
                error.kind,
                format!("{count} newly decrypted message(s) in {room} could not be stored (they stay encrypted in the cache): {error}"),
            )
        })?;
    // commit 之後才發（PR #32 的規矩）。
    for notice in notices {
        events.emit(notice);
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use serde_json::{json, Value};
    use wbf_sdk::chat::MessageKind;
    use wbf_sdk::IncomingEvent;

    use super::*;
    use crate::test_support::*;

    /// 一條放進池裡的 `Misc`（記憶體對接的假 server）；這個房加密、成員只有自己、房間版本號 7。
    /// `Misc` 線接上假 server，房間（`ROOM`）在 server 那邊與本地都是 `encrypted` 那樣（送文字只看本地，wbf_rooms.rs）。
    async fn room_on_misc(core: &Core, account: &AccountDir, encrypted: bool) -> FakeServer {
        let (client, fake) = memory_client_with_hello(Arc::new(Mutex::new(Vec::new()))).await;
        let pool = core.pool_of_account(account).unwrap();
        drop(
            pool.acquire(LinkRole::Misc, || async move { Ok(client) })
                .await
                .unwrap(),
        );
        fake.room_is_encrypted
            .store(encrypted, std::sync::atomic::Ordering::SeqCst);
        remember_room(core, account, ROOM, encrypted).await;
        *fake.members.lock().unwrap() = Some(members_body(7));
        *fake.current_room_version.lock().unwrap() = Some(7);
        fake
    }

    fn options(devices: &RoomDevices, txn_id: &str) -> SendOptions {
        SendOptions {
            room_devices: Some(devices.clone()),
            txn_id: Some(txn_id.to_string()),
        }
    }

    /// /docs/design/keys/e2ee-rpc.md 的整條送出：加密房沒帶 `room_devices` 就拒（🚫 不送明文）；refresh 回 UI 要存的那份；帶著它送出去的是密文、帶那個號碼；
    /// 號碼過期被 1506 擋 → **daemon 自動重拿房間狀態**，錯誤是 `RoomDevicesChanged`、`data` 帶新狀態與同一個 `txn_id`，訊息沒送（🚫 不自動重送）；
    /// UI 帶新狀態、同一個 `txn_id` 重送就過。
    #[tokio::test]
    async fn an_encrypted_send_carries_the_room_version_and_a_stale_one_comes_back_with_the_fresh_state(
    ) {
        let dir = scratch("crypto-send");
        let (core, account) = core_with_wbf_account(&dir).await;
        let fake = room_on_misc(&core, &account, true).await;
        let target = Target::default();

        let refused = core
            .send_text(ROOM, "hi", &SendOptions::default(), &target)
            .await
            .unwrap_err();
        assert_eq!(refused.kind, CoreErrorKind::Usage, "{refused:?}");
        assert!(refused.message.contains("room_devices"), "{refused:?}");
        assert!(
            fake.sent_events.lock().unwrap().is_empty(),
            "🚫 加密房不送明文"
        );

        let refreshed = core
            .refresh_room_devices(ROOM, None, &target)
            .await
            .unwrap();
        assert_eq!(refreshed.devices.room_version, 7);
        assert_eq!(
            refreshed.devices.members.keys().collect::<Vec<_>>(),
            vec![ME]
        );
        let event_id = core
            .send_text(ROOM, "hi", &options(&refreshed.devices, "t1"), &target)
            .await
            .unwrap();
        assert_eq!(event_id, "$sent-1");
        {
            let sent = fake.sent_events.lock().unwrap();
            let (room, event_type, room_version, txn_id, content) = sent.last().unwrap();
            assert_eq!(
                (
                    room.as_str(),
                    event_type.as_str(),
                    *room_version,
                    txn_id.as_str()
                ),
                (ROOM, "m.room.encrypted", Some(7), "t1")
            );
            let content: Value = serde_json::from_slice(content).unwrap();
            assert!(
                content["ciphertext"].is_string(),
                "送出去的是密文：{content}"
            );
            assert!(!content.to_string().contains("\"hi\""), "明文不在線上");
        }

        // 有人換了裝置：server 的號碼變 9。帶 7 送 → 被擋，daemon 自動重拿，新狀態跟錯誤一起回。
        *fake.current_room_version.lock().unwrap() = Some(9);
        *fake.members.lock().unwrap() = Some(members_body(9));
        let blocked = core
            .send_text(ROOM, "again", &options(&refreshed.devices, "t2"), &target)
            .await
            .unwrap_err();
        assert_eq!(
            blocked.kind,
            CoreErrorKind::RoomDevicesChanged,
            "{blocked:?}"
        );
        let data = blocked.data.clone().expect("1506 帶新的房間狀態");
        assert_eq!(data["room_version"], 9);
        assert_eq!(data["txn_id"], "t2", "UI 重送要用同一個 txn_id");
        assert!(data["members"].get(ME).is_some(), "{data}");
        assert_eq!(
            fake.sent_events.lock().unwrap().len(),
            1,
            "被擋的那則沒送、🚫 不自動重送"
        );

        let fresh: RoomDevices = serde_json::from_value(data).unwrap();
        let resent = core
            .send_text(ROOM, "again", &options(&fresh, "t2"), &target)
            .await
            .unwrap();
        assert_eq!(resent, "$sent-2");
        assert_eq!(
            fake.sent_events.lock().unwrap().last().map(|sent| sent.2),
            Some(Some(9))
        );
        fake.task.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 明文房照舊：不看 `room_devices`、不帶號碼、送的是明文。
    #[tokio::test]
    async fn a_plaintext_room_sends_plaintext_without_room_devices() {
        let dir = scratch("crypto-plain");
        let (core, account) = core_with_wbf_account(&dir).await;
        let fake = room_on_misc(&core, &account, false).await;
        let event_id = core
            .send_text(ROOM, "hi", &SendOptions::default(), &Target::default())
            .await
            .unwrap();
        assert_eq!(event_id, "$sent-1");
        let sent = fake.sent_events.lock().unwrap();
        let (_, event_type, room_version, _, content) = sent.last().unwrap();
        assert_eq!(
            (event_type.as_str(), *room_version),
            ("m.room.message", None)
        );
        assert_eq!(
            serde_json::from_slice::<Value>(content).unwrap()["body"],
            "hi"
        );
        drop(sent);
        assert!(
            fake.bridge_calls.lock().unwrap().is_empty(),
            "送文字只看本地記的加不加密，🚫 為它問 server（維護者 2026-10-05）"
        );
        fake.task.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// UI 帶回來的裝置版本號形狀不對：`Usage`，🚫 不猜、不送。
    #[test]
    fn room_devices_with_a_bad_device_version_are_refused() {
        let devices = RoomDevices {
            room_version: 7,
            members: [(ME.to_string(), "not-a-version".to_string())].into(),
        };
        let error = devices.to_versions().unwrap_err();
        assert_eq!(error.kind, CoreErrorKind::Usage);
        assert!(error.message.contains(ME), "{error:?}");
    }

    /// 自己送一則加密的，回傳 (那則在線上的樣子, 它的 session id)——給補解的測試當「cache 裡還沒解的密文」。
    async fn an_encrypted_message(
        core: &Core,
        account: &AccountDir,
        event_id: &str,
    ) -> (Value, String) {
        let fake = room_on_misc(core, account, true).await;
        let refreshed = core
            .refresh_room_devices(ROOM, None, &Target::default())
            .await
            .unwrap();
        core.send_text(
            ROOM,
            "secret",
            &options(&refreshed.devices, event_id),
            &Target::default(),
        )
        .await
        .unwrap();
        let content: Value =
            serde_json::from_slice(&fake.sent_events.lock().unwrap().last().unwrap().4).unwrap();
        fake.task.abort();
        // 這條假線用完就關掉那格，之後的測試要用 `Misc` 會拿到自己放進去的那條。
        core.pool_of_account(account)
            .unwrap()
            .close(LinkRole::Misc, "test line done")
            .await;
        let session_id = content["session_id"].as_str().unwrap().to_string();
        let event = json!({
            "type": "m.room.encrypted", "event_id": event_id, "room_id": ROOM, "sender": ME,
            "origin_server_ts": 5, "content": content,
            "unsigned": { wbf_sdk::protocol::R_SEQ_KEY: 5, wbf_sdk::protocol::G_SEQ_KEY: 5 },
        });
        (event, session_id)
    }

    /// 金鑰到了（這裡：引擎解得開了）→ 找出那把 session 還沒解的、解開、補存明文、照收訊息那條路發 `room.message`（同 `event_id`）；
    /// 已經解了的不再動、不再發。
    #[tokio::test]
    async fn stored_ciphertexts_of_a_session_are_decrypted_stored_and_announced_once() {
        let dir = scratch("crypto-redecrypt");
        let (core, account) = core_with_wbf_account(&dir).await;
        let (ciphertext, session_id) = an_encrypted_message(&core, &account, "$enc").await;
        let (cache, me) = core.server_cache_and_me(&account).unwrap();
        cache
            .run(move |cache| {
                cache.upsert_events(
                    ME,
                    ROOM,
                    &[IncomingEvent::Undecrypted {
                        ciphertext,
                        reason: "MissingRoomKey".into(),
                    }],
                )
            })
            .await
            .unwrap();
        let mut seen = core.subscribe();
        let engine = core.olm_engine_of(&account).await.unwrap();

        let opened = decrypt_stored(
            &engine,
            &cache,
            &core.events,
            &me,
            ROOM,
            StoredToDecrypt::Session(session_id.clone()),
            true,
        )
        .await
        .unwrap();
        assert_eq!(opened, 1);
        let announced = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let CoreEvent::Message { message, .. } = seen.recv().await.unwrap() {
                    return message;
                }
            }
        })
        .await
        .expect("a room.message for the decrypted one");
        assert_eq!(announced.id, "$enc", "同一個 event_id，UI 當更新");
        assert!(
            matches!(&announced.kind, MessageKind::Text { body, .. } if body == "secret"),
            "{announced:?}"
        );
        let stored = cache
            .read()
            .await
            .list_messages_by_event_ids(&me, ROOM, &["$enc".to_string()])
            .unwrap();
        assert_eq!(
            stored.first().and_then(|message| message.decrypted),
            Some(true)
        );

        let again = decrypt_stored(
            &engine,
            &cache,
            &core.events,
            &me,
            ROOM,
            StoredToDecrypt::Session(session_id),
            true,
        )
        .await
        .unwrap();
        assert_eq!(again, 0, "解過的不再動");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 房間那條線推來一則密文、金鑰已經在：寫進 cache 的是解開的（密文照留）、`room.message` 帶明文。
    /// 同一條線推來 `DeviceChanged`：原樣轉成 `CoreEvent::DeviceChanged` 給 UI（daemon 自己🚫 不動作）。
    #[tokio::test]
    async fn the_rooms_line_decrypts_what_it_can_and_forwards_device_changes() {
        let dir = scratch("crypto-rooms-line");
        let (core, account) = core_with_wbf_account(&dir).await;
        let (ciphertext, _) = an_encrypted_message(&core, &account, "$pushed").await;
        let events: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
        let (subscription_id, fake, _pool) = subscribed(&core, &account, &events).await;
        let mut seen = core.subscribe();

        fake.outbound
            .send(push(subscription_id, 0, false, &[ciphertext]))
            .await
            .unwrap();
        let announced = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let CoreEvent::Message { message, .. } = seen.recv().await.unwrap() {
                    return message;
                }
            }
        })
        .await
        .expect("the pushed message is announced");
        assert_eq!(announced.id, "$pushed");
        assert!(
            matches!(&announced.kind, MessageKind::Text { body, .. } if body == "secret"),
            "{announced:?}"
        );
        assert_eq!(announced.decrypted, Some(true));

        fake.outbound
            .send(response(
                wbf_wire::Kind::Event,
                wbf_wire::pack::event::DEVICE_CHANGED,
                subscription_id,
                1,
                json!({ "user_id": "@b:localhost", "device_version": "4-0a1b2c3d4e", "rooms": { ROOM: 9 }, "gap": false }),
                Vec::new(),
            ))
            .await
            .unwrap();
        let changed = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let event @ CoreEvent::DeviceChanged { .. } = seen.recv().await.unwrap() {
                    return event;
                }
            }
        })
        .await
        .expect("the device change is forwarded");
        assert_eq!(
            changed,
            CoreEvent::DeviceChanged {
                user: ME.to_string(),
                changed_user: "@b:localhost".to_string(),
                device_version: "4-0a1b2c3d4e".to_string(),
                rooms: [(ROOM.to_string(), 9)].into(),
                gap: false,
            }
        );
        assert!(core.is_room_syncing(&account), "轉完照收");
        fake.task.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `sync.recent` 拉到的密文：收批回呼是同步的、當下不解；整輪拉完、回給 UI 之前解開補存（🚫 不發 `room.message`：Recent 本來就不推）。
    #[tokio::test]
    async fn recent_decrypts_what_it_pulled_before_answering() {
        let dir = scratch("crypto-recent");
        let (core, account) = core_with_wbf_account(&dir).await;
        let (ciphertext, _) = an_encrypted_message(&core, &account, "$pulled").await;
        let (misc, fake) = memory_client_with_hello(Arc::new(Mutex::new(vec![ciphertext]))).await;
        let pool = core.pool_of_account(&account).unwrap();
        drop(
            pool.acquire(LinkRole::Misc, || async move { Ok(misc) })
                .await
                .unwrap(),
        );
        let mut seen = core.subscribe();
        let summary = core
            .recent(
                wbf_sdk::RecentPlan::default(),
                None,
                true,
                Transport::WebSocket,
                &Target::default(),
            )
            .await
            .unwrap();
        assert_eq!(summary.pulled, 1);
        let (cache, me) = core.server_cache_and_me(&account).unwrap();
        let stored = cache
            .read()
            .await
            .list_messages_by_event_ids(&me, ROOM, &["$pulled".to_string()])
            .unwrap();
        let message = stored.first().expect("stored");
        assert_eq!(message.decrypted, Some(true), "{message:?}");
        assert!(
            matches!(&message.kind, MessageKind::Text { body, .. } if body == "secret"),
            "{message:?}"
        );
        assert!(
            !std::iter::from_fn(|| seen.try_recv().ok())
                .any(|event| matches!(event, CoreEvent::Message { .. })),
            "Recent 不推 room.message"
        );
        fake.task.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 等這個 `Core` 發出 `event_id` 那則**解開了的** `room.message`（先到密文、之後補解再發一次也算），回它的內文。
    async fn decrypted_body_of(
        seen: &mut tokio::sync::broadcast::Receiver<CoreEvent>,
        event_id: &str,
    ) -> String {
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                match seen.recv().await {
                    Ok(CoreEvent::Message { message, .. }) => {
                        if message.id == event_id && message.decrypted == Some(true) {
                            if let MessageKind::Text { body, .. } = &message.kind {
                                return body.clone();
                            }
                        }
                    }
                    Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    // 事件來源收攤了就不會再來：🚫 不空轉到 30 秒逾時（PR #62 審查 cirno 🟢）。
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        panic!("the event channel closed before {event_id} arrived decrypted")
                    }
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("{event_id} arrives decrypted within 30 s"))
    }

    /// /docs/design/keys/e2ee-rpc.md 整條對真的 wbfuwunel（#45 的驗收，這次走 daemon 的形狀）：
    /// alice、bob 各一個 `Core`（各自的資料目錄）登入、鉤子開五條線（`Keys` 線上傳裝置金鑰）→ alice `refresh` 拿到 UI 要存的 `RoomDevices`
    /// → 帶著它送加密訊息 → bob 收到的 `room.message` 是解開的；
    /// bob 再登一台新裝置（第三個 `Core`）→ alice 帶**舊的** `RoomDevices` 送 → 被 server 擋（1506），`RoomDevicesChanged` 的 `data` 是 daemon 自動重拿的新狀態、訊息沒送
    /// → alice 帶新狀態、同一個 `txn_id` 重送 → bob 的新裝置解得開（金鑰是 1506 之後那次 refresh 補給它的）。
    ///
    /// `--ignored`；環境變數：`WBF_E2E_SERVER`、`WBF_E2E_USER`（完整 mxid）、`WBF_E2E_PASSWORD_FILE`、`WBF_E2E_USER_B`、`WBF_E2E_PASSWORD_B_FILE`、
    /// `WBF_E2E_ENCRYPTED_ROOM`（兩人都在的加密房）。
    #[tokio::test]
    #[ignore = "needs a running wbfuwunel: WBF_E2E_SERVER, WBF_E2E_USER, WBF_E2E_PASSWORD_FILE, WBF_E2E_USER_B, WBF_E2E_PASSWORD_B_FILE, WBF_E2E_ENCRYPTED_ROOM"]
    async fn an_encrypted_conversation_survives_a_new_device_over_the_real_server() {
        let env = |name: &str| std::env::var(name).unwrap_or_else(|_| panic!("{name}"));
        let password_of = |file: &str| {
            std::fs::read_to_string(file)
                .unwrap()
                .trim_end_matches(['\r', '\n'])
                .to_string()
        };
        let server = env("WBF_E2E_SERVER");
        let room = env("WBF_E2E_ENCRYPTED_ROOM");
        let (alice, alice_password) = (
            env("WBF_E2E_USER"),
            password_of(&env("WBF_E2E_PASSWORD_FILE")),
        );
        let (bob, bob_password) = (
            env("WBF_E2E_USER_B"),
            password_of(&env("WBF_E2E_PASSWORD_B_FILE")),
        );
        let signed_in = |name: &'static str, user: String, password: String| {
            let server = server.clone();
            async move {
                let dir = scratch(name);
                let core = Core::open(&dir);
                core.create_vault(None).unwrap();
                core.log_in(&server, &user, &password, name, true)
                    .await
                    .unwrap_or_else(|error| panic!("{name} logs in: {error}"));
                let ensured = core.ensure_links().await;
                assert!(ensured.failed.is_empty(), "{name}: {ensured:?}");
                (core, dir)
            }
        };
        let target = Target::default();
        let (core_a, dir_a) = signed_in("real-e2ee-alice", alice.clone(), alice_password).await;
        let (core_b, dir_b) = signed_in("real-e2ee-bob", bob.clone(), bob_password.clone()).await;
        // 送文字只看本地記的加不加密（維護者 2026-10-05）：照 UI 的順序先拿那間房。
        core_a
            .conversation(&room, crate::SyncMode::Both, &target)
            .await
            .expect("alice fetches the room");
        let mut seen_b = core_b.subscribe();

        let first = core_a
            .refresh_room_devices(&room, None, &target)
            .await
            .unwrap();
        assert!(
            first.devices.members.contains_key(&bob),
            "bob 在成員裡：{first:?}"
        );
        let body_1 = format!("e2ee rpc 1 {}", std::process::id());
        let event_1 = core_a
            .send_text(
                &room,
                &body_1,
                &options(&first.devices, &format!("t1-{}", std::process::id())),
                &target,
            )
            .await
            .expect("alice sends encrypted");
        assert_eq!(
            decrypted_body_of(&mut seen_b, &event_1).await,
            body_1,
            "bob 解得開"
        );

        // bob 登一台新裝置：他的裝置版本號變了，房間版本號跟著變。
        let (core_c, dir_c) = signed_in("real-e2ee-bob-2", bob.clone(), bob_password).await;
        let mut seen_c = core_c.subscribe();
        let body_2 = format!("e2ee rpc 2 {}", std::process::id());
        let txn_2 = format!("t2-{}", std::process::id());
        let blocked = core_a
            .send_text(&room, &body_2, &options(&first.devices, &txn_2), &target)
            .await
            .expect_err("the stale room version is refused by the server");
        assert_eq!(
            blocked.kind,
            CoreErrorKind::RoomDevicesChanged,
            "{blocked:?}"
        );
        let data = blocked.data.clone().expect("1506 帶新的房間狀態");
        assert_eq!(data["txn_id"], txn_2.as_str());
        let fresh: RoomDevices = serde_json::from_value(data).unwrap();
        assert_ne!(fresh.room_version, first.devices.room_version, "號碼變了");
        assert_ne!(
            fresh.members.get(&bob),
            first.devices.members.get(&bob),
            "bob 的裝置版本號變了"
        );

        let event_2 = core_a
            .send_text(&room, &body_2, &options(&fresh, &txn_2), &target)
            .await
            .expect("resending with the fresh state and the same txn_id goes through");
        assert_eq!(
            decrypted_body_of(&mut seen_c, &event_2).await,
            body_2,
            "bob 的新裝置解得開"
        );
        assert_eq!(
            decrypted_body_of(&mut seen_b, &event_2).await,
            body_2,
            "bob 的舊裝置也是"
        );

        for (core, user, dir) in [
            (core_c, &bob, dir_c),
            (core_b, &bob, dir_b),
            (core_a, &alice, dir_a),
        ] {
            let _ = core.log_out(user, None, true, true).await;
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    /// 收到時就有金鑰：`to_incoming` 直接解（密文照帶）；沒引擎（開不起來）就原樣、標成沒解。
    #[tokio::test]
    async fn an_incoming_ciphertext_is_decrypted_when_the_key_is_here() {
        let dir = scratch("crypto-incoming");
        let (core, account) = core_with_wbf_account(&dir).await;
        let (ciphertext, _) = an_encrypted_message(&core, &account, "$in").await;
        let engine = core.olm_engine_of(&account).await.unwrap();
        assert!(matches!(
            to_incoming(Some(&engine), ROOM, ciphertext.clone()).await,
            IncomingEvent::Decrypted {
                ciphertext: Some(_),
                ..
            }
        ));
        assert!(matches!(
            to_incoming(None, ROOM, ciphertext).await,
            IncomingEvent::Undecrypted { .. }
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }
}

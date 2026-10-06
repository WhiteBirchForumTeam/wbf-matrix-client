//! E2EE 引擎：把 `matrix-sdk-crypto` 的 `OlmMachine` **只當狀態機用**（/docs/design/keys/e2ee-walkthrough.md §13）——
//! server 給的推進去（to-device、自己的 OTK 存量），它要送的拉出來（`outgoing_requests`）走我們的橋送，回應再交回去。
//! 網路、水位、銷毀都在這個 crate 自己手上；🚫 沒有 `/sync`、🚫 沒有 matrix-sdk 的 `Client`。
//!
//! 這是 wbf-sdk 裡**第二個**碰上游的地方（第一個是 `backend/matrix_sdk`）；兩者共用同一個 sqlite crypto store（`m/`），
//! ⚠️ 同一時間只能有一個持有者（/docs/design/keys/e2ee-walkthrough.md §13 第 9 條）——過渡期由呼叫端保證不同時開。
//!
//! 涵蓋到哪：金鑰上傳／查詢／claim、收 to-device、建／換房間金鑰並送到每台裝置（`distribute_room_key`，後台叫）、
//! 房間金鑰就緒了沒（`room_key_state`）、**只用已經分好的金鑰**加密帶房間版本號送出（`Event/Send`，🚫 產生金鑰、🚫 上網分金鑰）、
//! 解密（WS 收到的密文 → 要寫進 cache 的樣子）。什麼時候準備金鑰、等它就緒、什麼時候換是呼叫端的事（daemon 的後台，/docs/design/keys/e2ee-rpc.md §3、§3.1）。

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures_util::FutureExt as _;

use matrix_sdk::ruma;
use matrix_sdk::{SqliteCryptoStore, SqliteStoreConfig};
use matrix_sdk_crypto::store::types::RoomKeyInfo;
use matrix_sdk_crypto::types::events::room::encrypted::EncryptedEvent;
use matrix_sdk_crypto::types::requests::{AnyOutgoingRequest, OutgoingRequest, ToDeviceRequest};
use matrix_sdk_crypto::{
    CollectStrategy, DecryptionSettings, EncryptionSettings, EncryptionSyncChanges, OlmMachine,
    OlmMachineBuilder, TrustRequirement,
};
use ruma::api::auth_scheme::SendAccessToken;
use ruma::api::client::keys::{claim_keys, get_keys, upload_keys, upload_signatures};
use ruma::api::client::sync::sync_events::DeviceLists;
use ruma::api::client::to_device::send_event_to_device;
use ruma::api::{IncomingResponseExt as _, OutgoingRequestExt as _, SupportedVersions};
use ruma::events::{AnyMessageLikeEventContent, AnyToDeviceEvent};
use ruma::serde::Raw;
use ruma::{DeviceId, OneTimeKeyAlgorithm, OwnedRoomId, OwnedUserId, RoomId, UInt, UserId};

use crate::channel::PackChannel;
use crate::client::{DeviceWindow, WbfClient};
use crate::device_version::{compute_device_keys_hash, MembersDiff, RoomDeviceVersions};
use crate::error_code::WbfErrorCode;
use crate::incoming::IncomingEvent;
use crate::protocol::{self, BridgedEndpoint, DeviceFetchRequest, NoVariables, SendRequest};
use crate::to_device_state::ToDeviceState;
use crate::vault::Key32;
use crate::SdkError;

/// 一輪 `send_outgoing_requests` 最多繞幾圈：狀態機每送完一批可能又生出下一批（上傳完金鑰就要查、查完就要 claim），
/// 但不會沒完沒了；繞到這個數還有東西，是我們或上游的 bug，停下來報錯比轉到天荒地老好。
const OUTGOING_ROUNDS_LIMIT: usize = 16;

/// 走橋時 ruma 請求要一個 base URL 才能組 `http::Request`；橋只看 body，這個字串不會出現在線上。
const BRIDGE_BASE_URL: &str = "http://bridge.invalid";

/// 一窗 to-device 走完「匯入 → 落地 → 銷毀」之後的結果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImportReport {
    /// 這一窗帶進來的新房間金鑰（呼叫者拿它決定重解哪些密文）。
    pub room_keys: Vec<RoomKeyInfo>,
    /// 這一窗匯進 crypto store 的則數（狀態機吃過，不等於解得開）。
    pub imported: usize,
    /// 這一輪 server 說已經沒了的 count（含上次沒銷成、這次補送的）。
    pub destroyed: Vec<u64>,
    /// 落地後的「處理過的最新 count」（紀錄，🚫 不是 `Fetch` 的游標）。
    pub cd_seq: Option<u64>,
    /// 還留在待銷毀清單上的（server 這輪沒回來的，下次再送）。
    pub still_to_destroy: usize,
}

/// `pull_to_device` 最多拉幾窗：一窗 1000 則、六十四窗就是六萬多則 to-device，到這個數還沒追平是不對勁，停下來報錯。
const PULL_WINDOWS_LIMIT: usize = 64;

/// 房間金鑰還剩幾則、多久就到期時，算「該提早換了」：送出之後後台先換一把、先分完，下一則拿到的就是就緒的（維護者 2026-10-06，/docs/design/keys/e2ee-rpc.md §3.1）。
/// 房間自己設的期限很短時，門檻跟著縮到期限的四分之一（[`pre_rotate_messages`]、[`pre_rotate_age`]）：不然「剩一小時」對一小時的期限永遠成立，每送一則就換一把。
const PRE_ROTATE_MESSAGES: u64 = 5;
const PRE_ROTATE_AGE: Duration = Duration::from_secs(60 * 60);
/// 上游把 `rotation_period_ms` 夾在一小時以上才算到期（`OutboundGroupSession::safe_rotation_period`，防房主設太短）：我們算「快到期」用同一個下限。
const UPSTREAM_MIN_ROTATION_AGE: Duration = Duration::from_secs(60 * 60);
/// 上游把 `rotation_period_msgs` 夾在 1 到 10 000 之間（`OutboundGroupSession::expired`）。
const UPSTREAM_MAX_ROTATION_MESSAGES: u64 = 10_000;

/// 房間自己設的換金鑰期限（`m.room.encryption` 的 `rotation_period_ms`／`rotation_period_msgs`，/docs/design/keys/e2ee-rpc.md §3.1）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RoomKeyRotation {
    /// 一把房間金鑰最多用多久, example: Duration::from_secs(7 * 24 * 3600)
    pub max_age: Duration,
    /// 一把房間金鑰最多加密幾則, example: 100
    pub max_messages: u64,
}

impl RoomKeyRotation {
    /// Args:
    ///     content: 這個房 `m.room.encryption` 的 content；None 是沒有這個狀態事件, example: Some(&json!({"algorithm": "m.megolm.v1.aes-sha2", "rotation_period_msgs": 20}))
    /// Return:
    ///     RoomKeyRotation   沒設、或不是正整數的那一項用上游預設（一週／100 則）；太短的照寫，上游自己夾
    pub fn of_encryption_content(content: Option<&serde_json::Value>) -> RoomKeyRotation {
        let defaults = EncryptionSettings::default();
        let positive = |field: &str| {
            content
                .and_then(|content| content.get(field))
                .and_then(|value| value.as_u64())
                .filter(|value| *value > 0)
        };
        RoomKeyRotation {
            max_age: positive("rotation_period_ms")
                .map(Duration::from_millis)
                .unwrap_or(defaults.rotation_period),
            max_messages: positive("rotation_period_msgs").unwrap_or(defaults.rotation_period_msgs),
        }
    }
}

/// 剩幾則以內算「該提早換了」。
///
/// Return:
///     u64  `PRE_ROTATE_MESSAGES` 與期限的四分之一取小的；期限 1 則時是 0（送完那一則就換）
fn pre_rotate_messages(max_messages: u64) -> u64 {
    PRE_ROTATE_MESSAGES.min(max_messages / 4)
}

/// 剩多久以內算「該提早換了」。
///
/// Return:
///     Duration  `PRE_ROTATE_AGE` 與期限的四分之一取小的
fn pre_rotate_age(max_age: Duration) -> Duration {
    PRE_ROTATE_AGE.min(max_age / 4)
}

/// `refresh_room_devices` 一輪的結果：這一刻的房間快照（下次送帶它的 `room_version`）、跟上一份比出來的差。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RoomRefresh {
    /// 哪個房。
    pub room_id: String,
    /// 這一刻的房間版本號與每個 `join` 成員的裝置版本號——**呼叫者要存下來**（維護者 2026-09-29：存在 UI），
    /// 下次 refresh 當 `previous`、送出時帶它的 `room_version` 與成員。
    pub versions: RoomDeviceVersions,
    /// 誰要重查（新加入、裝置版本號變了）、誰離開了；`previous` 是 None 時全部算 changed。
    pub diff: MembersDiff,
    /// 雜湊第一次對不上、重查一次才對上的人（通常是空的；非空代表查詢與清單之間有人換了金鑰）。
    pub rechecked: Vec<String>,
}

/// 一則要加密送出的房間事件（`encrypt_and_send` 的輸入）；房間與房間版本號從 `RoomRefresh` 來，不在這裡。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutgoingRoomEvent {
    /// 明文的事件型別, example: "m.room.message"
    pub event_type: String,
    /// 明文 content, example: json!({"msgtype":"m.text","body":"hi"})
    pub content: serde_json::Value,
    /// 重送用同一個, example: "txn-1"
    pub txn_id: String,
    /// 這則用到的 mxc（server 讀不到密文，靠它替媒體 +1）, example: vec![]
    pub attachments: Vec<String>,
}

/// `encrypt_and_send` 的結果：送進去了、金鑰還沒就緒沒送、或被 1506 擋下來。
/// 後兩種不是 `Err`：那是這條路上**預期內**的結果（維護者定：daemon 準備金鑰、UI 決定重送，/docs/design/keys/e2ee-walkthrough.md §16.6）。
#[derive(Debug)]
pub enum SendOutcome {
    Sent {
        event_id: String,
    },
    /// 這個房的房間金鑰還沒就緒（沒有、到期或作廢了、或還有 to-device 沒拿到 ACK），訊息沒加密、沒送。
    /// 呼叫端要先讓後台 `distribute_room_key` 分完，再送。
    RoomKeyNotReady {
        /// 給人看的, example: "the room key expired"
        reason: String,
    },
    /// server 說帶的 `room_version` 過期，訊息沒送。拿 `current_room_version` 只能知道自己過期，🚫 不能直接拿它重送：
    /// 先 `refresh_room_devices`（拿新的快照）、讓後台對它分好金鑰，再帶那份快照的 `room_version` 重送（同一個 `txn_id`）。
    RoomDevicesChanged {
        current_room_version: Option<u64>,
        error: SdkError,
    },
}

/// 一個房的房間金鑰能不能拿來加密（`room_key_state`）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RoomKeyState {
    /// 可以：排過的 to-device 全拿到 ACK、沒到期、沒作廢。
    Ready {
        /// example: "3OfEg1QBw1tyjrBe9SP+VTY/nAmXETEApGIUsrohAiI"
        session_id: String,
        /// 這把加密過幾則, example: 1
        message_count: u64,
        /// 快到期了（剩 `PRE_ROTATE_MESSAGES` 則以內或 `PRE_ROTATE_AGE` 以內）：送完之後該提早換一把
        due_for_rotation: bool,
    },
    /// 不行，例：還沒建過、到期或作廢了、還有 to-device 沒拿到 ACK。
    NotReady {
        /// 給人看的, example: "the room key expired"
        reason: String,
    },
}

pub struct OlmEngine {
    machine: OlmMachine,
    /// crypto store 與 `td.json` 所在的 `m/`。
    store_dir: PathBuf,
    /// 最近一次 `/keys/query` 回答這個人的 body（整份）：拿來重算裝置雜湊跟成員清單上的比（wbfuwunel 的 /docs/design/wbf-room-device-version.md §3.4）。
    /// 只在記憶體：重開就重查。
    last_keys_query: Mutex<BTreeMap<String, serde_json::Value>>,
}

impl OlmEngine {
    /// 開（或建）這台裝置的 crypto store 並載入狀態機。
    ///
    /// Args:
    ///     store_dir: 帳號目錄的 `m/`（跟 `backend/matrix_sdk` 同一個目錄、同一把 key）, example: "<account dir>/m"
    ///     store_key: `Vault::matrix_store_key()`
    ///     user_id: example: "@alice:localhost"
    ///     device_id: example: "RJYKSTBOIE"
    /// Return:
    ///     Ok(OlmEngine)
    ///     Err(Usage)      user_id／device_id 不合法
    ///     Err(Protocol)   store 開不了或壞了（含 key 不對）
    pub async fn open(
        store_dir: &Path,
        store_key: &Key32,
        user_id: &str,
        device_id: &str,
    ) -> Result<OlmEngine, SdkError> {
        let user_id = UserId::parse(user_id)
            .map_err(|error| SdkError::Usage(format!("bad user id {user_id:?}: {error}")))?;
        let device_id: &DeviceId = device_id.into();
        std::fs::create_dir_all(store_dir)?;
        let store = SqliteCryptoStore::open_with_config(
            &SqliteStoreConfig::new(store_dir).key(Some(store_key.as_bytes())),
        )
        .await
        .map_err(|error| {
            SdkError::Protocol(format!(
                "cannot open the crypto store at {}: {error}",
                store_dir.display()
            ))
        })?;
        let machine = OlmMachineBuilder::new(&user_id, device_id)
            .with_crypto_store(store)
            .build()
            .await
            .map_err(crypto_store_error)?;
        Ok(OlmEngine {
            machine,
            store_dir: store_dir.to_path_buf(),
            last_keys_query: Mutex::new(BTreeMap::new()),
        })
    }

    /// UI 叫 `room.refresh_devices`（點進房間、收到 `devices.changed` 之後由 UI 決定）與被 1506 擋之後都叫這一支（/docs/design/keys/e2ee-rpc.md §2、§3）：
    /// 拿這一刻的成員清單與版本號 → 跟上一份比出誰變了 → 只重查那些人 → 雜湊對一次（不對再查一次，還不對就拒絕）。
    /// 🚫 碰房間金鑰：建、換、送都是後台的事（呼叫端拿回來的 `versions` 交給 `distribute_room_key`）。
    ///
    /// Args:
    ///     client: 要能走橋的連線
    ///     room_id: example: "!r:localhost"
    ///     previous: 上一次的 `RoomRefresh::versions`；None ＝ 第一次進這個房，每個人都查
    /// Return:
    ///     Ok(RoomRefresh)  下次送帶 `versions.room_version`
    ///     Err(Server)      不在房裡（`Forbidden`）等
    ///     Err(Protocol)    server 沒給號碼；或某人的裝置雜湊重查一次後仍對不上（🚨 fail closed：看到的不是同一組金鑰就不發）
    pub async fn refresh_room_devices<C: PackChannel>(
        &self,
        client: &mut WbfClient<C>,
        room_id: &str,
        previous: Option<&RoomDeviceVersions>,
    ) -> Result<RoomRefresh, SdkError> {
        let versions = client.room_device_versions(room_id).await?;
        let members: Vec<String> = versions.members.keys().cloned().collect();
        let diff = match previous {
            Some(previous) => versions.diff_from(previous),
            None => MembersDiff {
                changed: members.clone(),
                left: Vec::new(),
            },
        };
        self.track_users(&members).await?;
        if !diff.changed.is_empty() {
            self.mark_users_changed(&diff.changed).await?;
        }
        self.send_outgoing_requests(client).await?;
        let rechecked = self.mismatched_device_hashes(&versions);
        if !rechecked.is_empty() {
            // 查詢與清單之間可能有人換了金鑰：再查一次。還對不上就不是競態，是我們或 server 壞了，🚫 不帶著存疑的名單發金鑰。
            self.mark_users_changed(&rechecked).await?;
            self.send_outgoing_requests(client).await?;
            let still = self.mismatched_device_hashes(&versions);
            if !still.is_empty() {
                return Err(SdkError::Protocol(format!(
                    "device keys hash still does not match the member list for {still:?} after re-querying: refusing to share the room key"
                )));
            }
        }
        Ok(RoomRefresh {
            room_id: room_id.to_string(),
            versions,
            diff,
            rechecked,
        })
    }

    /// 這個房的房間金鑰能不能拿來加密：只讀本機的 crypto store，🚫 建、🚫 換、🚫 上網。
    ///
    /// Args:
    ///     room_id: example: "!r:localhost"
    /// Return:
    ///     Ok(RoomKeyState::Ready)      排過的 to-device 全拿到 ACK、沒到期、沒作廢；`due_for_rotation` 說要不要提早換
    ///     Ok(RoomKeyState::NotReady)   還沒建過、到期或作廢了、或還有 to-device 沒拿到 ACK
    ///     Err(Usage)                   房間 id 不合法
    ///     Err(Protocol)                crypto store 讀不了
    pub async fn room_key_state(&self, room_id: &str) -> Result<RoomKeyState, SdkError> {
        let room_id: OwnedRoomId = RoomId::parse(room_id)
            .map_err(|error| SdkError::Usage(format!("bad room id {room_id:?}: {error}")))?;
        let Some(session) = self
            .machine
            .store()
            .get_outbound_group_session(&room_id)
            .await
            .map_err(crypto_store_error)?
        else {
            return Ok(RoomKeyState::NotReady {
                reason: "this room has no room key yet".into(),
            });
        };
        if session.invalidated() {
            return Ok(RoomKeyState::NotReady {
                reason: "the room key was discarded".into(),
            });
        }
        if session.expired() {
            return Ok(RoomKeyState::NotReady {
                reason: "the room key expired".into(),
            });
        }
        let pickle = session.pickle().await;
        if !pickle.requests.is_empty() {
            return Ok(RoomKeyState::NotReady {
                reason: format!(
                    "{} to-device message(s) of the room key are not acknowledged by the server yet",
                    pickle.requests.len()
                ),
            });
        }
        // 跟上游判「到期」用同樣的夾法，門檻再跟著期限縮。
        let max_messages = pickle
            .settings
            .rotation_period_msgs
            .clamp(1, UPSTREAM_MAX_ROTATION_MESSAGES);
        let messages_left = max_messages.saturating_sub(pickle.message_count);
        let max_age = pickle
            .settings
            .rotation_period
            .max(UPSTREAM_MIN_ROTATION_AGE);
        let created = Duration::from_secs(u64::from(pickle.creation_time.get()));
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        let time_left = (created + max_age).saturating_sub(now);
        Ok(RoomKeyState::Ready {
            session_id: session.session_id().to_string(),
            message_count: pickle.message_count,
            due_for_rotation: messages_left <= pre_rotate_messages(max_messages)
                || time_left <= pre_rotate_age(max_age),
        })
    }

    /// 丟掉這個房現在的房間金鑰（提早換用）：下一次 `distribute_room_key` 會建一把新的、分給每台裝置。只碰本機。
    ///
    /// Args:
    ///     room_id: example: "!r:localhost"
    /// Return:
    ///     Ok(bool)        true ＝ 本來有一把、丟了；false ＝ 本來就沒有
    ///     Err(Usage)      房間 id 不合法
    ///     Err(Protocol)   crypto store 寫不了
    pub async fn discard_room_key(&self, room_id: &str) -> Result<bool, SdkError> {
        let room_id: OwnedRoomId = RoomId::parse(room_id)
            .map_err(|error| SdkError::Usage(format!("bad room id {room_id:?}: {error}")))?;
        self.machine
            .discard_room_key(&room_id)
            .await
            .map_err(crypto_store_error)
    }

    /// **只用已經分好的房間金鑰**加密、帶房間版本號送出（`Event/Send`，維護者 2026-10-06，/docs/design/keys/e2ee-rpc.md §3）。
    ///
    /// 🚨 這條路🚫 產生金鑰、🚫 換金鑰、🚫 任何金鑰的網路動作：先看 [`OlmEngine::room_key_state`]，不是 `Ready` 就不加密、回 `RoomKeyNotReady`，
    /// 由呼叫端讓後台（`distribute_room_key`）分完再送。所以每一則用的金鑰，排過的 to-device 都已經拿到 server 的 ACK。
    /// 上游的加密在「沒有 outbound session」與「session 過期」時是 **panic** 不是回錯：就緒檢查已經排除這兩條，
    /// 檢查到加密之間剛好跨過期限（或被後台換掉）的那一瞬間，用 `catch_unwind` 接住、回 `RoomKeyNotReady`（全域 P 條：失敗要有去處）。
    /// 號碼由呼叫端帶（UI 存著）：server 用它擋「送出方不知道最新裝置組合」。被 1506 擋是 `Ok(RoomDevicesChanged)` 不是 `Err`。
    ///
    /// Args:
    ///     room_id: example: "!r:localhost"
    ///     room_version: 呼叫端手上那個房的房間版本號（`refresh_room_devices` 回的）, example: 81234
    ///     message: example: &OutgoingRoomEvent { event_type: "m.room.message".into(), content: json!({"msgtype":"m.text","body":"hi"}), txn_id: "txn-1".into(), attachments: vec![] }
    /// Return:
    ///     Ok(SendOutcome::Sent)                 送進去了
    ///     Ok(SendOutcome::RoomKeyNotReady)      金鑰還沒就緒（含加密當下剛好到期），訊息沒送
    ///     Ok(SendOutcome::RoomDevicesChanged)   1506：號碼過期，訊息沒送
    ///     Err(Usage)                            房間 id 不合法
    ///     Err(Protocol)                         加密失敗（上游回的錯）
    ///     Err(Server)                           其他拒絕（含宣告過 feature 卻漏帶號碼的 `InvalidRequest`）
    pub async fn encrypt_and_send<C: PackChannel>(
        &self,
        client: &mut WbfClient<C>,
        room_id: &str,
        room_version: u64,
        message: &OutgoingRoomEvent,
    ) -> Result<SendOutcome, SdkError> {
        let owned_room_id: OwnedRoomId = RoomId::parse(room_id)
            .map_err(|error| SdkError::Usage(format!("bad room id {room_id:?}: {error}")))?;
        // 1. 金鑰一定是分好的那把；🚫 在這裡建或換。
        if let RoomKeyState::NotReady { reason } = self.room_key_state(room_id).await? {
            return Ok(SendOutcome::RoomKeyNotReady { reason });
        }
        // 2. 加密。
        let raw_content: Raw<AnyMessageLikeEventContent> = Raw::from_json(
            serde_json::value::to_raw_value(&message.content)
                .map_err(|error| SdkError::Protocol(format!("event content: {error}")))?,
        );
        //    上游 panic ＝檢查完到加密之間金鑰剛好到期或被後台換掉：跟「沒就緒」同一個結果，呼叫端交後台、回 1402。
        let Ok(encrypted) = AssertUnwindSafe(self.machine.encrypt_room_event_raw(
            &owned_room_id,
            &message.event_type,
            &raw_content,
        ))
        .catch_unwind()
        .await
        else {
            return Ok(SendOutcome::RoomKeyNotReady {
                reason: "the room key expired between the readiness check and encrypting".into(),
            });
        };
        let encrypted =
            encrypted.map_err(|error| SdkError::Protocol(format!("encrypt: {error}")))?;
        let request = SendRequest {
            room_id: room_id.to_string(),
            event_type: "m.room.encrypted".to_string(),
            txn_id: message.txn_id.clone(),
            attachments: message.attachments.clone(),
            room_version: Some(room_version),
        };
        let body = encrypted.content.json().get().as_bytes().to_vec();
        match client.send_event(&request, body).await {
            Ok(ack) => Ok(SendOutcome::Sent {
                event_id: ack.event_id,
            }),
            Err(error) if error.wbf_code() == Some(WbfErrorCode::RoomDevicesChanged) => {
                Ok(SendOutcome::RoomDevicesChanged {
                    current_room_version: error.current_room_version(),
                    error,
                })
            }
            Err(error) => Err(error),
        }
    }

    /// 一則 WS 收到的事件（`Recent`／`Push` 的原樣 JSON）→ 要寫進 cache 的樣子（維護者 2026-09-29：收到時有金鑰就解，密文明文一起存；沒金鑰就只存密文）。
    ///
    /// Args:
    ///     room_id: example: "!r:localhost"
    ///     event: example: {"type":"m.room.encrypted","event_id":"$e","sender":"@b:x","origin_server_ts":1,"content":{"algorithm":"m.megolm.v1.aes-sha2","session_id":"S","ciphertext":"…"}}
    /// Return:
    ///     IncomingEvent  不是 `m.room.encrypted` → Plain；解得開 → Decrypted（密文照帶）；解不開 → Undecrypted（原因是上游的那句）
    pub async fn to_incoming(&self, room_id: &str, event: serde_json::Value) -> IncomingEvent {
        if event.get("type").and_then(|value| value.as_str()) != Some("m.room.encrypted") {
            return IncomingEvent::Plain { event };
        }
        match self.decrypt_room_event(room_id, &event).await {
            Ok(cleartext) => IncomingEvent::Decrypted {
                ciphertext: Some(event),
                cleartext,
            },
            Err(error) => IncomingEvent::Undecrypted {
                ciphertext: event,
                reason: error.to_string(),
            },
        }
    }

    /// 解一則 WS 拉到的 `m.room.encrypted` 事件（`Recent`／`Push` 給的原樣 JSON）。
    ///
    /// Args:
    ///     room_id: example: "!r:localhost"
    ///     event: 完整的密文事件（有 `event_id`、`sender`、`content.ciphertext`…）
    /// Return:
    ///     Ok(Value)       解開後的完整事件（`type`、`content` 是明文，`event_id`／`sender` 照帶）
    ///     Err(Protocol)   解不開（沒有那把房間金鑰、密文壞了…）；訊息帶上游的原因
    pub async fn decrypt_room_event(
        &self,
        room_id: &str,
        event: &serde_json::Value,
    ) -> Result<serde_json::Value, SdkError> {
        let owned_room_id: OwnedRoomId = RoomId::parse(room_id)
            .map_err(|error| SdkError::Usage(format!("bad room id {room_id:?}: {error}")))?;
        let raw: Raw<EncryptedEvent> = Raw::from_json(
            serde_json::value::to_raw_value(event)
                .map_err(|error| SdkError::Protocol(format!("encrypted event: {error}")))?,
        );
        let decrypted = self
            .machine
            .decrypt_room_event(&raw, &owned_room_id, &decryption_settings())
            .await
            .map_err(|error| SdkError::Protocol(format!("decrypt: {error}")))?;
        serde_json::from_str(decrypted.event.json().get())
            .map_err(|error| SdkError::Protocol(format!("decrypted event is not JSON: {error}")))
    }

    /// 成員清單上的裝置雜湊，跟我們最近一次 `/keys/query` 答案照 wbfuwunel 的 /docs/design/wbf-room-device-version.md §3.4 重算的比。
    ///
    /// Return:
    ///     Vec<String>  對不上的人（排序）。沒查過的人、server 說 `unhashable` 的人不算；我們自己算不出來的（到不了）**算對不上**——寬可多查一次，不拿舊金鑰送
    pub fn mismatched_device_hashes(&self, versions: &RoomDeviceVersions) -> Vec<String> {
        let answers = self
            .last_keys_query
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        versions
            .members
            .iter()
            .filter(|(user_id, version)| {
                version.is_hashable()
                    && answers.get(*user_id).is_some_and(|body| {
                        compute_device_keys_hash(user_id, body)
                            .is_none_or(|hash| hash != version.hash)
                    })
            })
            .map(|(user_id, _)| user_id.clone())
            .collect()
    }

    pub fn machine(&self) -> &OlmMachine {
        &self.machine
    }

    /// Return:
    ///     Result<ToDeviceState>  `m/td.json` 現在的水位與待銷毀清單（沒檔就是從頭）
    pub fn to_device_state(&self) -> Result<ToDeviceState, SdkError> {
        ToDeviceState::load(&self.store_dir)
    }

    /// 一批 to-device 的完整處理（`Fetch` 的一窗、或推來的一包 `Push`：同一支，維護者 2026-09-24），**順序鎖死**（/docs/design/keys/to-device-client.md §4、§7）：
    /// 匯進 crypto store（sqlite commit 了才回）→ 水位與待銷毀清單落地（`m/td.json`，原子寫）→ 才叫 server 銷毀
    /// （連上次沒銷成的一起）→ 只清 `ItemsDestroyed` 回來的。
    /// 🚨 呼叫者拿不到「先銷毀再匯入」的路，這就是這個函式存在的理由。中途任何一步失敗，已落地的照樣有效：
    /// 下次從佇列頭再拉是**重複**不是遺失（匯入與銷毀都冪等）。
    ///
    /// Args:
    ///     client: 要先 `device_subscribe` 過的連線（沒訂閱，銷毀那一步被 `Forbidden`，但匯入與落地已經完成）
    ///     items: `(count, 事件)` 舊→新：`DeviceWindow::items` 或 `SubscribeReply::Push` 的 `items`（可以是空的：那就只補送上次沒銷成的）
    ///     per_pack_timeout: example: Duration::from_secs(30)
    /// Return:
    ///     Ok(ImportReport)
    ///     Err(Protocol)   crypto store 寫入失敗（整批；單則壞的狀態機會跳過、不會讓這裡回錯）——不銷毀，整批還在佇列裡
    ///     Err(Server)     銷毀被拒（例：沒訂閱的 `Forbidden`）——匯入與落地已完成，清單留著下次再送
    pub async fn import_items<C: PackChannel>(
        &self,
        client: &mut WbfClient<C>,
        items: Vec<(u64, serde_json::Value)>,
        per_pack_timeout: std::time::Duration,
    ) -> Result<ImportReport, SdkError> {
        let mut state = ToDeviceState::load(&self.store_dir)?;
        let counts: Vec<u64> = items.iter().map(|(count, _)| *count).collect();
        let events: Vec<serde_json::Value> = items.into_iter().map(|(_, event)| event).collect();
        // 1. 匯入：Ok 就是 crypto store 的交易 commit 了（上游 receive_sync_changes 的最後兩行）。
        let room_keys = self.receive_to_device(events, None, None).await?;
        // 2. 落地：水位前進、這些 count 進待銷毀清單。
        for count in &counts {
            state.mark_processed(*count);
        }
        state.save(&self.store_dir)?;
        // 3. 銷毀：送整份清單（含上次沒回來的）；只清回來的。
        let destroyed = client
            .device_items_destroy(&state.to_destroy.clone(), per_pack_timeout)
            .await?;
        state.mark_destroyed(&destroyed);
        state.save(&self.store_dir)?;
        Ok(ImportReport {
            room_keys,
            imported: counts.len(),
            destroyed,
            cd_seq: state.cd_seq,
            still_to_destroy: state.to_destroy.len(),
        })
    }

    /// 從佇列最舊還沒銷毀的起一窗一窗拉到追平（/docs/design/keys/to-device-client.md §7）：上線時「主動拉一次」就是它，推播說 `gap`、匯失敗也是它。
    /// 每一窗都走 `import_items`。🚫 不帶 `cd_seq`：佇列頭就是水位，`ItemsDestroy` 是唯一的「處理完了」（wbfuwunel #87）。
    /// 空窗也走一次（把上次沒銷成的補送）。
    ///
    /// Args:
    ///     client: 要先 `device_subscribe` 過的連線
    ///     per_pack_timeout: example: Duration::from_secs(30)
    /// Return:
    ///     Ok(Vec<ImportReport>)  每窗一筆；追平（最後一窗 `more: false`）才回
    ///     Err(Protocol)          拉了 PULL_WINDOWS_LIMIT 窗還沒追平
    pub async fn pull_to_device<C: PackChannel>(
        &self,
        client: &mut WbfClient<C>,
        per_pack_timeout: std::time::Duration,
    ) -> Result<Vec<ImportReport>, SdkError> {
        let mut reports = Vec::new();
        for _ in 0..PULL_WINDOWS_LIMIT {
            // 🚫 不帶 `cd_seq`（維護者 2026-09-26，wbfuwunel #87）：讓 server 從這台裝置佇列裡**最舊還沒銷毀的**起給。
            // 佇列本身就是沒有洞的（銷毀前不刪），洞只會是 client 自己的游標造出來的：游標跑到一則還沒進 store 的 item 前面，那則就再也問不到。
            // 這一窗匯完、銷掉的不會再回來，所以下一次不帶游標的 `Fetch` 自然是下一窗；銷不掉的會再回來一次（重複匯入無害）。
            let window = client
                .device_fetch_window(&DeviceFetchRequest { limit: None }, per_pack_timeout)
                .await?;
            let more = more_is_only_meaningful_with_items(&window);
            reports.push(
                self.import_items(client, window.items, per_pack_timeout)
                    .await?,
            );
            if !more {
                return Ok(reports);
            }
        }
        Err(SdkError::Protocol(format!(
            "the to-device queue did not catch up after {PULL_WINDOWS_LIMIT} windows"
        )))
    }

    /// 把一批 to-device 事件（`Device/Fetch` 拉到的，舊→新）與自己的 OTK 存量推進狀態機。
    /// 房間金鑰、金鑰請求與轉發、驗證的 to-device 都在這裡被吃掉；不認得的型別上游自己略過。
    ///
    /// Args:
    ///     events: 每則 `{type, sender, content}`, example: vec![json!({"type":"m.room.encrypted","sender":"@a:x","content":{…}})]
    ///     otk_counts: 自己每種演算法剩幾把 OTK（`CryptoState` 或 `/keys/upload` 回的）；None ＝ 這一批沒帶
    ///     unused_fallback_key_types: ⚠️ `Some(&[])` 是「都用掉了，該換」、None 是「沒給」，兩者不同
    /// Return:
    ///     Ok(Vec<RoomKeyInfo>)  這一批帶進來的**新房間金鑰**（呼叫者拿它決定重解哪些密文）
    ///     Err(Protocol)         crypto store 的交易寫不進去（整批，跟哪一則無關）。
    ///                           📎 單則壞掉的**不會**讓整批回錯：上游狀態機對解不出形狀的 to-device 是記成 `Invalid` 跳過
    ///                           （`receive_to_device_event` 的 "Skip invalid events"），所以沒有「一則壞 item 卡住佇列」這回事
    pub async fn receive_to_device(
        &self,
        events: Vec<serde_json::Value>,
        otk_counts: Option<&BTreeMap<String, u64>>,
        unused_fallback_key_types: Option<&[String]>,
    ) -> Result<Vec<RoomKeyInfo>, SdkError> {
        let mut to_device_events = Vec::with_capacity(events.len());
        for event in events {
            let raw: Raw<AnyToDeviceEvent> = Raw::from_json(
                serde_json::value::to_raw_value(&event)
                    .map_err(|error| SdkError::Protocol(format!("to-device event: {error}")))?,
            );
            to_device_events.push(raw);
        }
        let counts: BTreeMap<OneTimeKeyAlgorithm, UInt> = otk_counts
            .into_iter()
            .flat_map(|counts| counts.iter())
            .map(|(algorithm, count)| {
                (
                    OneTimeKeyAlgorithm::from(algorithm.as_str()),
                    UInt::new(*count).unwrap_or(UInt::MAX),
                )
            })
            .collect();
        let fallback: Option<Vec<OneTimeKeyAlgorithm>> = unused_fallback_key_types.map(|types| {
            types
                .iter()
                .map(|algorithm| OneTimeKeyAlgorithm::from(algorithm.as_str()))
                .collect()
        });
        let changes = EncryptionSyncChanges {
            to_device_events,
            changed_devices: &DeviceLists::default(),
            one_time_keys_counts: &counts,
            unused_fallback_keys: fallback.as_deref(),
            next_batch_token: None,
        };
        let (_processed, room_keys) = self
            .machine
            .receive_sync_changes_msc4186(changes, &decryption_settings())
            .await
            .map_err(olm_error)?;
        Ok(room_keys)
    }

    /// 把狀態機要送的每一個請求走橋送出去、回應交回去，直到它沒東西要送。
    /// 上傳自己的金鑰、補 OTK、查別人的裝置、claim OTK、發 to-device、上傳簽章都在這裡。
    ///
    /// Return:
    ///     Ok(usize)      送了幾個請求
    ///     Err(Usage)     狀態機要送房間內的驗證訊息（`RoomMessage`）——WS 這條路還沒接，🚫 不靜默丟掉
    ///     Err(Server)    橋或 Matrix 端點拒絕（原樣往上，這一個請求下次 `outgoing_requests` 還會再出現）
    ///     Err(Protocol)  回應解不成 ruma 的型別、或繞了 OUTGOING_ROUNDS_LIMIT 圈還有東西
    pub async fn send_outgoing_requests<C: PackChannel>(
        &self,
        client: &mut WbfClient<C>,
    ) -> Result<usize, SdkError> {
        let mut sent = 0usize;
        for _ in 0..OUTGOING_ROUNDS_LIMIT {
            let requests = self
                .machine
                .outgoing_requests()
                .await
                .map_err(crypto_store_error)?;
            if requests.is_empty() {
                return Ok(sent);
            }
            for request in requests {
                self.send_one(client, &request).await?;
                sent += 1;
            }
        }
        Err(SdkError::Protocol(format!(
            "the crypto state machine still has requests to send after {OUTGOING_ROUNDS_LIMIT} rounds"
        )))
    }

    /// 開始追蹤這些人（之後 `outgoing_requests` 會有他們的 `/keys/query`）。加入、被邀請進來時叫。
    ///
    /// Args:
    ///     users: example: &["@bob:localhost".to_string()]
    pub async fn track_users(&self, users: &[String]) -> Result<(), SdkError> {
        let users = parse_user_ids(users)?;
        self.machine
            .update_tracked_users(users.iter().map(OwnedUserId::as_ref))
            .await
            .map_err(crypto_store_error)
    }

    /// 這些人的裝置變了（成員清單的裝置版本號跟上次不同、或 `DeviceChanged` 推來的）：下一次 `outgoing_requests`
    /// 會重新 `/keys/query` 他們。已經追蹤中的人只靠 `track_users` **不會**再查——那是 1506 之後一定要走這裡的原因。
    ///
    /// Args:
    ///     users: example: &["@bob:localhost".to_string()]
    pub async fn mark_users_changed(&self, users: &[String]) -> Result<(), SdkError> {
        // 跟 `/sync` 的 `device_lists.changed` 同一個入口：狀態機把他們標成要重查。
        let mut device_lists = DeviceLists::new();
        device_lists.changed = parse_user_ids(users)?;
        let changes = EncryptionSyncChanges {
            to_device_events: Vec::new(),
            changed_devices: &device_lists,
            one_time_keys_counts: &BTreeMap::new(),
            unused_fallback_keys: None,
            next_batch_token: None,
        };
        self.machine
            .receive_sync_changes_msc4186(changes, &decryption_settings())
            .await
            .map_err(olm_error)?;
        Ok(())
    }

    /// 這個人在 crypto store 裡已知的裝置（查過 `/keys/query` 之後才會有）。
    ///
    /// Args:
    ///     user_id: example: "@bob:localhost"
    /// Return:
    ///     Ok(Vec<String>)  裝置 id，排序；沒查過或沒有裝置就是空的
    pub async fn known_devices_of(&self, user_id: &str) -> Result<Vec<String>, SdkError> {
        let user_id = UserId::parse(user_id)
            .map_err(|error| SdkError::Usage(format!("bad user id {user_id:?}: {error}")))?;
        let devices = self
            .machine
            .get_user_devices(&user_id, None)
            .await
            .map_err(crypto_store_error)?;
        let mut ids: Vec<String> = devices.keys().map(|id| id.to_string()).collect();
        ids.sort();
        Ok(ids)
    }

    /// 備好 `room_id` 的房間金鑰並送到這些人的每台裝置——**房間金鑰只在這裡建、換、送**（後台叫，/docs/design/keys/e2ee-rpc.md §3.1），全部走橋：
    /// 追蹤中的該查的 `/keys/query` → 缺 Olm 通道的 `/keys/claim` → 上游 `share_room_key`（沒有、到期、作廢、有人離開就建新的）→
    /// 這把 session 上所有還沒送出的 to-device 一個一個送、交回上游。上游決定該不該輪換、發給哪些裝置（`room_key_share_settings`）。
    /// 通道一定在排之前建好：上游的房間金鑰是在排的那一刻、照 session 當下的位置匯出的，沒通道的裝置只能排成 `m.no_olm`，
    /// 之後再補只拿得到之後的位置（真 server 實測：「unknown message index, first known index 1」）。加密只用這裡分完的金鑰，所以加密之前通道都在。
    ///
    /// Args:
    ///     client: 走橋用的連線
    ///     room_id: example: "!r:localhost"
    ///     users: 房間裡要拿金鑰的人（含自己）, example: &["@alice:localhost".to_string()]
    ///     rotation: 這個房自己設的期限；現在那把是照別的期限建的（房主改過）就先丟掉、照這個建新的（上游只在到期／作廢時換）
    /// Return:
    ///     Ok(usize)        送了幾個 to-device（0 ＝ 每台已知的裝置都已經有這把）
    ///     Err(Usage)       房間 id、成員的 mxid 不合法
    ///     Err(Server)／Err(Network)／Err(Protocol)  任何一個請求失敗：還沒送的留在 session 上，下次再叫就接著送
    pub async fn distribute_room_key<C: PackChannel>(
        &self,
        client: &mut WbfClient<C>,
        room_id: &str,
        users: &[String],
        rotation: RoomKeyRotation,
    ) -> Result<usize, SdkError> {
        let room_id: OwnedRoomId = RoomId::parse(room_id)
            .map_err(|error| SdkError::Usage(format!("bad room id {room_id:?}: {error}")))?;
        if let Some(session) = self
            .machine
            .store()
            .get_outbound_group_session(&room_id)
            .await
            .map_err(crypto_store_error)?
        {
            let settings = &session.pickle().await.settings;
            if (settings.rotation_period, settings.rotation_period_msgs)
                != (rotation.max_age, rotation.max_messages)
            {
                self.machine
                    .discard_room_key(&room_id)
                    .await
                    .map_err(crypto_store_error)?;
            }
        }
        self.track_users(users).await?;
        let users = parse_user_ids(users)?;
        // 先把追蹤中的查詢送完，名單才是最新的。
        self.send_outgoing_requests(client).await?;
        self.claim_missing_sessions(client, &users).await?;
        let requests = self
            .machine
            .share_room_key(
                &room_id,
                users.iter().map(OwnedUserId::as_ref),
                room_key_share_settings(rotation),
            )
            .await
            .map_err(olm_error)?;
        let count = requests.len();
        for request in requests {
            self.send_to_device_request(client, &request).await?;
        }
        Ok(count)
    }

    /// 這些人已知、還沒有 Olm 通道的裝置：`/keys/claim` 一次性金鑰、建好通道（走橋）。都有了就什麼都不送。
    ///
    /// Return:
    ///     Ok(())
    ///     Err(Server)／Err(Network)／Err(Protocol)   claim 失敗（下次再叫會再 claim 還缺的）
    async fn claim_missing_sessions<C: PackChannel>(
        &self,
        client: &mut WbfClient<C>,
        users: &[OwnedUserId],
    ) -> Result<(), SdkError> {
        let Some((request_id, claim)) = self
            .machine
            .get_missing_sessions(users.iter().map(OwnedUserId::as_ref))
            .await
            .map_err(crypto_store_error)?
        else {
            return Ok(());
        };
        let reply = self
            .call_bridge_with(client, protocol::BRIDGE_KEYS_CLAIM, claim)
            .await?;
        let response = parse_response::<claim_keys::v3::Response>(&reply, "KeysClaim")?;
        self.machine
            .mark_request_as_sent(&request_id, &response)
            .await
            .map_err(olm_error)
    }

    async fn send_one<C: PackChannel>(
        &self,
        client: &mut WbfClient<C>,
        request: &OutgoingRequest,
    ) -> Result<(), SdkError> {
        let request_id = request.request_id();
        match request.request() {
            AnyOutgoingRequest::KeysUpload(upload) => {
                let reply = self
                    .call_bridge_with(client, protocol::BRIDGE_KEYS_UPLOAD, upload.clone())
                    .await?;
                let response = parse_response::<upload_keys::v3::Response>(&reply, "KeysUpload")?;
                self.machine
                    .mark_request_as_sent(request_id, &response)
                    .await
                    .map_err(olm_error)
            }
            AnyOutgoingRequest::KeysQuery(query) => {
                let mut ruma_request = get_keys::v3::Request::new();
                ruma_request.device_keys = query.device_keys.clone();
                ruma_request.timeout = query.timeout;
                let reply = self
                    .call_bridge_with(client, protocol::BRIDGE_KEYS_QUERY, ruma_request)
                    .await?;
                // 留一份原樣的答案給雜湊比對（狀態機吃進去就拿不回來了）。
                let body = reply.json("KeysQuery")?;
                {
                    let mut answers = self
                        .last_keys_query
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    for user_id in query.device_keys.keys() {
                        answers.insert(user_id.to_string(), body.clone());
                    }
                }
                let response = parse_response::<get_keys::v3::Response>(&reply, "KeysQuery")?;
                self.machine
                    .mark_request_as_sent(request_id, &response)
                    .await
                    .map_err(olm_error)
            }
            AnyOutgoingRequest::KeysClaim(claim) => {
                let reply = self
                    .call_bridge_with(client, protocol::BRIDGE_KEYS_CLAIM, claim.clone())
                    .await?;
                let response = parse_response::<claim_keys::v3::Response>(&reply, "KeysClaim")?;
                self.machine
                    .mark_request_as_sent(request_id, &response)
                    .await
                    .map_err(olm_error)
            }
            AnyOutgoingRequest::ToDeviceRequest(to_device) => {
                self.send_to_device_request(client, to_device).await
            }
            AnyOutgoingRequest::SignatureUpload(upload) => {
                let reply = self
                    .call_bridge_with(client, protocol::BRIDGE_SIGNATURES_UPLOAD, upload.clone())
                    .await?;
                let response =
                    parse_response::<upload_signatures::v3::Response>(&reply, "SignaturesUpload")?;
                self.machine
                    .mark_request_as_sent(request_id, &response)
                    .await
                    .map_err(olm_error)
            }
            AnyOutgoingRequest::RoomMessage(_) => Err(SdkError::Usage(
                "the crypto state machine wants to send an in-room verification event; \
                 that path is not on the wbf channel yet"
                    .into(),
            )),
        }
    }

    /// 一個 to-device 請求走橋（`0x16 0x25`），成功後交回狀態機（它才會把「這把金鑰發給過誰」記下來）。
    async fn send_to_device_request<C: PackChannel>(
        &self,
        client: &mut WbfClient<C>,
        request: &ToDeviceRequest,
    ) -> Result<(), SdkError> {
        let body = serde_json::to_vec(&serde_json::json!({ "messages": request.messages }))
            .map_err(|error| crate::error::cannot_serialize("to-device messages", error))?;
        client
            .send_to_device(
                &request.event_type.to_string(),
                request.txn_id.as_str(),
                body,
            )
            .await?;
        self.machine
            .mark_request_as_sent(&request.txn_id, &send_event_to_device::v3::Response::new())
            .await
            .map_err(olm_error)
    }

    /// 一個 ruma 請求 → 它的 HTTP body → 走橋。路徑與 method 由 server 的表決定，這裡只要 body 對。
    async fn call_bridge_with<C: PackChannel, R>(
        &self,
        client: &mut WbfClient<C>,
        endpoint: BridgedEndpoint,
        request: R,
    ) -> Result<protocol::BridgeReply, SdkError>
    where
        R: ruma::api::OutgoingRequest,
        R::Authentication:
            ruma::api::auth_scheme::AuthScheme<Input<'static> = SendAccessToken<'static>>,
        R::PathBuilder:
            ruma::api::path_builder::PathBuilder<Input<'static> = Cow<'static, SupportedVersions>>,
    {
        let body = ruma_request_body(request)?;
        client.call_bridge(endpoint, &NoVariables {}, body).await
    }
}

/// 房間金鑰發給誰（/docs/design/keys/e2ee-walkthrough.md §13 第 8 條：要明確選，🚫 不默默用預設）。
///
/// 選 `AllDevices`：發給成員每一台上傳過金鑰的裝置。wbfuwunel 的 /docs/design/wbf-room-device-version.md §1 建議的 `IdentityBasedStrategy`（只發給被擁有者交叉簽章過的裝置）
/// 要每個帳號都 bootstrap 過交叉簽章才有意義——client 這邊還沒做（`SigningKeysUpload` 只有號碼），現在選它等於發給零台裝置。
/// ✅ 交叉簽章做好之後要換成 `IdentityBasedStrategy`，這裡是唯一要改的地方。
/// 期限照房間自己設的（[`RoomKeyRotation`]）。
pub fn room_key_share_settings(rotation: RoomKeyRotation) -> EncryptionSettings {
    EncryptionSettings {
        sharing_strategy: CollectStrategy::AllDevices,
        rotation_period: rotation.max_age,
        rotation_period_msgs: rotation.max_messages,
        ..EncryptionSettings::default()
    }
}

/// 空窗就是追平，不管 `more`（server 保證第一則一定收進窗，所以停在上限的窗不會是空的）。
fn more_is_only_meaningful_with_items(window: &DeviceWindow) -> bool {
    !window.items.is_empty() && window.more
}

/// 只把 Olm session 的密文交給狀態機解，信任要求先照上游的預設（🚨 送出那一半決定「誰收得到金鑰」時要明確選，/docs/design/keys/e2ee-walkthrough.md §13 第 8 條）。
fn decryption_settings() -> DecryptionSettings {
    DecryptionSettings {
        sender_device_trust_requirement: TrustRequirement::Untrusted,
    }
}

fn parse_user_ids(users: &[String]) -> Result<Vec<OwnedUserId>, SdkError> {
    users
        .iter()
        .map(|user| {
            UserId::parse(user)
                .map_err(|error| SdkError::Usage(format!("bad user id {user:?}: {error}")))
        })
        .collect()
}

/// ruma 組請求時，要 token 的端點沒給 token 會直接拒絕；這裡給一個占位字串——只取 body，header 整個丟掉，
/// 真正的 `Authorization` 由橋在 server 那端用這條連線的 session 填（wbfuwunel 的 /docs/design/wbf-api-bridge.md §2.2 規則 1，client 蓋不掉）。
const PLACEHOLDER_ACCESS_TOKEN: &str = "not-sent-over-the-bridge";

/// ruma 請求組成 HTTP 請求（用一個假的 base URL）只為了拿它的 body bytes。
fn ruma_request_body<R>(request: R) -> Result<Vec<u8>, SdkError>
where
    R: ruma::api::OutgoingRequest,
    R::Authentication:
        ruma::api::auth_scheme::AuthScheme<Input<'static> = SendAccessToken<'static>>,
    R::PathBuilder:
        ruma::api::path_builder::PathBuilder<Input<'static> = Cow<'static, SupportedVersions>>,
{
    let versions = SupportedVersions::from_parts(&["v1.11".to_string()], &BTreeMap::new());
    let http_request = request
        .try_into_http_request::<Vec<u8>>(
            BRIDGE_BASE_URL,
            SendAccessToken::IfRequired(PLACEHOLDER_ACCESS_TOKEN),
            Cow::Owned(versions),
        )
        .map_err(|error| SdkError::Protocol(format!("cannot build the request body: {error}")))?;
    Ok(http_request.into_body())
}

/// 橋回的 body → ruma 的回應型別。狀態碼照橋的 `status`（一定是 2xx，`expect_bridge_reply` 擋過）。
fn parse_response<T: ruma::api::IncomingResponse>(
    reply: &protocol::BridgeReply,
    endpoint_name: &str,
) -> Result<T, SdkError> {
    let http_response = http::Response::builder()
        .status(reply.status)
        .header(http::header::CONTENT_TYPE, "application/json")
        .body(reply.body.as_slice())
        .map_err(|error| SdkError::Protocol(format!("{endpoint_name}: {error}")))?;
    T::try_from_http_response(http_response)
        .map_err(|error| SdkError::Protocol(format!("{endpoint_name} response: {error}")))
}

fn crypto_store_error(error: matrix_sdk_crypto::store::CryptoStoreError) -> SdkError {
    SdkError::Protocol(format!("crypto store: {error}"))
}

fn olm_error(error: matrix_sdk_crypto::OlmError) -> SdkError {
    SdkError::Protocol(format!("crypto: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `m.room.encryption` 的期限：有給正整數就照它，沒給、0、不是數字的那一項用上游預設；沒有這個狀態事件就全用預設。
    #[test]
    fn the_rotation_comes_from_the_rooms_encryption_content() {
        let defaults = EncryptionSettings::default();
        let default = RoomKeyRotation {
            max_age: defaults.rotation_period,
            max_messages: defaults.rotation_period_msgs,
        };
        assert_eq!(RoomKeyRotation::of_encryption_content(None), default);
        let set = serde_json::json!({ "algorithm": "m.megolm.v1.aes-sha2", "rotation_period_ms": 7_200_000, "rotation_period_msgs": 20 });
        assert_eq!(
            RoomKeyRotation::of_encryption_content(Some(&set)),
            RoomKeyRotation {
                max_age: Duration::from_secs(7200),
                max_messages: 20
            }
        );
        let odd = serde_json::json!({ "rotation_period_ms": "soon", "rotation_period_msgs": 0 });
        assert_eq!(RoomKeyRotation::of_encryption_content(Some(&odd)), default);
    }

    /// 提早換的門檻跟著期限縮：預設的期限照舊（5 則、1 小時），很短的期限 🚫 每送一則就換。
    #[test]
    fn the_early_rotation_margin_shrinks_with_a_short_period() {
        assert_eq!(pre_rotate_messages(100), 5);
        assert_eq!(pre_rotate_messages(8), 2);
        assert_eq!(
            pre_rotate_messages(1),
            0,
            "a one-message key is replaced right after its message"
        );
        assert_eq!(
            pre_rotate_age(Duration::from_secs(7 * 24 * 3600)),
            PRE_ROTATE_AGE
        );
        assert_eq!(
            pre_rotate_age(UPSTREAM_MIN_ROTATION_AGE),
            Duration::from_secs(15 * 60),
            "the shortest period upstream allows leaves 45 minutes of use"
        );
    }
}

//! E2EE 引擎：把 `matrix-sdk-crypto` 的 `OlmMachine` **只當狀態機用**（e2ee-walkthrough.md §13）——
//! server 給的推進去（to-device、自己的 OTK 存量），它要送的拉出來（`outgoing_requests`）走我們的橋送，回應再交回去。
//! 網路、水位、銷毀都在這個 crate 自己手上；🚫 沒有 `/sync`、🚫 沒有 matrix-sdk 的 `Client`。
//!
//! 這是 wbf-sdk 裡**第二個**碰上游的地方（第一個是 `backend/matrix_sdk`）；兩者共用同一個 sqlite crypto store（`m/`），
//! ⚠️ 同一時間只能有一個持有者（§13 第 9 條）——過渡期由呼叫端保證不同時開。
//!
//! 涵蓋到哪：這一版做「收」與「發金鑰」：金鑰上傳／查詢／claim、收 to-device、把房間金鑰分給一群人。
//! 加密房間事件並帶房間版本號送出（`Event/Send`）、1506 的重送迴圈在下一支。

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

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
    /// 落地後的水位。
    pub cd_seq: Option<u64>,
    /// 還留在待銷毀清單上的（server 這輪沒回來的，下次再送）。
    pub still_to_destroy: usize,
}

/// `pull_to_device` 最多拉幾窗：一窗 1000 則、六十四窗就是六萬多則 to-device，到這個數還沒追平是不對勁，停下來報錯。
const PULL_WINDOWS_LIMIT: usize = 64;

/// `refresh_room_devices` 一輪的結果：這一刻的房間快照（下次送帶它的 `room_version`）、跟上一份比出來的差、發了幾個 to-device。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RoomRefresh {
    /// 哪個房；`encrypt_and_send` 只收這個 struct，所以沒 refresh 過的房送不了（上游在沒有 outbound session 時是 panic 不是回錯）。
    pub room_id: String,
    /// 這一刻的房間版本號與每個 `join` 成員的裝置版本號——**呼叫者要存下來**，下次 refresh 當 `previous`、下次送帶它的 `room_version`。
    pub versions: RoomDeviceVersions,
    /// 誰要重查（新加入、裝置版本號變了）、誰離開了；`previous` 是 None 時全部算 changed。
    pub diff: MembersDiff,
    /// 這輪發了幾個 to-device（房間金鑰補發給新裝置）；0 ＝ 每台裝置都已經有了。
    pub shared_to_device_requests: usize,
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

/// `encrypt_and_send` 的結果：送進去了，或被 1506 擋下來。
/// 被擋不是 `Err`：那是這條路上**預期內**的結果（維護者定：daemon 補金鑰、UI 決定重送，e2ee-walkthrough §16.6）。
#[derive(Debug)]
pub enum SendOutcome {
    Sent {
        event_id: String,
    },
    /// server 說帶的 `room_version` 過期，訊息沒送。拿 `current_room_version` 只能知道自己過期，🚫 不能直接拿它重送：
    /// 先 `refresh_room_devices`（金鑰補到新的裝置），再帶那份快照的 `room_version` 重送（同一個 `txn_id`）。
    RoomDevicesChanged {
        current_room_version: Option<u64>,
        error: SdkError,
    },
}

pub struct OlmEngine {
    machine: OlmMachine,
    /// crypto store 與 `td.json` 所在的 `m/`。
    store_dir: PathBuf,
    /// 最近一次 `/keys/query` 回答這個人的 body（整份）：拿來重算裝置雜湊跟成員清單上的比（server §3.4）。
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

    /// 點進房間、被 1506 擋、（將來）收到 `DeviceChanged` 都叫這一支（e2ee-walkthrough §16.6：一支例行程序、三個觸發點）：
    /// 拿這一刻的成員清單與版本號 → 跟上一份比出誰變了 → 只重查那些人 → 雜湊對一次（不對再查一次，還不對就拒絕）→
    /// 把房間金鑰補給每台還沒有的裝置（有人離開由上游決定輪換）。
    ///
    /// Args:
    ///     client: 要能走橋的連線
    ///     room_id: example: "!r:localhost"
    ///     previous: 上一次的 `RoomRefresh::versions`（發上一輪房間金鑰時依據的那份）；None ＝ 第一次進這個房，每個人都查
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
        let shared_to_device_requests = self
            .share_room_key(client, room_id, &members, room_key_share_settings())
            .await?;
        Ok(RoomRefresh {
            room_id: room_id.to_string(),
            versions,
            diff,
            shared_to_device_requests,
            rechecked,
        })
    }

    /// 加密一則房間事件並帶房間版本號送出（`Event/Send`）。房間與號碼從 `refresh` 來：🚨 只收 `refresh_room_devices` 回的那份，
    /// 因為上游在這個房還沒有 outbound session（從沒 share 過）時是 **panic** 不是回錯——refresh 就是建 session 的那一步。
    /// 被 1506 擋是 `Ok(RoomDevicesChanged)` 不是 `Err`：這條路預期內的結果，訊息沒送，金鑰要補（再 refresh 一次）、重不重送由上層決定。
    ///
    /// Args:
    ///     refresh: 這個房最近一次 `refresh_room_devices` 的結果（帶它的 `versions.room_version`）
    ///     message: example: &OutgoingRoomEvent { event_type: "m.room.message".into(), content: json!({"msgtype":"m.text","body":"hi"}), txn_id: "txn-1".into(), attachments: vec![] }
    /// Return:
    ///     Ok(SendOutcome::Sent)                 送進去了
    ///     Ok(SendOutcome::RoomDevicesChanged)   1506：號碼過期，訊息沒送
    ///     Err(Protocol)                         加密失敗（例：這個房沒有 outbound session）
    ///     Err(Server)                           其他拒絕（含宣告過 feature 却漏帶號碼的 `InvalidRequest`）
    pub async fn encrypt_and_send<C: PackChannel>(
        &self,
        client: &mut WbfClient<C>,
        refresh: &RoomRefresh,
        message: &OutgoingRoomEvent,
    ) -> Result<SendOutcome, SdkError> {
        let owned_room_id: OwnedRoomId = RoomId::parse(&refresh.room_id).map_err(|error| {
            SdkError::Usage(format!("bad room id {:?}: {error}", refresh.room_id))
        })?;
        let raw_content: Raw<AnyMessageLikeEventContent> = Raw::from_json(
            serde_json::value::to_raw_value(&message.content)
                .map_err(|error| SdkError::Protocol(format!("event content: {error}")))?,
        );
        let encrypted = self
            .machine
            .encrypt_room_event_raw(&owned_room_id, &message.event_type, &raw_content)
            .await
            .map_err(|error| SdkError::Protocol(format!("encrypt: {error}")))?;
        let request = SendRequest {
            room_id: refresh.room_id.clone(),
            event_type: "m.room.encrypted".to_string(),
            txn_id: message.txn_id.clone(),
            attachments: message.attachments.clone(),
            room_version: Some(refresh.versions.room_version),
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

    /// 成員清單上的裝置雜湊，跟我們最近一次 `/keys/query` 答案照 server §3.4 重算的比。
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

    /// 一批 to-device 的完整處理（`Fetch` 的一窗、或推來的一包 `Push`：同一支，維護者 2026-09-24），**順序鎖死**（to-device-client.md §4、§7）：
    /// 匯進 crypto store（sqlite commit 了才回）→ 水位與待銷毀清單落地（`m/td.json`，原子寫）→ 才叫 server 銷毀
    /// （連上次沒銷成的一起）→ 只清 `ItemsDestroyed` 回來的。
    /// 🚨 呼叫者拿不到「先銷毀再匯入」的路，這就是這個函式存在的理由。中途任何一步失敗，已落地的照樣有效：
    /// 下次從 `cd_seq` 再拉是**重複**不是遺失（匯入與銷毀都冪等）。
    ///
    /// Args:
    ///     client: 要先 `device_subscribe` 過的連線（沒訂閱，銷毀那一步被 `Forbidden`，但匯入與落地已經完成）
    ///     items: `(count, 事件)` 舊→新：`DeviceWindow::items` 或 `SubscribeReply::Push` 的 `items`（可以是空的：那就只補送上次沒銷成的）
    ///     per_pack_timeout: example: Duration::from_secs(30)
    /// Return:
    ///     Ok(ImportReport)
    ///     Err(Protocol)   某則不是合法的 to-device 事件、store 寫入失敗——水位不動、不銷毀
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

    /// 從水位起一窗一窗拉到追平（to-device-client.md §7）：上線時「主動拉一次」就是它，推播說 `gap` 也是它。每一窗都走 `import_items`。
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
            let cd_seq = ToDeviceState::load(&self.store_dir)?.cd_seq;
            let window = client
                .device_fetch_window(
                    &DeviceFetchRequest {
                        cd_seq,
                        limit: None,
                    },
                    per_pack_timeout,
                )
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
    ///     Err(Protocol)         某則不是合法的 to-device 事件、或 store 寫入失敗
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

    /// 把 `room_id` 目前的房間金鑰分給這些人的裝置（缺 Olm session 的先 claim OTK 建），全部走橋。
    /// 上游決定該不該輪換、發給哪些裝置（`settings.sharing_strategy`）。
    ///
    /// Args:
    ///     client: 走橋用的連線
    ///     room_id: example: "!r:localhost"
    ///     users: 房間裡要拿金鑰的人（含自己）, example: &["@alice:localhost".to_string()]
    ///     settings: example: EncryptionSettings::default()
    /// Return:
    ///     Ok(usize)     送了幾個 to-device 請求（0 ＝ 每台裝置都已經有這把）
    pub async fn share_room_key<C: PackChannel>(
        &self,
        client: &mut WbfClient<C>,
        room_id: &str,
        users: &[String],
        settings: EncryptionSettings,
    ) -> Result<usize, SdkError> {
        let room_id: OwnedRoomId = RoomId::parse(room_id)
            .map_err(|error| SdkError::Usage(format!("bad room id {room_id:?}: {error}")))?;
        let users = parse_user_ids(users)?;
        // 先把追蹤中的查詢送完，名單才是最新的。
        self.send_outgoing_requests(client).await?;
        if let Some((request_id, claim)) = self
            .machine
            .get_missing_sessions(users.iter().map(OwnedUserId::as_ref))
            .await
            .map_err(crypto_store_error)?
        {
            let reply = self
                .call_bridge_with(client, protocol::BRIDGE_KEYS_CLAIM, claim)
                .await?;
            let response = parse_response::<claim_keys::v3::Response>(&reply, "KeysClaim")?;
            self.machine
                .mark_request_as_sent(&request_id, &response)
                .await
                .map_err(olm_error)?;
        }
        let requests = self
            .machine
            .share_room_key(&room_id, users.iter().map(OwnedUserId::as_ref), settings)
            .await
            .map_err(olm_error)?;
        let count = requests.len();
        for request in requests {
            self.send_to_device_request(client, &request).await?;
        }
        Ok(count)
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

/// 房間金鑰發給誰（§13 第 8 條：要明確選，🚫 不默默用預設）。
///
/// 選 `AllDevices`：發給成員每一台上傳過金鑰的裝置。server 規格 §1 建議的 `IdentityBasedStrategy`（只發給被擁有者交叉簽章過的裝置）
/// 要每個帳號都 bootstrap 過交叉簽章才有意義——client 這邊還沒做（`SigningKeysUpload` 只有號碼），現在選它等於發給零台裝置。
/// ✅ 交叉簽章做好之後要換成 `IdentityBasedStrategy`，這裡是唯一要改的地方。
pub fn room_key_share_settings() -> EncryptionSettings {
    EncryptionSettings {
        sharing_strategy: CollectStrategy::AllDevices,
        ..EncryptionSettings::default()
    }
}

/// 空窗就是追平，不管 `more`（server 保證第一則一定收進窗，所以停在上限的窗不會是空的）。
fn more_is_only_meaningful_with_items(window: &DeviceWindow) -> bool {
    !window.items.is_empty() && window.more
}

/// 只把 Olm session 的密文交給狀態機解，信任要求先照上游的預設（🚨 送出那一半決定「誰收得到金鑰」時要明確選，§13 第 8 條）。
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
/// 真正的 `Authorization` 由橋在 server 那端用這條連線的 session 填（wbf-api-bridge.md §2.2 規則 1，client 蓋不掉）。
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

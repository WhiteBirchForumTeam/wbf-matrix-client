//! 線上規格 §1、§3、§4 的訊息：怎麼組請求 pack、怎麼讀回應。
//!
//! 這裡不碰網路、不碰加密：輸入輸出都是 `Pack` 與 JSON。通道在 `channel`，加密在 `chunk_crypto`。

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use wbf_wire::pack::{control, device, download, event, flags, upload};
use wbf_wire::{EncryptedFileInfo, Kind, Pack};

use crate::error::SdkError;

/// `Hello` 的 meta（線上規格 §1）。
pub const PROTOCOL_VERSION: u32 = 1;

/// `Hello.features` 裡唯一 server 會讀的字串（wbfuwunel `wbf-room-device-version.md` §6.2）。
/// 宣告了，server 才推 `Event/DeviceChanged`；**宣告了之後，加密訊息漏帶 `room_version` 會被 `InvalidRequest` 拒**——
/// 所以只有「送出前會比對房間版本號」的那條路才能宣告它。server 的 `Hello` 回應 `features` 也列這一項，
/// 拿來判斷對面支不支援。
pub const DEVICE_VERSIONS_FEATURE: &str = "org.wbftw.device_versions";

// ---- 請求 ----

/// Args:
///     client_name: example: "wbf-cli/0.1"
///     features: 這條連線向 server 宣告的能力, example: &[DEVICE_VERSIONS_FEATURE]；平常是 &[]
///     seq: 請求號
pub fn hello(client_name: &str, features: &[&str], seq: u32) -> Pack {
    let meta = serde_json::json!({ "protocol": PROTOCOL_VERSION, "client": client_name, "features": features });
    control_request(control::HELLO, seq, meta.to_string().into_bytes())
}

pub fn ping(seq: u32) -> Pack {
    control_request(control::PING, seq, Vec::new())
}

/// Args:
///     info: 明文事實；串流模式 file_size 0 且 chunk_count 0
///     description_data: 描述（加密模式：密文；明文模式：JSON）
pub fn create(info: EncryptedFileInfo, description_data: Vec<u8>, seq: u32) -> Pack {
    Pack {
        kind: Kind::Upload,
        subtype: upload::CREATE,
        flags: 0,
        id: 0,
        seq,
        meta: info.to_bytes().to_vec(),
        data: description_data,
    }
}

/// Args:
///     upload_id: `Create` Ack 的 id
///     index: 塊索引，也是 pack 的 seq
///     is_last: 最後一塊帶 `IS_LAST`（串流模式必要）
pub fn chunk(upload_id: u64, index: u32, data: Vec<u8>, is_last: bool) -> Pack {
    Pack {
        kind: Kind::Upload,
        subtype: upload::CHUNK,
        flags: if is_last { flags::IS_LAST } else { 0 },
        id: upload_id,
        seq: index,
        meta: Vec::new(),
        data,
    }
}

pub fn status(upload_id: u64, seq: u32) -> Pack {
    upload_request(upload::STATUS, upload_id, seq, Vec::new())
}

/// Args:
///     description_data: 最終描述，取代 `Create` 那份（約定 §4：一律帶）
pub fn seal(upload_id: u64, description_data: Vec<u8>, seq: u32) -> Pack {
    upload_request(upload::SEAL, upload_id, seq, description_data)
}

pub fn abort(upload_id: u64, seq: u32) -> Pack {
    upload_request(upload::ABORT, upload_id, seq, Vec::new())
}

pub fn info(mxc: &str, seq: u32) -> Pack {
    download_request(download::INFO, seq, serde_json::json!({ "mxc": mxc }))
}

pub fn read_chunk(mxc: &str, index: u32, seq: u32) -> Pack {
    download_request(
        download::READ,
        seq,
        serde_json::json!({ "mxc": mxc, "chunk": index }),
    )
}

fn control_request(subtype: u8, seq: u32, meta: Vec<u8>) -> Pack {
    Pack {
        kind: Kind::Control,
        subtype,
        flags: 0,
        id: 0,
        seq,
        meta,
        data: Vec::new(),
    }
}

fn upload_request(subtype: u8, upload_id: u64, seq: u32, data: Vec<u8>) -> Pack {
    Pack {
        kind: Kind::Upload,
        subtype,
        flags: 0,
        id: upload_id,
        seq,
        meta: Vec::new(),
        data,
    }
}

fn download_request(subtype: u8, seq: u32, meta: serde_json::Value) -> Pack {
    Pack {
        kind: Kind::Download,
        subtype,
        flags: 0,
        id: 0,
        seq,
        meta: meta.to_string().into_bytes(),
        data: Vec::new(),
    }
}

// ---- 回應 ----

/// 線上規格 §2 的回應規則：Control、`IS_RESPONSE`、id 與 seq 抄請求的；`Ack` 過、`Error` 變 `SdkError::Server`。
///
/// Args:
///     request: 送出去的那個
///     response: 收回來的那個
/// Return:
///     Ok(Pack)          Ack，meta／data 給呼叫者解
///     Err(SdkError)     `Server`（Error pack）或 `Protocol`（形狀不對）
pub fn expect_ack(request: &Pack, response: Pack) -> Result<Pack, SdkError> {
    if response.kind != Kind::Control || response.flags & flags::IS_RESPONSE == 0 {
        return Err(SdkError::Protocol(format!(
            "response is not a Control response: kind {:?} flags {:#04x}",
            response.kind, response.flags
        )));
    }
    // id 與 seq 都要抄回（線上規格 §2）。唯一的放寬：wbfuwunel 對 `Create` 的回應把新發的上傳 id 放在標頭，
    // 所以只有 `Create` 允許標頭 id 不是 0，而 `create_upload` 會再拿它對 Ack meta 的 `id`。其他 id 0 的請求
    // （Hello、Ping、Info、Read）回應 id 必須是 0。
    let is_create = request.kind == Kind::Upload && request.subtype == upload::CREATE;
    let id_echoed = response.id == request.id || is_create;
    if !id_echoed || response.seq != request.seq {
        return Err(SdkError::Protocol(format!(
            "response id/seq {}/{} does not echo request {}/{}",
            response.id, response.seq, request.id, request.seq
        )));
    }
    match response.subtype {
        control::ACK | control::PONG => Ok(response),
        control::ERROR => Err(server_error(&response.meta)),
        other => Err(SdkError::Protocol(format!(
            "unexpected response subtype {other:#04x}"
        ))),
    }
}

// ---- 橋：pack 帶 flags bit4，server 轉成內部 HTTP 請求交給 Matrix 端點（wbfuwunel `wbf-api-bridge.md`）----
//
// 🚨 **號碼的權威在 server 的 `docs/bridge-specs/index.md`**（wire-format §3.2 只列原生的）。這裡只抄**用得到的**那幾個，
// 用到一個抄一個，🚫 不整張表搬過來 —— 搬過來的那份不會知道 server 改了。

/// server 在 `Hello.features` 宣告「橋在」的字串（wbf-api-bridge.md §3 批 3-C）。沒宣告的 server 不送橋的 pack。
pub const BRIDGE_FEATURE: &str = "bridge";
/// server 宣告「`0x16 Device` 的原生 pack（to-device 佇列）在」的字串（同上）。
pub const DEVICE_FEATURE: &str = "device";

/// 一支走橋的 Matrix 端點：kind ＋ subtype 決定 method 與路徑模板（server 那邊的白名單）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BridgedEndpoint {
    pub kind: Kind,
    pub subtype: u8,
}

/// `GET /_matrix/client/v3/rooms/{room_id}/members`（bridge-specs `0x13-room.md` §0x29）。
/// 回應多兩個 Matrix 沒有的欄位：最外層 `org.wbftw.room_version`、每個 `join` 成員的 `unsigned["org.wbftw.device_version"]`
/// —— 讀法在 `device_version::RoomDeviceVersions::from_members_body`。🚫 不收 `at`。
pub const BRIDGE_MEMBERS: BridgedEndpoint = BridgedEndpoint {
    kind: Kind::Room,
    subtype: 0x29,
};
/// `PUT /_matrix/client/v3/sendToDevice/{event_type}/{txn_id}`（bridge-specs `0x16-device.md` §0x25）：**發** to-device。
/// `txn_id` 是冪等鍵：重試用同一個，新的一則換一個。
pub const BRIDGE_SEND_TO_DEVICE: BridgedEndpoint = BridgedEndpoint {
    kind: Kind::Device,
    subtype: 0x25,
};
/// `POST /_matrix/client/v3/keys/upload`（bridge-specs `0x17-keys.md` §0x20）。data 送 `{}` 就是讀回目前的 OTK 數量。
pub const BRIDGE_KEYS_UPLOAD: BridgedEndpoint = BridgedEndpoint {
    kind: Kind::Keys,
    subtype: 0x20,
};
/// `POST /_matrix/client/v3/keys/query`（§0x21）。
pub const BRIDGE_KEYS_QUERY: BridgedEndpoint = BridgedEndpoint {
    kind: Kind::Keys,
    subtype: 0x21,
};
/// `POST /_matrix/client/v3/keys/claim`（§0x22）。⚠️ claim 走的那把就從對方的庫存消失。
pub const BRIDGE_KEYS_CLAIM: BridgedEndpoint = BridgedEndpoint {
    kind: Kind::Keys,
    subtype: 0x22,
};
/// `POST /_matrix/client/v3/keys/device_signing/upload`（§0x24）。換掉既有的交叉簽章金鑰要 UIAA（index.md §1.5），第一次上傳不用。
pub const BRIDGE_SIGNING_KEYS_UPLOAD: BridgedEndpoint = BridgedEndpoint {
    kind: Kind::Keys,
    subtype: 0x24,
};
/// `POST /_matrix/client/v3/keys/signatures/upload`（§0x25）。
pub const BRIDGE_SIGNATURES_UPLOAD: BridgedEndpoint = BridgedEndpoint {
    kind: Kind::Keys,
    subtype: 0x25,
};

/// 沒有任何變數的端點（`/keys/*` 那幾支）：meta 是 `{}`。
#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
pub struct NoVariables {}

/// `Members` 的變數：欄位順序照 bridge-specs 的範例。`membership` 沒給就整個省掉（index.md §1.3：query 變數不送空值）。
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct MembersVariables<'a> {
    pub room_id: &'a str,
    /// `join`／`invite`／`leave`／`ban`／`knock`；None ＝ 照 Matrix 預設回所有成員事件
    #[serde(skip_serializing_if = "Option::is_none")]
    pub membership: Option<&'a str>,
}

/// `SendToDevice` 的變數（兩個都是 path 變數，缺了 server 回 `InvalidRequest`）。
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct SendToDeviceVariables<'a> {
    pub event_type: &'a str,
    pub txn_id: &'a str,
}

/// 走橋的請求：`id` 填 0（一個請求一個回應，不開會話）、flags 只有 `IS_BRIDGED`。
///
/// Args:
///     endpoint: example: BRIDGE_MEMBERS
///     variables: 路徑與 query 變數的 JSON 物件（**欄位順序就是線上的順序**，用 struct 定），example: &MembersVariables { room_id: "!r:x", membership: Some("join") }
///     body: HTTP body 原樣；沒有 body 的端點給空
///     seq: 請求號
pub fn bridge_request(
    endpoint: BridgedEndpoint,
    variables: &impl Serialize,
    body: Vec<u8>,
    seq: u32,
) -> Pack {
    Pack {
        kind: endpoint.kind,
        subtype: endpoint.subtype,
        flags: flags::IS_BRIDGED,
        id: 0,
        seq,
        meta: serde_json::to_vec(variables).expect("bridge variables serialize"),
        data: body,
    }
}

/// 走橋成功回覆（`Control/Ack` ＋ `IS_BRIDGED`）的 meta。
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct BridgeAckMeta {
    pub status: u16,
    /// 只有表上宣告要轉的 header 才會出現。
    #[serde(default)]
    pub headers: std::collections::BTreeMap<String, String>,
}

/// 走橋的成功回覆：Matrix 端點回的 HTTP 狀態、header 與 body 原樣。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BridgeReply {
    pub status: u16,
    pub headers: std::collections::BTreeMap<String, String>,
    pub body: Vec<u8>,
}

impl BridgeReply {
    /// Return:
    ///     Ok(Value)       body 解成 JSON
    ///     Err(Protocol)   body 不是 JSON（帶端點名字，好認）
    pub fn json(&self, endpoint_name: &str) -> Result<serde_json::Value, SdkError> {
        serde_json::from_slice(&self.body).map_err(|error| {
            SdkError::Protocol(format!("{endpoint_name} body is not JSON: {error}"))
        })
    }
}

/// 驗走橋的回應（index.md §1.2）。
///
/// - `Error`：一律 `SdkError::Server`（meta 帶 `status`／`errcode`…）。⚠️ **不看有沒有 bit4**：session 在橋之前就被 WS 擋下時，
///   回的是通道自己的 `Error`（沒有 bit4、data 空），那一樣是被拒。
/// - `Ack`：🚨 **必須帶 bit4、狀態必須是 2xx**，否則是 `Protocol` —— 不帶 bit4 的 Ack 不是這個請求的答案，🚫 不當成功。
///
/// Return:
///     Ok(BridgeReply)
///     Err(Server)    server 或 Matrix 端點拒絕
///     Err(Protocol)  形狀不對（id／seq 沒抄、不是 Control 回應、Ack 沒帶 bit4、meta 解不開、狀態不是 2xx）
pub fn expect_bridge_reply(request: &Pack, response: Pack) -> Result<BridgeReply, SdkError> {
    let is_bridged = response.flags & flags::IS_BRIDGED != 0;
    let ack = expect_ack(request, response)?;
    // 🚨 `expect_ack` 也放行 `Pong`（ping 要用）；橋的成功回覆只有 `Ack`（index.md §1.2）。一個 meta 湊成 `{"status":200}`
    // 的 Pong 不是這個請求的答案，🚫 不當成功。
    if ack.subtype != control::ACK {
        return Err(SdkError::Protocol(format!(
            "a bridged request must be answered by Control/Ack, got subtype {:#04x}",
            ack.subtype
        )));
    }
    if !is_bridged {
        return Err(SdkError::Protocol(
            "a bridged request was answered by an Ack without IS_BRIDGED".into(),
        ));
    }
    let meta: BridgeAckMeta = parse_meta(&ack)?;
    if !(200..300).contains(&meta.status) {
        return Err(SdkError::Protocol(format!(
            "a bridged Ack must carry a 2xx status, got {}",
            meta.status
        )));
    }
    Ok(BridgeReply {
        status: meta.status,
        headers: meta.headers,
        body: ack.data,
    })
}

/// Error pack 的 meta → `SdkError::Server`。meta 不是 JSON 也照樣回 `Server`，code 填 `unknown`：
/// 對方明說拒絕了，不能因為訊息壞掉就當成別的。
pub fn server_error(meta: &[u8]) -> SdkError {
    let value: serde_json::Value = serde_json::from_slice(meta).unwrap_or(serde_json::Value::Null);
    let code = value
        .get("code")
        .and_then(|code| code.as_str())
        .unwrap_or("unknown")
        .to_string();
    let message = value
        .get("message")
        .and_then(|message| message.as_str())
        .unwrap_or("(no message)")
        .to_string();
    // 🚨 只收**非 0 的整數**：`0` 是欄位漏了的預設值（server 表：`0` 永遠不是合法的碼），
    // 字串 `"1503"`、負數、小數都不是 server 會送的形狀 —— 🚫 不猜，當成沒有。
    let code_id = value
        .get("code_id")
        .and_then(|code_id| code_id.as_u64())
        .filter(|code_id| *code_id != 0);
    SdkError::Server {
        code,
        message,
        meta: value,
        code_id,
    }
}

/// Args:
///     ack: `expect_ack` 回的
/// Return:
///     Ok(T)             meta 解成 T
///     Err(SdkError)     `Protocol`：不是 T 的形狀
pub fn parse_meta<T: DeserializeOwned>(ack: &Pack) -> Result<T, SdkError> {
    serde_json::from_slice(&ack.meta).map_err(|error| {
        SdkError::Protocol(format!(
            "ack meta: {error}: {}",
            String::from_utf8_lossy(&ack.meta)
        ))
    })
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct HelloAck {
    pub protocol: u32,
    pub server: String,
    #[serde(default)]
    pub features: Vec<String>,
    pub chunk_size_default: u32,
    pub chunk_size_large: u32,
    pub data_max_bytes: u64,
    /// pack-pipeline §6：`Recent` 一窗的預設與上限；舊 server 沒有這些欄，None 時 client 用自己的預設（`RECENT_*`）。
    #[serde(default)]
    pub recent_default_limit: Option<u32>,
    #[serde(default)]
    pub recent_max_limit: Option<u32>,
    #[serde(default)]
    pub recent_default_batch: Option<u32>,
    #[serde(default)]
    pub recent_max_batch: Option<u32>,
    /// pack-pipeline §2.1：每個 device 最多幾條 WS；超過 server 回 `TooManyConnections`。
    #[serde(default)]
    pub max_connections_per_device: Option<u32>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct CreateAck {
    pub id: u64,
    pub mxc: String,
    pub chunk_size: u32,
    pub chunk_max_bytes: u64,
    pub expires_at: u64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct ChunkAck {
    pub received: u32,
    /// 串流模式 null。
    pub chunk_count: Option<u32>,
    pub total_len: u64,
    pub finished: bool,
    pub truncated: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct StatusAck {
    pub received: u32,
    pub chunk_count: Option<u32>,
    pub total_len: u64,
    pub finished: bool,
    pub truncated: bool,
    pub chunk_size: u32,
    /// 串流模式 null。
    pub file_size: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct SealAck {
    pub mxc: String,
}

/// 線上規格 §4.1：整檔媒體（舊上傳）的分塊欄位是 null。
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct InfoAck {
    pub total_len: u64,
    pub file_size: Option<u64>,
    pub chunk_size: Option<u32>,
    pub chunk_count: Option<u32>,
    pub truncated: Option<bool>,
    pub content_type: Option<String>,
    pub read_len: Option<u64>,
    pub chunk_size_large: Option<u32>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct ReadAck {
    pub chunk: u32,
    pub pos: u64,
    pub len: u64,
    pub chunk_size: u32,
    pub chunk_count: u32,
    pub total_len: u64,
}

// ---- Event（kind 0x14）：server 的 room-seq-and-recent.md §2、media-attachments.md §3 ----

/// `unsigned` 裡 server 加的每房連續序號（第一個事件是 1；聯邦補回的歷史 0、−1、…）。
pub const R_SEQ_KEY: &str = "org.wbftw.wbfuwunel.r_seq";
/// `unsigned` 裡 server 加的本站全域序號，跨房間可比大小，client 當水位線。
pub const G_SEQ_KEY: &str = "org.wbftw.wbfuwunel.g_seq";

/// `Recent` 的預設與上限（server 的 `wbf_recent_*`；pack-pipeline §6.1）。server 的 `Hello` 有給就用它的。
pub const RECENT_DEFAULT_LIMIT: u32 = 320;
pub const RECENT_MAX_LIMIT: u32 = 500;
pub const RECENT_DEFAULT_BATCH: u32 = 10;
pub const RECENT_MAX_BATCH: u32 = 100;

/// `Event/Recent` 的請求 meta：**一窗**。欄位順序就是線上的 JSON 順序（向量檔逐 byte 比），不要重排。
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct RecentRequest {
    /// 只讀這幾個房間；`None` ＝ 每個加入的房（wbfuwunel #51）。⭐ **一個房 ＋ `before` 就是那個房的歷史**。
    /// ⚠️ 點名一個自己不在的房，整個請求回 `Forbidden`；`Some(vec![])` 是問零個房、拿空窗。
    /// 📎 放在第一個欄位只是為了讓既有三筆向量的 byte 順序不變（`limit` 仍在 `cg_seq` 前面）；
    /// server 用 JSON 解析，🚫 不看順序。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rooms: Option<Vec<String>>,
    /// 這一窗最多幾則；server 的 `wbf_recent_max_limit`（預設 500）以上會被 clamp，所以 client 也先 clamp（不然算不出「窗滿了沒」）。
    pub limit: u32,
    /// client 快取裡最新的 `g_seq`；None 或 0 = 沒有快取。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cg_seq: Option<i64>,
    /// 下一窗：只要比它舊的（上一窗最後一個 Batch 的 `ls`）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub before: Option<i64>,
    /// 每個 Batch 幾則；None 用 server 預設（10），上限 100。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub batch: Option<u32>,
}

/// Args:
///     request: example: RecentRequest { rooms: None, limit: 320, cg_seq: Some(4700), before: None, batch: Some(10) }
///     id: client 自己選的，回應（一串 `Batch`）抄它；不能是靠 seq 對回應的 0
///     seq: 請求號
pub fn recent(request: &RecentRequest, id: u64, seq: u32) -> Pack {
    Pack {
        kind: Kind::Event,
        subtype: event::RECENT,
        flags: 0,
        id,
        seq,
        meta: serde_json::to_vec(request).expect("RecentRequest serializes"),
        data: Vec::new(),
    }
}

/// `Event/Batch` 的 meta（pack-pipeline §6.2）。
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
pub struct BatchMeta {
    /// total count：這一窗總共幾則（≤ limit），同一窗每個 Batch 都一樣。
    pub tc: u32,
    /// batch count：這個 Batch 幾則。
    pub bc: u32,
    /// 這批最新那則的 g_seq（空 Batch 是 0）。
    pub fs: i64,
    /// 這批最舊那則的 g_seq；最後一個 Batch 的 `ls` 是下一窗的 `before`。
    pub ls: i64,
    /// remain：這批之後這一窗還剩幾則；0 就是這窗結束。
    pub r: u32,
    /// 🚨 **這一窗停在上限（則數或位元組）而不是事件用完**（wbfuwunel 窗的位元組上限，2026-09-14 合併）。
    ///
    /// ⚠️ 位元組上限滿的窗 `tc < limit`，所以「`tc < limit` ＝沒有更多」**不再成立** ——
    /// 照舊規則會把水位推過還在的事件。追平與否一律看這個欄位。
    /// 📎 **沒有這個欄位要當 `true`**（server 的規則）：多一趟請求，換不留洞。
    #[serde(default = "more_when_absent")]
    pub more: bool,
}

/// `BatchMeta::more` 缺欄位時的值。⚠️ 是 `true`：不確定就再問一趟，🚫 不假設已經拿完。
fn more_when_absent() -> bool {
    true
}

/// `Recent` 的回應要是 `Event/Batch`：`IS_RESPONSE`、`id` 抄請求、`seq` 是這窗的第幾個 Batch（從 0 嚴格 +1）。
/// `Control/Error` 照 `expect_ack` 一樣變 `Server`。
///
/// Args:
///     request: 送出去的 `Recent`
///     response: 收到的一個 pack
///     expected_seq: 這是這窗的第幾個 Batch，example: 0
/// Return:
///     Ok(Pack)          Batch，交給 `parse_batch`
///     Err(Server)       server 回 Error（例：走 HTTP 的 `Unsupported`）
///     Err(Protocol)     不是 Batch、id 沒抄、seq 跳號
pub fn expect_batch(request: &Pack, response: Pack, expected_seq: u32) -> Result<Pack, SdkError> {
    if response.kind == Kind::Control && response.subtype == control::ERROR {
        return Err(server_error(&response.meta));
    }
    if response.kind != Kind::Event
        || response.subtype != event::BATCH
        || response.flags & flags::IS_RESPONSE == 0
    {
        return Err(SdkError::Protocol(format!(
            "expected an Event/Batch response, got kind {:?} subtype {:#04x} flags {:#04x}",
            response.kind, response.subtype, response.flags
        )));
    }
    if response.id != request.id {
        return Err(SdkError::Protocol(format!(
            "Batch id {} does not echo the Recent id {}",
            response.id, request.id
        )));
    }
    if response.seq != expected_seq {
        return Err(SdkError::Protocol(format!(
            "Batch seq {} where {expected_seq} was expected (batches must be 0, 1, 2, …)",
            response.seq
        )));
    }
    Ok(response)
}

/// 一個 Batch → meta 與事件 JSON（新到舊）。data 是 `bc` 則「u32 大端長度 ＋ 事件 JSON」。
///
/// Return:
///     Ok((BatchMeta, Vec<Value>))
///     Err(Protocol)     meta 不是 BatchMeta、長度前綴切不齊、則數與 `bc` 不符、事件不是 JSON、`tc < bc + r`
pub fn parse_batch(batch: &Pack) -> Result<(BatchMeta, Vec<serde_json::Value>), SdkError> {
    let meta: BatchMeta = parse_meta(batch)?;
    let items = split_length_prefixed(&batch.data)?;
    if items.len() as u32 != meta.bc {
        return Err(SdkError::Protocol(format!(
            "Batch meta says bc {} but data holds {} events",
            meta.bc,
            items.len()
        )));
    }
    if meta.tc < meta.bc.saturating_add(meta.r) {
        return Err(SdkError::Protocol(format!(
            "Batch meta inconsistent: tc {} < bc {} + r {}",
            meta.tc, meta.bc, meta.r
        )));
    }
    if meta.bc > 0 && meta.fs < meta.ls {
        return Err(SdkError::Protocol(format!(
            "Batch fs {} < ls {} (events must be newest first)",
            meta.fs, meta.ls
        )));
    }
    let mut events = Vec::with_capacity(items.len());
    for (position, item) in items.iter().enumerate() {
        let event: serde_json::Value = serde_json::from_slice(item).map_err(|error| {
            SdkError::Protocol(format!("Batch event {position} is not JSON: {error}"))
        })?;
        events.push(event);
    }
    Ok((meta, events))
}

/// `u32 大端長度 ＋ bytes` 重複到底。多一個 byte、長度指過檔尾都是錯。
///
/// Args:
///     data: example: [0,0,0,2, b'{', b'}']
/// Return:
///     Ok(Vec<&[u8]>)    每一段
///     Err(Protocol)     切不齊
pub fn split_length_prefixed(data: &[u8]) -> Result<Vec<&[u8]>, SdkError> {
    let mut items = Vec::new();
    let mut rest = data;
    while !rest.is_empty() {
        if rest.len() < 4 {
            return Err(SdkError::Protocol(format!(
                "length-prefixed data ends with {} stray byte(s)",
                rest.len()
            )));
        }
        let len = u32::from_be_bytes([rest[0], rest[1], rest[2], rest[3]]) as usize;
        rest = &rest[4..];
        if rest.len() < len {
            return Err(SdkError::Protocol(format!(
                "length prefix {len} runs past the end ({} bytes left)",
                rest.len()
            )));
        }
        items.push(&rest[..len]);
        rest = &rest[len..];
    }
    Ok(items)
}

/// `split_length_prefixed` 的反向（測試與 fake server 用）。
pub fn join_length_prefixed(items: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    for item in items {
        out.extend_from_slice(&(item.len() as u32).to_be_bytes());
        out.extend_from_slice(item);
    }
    out
}

/// 從事件 JSON 的 `unsigned` 讀 server 加的序號；沒有就是 None（非 fork server、或舊事件），呼叫者顯式判斷，不猜。
///
/// Args:
///     event: `parse_batch`（`recent_window`／`recent_sync` 交出來的）或 sync 回來的一則事件
/// Return:
///     (Option<i64>, Option<i64>)   (r_seq, g_seq)
pub fn event_seqs(event: &serde_json::Value) -> (Option<i64>, Option<i64>) {
    let unsigned = event.get("unsigned");
    let read = |key: &str| {
        unsigned
            .and_then(|unsigned| unsigned.get(key))
            .and_then(|value| value.as_i64())
    };
    (read(R_SEQ_KEY), read(G_SEQ_KEY))
}

/// `Event/Send` 的請求 meta（media-attachments.md §3、wbfuwunel `wbf-room-device-version.md` §7）。
/// `attachments` 是這則訊息用到的 mxc，server 讀不到 E2EE 內容，靠它替媒體 +1；不宣告的媒體過保護期會被清掉（spec §12）。
/// 鍵序就是線上的 JSON 序（向量逐 byte 比），不要重排。
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct SendRequest {
    pub room_id: String,
    #[serde(rename = "type")]
    pub event_type: String,
    pub txn_id: String,
    /// 空的就不寫（server 向量 `send_encrypted_with_room_version` 沒有這個欄位；server 把缺欄位當空清單）。
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<String>,
    /// 這則加密訊息的房間金鑰是照哪個房間版本號發的（成員清單最外層的 `org.wbftw.room_version`）。
    /// server 只對 `m.room.encrypted` 檢查；對不上回 `RoomDevicesChanged`（1506），訊息沒送。
    /// None ＝ 不帶：沒宣告 `DEVICE_VERSIONS_FEATURE` 的連線不檢查；宣告過的連線送加密訊息不帶會被 `InvalidRequest` 拒。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub room_version: Option<u64>,
}

/// Args:
///     request: example: SendRequest { room_id: "!r:localhost".into(), event_type: "m.room.encrypted".into(), txn_id: "t1".into(), attachments: vec!["mxc://localhost/1122334455667788".into()], room_version: Some(81234) }
///     content: 事件 content 的 JSON bytes（E2EE 就是 `m.room.encrypted` 的 content）
pub fn send_event(request: &SendRequest, content: Vec<u8>, seq: u32) -> Pack {
    Pack {
        kind: Kind::Event,
        subtype: event::SEND,
        flags: 0,
        id: 0,
        seq,
        meta: serde_json::to_vec(request).expect("SendRequest serializes"),
        data: content,
    }
}

/// `Event/Send` 的 Ack meta（server 向量 `ack_send`）：`event_id`。
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct SendAck {
    pub event_id: String,
}

/// `Event/DeviceChanged`（`0x14 0x07`，只有 server → client）的 meta（wbfuwunel `wbf-room-device-version.md` §6）。
/// 某人的裝置版本號變了；`rooms` 是這條連線訂閱中、而且他在裡面的房間 → 各自新的房間版本號。
/// 一條連線一次變動只收一個，不管共同幾個房。丟了由 `gap` 提醒，最後由送出時的 1506 擋。
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct DeviceChangedMeta {
    pub user_id: String,
    /// `序號-雜湊`，解讀用 `device_version::DeviceVersion::parse`。
    pub device_version: String,
    pub rooms: std::collections::BTreeMap<String, u64>,
    /// true ＝ 這條連線前面有推送（`Push` 或 `DeviceChanged`）被丟掉：重拿那些房間的成員清單。
    /// 🚨 缺欄位當 true（不確定就多拿一次，🚫 不假設沒丟）。
    #[serde(default = "gap_when_missing")]
    pub gap: bool,
}

/// `Device/CryptoState`（`0x16 0x08`，只有 server → client）的 meta（wbfuwunel `wbf-e2ee.md` §3）：
/// **自己這台裝置**的金鑰存量，跟 `/sync` 的 `device_one_time_keys_count`、`device_unused_fallback_key_types` 同義。
/// 每個 `Device/Subscribe` 之後一定跟一個；OTK 被 claim、上傳、fallback 被用掉時再推。
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct CryptoStateMeta {
    /// 每種演算法剩幾把 OTK, example: {"signed_curve25519": 42}
    pub otk_counts: std::collections::BTreeMap<String, u64>,
    /// 還沒被用掉的 fallback key 演算法。⚠️ `[]` 與「沒給」意思不同：OlmMachine 把沒給當 server 不支援、把 `[]` 當「都用掉了，該換」。
    /// server 保證一定給，所以缺欄位是形狀錯，🚫 不用 `default` 補成空。
    pub unused_fallback_key_types: Vec<String>,
    /// 同 [`DeviceChangedMeta::gap`]：缺欄位當 true。
    #[serde(default = "gap_when_missing")]
    pub gap: bool,
}

fn gap_when_missing() -> bool {
    true
}

// ---- Device（kind 0x16）：to-device 佇列的原生 pack（wbfuwunel `wbf-to-device.md` §3；client 端的解讀在 to-device-client.md）----
//
// 跟 `Event` 那組刻意不同的三處（server §3.1）：**順序舊→新**；`ot`／`nt` 不是 `fs`／`ls`（兩邊方向相反，🚫 不混用）；
// 每則的 count 不在事件裡，在 meta 的 `counts`（跟 data 一一對應）。
// 📎 訂閱（`Subscribe`／`Push`／`CryptoState`）要能收非回應的 pack，通道還沒有那個能力（daemon-runtime 第 4 階段）；
// 這裡先只有 `Fetch`／`Batch`／`ItemsDestroy`／`ItemsDestroyed` 這條「拉」的路，`Subscribe` 只有編碼。

/// `Device/Fetch` 的請求 meta。鍵序照 server 向量 `device_fetch`（`cd_seq` 在 `limit` 前）。
#[derive(Clone, Debug, Default, Serialize, PartialEq, Eq)]
pub struct DeviceFetchRequest {
    /// 只要比它新的：我**已經處理完**到哪（to-device-client.md §2）；None ＝ 從頭。下一窗帶上一窗的 `nt`。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cd_seq: Option<u64>,
    /// 這一窗最多幾則；None 用 server 預設（1000）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

/// Args:
///     request: example: DeviceFetchRequest { cd_seq: Some(4711), limit: Some(1000) }
///     id: client 選的會話號（型別 `SESSION`）, example: pack::id::compose(pack::id::SESSION, 31)
pub fn device_fetch(request: &DeviceFetchRequest, id: u64, seq: u32) -> Pack {
    Pack {
        kind: Kind::Device,
        subtype: device::FETCH,
        flags: 0,
        id,
        seq,
        meta: serde_json::to_vec(request).expect("DeviceFetchRequest serializes"),
        data: Vec::new(),
    }
}

/// `Device/Subscribe` 的 meta。鍵序照向量 `device_subscribe`（`cd_seq`、`device_id`）。
/// ⚠️ `device_id` 是明示意圖，server 會跟 session 的比對，不合回 `Forbidden`（to-device-client.md §5）。
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct DeviceSubscribeRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cd_seq: Option<u64>,
    pub device_id: String,
}

pub fn device_subscribe(request: &DeviceSubscribeRequest, id: u64, seq: u32) -> Pack {
    Pack {
        kind: Kind::Device,
        subtype: device::SUBSCRIBE,
        flags: 0,
        id,
        seq,
        meta: serde_json::to_vec(request).expect("DeviceSubscribeRequest serializes"),
        data: Vec::new(),
    }
}

/// `Device/Subscribe`（不帶 `cd_seq`）的回覆，照 server 送來的順序：先 `Ack`（登記好了），再 `CryptoState`（自己的金鑰存量）。
/// 📎 帶 `cd_seq` 的訂閱會在兩者之間補一輪 `Push`——那要通道能收推播，還沒有（daemon-runtime 第 4 階段），所以這裡不認 `Push`。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SubscribeReply {
    /// 登記好了：這條連線現在持有這台裝置的佇列（之後才能 `ItemsDestroy`）。
    Acknowledged,
    /// 自己這台裝置的 OTK 存量；每個 `Subscribe` 之後一定跟一個。
    CryptoState(CryptoStateMeta),
}

/// Args:
///     request: 送出的 `Subscribe` pack
/// Return:
///     Ok(SubscribeReply)
///     Err(Server)      `Error`（含 `Forbidden`：`device_id` 不是這個 session 的）
///     Err(Protocol)    `id` 沒抄、`Ack` 的 `seq` 沒抄、或不是 Ack／CryptoState（例：帶 `cd_seq` 才會有的 `Push`）
pub fn parse_subscribe_reply(request: &Pack, response: &Pack) -> Result<SubscribeReply, SdkError> {
    if response.kind == Kind::Control && response.subtype == control::ERROR {
        return Err(server_error(&response.meta));
    }
    if response.id != request.id {
        return Err(SdkError::Protocol(format!(
            "Subscribe reply id {} does not echo the request id {}",
            response.id, request.id
        )));
    }
    if response.flags & flags::IS_RESPONSE == 0 {
        return Err(SdkError::Protocol(
            "Subscribe reply without IS_RESPONSE".into(),
        ));
    }
    match (response.kind, response.subtype) {
        (Kind::Control, control::ACK) if response.seq == request.seq => Ok(SubscribeReply::Acknowledged),
        (Kind::Control, control::ACK) => Err(SdkError::Protocol(format!(
            "Subscribe Ack seq {} does not echo the request seq {}",
            response.seq, request.seq
        ))),
        (Kind::Device, device::CRYPTO_STATE) => {
            Ok(SubscribeReply::CryptoState(parse_meta(response)?))
        }
        (kind, subtype) => Err(SdkError::Protocol(format!(
            "expected Ack or Device/CryptoState after Subscribe, got kind {kind:?} subtype {subtype:#04x}"
        ))),
    }
}

/// `Device/Unsubscribe`：說出口的退出（wbf-to-device.md §4）——解除這條連線對裝置佇列的持有；回 `Ack {}`，沒訂也是 no-op。
/// 🚨 下線前要叫：不叫的話這條連線退了卻還佔著裝置，別的連線得靠搶佔才進得來。斷線 server 會自動退，但那是「沒說出口的退出」，兩條路都要有。
///
/// Args:
///     id: client 選的會話號（這個 kind 每個 subtype 都要 `SESSION` 型別的 id）
pub fn device_unsubscribe(id: u64, seq: u32) -> Pack {
    Pack {
        kind: Kind::Device,
        subtype: device::UNSUBSCRIBE,
        flags: 0,
        id,
        seq,
        meta: b"{}".to_vec(),
        data: Vec::new(),
    }
}

/// `Device/ItemsDestroy`：meta `{"tc"}`，data 是 `tc` 個 u64 大端的 count（不是 JSON、沒有分隔符）。
/// 送的是**清單**不是水位（to-device-client.md §4）：只列匯進 crypto store 成功的那些。
///
/// Args:
///     counts: example: &[4712, 4713]
pub fn device_items_destroy(counts: &[u64], id: u64, seq: u32) -> Pack {
    let mut data = Vec::with_capacity(counts.len() * 8);
    for count in counts {
        data.extend_from_slice(&count.to_be_bytes());
    }
    Pack {
        kind: Kind::Device,
        subtype: device::ITEMS_DESTROY,
        flags: 0,
        id,
        seq,
        meta: serde_json::json!({ "tc": counts.len() })
            .to_string()
            .into_bytes(),
        data,
    }
}

/// `Device/Batch` 的 meta（server §3）。
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct DeviceBatchMeta {
    /// 這一窗總共幾則（≤ limit）。
    pub tc: u32,
    /// 這個 Batch 幾則。
    pub bc: u32,
    /// oldest：這批最舊的 count（舊→新，所以是第一則）。
    pub ot: u64,
    /// newest：這批最新的 count（最後一則）——下一窗的 `cd_seq`。
    pub nt: u64,
    /// 每則的 count，跟 data 一一對應（`bc` 個）。
    pub counts: Vec<u64>,
    /// 這窗還剩幾則沒送。
    pub r: u32,
    /// 這窗停在上限（則數或 byte）；🚨 缺欄位當 true（跟 `Event/Batch` 同一條規則）。
    #[serde(default = "more_when_missing")]
    pub more: bool,
}

/// `Device/ItemsDestroyed` 的 meta：`tc` 抄命令的總數，`bc` 是真的沒了幾則（data 是那 `bc` 個 count）。
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
pub struct ItemsDestroyedMeta {
    pub tc: u32,
    pub bc: u32,
}

fn more_when_missing() -> bool {
    true
}

/// 驗一個 `Device/Batch`：`IS_RESPONSE`、`id` 抄 `Fetch`、`seq` 是這窗的第幾個（從 0 嚴格 +1）。`Error` → `Server`。
pub fn expect_device_batch(
    request: &Pack,
    response: Pack,
    expected_seq: u32,
) -> Result<Pack, SdkError> {
    if response.kind == Kind::Control && response.subtype == control::ERROR {
        return Err(server_error(&response.meta));
    }
    if response.kind != Kind::Device
        || response.subtype != device::BATCH
        || response.flags & flags::IS_RESPONSE == 0
    {
        return Err(SdkError::Protocol(format!(
            "expected a Device/Batch response, got kind {:?} subtype {:#04x} flags {:#04x}",
            response.kind, response.subtype, response.flags
        )));
    }
    if response.id != request.id {
        return Err(SdkError::Protocol(format!(
            "Device/Batch id {} does not echo the Fetch id {}",
            response.id, request.id
        )));
    }
    if response.seq != expected_seq {
        return Err(SdkError::Protocol(format!(
            "Device/Batch seq {} where {expected_seq} was expected",
            response.seq
        )));
    }
    Ok(response)
}

/// 一個 `Device/Batch` → meta 與 `(count, 事件 JSON)`，舊→新。
///
/// Return:
///     Ok((DeviceBatchMeta, Vec<(u64, Value)>))
///     Err(Protocol)   meta 不是這個形狀、長度前綴切不齊、則數跟 `bc` 或 `counts` 對不上、count 不是嚴格遞增、
///                     `ot`／`nt` 不是第一／最後一個 count、`tc < bc + r`、事件不是 JSON 物件
pub fn parse_device_batch(
    batch: &Pack,
) -> Result<(DeviceBatchMeta, Vec<(u64, serde_json::Value)>), SdkError> {
    let meta: DeviceBatchMeta = parse_meta(batch)?;
    let items = split_length_prefixed(&batch.data)?;
    if items.len() as u32 != meta.bc || meta.counts.len() as u32 != meta.bc {
        return Err(SdkError::Protocol(format!(
            "Device/Batch meta says bc {} but data holds {} events and counts has {}",
            meta.bc,
            items.len(),
            meta.counts.len()
        )));
    }
    if meta.tc < meta.bc.saturating_add(meta.r) {
        return Err(SdkError::Protocol(format!(
            "Device/Batch meta inconsistent: tc {} < bc {} + r {}",
            meta.tc, meta.bc, meta.r
        )));
    }
    if meta.bc > 0 {
        // 舊→新、count 嚴格遞增；`ot`／`nt` 就是頭尾。錯一個就是 server 或通道壞了，🚫 不猜。
        if meta.counts.windows(2).any(|pair| pair[0] >= pair[1]) {
            return Err(SdkError::Protocol(format!(
                "Device/Batch counts are not strictly increasing: {:?}",
                meta.counts
            )));
        }
        if meta.counts[0] != meta.ot || meta.counts[meta.counts.len() - 1] != meta.nt {
            return Err(SdkError::Protocol(format!(
                "Device/Batch ot {} / nt {} do not match counts {:?}",
                meta.ot, meta.nt, meta.counts
            )));
        }
    }
    let mut events = Vec::with_capacity(items.len());
    for (count, item) in meta.counts.iter().zip(items) {
        let event: serde_json::Value = serde_json::from_slice(item).map_err(|error| {
            SdkError::Protocol(format!("to-device item {count} is not JSON: {error}"))
        })?;
        if !event.is_object() {
            return Err(SdkError::Protocol(format!(
                "to-device item {count} is not a JSON object"
            )));
        }
        events.push((*count, event));
    }
    Ok((meta, events))
}

/// `ItemsDestroy` 的兩個回應（to-device-client.md §4）：先 `Control/Ack`（只是命令收到），再 `Device/ItemsDestroyed`（真的沒了的那些）。
///
/// Args:
///     request: 送出的 `ItemsDestroy` pack
///     sent_count: 命令裡列了幾則, example: 2
/// Return:
///     Ok(None)         `Ack`：命令收到，🚫 不能據此清待銷毀清單
///     Ok(Some(counts)) `ItemsDestroyed`：這些 count 遠端已經沒有了（可能是這次刪的、也可能本來就不在）
///     Err(Server)      `Error`
///     Err(Protocol)    形狀不對：id／seq 沒抄、`tc` 跟命令不符、data 長度不是 `bc × 8`、回來的 count 不在命令裡
pub fn parse_items_destroyed(
    request: &Pack,
    response: &Pack,
    sent: &[u64],
) -> Result<Option<Vec<u64>>, SdkError> {
    if response.kind == Kind::Control && response.subtype == control::ERROR {
        return Err(server_error(&response.meta));
    }
    // 兩個回覆都抄 `id`；`seq` 只有 `Ack` 抄命令的（`ItemsDestroyed` 自己是一則無序類，server 給 0——
    // 2026-09-21 對真 server 實跑看到的；向量裡命令的 seq 剛好也是 0，看不出來）。
    if response.id != request.id {
        return Err(SdkError::Protocol(format!(
            "ItemsDestroy reply id {} does not echo the command id {}",
            response.id, request.id
        )));
    }
    if response.flags & flags::IS_RESPONSE == 0 {
        return Err(SdkError::Protocol(
            "ItemsDestroy reply without IS_RESPONSE".into(),
        ));
    }
    match (response.kind, response.subtype) {
        (Kind::Control, control::ACK) if response.seq == request.seq => Ok(None),
        (Kind::Control, control::ACK) => Err(SdkError::Protocol(format!(
            "ItemsDestroy Ack seq {} does not echo the command seq {}",
            response.seq, request.seq
        ))),
        (Kind::Device, device::ITEMS_DESTROYED) => {
            let meta: ItemsDestroyedMeta = parse_meta(response)?;
            if meta.tc as usize != sent.len() {
                return Err(SdkError::Protocol(format!(
                    "ItemsDestroyed tc {} but the command listed {}",
                    meta.tc,
                    sent.len()
                )));
            }
            let destroyed = decode_counts(&response.data)?;
            if destroyed.len() as u32 != meta.bc {
                return Err(SdkError::Protocol(format!(
                    "ItemsDestroyed bc {} but data holds {} counts",
                    meta.bc,
                    destroyed.len()
                )));
            }
            // 🚨 消費端再驗一次：只認命令裡列過的。沒列過的 count 回來，是 server 或通道壞了，🚫 不拿它去清清單。
            if let Some(stranger) = destroyed.iter().find(|count| !sent.contains(count)) {
                return Err(SdkError::Protocol(format!(
                    "ItemsDestroyed names count {stranger} which the command did not list"
                )));
            }
            Ok(Some(destroyed))
        }
        (kind, subtype) => Err(SdkError::Protocol(format!(
            "expected Ack or Device/ItemsDestroyed, got kind {kind:?} subtype {subtype:#04x}"
        ))),
    }
}

/// `tc × 8 byte` 的 u64 大端 count 串。
///
/// Return:
///     Ok(Vec<u64>)
///     Err(Protocol)  長度不是 8 的倍數
pub fn decode_counts(data: &[u8]) -> Result<Vec<u64>, SdkError> {
    if !data.len().is_multiple_of(8) {
        return Err(SdkError::Protocol(format!(
            "a count list must be a multiple of 8 bytes, got {}",
            data.len()
        )));
    }
    Ok(data
        .chunks_exact(8)
        .map(|chunk| u64::from_be_bytes(chunk.try_into().expect("chunks_exact(8)")))
        .collect())
}

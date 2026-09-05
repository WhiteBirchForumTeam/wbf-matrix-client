//! 線上規格 §1、§3、§4 的訊息：怎麼組請求 pack、怎麼讀回應。
//!
//! 這裡不碰網路、不碰加密：輸入輸出都是 `Pack` 與 JSON。通道在 `channel`，加密在 `chunk_crypto`。

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use wbf_wire::pack::{control, download, event, flags, upload};
use wbf_wire::{EncryptedFileInfo, Kind, Pack};

use crate::error::SdkError;

/// `Hello` 的 meta（線上規格 §1）。
pub const PROTOCOL_VERSION: u32 = 1;

// ---- 請求 ----

/// Args:
///     client_name: example: "wbf-cli/0.1"
///     seq: 請求號
pub fn hello(client_name: &str, seq: u32) -> Pack {
    let meta =
        serde_json::json!({ "protocol": PROTOCOL_VERSION, "client": client_name, "features": [] });
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
    SdkError::Server {
        code,
        message,
        meta: value,
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

/// `Event/Recent` 的請求 meta。欄位順序就是線上的 JSON 順序（向量檔逐 byte 比），不要重排。
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct RecentRequest {
    /// 這頁最多幾則；server 的 `wbf_recent_max_limit`（預設 10000）以上會被 clamp。
    pub limit: u32,
    /// client 快取裡最新的 `g_seq`；None 或 0 = 沒有快取。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cg_seq: Option<i64>,
    /// 補洞：只要比它舊的（上一頁 Ack 的 `next`）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub before: Option<i64>,
}

/// Args:
///     request: example: RecentRequest { limit: 10000, cg_seq: Some(4700), before: None }
pub fn recent(request: &RecentRequest, seq: u32) -> Pack {
    Pack {
        kind: Kind::Event,
        subtype: event::RECENT,
        flags: 0,
        id: 0,
        seq,
        meta: serde_json::to_vec(request).expect("RecentRequest serializes"),
        data: Vec::new(),
    }
}

/// `Event/Recent` 的 Ack meta。
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct RecentAck {
    pub returned: u32,
    /// server 此刻最新的全域序號，存下來當下次的 `cg_seq`。
    pub latest_g_seq: i64,
    /// false = `cg_seq` 到 `latest_g_seq` 之間有洞，帶同一個 `cg_seq` 加 `before = next` 再問。
    pub complete: bool,
    pub next: Option<i64>,
}

/// `Event/Recent` Ack 的 data：事件的 JSON 陣列，新到舊，每則自帶 `room_id` 與 `unsigned` 的 `r_seq`／`g_seq`。
///
/// Return:
///     Ok(Vec<Value>)    原樣的事件 JSON，這裡不解讀
///     Err(Protocol)     data 不是 JSON 陣列
pub fn parse_recent_events(ack: &Pack) -> Result<Vec<serde_json::Value>, SdkError> {
    if ack.data.is_empty() {
        return Ok(Vec::new());
    }
    serde_json::from_slice(&ack.data)
        .map_err(|error| SdkError::Protocol(format!("Recent ack data: {error}")))
}

/// 從事件 JSON 的 `unsigned` 讀 server 加的序號；沒有就是 None（非 fork server、或舊事件），呼叫者顯式判斷，不猜。
///
/// Args:
///     event: `parse_recent_events` 或 sync 回來的一則事件
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

/// `Event/Send` 的請求 meta（media-attachments.md §3）。`attachments` 是這則訊息用到的 mxc，
/// server 讀不到 E2EE 內容，靠它替媒體 +1；不宣告的媒體過保護期會被清掉（spec §12）。
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct SendRequest {
    pub room_id: String,
    #[serde(rename = "type")]
    pub event_type: String,
    pub txn_id: String,
    pub attachments: Vec<String>,
}

/// Args:
///     request: example: SendRequest { room_id: "!r:localhost".into(), event_type: "m.room.encrypted".into(), txn_id: "t1".into(), attachments: vec!["mxc://localhost/1122334455667788".into()] }
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

/// `Event/Send` 的 Ack meta。server 端還沒實作（提案階段），形狀照 Matrix 的 send 回應猜：`event_id`。
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct SendAck {
    pub event_id: String,
}

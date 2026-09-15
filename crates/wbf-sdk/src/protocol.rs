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

// ---- 橋：pack 帶 flags bit4，server 轉成內部 HTTP 請求交給 Matrix 端點（wbfuwunel #56）----
//
// 🚨 **號碼的權威在 server 的 `docs/bridge-specs/index.md`**（wire-format §3.2 只列原生的）。這裡只抄**用得到的**那幾個，
// 用到一個抄一個，🚫 不整張表搬過來 —— 搬過來的那份不會知道 server 改了。

/// 一支走橋的 Matrix 端點：kind ＋ subtype 決定 method 與路徑模板（server 那邊的白名單）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BridgedEndpoint {
    pub kind: Kind,
    pub subtype: u8,
}

/// `GET /_matrix/client/v3/rooms/{room_id}/event/{event_id}`（bridge-specs `0x14-event.md` §0x20）。
pub const BRIDGE_GET_EVENT: BridgedEndpoint = BridgedEndpoint {
    kind: Kind::Event,
    subtype: 0x20,
};

/// 走橋的請求：`id` 填 0（一個請求一個回應，不開會話）、flags 只有 `IS_BRIDGED`。
///
/// Args:
///     endpoint: example: BRIDGE_GET_EVENT
///     variables: 路徑與 query 變數的 JSON 物件（**欄位順序就是線上的順序**，用 struct 定），example: {"room_id":"!r:x","event_id":"$e"}
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

/// `GetEvent` 的變數：欄位順序照 bridge-specs 的範例。
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct GetEventVariables<'a> {
    pub room_id: &'a str,
    pub event_id: &'a str,
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

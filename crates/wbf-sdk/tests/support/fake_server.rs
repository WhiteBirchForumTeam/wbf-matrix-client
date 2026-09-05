//! 線上規格 §3、§4 的記憶體版 server，實作 `PackChannel`，讓上傳／下載管線不用真 server 就能測。
//! 它照規格拒絕（`OutOfOrder`、`Conflict`、`NotFound`…），並提供故障旋鈕（掉 Ack、竄改一塊）。
//! 它不是權威：與真 server 的差異由 `e2e_local_server.rs` 抓。

use std::collections::HashMap;

use wbf_sdk::{PackChannel, SdkError};
use wbf_wire::pack::{control, download, flags, upload};
use wbf_wire::{EncryptedFileInfo, Kind, Pack};

#[derive(Default)]
struct Upload {
    chunk_size: u32,
    file_size: Option<u64>,
    chunk_count: Option<u32>,
    chunks: Vec<Vec<u8>>,
    finished: bool,
    description: Vec<u8>,
    mxc: String,
}

pub struct Media {
    pub chunk_size: u32,
    pub file_size: Option<u64>,
    pub chunks: Vec<Vec<u8>>,
    pub description: Vec<u8>,
}

#[derive(Default)]
pub struct FakeServer {
    uploads: HashMap<u64, Upload>,
    pub media: HashMap<String, Media>,
    next_id: u64,
    /// 這塊存了但 Ack 不回（回 Network 錯），模擬斷線；只觸發一次。
    pub drop_ack_once_at: Option<(u64, u32)>,
    /// 請求紀錄：(kind, subtype, seq)。
    pub requests: Vec<(Kind, u8, u32)>,
    /// 學 wbfuwunel：`Create` 的回應標頭 `id` 填新發的上傳 id，不是抄請求的 0。
    pub create_ack_header_is_upload_id: bool,
    /// 故障：下一個回應的標頭 id 填這個值（模擬不抄回的 server），只觸發一次。
    pub wrong_response_id_once: Option<u64>,
}

impl FakeServer {
    pub fn new() -> FakeServer {
        FakeServer {
            next_id: 0x1122_3344_5566_7788,
            ..Default::default()
        }
    }

    pub fn handle(&mut self, request: &Pack) -> Pack {
        self.requests
            .push((request.kind, request.subtype, request.seq));
        let result = match (request.kind, request.subtype) {
            (Kind::Control, control::HELLO) => Ok((
                serde_json::json!({ "protocol": 1, "server": "fake", "features": ["upload", "download"],
                    "chunk_size_default": 65536, "chunk_size_large": 1048576, "data_max_bytes": 16781312 }),
                Vec::new(),
            )),
            (Kind::Control, control::PING) => {
                return Pack {
                    kind: Kind::Control,
                    subtype: control::PONG,
                    flags: flags::IS_RESPONSE,
                    id: request.id,
                    seq: request.seq,
                    meta: request.meta.clone(),
                    data: Vec::new(),
                }
            }
            (Kind::Upload, upload::CREATE) => self.create(request),
            (Kind::Upload, upload::CHUNK) => self.chunk(request),
            (Kind::Upload, upload::STATUS) => self.status(request),
            (Kind::Upload, upload::SEAL) => self.seal(request),
            (Kind::Upload, upload::ABORT) => {
                self.uploads.remove(&request.id);
                Ok((serde_json::json!({ "ok": true }), Vec::new()))
            }
            (Kind::Download, download::INFO) => self.info(request),
            (Kind::Download, download::READ) => self.read(request),
            _ => Err((
                "UnknownKind",
                "no such kind/subtype".to_string(),
                serde_json::json!({}),
            )),
        };
        let is_create = request.kind == Kind::Upload && request.subtype == upload::CREATE;
        let mut response_id = request.id;
        if let Some(wrong) = self.wrong_response_id_once.take() {
            response_id = wrong;
        } else if is_create && self.create_ack_header_is_upload_id {
            response_id = self.next_id;
        }
        match result {
            Ok((meta, data)) => Pack {
                kind: Kind::Control,
                subtype: control::ACK,
                flags: flags::IS_RESPONSE,
                id: response_id,
                seq: request.seq,
                meta: meta.to_string().into_bytes(),
                data,
            },
            Err((code, message, mut extra)) => {
                extra["code"] = serde_json::Value::String(code.to_string());
                extra["message"] = serde_json::Value::String(message);
                Pack {
                    kind: Kind::Control,
                    subtype: control::ERROR,
                    flags: flags::IS_RESPONSE,
                    id: request.id,
                    seq: request.seq,
                    meta: extra.to_string().into_bytes(),
                    data: Vec::new(),
                }
            }
        }
    }

    fn create(
        &mut self,
        request: &Pack,
    ) -> Result<(serde_json::Value, Vec<u8>), (&'static str, String, serde_json::Value)> {
        let info = EncryptedFileInfo::from_bytes(&request.meta).ok_or((
            "Conflict",
            "meta_len != 16".to_string(),
            serde_json::json!({}),
        ))?;
        if (info.file_size == 0) != (info.chunk_count == 0) {
            return Err((
                "Conflict",
                "streaming sentinel needs both zero".into(),
                serde_json::json!({}),
            ));
        }
        let chunk_size = if info.chunk_size == 0 {
            65536
        } else {
            info.chunk_size
        };
        if !info.is_streaming()
            && info.chunk_count != info.file_size.div_ceil(u64::from(chunk_size)) as u32
        {
            return Err((
                "Conflict",
                "chunk_count mismatch".into(),
                serde_json::json!({}),
            ));
        }
        if request.data.len() > 65536 {
            return Err(("TooLarge", "description".into(), serde_json::json!({})));
        }
        self.next_id += 1;
        let id = self.next_id;
        let mxc = format!("mxc://fake/{id:016x}");
        self.uploads.insert(
            id,
            Upload {
                chunk_size,
                file_size: (!info.is_streaming()).then_some(info.file_size),
                chunk_count: (!info.is_streaming()).then_some(info.chunk_count),
                description: request.data.clone(),
                mxc: mxc.clone(),
                ..Default::default()
            },
        );
        Ok((
            serde_json::json!({ "id": id, "mxc": mxc, "chunk_size": chunk_size,
                "chunk_max_bytes": u64::from(chunk_size) + 4096, "expires_at": 1_800_000_000u64 }),
            Vec::new(),
        ))
    }

    fn chunk(
        &mut self,
        request: &Pack,
    ) -> Result<(serde_json::Value, Vec<u8>), (&'static str, String, serde_json::Value)> {
        let upload = self.uploads.get_mut(&request.id).ok_or((
            "NotFound",
            "no upload".to_string(),
            serde_json::json!({}),
        ))?;
        let received = upload.chunks.len() as u32;
        let is_last = request.flags & flags::IS_LAST != 0;
        if request.data.is_empty() {
            return Err(("Conflict", "empty chunk".into(), serde_json::json!({})));
        }
        if request.data.len() as u64 > u64::from(upload.chunk_size) + 4096 {
            return Err(("TooLarge", "chunk".into(), serde_json::json!({})));
        }
        if request.seq > received {
            return Err((
                "OutOfOrder",
                "gap".into(),
                serde_json::json!({ "expected_seq": received }),
            ));
        }
        if request.seq < received {
            return Ok((chunk_ack(upload), Vec::new()));
        }
        if upload.finished {
            return Err(("Conflict", "already finished".into(), serde_json::json!({})));
        }
        if let Some(count) = upload.chunk_count {
            if request.seq >= count {
                return Err((
                    "Conflict",
                    "seq beyond chunk_count".into(),
                    serde_json::json!({}),
                ));
            }
            if is_last && request.seq + 1 != count {
                return Err((
                    "Conflict",
                    "IS_LAST on wrong chunk".into(),
                    serde_json::json!({}),
                ));
            }
            if request.seq + 1 == count {
                upload.finished = true;
            }
        } else if is_last {
            upload.finished = true;
        }
        upload.chunks.push(request.data.clone());
        let ack = chunk_ack(upload);
        if self.drop_ack_once_at == Some((request.id, request.seq)) {
            self.drop_ack_once_at = None;
            return Err((
                "__drop__",
                "simulated disconnect".into(),
                serde_json::json!({}),
            ));
        }
        Ok((ack, Vec::new()))
    }

    fn status(
        &mut self,
        request: &Pack,
    ) -> Result<(serde_json::Value, Vec<u8>), (&'static str, String, serde_json::Value)> {
        let upload = self.uploads.get(&request.id).ok_or((
            "NotFound",
            "no upload".to_string(),
            serde_json::json!({}),
        ))?;
        let mut ack = chunk_ack(upload);
        ack["chunk_size"] = upload.chunk_size.into();
        ack["file_size"] = upload.file_size.into();
        Ok((ack, Vec::new()))
    }

    fn seal(
        &mut self,
        request: &Pack,
    ) -> Result<(serde_json::Value, Vec<u8>), (&'static str, String, serde_json::Value)> {
        let upload = self.uploads.get(&request.id).ok_or((
            "NotFound",
            "no upload".to_string(),
            serde_json::json!({}),
        ))?;
        if !upload.finished {
            return Err(("Conflict", "not finished".into(), serde_json::json!({})));
        }
        let upload = self.uploads.remove(&request.id).expect("checked");
        let description = if request.data.is_empty() {
            upload.description
        } else {
            request.data.clone()
        };
        let mxc = upload.mxc.clone();
        self.media.insert(
            mxc.clone(),
            Media {
                chunk_size: upload.chunk_size,
                file_size: upload.file_size,
                chunks: upload.chunks,
                description,
            },
        );
        Ok((serde_json::json!({ "mxc": mxc }), Vec::new()))
    }

    fn info(
        &mut self,
        request: &Pack,
    ) -> Result<(serde_json::Value, Vec<u8>), (&'static str, String, serde_json::Value)> {
        let mxc = mxc_of(&request.meta)?;
        let media = self.media.get(&mxc).ok_or((
            "NotFound",
            "no media".to_string(),
            serde_json::json!({}),
        ))?;
        let total_len: usize = media.chunks.iter().map(Vec::len).sum();
        Ok((
            serde_json::json!({ "total_len": total_len, "file_size": media.file_size, "chunk_size": media.chunk_size,
                "chunk_count": media.chunks.len(), "truncated": false, "content_type": null, "read_len": 65536,
                "chunk_size_large": 1048576 }),
            media.description.clone(),
        ))
    }

    fn read(
        &mut self,
        request: &Pack,
    ) -> Result<(serde_json::Value, Vec<u8>), (&'static str, String, serde_json::Value)> {
        let mxc = mxc_of(&request.meta)?;
        let meta: serde_json::Value = serde_json::from_slice(&request.meta).expect("json");
        let media = self.media.get(&mxc).ok_or((
            "NotFound",
            "no media".to_string(),
            serde_json::json!({}),
        ))?;
        let index = meta["chunk"].as_u64().ok_or((
            "Conflict",
            "no chunk".to_string(),
            serde_json::json!({}),
        ))? as usize;
        if index >= media.chunks.len() {
            return Err((
                "Conflict",
                "chunk beyond count".into(),
                serde_json::json!({}),
            ));
        }
        let data = media.chunks[index].clone();
        let total_len: usize = media.chunks.iter().map(Vec::len).sum();
        Ok((
            serde_json::json!({ "chunk": index, "pos": index as u64 * u64::from(media.chunk_size), "len": data.len(),
                "chunk_size": media.chunk_size, "chunk_count": media.chunks.len(), "total_len": total_len }),
            data,
        ))
    }
}

fn chunk_ack(upload: &Upload) -> serde_json::Value {
    let total_len: usize = upload.chunks.iter().map(Vec::len).sum();
    serde_json::json!({ "received": upload.chunks.len(), "chunk_count": upload.chunk_count,
        "total_len": total_len, "finished": upload.finished, "truncated": false })
}

fn mxc_of(meta: &[u8]) -> Result<String, (&'static str, String, serde_json::Value)> {
    let value: serde_json::Value = serde_json::from_slice(meta).map_err(|_| {
        (
            "Conflict",
            "meta not json".to_string(),
            serde_json::json!({}),
        )
    })?;
    value["mxc"].as_str().map(str::to_string).ok_or((
        "Conflict",
        "no mxc".to_string(),
        serde_json::json!({}),
    ))
}

impl PackChannel for FakeServer {
    async fn request(&mut self, pack: Pack) -> Result<Pack, SdkError> {
        // 走一次真正的編碼與解碼，pack 層的問題也會在這裡冒出來。
        let bytes = pack.encode()?;
        let decoded = Pack::decode(&bytes)?;
        let response = self.handle(&decoded);
        if response.subtype == control::ERROR && response.meta.starts_with(br#"{"code":"__drop__""#)
        {
            return Err(SdkError::Network("simulated disconnect".into()));
        }
        Ok(Pack::decode(&response.encode()?)?)
    }
}

/// 同一個 server 給多個 client 用：`&mut FakeServer` 也是通道。
impl PackChannel for &mut FakeServer {
    async fn request(&mut self, pack: Pack) -> Result<Pack, SdkError> {
        (**self).request(pack).await
    }
}

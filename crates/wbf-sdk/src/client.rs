//! `WbfClient`：一條通道加一個請求號計數器。每個命令一個方法；上傳在 `upload.rs`、下載在 `download.rs`
//! 的 `impl` 區塊裡，這裡只有共用的 `call` 與小命令。

use wbf_wire::Pack;

use crate::channel::PackChannel;
use crate::error::SdkError;
use crate::protocol::{
    self, HelloAck, InfoAck, ReadAck, RecentAck, RecentRequest, SendAck, SendRequest, StatusAck,
};

pub struct WbfClient<C: PackChannel> {
    channel: C,
    next_seq: u32,
    /// `hello()` 回的 `features`；None = 還沒問過。需要 feature 的命令（`recent`、`send_event`）用它把關，
    /// 不靠呼叫者記得看 docstring。
    features: Option<Vec<String>>,
}

impl<C: PackChannel> WbfClient<C> {
    pub fn new(channel: C) -> WbfClient<C> {
        WbfClient {
            channel,
            next_seq: 1,
            features: None,
        }
    }

    /// 發一個請求號、送出、驗回應是 Ack。所有非 `Chunk` 的請求都走這裡（`Chunk` 的 seq 是塊索引，見 `upload.rs`）。
    ///
    /// Args:
    ///     build: 拿請求號組 pack, example: |seq| protocol::status(upload_id, seq)
    /// Return:
    ///     Ok(Pack)          Ack
    ///     Err(SdkError)     `Server`、`Network`、`Protocol`、`Integrity`
    pub(crate) async fn call(&mut self, build: impl FnOnce(u32) -> Pack) -> Result<Pack, SdkError> {
        let seq = self.next_seq;
        self.next_seq = self.next_seq.wrapping_add(1);
        let request = build(seq);
        self.send_and_expect_ack(request).await
    }

    pub(crate) async fn send_and_expect_ack(&mut self, request: Pack) -> Result<Pack, SdkError> {
        let response = self.channel.request(request.clone()).await?;
        protocol::expect_ack(&request, response)
    }

    /// Args:
    ///     client_name: example: "wbf-cli/0.1"
    pub async fn hello(&mut self, client_name: &str) -> Result<HelloAck, SdkError> {
        let ack = self.call(|seq| protocol::hello(client_name, seq)).await?;
        let hello: HelloAck = protocol::parse_meta(&ack)?;
        self.features = Some(hello.features.clone());
        Ok(hello)
    }

    /// Return:
    ///     Option<&[String]>  `hello()` 回的 features；None = 還沒問過
    pub fn features(&self) -> Option<&[String]> {
        self.features.as_deref()
    }

    /// Args:
    ///     name: example: "recent"
    /// Return:
    ///     bool  1 = 問過且 server 宣告了這個 feature；沒問過一律 0（fail closed）
    pub fn has_feature(&self, name: &str) -> bool {
        self.features
            .as_ref()
            .is_some_and(|features| features.iter().any(|feature| feature == name))
    }

    /// 需要 feature 的命令先過這關：沒問過 `hello()` 或 server 沒宣告，都是 `Usage`，不送。
    fn require_feature(&self, name: &str) -> Result<(), SdkError> {
        match &self.features {
            None => Err(SdkError::Usage(format!(
                "call hello() before using `{name}`: the server's features are not known yet"
            ))),
            Some(features) if features.iter().any(|feature| feature == name) => Ok(()),
            Some(_) => Err(SdkError::Usage(format!(
                "this server does not advertise the `{name}` feature"
            ))),
        }
    }

    pub async fn ping(&mut self) -> Result<(), SdkError> {
        self.call(protocol::ping).await?;
        Ok(())
    }

    pub async fn upload_status(&mut self, upload_id: u64) -> Result<StatusAck, SdkError> {
        let ack = self.call(|seq| protocol::status(upload_id, seq)).await?;
        protocol::parse_meta(&ack)
    }

    pub async fn abort_upload(&mut self, upload_id: u64) -> Result<(), SdkError> {
        self.call(|seq| protocol::abort(upload_id, seq)).await?;
        Ok(())
    }

    /// 線上規格 §4.1。
    ///
    /// Return:
    ///     Ok((InfoAck, Vec<u8>))   meta 與 data（server 存的那份描述，原樣）
    pub async fn fetch_info(&mut self, mxc: &str) -> Result<(InfoAck, Vec<u8>), SdkError> {
        let ack = self.call(|seq| protocol::info(mxc, seq)).await?;
        let info = protocol::parse_meta(&ack)?;
        Ok((info, ack.data))
    }

    /// 線上規格 §4.2：整整一塊，照上傳時的 bytes。這裡只驗 `len` 與 data 長度一致；解密與長度規則在下載端。
    pub async fn read_chunk(
        &mut self,
        mxc: &str,
        index: u32,
    ) -> Result<(ReadAck, Vec<u8>), SdkError> {
        let ack = self
            .call(|seq| protocol::read_chunk(mxc, index, seq))
            .await?;
        let read: ReadAck = protocol::parse_meta(&ack)?;
        if read.len != ack.data.len() as u64 {
            return Err(SdkError::Protocol(format!(
                "Read ack says len {} but data is {} bytes",
                read.len,
                ack.data.len()
            )));
        }
        if read.chunk != index {
            return Err(SdkError::Protocol(format!(
                "asked for chunk {index}, got {}",
                read.chunk
            )));
        }
        Ok((read, ack.data))
    }

    /// `Event/Recent`：跨房間、在 `cg_seq` 之後的事件，新到舊（server 的 room-seq-and-recent.md §2）。
    /// `Hello.features` 有 `recent` 才能用。
    ///
    /// Args:
    ///     request: example: RecentRequest { limit: 10000, cg_seq: Some(4700), before: None }
    /// Return:
    ///     Ok((RecentAck, Vec<Value>))   meta 與事件陣列；`complete` 是 false 就帶 `before = next` 再問
    pub async fn recent(
        &mut self,
        request: &RecentRequest,
    ) -> Result<(RecentAck, Vec<serde_json::Value>), SdkError> {
        self.require_feature("recent")?;
        let ack = self.call(|seq| protocol::recent(request, seq)).await?;
        let meta: RecentAck = protocol::parse_meta(&ack)?;
        let events = protocol::parse_recent_events(&ack)?;
        if events.len() as u32 != meta.returned {
            return Err(SdkError::Protocol(format!(
                "Recent ack says returned {} but data has {} events",
                meta.returned,
                events.len()
            )));
        }
        Ok((meta, events))
    }

    /// `Event/Send`：送事件並宣告附件（media-attachments.md §3、spec §12）。
    /// ⚠️ server 端還是提案（2026-09-06），`Hello.features` 有 `attachments` 才能用；沒有就走 HTTP 加 `X-Wbf-Attachments`。
    ///
    /// Args:
    ///     request: room、type、txn_id、這則用到的 mxc
    ///     content: 事件 content 的 JSON bytes
    /// Return:
    ///     Ok(SendAck)       server 收下的 event_id
    ///     Err(Server)       `Conflict`：某個 mxc 不是本站的、找不到、不是 sender 傳的、或有墓碑；整則沒送
    pub async fn send_event(
        &mut self,
        request: &SendRequest,
        content: Vec<u8>,
    ) -> Result<SendAck, SdkError> {
        self.require_feature("attachments")?;
        let ack = self
            .call(|seq| protocol::send_event(request, content, seq))
            .await?;
        protocol::parse_meta(&ack)
    }
}

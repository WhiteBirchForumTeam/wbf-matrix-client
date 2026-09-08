//! `WbfClient`：一條通道加一個請求號計數器。每個命令一個方法；上傳在 `upload.rs`、下載在 `download.rs`
//! 的 `impl` 區塊裡，這裡只有共用的 `call` 與小命令。

use wbf_wire::Pack;

use crate::channel::PackChannel;
use crate::error::SdkError;
use crate::protocol::{
    self, BatchMeta, HelloAck, InfoAck, ReadAck, RecentRequest, SendAck, SendRequest, StatusAck,
};

pub struct WbfClient<C: PackChannel> {
    channel: C,
    next_seq: u32,
    /// `hello()` 整份回應；`recent_sync` 用它的 `recent_max_*` clamp。
    hello: Option<HelloAck>,
    /// `Recent` 這種一串回應的請求用的 id，client 自己選、從 1 起。
    next_stream_id: u64,
    /// `hello()` 回的 `features`；None = 還沒問過。需要 feature 的命令（`recent`、`send_event`）用它把關，
    /// 不靠呼叫者記得看 docstring。
    features: Option<Vec<String>>,
}

/// `recent_window`／`recent_sync` 每收到一個 Batch 叫一次：meta 加這批的事件（新到舊）。回 `Err` 就中止。
pub type OnBatch<'a> =
    &'a mut dyn FnMut(&BatchMeta, Vec<serde_json::Value>) -> Result<(), SdkError>;

/// `Recent` 一窗的結果。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RecentWindow {
    pub tc: u32,
    pub batches: u32,
    pub events: u64,
    /// 第一個 Batch 的 `fs`（這窗最新的一則）；空窗 None。
    pub first_fs: Option<i64>,
    /// 最後一個非空 Batch 的 `ls`；下一窗的 `before`。
    pub last_ls: Option<i64>,
}

/// `recent_sync` 整輪的結果。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RecentSync {
    /// 追平後該存的新水位（第一窗第一個 Batch 的 `fs`）；整輪沒有事件就 None，水位不動。
    pub new_cg_seq: Option<i64>,
    pub windows: u32,
    pub events: u64,
    pub last_ls: Option<i64>,
}

/// client 約定的等待上限（維護者定）：第一窗 server 要合併＋數，給 60 秒；之後的窗 10 秒。
pub const RECENT_FIRST_WINDOW_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
pub const RECENT_NEXT_WINDOW_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

impl<C: PackChannel> WbfClient<C> {
    pub fn new(channel: C) -> WbfClient<C> {
        WbfClient {
            channel,
            next_seq: 1,
            features: None,
            hello: None,
            next_stream_id: 0,
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
        self.hello = Some(hello.clone());
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

    /// `Event/Recent` 的**一窗**（pack-pipeline §6）：送請求、收一串 `Event/Batch` 直到 `r = 0`。
    /// 每個 Batch 交給 `on_batch`（新到舊）；`Hello.features` 有 `recent` 才能用。
    ///
    /// Args:
    ///     request: example: RecentRequest { limit: 320, cg_seq: Some(4700), before: None, batch: Some(10) }
    ///     per_pack_timeout: 兩個 Batch 之間最多等多久（第一窗 60 秒、之後 10 秒是 `recent_sync` 的約定）
    /// Return:
    ///     Ok(RecentWindow)   這窗的 `tc`、幾個 Batch、第一個 Batch 的 `fs`、最後一個的 `ls`
    ///     Err(Server)        server 拒絕（走 HTTP 是 `Unsupported`）
    ///     Err(Protocol)      不是 Batch、id／seq 對不上、data 切不齊、meta 不變量不成立
    ///     Err(Network)       逾時或斷線；`on_batch` 已收到的照樣有效，呼叫者從最後的 `ls` 續
    pub async fn recent_window(
        &mut self,
        request: &RecentRequest,
        per_pack_timeout: std::time::Duration,
        on_batch: OnBatch<'_>,
    ) -> Result<RecentWindow, SdkError> {
        self.require_feature("recent")?;
        let seq = self.next_seq;
        self.next_seq = self.next_seq.wrapping_add(1);
        // id 由 client 選（回應是一串 Batch，不能靠 seq 對）；從 1 起，永遠不是 0。
        self.next_stream_id = self.next_stream_id.wrapping_add(1).max(1);
        let pack = protocol::recent(request, self.next_stream_id, seq);
        let mut window = RecentWindow::default();
        let mut expected_seq = 0u32;
        let mut sent = 0u32;
        let mut finished = false;
        let mut on_pack = |response: Pack| -> Result<bool, SdkError> {
            let batch = protocol::expect_batch(&pack, response, expected_seq)?;
            let (meta, events) = protocol::parse_batch(&batch)?;
            if expected_seq == 0 {
                window.tc = meta.tc;
                window.first_fs = (meta.bc > 0).then_some(meta.fs);
            } else if meta.tc != window.tc {
                return Err(SdkError::Protocol(format!(
                    "Batch {expected_seq} says tc {} but the window started with tc {}",
                    meta.tc, window.tc
                )));
            }
            sent = sent.saturating_add(meta.bc);
            if window.tc != sent.saturating_add(meta.r) {
                return Err(SdkError::Protocol(format!(
                    "Batch {expected_seq}: tc {} ≠ sent {sent} + r {}",
                    window.tc, meta.r
                )));
            }
            if meta.bc > 0 {
                window.last_ls = Some(meta.ls);
            }
            window.batches += 1;
            window.events += u64::from(meta.bc);
            expected_seq += 1;
            let more = meta.r > 0;
            finished = !more;
            on_batch(&meta, events)?;
            Ok(more)
        };
        self.channel
            .request_stream(pack.clone(), per_pack_timeout, &mut on_pack)
            .await?;
        if !finished {
            return Err(SdkError::Protocol(
                "the channel ended the Recent window before a Batch with r = 0".into(),
            ));
        }
        Ok(window)
    }

    /// 整輪同步（pack-pipeline §6.4 的水位規則）：從 `cg_seq` 起一窗一窗拉到追平。
    /// - 一窗收完且 `tc < limit`：追平，`new_cg_seq` 是**第一窗第一個 Batch 的 `fs`**（這輪最新的一則）。
    /// - `tc == limit`：可能還有更舊的，帶 `before = 最後的 ls` 再一窗，水位不動。
    /// - 中途錯誤：原樣回；已交給 `on_batch` 的事件有效，`new_cg_seq` 不會給（呼叫者不推水位，下次重來會拿到同樣的）。
    ///
    /// 逾時：第一窗每個 Batch 之間 `RECENT_FIRST_WINDOW_TIMEOUT`，之後 `RECENT_NEXT_WINDOW_TIMEOUT`（維護者定）。
    ///
    /// Args:
    ///     cg_seq: 快取的水位線，example: Some(4700)
    ///     limit: 一窗幾則，example: 320（會 clamp 到 server 的上限）
    ///     batch: 每 Batch 幾則，example: Some(10)
    /// Return:
    ///     Ok(RecentSync)    `new_cg_seq` 是 None 表示這輪一則都沒有（水位維持原樣）
    pub async fn recent_sync(
        &mut self,
        cg_seq: Option<i64>,
        limit: u32,
        batch: Option<u32>,
        on_batch: OnBatch<'_>,
    ) -> Result<RecentSync, SdkError> {
        let max_limit = self
            .hello
            .as_ref()
            .and_then(|hello| hello.recent_max_limit)
            .unwrap_or(protocol::RECENT_MAX_LIMIT);
        let max_batch = self
            .hello
            .as_ref()
            .and_then(|hello| hello.recent_max_batch)
            .unwrap_or(protocol::RECENT_MAX_BATCH);
        let limit = limit.clamp(1, max_limit);
        let batch = batch.map(|batch| batch.clamp(1, max_batch));
        let mut request = RecentRequest {
            limit,
            cg_seq: cg_seq.filter(|seq| *seq > 0),
            before: None,
            batch,
        };
        let mut summary = RecentSync::default();
        loop {
            let timeout = if summary.windows == 0 {
                RECENT_FIRST_WINDOW_TIMEOUT
            } else {
                RECENT_NEXT_WINDOW_TIMEOUT
            };
            let window = self.recent_window(&request, timeout, on_batch).await?;
            if summary.windows == 0 {
                summary.new_cg_seq = window.first_fs;
            }
            summary.windows += 1;
            summary.events += window.events;
            summary.last_ls = window.last_ls.or(summary.last_ls);
            if window.tc < limit {
                break;
            }
            let Some(last_ls) = window.last_ls else {
                break;
            };
            request.before = Some(last_ls);
        }
        Ok(summary)
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

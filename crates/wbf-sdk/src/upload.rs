//! 上傳：`Create` → 逐塊 → `Seal`（線上規格 §3、約定 §4、§6）。固定大小可續傳，串流不行。
//!
//! 三步分開，中間的 `UploadState` 由呼叫者落地（CLI 規格 §6）：SDK 不碰檔案系統，
//! 所以「殺掉再跑一次接著送」是 CLI 拿狀態檔呼叫 `send_chunks(from)` 的事，這裡只保證每一步各自正確。

use std::io::{Read, Seek, SeekFrom};

use sha2::{Digest, Sha256};
use wbf_wire::EncryptedFileInfo;

use crate::channel::PackChannel;
use crate::chunk_block::ChunkedBlock;
use crate::chunk_crypto::{chunk_count, expected_plain_len, DescriptionSlot, FileCipher};
use crate::client::WbfClient;
use crate::error::SdkError;
use crate::manifest::{Manifest, UploadState};
use crate::protocol::{self, ChunkAck, CreateAck, SealAck};

/// 進度回呼：(已送的塊數, 總塊數；串流模式 None)。
pub type ProgressFn<'a> = &'a mut dyn FnMut(u32, Option<u32>);

/// `send_chunks`／`send_stream` 回的事實。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SentSummary {
    pub chunks_sent: u32,
    pub file_size: u64,
    /// 整檔明文 SHA-256 十六進位小寫；固定大小上傳只在 `hash` 要求時算。
    pub sha256: Option<String>,
    pub truncated: bool,
}

impl<C: PackChannel> WbfClient<C> {
    /// 線上規格 §3.1。`block.file_size` 有 → 固定大小；None → 串流（`0/0` 哨兵）。
    /// 描述用 `Create` 的 nonce 加密（約定 §4）。
    ///
    /// Args:
    ///     user_id: 寫進狀態檔，續傳時核對, example: "@alice:localhost"
    ///     file_cipher: `FileCipher::generate(...)`
    ///     block: 描述的內容（`name`／`mimetype`；固定大小要有 `file_size`），`key` 不看
    /// Return:
    ///     Ok(UploadState)   呼叫者立刻落地
    ///     Err(SdkError)     `Usage`（空檔、chunk_size 對不上）、`Server`、`Network`
    pub async fn create_upload(
        &mut self,
        server: &str,
        user_id: &str,
        file_cipher: &FileCipher,
        block: &ChunkedBlock,
    ) -> Result<UploadState, SdkError> {
        let mut block = block.clone();
        // 參數以 file_cipher 為準；描述只是它的副本。
        let template = file_cipher.to_event_block(0);
        block.v = template.v;
        block.cipher = template.cipher;
        block.key = template.key;
        block.nonce_base = template.nonce_base;
        block.chunk_size = template.chunk_size;

        let info = match block.file_size {
            Some(0) => {
                return Err(SdkError::Usage(
                    "empty file: the protocol has no zero-chunk upload".into(),
                ))
            }
            Some(file_size) => EncryptedFileInfo {
                file_size,
                chunk_size: block.chunk_size,
                chunk_count: chunk_count(file_size, block.chunk_size).ok_or_else(|| {
                    SdkError::Usage("file too large for u32 chunk indices".into())
                })?,
            },
            None => EncryptedFileInfo {
                file_size: 0,
                chunk_size: block.chunk_size,
                chunk_count: 0,
            },
        };
        let description =
            file_cipher.seal_description(DescriptionSlot::Create, &block.to_description_json());
        let ack = self
            .call(|seq| protocol::create(info, description, seq))
            .await?;
        let created: CreateAck = protocol::parse_meta(&ack)?;
        // 標頭 id 要嘛抄回 0（線上規格 §2），要嘛就是新發的上傳 id（wbfuwunel 的做法）；其他值是對方講錯話。
        if ack.id != 0 && ack.id != created.id {
            return Err(SdkError::Protocol(format!(
                "Create ack header id {} is neither 0 nor the new upload id {}",
                ack.id, created.id
            )));
        }
        if created.chunk_size != block.chunk_size {
            // 我們從不送 0，所以 server 回的必須就是我們給的；不一樣就是對方換了規則。
            return Err(SdkError::Protocol(format!(
                "server chose chunk_size {} but we asked for {}",
                created.chunk_size, block.chunk_size
            )));
        }
        Ok(UploadState {
            server: server.trim_end_matches('/').to_string(),
            user_id: user_id.to_string(),
            upload_id: created.id,
            mxc: created.mxc,
            chunk_max_bytes: created.chunk_max_bytes,
            block,
        })
    }

    /// 固定大小：從第 `from_chunk` 塊送到最後一塊（續傳就是 `from_chunk = Status.received`）。
    /// 每塊從 `source` 的 `i × chunk_size` 讀，用同一把 key 與 nonce_base（同一個上傳，不是重傳）。
    ///
    /// Args:
    ///     from_chunk: example: 0
    ///     hash: 1 = 整檔（從第 0 塊）再讀一遍算 SHA-256；續傳時也是整檔算
    /// Return:
    ///     Ok(SentSummary)
    ///     Err(SdkError)     `Server`（含 `Truncated`）、`Network`、`Io`
    pub async fn send_chunks<R: Read + Seek>(
        &mut self,
        state: &UploadState,
        source: &mut R,
        from_chunk: u32,
        hash: bool,
        on_progress: ProgressFn<'_>,
    ) -> Result<SentSummary, SdkError> {
        let file_size = state.block.file_size.ok_or_else(|| {
            SdkError::Usage("send_chunks needs file_size; use send_stream for unknown size".into())
        })?;
        let chunk_size = state.block.chunk_size;
        let total = chunk_count(file_size, chunk_size)
            .ok_or_else(|| SdkError::Usage("file too large for u32 chunk indices".into()))?;
        if from_chunk > total {
            return Err(SdkError::Usage(format!(
                "resume from chunk {from_chunk} but there are only {total}"
            )));
        }
        let file_cipher = state.file_cipher()?;

        let mut index = from_chunk;
        let mut buffer = vec![0u8; chunk_size as usize];
        let mut truncated = false;
        while index < total {
            let plain_len =
                expected_plain_len(file_size, chunk_size, index).expect("index < total");
            source.seek(SeekFrom::Start(u64::from(index) * u64::from(chunk_size)))?;
            source.read_exact(&mut buffer[..plain_len])?;
            let sealed = file_cipher.seal_chunk(index, &buffer[..plain_len])?;
            let is_last = index + 1 == total;
            match self
                .send_one_chunk(state.upload_id, index, sealed, is_last)
                .await
            {
                Ok(ack) => {
                    truncated = ack.truncated;
                    index = ack.received;
                }
                // 線上規格 §3.2：server 說該送哪塊就跳去哪塊（漏了 Ack 的重送、或狀態檔比 server 舊）。
                Err(SdkError::Server { code, meta, .. }) if code == "OutOfOrder" => {
                    let expected = meta
                        .get("expected_seq")
                        .and_then(|value| value.as_u64())
                        .ok_or_else(|| {
                            SdkError::Protocol("OutOfOrder without expected_seq".into())
                        })?;
                    // 往前或往後跳都可能（server 收過更多／更少）；要的就是我們剛送的那塊，才是死循環。
                    if expected == u64::from(index) || expected > u64::from(total) {
                        return Err(SdkError::Protocol(format!(
                            "server expects chunk {expected} but rejected chunk {index} of {total} as out of order"
                        )));
                    }
                    index = expected as u32;
                }
                Err(other) => return Err(other),
            }
            on_progress(index, Some(total));
        }

        let sha256 = if hash {
            Some(hash_reader(source, file_size)?)
        } else {
            None
        };
        Ok(SentSummary {
            chunks_sent: total,
            file_size,
            sha256,
            truncated,
        })
    }

    /// 串流：從 `reader` 讀到 EOF，每滿一塊送一塊，最後一塊帶 `IS_LAST`（線上規格 §3.2、約定 §6）。
    /// 一律算 SHA-256（反正要讀過一遍）。空輸入是 `Usage` 錯：協議沒有零塊的上傳。
    ///
    /// Return:
    ///     Ok(SentSummary)   `sha256` 一定有
    pub async fn send_stream<R: Read>(
        &mut self,
        state: &UploadState,
        reader: &mut R,
        on_progress: ProgressFn<'_>,
    ) -> Result<SentSummary, SdkError> {
        if state.block.file_size.is_some() {
            return Err(SdkError::Usage(
                "send_stream is for uploads created without file_size".into(),
            ));
        }
        let chunk_size = state.block.chunk_size as usize;
        let file_cipher = state.file_cipher()?;
        let mut hasher = Sha256::new();
        let mut file_size = 0u64;
        let mut index = 0u32;

        // 要知道「這是最後一塊」得先看下一塊有沒有東西，所以永遠留一塊在手上。
        let mut pending = read_up_to(reader, chunk_size)?;
        if pending.is_empty() {
            return Err(SdkError::Usage(
                "empty stream: the protocol has no zero-chunk upload".into(),
            ));
        }
        let truncated = loop {
            let next = if pending.len() < chunk_size {
                Vec::new()
            } else {
                read_up_to(reader, chunk_size)?
            };
            let is_last = next.is_empty();
            hasher.update(&pending);
            file_size += pending.len() as u64;
            let sealed = file_cipher.seal_chunk(index, &pending)?;
            let ack = self
                .send_one_chunk(state.upload_id, index, sealed, is_last)
                .await?;
            index += 1;
            on_progress(index, None);
            if is_last || ack.finished {
                break ack.truncated;
            }
            pending = next;
        };
        Ok(SentSummary {
            chunks_sent: index,
            file_size,
            sha256: Some(hex::encode(hasher.finalize())),
            truncated,
        })
    }

    /// `Seal` 帶最終描述（約定 §4：一律帶，整份覆蓋），回 manifest。
    ///
    /// Args:
    ///     final_block: `state.block` 填上 `file_size`／`sha256` 之後的
    /// Return:
    ///     Ok(Manifest)      `block` 含 key，是機密
    pub async fn seal_upload(
        &mut self,
        state: &UploadState,
        final_block: &ChunkedBlock,
    ) -> Result<Manifest, SdkError> {
        final_block.check_as_event_block()?;
        let file_cipher = state.file_cipher()?;
        let description =
            file_cipher.seal_description(DescriptionSlot::Seal, &final_block.to_description_json());
        let ack = self
            .call(|seq| protocol::seal(state.upload_id, description, seq))
            .await?;
        let sealed: SealAck = protocol::parse_meta(&ack)?;
        if sealed.mxc != state.mxc {
            return Err(SdkError::Protocol(format!(
                "Seal returned mxc {} for upload {}",
                sealed.mxc, state.mxc
            )));
        }
        Ok(Manifest {
            server: state.server.clone(),
            mxc: state.mxc.clone(),
            block: final_block.clone(),
        })
    }

    /// 一塊的請求：seq 是塊索引，不走 `call` 的計數器。`Corrupt` 重送一次（線上規格 §5）。
    async fn send_one_chunk(
        &mut self,
        upload_id: u64,
        index: u32,
        sealed: Vec<u8>,
        is_last: bool,
    ) -> Result<ChunkAck, SdkError> {
        let request = protocol::chunk(upload_id, index, sealed, is_last);
        let ack = match self.send_and_expect_ack(request.clone()).await {
            Err(SdkError::Server { code, .. }) if code == "Corrupt" => {
                self.send_and_expect_ack(request).await?
            }
            other => other?,
        };
        protocol::parse_meta(&ack)
    }
}

fn read_up_to<R: Read>(reader: &mut R, want: usize) -> Result<Vec<u8>, SdkError> {
    let mut buffer = vec![0u8; want];
    let mut filled = 0;
    while filled < want {
        let got = reader.read(&mut buffer[filled..])?;
        if got == 0 {
            break;
        }
        filled += got;
    }
    buffer.truncate(filled);
    Ok(buffer)
}

/// 從頭讀 `len` byte 算 SHA-256。
fn hash_reader<R: Read + Seek>(source: &mut R, len: u64) -> Result<String, SdkError> {
    source.seek(SeekFrom::Start(0))?;
    let mut hasher = Sha256::new();
    let mut remaining = len;
    let mut buffer = vec![0u8; 1 << 16];
    while remaining > 0 {
        let want = remaining.min(buffer.len() as u64) as usize;
        source.read_exact(&mut buffer[..want])?;
        hasher.update(&buffer[..want]);
        remaining -= want as u64;
    }
    Ok(hex::encode(hasher.finalize()))
}

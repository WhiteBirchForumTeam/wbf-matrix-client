//! 下載與 seek（線上規格 §4、約定 §3.1、§7、CLI 規格 §3.3.1）。約定 §3.1 的五條全在這裡，任一不過就 `Integrity`。

use std::io::Write;

use sha2::{Digest, Sha256};

use crate::channel::PackChannel;
use crate::chunk_block::ChunkedBlock;
use crate::chunk_crypto::{chunk_count, expected_plain_len, locate, DescriptionSlot, FileCipher};
use crate::client::WbfClient;
use crate::error::SdkError;
use crate::manifest::Manifest;
use crate::protocol::InfoAck;

/// 下載完成後的事實。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DownloadReport {
    pub chunks: u32,
    pub bytes: u64,
    /// 區塊有 `sha256` 才會是 true；沒有就沒驗（約定 §3.1 第 5 條「有就要驗」）。
    pub sha256_verified: bool,
}

/// `seek` 的結果（CLI 規格 §3.3.1 的 stderr 摘要）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SeekResult {
    pub bytes: Vec<u8>,
    pub chunks_read: Vec<u32>,
    /// `--len` 超過檔尾，印到檔尾為止。
    pub truncated: bool,
}

/// `Info` 核對過的下載參數；`download` 與 `seek_read` 都先過這關。
pub struct VerifiedTarget {
    pub file_cipher: FileCipher,
    pub file_size: u64,
    pub chunk_count: u32,
}

impl<C: PackChannel> WbfClient<C> {
    /// 約定 §3.1 第 1、2 條，加描述交叉核對（約定 §4）。
    ///
    /// Return:
    ///     Ok(VerifiedTarget)
    ///     Err(SdkError)     `Integrity`：區塊壞、與 `Info` 對不上、描述解不開或對不上；`Server`：NotFound 等
    pub async fn verify_target(&mut self, manifest: &Manifest) -> Result<VerifiedTarget, SdkError> {
        let file_cipher = manifest.file_cipher()?;
        let file_size = manifest.file_size();
        let expected_count =
            chunk_count(file_size, manifest.block.chunk_size).ok_or_else(|| {
                SdkError::Integrity("file_size / chunk_size gives more chunks than u32".into())
            })?;

        let (info, description_data) = self.fetch_info(&manifest.mxc).await?;
        check_info_against_block(&info, &manifest.block, expected_count)?;
        check_description(&file_cipher, &manifest.block, &description_data)?;

        Ok(VerifiedTarget {
            file_cipher,
            file_size,
            chunk_count: expected_count,
        })
    }

    /// 整檔：逐塊 `Read` → 驗長度 → 解密 → 寫出；有 `sha256` 就整檔核對（約定 §3.1 第 3、4、5 條）。
    /// 任一塊不過就回 `Integrity`，`out` 已寫的是半成品，呼叫者刪（CLI exit 3 的語意）。
    pub async fn download<W: Write>(
        &mut self,
        manifest: &Manifest,
        out: &mut W,
        on_progress: &mut (dyn FnMut(u32, u32) + Send),
    ) -> Result<DownloadReport, SdkError> {
        let target = self.verify_target(manifest).await?;
        let expected_sha256 = parse_sha256_field(&manifest.block)?;
        let mut hasher = Sha256::new();
        let mut bytes = 0u64;
        for index in 0..target.chunk_count {
            let plain = self.read_and_open_chunk(manifest, &target, index).await?;
            hasher.update(&plain);
            bytes += plain.len() as u64;
            out.write_all(&plain)?;
            on_progress(index + 1, target.chunk_count);
        }
        if bytes != target.file_size {
            return Err(SdkError::Integrity(format!(
                "wrote {bytes} bytes, file_size says {}",
                target.file_size
            )));
        }
        let sha256_verified = match expected_sha256 {
            Some(expected) => {
                let actual = hex::encode(hasher.finalize());
                if actual != expected {
                    return Err(SdkError::Integrity(format!(
                        "sha256 mismatch: block {expected}, file {actual}"
                    )));
                }
                true
            }
            None => false,
        };
        Ok(DownloadReport {
            chunks: target.chunk_count,
            bytes,
            sha256_verified,
        })
    }

    /// CLI 規格 §3.3.1：`at` 是明文位置；不帶 `len` 印到含 `at` 那塊的塊尾；跨塊裁頭裁尾；超過檔尾 `truncated`。
    ///
    /// Args:
    ///     at: example: 71680
    ///     len: example: Some(71680)
    /// Return:
    ///     Ok(SeekResult)
    ///     Err(SdkError)     `Usage`：`at` 不小於檔長；`Integrity`：任一塊不過
    pub async fn seek_read(
        &mut self,
        manifest: &Manifest,
        at: u64,
        len: Option<u64>,
    ) -> Result<SeekResult, SdkError> {
        let target = self.verify_target(manifest).await?;
        if at >= target.file_size {
            return Err(SdkError::Usage(format!(
                "--at {at} is not below file_size {}",
                target.file_size
            )));
        }
        let chunk_size = u64::from(manifest.block.chunk_size);
        let start = locate(at, manifest.block.chunk_size)
            .ok_or_else(|| SdkError::Usage("chunk_size 0".into()))?;
        let (end, truncated) = match len {
            None => (
                ((u64::from(start.index) + 1) * chunk_size).min(target.file_size),
                false,
            ),
            Some(len) => {
                let wanted_end = at.saturating_add(len);
                (
                    wanted_end.min(target.file_size),
                    wanted_end > target.file_size,
                )
            }
        };
        let mut bytes = Vec::with_capacity((end - at) as usize);
        let mut chunks_read = Vec::new();
        let mut index = start.index;
        while u64::from(index) * chunk_size < end {
            let plain = self.read_and_open_chunk(manifest, &target, index).await?;
            let chunk_start = u64::from(index) * chunk_size;
            let from = at.saturating_sub(chunk_start) as usize;
            let to = (end - chunk_start).min(plain.len() as u64) as usize;
            let wanted = plain.get(from..to).ok_or_else(|| {
                SdkError::Integrity(format!(
                    "chunk {index} has {} bytes, wanted {from}..{to}",
                    plain.len()
                ))
            })?;
            bytes.extend_from_slice(wanted);
            chunks_read.push(index);
            index += 1;
        }
        Ok(SeekResult {
            bytes,
            chunks_read,
            truncated,
        })
    }

    pub(crate) async fn read_and_open_chunk(
        &mut self,
        manifest: &Manifest,
        target: &VerifiedTarget,
        index: u32,
    ) -> Result<Vec<u8>, SdkError> {
        let expected = expected_plain_len(target.file_size, manifest.block.chunk_size, index)
            .ok_or_else(|| SdkError::Integrity(format!("chunk {index} is beyond chunk_count")))?;
        let (_ack, data) = self.read_chunk(&manifest.mxc, index).await?;
        Ok(target.file_cipher.open_chunk(index, &data, expected)?)
    }
}

/// 約定 §3.1 第 2 條：`chunk_size`、`chunk_count` 要與 `Info` 一致；`Info` 有 `file_size` 也要一致。
fn check_info_against_block(
    info: &InfoAck,
    block: &ChunkedBlock,
    expected_count: u32,
) -> Result<(), SdkError> {
    let (Some(info_chunk_size), Some(info_chunk_count)) = (info.chunk_size, info.chunk_count)
    else {
        return Err(SdkError::Integrity(
            "Info says this is not chunked media".into(),
        ));
    };
    if info_chunk_size != block.chunk_size {
        return Err(SdkError::Integrity(format!(
            "chunk_size: block {} vs Info {info_chunk_size}",
            block.chunk_size
        )));
    }
    if info_chunk_count != expected_count {
        return Err(SdkError::Integrity(format!(
            "chunk_count: block implies {expected_count} vs Info {info_chunk_count}"
        )));
    }
    if let (Some(info_file_size), Some(block_file_size)) = (info.file_size, block.file_size) {
        if info_file_size != block_file_size {
            return Err(SdkError::Integrity(format!(
                "file_size: block {block_file_size} vs Info {info_file_size}"
            )));
        }
    }
    if info.truncated == Some(true) {
        return Err(SdkError::Integrity(
            "Info says the upload was truncated".into(),
        ));
    }
    Ok(())
}

/// 約定 §4：描述解開後要與區塊一致。我們自己的上傳一定 `Seal` 過，所以先試 `Seal` 的 nonce，
/// 再試 `Create`（別的 client 可能沒 `Seal` 帶描述）。兩個都解不開 → 拒絕：描述必帶不空，解不開就是對不上。
fn check_description(
    file_cipher: &FileCipher,
    block: &ChunkedBlock,
    data: &[u8],
) -> Result<(), SdkError> {
    if data.is_empty() {
        return Err(SdkError::Integrity(
            "Info returned an empty description".into(),
        ));
    }
    let json = file_cipher
        .open_description(DescriptionSlot::Seal, data)
        .or_else(|_| file_cipher.open_description(DescriptionSlot::Create, data))
        .map_err(|_| SdkError::Integrity("description does not decrypt with this key".into()))?;
    let description = ChunkedBlock::from_description_json(&json)
        .map_err(|error| SdkError::Integrity(format!("description: {error}")))?;
    if !block.is_consistent_with_description(&description) {
        return Err(SdkError::Integrity(
            "description disagrees with the event block".into(),
        ));
    }
    Ok(())
}

/// 約定 §4：`sha256` 是十六進位小寫 64 字；不是就拒絕，不猜。
fn parse_sha256_field(block: &ChunkedBlock) -> Result<Option<String>, SdkError> {
    let Some(text) = &block.sha256 else {
        return Ok(None);
    };
    let is_lower_hex_64 = text.len() == 64
        && text
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'));
    if !is_lower_hex_64 {
        return Err(SdkError::Integrity(format!(
            "sha256 field is not 64 lowercase hex chars: {text}"
        )));
    }
    Ok(Some(text.clone()))
}

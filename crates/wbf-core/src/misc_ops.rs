//! 剩下三個：`info`（問 server 一份上傳的形狀）、`seek`（只讀中間一段）、
//! 串流上傳（stdin 那種）。

use std::io::Read;

use serde::Serialize;

use wbf_sdk::chunk_crypto::{choose_stream_chunk_size, DescriptionSlot, Link};
use wbf_sdk::manifest::Manifest;
use wbf_sdk::{ChunkedBlock, FileCipher, Transport};

use crate::backend_choice::MethodHome;
use crate::error::{CoreError, CoreErrorKind};
use crate::link_pool::LinkRole;
use crate::upload_ops::UploadRequest;
use crate::{Core, Target};

/// `info`：這份媒體長什麼樣。**本地與上游都答得出大部分**（daemon-runtime §3.1 的 `sync`）。
///
/// ⭐ 媒體是**不可變**的：`file_size`／`chunk_size`／`mimetype` 上傳完就不會變，所以本地那張
/// `media` 表存的就是同一份事實 —— 🚫 沒有理由為了這些欄位跑一趟 server（維護者 2026-09-13）。
/// ⚠️ 真正只有 server 知道的是 `total_len`（線上那份的總長）、`truncated`，
/// 以及要拿 server 的描述才算得出來的 `description`／`verified` —— `sync=local` 時它們**不在**，
/// 🚫 不編造。
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct MediaInfo {
    /// ⚠️ **只有問過 server 才有**：線上那份的總長。`sync=local` 時不在。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_len: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_size: Option<u64>,
    /// ⚠️ 這三個是 `Option` **有語意**，不是圖方便：整檔媒體（舊上傳）沒有分塊，
    /// 線上規格 §4.1 定它們是 `null`。🚫 不要填 0 或 `false` 頂替——
    /// 「沒有分塊」跟「切成 0 塊」是兩件事。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chunk_size: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chunk_count: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncated: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
    /// 有給 manifest 才有：交叉核對過的描述。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<serde_json::Value>,
    /// ⚠️ `true` 只在**真的拿 manifest 核對過**時出現。沒給 manifest 時這個欄位不在，
    /// 🚫 不是 `false`——「沒驗」跟「驗過但不對」是兩件事。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verified: Option<bool>,
    /// 這台機器上的狀態（下載到哪了）。⭐ 這是**上游答不出來**的那一半。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cached: Option<CachedMedia>,
}

/// `media` 表那一列裡「本地才知道」的部分。
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CachedMedia {
    /// 整檔都在池裡了嗎。
    pub complete: bool,
    /// 已經收下幾塊（續傳點）。
    pub chunks_written: u64,
    /// 池裡那份佔多少磁碟（**加密後**的大小，🚫 不等於 `file_size`）。
    pub bytes_on_disk: u64,
}

/// `seek` 的結果。**bytes 是明文**，由呼叫端決定要寫到哪（stdout、檔案、播放器）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SeekResult {
    pub bytes: Vec<u8>,
    /// 讀了哪幾塊（不是「幾塊」）——一次 seek 可能跨兩塊。
    pub chunks_read: Vec<u32>,
    pub truncated: bool,
}

/// `seek` 的摘要（不含 bytes，可以直接序列化）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SeekSummary {
    pub at: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub len: Option<u64>,
    pub bytes: usize,
    pub chunks_read: Vec<u32>,
    pub truncated: bool,
}

impl SeekResult {
    pub fn summary(&self, at: u64, len: Option<u64>) -> SeekSummary {
        SeekSummary {
            at,
            len,
            bytes: self.bytes.len(),
            chunks_read: self.chunks_read.clone(),
            truncated: self.truncated,
        }
    }
}

impl Core {
    /// 這個 mxc 的媒體有多大、切成幾塊、本地下載到哪了。
    ///
    /// `sync` 決定去哪問（daemon-runtime §3.1）：
    ///
    /// | `sync` | 回什麼 |
    /// |---|---|
    /// | `Local`（預設） | 只讀 `media` 表：`file_size`／`chunk_size`／`content_type`／`cached`。⚠️ `total_len`／`truncated`／`description`／`verified` **不在**（只有 server 知道），🚫 不編造 |
    /// | `Server` | 問 server，🚫 不寫庫、🚫 不附 `cached` |
    /// | `Both` | 問 server ＋ 把它寫進 `media` 表 ＋ 附上 `cached` |
    ///
    /// 給了 `manifest` 就多做一次**交叉核對**（約定 §3.1 第 2 條）並把描述解出來 ——
    /// ⚠️ 那需要 server 的描述，所以 `Local` 時🚫 不做（給了 manifest 也一樣）。
    pub async fn media_info(
        &self,
        mxc: &str,
        manifest: Option<&Manifest>,
        sync: crate::SyncMode,
        transport: Transport,
        target: &Target,
    ) -> Result<MediaInfo, CoreError> {
        let account = self.account_or_current(target)?;
        if sync == crate::SyncMode::Local {
            return self.cached_media_info(&account, mxc).await;
        }
        let mut client = self
            .client_of(&account, transport, MethodHome::WbfSdkOnly, LinkRole::Misc)
            .await?;
        let (info, description_data) = client.fetch_info(mxc).await?;
        if sync == crate::SyncMode::Both {
            // 把 server 說的寫進 `media` 表（下次 `Local` 就答得出來）。
            // ⚠️ 只有兩邊都知道的欄位才寫進去 —— `media_begin` 要的正好就是那些。
            let (cache, _me) = self.server_cache_and_me(&account)?;
            let (mxc_here, size, chunk) = (
                mxc.to_string(),
                info.file_size.unwrap_or(info.total_len),
                info.chunk_size.unwrap_or(0),
            );
            let content_type = info.content_type.clone();
            cache
                .run(move |cache| {
                    cache
                        .media_begin(&mxc_here, None, content_type.as_deref(), None, size, chunk)
                        .map(|_| ())
                })
                .await?;
        }
        let mut result = MediaInfo {
            total_len: Some(info.total_len),
            file_size: info.file_size,
            chunk_size: info.chunk_size,
            chunk_count: info.chunk_count,
            truncated: info.truncated,
            content_type: info.content_type,
            description: None,
            verified: None,
            cached: match sync {
                // `Server` 是「看一眼上游」：🚫 不順手提本地的事（對帳時要分得出來）。
                crate::SyncMode::Server => None,
                _ => self.cached_media_of(&account, mxc).await?,
            },
        };
        let Some(manifest) = manifest else {
            return Ok(result);
        };
        if manifest.mxc != mxc {
            return Err(CoreError::new(
                CoreErrorKind::Usage,
                format!("manifest is for {}, not {mxc}", manifest.mxc),
            ));
        }
        let target = client.verify_target(manifest).await?;
        let json = target
            .file_cipher
            .open_description(DescriptionSlot::Seal, &description_data)
            .or_else(|_| {
                target
                    .file_cipher
                    .open_description(DescriptionSlot::Create, &description_data)
            })
            .map_err(|error| CoreError::new(CoreErrorKind::Integrity, format!("{error}")))?;
        let description = ChunkedBlock::from_description_json(&json)
            .map_err(|error| CoreError::new(CoreErrorKind::Integrity, error))?;
        result.description = Some(serde_json::to_value(&description).expect("block serializes"));
        result.verified = Some(true);
        Ok(result)
    }

    /// 只讀本地那一列（`sync=local`）。
    ///
    /// Return:
    ///     Ok(MediaInfo)  本地知道的那幾個欄位；只有 server 知道的那些**不在**
    ///     Err(Usage)     這台機器沒有這份媒體的紀錄
    async fn cached_media_info(
        &self,
        account: &crate::accounts::AccountDir,
        mxc: &str,
    ) -> Result<MediaInfo, CoreError> {
        let (cache, _me) = self.server_cache_and_me(account)?;
        let found = cache.read().await.find_media(mxc)?;
        let entry = found.ok_or_else(|| {
            CoreError::new(
                CoreErrorKind::Usage,
                format!("{mxc} is not in the local cache; ask the server with sync=server"),
            )
        })?;
        Ok(MediaInfo {
            // 🚫 本地不知道線上那份的總長與有沒有被截斷。
            total_len: None,
            truncated: None,
            file_size: Some(entry.file_size),
            chunk_size: Some(entry.chunk_size),
            // ⚠️ 沒下完時「一共幾塊」是不知道的：`chunks_written` 是進度，🚫 不是總數。
            chunk_count: match entry.complete {
                true => u32::try_from(entry.chunks_written).ok(),
                false => None,
            },
            content_type: entry.mimetype.clone(),
            // 這兩個要 server 的描述才算得出來。
            description: None,
            verified: None,
            cached: Some(CachedMedia {
                complete: entry.complete,
                chunks_written: entry.chunks_written,
                bytes_on_disk: entry.bytes_on_disk,
            }),
        })
    }

    /// 本地那一列的「下載到哪了」；沒有那一列就是 `None`（🚫 不算錯）。
    async fn cached_media_of(
        &self,
        account: &crate::accounts::AccountDir,
        mxc: &str,
    ) -> Result<Option<CachedMedia>, CoreError> {
        let (cache, _me) = self.server_cache_and_me(account)?;
        let found = cache.read().await.find_media(mxc)?;
        Ok(found.map(|entry| CachedMedia {
            complete: entry.complete,
            chunks_written: entry.chunks_written,
            bytes_on_disk: entry.bytes_on_disk,
        }))
    }

    /// 只讀含 `at` 的那一段（約定 §7）。⚠️ 回的是**明文 bytes**。
    pub async fn seek_read(
        &self,
        manifest: &Manifest,
        at: u64,
        len: Option<u64>,
        transport: Transport,
        target: &Target,
    ) -> Result<SeekResult, CoreError> {
        let account = self.account_or_current(target)?;
        let mut client = self
            .client_of(
                &account,
                transport,
                MethodHome::WbfSdkOnly,
                LinkRole::Download,
            )
            .await?;
        let result = client.seek_read(manifest, at, len).await?;
        Ok(SeekResult {
            bytes: result.bytes,
            chunks_read: result.chunks_read,
            truncated: result.truncated,
        })
    }

    /// 串流上傳：邊讀邊傳，**事先不知道總長**。
    ///
    /// ⚠️ **這個方法是過渡的**。它收一個
    /// `&mut dyn Read`，而那是 trait object——architecture-v2 §7 明文說公開介面上
    /// 🚫 不要有 trait object（過不了 FFI、序列化不了）。
    ///
    /// 為什麼還是放這裡：§4.8 定了 daemon 模型下**上傳走資料平面的 HTTP PUT**
    /// （Android 的 SAF 只給 `content://`，根本沒有路徑可傳）。所以這條路徑在 daemon
    /// 落地時會**整個被 PUT 取代**，不是要長期維護的介面。
    /// 🚫 daemon 不要把它開成 RPC method；rpc-cli 用它讀 stdin，到此為止。
    pub async fn upload_stream(
        &self,
        mut source: &mut dyn Read,
        request: &UploadRequest,
        wifi: bool,
        transport: Transport,
        target: &Target,
    ) -> Result<Manifest, CoreError> {
        let account = self.account_or_current(target)?;
        let session = self.session_of(&account)?;
        let mut client = self
            .client_of(
                &account,
                transport,
                MethodHome::WbfSdkOnly,
                LinkRole::Upload,
            )
            .await?;
        let cipher = crate::upload_ops::parse_cipher(request.cipher.as_deref())?;
        let link = match wifi {
            true => Link::WifiOrWired,
            false => Link::MobileOrUnknown,
        };
        let chunk_size = request
            .chunk_size
            .unwrap_or_else(|| choose_stream_chunk_size(link));
        let file_cipher = FileCipher::generate(cipher, chunk_size);
        let mut block = file_cipher.to_event_block(0);
        // ⚠️ 串流不知道總長，所以 `file_size` 是 `None` 而不是 0——那兩件事不一樣，
        // 收檔端要分得出「空檔」與「還不知道多大」。
        block.file_size = None;
        block.name = request.name.clone();
        block.mimetype = request.mimetype.clone();
        let state = client
            .create_upload(&session.server, &session.user_id, &file_cipher, &block)
            .await?;
        let summary = client
            // ⚠️ 串流事先不知道總長：`total` 是 `None`，🚫 不填 0 假裝知道。
            .send_stream(&state, &mut source, &mut |done, _| {
                self.events
                    .progress_of(done as u64, None, format!("chunk {done}"))
            })
            .await?;
        let mut final_block = state.block.clone();
        final_block.file_size = Some(summary.file_size);
        final_block.sha256 = summary.sha256;
        let manifest = client.seal_upload(&state, &final_block).await?;
        if summary.truncated {
            self.events
                .progress("warning: server truncated this upload at its size limit");
        }
        Ok(manifest)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn not_verified_and_verified_false_are_different_things() {
        // 沒給 manifest 就沒驗過——`verified` 不在，🚫 不是 `false`。
        let info = MediaInfo {
            total_len: Some(10),
            file_size: Some(10),
            chunk_size: Some(4),
            chunk_count: Some(3),
            truncated: Some(false),
            content_type: None,
            description: None,
            verified: None,
            cached: None,
        };
        let json = serde_json::to_value(&info).unwrap();
        assert!(json.get("verified").is_none(), "{json}");
        assert!(json.get("description").is_none());
    }

    /// `sync=local` 回的那份：**只有 server 知道的欄位不在**，🚫 不填假的
    /// （daemon-runtime §3.2）。
    #[test]
    fn a_local_answer_leaves_out_what_only_the_server_knows() {
        let local = MediaInfo {
            total_len: None,
            truncated: None,
            file_size: Some(10),
            chunk_size: Some(4),
            chunk_count: Some(3),
            content_type: Some("video/mp4".to_string()),
            description: None,
            verified: None,
            cached: Some(CachedMedia {
                complete: true,
                chunks_written: 3,
                bytes_on_disk: 1234,
            }),
        };
        let json = serde_json::to_value(&local).unwrap();
        assert!(json.get("total_len").is_none(), "{json}");
        assert!(json.get("truncated").is_none(), "{json}");
        // 反過來，本地才知道的那一半要在。
        assert_eq!(json["cached"]["complete"], true);
        assert_eq!(json["cached"]["bytes_on_disk"], 1234);
    }
}

/// `ping` 回的東西（server 的能力與上限）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ServerHello {
    pub protocol: u32,
    pub server: String,
    pub features: Vec<String>,
    pub chunk_size_default: u32,
    pub chunk_size_large: u32,
    pub data_max_bytes: u64,
}

/// `status`：server 收到這個上傳的哪裡了。
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct UploadStatusReport {
    pub received: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chunk_count: Option<u32>,
    pub total_len: u64,
    pub finished: bool,
    pub truncated: bool,
    pub chunk_size: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_size: Option<u64>,
}

impl Core {
    /// `Hello` 加 `Ping`：server 講哪個協議版本、支援什麼、上限多少。
    pub async fn ping(
        &self,
        transport: Transport,
        client_name: &str,
        target: &Target,
    ) -> Result<ServerHello, CoreError> {
        let account = self.account_or_current(target)?;
        let mut client = self
            .client_of(&account, transport, MethodHome::WbfSdkOnly, LinkRole::Misc)
            .await?;
        let hello = client.hello(client_name, &[]).await?;
        client.ping().await?;
        Ok(ServerHello {
            protocol: hello.protocol,
            server: hello.server,
            features: hello.features,
            chunk_size_default: hello.chunk_size_default,
            chunk_size_large: hello.chunk_size_large,
            data_max_bytes: hello.data_max_bytes,
        })
    }

    /// server 收到這個上傳的哪裡了（續傳前先問它）。
    pub async fn upload_status(
        &self,
        upload_id: u64,
        transport: Transport,
        target: &Target,
    ) -> Result<UploadStatusReport, CoreError> {
        let account = self.account_or_current(target)?;
        let mut client = self
            .client_of(&account, transport, MethodHome::WbfSdkOnly, LinkRole::Misc)
            .await?;
        let status = client.upload_status(upload_id).await?;
        Ok(UploadStatusReport {
            received: status.received,
            chunk_count: status.chunk_count,
            total_len: status.total_len,
            finished: status.finished,
            truncated: status.truncated,
            chunk_size: status.chunk_size,
            file_size: status.file_size,
        })
    }

    /// 放棄一個上傳，並把它的續傳狀態檔一起刪掉。
    ///
    /// ⚠️ 兩邊都要清：只跟 server 說放棄、狀態檔留著的話，下次會拿一個 server 已經
    /// 忘掉的 `upload_id` 去續傳。
    pub async fn abort_upload(
        &self,
        upload_id: u64,
        file: Option<&std::path::Path>,
        transport: Transport,
        target: &Target,
    ) -> Result<(), CoreError> {
        let account = self.account_or_current(target)?;
        let mut client = self
            .client_of(&account, transport, MethodHome::WbfSdkOnly, LinkRole::Misc)
            .await?;
        client.abort_upload(upload_id).await?;
        if let Some(file) = file {
            crate::upload_ops::remove_resume_state(file)?;
        }
        Ok(())
    }
}

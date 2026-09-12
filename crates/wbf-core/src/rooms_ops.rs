//! 房間的讀與寫：清單、歷史、附件、送訊息。
//!
//! ⚠️ **快取是寫穿的，而且寫穿失敗只報不擋**（local-cache-db.md §1：快取壞了的代價是
//! 重拉，不是命令失敗）。所以這裡每個從 server 拿資料的路徑都是「拿到 → 試著寫快取 →
//! 不管成不成功都回傳」。
//!
//! 🚫 這裡**不做任何互動**：「這個房間沒加密，你確定要送嗎」那種確認是前端的事（§3）。
//! core 提供的是[`Core::conversation`]（讓前端問得到「加密了沒」）與一個**照做**的
//! [`Core::send_file`]。

use std::path::Path;

use serde::{Deserialize, Serialize};

use wbf_sdk::chat::{ChatBackend, Conversation, Message, MessageKind};
use wbf_sdk::manifest::Manifest;
use wbf_sdk::Cipher;

use crate::accounts::AccountDir;
use crate::error::{CoreError, CoreErrorKind};
use crate::{Core, Target};

/// 一頁訊息。`next` 是下一頁的游標；⚠️ 過濾之後 `events` 可能是空的但 `next` 還在，
/// 呼叫端要照 `next` 判斷有沒有下一頁，🚫 不要看 `events` 空不空。
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct MessagePage {
    pub events: Vec<Message>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next: Option<String>,
}

/// 一則附件：事件的身分加一份可以拿去下載的 manifest。
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct FileEntry {
    pub event_id: String,
    pub sender: String,
    pub ts: u64,
    pub manifest: serde_json::Value,
}

/// `files` 的一頁。
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct FilePage {
    pub files: Vec<FileEntry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next: Option<String>,
}

/// 讀一頁歷史要什麼。
///
/// 📎 併成一個型別的理由跟 [`crate::Target`] 一樣：**RPC 的 `params` 就是這個形狀**。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryQuery {
    pub room: String,
    pub limit: u32,
    /// `Server` 時是 server 的翻頁 token；`Cache` 時是 `r_seq` 的數字。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before: Option<String>,
    pub source: HistorySource,
    /// 空的就不濾。⚠️ 過濾在**這一層**做（CLI 規格 §3.4.1）：server 不知道我們的 kind 名字。
    #[serde(default)]
    pub types: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sender: Option<String>,
}

/// 讀歷史要從哪拿。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HistorySource {
    /// 打 server，順手寫穿快取。
    Server,
    /// 只讀 `cache.db`，🚫 不連網。⚠️ `before` 這時是 `r_seq` 的數字，不是 server 的翻頁 token。
    Cache,
}

impl Core {
    /// 加入的房間。順手寫穿快取。
    pub async fn list_conversations(
        &self,
        target: &Target,
    ) -> Result<Vec<Conversation>, CoreError> {
        let account = self.account_or_current(target)?;
        let backend = self
            .synced_backend_of(&account, target.server_backup)
            .await?;
        let conversations = backend.conversations().await?;
        if let Ok((mut cache, me)) = self.cache_and_me(&account) {
            self.write_through(cache.upsert_conversations(&me, &conversations));
        }
        Ok(conversations)
    }

    /// 一個房間本身（前端要問「加密了沒」就用它）。
    pub async fn conversation(
        &self,
        room: &str,
        target: &Target,
    ) -> Result<Conversation, CoreError> {
        let account = self.account_or_current(target)?;
        let backend = self
            .synced_backend_of(&account, target.server_backup)
            .await?;
        Ok(backend.conversation(room).await?)
    }

    /// 送一則文字。
    pub async fn send_text(
        &self,
        room: &str,
        body: &str,
        target: &Target,
    ) -> Result<String, CoreError> {
        let account = self.account_or_current(target)?;
        let backend = self
            .synced_backend_of(&account, target.server_backup)
            .await?;
        Ok(backend.send_text(room, body).await?)
    }

    /// 讀一頁歷史。
    ///
    /// Args:
    ///     before: `Server` 時是 server 的翻頁 token；`Cache` 時是 `r_seq` 的數字
    ///     types: 空的就不濾, example: &["text".to_string()]
    ///     sender: example: Some("@bob:localhost")
    pub async fn history(
        &self,
        query: &HistoryQuery,
        target: &Target,
    ) -> Result<MessagePage, CoreError> {
        let (room, limit, before) = (query.room.as_str(), query.limit, query.before.as_deref());
        let (types, sender) = (&query.types, query.sender.as_deref());
        let account = self.account_or_current(target)?;
        let (events, next) = self
            .page_of(
                &account,
                room,
                limit,
                before,
                query.source,
                target.server_backup,
            )
            .await?;
        // 過濾在這一層（CLI 規格 §3.4.1）：server 不知道我們的 kind 名字。
        let events = events
            .into_iter()
            .filter(|message| sender.is_none_or(|sender| message.sender == sender))
            .filter(|message| {
                types.is_empty() || types.iter().any(|wanted| kind_matches(message, wanted))
            })
            .collect();
        Ok(MessagePage { events, next })
    }

    /// 這個房間裡的附件，每則配一份 manifest。
    ///
    /// Args:
    ///     save_to: 給了就把每份 manifest 寫成 `<event id>.json`（0600）
    pub async fn files(
        &self,
        room: &str,
        limit: u32,
        before: Option<&str>,
        source: HistorySource,
        save_to: Option<&Path>,
        target: &Target,
    ) -> Result<FilePage, CoreError> {
        let account = self.account_or_current(target)?;
        let session_server = self.session_of(&account)?.server;
        let (events, next) = self
            .page_of(&account, room, limit, before, source, target.server_backup)
            .await?;
        let mut files = Vec::new();
        for message in &events {
            let MessageKind::File { attachment, .. } = &message.kind else {
                continue;
            };
            let manifest = Manifest {
                server: session_server.clone(),
                mxc: attachment.mxc.clone(),
                block: attachment.block.clone(),
            };
            if let Some(dir) = save_to {
                std::fs::create_dir_all(dir)
                    .map_err(|error| CoreError::new(CoreErrorKind::Io, format!("{error}")))?;
                let file_name = format!(
                    "{}.json",
                    message
                        .id
                        .trim_start_matches('$')
                        .replace(['/', '\\', ':'], "_")
                );
                wbf_sdk::vault::write_private(&dir.join(file_name), &manifest.to_json())?;
            }
            files.push(FileEntry {
                event_id: message.id.clone(),
                sender: message.sender.clone(),
                ts: message.sent_at,
                manifest: serde_json::from_slice(&manifest.to_json()).expect("manifest is json"),
            });
        }
        Ok(FilePage { files, next })
    }

    /// `history` 與 `files` 共用的取頁：從 server 拿就順手寫穿快取。
    async fn page_of(
        &self,
        account: &AccountDir,
        room: &str,
        limit: u32,
        before: Option<&str>,
        source: HistorySource,
        server_backup: bool,
    ) -> Result<(Vec<Message>, Option<String>), CoreError> {
        match source {
            HistorySource::Cache => {
                let (cache, me) = self.cache_and_me(account)?;
                let messages = cache.history(&me, room, parse_before_r_seq(before)?, limit)?;
                Ok(cached_page(messages))
            }
            HistorySource::Server => {
                let backend = self.synced_backend_of(account, server_backup).await?;
                let page = backend.history(room, before, limit).await?;
                if let Ok((mut cache, me)) = self.cache_and_me(account) {
                    self.write_through(cache.upsert_messages(&me, &page.events));
                }
                Ok((page.events, page.next))
            }
        }
    }

    /// 開 backend 並做一次增量 sync（timeout 0）：房間列表與新事件到 store，之後才看得到現況。
    pub(crate) async fn synced_backend_of(
        &self,
        account: &AccountDir,
        server_backup: bool,
    ) -> Result<wbf_sdk::backend::matrix_sdk::MatrixBackend, CoreError> {
        let backend = self.backend_of(account, server_backup).await?;
        backend.sync_once(None, std::time::Duration::ZERO).await?;
        Ok(backend)
    }

    /// 這個帳號的 `cache.db` 與「我是誰」（讀寫快取都要帶 mxid）。
    pub(crate) fn cache_and_me(
        &self,
        account: &AccountDir,
    ) -> Result<(wbf_sdk::cache::Cache, String), CoreError> {
        let session = self.session_of(account)?;
        let cache = self.cache_of(account, &session.server)?;
        Ok((cache, session.user_id))
    }

    /// 寫穿快取的錯誤**只報不擋**（§1：快取壞了的代價是重拉，不是命令失敗）。
    pub(crate) fn write_through<T>(&self, result: Result<T, wbf_sdk::SdkError>) {
        if let Err(error) = result {
            self.events
                .progress(format!("cache write failed (ignored): {error}"));
        }
    }
}

/// 從快取印一頁時，`next` 是這頁**最小**的 `r_seq`（下一頁的 `before`）。
/// 沒有 `r_seq` 的房間翻不了頁（chat-model §4.3 的退化表）。
fn cached_page(messages: Vec<Message>) -> (Vec<Message>, Option<String>) {
    let next = messages
        .iter()
        .filter_map(|message| message.r_seq)
        .min()
        .map(|r_seq| r_seq.to_string());
    (messages, next)
}

/// ⚠️ `before` 在 `Cache` 來源時是 `r_seq` 的數字，**不是** server 的翻頁 token。
fn parse_before_r_seq(before: Option<&str>) -> Result<Option<i64>, CoreError> {
    before
        .map(|text| {
            text.parse::<i64>().map_err(|_| {
                CoreError::new(
                    CoreErrorKind::Usage,
                    format!(
                        "{text} is not an r_seq number (reading from the cache pages by r_seq, not by a server token)"
                    ),
                )
            })
        })
        .transpose()
}

/// `--type` 對的是我們模型的 kind 名（text／file／deleted／system／unsupported），
/// 或 `Unsupported` 帶的原始 event type。
fn kind_matches(message: &Message, wanted: &str) -> bool {
    match &message.kind {
        MessageKind::Text { .. } => wanted == "text" || wanted == "m.room.message",
        MessageKind::File { .. } => wanted == "file" || wanted == "org.wbftw.wbfuwunel.file",
        MessageKind::Deleted { .. } => wanted == "deleted",
        MessageKind::System { event_type, .. } => wanted == "system" || wanted == event_type,
        MessageKind::Unsupported { event_type, .. } => {
            wanted == "unsupported" || wanted == event_type
        }
    }
}

/// 在沒有 E2EE 的房間送檔案時，唯一准的密碼學選擇（約定 §5.1）。
///
/// 🚫 **永遠不在沒 E2EE 的房間送加密的區塊**：那個區塊的金鑰會公開，等於用一個假的
/// 保護感騙人。呼叫端要嘛用 `Cipher::None`，要嘛別送。
pub fn cipher_for_plaintext_room(requested: Option<&str>) -> Result<Cipher, CoreError> {
    match requested {
        None => Ok(Cipher::None),
        Some(name) if name == Cipher::None.name() => Ok(Cipher::None),
        Some(_) => Err(CoreError::new(
            CoreErrorKind::Usage,
            "an unencrypted room only takes cipher `none` (an encrypted block's key would be public)",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paging_from_the_cache_needs_an_r_seq_number_not_a_server_token() {
        assert_eq!(parse_before_r_seq(None).unwrap(), None);
        assert_eq!(parse_before_r_seq(Some("42")).unwrap(), Some(42));
        // server 的翻頁 token 長這樣，拿來當 r_seq 用要報錯而不是默默當成 None。
        let error = parse_before_r_seq(Some("t57-1234_0_0_0")).unwrap_err();
        assert_eq!(error.kind, CoreErrorKind::Usage);
    }

    #[test]
    fn a_plaintext_room_only_takes_cipher_none() {
        // 🚫 加密的區塊在明文房間裡，金鑰是公開的——那是假的保護感。
        assert_eq!(cipher_for_plaintext_room(None).unwrap(), Cipher::None);
        assert_eq!(
            cipher_for_plaintext_room(Some("none")).unwrap(),
            Cipher::None
        );
        assert_eq!(
            cipher_for_plaintext_room(Some("xchacha20poly1305"))
                .unwrap_err()
                .kind,
            CoreErrorKind::Usage
        );
    }
}

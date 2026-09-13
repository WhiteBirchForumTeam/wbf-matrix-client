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
    /// 打上游時是 server 的翻頁 token；讀本地時是 `r_seq` 的數字。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before: Option<String>,
    #[serde(default)]
    pub sync: SyncMode,
    /// 空的就不濾。⚠️ 過濾在**這一層**做（CLI 規格 §3.4.1）：server 不知道我們的 kind 名字。
    #[serde(default)]
    pub types: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sender: Option<String>,
}

/// 這次查詢要本地的、上游的、還是兩者（rpc-spec §2 的 `sync`；daemon-runtime §3.1）。
///
/// ⭐ **預設是 `Local`**：RPC 大部分是對本地資料庫的呼叫，要打上游得**明講**。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncMode {
    /// 只讀 `cache.db`，🚫 不連網。⚠️ 翻頁的 `before` 這時是 `r_seq` 的數字，不是 server 的 token。
    #[default]
    Local,
    /// 打上游、拿到什麼回什麼。🚫 **不寫快取** —— 這是「看一眼」，不是同步。
    /// 📎 分開的理由：對帳時要看得見「上游說 A、本地存的是 B」，順手寫回去那個差異就消失了。
    Server,
    /// 打上游 → 寫進 `cache.db` → **再從本地讀一次**回傳。
    /// ⭐ 回的是本地讀的結果，所以形狀跟 `Local` 一模一樣，呼叫端🚫 不必寫兩套解析。
    Both,
}

impl Core {
    /// 加入的房間。`sync` 決定要本地的還是上游的（daemon-runtime §3.1）。
    pub async fn list_conversations(
        &self,
        sync: SyncMode,
        target: &Target,
    ) -> Result<Vec<Conversation>, CoreError> {
        let account = self.account_or_current(target)?;
        if sync == SyncMode::Local {
            let (cache, me) = self.server_cache_and_me(&account)?;
            let rows = cache.read().await.list_conversations(&me)?;
            return Ok(rows);
        }
        let backend = self
            .synced_backend_of(&account, target.server_backup)
            .await?;
        let conversations = backend.conversations().await?;
        if sync == SyncMode::Server {
            // 🚫 看一眼不寫庫。
            return Ok(conversations);
        }
        // `Both`：寫進去**等它落地**，再從本地讀一次回傳 —— 這樣回的形狀跟 `Local` 一樣。
        let (cache, me) = self.server_cache_and_me(&account)?;
        let rows = conversations;
        let me_here = me.clone();
        cache
            .run(move |cache| cache.upsert_conversations(&me_here, &rows).map(|_| ()))
            .await?;
        let rows = cache.read().await.list_conversations(&me)?;
        Ok(rows)
    }

    /// 一個房間本身（前端要問「加密了沒」就用它）。`sync` 同 [`Core::list_conversations`]。
    ///
    /// 📎 本地那條是從房間列表裡挑 —— `cache.db` 存的就是整個 `Conversation`（`room_list` 表），
    /// 🚫 沒有另一張「單一房間」的表。
    pub async fn conversation(
        &self,
        room: &str,
        sync: SyncMode,
        target: &Target,
    ) -> Result<Conversation, CoreError> {
        let account = self.account_or_current(target)?;
        if sync != SyncMode::Server {
            if sync == SyncMode::Both {
                // 先讓 `Both` 去上游更新一輪（它自己會寫進去）。
                self.list_conversations(SyncMode::Both, target).await?;
            }
            let (cache, me) = self.server_cache_and_me(&account)?;
            let found = cache
                .read()
                .await
                .list_conversations(&me)?
                .into_iter()
                .find(|conversation| conversation.id == room);
            return found.ok_or_else(|| {
                CoreError::new(
                    CoreErrorKind::NoSuchAccount,
                    format!("{room} is not in the local cache; try sync=both"),
                )
            });
        }
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
                query.sync,
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
        sync: SyncMode,
        save_to: Option<&Path>,
        target: &Target,
    ) -> Result<FilePage, CoreError> {
        let account = self.account_or_current(target)?;
        let session_server = self.session_of(&account)?.server;
        let (events, next) = self
            .page_of(&account, room, limit, before, sync, target.server_backup)
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

    /// `history` 與 `files` 共用的取頁（daemon-runtime §3.1 的三種 `sync`）。
    async fn page_of(
        &self,
        account: &AccountDir,
        room: &str,
        limit: u32,
        before: Option<&str>,
        sync: SyncMode,
        server_backup: bool,
    ) -> Result<(Vec<Message>, Option<String>), CoreError> {
        if sync == SyncMode::Local {
            return self.cached_page_of(account, room, limit, before).await;
        }
        let backend = self.synced_backend_of(account, server_backup).await?;
        let page = backend.history(room, before, limit).await?;
        if sync == SyncMode::Server {
            // 🚫 看一眼不寫庫。⚠️ 這時的 `next` 是 server 的翻頁 token。
            return Ok((page.events, page.next));
        }
        // `Both`：寫進去**等它落地**，再從本地讀同一頁 —— 回的形狀因此跟 `Local` 一樣。
        let (cache, me) = self.server_cache_and_me(account)?;
        let events = page.events;
        cache
            .run(move |cache| cache.upsert_messages(&me, &events).map(|_| ()))
            .await?;
        self.cached_page_of(account, room, limit, before).await
    }

    /// 純本地的一頁。⚠️ `before` 在這裡是 `r_seq` 的數字，🚫 不是 server 的翻頁 token。
    async fn cached_page_of(
        &self,
        account: &AccountDir,
        room: &str,
        limit: u32,
        before: Option<&str>,
    ) -> Result<(Vec<Message>, Option<String>), CoreError> {
        let (cache, me) = self.server_cache_and_me(account)?;
        let before = parse_before_r_seq(before)?;
        let messages = cache.read().await.history(&me, room, before, limit)?;
        Ok(cached_page(messages))
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
    /// 那個 server 的快取（寫入者＋讀連線）與「我是誰」。**新的路徑都走這個**。
    ///
    /// 📎 舊的 [`Core::cache_and_me`] 還在：媒體那幾條會抓著 `&mut Cache` 跨越網路 I/O
    /// （邊下載邊寫），塞不進「一個工作 = 一個交易」，所以它們維持自己的連線（daemon-runtime §2.2）。
    pub(crate) fn server_cache_and_me(
        &self,
        account: &AccountDir,
    ) -> Result<(std::sync::Arc<crate::server_cache::ServerCache>, String), CoreError> {
        let session = self.session_of(account)?;
        let cache = self.server_cache_of(account, &session.server)?;
        Ok((cache, session.user_id))
    }

    pub(crate) fn cache_and_me(
        &self,
        account: &AccountDir,
    ) -> Result<(wbf_sdk::cache::Cache, String), CoreError> {
        let session = self.session_of(account)?;
        let cache = self.cache_of(account, &session.server)?;
        Ok((cache, session.user_id))
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

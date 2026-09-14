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
use wbf_sdk::{Cipher, EventPage, IncomingEvent};

use crate::accounts::AccountDir;
use crate::backend_choice::{get_backend_for, BackendKind, MethodHome};
use crate::error::{CoreError, CoreErrorKind};
use crate::{Core, Target};
use wbf_sdk::Transport;

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
    /// 往回翻的錨：**這一頁要比它舊**的那則的 `event_id`（上一頁的 `next`）。三種 `sync` 都一樣。
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
                // ⚠️ 兩種「找不到」講的不是同一件事，🚫 不要都叫人去 `sync=both`
                //（`Both` 走到這裡，代表上游剛剛才問過）（PR #32 審查 salvia🟡1）。
                let why = match sync {
                    SyncMode::Both => "the homeserver does not list it either",
                    _ => "it is not in the local cache; try sync=both",
                };
                CoreError::new(CoreErrorKind::NoSuchAccount, format!("{room}: {why}"))
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
    ///     before: 上一頁的 `next`（那頁最舊那則的 `event_id`）；None 從最新開始, example: Some("$old")
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
    ///
    /// 🚨 **`before` 與回傳的 `next` 都是 `event_id`**（這一頁最舊那則）—— UI 不分 server 是誰，
    /// 一律拿手上最舊那則往回問（chat-model §4.3；維護者 2026-09-14）。
    ///
    /// | `sync` | 做什麼 |
    /// |---|---|
    /// | `Local` | 只讀本地。🚫 **非 fork server 的房間不答**（沒有 `r_seq` 排不出順序，那種房「不快取、總是詢問」） |
    /// | `Server` | 問上游、不寫庫 |
    /// | `Both` | **永遠問上游** → 寫進去 → 用這一頁的 `event_id` 從本地讀回，**順序照上游** |
    ///
    /// ⚠️ `Both` 現在**每一頁都問上游**，🚫 不先判「本地有沒有洞」：本地的 `r_seq` 天生不連號
    /// （reaction／edit／redaction 併進目標不存列、看不到的事件 server 會跳過、超大事件被跨過），
    /// 所以「連號＝沒洞」判不出來。要省這一趟，得記 server 保證過的範圍（之後的事）。
    async fn page_of(
        &self,
        account: &AccountDir,
        room: &str,
        limit: u32,
        before: Option<&str>,
        sync: SyncMode,
        server_backup: bool,
    ) -> Result<(Vec<Message>, Option<String>), CoreError> {
        // 🚫 `limit = 0` 在入口就擋（PR #36 審查 cirno💡1）：wbf 那條會送 `Recent{limit: 0}` 拿回空頁、`next` 是 None，
        // UI 會讀成「到頭了」；matrix 那條卻回 Usage —— 同一個輸入兩種答案。三種 `sync` 都經過這裡，所以只擋這一次。
        if limit == 0 {
            return Err(CoreError::new(
                CoreErrorKind::Usage,
                "limit must be at least 1: an empty page means \"no older messages\", so asking for zero would say that falsely",
            ));
        }
        if sync == SyncMode::Local {
            return self.cached_page_of(account, room, limit, before).await;
        }
        let page = self
            .upstream_page_of(account, room, limit, before, server_backup)
            .await?;
        if sync == SyncMode::Server {
            // 🚫 看一眼不寫庫：只折這一頁裡的關係事件。
            let messages = wbf_sdk::event_json::messages_from_incoming(room, &page.events);
            return Ok((messages, page.next));
        }
        // `Both`：**原樣**寫進去等它落地（local-cache-db.md §7），再用這一頁的 event_id 讀回。
        // ⭐ 上游決定**哪幾則、什麼順序**，本地決定**每一則長什麼樣**（已解密的明文不會被密文蓋掉、
        // 別頁的 edit／redact／reaction 已經套上、`hidden` 的不出來）。
        // 🚫 不從本地「照 r_seq 重讀一頁」：非 fork server 的事件沒有 r_seq。
        let (cache, me) = self.server_cache_and_me(account)?;
        let event_ids: Vec<String> = page
            .events
            .iter()
            .filter_map(IncomingEvent::find_event_id)
            .map(str::to_string)
            .collect();
        let events = page.events;
        let (writer_me, writer_room) = (me.clone(), room.to_string());
        cache
            .run(move |cache| {
                cache
                    .upsert_events(&writer_me, &writer_room, &events)
                    .map(|_| ())
            })
            .await?;
        let read_back = cache
            .read()
            .await
            .list_messages_by_event_ids(&me, room, &event_ids)?;
        // 🚨 `next` 用**上游那一頁**的，🚫 不用讀回來的：最舊那則如果被 `hidden` 了，
        // 讀回來的最後一則會比較新，UI 拿它往回問就會一直拿到同一頁。
        Ok((read_back, page.next))
    }

    /// 問上游一頁。backend 照探測：講 wbf 的就用 `Event/Recent` 點名這個房間，否則走 matrix-sdk。
    ///
    /// ⚠️ `room.history` 沒有 `transport` 參數，所以用**預設的那條**（`Transport::default()` ＝ ws）——
    /// 對方講 wbf 就用 wbf，不講就 fallback 到 matrix-sdk（backend_choice）。
    async fn upstream_page_of(
        &self,
        account: &AccountDir,
        room: &str,
        limit: u32,
        before: Option<&str>,
        server_backup: bool,
    ) -> Result<EventPage, CoreError> {
        let speaks_wbf = self.get_backend_kind(account).await == BackendKind::WbfSdk;
        let backend = get_backend_for(Transport::default(), speaks_wbf, MethodHome::BothSides)?;
        if backend == BackendKind::WbfSdk {
            match self.g_seq_anchor_of(account, room, before).await? {
                Anchor::Found(before_g_seq) => {
                    return self
                        .wbf_room_page_of(account, room, limit, before_g_seq)
                        .await;
                }
                // ⚠️ wbf 要的是 `g_seq`，而它只在本地有。那則不在本地（`sync=server` 不寫庫，
                // 所以它給的 `next` 本地查不到）→ 改走 `/context`：它只要 `event_id`。
                Anchor::NotInLocalCache => {}
            }
        }
        let backend = self.synced_backend_of(account, server_backup).await?;
        Ok(backend.history(room, before, limit).await?)
    }

    /// 把 `before`（event_id）換成 wbf 聽得懂的 `g_seq`。
    ///
    /// Return:
    ///     Ok(Anchor::Found(None))     沒帶 `before`：最新的一頁
    ///     Ok(Anchor::Found(Some(g)))  本地有、而且有 `g_seq`
    ///     Ok(Anchor::NotInLocalCache) 這個帳號本地沒有這則，或它沒有 `g_seq`（呼叫端改走 `/context`）
    async fn g_seq_anchor_of(
        &self,
        account: &AccountDir,
        room: &str,
        before: Option<&str>,
    ) -> Result<Anchor, CoreError> {
        let Some(event_id) = before else {
            return Ok(Anchor::Found(None));
        };
        let (cache, me) = self.server_cache_and_me(account)?;
        let position = cache
            .read()
            .await
            .find_event_position(&me, room, event_id)?;
        Ok(match position.and_then(|position| position.g_seq) {
            Some(g_seq) => Anchor::Found(Some(g_seq)),
            None => Anchor::NotInLocalCache,
        })
    }

    /// wbf 的一頁房間歷史：`Event/Recent{ rooms: [這個房], before }`（wbfuwunel #51）。
    async fn wbf_room_page_of(
        &self,
        account: &AccountDir,
        room: &str,
        limit: u32,
        before_g_seq: Option<i64>,
    ) -> Result<EventPage, CoreError> {
        let mut client = self
            .client_of(account, Transport::default(), MethodHome::BothSides)
            .await?;
        client.hello(HISTORY_CLIENT_NAME).await?;
        let request = wbf_sdk::protocol::RecentRequest {
            rooms: Some(vec![room.to_string()]),
            // server 在上限以上會 clamp，client 先 clamp 才算得出「窗滿了沒」（protocol.rs）。
            limit: limit.min(wbf_sdk::protocol::RECENT_MAX_LIMIT),
            cg_seq: None,
            before: before_g_seq,
            batch: None,
        };
        let mut raws: Vec<serde_json::Value> = Vec::new();
        let mut on_batch = |_meta: &wbf_sdk::protocol::BatchMeta,
                            batch: Vec<serde_json::Value>|
         -> Result<(), wbf_sdk::SdkError> {
            raws.extend(batch);
            Ok(())
        };
        client
            .recent_window(
                &request,
                wbf_sdk::client::RECENT_FIRST_WINDOW_TIMEOUT,
                &mut on_batch,
            )
            .await?;
        // 回來的是新到舊（g_seq 遞減），跟 `/messages` 往回翻同一個方向。
        // WS 這條路不解密：密文原樣交出去（local-cache-db.md §7.2）。
        Ok(EventPage::from_upstream_order(
            raws.into_iter().map(IncomingEvent::from_ws_json).collect(),
        ))
    }

    /// 純本地的一頁。
    ///
    /// 🚫 **非 fork server 的房間不答**（維護者 2026-09-14：那種房「不快取、總是詢問」）——
    /// 沒有 `r_seq` 就排不出順序，而拿時間戳排是錯的（chat-model §4.3）。
    async fn cached_page_of(
        &self,
        account: &AccountDir,
        room: &str,
        limit: u32,
        before: Option<&str>,
    ) -> Result<(Vec<Message>, Option<String>), CoreError> {
        let (cache, me) = self.server_cache_and_me(account)?;
        let reader = cache.read().await;
        let before_r_seq = match before {
            None => None,
            Some(event_id) => match reader.find_event_position(&me, room, event_id)? {
                Some(position) => match position.r_seq {
                    Some(r_seq) => Some(r_seq),
                    None => return Err(refuse_local_paging_without_r_seq(room)),
                },
                None => {
                    return Err(CoreError::new(
                        CoreErrorKind::Usage,
                        format!(
                            "{event_id} is not in the local cache for this account, so there is nothing \
                             to page back from; use sync=both"
                        ),
                    ))
                }
            },
        };
        let messages = reader.history(&me, room, before_r_seq, limit)?;
        if messages.iter().any(|message| message.r_seq.is_none()) {
            return Err(refuse_local_paging_without_r_seq(room));
        }
        let next = messages.last().map(|message| message.id.clone());
        Ok((messages, next))
    }

    /// 開 backend 並做一次增量 sync（timeout 0）：房間列表與新事件到 store，之後才看得到現況。
    ///
    /// 🚧 **這是 matrix-sdk 那一套**。房間歷史（`history`／`files`）已經照探測分派了
    /// （`upstream_page_of`，`MethodHome::BothSides`）；其餘房間命令（`list`／`get`／`send_text`）
    /// 還在「還沒有 ws」的清單上（`MethodHome::StillOnMatrixSdk`），所以不管 `transport`、
    /// 不管對方是不是 wbf 都走這裡。⭐ 清單上的東西**沒有選擇**，所以這裡刻意不看 `transport`。
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

/// 跟 server 講房間歷史時報的 client 名字。⚠️ server 會 log 它。
const HISTORY_CLIENT_NAME: &str = "wbf-client history";

/// `before`（event_id）在 wbf 那條路上換不換得到 `g_seq`。
enum Anchor {
    /// 換到了；`None` ＝ 沒帶 `before`（最新的一頁）。
    Found(Option<i64>),
    /// 這個帳號本地沒有那則，或它沒有 `g_seq` —— 呼叫端改走 `/context`。
    NotInLocalCache,
}

fn refuse_local_paging_without_r_seq(room: &str) -> CoreError {
    CoreError::new(
        CoreErrorKind::Usage,
        format!(
            "{room} has no r_seq (a plain Matrix server), so its history is never answered from the local \
             cache alone; use sync=both"
        ),
    )
}

/// `--type` 對的是我們模型的 kind 名（text／file／deleted／system／unsupported），
/// 或 `Unsupported` 帶的原始 event type。
fn kind_matches(message: &Message, wanted: &str) -> bool {
    match &message.kind {
        MessageKind::Text { .. } => wanted == "text" || wanted == "m.room.message",
        MessageKind::File { .. } => wanted == "file" || wanted == "org.wbftw.wbfuwunel.file",
        MessageKind::Deleted { .. } => wanted == "deleted",
        MessageKind::Undecryptable => wanted == "undecryptable",
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

    const SERVER: &str = "http://127.0.0.1:1";
    const ME: &str = "@a:localhost";

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("wbf-core-rooms-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 解鎖好、有一個帳號（session 指向沒人在聽的位址 —— 本地模式不該碰網路）。
    fn core_with_account(dir: &std::path::Path) -> (Core, AccountDir) {
        wbf_sdk::vault::Vault::create(dir, &wbf_sdk::Unlock::NoPassphrase).unwrap();
        let core = Core::open(dir);
        core.unlock(None).unwrap();
        let vault = core.vault().unwrap();
        let account = AccountDir::locate(dir, &vault.account_dir_key(), SERVER, ME).unwrap();
        std::fs::create_dir_all(&account.dir).unwrap();
        vault
            .seal_session(
                &account.session_path(),
                &wbf_sdk::login::Session {
                    server: SERVER.to_string(),
                    user_id: ME.to_string(),
                    device_id: "DEV".to_string(),
                    access_token: "syt_nobody_is_listening".to_string(),
                    store_dir: None,
                },
            )
            .unwrap();
        (core, account)
    }

    fn text(id: &str, r_seq: Option<i64>, ts: u64) -> IncomingEvent {
        let mut event = serde_json::json!({
            "type": "m.room.message", "event_id": id, "room_id": "!r", "sender": "@b:localhost",
            "origin_server_ts": ts, "content": { "msgtype": "m.text", "body": id },
        });
        if let Some(r_seq) = r_seq {
            event["unsigned"] = serde_json::json!({
                wbf_sdk::protocol::R_SEQ_KEY: r_seq, wbf_sdk::protocol::G_SEQ_KEY: r_seq * 10,
            });
        }
        IncomingEvent::Plain { event }
    }

    fn seed(core: &Core, account: &AccountDir, events: Vec<IncomingEvent>) {
        let (cache, me) = core.server_cache_and_me(account).unwrap();
        cache
            .run_blocking(move |cache| cache.upsert_events(&me, "!r", &events).map(|_| ()))
            .unwrap();
    }

    fn local_query(before: Option<&str>) -> HistoryQuery {
        HistoryQuery {
            room: "!r".to_string(),
            limit: 2,
            before: before.map(str::to_string),
            sync: SyncMode::Local,
            types: Vec::new(),
            sender: None,
        }
    }

    fn me() -> Target {
        Target {
            user: Some(ME.to_string()),
            ..Target::default()
        }
    }

    fn ids(page: &MessagePage) -> Vec<&str> {
        page.events
            .iter()
            .map(|message| message.id.as_str())
            .collect()
    }

    /// fork 房的本地翻頁：`before` 是 **event_id**、`next` 是這頁最舊那則的 event_id，
    /// 拿 `next` 接著問就接得上。
    #[tokio::test]
    async fn local_paging_walks_back_by_event_id() {
        let dir = scratch("local-paging");
        let (core, account) = core_with_account(&dir);
        seed(
            &core,
            &account,
            (1..=5)
                .map(|seq| text(&format!("${seq}"), Some(seq), seq as u64))
                .collect(),
        );

        let first = core.history(&local_query(None), &me()).await.unwrap();
        assert_eq!(ids(&first), ["$5", "$4"]);
        assert_eq!(first.next.as_deref(), Some("$4"), "next 是這頁最舊那則");

        let second = core
            .history(&local_query(first.next.as_deref()), &me())
            .await
            .unwrap();
        assert_eq!(ids(&second), ["$3", "$2"], "拿 next 接著問要接得上");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 🚫 **一般 Matrix 房（沒有 `r_seq`）本地不答**（維護者 2026-09-14：「不快取，總是詢問」）——
    /// 沒有 `r_seq` 就只剩時間戳可排，而那是錯的（chat-model §4.3）。第一頁、翻頁都一樣。
    #[tokio::test]
    async fn a_room_without_r_seq_is_never_answered_from_the_local_cache() {
        let dir = scratch("plain-room");
        let (core, account) = core_with_account(&dir);
        // 時間戳故意跟真實順序相反：答了就會排錯。
        seed(
            &core,
            &account,
            vec![text("$old", None, 900), text("$new", None, 100)],
        );

        for before in [None, Some("$new")] {
            let error = core.history(&local_query(before), &me()).await.unwrap_err();
            assert_eq!(error.kind, CoreErrorKind::Usage, "before={before:?}");
            assert!(error.message.contains("sync=both"), "{}", error.message);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 🚫 `limit = 0` 在入口就擋：空頁的意思是「沒有更舊的了」，要 0 則會假裝那件事（PR #36 審查 cirno💡1）。
    /// ⚠️ 用 `server` 模式驗：它會打上游，所以要在**連網之前**就擋下來（session 指向沒人在聽的位址，連了就是 Network 錯）。
    #[tokio::test]
    async fn a_zero_limit_is_refused_before_anything_is_asked() {
        let dir = scratch("zero-limit");
        let (core, _account) = core_with_account(&dir);
        for sync in [SyncMode::Local, SyncMode::Server, SyncMode::Both] {
            let query = HistoryQuery {
                limit: 0,
                sync,
                ..local_query(None)
            };
            let error = core.history(&query, &me()).await.unwrap_err();
            assert_eq!(
                error.kind,
                CoreErrorKind::Usage,
                "sync={sync:?}: {}",
                error.message
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 錨點不在本地（沒同步過、或上一頁是 `sync=server` 拿的）：🚫 不答一頁空的，說清楚。
    /// ⚠️ 空頁會被 UI 讀成「到頭了」。
    #[tokio::test]
    async fn an_anchor_that_is_not_in_the_local_cache_is_refused_not_answered_empty() {
        let dir = scratch("unknown-anchor");
        let (core, account) = core_with_account(&dir);
        seed(&core, &account, vec![text("$1", Some(1), 1)]);

        let error = core
            .history(&local_query(Some("$never-seen")), &me())
            .await
            .unwrap_err();
        assert_eq!(error.kind, CoreErrorKind::Usage);
        assert!(
            error.message.contains("not in the local cache"),
            "{}",
            error.message
        );
        let _ = std::fs::remove_dir_all(&dir);
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

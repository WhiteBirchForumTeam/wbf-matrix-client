//! 傳統格式附件的下載（`kind` 2、3：標準 Matrix 的 `file`／`url`，/docs/design/media/media-download.md §12）。
//!
//! 一個檔一個 task：它是這個檔主檔（池格式 v2）唯一的寫入者，GET 要還在下載的明文也問它。下載本身（HTTP、解密）在 sdk 的
//! `matrix_media::stream_matrix_media`，明文經一個有界 channel 進來，這個 task 寫進池、回答 GET；讀完自動驗、記結果。
//! 跟 wbf 分塊的下載處理端（`download_queue.rs`）分開：那邊是 `Download` 線上一塊一塊的請求，這邊是一個 HTTP 從頭讀到尾，
//! 🚫 seek、🚫 續傳（§12.4）；共用的只有池、`cache.db` 的列與推播 `media.download`。

use std::collections::{HashMap, HashSet};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use tokio::sync::{mpsc, oneshot};
use wbf_sdk::cache::MediaDescription;
use wbf_sdk::chat::MatrixAttachment;
use wbf_sdk::media;
use wbf_sdk::media_kind::{MediaKind, Verification};
use wbf_sdk::media_pool::{MediaPool, PoolWriter, SEGMENT_SIZE};
use zeroize::Zeroizing;

use crate::accounts::AccountDir;
use crate::download_queue::{cancelled_error, SeekPiece};
use crate::error::{CoreError, CoreErrorKind};
use crate::event::{CoreEvent, DownloadState, EventSink};
use crate::media_ops::{other_description_error, MediaJob};
use crate::server_cache::ServerCache;
use crate::Core;

/// GET 一次最多拿這麼多明文。
const PIECE: u64 = 64 * 1024;
/// 下載途中的進度推播，每個檔最多這麼久一則（/docs/design/media/media-download.md §5.5）。
const PROGRESS_EVERY: Duration = Duration::from_secs(1);
/// 下載那頭跟這個 task 之間最多積幾段明文：池寫得慢就晚一點讀 HTTP 的下一段（背壓）。
const PIECES_IN_FLIGHT: usize = 4;

type Waiter = oneshot::Sender<Result<(), CoreError>>;
type PieceReply = oneshot::Sender<Result<SeekPiece, CoreError>>;

enum Command {
    /// GET 要明文第 `position` 個 byte 起的那一段：已經寫進主檔就給，還沒就等
    Read { position: u64, reply: PieceReply },
    /// 等它結束（匯出）
    Wait(Waiter),
    /// `media.cancel`、登出
    Cancel,
}

/// 一個正在下載的傳統檔。
pub(crate) struct MatrixTransfer {
    account_dir: PathBuf,
    /// 這個 task 照哪一份描述（金鑰、hash、大小）在下載：另一份描述🚫 掛上來
    description: MediaDescription,
    pending_name: String,
    commands: mpsc::UnboundedSender<Command>,
}

impl MatrixTransfer {
    /// 明文第 `position` 個 byte 起的那一段（最多 64 KiB）：已經寫進主檔就馬上給，還沒就等它寫到。
    ///
    /// Return:
    ///     Ok(SeekPiece)   `start` 就是 `position`
    ///     Err(...)        下載失敗、被取消、或 task 已經收了（呼叫者重開 GET 會走池）
    pub(crate) async fn read_at(&self, position: u64) -> Result<SeekPiece, CoreError> {
        let (reply, answer) = oneshot::channel();
        self.commands
            .send(Command::Read { position, reply })
            .map_err(|_| transfer_gone())?;
        answer.await.map_err(|_| transfer_gone())?
    }
}

fn transfer_gone() -> CoreError {
    CoreError::new(
        CoreErrorKind::Io,
        "the download of this file stopped; open it again",
    )
}

/// 這個程序裡所有正在下載的傳統檔，key 是（server 目錄、mxc）：同一台 server 的同一個 mxc 只有一個 task 在寫。
#[derive(Default)]
pub(crate) struct MatrixTransfers {
    by_file: Mutex<HashMap<(PathBuf, String), Arc<MatrixTransfer>>>,
}

impl MatrixTransfers {
    fn files(&self) -> MutexGuard<'_, HashMap<(PathBuf, String), Arc<MatrixTransfer>>> {
        self.by_file
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub(crate) fn find(&self, server_dir: &Path, mxc: &str) -> Option<Arc<MatrixTransfer>> {
        self.files()
            .get(&(server_dir.to_path_buf(), mxc.to_string()))
            .cloned()
    }

    /// 這台 server 上正在寫的暫存名（掃描不准碰，/docs/design/media/media-download.md §4.3）。
    pub(crate) fn list_pending_names(&self, server_dir: &Path) -> HashSet<String> {
        self.files()
            .iter()
            .filter(|((dir, _), _)| dir == server_dir)
            .map(|(_, transfer)| transfer.pending_name.clone())
            .collect()
    }
}

/// 同一個 mxc 正照另一份描述在下載（/docs/design/media/media-download.md §12.1）。
fn described_otherwise(mxc: &str) -> CoreError {
    CoreError::new(
        CoreErrorKind::Integrity,
        format!("{mxc} is being downloaded under another description (hash or size differs); this one is refused"),
    )
}

/// 這個 task 照著下載的那一列不在了：被 `media.delete_local` 刪了、或刪了又由別則事件重建。
fn record_changed_error(mxc: &str) -> CoreError {
    CoreError::new(
        CoreErrorKind::Integrity,
        format!("the local record of {mxc} was deleted or replaced while it was downloading; download it again from its message"),
    )
}

/// 明文總長換成段數（`media.download` 的 `done`／`total`，傳統檔沒有塊、用池的 64 KiB 段）；不知道總長是 0。
fn segments_of(size: Option<u64>) -> u32 {
    size.map(|size| u32::try_from(size.div_ceil(u64::from(SEGMENT_SIZE))).unwrap_or(u32::MAX))
        .unwrap_or(0)
}

impl Core {
    /// 下載一個傳統檔（`media.download`／`open`／`export_to` 的 `kind` 2、3）：池裡已經完整就不下載；
    /// 正在下載就把 `waiter` 掛上去；都沒有就起一個 task（/docs/design/media/media-download.md §12.1）。
    ///
    /// Args:
    ///     attachment: 從這個帳號看得到的事件來的（金鑰在它的 `file`）
    ///     waiter: 要等它結束的話給一個；已經完整時直接丟掉（呼叫者看 `state` 就知道不用等）
    /// Return:
    ///     Ok(MediaJob)     `complete`（不下載）或 `downloading`
    ///     Err(Integrity)   本地這個 mxc 的列跟這則的描述對不上（格式、密文 hash、大小；不管下載完了沒），或正照另一份描述在下載
    ///     Err(...)         DB、池開不了
    pub(crate) async fn ensure_matrix_download(
        &self,
        account: &AccountDir,
        attachment: MatrixAttachment,
        waiter: Option<Waiter>,
    ) -> Result<MediaJob, CoreError> {
        let (cache, me) = self.server_cache_and_me(account)?;
        let pool = self.pool_of(account)?;
        let server_dir = account.server_dir();
        let mxc = attachment.mxc.clone();
        let description = MediaDescription::of_matrix_attachment(&attachment);
        let (mxc_here, description_here) = (mxc.clone(), description.clone());
        let (entry, pending_name) = cache
            .run(move |cache| {
                let entry = cache.media_begin(&mxc_here, &description_here)?;
                let pending_name = cache.media_pending_name(&mxc_here)?.ok_or_else(|| {
                    wbf_sdk::SdkError::Io(std::io::Error::other(
                        "the media row vanished after insert",
                    ))
                })?;
                Ok((entry, pending_name))
            })
            .await?;
        // 格式、密文 hash、大小有一樣不合就拒，不管下載完了沒、有沒有人在下載（維護者 2026-10-07，取代先前的「沒完成就換描述重來」，/docs/design/media/media-download.md §12.1）。
        if !media::is_same_matrix_file(&entry, &description) {
            return Err(other_description_error(&mxc, &entry));
        }
        let total = segments_of(attachment.size.or(entry.file_size));
        if media::open_complete(&pool, &entry).is_some() {
            return Ok(MediaJob {
                mxc,
                state: DownloadState::Complete,
                done: total,
                total,
                kind: Some(entry.kind),
                verified: Some(entry.verified),
            });
        }
        let downloading = MediaJob {
            mxc: mxc.clone(),
            state: DownloadState::Downloading,
            done: 0,
            total,
            kind: Some(entry.kind),
            verified: Some(Verification::Unknown),
        };
        let session = self.session_of(account)?;
        let key = (server_dir, mxc.clone());
        let mut files = self.matrix_transfers.files();
        if let Some(existing) = files.get(&key) {
            // 上面比過列，這裡消費端自己再問一次：比完到這裡之間，可能有別則事件換了描述、起了自己的 task。
            if !media::is_same_matrix_description(&existing.description, &description) {
                return Err(described_otherwise(&mxc));
            }
            if let Some(waiter) = waiter {
                // 那個 task 剛好收了：等的人拿到「它停了」，重叫一次就會走池或重起。
                if let Err(mpsc::error::SendError(Command::Wait(waiter))) =
                    existing.commands.send(Command::Wait(waiter))
                {
                    let _ = waiter.send(Err(transfer_gone()));
                }
            }
            return Ok(downloading);
        }
        let (commands, inbox) = mpsc::unbounded_channel();
        if let Some(waiter) = waiter {
            let _ = commands.send(Command::Wait(waiter));
        }
        files.insert(
            key.clone(),
            Arc::new(MatrixTransfer {
                account_dir: account.dir.clone(),
                description: description.clone(),
                pending_name: pending_name.clone(),
                commands,
            }),
        );
        drop(files);
        let task = TransferTask {
            key,
            transfers: self.matrix_transfers.clone(),
            attachment,
            total,
            server: session.server,
            access_token: session.access_token,
            user: me,
            cache,
            pool,
            events: self.events.clone(),
            pending_name,
            inbox,
            reads: Vec::new(),
            waiters: Vec::new(),
        };
        tokio::spawn(task.run());
        Ok(downloading)
    }

    /// `media.cancel` 的傳統檔那半。
    ///
    /// Return:
    ///     bool   true ＝ 這台 server 上有這個 mxc 在下載、交了取消
    pub(crate) fn cancel_matrix_download(&self, account: &AccountDir, mxc: &str) -> bool {
        match self.matrix_transfers.find(&account.server_dir(), mxc) {
            Some(transfer) => transfer.commands.send(Command::Cancel).is_ok(),
            None => false,
        }
    }

    /// 登出、換 session：這個帳號起的傳統下載全部停（🚫 續傳，半成品刪掉，§12.4）。
    pub(crate) fn stop_matrix_downloads_of(&self, account: &AccountDir) {
        let theirs: Vec<Arc<MatrixTransfer>> = self
            .matrix_transfers
            .files()
            .values()
            .filter(|transfer| transfer.account_dir == account.dir)
            .cloned()
            .collect();
        for transfer in theirs {
            let _ = transfer.commands.send(Command::Cancel);
        }
    }
}

/// 一個傳統檔的下載 task。
struct TransferTask {
    key: (PathBuf, String),
    transfers: Arc<MatrixTransfers>,
    attachment: MatrixAttachment,
    /// 推播的 `total`：跟 `media.download` 回的同一個數（事件沒給大小就用列上的）
    total: u32,
    server: String,
    access_token: String,
    user: String,
    cache: Arc<ServerCache>,
    pool: MediaPool,
    events: EventSink,
    pending_name: String,
    inbox: mpsc::UnboundedReceiver<Command>,
    /// 還沒寫到的 GET
    reads: Vec<(u64, PieceReply)>,
    waiters: Vec<Waiter>,
}

/// 下載怎麼結束的。
enum Outcome {
    /// 完成（驗不過也是完成，結果記在列上），帶池檔名
    Complete(String),
    Cancelled,
    Failed(CoreError),
}

impl TransferTask {
    async fn run(mut self) {
        let outcome = match self.download().await {
            Ok(Some(pool_file)) => Outcome::Complete(pool_file),
            Ok(None) => Outcome::Cancelled,
            Err(error) => Outcome::Failed(error),
        };
        if !matches!(outcome, Outcome::Complete(_)) {
            // 🚫 續傳（§12.4）：半成品刪掉、列回到「還沒下載」。
            let _ = self.pool.discard_pending(&self.pending_name);
            let (mxc, own_name) = (self.key.1.clone(), self.pending_name.clone());
            // 列已經被刪掉、或刪了又由別則事件重建（`media.delete_local`）：🚫 去清別人的列。
            self.cache.post(
                move |cache| match cache.media_pending_name(&mxc)? {
                    Some(name) if name == own_name => cache.media_reset(&mxc),
                    _ => Ok(()),
                },
                Vec::new(),
            );
        }
        // 先從登記表拿掉：之後的 GET 走池（完成了）或重起一個（沒完成）。還在路上的命令收完再回。
        self.transfers.files().remove(&self.key);
        self.inbox.close();
        while let Ok(command) = self.inbox.try_recv() {
            match command {
                Command::Read { position, reply } => self.reads.push((position, reply)),
                Command::Wait(waiter) => self.waiters.push(waiter),
                Command::Cancel => {}
            }
        }
        let mxc = self.key.1.clone();
        let result = match &outcome {
            Outcome::Complete(pool_file) => {
                for (position, reply) in std::mem::take(&mut self.reads) {
                    let _ = reply.send(read_from_pool(&self.pool, pool_file, position));
                }
                Ok(())
            }
            Outcome::Cancelled => Err(cancelled_error(&mxc)),
            Outcome::Failed(error) => Err(error.clone()),
        };
        if let Err(error) = &result {
            for (_, reply) in std::mem::take(&mut self.reads) {
                let _ = reply.send(Err(error.clone()));
            }
            match &outcome {
                Outcome::Cancelled => self.push(DownloadState::Cancelled, 0, None, None),
                _ => self.push(DownloadState::Failed, 0, None, Some(error.message.clone())),
            }
        }
        for waiter in std::mem::take(&mut self.waiters) {
            let _ = waiter.send(result.clone());
        }
    }

    /// Return:
    ///     Ok(Some(pool_file))   完成、進了池、列記好了（含驗證結果）
    ///     Ok(None)              被取消
    ///     Err(...)              下載、解密、寫檔、DB 任一步失敗
    async fn download(&mut self) -> Result<Option<String>, CoreError> {
        let mxc = self.key.1.clone();
        let total = self.total;
        // 起 task 之前看到的列可能已經舊了：上一個 task 在那之後剛收尾、從登記表拿掉，這裡才補上（PR #75 審查 salvia 1）。
        // 開頭再問一次列：已經完整就直接收尾、🚫 重下；列被刪了或換了就🚫 下載。
        let own = MediaDescription::of_matrix_attachment(&self.attachment);
        let recorded = self.cache.read().await.find_media(&mxc)?;
        let Some(entry) = recorded.filter(|entry| media::is_same_matrix_file(entry, &own)) else {
            return Err(record_changed_error(&mxc));
        };
        if let (Some(pool_file), Some(_)) = (
            entry.pool_file.clone(),
            media::open_complete(&self.pool, &entry),
        ) {
            self.push(DownloadState::Complete, total, Some(entry.verified), None);
            return Ok(Some(pool_file));
        }
        // 🚫 續傳（§12.4）：之前留下的半成品（daemon 重開、取消）丟掉，從頭來。
        let _ = self.pool.discard_pending(&self.pending_name);
        let mut writer = self.pool.create_pending(&self.pending_name, &mxc)?;
        self.push(DownloadState::Downloading, 0, None, None);
        let (sink, mut pieces) = mpsc::channel::<Vec<u8>>(PIECES_IN_FLIGHT);
        let (server, token, attachment) = (
            self.server.clone(),
            self.access_token.clone(),
            self.attachment.clone(),
        );
        let fetch = tokio::spawn(async move {
            wbf_sdk::matrix_media::stream_matrix_media(&server, &token, &attachment, &sink).await
        });
        let mut last_push = Instant::now();
        loop {
            tokio::select! {
                piece = pieces.recv() => match piece {
                    Some(piece) => {
                        writer.write_all(&piece)?;
                        self.answer_reads(&mut writer)?;
                        if last_push.elapsed() >= PROGRESS_EVERY {
                            last_push = Instant::now();
                            let done = u32::try_from(writer.segments_written()).unwrap_or(u32::MAX);
                            self.push(DownloadState::Downloading, done, None, None);
                        }
                    }
                    // 下載那頭結束了（讀完、或失敗）：結果看 `fetch`。
                    None => break,
                },
                command = self.inbox.recv() => match command {
                    Some(Command::Read { position, reply }) => {
                        self.reads.push((position, reply));
                        self.answer_reads(&mut writer)?;
                    }
                    Some(Command::Wait(waiter)) => self.waiters.push(waiter),
                    Some(Command::Cancel) | None => {
                        fetch.abort();
                        return Ok(None);
                    }
                },
            }
        }
        let end = fetch.await.map_err(|error| {
            CoreError::new(
                CoreErrorKind::Io,
                format!("the download task died: {error}"),
            )
        })??;
        // 有 hash 可比就先報「驗證中」，收尾那則再報結果（/docs/design/media/media-download.md §12.3）。
        // hash 是邊下載邊算的（sdk 那頭），到這裡已經比完；這一則讓「下載完」與「可以信」在推播上分開。
        if self.attachment.kind == MediaKind::MatrixEncrypted {
            self.push(DownloadState::Verifying, total, None, None);
        }
        // 收尾前再問一次列：下載途中列被刪了（`media.delete_local`）、或刪了又由別則事件重建，這份結果🚫 記到它上面。走失敗的路：半成品丟掉。
        let (recorded, recorded_name) = {
            let reader = self.cache.read().await;
            (reader.find_media(&mxc)?, reader.media_pending_name(&mxc)?)
        };
        let is_still_our_row = recorded_name.as_deref() == Some(self.pending_name.as_str())
            && recorded.is_some_and(|entry| media::is_same_matrix_file(&entry, &own));
        if !is_still_our_row {
            return Err(record_changed_error(&mxc));
        }
        let finished = writer.finish()?;
        self.pool.adopt(&self.pending_name, &finished.hash_hex)?;
        let bytes_on_disk = self.pool.bytes_on_disk(&finished.hash_hex)?;
        let (mxc_here, hash, segments, plain_len, verified) = (
            mxc.clone(),
            finished.hash_hex.clone(),
            finished.segments,
            finished.plain_len,
            end.verified,
        );
        self.cache
            .run(move |cache| {
                cache.media_finish(
                    &mxc_here,
                    &hash,
                    segments,
                    plain_len,
                    bytes_on_disk,
                    verified,
                )
            })
            .await?;
        let done = u32::try_from(finished.segments).unwrap_or(u32::MAX);
        self.push(DownloadState::Complete, done, Some(end.verified), None);
        Ok(Some(finished.hash_hex))
    }

    /// 已經寫進主檔（封好的段）涵蓋到的 GET 先回。
    fn answer_reads(&mut self, writer: &mut PoolWriter) -> Result<(), CoreError> {
        let sealed = writer.sealed_plain_len();
        let (ready, waiting): (Vec<_>, Vec<_>) = std::mem::take(&mut self.reads)
            .into_iter()
            .partition(|(position, _)| *position < sealed);
        self.reads = waiting;
        for (position, reply) in ready {
            let end = sealed.min(position + PIECE);
            let plain = writer.read_sealed(position, end)?;
            let _ = reply.send(Ok(SeekPiece {
                start: position,
                plain,
            }));
        }
        Ok(())
    }

    fn push(
        &self,
        state: DownloadState,
        done: u32,
        verified: Option<Verification>,
        reason: Option<String>,
    ) {
        self.events.emit(CoreEvent::MediaDownload {
            user: self.user.clone(),
            mxc: self.key.1.clone(),
            state,
            done,
            total: self.total,
            verified,
            reason,
        });
    }
}

/// 完成之後還在等的 GET：從池檔給。
fn read_from_pool(
    pool: &MediaPool,
    pool_file: &str,
    position: u64,
) -> Result<SeekPiece, CoreError> {
    let mut reader = pool.open_read(pool_file)?;
    let plain_len = reader.plain_len();
    if position >= plain_len {
        return Err(CoreError::new(
            CoreErrorKind::Usage,
            format!("position {position} is past the end ({plain_len})"),
        ));
    }
    let end = plain_len.min(position + PIECE);
    reader.seek(SeekFrom::Start(position))?;
    let mut plain = Zeroizing::new(vec![0u8; (end - position) as usize]);
    reader.read_exact(&mut plain)?;
    Ok(SeekPiece {
        start: position,
        plain,
    })
}

#[cfg(test)]
mod tests {
    use std::io::Read;
    use std::sync::Arc;
    use std::time::Duration;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::sync::broadcast::Receiver;
    use tokio::sync::{mpsc, oneshot, Notify};
    use wbf_sdk::cache::MediaDescription;
    use wbf_sdk::login::SessionBackend;
    use wbf_sdk::media_kind::{MediaKind, Verification};

    use crate::accounts::AccountDir;
    use crate::error::CoreErrorKind;
    use crate::event::{CoreEvent, DownloadState};
    use crate::test_support::*;
    use crate::{Core, MediaRef, Target};

    use super::{segments_of, Command, MatrixTransfer, TransferTask};

    /// 一台只回媒體 bytes 的 HTTP server。`hold` 給了就先送前 `head` 個 byte、等它 notify 才送剩下的（測「邊下載邊讀」與取消）。
    async fn media_server(body: Vec<u8>, hold: Option<(usize, Arc<Notify>)>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let body = body.clone();
                let hold = hold.clone();
                tokio::spawn(async move {
                    let mut head = Vec::new();
                    let mut byte = [0u8; 1];
                    while !head.ends_with(b"\r\n\r\n") {
                        if socket.read(&mut byte).await.unwrap_or(0) == 0 {
                            return;
                        }
                        head.push(byte[0]);
                    }
                    let header = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = socket.write_all(header.as_bytes()).await;
                    match hold {
                        Some((first, notify)) => {
                            let _ = socket.write_all(&body[..first]).await;
                            let _ = socket.flush().await;
                            notify.notified().await;
                            let _ = socket.write_all(&body[first..]).await;
                        }
                        None => {
                            let _ = socket.write_all(&body).await;
                        }
                    }
                    let _ = socket.shutdown().await;
                });
            }
        });
        base
    }

    fn sample(len: usize) -> Vec<u8> {
        (0..len).map(|index| (index * 13 + 5) as u8).collect()
    }

    /// 用上游的加密器加密（就是 Element 這類 client 的格式），回（密文、事件裡的 `file`）。
    fn encrypt(plain: &[u8], mxc: &str) -> (Vec<u8>, serde_json::Value) {
        let mut source = plain;
        let mut encryptor = matrix_sdk_crypto::AttachmentEncryptor::new(&mut source);
        let mut cipher = Vec::new();
        encryptor.read_to_end(&mut cipher).unwrap();
        let mut file = serde_json::to_value(encryptor.finish()).unwrap();
        file["url"] = serde_json::json!(mxc);
        (cipher, file)
    }

    /// 這個帳號看得到的一則標準 Matrix 附件事件。
    async fn store_matrix_file(
        core: &Core,
        account: &AccountDir,
        event_id: &str,
        content: serde_json::Value,
    ) {
        let (cache, _) = core.server_cache_and_me(account).unwrap();
        let event = serde_json::json!({
            "type": "m.room.message", "event_id": event_id, "room_id": ROOM,
            "sender": "@carol:localhost", "origin_server_ts": 1000, "content": content,
        });
        cache
            .run(move |cache| {
                cache
                    .upsert_events(
                        ME,
                        ROOM,
                        &[wbf_sdk::incoming::IncomingEvent::Plain { event }],
                    )
                    .map(|_| ())
            })
            .await
            .unwrap();
    }

    fn encrypted_content(mxc: &str, file: serde_json::Value, size: usize) -> serde_json::Value {
        let mut file = file;
        file["url"] = serde_json::json!(mxc);
        serde_json::json!({ "msgtype": "m.file", "body": "a.bin", "info": { "size": size }, "file": file })
    }

    /// 這個 mxc 接下來的推播狀態（含 `downloading`），收到 `complete`／`failed`／`cancelled` 就停。
    async fn states_until_the_end(
        events: &mut Receiver<CoreEvent>,
        mxc: &str,
    ) -> Vec<(DownloadState, Option<Verification>)> {
        tokio::time::timeout(Duration::from_secs(10), async {
            let mut seen = Vec::new();
            loop {
                if let Ok(CoreEvent::MediaDownload {
                    mxc: event_mxc,
                    state,
                    verified,
                    ..
                }) = events.recv().await
                {
                    if event_mxc != mxc {
                        continue;
                    }
                    seen.push((state, verified));
                    if matches!(
                        state,
                        DownloadState::Complete | DownloadState::Failed | DownloadState::Cancelled
                    ) {
                        return seen;
                    }
                }
            }
        })
        .await
        .expect("the download ends")
    }

    async fn read_whole(core: &Core, mxc: &str) -> (bool, Vec<u8>) {
        let source = core.find_media_source(mxc).await.unwrap().unwrap();
        let trusted = source.is_trusted();
        let size = source.size;
        let mut stream = source.into_stream(0, size);
        let mut out = Vec::new();
        while let Some(piece) = stream.next_piece().await.unwrap() {
            out.extend_from_slice(&piece);
        }
        (trusted, out)
    }

    async fn on_server(name: &str, base: &str, backend: SessionBackend) -> (Core, AccountDir) {
        core_with_account_on(&scratch(name), base, backend).await
    }

    /// 傳統加密檔（/docs/design/media/media-download.md §12）：下載進池、推播 downloading → verifying → complete 帶 Matched、
    /// 列記好 kind 2 與 verified 1；之後 GET 從池給、可信；匯出成功。
    #[tokio::test]
    async fn an_encrypted_matrix_attachment_lands_in_the_pool_verified() {
        let mxc = "mxc://localhost/EncOk";
        let plain = sample(200_000);
        let (cipher, file) = encrypt(&plain, mxc);
        let base = media_server(cipher, None).await;
        let (core, account) = on_server("mx-enc-ok", &base, SessionBackend::WbfSdk).await;
        store_matrix_file(
            &core,
            &account,
            "$enc",
            encrypted_content(mxc, file, plain.len()),
        )
        .await;
        let mut events = core.subscribe();
        let media = MediaRef::Event {
            room: ROOM.into(),
            event_id: "$enc".into(),
            mxc: Some(mxc.into()),
        };
        let job = core
            .media_download(&media, &Target::default())
            .await
            .unwrap();
        assert_eq!(
            (job.state, job.kind),
            (DownloadState::Downloading, Some(MediaKind::MatrixEncrypted))
        );
        let states = states_until_the_end(&mut events, mxc).await;
        assert_eq!(
            states
                .iter()
                .filter(|(state, _)| *state != DownloadState::Downloading)
                .cloned()
                .collect::<Vec<_>>(),
            vec![
                (DownloadState::Verifying, None),
                (DownloadState::Complete, Some(Verification::Matched))
            ]
        );
        let (cache, _) = core.server_cache_and_me(&account).unwrap();
        let entry = cache.read().await.find_media(mxc).unwrap().unwrap();
        assert_eq!(
            (entry.kind, entry.verified, entry.complete, entry.file_size),
            (
                MediaKind::MatrixEncrypted,
                Verification::Matched,
                true,
                Some(plain.len() as u64)
            )
        );
        assert_eq!(read_whole(&core, mxc).await, (true, plain.clone()));
        let out = scratch("mx-enc-ok-out").join("a.bin");
        let exported = core
            .export_media_to(&media, &out, false, &Target::default())
            .await
            .unwrap();
        assert_eq!(
            (exported.source.as_str(), exported.verified),
            ("cache", Verification::Matched)
        );
        assert_eq!(std::fs::read(&out).unwrap(), plain);
    }

    /// 翻一個密文 bit（維護者 2026-10-06）：🚫 刪檔、記 verified 2；GET 照給但不可信（412）；匯出照寫、回 1501、data 是那份結果。
    #[tokio::test]
    async fn a_tampered_matrix_attachment_is_kept_untrusted_and_exports_as_1501() {
        let mxc = "mxc://localhost/EncBad";
        let plain = sample(150_000);
        let (mut cipher, file) = encrypt(&plain, mxc);
        cipher[70_000] ^= 0x10;
        let base = media_server(cipher, None).await;
        let (core, account) = on_server("mx-enc-bad", &base, SessionBackend::WbfSdk).await;
        store_matrix_file(
            &core,
            &account,
            "$bad",
            encrypted_content(mxc, file, plain.len()),
        )
        .await;
        let mut events = core.subscribe();
        let media = MediaRef::Mxc(mxc.into());
        core.media_download(&media, &Target::default())
            .await
            .unwrap();
        let states = states_until_the_end(&mut events, mxc).await;
        assert_eq!(
            states.last().copied(),
            Some((DownloadState::Complete, Some(Verification::Mismatched)))
        );
        let (trusted, got) = read_whole(&core, mxc).await;
        assert!(!trusted, "a mismatched hash is not trusted");
        assert_eq!(
            got[70_000],
            plain[70_000] ^ 0x10,
            "the data is given as it came"
        );
        let out = scratch("mx-enc-bad-out").join("a.bin");
        let error = core
            .export_media_to(&media, &out, false, &Target::default())
            .await
            .unwrap_err();
        assert_eq!(error.kind, CoreErrorKind::Unverified, "{error:?}");
        assert_eq!(
            error
                .data
                .as_ref()
                .map(|data| (data["verified"].clone(), data["kind"].clone())),
            Some((serde_json::json!(2), serde_json::json!(2)))
        );
        assert_eq!(
            std::fs::read(&out).unwrap().len(),
            plain.len(),
            "the file was written anyway"
        );
        let (cache, _) = core.server_cache_and_me(&account).unwrap();
        let entry = cache.read().await.find_media(mxc).unwrap().unwrap();
        assert_eq!(
            (entry.complete, entry.verified),
            (true, Verification::Mismatched)
        );
    }

    /// 邊下載邊讀（/docs/design/media/media-download.md §12.2）：主檔已經寫到的段 GET 先拿得到，而且還沒驗、不可信；
    /// 剩下的到了才完成、驗過。
    #[tokio::test]
    async fn a_get_reads_what_is_written_while_the_rest_is_still_coming() {
        let mxc = "mxc://localhost/EncSlow";
        let plain = sample(300_000);
        let (cipher, file) = encrypt(&plain, mxc);
        let release = Arc::new(Notify::new());
        let base = media_server(cipher, Some((200_000, release.clone()))).await;
        let (core, account) = on_server("mx-enc-slow", &base, SessionBackend::WbfSdk).await;
        store_matrix_file(
            &core,
            &account,
            "$slow",
            encrypted_content(mxc, file, plain.len()),
        )
        .await;
        let mut events = core.subscribe();
        let source = core.find_media_source(mxc).await.unwrap().unwrap();
        assert!(!source.is_trusted(), "still downloading: not verified yet");
        let mut stream = source.into_stream(0, 100_000);
        let mut early = Vec::new();
        while let Some(piece) = stream.next_piece().await.unwrap() {
            early.extend_from_slice(&piece);
        }
        assert_eq!(
            early,
            plain[..100_000],
            "the first 100 000 bytes came before the rest was sent"
        );
        release.notify_one();
        let states = states_until_the_end(&mut events, mxc).await;
        assert_eq!(
            states.last().copied(),
            Some((DownloadState::Complete, Some(Verification::Matched)))
        );
        assert_eq!(read_whole(&core, mxc).await, (true, plain));
        let _ = account;
    }

    /// 明文的傳統檔（kind 3）：沒有 hash 可比，🚫 verifying、完成時 verified 0；可信（GET 2xx）。一般 Matrix 帳號也下載得了。
    #[tokio::test]
    async fn a_plain_matrix_attachment_has_nothing_to_verify_and_works_for_a_general_account() {
        let mxc = "mxc://localhost/Plain";
        let plain = sample(90_000);
        let base = media_server(plain.clone(), None).await;
        let (core, account) = on_server("mx-plain", &base, SessionBackend::MatrixSdkClient).await;
        store_matrix_file(
            &core,
            &account,
            "$plain",
            serde_json::json!({ "msgtype": "m.image", "body": "p.png", "url": mxc }),
        )
        .await;
        let mut events = core.subscribe();
        let media = MediaRef::Event {
            room: ROOM.into(),
            event_id: "$plain".into(),
            mxc: None,
        };
        core.media_download(&media, &Target::default())
            .await
            .unwrap();
        let states = states_until_the_end(&mut events, mxc).await;
        assert!(!states
            .iter()
            .any(|(state, _)| *state == DownloadState::Verifying));
        assert_eq!(
            states.last().copied(),
            Some((DownloadState::Complete, Some(Verification::Unknown)))
        );
        assert_eq!(read_whole(&core, mxc).await, (true, plain));
    }

    /// 同一個 mxc、兩則事件給不同的 `file`（/docs/design/media/media-download.md §5.3、§12.1，維護者 2026-10-07）：
    /// 先到的是偽造的（列照它建）→ 從真的那則下載回 1500、🚫 下載、列🚫 動；`media.delete_local` 之後從真的那則下載成功、列換成真的；
    /// 之後偽造那則再來回錯；只給 mxc 時挑的是跟列一致的那則（偽造那則的事件比較早，但🚫 挑它）。
    #[tokio::test]
    async fn a_forged_description_that_came_first_is_refused_until_the_local_copy_is_deleted() {
        let mxc = "mxc://localhost/Twice";
        let plain = sample(80_000);
        let (cipher, real_file) = encrypt(&plain, mxc);
        let (_, forged_file) = encrypt(
            &sample(80_000)
                .iter()
                .map(|byte| byte ^ 1)
                .collect::<Vec<_>>(),
            mxc,
        );
        let forged_hash = format!(
            "matrix-sha256:{}",
            forged_file["hashes"]["sha256"].as_str().unwrap()
        );
        let real_hash = format!(
            "matrix-sha256:{}",
            real_file["hashes"]["sha256"].as_str().unwrap()
        );
        let base = media_server(cipher, None).await;
        let (core, account) = on_server("mx-twice", &base, SessionBackend::WbfSdk).await;
        store_matrix_file(
            &core,
            &account,
            "$forged",
            encrypted_content(mxc, forged_file, plain.len()),
        )
        .await;
        store_matrix_file(
            &core,
            &account,
            "$real",
            encrypted_content(mxc, real_file, plain.len()),
        )
        .await;
        let (cache, _) = core.server_cache_and_me(&account).unwrap();
        let recorded_hash =
            |cache: &wbf_sdk::cache::Cache| cache.find_media(mxc).unwrap().map(|entry| entry.hash);
        let real = MediaRef::Event {
            room: ROOM.into(),
            event_id: "$real".into(),
            mxc: Some(mxc.into()),
        };
        let forged = MediaRef::Event {
            room: ROOM.into(),
            event_id: "$forged".into(),
            mxc: None,
        };
        let error = core
            .media_download(&real, &Target::default())
            .await
            .unwrap_err();
        assert_eq!(error.kind, CoreErrorKind::Integrity, "{error:?}");
        assert_eq!(
            recorded_hash(&*cache.read().await),
            Some(Some(forged_hash.clone()))
        );
        assert!(
            core.matrix_transfers
                .find(&account.server_dir(), mxc)
                .is_none(),
            "nothing was started"
        );
        // 給的 mxc 不是那則的附件：拒。
        let elsewhere = MediaRef::Event {
            room: ROOM.into(),
            event_id: "$real".into(),
            mxc: Some("mxc://localhost/Other".into()),
        };
        let error = core
            .media_download(&elsewhere, &Target::default())
            .await
            .unwrap_err();
        assert_eq!(error.kind, CoreErrorKind::Usage, "{error:?}");
        // 清掉本地的、從真的那則再來。
        let deleted = core.del_local_media(mxc, &Target::default()).await.unwrap();
        assert_eq!((deleted.removed, deleted.cancelled), (true, false));
        assert_eq!(recorded_hash(&*cache.read().await), None);
        let mut events = core.subscribe();
        core.media_download(&real, &Target::default())
            .await
            .unwrap();
        let states = states_until_the_end(&mut events, mxc).await;
        assert_eq!(
            states.last().copied(),
            Some((DownloadState::Complete, Some(Verification::Matched)))
        );
        assert_eq!(
            recorded_hash(&*cache.read().await),
            Some(Some(real_hash.clone()))
        );
        let error = core
            .media_download(&forged, &Target::default())
            .await
            .unwrap_err();
        assert_eq!(error.kind, CoreErrorKind::Integrity, "{error:?}");
        // 偽造那則現在也連在這一列上（從它點過），而且比真的那則早寫進來：只給 mxc 時🚫 挑它。
        assert_eq!(
            cache
                .read()
                .await
                .list_matrix_attachments_for(ME, mxc)
                .unwrap()
                .len(),
            2
        );
        let job = core
            .media_download(&MediaRef::Mxc(mxc.into()), &Target::default())
            .await
            .unwrap();
        assert_eq!(
            (job.state, job.verified),
            (DownloadState::Complete, Some(Verification::Matched))
        );
        assert_eq!(read_whole(&core, mxc).await, (true, plain));
    }

    /// 只給 mxc、列還沒下載（/docs/design/media/media-download.md §5.3）：用的是跟列一致的那則的金鑰，🚫 最早寫進來的那則。
    #[tokio::test]
    async fn an_mxc_alone_downloads_with_the_key_of_the_event_that_matches_the_record() {
        let mxc = "mxc://localhost/Pick";
        let plain = sample(70_000);
        let (cipher, real_file) = encrypt(&plain, mxc);
        let (_, forged_file) = encrypt(&plain, mxc);
        let base = media_server(cipher, None).await;
        let (core, account) = on_server("mx-pick", &base, SessionBackend::WbfSdk).await;
        store_matrix_file(
            &core,
            &account,
            "$forged",
            encrypted_content(mxc, forged_file, plain.len()),
        )
        .await;
        // 列換成真的那則的：刪掉再從它點（這時偽造那則的連結跟著列沒了），再從偽造那則點一次（被拒，但連上了）。
        store_matrix_file(
            &core,
            &account,
            "$real",
            encrypted_content(mxc, real_file, plain.len()),
        )
        .await;
        core.del_local_media(mxc, &Target::default()).await.unwrap();
        let (cache, _) = core.server_cache_and_me(&account).unwrap();
        let relinked = cache
            .run(|cache| {
                Ok((
                    cache.media_link_event(ME, ROOM, "$real")?,
                    cache.media_link_event(ME, ROOM, "$forged")?,
                ))
            })
            .await
            .unwrap();
        assert_eq!(relinked, (true, true));
        let mut events = core.subscribe();
        core.media_download(&MediaRef::Mxc(mxc.into()), &Target::default())
            .await
            .unwrap();
        let states = states_until_the_end(&mut events, mxc).await;
        assert_eq!(
            states.last().copied(),
            Some((DownloadState::Complete, Some(Verification::Matched))),
            "the forged key would not decrypt to the hash"
        );
        assert_eq!(read_whole(&core, mxc).await, (true, plain));
    }

    /// `media.delete_local`（/docs/design/media/media-download.md §7.4）：正在下載的傳統檔先取消，半成品、列都沒了；
    /// 只給 mxc 就不能再下載（沒有列），從訊息點就由那則重建、從頭下載。
    #[tokio::test]
    async fn deleting_a_matrix_file_cancels_its_download_and_leaves_nothing() {
        let mxc = "mxc://localhost/Delete";
        let plain = sample(300_000);
        let release = Arc::new(Notify::new());
        let base = media_server(plain.clone(), Some((150_000, release.clone()))).await;
        let (core, account) = on_server("mx-delete", &base, SessionBackend::WbfSdk).await;
        store_matrix_file(
            &core,
            &account,
            "$d",
            serde_json::json!({ "msgtype": "m.file", "body": "d.bin", "info": { "size": plain.len() }, "url": mxc }),
        )
        .await;
        let mut events = core.subscribe();
        core.media_download(&MediaRef::Mxc(mxc.into()), &Target::default())
            .await
            .unwrap();
        let deleted = core.del_local_media(mxc, &Target::default()).await.unwrap();
        assert_eq!(
            deleted,
            crate::DeletedMedia {
                mxc: mxc.into(),
                removed: true,
                cancelled: true
            }
        );
        let states = states_until_the_end(&mut events, mxc).await;
        assert_eq!(
            states.last().copied(),
            Some((DownloadState::Cancelled, None))
        );
        let pool = core.pool_of(&account).unwrap();
        assert!(pool.list_pending().unwrap().is_empty());
        let (cache, _) = core.server_cache_and_me(&account).unwrap();
        // 被取消的 task 收尾時🚫 去清（或重建）列。
        cache.run(|_| Ok(())).await.unwrap();
        assert!(cache.read().await.find_media(mxc).unwrap().is_none());
        let error = core
            .media_download(&MediaRef::Mxc(mxc.into()), &Target::default())
            .await
            .unwrap_err();
        assert_eq!(error.kind, CoreErrorKind::Usage, "{error:?}");
        release.notify_waiters();
        let from_message = MediaRef::Event {
            room: ROOM.into(),
            event_id: "$d".into(),
            mxc: Some(mxc.into()),
        };
        let job = core
            .media_download(&from_message, &Target::default())
            .await
            .unwrap();
        assert_eq!(job.state, DownloadState::Downloading);
        release.notify_one();
        let states = states_until_the_end(&mut events, mxc).await;
        assert_eq!(
            states.last().copied(),
            Some((DownloadState::Complete, Some(Verification::Unknown)))
        );
        assert_eq!(read_whole(&core, mxc).await, (true, plain));
    }

    /// PR #75 審查 salvia 1：起 task 之前讀的列已經舊了（上一個 task 剛收尾）——task 開頭再問一次列，完整就直接收尾、🚫 重下。
    /// 這裡直接起一個 task 對著已經完整的列，server 是一個連不上的位址：真的去下載就會失敗。
    #[tokio::test]
    async fn a_task_started_after_the_file_completed_finishes_without_downloading() {
        let mxc = "mxc://localhost/Late";
        let plain = sample(90_000);
        let (cipher, file) = encrypt(&plain, mxc);
        let base = media_server(cipher, None).await;
        let (core, account) = on_server("mx-late", &base, SessionBackend::WbfSdk).await;
        store_matrix_file(
            &core,
            &account,
            "$late",
            encrypted_content(mxc, file, plain.len()),
        )
        .await;
        let mut events = core.subscribe();
        core.media_download(&MediaRef::Mxc(mxc.into()), &Target::default())
            .await
            .unwrap();
        states_until_the_end(&mut events, mxc).await;
        let (cache, me) = core.server_cache_and_me(&account).unwrap();
        let attachment = cache
            .read()
            .await
            .find_event_matrix_attachment(ME, ROOM, "$late")
            .unwrap()
            .unwrap();
        let pending_name = cache.read().await.media_pending_name(mxc).unwrap().unwrap();
        let key = (account.server_dir(), mxc.to_string());
        let (commands, inbox) = mpsc::unbounded_channel();
        let (waiter, done) = oneshot::channel();
        let _ = commands.send(Command::Wait(waiter));
        core.matrix_transfers.files().insert(
            key.clone(),
            Arc::new(MatrixTransfer {
                account_dir: account.dir.clone(),
                description: MediaDescription::of_matrix_attachment(&attachment),
                pending_name: pending_name.clone(),
                commands,
            }),
        );
        let task = TransferTask {
            key,
            transfers: core.matrix_transfers.clone(),
            total: segments_of(attachment.size),
            attachment,
            server: "http://127.0.0.1:1".into(),
            access_token: "unused".into(),
            user: me,
            cache: cache.clone(),
            pool: core.pool_of(&account).unwrap(),
            events: core.events.clone(),
            pending_name,
            inbox,
            reads: Vec::new(),
            waiters: Vec::new(),
        };
        tokio::spawn(task.run());
        let finished = tokio::time::timeout(Duration::from_secs(10), done)
            .await
            .unwrap()
            .unwrap();
        assert!(finished.is_ok(), "{finished:?}");
        let states = states_until_the_end(&mut events, mxc).await;
        assert_eq!(
            states,
            vec![(DownloadState::Complete, Some(Verification::Matched))],
            "no downloading, no second verification"
        );
        let entry = cache.read().await.find_media(mxc).unwrap().unwrap();
        assert_eq!(
            (entry.complete, entry.verified),
            (true, Verification::Matched)
        );
        assert_eq!(read_whole(&core, mxc).await, (true, plain));
    }

    /// 正照一份描述在下載時，另一份描述來要同一個 mxc：跟列對不上、回錯（🚫 掛上去，會拿到別把金鑰解的資料）；正在下載的照常完成。
    #[tokio::test]
    async fn another_description_cannot_join_a_download_in_progress() {
        let mxc = "mxc://localhost/Busy";
        let plain = sample(200_000);
        let (cipher, real_file) = encrypt(&plain, mxc);
        let (_, other_file) = encrypt(&plain, mxc);
        let real_hash = format!(
            "matrix-sha256:{}",
            real_file["hashes"]["sha256"].as_str().unwrap()
        );
        let release = Arc::new(Notify::new());
        let base = media_server(cipher, Some((100_000, release.clone()))).await;
        let (core, account) = on_server("mx-busy", &base, SessionBackend::WbfSdk).await;
        store_matrix_file(
            &core,
            &account,
            "$real",
            encrypted_content(mxc, real_file, plain.len()),
        )
        .await;
        store_matrix_file(
            &core,
            &account,
            "$other",
            encrypted_content(mxc, other_file, plain.len()),
        )
        .await;
        let mut events = core.subscribe();
        let real = MediaRef::Event {
            room: ROOM.into(),
            event_id: "$real".into(),
            mxc: None,
        };
        core.media_download(&real, &Target::default())
            .await
            .unwrap();
        let other = MediaRef::Event {
            room: ROOM.into(),
            event_id: "$other".into(),
            mxc: None,
        };
        let error = core
            .media_download(&other, &Target::default())
            .await
            .unwrap_err();
        assert_eq!(error.kind, CoreErrorKind::Integrity, "{error:?}");
        release.notify_one();
        let states = states_until_the_end(&mut events, mxc).await;
        assert_eq!(
            states.last().copied(),
            Some((DownloadState::Complete, Some(Verification::Matched)))
        );
        assert_eq!(read_whole(&core, mxc).await, (true, plain));
        // 結果記在真的那份描述上：被拒的那份🚫 換掉列的描述。
        let (cache, _) = core.server_cache_and_me(&account).unwrap();
        let entry = cache.read().await.find_media(mxc).unwrap().unwrap();
        assert_eq!(
            (entry.hash, entry.verified),
            (Some(real_hash), Verification::Matched)
        );
    }

    /// 取消（/docs/design/media/media-download.md §12.4）：🚫 續傳，半成品刪掉、列回到沒下載、推播 cancelled；再要一次從頭來。
    #[tokio::test]
    async fn a_cancelled_matrix_download_leaves_nothing_and_starts_over_next_time() {
        let mxc = "mxc://localhost/Cancel";
        let plain = sample(300_000);
        let release = Arc::new(Notify::new());
        let base = media_server(plain.clone(), Some((150_000, release.clone()))).await;
        let (core, account) = on_server("mx-cancel", &base, SessionBackend::WbfSdk).await;
        store_matrix_file(
            &core,
            &account,
            "$c",
            serde_json::json!({ "msgtype": "m.file", "body": "c.bin", "info": { "size": plain.len() }, "url": mxc }),
        )
        .await;
        let mut events = core.subscribe();
        let media = MediaRef::Mxc(mxc.into());
        core.media_download(&media, &Target::default())
            .await
            .unwrap();
        assert!(core.media_cancel(mxc, &Target::default()).await.unwrap());
        let states = states_until_the_end(&mut events, mxc).await;
        assert_eq!(
            states.last().copied(),
            Some((DownloadState::Cancelled, None))
        );
        let pool = core.pool_of(&account).unwrap();
        wait_for_async(
            || async { pool.list_pending().unwrap().is_empty() },
            "the unfinished file is gone",
        )
        .await;
        let (cache, _) = core.server_cache_and_me(&account).unwrap();
        let entry = cache.read().await.find_media(mxc).unwrap().unwrap();
        assert!(!entry.complete);
        // 再要一次：從頭下載、完成。
        release.notify_waiters();
        let job = core
            .media_download(&media, &Target::default())
            .await
            .unwrap();
        assert_eq!(job.state, DownloadState::Downloading);
        release.notify_one();
        let states = states_until_the_end(&mut events, mxc).await;
        assert_eq!(
            states.last().copied(),
            Some((DownloadState::Complete, Some(Verification::Unknown)))
        );
    }
}

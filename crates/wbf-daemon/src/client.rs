//! RPC 的 client 那一頭（/docs/design/rpc-specs/rpc-cli.md §3）：起 daemon 或找到在跑的那支、連線、`hello`、`call`、收推播、資料平面的讀寫。
//!
//! rpc-cli 用它；之後原生 Rust 的 Desktop 也直接用它（/docs/design/overview/architecture-v2.md §0.2「Rust 端要用 RPC 的時候接口直接重用」）。
//! 別的語言（JS、Kotlin）照這裡的順序寫自己的一份：讀 `daemon.json`（port）與 `daemon.token`（或起 daemon、讀 ready 那一行）→ 連線 → `hello` → 每則加解密。

use std::collections::VecDeque;
use std::path::Path;
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use http_body_util::{BodyExt, StreamBody};
use hyper::body::Frame;
use hyper_util::rt::TokioIo;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio_tungstenite::tungstenite::Message;
use zeroize::Zeroizing;

use crate::pack::{self, PackType, RpcKeys, Side, TOKEN_LEN};
use crate::protocol::CURRENT_PROTOCOL;
use crate::token::{read_token_file, to_env_value, TOKEN_ENV};

/// 起 daemon 之後最多等它宣告 ready 多久：它要拿鎖、讀 conf、開兩個 port，正常一兩秒；等不到就收掉它、講出來（禁止無限等）。
pub const READY_WAIT: Duration = Duration::from_secs(30);

/// client 這一頭出的錯。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientError {
    /// 要給人看的那句
    pub message: String,
    /// daemon 關這條連線之前送的通知的 `code`（/docs/design/rpc-specs/rpc-spec.md §1.4，例 9006 ＝ daemon 要關了）；不是這種錯就是 None
    pub closing_code: Option<u64>,
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ClientError {}

fn fail(message: impl Into<String>) -> ClientError {
    ClientError {
        message: message.into(),
        closing_code: None,
    }
}

/// JSON 物件的一個欄位；沒有（或不是物件）就是 `Null`。用它而不是 `value["x"]`：crate 根擋了索引（寫入那種會 panic）。
fn field<'a>(value: &'a Value, name: &str) -> &'a Value {
    value.get(name).unwrap_or(&Value::Null)
}

/// 一支在跑的 daemon 在哪（daemon 起來時 stdout 那一行，或 `<data dir>/daemon.json`；/docs/design/rpc-specs/local-interface.md §3）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ready {
    pub rpc_port: u16,
    pub data_port: u16,
    pub pid: u32,
    /// 這次啟動的 UUID（判斷「還是剛才那一個 daemon 嗎」用；禁止拿 port 或 pid 判斷）
    pub instance: String,
}

/// 我們起的那個 daemon 程序。
///
/// ⚠️ 丟掉它（沒走 [`DaemonProcess::wait_for_exit`] 等到結束）就會**殺掉**那支 daemon：起到一半出錯的路
/// （ready 之後連不上、`hello` 被拒）不必各自記得收尾（`kill_on_drop`）。
pub struct DaemonProcess {
    child: tokio::process::Child,
    pub ready: Ready,
}

/// 起一個 daemon（`-s`），token 用環境變數指定，等它在 stdout 宣告 ready（最多 [`READY_WAIT`]）。
///
/// Args:
///     daemon: daemon 的執行檔, example: "target/debug/wbf-matrix-client-daemon.exe"
///     data_dir: example: "<data dir>"
///     token: 這次用的 token（`crate::token::new_token`）
///     stderr: daemon 的 stderr 給誰；rpc-cli 給 `Stdio::inherit()`，測試給 `Stdio::piped()`
/// Return:
///     Ok(DaemonProcess)   起來了、宣告了 ready
///     Err(ClientError)    執行檔起不來、它沒宣告 ready 就結束了（資料目錄被別的 daemon 佔著、token 不對…，原因在它的 stderr）、
///                         或 [`READY_WAIT`] 內沒宣告（那支已經殺掉了）
pub async fn spawn_daemon(
    daemon: &Path,
    data_dir: &Path,
    token: &[u8],
    stderr: Stdio,
) -> Result<DaemonProcess, ClientError> {
    let token_value = to_env_value(token);
    let mut child = tokio::process::Command::new(daemon)
        .arg("-s")
        .arg("--data-dir")
        .arg(data_dir)
        .env(TOKEN_ENV, token_value.as_str())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(stderr)
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| fail(format!("cannot start {}: {error}", daemon.display())))?;
    drop(token_value);
    let mut line = String::new();
    let read = match child.stdout.take() {
        Some(stdout) => {
            let mut stdout = tokio::io::BufReader::new(stdout);
            tokio::time::timeout(READY_WAIT, stdout.read_line(&mut line)).await
        }
        None => Ok(Ok(0)),
    };
    let ready = match read {
        Ok(Ok(read)) if read > 0 => parse_ready(&line),
        Ok(Ok(_)) => Err(fail("the daemon exited before saying ready")),
        Ok(Err(error)) => Err(fail(format!(
            "cannot read the daemon's ready line: {error}"
        ))),
        Err(_) => {
            // 既沒說 ready 也沒結束：收掉它（丟掉 child 也會殺，這裡明講、等它真的走）。
            let _ = child.kill().await;
            return Err(fail(format!(
                "the daemon did not say ready within {}s; it was stopped",
                READY_WAIT.as_secs()
            )));
        }
    };
    match ready {
        Ok(ready) => Ok(DaemonProcess { child, ready }),
        Err(error) => {
            // 沒說 ready 就結束了：把 exit code 一起講（還活著的話丟掉 child 時會被殺）。
            let status = tokio::time::timeout(Duration::from_secs(5), child.wait())
                .await
                .ok()
                .and_then(|waited| waited.ok())
                .map(|status| status.to_string())
                .unwrap_or_else(|| "still running; stopped".to_string());
            Err(fail(format!("{error} ({status})")))
        }
    }
}

/// Args:
///     line: daemon stdout 的那一行, example: "{\"ready\":true,\"rpc_port\":51234,\"data_port\":51235,\"pid\":4321,\"instance\":\"…\"}"
/// Return:
///     Ok(Ready)
///     Err(ClientError)   不是 JSON、不是 ready 那一行、或少了欄位（禁止猜 port）
pub fn parse_ready(line: &str) -> Result<Ready, ClientError> {
    let ready: Value = serde_json::from_str(line.trim()).map_err(|error| {
        fail(format!(
            "the daemon's ready line is not JSON ({error}): {line:?}"
        ))
    })?;
    if field(&ready, "ready") != true {
        return Err(fail(format!(
            "the daemon's first line is not a ready line: {ready}"
        )));
    }
    where_it_runs(&ready, "the daemon's ready line")
}

/// `daemon.json` 與 ready 那一行共同的欄位。
fn where_it_runs(value: &Value, source: &str) -> Result<Ready, ClientError> {
    let port = |name: &str| {
        field(value, name)
            .as_u64()
            .and_then(|port| u16::try_from(port).ok())
            .filter(|port| *port != 0)
            .ok_or_else(|| fail(format!("{source} has no usable {name}: {value}")))
    };
    Ok(Ready {
        rpc_port: port("rpc_port")?,
        data_port: port("data_port")?,
        pid: field(value, "pid")
            .as_u64()
            .and_then(|pid| u32::try_from(pid).ok())
            .ok_or_else(|| fail(format!("{source} has no pid: {value}")))?,
        instance: field(value, "instance")
            .as_str()
            .ok_or_else(|| fail(format!("{source} has no instance: {value}")))?
            .to_string(),
    })
}

/// 這個資料目錄裡在跑的那支 daemon：`daemon.json` 的 port ＋ `daemon.token`（/docs/design/rpc-specs/local-interface.md §3）。
///
/// ⚠️ daemon 被強制殺掉時兩個檔都會留著：讀得到**不**保證它還活著，連不上就是答案（[`RpcClient::connect`] 會講）。
///
/// Args:
///     data_dir: example: "<data dir>"
/// Return:
///     Ok((Ready, token))   兩個檔都在、形狀對
///     Err(ClientError)     沒有 daemon 在跑（檔不在）、讀不了、形狀不對
pub fn find_running_daemon(data_dir: &Path) -> Result<(Ready, Zeroizing<Vec<u8>>), ClientError> {
    let path = data_dir.join("daemon.json");
    let text = std::fs::read_to_string(&path).map_err(|error| {
        fail(format!(
            "cannot read {} ({error}); is a daemon serving this data directory?",
            path.display()
        ))
    })?;
    let value: Value = serde_json::from_str(&text)
        .map_err(|error| fail(format!("{} is not JSON: {error}", path.display())))?;
    let ready = where_it_runs(&value, &path.display().to_string())?;
    let token = read_token_file(data_dir).map_err(|problem| fail(problem.0))?;
    Ok((ready, token))
}

impl DaemonProcess {
    /// 等 daemon 自己結束（送了 `daemon.shutdown` 之後）。
    ///
    /// Return:
    ///     Ok(ExitStatus)      結束了
    ///     Err(ClientError)    `limit` 內沒結束（`self` 在這裡丟掉，那支會被殺掉）
    pub async fn wait_for_exit(mut self, limit: Duration) -> Result<ExitStatus, ClientError> {
        match tokio::time::timeout(limit, self.child.wait()).await {
            Ok(Ok(status)) => Ok(status),
            Ok(Err(error)) => Err(fail(format!("cannot wait for the daemon: {error}"))),
            Err(_) => Err(fail(format!(
                "the daemon (pid {}) did not exit within {}s; it was stopped",
                self.ready.pid,
                limit.as_secs()
            ))),
        }
    }
}

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// 一條 RPC 連線。一次一個請求（`call` 等到自己的回應才回）；等的時候收到的推播排在 [`RpcClient::next_push`]。
pub struct RpcClient {
    socket: Socket,
    keys: RpcKeys,
    next_id: u64,
    pushes: VecDeque<Value>,
}

impl RpcClient {
    /// 連上、`hello`。
    ///
    /// Args:
    ///     rpc_port: ready 那一行的, example: 51234
    ///     token: 那支 daemon 的 token（交給它的那把，或 `daemon.token` 讀出來的）
    ///     client_name: 要 `wbf-matrix` 開頭（/docs/design/rpc-specs/rpc-spec.md §1.3）, example: "wbf-matrix-rpc-cli 0.1.0"
    /// Return:
    ///     Ok((RpcClient, Value))   連上了，第二個是 `hello` 的 result
    ///     Err(ClientError)         token 長度不對、連不上、或 `hello` 被拒
    pub async fn connect(
        rpc_port: u16,
        token: &[u8],
        client_name: &str,
    ) -> Result<(RpcClient, Value), ClientError> {
        let keys = RpcKeys::from_token_checking_length(token).map_err(|actual| {
            fail(format!(
                "the daemon token is {actual} bytes; it must be exactly {TOKEN_LEN}"
            ))
        })?;
        let (socket, _) = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{rpc_port}"))
            .await
            .map_err(|error| {
                fail(format!(
                    "cannot connect to the daemon on port {rpc_port}: {error}"
                ))
            })?;
        let mut client = RpcClient {
            socket,
            keys,
            next_id: 1,
            pushes: VecDeque::new(),
        };
        let hello = client
            .call(
                "hello",
                json!({ "protocols": [CURRENT_PROTOCOL], "client": client_name }),
            )
            .await?;
        if code_of(&hello) != 0 {
            return Err(fail(format!("the daemon refused hello: {hello}")));
        }
        Ok((client, field(&hello, "result").clone()))
    }

    /// 送一則請求、等**它自己的**回應（`id` 由這裡發）。等的時候收到的推播排起來。
    ///
    /// Args:
    ///     method: example: "room.list"
    ///     params: example: json!({"sync": "both"})；沒有就給 `Value::Null`
    /// Return:
    ///     Ok(Value)          整則回應（`code`、`result`、`msg`、`id`…）；`code` 不是 0 也算 Ok，由呼叫端看
    ///     Err(ClientError)   送不出去、連線斷了、daemon 送了關閉通知
    pub async fn call(&mut self, method: &str, params: Value) -> Result<Value, ClientError> {
        let id = self.next_id;
        self.next_id += 1;
        let request = if params.is_null() {
            json!({ "method": method, "id": id })
        } else {
            json!({ "method": method, "params": params, "id": id })
        };
        self.send(&request).await?;
        loop {
            let message = self.receive().await?;
            if field(&message, "id") == id {
                return Ok(message);
            }
            if field(&message, "id").is_null() {
                self.pushes.push_back(message);
            }
        }
    }

    /// 下一則推播（先給排著的）。
    ///
    /// Return:
    ///     Ok(Value)          一則推播（`method`、`params`）
    ///     Err(ClientError)   連線斷了、daemon 送了關閉通知
    pub async fn next_push(&mut self) -> Result<Value, ClientError> {
        if let Some(push) = self.pushes.pop_front() {
            return Ok(push);
        }
        loop {
            let message = self.receive().await?;
            if field(&message, "id").is_null() {
                return Ok(message);
            }
        }
    }

    /// 取走等回應時排起來的推播，**不**等新的。
    ///
    /// Return:
    ///     Vec<Value>  照收到的順序；沒有就是空的
    pub fn take_buffered_pushes(&mut self) -> Vec<Value> {
        self.pushes.drain(..).collect()
    }

    async fn send(&mut self, request: &Value) -> Result<(), ClientError> {
        let frame = pack::seal(
            &self.keys,
            Side::Client,
            PackType::Cipher,
            request.to_string().as_bytes(),
        )
        .map_err(|error| fail(format!("cannot seal the request: {error:?}")))?;
        self.socket
            .send(Message::Binary(frame.into()))
            .await
            .map_err(|error| fail(format!("cannot send to the daemon: {error}")))
    }

    /// 下一則訊息（解密後的 JSON）。明文的那種只有關閉通知（/docs/design/rpc-specs/rpc-spec.md §1.4）：當成錯回。
    async fn receive(&mut self) -> Result<Value, ClientError> {
        loop {
            let message = match self.socket.next().await {
                Some(Ok(message)) => message,
                Some(Err(error)) => {
                    return Err(fail(format!("the daemon connection broke: {error}")))
                }
                None => return Err(fail("the daemon closed the connection")),
            };
            let bytes = match message {
                Message::Binary(bytes) => bytes,
                Message::Close(frame) => {
                    return Err(fail(format!("the daemon closed the connection: {frame:?}")))
                }
                _ => continue,
            };
            let (pack_type, json) = pack::open(&self.keys, Side::Client, &bytes)
                .map_err(|error| fail(format!("cannot open a frame from the daemon: {error:?}")))?;
            let message: Value = serde_json::from_slice(&json).map_err(|error| {
                fail(format!(
                    "the daemon sent something that is not JSON: {error}"
                ))
            })?;
            if pack_type == PackType::Plain {
                return Err(ClientError {
                    message: format!("the daemon is closing this connection: {message}"),
                    closing_code: Some(code_of(&message)),
                });
            }
            return Ok(message);
        }
    }

    /// 好好關掉這條連線（送 Close、等對方回）。
    pub async fn close(mut self) {
        let _ = self.socket.close(None).await;
    }
}

/// Return:
///     u64  回應的 `code`；沒有這個欄位當成 108（daemon 的 bug，/docs/design/rpc-specs/rpc-spec.md §5.1），**不**當成 0
pub fn code_of(response: &Value) -> u64 {
    field(response, "code").as_u64().unwrap_or(108)
}

/// 資料平面的一次回應：狀態碼與 rpc-cli 會用到的 header。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataReply {
    pub status: u16,
    /// `wbf-media-verified`（GET 才有）, example: Some("1")
    pub verified: Option<String>,
    /// GET 不是資料（不是 200／206／412）時 daemon 回的那段字（一則錯誤的 JSON）；是資料就是 None, example: Some("{\"code\":1100,…}")
    pub refusal: Option<String>,
}

/// GET 回這幾個時 body 是檔案的內容（412 ＝ 沒驗過或驗不過，資料照給，/docs/design/rpc-specs/data-plane.md §8.2）。
fn is_media_body(status: u16) -> bool {
    matches!(status, 200 | 206 | 412)
}

/// 拒絕的 body 最多收多少：daemon 的錯誤是一則短 JSON，禁止把不認得的大 body 整個收進記憶體。
const REFUSAL_LIMIT: usize = 64 * 1024;

/// `PUT` 一份 body 到 `media.create` 給的 URL（/docs/design/rpc-specs/data-plane.md 的兩步）。
///
/// Args:
///     created: `media.create` 的 result（要 `url` 與 `headers`）
///     body: 檔案或 stdin
///     length: 知道長度就給（`Content-Length`）；None ＝ chunked
/// Return:
///     Ok((DataReply, Value))   狀態碼與回應的 JSON（成功時是 manifest）
///     Err(ClientError)         `url` 不是 loopback 的 http、連不上、送到一半斷了
pub async fn put(
    created: &Value,
    body: impl AsyncRead + Send + Sync + Unpin + 'static,
    length: Option<u64>,
) -> Result<(DataReply, Value), ClientError> {
    let url = field(created, "url")
        .as_str()
        .ok_or_else(|| fail(format!("media.create gave no url: {created}")))?;
    let (port, path) = split_loopback_url(url)?;
    let stream = tokio_util::io::ReaderStream::new(body).map(|piece| piece.map(Frame::data));
    let mut request = hyper::Request::put(path).header(hyper::header::HOST, "127.0.0.1");
    if let Some(headers) = field(created, "headers").as_object() {
        for (name, value) in headers {
            let value = value.as_str().ok_or_else(|| {
                fail(format!(
                    "media.create gave a header that is not a string: {name}"
                ))
            })?;
            request = request.header(name.as_str(), value);
        }
    }
    if let Some(length) = length {
        request = request.header(hyper::header::CONTENT_LENGTH, length);
    }
    let request = request
        .body(StreamBody::new(stream))
        .map_err(|error| fail(format!("cannot build the upload request: {error}")))?;
    let (status, body) = send_and_collect(port, request).await?;
    let json = serde_json::from_slice(&body).unwrap_or(Value::Null);
    Ok((
        DataReply {
            status,
            verified: None,
            refusal: None,
        },
        json,
    ))
}

/// `GET` 一個 `media.open` 給的 URL，檔案內容寫進 `sink`。
///
/// Args:
///     url: `media.open` 的 result 的 `url`
///     range: 例 "bytes=100-199"；None 就整檔
///     sink: 檔案或 stdout
/// Return:
///     Ok(DataReply)       200／206／412 時 body 寫進了 `sink`（412 ＝ 沒驗過或驗不過，資料照給）；其他狀態碼 `sink` 一個 byte 都沒寫，body 在 `refusal`
///     Err(ClientError)    `url` 不是 loopback 的 http、連不上、寫不進 sink
pub async fn get(
    url: &str,
    range: Option<&str>,
    mut sink: impl AsyncWrite + Unpin,
) -> Result<DataReply, ClientError> {
    let (port, path) = split_loopback_url(url)?;
    let mut request = hyper::Request::get(path).header(hyper::header::HOST, "127.0.0.1");
    if let Some(range) = range {
        request = request.header(hyper::header::RANGE, range);
    }
    let request = request
        .body(http_body_util::Empty::<Bytes>::new())
        .map_err(|error| fail(format!("cannot build the download request: {error}")))?;
    let tcp = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .map_err(|error| {
            fail(format!(
                "cannot connect to the data plane on port {port}: {error}"
            ))
        })?;
    let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(tcp))
        .await
        .map_err(|error| fail(format!("the data plane handshake failed: {error}")))?;
    let driving = tokio::spawn(connection);
    let outcome = async {
        let response = sender
            .send_request(request)
            .await
            .map_err(|error| fail(format!("the download request failed: {error}")))?;
        let status = response.status().as_u16();
        let verified = response
            .headers()
            .get("wbf-media-verified")
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let mut body = response.into_body();
        // 不是檔案內容就不碰 sink：錯誤的 JSON 混進輸出的檔（或 stdout）比沒寫更糟。
        let mut refusal = (!is_media_body(status)).then(Vec::new);
        while let Some(frame) = body.frame().await {
            let frame = frame.map_err(|error| fail(format!("the download broke off: {error}")))?;
            let Ok(data) = frame.into_data() else {
                continue;
            };
            match refusal.as_mut() {
                Some(collected) => {
                    let room = REFUSAL_LIMIT.saturating_sub(collected.len());
                    collected.extend(data.iter().take(room));
                }
                None => sink
                    .write_all(&data)
                    .await
                    .map_err(|error| fail(format!("cannot write what was downloaded: {error}")))?,
            }
        }
        sink.flush()
            .await
            .map_err(|error| fail(format!("cannot write what was downloaded: {error}")))?;
        Ok(DataReply {
            status,
            verified,
            refusal: refusal.map(|bytes| String::from_utf8_lossy(&bytes).into_owned()),
        })
    }
    .await;
    // 連線那頭：送出端丟掉它就結束，等它回來（禁止丟著不管）。
    drop(sender);
    let _ = driving.await;
    outcome
}

/// 送一個請求、把整個 body 收回來（`PUT` 的回應很小：一份 manifest 或一則錯誤）。
async fn send_and_collect<B>(
    port: u16,
    request: hyper::Request<B>,
) -> Result<(u16, Bytes), ClientError>
where
    B: hyper::body::Body + Send + 'static,
    B::Data: Send,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    let tcp = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .map_err(|error| {
            fail(format!(
                "cannot connect to the data plane on port {port}: {error}"
            ))
        })?;
    let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(tcp))
        .await
        .map_err(|error| fail(format!("the data plane handshake failed: {error}")))?;
    let driving = tokio::spawn(connection);
    let outcome = async {
        let response = sender
            .send_request(request)
            .await
            .map_err(|error| fail(format!("the upload request failed: {error}")))?;
        let status = response.status().as_u16();
        let body = response
            .into_body()
            .collect()
            .await
            .map_err(|error| fail(format!("the upload reply broke off: {error}")))?
            .to_bytes();
        Ok((status, body))
    }
    .await;
    drop(sender);
    let _ = driving.await;
    outcome
}

/// 資料平面的 URL 只會是 `http://127.0.0.1:<port>/…`（daemon 發的）。別的一律拒絕：禁止把本地的 bytes 送到別處。
///
/// Args:
///     url: example: "http://127.0.0.1:51235/media/mxc/e_…"
/// Return:
///     Ok((u16, String))   (port, path)
///     Err(ClientError)    不是 loopback 的 http
pub fn split_loopback_url(url: &str) -> Result<(u16, String), ClientError> {
    let rest = url
        .strip_prefix("http://127.0.0.1:")
        .ok_or_else(|| fail(format!("not a data plane URL of this machine: {url}")))?;
    let (port, path) = match rest.find('/') {
        Some(slash) => rest.split_at(slash),
        None => (rest, "/"),
    };
    let port: u16 = port
        .parse()
        .map_err(|_| fail(format!("not a data plane URL of this machine: {url}")))?;
    Ok((port, path.to_string()))
}

/// 一把新 token（[`crate::token::new_token`] 的轉手，讓 client 這一頭只需要這個模組）。
///
/// Return:
///     Ok(token)              [`TOKEN_LEN`] byte 的 OS 亂數
///     Err(ClientError)       OS 給不出亂數
pub fn new_token() -> Result<Zeroizing<[u8; TOKEN_LEN]>, ClientError> {
    crate::token::new_token().map_err(|problem| fail(problem.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_ready_line_needs_every_field() {
        let ready = parse_ready(
            r#"{"ready":true,"rpc_port":51234,"data_port":51235,"pid":4321,"instance":"0f8fad5b-d9cb-469f-a165-70867728950e"}"#,
        )
        .unwrap();
        assert_eq!(ready.rpc_port, 51234);
        assert_eq!(ready.data_port, 51235);
        assert_eq!(ready.pid, 4321);
        assert!(parse_ready("listening…").is_err());
        assert!(
            parse_ready(r#"{"ready":true,"rpc_port":0,"data_port":1,"pid":1,"instance":"x"}"#)
                .is_err()
        );
        assert!(parse_ready(
            r#"{"ready":false,"rpc_port":1,"data_port":1,"pid":1,"instance":"x"}"#
        )
        .is_err());
    }

    #[test]
    fn only_loopback_data_plane_urls_are_accepted() {
        assert_eq!(
            split_loopback_url("http://127.0.0.1:51235/media/mxc/e_abc").unwrap(),
            (51235, "/media/mxc/e_abc".to_string())
        );
        assert!(split_loopback_url("http://example.org:51235/media/mxc/e_abc").is_err());
        assert!(split_loopback_url("https://127.0.0.1:51235/media").is_err());
        assert!(split_loopback_url("http://127.0.0.1:notaport/media").is_err());
    }

    #[test]
    fn a_response_without_a_code_is_not_success() {
        assert_eq!(code_of(&json!({ "code": 0 })), 0);
        assert_eq!(code_of(&json!({ "result": {} })), 108);
    }

    /// 回一則固定的 HTTP 回應的假資料平面（只收一次連線）。
    async fn answering_once(status_line: &str, body: &'static str) -> u16 {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let reply = format!(
            "HTTP/1.1 {status_line}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 1024];
            let _ = socket.read(&mut request).await;
            socket.write_all(reply.as_bytes()).await.unwrap();
        });
        port
    }

    /// 不是資料的回應（例 502）一個 byte 都不寫進輸出：錯誤的 JSON 混進下載的檔比沒寫更糟。它在 `refusal`。
    #[tokio::test]
    async fn a_refusal_is_not_written_where_the_file_goes() {
        let port = answering_once("502 Bad Gateway", r#"{"code":1100}"#).await;
        let mut sink = Vec::new();
        let reply = get(
            &format!("http://127.0.0.1:{port}/media/mxc/x"),
            None,
            &mut sink,
        )
        .await
        .unwrap();
        assert_eq!(reply.status, 502);
        assert!(sink.is_empty(), "{sink:?}");
        assert_eq!(reply.refusal.as_deref(), Some(r#"{"code":1100}"#));

        let port = answering_once("200 OK", "the bytes").await;
        let mut sink = Vec::new();
        let reply = get(
            &format!("http://127.0.0.1:{port}/media/mxc/x"),
            None,
            &mut sink,
        )
        .await
        .unwrap();
        assert_eq!(
            (reply.status, sink.as_slice()),
            (200, b"the bytes".as_slice())
        );
        assert_eq!(reply.refusal, None);
    }
}

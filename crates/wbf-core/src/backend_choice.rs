//! 「這台 homeserver 要用哪一套協議講話，走哪條管子？」
//!
//! ## 兩個軸，🚫 不要混
//!
//! | | 意思 | 值 |
//! |---|---|---|
//! | **backend** | 用哪一套**協議** | matrix-sdk（標準 Matrix）／wbf 的 pack 協議 |
//! | **transport** | wbf 協議走哪條**管子** | `ws`（預設）／`http` |
//!
//! 🚫 `--transport http` **不是**「退回標準 Matrix」，是「wbf 協議走 HTTP」。這兩個正交，
//! 而它們在文件與程式裡被混用過（daemon-runtime §3.5）。
//!
//! ## 誰決定 backend
//!
//! **探測，🚫 不是設定**（architecture-v2 §6.1）：問一次 `Hello`，對方講得出 wbf 的
//! features 就是 wbf，否則是一般 homeserver。⭐ 不確定一律落到 matrix-sdk ——
//! 壞在「用了比較慢但一定能動的那條」，🚫 不壞在「以為對方懂我們的協議」。
//!
//! ## 誰決定 transport
//!
//! [`get_transport_plan`]。⚠️ 它是**純函數**，所以那張規則表測得到 ——
//! 這種「哪個組合該報錯」的判斷散在呼叫點上，遲早有一格漏掉，而漏掉的那格是放行。

use wbf_sdk::Transport;

use crate::error::{CoreError, CoreErrorKind};

/// 用哪一套協議跟 homeserver 講話。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackendKind {
    /// 一般 Matrix（Synapse、Dendrite……）。⭐ 探不出來就是這個。
    MatrixSdk,
    /// wbfuwunel：講 wbf-pack。
    Wbf,
}

/// 一個方法對管子的要求。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransportNeed {
    /// 兩條都行（一問一答）。
    Either,
    /// 🚨 **只有 WS**：回應不只一個 pack（`Event/Recent` 的一串 `Batch`），
    /// 或根本是 server 主動推的（`Subscribe`／`Push`／`Device/*`）。
    /// HTTP 一請求只回一個 pack（`channel.rs` 的 `HttpChannel::request_stream`）。
    WebSocketOnly,
}

/// 這次實際要走哪條管子。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransportPlan {
    /// 🚫 **這個 backend 沒有管子可選**（matrix-sdk 只有一種講法）。
    /// ⭐ 呼叫端指定了什麼都**照樣做事、不報錯** —— 那是個 no-op，不是錯誤。
    NotApplicable,
    /// 用這一條。
    Use(Transport),
}

/// 這次要走哪條管子 —— 順便把「問不出東西的組合」擋掉。
///
/// | backend | 顯式 `ws` | 顯式 `http` | 沒指定 |
/// |---|---|---|---|
/// | matrix-sdk | `NotApplicable` | `NotApplicable` | `NotApplicable` |
/// | wbf | `Use(WebSocket)` | `Use(Http)` | `Use(WebSocket)` |
/// | wbf ＋ `WebSocketOnly` | `Use(WebSocket)` | **`Err(Usage)`** | `Use(WebSocket)` |
///
/// ⚠️ **matrix-sdk 那一列不看 `requested`**（維護者 2026-09-13：「顯式調用 ws no op 不報錯，
/// 它就只有一種」）。📎 `http` 也一樣 no-op：同一個理由 —— 那個 backend 根本沒有這個維度，
/// 🚫 為一個不存在的選擇報錯只是讓前端得先知道對方是誰才敢送參數。
///
/// 🚨 **`WebSocketOnly` ＋ 顯式 `http` 是報錯，🚫 不是默默改用 WS**：呼叫端說 http 通常是有
/// 理由的（除錯、環境擋 WS），偷偷換掉會讓它以為驗過的是 http 那條路（原則 A5 fail closed）。
///
/// Args:
///     backend: 探測的結果, example: BackendKind::Wbf
///     requested: 呼叫端顯式指定的；`None` = 沒指定, example: Some(Transport::Http)
///     need: 這個方法撐不撐得住 HTTP, example: TransportNeed::WebSocketOnly
/// Return:
///     Ok(NotApplicable)  matrix-sdk：沒有這個維度，指定了也不算錯
///     Ok(Use(t))         wbf：走這一條
///     Err(Usage)         wbf ＋ 只支援 WS 的方法 ＋ 呼叫端硬要 http
pub fn get_transport_plan(
    backend: BackendKind,
    requested: Option<Transport>,
    need: TransportNeed,
) -> Result<TransportPlan, CoreError> {
    if backend == BackendKind::MatrixSdk {
        return Ok(TransportPlan::NotApplicable);
    }
    match (requested, need) {
        (Some(Transport::Http), TransportNeed::WebSocketOnly) => Err(CoreError::new(
            CoreErrorKind::Usage,
            "this method needs the WebSocket transport: its answer is more than one pack \
             (or the server pushes it), and one HTTP request carries exactly one pack. \
             Drop `transport` to use the default (ws).",
        )),
        // 沒指定就是 ws（維護者 2026-09-13）。⚠️ 「沒指定」與「指定 ws」走同一格是刻意的：
        // 🚫 不要讓「沒說」比「說了」拿到不一樣的東西。
        (None, _) | (Some(Transport::WebSocket), _) => {
            Ok(TransportPlan::Use(Transport::WebSocket))
        }
        (Some(Transport::Http), TransportNeed::Either) => Ok(TransportPlan::Use(Transport::Http)),
    }
}

/// 探測要用的 client 名字。⚠️ server 會 log 它，所以講得出是誰在問。
const PROBE_CLIENT_NAME: &str = "wbf-client probe";

impl crate::Core {
    /// 這台 homeserver 講不講 wbf-pack。**探測，🚫 不是設定**（architecture-v2 §6.1）。
    ///
    /// ⭐ **不確定一律回 [`BackendKind::MatrixSdk`]**：連不上、`Hello` 不回、token 被拒、
    /// 回來的東西看不懂 —— 全部當成一般 homeserver。壞在「用了比較慢但一定能動的那條」，
    /// 🚫 不壞在「以為對方懂我們的協議」。所以這個函數**不回 `Err`**：探測失敗不是錯誤，
    /// 是一個答案。
    ///
    /// ⚠️ 一個 server dir 探一次就記住（`Core::backends`），🚫 不寫進磁碟。
    ///
    /// Args:
    ///     account: 哪個帳號（它決定 server dir 與 access_token）
    /// Return:
    ///     BackendKind  Wbf ＝ `Hello` 通了；MatrixSdk ＝ 其他所有情況
    pub async fn get_backend_kind(&self, account: &crate::accounts::AccountDir) -> BackendKind {
        let dir = account.server_dir();
        if let Some(known) = self
            .backends
            .lock()
            .expect("the backend registry is never poisoned")
            .get(&dir)
        {
            return *known;
        }
        let found = match self.probe_wbf(account).await {
            Ok(true) => BackendKind::Wbf,
            Ok(false) | Err(_) => BackendKind::MatrixSdk,
        };
        self.events.progress(format!(
            "{} speaks {}",
            account.server_host,
            match found {
                BackendKind::Wbf => "wbf-pack",
                BackendKind::MatrixSdk => "plain Matrix",
            }
        ));
        self.backends
            .lock()
            .expect("the backend registry is never poisoned")
            .insert(dir, found);
        found
    }

    /// 🚫 **只有階段 7／8 的會話監督者該叫它**：連線重起時，「這台是不是 wbf」要重問一次。
    ///
    /// Args:
    ///     account: 哪個帳號
    /// Return:
    ///     bool  true ＝ 本來記著、現在忘了；false ＝ 本來就沒探過
    pub fn forget_backend_probe(&self, account: &crate::accounts::AccountDir) -> bool {
        self.backends
            .lock()
            .expect("the backend registry is never poisoned")
            .remove(&account.server_dir())
            .is_some()
    }

    /// 🚨 **探測一律走 WS**：wbf backend 的預設就是 WS，而「HTTP 連得上」證明不了對方講 wbf
    /// （任何 HTTP server 都會回東西）。⚠️ 這裡🚫 不看 conf 的 `TRANSPORT` ——
    /// 那是「之後怎麼講話」的上限，不是「怎麼問你是誰」。
    ///
    /// Return:
    ///     Ok(true)   `Hello` 回了、協議版本認得
    ///     Ok(false)  接得上但講的不是我們認得的協議版本
    ///     Err(...)   連不上、token 被拒、逾時 —— 呼叫端一律當成「不是 wbf」
    async fn probe_wbf(&self, account: &crate::accounts::AccountDir) -> Result<bool, CoreError> {
        let mut client = self
            .connect_wbf_client(account, Transport::WebSocket)
            .await?;
        let hello = client.hello(PROBE_CLIENT_NAME).await?;
        Ok(hello.protocol == wbf_sdk::protocol::PROTOCOL_VERSION)
    }

    /// 🚨 **wbf-pack 通道的唯一閘門**：探 backend、按規則挑管子、再開。
    ///
    /// ⭐ 「哪個組合可以、哪個要報錯」只有這一個地方在判斷（原則 A4 的接縫）——
    /// 🚫 散在十個呼叫點上遲早漏掉一格，而漏掉的那格是放行。
    ///
    /// Args:
    ///     account: 哪個帳號
    ///     requested: 呼叫端顯式指定的管子；`None` = 沒指定（就是預設的 ws）
    ///     need: 這個方法撐不撐得住 HTTP, example: TransportNeed::WebSocketOnly
    /// Return:
    ///     Ok(WbfClient)  開好了
    ///     Err(Usage)     這台不講 wbf-pack；或只支援 WS 的方法被指定了 http
    ///     Err(Network)   探到是 wbf，但那條管子開不起來（維護者 2026-09-13：ws 開失敗要報錯，
    ///                    http 出錯也一樣）
    pub(crate) async fn client_of(
        &self,
        account: &crate::accounts::AccountDir,
        requested: Option<Transport>,
        need: TransportNeed,
    ) -> Result<wbf_sdk::client::WbfClient<wbf_sdk::channel::Channel>, CoreError> {
        let backend = self.get_backend_kind(account).await;
        match get_transport_plan(backend, requested, need)? {
            TransportPlan::Use(transport) => self.connect_wbf_client(account, transport).await,
            // ⚠️ 探到不是 wbf，而這條路只有 wbf 講得出來 —— 🚫 不要連上去讓它在更深的地方
            // 用一個看不懂的錯誤失敗。⭐ 這是「這台 server 沒有這個能力」，不是傳輸的問題。
            TransportPlan::NotApplicable => Err(CoreError::new(
                CoreErrorKind::Usage,
                format!(
                    "{} does not speak wbf-pack, and this method has no plain-Matrix path yet",
                    account.server_host
                ),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// matrix-sdk 只有一種講法：指定什麼都是 no-op，🚫 一個都不該報錯。
    #[test]
    fn the_matrix_backend_ignores_transport_instead_of_refusing_it() {
        for requested in [None, Some(Transport::WebSocket), Some(Transport::Http)] {
            for need in [TransportNeed::Either, TransportNeed::WebSocketOnly] {
                assert_eq!(
                    get_transport_plan(BackendKind::MatrixSdk, requested, need).unwrap(),
                    TransportPlan::NotApplicable,
                    "requested={requested:?} need={need:?}"
                );
            }
        }
    }

    /// wbf 沒指定就是 ws —— 而且跟「明說 ws」是同一個答案。
    #[test]
    fn wbf_defaults_to_the_websocket_and_saying_so_changes_nothing() {
        for need in [TransportNeed::Either, TransportNeed::WebSocketOnly] {
            assert_eq!(
                get_transport_plan(BackendKind::Wbf, None, need).unwrap(),
                TransportPlan::Use(Transport::WebSocket)
            );
            assert_eq!(
                get_transport_plan(BackendKind::Wbf, Some(Transport::WebSocket), need).unwrap(),
                TransportPlan::Use(Transport::WebSocket)
            );
        }
    }

    #[test]
    fn wbf_takes_http_when_the_method_can_answer_in_one_pack() {
        assert_eq!(
            get_transport_plan(BackendKind::Wbf, Some(Transport::Http), TransportNeed::Either)
                .unwrap(),
            TransportPlan::Use(Transport::Http)
        );
    }

    /// 🚨 只支援 WS 的方法被硬指定 http：報錯，🚫 不是默默改用 WS
    /// ——偷偷換掉會讓呼叫端以為它驗過的是 http 那條路。
    #[test]
    fn asking_for_http_on_a_websocket_only_method_is_refused_not_quietly_upgraded() {
        let error = get_transport_plan(
            BackendKind::Wbf,
            Some(Transport::Http),
            TransportNeed::WebSocketOnly,
        )
        .unwrap_err();
        assert_eq!(error.kind, CoreErrorKind::Usage);
        assert!(error.message.contains("one pack"), "要說出為什麼");
    }
}

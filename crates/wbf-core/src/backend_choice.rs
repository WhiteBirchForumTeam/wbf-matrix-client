//! 「這個呼叫要用哪一套協議跟 homeserver 講話？」
//!
//! ## 一個軸，🚫 不是兩個（維護者 2026-09-13 簡化）
//!
//! `transport` 有兩個值，而**它們各自就是一個 backend**：
//!
//! | `transport` | 協議 | 誰實作 |
//! |---|---|---|
//! | **`ws`**（預設） | wbf 客製協議 | `wbf-sdk` 的 pack over WebSocket |
//! | **`http`** | 原生 Matrix HTTP | `matrix-sdk` |
//!
//! 🚫 **wbf 底下不再細分 ws／http —— wbf 協議一律 WS。** pack-over-HTTP（`channel.rs` 的
//! `HttpChannel`）只剩 **debug** 用途：⚠️ 它不是「wbf 的 HTTP 模式」，🚫 不該當成正式路徑。
//! 📎 之前的設計在 wbf 底下又分了一層管子 —— 那是多的一層，拿掉了。
//!
//! ## 所以解析很短
//!
//! ```text
//! http ─────────────────────────> matrix-sdk（永遠）
//! ws ──┬── 這台不講 wbf ────────> matrix-sdk（🚫 不報錯，那是 no-op）
//!      ├── 這個方法還沒有 ws ───> matrix-sdk（🚧 暫時）
//!      └── 其他 ────────────────> wbf
//! ```
//!
//! ⚠️ 只有一種情況**報錯**：這個功能**只有 wbf 講得出來**，而這條路到不了 wbf ——
//! 那時它就是**關的**（維護者 2026-09-13：「萬一 ui 遇到 feature 需要用到 ws，那就是關掉」）。
//! ⭐ 講出來比默默給一個空答案好。
//!
//! ## 🚧 那份「還沒有 ws」的清單
//!
//! wbf 還沒把原生 HTTP 全部取代掉，所以有些方法**就算走 ws 也得先用 matrix-sdk**，
//! 等 server 端補上 ws 的定義再搬過去。
//!
//! 🚫 清單**不是一串字串**：每個呼叫點自己用 [`MethodHome`] 說出它住在哪一邊 ——
//! ⭐ 那樣「名字」跟「實際走哪條」不可能漂移（原則 A4），而 `MethodHome::StillOnMatrixSdk`
//! 的呼叫點就是那份清單。📎 給人看的版本是 rpc-spec §10「底層」那一欄。

use wbf_sdk::Transport;

use crate::error::{CoreError, CoreErrorKind};

/// 用哪一套協議跟 homeserver 講話。⚠️ 這是**結果**，🚫 不是呼叫端給的參數。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackendKind {
    /// 原生 Matrix HTTP（`matrix-sdk`）。
    MatrixSdk,
    /// wbf 客製協議（pack over WebSocket）。
    WbfSdk,
}

/// 一個方法現在**住在哪一邊**。
///
/// ⭐ 每個呼叫點自己講，所以這個 enum 的用法**就是**那份清單。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MethodHome {
    /// **只有 wbf 講得出來**：上傳、媒體、`Recent`、`Hello`／`Ping`。
    /// ⚠️ 走 `http`、或這台不是 wbf → 這個功能是**關的**（報錯，🚫 不裝作沒事）。
    WbfSdkOnly,
    /// 兩邊都有定義：`ws` 走 wbf，`http` 走 matrix-sdk。
    BothSides,
    /// 🚧 **暫時**：wbf 那邊還沒有定義，所以**連 `ws` 也先走 matrix-sdk**。
    /// ⭐ server 端補上之後，把呼叫點改成 [`MethodHome::BothSides`] 就搬過去了 ——
    /// 🚫 不必動 rpc-spec，前端看不到這件事。
    StillOnMatrixSdk,
}

/// 這次用哪一套協議 —— 順便把「這個功能在這條路上是關的」講出來。
///
/// | `transport` | 這台講 wbf | `WbfSdkOnly` | `BothSides` | `StillOnMatrixSdk` |
/// |---|---|---|---|---|
/// | `http` | 不管 | **`Err`**（feature 關掉） | `MatrixSdk` | `MatrixSdk` |
/// | `ws` | ❌ | **`Err`**（這台不講 wbf） | `MatrixSdk`（no-op） | `MatrixSdk` |
/// | `ws` | ✅ | `Wbf` | `Wbf` | `MatrixSdk`（🚧 暫時） |
///
/// ⭐ **`ws` 打到一般 homeserver 不是錯**，是 no-op —— 前端不必先知道對方是誰才敢送參數
/// （維護者 2026-09-13：「如果 server 是 old style matrix 那套，就走 matrix sdk backend，no op」）。
///
/// 🚨 **沒帶 `transport` 就是 `ws`**（維護者 2026-09-13），所以**預設會落到上面那兩列 `ws`**：
/// 對方講 wbf 就用 wbf，不講就 **fallback 到 matrix-sdk**，⚠️ 兩種都🚫 不報錯。
/// 📎 這個函數收的是**已經定好**的 `transport`；「沒帶」在外面就變成 `ws` 了，
/// 而那份預設只有一個地方寫著 —— `Transport::default()`（`wbf-sdk` 的 `channel.rs`）。
///
/// Args:
///     transport: 呼叫端要的那條, example: Transport::WebSocket
///     server_speaks_wbf: 探測的結果（[`crate::Core::get_backend_kind`]）, example: true
///     home: 這個方法住在哪一邊, example: MethodHome::WbfSdkOnly
/// Return:
///     Ok(BackendKind)  用這一套
///     Err(Usage)       只有 wbf 有這個功能，而這條路到不了 wbf —— 它是關的
pub fn get_backend_for(
    transport: Transport,
    server_speaks_wbf: bool,
    home: MethodHome,
) -> Result<BackendKind, CoreError> {
    // 🚧 這一格優先：wbf 那邊還沒有定義，所以連 `ws` 也到不了它。
    if home == MethodHome::StillOnMatrixSdk {
        return Ok(BackendKind::MatrixSdk);
    }
    let reaches_wbf = transport == Transport::WebSocket && server_speaks_wbf;
    match (reaches_wbf, home) {
        (true, _) => Ok(BackendKind::WbfSdk),
        (false, MethodHome::BothSides) => Ok(BackendKind::MatrixSdk),
        // ⚠️ 兩種「到不了 wbf」的理由要分開講：一個是呼叫端自己選的，一個是對方的事實。
        (false, MethodHome::WbfSdkOnly) => Err(CoreError::new(
            CoreErrorKind::Usage,
            match transport {
                Transport::Http => {
                    "this feature only exists in the wbf protocol, which runs over the ws \
                     transport: over http it is off"
                }
                Transport::WebSocket => {
                    "this homeserver does not speak the wbf protocol, and this feature only \
                     exists there"
                }
            },
        )),
        // `StillOnMatrixSdk` 在函數開頭就回掉了。
        (false, MethodHome::StillOnMatrixSdk) => Ok(BackendKind::MatrixSdk),
    }
}

/// 探測要用的 client 名字。⚠️ server 會 log 它，所以講得出是誰在問。
const PROBE_CLIENT_NAME: &str = "wbf-client probe";

impl crate::Core {
    /// 這台 homeserver 講不講 wbf 協議。**探測，🚫 不是設定**（architecture-v2 §6.1；account-session.md §1）。
    ///
    /// ⭐ **不確定一律回 [`BackendKind::MatrixSdk`]**：連不上、`Hello` 不回、回來的東西看不懂——
    /// 全部當成一般 homeserver。壞在「用了比較慢但一定能動的那條」，🚫 不壞在「以為對方懂我們的協議」。
    /// 所以這個函數**不回 `Err`**：探測失敗不是錯誤，是一個答案。
    ///
    /// 🚨 **一台 server 一格，而且只有「server 自己回答過的」才記住**：
    ///
    /// | 探測結果 | 這次回 | 記住嗎 |
    /// |---|---|---|
    /// | `Hello` 回了、版本認得 | `WbfSdk` | ✅ |
    /// | `Hello` 回了、版本不認得 | `MatrixSdk` | ✅ |
    /// | 連不上／逾時 | `MatrixSdk` | 🚫 **不記**（下次重探） |
    ///
    /// 📌 探活**不帶 token**（`WsChannel::connect_anonymous` → `Hello` → 丟掉）：server 允許未登入的升級、只接受 Hello／Ping。
    /// 所以 key 是 **server URL**——之前以帳號為鍵是因為拿帳號的 token 去探（PR #33 審查 rumia：A 的 token 壞了不能拖累 B），
    /// 不帶 token 之後那個理由沒了，而「講不講 wbf」本來就是 server 的事實；同一台 server 的 N 個帳號共用一次。
    /// 同時進來的呼叫共用一次探測（`OnceCell::get_or_try_init`：出錯不寫進去、成功才寫）。只在記憶體裡，🚫 不寫進磁碟；
    /// daemon 重開自然作廢，監督者重連時用 [`Core::forget_backend_probe`] 重探。
    ///
    /// Args:
    ///     server: homeserver base URL, example: "http://localhost:6167"
    /// Return:
    ///     BackendKind  WbfSdk ＝ `Hello` 通了；MatrixSdk ＝ 其他所有情況
    pub async fn get_backend_kind_of_server(&self, server: &str) -> BackendKind {
        let key = server.trim_end_matches('/').to_string();
        let cell = self
            .backends
            .lock()
            .expect("the backend registry is never poisoned")
            .entry(key)
            .or_default()
            .clone();
        // ⚠️ 註冊表的鎖在上面那一段就放掉了 —— 🚫 std 的 Mutex 不能跨 await 持有。
        let answered = cell
            .get_or_try_init(|| async {
                let kind = match self.probe_wbf(server).await? {
                    true => BackendKind::WbfSdk,
                    false => BackendKind::MatrixSdk,
                };
                self.events.progress(format!(
                    "{server} speaks {}",
                    match kind {
                        BackendKind::WbfSdk => "the wbf protocol",
                        BackendKind::MatrixSdk => "plain Matrix",
                    }
                ));
                Ok::<BackendKind, CoreError>(kind)
            })
            .await;
        match answered {
            Ok(kind) => *kind,
            // ⚠️ 問不到就當一般 homeserver —— **只算這一次**，🚫 沒有記下來。
            Err(_) => BackendKind::MatrixSdk,
        }
    }

    /// 同上，server 從帳號的 session 拿。沒登入的帳號沒有 server 可問 → `MatrixSdk`（fail safe，🚫 不探、不記）。
    ///
    /// 📌 登入時就走了 wbf 那條的帳號（`Session::backend == WbfSdk`，account-session.md §2）**不再探**：答案登入時就定了，
    /// 而 server 暫時不通時把它探成「一般 Matrix」，會讓池那條路回「你接錯線了」而不是 `Network`——錯的那個訊息。
    ///
    /// Args:
    ///     account: 哪個帳號
    pub async fn get_backend_kind(&self, account: &crate::accounts::AccountDir) -> BackendKind {
        match self.session_of(account) {
            Ok(session) if session.backend == Some(wbf_sdk::login::SessionBackend::WbfSdk) => {
                BackendKind::WbfSdk
            }
            Ok(session) => self.get_backend_kind_of_server(&session.server).await,
            Err(_) => BackendKind::MatrixSdk,
        }
    }

    /// 註冊表現在有幾格。⚠️ 只給測試用：「一台 server 一格」是格數唯一看得出來的地方。
    #[cfg(test)]
    pub(crate) fn count_probe_cells(&self) -> usize {
        self.backends
            .lock()
            .expect("the backend registry is never poisoned")
            .len()
    }

    /// 直接替這台 server 記一個探測結果。⚠️ 只給測試用：成功的探測要一台真的 wbf server。
    #[cfg(test)]
    pub(crate) fn set_remembered_backend(&self, server: &str, kind: BackendKind) {
        let cell = tokio::sync::OnceCell::new_with(Some(kind));
        self.backends
            .lock()
            .expect("the backend registry is never poisoned")
            .insert(
                server.trim_end_matches('/').to_string(),
                std::sync::Arc::new(cell),
            );
    }

    /// 這台 server **現在記住了什麼**。⚠️ 只給測試用：唯一能分辨「記住了」與「只是回了一次」的方式就是問它。
    ///
    /// Return:
    ///     Some(BackendKind)  探過而且 server 回答過
    ///     None               沒探過，或探了但**失敗**（🚫 失敗不留下結論）
    #[cfg(test)]
    pub(crate) fn get_remembered_backend(&self, server: &str) -> Option<BackendKind> {
        self.backends
            .lock()
            .expect("the backend registry is never poisoned")
            .get(server.trim_end_matches('/'))
            .and_then(|cell| cell.get().copied())
    }

    /// 忘掉這台 server 的探測結果——**重連時要重問一次**（🚧 第 8 階段的監督者用；現在沒有呼叫點）。
    /// 📎 登入、登出🚫 不叫它：探活不帶 token，session 換了或沒了都不影響「這台講不講 wbf」。
    ///
    /// Args:
    ///     server: homeserver base URL
    /// Return:
    ///     bool  true ＝ 本來記著、現在忘了；false ＝ 本來就沒探過
    pub fn forget_backend_probe(&self, server: &str) -> bool {
        self.backends
            .lock()
            .expect("the backend registry is never poisoned")
            .remove(server.trim_end_matches('/'))
            .is_some()
    }

    /// 🚨 **探測一律走 WS、不帶 token**：wbf 協議就是 WS，而「HTTP 連得上」證明不了對方講 wbf
    /// （任何 HTTP server 都會回東西）。server 允許未登入的升級、30 秒內只接受 Hello／Ping：問完就丟掉那條線。
    ///
    /// Return:
    ///     Ok(true)   `Hello` 回了、協議版本認得
    ///     Ok(false)  接得上但講的不是我們認得的協議版本
    ///     Err(...)   連不上、逾時 —— 呼叫端一律當成「不是 wbf」
    async fn probe_wbf(&self, server: &str) -> Result<bool, CoreError> {
        let channel = wbf_sdk::channel::WsChannel::connect_anonymous(server).await?;
        let mut client = wbf_sdk::client::WbfClient::new(wbf_sdk::channel::Channel::WebSocket(
            Box::new(channel),
        ));
        let hello = client.hello(PROBE_CLIENT_NAME, &[]).await?;
        Ok(hello.protocol == wbf_sdk::protocol::PROTOCOL_VERSION)
    }
    /// 🚨 **wbf 通道的唯一閘門**：探 backend、照 [`get_backend_for`] 判、再開。
    ///
    /// ⭐ 「哪條路到得了 wbf、哪條是關的」只有這一個地方在判斷（原則 A4 的接縫）——
    /// 🚫 散在十個呼叫點上遲早漏掉一格，而漏掉的那格是放行。
    ///
    /// ⚠️ 回得到東西就一定是 **wbf over WS**。要 matrix-sdk 的呼叫端走
    /// [`crate::Core::synced_backend_of`]，🚫 不是這裡。
    ///
    /// Args:
    ///     account: 哪個帳號
    ///     transport: 呼叫端要的那條, example: Transport::WebSocket
    ///     home: 這個方法住在哪一邊, example: MethodHome::WbfSdkOnly
    ///     role: 走池裡哪一條線（link-pool.md §2：角色是呼叫點的屬性）, example: LinkRole::Misc
    /// Return:
    ///     Ok(PooledClient)  池裡那條線（沒開就開、死了重開）；丟掉就還回去
    ///     Err(Usage)        這條路到不了 wbf，而這個功能只有它有 —— 它是關的；或帳號沒登入
    ///     Err(Network)      到得了，但 WS 開不起來（維護者 2026-09-13：開失敗要報錯）
    pub(crate) async fn client_of(
        &self,
        account: &crate::accounts::AccountDir,
        transport: Transport,
        home: MethodHome,
        role: crate::link_pool::LinkRole,
    ) -> Result<crate::link_pool::PooledClient, CoreError> {
        let speaks_wbf = self.get_backend_kind(account).await == BackendKind::WbfSdk;
        match get_backend_for(transport, speaks_wbf, home)? {
            BackendKind::WbfSdk => {
                let pool = self.pool_of_account(account)?;
                pool.acquire(role, || self.open_link(account, role)).await
            }
            // ⚠️ `BothSides`／`StillOnMatrixSdk` 落到這裡：這個呼叫該走 matrix-sdk，而它
            // 🚫 不該來要 wbf 的 client。⭐ 這是程式接錯線，不是使用者填錯參數 ——
            // 所以訊息講「你要錯東西了」，而不是「你的參數不對」。
            BackendKind::MatrixSdk => Err(CoreError::new(
                CoreErrorKind::Usage,
                "this call resolves to the matrix-sdk backend, so it must not ask for a wbf \
                 client: use the matrix backend instead",
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ⚠️ 沒有人在這個 port 上聽 —— 探測一定連不上，那正是要驗的那條路。
    /// 📎 用 `127.0.0.1` 而不是一個不存在的網域：🚫 不要讓測試去打 DNS。
    const DEAD: &str = "http://127.0.0.1:1";

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("wbf-core-bc-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn unlocked(dir: &std::path::Path) -> crate::Core {
        wbf_sdk::vault::Vault::create(dir, &wbf_sdk::Unlock::NoPassphrase).unwrap();
        let core = crate::Core::open(dir);
        core.unlock(None).unwrap();
        core
    }

    /// `http` 永遠是 matrix-sdk —— 🚫 沒有「wbf over http」這種東西了。
    #[test]
    fn http_always_means_the_matrix_backend() {
        for speaks_wbf in [true, false] {
            for home in [MethodHome::BothSides, MethodHome::StillOnMatrixSdk] {
                assert_eq!(
                    get_backend_for(Transport::Http, speaks_wbf, home).unwrap(),
                    BackendKind::MatrixSdk,
                    "speaks_wbf={speaks_wbf} home={home:?}"
                );
            }
        }
    }

    /// ⭐ `ws` 打到一般 homeserver **不是錯**，是 no-op：照樣用 matrix-sdk 做事。
    #[test]
    fn asking_for_ws_on_a_plain_homeserver_is_a_no_op_not_an_error() {
        assert_eq!(
            get_backend_for(Transport::WebSocket, false, MethodHome::BothSides).unwrap(),
            BackendKind::MatrixSdk
        );
    }

    #[test]
    fn ws_on_a_wbf_homeserver_uses_the_wbf_protocol() {
        assert_eq!(
            get_backend_for(Transport::WebSocket, true, MethodHome::BothSides).unwrap(),
            BackendKind::WbfSdk
        );
        assert_eq!(
            get_backend_for(Transport::WebSocket, true, MethodHome::WbfSdkOnly).unwrap(),
            BackendKind::WbfSdk
        );
    }

    /// 🚨 **預設那條路**：沒帶 `transport` ＝ `ws`（`Settings::transport` 的預設），
    /// 所以預設行為就是「對方講 wbf 就用 wbf，不講就 fallback 到 matrix-sdk」——
    /// ⚠️ 兩種都🚫 **不報錯**。這條測試釘的是那個預設，🚫 不是某個特例。
    #[test]
    fn the_default_path_is_ws_and_it_falls_back_instead_of_failing() {
        // ⭐ 問的是**那一份預設**（`Transport::default()`），🚫 不是這裡抄一個值來比 ——
        // 抄一份就變成「WebSocket == WebSocket」，那證明不了任何事。
        assert_eq!(Transport::default(), Transport::WebSocket, "預設是 ws");
        assert_eq!(
            get_backend_for(Transport::default(), true, MethodHome::BothSides).unwrap(),
            BackendKind::WbfSdk,
            "對方講 wbf：用 wbf"
        );
        assert_eq!(
            get_backend_for(Transport::default(), false, MethodHome::BothSides).unwrap(),
            BackendKind::MatrixSdk,
            "對方不講：fallback，🚫 不是報錯"
        );
    }

    /// 🚨 **探測失敗不留下結論** —— 真的跑 `get_backend_kind_of_server`（PR #33 審查 rumia🔴1）。
    ///
    /// 位址沒有人在聽，探測一定失敗。它要：回 `MatrixSdk`（fail safe），但🚫 **不准記住**——
    /// 不然下次那台 server 起來了也永遠被當成一般 homeserver。⭐ 「記住了沒有」是唯一分辨得出來的地方，所以斷言的是它。
    #[tokio::test]
    async fn a_failed_probe_answers_matrix_sdk_without_remembering_it() {
        let dir = scratch("probe-fails");
        let core = unlocked(&dir);
        assert_eq!(
            core.get_backend_kind_of_server(DEAD).await,
            BackendKind::MatrixSdk,
            "探不到就當一般 homeserver"
        );
        assert_eq!(
            core.get_remembered_backend(DEAD),
            None,
            "🚫 失敗不准留下結論"
        );
        // 再問一次還是一樣的答案，而且還是沒記住（所以下一次仍然會重探）。
        assert_eq!(
            core.get_backend_kind_of_server(DEAD).await,
            BackendKind::MatrixSdk
        );
        assert_eq!(core.get_remembered_backend(DEAD), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 🚨 **一台 server 一格**：同一台 server 的兩個帳號共用一次探測（account-session.md §1）。
    /// 探活不帶 token，所以沒有「A 的 token 壞了拖累 B」（PR #33 那條反過來的理由）；「講不講 wbf」是 server 的事實。
    /// ⚠️ 這條在「key 是帳號目錄」的舊寫法上會紅（那時是兩格）。
    #[tokio::test]
    async fn two_accounts_on_one_server_share_one_probe() {
        let dir = scratch("two-accounts");
        let core = unlocked(&dir);
        let key = core.vault().unwrap().account_dir_key();
        let accounts: Vec<_> = ["@a:dead", "@b:dead"]
            .into_iter()
            .map(|user| {
                let account = crate::accounts::AccountDir::locate(&dir, &key, DEAD, user).unwrap();
                std::fs::create_dir_all(&account.dir).unwrap();
                seal_dead_session(&core, &account, user);
                account
            })
            .collect();
        let (first, second) = tokio::join!(
            core.get_backend_kind(&accounts[0]),
            core.get_backend_kind(&accounts[1])
        );
        assert_eq!(
            (first, second),
            (BackendKind::MatrixSdk, BackendKind::MatrixSdk)
        );
        assert_eq!(core.count_probe_cells(), 1, "一台 server 一格");
        assert_eq!(core.get_remembered_backend(DEAD), None, "🚫 失敗不留下結論");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 沒登入的帳號問不出 server：fail safe 回 `MatrixSdk`，🚫 不探、不記。
    #[tokio::test]
    async fn an_account_without_a_session_is_treated_as_plain_matrix() {
        let dir = scratch("no-session");
        let core = unlocked(&dir);
        let key = core.vault().unwrap().account_dir_key();
        let account = crate::accounts::AccountDir::locate(&dir, &key, DEAD, "@a:dead").unwrap();
        assert_eq!(
            core.get_backend_kind(&account).await,
            BackendKind::MatrixSdk
        );
        assert_eq!(core.count_probe_cells(), 0, "沒有 server 可問，連格都不開");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 探測結論屬於 **server**，帳號登出不影響它（探活不帶 token，PR #33 那條「session 沒了結論跟著走」不再成立）。
    /// ⭐ 這條**真的跑 `Core::log_out`**：驗的是登出**沒有**接到 forget。
    #[tokio::test]
    async fn logging_out_keeps_the_servers_probe_result() {
        let dir = scratch("logout-keeps");
        let core = unlocked(&dir);
        let key = core.vault().unwrap().account_dir_key();
        let account = crate::accounts::AccountDir::locate(&dir, &key, DEAD, "@a:dead").unwrap();
        std::fs::create_dir_all(&account.dir).unwrap();
        // 🚫 不封 session：`is_logged_in()` 是 false，logout 不必連網路就會走到本地清理。
        core.set_remembered_backend(DEAD, BackendKind::WbfSdk);
        core.log_out("@a:dead", None, true, false)
            .await
            .expect("沒有 session 的帳號，登出只是清本地");
        assert_eq!(
            core.get_remembered_backend(DEAD),
            Some(BackendKind::WbfSdk),
            "server 的事實不因為一個帳號登出而變"
        );
        assert!(core.forget_backend_probe(DEAD), "監督者重連時用這個重探");
        assert_eq!(core.get_remembered_backend(DEAD), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 登出的封池（account-session.md §4）：封了 `pool_of_account` 就拒（`AccountBusy`），解封就過。
    #[tokio::test]
    async fn logging_out_blocks_the_pool_until_unblocked() {
        let dir = scratch("logout-blocks");
        let core = unlocked(&dir);
        let key = core.vault().unwrap().account_dir_key();
        let account = crate::accounts::AccountDir::locate(&dir, &key, DEAD, "@a:dead").unwrap();
        std::fs::create_dir_all(&account.dir).unwrap();
        seal_dead_session(&core, &account, "@a:dead");
        assert!(core.pool_of_account(&account).is_ok(), "有 session 就有池");
        core.begin_logging_out(&account);
        let refused = core
            .pool_of_account(&account)
            .err()
            .expect("logging out: the pool is blocked");
        assert_eq!(refused.kind, CoreErrorKind::AccountBusy, "{refused:?}");
        core.end_logging_out(&account);
        assert!(core.pool_of_account(&account).is_ok(), "解封就過");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 🚨 HTTP 登出失敗 ＝ no-op（維護者 2026-09-21）：session 還在、池沒關、封鎖解掉，連線照常。
    /// 位址沒人在聽，所以 `/logout` 一定失敗——這正是要驗的那條路。
    #[tokio::test]
    async fn a_failed_http_logout_changes_nothing_and_unblocks_the_pool() {
        let dir = scratch("logout-fails");
        let core = unlocked(&dir);
        let key = core.vault().unwrap().account_dir_key();
        let account = crate::accounts::AccountDir::locate(&dir, &key, DEAD, "@a:dead").unwrap();
        std::fs::create_dir_all(&account.dir).unwrap();
        seal_dead_session(&core, &account, "@a:dead");
        let pool_before = core.pool_of_account(&account).unwrap();
        let outcome = core.log_out("@a:dead", None, true, false).await;
        assert!(outcome.is_err(), "沒人在聽，登出一定失敗：{outcome:?}");
        assert!(account.is_logged_in(), "session 還在");
        assert!(!core.is_logging_out(&account), "封鎖解掉了");
        let pool_after = core.pool_of_account(&account).unwrap();
        assert!(
            std::sync::Arc::ptr_eq(&pool_before, &pool_after),
            "池沒被拿掉，還是同一個"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 🚨 **成功登出之後要解封**（PR #54 審查 cirno／salvia／rumia 🔴）：第一版只在 HTTP 失敗那半解封，成功登出後
    /// 旗標留著，重登入落在同一個目錄（`AccountDir::locate` 是決定性的）就被 `AccountBusy` 磚死到 daemon 重開。
    /// 這裡起一個回 200 的迷你 HTTP 當 homeserver 的 `/logout`，真的跑 `Core::log_out` 的成功路徑。
    #[tokio::test]
    async fn a_successful_logout_unblocks_the_account_so_it_can_log_in_again() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let server = format!("http://{}", listener.local_addr().unwrap());
        // 只回應一次：`POST /logout` → 200 `{}`。
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = vec![0u8; 4096];
            let _ = socket.read(&mut request).await;
            let _ = socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
                )
                .await;
        });
        let dir = scratch("logout-succeeds");
        let core = unlocked(&dir);
        let key = core.vault().unwrap().account_dir_key();
        let account = crate::accounts::AccountDir::locate(&dir, &key, &server, "@a:local").unwrap();
        std::fs::create_dir_all(&account.dir).unwrap();
        core.vault()
            .unwrap()
            .seal_session(
                &account.session_path(),
                &wbf_sdk::login::Session {
                    server: server.clone(),
                    user_id: "@a:local".to_string(),
                    device_id: "DEV".to_string(),
                    access_token: "syt_about_to_be_revoked".to_string(),
                    store_dir: None,
                    backend: None,
                },
            )
            .unwrap();
        assert!(core.pool_of_account(&account).is_ok());

        core.log_out("@a:local", None, true, false)
            .await
            .expect("the mini homeserver said 200");
        assert!(!account.is_logged_in(), "session 刪了");
        assert!(!core.is_logging_out(&account), "🚨 成功登出之後要解封");

        // 重登入落在同一個目錄：封一個新 session，池要能開。
        core.vault()
            .unwrap()
            .seal_session(
                &account.session_path(),
                &wbf_sdk::login::Session {
                    server: server.clone(),
                    user_id: "@a:local".to_string(),
                    device_id: "DEV2".to_string(),
                    access_token: "syt_new".to_string(),
                    store_dir: None,
                    backend: None,
                },
            )
            .unwrap();
        assert!(
            core.pool_of_account(&account).is_ok(),
            "重登入之後池要開得起來，不是 AccountBusy"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 把一個「指向沒人在聽的位址」的 session 封進這個帳號。
    fn seal_dead_session(core: &crate::Core, account: &crate::accounts::AccountDir, user: &str) {
        core.vault()
            .unwrap()
            .seal_session(
                &account.session_path(),
                &wbf_sdk::login::Session {
                    server: DEAD.to_string(),
                    user_id: user.to_string(),
                    device_id: "DEV".to_string(),
                    access_token: "syt_nobody_is_listening".to_string(),
                    store_dir: None,
                    backend: None,
                },
            )
            .unwrap();
    }

    /// ⚠️ 上面那條驗的是**失敗**那半（那是真的跑我們的 code）。成功那半沒有假的 wbf server
    /// 可以驅動，所以這裡直接釘 `get_backend_kind` 依賴的機制：
    /// `OnceCell::get_or_try_init` **出錯不寫進去、成功才寫**。
    /// 📎 講明白比假裝蓋到好。
    #[tokio::test]
    async fn a_failed_probe_leaves_no_conclusion_so_the_next_account_can_still_win() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let cell: tokio::sync::OnceCell<BackendKind> = tokio::sync::OnceCell::new();
        let probes = AtomicUsize::new(0);

        // A 帳號：token 被拒。
        let refused = cell
            .get_or_try_init(|| async {
                probes.fetch_add(1, Ordering::SeqCst);
                Err::<BackendKind, CoreError>(CoreError::new(CoreErrorKind::Usage, "token refused"))
            })
            .await;
        assert!(refused.is_err());
        assert!(cell.get().is_none(), "🚫 失敗不准留下結論");

        // B 帳號：同一台 server，token 是好的 —— 它必須探得到、而且結論是 wbf。
        let answered = cell
            .get_or_try_init(|| async {
                probes.fetch_add(1, Ordering::SeqCst);
                Ok::<BackendKind, CoreError>(BackendKind::WbfSdk)
            })
            .await;
        assert_eq!(*answered.unwrap(), BackendKind::WbfSdk);
        assert_eq!(probes.load(Ordering::SeqCst), 2, "失敗之後要再探一次");

        // 成功之後才記住：第三個人不再探。
        let reused = cell
            .get_or_try_init(|| async {
                probes.fetch_add(1, Ordering::SeqCst);
                Ok::<BackendKind, CoreError>(BackendKind::MatrixSdk)
            })
            .await;
        assert_eq!(*reused.unwrap(), BackendKind::WbfSdk, "用記住的那個");
        assert_eq!(probes.load(Ordering::SeqCst), 2, "🚫 成功之後不再探");
    }

    /// 🚨 **同時進來的人共用一次探測**（PR #33 審查 rumia🟡2）——
    /// 🚫 不是各開一條 WS、各問一次 `Hello`。
    #[tokio::test]
    async fn three_callers_at_once_share_a_single_probe() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let cell: tokio::sync::OnceCell<BackendKind> = tokio::sync::OnceCell::new();
        let probes = AtomicUsize::new(0);
        let probe_once = || async {
            probes.fetch_add(1, Ordering::SeqCst);
            // 讓另外兩個排隊者真的有機會進來。
            tokio::task::yield_now().await;
            Ok::<BackendKind, CoreError>(BackendKind::WbfSdk)
        };

        let (first, second, third) = tokio::join!(
            cell.get_or_try_init(probe_once),
            cell.get_or_try_init(probe_once),
            cell.get_or_try_init(probe_once)
        );
        for answer in [first, second, third] {
            assert_eq!(*answer.unwrap(), BackendKind::WbfSdk);
        }
        assert_eq!(probes.load(Ordering::SeqCst), 1, "只准探一次");
    }

    /// 🚧 還沒有 ws 定義的方法**先走 matrix-sdk**，不管走哪條、不管對方是誰。
    #[test]
    fn a_method_without_a_ws_definition_yet_stays_on_matrix_sdk() {
        for transport in [Transport::WebSocket, Transport::Http] {
            for speaks_wbf in [true, false] {
                assert_eq!(
                    get_backend_for(transport, speaks_wbf, MethodHome::StillOnMatrixSdk).unwrap(),
                    BackendKind::MatrixSdk
                );
            }
        }
    }

    /// 只有 wbf 有的功能，在到不了 wbf 的路上就是**關的** —— ⚠️ 而兩種理由要分開講。
    #[test]
    fn a_wbf_only_feature_is_off_and_says_which_reason() {
        let over_http = get_backend_for(Transport::Http, true, MethodHome::WbfSdkOnly).unwrap_err();
        assert_eq!(over_http.kind, CoreErrorKind::Usage);
        assert!(
            over_http.message.contains("over http it is off"),
            "{}",
            over_http.message
        );

        let plain_server =
            get_backend_for(Transport::WebSocket, false, MethodHome::WbfSdkOnly).unwrap_err();
        assert!(
            plain_server
                .message
                .contains("does not speak the wbf protocol"),
            "{}",
            plain_server.message
        );
    }
}

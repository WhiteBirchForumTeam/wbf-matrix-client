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
    /// 這台 homeserver 講不講 wbf 協議。**探測，🚫 不是設定**（architecture-v2 §6.1）。
    ///
    /// ⭐ **不確定一律回 [`BackendKind::MatrixSdk`]**：連不上、`Hello` 不回、token 被拒、
    /// 回來的東西看不懂 —— 全部當成一般 homeserver。壞在「用了比較慢但一定能動的那條」，
    /// 🚫 不壞在「以為對方懂我們的協議」。所以這個函數**不回 `Err`**：探測失敗不是錯誤，
    /// 是一個答案。
    ///
    /// 🚨 **只有「server 自己回答過的」才記住**（PR #33 審查 rumia🔴1）：
    ///
    /// | 探測結果 | 這次回 | 記住嗎 |
    /// |---|---|---|
    /// | `Hello` 回了、版本認得 | `WbfSdk` | ✅ 那是 server 的事實 |
    /// | `Hello` 回了、版本不認得 | `MatrixSdk` | ✅ 同上 |
    /// | 連不上／token 被拒／逾時 | `MatrixSdk` | 🚫 **不記** |
    ///
    /// ⚠️ 為什麼失敗不能記：這個快取的 key 是 **server dir**（一台 server 一格，因為
    /// 「講不講 wbf」是 server 的性質），但探測是**拿某一個帳號的 token 去問的**。
    /// 🚨 所以 A 帳號的 token 過期，如果把失敗記下來，同一台 server 上 token 好的 B 帳號
    /// 會被**永久降級**成 `MatrixSdk`，wbf 的功能整個消失 —— ⭐ 那是把「帳號的狀態」
    /// 寫進了「server 的事實」。📎 成功可以共用（server 對誰都講同一套協議），失敗不行。
    ///
    /// 📎 代價：對一般 homeserver，每次用到 wbf-only 的功能都會再試一次 WS handshake。
    /// ⭐ 可以接受 —— 那些呼叫本來就會失敗（那個功能在那台 server 上是關的），
    /// 而多記一個錯的結論會讓**能動的**帳號也不能動。
    ///
    /// ⚠️ 一個 server dir 一個 [`tokio::sync::OnceCell`]，所以**同時進來的人共用同一次探測**，
    /// 🚫 不是各探一次（審查 rumia🟡2）。只在記憶體裡，🚫 不寫進磁碟。
    ///
    /// Args:
    ///     account: 哪個帳號（它決定 server dir，也決定拿誰的 token 去問）
    /// Return:
    ///     BackendKind  WbfSdk ＝ `Hello` 通了；MatrixSdk ＝ 其他所有情況
    pub async fn get_backend_kind(&self, account: &crate::accounts::AccountDir) -> BackendKind {
        let cell = self
            .backends
            .lock()
            .expect("the backend registry is never poisoned")
            .entry(account.server_dir())
            .or_default()
            .clone();
        // ⭐ `get_or_try_init` 的兩個性質正好是要的：**失敗不寫進去**（所以下次會重探），
        // 而且同時進來的人**等同一次**探測（🚫 不是各開一條 WS）。
        // ⚠️ 註冊表的鎖在上面那一段就放掉了 —— 🚫 std 的 Mutex 不能跨 await 持有。
        let answered = cell
            .get_or_try_init(|| async {
                let kind = match self.probe_wbf(account).await? {
                    true => BackendKind::WbfSdk,
                    false => BackendKind::MatrixSdk,
                };
                self.events.progress(format!(
                    "{} speaks {}",
                    account.server_host,
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

    /// 註冊表**現在記住了什麼**。⚠️ 只給測試用：唯一能分辨「記住了」與「只是回了一次」
    /// 的方式就是問它 —— 而那個分辨正是 rumia🔴1 要的（PR #33）。
    ///
    /// Args:
    ///     account: 哪個帳號（它決定 server dir）
    /// Return:
    ///     Some(BackendKind)  探過而且 server 回答過
    ///     None               沒探過，或探了但**失敗**（🚫 失敗不留下結論）
    #[cfg(test)]
    pub(crate) fn get_remembered_backend(
        &self,
        account: &crate::accounts::AccountDir,
    ) -> Option<BackendKind> {
        self.backends
            .lock()
            .expect("the backend registry is never poisoned")
            .get(&account.server_dir())
            .and_then(|cell| cell.get().copied())
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

    /// 🚨 **探測一律走 WS**：wbf 協議就是 WS，而「HTTP 連得上」證明不了對方講 wbf
    /// （任何 HTTP server 都會回東西）。
    ///
    /// Return:
    ///     Ok(true)   `Hello` 回了、協議版本認得
    ///     Ok(false)  接得上但講的不是我們認得的協議版本
    ///     Err(...)   連不上、token 被拒、逾時 —— 呼叫端一律當成「不是 wbf」
    async fn probe_wbf(&self, account: &crate::accounts::AccountDir) -> Result<bool, CoreError> {
        let mut client = self.connect_wbf_client(account).await?;
        let hello = client.hello(PROBE_CLIENT_NAME).await?;
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
    /// Return:
    ///     Ok(WbfClient)  開好了（WS）
    ///     Err(Usage)     這條路到不了 wbf，而這個功能只有它有 —— 它是關的
    ///     Err(Network)   到得了，但 WS 開不起來（維護者 2026-09-13：開失敗要報錯）
    pub(crate) async fn client_of(
        &self,
        account: &crate::accounts::AccountDir,
        transport: Transport,
        home: MethodHome,
    ) -> Result<wbf_sdk::client::WbfClient<wbf_sdk::channel::Channel>, CoreError> {
        let speaks_wbf = self.get_backend_kind(account).await == BackendKind::WbfSdk;
        match get_backend_for(transport, speaks_wbf, home)? {
            BackendKind::WbfSdk => self.connect_wbf_client(account).await,
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

    /// 🚨 **探測失敗不留下結論** —— 真的跑 `get_backend_kind`（PR #33 審查 rumia🔴1）。
    ///
    /// session 指向一個**沒有人在聽**的位址，所以探測一定失敗。它要：
    /// 回 `MatrixSdk`（fail safe），但🚫 **不准記住** —— 不然同一台 server 上
    /// token 好的另一個帳號會被那個失敗**永久降級**，wbf 的功能整個消失。
    /// ⭐ 「記住了沒有」是唯一分辨得出來的地方，所以斷言的是它。
    #[tokio::test]
    async fn a_failed_probe_answers_matrix_sdk_without_remembering_it() {
        let dir = scratch("probe-fails");
        let core = unlocked(&dir);
        let vault = core.vault().unwrap();
        let account =
            crate::accounts::AccountDir::locate(&dir, &vault.account_dir_key(), DEAD, "@a:dead")
                .unwrap();
        std::fs::create_dir_all(&account.dir).unwrap();
        vault
            .seal_session(
                &account.session_path(),
                &wbf_sdk::login::Session {
                    server: DEAD.to_string(),
                    user_id: "@a:dead".to_string(),
                    device_id: "DEV".to_string(),
                    access_token: "syt_nobody_is_listening".to_string(),
                    store_dir: None,
                },
            )
            .unwrap();

        assert_eq!(
            core.get_backend_kind(&account).await,
            BackendKind::MatrixSdk,
            "探不到就當一般 homeserver"
        );
        assert_eq!(
            core.get_remembered_backend(&account),
            None,
            "🚫 失敗不准留下結論——不然同 server 的其他帳號會被連坐"
        );
        // 再問一次還是一樣的答案，而且還是沒記住（所以下一次仍然會重探）。
        assert_eq!(core.get_backend_kind(&account).await, BackendKind::MatrixSdk);
        assert_eq!(core.get_remembered_backend(&account), None);

        let _ = std::fs::remove_dir_all(&dir);
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
                Err::<BackendKind, CoreError>(CoreError::new(
                    CoreErrorKind::Usage,
                    "token refused",
                ))
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

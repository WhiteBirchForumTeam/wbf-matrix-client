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
            Ok(true) => BackendKind::WbfSdk,
            Ok(false) | Err(_) => BackendKind::MatrixSdk,
        };
        self.events.progress(format!(
            "{} speaks {}",
            account.server_host,
            match found {
                BackendKind::WbfSdk => "the wbf protocol",
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

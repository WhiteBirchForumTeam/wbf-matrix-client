//! core 的錯誤型別。
//!
//! **為什麼不直接用 `SdkError`**（PR #24 審查 salvia＋rumia）：`SdkError` 只 derive 了
//! `Debug`，序列化不了。而 core 的公開面之後要被 RPC 包住，§4.6 定的回應形狀是
//! `code` ＋ `msg`——所以錯誤必須是**結構化、可序列化**的東西，不能是一句人話。
//!
//! # 🚫 這裡刻意**沒有**號碼
//!
//! §4.6：「`code` 是穩定的整數，一個意思一個號碼、**定了就不改**」，而那張表的權威位置是
//! `rpc-spec.md`（還沒寫）。在規格還沒寫的時候先配號碼，等於現在就兌現一個「不能改」的
//! 承諾——所以這裡只定**種類**（[`CoreErrorKind`]），號碼等 `rpc-spec.md` 一起定
//!（維護者 2026-09-11 定）。
//!
//! ⚠️ 加新 variant 時：種類是給**程式**判斷的，`message` 是給**人**看的。
//! 🚫 前端不要拿 `message` 做邏輯——那句話會改，`kind` 不會。

use std::fmt;

use serde::Serialize;
use wbf_sdk::SdkError;

/// 錯誤的種類。⚠️ 判斷用這個，🚫 不要用 `message`。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CoreErrorKind {
    /// vault 還沒解鎖。RPC 那端對應 §4.5 的「只接受 `hello` 與 `vault.unlock`」。
    Locked,
    /// 這個資料目錄還沒有 `local.key`——沒登入過。
    NoKeyFile,
    /// `local.key` 是 passphrase 模式，但沒給 passphrase。
    /// ⚠️ 跟 [`CoreErrorKind::WrongPassphrase`] **分開**：前端要問使用者 vs 要說「打錯了」。
    NeedPassphrase,
    /// `local.key` 是 plain 模式，卻給了 passphrase。🚫 不靜默忽略。
    UnexpectedPassphrase,
    /// passphrase 不對。
    WrongPassphrase,
    /// 找不到這個帳號。
    NoSuchAccount,
    /// 這串指到不只一個帳號（同名 localpart 在多個 server、或只差大小寫的有好幾個）。
    /// 🚫 core 不挑一個——fail closed。
    AmbiguousAccount,
    /// 這個帳號沒登入（`session.sealed` 不在）。
    NotLoggedIn,
    /// 這台機器沒保管這個帳號的 recovery key（`key-backup restore` 要它）。
    NoRecoveryKeyHere,
    /// 登出／摧毀的閘門擋下來了：刪掉之後歷史救不回來（local-cache-db.md §10.7）。
    ///
    /// ⚠️ core 的訊息只說**條件**，🚫 不提命令名字——前端看到這個 kind 再補上自己那句
    /// （rpc-cli 說 `wbf-cli key-backup recovery`，Desktop 可能是一個按鈕）。
    HistoryWouldBeLost,
    /// 使用者用錯了，還沒細分到上面任何一種。
    ///
    /// ⚠️ 這是**過渡**的桶子：底下的 `SdkError::Usage` 還很多樣。每次有前端需要分辨的
    /// 情況，就從這裡拆一個新的 variant 出去，🚫 不要讓前端去 match `message`。
    Usage,
    Io,
    Network,
    /// server 拒絕或不講協議。
    Server,
    /// 完整性檢查不過（CRC、AEAD 標籤）。
    Integrity,
    Timeout,
}

/// core 吐出去的錯誤：**種類**給程式判斷，**訊息**給人看。
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CoreError {
    pub kind: CoreErrorKind,
    pub message: String,
}

impl CoreError {
    pub fn new(kind: CoreErrorKind, message: impl Into<String>) -> CoreError {
        CoreError {
            kind,
            message: message.into(),
        }
    }

    pub fn locked(data_dir: &std::path::Path) -> CoreError {
        CoreError::new(
            CoreErrorKind::Locked,
            format!(
                "the vault in {} is locked; unlock it first",
                data_dir.display()
            ),
        )
    }
}

impl fmt::Display for CoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        // 🚫 不印 kind：使用者看的是那句話，種類是給程式的。
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for CoreError {}

/// 還沒細分的一律從 `SdkError` 抬上來，種類照它原本的分類走。
///
/// ⚠️ `SdkError::Usage` 會落到 [`CoreErrorKind::Usage`] 這個過渡桶子——想分辨得更細的
/// 路徑要在 core 裡**明確**造一個有種類的 `CoreError`，🚫 不要靠 `From` 猜。
impl From<SdkError> for CoreError {
    fn from(error: SdkError) -> CoreError {
        let kind = match &error {
            SdkError::Usage(_) => CoreErrorKind::Usage,
            SdkError::Io(_) => CoreErrorKind::Io,
            SdkError::Network(_) => CoreErrorKind::Network,
            SdkError::Server { .. } | SdkError::Protocol(_) => CoreErrorKind::Server,
            SdkError::Integrity(_) => CoreErrorKind::Integrity,
            SdkError::Timeout(_) => CoreErrorKind::Timeout,
        };
        CoreError::new(kind, format!("{error}"))
    }
}

/// 檔案系統的錯誤一律落到 [`CoreErrorKind::Io`]。
impl From<std::io::Error> for CoreError {
    fn from(error: std::io::Error) -> CoreError {
        CoreError::new(CoreErrorKind::Io, format!("{error}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_kind_survives_serialisation_and_the_message_is_what_people_read() {
        let error = CoreError::new(CoreErrorKind::NeedPassphrase, "this key file needs one");
        let json = serde_json::to_value(&error).unwrap();
        // kind 是給程式判斷的，序列化成一個穩定的字串。
        assert_eq!(json["kind"], "need_passphrase");
        // Display 只給人話，🚫 不夾 kind。
        assert_eq!(format!("{error}"), "this key file needs one");
    }

    #[test]
    fn an_sdk_error_keeps_its_category_on_the_way_up() {
        let network: CoreError = SdkError::Network("connection refused".into()).into();
        assert_eq!(network.kind, CoreErrorKind::Network);
        // ⚠️ Usage 是過渡桶子：每次前端需要分辨，就從它拆一個新 variant 出去。
        let usage: CoreError = SdkError::Usage("no current account".into()).into();
        assert_eq!(usage.kind, CoreErrorKind::Usage);
    }
}

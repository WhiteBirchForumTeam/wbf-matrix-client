//! daemon 的 token：哪來的、放哪（/docs/design/rpc-specs/local-interface.md §3；維護者 2026-11-20 定）。
//!
//! token 是前端與 daemon 之間唯一的憑證（§4 從它導兩把金鑰）。它保護的是**通訊**：
//! 擋的是這台機器上別的帳號，同一個 OS 帳號的程序本來就讀得到資料目錄。
//!
//! ```text
//! daemon 起來：WBF_DAEMON_TOKEN 有指定 → 用它（take_token_from_env，讀完從自己的環境清掉）
//!              沒指定              → 自己隨機產生一把（new_token）
//!              不管哪一種都寫進 <data dir>/daemon.token（write_token_file：只有本人讀得到、一行 base64），在 daemon.json 之前
//! 前端連線：  讀 daemon.json（port）＋ daemon.token（read_token_file）
//! daemon 結束：正常結束時刪掉 daemon.token；被強制殺掉留著無所謂，下一支起來會蓋掉
//! ```
//!
//! - 禁止用命令列參數傳：那會進 `ps` 的輸出。

use std::path::{Path, PathBuf};

use base64::Engine as _;
use zeroize::Zeroizing;

use crate::pack::TOKEN_LEN;

/// 前端要指定 token 時用的環境變數名。值是 [`TOKEN_LEN`] byte 的標準 base64（含 padding）。
pub const TOKEN_ENV: &str = "WBF_DAEMON_TOKEN";

/// 資料目錄裡放 token 的檔名。內容同 [`TOKEN_ENV`] 的值，後面一個換行。
pub const TOKEN_FILE_NAME: &str = "daemon.token";

/// 讀 token 出問題。訊息就是要印給人看的那句。
#[derive(Debug, PartialEq, Eq)]
pub struct TokenProblem(pub String);

impl std::fmt::Display for TokenProblem {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// 從 [`TOKEN_ENV`] 讀指定的 token，**讀完就從這個程序的環境裡清掉**（之後起的子程序不會繼承到）。
///
/// ⚠️ 要在起 runtime、開任何執行緒**之前**呼叫：清環境變數在多執行緒時不安全。
///
/// Return:
///     Ok(Some(token))        有指定，恰好 [`TOKEN_LEN`] byte
///     Ok(None)               沒指定（daemon 自己產生一把）
///     Err(TokenProblem)      指定了卻不是 UTF-8、base64 解不開、長度不對（fail closed：不啟動，**不**改用自己產生的）
pub fn take_token_from_env() -> Result<Option<Zeroizing<Vec<u8>>>, TokenProblem> {
    let raw = std::env::var_os(TOKEN_ENV);
    std::env::remove_var(TOKEN_ENV);
    let Some(raw) = raw else {
        return Ok(None);
    };
    let raw =
        Zeroizing::new(raw.into_string().map_err(|_| {
            TokenProblem(format!("{TOKEN_ENV} is not valid UTF-8; it must be base64"))
        })?);
    decode_token(&raw, TOKEN_ENV).map(Some)
}

/// Args:
///     encoded: base64 的 token, example: "CQkJ…"（[`TOKEN_LEN`] byte 的 base64）
///     source: 錯誤訊息裡說它是從哪來的, example: "WBF_DAEMON_TOKEN"
/// Return:
///     Ok(token)              恰好 [`TOKEN_LEN`] byte
///     Err(TokenProblem)      base64 解不開、長度不對
pub fn decode_token(encoded: &str, source: &str) -> Result<Zeroizing<Vec<u8>>, TokenProblem> {
    let token = Zeroizing::new(
        base64::engine::general_purpose::STANDARD
            .decode(encoded.trim())
            .map_err(|_| TokenProblem(format!("the token in {source} is not valid base64")))?,
    );
    if token.len() != TOKEN_LEN {
        return Err(TokenProblem(format!(
            "the token in {source} is {} bytes; it must be exactly {TOKEN_LEN} \
             (/docs/design/rpc-specs/local-interface.md §3)",
            token.len()
        )));
    }
    Ok(token)
}

/// 產生一把新的 token。
///
/// Return:
///     Ok(token)              [`TOKEN_LEN`] byte 的 OS 亂數
///     Err(TokenProblem)      OS 給不出亂數（禁止退回任何「看起來像亂數」的東西）
pub fn new_token() -> Result<Zeroizing<[u8; TOKEN_LEN]>, TokenProblem> {
    let mut token = Zeroizing::new([0u8; TOKEN_LEN]);
    getrandom::getrandom(token.as_mut())
        .map_err(|error| TokenProblem(format!("no OS randomness for the token: {error}")))?;
    Ok(token)
}

/// 把 token 編成 [`TOKEN_ENV`] 的值（也是 [`TOKEN_FILE_NAME`] 的內容）。
pub fn to_env_value(token: &[u8]) -> Zeroizing<String> {
    Zeroizing::new(base64::engine::general_purpose::STANDARD.encode(token))
}

/// Args:
///     data_dir: example: "<data dir>"
/// Return:
///     PathBuf  example: "<data dir>/daemon.token"
pub fn token_file_path(data_dir: &Path) -> PathBuf {
    data_dir.join(TOKEN_FILE_NAME)
}

/// daemon 那一頭：把這次用的 token 寫進 `<data dir>/daemon.token`（temp＋rename、只有本人讀得到，同 `local.key`）。
///
/// Return:
///     Ok(())
///     Err(TokenProblem)   寫不了（daemon 就不宣告 ready：前端連不上）
pub fn write_token_file(data_dir: &Path, token: &[u8]) -> Result<(), TokenProblem> {
    let mut text = to_env_value(token);
    text.push('\n');
    let path = token_file_path(data_dir);
    wbf_sdk::vault::write_private(&path, text.as_bytes())
        .map_err(|error| TokenProblem(format!("cannot write {}: {error}", path.display())))
}

/// 前端那一頭：讀 `<data dir>/daemon.token`。
///
/// Return:
///     Ok(token)              恰好 [`TOKEN_LEN`] byte
///     Err(TokenProblem)      檔不在（daemon 沒在跑）、讀不了、內容不對
pub fn read_token_file(data_dir: &Path) -> Result<Zeroizing<Vec<u8>>, TokenProblem> {
    let path = token_file_path(data_dir);
    let text = Zeroizing::new(std::fs::read_to_string(&path).map_err(|error| {
        TokenProblem(format!(
            "cannot read {} ({error}); is a daemon serving this data directory?",
            path.display()
        ))
    })?);
    decode_token(&text, &path.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 有指定就用、讀完就清掉：同一個程序裡再看，環境變數已經不在（之後起的子程序不會繼承到）；沒指定是 `None`，不是錯。
    /// 📎 這條改環境變數，跟別的測試共用同一個程序：這個 crate 只有這條碰 [`TOKEN_ENV`]。
    #[test]
    fn a_given_token_is_taken_out_of_the_environment_and_none_given_is_not_an_error() {
        let token = [9u8; TOKEN_LEN];
        std::env::set_var(TOKEN_ENV, to_env_value(&token).as_str());
        let read = take_token_from_env().unwrap().unwrap();
        assert_eq!(read.as_slice(), token.as_slice());
        assert!(
            std::env::var_os(TOKEN_ENV).is_none(),
            "cleared from the environment"
        );
        assert_eq!(take_token_from_env(), Ok(None));
        // 指定了卻壞掉：拒絕，**不**默默改用自己產生的。
        std::env::set_var(TOKEN_ENV, "not base64 !!!");
        assert!(take_token_from_env().is_err());
        assert!(std::env::var_os(TOKEN_ENV).is_none());
    }

    #[test]
    fn a_token_that_is_not_base64_or_the_wrong_length_is_refused() {
        assert!(decode_token("not base64 !!!", TOKEN_ENV).is_err());
        let short = base64::engine::general_purpose::STANDARD.encode([1u8; TOKEN_LEN - 1]);
        let complaint = decode_token(&short, TOKEN_ENV).unwrap_err();
        assert!(complaint.0.contains("255 bytes"), "{complaint}");
        let long = base64::engine::general_purpose::STANDARD.encode([1u8; TOKEN_LEN + 1]);
        assert!(decode_token(&long, TOKEN_ENV).is_err());
    }

    #[test]
    fn a_new_token_round_trips_through_the_token_file() {
        let dir = tempfile::tempdir().unwrap();
        let token = new_token().unwrap();
        assert_ne!(
            token.as_slice(),
            [0u8; TOKEN_LEN].as_slice(),
            "not left zeroed"
        );
        write_token_file(dir.path(), token.as_slice()).unwrap();
        let text = std::fs::read_to_string(token_file_path(dir.path())).unwrap();
        assert!(text.ends_with('\n'), "one line");
        let read = read_token_file(dir.path()).unwrap();
        assert_eq!(read.as_slice(), token.as_slice());
    }

    #[test]
    fn a_missing_token_file_says_no_daemon_is_serving() {
        let dir = tempfile::tempdir().unwrap();
        let complaint = read_token_file(dir.path()).unwrap_err();
        assert!(complaint.0.contains("is a daemon serving"), "{complaint}");
    }
}

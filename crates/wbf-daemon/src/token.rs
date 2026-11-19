//! token 怎麼交給 daemon（/docs/design/rpc-specs/local-interface.md §3；維護者 2026-11-17 改成環境變數）。
//!
//! token 是**叫起 daemon 的那個前端**產生的、前端與 daemon 之間唯一的憑證（§4 從它導兩把金鑰）。
//!
//! ```text
//! 1. 前端產生 256 byte 隨機 token（new_token），base64 之後放進子程序的環境變數 WBF_DAEMON_TOKEN，spawn `daemon -s`
//! 2. daemon 在起 runtime、開任何執行緒之前讀它、從自己的環境裡清掉（take_token_from_env）、導金鑰、開 port、宣告 ready
//! 3. token 只活在兩個程序的記憶體裡，從頭到尾不落地
//! ```
//!
//! - 禁止用命令列參數傳：那會進 `ps` 的輸出。
//! - 安全邊界跟以前的 0600 檔一樣：同一個 OS 帳號的程序讀得到別的程序的環境變數；差別是少了「在磁碟上那幾秒」。
//! - 禁止重用：重連就重產生一把（一把 token 活得越久，被抄走的機會越多）。

use base64::Engine as _;
use zeroize::Zeroizing;

use crate::pack::TOKEN_LEN;

/// 前端交 token 給 daemon 用的環境變數名。值是 [`TOKEN_LEN`] byte 的標準 base64（含 padding）。
pub const TOKEN_ENV: &str = "WBF_DAEMON_TOKEN";

/// 讀 token 出問題。訊息就是要印給人看的那句。
#[derive(Debug, PartialEq, Eq)]
pub struct TokenProblem(pub String);

impl std::fmt::Display for TokenProblem {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// 從 [`TOKEN_ENV`] 讀 token，**讀完就從這個程序的環境裡清掉**（之後起的子程序不會繼承到）。
///
/// ⚠️ 要在起 runtime、開任何執行緒**之前**呼叫：清環境變數在多執行緒時不安全。
///
/// Return:
///     Ok(token)              恰好 [`TOKEN_LEN`] byte
///     Err(TokenProblem)      沒帶、不是 UTF-8、base64 解不開、長度不對（fail closed：一律不啟動）
pub fn take_token_from_env() -> Result<Zeroizing<Vec<u8>>, TokenProblem> {
    let raw = std::env::var_os(TOKEN_ENV);
    std::env::remove_var(TOKEN_ENV);
    let Some(raw) = raw else {
        return Err(TokenProblem(format!(
            "no token: the frontend that starts the daemon passes it in {TOKEN_ENV} \
             (/docs/design/rpc-specs/local-interface.md §3)"
        )));
    };
    let raw =
        Zeroizing::new(raw.into_string().map_err(|_| {
            TokenProblem(format!("{TOKEN_ENV} is not valid UTF-8; it must be base64"))
        })?);
    decode_token(&raw)
}

/// Args:
///     encoded: base64 的 token, example: "CQkJ…"（[`TOKEN_LEN`] byte 的 base64）
/// Return:
///     Ok(token)              恰好 [`TOKEN_LEN`] byte
///     Err(TokenProblem)      base64 解不開、長度不對
pub fn decode_token(encoded: &str) -> Result<Zeroizing<Vec<u8>>, TokenProblem> {
    let token = Zeroizing::new(
        base64::engine::general_purpose::STANDARD
            .decode(encoded.trim())
            .map_err(|_| TokenProblem(format!("{TOKEN_ENV} is not valid base64")))?,
    );
    if token.len() != TOKEN_LEN {
        return Err(TokenProblem(format!(
            "the token in {TOKEN_ENV} is {} bytes; it must be exactly {TOKEN_LEN} \
             (/docs/design/rpc-specs/local-interface.md §3)",
            token.len()
        )));
    }
    Ok(token)
}

/// 前端那一頭：產生一把新的 token。
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

/// 前端那一頭：把 token 編成 [`TOKEN_ENV`] 的值。
pub fn to_env_value(token: &[u8; TOKEN_LEN]) -> Zeroizing<String> {
    Zeroizing::new(base64::engine::general_purpose::STANDARD.encode(token))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 讀完就清掉：同一個程序裡再看，環境變數已經不在（之後起的子程序不會繼承到）。
    /// 📎 這條改環境變數，跟別的測試共用同一個程序：這個 crate 只有這條碰 [`TOKEN_ENV`]。
    #[test]
    fn the_token_is_taken_out_of_the_environment_once_read() {
        let token = [9u8; TOKEN_LEN];
        std::env::set_var(TOKEN_ENV, to_env_value(&token).as_str());
        let read = take_token_from_env().unwrap();
        assert_eq!(read.as_slice(), token.as_slice());
        assert!(
            std::env::var_os(TOKEN_ENV).is_none(),
            "cleared from the environment"
        );
        // 第二次讀：已經沒有了（fail closed，不是讀到舊的）。
        assert!(take_token_from_env().is_err());
    }

    #[test]
    fn a_token_that_is_not_base64_or_the_wrong_length_is_refused() {
        assert!(decode_token("not base64 !!!").is_err());
        let short = base64::engine::general_purpose::STANDARD.encode([1u8; TOKEN_LEN - 1]);
        let complaint = decode_token(&short).unwrap_err();
        assert!(complaint.0.contains("255 bytes"), "{complaint}");
        let long = base64::engine::general_purpose::STANDARD.encode([1u8; TOKEN_LEN + 1]);
        assert!(decode_token(&long).is_err());
    }

    #[test]
    fn a_new_token_round_trips_through_the_environment_value() {
        let token = new_token().unwrap();
        let decoded = decode_token(&to_env_value(&token)).unwrap();
        assert_eq!(decoded.as_slice(), token.as_slice());
        assert_ne!(
            token.as_slice(),
            [0u8; TOKEN_LEN].as_slice(),
            "not left zeroed"
        );
    }
}

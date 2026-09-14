//! wbf 協議的錯誤碼：server 那張表（wbfuwunel `wbf-wire-format.md` §3.4）在 client 這邊的投影。
//!
//! 🚨 **程式只比對 `code_id`（整數），🚫 不比對 `code`（名字）**。名字是給人看 log 用的。
//! 📎 在這之前 client 比的是名字（`code == "Corrupt"`），而 `SdkError::Server` 同時裝著 Matrix 的
//! `errcode`（`M_FORBIDDEN`）與我們自己合成的（HTTP 401 的 `Unauthorized`）—— 三種來源擠在同一個字串上，
//! 比名字等於賭它們永遠不撞名（issue #29 第 2 項）。
//!
//! 🚨 **不認得就不認得**（server 表的規則，維護者 2026-09-10 定）：[`WbfErrorCode::from_id`] 回 `None`
//! 的碼，呼叫端一律當「失敗了，而且**不知道**能不能重試」—— 不重試、往上報。
//! 🚫 不照序號範圍（1500–1599 之類）自己補一套行為：範圍是給人分類看的，不是「同一家照同一套處理」的授權。
//!
//! ⚠️ 加新碼的順序是 server 的表先加一列、這裡再加一個變體。🚫 **序號與名字都不重用**，所以這裡也不准改既有的號。

/// server 表上的每一個碼。值就是線上的 `code_id`。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u16)]
pub enum WbfErrorCode {
    /// pack 的版本位不是這個 server 支援的。重試沒用，要升級 client。
    UnsupportedVersion = 1001,
    /// 這個 pack 解不開（CRC、截斷、保留旗標）。重連／重送**一次**就好，再來就是編碼端的 bug。
    Corrupt = 1002,
    /// 這個 `(kind, subtype)` 沒有 handler。別重試。
    UnknownKind = 1101,
    /// 有 handler，但不走這個傳輸（`Recent`／`Subscribe`／`Session` 只走 WS）。換傳輸，不是重試。
    Unsupported = 1102,
    /// 超過大小上限。切小再送。
    TooLarge = 1103,
    /// meta／data 不是這個 subtype 要的。client 的 bug，重送同樣的東西一定再錯。
    InvalidRequest = 1201,
    /// 沒登入，或身分不再有效。重新登入。
    Unauthorized = 1301,
    /// 身分驗過了但不准。不要自動重試。
    Forbidden = 1302,
    /// 太快了。等 `retry_after_ms` 再試。
    RateLimited = 1401,
    /// 這個 device 的 WS 名額滿了（meta 有 `max_connections`）。
    TooManyConnections = 1402,
    /// 指名的東西不存在。
    NotFound = 1501,
    /// 請求合法，但跟 server 目前的狀態衝突。先讀狀態再決定。
    Conflict = 1502,
    /// 有序類的 `seq` 不對。從 meta 的 `expected_seq` 重送，🚫 不要自己重排。
    OutOfOrder = 1503,
    /// 上傳觸到大小上限，被封成不完整。
    Truncated = 1504,
    /// 這個訂閱被同一裝置後來的連線接手了（server 主動送的，不是某個請求的回應）。
    Superseded = 1505,
    /// server 自己的錯。可以退避重試。
    Internal = 1901,
}

impl WbfErrorCode {
    /// Args:
    ///     code_id: Error meta 的 `code_id`, example: 1503
    /// Return:
    ///     Some(WbfErrorCode)  認得
    ///     None                不認得 —— 包括 `0`（server 表：`0` 永遠不是合法的碼，是欄位漏了的預設值）
    pub fn from_id(code_id: u64) -> Option<WbfErrorCode> {
        use WbfErrorCode::*;
        let known = match code_id {
            1001 => UnsupportedVersion,
            1002 => Corrupt,
            1101 => UnknownKind,
            1102 => Unsupported,
            1103 => TooLarge,
            1201 => InvalidRequest,
            1301 => Unauthorized,
            1302 => Forbidden,
            1401 => RateLimited,
            1402 => TooManyConnections,
            1501 => NotFound,
            1502 => Conflict,
            1503 => OutOfOrder,
            1504 => Truncated,
            1505 => Superseded,
            1901 => Internal,
            _ => return None,
        };
        Some(known)
    }

    /// Return:
    ///     u16  線上的 `code_id`, example: 1503
    pub fn id(self) -> u16 {
        self as u16
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 表上的每一個碼都要來回得了；🚫 不准有兩個變體擠同一個號。
    #[test]
    fn every_code_on_the_table_round_trips_through_its_id() {
        use WbfErrorCode::*;
        let all = [
            UnsupportedVersion,
            Corrupt,
            UnknownKind,
            Unsupported,
            TooLarge,
            InvalidRequest,
            Unauthorized,
            Forbidden,
            RateLimited,
            TooManyConnections,
            NotFound,
            Conflict,
            OutOfOrder,
            Truncated,
            Superseded,
            Internal,
        ];
        for code in all {
            assert_eq!(
                WbfErrorCode::from_id(u64::from(code.id())),
                Some(code),
                "{code:?}"
            );
        }
        let mut ids: Vec<u16> = all.iter().map(|code| code.id()).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), all.len(), "序號不准重複");
    }

    /// 🚨 不認得就不認得：`0`（漏了欄位的預設值）、表上沒有的號、同一家範圍裡沒定義的號 —— 全部 `None`。
    /// 🚫 不照範圍猜：1599 在「狀態」那一家，但它不是任何一個碼。
    #[test]
    fn zero_and_unlisted_ids_are_unknown_even_inside_a_known_family() {
        for code_id in [0, 999, 1003, 1599, 1902, 9999, u64::MAX] {
            assert_eq!(WbfErrorCode::from_id(code_id), None, "{code_id}");
        }
    }
}

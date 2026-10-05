//! `hello` 的兩關（/docs/design/rpc-specs/rpc-spec.md §1.3）：client 名字、protocol 協商表。純函數。

/// daemon 會講的 protocol 版本。**新的在前**。
/// ⚠️ 從這裡拿掉一個版本＝breaking：舊前端一連上來就被拒絕。這是刻意的（/docs/design/rpc-specs/rpc-spec.md §1.3）。
/// 2：`room.list` 回 `[RoomListEntry]`、送文字前要 `room.get` 過那間房（維護者 2026-10-05）；1 拿掉了。
pub const SUPPORTED_PROTOCOLS: &[u32] = &[2];

/// client 正式名稱的前綴。不是這個開頭的一律拒絕。
pub const CLIENT_NAME_PREFIX: &str = "wbf-matrix";

/// 取交集裡最大的。
///
/// Args:
///     offered: 前端送的 `protocols`, example: &[2, 1]
/// Return:
///     Some(u32)  談定的版本
///     None       沒有交集
pub fn negotiate(offered: &[u32]) -> Option<u32> {
    offered
        .iter()
        .copied()
        .filter(|version| SUPPORTED_PROTOCOLS.contains(version))
        .max()
}

/// Args:
///     client: `hello.client`, example: "wbf-matrix-rpc-cli 0.1.0"
/// Return:
///     bool  1 是我們家的前端
pub fn is_client_name_acceptable(client: &str) -> bool {
    client.starts_with(CLIENT_NAME_PREFIX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn negotiation_picks_the_largest_common_version_or_nothing() {
        assert_eq!(negotiate(&[2]), Some(2));
        // 前端比 daemon 新：談成 daemon 會的那個，前端自己降級。
        assert_eq!(negotiate(&[3, 2, 1]), Some(2));
        assert_eq!(negotiate(&[3]), None);
        assert_eq!(negotiate(&[]), None);
        // 1 拿掉了（`room.list` 的形狀換了）：只會講 1 的舊前端一連上來就被拒。
        assert_eq!(negotiate(&[1]), None);
    }

    #[test]
    fn only_our_own_frontends_pass_the_name_check() {
        assert!(is_client_name_acceptable("wbf-matrix-rpc-cli 0.1.0"));
        assert!(is_client_name_acceptable("wbf-matrix-desktop 0.3.0"));
        // 簡稱是文件裡的寫法，不是線上的識別。
        assert!(!is_client_name_acceptable("rpc-cli 0.1.0"));
        assert!(!is_client_name_acceptable(""));
        assert!(!is_client_name_acceptable("WBF-MATRIX-rpc-cli"));
    }
}

//! daemon 從 `wbf.conf` 讀出來、會影響 core 呼叫的那幾個值（/docs/design/rpc-specs/wbf-cli-spec.md §10；/docs/design/rpc-specs/rpc-spec.md §2）。
//!
//! ⚠️ **這些不在 RPC 上**：`server_backup`、`local_room_keys` 是使用者對這台機器的設定，
//! 前端不該替使用者決定；daemon 是 conf 的主人（/docs/design/overview/architecture-v2.md §0.2），讀一次、填進每個 `Target`。
//! 改設定是 UI 寫 `wbf.conf` 再叫 `daemon.reload_conf`（/docs/design/messages/read-receipts.md §6）。
//! 解析在 `wbf_core::conf`（跟 rpc-cli 共用同一份，禁止各寫一套）。
//!
//! **優先序是旗標 > 環境變數 > conf > 內建預設**（/docs/design/rpc-specs/wbf-cli-spec.md §10.2）。⚠️ 這個檔**不**讀環境變數：
//! daemon 的兩個（`WBF_DATA_DIR`、`WBF_CONFIG`）掛在 `main.rs` 的 clap `env = …` 上，旗標壓環境
//! 由 clap 負責。要給這裡的鍵加環境變數就加在那邊，禁止在這裡自己讀 `std::env` ——
//! 那會繞過旗標，也會讓同一條優先序有兩個實作。

use std::path::{Path, PathBuf};

use wbf_core::conf;
use wbf_core::CoreError;
use wbf_sdk::Transport;

/// daemon 認得的鍵；其餘的在 `warnings` 裡（/docs/design/rpc-specs/wbf-cli-spec.md §10.4：警告、忽略、照跑）。
const KNOWN_CONF_KEYS: &[&str] = &[
    "SERVER_BACKUP",
    "LOCAL_ROOM_KEYS",
    "TRANSPORT",
    "READ_RECEIPTS",
];

/// `READ_RECEIPTS`：沒帶 `visible_to_others` 的 `room.read` 送哪一種回條（/docs/design/messages/read-receipts.md §5）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadReceipts {
    /// `m.read.private`：只有自己的其他裝置看得到
    Private,
    /// `m.read`：同房間的人看得到
    Public,
}

impl ReadReceipts {
    /// Return:
    ///     &str  conf 裡的寫法, example: "private"
    pub fn as_str(self) -> &'static str {
        match self {
            ReadReceipts::Private => "private",
            ReadReceipts::Public => "public",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Settings {
    /// `SERVER_BACKUP`：標準 Matrix key backup 開著嗎。認不得落到 **開**。
    pub server_backup: bool,
    /// `LOCAL_ROOM_KEYS`：本地全量快照開著嗎。認不得落到 **開**。
    pub local_room_keys: bool,
    /// `TRANSPORT`：沒在 params 帶 `transport` 時的預設。認不得落到 **ws**。會影響連線，所以 `daemon.reload_conf` **不**換它。
    pub transport: Transport,
    /// `READ_RECEIPTS`：認不得落到 **private**（被看見是收不回來的）。
    pub read_receipts: ReadReceipts,
    /// 讀 conf 時攢下的警告。這一層不印，`main.rs` 決定印不印。
    pub warnings: Vec<String>,
    /// `--config`：`daemon.reload_conf` 照它重讀同一份；None ＝ 資料目錄裡的 `wbf.conf`
    pub explicit_conf: Option<PathBuf>,
}

impl Default for Settings {
    /// 沒有 conf 檔時的值：全部是安全那一邊。
    fn default() -> Settings {
        Settings {
            server_backup: true,
            local_room_keys: true,
            transport: Transport::default(),
            read_receipts: ReadReceipts::Private,
            warnings: Vec::new(),
            explicit_conf: None,
        }
    }
}

/// [`Settings::reload`] 的結果（/docs/design/messages/read-receipts.md §6）。
#[derive(Clone, Debug)]
pub struct Reloaded {
    /// 換上去的那一份：會影響連線的鍵還是舊值
    pub settings: Settings,
    /// 值真的變了、而且換上去就生效的鍵, example: vec!["READ_RECEIPTS"]
    pub changed: Vec<&'static str>,
    /// 值變了、但要 daemon 完整重開才載入的鍵, example: vec!["TRANSPORT"]
    pub needs_restart: Vec<&'static str>,
}

impl Settings {
    /// Args:
    ///     explicit_conf: `--config`, example: None
    ///     data_dir: example: "<data dir>"
    /// Return:
    ///     Ok(Settings)   沒有檔就是 `Default`
    ///     Err(Usage)     `--config` 明指的不在、或檔案語法壞了（**不**當作沒有）
    pub fn load(explicit_conf: Option<&Path>, data_dir: &Path) -> Result<Settings, CoreError> {
        let conf = conf::load(explicit_conf, data_dir)?;
        let mut warnings = conf.warnings().to_vec();
        warnings.extend(conf.warn_about_unknown_keys(KNOWN_CONF_KEYS));
        let server_backup = conf.is_on("SERVER_BACKUP", true, &mut warnings);
        let local_room_keys = conf.is_on("LOCAL_ROOM_KEYS", true, &mut warnings);
        // ⭐ 預設來自 `Transport::default()`（`wbf-sdk`），禁止在這裡再寫死一次。
        let transport = match conf.find("TRANSPORT") {
            None => Transport::default(),
            Some(name) => Transport::from_name(name).unwrap_or_else(|| {
                warnings.push(format!(
                    "warning: TRANSPORT={name:?} is not `ws` or `http`; using ws"
                ));
                Transport::default()
            }),
        };
        // 正面認得 `public` 才公開；其餘落到 private（/docs/design/messages/read-receipts.md §5）。
        let read_receipts = match conf.find("READ_RECEIPTS") {
            None => ReadReceipts::Private,
            Some(value) if value.eq_ignore_ascii_case("public") => ReadReceipts::Public,
            Some(value) if value.eq_ignore_ascii_case("private") => ReadReceipts::Private,
            Some(value) => {
                warnings.push(format!(
                    "warning: READ_RECEIPTS={value:?} is not `private` or `public`; using private"
                ));
                ReadReceipts::Private
            }
        };
        Ok(Settings {
            server_backup,
            local_room_keys,
            transport,
            read_receipts,
            warnings,
            explicit_conf: explicit_conf.map(Path::to_path_buf),
        })
    }

    /// `daemon.reload_conf`（/docs/design/messages/read-receipts.md §6）：照啟動時那一份重讀。
    /// 會影響連線的鍵（`TRANSPORT`）留現在的值，變了就列進 `needs_restart`。
    ///
    /// Args:
    ///     data_dir: example: "<data dir>"
    /// Return:
    ///     Ok(Reloaded)
    ///     Err(Usage)     讀不到或語法壞了：呼叫者整份照舊
    pub fn reload(&self, data_dir: &Path) -> Result<Reloaded, CoreError> {
        let mut fresh = Settings::load(self.explicit_conf.as_deref(), data_dir)?;
        let mut needs_restart = Vec::new();
        if fresh.transport != self.transport {
            needs_restart.push("TRANSPORT");
            fresh.transport = self.transport;
        }
        let changed = [
            ("SERVER_BACKUP", fresh.server_backup != self.server_backup),
            (
                "LOCAL_ROOM_KEYS",
                fresh.local_room_keys != self.local_room_keys,
            ),
            ("READ_RECEIPTS", fresh.read_receipts != self.read_receipts),
        ]
        .into_iter()
        .filter(|(_, is_changed)| *is_changed)
        .map(|(key, _)| key)
        .collect();
        Ok(Reloaded {
            settings: fresh,
            changed,
            needs_restart,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_conf_file_means_the_safe_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let settings = Settings::load(None, dir.path()).unwrap();
        assert!(settings.server_backup);
        assert!(settings.local_room_keys);
        assert_eq!(settings.transport, Transport::WebSocket);
        assert_eq!(settings.read_receipts, ReadReceipts::Private);
        assert!(settings.warnings.is_empty());
    }

    #[test]
    fn the_switches_and_transport_are_read_and_unknown_values_fall_to_safe() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(conf::CONF_FILE_NAME),
            "[backup]\nSERVER_BACKUP=off\nLOCAL_ROOM_KEYS=maybe\n[general]\nTRANSPORT=http\nSERVER=http://x\n[read]\nREAD_RECEIPTS=Public\n",
        )
        .unwrap();
        let settings = Settings::load(None, dir.path()).unwrap();
        assert!(!settings.server_backup);
        // `maybe` 不是 on／off：落到開（壞在「還在備份」那一邊）。
        assert!(settings.local_room_keys);
        assert_eq!(settings.transport, Transport::Http);
        assert_eq!(settings.read_receipts, ReadReceipts::Public);
        // SERVER 是 rpc-cli 的鍵，daemon 不認得：警告但不擋。
        assert!(
            settings.warnings.iter().any(|line| line.contains("SERVER")),
            "{:?}",
            settings.warnings
        );
        assert!(settings
            .warnings
            .iter()
            .any(|line| line.contains("LOCAL_ROOM_KEYS")));
        // `[read]` 是認得的區段
        assert!(
            !settings.warnings.iter().any(|line| line.contains("[READ]")),
            "{:?}",
            settings.warnings
        );
    }

    /// /docs/design/messages/read-receipts.md §5：認不得的值落到 private、進 `warnings`。
    #[test]
    fn an_unknown_read_receipts_value_falls_to_private() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(conf::CONF_FILE_NAME),
            "READ_RECEIPTS=everyone\n",
        )
        .unwrap();
        let settings = Settings::load(None, dir.path()).unwrap();
        assert_eq!(settings.read_receipts, ReadReceipts::Private);
        assert!(
            settings
                .warnings
                .iter()
                .any(|line| line.contains("READ_RECEIPTS")),
            "{:?}",
            settings.warnings
        );
    }

    #[test]
    fn an_explicit_conf_that_does_not_exist_is_an_error_not_a_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope.conf");
        assert!(Settings::load(Some(&missing), dir.path()).is_err());
    }

    /// /docs/design/messages/read-receipts.md §6：能重讀的鍵換掉並列進 `changed`；`TRANSPORT` 留舊值、列進 `needs_restart`；
    /// 讀的是同一個 `--config`。
    #[test]
    fn reload_swaps_what_it_can_and_keeps_the_transport() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("elsewhere.conf");
        std::fs::write(&path, "TRANSPORT=ws\n").unwrap();
        let started = Settings::load(Some(&path), dir.path()).unwrap();

        let unchanged = started.reload(dir.path()).unwrap();
        assert!(unchanged.changed.is_empty() && unchanged.needs_restart.is_empty());

        std::fs::write(
            &path,
            "TRANSPORT=http\nREAD_RECEIPTS=public\nSERVER_BACKUP=off\n",
        )
        .unwrap();
        let reloaded = started.reload(dir.path()).unwrap();
        assert_eq!(reloaded.changed, vec!["SERVER_BACKUP", "READ_RECEIPTS"]);
        assert_eq!(reloaded.needs_restart, vec!["TRANSPORT"]);
        assert_eq!(reloaded.settings.transport, Transport::WebSocket);
        assert_eq!(reloaded.settings.read_receipts, ReadReceipts::Public);
        assert_eq!(reloaded.settings.explicit_conf, Some(path.clone()));
    }

    /// §6：讀失敗是錯誤，呼叫者整份照舊（不套一半）。
    #[test]
    fn a_broken_conf_on_reload_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let started = Settings::load(None, dir.path()).unwrap();
        std::fs::write(
            dir.path().join(conf::CONF_FILE_NAME),
            "READ_RECEIPTS=public\nthis is not a pair\n",
        )
        .unwrap();
        assert!(started.reload(dir.path()).is_err());
    }
}

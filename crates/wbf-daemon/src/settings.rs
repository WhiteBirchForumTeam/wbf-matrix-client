//! daemon 從 `wbf.conf` 讀出來、會影響 core 呼叫的那幾個值（CLI 規格 §10；rpc-spec §2）。
//!
//! ⚠️ **這些不在 RPC 上**：`server_backup`、`local_room_keys` 是使用者對這台機器的設定，
//! 前端不該替使用者決定；daemon 是 conf 的主人（architecture-v2 §0.2），讀一次、填進每個 `Target`。
//! 解析在 `wbf_core::conf`（跟 rpc-cli 共用同一份，🚫 不各寫一套）。
//!
//! **優先序是旗標 > 環境變數 > conf > 內建預設**（CLI 規格 §10.2）。⚠️ 這個檔🚫 **不讀環境變數**：
//! daemon 的兩個（`WBF_DATA_DIR`、`WBF_CONFIG`）掛在 `main.rs` 的 clap `env = …` 上，旗標壓環境
//! 由 clap 負責。要給這裡的鍵加環境變數就加在那邊，🚫 不要在這裡自己讀 `std::env` ——
//! 那會繞過旗標，也會讓同一條優先序有兩個實作。

use std::path::Path;

use wbf_core::conf;
use wbf_core::CoreError;
use wbf_sdk::Transport;

/// daemon 認得的鍵；其餘的在 `warnings` 裡（CLI 規格 §10.4：警告、忽略、照跑）。
const KNOWN_CONF_KEYS: &[&str] = &["SERVER_BACKUP", "LOCAL_ROOM_KEYS", "TRANSPORT"];

#[derive(Clone, Debug)]
pub struct Settings {
    /// `SERVER_BACKUP`：標準 Matrix key backup 開著嗎。認不得落到 **開**。
    pub server_backup: bool,
    /// `LOCAL_ROOM_KEYS`：本地全量快照開著嗎。認不得落到 **開**。
    pub local_room_keys: bool,
    /// `TRANSPORT`：沒在 params 帶 `transport` 時的預設。認不得落到 **ws**。
    pub transport: Transport,
    /// 讀 conf 時攢下的警告。🚫 這一層不印，`main.rs` 決定印不印。
    pub warnings: Vec<String>,
}

impl Default for Settings {
    /// 沒有 conf 檔時的值：全部是安全那一邊。
    fn default() -> Settings {
        Settings {
            server_backup: true,
            local_room_keys: true,
            transport: Transport::default(),
            warnings: Vec::new(),
        }
    }
}

impl Settings {
    /// Args:
    ///     explicit_conf: `--config`, example: None
    ///     data_dir: example: "<data dir>"
    /// Return:
    ///     Ok(Settings)   沒有檔就是 `Default`
    ///     Err(Usage)     `--config` 明指的不在、或檔案語法壞了（🚫 不當作沒有）
    pub fn load(explicit_conf: Option<&Path>, data_dir: &Path) -> Result<Settings, CoreError> {
        let conf = conf::load(explicit_conf, data_dir)?;
        let mut warnings = conf.warnings().to_vec();
        warnings.extend(conf.warn_about_unknown_keys(KNOWN_CONF_KEYS));
        let server_backup = conf.is_on("SERVER_BACKUP", true, &mut warnings);
        let local_room_keys = conf.is_on("LOCAL_ROOM_KEYS", true, &mut warnings);
        // ⭐ 預設來自 `Transport::default()`（`wbf-sdk`），🚫 不在這裡再寫死一次。
        let transport = match conf.find("TRANSPORT") {
            None => Transport::default(),
            Some(name) => Transport::from_name(name).unwrap_or_else(|| {
                warnings.push(format!(
                    "warning: TRANSPORT={name:?} is not `ws` or `http`; using ws"
                ));
                Transport::default()
            }),
        };
        Ok(Settings {
            server_backup,
            local_room_keys,
            transport,
            warnings,
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
        assert!(settings.warnings.is_empty());
    }

    #[test]
    fn the_two_switches_and_transport_are_read_and_unknown_values_fall_to_safe() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(conf::CONF_FILE_NAME),
            "[backup]\nSERVER_BACKUP=off\nLOCAL_ROOM_KEYS=maybe\n[general]\nTRANSPORT=http\nSERVER=http://x\n",
        )
        .unwrap();
        let settings = Settings::load(None, dir.path()).unwrap();
        assert!(!settings.server_backup);
        // `maybe` 不是 on／off：落到開（壞在「還在備份」那一邊）。
        assert!(settings.local_room_keys);
        assert_eq!(settings.transport, Transport::Http);
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
    }

    #[test]
    fn an_explicit_conf_that_does_not_exist_is_an_error_not_a_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope.conf");
        assert!(Settings::load(Some(&missing), dir.path()).is_err());
    }
}

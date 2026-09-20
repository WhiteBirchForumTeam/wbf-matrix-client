//! to-device 的兩份本地狀態（to-device-client.md §2、§4）：`cd_seq`（**已經處理完**到哪）與**待銷毀清單**（哪些可以叫 server 刪）。
//!
//! 落在帳號目錄的 `m/` 裡面、跟 crypto store 同一個資料夾（維護者 2026-09-12：「你同步到哪，就應該寫到哪」）：
//! `m/` 被刪（logout、壞掉重來、`key-backup import`）它就一起沒，下次從頭拉——🚫 不進 `cache.db`，否則 `m/` 沒了它還在，
//! 那段區間**永遠不會再拉**，而那些是金鑰。
//!
//! 兩個數字🚫 不合成一個：「處理到哪」是水位，「可以刪哪些」是清單——批次中間匯入失敗時，清單不是水位的前綴。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::SdkError;

/// 檔名短是為了 Windows 的 MAX_PATH（`m/` 底下上游的 sqlite 檔名已經很長）。
pub const TO_DEVICE_STATE_FILE_NAME: &str = "td.json";

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ToDeviceState {
    /// 匯進 crypto store 成功的最新 count；None ＝ 還沒處理過任何一則（下次 `Fetch` 從頭）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cd_seq: Option<u64>,
    /// 匯入成功、但 server 還沒回 `ItemsDestroyed` 說沒了的 count。命令冪等，重送安全。
    #[serde(default)]
    pub to_destroy: Vec<u64>,
}

impl ToDeviceState {
    /// Args:
    ///     store_dir: 帳號目錄的 `m/`, example: "<account dir>/m"
    /// Return:
    ///     Ok(ToDeviceState)  沒有檔案 ＝ 預設（從頭）
    ///     Err(Io)            讀不到（不是「沒有」）
    ///     Err(Protocol)      檔案不是這個形狀——🚫 不當成從頭：那會把一批已經匯過、可能已經銷毀的區間再拉一次
    ///                        （匯入冪等，但 server 那邊已經沒了會拉到空，而更糟的是掩蓋一個壞掉的檔）
    pub fn load(store_dir: &Path) -> Result<ToDeviceState, SdkError> {
        let path = Self::path_in(store_dir);
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(ToDeviceState::default())
            }
            Err(error) => return Err(SdkError::Io(error)),
        };
        serde_json::from_slice(&bytes).map_err(|error| {
            SdkError::Protocol(format!(
                "{} is not a to-device state file: {error}",
                path.display()
            ))
        })
    }

    /// 原子寫：先寫 `.tmp` 再 rename，半寫的檔不會被下次 `load` 讀到。
    pub fn save(&self, store_dir: &Path) -> Result<(), SdkError> {
        std::fs::create_dir_all(store_dir)?;
        let path = Self::path_in(store_dir);
        let scratch_path = path.with_extension("json.tmp");
        std::fs::write(
            &scratch_path,
            serde_json::to_vec(self).expect("ToDeviceState serializes"),
        )?;
        std::fs::rename(&scratch_path, &path)?;
        Ok(())
    }

    /// 一則匯進 crypto store 成功之後叫：水位前進、count 進待銷毀清單。
    ///
    /// Args:
    ///     count: example: 4712
    pub fn mark_processed(&mut self, count: u64) {
        self.cd_seq = Some(self.cd_seq.map_or(count, |current| current.max(count)));
        if !self.to_destroy.contains(&count) {
            self.to_destroy.push(count);
        }
    }

    /// `ItemsDestroyed` 回來之後叫：只拿掉**真的回來的那些**（§4 第 1、3 條）。
    ///
    /// Args:
    ///     destroyed: example: &[4712]
    pub fn mark_destroyed(&mut self, destroyed: &[u64]) {
        self.to_destroy.retain(|count| !destroyed.contains(count));
    }

    fn path_in(store_dir: &Path) -> PathBuf {
        store_dir.join(TO_DEVICE_STATE_FILE_NAME)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("wbf-td-state-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn missing_file_is_the_default_and_round_trips() {
        let dir = scratch_dir("round-trip");
        assert_eq!(ToDeviceState::load(&dir).unwrap(), ToDeviceState::default());
        let mut state = ToDeviceState::default();
        state.mark_processed(4712);
        state.mark_processed(4713);
        state.save(&dir).unwrap();
        assert_eq!(ToDeviceState::load(&dir).unwrap(), state);
        assert_eq!(state.cd_seq, Some(4713));
        assert_eq!(state.to_destroy, vec![4712, 4713]);
        assert!(!dir.join("td.json.tmp").exists(), "暫存檔已 rename 掉");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 第二次 `save` 要蓋掉既有的 `td.json`（PR #48 審查 rumia 🔴 說 Windows 的 rename 不能覆蓋——std 的 `rename` 在 Windows
    /// 走 `MoveFileExW(MOVEFILE_REPLACE_EXISTING)`，這條測試在 Windows 上實跑就是證據）。
    #[test]
    fn saving_again_replaces_the_existing_file() {
        let dir = scratch_dir("overwrite");
        let mut state = ToDeviceState::default();
        state.mark_processed(1);
        state.save(&dir).unwrap();
        state.mark_processed(2);
        state.mark_destroyed(&[1]);
        state.save(&dir).unwrap();
        state.mark_processed(3);
        state.save(&dir).unwrap();
        assert_eq!(
            ToDeviceState::load(&dir).unwrap(),
            ToDeviceState {
                cd_seq: Some(3),
                to_destroy: vec![2, 3]
            }
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 🚨 `Ack` 不清清單；`ItemsDestroyed` 只清回來的那些，沒回來的留著下次再送。
    #[test]
    fn only_counts_the_server_says_are_gone_leave_the_list() {
        let mut state = ToDeviceState::default();
        for count in [10, 11, 12] {
            state.mark_processed(count);
        }
        state.mark_destroyed(&[11]);
        assert_eq!(state.to_destroy, vec![10, 12]);
        state.mark_destroyed(&[]);
        assert_eq!(
            state.to_destroy,
            vec![10, 12],
            "空的 ItemsDestroyed 什麼都不清"
        );
        state.mark_destroyed(&[99]);
        assert_eq!(state.to_destroy, vec![10, 12], "沒列過的 count 不影響");
    }

    /// 水位只升不降；重複 mark 同一則不會讓清單長出重複的 count。
    #[test]
    fn watermark_never_moves_backwards_and_the_list_has_no_duplicates() {
        let mut state = ToDeviceState::default();
        state.mark_processed(20);
        state.mark_processed(15);
        state.mark_processed(20);
        assert_eq!(state.cd_seq, Some(20));
        assert_eq!(state.to_destroy, vec![20, 15]);
    }

    /// 🚨 壞掉的檔是錯，不是「從頭」。
    #[test]
    fn a_corrupt_file_is_an_error_not_a_fresh_start() {
        let dir = scratch_dir("corrupt");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(TO_DEVICE_STATE_FILE_NAME), b"{not json").unwrap();
        assert!(matches!(
            ToDeviceState::load(&dir),
            Err(SdkError::Protocol(_))
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }
}

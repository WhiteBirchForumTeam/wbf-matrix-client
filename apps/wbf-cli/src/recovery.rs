//! recovery key 的本機保管（local-cache-db.md §10.9；維護者 2026-09-09 定）：
//!
//! ```text
//! <data dir>/recovery/<b58>_<b58>      檔名是 `recovery-key@bob:matrix.org` 加密後的樣子
//! ```
//!
//! **為什麼不放在帳號目錄底下**：`logout`／`account del` 要把帳號目錄整個清乾淨
//! （`session.sealed`、`matrix/`、`room-keys/`），而 recovery key 正好是**清完之後唯一回得去的路**——
//! 它是「回到 server 備份的鑰匙」，不是「這台機器上的裝置狀態」。放在一起就會一起被刪，
//! 那等於備份也沒了。
//!
//! 所以：
//!
//! | 誰 | 對 recovery key 做什麼 |
//! |---|---|
//! | `key-backup recovery` | 產生並封進來 |
//! | `logout`／`account del` | **不碰**——這是它跟帳號目錄分開放的全部理由 |
//! | `account destroy` | **一起摧毀**（那個命令的語意就是「什麼都不留」） |
//! | `recovery list`／`recovery show` | 讀 |
//!
//! 檔名跟資料目錄其他兩層一樣是加密的（`DirScope::Recovery`）：外面看不出這台機器
//! 保管著誰的 recovery key。內容用 vault 的第三把子金鑰封（`Vault::seal_recovery_key`）。

use std::path::{Path, PathBuf};

use wbf_sdk::account_dir::{find_dir_name_plaintext, to_dir_name, DirScope};
use wbf_sdk::vault::Vault;
use wbf_sdk::SdkError;

pub const RECOVERY_DIR_NAME: &str = "recovery";
/// 檔名的明文長這樣，example: `recovery-key@alice:localhost`
const NAME_PREFIX: &str = "recovery-key";

/// `<data dir>/recovery/`。
pub fn dir(data_dir: &Path) -> PathBuf {
    data_dir.join(RECOVERY_DIR_NAME)
}

/// 這個帳號的 recovery key 檔在哪。**不掃描**：檔名是確定性加密的，算得出來。
///
/// Args:
///     data_dir: example: "<data dir>"
///     vault: example: context.vault()?
///     user_id: 完整 mxid, example: "@alice:localhost"
/// Return:
///     Ok(PathBuf)   `<data dir>/recovery/<b58>_<b58>`
///     Err(Usage)    加密後的名字太長（§11.4）
pub fn path_of(data_dir: &Path, vault: &Vault, user_id: &str) -> Result<PathBuf, SdkError> {
    let name = to_dir_name(
        &vault.account_dir_key(),
        DirScope::Recovery,
        &format!("{NAME_PREFIX}{user_id}"),
    )?;
    Ok(dir(data_dir).join(name))
}

/// 這台機器保管著誰的 recovery key（`recovery list`）。
///
/// 掃 `<data dir>/recovery/` 解密**檔名**——🚫 不開檔、不解內容：列清單不需要看到金鑰本身。
/// 解不開的檔一律跳過（別把 `local.key` 建的，fail closed）。
///
/// Return:
///     Ok(Vec<String>)   完整 mxid，排序過；一個都沒有就是空的
pub fn list_users(data_dir: &Path, vault: &Vault) -> Result<Vec<String>, SdkError> {
    let key = vault.account_dir_key();
    let mut users = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir(data_dir)) else {
        return Ok(users);
    };
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(plaintext) = find_dir_name_plaintext(&key, DirScope::Recovery, &name) else {
            continue;
        };
        if let Some(user_id) = plaintext.strip_prefix(NAME_PREFIX) {
            users.push(user_id.to_string());
        }
    }
    users.sort();
    Ok(users)
}

/// 把 recovery key 封進來（`key-backup recovery` 產生之後）。
pub fn save(
    data_dir: &Path,
    vault: &Vault,
    user_id: &str,
    recovery_key: &str,
) -> Result<(), SdkError> {
    std::fs::create_dir_all(dir(data_dir))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir(data_dir), std::fs::Permissions::from_mode(0o700))?;
    }
    vault.seal_recovery_key(&path_of(data_dir, vault, user_id)?, recovery_key)
}

/// Return:
///     Ok(Some(String))   封著的 recovery key
///     Ok(None)           這個帳號沒有（還沒跑過 `key-backup recovery`）
///     Err(Usage)         檔案壞了、或不是這把 `local.key` 封的
pub fn find(
    data_dir: &Path,
    vault: &Vault,
    user_id: &str,
) -> Result<Option<zeroize::Zeroizing<String>>, SdkError> {
    vault.unseal_recovery_key(&path_of(data_dir, vault, user_id)?)
}

/// 摧毀這個帳號的 recovery key（**只有 `account destroy` 該叫它**）。
///
/// ⚠️ 不可逆：刪掉之後 server 上那份備份就再也解不開了。
/// 🚫 `logout`／`account del` 不准叫這個——它們留著 recovery key 正是為了讓歷史救得回來。
///
/// Return:
///     Ok(true)    刪掉了
///     Ok(false)   本來就沒有
pub fn del(data_dir: &Path, vault: &Vault, user_id: &str) -> Result<bool, SdkError> {
    match std::fs::remove_file(path_of(data_dir, vault, user_id)?) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wbf_sdk::Unlock;

    fn scratch(name: &str) -> (PathBuf, Vault) {
        let dir = std::env::temp_dir().join(format!("wbf-recovery-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let vault = Vault::create(&dir, &Unlock::NoPassphrase).unwrap();
        (dir, vault)
    }

    #[test]
    fn save_find_list_del_round_trip() {
        let (data_dir, vault) = scratch("round-trip");
        assert!(list_users(&data_dir, &vault).unwrap().is_empty());
        assert!(find(&data_dir, &vault, "@alice:localhost")
            .unwrap()
            .is_none());

        save(&data_dir, &vault, "@alice:localhost", "EsTc 1234 abcd").unwrap();
        save(&data_dir, &vault, "@bob:matrix.org", "EsTc 9999 zzzz").unwrap();

        assert_eq!(
            list_users(&data_dir, &vault).unwrap(),
            vec![
                "@alice:localhost".to_string(),
                "@bob:matrix.org".to_string()
            ]
        );
        assert_eq!(
            find(&data_dir, &vault, "@alice:localhost")
                .unwrap()
                .map(|key| key.to_string()),
            Some("EsTc 1234 abcd".to_string())
        );

        assert!(del(&data_dir, &vault, "@alice:localhost").unwrap());
        assert!(!del(&data_dir, &vault, "@alice:localhost").unwrap());
        assert_eq!(
            list_users(&data_dir, &vault).unwrap(),
            vec!["@bob:matrix.org".to_string()],
            "刪一個不該動到另一個"
        );
        let _ = std::fs::remove_dir_all(&data_dir);
    }

    #[test]
    fn the_file_name_does_not_leak_who_it_belongs_to() {
        let (data_dir, vault) = scratch("filename");
        save(&data_dir, &vault, "@alice:localhost", "EsTc 1234").unwrap();
        let name = std::fs::read_dir(dir(&data_dir))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .file_name()
            .to_string_lossy()
            .into_owned();
        assert!(
            !name.contains("alice") && !name.contains("localhost"),
            "{name}"
        );
        assert!(name.contains('_'), "應該是 <b58 nonce>_<b58 密文>：{name}");
        let _ = std::fs::remove_dir_all(&data_dir);
    }

    #[test]
    fn another_key_file_sees_nothing() {
        let (data_dir, vault) = scratch("otherkey");
        save(&data_dir, &vault, "@alice:localhost", "EsTc 1234").unwrap();

        let (other_dir, other_vault) = scratch("otherkey-2");
        assert!(
            list_users(&data_dir, &other_vault).unwrap().is_empty(),
            "別把 local.key 連檔名都解不開"
        );
        let _ = std::fs::remove_dir_all(&data_dir);
        let _ = std::fs::remove_dir_all(&other_dir);
    }
}

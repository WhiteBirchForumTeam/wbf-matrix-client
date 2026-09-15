//! 每個帳號的資料放哪（CLI 規格 §7；local-cache-db.md §5.6、§11）：
//!
//! ```text
//! <data dir>/
//!   local.key                          一台機器一把主金鑰（vault）
//!   current                             目前帳號：一行 "<加密的 server 目錄名>/<加密的帳號目錄名>"
//!   r/<b58>_<b58>                       recovery key（recovery.rs）；🚫 logout 不碰
//!   s/<b58>_<b58>/                      正規化過的 server host，加密（§11.2）
//!     cache.db                          這個 server 上所有帳號共用的快取
//!     a/<b58>_<b58>/                    localpart，加密
//!       session.sealed                  這個帳號的 session（第三把子金鑰封住）
//!       m/                              matrix-sdk store，綁 device；logout 刪
//! ```
//!
//! **兩層目錄名都是加密的**，所以這個模組的每個進入點都要第六把子金鑰（`vault.account_dir_key()`）。
//! 中間那幾段（`s`／`a`／`m`）短到只剩一個字母，理由是 Windows 的 MAX_PATH——見 `SERVERS_DIR_NAME`。
//! 明文的 server URL 與 mxid 仍然在 `session.sealed` 裡，不從目錄名反推。
//!
//! 路徑映射有兩條路，用途不同（維護者 2026-09-10 定）：
//!
//! | 手上有什麼 | 用哪個 |
//! |---|---|
//! | **確定就是這個明文**（剛登入、`current` 解出來的） | `AccountDir::locate`：加密是確定性的，直接算，不碰磁碟 |
//! | **使用者打的字串**（`account del @BOB:…`） | `refresh_data_dir_map` 當場掃一次，再從 map 比對 |
//!
//! 第二條路不能用算的：`@BOB:matrix.org` 加密出來的名字跟 `@bob:matrix.org` 完全不同，
//! 而大小寫不敏感的比對**沒有算式**，只能拿現場有什麼來比。所以會刪檔的命令一律是
//! 「**當場**刷新 → 解密 → 建 map → 比對」，🚫 不留成長命的全域狀態——存起來的那一份
//! 不會知道中間有東西被刪掉，而它決定的是刪哪個目錄。

use std::path::{Path, PathBuf};

use serde::Serialize;
use wbf_sdk::account_dir::{find_dir_name_plaintext, to_dir_name, DirScope};
use wbf_sdk::vault::{write_private, Key32, Vault, SEALED_SESSION_FILE_NAME};
use wbf_sdk::SdkError;

// ⚠️ 這三個名字**故意很短**：它們夾在兩段加密目錄名之間，而 Windows 的 MAX_PATH 是 260
// （2026-09-09 對真 server 驗證時撞到）。可讀性在這裡本來就沒了——下一層就是密文。
pub const SERVERS_DIR_NAME: &str = "s";
pub const ACCOUNTS_DIR_NAME: &str = "a";
pub const MATRIX_STORE_DIR_NAME: &str = "m";
pub const CURRENT_FILE_NAME: &str = "current";

/// 一個帳號在磁碟上的位置。`server_host` 與 `localpart` 是明文，`dir` 裡的兩段是加密的。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccountDir {
    /// 正規化過的 server host（明文），example: "localhost:6167"
    pub server_host: String,
    /// localpart（明文），example: "alice"
    pub localpart: String,
    /// `<data dir>/s/<b58>_<b58>/a/<b58>_<b58>`
    pub dir: PathBuf,
    /// `current` 檔記的就是這兩段（都是加密後的名字）。
    server_dir_name: String,
    account_dir_name: String,
}

impl AccountDir {
    /// 算出這個帳號的目錄。**不掃描、不碰磁碟**：加密是確定性的，同一把金鑰配同一個帳號永遠是同一條路徑。
    ///
    /// Args:
    ///     data_dir: example: "<data dir>"
    ///     key: example: vault.account_dir_key()
    ///     server: example: "http://localhost:6167"
    ///     user: mxid 或 localpart, example: "@alice:localhost"
    /// Return:
    ///     Ok(AccountDir)
    ///     Err(Usage)   localpart 是空的、或加密後的名字太長（§11.4）
    pub fn locate(
        data_dir: &Path,
        key: &Key32,
        server: &str,
        user: &str,
    ) -> Result<AccountDir, SdkError> {
        let server_host = server_host_of(server);
        let localpart = localpart_of(user).to_string();
        let server_dir_name = to_dir_name(key, DirScope::Server, &server_host)?;
        let account_dir_name = to_dir_name(
            key,
            DirScope::Account {
                server_host: &server_host,
            },
            &localpart,
        )?;
        Ok(AccountDir {
            dir: data_dir
                .join(SERVERS_DIR_NAME)
                .join(&server_dir_name)
                .join(ACCOUNTS_DIR_NAME)
                .join(&account_dir_name),
            server_host,
            localpart,
            server_dir_name,
            account_dir_name,
        })
    }

    pub fn session_path(&self) -> PathBuf {
        self.dir.join(SEALED_SESSION_FILE_NAME)
    }

    pub fn matrix_store_dir(&self) -> PathBuf {
        self.dir.join(MATRIX_STORE_DIR_NAME)
    }

    /// `s/<b58>_<b58>/`：`cache.db` 與媒體池在這一層，同 server 的帳號共用。
    pub fn server_dir(&self) -> PathBuf {
        self.dir
            .parent()
            .and_then(Path::parent)
            .map(Path::to_path_buf)
            .unwrap_or_else(|| self.dir.clone())
    }

    pub fn is_logged_in(&self) -> bool {
        self.session_path().exists()
    }

    /// `current` 檔的內容：兩段**加密後**的目錄名。🚫 不寫明文（寫了等於把剛加密的名字再漏一次）。
    pub fn key(&self) -> String {
        format!("{}/{}", self.server_dir_name, self.account_dir_name)
    }

    /// 給人看的名字（錯誤訊息用），example: "alice on localhost:6167"
    pub fn label(&self) -> String {
        format!("{} on {}", self.localpart, self.server_host)
    }

    /// 裝置層的狀態：matrix-sdk 的 store（綁 device_id）。logout 或換裝置時丟；`cache.db`（綁 server）與 `local.key` 不動。
    ///
    /// ⚠️ Windows 上剛用完的 SQLite store **有時還被握著**（`os error 32`：檔案正由另一個程序使用）——
    /// 登出的閘門才剛開過一次 backend 去問 recovery 狀態，那個 handle 關掉與 OS 真的放手之間有延遲。
    /// 所以這裡**重試幾次**（2026-09-13 對真 server 跑 daemon e2e 時遇到，第二次跑就過了 —— 典型的 race）。
    /// 🚫 重試完還是不行就回錯，不吞掉：那時多半是**別的程序**開著同一個 store（例如一個常駐的
    /// daemon 加一個單發命令，architecture-v2 §0.2），而那件事必須讓呼叫端知道。
    pub fn delete_matrix_store(&self) -> Result<(), SdkError> {
        const TRIES: u32 = 10;
        const WAIT: std::time::Duration = std::time::Duration::from_millis(100);
        for remaining in (0..TRIES).rev() {
            match std::fs::remove_dir_all(self.matrix_store_dir()) {
                Ok(()) => return Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                Err(error) if remaining == 0 => return Err(error.into()),
                Err(_) => std::thread::sleep(WAIT),
            }
        }
        unreachable!("the loop returns on the last try")
    }
}

/// `accounts` 命令印的一列。
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct AccountSummary {
    /// 權威的完整 mxid，來源是 `session.sealed`。**登出的帳號沒有**（目錄名只解得出 localpart 與 host，
    /// 組不出可靠的 mxid），所以是 `null` —— 🚫 不自己拼一個出來騙人。
    pub user_id: Option<String>,
    pub server: String,
    pub localpart: String,
    pub logged_in: bool,
    pub current: bool,
}

/// 資料目錄現在有什麼：明文 → 磁碟上那個（加密的）名字。
///
/// ⚠️ **快照，不是快取**：`refresh_data_dir_map` 那一刻的磁碟狀態。要用就當場刷新一次。
pub struct DataDirMap {
    data_dir: PathBuf,
    servers: Vec<ServerEntry>,
    /// 完整 mxid → `r/` 底下那個檔名（`recovery::list_kept`）。
    recovery_keys: Vec<Mapping>,
}

/// 一段名字：明文，與它在磁碟上加密後的樣子。
struct Mapping {
    plaintext: String,
    dir_name: String,
}

struct ServerEntry {
    /// 明文是正規化過的 server host, example: "localhost:6167"
    server: Mapping,
    /// 明文是 localpart, example: "alice"
    accounts: Vec<Mapping>,
}

/// 掃 `s/*/a/*` 兩層與 `r/`，解密每一段名字，做成 map（local-cache-db.md §11.5）。
///
/// 解不開的一律跳過（fail closed）：可能是別把 `local.key` 建的，也可能是舊版留下的明文佈局。
/// 🚫 不猜、🚫 不刪、🚫 不報錯——當它不存在。
///
/// Args:
///     data_dir: example: "<data dir>"
///     vault: example: context.vault()?
/// Return:
///     Ok(DataDirMap)   解得開的都在裡面；一個都解不開就是空的
pub fn refresh_data_dir_map(data_dir: &Path, vault: &Vault) -> Result<DataDirMap, SdkError> {
    let key = vault.account_dir_key();
    let mut map = DataDirMap {
        data_dir: data_dir.to_path_buf(),
        servers: Vec::new(),
        recovery_keys: Vec::new(),
    };
    if let Ok(server_entries) = std::fs::read_dir(data_dir.join(SERVERS_DIR_NAME)) {
        for server_entry in server_entries {
            let server_entry = server_entry?;
            if !server_entry.file_type()?.is_dir() {
                continue;
            }
            let server_dir_name = server_entry.file_name().to_string_lossy().into_owned();
            // destroy 改名後還沒刪完的舊目錄：垃圾，🚫 不當成 server（local-cache-db.md §6）。
            if crate::account_lock::is_to_be_deleted_dir_name(&server_dir_name) {
                continue;
            }
            let Some(server_host) =
                find_dir_name_plaintext(&key, DirScope::Server, &server_dir_name)
            else {
                continue;
            };
            map.add_server(server_host.clone(), server_dir_name);
            // localpart 的密文綁著它上面那層的 host 明文（§11.2），所以 scope 要帶進去。
            let scope = DirScope::Account {
                server_host: &server_host,
            };
            let Ok(account_entries) =
                std::fs::read_dir(server_entry.path().join(ACCOUNTS_DIR_NAME))
            else {
                continue;
            };
            for account_entry in account_entries {
                let account_entry = account_entry?;
                if !account_entry.file_type()?.is_dir() {
                    continue;
                }
                let account_dir_name = account_entry.file_name().to_string_lossy().into_owned();
                let Some(localpart) = find_dir_name_plaintext(&key, scope, &account_dir_name)
                else {
                    continue;
                };
                map.add_account(&server_host, localpart, account_dir_name);
            }
        }
    }
    for (user_id, file_name) in crate::recovery::list_kept(data_dir, vault)? {
        map.add_recovery_key(user_id, file_name);
    }
    Ok(map)
}

impl DataDirMap {
    fn add_server(&mut self, server_host: String, dir_name: String) {
        self.servers.push(ServerEntry {
            server: Mapping {
                plaintext: server_host,
                dir_name,
            },
            accounts: Vec::new(),
        });
    }

    /// ⚠️ `server_host` 要是 `add_server` 進來過的那個明文——它就是上一層掃出來的，所以對得上。
    fn add_account(&mut self, server_host: &str, localpart: String, dir_name: String) {
        if let Some(entry) = self
            .servers
            .iter_mut()
            .find(|entry| entry.server.plaintext == server_host)
        {
            entry.accounts.push(Mapping {
                plaintext: localpart,
                dir_name,
            });
        }
    }

    fn add_recovery_key(&mut self, user_id: String, file_name: String) {
        self.recovery_keys.push(Mapping {
            plaintext: user_id,
            dir_name: file_name,
        });
    }

    /// 使用者打的那串 → 帳號目錄。`server` 沒給就掃所有 server 找同名 localpart。
    ///
    /// Args:
    ///     user: mxid 或 localpart, example: "@BOB:matrix.org"
    ///     server: example: Some("http://localhost:6167")
    /// Return:
    ///     Ok(AccountDir)
    ///     Err(Usage)   找不到；或多個 server 都有這個 localpart 而 `server` 沒給
    pub fn find_account_dir(
        &self,
        user: &str,
        server: Option<&str>,
    ) -> Result<AccountDir, SdkError> {
        let localpart = localpart_of(user);
        let servers: Vec<&ServerEntry> = match server {
            Some(server) => find_one(
                &self.servers,
                |entry| &entry.server.plaintext,
                &server_host_of(server),
                "server",
            )?
            .into_iter()
            .collect(),
            None => self.servers.iter().collect(),
        };
        let mut matches: Vec<(&ServerEntry, &Mapping)> = Vec::new();
        for entry in servers {
            if let Some(account) = find_one(
                &entry.accounts,
                |account| &account.plaintext,
                localpart,
                "account",
            )? {
                matches.push((entry, account));
            }
        }
        match matches.as_slice() {
            [] => Err(SdkError::Usage(format!(
                "no account {user}{} in {}; run `login` first",
                server
                    .map(|server| format!(" on {server}"))
                    .unwrap_or_default(),
                self.data_dir.display()
            ))),
            [(entry, account)] => Ok(self.account_dir_of(entry, account)),
            many => Err(SdkError::Usage(format!(
                "{user} exists on {} servers ({}); pass --server too",
                many.len(),
                many.iter()
                    .map(|(entry, _)| entry.server.plaintext.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ))),
        }
    }

    /// 使用者打的那串 → 這台機器**實際保管**的那個 mxid（`account destroy` 用）。
    ///
    /// 封存時用的是 server 的權威 mxid，而使用者可能打成 `@BOB:Matrix.org`——檔名是那串明文
    /// 加密出來的，大小寫差一個字就算出另一個名字，於是刪不到（PR #19 審查 rumia🟡1／salvia）。
    ///
    /// Args:
    ///     user_id: 使用者打的 mxid, example: "@BOB:matrix.org"
    /// Return:
    ///     Ok(Some(&str))   這台機器實際保管的那串, example: "@bob:matrix.org"
    ///     Ok(None)         沒保管
    ///     Err(Usage)       只差大小寫的保管著好幾把——🚫 不猜，要精確的那串
    pub fn find_recovery_key_user_id(&self, user_id: &str) -> Result<Option<&str>, SdkError> {
        Ok(find_one(
            &self.recovery_keys,
            |entry| &entry.plaintext,
            user_id,
            "recovery key",
        )?
        .map(|entry| entry.plaintext.as_str()))
    }

    /// 這台機器上的每個帳號（`account status`）。
    ///
    /// 🚫 目錄名解出來的只有 localpart 與 host，組不出可靠的 mxid——權威的那串在
    /// `session.sealed` 裡，所以這裡才需要 `vault`。
    ///
    /// Return:
    ///     Ok(Vec<AccountSummary>)   照 server、localpart 排序；一個都沒有就是空的
    pub fn list_accounts(&self, vault: &Vault) -> Result<Vec<AccountSummary>, SdkError> {
        let current = read_current(&self.data_dir)?;
        let current = current.as_deref();
        let mut summaries: Vec<AccountSummary> = self
            .servers
            .iter()
            .flat_map(|server| {
                server.accounts.iter().map(move |account| {
                    let dir = self.account_dir_of(server, account);
                    AccountSummary {
                        // 解不開的 session 不擋掉整份清單：那個帳號就當登出的看待（fail closed）。
                        user_id: vault
                            .unseal_session(&dir.session_path())
                            .ok()
                            .flatten()
                            .map(|session| session.user_id),
                        logged_in: dir.is_logged_in(),
                        current: current == Some(dir.key().as_str()),
                        server: dir.server_host,
                        localpart: dir.localpart,
                    }
                })
            })
            .collect();
        summaries.sort_by(|left, right| {
            left.server
                .cmp(&right.server)
                .then(left.localpart.cmp(&right.localpart))
        });
        Ok(summaries)
    }

    /// `s/` 底下有目錄，但一個都解不開 —— 多半是舊版（明文目錄名）留下的，或換過 `local.key`。
    /// 維護者 2026-09-09：不寫遷移，砍掉重來，所以這裡只回一句提示給呼叫者印（local-cache-db.md §11.7）。
    ///
    /// Return:
    ///     Some(String)   該印的那一行
    ///     None           沒有 `s/`、或至少解得開一個
    pub fn find_undecryptable_layout_hint(&self) -> Option<String> {
        if !self.servers.is_empty() {
            return None;
        }
        let servers = self.data_dir.join(SERVERS_DIR_NAME);
        // 等著被刪的舊 server 目錄不算：它們本來就不該解得開，拿它們提示「local.key 換過了」是誤報。
        let any_entry = std::fs::read_dir(&servers).ok()?.flatten().any(|entry| {
            !crate::account_lock::is_to_be_deleted_dir_name(&entry.file_name().to_string_lossy())
        });
        any_entry.then(|| {
            format!(
                "warning: no directory in {} could be decrypted with this local.key; if this data dir was made by an older build, delete it and run `login` again",
                servers.display()
            )
        })
    }

    fn account_dir_of(&self, server: &ServerEntry, account: &Mapping) -> AccountDir {
        AccountDir {
            dir: self
                .data_dir
                .join(SERVERS_DIR_NAME)
                .join(&server.server.dir_name)
                .join(ACCOUNTS_DIR_NAME)
                .join(&account.dir_name),
            server_host: server.server.plaintext.clone(),
            localpart: account.plaintext.clone(),
            server_dir_name: server.server.dir_name.clone(),
            account_dir_name: account.dir_name.clone(),
        }
    }
}

/// 明文比對的**唯一**規則，整個資料目錄共用：先精確，再大小寫不敏感。
///
/// ⚠️ 大小寫那一輪對到兩個以上就 `Err`，🚫 不挑一個最像的 —— 這些名字決定的是刪哪個
/// 目錄。而且**不是靜默的 None**：`destroy` 說的是「什麼都不留」，它自己不知道有沒有
/// 留乾淨，對呼叫者就是另一種謊（PR #21 審查 rumia🟡2a）。
///
/// Args:
///     wanted: 使用者打的那串, example: "@BOB:matrix.org"
///     what: 講給人聽的名稱, example: "account"
/// Return:
///     Ok(Some(&T))   正面認得一個
///     Ok(None)       沒有
///     Err(Usage)     只差大小寫的有好幾個，訊息列出候選
fn find_one<'a, T>(
    entries: &'a [T],
    plaintext_of: impl Fn(&T) -> &str,
    wanted: &str,
    what: &str,
) -> Result<Option<&'a T>, SdkError> {
    if let Some(exact) = entries.iter().find(|entry| plaintext_of(entry) == wanted) {
        return Ok(Some(exact));
    }
    let insensitive: Vec<&T> = entries
        .iter()
        .filter(|entry| plaintext_of(entry).eq_ignore_ascii_case(wanted))
        .collect();
    match insensitive.as_slice() {
        [] => Ok(None),
        [only] => Ok(Some(only)),
        many => Err(SdkError::Usage(format!(
            "\"{wanted}\" matches {} {what}s that differ only in case ({}); pass the exact one",
            many.len(),
            many.iter()
                .map(|entry| plaintext_of(entry))
                .collect::<Vec<_>>()
                .join(", ")
        ))),
    }
}

/// `find_one` 的公開版，給資料目錄以外、但同樣要「使用者打的字串 → 權威值」的地方用
/// （例如 `cache.db` 的 `users` 列）。比對規則只有這一份，🚫 不要在別的模組再寫一次。
///
/// Args:
///     candidates: 權威值, example: &["@alice:localhost".to_string()]
///     wanted: 使用者打的那串, example: "@ALICE:LocalHost"
///     what: 講給人聽的名稱, example: "cached account"
/// Return:
///     Ok(Some(&str))   權威值本身
///     Ok(None)         沒有
///     Err(Usage)       只差大小寫的有好幾個
pub fn find_matching_plaintext<'a>(
    candidates: &'a [String],
    wanted: &str,
    what: &str,
) -> Result<Option<&'a str>, SdkError> {
    Ok(find_one(candidates, |candidate| candidate.as_str(), wanted, what)?.map(String::as_str))
}

/// 這個 server 底下還有沒有任何登入中的帳號（`logout` 用：都沒有就把 `cache.db` 一起刪）。
pub fn has_any_logged_in_account(server_dir: &Path) -> bool {
    std::fs::read_dir(server_dir.join(ACCOUNTS_DIR_NAME))
        .map(|entries| {
            entries
                .flatten()
                .any(|entry| entry.path().join(SEALED_SESSION_FILE_NAME).exists())
        })
        .unwrap_or(false)
}

/// Return:
///     Ok(Some(String))   `current` 的內容：兩段加密後的目錄名
///     Ok(None)           沒登入過
pub fn read_current(data_dir: &Path) -> Result<Option<String>, SdkError> {
    match std::fs::read_to_string(data_dir.join(CURRENT_FILE_NAME)) {
        Ok(text) => {
            let trimmed = text.trim();
            Ok((!trimmed.is_empty()).then(|| trimmed.to_string()))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

pub fn write_current(data_dir: &Path, account: &AccountDir) -> Result<(), SdkError> {
    write_private(&data_dir.join(CURRENT_FILE_NAME), account.key().as_bytes())
}

pub fn clear_current_if(data_dir: &Path, account: &AccountDir) -> Result<(), SdkError> {
    if read_current(data_dir)?.as_deref() == Some(account.key().as_str()) {
        match std::fs::remove_file(data_dir.join(CURRENT_FILE_NAME)) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

/// `current` 的內容（兩段密文）→ 目錄。解不開或目錄不在就當沒有（fail closed）。
///
/// Args:
///     data_dir: example: "<data dir>"
///     key: example: vault.account_dir_key()
///     current: `read_current` 的回傳, example: "3vQ…_2NE…/5Ab…_7Cd…"
/// Return:
///     Some(AccountDir)
///     None   格式不對、解不開、或那個目錄不存在
pub fn find_account_of_current(data_dir: &Path, key: &Key32, current: &str) -> Option<AccountDir> {
    let (server_dir_name, account_dir_name) = current.split_once('/')?;
    let server_host = find_dir_name_plaintext(key, DirScope::Server, server_dir_name)?;
    let localpart = find_dir_name_plaintext(
        key,
        DirScope::Account {
            server_host: &server_host,
        },
        account_dir_name,
    )?;
    let dir = data_dir
        .join(SERVERS_DIR_NAME)
        .join(server_dir_name)
        .join(ACCOUNTS_DIR_NAME)
        .join(account_dir_name);
    dir.is_dir().then(|| AccountDir {
        server_host,
        localpart,
        dir,
        server_dir_name: server_dir_name.to_string(),
        account_dir_name: account_dir_name.to_string(),
    })
}

/// `http://localhost:6167` → `localhost:6167`；`https://matrix.example.org` → `matrix.example.org`（預設 port 不帶）。
///
/// ⚠️ 這是**加密的輸入**（§11.3），不是檔名了：所以要正規化到底（小寫），
/// 🚫 不再過濾 `[A-Za-z0-9._-]` —— 那是為了當檔名才做的，留著只會讓不同的 host 撞成同一個目錄。
pub fn server_host_of(server: &str) -> String {
    // scheme 先小寫再比：`HTTPS://x:443` 與 `https://x:443` 必須算同一台，
    // 不然 443 只在其中一邊被當成預設 port 拿掉，同一個 server 長出兩個目錄
    // （PR #19 審查 rumia🟢／salvia🟡4；正是 §11.3 要根除的形狀）。
    let lowered = server.to_lowercase();
    let without_scheme = lowered
        .trim_end_matches('/')
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(&lowered);
    // `user:pass@host` 的憑證不是 host 的一部分（Matrix 的 URL 極少這樣寫，但寫了不該壞）。
    let without_credentials = without_scheme
        .rsplit_once('@')
        .map(|(_, host)| host)
        .unwrap_or(without_scheme);
    let host_port = without_credentials
        .split(['/', '?', '#'])
        .next()
        .unwrap_or(without_credentials);
    let is_https = lowered.starts_with("https://");
    let host = match host_port.rsplit_once(':') {
        Some((host, port)) if port.chars().all(|c| c.is_ascii_digit()) => {
            let default = if is_https { "443" } else { "80" };
            if port == default {
                host.to_string()
            } else {
                format!("{host}:{port}")
            }
        }
        _ => host_port.to_string(),
    };
    host
}

/// `@alice:localhost` → `alice`；`alice` → `alice`。
pub fn localpart_of(user: &str) -> &str {
    let stripped = user.strip_prefix('@').unwrap_or(user);
    stripped
        .split_once(':')
        .map(|(local, _)| local)
        .unwrap_or(stripped)
}

#[cfg(test)]
mod tests {
    use super::*;

    use wbf_sdk::Unlock;

    /// 測試用的資料目錄，裡面建好一把 `local.key`（目錄名的加密要它）。
    fn scratch_dir_with_vault(name: &str) -> (PathBuf, Vault) {
        let dir = std::env::temp_dir().join(format!("wbf-accounts-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let vault = Vault::create(&dir, &Unlock::NoPassphrase).unwrap();
        (dir, vault)
    }

    #[test]
    fn server_host_drops_scheme_and_default_port() {
        assert_eq!(server_host_of("http://localhost:6167"), "localhost:6167");
        assert_eq!(
            server_host_of("https://matrix.example.org"),
            "matrix.example.org"
        );
        assert_eq!(
            server_host_of("https://matrix.example.org:443/"),
            "matrix.example.org"
        );
        assert_eq!(server_host_of("http://example.org:80"), "example.org");
        assert_eq!(server_host_of("http://10.0.0.1:8008/path"), "10.0.0.1:8008");
    }

    #[test]
    fn server_host_is_lowercased_so_one_server_gets_one_directory() {
        // 加密是逐 byte 的：漏了這一步，同一台 server 打成大寫就會長出第二個目錄（§11.3）。
        assert_eq!(
            server_host_of("https://MATRIX.example.ORG"),
            server_host_of("https://matrix.example.org")
        );
        // scheme 也要一起小寫，不然只有其中一邊會把預設 port 拿掉（PR #19 審查）。
        assert_eq!(
            server_host_of("HTTPS://matrix.example.org:443"),
            server_host_of("https://matrix.example.org")
        );
        assert_eq!(
            server_host_of("HTTP://localhost:6167"),
            server_host_of("http://localhost:6167")
        );
    }

    #[test]
    fn credentials_in_the_url_are_not_part_of_the_host() {
        assert_eq!(
            server_host_of("http://token@localhost:6167"),
            "localhost:6167"
        );
        assert_eq!(
            server_host_of("https://user:pass@matrix.example.org"),
            "matrix.example.org"
        );
    }

    #[test]
    fn localpart_parsing() {
        assert_eq!(localpart_of("@alice:localhost"), "alice");
        assert_eq!(localpart_of("alice"), "alice");
    }

    #[test]
    fn locate_is_deterministic_and_hides_both_levels() {
        let (data_dir, vault) = scratch_dir_with_vault("locate");
        let key = vault.account_dir_key();
        let alice =
            AccountDir::locate(&data_dir, &key, "http://localhost:6167", "@alice:localhost")
                .unwrap();
        let again =
            AccountDir::locate(&data_dir, &key, "http://localhost:6167", "@alice:localhost")
                .unwrap();
        assert_eq!(alice, again);
        assert_eq!(alice.server_host, "localhost:6167");
        assert_eq!(alice.localpart, "alice");

        let path = alice.dir.display().to_string();
        assert!(
            !path.contains("alice") && !path.contains("localhost"),
            "路徑不該洩漏 server 或帳號：{path}"
        );
        assert!(alice.key().contains('/') && !alice.key().contains("alice"));
        let _ = std::fs::remove_dir_all(&data_dir);
    }

    #[test]
    fn list_and_current_round_trip() {
        let (data_dir, vault) = scratch_dir_with_vault("list");
        let key = vault.account_dir_key();
        assert!(refresh_data_dir_map(&data_dir, &vault)
            .unwrap()
            .list_accounts(&vault)
            .unwrap()
            .is_empty());
        assert_eq!(read_current(&data_dir).unwrap(), None);

        let alice =
            AccountDir::locate(&data_dir, &key, "http://localhost:6167", "@alice:localhost")
                .unwrap();
        std::fs::create_dir_all(&alice.dir).unwrap();
        // 不是真的 session.sealed（解不開），所以 logged_in 是 true 但 user_id 拿不到——
        // 這正是「解不開的 session 不擋掉整份清單」那條。
        std::fs::write(alice.session_path(), b"x").unwrap();
        let bob =
            AccountDir::locate(&data_dir, &key, "http://localhost:6167", "@bob:localhost").unwrap();
        std::fs::create_dir_all(&bob.dir).unwrap();
        write_current(&data_dir, &alice).unwrap();

        let listed = refresh_data_dir_map(&data_dir, &vault)
            .unwrap()
            .list_accounts(&vault)
            .unwrap();
        assert_eq!(listed.len(), 2);
        assert!(listed[0].logged_in && listed[0].current && listed[0].localpart == "alice");
        assert_eq!(listed[0].user_id, None, "session 解不開就沒有權威 mxid");
        assert!(!listed[1].logged_in && !listed[1].current && listed[1].localpart == "bob");
        assert!(has_any_logged_in_account(&alice.server_dir()));

        let current = read_current(&data_dir).unwrap().unwrap();
        assert_eq!(
            find_account_of_current(&data_dir, &key, &current).unwrap(),
            alice
        );
        clear_current_if(&data_dir, &alice).unwrap();
        assert_eq!(read_current(&data_dir).unwrap(), None);
        let _ = std::fs::remove_dir_all(&data_dir);
    }

    #[test]
    fn another_key_sees_nothing_and_gets_a_hint() {
        let (data_dir, vault) = scratch_dir_with_vault("otherkey");
        let alice = AccountDir::locate(
            &data_dir,
            &vault.account_dir_key(),
            "http://localhost:6167",
            "@alice:localhost",
        )
        .unwrap();
        std::fs::create_dir_all(&alice.dir).unwrap();

        // 另一台機器的 local.key（同一份 data dir 被拷走的情境）。
        let (other_dir, other_vault) = scratch_dir_with_vault("otherkey-2");
        assert!(
            refresh_data_dir_map(&data_dir, &other_vault)
                .unwrap()
                .list_accounts(&other_vault)
                .unwrap()
                .is_empty(),
            "別把金鑰不該看到任何帳號"
        );
        assert!(refresh_data_dir_map(&data_dir, &other_vault)
            .unwrap()
            .find_undecryptable_layout_hint()
            .is_some());
        assert!(
            refresh_data_dir_map(&data_dir, &vault)
                .unwrap()
                .find_undecryptable_layout_hint()
                .is_none(),
            "自己的金鑰解得開就不該印提示"
        );
        let _ = std::fs::remove_dir_all(&data_dir);
        let _ = std::fs::remove_dir_all(&other_dir);
    }

    #[test]
    fn a_differently_cased_mxid_still_finds_the_account_and_its_recovery_key() {
        let (data_dir, vault) = scratch_dir_with_vault("casing");
        let alice = AccountDir::locate(
            &data_dir,
            &vault.account_dir_key(),
            "http://localhost:6167",
            "@alice:localhost",
        )
        .unwrap();
        std::fs::create_dir_all(&alice.dir).unwrap();
        crate::recovery::save(&data_dir, &vault, "@alice:localhost", "EsTc 1234").unwrap();

        let map = refresh_data_dir_map(&data_dir, &vault).unwrap();
        // 精確的那串本來就找得到。
        assert_eq!(
            map.find_account_dir("@alice:localhost", None).unwrap(),
            alice
        );
        assert_eq!(
            map.find_recovery_key_user_id("@alice:localhost").unwrap(),
            Some("@alice:localhost")
        );
        // 打成大寫也要對上——不然 `account del` 說找不到、`account destroy` 宣稱
        // 「什麼都不留」卻留著 recovery key。
        assert_eq!(
            map.find_account_dir("@ALICE:LocalHost", None).unwrap(),
            alice
        );
        assert_eq!(
            map.find_recovery_key_user_id("@ALICE:LocalHost").unwrap(),
            Some("@alice:localhost")
        );
        // server 那一段同樣不敏感（大小寫在 URL 裡很常見）。
        assert_eq!(
            map.find_account_dir("@alice:localhost", Some("http://LOCALHOST:6167"))
                .unwrap(),
            alice
        );
        // 🚫 沒保管的就是沒有，不要挑一個最像的。
        assert_eq!(
            map.find_recovery_key_user_id("@bob:localhost").unwrap(),
            None
        );
        let _ = std::fs::remove_dir_all(&data_dir);
    }

    #[test]
    fn two_names_differing_only_in_case_are_refused_not_guessed() {
        let (data_dir, vault) = scratch_dir_with_vault("ambiguous-case");
        crate::recovery::save(&data_dir, &vault, "@alice:localhost", "EsTc 1").unwrap();
        crate::recovery::save(&data_dir, &vault, "@Alice:localhost", "EsTc 2").unwrap();

        let map = refresh_data_dir_map(&data_dir, &vault).unwrap();
        // 精確打得出來的照樣認得。
        assert_eq!(
            map.find_recovery_key_user_id("@Alice:localhost").unwrap(),
            Some("@Alice:localhost")
        );
        // 🚫 只差大小寫的有兩把時不挑——而且要說出來，不是靜默當作「沒有」：
        // `destroy` 說的是「什麼都不留」，它得知道自己沒留乾淨。
        let error = map
            .find_recovery_key_user_id("@ALICE:localhost")
            .unwrap_err();
        let message = format!("{error}");
        assert!(message.contains("differ only in case"), "{message}");
        assert!(message.contains("@alice:localhost") && message.contains("@Alice:localhost"));
        let _ = std::fs::remove_dir_all(&data_dir);
    }

    #[test]
    fn the_map_is_a_snapshot_so_a_stale_one_is_never_reused() {
        let (data_dir, vault) = scratch_dir_with_vault("snapshot");
        let alice = AccountDir::locate(
            &data_dir,
            &vault.account_dir_key(),
            "http://localhost:6167",
            "@alice:localhost",
        )
        .unwrap();
        std::fs::create_dir_all(&alice.dir).unwrap();
        let before = refresh_data_dir_map(&data_dir, &vault).unwrap();
        assert!(before.find_account_dir("@alice:localhost", None).is_ok());

        std::fs::remove_dir_all(&alice.dir).unwrap();
        // 舊的那份還說得出路徑——這正是它不能被存起來重複用的理由（會刪檔的命令要當場刷新）。
        assert!(before.find_account_dir("@alice:localhost", None).is_ok());
        let after = refresh_data_dir_map(&data_dir, &vault).unwrap();
        assert!(after.find_account_dir("@alice:localhost", None).is_err());
        let _ = std::fs::remove_dir_all(&data_dir);
    }

    #[test]
    fn find_account_needs_a_server_when_the_localpart_is_ambiguous() {
        let (data_dir, vault) = scratch_dir_with_vault("ambiguous");
        for server in ["http://localhost:6167", "https://matrix.example.org"] {
            let account = AccountDir::locate(
                &data_dir,
                &vault.account_dir_key(),
                server,
                "@alice:whatever",
            )
            .unwrap();
            std::fs::create_dir_all(&account.dir).unwrap();
        }
        let map = refresh_data_dir_map(&data_dir, &vault).unwrap();
        let error = map.find_account_dir("alice", None).unwrap_err();
        assert!(format!("{error}").contains("2 servers"), "{error}");
        assert!(map
            .find_account_dir("alice", Some("http://localhost:6167"))
            .is_ok());
        let _ = std::fs::remove_dir_all(&data_dir);
    }

    /// destroy 改名後沒刪完的 `🗑️…`：🚫 不當 server、🚫 不觸發「local.key 換過了」的提示。
    #[test]
    fn to_be_deleted_server_dirs_are_skipped_by_the_scan() {
        let dir = std::env::temp_dir().join(format!(
            "wbf-core-scan-to-be-deleted-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let vault = Vault::create(&dir, &wbf_sdk::Unlock::NoPassphrase).unwrap();
        let leftover = dir.join(SERVERS_DIR_NAME).join(format!(
            "{}anything",
            crate::account_lock::TO_BE_DELETED_PREFIX
        ));
        std::fs::create_dir_all(leftover.join(ACCOUNTS_DIR_NAME)).unwrap();
        let map = refresh_data_dir_map(&dir, &vault).unwrap();
        assert!(map.list_accounts(&vault).unwrap().is_empty());
        assert_eq!(
            map.find_undecryptable_layout_hint(),
            None,
            "垃圾目錄不是換過 local.key 的證據"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

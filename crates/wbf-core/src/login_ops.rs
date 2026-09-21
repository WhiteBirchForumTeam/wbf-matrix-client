//! 登入：建 `local.key`（如果還沒有）、探活、登入（一般 Matrix 走 matrix-sdk 的 Client；wbf 走自己包的 HTTP `/login`，
//! `m/` 只建 crypto store）、封 session、切成 current（account-session.md §3）。

use serde::Serialize;

use wbf_sdk::backend::matrix_sdk::MatrixBackend;
use wbf_sdk::crypto_engine::OlmEngine;
use wbf_sdk::login::{Session, SessionBackend};
use wbf_sdk::vault::{Key32, KeyMode, Vault};
use wbf_sdk::Unlock;

use crate::accounts::AccountDir;
use crate::backend_choice::BackendKind;
use crate::error::{CoreError, CoreErrorKind};
use crate::Core;

/// `login`／`account add` 的結果。
///
/// 🚫 刻意**沒有** `access_token`：那是秘密，不過邊界（`handles` 模組註解）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct LoginResult {
    pub user_id: String,
    pub device_id: String,
    pub server: String,
    /// 登入成功自動切成 current（CLI 規格 §3.1.1）；換掉的是誰。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub switched_from: Option<String>,
}

impl Core {
    /// 建這個資料目錄的 `local.key`。
    ///
    /// ⚠️ 「要不要設 passphrase」是**前端的決定**（§3），所以它在這裡就是一個參數：
    /// 給 `Some(bytes)` 就是 passphrase 模式，`None` 就是 plain。🚫 core 不問、不猜。
    ///
    /// Return:
    ///     Ok(KeyMode)   建好了，回它是哪種模式
    ///     Err(Usage)    已經有一把了（🚫 不覆蓋：那會把既有的帳號全部鎖在外面）
    pub fn create_vault(&self, passphrase: Option<&[u8]>) -> Result<KeyMode, CoreError> {
        if self.key_mode()?.is_some() {
            return Err(CoreError::new(
                CoreErrorKind::Usage,
                format!(
                    "{} already has a key file; it will not be overwritten",
                    self.data_dir.display()
                ),
            ));
        }
        let unlock = match passphrase {
            Some(bytes) => Unlock::Passphrase(zeroize::Zeroizing::new(bytes.to_vec())),
            None => Unlock::NoPassphrase,
        };
        let vault = Vault::create(&self.data_dir, &unlock)?;
        let mode = vault.mode();
        self.adopt_unlocked_vault(vault);
        Ok(mode)
    }

    /// 登入一個帳號。**要先解鎖或先 [`Core::create_vault`]**：store 的金鑰與
    /// `session.sealed` 都從 vault 來。
    ///
    /// Args:
    ///     user: mxid 或 localpart, example: "@alice:localhost"
    ///     password: 🚫 不印、不 log；⚠️ 它要送給 homeserver，所以是**字串**不是任意
    ///         bytes（local-cache-db §12.4：跟 passphrase 分家的理由）
    ///     device_name: example: "wbf-cli"
    pub async fn log_in(
        &self,
        server: &str,
        user: &str,
        password: &str,
        device_name: &str,
        server_backup: bool,
    ) -> Result<LoginResult, CoreError> {
        let vault = self.vault()?;
        // 🚨 全程握著帳號生命週期鎖（`account_lock`）：destroy／logout 正在刪目錄的時候不准建，反之亦然。
        // ⚠️ 排在碰任何目錄之前（下面那行就可能刪 `m/`）。
        let _lifecycle = crate::account_lock::lock_account_lifecycle(&self.data_dir)?;
        let dir_key = vault.account_dir_key();
        // 帳號目錄由 server host 加 localpart 決定（store 在 login 前就要有路徑）。
        let account = AccountDir::locate(&self.data_dir, &dir_key, server, user)?;
        // 🚨 這台 server 的目錄正在（或上次刪到一半停在）被刪：🚫 不准在上面建東西（維護者 2026-09-15）。
        let server_lock = account
            .server_dir()
            .join(crate::account_lock::TO_BE_DELETED_LOCK_FILE_NAME);
        if server_lock.exists() {
            return Err(CoreError::new(
                CoreErrorKind::ServerPendingRemoval,
                format!(
                    "the local data for this server is being removed ({} exists); \
                     if no destroy is running, a previous one stopped part way: delete {} by hand, then log in again",
                    server_lock.display(),
                    account.server_dir().display()
                ),
            ));
        }
        // 沒有 session 卻留著 `m/`：上次沒走 logout（或舊版的 logout 沒刪），那個 store
        // 綁著已經失效的裝置。消費端自己再清一次。
        if !account.is_logged_in() && account.matrix_store_dir().exists() {
            self.events
                .progress("removing a matrix store left over from a previous device");
            account.delete_matrix_store()?;
        }
        // account-session.md §3：先探活（不帶 token），再決定走哪一邊。探不到當一般 Matrix（探活自己的規矩：只算這一次）。
        let speaks_wbf = self.get_backend_kind_of_server(server).await == BackendKind::WbfSdk;
        let session = if speaks_wbf {
            // wbf：標準 HTTP `/login`（自己包的那支），🚫 不建 Client。之後房間、訊息、媒體、金鑰全走 WS（§2）。
            let mut session =
                wbf_sdk::login::login_with_password(server, user, password, device_name).await?;
            session.backend = Some(SessionBackend::WbfSdk);
            session
        } else {
            let (_backend, session) = MatrixBackend::login(
                server,
                user,
                password,
                device_name,
                &account.matrix_store_dir(),
                &vault.matrix_store_key(),
                server_backup,
            )
            .await?;
            session
        };
        // ⚠️ server 回的 `user_id` 才是**權威**（大小寫、localpart 正規化可能跟打的不一樣）：
        // 目錄名對不上就搬過去。
        // 🚫 打字算出來的目錄上不該有池（池只從 `client_of` 建、鍵是有 session 的目錄，而下面的改名只在正規目錄不存在時發生），
        // 但這一行不靠那個前提：改名之前先關掉那裡可能有的線，之後池的鍵就找不到它了（PR #53 審查 rumia 🔴；消費端自己再問一次）。
        self.close_links(&account, "session replaced by a new login")
            .await;
        let account = self.move_to_canonical_dir(account, &dir_key, server, &session.user_id)?;
        if speaks_wbf {
            // 改完名才建：`m/` 要落在正規目錄（Client 那條路是登入前就建、跟著目錄一起搬）。
            self.create_crypto_store_for(&account, &session, &vault.matrix_store_key())
                .await?;
        }
        vault.seal_session(&account.session_path(), &session)?;
        // 📎 探活以 server 為鍵、不帶 token（account-session.md §1）：換 session 不影響它，這裡不再忘掉探測結果。
        // 🚨 舊 session 開著的線也不算數（它們拿的是舊 token）：整個池關掉，下一個命令用新 session 重開（link-pool.md §3；PR #53 審查 cirno 🟡1）。
        self.close_links(&account, "session replaced by a new login")
            .await;
        let switched_from = self.switch_current_to(&account)?;
        Ok(LoginResult {
            user_id: session.user_id,
            device_id: session.device_id,
            server: session.server,
            switched_from,
        })
    }

    /// wbf 帳號的 `m/`：**只有 crypto store**，由 `OlmEngine` 開（account-session.md §2；裝置身分金鑰在這一步生出來，
    /// 上傳等 E2EE 那支）。這裡只要它建好，引擎本身丟掉；長活的引擎 E2EE 的 RPC 面再接。
    ///
    /// 🚨 建不起來就把剛拿到的 token 撤掉（best effort）再回錯：🚫 不留一個「登入了、但沒有金鑰庫」的帳號——
    /// 那種帳號下一次碰到 E2EE 才發現，而那時已經有人把房間金鑰發給一台不存在的裝置。
    async fn create_crypto_store_for(
        &self,
        account: &AccountDir,
        session: &Session,
        store_key: &Key32,
    ) -> Result<(), CoreError> {
        match OlmEngine::open(
            &account.matrix_store_dir(),
            store_key,
            &session.user_id,
            &session.device_id,
        )
        .await
        {
            Ok(_engine) => Ok(()),
            Err(error) => {
                let _ = wbf_sdk::login::logout(session).await;
                let _ = account.delete_matrix_store();
                Err(CoreError::new(
                    CoreErrorKind::Io,
                    format!(
                        "logged in, but the crypto store could not be created in {}, so the new device was logged out again: {error}",
                        account.matrix_store_dir().display()
                    ),
                ))
            }
        }
    }

    /// 目錄名是拿 `--user` 打的那串算的，但權威是 server 回的 mxid。對不上就搬。
    ///
    /// 🚫 目標已經存在就**拒絕**，不合併、不覆蓋：那是兩個帳號的資料撞在一起。
    fn move_to_canonical_dir(
        &self,
        account: AccountDir,
        dir_key: &wbf_sdk::vault::Key32,
        server: &str,
        canonical_user_id: &str,
    ) -> Result<AccountDir, CoreError> {
        let canonical = AccountDir::locate(&self.data_dir, dir_key, server, canonical_user_id)?;
        if canonical.dir == account.dir {
            return Ok(account);
        }
        if canonical.dir.exists() {
            return Err(CoreError::new(
                CoreErrorKind::Usage,
                format!(
                    "server says you are {canonical_user_id} but {} already exists; log that account out first",
                    canonical.dir.display()
                ),
            ));
        }
        std::fs::create_dir_all(canonical.dir.parent().expect("account dir has a parent"))
            .map_err(|error| CoreError::new(CoreErrorKind::Io, format!("{error}")))?;
        std::fs::rename(&account.dir, &canonical.dir)
            .map_err(|error| CoreError::new(CoreErrorKind::Io, format!("{error}")))?;
        Ok(canonical)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("wbf-core-login-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// account-session.md §3：探到 wbf → 標準 HTTP `/login`、🚫 不建 Client、`m/` 只有 crypto store、session 記著 `WbfSdk`。
    /// 探活對真 server 要 WS，這裡直接把探測結果記進註冊表；`/login` 由本機一個回 200 的迷你 HTTP 扮。
    #[tokio::test]
    async fn logging_in_to_a_wbf_server_builds_no_matrix_client() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let server = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = vec![0u8; 8192];
            let read = socket.read(&mut request).await.unwrap_or(0);
            let head = String::from_utf8_lossy(&request[..read]).to_string();
            assert!(
                head.starts_with("POST /_matrix/client/v3/login "),
                "wbf 帳號的登入是標準 HTTP /login：{head}"
            );
            let body = r#"{"user_id":"@a:local","device_id":"DEVWBF","access_token":"syt_wbf"}"#;
            let _ = socket
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await;
        });
        let dir = scratch("wbf-login");
        let core = Core::open(&dir);
        core.create_vault(None).unwrap();
        core.set_remembered_backend(&server, BackendKind::WbfSdk);

        let result = core
            .log_in(&server, "@a:local", "pw", "test device", true)
            .await
            .expect("login over the mini /login");
        assert_eq!(result.device_id, "DEVWBF");

        let account = AccountDir::locate(
            &dir,
            &core.vault().unwrap().account_dir_key(),
            &server,
            "@a:local",
        )
        .unwrap();
        let session = core.session_of(&account).unwrap();
        assert_eq!(session.backend, Some(SessionBackend::WbfSdk));
        assert_eq!(session.store_dir, None, "沒有 Client 就沒有它的 store 目錄");
        let store = account.matrix_store_dir();
        assert!(
            store.join("matrix-sdk-crypto.sqlite3").exists(),
            "m/ 有 OlmEngine 開的 crypto store：{}",
            store.display()
        );
        assert!(
            !store.join("matrix-sdk-state.sqlite3").exists(),
            "🚫 沒有 matrix-sdk Client 的 state store"
        );
        // 之後不再探：探測結果忘掉也還是 wbf（答案在 session 裡）。
        core.forget_backend_probe(&server);
        assert_eq!(
            core.get_backend_kind(&account).await,
            BackendKind::WbfSdk,
            "wbf 帳號的 backend 登入時就定了，🚫 不靠每次重探"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn creating_a_vault_is_the_callers_passphrase_decision() {
        let dir = scratch("create");
        let core = Core::open(&dir);
        // 🚫 core 不問要不要 passphrase——`None` 就是 plain。
        assert_eq!(core.create_vault(None).unwrap(), KeyMode::Plain);
        assert!(core.is_unlocked(), "建完就是解開的");
        // 🚫 已經有一把就拒絕：覆蓋等於把既有的帳號全鎖在外面。
        assert_eq!(
            core.create_vault(None).unwrap_err().kind,
            CoreErrorKind::Usage
        );

        let with_passphrase = scratch("create-pp");
        let core = Core::open(&with_passphrase);
        assert_eq!(
            core.create_vault(Some(b"hunter2")).unwrap(),
            KeyMode::Passphrase
        );
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&with_passphrase);
    }
}

//! 「該開的線都開著嗎」的鉤子（維護者 2026-09-29，link-pool.md §3.1）：vault 解鎖了，就一個一個帳號看它的五條線，
//! 沒開的、死了的，對那一類重開一條。訂閱線（`Rooms`／`Keys`）開的時候 `init_connection` 就訂了，所以「開著」＝「在收」。
//!
//! 誰叫它（daemon）：`vault.unlock` 與 `account.add` 成功之後馬上叫一次（背景跑，🚫 不擋那個 RPC 的回應）；
//! 常駐的時候還有一個背景迴圈定時叫（`wbf-daemon` 的 `Handle::keep_links_open`），被關掉的線就是在這裡被重開的。
//! 這裡只管「看一次、補一次」，多久看一次、關機時停是 daemon 的事。

use std::sync::atomic::{AtomicBool, Ordering};

use wbf_sdk::login::SessionBackend;

use crate::link_pool::LinkRole;
use crate::Core;

/// 一次鉤子做了什麼（給測試與 log；daemon 不看它——開關線各自發了 `link.state`，開不起來的發了 `Note`）。
#[derive(Debug, Default, PartialEq, Eq)]
pub struct EnsuredLinks {
    /// 這次開的（本來沒開、或死了）：(帳號, 角色)
    pub opened: Vec<(String, LinkRole)>,
    /// 開不起來的：(帳號, 角色, 為什麼)
    pub failed: Vec<(String, LinkRole, String)>,
    /// true ＝ 有登入／登出／摧毀正在進行，這一輪整個跳過（什麼都沒看）
    pub skipped_busy: bool,
    /// true ＝ 已經有一輪在跑（例如背景迴圈那輪還沒跑完、又來一次 `vault.unlock`），這次跳過
    pub skipped_already_running: bool,
}

/// 「正在跑一輪」：拿到的那一輪握著它，丟掉就放——跑完、提前 return、future 被丟掉都一樣。
struct RoundInProgress<'a>(&'a AtomicBool);

impl Drop for RoundInProgress<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

impl Core {
    /// 每個登入的 wbf 帳號的五條線都確保開著：開著的不動（有命令在用的也算開著）、沒開或死了的開一條。
    /// 一條開不起來不擋其他條、其他帳號：講一聲（`Note`）、記下來、繼續。
    ///
    /// 跳過的：vault 還鎖著（什麼都不做）、已經有一輪在跑、有登入／登出／摧毀正在進行（整輪跳過）、沒登入的帳號、登出中的帳號（封池）、
    /// 不是 wbf 的帳號——判準是登入時記下的 `session.backend == Some(WbfSdk)`（跟 `init_keys`／`olm_engine_of` 同一條），
    /// 🚫 這裡不探 server（維護者 2026-09-29 選的，PR #61 審查 cirno 🟡1：探測沒有逾時、失敗不記，每 15 秒一輪會對一般 Matrix 帳號一直敲門）。
    /// 沒記 backend 的舊 session（PR #56 之前登入的）因此不會自動開線，重新登入一次就有。
    ///
    /// Return:
    ///     EnsuredLinks  opened 空＋failed 空 ＝ 本來就全開著，或沒有該開的；skipped_busy／skipped_already_running ＝ 這輪沒看
    pub async fn ensure_links(&self) -> EnsuredLinks {
        let mut ensured = EnsuredLinks::default();
        if !self.is_unlocked() {
            return ensured;
        }
        if self
            .ensuring_links
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            ensured.skipped_already_running = true;
            return ensured;
        }
        let _round = RoundInProgress(&self.ensuring_links);
        // 登入／登出／摧毀全程握著資料目錄的帳號生命週期鎖：拿不到（或鎖檔開不了）就整輪跳過，下一輪再看。
        // 拿到就馬上放：🚫 不握著它開線——開線可能要幾秒（server 不在時更久），握著會把使用者的登入／登出擋成 AccountBusy。
        // 之後才開始的登出由封池擋（`pool_of_account`）；之後才開始的登入會自己 `close_links` 舊 session 的線，下一輪用新 session 重開。
        if crate::account_lock::lock_account_lifecycle(&self.data_dir).is_err() {
            ensured.skipped_busy = true;
            return ensured;
        }
        let accounts = match self.refresh_data_dir_map() {
            Ok(map) => map.list_account_dirs(),
            Err(error) => {
                self.events
                    .progress(format!("links: could not list the accounts: {error}"));
                return ensured;
            }
        };
        for account in accounts.iter().filter(|account| account.is_logged_in()) {
            let Ok(session) = self.session_of(account) else {
                continue;
            };
            if session.backend != Some(SessionBackend::WbfSdk) {
                continue;
            }
            let user = session.user_id;
            // 登出中（封池）：不替它開。
            let pool = match self.pool_of_account(account) {
                Ok(pool) => pool,
                Err(error) => {
                    self.events
                        .progress(format!("links: not opening lines for {user}: {error}"));
                    continue;
                }
            };
            for role in LinkRole::ALL {
                match pool
                    .ensure_open(role, || self.open_link(account, role))
                    .await
                {
                    Ok(true) => ensured.opened.push((user.clone(), role)),
                    Ok(false) => {}
                    Err(error) => {
                        self.events.progress(format!(
                            "links: could not open the {} line for {user}: {error}",
                            role.name()
                        ));
                        ensured.failed.push((user.clone(), role, error.to_string()));
                    }
                }
            }
        }
        ensured
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use serde_json::Value;
    use wbf_sdk::login::Session;

    use super::*;
    use crate::event::LinkState;
    use crate::test_support::*;
    use crate::CoreEvent;

    fn roles_of(failed: &[(String, LinkRole, String)]) -> Vec<LinkRole> {
        failed.iter().map(|(_, role, _)| *role).collect()
    }

    /// 還沒解鎖：什麼都不看、什麼都不開。
    #[tokio::test]
    async fn a_locked_vault_opens_nothing() {
        let dir = scratch("keeper-locked");
        wbf_sdk::vault::Vault::create(&dir, &wbf_sdk::Unlock::NoPassphrase).unwrap();
        let core = Core::open(&dir);
        assert_eq!(core.ensure_links().await, EnsuredLinks::default());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 登入的 wbf 帳號、一條線都沒開：五條都試、照 `LinkRole::ALL` 的順序；server 不在，五條都開不起來、每條講一聲。
    #[tokio::test]
    async fn every_line_of_a_logged_in_wbf_account_is_tried_and_failures_are_reported() {
        let dir = scratch("keeper-all");
        let (core, _account) = core_with_wbf_account(&dir).await;
        let mut seen = core.subscribe();
        let ensured = core.ensure_links().await;
        assert!(ensured.opened.is_empty());
        assert_eq!(roles_of(&ensured.failed), LinkRole::ALL.to_vec());
        assert!(ensured.failed.iter().all(|(user, _, _)| user == ME));
        let notes = std::iter::from_fn(|| seen.try_recv().ok())
            .filter(|event| {
                matches!(event, CoreEvent::Note { text, .. } if text.starts_with("links: could not open the"))
            })
            .count();
        assert_eq!(notes, 5, "開不起來的每條都講一聲");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 已經有一輪在跑（背景迴圈那輪還沒完、又來一次 `vault.unlock`）：這次跳過、一條都不碰，🚫 不疊第二輪；
    /// 那輪跑完（旗子放掉）之後照常。上面那條測試跑完一整輪之後旗子也放了——下一次叫得動就是證明。
    #[tokio::test]
    async fn a_round_already_running_skips_the_second_one() {
        let dir = scratch("keeper-running");
        let (core, _account) = core_with_wbf_account(&dir).await;
        core.ensuring_links
            .store(true, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(
            core.ensure_links().await,
            EnsuredLinks {
                skipped_already_running: true,
                ..EnsuredLinks::default()
            }
        );
        core.ensuring_links
            .store(false, std::sync::atomic::Ordering::SeqCst);
        let first = core.ensure_links().await;
        assert_eq!(roles_of(&first.failed), LinkRole::ALL.to_vec());
        let second = core.ensure_links().await;
        assert!(!second.skipped_already_running, "跑完就放旗子");
        assert_eq!(roles_of(&second.failed), LinkRole::ALL.to_vec());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 有登入／登出／摧毀正在進行（生命週期鎖在別人手上）：整輪跳過、一條都不碰；放手之後下一輪照常。
    #[tokio::test]
    async fn a_login_or_logout_in_progress_skips_the_whole_round() {
        let dir = scratch("keeper-busy");
        let (core, _account) = core_with_wbf_account(&dir).await;
        let in_progress = crate::account_lock::lock_account_lifecycle(&dir).unwrap();
        assert_eq!(
            core.ensure_links().await,
            EnsuredLinks {
                skipped_busy: true,
                ..EnsuredLinks::default()
            }
        );
        drop(in_progress);
        let ensured = core.ensure_links().await;
        assert!(!ensured.skipped_busy);
        assert_eq!(
            roles_of(&ensured.failed),
            LinkRole::ALL.to_vec(),
            "放手之後照常看"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 開著的那條不動（🚫 不重開、🚫 不發事件）；死了的那條重開（池發 `closed`，再試著開）。
    #[tokio::test]
    async fn an_open_line_is_left_alone_and_a_dead_one_is_reopened() {
        let dir = scratch("keeper-mixed");
        let (core, account) = core_with_wbf_account(&dir).await;
        let pool = core.pool_of_account(&account).unwrap();
        let events: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
        let (alive, fake) = memory_client_with_hello(events.clone()).await;
        drop(
            pool.acquire(LinkRole::Misc, || async move { Ok(alive) })
                .await
                .unwrap(),
        );
        let (dying, dying_fake) = memory_client_with_hello(events).await;
        drop(
            pool.acquire(LinkRole::Upload, || async move { Ok(dying) })
                .await
                .unwrap(),
        );
        dying_fake.task.abort();
        wait_for_async(
            || async { pool.open_count() == 1 },
            "the upload line is seen dead",
        )
        .await;
        let mut seen = core.subscribe();

        let ensured = core.ensure_links().await;
        assert_eq!(
            roles_of(&ensured.failed),
            vec![
                LinkRole::Upload,
                LinkRole::Download,
                LinkRole::Rooms,
                LinkRole::Keys
            ],
            "Misc 開著就不動；死掉的 Upload 跟沒開的三條一樣重開（server 不在，所以開不起來）"
        );
        let upload_closed = std::iter::from_fn(|| seen.try_recv().ok()).any(|event| {
            matches!(
                event,
                CoreEvent::Link {
                    role: LinkRole::Upload,
                    state: LinkState::Closed,
                    ..
                }
            )
        });
        assert!(upload_closed, "死掉的那條先發 closed 再重開");
        assert_eq!(pool.open_count(), 1, "Misc 還是那一條");
        fake.task.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 不是 wbf 的帳號（session 沒記 `backend: WbfSdk`：一般 Matrix、或 PR #56 之前登的舊 session）與沒登入的帳號：一條都不開，
    /// 而且🚫 不去探 server（維護者 2026-09-29 選的 (a)：判準只看 session，探測沒有逾時、每 15 秒一輪會一直敲門）。
    #[tokio::test]
    async fn plain_matrix_and_logged_out_accounts_are_skipped_without_probing() {
        let dir = scratch("keeper-skip");
        let (core, account) = core_with_wbf_account(&dir).await;
        core.vault()
            .unwrap()
            .seal_session(
                &account.session_path(),
                &Session {
                    server: DEAD.to_string(),
                    user_id: ME.to_string(),
                    device_id: "DEV".to_string(),
                    access_token: "syt_memory".to_string(),
                    store_dir: None,
                    backend: None,
                },
            )
            .unwrap();
        assert_eq!(
            core.ensure_links().await,
            EnsuredLinks::default(),
            "沒記 backend 的 session：🚫 不開 WS"
        );
        assert_eq!(core.count_probe_cells(), 0, "🚫 不探 server");

        std::fs::remove_file(account.session_path()).unwrap();
        assert_eq!(
            core.ensure_links().await,
            EnsuredLinks::default(),
            "沒登入：沒有 token 可開線"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

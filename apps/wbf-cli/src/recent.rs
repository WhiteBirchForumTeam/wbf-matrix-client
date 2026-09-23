//! `recent`：`Event/Recent` 一窗一窗把 `cg_seq` 之後的事件跨房間拉回來寫進 `cache.db`
//! （local-cache-db.md §6「開 app 的同步」、server 的 pack-pipeline §6）。
//!
//! ⚠️ 做什麼在 `wbf_core::Core::recent`；這裡只是把結果印出來。

use wbf_core::CoreError;

use crate::commands::Context;

/// Args:
///     plan: 總量／一窗幾則／一批幾則, example: RecentPlan { max_events: Some(10000), window: 320, batch: None }
///     from_scratch: true 就不帶 `cg_seq`（把快取水位當沒有），server 從最新往回給
pub async fn recent_command(
    context: &Context,
    plan: wbf_sdk::RecentPlan,
    from_scratch: bool,
) -> Result<(), CoreError> {
    let summary = context
        .core()?
        .recent(
            plan,
            // CLI 沒有 `--since`：起點是存的水位（rpc-spec §3.4 的 `since` 給 UI 用）。
            None,
            from_scratch,
            context.transport,
            crate::commands::CLIENT_NAME,
            &context.target(),
        )
        .await?;
    crate::rooms::print_json(&serde_json::to_value(summary).expect("serializes"))
}

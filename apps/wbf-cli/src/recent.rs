//! `recent`：`Event/Recent` 一窗一窗把 `cg_seq` 之後的事件跨房間拉回來寫進 `cache.db`（local-cache-db.md §6「開 app 的同步」、
//! server 的 pack-pipeline §6）。這是快取的主要進料口；`read`／`files`／`watch` 只是順手寫穿。

use serde_json::json;
use wbf_sdk::event_json::messages_from_json;
use wbf_sdk::SdkError;

use crate::commands::Context;

/// Args:
///     plan: 總量／一窗幾則／一批幾則，example: RecentPlan { max_events: Some(10000), window: 320, batch: None }
///     from_scratch: true 就不帶 cg_seq（把快取水位線當沒有），server 從最新往回給
pub async fn recent_command(
    context: &Context,
    plan: wbf_sdk::RecentPlan,
    from_scratch: bool,
) -> Result<(), SdkError> {
    let (mut cache, me) = context.cache().await?;
    let mut client = context.client().await?;
    client.hello(crate::commands::CLIENT_NAME).await?;
    let cg_seq = if from_scratch {
        None
    } else {
        cache.get_cg_seq(&me)?
    };
    let mut pulled = 0usize;
    let mut written = 0usize;
    let mut batches = 0u32;
    let mut skipped_without_room = 0usize;
    let mut on_batch = |meta: &wbf_sdk::protocol::BatchMeta,
                        raws: Vec<serde_json::Value>|
     -> Result<(), SdkError> {
        batches += 1;
        pulled += raws.len();
        // Recent 的事件自帶 room_id（server 的 room-seq-and-recent.md §2）。沒帶的不猜、不寫（會落到 room "unknown"，
        // 之後查不到，PR #13 審查 salvia 🟢2）。一個 Batch 一起 aggregate、寫一次 DB。
        let (with_room, without_room): (Vec<_>, Vec<_>) = raws.into_iter().partition(|raw| {
            raw.get("room_id")
                .and_then(|value| value.as_str())
                .is_some()
        });
        skipped_without_room += without_room.len();
        let messages = messages_from_json("unknown", &with_room);
        written += cache.upsert_messages(&me, &messages)?;
        context.progress(format!(
            "recent: batch {batches}: {} events (window {}, {} left, g_seq {}..{})",
            meta.bc, meta.tc, meta.r, meta.fs, meta.ls
        ));
        Ok(())
    };
    // 中途斷線或 server 回錯：已寫進快取的有效、水位不動（pack-pipeline §6.4）；下次再跑會從水位重來。
    let summary = client.recent_sync(cg_seq, plan, &mut on_batch).await?;
    if skipped_without_room > 0 {
        context.progress(format!(
            "recent: skipped {skipped_without_room} event(s) without room_id"
        ));
    }
    if let Some(new_cg_seq) = summary.new_cg_seq {
        cache.set_cg_seq(&me, new_cg_seq)?;
    }
    if !summary.caught_up {
        context.progress(format!(
            "recent: stopped at the {} event limit; older events (below g_seq {:?}) are not in the cache yet",
            summary.events, summary.last_ls
        ));
    }
    crate::rooms::print_json(&json!({
        "pulled": pulled, "written": written, "windows": summary.windows, "batches": batches,
        "caught_up": summary.caught_up,
        "cg_seq_before": cg_seq, "cg_seq_after": summary.new_cg_seq.or(cg_seq),
    }))
}

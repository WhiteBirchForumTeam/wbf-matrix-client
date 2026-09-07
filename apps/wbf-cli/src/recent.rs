//! `recent`：`Event/Recent` 把 `cg_seq` 之後的事件跨房間拉回來寫進 `cache.db`（local-cache-db.md §6「開 app 的同步」、
//! chat-model §4.3）。這是快取的主要進料口；`read`／`files`／`watch` 只是順手寫穿。

use serde_json::json;
use wbf_sdk::event_json::messages_from_json;
use wbf_sdk::protocol::RecentRequest;
use wbf_sdk::SdkError;

use crate::commands::Context;

/// Args:
///     limit: 一次最多要幾則，example: 10000
///     from_scratch: true 就不帶 cg_seq（把快取水位線當沒有），server 從最新往回給
pub async fn recent_command(
    context: &Context,
    limit: u32,
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
    let mut request = RecentRequest {
        limit,
        cg_seq,
        before: None,
    };
    let mut pulled = 0usize;
    let mut written = 0usize;
    let mut rounds = 0u32;
    let latest_g_seq = loop {
        let (ack, raws) = client.recent(&request).await?;
        rounds += 1;
        pulled += raws.len();
        // Recent 的事件自帶 room_id；conversation 參數只在事件沒帶時用到。一頁一起 aggregate：關係事件折進同頁的目標。
        let messages = messages_from_json("unknown", &raws);
        written += cache.upsert_messages(&me, &messages)?;
        context.progress(format!(
            "recent: round {rounds}, {} events, complete {}, latest_g_seq {}",
            raws.len(),
            ack.complete,
            ack.latest_g_seq
        ));
        // 有洞：同一個 cg_seq，加 before = next 再問（server 的 room-seq-and-recent.md §2）。next 沒給就不再猜。
        if ack.complete || raws.is_empty() || ack.next.is_none() {
            break ack.latest_g_seq;
        }
        request.before = ack.next;
    };
    cache.set_cg_seq(&me, latest_g_seq)?;
    crate::rooms::print_json(&json!({
        "pulled": pulled, "written": written, "rounds": rounds,
        "cg_seq_before": cg_seq, "cg_seq_after": latest_g_seq,
    }))
}

//! Reading inbox messages posted while the bot wasn't listening.

use super::*;

/// Pages of missed inbox history read on start, 100 messages each.
pub(super) const CATCH_UP_PAGES: usize = 50;
/// Catch-up starts this long before the newest saved message, in case one
/// just before it wasn't saved (seconds).
pub(super) const CATCH_UP_OVERLAP: u64 = 600;
/// Discord's epoch, in Unix milliseconds.
pub(super) const DISCORD_EPOCH_MS: u64 = 1_420_070_400_000;

/// The first snowflake of a moment (Unix seconds).
pub(super) fn snowflake_at(ts: i64) -> u64 {
    ((ts.max(0) as u64 * 1000).saturating_sub(DISCORD_EPOCH_MS)) << 22
}

/// When a message was posted (Unix seconds), from its id. Discord sets it,
/// so it stays the same when the message is read back later.
pub(super) fn posted_at(msg: u64) -> i64 {
    (((msg >> 22) + DISCORD_EPOCH_MS) / 1000) as i64
}

/// Save inbox messages posted while we weren't listening. Messages edited
/// since they were posted are ignored: only the webhook's holder can edit
/// them, and what they say now isn't what was sent.
pub(super) async fn catch_up(ctx: &Ctx) {
    let Some(channel) = inbox_channel(ctx) else { return };
    let fed = ctx.store.fed(&ctx.gkey);
    // Anything older would be rejected as too old anyway.
    let floor = snowflake_at(now() - proto::MAX_SKEW);
    let overlap = (CATCH_UP_OVERLAP * 1000) << 22;
    let mut after = match fed.inbox_last().await {
        Ok(last) => last.map_or(0, |l| l.saturating_sub(overlap)).max(floor),
        Err(e) => return tracing::warn!(?e, "reading the partner inbox failed; not catching up"),
    };
    let mut edited = 0;
    for _ in 0..CATCH_UP_PAGES {
        let Some(id) = Id::<twilight_model::id::marker::MessageMarker>::new_checked(after) else { break };
        let page = match ctx.get(&format!("channels/{channel}/messages?after={id}&limit=100")).await {
            Ok(v) => v.as_array().cloned().unwrap_or_default(),
            Err(e) => {
                tracing::warn!(?e, "reading missed partner messages failed");
                break;
            }
        };
        let mut page: Vec<(u64, serde_json::Value)> =
            page.into_iter().filter_map(|m| Some((chipsoc_discord::wire::id(&m["id"])?.0, m))).collect();
        page.sort_by_key(|(id, _)| *id);
        let full = page.len() == 100;
        for (id, m) in page {
            after = after.max(id);
            let content = m["content"].as_str().unwrap_or_default();
            if !proto::is_envelope(content) {
                continue;
            }
            if !m["edited_timestamp"].is_null() {
                tracing::warn!(msg = id, "a partner inbox message was edited; ignored");
                edited += 1;
                continue;
            }
            ctx.log_err(fed.inbox_add(id, content, now()).await, "saving a partner message");
        }
        if !full {
            break;
        }
    }
    if edited > 0 {
        let text = format!(
            "⚠️ **{edited} partner message(s) in the inbox were edited after they were posted**, so the bot ignored them. Only someone with the inbox webhook can do that: if it wasn't you, make a new webhook and send it to your partners."
        );
        ctx.log_bot(&text).await;
    }
}

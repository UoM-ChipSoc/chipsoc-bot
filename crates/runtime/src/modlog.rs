//! The people log: edits and deletes (with the text, kept for `log.keep_days`),
//! joins and leaves. Text is shown only from channels everyone who can read
//! the people log can read anyway; from others, just who and where.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use chipsoc_config::{Overwrite, Snapshot};
use chipsoc_store::Archived;
use twilight_model::channel::Message;
use twilight_model::id::Id;
use twilight_model::id::marker::{ChannelMarker, MessageMarker};
use twilight_model::user::User;

use crate::{Ctx, now, ui};

fn is_log(ctx: &Ctx, channel: Id<ChannelMarker>) -> bool {
    let cfg = ctx.cfg();
    [&cfg.bot.log.people, &cfg.bot.log.bot].into_iter().flatten().any(|k| ctx.channel(k) == Some(channel))
}

/// Who can see a channel, by its config: `None` for everyone, else the
/// overwrite targets let in (role keys, and member ids as `@id`). Unknown
/// channels let in nobody we know of.
fn audience(snap: &Snapshot, key: &str) -> Option<BTreeSet<String>> {
    if chipsoc_config::public_channels(snap).iter().any(|k| k == key) {
        return None;
    }
    let Some((ch, cat)) = snap.channels().find(|(c, _)| c.key == key) else { return Some(BTreeSet::new()) };
    let lets_in = |o: &Overwrite| o.allow.iter().any(|p| p == "VIEW_CHANNEL");
    let who = |o: &Overwrite| o.role.clone().or_else(|| o.member.map(|m| format!("@{m}")));
    Some(chipsoc_config::effective_overwrites(cat, ch).iter().filter(|o| lets_in(o)).filter_map(who).collect())
}

/// Who a channel's overwrites keep out, other than @everyone: role keys, and
/// member ids as `@id`.
fn denied(snap: &Snapshot, key: &str) -> BTreeSet<String> {
    let Some((ch, cat)) = snap.channels().find(|(c, _)| c.key == key) else { return BTreeSet::new() };
    let keeps_out = |o: &Overwrite| o.deny.iter().any(|p| p == "VIEW_CHANNEL") && o.role.as_deref() != Some("everyone");
    let who = |o: &Overwrite| o.role.clone().or_else(|| o.member.map(|m| format!("@{m}")));
    chipsoc_config::effective_overwrites(cat, ch).iter().filter(|o| keeps_out(o)).filter_map(who).collect()
}

/// Whether everyone who can read the people log can already read `key`. A
/// channel that keeps out a role or a member (who might read the log) is never
/// quoted, unless it's a member the log keeps out too.
fn log_may_quote(snap: &Snapshot, log: &str, key: &str) -> bool {
    let log_denies = denied(snap, log);
    let shut_out = denied(snap, key);
    if shut_out.iter().any(|who| !(who.starts_with('@') && log_denies.contains(who))) {
        return false;
    }
    match (audience(snap, log), audience(snap, key)) {
        (_, None) => true,
        (None, Some(_)) => false,
        (Some(readers), Some(allowed)) => readers.is_subset(&allowed),
    }
}

/// [`log_may_quote`] for a channel or thread by id; threads count as their
/// channel, unless they're private.
async fn may_quote(ctx: &Ctx, channel: Id<ChannelMarker>) -> bool {
    /// Discord's channel type for a private thread.
    const PRIVATE_THREAD: u64 = 12;
    let cfg = ctx.cfg();
    let Some(log) = cfg.bot.log.people.as_deref() else { return false };
    let lock = &cfg.desired.lock;
    let key = match lock.channel_key(chipsoc_config::Snowflake(channel.get())) {
        Some(k) => k.to_owned(),
        None => {
            let Ok(thread) = ctx.get(&format!("channels/{channel}")).await else { return false };
            let parent = chipsoc_discord::wire::id(&thread["parent_id"]).and_then(|p| lock.channel_key(p));
            match parent {
                Some(k) if thread["type"].as_u64() != Some(PRIVATE_THREAD) => k.to_owned(),
                _ => return false,
            }
        }
    };
    log_may_quote(&cfg.desired, log, &key)
}

fn quote(s: &str) -> String {
    if s.is_empty() {
        return "> _(no text: an attachment or embed)_".into();
    }
    // Escaped: an edited message mustn't make bold "Accepted by" lines or masked links here.
    ui::quoted(&ui::truncate(s, 800))
}

pub async fn on_create(ctx: &Ctx, m: &Message) {
    if m.author.bot || ctx.cfg().bot.log.keep_days == 0 {
        return;
    }
    let a = Archived {
        id: m.id.to_string(),
        channel_id: m.channel_id.to_string(),
        author_id: m.author.id.to_string(),
        author_name: m.author.name.clone(),
        content: m.content.clone(),
        ts: m.timestamp.as_secs(),
    };
    ctx.log_err(ctx.db().archive_message(&a).await, "archiving message");
}

pub async fn on_update(ctx: &Ctx, m: &Message) {
    if m.author.bot || is_log(ctx, m.channel_id) {
        return;
    }
    let old = match ctx.db().archived(&m.id.to_string()).await {
        Ok(old) => old,
        Err(e) => {
            tracing::warn!(?e, "reading archived message failed");
            None
        }
    };
    if let Some(old) = old
        && old.content != m.content
    {
        let text = if may_quote(ctx, m.channel_id).await {
            format!(
                "✏️ **{}** edited a message in <#{}>\n**Before**\n{}\n**After**\n{}",
                ui::escape(&m.author.name),
                m.channel_id,
                quote(&old.content),
                quote(&m.content)
            )
        } else {
            format!(
                "✏️ **{}** edited a message in <#{}> (a channel this log doesn't show)",
                ui::escape(&m.author.name),
                m.channel_id
            )
        };
        ctx.log_people(&text).await;
    }
    on_create(ctx, m).await;
}

pub async fn on_delete(ctx: &Ctx, channel: Id<ChannelMarker>, id: Id<MessageMarker>) {
    if is_log(ctx, channel) {
        return;
    }
    match ctx.db().archived(&id.to_string()).await {
        Ok(Some(a)) => {
            let text = if may_quote(ctx, channel).await {
                format!(
                    "🗑️ Message by **{}** deleted in <#{channel}>\n{}",
                    ui::escape(&a.author_name),
                    quote(&a.content)
                )
            } else {
                format!(
                    "🗑️ Message by **{}** deleted in <#{channel}> (a channel this log doesn't show)",
                    ui::escape(&a.author_name)
                )
            };
            ctx.log_people(&text).await;
            ctx.log_err(ctx.db().mark_deleted(&id.to_string()).await, "marking message deleted");
        }
        Ok(None) => ctx.log_people(&format!("🗑️ A message (older than the log) was deleted in <#{channel}>")).await,
        Err(e) => tracing::warn!(?e, "reading archived message failed"),
    }
}

fn created(user: &User) -> i64 {
    ((user.id.get() >> 22) as i64 + 1_420_070_400_000) / 1000
}

pub async fn on_join(ctx: &Ctx, user: &User) {
    ctx.log_people(&joined(user)).await;
}

pub async fn on_leave(ctx: &Ctx, user: &User) {
    ctx.log_people(&left(user)).await;
}

/// The people log's lines for someone joining and leaving. Names are theirs
/// to choose, so shown as written: no bold or links that pass for the log's own.
fn joined(user: &User) -> String {
    let bot = if user.bot { " 🤖" } else { "" };
    format!("📥 **{}**{bot} (<@{}>) joined · account made <t:{}:R>", ui::escape(&user.name), user.id, created(user))
}

fn left(user: &User) -> String {
    format!("📤 **{}** (<@{}>) left", ui::escape(&user.name), user.id)
}

/// Forget message text older than `log.keep_days`, daily.
pub async fn retention_loop(ctx: Arc<Ctx>) {
    let mut tick = tokio::time::interval(Duration::from_secs(24 * 3600));
    loop {
        tick.tick().await;
        let days = i64::from(ctx.cfg().bot.log.keep_days);
        match ctx.db().prune_messages(now() - days * 24 * 3600).await {
            Ok(n) if n > 0 => tracing::info!(n, "forgot old message text"),
            Ok(_) => {}
            Err(e) => tracing::warn!(?e, "pruning messages failed"),
        }
    }
}

#[cfg(test)]
mod tests {
    use chipsoc_config::{Category, CategoryFile, Channel};

    use super::*;

    #[test]
    fn quotes_cant_pass_for_the_bot_s_own_lines() {
        let q = quote("**✅ Accepted by <@1>.**\n[x](https://evil.example)");
        assert!(q.lines().all(|l| l.starts_with("> ")), "{q}");
        assert!(!q.contains("**") && !q.contains("](") && !q.contains("<@"), "escaped: {q}");
    }

    fn ow(role: &str, allow: &[&str], deny: &[&str]) -> Overwrite {
        Overwrite {
            role: Some(role.into()),
            allow: allow.iter().map(|s| s.to_string()).collect(),
            deny: deny.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    fn ch(key: &str, overwrites: Vec<Overwrite>) -> Channel {
        Channel { key: key.into(), name: key.into(), overwrites, ..Default::default() }
    }

    #[test]
    fn private_text_stays_out_of_a_wider_log() {
        let hidden = || ow("everyone", &[], &["VIEW_CHANNEL"]);
        let snap = Snapshot {
            categories: vec![CategoryFile {
                category: Category::default(),
                channels: vec![
                    ch("general", vec![]),
                    ch("mod-log", vec![hidden(), ow("mods", &["VIEW_CHANNEL"], &[])]),
                    ch("committee", vec![hidden(), ow("committee", &["VIEW_CHANNEL"], &[])]),
                    ch(
                        "staff",
                        vec![hidden(), ow("mods", &["VIEW_CHANNEL"], &[]), ow("committee", &["VIEW_CHANNEL"], &[])],
                    ),
                    ch("open-log", vec![]),
                ],
            }],
            ..Default::default()
        };
        assert!(log_may_quote(&snap, "mod-log", "general"), "public text is fine anywhere");
        assert!(!log_may_quote(&snap, "mod-log", "committee"), "mods can't read the committee's channel");
        assert!(log_may_quote(&snap, "mod-log", "staff"), "everyone reading the log reads staff");
        assert!(!log_may_quote(&snap, "open-log", "staff"), "a public log quotes no private channel");
        assert!(!log_may_quote(&snap, "mod-log", "nowhere"), "unknown channels aren't quoted");
    }

    #[test]
    fn a_channel_that_keeps_someone_out_isnt_quoted_to_them() {
        let member = |id: u64, deny: &[&str]| Overwrite {
            member: Some(chipsoc_config::Snowflake(id)),
            deny: deny.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        };
        let snap = Snapshot {
            categories: vec![CategoryFile {
                category: Category::default(),
                channels: vec![
                    ch("log", vec![]),
                    ch("chat", vec![member(123, &["VIEW_CHANNEL"])]),
                    ch("no-guests", vec![ow("guest", &[], &["VIEW_CHANNEL"])]),
                    ch("log-without-123", vec![member(123, &["VIEW_CHANNEL"])]),
                ],
            }],
            ..Default::default()
        };
        assert!(!log_may_quote(&snap, "log", "chat"), "123 reads the log but not chat");
        assert!(!log_may_quote(&snap, "log", "no-guests"), "guests read the log");
        assert!(log_may_quote(&snap, "log-without-123", "chat"), "123 can't read that log either");
    }

    /// R3-L1: a name can't fake a second log line or a masked link.
    #[test]
    fn names_are_shown_as_written() {
        let user: User = serde_json::from_value(serde_json::json!({
            "id": "7", "username": "X** left · **[Y](https://evil.example)", "discriminator": "0", "avatar": null,
        }))
        .unwrap();
        let line = left(&user);
        assert!(line.starts_with("📤 **X\\*\\* left · \\*\\*\\[Y\\]\\("), "{line}");
        assert!(!joined(&user).contains("**[Y]"), "{}", joined(&user));
    }
}

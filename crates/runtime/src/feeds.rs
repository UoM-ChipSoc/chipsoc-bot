//! Feeds: members suggest an RSS/Atom feed (or a website, whose feed is found
//! from its page), an admin approves it, and new items appear in the feeds
//! channel (one post each if it's a forum).

use std::sync::Arc;
use std::time::Duration;

use chipsoc_admin::feeds::{fetch, find, title};
use chipsoc_admin::{Answer, fmt};
use chipsoc_discord::Method;
use chipsoc_store::AppKind;
use twilight_model::channel::message::Component;
use twilight_model::channel::message::component::TextInputStyle;

use crate::review::{Filing, Refusal, Vetted};
use crate::ui::Values;
use crate::{Ctx, ui};

const POLL_EVERY: Duration = Duration::from_secs(15 * 60);

/// Suggestions need the feeds channel and the review queue.
pub fn ready(ctx: &Ctx) -> Result<(), String> {
    let cfg = ctx.cfg();
    if cfg.bot.feeds.is_none() || cfg.bot.review.is_none() {
        return Err("Feed suggestions aren't set up here.".into());
    }
    Ok(())
}

pub fn form() -> (String, Vec<Component>) {
    let fields = vec![
        ui::input("url", "Feed or website address", TextInputStyle::Short, true, Some("https://…")),
        ui::input("why", "What is it, and why follow it?", TextInputStyle::Paragraph, true, None),
    ];
    ("Suggest a feed".into(), fields)
}

/// The check that costs nothing: a public web address.
pub fn precheck(v: &Values) -> Result<(), Refusal> {
    chipsoc_admin::feeds::check_address(&v.text("url")).map_err(|why| {
        Refusal::fix(format!("{} **Couldn't read that feed:** {}", fmt::ATTENTION, fmt::error_str(&why)))
    })
}

/// A suggested feed: fetched to check it's real (a website's feed is found
/// from its page). Fetching costs, so past [`precheck`] the turn counts.
pub async fn vet(ctx: &Ctx, v: &Values) -> Result<Vetted, Refusal> {
    let given = v.text("url");
    let (url, feed) = match find(&given).await {
        Ok(found) => found,
        Err(e) => {
            let text = format!("{} **Couldn't read that feed:** {}", fmt::ATTENTION, fmt::error_str(&e));
            return Err(Refusal::fix(text));
        }
    };
    if let Some(why) = known(ctx, &url).await {
        return Err(Refusal::fix(why));
    }
    let title = title(&feed, &url);
    let latest = feed.entries.first().map(raw_line);
    let mut answers = vec![Answer::new("Address", Some(&url))];
    if url != given {
        answers.push(Answer::new("Found from", Some(&given)));
    }
    answers.extend([Answer::new("Why", Some(&v.text("why"))), Answer::new("Latest item", latest.as_ref())]);
    let title = ui::escape(&title);
    let thanks = format!("Thanks! **{title}** is with the admins for a look. You'll get a DM once it's reviewed.");
    Ok(Vetted::Review(Filing::new(
        AppKind::Feed,
        &url,
        format!("{} · {title}", AppKind::Feed.label()),
        answers,
        thanks,
    )))
}

/// Why a found feed needn't be suggested: it's followed already, or waiting for review.
async fn known(ctx: &Ctx, url: &str) -> Option<&'static str> {
    if ctx.db().feeds().await.is_ok_and(|f| f.iter().any(|f| f.url == url)) {
        return Some("That feed is followed already.");
    }
    let waiting = ctx.db().pending_applications().await.unwrap_or_default();
    waiting
        .iter()
        .any(|a| a.kind == AppKind::Feed && a.target == url)
        .then_some("Someone has already suggested that feed; it's waiting for an admin to look at it.")
}

/// A feed item as a line: its title and its address in full, never a masked
/// link. Not escaped yet: the feed's author wrote it.
fn raw_line(e: &feed_rs::model::Entry) -> String {
    let title = e.title.as_ref().map_or_else(|| "(untitled)".into(), |t| t.content.trim().replace('\n', " "));
    let title = ui::truncate(&title, 200);
    match e.links.first().map(|l| l.href.trim()).filter(|h| h.starts_with("https://") || h.starts_with("http://")) {
        Some(href) if !href.contains(char::is_whitespace) => format!("{title} {href}"),
        _ => title,
    }
}

/// Share a posted item with partners we send feeds to.
async fn share(ctx: &Ctx, feed: &str, name: &str, e: &feed_rs::model::Entry) {
    let origin = chipsoc_model::stable_hash(format!("{feed}\n{}", e.id));
    let title = e.title.as_ref().map_or_else(|| name.to_owned(), |t| t.content.trim().replace('\n', " "));
    let url = e.links.first().map(|l| l.href.trim()).filter(|h| h.starts_with("https://"));
    chipsoc_admin::shares::send_quietly(ctx, &chipsoc_admin::shares::feed_item(&origin, name, &title, url), None).await;
}

/// [`raw_line`], escaped, to post.
fn line(e: &feed_rs::model::Entry) -> String {
    ui::escape(&raw_line(e))
}

pub async fn poll_loop(ctx: Arc<Ctx>) {
    let mut tick = tokio::time::interval(POLL_EVERY);
    loop {
        tick.tick().await;
        let feeds = match ctx.db().feeds().await {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!(?e, "reading feeds failed");
                continue;
            }
        };
        for f in feeds.into_iter().filter(|f| !f.paused) {
            let feed = match fetch(&f.url).await {
                Ok(feed) => feed,
                Err(e) => {
                    tracing::warn!(url = f.url, e, "feed fetch failed");
                    continue;
                }
            };
            post_new(&ctx, &f, &feed).await;
        }
    }
}

/// Post what's new in `feed` (as just read; its newest 10, oldest first) in
/// `f`'s channel: a line each, or a post each in a forum. An item counts as
/// seen once it's posted; one that fails is tried again next time.
async fn post_new(ctx: &Ctx, f: &chipsoc_store::Feed, feed: &feed_rs::model::Feed) {
    let Some(channel) = ctx.channel(&f.channel_key) else { return };
    let name = title(feed, &f.url);
    let forum = ctx.cfg().desired.channel(&f.channel_key).is_some_and(|c| c.kind.has_tags());
    for entry in feed.entries.iter().take(10).rev() {
        if !ctx.db().mark_item(f.id, &entry.id).await.unwrap_or(false) {
            continue;
        }
        let posted = if forum {
            let item = entry.title.as_ref().map_or_else(|| name.clone(), |t| t.content.trim().to_owned());
            let body = format!("{} · from **{}**", line(entry), ui::escape(&name));
            let post = serde_json::json!({
                "name": ui::truncate(&item, 95),
                "message": { "content": ui::truncate(&body, ui::MAX_CONTENT) },
            });
            ctx.call(Method::Post, &format!("channels/{channel}/threads"), Some(post)).await.map(drop)
        } else {
            let text = format!("📰 **{}** · {}", ui::escape(&ui::truncate(&name, 80)), line(entry));
            ctx.post(channel, &text, &[]).await.map(drop)
        };
        if let Err(e) = posted {
            tracing::warn!(?e, url = f.url, "posting feed item failed");
            ctx.log_err(ctx.db().unmark_item(f.id, &entry.id).await, "unmarking feed item");
        } else {
            share(ctx, &f.url, &name, entry).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ATOM: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<feed xmlns="http://www.w3.org/2005/Atom">
  <title>Chip News</title>
  <id>urn:chip-news</id>
  <updated>2026-01-02T00:00:00Z</updated>
  <entry><title>Second *chip*</title><id>urn:2</id><updated>2026-01-02T00:00:00Z</updated>
    <link href="https://example.org/2"/></entry>
  <entry><title>First chip</title><id>urn:1</id><updated>2026-01-01T00:00:00Z</updated>
    <link href="https://example.org/1"/></entry>
</feed>"#;

    /// New items are posted oldest first, each once; one whose post fails
    /// is posted the next time round.
    #[tokio::test]
    async fn new_items_are_posted_once_and_a_failed_one_again() {
        let (_root, fake, ctx) = crate::tests::server().await;
        let ch = ctx.cfg().desired.lock.channel("events").unwrap();
        let id = ctx.db().add_feed("https://example.org/feed", "events", "7", 0).await.unwrap();
        let f = ctx.db().feed(id).await.unwrap().unwrap();
        let feed = feed_rs::parser::parse(ATOM.as_bytes()).unwrap();
        let texts = || -> Vec<String> {
            let st = fake.state.lock().unwrap();
            let here = st.messages.iter().filter(|m| m["channel_id"] == ch.to_string());
            here.map(|m| m["content"].as_str().unwrap_or_default().to_owned()).collect()
        };

        fake.fail(&format!("POST channels/{ch}/messages"), 500, false);
        post_new(&ctx, &f, &feed).await;
        assert_eq!(texts(), ["📰 **Chip News** · Second \\*chip\\* https://example.org/2"], "the first failed");

        post_new(&ctx, &f, &feed).await;
        post_new(&ctx, &f, &feed).await;
        let want = [
            "📰 **Chip News** · Second \\*chip\\* https://example.org/2",
            "📰 **Chip News** · First chip https://example.org/1",
        ];
        assert_eq!(texts(), want, "tried again once, nothing twice");
    }
}

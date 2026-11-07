//! Sharing content with partners, the sending side (receiving is in
//! `fedrecv`): what [`chipsoc_admin::shares`] queued goes out as signed
//! envelopes, in parts if its text is long, every 30 seconds or as soon as
//! something is queued; a partner that fails backs off on its own, and an
//! item that keeps failing is given up on (said in the bot log). Also here:
//! noticing questions asked in the help forum, and edits and take-downs of
//! what we shared, and `/server share` with its partner picker.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chipsoc_admin::shares::{self, Pick, Share, ShareKind};
use chipsoc_admin::{Actor, fmt};
use chipsoc_config::{Peer, Snowflake};
use chipsoc_fedproto::{self as proto, Body};
use chipsoc_store::OutShare;
use tokio::sync::Notify;
use twilight_model::application::interaction::Interaction;
use twilight_model::channel::Message;
use twilight_model::channel::message::component::Component;
use twilight_model::guild::scheduled_event::{GuildScheduledEvent, Status};

use crate::commands::CustomId;
use crate::fedrecv::Env;
use crate::{Ctx, fedsync, ui};

const SEND_EVERY: Duration = Duration::from_secs(30);
/// Failed sends of one item before it's given up on.
const MAX_TRIES: i64 = 10;

/// What's waiting to go out.
#[derive(Default)]
pub struct Outgoing {
    /// Something was queued: send now rather than at the next tick.
    pub(crate) ready: Notify,
    /// Partners whose last send failed: (failures in a row, don't retry before).
    backoff: std::sync::Mutex<HashMap<String, (u32, Instant)>>,
}

/// Send what's queued, every [`SEND_EVERY`] or when something is.
pub async fn send_loop(ctx: Arc<Ctx>) {
    loop {
        tokio::select! {
            _ = tokio::time::sleep(SEND_EVERY) => {}
            _ = ctx.fed.shares.ready.notified() => {}
        }
        if fedsync::enabled(&ctx) {
            flush(&*ctx).await;
        }
    }
}

/// Send each queued item to its partner, unless the partner is backing off.
pub(crate) async fn flush<E: Env>(e: &E) {
    let cfg = e.cfg();
    let from = cfg.federation.name.clone();
    let Some(sk) = e.state().key.clone() else { return };
    let fed = e.store().fed(e.gkey());
    let pending = match fed.out_pending().await {
        Ok(p) => p,
        Err(err) => return tracing::warn!(?err, "reading what to share failed"),
    };
    let mut failed_now = HashSet::new();
    for row in pending {
        let done = |r: &OutShare| {
            let fed = e.store().fed(e.gkey());
            let r = r.clone();
            async move {
                if let Err(err) = fed.out_done(&r).await {
                    tracing::warn!(?err, "marking a share sent failed");
                }
            }
        };
        let peer = cfg.federation.peers.iter().find(|p| p.id == row.peer && !p.id.is_empty());
        let what = ShareKind::parse(&row.what);
        let (Some(peer), Some(what)) = (peer, what) else {
            done(&row).await; // the partner is gone
            continue;
        };
        if !row.removed && !peer.sends(what.plural()) {
            done(&row).await; // we stopped sending them this
            continue;
        }
        if failed_now.contains(&peer.id) || backing_off(e, &peer.id) {
            continue;
        }
        if row.tries >= MAX_TRIES {
            let text = format!(
                "{} **Gave up sharing {} with {}** after {MAX_TRIES} tries. Check their webhook (`webhook_env`) and that their bot is up.",
                fmt::ATTENTION,
                shares::a_noun(what),
                peer.name
            );
            e.log_bot(&text).await;
            done(&row).await;
            continue;
        }
        let bodies = if row.removed {
            vec![Body::Unshare { what, origin: row.origin.clone() }]
        } else {
            let Ok(item) = serde_json::from_str::<Share>(&row.body) else {
                done(&row).await;
                continue;
            };
            // Two edits within a second are still two batches.
            let batch = chipsoc_model::stable_hash(format!("{}{}{}{}", row.origin, row.changed, peer.id, row.body));
            match parts(&sk, &from, &peer.id, item, &batch) {
                Some(bodies) => bodies,
                None => {
                    // Not the partner's fault: given up on at once, the rest go on.
                    let text = format!(
                        "{} Couldn't share {} with {}: it's too big to send even cut down.",
                        fmt::ATTENTION,
                        shares::a_noun(what),
                        peer.name
                    );
                    e.log_bot(&text).await;
                    done(&row).await;
                    continue;
                }
            }
        };
        if deliver_all(e, &from, peer, bodies).await {
            done(&row).await;
            e.state().shares.backoff.lock().unwrap_or_else(|p| p.into_inner()).remove(&peer.id);
        } else {
            failed_now.insert(peer.id.clone());
            if let Err(err) = fed.out_failed(&row).await {
                tracing::warn!(?err, "counting a failed share failed");
            }
            let mut b = e.state().shares.backoff.lock().unwrap_or_else(|p| p.into_inner());
            let failures = b.get(&peer.id).map_or(0, |(n, _)| *n) + 1;
            b.insert(peer.id.clone(), (failures, Instant::now() + fedsync::backoff(failures)));
        }
    }
}

pub(crate) fn backing_off<E: Env>(e: &E, peer: &str) -> bool {
    let b = e.state().shares.backoff.lock().unwrap_or_else(|p| p.into_inner());
    b.get(peer).is_some_and(|(_, until)| *until > Instant::now())
}

/// Seal and send every part, stopping at the first that fails.
async fn deliver_all<E: Env>(e: &E, from: &str, peer: &Peer, bodies: Vec<Body>) -> bool {
    for body in bodies {
        let Some((content, _)) = fedsync::seal_for(e.state(), from, peer, body) else { return false };
        if !e.deliver(peer, &content).await {
            return false;
        }
    }
    true
}

/// `item` as envelopes that each fit a message: whole if it fits, else its
/// text split over the fewest parts that do (each carrying the rest of
/// it), else its text cut, and its link (to a resource or feed item)
/// dropped if it still doesn't fit. None if nothing does.
fn parts(sk: &proto::SecretKey, from: &str, to: &str, item: Share, batch: &str) -> Option<Vec<Body>> {
    let fits = |b: &Body| fedsync::fits(sk, from, to, b.clone());
    let whole = Body::Share(item.clone());
    if fits(&whole) {
        return Some(vec![whole]);
    }
    let chars: Vec<char> = item.text.chars().collect();
    for n in 2..=proto::MAX_SHARE_PARTS as usize {
        let chunks: Vec<String> = chars.chunks(chars.len().div_ceil(n).max(1)).map(|c| c.iter().collect()).collect();
        let parts = chunks.len() as u64;
        let bodies: Vec<Body> = chunks
            .into_iter()
            .enumerate()
            .map(|(i, text)| {
                Body::Share(Share { text, batch: batch.to_owned(), part: i as u64, parts, ..item.clone() })
            })
            .collect();
        if bodies.iter().all(fits) {
            return Some(bodies);
        }
    }
    let mut cut = item;
    while !cut.text.is_empty() && !fits(&Body::Share(cut.clone())) {
        let keep = cut.text.chars().count() * 3 / 4;
        cut.text = cut.text.chars().take(keep).collect();
    }
    if !fits(&Body::Share(cut.clone())) {
        cut.url = None;
    }
    let cut = Body::Share(cut);
    fits(&cut).then(|| vec![cut])
}

// --- Noticing what changes here.

/// Whether any partner gets `what` from us.
fn anyone(ctx: &Ctx, what: ShareKind) -> bool {
    !shares::partners_for(&ctx.cfg(), what).is_empty()
}

/// A new message: if it starts a post in the help forum, share the question.
pub async fn on_message(ctx: &Ctx, m: &Message) {
    if m.author.bot || m.id.get() != m.channel_id.get() || !anyone(ctx, ShareKind::Question) {
        return;
    }
    let Some(help) = ctx.cfg().bot.help.as_ref().and_then(|h| ctx.channel(&h.forum)) else { return };
    let thread = match ctx.get(&format!("channels/{}", m.channel_id)).await {
        Ok(c) => c,
        Err(e) => return tracing::warn!(?e, "reading a new forum post failed"),
    };
    if chipsoc_discord::wire::id(&thread["parent_id"]).map(|p| p.0) != Some(help.get()) {
        return;
    }
    let by = m
        .member
        .as_ref()
        .and_then(|mm| mm.nick.clone())
        .or_else(|| m.author.global_name.clone())
        .unwrap_or_else(|| m.author.name.clone());
    let guild = Snowflake(ctx.guild.get());
    let item = shares::question(
        guild,
        Snowflake(m.id.get()),
        thread["name"].as_str().unwrap_or("A question"),
        &m.content,
        &by,
    );
    // Within the hourly caps, per member and in all, so one member's posts
    // can't fill partners' queues.
    shares::send_quietly(ctx, &item, Some(Snowflake(m.author.id.get()))).await;
}

/// A message was edited: if we shared it (a question or an announcement),
/// send partners the new text, if whoever shared it (or an admin) edited
/// it: what partners post of it says it's from us.
pub async fn on_update(ctx: &Ctx, m: &Message) {
    edited(ctx, m.id.get(), Snowflake(m.author.id.get()), &m.content).await;
}

/// [`on_update`]: message `id` now says `text`, as edited by `by`.
pub(crate) async fn edited<E: chipsoc_admin::Host>(e: &E, id: u64, by: Snowflake, text: &str) {
    for what in [ShareKind::Question, ShareKind::Announcement] {
        let origin = id.to_string();
        let Ok(Some(mut item)) = shares::shared(e, what, &origin).await else { continue };
        match shares::may_change(e, what, &origin, by).await {
            Ok(true) => {}
            Ok(false) => {
                tracing::info!(origin, "not sending partners an edit by someone who didn't share it");
                continue;
            }
            Err(err) => {
                tracing::warn!("checking who may change a shared message failed: {err:#}");
                continue;
            }
        }
        let was = item.clone();
        item.text = ui::truncate(text.trim(), shares::MAX_TEXT);
        if what == ShareKind::Announcement {
            item.title = shares::announcement_title(&item.text);
        }
        // An edit that changes nothing shared (Discord adding a link's preview, say) isn't sent.
        if item == was {
            continue;
        }
        if let Err(err) = shares::update(e, &item).await {
            tracing::warn!("sending partners an edit failed: {err:#}");
        }
    }
}

/// A message or post was deleted: tell partners who had it.
pub async fn on_delete(ctx: &Ctx, id: u64) {
    if ctx.cfg().federation.peers.is_empty() {
        return;
    }
    for what in [ShareKind::Question, ShareKind::Announcement, ShareKind::Resource] {
        if let Err(e) = shares::remove(ctx, what, &id.to_string()).await {
            tracing::warn!("telling partners something was deleted failed: {e:#}");
        }
    }
}

/// One of our events changed: partners who have it get the change, or
/// hear it's off if it was cancelled.
pub async fn on_event(ctx: &Ctx, e: &GuildScheduledEvent) {
    let origin = e.id.to_string();
    if !matches!(shares::shared(ctx, ShareKind::Event, &origin).await, Ok(Some(_))) {
        return;
    }
    if e.status == Status::Cancelled {
        return on_event_gone(ctx, e.id.get()).await;
    }
    let Some(item) = serde_json::to_value(e).ok().and_then(|v| shares::event_from(Snowflake(ctx.guild.get()), &v))
    else {
        return;
    };
    if let Err(err) = shares::update(ctx, &item).await {
        tracing::warn!("sending partners an event's change failed: {err:#}");
    }
}

pub async fn on_event_gone(ctx: &Ctx, id: u64) {
    if let Err(e) = shares::remove(ctx, ShareKind::Event, &id.to_string()).await {
        tracing::warn!("telling partners an event is off failed: {e:#}");
    }
}

// --- `/server share`.

/// A pick, as a custom id carries it.
fn encode(p: Pick) -> String {
    match p {
        Pick::Announcement { channel, message } => format!("a:{channel}:{message}"),
        Pick::Event(e) => format!("e:{e}"),
    }
}

fn decode(s: &str) -> Option<Pick> {
    let mut it = s.split(':');
    match (it.next()?, it.next(), it.next()) {
        ("a", Some(c), Some(m)) => Some(Pick::Announcement { channel: c.parse().ok()?, message: m.parse().ok()? }),
        ("e", Some(e), None) => Some(Pick::Event(e.parse().ok()?)),
        _ => None,
    }
}

/// The partners that may get `what`, to pick from: (name, id).
pub fn choices(ctx: &Ctx, what: ShareKind) -> Vec<(String, String)> {
    shares::partners_for(&ctx.cfg(), what).into_iter().map(|p| (p.name.clone(), p.id.clone())).collect()
}

/// A menu picking several partners.
fn picker(id: &str, options: &[(String, String)]) -> Component {
    let mut row = ui::menu(id, "Which partners?", options);
    if let Component::ActionRow(r) = &mut row
        && let Some(Component::SelectMenu(m)) = r.components.first_mut()
    {
        m.max_values = Some(u8::try_from(options.len().clamp(1, 25)).unwrap_or(25));
    }
    row
}

/// The partner picker for `pick`, if any partner takes its kind from us.
pub fn share_menu(ctx: &Ctx, pick: Pick) -> Option<Component> {
    let options = choices(ctx, pick.kind());
    (!options.is_empty()).then(|| picker(&CustomId::ShareTo(encode(pick)).to_string(), &options))
}

/// `/server share announcement|event`: ask which partners (those we send it to).
pub async fn command(ctx: &Ctx, i: &Interaction, what: &str, given: &str) {
    let pick = match Pick::parse(what, given) {
        Ok(p) => p,
        Err(why) => return ctx.respond(i, &ui::ephemeral(&why)).await,
    };
    let kind = pick.kind();
    let options = choices(ctx, kind);
    if options.is_empty() {
        let text = format!(
            "No partner takes {} from us yet: list `{}` in a peer's `send` in federation.toml.",
            kind.plural(),
            kind.plural()
        );
        return ctx.respond(i, &ui::ephemeral(&text)).await;
    }
    let Some(menu) = share_menu(ctx, pick) else { return };
    let text = format!(
        "Share this {} with which partners? Each posts it where they've chosen, after their own review (unless they've turned that off).",
        shares::noun(kind)
    );
    ctx.respond(i, &ui::ephemeral_with(&text, vec![menu])).await;
}

/// Partners picked under `/server share`: share it with them.
pub async fn picked(ctx: &Ctx, i: &Interaction, payload: &str, values: &[String]) {
    let by = crate::apps::invoker(i).map(|u| Actor { id: Some(Snowflake(u.id.get())), name: u.name.clone() });
    let Some(by) = by.filter(|_| ctx.is_admin(i.member.as_ref())) else {
        return ctx.respond(i, &ui::ephemeral(&crate::manage::admins_only(&ctx.cfg()))).await;
    };
    let Some(pick) = decode(payload) else { return };
    ctx.respond(i, &ui::defer_update()).await;
    let text = shared_text(shares::share_now(ctx, pick, values, &by).await);
    ctx.edit_response(i, &text, &[]).await;
}

/// What sharing said, for whoever asked.
pub fn shared_text(r: anyhow::Result<Vec<String>>) -> String {
    match r {
        Ok(names) if names.is_empty() => format!("{} Nobody to share it with.", fmt::ATTENTION),
        Ok(names) => format!(
            "{} **Shared with {}.** Each posts it after their own review (unless they've turned that off).",
            fmt::DONE,
            fmt::and_list(&names)
        ),
        Err(e) => match e.downcast_ref::<chipsoc_admin::Refused>() {
            Some(why) => format!("{} {why}", fmt::ATTENTION),
            None => {
                tracing::warn!("sharing with partners failed: {e:#}");
                format!("{} **Couldn't share it:** {}", fmt::ATTENTION, fmt::error(&e))
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_long_text_goes_in_parts_that_each_fit() {
        let sk = proto::signing_key(&proto::generate().unwrap().0).unwrap();
        let mut item = Share::new(ShareKind::Announcement, "1", "News");
        item.text = "Quite a lot to say, with \"quotes\" and\nnew lines. ".repeat(80);
        let bodies = parts(&sk, "us", "them", item.clone(), "b").unwrap();
        assert!(bodies.len() > 1 && bodies.len() as u64 <= proto::MAX_SHARE_PARTS, "{}", bodies.len());
        let mut text = String::new();
        for (n, b) in bodies.iter().enumerate() {
            let Body::Share(s) = b else { panic!() };
            assert_eq!((s.part, s.parts, s.title.as_str()), (n as u64, bodies.len() as u64, "News"));
            assert!(fedsync::fits(&sk, "us", "them", b.clone()));
            text.push_str(&s.text);
        }
        assert_eq!(text, item.text, "nothing lost");
        item.text = "short".into();
        assert_eq!(parts(&sk, "us", "them", item.clone(), "b"), Some(vec![Body::Share(item)]));
    }

    /// Review 5 M4: a link too long to send is dropped, and what can't be
    /// made to fit at all is said to be so (not sent as is, again and again).
    #[test]
    fn what_cant_fit_loses_its_link_or_isnt_sent() {
        let sk = proto::signing_key(&proto::generate().unwrap().0).unwrap();
        let mut item = Share::new(ShareKind::Feed, "1", "A long link");
        item.url = Some(format!("https://example.com/{}", "a".repeat(2500)));
        item.text = "Some words.".into();
        let sent = parts(&sk, "us", "them", item.clone(), "b").unwrap();
        let [Body::Share(sent)] = sent.as_slice() else { panic!("one part: {sent:?}") };
        assert_eq!((sent.url.as_deref(), sent.title.as_str()), (None, "A long link"));
        item.by = "x".repeat(2500);
        assert_eq!(parts(&sk, "us", "them", item, "b"), None);
    }

    #[test]
    fn picks_survive_the_custom_id() {
        for p in [Pick::Announcement { channel: Snowflake(1), message: Snowflake(2) }, Pick::Event(Snowflake(3))] {
            assert_eq!(decode(&encode(p)), Some(p));
        }
        assert_eq!(decode("x:1"), None);
    }
}

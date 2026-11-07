//! Content shared with partner servers. Outbound: resources, feed items and
//! questions as they're posted (to partners whose `send` lists them), and
//! announcements and events when an admin shares one; each is queued in the
//! store, and the bot sends it, and later edits or take-downs, as signed
//! envelopes. Inbound: what a partner shares lands where its `accept` says,
//! after our review unless `review = false`, always saying whom it's from,
//! with their text escaped, links plain https and nobody pinged.

use anyhow::{Context, Result, anyhow, bail};
use chipsoc_config::{Config, Peer, Snowflake};
use chipsoc_discord::wire::{id, s};
use chipsoc_discord::{Method, Rest};
use chipsoc_store::{AppKind, Application, Mirror, MirrorState};
use serde_json::{Value, json};

pub use chipsoc_fedproto::{Share, ShareKind};

use crate::fmt::{MAX_CONTENT, escape, partner_name, partner_text, plain, safe_link, truncate};
use crate::{Actor, Answer, AuditAction, Host, audit, now};

/// Longest title and text sent; what arrives is cut to the same.
pub const MAX_TITLE: usize = 100;
pub const MAX_TEXT: usize = 3000;
/// A partner's items (or edits) of one kind waiting in our review queue at
/// once; more of that kind are ignored. Per kind, so a flood of questions
/// never crowds out their announcements and events.
pub const MAX_IN_REVIEW: i64 = 10;
/// Items of a kind shared as they're posted (resources, feed items,
/// questions) first shared within an hour; more wait for the next hour's
/// posts. Announcements and events, which an admin shares, aren't capped.
pub const MAX_AUTO_PER_HOUR: i64 = 20;
/// Of those, what one member's posts share within an hour.
pub const MAX_PER_MEMBER: i64 = 3;
/// Longest a partner's event may run (seconds).
const MAX_EVENT: i64 = 7 * 24 * 3600;

/// The review-queue kind for what a partner shares.
pub fn app_kind(what: ShareKind) -> Option<AppKind> {
    Some(match what {
        ShareKind::Resource => AppKind::PartnerResource,
        ShareKind::Question => AppKind::PartnerQuestion,
        ShareKind::Feed => AppKind::PartnerFeed,
        ShareKind::Event => AppKind::PartnerEvent,
        ShareKind::Announcement => AppKind::PartnerAnnouncement,
        ShareKind::Other => return None,
    })
}

/// What a review-queue entry of a partner's is.
pub fn share_kind(kind: AppKind) -> Option<ShareKind> {
    ShareKind::ALL.into_iter().find(|k| app_kind(*k) == Some(kind))
}

/// What people call a kind: "resource", "question"…
pub fn noun(what: ShareKind) -> &'static str {
    match what {
        ShareKind::Feed => "feed item",
        k => k.as_str(),
    }
}

/// "a resource", "an event"…
pub fn a_noun(what: ShareKind) -> String {
    let n = noun(what);
    let an = n.starts_with(['a', 'e', 'i', 'o', 'u']);
    format!("{} {n}", if an { "an" } else { "a" })
}

/// Partners we send `what` to (signed sync set up: an id and their inbox).
pub fn partners_for(cfg: &Config, what: ShareKind) -> Vec<&Peer> {
    cfg.federation.peers.iter().filter(|p| !p.id.is_empty() && p.sends(what.plural())).collect()
}

/// A partner by name or id.
fn peer_named<'a>(cfg: &'a Config, name: &str) -> Option<&'a Peer> {
    let name = name.trim();
    cfg.federation.peers.iter().find(|p| p.id == name || p.name.eq_ignore_ascii_case(name))
}

fn body_of(item: &Share) -> String {
    serde_json::to_string(item).unwrap_or_default()
}

// --- What we share.

/// Queue `item` for the partners we send its kind to, or just those of them
/// named in `only` (names or ids). `author`: who shared it, the only member
/// (besides admins) whose later changes to it are sent. Says whom it's queued for.
pub async fn send<H: Host>(
    h: &H,
    item: &Share,
    only: Option<&[String]>,
    author: Option<Snowflake>,
) -> Result<Vec<String>> {
    let cfg = h.cfg();
    let mut to = partners_for(&cfg, item.what);
    if let Some(only) = only {
        to.retain(|p| only.iter().any(|n| peer_named(&cfg, n).is_some_and(|q| q.id == p.id)));
    }
    let fed = h.db();
    let fed = fed.fed();
    let body = body_of(item);
    let author = author.map(|a| a.to_string());
    for p in &to {
        fed.share_out(&p.id, item.what.as_str(), &item.origin, &body, author.as_deref(), now()).await?;
    }
    if !to.is_empty() {
        h.shares_queued();
    }
    Ok(to.iter().map(|p| p.name.clone()).collect())
}

/// Why something shared as it's posted wasn't.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Capped {
    /// [`MAX_AUTO_PER_HOUR`] of its kind went out this hour.
    Kind,
    /// [`MAX_PER_MEMBER`] of this member's went out this hour.
    Member,
}

/// Whether one more `what` shared as it's posted (by `author`) fits this hour's caps.
pub async fn capped<H: Host>(h: &H, what: ShareKind, author: Option<Snowflake>) -> Result<Option<Capped>> {
    if matches!(what, ShareKind::Announcement | ShareKind::Event) {
        return Ok(None);
    }
    let fed = h.db();
    let fed = fed.fed();
    let since = now() - 3600;
    if fed.shared_since(what.as_str(), None, since).await? >= MAX_AUTO_PER_HOUR {
        return Ok(Some(Capped::Kind));
    }
    if let Some(a) = author
        && fed.shared_since(what.as_str(), Some(&a.to_string()), since).await? >= MAX_PER_MEMBER
    {
        return Ok(Some(Capped::Member));
    }
    Ok(None)
}

/// [`send`] for something shared as it's posted, within the hourly caps
/// ([`capped`]); logs (doesn't return) a failure.
pub async fn send_quietly<H: Host>(h: &H, item: &Share, author: Option<Snowflake>) {
    if partners_for(&h.cfg(), item.what).is_empty() {
        return;
    }
    let sent = match capped(h, item.what, author).await {
        Ok(Some(c)) => {
            return tracing::info!(?c, "not sharing {} with partners: over this hour's cap", a_noun(item.what));
        }
        Ok(None) => send(h, item, None, author).await,
        Err(e) => Err(e),
    };
    if let Err(e) = sent {
        tracing::warn!("queueing {} for partners failed: {e:#}", a_noun(item.what));
    }
}

/// Something shared changed: send it again to the partners who have it.
pub async fn update<H: Host>(h: &H, item: &Share) -> Result<u64> {
    let n = h.db().fed().update_out(item.what.as_str(), &item.origin, &body_of(item), now()).await?;
    if n > 0 {
        h.shares_queued();
    }
    Ok(n)
}

/// Whether `user`'s change to what we shared from `origin` goes to partners:
/// only if they shared it, or they're an admin. What partners post of it
/// says it's from us, so nobody else may change it there.
pub async fn may_change<H: Host>(h: &H, what: ShareKind, origin: &str, user: Snowflake) -> Result<bool> {
    let author = h.db().fed().out_author(what.as_str(), origin).await?;
    if author.as_deref() == Some(user.to_string().as_str()) {
        return Ok(true);
    }
    let cfg = h.cfg();
    let admin_role = cfg.bot.admin_role.as_deref().and_then(|k| cfg.desired.lock.role(k));
    crate::is_admin(h.rest(), h.guild(), admin_role, user).await
}

/// Something shared was taken down: tell the partners who have it.
pub async fn remove<H: Host>(h: &H, what: ShareKind, origin: &str) -> Result<u64> {
    let n = h.db().fed().remove_out(what.as_str(), origin, now()).await?;
    if n > 0 {
        h.shares_queued();
    }
    Ok(n)
}

/// What we last shared from `origin`, if anything.
pub async fn shared<H: Host>(h: &H, what: ShareKind, origin: &str) -> Result<Option<Share>> {
    let rows = h.db().fed().out_bodies(what.as_str(), origin).await?;
    Ok(rows.into_iter().find_map(|b| serde_json::from_str(&b).ok()))
}

/// How a member shows on this server: nickname, display name or user name.
pub async fn display_name<H: Host>(h: &H, user: Snowflake) -> String {
    match h.rest().call(Method::Get, &format!("guilds/{}/members/{user}", h.guild()), None).await {
        Ok(m) => chipsoc_discord::wire::shown_name(&m).unwrap_or_else(|| "a member".to_owned()),
        Err(_) => "a member".to_owned(),
    }
}

/// A Discord link to a message (or a forum post, whose first message has its id).
pub fn message_link(guild: Snowflake, channel: Snowflake, message: Option<Snowflake>) -> String {
    match message {
        Some(m) if m != channel => format!("https://discord.com/channels/{guild}/{channel}/{m}"),
        _ => format!("https://discord.com/channels/{guild}/{channel}"),
    }
}

/// A resource we posted, to share.
pub fn resource(post: &crate::Post, r: &crate::NewResource, by: &str) -> Share {
    let mut item = Share::new(ShareKind::Resource, &post.id.to_string(), &truncate(r.title.trim(), MAX_TITLE));
    item.url = Some(r.url.trim().to_owned());
    item.text = truncate(r.description.trim(), MAX_TEXT);
    item.link = Some(post.link.clone());
    item.by = by.to_owned();
    item.tags = r.tags.clone();
    item
}

/// A post in the help forum, to share.
pub fn question(guild: Snowflake, thread: Snowflake, title: &str, text: &str, by: &str) -> Share {
    let mut item = Share::new(ShareKind::Question, &thread.to_string(), &truncate(title.trim(), MAX_TITLE));
    item.text = truncate(text.trim(), MAX_TEXT);
    item.link = Some(message_link(guild, thread, None));
    item.by = by.to_owned();
    item
}

/// An item a feed posted, to share. `origin` is stable for the item.
pub fn feed_item(origin: &str, feed: &str, title: &str, url: Option<&str>) -> Share {
    let mut item = Share::new(ShareKind::Feed, origin, &truncate(title.trim(), MAX_TITLE));
    item.url = url.map(str::to_owned);
    item.by = truncate(feed.trim(), MAX_TITLE);
    item
}

/// What an admin shares on demand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pick {
    /// A message here (in an announcement channel, usually).
    Announcement { channel: Snowflake, message: Snowflake },
    /// One of our scheduled events.
    Event(Snowflake),
}

impl Pick {
    /// From what an admin gave: an announcement's link (or `channel-message`
    /// ids, as Discord copies them), or an event's link or id.
    pub fn parse(what: &str, given: &str) -> Result<Self, String> {
        let given = given.trim().trim_end_matches('/');
        let ids: Vec<Snowflake> = given
            .rsplit(['/', '-'])
            .map_while(|p| p.parse::<Snowflake>().ok())
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        match what {
            "announcement" | "announcements" => match ids.as_slice() {
                [.., channel, message] => Ok(Self::Announcement { channel: *channel, message: *message }),
                _ => Err("Give the message's link (in Discord: ⋯ → Copy Message Link).".into()),
            },
            "event" | "events" => match ids.last() {
                Some(e) => Ok(Self::Event(*e)),
                None => Err("Give the event's link or id.".into()),
            },
            _ => Err("Share an announcement or an event.".into()),
        }
    }

    pub fn kind(self) -> ShareKind {
        match self {
            Self::Announcement { .. } => ShareKind::Announcement,
            Self::Event(_) => ShareKind::Event,
        }
    }
}

/// A message in one of our channels, to share: refused if it's elsewhere
/// (another server, a thread, a channel the config doesn't manage), in a
/// channel members can't all see, or where partners' posts land (that's
/// theirs to share).
pub async fn announcement<H: Host>(h: &H, channel: Snowflake, message: Snowflake) -> Result<Share> {
    shareable(&h.cfg(), channel)?;
    let m = h.rest().call(Method::Get, &format!("channels/{channel}/messages/{message}"), None).await?;
    let text = s(&m["content"]);
    if text.trim().is_empty() {
        bail!(crate::Refused("That message has no text to share.".into()));
    }
    let mut item = Share::new(ShareKind::Announcement, &message.to_string(), &announcement_title(&text));
    item.text = truncate(&text, MAX_TEXT);
    item.link = Some(message_link(h.guild(), channel, Some(message)));
    item.by = match id(&m["author"]["id"]) {
        Some(u) => display_name(h, u).await,
        None => String::new(),
    };
    Ok(item)
}

/// An announcement's title: its first line, without heading or bold marks.
pub fn announcement_title(text: &str) -> String {
    let first = text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or_default();
    truncate(first.trim_start_matches(['#', ' ', '*']).trim_end_matches('*'), MAX_TITLE)
}

/// Refused unless `channel` is one of ours that every member can see, and
/// not where partners' posts land.
fn shareable(cfg: &Config, channel: Snowflake) -> Result<()> {
    let Some(key) = cfg.desired.lock.channels.iter().find(|(_, c)| **c == channel).map(|(k, _)| k.as_str()) else {
        bail!(crate::Refused("That message isn't in one of this server's channels.".into()));
    };
    if cfg.federation.peers.iter().any(|p| p.accept.channels().any(|(_, k)| k == key)) {
        bail!(crate::Refused("That's where partners' posts land: it's theirs to share, not ours.".into()));
    }
    if !members_see(cfg, key) {
        bail!(crate::Refused(
            "That channel isn't open to all members, so what's said there isn't shared with partners.".into()
        ));
    }
    Ok(())
}

/// Whether a channel is open to every member: to @everyone, or (behind the
/// join gate) to everyone holding the access role.
fn members_see(cfg: &Config, key: &str) -> bool {
    let view = "VIEW_CHANNEL";
    let mut snap = cfg.desired.clone();
    if cfg.join.as_ref().is_some_and(|j| j.gate && j.access_role.is_some())
        && let Some(everyone) = snap.roles.roles.iter_mut().find(|r| r.key == "everyone")
        && !everyone.permissions.iter().any(|p| p == view)
    {
        // The access role sees what @everyone would without the gate.
        everyone.permissions.push(view.into());
    }
    chipsoc_config::public_channels(&snap).iter().any(|k| k == key)
}

/// Whether `user` can see `channel` in Discord now: the server's owner, an
/// Administrator, or View Channel after the channel's overwrites.
pub async fn can_view<H: Host>(h: &H, user: Snowflake, channel: Snowflake) -> Result<bool> {
    use chipsoc_discord::wire::ids;
    let bits = |v: &Value| v.as_str().and_then(|b| b.parse::<u64>().ok()).unwrap_or(0);
    let g = h.guild();
    let rest = h.rest();
    let guild = rest.call(Method::Get, &format!("guilds/{g}"), None).await?;
    if id(&guild["owner_id"]) == Some(user) {
        return Ok(true);
    }
    let member = match rest.call(Method::Get, &format!("guilds/{g}/members/{user}"), None).await {
        Ok(m) => m,
        Err(e) if chipsoc_discord::ApiError::status_of(&e) == Some(404) => return Ok(false),
        Err(e) => return Err(e),
    };
    let held = ids(&member["roles"]);
    let roles = rest.call(Method::Get, &format!("guilds/{g}/roles"), None).await?;
    let mut perms = 0;
    for r in roles.as_array().into_iter().flatten() {
        if id(&r["id"]).is_some_and(|r| r == g || held.contains(&r)) {
            perms |= bits(&r["permissions"]);
        }
    }
    let admin = 1u64 << 3;
    if perms & admin != 0 {
        return Ok(true);
    }
    let ch = rest.call(Method::Get, &format!("channels/{channel}"), None).await?;
    let overwrites: Vec<&Value> = ch["permission_overwrites"].as_array().into_iter().flatten().collect();
    let (mut allow, mut deny) = (0, 0);
    for o in &overwrites {
        if id(&o["id"]) == Some(g) {
            perms = (perms & !bits(&o["deny"])) | bits(&o["allow"]);
        } else if o["type"].as_u64() == Some(0) && id(&o["id"]).is_some_and(|r| held.contains(&r)) {
            (allow, deny) = (allow | bits(&o["allow"]), deny | bits(&o["deny"]));
        }
    }
    perms = (perms & !deny) | allow;
    for o in &overwrites {
        if o["type"].as_u64() == Some(1) && id(&o["id"]) == Some(user) {
            perms = (perms & !bits(&o["deny"])) | bits(&o["allow"]);
        }
    }
    Ok(perms & (1 << chipsoc_model::perms::VIEW_CHANNEL) != 0)
}

/// One of our scheduled events, to share.
pub async fn event<H: Host>(h: &H, event: Snowflake) -> Result<Share> {
    let e = h.rest().call(Method::Get, &format!("guilds/{}/scheduled-events/{event}", h.guild()), None).await?;
    event_from(h.guild(), &e).ok_or_else(|| anyhow!(crate::Refused("That event has no start time.".into())))
}

/// A scheduled event as Discord sends it, to share.
pub fn event_from(guild: Snowflake, e: &Value) -> Option<Share> {
    let time = |k: &str| chrono::DateTime::parse_from_rfc3339(&s(&e[k])).ok().map(|t| t.timestamp());
    let event = id(&e["id"])?;
    let mut item = Share::new(ShareKind::Event, &event.to_string(), &truncate(&s(&e["name"]), MAX_TITLE));
    item.start = Some(time("scheduled_start_time")?);
    item.end = time("scheduled_end_time");
    item.text = truncate(&s(&e["description"]), MAX_TEXT);
    item.location = truncate(&s(&e["entity_metadata"]["location"]), 100);
    item.link = Some(format!("https://discord.com/events/{guild}/{event}"));
    Some(item)
}

/// Share an announcement or event with the partners named (names or ids;
/// none: every partner we send that kind to). Says whom it went to.
pub async fn share_now<H: Host>(h: &H, pick: Pick, to: &[String], by: &Actor) -> Result<Vec<String>> {
    let cfg = h.cfg();
    let kind = pick.kind();
    let open = partners_for(&cfg, kind);
    if open.is_empty() {
        bail!(crate::Refused(format!(
            "No partner takes {} from us: list `{}` in a peer's `send` in federation.toml.",
            kind.plural(),
            kind.plural()
        )));
    }
    for n in to {
        match peer_named(&cfg, n) {
            None => bail!(crate::Refused(format!("No partner is called `{}`.", escape(n)))),
            Some(p) if !open.iter().any(|o| o.id == p.id) => bail!(crate::Refused(format!(
                "We don't send {} to {} (`send` in federation.toml).",
                kind.plural(),
                p.name
            ))),
            Some(_) => {}
        }
    }
    if let Pick::Announcement { channel, .. } = pick {
        shareable(&cfg, channel)?;
    }
    if let (Pick::Announcement { channel, .. }, Some(u)) = (pick, by.id)
        && !can_view(h, u, channel).await?
    {
        bail!(crate::Refused("You can't see that channel, so you can't share what's in it.".into()));
    }
    let mut item = match pick {
        Pick::Announcement { channel, message } => announcement(h, channel, message).await?,
        Pick::Event(e) => event(h, e).await?,
    };
    if item.by.is_empty() {
        item.by = match by.id {
            Some(u) => display_name(h, u).await,
            None => by.name.clone(),
        };
    }
    let only = (!to.is_empty()).then_some(to);
    let names = send(h, &item, only, by.id).await?;
    audit(h, by, AuditAction::SharePartners, Some(&item.origin), Some(&names.join(", "))).await;
    Ok(names)
}

// --- What partners share with us.

/// What became of something a partner shared.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Taken {
    /// In the review queue, as this entry (something new, or an edit of
    /// something posted).
    Filed(i64),
    /// Posted (review is off for it): its link.
    Posted(String),
    /// What we had of it was updated.
    Updated,
    /// Left alone, and why (for the bot log, if it's worth saying).
    Ignored(Ignored),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ignored {
    /// We don't take this kind from them.
    NotTaken,
    /// We have it from a newer envelope, or rejected or archived it.
    Stale,
    /// Too many of theirs of this kind wait for review already.
    QueueFull,
    /// An event that has started, or makes no sense.
    BadEvent,
    /// It has no title.
    Untitled,
}

/// Where a kind lands from `peer`, and whether it waits for review.
fn landing(peer: &Peer, what: ShareKind) -> Option<(Option<String>, bool)> {
    peer.accept.landing(what.plural()).map(|(to, review)| (to.map(str::to_owned), review))
}

/// Whether Discord said what we posted is gone (deleted here).
fn gone(e: &anyhow::Error) -> bool {
    chipsoc_discord::ApiError::status_of(e) == Some(404)
}

/// Whether two versions of an item say the same (however they were sent).
fn same(a: &Share, b: &Share) -> bool {
    let bare = |s: &Share| Share { batch: String::new(), part: 0, parts: 1, ..s.clone() };
    bare(a) == bare(b)
}

/// Take what `peer` shared, sent at `seq`: file it for review, post it,
/// or update what we have of it. An update of something waiting for review
/// redraws its card (and voids decisions on the old one); of something
/// posted, it waits for review too unless their `edits` say otherwise.
pub async fn take<H: Host>(h: &H, peer: &Peer, item: Share, seq: u64) -> Result<Taken> {
    let Some((_, review)) = landing(peer, item.what) else { return Ok(Taken::Ignored(Ignored::NotTaken)) };
    if partner_name(&item.title, MAX_TITLE).is_empty() {
        return Ok(Taken::Ignored(Ignored::Untitled));
    }
    if item.what == ShareKind::Event && !event_ok(&item) {
        return Ok(Taken::Ignored(Ignored::BadEvent));
    }
    let db = h.db();
    let fed = db.fed();
    let what = item.what.as_str();
    if let Some(mut m) = fed.mirror(&peer.id, what, &item.origin).await? {
        if m.seq >= seq || matches!(m.state, MirrorState::Refused | MirrorState::Removed) {
            return Ok(Taken::Ignored(Ignored::Stale));
        }
        let had: Option<Share> = serde_json::from_str(&m.body).ok();
        m.seq = seq;
        match m.state {
            MirrorState::Posted if peer.accept.edits_reviewed(item.what.plural()) => {
                return edit_for_review(h, peer, m, had, item).await;
            }
            MirrorState::Posted => {
                if !had.as_ref().is_some_and(|b| same(b, &item)) {
                    match edit(h, peer, &m, &item).await {
                        Ok(()) => {}
                        // Deleted here: nothing to update, now or later (not tried again).
                        Err(e) if gone(&e) => {
                            m.state = MirrorState::Removed;
                            if let Some(app) = m.edit_app.take() {
                                m.edit_body = None;
                                withdraw(h, peer, app, "changed it, but it was deleted here").await?;
                            }
                            fed.set_mirror(&m, now()).await?;
                            return Ok(Taken::Ignored(Ignored::Stale));
                        }
                        Err(e) => return Err(e),
                    }
                }
                m.body = body_of(&item);
                if let Some(app) = m.edit_app.take() {
                    m.edit_body = None;
                    withdraw(h, peer, app, "applied a newer change of theirs").await?;
                }
            }
            _ => {
                m.body = body_of(&item);
                fed.set_mirror(&m, now()).await?;
                if let Some(app) = m.app_id
                    && !had.as_ref().is_some_and(|b| same(b, &item))
                {
                    db.set_answers(app, &Answer::to_json(&entry(peer, &item, None))).await?;
                    let note = format!("\n✏️ **{} changed this while it waited**: this is the new version.", peer.name);
                    redraw(h, app, &note).await;
                }
                return Ok(Taken::Updated);
            }
        }
        fed.set_mirror(&m, now()).await?;
        return Ok(Taken::Updated);
    }
    let mut m = Mirror {
        peer: peer.id.clone(),
        what: what.to_owned(),
        origin: item.origin.clone(),
        seq,
        state: MirrorState::Review,
        body: body_of(&item),
        app_id: None,
        channel_id: None,
        message_id: None,
        edit_body: None,
        edit_app: None,
    };
    if review {
        let Some(app) = file(h, peer, &item, None).await? else { return Ok(Taken::Ignored(Ignored::QueueFull)) };
        m.app_id = Some(app);
        fed.set_mirror(&m, now()).await?;
        card(h, app).await;
        return Ok(Taken::Filed(app));
    }
    let (channel, message, link) = post(h, peer, &item).await?;
    m.state = MirrorState::Posted;
    (m.channel_id, m.message_id) = (channel, Some(message));
    fed.set_mirror(&m, now()).await?;
    Ok(Taken::Posted(link))
}

/// Put an item (or, with `before`, an edit of what's posted) in the review
/// queue; `None` if too many of the partner's of its kind wait already.
async fn file<H: Host>(h: &H, peer: &Peer, item: &Share, before: Option<&Posted>) -> Result<Option<i64>> {
    let db = h.db();
    if db.fed().mirrors_in_review(&peer.id, item.what.as_str()).await? >= MAX_IN_REVIEW {
        return Ok(None);
    }
    let kind = app_kind(item.what).context("not a kind this version takes")?;
    let target = format!("{}/{}", peer.id, item.origin);
    let user = format!("partner:{}", peer.id);
    let answers = Answer::to_json(&entry(peer, item, before));
    Ok(Some(db.add_application(kind, &target, &user, &answers, false, now()).await?))
}

/// What's posted of an item, as an edit's entry shows it.
struct Posted {
    item: Share,
    link: String,
}

/// A partner changed something posted here, and their edits wait for
/// review: file the change (or update the change already waiting), and
/// leave the post as it is until it's accepted.
async fn edit_for_review<H: Host>(h: &H, peer: &Peer, mut m: Mirror, had: Option<Share>, item: Share) -> Result<Taken> {
    let db = h.db();
    let fed = db.fed();
    let link = posted_link(h, &m);
    let unchanged = had.as_ref().is_some_and(|b| same(b, &item));
    let before = had.map(|item| Posted { item, link });
    match m.edit_app {
        _ if unchanged => {
            // Back to what's posted: nothing to review.
            if let Some(app) = m.edit_app.take() {
                m.edit_body = None;
                withdraw(h, peer, app, "changed it back").await?;
            }
        }
        Some(app) => {
            db.set_answers(app, &Answer::to_json(&entry(peer, &item, before.as_ref()))).await?;
            m.edit_body = Some(body_of(&item));
            fed.set_mirror(&m, now()).await?;
            let note = format!("\n✏️ **{} changed this again while it waited**: this is the newest.", peer.name);
            redraw(h, app, &note).await;
            return Ok(Taken::Updated);
        }
        None => {
            let Some(app) = file(h, peer, &item, before.as_ref()).await? else {
                return Ok(Taken::Ignored(Ignored::QueueFull));
            };
            (m.edit_body, m.edit_app) = (Some(body_of(&item)), Some(app));
            fed.set_mirror(&m, now()).await?;
            card(h, app).await;
            return Ok(Taken::Filed(app));
        }
    }
    fed.set_mirror(&m, now()).await?;
    Ok(Taken::Updated)
}

/// Take a waiting entry off the queue (rejected as the partner, so nobody
/// here is credited), saying why on its card.
async fn withdraw<H: Host>(h: &H, peer: &Peer, app: i64, why: &str) -> Result<()> {
    let db = h.db();
    if db.decide(app, false, &format!("partner:{}", peer.id), now()).await?
        && let Some(a) = db.application(app).await?
    {
        let note = format!("\n🗃️ **{} {why}**, so it's off the queue.", peer.name);
        if let Err(e) = crate::mark_card(h, &a, &note).await {
            tracing::warn!("marking a partner's card failed: {e:#}");
        }
    }
    Ok(())
}

/// `peer` took down what it shared from `origin`: archive what we posted of
/// it, or take it (or an edit of it) out of the review queue. False if we
/// had nothing of it.
pub async fn take_down<H: Host>(h: &H, peer: &Peer, what: ShareKind, origin: &str, seq: u64) -> Result<bool> {
    let db = h.db();
    let fed = db.fed();
    let Some(mut m) = fed.mirror(&peer.id, what.as_str(), origin).await? else { return Ok(false) };
    if m.seq >= seq || matches!(m.state, MirrorState::Refused | MirrorState::Removed) {
        return Ok(false);
    }
    if m.state == MirrorState::Posted {
        match archive(h, &m, &format!("🗃️ {} took this down.", peer.name)).await {
            Err(e) if !gone(&e) => return Err(e),
            _ => {} // deleted here already: nothing to archive
        }
    }
    for app in [m.app_id.filter(|_| m.state == MirrorState::Review), m.edit_app.take()].into_iter().flatten() {
        withdraw(h, peer, app, "took this down").await?;
    }
    m.seq = seq;
    m.state = MirrorState::Removed;
    m.edit_body = None;
    fed.set_mirror(&m, now()).await?;
    Ok(true)
}

/// A partner was removed from federation.toml (`peer`: its id): what it
/// shared is archived here (posts closed and locked, events cancelled),
/// what waits for review is taken off the queue, and every record of what
/// it shared, or we shared with it, is forgotten, so adding it back starts
/// afresh. How many items that was.
pub async fn forget_partner<H: Host>(h: &H, peer: &str) -> Result<usize> {
    let db = h.db();
    let mirrors = db.fed().forget_shares(peer).await?;
    let name = plain(peer, 100);
    for m in &mirrors {
        if m.state == MirrorState::Posted
            && let Err(e) = archive(h, m, &format!("🗃️ No longer shared: `{name}` isn't a partner any more.")).await
        {
            tracing::warn!("archiving a removed partner's post failed: {e:#}");
        }
        let waiting = m.app_id.filter(|_| m.state == MirrorState::Review);
        for app in [waiting, m.edit_app].into_iter().flatten() {
            if db.decide(app, false, &format!("partner:{peer}"), now()).await?
                && let Some(a) = db.application(app).await?
            {
                let note = format!("\n🗃️ **`{name}` isn't a partner any more**, so it's off the queue.");
                if let Err(e) = crate::mark_card(h, &a, &note).await {
                    tracing::warn!("marking a partner's card failed: {e:#}");
                }
            }
        }
    }
    Ok(mirrors.len())
}

/// An event worth taking: it hasn't started, and its end (if given) is after its start.
fn event_ok(item: &Share) -> bool {
    let Some(start) = item.start else { return false };
    let end = item.end.unwrap_or(start + 3600);
    start > now() && end > start && end - start <= MAX_EVENT
}

/// An event's place, as we'd post it: their words, or a link only if it's
/// plain https ([`safe_link`]); `None` if there's none (or the link isn't).
fn place(item: &Share) -> Option<String> {
    let l = partner_name(&item.location, 100);
    let lower = l.to_ascii_lowercase();
    if lower.contains("://") || lower.starts_with("www.") || lower.contains("discord.gg/") {
        return safe_link(&l).map(|s| s.trim_matches(['<', '>']).to_owned());
    }
    (!l.is_empty()).then_some(l)
}

/// What the review queue keeps of a partner's item: who it's from and what
/// it says (with `before`, what's posted now, for an edit). Accepting posts
/// exactly this: it's checked against what's stored then.
fn entry(peer: &Peer, item: &Share, before: Option<&Posted>) -> Vec<Answer> {
    let a = |q: &str, v: &str| Answer::new(q, Some(&v.to_owned()));
    let at = |t: i64| {
        chrono::DateTime::from_timestamp(t, 0).map(|t| t.format("%Y-%m-%d %H:%M UTC").to_string()).unwrap_or_default()
    };
    let mut out = vec![a("From", &peer.name), a("Shared by", &item.by)];
    if let Some(b) = before {
        out.push(a("Changes", &format!("what's posted at {}", b.link)));
        let was = &b.item;
        if was.title != item.title {
            out.push(a("Title before", &was.title));
        }
        if was.url != item.url {
            out.push(a("Link before", was.url.as_deref().unwrap_or("(none)")));
        }
        if was.location != item.location {
            out.push(a("Where before", &was.location));
        }
        if (was.start, was.end) != (item.start, item.end) {
            out.push(a("Time before", &was.start.map(at).unwrap_or_default()));
        }
        if was.text != item.text {
            out.push(a("Text before", &was.text));
        }
    }
    out.push(a("Title", &item.title));
    if let Some(url) = &item.url {
        out.push(a("Link", url));
    }
    if let Some(start) = item.start {
        out.push(a("Starts", &at(start)));
    }
    if let Some(end) = item.end {
        out.push(a("Ends", &at(end)));
    }
    if !item.location.is_empty() {
        out.push(a("Where", &item.location));
    }
    out.push(a("Text", &item.text));
    if let Some(link) = &item.link {
        out.push(a("Original", link));
    }
    out
}

/// What a partner's entry in the review queue is, as people know it: its
/// title on one line, with nothing invisible (escape it for Discord).
pub fn what(app: &Application) -> String {
    let title = Answer::parse(&app.answers).into_iter().find(|a| a.question == "Title").map(|a| a.answer);
    partner_name(&title.unwrap_or_else(|| app.target.clone()), MAX_TITLE)
}

/// Who sent a partner's entry: the partner's name, from its `user`.
pub fn sender(cfg: &Config, app: &Application) -> String {
    let id = app.user.strip_prefix("partner:").unwrap_or(&app.user);
    cfg.federation.peers.iter().find(|p| p.id == id).map_or_else(|| plain(id, 50), |p| p.name.clone())
}

/// The version of an entry its card and the queue show: Accept carries it,
/// and is refused once the partner has changed the entry since.
pub fn version(app: &Application) -> String {
    chipsoc_model::stable_hash(&app.answers)
}

/// The card's text and buttons for a partner's entry, as it is now.
async fn card_body<H: Host>(h: &H, app: &Application) -> Result<Value> {
    let cfg = h.cfg();
    let m = h.db().fed().mirror_of_app(app.id).await?.context("what the partner shared isn't kept any more")?;
    let peer = cfg.federation.peers.iter().find(|p| p.id == m.peer).context("not a partner any more")?;
    let editing = m.edit_app == Some(app.id);
    let body = if editing { m.edit_body.as_deref().unwrap_or_default() } else { &m.body };
    let item: Share = serde_json::from_str(body)?;
    let (to, _) = landing(peer, item.what).unwrap_or_default();
    let place_here = to.and_then(|k| cfg.desired.lock.channel(&k)).map(|c| format!(" in <#{c}>")).unwrap_or_default();
    let mut text = if editing {
        let before: Option<Share> = serde_json::from_str(&m.body).ok();
        let mut t = format!(
            "**#{} · From {} · ✏️ changed {}: {}**\nshared by {} · {} changed what's posted at {}; accepting updates it there (until then it stays as it was)\n",
            app.id,
            peer.name,
            noun(item.what),
            plain(&item.title, MAX_TITLE),
            plain(&item.by, 80),
            peer.name,
            posted_link(h, &m)
        );
        if let Some(b) = &before {
            let mut changed = Vec::new();
            if b.title != item.title {
                changed.push(format!("**Title** was: {}", plain(&b.title, MAX_TITLE)));
            }
            if b.url != item.url {
                let was = b
                    .url
                    .as_deref()
                    .map_or_else(|| "(none)".to_owned(), |u| safe_link(u).unwrap_or_else(|| plain(u, 200)));
                changed.push(format!("**Link** was: {was}"));
            }
            if b.location != item.location {
                changed.push(format!("**Where** was: {}", plain(&b.location, 100)));
            }
            if (b.start, b.end) != (item.start, item.end) {
                changed.push(match b.start {
                    Some(s) => format!("**Starts** was: <t:{s}:F>"),
                    None => "**The time** changed".to_owned(),
                });
            }
            if b.text != item.text {
                changed.push("**The text** changed (the new text is below)".to_owned());
            }
            if !changed.is_empty() {
                t.push_str(&format!("\n{}\n", changed.join("\n")));
            }
        }
        t
    } else {
        format!(
            "**#{} · From {} · {}: {}**\nshared by {} · accepting posts it{place_here}\n",
            app.id,
            peer.name,
            noun(item.what),
            plain(&item.title, MAX_TITLE),
            plain(&item.by, 80)
        )
    };
    if let Some(url) = &item.url {
        let shown = safe_link(url).unwrap_or_else(|| "(a link that isn't plain https: it's left out)".to_owned());
        text.push_str(&format!("\n**Link**\n{shown}\n"));
    }
    if let Some(start) = item.start {
        let end = item.end.map(|e| format!(" to <t:{e}:t>")).unwrap_or_default();
        text.push_str(&format!("\n**Starts** <t:{start}:F>{end}\n"));
    }
    if item.what == ShareKind::Event {
        let shown = match place(&item) {
            Some(p) if p.contains("://") => format!("<{p}>"),
            Some(p) => plain(&p, 100),
            None if item.location.trim().is_empty() => "(not given)".to_owned(),
            None => "(a link that isn't plain https: it's left out)".to_owned(),
        };
        text.push_str(&format!("**Where** {shown}\n"));
    }
    if !item.text.trim().is_empty() {
        let quoted = partner_text(&item.text, 600).replace('\n', "\n> ");
        text.push_str(&format!("\n> {quoted}\n"));
    }
    if let Some(link) = item.link.as_deref().and_then(safe_link) {
        text.push_str(&format!("\nOriginal: {link}"));
    }
    let buttons = json!([{ "type": 1, "components": [
        { "type": 2, "style": 3, "label": "Accept", "custom_id": crate::rounds::review_seen(app.id, &version(app)) },
        { "type": 2, "style": 4, "label": "Reject", "custom_id": crate::rounds::review_button(false, app.id) },
    ]}]);
    Ok(
        json!({ "content": truncate(&text, MAX_CONTENT - 1), "components": buttons, "allowed_mentions": { "parse": [] } }),
    )
}

/// Post the card for a partner's entry in the review channel, with Accept and Reject.
async fn card<H: Host>(h: &H, app: i64) {
    let cfg = h.cfg();
    let Some(ch) = cfg.bot.review.as_ref().and_then(|r| cfg.desired.lock.channel(&r.channel)) else {
        return tracing::warn!("no review channel; partner item #{app} is in the queue only");
    };
    let body = match h.db().application(app).await {
        Ok(Some(a)) => card_body(h, &a).await,
        Ok(None) => return,
        Err(e) => Err(e),
    };
    let body = match body {
        Ok(b) => b,
        Err(e) => return tracing::warn!("drawing a partner's card failed (it's in the queue): {e:#}"),
    };
    match h.rest().call(Method::Post, &format!("channels/{ch}/messages"), Some(body)).await {
        Ok(m) => {
            if let Err(e) = h.db().set_card(app, &ch.to_string(), &s(&m["id"])).await {
                tracing::warn!("remembering a partner's card failed: {e:#}");
            }
        }
        Err(e) => tracing::warn!("posting a partner's card failed (it's in the queue): {e:#}"),
    }
}

/// Draw a partner's card again as the entry is now, with `note` under it:
/// what was on it before (a decision that didn't go through, say) is
/// gone, and its old Accept no longer works. Posts it if it has no card.
pub async fn redraw<H: Host>(h: &H, app: i64, note: &str) {
    let a = match h.db().application(app).await {
        Ok(Some(a)) => a,
        Ok(None) => return,
        Err(e) => return tracing::warn!("reading a partner's entry failed: {e:#}"),
    };
    let Some((ch, msg)) = &a.card else { return card(h, app).await };
    let mut body = match card_body(h, &a).await {
        Ok(b) => b,
        Err(e) => return tracing::warn!("drawing a partner's card failed: {e:#}"),
    };
    body["content"] = json!(crate::fmt::with_note(&s(&body["content"]), note));
    if let Err(e) = h.rest().call(Method::Patch, &format!("channels/{ch}/messages/{msg}"), Some(body)).await {
        tracing::warn!("drawing a partner's card again failed: {e:#}");
    }
}

/// An accepted entry: post it (or, for an edit, update the post), exactly
/// as the entry shows it. What happened, whether it went, and the post's link.
pub async fn accept<H: Host>(h: &H, app: &Application) -> (String, bool, Option<String>) {
    let failed = |why: String| (format!(" ⚠️ {why}"), false, None);
    let db = h.db();
    let fed = db.fed();
    let mut m = match fed.mirror_of_app(app.id).await {
        Ok(Some(m)) => m,
        Ok(None) => return failed("What the partner shared isn't kept any more; reject this instead.".into()),
        Err(e) => return failed(format!("Couldn't read it: {}", crate::fmt::error(&e))),
    };
    let cfg = h.cfg();
    let Some(peer) = cfg.federation.peers.iter().find(|p| p.id == m.peer) else {
        return failed("That partner is no longer in federation.toml; reject this instead.".into());
    };
    let editing = m.edit_app == Some(app.id);
    let waiting = if editing { m.state == MirrorState::Posted } else { m.state == MirrorState::Review };
    if !waiting {
        return failed("It isn't waiting any more; reject this instead.".into());
    }
    let body = if editing { m.edit_body.clone().unwrap_or_default() } else { m.body.clone() };
    let Ok(item) = serde_json::from_str::<Share>(&body) else {
        return failed("What the partner shared can't be read; reject this instead.".into());
    };
    if landing(peer, item.what).is_none() {
        return failed(format!(
            "We no longer take {} from {} (`accept`); reject this instead.",
            item.what.plural(),
            peer.name
        ));
    }
    if item.what == ShareKind::Event && !event_ok(&item) {
        return failed("The event has started already; reject this instead.".into());
    }
    // Exactly what was reviewed: the entry as it was when Accept was pressed.
    let before = if editing {
        serde_json::from_str::<Share>(&m.body).ok().map(|item| Posted { item, link: posted_link(h, &m) })
    } else {
        None
    };
    let now_says = Answer::to_json(&entry(peer, &item, before.as_ref()));
    if now_says != app.answers {
        if let Err(e) = db.undecide(app.id).await {
            tracing::warn!("putting a partner's entry back in the queue failed: {e:#}");
        }
        if let Err(e) = db.set_answers(app.id, &now_says).await {
            tracing::warn!("updating a partner's entry failed: {e:#}");
        }
        let why = format!("{} changed this since it was shown, so nothing was posted: review it again.", peer.name);
        redraw(h, app.id, &format!("\n⚠️ **{why}**")).await;
        return failed(why);
    }
    if editing {
        if let Err(e) = edit(h, peer, &m, &item).await {
            tracing::warn!("updating a partner's post failed: {e:#}");
            return failed(format!("Couldn't update the post: {}", crate::fmt::error(&e)));
        }
        let link = posted_link(h, &m);
        (m.body, m.edit_body, m.edit_app) = (body, None, None);
        if let Err(e) = fed.set_mirror(&m, now()).await {
            tracing::warn!("saving a partner's edit failed: {e:#}");
        }
        return (format!(" Updated: {link}"), true, Some(link));
    }
    match post(h, peer, &item).await {
        Ok((channel, message, link)) => {
            m.state = MirrorState::Posted;
            (m.channel_id, m.message_id) = (channel, Some(message));
            if let Err(e) = fed.set_mirror(&m, now()).await {
                tracing::warn!("saving where a partner's item went failed: {e:#}");
            }
            (format!(" Posted: {link}"), true, Some(link))
        }
        Err(e) => {
            tracing::warn!("posting a partner's item failed: {e:#}");
            failed(format!("Couldn't post it: {}", crate::fmt::error(&e)))
        }
    }
}

/// A rejected entry: later updates of it are ignored. A rejected edit
/// leaves the post as it was (their next edit is reviewed again).
pub async fn rejected<H: Host>(h: &H, app: &Application) {
    let db = h.db();
    let fed = db.fed();
    let r = match fed.mirror_of_app(app.id).await {
        Ok(Some(mut m)) if m.edit_app == Some(app.id) => {
            (m.edit_body, m.edit_app) = (None, None);
            fed.set_mirror(&m, now()).await
        }
        Ok(Some(mut m)) if m.state == MirrorState::Review => {
            m.state = MirrorState::Refused;
            fed.set_mirror(&m, now()).await
        }
        Ok(_) => Ok(()),
        Err(e) => Err(e),
    };
    if let Err(e) = r {
        tracing::warn!("saving a refused partner item failed: {e:#}");
    }
}

/// Where we posted a partner's item.
fn posted_link<H: Host>(h: &H, m: &Mirror) -> String {
    let g = h.guild();
    match (&m.channel_id, &m.message_id) {
        (None, Some(e)) => format!("https://discord.com/events/{g}/{e}"),
        (Some(c), Some(msg)) if c == msg => format!("https://discord.com/channels/{g}/{c}"),
        (Some(c), Some(msg)) => format!("https://discord.com/channels/{g}/{c}/{msg}"),
        _ => "(not posted)".to_owned(),
    }
}

/// The attribution line: whom it's from, who shared it, and the original.
fn credit(peer: &Peer, item: &Share) -> String {
    let who = match item.what {
        ShareKind::Question => "asked by",
        ShareKind::Feed => "from their feed",
        ShareKind::Announcement => "posted by",
        _ => "shared by",
    };
    let by = if item.by.trim().is_empty() { String::new() } else { format!(" · {who} {}", plain(&item.by, 80)) };
    let original = item.link.as_deref().and_then(safe_link);
    match (item.what, original) {
        (ShareKind::Question, Some(l)) => format!("From **{}**{by}. Answer there: {l}", peer.name),
        (ShareKind::Question, None) => format!("From **{}**{by}. Answer on their server.", peer.name),
        (_, Some(l)) => format!("From **{}**{by} · {l}", peer.name),
        (_, None) => format!("From **{}**{by}", peer.name),
    }
}

/// The text we post for a partner's item.
fn content(peer: &Peer, item: &Share, forum: bool) -> String {
    let mut parts = Vec::new();
    if !forum || item.what == ShareKind::Announcement {
        let mark = if item.what == ShareKind::Announcement { "📣" } else { "🔗" };
        parts.push(format!("{mark} **{}**", plain(&item.title, MAX_TITLE)));
    }
    if let Some(url) = item.url.as_deref().and_then(safe_link) {
        parts.push(url);
    }
    let credit = credit(peer, item);
    let note = if item.what == ShareKind::Question { "\n-# Replies here don't reach them." } else { "" };
    let room = MAX_CONTENT
        .saturating_sub(50 + credit.chars().count())
        .saturating_sub(parts.iter().map(|p| p.chars().count() + 2).sum::<usize>());
    let text = partner_text(&item.text, room.min(MAX_TEXT));
    if !text.is_empty() && !(item.what == ShareKind::Announcement && text == plain(&item.title, MAX_TITLE)) {
        parts.push(text);
    }
    parts.push(format!("{credit}{note}"));
    truncate(&parts.join("\n\n"), MAX_CONTENT - 1)
}

/// The scheduled event we make for a partner's.
fn event_body(peer: &Peer, item: &Share) -> Value {
    let iso = |t: i64| chrono::DateTime::from_timestamp(t, 0).map(|t| t.to_rfc3339()).unwrap_or_default();
    let start = item.start.unwrap_or_default();
    let end = item.end.filter(|e| *e > start).unwrap_or(start + 3600);
    let location = match (place(item), item.link.as_deref().and_then(safe_link)) {
        (Some(l), _) => l,
        (_, Some(link)) => truncate(link.trim_matches(['<', '>']), 100),
        _ => format!("With {}", peer.name),
    };
    let description = format!("{}\n\n{}", partner_text(&item.text, 700), credit(peer, item));
    // Their name always shows: the title is cut to leave room for it.
    let whose = format!(" ({})", partner_name(&peer.name, 40));
    let title = partner_name(&item.title, 100 - whose.chars().count());
    json!({
        "name": format!("{title}{whose}"),
        "privacy_level": 2,
        "entity_type": 3,
        "entity_metadata": { "location": location },
        "scheduled_start_time": iso(start),
        "scheduled_end_time": iso(end),
        "description": truncate(description.trim(), 1000),
    })
}

/// Post a partner's item where `accept` says: a post in a forum, a message,
/// or an event. Its (channel, message or event id, link).
async fn post<H: Host>(h: &H, peer: &Peer, item: &Share) -> Result<(Option<String>, String, String)> {
    let cfg = h.cfg();
    let (to, _) = landing(peer, item.what).context("we don't take that kind from them")?;
    let none = json!({ "parse": [] });
    if item.what == ShareKind::Event {
        let path = format!("guilds/{}/scheduled-events", h.guild());
        let made = h.rest().call(Method::Post, &path, Some(event_body(peer, item))).await?;
        let event = s(&made["id"]);
        return Ok((None, event.clone(), format!("https://discord.com/events/{}/{event}", h.guild())));
    }
    let key = to.context("no channel to post in")?;
    let channel = cfg.desired.lock.channel(&key).with_context(|| format!("`{key}` isn't made in Discord yet"))?;
    let forum = cfg.desired.channel(&key).is_some_and(|c| c.kind.has_tags());
    let text = content(peer, item, forum);
    if forum {
        let body = json!({
            "name": partner_name(&item.title, 95),
            "message": { "content": text, "allowed_mentions": none },
        });
        let thread = h.rest().call(Method::Post, &format!("channels/{channel}/threads"), Some(body)).await?;
        let tid = s(&thread["id"]);
        let link = format!("https://discord.com/channels/{}/{tid}", h.guild());
        return Ok((Some(tid.clone()), tid, link));
    }
    let body = json!({ "content": text, "allowed_mentions": none });
    let msg = h.rest().call(Method::Post, &format!("channels/{channel}/messages"), Some(body)).await?;
    let mid = s(&msg["id"]);
    Ok((Some(channel.to_string()), mid.clone(), format!("https://discord.com/channels/{}/{channel}/{mid}", h.guild())))
}

/// Bring what we posted of a partner's item up to date.
async fn edit<H: Host>(h: &H, peer: &Peer, m: &Mirror, item: &Share) -> Result<()> {
    let rest = h.rest();
    let none = json!({ "parse": [] });
    let Some(msg) = &m.message_id else { return Ok(()) };
    match &m.channel_id {
        None => {
            let path = format!("guilds/{}/scheduled-events/{msg}", h.guild());
            rest.call(Method::Patch, &path, Some(event_body(peer, item))).await?;
        }
        Some(ch) => {
            let forum = ch == msg;
            let body = json!({ "content": content(peer, item, forum), "allowed_mentions": none });
            rest.call(Method::Patch, &format!("channels/{ch}/messages/{msg}"), Some(body)).await?;
            if forum {
                let name = json!({ "name": partner_name(&item.title, 95) });
                rest.call(Method::Patch, &format!("channels/{ch}"), Some(name)).await?;
            }
        }
    }
    Ok(())
}

/// What a partner took down: a post closed and locked, a message replaced
/// by `note`, an event cancelled. Nothing is deleted.
async fn archive<H: Host>(h: &H, m: &Mirror, note: &str) -> Result<()> {
    let rest = h.rest();
    let Some(msg) = &m.message_id else { return Ok(()) };
    match &m.channel_id {
        None => {
            let path = format!("guilds/{}/scheduled-events/{msg}", h.guild());
            rest.call(Method::Patch, &path, Some(json!({ "status": 4 }))).await?;
        }
        Some(ch) => {
            let body = json!({ "content": note, "allowed_mentions": { "parse": [] } });
            rest.call(Method::Patch, &format!("channels/{ch}/messages/{msg}"), Some(body)).await?;
            if ch == msg {
                let closed = json!({ "archived": true, "locked": true });
                rest.call(Method::Patch, &format!("channels/{ch}"), Some(closed)).await?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer() -> Peer {
        Peer { name: "Them".into(), id: "them".into(), ..Default::default() }
    }

    /// Review 4 #23: a partner's name longer than a message leaves no room, not a panic.
    #[test]
    fn a_huge_partner_name_cant_underflow_the_room_left() {
        let peer = Peer { name: "x".repeat(3000), id: "them".into(), ..Default::default() };
        let mut item = Share::new(ShareKind::Announcement, "1", "News");
        item.text = "Words.".into();
        assert!(content(&peer, &item, false).chars().count() < MAX_CONTENT);
    }

    #[test]
    fn a_pick_reads_links_and_ids() {
        let a = Pick::parse("announcement", "https://discord.com/channels/1/22/333").unwrap();
        assert_eq!(a, Pick::Announcement { channel: Snowflake(22), message: Snowflake(333) });
        assert_eq!(Pick::parse("announcement", "22-333").unwrap(), a);
        assert!(Pick::parse("announcement", "333").is_err(), "a message id alone says no channel");
        assert_eq!(Pick::parse("event", "https://discord.com/events/1/44").unwrap(), Pick::Event(Snowflake(44)));
        assert_eq!(Pick::parse("event", "44").unwrap(), Pick::Event(Snowflake(44)));
        assert!(Pick::parse("poll", "1").is_err());
    }

    #[test]
    fn partner_posts_say_whom_they_are_from_and_ping_nobody() {
        let mut item = Share::new(ShareKind::Question, "9", "How do I [click](https://phish.example)?");
        item.text = "@everyone <@&1> see\u{202E} [this](https://evil.example)\n# Big".into();
        item.by = "Ada **Lovelace**".into();
        item.link = Some("https://discord.com/channels/1/9".into());
        let text = content(&peer(), &item, true);
        assert!(
            text.contains(
                "From **Them** · asked by Ada \\*\\*Lovelace\\*\\*. Answer there: <https://discord.com/channels/1/9>"
            ),
            "{text}"
        );
        assert!(text.contains("Replies here don't reach them"));
        assert!(text.contains("\\@everyone \\<\\@&1\\>") && !text.contains('\u{202E}'), "{text}");
        assert!(text.contains("\\[this\\]") && text.contains("\\# Big"), "{text}");
        item.link = Some("http://discord.com/x".into());
        assert!(content(&peer(), &item, true).contains("Answer on their server."), "plain https only");

        let mut r = Share::new(ShareKind::Resource, "5", "Guide");
        r.url = Some("javascript:alert(1)".into());
        r.by = "Bo".into();
        let text = content(&peer(), &r, false);
        assert!(!text.contains("javascript"), "{text}");
        assert!(text.starts_with("🔗 **Guide**") && text.ends_with("From **Them** · shared by Bo"), "{text}");
    }

    #[test]
    fn a_partners_event_is_an_external_event_with_their_link() {
        let mut e = Share::new(ShareKind::Event, "7", "Social");
        e.start = Some(now() + 3600);
        e.link = Some("https://discord.com/events/1/7".into());
        let body = event_body(&peer(), &e);
        assert_eq!(body["entity_type"], 3);
        assert_eq!(body["name"], "Social (Them)");
        assert_eq!(body["entity_metadata"]["location"], "https://discord.com/events/1/7");
        assert!(s(&body["description"]).contains("From **Them** · <https://discord.com/events/1/7>"));
        assert!(event_ok(&e));
        e.start = Some(now() - 1);
        assert!(!event_ok(&e), "started already");
        e.start = Some(now() + 10);
        e.end = Some(now() + 10 + MAX_EVENT + 1);
        assert!(!event_ok(&e), "too long");
    }

    #[test]
    fn a_partners_event_name_always_says_whose_and_odd_places_are_left_out() {
        let mut e = Share::new(ShareKind::Event, "7", &"A".repeat(MAX_TITLE));
        e.start = Some(now() + 3600);
        let name = s(&event_body(&peer(), &e)["name"]);
        assert!(name.ends_with("… (Them)") && name.chars().count() <= 100, "{name}");
        for (given, want) in [
            ("Room 2.15", Some("Room 2.15")),
            ("https://example.com/rsvp", Some("https://example.com/rsvp")),
            ("https://github.com@evil.example/rsvp", None),
            ("http://plain.example", None),
            ("www.example.com", None),
            ("  ", None),
        ] {
            e.location = given.into();
            assert_eq!(place(&e).as_deref(), want, "{given}");
        }
    }
}

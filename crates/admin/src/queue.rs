//! The review queue: applications for positions and project teams, and what
//! members send to be posted (feeds, project ideas, resources). Deciding is split in two so the bot can answer Discord fast:
//! [`claim`] records the decision, [`settle`] does what it earns.

use anyhow::Result;
use chipsoc_config::{Config, Snowflake};
use chipsoc_discord::{Method, Rest};
use chipsoc_store::{AppKind, AppStatus, Application};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::fmt::plain;
use crate::{Actor, AuditAction, Host, Refusal, Said, accept_feed, audit, dm, give, log_people, role_name};

/// One question on a form and what they wrote.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Answer {
    pub question: String,
    pub answer: String,
    /// The config's name for the question, when it has one (`[ideas] questions`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// The answer as a list, when it's one (a resource's tags); `answer` is
    /// then the list as people read it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<String>,
}

impl Answer {
    pub fn new(question: &str, answer: Option<&String>) -> Self {
        Self {
            question: question.to_owned(),
            answer: answer.cloned().unwrap_or_default(),
            key: None,
            items: Vec::new(),
        }
    }

    /// An answer to the question the config calls `key`.
    pub fn keyed(key: &str, question: &str, answer: impl Into<String>) -> Self {
        Self { key: Some(key.to_owned()), ..Self::new(question, Some(&answer.into())) }
    }

    /// A list for an answer, kept as it is (a comma in an item is just a comma).
    pub fn list(question: &str, items: Vec<String>) -> Self {
        Self { items: items.clone(), ..Self::new(question, Some(&items.join(", "))) }
    }

    /// How answers are stored.
    pub fn to_json(answers: &[Self]) -> String {
        serde_json::to_string(answers).unwrap_or_default()
    }

    /// Stored answers: a list of these, or (older ones) form field id → text.
    pub fn parse(json: &str) -> Vec<Self> {
        if let Ok(list) = serde_json::from_str(json) {
            return list;
        }
        let fields: serde_json::Map<String, Value> = serde_json::from_str(json).unwrap_or_default();
        let text = |v: Value| v.as_str().map_or_else(|| v.to_string(), str::to_owned);
        fields.into_iter().map(|(question, v)| Self::new(&question, Some(&text(v)))).collect()
    }
}

/// An application waiting for a decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pending {
    pub id: i64,
    /// `position`, `project`, `feed`, `idea`, `resource` or `event`.
    pub kind: String,
    /// Role key, project slug, feed address, idea name, resource link or event title.
    pub target: String,
    /// What it's for, as people know it.
    pub what: String,
    pub user: String,
    /// Who sent it, as people know them (see [`crate::people::named`]).
    #[serde(default)]
    pub from: String,
    pub answers: Vec<Answer>,
    pub as_lead: bool,
    pub created_at: i64,
    /// The version shown (see [`crate::shares::version`]): give it back
    /// when accepting, and Accept is refused if the entry changed since.
    #[serde(default)]
    pub version: String,
}

pub async fn pending<H: Host>(h: &H) -> Result<Vec<Pending>> {
    let cfg = h.cfg();
    let apps = h.db().pending_applications().await?;
    let ids = crate::people::ids(apps.iter().map(|a| a.user.as_str()));
    let names = crate::people::names_of(h, &ids).await;
    let one = |a: Application| Pending {
        from: crate::people::named(&cfg, &names, &a.user),
        id: a.id,
        kind: a.kind.as_str().to_owned(),
        what: what(&cfg, &a),
        version: crate::shares::version(&a),
        answers: Answer::parse(&a.answers),
        target: a.target,
        user: a.user,
        as_lead: a.as_lead,
        created_at: a.created_at,
    };
    Ok(apps.into_iter().map(one).collect())
}

/// What an application is for: the position's label, the project, the
/// resource's title, or the feed or idea as sent.
pub fn what(cfg: &Config, app: &Application) -> String {
    match app.kind {
        k if k.is_partner() => crate::shares::what(app),
        AppKind::Feed | AppKind::Idea | AppKind::Event => app.target.clone(),
        AppKind::Resource => {
            let title = crate::NewResource::from_answers(&Answer::parse(&app.answers)).title;
            if title.is_empty() { app.target.clone() } else { title }
        }
        AppKind::Position => cfg
            .bot
            .positions
            .iter()
            .find(|p| p.role == app.target)
            .map_or_else(|| app.target.clone(), |p| p.label.clone().unwrap_or_else(|| role_name(cfg, &p.role))),
        _ => cfg.projects.iter().find(|p| p.slug == app.target).map_or_else(|| app.target.clone(), |p| p.display()),
    }
}

/// How a decision went.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Decided {
    pub id: i64,
    pub accepted: bool,
    /// What accepting did.
    pub outcome: Said,
    /// Whether the applicant can now be made a project lead.
    pub lead: bool,
    /// What the reviewer told the applicant, if anything.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Accepting didn't go through (nothing was given), so it's waiting for a decision again.
    #[serde(default)]
    pub pending: bool,
    /// Whether whoever sent it was told by DM (never, for a partner's).
    #[serde(default)]
    pub told: bool,
}

impl Decided {
    /// The line added under the review card, as Discord markdown.
    pub fn note(&self, by: &Actor) -> String {
        let outcome = self.outcome.spaced();
        if self.pending {
            return format!(
                "\n⚠️ **{} pressed Accept, but it didn't go through:**{outcome} It's waiting again.",
                by.mention(),
            );
        }
        let mark = if self.accepted { "✅ **Accepted" } else { "❌ **Rejected" };
        let why = self.reason.as_deref().map(|r| format!(" Reason: {r}")).unwrap_or_default();
        format!("\n{mark} by {}.**{why}{outcome}", by.mention())
    }
}

/// Longest reason kept and sent.
const MAX_REASON: usize = 500;

/// A reason as given: trimmed, short enough, none if blank.
fn reason(r: Option<&str>) -> Option<String> {
    r.map(str::trim).filter(|r| !r.is_empty()).map(|r| crate::truncate(r, MAX_REASON))
}

/// A decided application, for the history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Past {
    pub id: i64,
    pub kind: String,
    pub target: String,
    pub what: String,
    pub user: String,
    /// Who sent it, as people know them (see [`crate::people::named`]).
    #[serde(default)]
    pub from: String,
    /// `accepted` or `rejected`.
    pub status: String,
    pub reviewed_by: Option<String>,
    /// Who decided, as people know them.
    #[serde(default)]
    pub by: Option<String>,
    pub reviewed_at: Option<i64>,
    pub reason: Option<String>,
    pub created_at: i64,
    /// They said they'd like to lead (project applications): Make lead is offered.
    #[serde(default)]
    pub as_lead: bool,
}

/// The latest decisions, newest first.
pub async fn queue_history<H: Host>(h: &H, limit: i64) -> Result<Vec<Past>> {
    let cfg = h.cfg();
    let rows = h.db().decided_applications(limit).await?;
    let ids =
        crate::people::ids(rows.iter().flat_map(|d| [Some(d.app.user.as_str()), d.reviewed_by.as_deref()]).flatten());
    let names = crate::people::names_of(h, &ids).await;
    let one = |d: chipsoc_store::Decision| Past {
        from: crate::people::named(&cfg, &names, &d.app.user),
        by: d.reviewed_by.as_deref().map(|b| crate::people::named(&cfg, &names, b)),
        id: d.app.id,
        kind: d.app.kind.as_str().to_owned(),
        what: what(&cfg, &d.app),
        target: d.app.target,
        user: d.app.user,
        status: d.app.status.as_str().to_owned(),
        reviewed_by: d.reviewed_by,
        reviewed_at: d.reviewed_at,
        reason: d.reason,
        created_at: d.app.created_at,
        as_lead: d.app.as_lead,
    };
    Ok(rows.into_iter().map(one).collect())
}

/// Record a decision on a pending application. `Err` is what to tell the reviewer.
pub async fn claim<H: Host>(h: &H, id: i64, accept: bool, by: &Actor) -> Result<Application, Refusal> {
    claim_seen(h, id, accept, None, by).await
}

/// [`claim`], for a reviewer who saw `seen` (a [`Pending::version`]): Accept
/// is refused if the entry changed since (a partner updated what it shared).
pub async fn claim_seen<H: Host>(
    h: &H,
    id: i64,
    accept: bool,
    seen: Option<&str>,
    by: &Actor,
) -> Result<Application, Refusal> {
    let mut app = match h.db().application(id).await {
        Ok(Some(a)) => a,
        Ok(None) => return Err("That application no longer exists.".into()),
        Err(e) => {
            tracing::warn!("reading application failed: {e:#}");
            return Err("Couldn't read that application; try again.".into());
        }
    };
    if accept && seen.is_some_and(|v| !v.is_empty() && v != crate::shares::version(&app)) {
        return Err(Said::new()
            .text(format!(
                "{} changed this since — review again: the card and the queue show the new version now.",
                submitter_plain(&h.cfg(), &app)
            ))
            .into());
    }
    if accept
        && app.kind == AppKind::Position
        && by.id.is_some_and(|me| app.user == me.to_string())
        && crate::grants_admin(&h.cfg(), &app.target)
        && !match by.id {
            Some(me) => crate::is_owner(h, me).await,
            None => false,
        }
    {
        return Err("Another admin has to accept your own application for an admin position.".into());
    }
    if accept && app.kind == AppKind::Project {
        let cfg = h.cfg();
        match cfg.projects.iter().find(|p| p.slug == app.target) {
            None => return Err("That project is no longer in the config; reject this instead.".into()),
            Some(p) if p.status != chipsoc_config::ProjectStatus::Active => {
                return Err(Said::new()
                    .strong(&p.name)
                    .text(format!(
                        " is {} now, so it takes no members: reject this, or make the project active first.",
                        p.status
                    ))
                    .into());
            }
            Some(_) => {}
        }
    }
    match h.db().decide(id, accept, &by.key(), crate::now()).await {
        Ok(true) => {}
        Ok(false) => return Err(Said::new().text(format!("Someone already {} this.", app.status)).into()),
        Err(e) => {
            tracing::warn!("saving decision failed: {e:#}");
            return Err("Couldn't save the decision; try again.".into());
        }
    }
    app.status = if accept { AppStatus::Accepted } else { AppStatus::Rejected };
    Ok(app)
}

/// Do what a claimed decision earns: grant, audit, log, and tell the
/// applicant. An accept that gives nothing is put back in the queue.
pub async fn settle<H: Host>(h: &H, app: &Application, accept: bool, by: &Actor) -> Decided {
    settle_with(h, app, accept, None, by).await
}

/// [`settle`], telling the applicant `why` (kept with the application).
pub async fn settle_with<H: Host>(h: &H, app: &Application, accept: bool, why: Option<&str>, by: &Actor) -> Decided {
    let Granted { said: mut outcome, ok, link, waits } =
        if accept { grant(h, app, by).await } else { (Said::new(), true, None).into() };
    let from = submitter(&h.cfg(), app);
    if !ok {
        if let Err(e) = h.db().undecide(app.id).await {
            tracing::warn!("putting application {} back in the queue failed: {e:#}", app.id);
        }
        audit(h, by, AuditAction::AcceptFailed, Some(&app.target), Some(&app.user)).await;
        log_people(
            h,
            &format!(
                "⚠️ **{} pressed Accept on {from}'s submission ({}), but it didn't go through.**",
                by.mention(),
                what_md(&h.cfg(), app)
            ),
        )
        .await;
        return Decided { id: app.id, accepted: false, outcome, lead: false, reason: None, pending: true, told: false };
    }
    let why = reason(why);
    if let Some(r) = &why
        && let Err(e) = h.db().set_reason(app.id, r).await
    {
        tracing::warn!("saving the reason failed: {e:#}");
    }
    let status = if accept { "accepted" } else { "rejected" };
    let action = if accept { AuditAction::Accepted } else { AuditAction::Rejected };
    audit(h, by, action, Some(&app.target), Some(&app.user)).await;
    let cfg = h.cfg();
    // Make lead is offered only to those who said they'd like to lead.
    let lead = accept && app.kind == AppKind::Project && app.as_lead && lead_role(&cfg, &app.target).is_some();
    log_people(h, &format!("📝 {} {status} {from}'s submission: **{}**.", by.mention(), what_md(&cfg, app))).await;
    if !accept && app.kind.is_partner() {
        crate::shares::rejected(h, app).await;
    }
    let mut was_told = false;
    if let Ok(user) = app.user.parse::<Snowflake>() {
        let mut msg = told(&cfg, app, accept, why.is_some(), link.as_deref(), waits);
        if let Some(r) = &why {
            msg.push_str(&format!("\n> {}", r.replace('\n', "\n> ")));
        }
        match dm(h, user, &msg, None).await {
            Ok(()) => was_told = true,
            Err(why) => {
                // On the card, where the reviewer sees it, and in the log.
                outcome = outcome.then(format!("(Couldn't DM them: {why}.)").as_str());
                log_people(h, &format!("⚠️ Couldn't DM <@{user}> about what they sent: {why}.")).await;
            }
        }
    }
    Decided { id: app.id, accepted: accept, outcome, lead, reason: why, pending: false, told: was_told }
}

/// Who sent an application: a mention, or the partner that shared it.
fn submitter(cfg: &Config, app: &Application) -> String {
    submitted_by(cfg, &app.user)
}

/// [`submitter`] for any queue entry's `user`: a mention, or a partner's name in bold.
pub fn submitted_by(cfg: &Config, user: &str) -> String {
    match user.strip_prefix("partner:") {
        Some(id) => format!(
            "**{}**",
            cfg.federation.peers.iter().find(|p| p.id == id).map_or_else(|| plain(id, 50), |p| p.name.clone())
        ),
        None => format!("<@{user}>"),
    }
}

/// Who sent it, as words (a partner's name, or "the applicant").
fn submitter_plain(cfg: &Config, app: &Application) -> String {
    if app.kind.is_partner() { crate::shares::sender(cfg, app) } else { "The applicant".to_owned() }
}

/// [`what`], as Discord markdown: what a partner (or member) wrote is escaped.
pub fn what_md(cfg: &Config, app: &Application) -> String {
    let w = what(cfg, app);
    if app.kind.is_partner() { plain(&w, 100) } else { w }
}

/// What the member who sent it is told about a decision; `waits` says why
/// an accepted position's role isn't theirs yet, if it isn't.
fn told(
    cfg: &Config,
    app: &Application,
    accept: bool,
    reason: bool,
    link: Option<&str>,
    waits: Option<crate::Waits>,
) -> String {
    let what = what(cfg, app);
    // Named, as members may be in several servers with this bot.
    let here = if cfg.bot.name.is_empty() { String::new() } else { format!(" in {}", cfg.bot.name) };
    let at = link.map(|l| format!(": {l}")).unwrap_or_default();
    let ask = if reason { "" } else { " Feel free to ask an admin why." };
    match (app.kind, accept) {
        (AppKind::Position, true) => match waits {
            None => format!("🎉 Your application for **{what}**{here} was accepted: you now hold **{what}**."),
            Some(w) => format!("🎉 Your application for **{what}**{here} was accepted. {}", w.for_them()),
        },
        (AppKind::Project, true) => format!(
            "🎉 Your application to join **{what}**{here} was accepted: you're on the team, and its channels are open to you."
        ),
        (AppKind::Position | AppKind::Project, false) => {
            format!("Your application for **{what}**{here} wasn't taken up this time.{ask}")
        }
        (AppKind::Idea, true) => format!("🎉 Your project idea **{what}** was posted{here}{at}"),
        (AppKind::Idea, false) => format!("Your project idea **{what}** wasn't posted{here} this time.{ask}"),
        (AppKind::Resource, true) => format!("🎉 The link you shared, **{what}**, was posted{here}{at}"),
        (AppKind::Resource, false) => format!("The link you shared, **{what}**, wasn't posted{here} this time.{ask}"),
        (AppKind::Feed, true) => {
            let ch = cfg.bot.feeds.as_ref().and_then(|f| cfg.desired.lock.channel(&f.channel));
            let there = ch.map_or_else(|| "the feeds channel".to_owned(), |c| format!("<#{c}>"));
            format!("🎉 The feed you suggested, {what}, was added{here}: its new posts will appear in {there}.")
        }
        (AppKind::Feed, false) => format!("The feed you suggested, {what}, wasn't added{here} this time.{ask}"),
        (AppKind::Event, true) => {
            format!("🎉 The event you suggested, **{what}**, is on the calendar{here}{at}")
        }
        (AppKind::Event, false) => {
            format!("The event you suggested, **{what}**, wasn't put on the calendar{here} this time.{ask}")
        }
        // Nobody here sent it: there's no one to tell.
        (
            AppKind::PartnerResource
            | AppKind::PartnerQuestion
            | AppKind::PartnerFeed
            | AppKind::PartnerEvent
            | AppKind::PartnerAnnouncement,
            _,
        ) => String::new(),
    }
}

/// What pressing Accept on an entry of `kind` does, and who's told: one
/// sentence, the same on review cards' forms, the web, the TUI and the CLI.
pub fn accept_says(kind: AppKind) -> &'static str {
    match kind {
        AppKind::Position => "Accepting gives them the role (recorded in the config) and tells them by DM.",
        AppKind::Project => "Accepting puts them on the team (its role and channels) and tells them by DM.",
        AppKind::Idea => "Accepting posts the idea in the ideas forum and tells them by DM.",
        AppKind::Resource => "Accepting posts the link in the resources forum and tells them by DM.",
        AppKind::Feed => "Accepting follows the feed and tells them by DM.",
        AppKind::Event => "Accepting puts the event on the server's calendar and tells them by DM.",
        AppKind::PartnerResource
        | AppKind::PartnerQuestion
        | AppKind::PartnerFeed
        | AppKind::PartnerEvent
        | AppKind::PartnerAnnouncement => "Accepting posts it here, as shown; the partner isn't told.",
    }
}

/// What pressing Reject on an entry of `kind` does, and who's told (see [`accept_says`]).
pub fn reject_says(kind: AppKind) -> &'static str {
    if kind.is_partner() {
        "Rejecting leaves it out; the partner isn't told, and a reason is only kept in the history."
    } else {
        "Rejecting tells them by DM, with your reason if you give one."
    }
}

/// The label of Reject's reason box for an entry of `kind`: who reads it.
pub fn reason_label(kind: AppKind) -> &'static str {
    if kind.is_partner() { "Reason (optional, kept in the history)" } else { "Reason (optional, sent to them)" }
}

/// [`accept_says`], [`reject_says`] and [`reason_label`] for a kind as stored;
/// one this doesn't know reads as a member's.
pub fn says_of(kind: &str) -> (&'static str, &'static str, &'static str) {
    let k = AppKind::of(kind);
    let member = "Deciding tells them by DM.";
    (
        k.map_or(member, accept_says),
        k.map_or(member, reject_says),
        k.map_or("Reason (optional, sent to them)", reason_label),
    )
}

/// Claim and settle, then mark the review card (for front ends other than its buttons).
pub async fn decide<H: Host>(h: &H, id: i64, accept: bool, by: &Actor) -> Result<Decided, Refusal> {
    decide_with(h, id, accept, None, by).await
}

/// [`decide`], telling the applicant `why`.
pub async fn decide_with<H: Host>(
    h: &H,
    id: i64,
    accept: bool,
    why: Option<&str>,
    by: &Actor,
) -> Result<Decided, Refusal> {
    decide_seen(h, id, accept, why, None, by).await
}

/// [`decide_with`], for a reviewer who saw `seen` (see [`claim_seen`]).
pub async fn decide_seen<H: Host>(
    h: &H,
    id: i64,
    accept: bool,
    why: Option<&str>,
    seen: Option<&str>,
    by: &Actor,
) -> Result<Decided, Refusal> {
    let app = claim_seen(h, id, accept, seen, by).await?;
    let d = settle_with(h, &app, accept, why, by).await;
    // A failed accept keeps its buttons, to try again.
    if !d.pending
        && let Err(e) = mark_card(h, &app, &d.note(by)).await
    {
        tracing::warn!("marking the review card failed: {e:#}");
    }
    Ok(d)
}

/// Add `note` under an application's review card and take its buttons away.
/// A card listing a window's round is drawn again instead, showing each
/// applicant's state.
pub async fn mark_card<H: Host>(h: &H, app: &Application, note: &str) -> Result<()> {
    let Some((ch, msg)) = &app.card else { return Ok(()) };
    if crate::rounds::on_round_card(app) {
        return crate::rounds::redraw(h, ch, msg).await;
    }
    let path = format!("channels/{ch}/messages/{msg}");
    let card = h.rest().call(Method::Get, &path, None).await?;
    let text = crate::fmt::with_note(&chipsoc_discord::wire::s(&card["content"]), note);
    let body = json!({ "content": text, "components": [], "allowed_mentions": { "parse": [] } });
    h.rest().call(Method::Patch, &path, Some(body)).await.map(drop)
}

/// What [`grant`] did.
struct Granted {
    said: Said,
    /// Whether it went through (else the application goes back in the queue).
    ok: bool,
    /// The post it made, if it made one.
    link: Option<String>,
    /// Why an accepted position's role isn't held yet, if it isn't.
    waits: Option<crate::Waits>,
}

impl From<(Said, bool, Option<String>)> for Granted {
    fn from((said, ok, link): (Said, bool, Option<String>)) -> Self {
        Self { said, ok, link, waits: None }
    }
}

/// Give what an accepted application earns.
async fn grant<H: Host>(h: &H, app: &Application, by: &Actor) -> Granted {
    if app.kind.is_partner() {
        return match crate::shares::accept(h, app).await {
            Ok(done @ crate::shares::Accepted::Posted(_)) => {
                (Said::new().text(format!("Posted: {}", done.link())), true, Some(done.link().to_owned())).into()
            }
            Ok(done @ crate::shares::Accepted::Updated(_)) => {
                (Said::new().text(format!("Updated: {}", done.link())), true, Some(done.link().to_owned())).into()
            }
            Err(why) => (Said::md(&format!("{} {why}", crate::fmt::ATTENTION)), false, None).into(),
        };
    }
    let Ok(user) = app.user.parse::<Snowflake>() else {
        return (Said::new().text("(Not a Discord user.)"), false, None).into();
    };
    let posted = |r: Result<crate::Post>| match r {
        Ok(p) => (Said::new().text(format!("Posted: {}", p.link)), true, Some(p.link)),
        Err(e) => {
            tracing::warn!("posting an accepted submission failed: {e:#}");
            (
                Said::new().text(format!("{} Couldn't post it: {}", crate::fmt::ATTENTION, crate::fmt::error(&e))),
                false,
                None,
            )
        }
    };
    match app.kind {
        AppKind::Idea => {
            let idea = crate::NewIdea::from_answers(&Answer::parse(&app.answers));
            posted(crate::post_idea(h, &idea, user).await).into()
        }
        AppKind::Resource => {
            let r = crate::NewResource::from_answers(&Answer::parse(&app.answers));
            posted(crate::post_resource(h, &r, &Actor { id: Some(user), name: app.user.clone() }).await).into()
        }
        AppKind::Event => make_event(h, app, user).await.into(),
        AppKind::Position => grant_position(h, app, user).await,
        _ => {
            let (said, ok) = grant_role(h, app, user, by).await;
            Granted { said, ok, link: None, waits: None }
        }
    }
}

/// [`grant`] for a position: recorded in the config and given. A role
/// that's grown too powerful to give, or one the config can't record and
/// the bot can't give, keeps the application in the queue.
async fn grant_position<H: Host>(h: &H, app: &Application, user: Snowflake) -> Granted {
    let cfg = h.cfg();
    let no = |said: Said| Granted { said, ok: false, link: None, waits: None };
    if !cfg.bot.positions.iter().any(|p| p.role == app.target) {
        return no(Said::new().text("(That position is no longer in the config.)"));
    }
    match crate::add_holder_unless(h, &app.target, user).await {
        Ok(added) => Granted { said: added.said, ok: true, link: None, waits: added.waits },
        Err((why, true)) => no(why),
        Err((why, false)) => {
            let keys = crate::with_admin(&cfg, std::slice::from_ref(&app.target));
            let (said, ok) = crate::give_unrecorded(h, user, &keys, &why).await;
            Granted { said, ok, link: None, waits: None }
        }
    }
}

/// [`grant`] for projects' team roles, and feeds.
async fn grant_role<H: Host>(h: &H, app: &Application, user: Snowflake, by: &Actor) -> crate::Given {
    let cfg = h.cfg();
    match app.kind {
        AppKind::Project => {
            let Some(p) = cfg.projects.iter().find(|p| p.slug == app.target) else {
                return (Said::new().text("(That project is no longer in the config.)"), false);
            };
            let team = p.team_role_key();
            if !p.team_role || cfg.desired.lock.role(&team).is_none() {
                return (
                    Said::new().text("This project has no team role yet; an admin needs to set it up first."),
                    false,
                );
            }
            if let Some(why) = crate::too_powerful_to_give(h, &team).await {
                return (why, false);
            }
            match give(h, user, &team).await {
                Ok(mut out) => {
                    if app.as_lead {
                        out = out.then(
                            Said::new().text(format!("{LEAD_WISH} Press ")).strong("Make lead").text(" if agreed."),
                        );
                    }
                    (out, true)
                }
                Err(why) => (why.said(), false),
            }
        }
        AppKind::Feed => match accept_feed(h, &app.target, by).await {
            Ok(title) => (Said::new().text("Now following ").strong(title).text("."), true),
            Err(e) => {
                let why = format!(
                    "{} Couldn't add the feed: {}",
                    crate::fmt::ATTENTION,
                    crate::fmt::error_str(&e.markdown())
                );
                (Said::new().text(why), false)
            }
        },
        AppKind::Position
        | AppKind::Idea
        | AppKind::Resource
        | AppKind::Event
        | AppKind::PartnerResource
        | AppKind::PartnerQuestion
        | AppKind::PartnerFeed
        | AppKind::PartnerEvent
        | AppKind::PartnerAnnouncement => (Said::new(), false),
    }
}

/// [`grant`] for a suggested event: made now, crediting whoever suggested it.
async fn make_event<H: Host>(h: &H, app: &Application, user: Snowflake) -> (Said, bool, Option<String>) {
    let cfg = h.cfg();
    let no = |why: Said| (why, false, None);
    if cfg.bot.events.is_none() {
        return no(Said::new().text("Events aren't set up here any more (").code("[events]").text(" in bot.toml)."));
    }
    let tz = match crate::events::tz(&cfg) {
        Ok(tz) => tz,
        Err(why) => return no(why.said()),
    };
    let Some(e) = crate::events::NewEvent::from_answers(&Answer::parse(&app.answers), tz) else {
        return no(Said::new().text("Its start time can't be read; reject it instead."));
    };
    if e.start <= crate::now() {
        return no(Said::new().text("It would have started already; reject it instead."));
    }
    match crate::events::create(h, &e, Some(user)).await {
        Ok(made) => {
            let mut said = made.said(&e.title, e.start).then(made.link.clone().unwrap_or_default().as_str());
            if !crate::shares::partners_for(&cfg, crate::shares::ShareKind::Event).is_empty() {
                said = said.then(Said::new().text("To share it with partners: ").code("/server share event").text("."));
            }
            (said, true, made.link)
        }
        Err(err) => {
            tracing::warn!("making a suggested event failed: {err:#}");
            no(Said::new().text(format!(
                "{} Discord refused the event: {}",
                crate::fmt::ATTENTION,
                crate::fmt::error(&err)
            )))
        }
    }
}

/// A project's Lead role key, if it has one set up.
fn lead_role(cfg: &Config, slug: &str) -> Option<String> {
    let p = cfg.projects.iter().find(|p| p.slug == slug && p.lead_role)?;
    cfg.desired.lock.role(&p.lead_role_key()).map(|_| p.lead_role_key())
}

/// The line saying an applicant would like to lead (on the card, and when it's accepted).
pub const LEAD_WISH: &str = "⭐ They'd like to lead it.";

/// An accepted project application whose applicant asked to lead, with the
/// Lead role's key. `Err` is what to tell the admin (anyone else can be made
/// a lead from the project, e.g. `/server project lead`).
pub async fn claim_lead<H: Host>(h: &H, id: i64) -> Result<(Application, String), Said> {
    let app = match h.db().application(id).await {
        Ok(Some(a)) if a.kind == AppKind::Project && a.status == AppStatus::Accepted && a.as_lead => a,
        Ok(Some(a)) if a.kind == AppKind::Project && a.status == AppStatus::Accepted => {
            return Err(Said::new()
                .text("They didn't ask to lead. To make them a lead anyway, use ")
                .code("/server project lead")
                .text(" (or the web Members page)."));
        }
        Ok(_) => return Err("That isn't an accepted project application.".into()),
        Err(e) => {
            tracing::warn!("reading application failed: {e:#}");
            return Err("Couldn't read that application; try again.".into());
        }
    };
    match (lead_role(&h.cfg(), &app.target), app.user.parse::<Snowflake>()) {
        (Some(key), Ok(_)) => Ok((app, key)),
        _ => Err("That project has no Lead role set up.".into()),
    }
}

/// Make a claimed applicant a lead; says what happened. `Err`: they weren't
/// (the Lead role is too powerful to give, say), so nobody is told.
pub async fn lead<H: Host>(h: &H, app: &Application, key: &str, by: &Actor) -> Result<Said, Refusal> {
    let Ok(user) = app.user.parse::<Snowflake>() else { return Err("That application has no member.".into()) };
    if let Some(why) = crate::too_powerful_to_give(h, key).await {
        return Err(why.into());
    }
    let said = match crate::add_lead_unless(h, &app.target, key, user).await {
        Ok(said) => said,
        Err((why, true)) => return Err(why.into()),
        Err((why, false)) => match crate::give_unrecorded(h, user, &[key.to_owned()], &why).await {
            (said, true) => said,
            (said, false) => return Err(said.into()),
        },
    };
    audit(h, by, AuditAction::MadeLead, Some(&app.target), Some(&app.user)).await;
    Ok(said.then(tell_lead(h, &app.target, user).await))
}

/// DM someone just made a lead of a project; says so if it couldn't.
pub(crate) async fn tell_lead<H: Host>(h: &H, slug: &str, user: Snowflake) -> Said {
    let cfg = h.cfg();
    let name = cfg.projects.iter().find(|p| p.slug == slug).map_or_else(|| slug.to_owned(), |p| p.name.clone());
    let here = if cfg.bot.name.is_empty() { String::new() } else { format!(" in {}", cfg.bot.name) };
    let text = format!("⭐ You're now a lead of **{name}**{here}. Thanks for taking it on!");
    match dm(h, user, &text, None).await {
        Ok(()) => Said::new(),
        Err(why) => Said::new().text(format!("(Couldn't DM them: {why}.)")),
    }
}

/// Why `user` can't apply for `target` now, as a reply for them: they have
/// an application for it waiting already, or they hold it already (the
/// position, or a place on the project's team).
pub async fn already<H: Host>(h: &H, kind: AppKind, target: &str, user: Snowflake) -> Option<String> {
    let cfg = h.cfg();
    let me = user.to_string();
    match h.db().pending_applications().await {
        Ok(waiting) => {
            if let Some(a) = waiting.iter().find(|a| a.kind == kind && a.target == target && a.user == me) {
                return Some(format!(
                    "You applied for **{}** on <t:{}:D> and it's still waiting for an admin, so there's no need to again. You'll get a DM once it's reviewed.",
                    what(&cfg, a),
                    a.created_at
                ));
            }
        }
        Err(e) => tracing::warn!("reading the queue failed: {e:#}"),
    }
    let held = match crate::join::held(h, user).await {
        Ok(held) => held,
        Err(e) => {
            tracing::warn!("reading a member's roles failed: {e:#}");
            Default::default()
        }
    };
    match kind {
        AppKind::Position => {
            let listed = cfg.desired.holders.get(target).is_some_and(|u| u.contains(&user));
            (listed || held.contains(target)).then(|| format!("You already hold **{}**.", role_name(&cfg, target)))
        }
        AppKind::Project => {
            let p = cfg.projects.iter().find(|p| p.slug == target)?;
            let on = p.leads.contains(&user) || held.contains(&p.team_role_key()) || held.contains(&p.lead_role_key());
            on.then(|| format!("You're already on the **{}** team.", p.name))
        }
        _ => None,
    }
}

/// The note under a card once its applicant is made a lead, as Discord markdown.
pub fn made_lead(by: &Actor, said: &Said) -> String {
    format!("\n⭐ Made a lead by {}.{}", by.mention(), said.spaced())
}

/// Claim and make a lead, then mark the review card. Says what happened.
pub async fn make_lead<H: Host>(h: &H, id: i64, by: &Actor) -> Result<Said, Refusal> {
    let (app, key) = claim_lead(h, id).await?;
    let said = lead(h, &app, &key, by).await?;
    if let Err(e) = mark_card(h, &app, &made_lead(by, &said)).await {
        tracing::warn!("marking the review card failed: {e:#}");
    }
    Ok(said)
}

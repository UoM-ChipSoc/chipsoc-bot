//! What admins do day to day, shared by every front end (the bot's buttons
//! and `/server`, the web UI and its API, and so the CLI and TUI): the review
//! queue, who holds which roles, feeds, resources, ideas and the audit log.
//! Everything goes through a [`Host`], which each front end provides:
//! Discord's REST API, the store, the config, and a way to change it.

pub mod events;
pub mod feeds;
pub mod find;
pub mod fmt;
pub mod forums;
pub mod join;
pub mod owner;
pub mod people;
pub mod queue;
pub mod rounds;
pub mod said;
pub mod shares;
pub mod status;
pub mod trust;

#[cfg(test)]
mod tests;

use std::collections::{BTreeSet, HashMap};
use std::future::Future;
use std::path::Path;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use chipsoc_config::ops::Op;
use chipsoc_config::{Config, Snowflake};
use chipsoc_discord::{Method, Rest};
use chipsoc_engine::{By, Engine, Held};
use serde::{Deserialize, Serialize};
use serde_json::json;

pub use chipsoc_store::AppKind;
pub use feeds::{FeedInfo, accept_feed, add_feed, feeds, pause_feed, remove_feed};
pub use find::{Found, Member, Unresolved, find_members, members_of, resolve_user};
pub use forums::{
    Closed, Lead, NewIdea, NewResource, Post, archive_resource, close_idea, closing_line, ideas, post_idea,
    post_resource, precheck_resource, resources, vet_resource,
};
pub use owner::{Done, history_of, only_owner, owner_id, undo, undo_question, undo_question_for, undo_said};
pub use people::{
    FormerMember, Holding, Named, Person, Roster, appoint, forget_former, remove_lead, restore, roster, unappoint,
};
pub use queue::{
    Answer, Decided, LEAD_WISH, Past, Pending, accept_says, already, claim, claim_lead, claim_seen, decide,
    decide_seen, decide_with, history, lead, made_lead, make_lead, mark_card, pending, reason_label, reject_says,
    says_of, settle, settle_with, submitted_by, what, what_md,
};
pub use said::{Part, Refusal, Said};
pub use status::{Drift, Line, drift};

/// What giving roles through the config said, and whether it went through.
pub type Given = (Said, bool);

/// What a front end gives these functions.
pub trait Host: Sync {
    type Rest: Rest;
    fn rest(&self) -> &Self::Rest;
    fn db(&self) -> chipsoc_store::Guild<'_>;
    fn cfg(&self) -> Arc<Config>;
    fn guild(&self) -> Snowflake;

    /// Whether config edits are committed and pushed here (see [`chipsoc_engine::autosaves`]).
    fn saves(&self) -> bool;

    /// Why config edits can't be saved here, if they can't.
    fn unwritable(&self) -> Option<String> {
        (!self.saves()).then(|| {
            "this bot can't save config changes (autosave is off: the config here isn't a synced git checkout \
             with CONFIG_AUTOSAVE=1). Ask the maintainer, or change the files in the config repo"
                .to_owned()
        })
    }

    /// Make `ops` (checked, as one change) and save them, then bring `user`'s
    /// holding of `roles` in line with the config, holding whatever keeps
    /// changes from interleaving (usually by calling [`record_with`]). Says
    /// how giving or taking the roles went.
    fn record<'a>(
        &'a self,
        ops: &'a [Op],
        user: Snowflake,
        roles: &'a [String],
    ) -> impl Future<Output = Result<Recorded>> + Send + 'a;

    /// Where members' resource submits keep the forum's posts between them
    /// (none: read each time).
    fn seen_posts(&self) -> Option<&forums::SeenPosts> {
        None
    }

    /// Something was queued for partners (see [`shares`]): the bot sends it
    /// soon. Elsewhere (the web UI) it goes on the bot's next look.
    fn shares_queued(&self) {}

    /// Whether whoever works here holds the bot's own token (on the bot's
    /// machine, `--local`): what only the server owner may do through the
    /// bot, they could do with it anyway.
    fn trusted(&self) -> bool {
        false
    }

    /// Go back on config commit `commit` as `by` (see
    /// [`chipsoc_engine::Engine::undo`]), holding whatever keeps changes from
    /// interleaving. What the new commit is, `None` if nothing changed.
    fn undo<'a>(
        &'a self,
        _commit: &'a str,
        _how: chipsoc_engine::git::Undo,
        _by: &'a By,
    ) -> impl Future<Output = Result<Option<chipsoc_engine::git::Undid>>> + Send + 'a {
        async { anyhow::bail!("going back on config changes isn't possible from here") }
    }
}

/// Names for the audit log, one per thing admins do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditAction {
    Accepted,
    Rejected,
    AcceptFailed,
    MadeLead,
    Appoint,
    Unappoint,
    RemoveLead,
    Restore,
    ForgetFormer,
    AddFeed,
    RemoveFeed,
    PauseFeed,
    ResumeFeed,
    PostResource,
    ArchiveResource,
    CloseIdea,
    JoinNotice,
    SharePartners,
    CreateEvent,
    /// A help thread marked solved.
    Solved,
    /// Someone finished the join flow.
    JoinFlow,
    ApproveConfigChange,
    RejectConfigChange,
    /// A partner's held sync allowed or refused.
    AllowPartnerSync,
    RefusePartnerSync,
    /// A partner's grant taken back: all of it, its key, or a role it gave.
    RevokePartner,
    RevokePartnerKey,
    RevokePartnerRole,
    RecordingStarted,
    RecordingStopped,
}

impl AuditAction {
    /// As stored. Older entries keep the names they were written with.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Accepted => "accepted",
            Self::Rejected => "rejected",
            Self::AcceptFailed => "accept_failed",
            Self::MadeLead => "made_lead",
            Self::Appoint => "appoint",
            Self::Unappoint => "unappoint",
            Self::RemoveLead => "remove_lead",
            Self::Restore => "restore",
            Self::ForgetFormer => "forget_former",
            Self::AddFeed => "add_feed",
            Self::RemoveFeed => "remove_feed",
            Self::PauseFeed => "pause_feed",
            Self::ResumeFeed => "resume_feed",
            Self::PostResource => "post_resource",
            Self::ArchiveResource => "archive_resource",
            Self::CloseIdea => "close_idea",
            Self::JoinNotice => "join_notice",
            Self::SharePartners => "share_partners",
            Self::CreateEvent => "create_event",
            Self::Solved => "solved",
            Self::JoinFlow => "join_flow",
            Self::ApproveConfigChange => "approve_config_change",
            Self::RejectConfigChange => "reject_config_change",
            Self::AllowPartnerSync => "allow_partner_sync",
            Self::RefusePartnerSync => "refuse_partner_sync",
            Self::RevokePartner => "revoke_partner",
            Self::RevokePartnerKey => "revoke_partner_key",
            Self::RevokePartnerRole => "revoke_partner_role",
            Self::RecordingStarted => "recording_started",
            Self::RecordingStopped => "recording_stopped",
        }
    }
}

/// Who is acting. People signed in to the web UI with a code have no id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Actor {
    pub id: Option<Snowflake>,
    pub name: String,
}

impl Actor {
    /// For the store: their id, else their name.
    pub fn key(&self) -> String {
        self.id.map_or_else(|| self.name.clone(), |id| id.to_string())
    }

    /// For messages: a mention, else their name.
    pub fn mention(&self) -> String {
        self.id.map_or_else(|| self.name.clone(), |id| format!("<@{id}>"))
    }

    /// As the engine knows them (on a [`Host::trusted`] host, `trusted`).
    pub fn by(&self, trusted: bool) -> By {
        match self.id {
            _ if trusted => By::local(&self.name),
            Some(id) => By::person(id, &self.name),
            None => By::named(Some(&self.name)),
        }
    }
}

/// One audit-log entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub ts: i64,
    pub actor: Option<String>,
    /// Who, as people know them.
    #[serde(default)]
    pub actor_name: Option<String>,
    pub action: String,
    pub target: Option<String>,
    pub detail: Option<String>,
}

/// The latest audit-log entries, newest first.
pub async fn activity<H: Host>(h: &H, limit: i64) -> Result<Vec<Entry>> {
    let rows = h.db().recent_audit(limit).await?;
    let ids = people::ids(rows.iter().filter_map(|e| e.actor.as_deref()));
    let names = people::names_of(h, &ids).await;
    let cfg = h.cfg();
    let entry = |e: chipsoc_store::AuditEntry| Entry {
        actor_name: e.actor.as_deref().map(|a| people::named(&cfg, &names, a)),
        ts: e.ts,
        actor: e.actor,
        action: e.action,
        target: e.target,
        detail: e.detail,
    };
    Ok(rows.into_iter().map(entry).collect())
}

/// What [`record_with`] did.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Recorded {
    /// How it went, for whoever asked.
    pub said: Said,
    /// Of the roles asked about, those the config (as trusted) gives them
    /// and that they hold now.
    pub holds: BTreeSet<String>,
    /// Whether some of the config's sensitive settings wait for an Approve.
    pub held_back: bool,
    /// Whether giving and taking the roles couldn't be done at all.
    pub unapplied: bool,
}

/// Why someone doesn't hold a role they were given in the config yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Waits {
    /// It changes who's an admin: another admin approves it in the bot log first.
    Approval,
    /// The change couldn't be applied yet.
    Apply,
    /// Discord didn't let the bot give it just now.
    Discord,
}

impl Waits {
    /// Why `roles` aren't all held, from what [`record_with`] did; `None` if they are.
    pub fn of(r: &Recorded, roles: &[String]) -> Option<Self> {
        if roles.iter().all(|k| r.holds.contains(k)) {
            return None;
        }
        Some(if r.held_back {
            Self::Approval
        } else if r.unapplied {
            Self::Apply
        } else {
            Self::Discord
        })
    }

    /// For the person given the role: "… follows once {this}".
    pub fn for_them(self) -> &'static str {
        match self {
            Self::Approval => "You'll get the role once another admin has confirmed it.",
            Self::Apply => "You'll get the role soon; an admin is finishing it.",
            Self::Discord => "You'll get the role once an admin has given it to you.",
        }
    }
}

/// [`Host::record`] on an engine the host holds: make `ops`, save them as
/// `by`, then apply `user`'s holding of `roles` (nobody else's).
pub async fn record_with<R: Rest>(
    e: &mut Engine<R>,
    by: &By,
    ops: &[Op],
    user: Snowflake,
    roles: &[String],
) -> Result<Recorded> {
    let mut saved = Said::new();
    if !ops.is_empty() {
        let list = ops.to_vec();
        let edit = move |dir: &Path| {
            let change = chipsoc_config::ops::apply_ops(dir, &list)?;
            Ok(((), (!change.files.is_empty()).then_some(change.summary)))
        };
        let (_, result) = e.edit_with(edit, by).await?;
        if let Err(e) = result {
            tracing::warn!("saving to the config repo failed: {e:#}");
            saved =
                Said::new().text(format!("{} Saving to the config repo failed: {}", fmt::ATTENTION, fmt::error(&e)));
        }
    }
    let mut out = Recorded::default();
    let mut given = match e.apply_holders(user, roles).await {
        Ok(held) => {
            let wants = |k: &String| e.cfg.desired.holders.get(k).is_some_and(|w| w.contains(&user));
            let refused = |k: &String| held.iter().any(|h| &h.role == k && h.result.is_err());
            out.holds = roles.iter().filter(|k| wants(k) && !refused(k)).cloned().collect();
            given(&e.cfg, &held)
        }
        Err(e) => {
            tracing::warn!("applying who holds roles failed: {e:#}");
            out.unapplied = true;
            Said::new()
                .text(format!("{} Recorded, but couldn't apply it yet: {} Try ", fmt::ATTENTION, fmt::error(&e)))
                .code("/server plan")
                .text(".")
        }
    };
    if e.vetted.held.is_some() {
        out.held_back = true;
        given =
            given.then("It changes who's an admin, so it takes effect once another admin approves it in the bot log.");
    }
    out.said = given.then(saved);
    Ok(out)
}

fn given(cfg: &Config, held: &[Held]) -> Said {
    let mut out = Said::new();
    for h in held {
        let name = role_name(cfg, &h.role);
        out = out.then(match (&h.result, h.gave) {
            (Ok(()), true) => Said::new().text("Gave them ").strong(name).text("."),
            (Ok(()), false) => Said::new().text("Took ").strong(name).text(" away."),
            (Err(e), true) => Said::new()
                .text(format!("{} Couldn't give ", fmt::ATTENTION))
                .strong(name)
                .text(format!(": {}", fmt::error_str(e))),
            (Err(e), false) => Said::new()
                .text(format!("{} Couldn't take ", fmt::ATTENTION))
                .strong(name)
                .text(format!(" away: {}", fmt::error_str(e))),
        });
    }
    out
}

pub fn role_name(cfg: &Config, key: &str) -> String {
    cfg.desired.role(key).map_or_else(|| key.to_owned(), |r| r.name.clone())
}

/// A position's role, plus the admin role if it's an admin position.
pub fn with_admin(cfg: &Config, roles: &[String]) -> Vec<String> {
    let mut out = roles.to_vec();
    let admin = cfg.bot.positions.iter().any(|p| p.admin && roles.contains(&p.role));
    if let Some(a) = cfg.bot.admin_role.clone().filter(|_| admin) {
        out.push(a);
    }
    out
}

/// Give a role straight away, not through the config. `Err` says why not.
pub async fn give<H: Host>(h: &H, user: Snowflake, key: &str) -> Result<Said, Refusal> {
    let cfg = h.cfg();
    let name = role_name(&cfg, key);
    let Some(role) = cfg.desired.lock.role(key) else {
        return Err(Said::new().text(format!("{} ", fmt::ATTENTION)).strong(name).text(" isn't set up yet.").into());
    };
    let path = format!("guilds/{}/members/{user}/roles/{role}", h.guild());
    match h.rest().call(Method::Put, &path, None).await {
        Ok(_) => Ok(Said::new().text("Gave them ").strong(name).text(".")),
        Err(e) => {
            tracing::warn!("role grant failed: {e:#}");
            Err(Said::new()
                .text(format!("{} Couldn't give ", fmt::ATTENTION))
                .strong(name)
                .text(" (is the bot's role above it?).")
                .into())
        }
    }
}

/// Give roles straight away when the config can't record them; says what
/// happened, and whether every role went through. A role whose holders the
/// config manages isn't given this way: drift watch (in undo mode) or the
/// next apply would only take it back.
pub async fn give_unrecorded<H: Host>(h: &H, user: Snowflake, keys: &[String], why: &Said) -> (Said, bool) {
    let cfg = h.cfg();
    let taker = match cfg.bot.drift.mode {
        chipsoc_config::bot::DriftMode::Undo => "drift watch",
        _ => "the next apply",
    };
    let why = why.clone().mid_sentence();
    let mut out = Said::new().text("(Not recorded in the config: ").then(why).text(".)");
    let mut ok = true;
    // Asked of Discord once, however many roles.
    let live = live_for(h, keys).await;
    for key in keys {
        if cfg.desired.holders.contains_key(key) {
            ok = false;
            out = out.then(
                Said::new().text(format!("{} Didn't give ", fmt::ATTENTION)).strong(role_name(&cfg, key)).text(
                    format!(": the config decides who has it, and {taker} would take it back. Appoint them instead."),
                ),
            );
            continue;
        }
        if let Some(why) = grown(&cfg, &live, key) {
            ok = false;
            out = out.then(why);
            continue;
        }
        match give(h, user, key).await {
            Ok(said) => out = out.then(said),
            Err(why) => {
                ok = false;
                out = out.then(why.said());
            }
        }
    }
    (out, ok)
}

/// The dangerous permissions (see [`chipsoc_model::perms::DANGEROUS`]) each
/// role has in Discord right now: its own, and any a category or channel
/// overwrite allows it. Two requests, however many roles are then checked.
#[derive(Debug, Clone, Default)]
pub struct LivePowers(HashMap<Snowflake, u64>);

impl LivePowers {
    pub async fn fetch<H: Host>(h: &H) -> Result<Self> {
        use chipsoc_discord::wire;
        let bits = |v: &serde_json::Value| v.as_str().and_then(|p| p.parse::<u64>().ok()).unwrap_or(u64::MAX);
        let roles = h.rest().call(Method::Get, &format!("guilds/{}/roles", h.guild()), None).await?;
        let mut out: HashMap<Snowflake, u64> = roles
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|r| Some((wire::id(&r["id"])?, bits(&r["permissions"]))))
            .collect();
        let channels = h.rest().call(Method::Get, &format!("guilds/{}/channels", h.guild()), None).await?;
        for o in channels
            .as_array()
            .into_iter()
            .flatten()
            .flat_map(|c| c["permission_overwrites"].as_array().into_iter().flatten())
        {
            // Type 0 is a role overwrite.
            if o["type"].as_u64() == Some(0)
                && let Some(b) = wire::id(&o["id"]).and_then(|id| out.get_mut(&id))
            {
                *b |= bits(&o["allow"]);
            }
        }
        for b in out.values_mut() {
            *b &= chipsoc_model::perms::DANGEROUS;
        }
        Ok(Self(out))
    }

    /// A role's dangerous permissions, `None` if it isn't there.
    pub fn of(&self, id: Snowflake) -> Option<u64> {
        self.0.get(&id).copied()
    }

    /// [`powerful_now`], from what was fetched.
    pub fn powerful(&self, cfg: &Config, key: &str) -> bool {
        cfg.desired.lock.role(key).and_then(|id| self.of(id)).is_some_and(|b| b != 0)
    }

    /// [`powerful_beyond_config`], from what was fetched.
    pub fn beyond_config(&self, cfg: &Config, key: &str) -> bool {
        let meant = cfg.desired.power_bits(key);
        cfg.desired.lock.role(key).and_then(|id| self.of(id)).is_some_and(|b| b & !meant != 0)
    }
}

/// One role's [`LivePowers`].
async fn live_power<H: Host>(h: &H, id: Snowflake) -> Result<Option<u64>> {
    Ok(LivePowers::fetch(h).await?.of(id))
}

/// Whether a role has dangerous permissions in Discord right now, its own or
/// from an overwrite (someone may have changed it there since the config was
/// written), so the bot won't hand it out.
pub async fn powerful_now<H: Host>(h: &H, key: &str) -> Result<bool> {
    let Some(id) = h.cfg().desired.lock.role(key) else { return Ok(false) };
    Ok(live_power(h, id).await?.is_some_and(|b| b != 0))
}

/// Whether Discord gives a role dangerous permissions the config doesn't (it
/// was changed there since), for roles the config means to be powerful, such
/// as committee positions.
pub async fn powerful_beyond_config<H: Host>(h: &H, key: &str) -> Result<bool> {
    let cfg = h.cfg();
    if cfg.desired.lock.role(key).is_none() {
        return Ok(false);
    }
    Ok(LivePowers::fetch(h).await?.beyond_config(&cfg, key))
}

/// [`powerful_beyond_config`] as a refusal for people, for each of `keys`
/// (Discord is asked once, however many there are).
pub async fn grown_too_powerful<H: Host>(h: &H, keys: &[String]) -> Option<Said> {
    let live = live_for(h, keys).await;
    keys.iter().find_map(|k| grown(&h.cfg(), &live, k))
}

/// [`LivePowers`] if any of `keys` is set up (else there's nothing to ask).
async fn live_for<H: Host>(h: &H, keys: &[String]) -> Result<LivePowers> {
    let cfg = h.cfg();
    if !keys.iter().any(|k| cfg.desired.lock.role(k).is_some()) {
        return Ok(LivePowers::default());
    }
    LivePowers::fetch(h).await.inspect_err(|e| tracing::warn!("checking roles' permissions failed: {e:#}"))
}

/// [`grown_too_powerful`] for one role, from what was fetched.
fn grown(cfg: &Config, live: &Result<LivePowers>, key: &str) -> Option<Said> {
    let refused = |why: &str| {
        Some(Said::new().text(format!("{} Didn't give ", fmt::ATTENTION)).strong(role_name(cfg, key)).text(why))
    };
    match live {
        Ok(l) if !l.beyond_config(cfg, key) => None,
        Ok(_) => {
            refused(": in Discord it has moderator or admin permissions the config doesn't give it. Check the role (")
                .map(|s| s.code("/server plan").text(" shows it), then try again."))
        }
        Err(_) => refused(": couldn't check its permissions with Discord. Try again."),
    }
}

/// Whether `user` owns the server. The owner can do anything in Discord
/// anyway, so the "another admin has to" rules don't bind them; without this a
/// server with one admin could never appoint a second.
/// `false` when Discord can't say (the stricter rule holds).
pub async fn is_owner<H: Host>(h: &H, user: Snowflake) -> bool {
    owner_id(h).await.is_ok_and(|o| o == user)
}

/// Whether holding `role` makes someone an admin: the admin role, or an admin position's role.
pub fn grants_admin(cfg: &Config, role: &str) -> bool {
    cfg.bot.admin_role.as_deref() == Some(role) || cfg.bot.positions.iter().any(|p| p.admin && p.role == role)
}

/// Why the bot didn't give a role that's too powerful (or might be) to take
/// someone's word on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Withheld {
    /// In Discord it has moderator or admin permissions now.
    PowerfulNow { role: String },
    /// Its permissions couldn't be checked with Discord.
    Unchecked { role: String },
}

impl Withheld {
    /// For role `key`, from whether it's [`powerful_now`].
    pub fn from_check(cfg: &Config, key: &str, powerful: &Result<bool>) -> Option<Self> {
        let role = role_name(cfg, key);
        match powerful {
            Ok(false) => None,
            Ok(true) => Some(Self::PowerfulNow { role }),
            Err(e) => {
                tracing::warn!("checking a role's permissions failed: {e:#}");
                Some(Self::Unchecked { role })
            }
        }
    }
}

impl Withheld {
    pub fn said(&self) -> Said {
        match self {
            Self::PowerfulNow { role } => Said::new()
                .text("Didn't give ")
                .strong(role)
                .text(": in Discord it has moderator or admin permissions now. Check the role, then try again."),
            Self::Unchecked { role } => Said::new()
                .text("Didn't give ")
                .strong(role)
                .text(": couldn't check its permissions with Discord. Try again."),
        }
    }
}

impl std::fmt::Display for Withheld {
    /// As Discord markdown.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.said().markdown())
    }
}

/// [`powerful_now`] as a refusal for people: `Some` if the role mustn't be given.
pub async fn too_powerful_to_give<H: Host>(h: &H, key: &str) -> Option<Said> {
    let w = Withheld::from_check(&h.cfg(), key, &powerful_now(h, key).await)?;
    Some(Said::new().text(format!("{} ", fmt::ATTENTION)).then(w.said()))
}

/// What [`add_holder`] did.
#[derive(Debug, Clone, PartialEq)]
pub struct Added {
    pub said: Said,
    /// Why they don't hold every role yet, if they don't.
    pub waits: Option<Waits>,
}

/// Put someone in members.toml for `role` and give it to them (with the
/// admin role, for an admin position; nobody else's roles change). `Err`
/// says why it can't go through the config (the caller gives it directly),
/// or that the role has grown too powerful to give. The edit is checked
/// like any other [`Op`].
pub async fn add_holder<H: Host>(h: &H, role: &str, user: Snowflake) -> Result<Added, Refusal> {
    add_holder_unless(h, role, user).await.map_err(|(why, _)| why.into())
}

/// [`add_holder`], whose `Err` also says whether it's because the role
/// grew too powerful (so it mustn't be given some other way either).
pub(crate) async fn add_holder_unless<H: Host>(h: &H, role: &str, user: Snowflake) -> Result<Added, (Said, bool)> {
    let cfg = h.cfg();
    if !cfg.desired.holders.contains_key(role) {
        let why = Said::new().text("The config doesn't say who holds ").strong(role_name(&cfg, role));
        return Err((why, false));
    }
    if let Some(why) = h.unwritable() {
        return Err((Said::new().text(why), false));
    }
    let roles = with_admin(&cfg, &[role.to_owned()]);
    if let Some(why) = grown_too_powerful(h, &roles).await {
        return Err((why, true));
    }
    let listed = cfg.members.get(role).is_some_and(|u| u.contains(&user));
    let ops = if listed { Vec::new() } else { vec![Op::AddHolder { role: role.to_owned(), user }] };
    match h.record(&ops, user, &roles).await {
        Ok(r) => {
            let waits = Waits::of(&r, &roles);
            Ok(Added { said: Said::new().text("Recorded in the config.").then(r.said), waits })
        }
        Err(e) => {
            tracing::warn!("recording a holder failed: {e:#}");
            Err((Said::new().text(fmt::error(&e)), false))
        }
    }
}

/// Add someone to a project's leads and give them its Lead role. `Err` says
/// why not: it can't go through the config, or the Lead role has grown too
/// powerful to give (then nothing is recorded or given).
pub async fn add_lead<H: Host>(h: &H, slug: &str, lead_role: &str, user: Snowflake) -> Result<Said, Refusal> {
    add_lead_unless(h, slug, lead_role, user).await.map_err(|(why, _)| why.into())
}

/// [`add_lead`], whose `Err` also says whether it's because the Lead role
/// grew too powerful (so it mustn't be given some other way either).
pub(crate) async fn add_lead_unless<H: Host>(
    h: &H,
    slug: &str,
    lead_role: &str,
    user: Snowflake,
) -> Result<Said, (Said, bool)> {
    if let Some(why) = h.unwritable() {
        return Err((Said::new().text(why), false));
    }
    if let Some(why) = grown_too_powerful(h, &[lead_role.to_owned()]).await {
        return Err((why, true));
    }
    let led = h.cfg().projects.iter().any(|p| p.slug == slug && p.leads.contains(&user));
    let ops = if led { Vec::new() } else { vec![Op::AddLead { slug: slug.to_owned(), user }] };
    match h.record(&ops, user, &[lead_role.to_owned()]).await {
        Ok(r) => Ok(Said::new().text("Added to the project's leads.").then(r.said)),
        Err(e) => {
            tracing::warn!("recording a holder failed: {e:#}");
            Err((Said::new().text(fmt::error(&e)), false))
        }
    }
}

/// "Couldn't make @u a lead of **X**." heading a refusal.
fn couldnt_make_lead(name: &str, user: Snowflake) -> Said {
    Said::new().text("Couldn't make ").user(user).text(" a lead of ").strong(name).text(". ")
}

/// Make someone a lead of a project: checked, recorded, given and audited.
/// The mirror of [`remove_lead`].
pub async fn add_project_lead<H: Host>(h: &H, slug: &str, user: Snowflake, by: &Actor) -> Result<Said, Refusal> {
    let cfg = h.cfg();
    let p = cfg
        .projects
        .iter()
        .find(|p| p.slug == slug)
        .ok_or_else(|| Said::new().text("No project ").code(slug).text("."))?;
    if !p.lead_role {
        return Err(Said::new().strong(&p.name).text(" has no Lead role.").into());
    }
    let said = add_lead(h, slug, &p.lead_role_key(), user)
        .await
        .map_err(|why| couldnt_make_lead(&p.name, user).then(why.said()))?;
    audit(h, by, AuditAction::MadeLead, Some(slug), Some(&user.to_string())).await;
    Ok(said.then(queue::tell_lead(h, slug, user).await))
}

/// Who runs a server: its owner, whoever holds `admin_role`, and whoever
/// has Administrator or Manage Server from a role (`@everyone`'s too). The
/// one rule for `/server` and the bot's admin buttons, web sign-in, CLI
/// login and Tailscale shares.
///
/// It's read two ways, which agree. Here, from the server's roles and owner
/// (read from Discord once, then asked of each member with
/// [`Admins::include`], or of one person with [`is_admin`]). And, in the bot,
/// from the permissions Discord works out for each button press or command
/// ([`Admins::by_permissions`]): that needs no request, and there the owner
/// has every permission. Channel overwrites can't give or take either
/// permission, so where it was pressed doesn't matter.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Admins {
    guild: Snowflake,
    roles: std::collections::BTreeSet<Snowflake>,
    owner: Option<Snowflake>,
}

impl Admins {
    /// From the guild as Discord sends it (`GET guilds/{id}`).
    pub fn of(guild: Snowflake, g: &serde_json::Value, admin_role: Option<Snowflake>) -> Self {
        let bits = |r: &serde_json::Value| r["permissions"].as_str().and_then(|p| p.parse::<u64>().ok()).unwrap_or(0);
        let mut roles: std::collections::BTreeSet<Snowflake> = g["roles"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|r| bits(r) & Self::POWERS != 0)
            .filter_map(|r| chipsoc_discord::wire::id(&r["id"]))
            .collect();
        roles.extend(admin_role);
        Self { guild, roles, owner: chipsoc_discord::wire::id(&g["owner_id"]) }
    }

    pub async fn fetch(rest: &impl Rest, guild: Snowflake, admin_role: Option<Snowflake>) -> Result<Self> {
        let g = rest.call(Method::Get, &format!("guilds/{guild}"), None).await?;
        Ok(Self::of(guild, &g, admin_role))
    }

    /// Administrator and Manage Server: either makes an admin.
    const POWERS: u64 = (1 << chipsoc_model::perms::ADMINISTRATOR) | (1 << 5);

    /// Whether someone holding `roles` is an admin. `@everyone`'s id is the server's.
    pub fn include(&self, user: Snowflake, roles: &[Snowflake]) -> bool {
        self.owner == Some(user) || self.roles.contains(&self.guild) || roles.iter().any(|r| self.roles.contains(r))
    }

    /// The same rule, from the permissions Discord worked out for someone
    /// holding `roles` (as an interaction gives them).
    pub fn by_permissions(permissions: u64, roles: &[Snowflake], admin_role: Option<Snowflake>) -> bool {
        permissions & Self::POWERS != 0 || admin_role.is_some_and(|a| roles.contains(&a))
    }
}

/// Whether `user` is an admin of `guild` (see [`Admins`]); not if they aren't in it.
pub async fn is_admin(
    rest: &impl Rest,
    guild: Snowflake,
    admin_role: Option<Snowflake>,
    user: Snowflake,
) -> Result<bool> {
    let member = match rest.call(Method::Get, &format!("guilds/{guild}/members/{user}"), None).await {
        Ok(m) => m,
        Err(err) if chipsoc_discord::ApiError::status_of(&err) == Some(404) => return Ok(false),
        Err(err) => return Err(err),
    };
    let held = chipsoc_discord::wire::ids(&member["roles"]);
    if admin_role.is_some_and(|a| held.contains(&a)) {
        return Ok(true);
    }
    Ok(Admins::fetch(rest, guild, admin_role).await?.include(user, &held))
}

/// Why a DM didn't arrive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DmError {
    /// They don't take DMs from the server's members (or bots).
    Closed,
    /// Anything else, in words fit for the log.
    Failed(String),
}

impl DmError {
    /// From a failed send: `closed` says whether this error means their DMs are closed.
    fn from_send(e: &anyhow::Error, closed: bool) -> Self {
        if closed {
            return Self::Closed;
        }
        tracing::warn!("a DM failed: {e:#}");
        Self::Failed(fmt::error(e).trim_end_matches('.').to_owned())
    }
}

impl std::fmt::Display for DmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Closed => f.write_str("their DMs are closed"),
            Self::Failed(why) => f.write_str(why),
        }
    }
}

/// DM someone, pinging nobody, with `components` (rows of buttons, as
/// Discord takes them) if any. Their ids need the server in them, as DMs
/// have none: see [`in_dm`]. The one way the bot DMs.
pub async fn dm<H: Host>(
    h: &H,
    user: Snowflake,
    text: &str,
    components: Option<serde_json::Value>,
) -> Result<(), DmError> {
    let send = async {
        let ch = h.rest().call(Method::Post, "users/@me/channels", Some(json!({ "recipient_id": user }))).await?;
        let path = format!("channels/{}/messages", chipsoc_discord::wire::s(&ch["id"]));
        let mut body = json!({ "content": truncate(text, MAX_CONTENT), "allowed_mentions": { "parse": [] } });
        if let Some(c) = components {
            body["components"] = c;
        }
        h.rest().call(Method::Post, &path, Some(body)).await
    };
    send.await.map(drop).map_err(|e| {
        let closed = chipsoc_discord::ApiError::code_of(&e) == Some(chipsoc_discord::CANNOT_DM);
        DmError::from_send(&e, closed)
    })
}

/// A button or form id for a DM: `id` with the server it's for after an
/// `@`, as DMs have no server of their own. See [`server_in_dm`].
pub fn in_dm(id: &str, guild: Snowflake) -> String {
    format!("{id}@{guild}")
}

/// The id and server of one made by [`in_dm`] (`None` if it wasn't).
pub fn server_in_dm(id: &str) -> Option<(&str, Snowflake)> {
    let (id, g) = id.rsplit_once('@')?;
    Some((id, Snowflake(g.parse().ok()?)))
}

/// Post in a channel without pinging anyone.
pub async fn say<H: Host>(h: &H, channel: Snowflake, text: &str) -> Result<()> {
    let body = json!({ "content": truncate(text, MAX_CONTENT), "allowed_mentions": { "parse": [] } });
    h.rest().call(Method::Post, &format!("channels/{channel}/messages"), Some(body)).await.map(drop)
}

/// The bot log (or the people log if there's no bot log): config changes, drift, partner sync, problems.
pub async fn log_bot<H: Host>(h: &H, text: &str) {
    let cfg = h.cfg();
    let log = cfg.bot.log.bot.as_deref().or(cfg.bot.log.people.as_deref()).and_then(|k| cfg.desired.lock.channel(k));
    let Some(ch) = log else { return };
    if let Err(e) = say(h, ch, text).await {
        tracing::warn!("posting to the bot log failed: {e:#}");
    }
}

/// The people log: edits, deletes, joins, leaves, decisions.
pub async fn log_people<H: Host>(h: &H, text: &str) {
    let cfg = h.cfg();
    let Some(ch) = cfg.bot.log.people.as_deref().and_then(|k| cfg.desired.lock.channel(k)) else { return };
    if let Err(e) = say(h, ch, text).await {
        tracing::warn!("posting to the people log failed: {e:#}");
    }
}

pub async fn audit<H: Host>(h: &H, by: &Actor, action: AuditAction, target: Option<&str>, detail: Option<&str>) {
    if let Err(e) = h.db().audit(now(), Some(&by.key()), action.as_str(), target, detail).await {
        tracing::warn!("audit log failed: {e:#}");
    }
}

use fmt::{MAX_CONTENT, truncate};

/// Now, in Unix seconds.
pub fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

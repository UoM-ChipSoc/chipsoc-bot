//! The join flow's effects, for any front end: giving the roles a member's
//! answers come to (and taking those the flow gave that no longer apply), and
//! the one-off DM to members who haven't been through it.

use std::collections::BTreeSet;
use std::time::Duration;

use anyhow::Result;
use chipsoc_config::Snowflake;
use chipsoc_config::join::{Answers, Outcome, role_changes};
use chipsoc_discord::{Method, Rest};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{Actor, AuditAction, Host, audit, role_name};

/// Kinds of note kept in the store's `prompted` table, one per member.
pub const NOTIFIED: &str = "join:notify";
pub const REMINDED: &str = "join:reminder";
/// Listed in the bot log as waiting to finish.
pub const WAITING: &str = "join:waiting";

/// What confirming did.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Confirmed {
    /// Role names given and taken.
    pub added: Vec<String>,
    pub removed: Vec<String>,
    /// What didn't go through, as sentences.
    pub problems: Vec<String>,
    #[serde(skip)]
    pub outcome: Outcome,
}

/// Role keys someone holds, read from Discord.
pub async fn held<H: Host>(h: &H, user: Snowflake) -> Result<BTreeSet<String>> {
    let m = h.rest().call(Method::Get, &format!("guilds/{}/members/{user}", h.guild()), None).await?;
    let cfg = h.cfg();
    let ids = chipsoc_discord::wire::ids(&m["roles"]);
    Ok(ids.into_iter().filter_map(|id| cfg.desired.lock.role_key(id).map(str::to_owned)).collect())
}

/// Give what finished answers come to: their roles and the access role, and
/// take the flow's roles they no longer pick (never any other role, nor ones
/// partner sync gives). Saves the answers. `Err` says why nothing was done.
pub async fn confirm<H: Host>(h: &H, user: Snowflake, answers: &Answers) -> Result<Confirmed, String> {
    let cfg = h.cfg();
    let Some(flow) = &cfg.join else { return Err("The join flow isn't set up here any more.".into()) };
    let Some(outcome) = flow.outcome(answers) else {
        return Err("Some questions aren't answered (or have changed); please go through them again.".into());
    };
    let held = held(h, user).await.map_err(|e| {
        tracing::warn!("reading a member's roles failed: {e:#}");
        "Couldn't read your roles just now; please try again.".to_owned()
    })?;
    let partner: BTreeSet<String> =
        cfg.federation.peers.iter().flat_map(|p| &p.role_map).map(|m| m.ours.clone()).collect();
    let (add, remove) = role_changes(flow, &outcome, &held, &partner);
    let mut done = Confirmed::default();
    // Moderator or admin permissions in Discord now, the role's own or from an
    // overwrite. Unknown means unchecked: don't hand it out.
    let live = crate::LivePowers::fetch(h).await.map_err(|e| tracing::warn!("reading the roles failed: {e:#}")).ok();
    let powerful = |id: Snowflake| live.as_ref().and_then(|p| p.of(id)).is_none_or(|b| b != 0);
    for (key, adding) in add.iter().map(|k| (k, true)).chain(remove.iter().map(|k| (k, false))) {
        let name = role_name(&cfg, key);
        let Some(id) = cfg.desired.lock.role(key) else {
            done.problems.push(format!("**{name}** isn't set up yet."));
            continue;
        };
        // Whatever the files say (a pushed join.toml is checked, but never trusted):
        // the admin role, positions and members.toml's roles are admins' to give.
        if adding && (cfg.admins_decide(key) || powerful(id)) {
            done.problems.push(format!("Didn't give **{name}**: it's a role admins decide on."));
            continue;
        }
        let path = format!("guilds/{}/members/{user}/roles/{id}", h.guild());
        let method = if adding { Method::Put } else { Method::Delete };
        match h.rest().call(method, &path, None).await {
            Ok(_) if adding => done.added.push(name),
            Ok(_) => done.removed.push(name),
            Err(e) => {
                tracing::warn!("changing a join role failed: {e:#}");
                let verb = if adding { "give" } else { "take away" };
                done.problems.push(format!("Couldn't {verb} **{name}** (is the bot's role above it?)."));
            }
        }
    }
    let saved = h.db().set_join_answers(&user.to_string(), &outcome.answers.to_json(), true, crate::now()).await;
    if let Err(e) = saved {
        tracing::warn!("saving join answers failed: {e:#}");
        done.problems.push("Saving your answers failed, so next time they won't be filled in.".into());
    }
    done.outcome = outcome;
    Ok(done)
}

/// Why the bot can't set someone's nickname: Discord lets no bot rename the
/// server owner, nor anyone whose highest role is at or above the bot's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Unnameable {
    Owner,
    Above,
}

/// Whether Discord would refuse the bot changing `user`'s nickname, asked
/// before trying. `None` when it would let it, or that can't be told.
pub async fn unnameable<H: Host>(h: &H, user: Snowflake) -> Result<Option<Unnameable>> {
    let g = h.rest().call(Method::Get, &format!("guilds/{}", h.guild()), None).await?;
    if chipsoc_discord::wire::id(&g["owner_id"]) == Some(user) {
        return Ok(Some(Unnameable::Owner));
    }
    let me = h.rest().call(Method::Get, "users/@me", None).await?;
    let Some((bot_role, _)) = chipsoc_discord::bot_role_in(&g, &me["id"]) else { return Ok(None) };
    let roles: Vec<&Value> = g["roles"].as_array().into_iter().flatten().collect();
    let position = |id: Snowflake| {
        let role = roles.iter().find(|r| chipsoc_discord::wire::id(&r["id"]) == Some(id));
        role.and_then(|r| r["position"].as_i64())
    };
    let Some(bot) = position(bot_role) else { return Ok(None) };
    let m = h.rest().call(Method::Get, &format!("guilds/{}/members/{user}", h.guild()), None).await?;
    let top = chipsoc_discord::wire::ids(&m["roles"]).into_iter().filter_map(position).max().unwrap_or(0);
    Ok((top >= bot).then_some(Unnameable::Above))
}

/// What someone told the bot about themselves: their profile and their
/// join-flow answers.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct About {
    pub id: Snowflake,
    /// The name Discord shows here; empty if they aren't in the server.
    pub shown: String,
    pub name: String,
    pub surname: String,
    pub company: String,
    /// The join flow's questions on their path, with what they said.
    pub answers: Vec<Said>,
    /// When they last finished the join flow.
    pub finished: Option<i64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Said {
    pub question: String,
    pub answer: String,
}

impl About {
    /// Whether the bot knows anything about them.
    pub fn is_empty(&self) -> bool {
        self.name.is_empty() && self.company.is_empty() && self.answers.is_empty()
    }
}

/// [`About`] `user`, from the bot's database (and their name from Discord).
pub async fn about<H: Host>(h: &H, user: Snowflake) -> Result<About> {
    let key = user.to_string();
    let p = h.db().profile(&key).await?.unwrap_or_default();
    let record = h.db().join_record(&key).await?;
    let cfg = h.cfg();
    let answers = match (&cfg.join, &record) {
        (Some(j), Some(r)) => j.said(&Answers::parse(&r.answers)),
        _ => Vec::new(),
    };
    let shown = match h.rest().call(Method::Get, &format!("guilds/{}/members/{user}", h.guild()), None).await {
        Ok(m) => chipsoc_discord::wire::shown_name(&m).unwrap_or_else(|| key.clone()),
        Err(e) if chipsoc_discord::ApiError::status_of(&e) == Some(404) => String::new(),
        Err(e) => return Err(e),
    };
    Ok(About {
        id: user,
        shown,
        name: p.name,
        surname: p.surname,
        company: p.company,
        answers: answers.into_iter().map(|(question, answer)| Said { question, answer }).collect(),
        finished: record.and_then(|r| r.finished_at),
    })
}

/// Who someone is in one line, for a review card: their join answers
/// ("University of Manchester · EEE · Year 3 · …", see
/// [`chipsoc_config::join::Join::summary`]), or without any, the names of
/// the roles they hold that say (those the join flow gives, if there is
/// one). `None` if there's nothing to say.
pub async fn summary<H: Host>(h: &H, user: Snowflake) -> Option<String> {
    let cfg = h.cfg();
    if let Some(j) = &cfg.join {
        match h.db().join_record(&user.to_string()).await {
            Ok(Some(r)) => {
                let parts = j.summary(&Answers::parse(&r.answers));
                if !parts.is_empty() {
                    return Some(parts.join(" · "));
                }
            }
            Ok(None) => {}
            Err(e) => tracing::warn!("reading join answers failed: {e:#}"),
        }
    }
    let held = match held(h, user).await {
        Ok(held) => held,
        Err(e) => {
            tracing::warn!("reading a member's roles failed: {e:#}");
            return None;
        }
    };
    let telling = |k: &&String| match &cfg.join {
        Some(j) => j.option_roles().contains(*k),
        None => k.as_str() != "everyone" && !cfg.desired.role(k).is_some_and(|r| r.managed),
    };
    let names: Vec<String> = held.iter().filter(telling).map(|k| role_name(&cfg, k)).collect();
    (!names.is_empty()).then(|| names.join(" · "))
}

/// What [`notify`] did, or would do.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Notified {
    pub sent: usize,
    /// Their DMs are closed.
    pub closed: usize,
    pub failed: usize,
}

/// How the flow is going, from the bot's database alone.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Progress {
    /// Members who have finished it (some may have left since).
    pub finished: usize,
    /// Members sent the one-off notice.
    pub notified: usize,
    /// New members past `remind_after_hours` who haven't finished (gate on).
    pub waiting: Vec<Snowflake>,
}

pub async fn progress<H: Host>(h: &H) -> Result<Progress> {
    let records = h.db().join_records().await?;
    let finished: BTreeSet<String> =
        records.into_iter().filter(|r| r.finished_at.is_some()).map(|r| r.user_id).collect();
    let waiting = h.db().prompted(WAITING).await?;
    Ok(Progress {
        finished: finished.len(),
        notified: h.db().prompted(NOTIFIED).await?.len(),
        waiting: waiting.iter().filter(|u| !finished.contains(*u)).filter_map(|u| u.parse().ok()).collect(),
    })
}

/// Who the one-off notice would go to, and what it says.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Notice {
    /// Members it goes to: not through the flow, never sent it before.
    pub members: usize,
    pub text: String,
}

/// [`Notice`] for now. `Err` if there's no join flow.
pub async fn preview<H: Host>(h: &H) -> Result<Notice> {
    if h.cfg().join.is_none() {
        anyhow::bail!("there's no join.toml");
    }
    Ok(Notice { members: notify_targets(h).await?.len(), text: notice(h) })
}

/// The default DM for members who haven't been through the flow.
pub fn notify_text(name: &str) -> String {
    format!(
        "Hi! 👋 {name} now asks everyone a few quick questions about themselves, so people know who's who and \
         you see the right channels. It takes a minute:"
    )
}

/// Members who'd get [`notify`]'s DM: not bots, not finished the flow, not sent it before.
pub async fn notify_targets<H: Host>(h: &H) -> Result<Vec<Snowflake>> {
    let finished: BTreeSet<String> =
        h.db().join_records().await?.into_iter().filter(|r| r.finished_at.is_some()).map(|r| r.user_id).collect();
    let told = h.db().prompted(NOTIFIED).await?;
    let mut out = Vec::new();
    for m in members(h).await?.iter() {
        let Some(id) = chipsoc_discord::wire::id(&m["user"]["id"]) else { continue };
        let key = id.to_string();
        if m["user"]["bot"] != true && !finished.contains(&key) && !told.contains(&key) {
            out.push(id);
        }
    }
    Ok(out)
}

/// DM each of `users` once ever (noted before sending, so it's never sent
/// twice) with the join flow's button, one every `pace`.
pub async fn notify<H: Host>(h: &H, users: &[Snowflake], pace: Duration) -> Notified {
    let text = notice(h);
    let mut out = Notified::default();
    for (n, user) in users.iter().enumerate() {
        match h.db().first_prompt(&user.to_string(), NOTIFIED, crate::now()).await {
            Ok(true) => {}
            Ok(false) => continue,
            Err(e) => {
                tracing::warn!("noting a join DM failed: {e:#}");
                out.failed += 1;
                continue;
            }
        }
        if n > 0 {
            tokio::time::sleep(pace).await;
        }
        match dm_start(h, *user, &text).await {
            Ok(()) => out.sent += 1,
            Err(crate::DmError::Closed) => out.closed += 1,
            Err(_) => out.failed += 1,
        }
    }
    out
}

/// What the notice says: the config's text or [`notify_text`].
pub fn notice<H: Host>(h: &H) -> String {
    let cfg = h.cfg();
    cfg.join.as_ref().and_then(|j| j.notify_message.clone()).unwrap_or_else(|| notify_text(&cfg.bot.name))
}

/// Send the notice to everyone [`notify_targets`] finds, `by`'s doing: audited,
/// and the bot log hears how it went.
/// How long [`send_notice`] waits between DMs, so Discord doesn't take them for spam.
pub const NOTICE_PACE: Duration = Duration::from_secs(2);

pub async fn send_notice<H: Host>(h: &H, by: &Actor, pace: Duration) -> Result<Notified> {
    let targets = notify_targets(h).await?;
    audit(h, by, AuditAction::JoinNotice, None, Some(&targets.len().to_string())).await;
    let n = notify(h, &targets, pace).await;
    let text = format!(
        "📨 Join flow notice from {}: sent {}, {} with DMs closed, {} failed.",
        by.mention(),
        n.sent,
        n.closed,
        n.failed
    );
    crate::log_bot(h, &text).await;
    Ok(n)
}

/// The id of the runtime's button that starts the flow (`do:join`).
pub const START: &str = "do:join";

/// The button that starts the flow from a DM, with the server it's for (DMs have none of their own).
pub fn start_button(guild: Snowflake, label: &str) -> Value {
    let id = crate::in_dm(START, guild);
    json!([{ "type": 1, "components": [{ "type": 2, "style": 1, "label": label, "custom_id": id }] }])
}

/// DM someone with the button that starts the flow.
pub async fn dm_start<H: Host>(h: &H, user: Snowflake, text: &str) -> Result<(), crate::DmError> {
    crate::dm(h, user, text, Some(start_button(h.guild(), "Get started"))).await
}

/// Everyone in the server, as Discord lists members. Errors abort.
pub async fn members<H: Host>(h: &H) -> Result<std::sync::Arc<Vec<Value>>> {
    h.rest().members(h.guild()).await
}

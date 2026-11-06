//! Drift watch: compare Discord with the config soon after Discord changes, and
//! every 20 minutes anyway. In undo mode, changes made in Discord alone are put
//! back and the bot log says how to make them properly; the rest is reported.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use chipsoc_config::Snapshot;
use chipsoc_config::bot::DriftMode;
use chipsoc_engine::Options;
use chipsoc_model::{Action, Cause, ObjectKind, Plan, Snowflake};
use tokio::sync::Notify;
use twilight_gateway::Event;

use crate::alarm::Alarm;
use crate::debounce::Debounce;
use chipsoc_admin::fmt;

use crate::{Ctx, control, reload, ui};

const EVERY: Duration = Duration::from_secs(20 * 60);
/// Check this long after the last of a burst of events…
const WAIT: Duration = Duration::from_secs(15);
/// …or this long after the first, if they keep coming.
const MAX_WAIT: Duration = Duration::from_secs(60);
/// Bullets shown per section.
const SHOWN: usize = 10;
/// Undos tried again in a row when a config push or another apply gets in first.
const RETRIES: u32 = 3;

/// Events that may mean drift, gathered into one check.
pub struct Trigger {
    state: Mutex<Debounce>,
    wake: Notify,
    /// Undos in a row refused because the config or server moved meanwhile.
    retries: std::sync::atomic::AtomicU32,
}

impl Default for Trigger {
    fn default() -> Self {
        Self { state: Mutex::new(Debounce::new(WAIT, MAX_WAIT)), wake: Notify::new(), retries: Default::default() }
    }
}

impl Trigger {
    fn state(&self) -> std::sync::MutexGuard<'_, Debounce> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn poke(&self) {
        if self.state().poke(Instant::now()) {
            self.wake.notify_one();
        }
    }

    /// Check again soon: an undo was refused because something moved first.
    /// False after [`RETRIES`] in a row, so a server that keeps changing is
    /// reported instead.
    fn again(&self) -> bool {
        use std::sync::atomic::Ordering;
        if self.retries.fetch_add(1, Ordering::Relaxed) >= RETRIES {
            self.retries.store(0, Ordering::Relaxed);
            return false;
        }
        self.state().again(Instant::now());
        self.wake.notify_one();
        true
    }

    /// Ignore events for a while: the bot is changing Discord itself.
    pub fn quiet_for(&self, d: Duration) {
        self.state().quiet(Instant::now() + d);
    }
}

/// Whether an event may mean Discord no longer matches the config.
pub fn matters(ctx: &Ctx, e: &Event) -> bool {
    match e {
        Event::GuildUpdate(_)
        | Event::RoleCreate(_)
        | Event::RoleUpdate(_)
        | Event::RoleDelete(_)
        | Event::ChannelCreate(_)
        | Event::ChannelUpdate(_)
        | Event::ChannelDelete(_)
        | Event::AutoModerationRuleCreate(_)
        | Event::AutoModerationRuleUpdate(_)
        | Event::AutoModerationRuleDelete(_) => true,
        Event::ThreadUpdate(t) => {
            let forum = ctx.cfg().bot.projects.as_ref().and_then(|p| ctx.channel(&p.forum));
            forum.is_some() && t.parent_id == forum
        }
        Event::MemberUpdate(m) => {
            let roles: Vec<u64> = m.roles.iter().map(|r| r.get()).collect();
            holdings_differ(&ctx.cfg().desired, m.user.id.get(), &roles)
        }
        _ => false,
    }
}

/// Whether a member holds a managed role the config doesn't give them, or lacks one it does.
fn holdings_differ(want: &Snapshot, user: u64, roles: &[u64]) -> bool {
    want.holders.iter().any(|(key, people)| {
        want.lock.role(key).is_some_and(|id| people.contains(&Snowflake(user)) != roles.contains(&id.0))
    })
}

pub async fn watch(ctx: Arc<Ctx>) {
    let mut last = String::new();
    let mut alarm = Alarm::default();
    let mut backstop = tokio::time::interval(EVERY);
    backstop.tick().await; // let the gateway settle first
    loop {
        next_check(&ctx, &mut backstop).await;
        ctx.drift.state().done();
        backstop.reset();
        let checked = check(&ctx).await;
        if checked.is_ok() && alarm.ok() {
            ctx.log_bot(&format!("{} **Drift watch can check the server again.**", fmt::DONE)).await;
        }
        match checked {
            // The bot (or the web UI) is applying: what's half done isn't drift. The
            // events its changes cause bring the next check.
            Ok(None) => {}
            Ok(Some((report, undid))) if undid || report != last => {
                if !report.is_empty() {
                    ctx.log_bot(&report).await;
                }
                last = report;
            }
            Ok(_) => {}
            // Never silently: until it's fixed, nothing notices changes made in Discord.
            Err(e) => {
                tracing::warn!("drift check failed: {e:#}");
                if alarm.failed(Instant::now()) {
                    ctx.log_bot(&format!(
                        "{} **Drift watch can't compare Discord with the config**, so changes made in Discord go \
                         unnoticed.\n• Why: {}\n• This repeats every hour until it's fixed.",
                        fmt::ATTENTION,
                        fmt::error(&e)
                    ))
                    .await;
                }
            }
        }
    }
}

/// Wait for the backstop timer or a debounced burst of events.
async fn next_check(ctx: &Ctx, backstop: &mut tokio::time::Interval) {
    loop {
        let due = ctx.drift.state().due();
        let at = tokio::time::Instant::from_std(due.unwrap_or_else(Instant::now));
        tokio::select! {
            _ = backstop.tick() => return,
            _ = tokio::time::sleep_until(at), if due.is_some() => return,
            _ = ctx.drift.wake.notified() => {}
        }
    }
}

/// Reload config, undo changes made in Discord (in undo mode), and describe the
/// rest. Also says whether anything was undone. None while the bot or another
/// program is applying, when there's nothing to check yet.
pub(crate) async fn check(ctx: &Ctx) -> Result<Option<(String, bool)>> {
    let (plan, mode) = {
        // Planned holding the bot's change lock and the apply lease; undoing takes them again.
        let Ok(_one) = ctx.changing.try_lock() else { return Ok(None) };
        let Ok(_no_apply) = chipsoc_engine::Lease::take(&ctx.cfg().dir) else { return Ok(None) };
        let e = control::engine(ctx).await?;
        // A config the bot refuses (it has problems) is never planned from: that's said in the bot log already.
        if !reload::use_config(ctx, e.cfg.clone()).await {
            return Ok(None);
        }
        (e.plan(Options::default()).await?, e.cfg.bot.drift.mode)
    };
    if plan.is_empty() {
        return Ok(Some((String::new(), false)));
    }
    let u = if mode == DriftMode::Undo { undo(ctx, &plan).await } else { Undone::default() };
    // A push (pulled while undoing) or another apply got in first: plan again
    // with it soon, rather than waiting for the next event.
    if u.refused && ctx.drift.again() {
        return Ok(None);
    }
    if !u.refused {
        ctx.drift.retries.store(0, std::sync::atomic::Ordering::Relaxed);
    }
    let (undone, stale) = (u.done, u.stale);
    let mut report = describe(&ctx.cfg(), &plan, &undone, stale, mode);
    if u.unconfirmed > 0 {
        report = ui::truncate(
            &format!(
                "{report}\n\n{} made in Discord {} left as {}: the audit log doesn't show who made {}. Edit the \
                 config, or `/server apply` to put the config back.",
                fmt::count(u.unconfirmed, "change", "changes"),
                if u.unconfirmed == 1 { "was" } else { "were" },
                if u.unconfirmed == 1 { "it is" } else { "they are" },
                if u.unconfirmed == 1 { "it" } else { "them" },
            ),
            ui::MAX_CONTENT - 1,
        );
    }
    if let Some(why) = u.failed {
        report = ui::truncate(
            &format!("{} **Couldn't put back changes made in Discord:** {why}\n\n{report}", fmt::FAILED),
            ui::MAX_CONTENT - 1,
        );
    }
    Ok(Some((report, !undone.is_empty())))
}

/// What [`undo`] did and left.
#[derive(Debug, Default)]
struct Undone {
    done: Vec<Action>,
    /// Left because the bot made the change itself.
    stale: usize,
    /// Left because the audit log doesn't show a person making it.
    unconfirmed: usize,
    failed: Option<String>,
    /// Refused as stale or busy: nothing was done, and planning again may work.
    refused: bool,
}

/// Put back what was changed in Discord alone, where the audit log shows a
/// person (not the bot) changed it lately.
async fn undo(ctx: &Ctx, plan: &Plan) -> Undone {
    // Without the audit log the bot's changes look like anyone's: revert nothing.
    let Some(who) = who_changed(ctx).await else { return Undone::default() };
    let admin = ctx.cfg().bot.admin_role.clone();
    let (steps, stale, unconfirmed) = undo_steps(plan, &who, ctx.me.get(), admin.as_deref());
    let mut out = Undone { stale, unconfirmed, ..Default::default() };
    if steps.is_empty() {
        return out;
    }
    match control::apply(ctx, &Plan { actions: steps, ..plan.clone() }).await {
        Ok(r) => out.done = r.outcomes.into_iter().filter(|o| o.result.is_ok()).map(|o| o.action).collect(),
        Err(e) => {
            tracing::warn!("undoing drift failed: {e:#}");
            out.refused = e.downcast_ref::<chipsoc_engine::Refused>().is_some();
            out.failed = Some(fmt::error(&e));
        }
    }
    out
}

/// Every edit made in Discord alone whose latest change in the audit log was
/// made by someone other than the bot. Objects the bot changed last mean the
/// config here is older than what was applied; ones the log doesn't show
/// can't be confirmed (lock.toml says what was applied, and a pushed one could
/// make a config change look like Discord's). Who holds the admin role is
/// never changed behind people's backs, only reported.
/// Returns the steps and how many were left as stale and as unconfirmed.
fn undo_steps(plan: &Plan, who: &Changed, me: u64, admin: Option<&str>) -> (Vec<Action>, usize, usize) {
    let (mut stale, mut unconfirmed) = (0, 0);
    let steps = plan
        .actions
        .iter()
        .filter(|a| match a {
            Action::Edit { kind: ObjectKind::Holders, key, .. } if admin == Some(key.as_str()) => false,
            Action::Edit { cause: Cause::Discord, kind, id, .. } => {
                let by = if *kind == ObjectKind::Holders { who.holders.get(&id.0) } else { who.objects.get(&id.0) };
                match by {
                    Some(u) if *u == me => stale += 1,
                    Some(_) => return true,
                    None => unconfirmed += 1,
                }
                false
            }
            _ => false,
        })
        .cloned()
        .collect();
    (steps, stale, unconfirmed)
}

/// A part of the report: its bold head line, the steps, and what to do.
fn section(head: &str, lines: &[String], hint: &str) -> String {
    let mut out = vec![head.to_owned()];
    out.extend(fmt::bullets(lines, SHOWN));
    out.push(hint.to_owned());
    out.join("\n")
}

/// The drift report: what was put back, what was changed in Discord, what
/// needs a person, and what's waiting to be applied.
fn describe(cfg: &chipsoc_config::Config, plan: &Plan, undone: &[Action], stale: usize, mode: DriftMode) -> String {
    let names = fmt::Names::new(cfg);
    let left: Vec<Action> = plan.actions.iter().filter(|a| !undone.contains(a)).cloned().collect();
    // Sorted the way every front end shows drift.
    let sorted = chipsoc_admin::drift(&Plan { actions: left.clone(), ..Plan::default() });
    let steps = |lines: &[chipsoc_admin::Line]| -> Vec<String> {
        lines.iter().map(|l| fmt::step(&left[l.step], &names)).collect()
    };
    let changes = |n: usize| fmt::count(n, "change", "changes");
    let mut parts = Vec::new();
    if !undone.is_empty() {
        let lines: Vec<String> = undone.iter().map(|a| fmt::step(a, &names)).collect();
        let head = format!("{} **Put back {} made in Discord.**", fmt::DONE, changes(undone.len()));
        let hint = "To change these for good, change the config (the dashboard, `/server`, or the config repo): \
                    changes made only in Discord are put back.";
        parts.push(section(&head, &lines, hint));
    }
    let discord = steps(&sorted.discord);
    if !discord.is_empty() {
        let hint = match mode {
            DriftMode::Report => {
                "Keep Discord's version with `/server pull`, or put the config back with `/server apply`."
            }
            DriftMode::Undo => {
                "Drift watch puts these back. `/server pull` keeps Discord's version only if run first, so changing \
                 the config is safer."
            }
        };
        let head = format!("{} **{} made in Discord:**", fmt::ATTENTION, changes(discord.len()));
        parts.push(section(&head, &discord, hint));
    }
    let person = steps(&sorted.person);
    if !person.is_empty() {
        let head = format!(
            "{} **{} {} a person** (changed on both sides, or not known):",
            fmt::ATTENTION,
            changes(person.len()),
            if person.len() == 1 { "needs" } else { "need" }
        );
        parts.push(section(&head, &person, "Make the config say what you want, then `/server apply`."));
    }
    let waiting = sorted.waiting.len();
    if waiting > 0 {
        parts.push(format!(
            "{} {} in the config {} waiting to be applied: `/server plan`.",
            fmt::NEW_CONFIG,
            changes(waiting),
            if waiting == 1 { "is" } else { "are" }
        ));
    }
    if stale > 0 {
        parts.push(format!(
            "{} {} of these {} last changed by the bot itself, so the config on the bot's server is probably older \
             than what was applied. Deploy the latest config.",
            fmt::ATTENTION,
            stale,
            if stale == 1 { "was" } else { "were" }
        ));
    }
    ui::truncate(&parts.join("\n\n"), ui::MAX_CONTENT - 1)
}

/// Who made the latest change to each object, from the audit log's last 100 entries.
#[derive(Debug, Default)]
struct Changed {
    /// Object id → user id.
    objects: HashMap<u64, u64>,
    /// Role id → who last gave it to or took it from someone.
    holders: HashMap<u64, u64>,
}

/// [`Changed`], or `None` if the audit log couldn't be read.
async fn who_changed(ctx: &Ctx) -> Option<Changed> {
    use chipsoc_discord::wire::id;
    let log = ctx.get(&format!("guilds/{}/audit-logs?limit=100", ctx.guild)).await.ok()?;
    let mut out = Changed::default();
    for e in log["audit_log_entries"].as_array().into_iter().flatten() {
        let Some(u) = id(&e["user_id"]).map(|u| u.0) else { continue };
        // Newest first: keep the first actor seen per object.
        if let Some(t) = id(&e["target_id"]) {
            out.objects.entry(t.0).or_insert(u);
        }
        let changes = e["changes"].as_array().into_iter().flatten();
        for c in changes.filter(|c| matches!(c["key"].as_str(), Some("$add" | "$remove"))) {
            for r in c["new_value"].as_array().into_iter().flatten().filter_map(|r| id(&r["id"])) {
                out.holders.entry(r.0).or_insert(u);
            }
        }
    }
    Some(out)
}

/// For `/server status`: does Discord match the config?
pub async fn status(ctx: &Ctx) -> String {
    let e = match control::engine(ctx).await {
        Ok(e) => e,
        Err(e) => {
            return format!(
                "{} **Couldn't load the config:** {}\nFix it in the config repo; the bot keeps running the last good config.",
                fmt::ATTENTION,
                fmt::error(&e)
            );
        }
    };
    match e.plan(Options::default()).await {
        Ok(p) if p.is_empty() => format!("{} Discord matches the config.", fmt::DONE),
        Ok(p) => describe(&e.cfg, &p, &[], 0, e.cfg.bot.drift.mode),
        Err(e) => {
            tracing::warn!("checking for drift failed: {e:#}");
            format!("{} **Couldn't check:** {}", fmt::ATTENTION, fmt::error(&e))
        }
    }
}

#[cfg(test)]
mod tests {
    use chipsoc_model::{Change, Field, OrderScope};

    use super::*;

    fn edit(key: &str, id: u64, cause: Cause) -> Action {
        let changes = vec![Change { field: Field::Topic, from: "a".into(), to: "b".into() }];
        Action::Edit { kind: ObjectKind::Channel, key: key.into(), id: Snowflake(id), changes, cause }
    }

    fn plan() -> Plan {
        let actions = vec![
            edit("config", 1, Cause::Config),
            edit("discord", 2, Cause::Discord),
            edit("by-bot", 3, Cause::Discord),
            edit("both", 4, Cause::Both),
            edit("unknown", 5, Cause::Unknown),
            Action::Create { kind: ObjectKind::Channel, key: "new".into() },
            Action::Reorder { scope: OrderScope::Roles, from: vec![], to: vec![] },
        ];
        Plan { actions, ..Plan::default() }
    }

    const ME: u64 = 100;

    #[test]
    fn a_refused_undo_is_tried_again_a_few_times() {
        let t = Trigger::default();
        t.quiet_for(Duration::from_secs(600));
        for _ in 0..RETRIES {
            assert!(t.again());
            assert!(t.state().due().is_some(), "queued even while quiet");
            t.state().done();
        }
        assert!(!t.again(), "then reported, not tried forever");
        assert!(t.again(), "and the count starts over");
    }

    fn changed(objects: &[(u64, u64)], holders: &[(u64, u64)]) -> Changed {
        Changed { objects: objects.iter().copied().collect(), holders: holders.iter().copied().collect() }
    }

    #[test]
    fn only_discord_side_edits_are_undone() {
        let (steps, stale, unconfirmed) = undo_steps(&plan(), &changed(&[(2, 7), (3, ME)], &[]), ME, None);
        assert_eq!(steps, vec![edit("discord", 2, Cause::Discord)]);
        assert_eq!((stale, unconfirmed), (1, 0), "the bot's own change is left alone");
    }

    #[test]
    fn changes_the_audit_log_doesnt_show_a_person_making_are_only_reported() {
        // A pushed lock.toml can make a config change look like one made in
        // Discord; nobody made it in Discord, so the audit log doesn't show it.
        let (steps, stale, unconfirmed) = undo_steps(&plan(), &changed(&[(3, ME)], &[]), ME, None);
        assert!(steps.is_empty(), "{steps:?}");
        assert_eq!((stale, unconfirmed), (1, 1));
        let p = Plan { actions: vec![edit("discord", 2, Cause::Discord)], ..Plan::default() };
        // Someone changed who holds role 2, not the channel 2 itself.
        assert!(undo_steps(&p, &changed(&[], &[(2, 7)]), ME, None).0.is_empty());
    }

    #[test]
    fn only_managed_holders_are_put_back_and_never_the_admin_role() {
        let holders = |key: &str| {
            let changes = vec![Change { field: Field::Holders, from: "<@1> <@2>".into(), to: "<@1>".into() }];
            Action::Edit {
                kind: ObjectKind::Holders,
                key: key.into(),
                id: Snowflake(9),
                changes,
                cause: Cause::Discord,
            }
        };
        let p = Plan { actions: vec![holders("chair"), holders("admin")], ..Plan::default() };
        let (steps, _, _) = undo_steps(&p, &changed(&[], &[(9, 7)]), ME, Some("admin"));
        assert_eq!(steps, vec![holders("chair")], "the admin role is only reported");
        let text = describe(&cfg(), &p, &steps, 0, DriftMode::Undo);
        assert!(text.split("made in Discord:**").nth(1).is_some_and(|t| t.contains("<@&9>: take from <@2>")), "{text}");
    }

    fn cfg() -> chipsoc_config::Config {
        chipsoc_config::Config::default()
    }

    #[test]
    fn the_report_sorts_steps_by_who_changed_them() {
        let p = plan();
        let undone = [edit("discord", 2, Cause::Discord)];
        let text = describe(&cfg(), &p, &undone, 1, DriftMode::Undo);
        assert_eq!(
            text,
            "✅ **Put back 1 change made in Discord.**
• Edit <#2>: topic a → b (changed in Discord; Apply puts it back)
To change these for good, change the config (the dashboard, `/server`, or the config repo): changes made only in Discord are put back.

⚠️ **1 change made in Discord:**
• Edit <#3>: topic a → b (changed in Discord; Apply puts it back)
Drift watch puts these back. `/server pull` keeps Discord's version only if run first, so changing the config is safer.

⚠️ **3 changes need a person** (changed on both sides, or not known):
• Edit <#4>: topic a → b (changed on both sides: Apply keeps the config's, `/server pull` keeps Discord's)
• Edit <#5>: topic a → b
• Sort roles: put the new ones in place
Make the config say what you want, then `/server apply`.

📥 2 changes in the config are waiting to be applied: `/server plan`.

⚠️ 1 of these was last changed by the bot itself, so the config on the bot's server is probably older than what was applied. Deploy the latest config."
        );
        let report = describe(&cfg(), &p, &[], 0, DriftMode::Report);
        assert!(!report.contains("Put back") && report.contains("`/server pull`"), "{report}");
    }

    #[test]
    fn member_updates_matter_when_holdings_leave_the_config() {
        let mut want = Snapshot::default();
        want.lock.roles.insert("chair".into(), Snowflake(50));
        want.holders.insert("chair".into(), [Snowflake(7)].into());
        assert!(!holdings_differ(&want, 7, &[50, 99]), "holds what config gives");
        assert!(holdings_differ(&want, 7, &[99]), "lost it");
        assert!(holdings_differ(&want, 8, &[50]), "given it by hand");
        assert!(!holdings_differ(&want, 8, &[99]), "unmanaged roles don't count");
    }
}

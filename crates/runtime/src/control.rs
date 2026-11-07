//! Running the server from Discord: `/server`, the Apply and Dismiss buttons
//! under posted plans, and the one way the bot changes config or Discord.

use std::sync::Arc;
use std::time::Duration;

use crate::Discord;
use anyhow::Result;
use chipsoc_config::ops::Op;
use chipsoc_config::{Config, Snowflake};
use chipsoc_discord::exec::Report;
use chipsoc_engine::{By, Engine, Options, Pulled};
use chipsoc_model::{Action, Plan};
use tokio::sync::MutexGuard;
use twilight_model::application::interaction::Interaction;
use twilight_model::channel::message::MessageFlags;
use twilight_model::channel::message::component::{ButtonStyle, Component};

use crate::apps::invoker;
use crate::commands::CustomId;
use crate::manage::Basic;
use crate::{Ctx, drift, plans, reload, ui};
use chipsoc_admin::fmt;

/// Drift watch ignores events this long while the bot changes things…
const BUSY: Duration = Duration::from_secs(600);
/// …and this long after, while Discord's events for it arrive.
const SETTLE: Duration = Duration::from_secs(15);

/// An engine on the config, using the lock and settings the bot trusts (see
/// [`crate::trust`]), loaded once, vetted.
pub async fn engine(ctx: &Ctx) -> Result<Engine<Discord>> {
    let e = Engine::unloaded(ctx.rest.clone(), &ctx.cfg().dir).autosave(ctx.autosave);
    e.trusting(crate::trust::baseline(ctx)).await
}

pub fn noop(_: &Action, _: &Result<(), String>) {}

/// The bot's one change in progress. Drift watch ignores what it does to
/// Discord meanwhile; call [`Change::end`] to start using the config it leaves.
pub struct Change<'a> {
    ctx: &'a Ctx,
    pub engine: Engine<Discord>,
    _one: MutexGuard<'a, ()>,
}

pub async fn begin(ctx: &Ctx) -> Result<Change<'_>> {
    let one = ctx.changing.lock().await;
    let engine = engine(ctx).await?;
    ctx.drift.quiet_for(BUSY);
    Ok(Change { ctx, engine, _one: one })
}

impl Change<'_> {
    pub async fn end(mut self) {
        match self.engine.refresh().await {
            Ok(()) => {
                reload::use_config(self.ctx, self.engine.cfg.clone()).await;
            }
            Err(e) => tracing::warn!("re-reading the config after a change failed: {e:#}"),
        }
    }
}

impl Drop for Change<'_> {
    fn drop(&mut self) {
        self.ctx.drift.quiet_for(SETTLE);
    }
}

/// Shared admin work (the review queue, roles) runs on the bot's own client,
/// store and config; config changes go through [`begin`].
impl chipsoc_admin::Host for Ctx {
    type Rest = Discord;

    fn rest(&self) -> &Discord {
        &self.rest
    }

    fn db(&self) -> chipsoc_store::Guild<'_> {
        Ctx::db(self)
    }

    fn cfg(&self) -> Arc<Config> {
        Ctx::cfg(self)
    }

    fn guild(&self) -> Snowflake {
        Snowflake(self.guild.get())
    }

    fn saves(&self) -> bool {
        self.autosave
    }

    async fn record(&self, ops: &[Op], user: Snowflake, roles: &[String]) -> Result<chipsoc_admin::Recorded> {
        let mut c = begin(self).await?;
        let r = chipsoc_admin::record_with(&mut c.engine, &By::bot(), ops, user, roles).await;
        c.end().await;
        r
    }

    async fn undo(
        &self,
        commit: &str,
        how: chipsoc_engine::git::Undo,
        by: &By,
    ) -> Result<Option<chipsoc_engine::git::Undid>> {
        let mut c = begin(self).await?;
        let r = c.engine.undo(commit, how, by).await;
        c.end().await;
        r
    }

    fn seen_posts(&self) -> Option<&chipsoc_admin::forums::SeenPosts> {
        Some(&self.seen_posts)
    }

    fn shares_queued(&self) {
        self.fed.shares.ready.notify_one();
    }
}

/// Apply a plan as the bot. Refused (nothing done) if it's stale.
pub async fn apply(ctx: &Ctx, plan: &Plan) -> Result<Report> {
    apply_as(ctx, plan, &By::bot()).await
}

/// Apply a plan for `by` (refused if it holds what only the server owner may apply).
pub async fn apply_as(ctx: &Ctx, plan: &Plan, by: &By) -> Result<Report> {
    let mut c = begin(ctx).await?;
    let r = c.engine.apply_by(plan, by, &mut noop).await;
    // Noted while still changing, so the reload watch can't take it for another program's.
    if let Some(id) = r.as_ref().ok().and_then(|r| r.audit_id) {
        ctx.own_applies.lock().unwrap_or_else(|e| e.into_inner()).insert(id);
    }
    c.end().await;
    r
}

/// Close the bot log's plan posts made before each apply another program
/// logged since `seen` (the web UI, the API, the CLI). Skipped while the
/// bot is changing things.
pub async fn close_applied(ctx: &Ctx, seen: &mut i64) {
    let Ok(_one) = ctx.changing.try_lock() else { return };
    let applies = match ctx.db().applies_after(*seen).await {
        Ok(a) => a,
        Err(e) => return tracing::warn!("reading the audit log for applies failed: {e:#}"),
    };
    for a in applies {
        *seen = a.id;
        if ctx.own_applies.lock().unwrap_or_else(|e| e.into_inner()).remove(&a.id) {
            continue;
        }
        let posts = ctx.db().plan_posts_until(a.ts).await.unwrap_or_default();
        let who = match a.actor.as_deref() {
            Some(id) if !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()) => format!("<@{id}>"),
            Some(name) if !name.is_empty() => name.to_owned(),
            _ => "someone".to_owned(),
        };
        close_posts(ctx, posts, &format!("✅ Applied by {who} (from the CLI/web).")).await;
    }
}

/// Who pressed or typed `i`, as the engine knows them.
pub(crate) fn by(i: &Interaction) -> By {
    invoker(i).map_or_else(|| By::named(None), |u| By::person(chipsoc_model::Snowflake(u.id.get()), &u.name))
}

fn who(i: &Interaction) -> String {
    invoker(i).map_or_else(|| "someone".to_owned(), |u| format!("<@{}>", u.id))
}

fn buttons(id: &str, label: &str) -> Vec<Component> {
    let apply = CustomId::Plan { apply: true, id: id.to_owned() }.to_string();
    let dismiss = CustomId::Plan { apply: false, id: id.to_owned() }.to_string();
    vec![ui::row(vec![
        ui::button(&apply, label, ButtonStyle::Success),
        ui::button(&dismiss, "Dismiss", ButtonStyle::Secondary),
    ])]
}

/// `components` with every button greyed out, while what was pressed runs.
fn greyed(components: &[Component]) -> Vec<Component> {
    let mut out = components.to_vec();
    for c in &mut out {
        if let Component::ActionRow(row) = c {
            for b in &mut row.components {
                if let Component::Button(b) = b {
                    b.disabled = true;
                }
            }
        }
    }
    out
}

/// The plan behind the Apply button in `components`, if there is one.
fn plan_in(components: &[Component]) -> Option<String> {
    components.iter().find_map(|c| match c {
        Component::ActionRow(row) => row.components.iter().find_map(|b| match b {
            Component::Button(b) => match b.custom_id.as_deref().and_then(CustomId::parse) {
                Some(CustomId::Plan { apply: true, id }) => Some(id),
                _ => None,
            },
            _ => None,
        }),
        _ => None,
    })
}

/// A reply to an admin, which may carry a plan's buttons: shown, and noted,
/// so it's closed when a newer plan is shown or any is applied. Showing it
/// closes older replies' plans (not the bot log's: others see those).
pub(crate) async fn reply(ctx: &Ctx, i: &Interaction, text: &str, components: &[Component]) {
    ctx.edit_response(i, text, components).await;
    if let Some(id) = plan_in(components) {
        close_others(ctx, Some(&id), false, "↪️ Replaced by a newer plan.").await;
        ctx.plans.in_reply(&id, &i.token);
    }
}

/// Note a plan posted in a channel with live buttons, and close every other
/// plan shown, saying it was replaced (with a link to this one).
async fn posted(ctx: &Ctx, channel: &str, message: &str, plan: &str) {
    let post = chipsoc_store::PlanPost { channel_id: channel.into(), message_id: message.into(), plan_id: plan.into() };
    ctx.log_err(ctx.db().add_plan_post(&post, crate::now()).await, "noting a plan post");
    let link = format!("https://discord.com/channels/{}/{channel}/{message}", ctx.guild);
    close_others(ctx, Some(plan), true, &format!("↪️ Replaced by a newer plan ↓ {link}")).await;
}

/// Take the buttons off every plan shown (except `keep`'s) and say `why`
/// instead; their plans are dropped, so a press finds nothing. `posts`: the
/// ones in channels too, not only replies.
async fn close_others(ctx: &Ctx, keep: Option<&str>, posts: bool, why: &str) {
    use chipsoc_discord::{Method, Rest as _};
    let body = Some(serde_json::json!({ "content": why, "components": [] }));
    if posts {
        let all = ctx.db().plan_posts().await.unwrap_or_default();
        close_posts(ctx, all.into_iter().filter(|p| Some(p.plan_id.as_str()) != keep).collect(), why).await;
    }
    for (_, token) in ctx.plans.close_replies(keep) {
        let path = format!("webhooks/{}/{token}/messages/@original", ctx.app);
        if let Err(e) = ctx.rest.call(Method::Patch, &path, body.clone()).await {
            tracing::info!("closing an old plan reply failed: {e:#}");
        }
    }
}

/// Take the buttons off `posts` and say `why` instead; their plans are dropped.
async fn close_posts(ctx: &Ctx, posts: Vec<chipsoc_store::PlanPost>, why: &str) {
    use chipsoc_discord::{Method, Rest as _};
    let body = Some(serde_json::json!({ "content": why, "components": [] }));
    for p in posts {
        ctx.plans.take(&p.plan_id);
        let path = format!("channels/{}/messages/{}", p.channel_id, p.message_id);
        if let Err(e) = ctx.rest.call(Method::Patch, &path, body.clone()).await {
            tracing::info!("closing an old plan post failed: {e:#}");
        }
        ctx.log_err(ctx.db().close_plan_post(&p.message_id).await, "forgetting a plan post");
    }
}

/// Plans live in memory, so after a restart every plan post's buttons are
/// dead: take them off the bot log's recent posts (tracked or not) and say so.
pub(crate) async fn close_stale_posts(ctx: &Ctx) {
    use chipsoc_discord::{Method, Rest as _};
    let Some(channel) = ctx.bot_log() else { return };
    let recent = match ctx.get(&format!("channels/{channel}/messages?limit=100")).await {
        Ok(v) => v,
        Err(e) => return tracing::info!("reading the bot log for old plans failed: {e:#}"),
    };
    let me = ctx.me.to_string();
    for m in recent.as_array().into_iter().flatten() {
        let has_plan = m["components"].as_array().into_iter().flatten().any(|row| {
            row["components"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|b| b["custom_id"].as_str().is_some_and(|id| id.starts_with("plan:")))
        });
        if m["author"]["id"] != me.as_str() || !has_plan {
            continue;
        }
        let Some(id) = m["id"].as_str() else { continue };
        let note = "↪️ This plan ended when the bot restarted. `/server plan` shows what's waiting now.";
        let content = fmt::with_note(m["content"].as_str().unwrap_or_default(), note);
        let body = Some(serde_json::json!({ "content": content, "components": [] }));
        if let Err(e) = ctx.rest.call(Method::Patch, &format!("channels/{channel}/messages/{id}"), body).await {
            tracing::info!("closing an old plan post failed: {e:#}");
        }
        ctx.log_err(ctx.db().close_plan_post(id).await, "forgetting a plan post");
    }
}

/// A fresh plan as a message: `intro` (if any) above what it would change,
/// with Apply and Dismiss.
pub(crate) async fn planned(ctx: &Ctx, intro: &str, confirm: bool) -> (String, Vec<Component>) {
    let plan = match engine(ctx).await {
        Ok(e) => e.plan(Options::default()).await,
        Err(e) => Err(e),
    };
    let intro = if intro.is_empty() { String::new() } else { format!("{}\n", intro.trim_end()) };
    match plan {
        Ok(p) if p.is_empty() => (format!("{intro}{} Discord matches the config.", fmt::DONE), Vec::new()),
        Ok(p) => {
            let head = if confirm {
                match p.changes() {
                    1 => "Apply this change?".to_owned(),
                    n => format!("Apply these {n} changes?"),
                }
            } else {
                plans::to_apply(&p)
            };
            let label = if confirm {
                format!("Yes, apply {}", fmt::count(p.changes(), "change", "changes"))
            } else {
                "Apply".to_owned()
            };
            let text = format!("{intro}{}", plans::summary(&ctx.cfg(), &head, &p));
            (text, buttons(&ctx.plans.put(p), &label))
        }
        Err(e) => {
            tracing::warn!("planning failed: {e:#}");
            (format!("{intro}{} **Couldn't make a plan:** {}", fmt::ATTENTION, fmt::error(&e)), Vec::new())
        }
    }
}

/// After a config change: post its plan in the bot log for admins to apply,
/// unless there's nothing to apply or this very plan was posted already.
/// `head` says what changed ("📥 New config (abc1234)"). False if the bot
/// (or another program) is applying, so the plan would be half done: try
/// again later.
pub async fn announce(ctx: &Ctx, head: &str) -> bool {
    let Some(ch) = ctx.bot_log() else { return true };
    let mut last = ctx.announced.lock().await;
    let Ok(_one) = ctx.changing.try_lock() else { return false };
    let Ok(_no_apply) = chipsoc_engine::Lease::take(&ctx.cfg().dir) else { return false };
    let plan = match engine(ctx).await {
        Ok(e) => e.plan(Options::default()).await,
        Err(e) => Err(e),
    };
    let plan = match plan {
        Ok(p) if p.is_empty() => return true,
        Ok(p) if last.as_deref() == Some(p.fingerprint.as_str()) => return true,
        Ok(p) => p,
        Err(e) => {
            tracing::warn!("planning a config change failed: {e:#}");
            let text = format!("**{head}**\n{} Couldn't make a plan for it: {}", fmt::ATTENTION, fmt::error(&e));
            ctx.say(ch, &text).await;
            return true;
        }
    };
    *last = Some(plan.fingerprint.clone());
    let text = plans::summary(&ctx.cfg(), &format!("{head}: {}", plans::to_apply(&plan)), &plan);
    let id = ctx.plans.put(plan);
    match ctx.post(ch, &text, &buttons(&id, "Apply")).await {
        Ok(m) => posted(ctx, &ch.to_string(), chipsoc_discord::wire::s(&m["id"]).as_ref(), &id).await,
        Err(e) => tracing::warn!("posting a plan failed: {e:#}"),
    }
    true
}

/// `/server status`: the server at a glance, without asking Discord.
async fn overview(ctx: &Ctx) -> String {
    let cfg = ctx.cfg();
    let report = chipsoc_config::authoring::check_config(&cfg);
    let mut out = vec![
        format!(
            "**{}** at a glance",
            if cfg.bot.name.is_empty() { &cfg.desired.server.guild.name } else { &cfg.bot.name }
        ),
        format!(
            "• {} · {} · {} · {}",
            fmt::count(cfg.desired.roles.roles.len(), "role", "roles"),
            fmt::count(cfg.desired.channels().count(), "channel", "channels"),
            fmt::count(cfg.projects.len(), "project", "projects"),
            fmt::count(cfg.bot.positions.len(), "position", "positions"),
        ),
    ];
    if cfg.bot.review.is_some() {
        out.push(match ctx.db().pending().await {
            Ok(0) => "• Review queue: nothing waiting".to_owned(),
            Ok(n) => format!("• Review queue: **{n}** waiting (`/server queue list`)"),
            Err(e) => {
                tracing::warn!(?e, "counting the review queue failed");
                "• Review queue: unknown (couldn't read the bot's data)".to_owned()
            }
        });
    }
    if report.errors.is_empty() && report.warnings.is_empty() {
        out.push("• Config: no problems".to_owned());
    }
    out.extend(report.errors.iter().map(|e| format!("• ✗ {e}")));
    out.extend(report.warnings.iter().map(|w| format!("• ! {w}")));
    out.push("`/server drift`: does Discord match the config?".to_owned());
    ui::fit(&out[0], &out[1..])
}

/// `/server status|drift|plan|apply|pull|web|help`.
pub async fn server(ctx: &Ctx, i: &Interaction, sub: Basic) {
    if !ctx.is_admin(i.member.as_ref()) {
        return ctx.respond(i, &ui::ephemeral(&crate::manage::admins_only(&ctx.cfg()))).await;
    }
    ctx.respond(i, &ui::defer()).await;
    let (text, buttons) = match sub {
        Basic::Plan => planned(ctx, "", false).await,
        Basic::Apply => planned(ctx, "", true).await,
        Basic::Pull { permissions } => (pull(ctx, &who(i), &by(i), permissions).await, Vec::new()),
        Basic::Web => {
            let text = match crate::apps::invoker(i).map(|u| u.id) {
                Some(user) => match crate::webaccess::offer(ctx, user).await {
                    Ok(()) => format!("{} Sent you a DM with the link.", fmt::DONE),
                    Err(e) => {
                        tracing::warn!("sending a dashboard link failed: {e:#}");
                        format!("{} **Couldn't send you a dashboard link:** {}", fmt::ATTENTION, fmt::error(&e))
                    }
                },
                None => "Couldn't tell who you are.".to_owned(),
            };
            (text, Vec::new())
        }
        Basic::Status => (overview(ctx).await, Vec::new()),
        Basic::Drift => (drift::status(ctx).await, Vec::new()),
        Basic::Help => (crate::manage::help(&ctx.cfg()), Vec::new()),
    };
    reply(ctx, i, &text, &buttons).await;
}

/// Copy changes made in Discord into the config (and save it).
async fn pull(ctx: &Ctx, who: &str, by: &By, permissions: bool) -> String {
    let notes = match begin(ctx).await {
        Ok(mut c) => {
            let notes = c.engine.adopt_by(permissions, by).await;
            c.end().await;
            notes
        }
        Err(e) => Err(e),
    };
    let notes = match notes {
        Ok(n) if n.is_empty() => return "Nothing changed in Discord needs copying.".to_owned(),
        Ok(n) => n,
        Err(e) => {
            tracing::warn!("pulling from Discord failed: {e:#}");
            return format!("{} **Couldn't copy Discord's changes:** {}", fmt::ATTENTION, fmt::error(&e));
        }
    };
    let copied = notes.iter().filter(|n| matches!(n, Pulled::Adopted(_))).count();
    let changes = fmt::count(copied, "change", "changes");
    if copied > 0 {
        ctx.log_bot(&format!("{} **{who} copied {changes} made in Discord into the config.**", fmt::NEW_CONFIG)).await;
    }
    let person: Vec<String> =
        notes.iter().filter(|n| !matches!(n, Pulled::Adopted(_))).map(ToString::to_string).collect();
    let head = match person.len() {
        0 => format!("{} **Copied {changes} into the config.**", fmt::DONE),
        n => format!("**Copied {changes} into the config; {n} need a person:**"),
    };
    ui::fit(&head, &fmt::bullets(&person, 20))
}

/// Apply or Dismiss under a plan.
pub async fn plan_button(ctx: &Ctx, i: &Interaction, run: bool, id: &str) {
    if !ctx.is_admin(i.member.as_ref()) {
        return ctx.respond(i, &ui::ephemeral(&crate::manage::admins_only(&ctx.cfg()))).await;
    }
    let who = who(i);
    let original = i.message.as_ref().map(|m| m.content.clone()).unwrap_or_default();
    let ephemeral = i.message.as_ref().and_then(|m| m.flags).is_some_and(|f| f.contains(MessageFlags::EPHEMERAL));
    // This one's buttons go with what follows; it isn't closed again later.
    if let Some(m) = i.message.as_ref().filter(|_| !ephemeral) {
        ctx.log_err(ctx.db().close_plan_post(&m.id.to_string()).await, "forgetting a plan post");
    }
    ctx.plans.forget_reply(id);
    if !run {
        ctx.plans.take(id);
        let note = format!("\n\nDismissed by {who}; nothing applied.");
        return ctx.respond(i, &ui::update(&fmt::with_note(&original, &note))).await;
    }
    // Said at once, so the press visibly did something; the buttons wait greyed out.
    let pressed = i.message.as_ref().map(|m| greyed(&m.components)).unwrap_or_default();
    let applying = fmt::with_note(&original, &format!("\n\n⏳ Applying… (by {who})"));
    ctx.respond(i, &ui::update_with(&applying, pressed)).await;
    let report = match ctx.plans.take(id) {
        None => Err("**That plan was already used or has expired.**".to_owned()),
        Some(plan) => apply_as(ctx, &plan, &by(i)).await.map_err(|e| {
            tracing::warn!("applying a plan failed: {e:#}");
            format!("{} **Nothing was applied:** {}", fmt::ATTENTION, fmt::error(&e))
        }),
    };
    let report = match report {
        Ok(r) => plans::outcome(&ctx.cfg(), &who, &r),
        Err(why) => {
            let (text, buttons) = planned(ctx, &format!("{why}\nThe plan now:"), false).await;
            return match (i.message.as_ref(), plan_in(&buttons)) {
                (Some(m), Some(fresh)) if !ephemeral => {
                    ctx.edit_response(i, &text, &buttons).await;
                    posted(ctx, &m.channel_id.to_string(), &m.id.to_string(), &fresh).await;
                }
                _ => reply(ctx, i, &text, &buttons).await,
            };
        }
    };
    ctx.edit_response(i, &report, &[]).await;
    // Every other plan shown is moot now: Discord matches the config as it is.
    close_others(ctx, None, true, &format!("✅ Applied with a later plan by {who}.")).await;
    // Others see what was done, even when the plan was only shown to one admin.
    if ephemeral {
        ctx.log_bot(&report).await;
    }
}

//! `/server`: the common admin jobs from Discord, for admins. Besides
//! status, plan, apply and pull: the review queue, project status and
//! applications, positions, feeds, resources and who holds what. Each is the
//! shared admin code or a config [`Op`], as in the web UI and CLI; config
//! edits are saved and then shown as a plan with an Apply button.
//!
//! Discord allows 25 subcommands or groups per command, and 25 subcommands
//! per group; this stays well inside both.

mod command;

pub use command::command;

use chipsoc_admin::{Actor, Host, NewResource};
use chipsoc_config::ops::{Op, PositionField, ProjectField};
use chipsoc_config::{Config, ProjectStatus, Snowflake};
use chipsoc_engine::git::Undo;
use twilight_model::application::command::{Command, CommandOption, CommandOptionType, CommandOptionValue as Kind};
use twilight_model::application::interaction::Interaction;
use twilight_model::application::interaction::application_command::{CommandDataOption, CommandOptionValue};
use twilight_model::channel::message::component::{ButtonStyle, Component, TextInputStyle};

use crate::apps::invoker;
use crate::commands::{self, CustomId, choice, option, subcommand};
use chipsoc_admin::fmt;

use crate::{Ctx, control, ui};

/// Most choices Discord shows for an option.
const MAX_CHOICES: usize = 25;
/// Lines shown in a list reply before "…and N more".
const SHOWN: usize = 20;

/// `/server`'s own subcommands, outside any group.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Basic {
    /// The server at a glance.
    Status,
    /// Does Discord match the config.
    Drift,
    Plan,
    Apply,
    /// Copy Discord's changes into the config (`permissions`: role permissions too).
    Pull {
        permissions: bool,
    },
    /// DM a link to the web dashboard.
    Web,
    /// What admins can do, and where.
    Help,
}

/// A `/server` command, parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cmd {
    Basic(Basic),
    /// `join notify`: DM members who haven't been through the join flow (shows how many first).
    JoinNotify,
    Queue,
    Decide {
        id: i64,
        accept: bool,
        reason: Option<String>,
    },
    History,
    ProjectStatus {
        slug: String,
        status: ProjectStatus,
    },
    Applications {
        slug: String,
        open: bool,
    },
    RenameProject {
        slug: String,
        to: String,
    },
    /// `project lead`: make someone a lead (recorded in projects.toml, given the Lead role, DMed).
    ProjectLead {
        slug: String,
        user: Snowflake,
    },
    PositionOpen {
        role: String,
        open: bool,
    },
    Feeds,
    AddFeed(String),
    PauseFeed {
        id: i64,
        paused: bool,
    },
    RemoveFeed(i64),
    Resources,
    AddResource(NewResource),
    ArchiveResource(Snowflake),
    /// `idea close`: tag an idea's post closed, say why, archive and lock it.
    CloseIdea {
        post: Snowflake,
        reason: Option<String>,
    },
    Members,
    /// `member info`: what someone told the bot (profile, join answers).
    About(Snowflake),
    Appoint {
        role: String,
        user: Snowflake,
    },
    Unappoint {
        role: String,
        user: Snowflake,
    },
    /// `share announcement|event`: pick partners to share it with (see [`crate::fedshare`]).
    Share {
        what: String,
        given: String,
    },
    /// `history`: the config's latest changes, or what one admin did this week.
    Changes(Option<Snowflake>),
    /// `approvals`: what waits for an Approve, with its card posted again.
    Approvals,
    /// `revert`: go back on a config change (the server owner only).
    Undo {
        commit: String,
        how: Undo,
    },
}

/// A change that asks "are you sure?" first, carried in its button.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Sure {
    Status {
        slug: String,
        status: ProjectStatus,
    },
    RemoveFeed(i64),
    ArchiveResource(Snowflake),
    Unappoint {
        role: String,
        user: Snowflake,
    },
    /// `revert`/`rewind` a config commit (its id, or the start of one).
    Undo {
        commit: String,
        how: Undo,
    },
    /// Close an idea's post; `reason` is the key its reason is kept under
    /// meanwhile (see [`keep_reason`]), empty for none.
    CloseIdea {
        post: Snowflake,
        reason: String,
    },
}

impl Sure {
    /// For a button's custom id (Discord allows 100 characters; keys and slugs are short).
    pub fn encode(&self) -> String {
        match self {
            Self::Status { slug, status } => format!("status:{}:{slug}", status.as_str()),
            Self::RemoveFeed(id) => format!("rmfeed:{id}"),
            Self::ArchiveResource(id) => format!("rmres:{id}"),
            Self::Unappoint { role, user } => format!("unappoint:{user}:{role}"),
            Self::Undo { commit, how } => {
                format!("undo:{}:{commit}", if *how == Undo::Rewind { "rewind" } else { "revert" })
            }
            Self::CloseIdea { post, reason } => format!("idea:{post}:{reason}"),
        }
    }

    pub fn decode(s: &str) -> Option<Self> {
        let (head, rest) = s.split_once(':')?;
        Some(match head {
            "status" => {
                let (status, slug) = rest.split_once(':')?;
                Self::Status { slug: slug.to_owned(), status: status_named(status)? }
            }
            "rmfeed" => Self::RemoveFeed(rest.parse().ok()?),
            "rmres" => Self::ArchiveResource(rest.parse().ok()?),
            "unappoint" => {
                let (user, role) = rest.split_once(':')?;
                Self::Unappoint { role: role.to_owned(), user: user.parse().ok()? }
            }
            "undo" => {
                let (how, commit) = rest.split_once(':')?;
                let how = match how {
                    "revert" => Undo::Revert,
                    "rewind" => Undo::Rewind,
                    _ => return None,
                };
                Self::Undo { commit: commit.to_owned(), how }
            }
            "idea" => {
                let (post, reason) = rest.split_once(':')?;
                Self::CloseIdea { post: post.parse().ok()?, reason: reason.to_owned() }
            }
            _ => return None,
        })
    }

    /// The question to confirm; `feed` is the feed's title (else its address), if known.
    fn question(&self, cfg: &Config, feed: Option<&str>) -> String {
        if let Self::Undo { commit, how } = self {
            return chipsoc_admin::undo_question_for(&cfg.dir, commit, *how).markdown();
        }
        if let Self::CloseIdea { post, reason } = self {
            let line = chipsoc_admin::closing_line(kept_reason(reason).as_deref());
            let quoted = line.lines().map(|l| format!("> {l}")).collect::<Vec<_>>().join("\n");
            return format!(
                "Close the idea <#{post}>? It's locked and archived (never deleted), and this is posted in it:\n{quoted}"
            );
        }
        match self {
            Self::Status { slug, status } => {
                let from = cfg.projects.iter().find(|p| &p.slug == slug).map(|p| p.status);
                match from.and_then(|f| chipsoc_config::projects::status_change(f, *status)) {
                    Some(what) => format!("Make **{}** {status}? {}.", project(cfg, slug), capitalised(what)),
                    None => format!("Make **{}** {status}? It already is.", project(cfg, slug)),
                }
            }
            Self::RemoveFeed(id) => match feed {
                Some(name) => format!("Stop following **{name}** (#{id})?"),
                None => format!("Stop following feed #{id}?"),
            },
            Self::ArchiveResource(id) => format!("Close and lock resource post <#{id}>? It isn't deleted."),
            Self::Unappoint { role, user } => {
                format!("Take **{}** from <@{user}>? They lose the role.", chipsoc_admin::role_name(cfg, role))
            }
            Self::Undo { .. } | Self::CloseIdea { .. } => String::new(),
        }
    }
}

/// Reasons for closing ideas, kept between the question and Confirm (by a
/// short hash, as a button's id can't hold them), with when they were kept.
fn reasons() -> &'static std::sync::Mutex<std::collections::HashMap<String, (std::time::Instant, String)>> {
    static KEPT: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<String, (std::time::Instant, String)>>,
    > = std::sync::OnceLock::new();
    KEPT.get_or_init(Default::default)
}

/// How long a kept reason lasts: as long as Discord lets the question be answered.
const REASON_KEPT: std::time::Duration = std::time::Duration::from_secs(15 * 60);

/// The key `reason` is kept under (empty for none).
fn reason_key(reason: Option<&str>) -> String {
    match reason.map(str::trim).filter(|r| !r.is_empty()) {
        Some(r) => chipsoc_model::stable_hash(r).chars().take(16).collect(),
        None => String::new(),
    }
}

/// Keep `reason` until Confirm, forgetting ones kept too long.
fn keep_reason(reason: Option<&str>) {
    let Some(r) = reason.map(str::trim).filter(|r| !r.is_empty()) else { return };
    let mut kept = reasons().lock().unwrap_or_else(|e| e.into_inner());
    kept.retain(|_, (at, _)| at.elapsed() < REASON_KEPT);
    kept.insert(reason_key(Some(r)), (std::time::Instant::now(), r.to_owned()));
}

/// A kept reason by its key: `None` for none, or one no longer kept.
fn kept_reason(key: &str) -> Option<String> {
    if key.is_empty() {
        return None;
    }
    let kept = reasons().lock().unwrap_or_else(|e| e.into_inner());
    kept.get(key).filter(|(at, _)| at.elapsed() < REASON_KEPT).map(|(_, r)| r.clone())
}

fn status_named(s: &str) -> Option<ProjectStatus> {
    s.parse().ok()
}

fn capitalised(s: &str) -> String {
    let mut c = s.chars();
    c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default()
}

fn project(cfg: &Config, slug: &str) -> String {
    cfg.projects.iter().find(|p| p.slug == slug).map_or_else(|| slug.to_owned(), |p| p.name.clone())
}

/// The options of one subcommand.
struct Args<'a>(&'a [CommandDataOption]);

impl Args<'_> {
    fn get(&self, name: &str) -> Option<&CommandOptionValue> {
        self.0.iter().find(|o| o.name == name).map(|o| &o.value)
    }

    fn text(&self, name: &str) -> Option<String> {
        match self.get(name) {
            Some(CommandOptionValue::String(s)) if !s.trim().is_empty() => Some(s.trim().to_owned()),
            _ => None,
        }
    }

    fn need(&self, name: &str) -> Result<String, String> {
        self.text(name).ok_or_else(|| format!("Give the {name}."))
    }

    fn int(&self, name: &str) -> Result<i64, String> {
        match self.get(name) {
            Some(CommandOptionValue::Integer(n)) => Ok(*n),
            _ => Err(format!("Give the {name}.")),
        }
    }

    fn flag(&self, name: &str) -> Option<bool> {
        match self.get(name) {
            Some(CommandOptionValue::Boolean(b)) => Some(*b),
            _ => None,
        }
    }

    fn maybe_user(&self, name: &str) -> Option<Snowflake> {
        match self.get(name) {
            Some(CommandOptionValue::User(u)) => Some(Snowflake(u.get())),
            _ => None,
        }
    }

    fn user(&self, name: &str) -> Result<Snowflake, String> {
        match self.get(name) {
            Some(CommandOptionValue::User(u)) => Ok(Snowflake(u.get())),
            _ => Err(format!("Give the {name}.")),
        }
    }
}

/// `/server …`'s options as a [`Cmd`]; `Err` is what to tell the admin.
pub fn parse(options: &[CommandDataOption]) -> Result<Cmd, String> {
    let Some(first) = options.first() else { return Ok(Cmd::Basic(Basic::Status)) };
    let (group, sub, args) = match &first.value {
        CommandOptionValue::SubCommand(o) => (None, first.name.as_str(), Args(o)),
        CommandOptionValue::SubCommandGroup(subs) => match subs.first() {
            Some(CommandDataOption { name, value: CommandOptionValue::SubCommand(o) }) => {
                (Some(first.name.as_str()), name.as_str(), Args(o))
            }
            _ => return Err("Pick what to do.".into()),
        },
        _ => return Ok(Cmd::Basic(Basic::Status)),
    };
    Ok(match (group, sub) {
        (None, "status") => Cmd::Basic(Basic::Status),
        (None, "drift") => Cmd::Basic(Basic::Drift),
        (None, "plan") => Cmd::Basic(Basic::Plan),
        (None, "apply") => Cmd::Basic(Basic::Apply),
        (None, "pull") => Cmd::Basic(Basic::Pull { permissions: args.flag("permissions") == Some(true) }),
        (None, "web") => Cmd::Basic(Basic::Web),
        (None, "help") => Cmd::Basic(Basic::Help),
        (None, "history") => Cmd::Changes(args.maybe_user("admin")),
        (None, "approvals") => Cmd::Approvals,
        (None, "revert") => {
            let commit = args.need("change")?;
            if commit.len() > 40 || !commit.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err("Give the change's id, from `/server history` (e.g. `abc1234`).".into());
            }
            Cmd::Undo { commit, how: if args.flag("rewind") == Some(true) { Undo::Rewind } else { Undo::Revert } }
        }
        (Some("queue"), "list") => Cmd::Queue,
        (Some("queue"), "history") => Cmd::History,
        (Some("queue"), "accept") => Cmd::Decide { id: args.int("id")?, accept: true, reason: None },
        (Some("queue"), "reject") => Cmd::Decide { id: args.int("id")?, accept: false, reason: args.text("reason") },
        (Some("project"), "status") => {
            let status = status_named(&args.need("status")?).ok_or("Pick proposed, active or past.")?;
            Cmd::ProjectStatus { slug: args.need("project")?, status }
        }
        (Some("project"), "applications") => {
            Cmd::Applications { slug: args.need("project")?, open: args.flag("open").ok_or("Say open or not.")? }
        }
        (Some("project"), "rename") => Cmd::RenameProject { slug: args.need("project")?, to: args.need("to")? },
        (Some("project"), "lead") => Cmd::ProjectLead { slug: args.need("project")?, user: args.user("user")? },
        (Some("position"), "open") => Cmd::PositionOpen { role: args.need("position")?, open: true },
        (Some("position"), "close") => Cmd::PositionOpen { role: args.need("position")?, open: false },
        (Some("feed"), "list") => Cmd::Feeds,
        (Some("feed"), "add") => Cmd::AddFeed(args.need("url")?),
        (Some("feed"), "pause") => Cmd::PauseFeed { id: args.int("id")?, paused: true },
        (Some("feed"), "resume") => Cmd::PauseFeed { id: args.int("id")?, paused: false },
        (Some("feed"), "remove") => Cmd::RemoveFeed(args.int("id")?),
        (Some("resource"), "list") => Cmd::Resources,
        (Some("resource"), "add") => Cmd::AddResource(NewResource {
            title: args.need("title")?,
            url: args.need("url")?,
            description: args.text("description").unwrap_or_default(),
            tags: args.text("tags").map(|t| t.split(',').map(|s| s.trim().to_owned()).collect()).unwrap_or_default(),
        }),
        (Some("resource"), "archive") => Cmd::ArchiveResource(post_id(&args.need("post")?)?),
        (Some("idea"), "close") => {
            let post = args.text("post").or_else(|| args.text("thread")).ok_or("Give the post's link.")?;
            Cmd::CloseIdea { post: post_id(&post)?, reason: args.text("reason") }
        }
        (Some("member"), "list") => Cmd::Members,
        (Some("member"), "info") => Cmd::About(args.user("user")?),
        (Some("join"), "notify") => Cmd::JoinNotify,
        (Some("member"), "appoint") => Cmd::Appoint { role: args.need("role")?, user: args.user("user")? },
        (Some("member"), "remove") => Cmd::Unappoint { role: args.need("role")?, user: args.user("user")? },
        (Some("share"), "announcement") => Cmd::Share { what: sub.to_owned(), given: args.need("message")? },
        (Some("share"), "event") => Cmd::Share { what: sub.to_owned(), given: args.need("event")? },
        _ => return Err("That isn't something `/server` does.".into()),
    })
}

/// A post's id, from the id itself or a link to it.
fn post_id(text: &str) -> Result<Snowflake, String> {
    let last = text.trim().trim_end_matches('/').rsplit('/').next().unwrap_or_default();
    let digits = last.trim_start_matches("<#").trim_end_matches('>');
    digits
        .parse()
        .map_err(|_| "Paste the post's link: right-click the post (long-press on mobile) → Copy Link.".to_owned())
}

/// The config edit a command makes, if it's one.
pub fn op(cmd: &Cmd) -> Option<Op> {
    Some(match cmd {
        Cmd::ProjectStatus { slug, status } => {
            Op::SetProject { slug: slug.clone(), set: ProjectField::Status(*status) }
        }
        Cmd::Applications { slug, open } => {
            Op::SetProject { slug: slug.clone(), set: ProjectField::Applications(*open) }
        }
        Cmd::RenameProject { slug, to } => Op::RenameProject { from: slug.clone(), to: to.trim().to_owned() },
        Cmd::PositionOpen { role, open } => Op::SetPosition { role: role.clone(), set: PositionField::Open(*open) },
        _ => return None,
    })
}

/// Whether a command asks first: it archives, removes or takes something away.
fn sure(cmd: &Cmd) -> Option<Sure> {
    Some(match cmd {
        Cmd::ProjectStatus { slug, status } => Sure::Status { slug: slug.clone(), status: *status },
        Cmd::RemoveFeed(id) => Sure::RemoveFeed(*id),
        Cmd::ArchiveResource(id) => Sure::ArchiveResource(*id),
        Cmd::Unappoint { role, user } => Sure::Unappoint { role: role.clone(), user: *user },
        Cmd::Undo { commit, how } => Sure::Undo { commit: commit.clone(), how: *how },
        Cmd::CloseIdea { post, reason } => Sure::CloseIdea { post: *post, reason: reason_key(reason.as_deref()) },
        _ => return None,
    })
}

fn actor(i: &Interaction) -> Option<Actor> {
    invoker(i).map(|u| Actor { id: Some(Snowflake(u.id.get())), name: u.name.clone() })
}

/// What `/server help` says: the dashboard first, where to look, then one
/// line per group of commands (only those the config turns on). Fits one
/// message whatever is on.
pub fn help(cfg: &Config) -> String {
    let b = &cfg.bot;
    let mut out = vec![format!(
        "**Running {} from Discord.** Every `/server` command is for admins{}; replies are only shown to you.",
        if b.name.is_empty() { "the server" } else { &b.name },
        b.admin_role
            .as_deref()
            .map(|k| format!(" (**{}**, or Manage Server)", chipsoc_admin::role_name(cfg, k)))
            .unwrap_or_default()
    )];
    if let Some(w) = &b.web {
        out.push(format!("**Dashboard:** {} (`/server web` DMs you a way in).", w.url));
    }
    let channel = |k: &str| cfg.desired.lock.channel(k).map_or_else(|| format!("#{k}"), |c| format!("<#{c}>"));
    let log = b.log.bot.as_deref().or(b.log.people.as_deref()).map_or_else(|| "the bot log".to_owned(), channel);
    let mut lines = vec![format!("{log}: plans to **Apply**, changes to **Approve**, drift and problems")];
    if let Some(r) = &b.review {
        lines.push(format!(
            "{}: applications and suggestions; press **Accept** or **Reject** on each",
            channel(&r.channel)
        ));
    }
    let mut add = |l: &str| lines.push(l.to_owned());
    add("`/server status|drift|plan|apply|pull`: Discord against the config; apply it, or copy Discord's changes in");
    add(
        "`/server history|revert|approvals`: who changed the config; undo a change (the owner); what waits for an Approve",
    );
    add("`/server member list|info|appoint|remove`: who holds what; appoint someone, or take them out");
    if !b.positions.is_empty() {
        add("`/server position open|close`: take applications for a position, or stop");
    }
    if cfg.join.is_some() {
        add("`/server join notify`: DM members who haven't been through the join flow");
    }
    if !cfg.projects.is_empty() {
        add("`/server project status|applications|rename|lead`: a project's status, applications, slug and leads");
    }
    if b.review.is_some() {
        add("`/server queue list|accept|reject|history`: the review queue by number");
    }
    if b.feeds.is_some() {
        add("`/server feed list|add|pause|resume|remove`: feeds the bot posts from");
    }
    if b.resources.is_some() {
        add("`/server resource list|add|archive`: links in the resources forum");
    }
    if b.ideas.is_some() {
        add("`/server idea close`: close an idea's post, saying why");
    }
    if b.events.is_some() {
        add("`/event`: create an event (announced, with reminders)");
    }
    if b.recording.is_some() {
        add(
            "`/record start|stop`: record a meeting in a call (everyone in it is told); recordings are on the dashboard",
        );
    }
    if cfg.federation.peers.iter().any(|p| !p.id.is_empty()) {
        add(if shares_on_demand(cfg) {
            "`/partners`, `/server share announcement|event`: what partners share; share one with them"
        } else {
            "`/partners`: what partner communities share"
        });
    }
    out.extend(lines.iter().map(|l| format!("• {l}")));
    if let Some(w) = &b.web {
        out.push(format!("Terminal: `chipsoc-bot login --server {}`, then any command.", w.url));
    }
    ui::truncate(&out.join("\n"), ui::MAX_CONTENT)
}

/// Whether any partner takes announcements or events from us.
fn shares_on_demand(cfg: &Config) -> bool {
    use chipsoc_admin::shares::{ShareKind, partners_for};
    !partners_for(cfg, ShareKind::Announcement).is_empty() || !partners_for(cfg, ShareKind::Event).is_empty()
}

/// Who may use `/server`, for those who may not.
pub(crate) fn admins_only(cfg: &Config) -> String {
    match cfg.bot.admin_role.as_deref() {
        Some(k) => format!(
            "Only admins can do this: members of **{}**, or anyone with Manage Server. `/server help` says what admins can do.",
            chipsoc_admin::role_name(cfg, k)
        ),
        None => "Only admins (anyone with Manage Server) can do this. `/server help` says what they can do.".to_owned(),
    }
}

/// Handle `/server …`.
pub async fn server(ctx: &Ctx, i: &Interaction, options: &[CommandDataOption]) {
    let cmd = match parse(options) {
        Ok(c) => c,
        Err(why) => return ctx.respond(i, &ui::ephemeral(&why)).await,
    };
    // Anyone may read what admins can do.
    if cmd == Cmd::Basic(Basic::Help) {
        return ctx.respond(i, &ui::ephemeral(&help(&ctx.cfg()))).await;
    }
    if !ctx.is_admin(i.member.as_ref()) {
        return ctx.respond(i, &ui::ephemeral(&admins_only(&ctx.cfg()))).await;
    }
    if cmd == Cmd::JoinNotify {
        return crate::join::notify_command(ctx, i).await;
    }
    if let Cmd::Share { what, given } = &cmd {
        return crate::fedshare::command(ctx, i, what, given).await;
    }
    if let Cmd::Basic(sub) = cmd {
        return control::server(ctx, i, sub).await;
    }
    if let Some(s) = sure(&cmd) {
        // Only the owner may go back on a change: say so now rather than after Confirm.
        if let (Sure::Undo { .. }, Some(by)) = (&s, actor(i))
            && let Err(why) = chipsoc_admin::only_owner(ctx, &by, &chipsoc_engine::Ask::Undo).await
        {
            return ctx.respond(i, &ui::ephemeral(&format!("{} {}", fmt::ATTENTION, why.markdown()))).await;
        }
        if let Cmd::CloseIdea { reason, .. } = &cmd {
            keep_reason(reason.as_deref());
        }
        let yes = ui::button(&CustomId::Sure(s.encode()).to_string(), "Confirm", ButtonStyle::Danger);
        let no = ui::button(&CustomId::Sure(CANCEL.into()).to_string(), "Cancel", ButtonStyle::Secondary);
        let feed = match &s {
            Sure::RemoveFeed(id) => {
                ctx.db().feed(*id).await.ok().flatten().map(|f| fmt::escape(&f.title.unwrap_or(f.url)))
            }
            _ => None,
        };
        let q = s.question(&ctx.cfg(), feed.as_deref());
        return ctx.respond(i, &ui::ephemeral_with(&q, vec![ui::row(vec![yes, no])])).await;
    }
    let Some(by) = actor(i) else { return };
    ctx.respond(i, &ui::defer()).await;
    let (text, buttons) = run(ctx, &cmd, &by).await;
    control::reply(ctx, i, &text, &buttons).await;
}

/// What Cancel under a question from [`server`] carries.
const CANCEL: &str = "cancel";

/// Confirm (or Cancel) under a question from [`server`].
pub async fn confirmed(ctx: &Ctx, i: &Interaction, payload: &str) {
    if payload == CANCEL {
        return ctx.respond(i, &ui::update("Cancelled; nothing was changed.")).await;
    }
    let Some(by) = actor(i).filter(|_| ctx.is_admin(i.member.as_ref())) else {
        return ctx.respond(i, &ui::ephemeral(&admins_only(&ctx.cfg()))).await;
    };
    let Some(sure) = Sure::decode(payload) else { return crate::commands::unknown(ctx, i).await };
    let cmd = match sure {
        Sure::Status { slug, status } => Cmd::ProjectStatus { slug, status },
        Sure::RemoveFeed(id) => Cmd::RemoveFeed(id),
        Sure::ArchiveResource(id) => Cmd::ArchiveResource(id),
        Sure::Unappoint { role, user } => Cmd::Unappoint { role, user },
        Sure::Undo { commit, how } => Cmd::Undo { commit, how },
        Sure::CloseIdea { post, reason } => {
            let kept = kept_reason(&reason);
            if !reason.is_empty() && kept.is_none() {
                let why = "That question has expired, so nothing was closed; run `/server idea close` again.";
                return ctx.respond(i, &ui::update(why)).await;
            }
            Cmd::CloseIdea { post, reason: kept }
        }
    };
    ctx.respond(i, &ui::defer_update()).await;
    let (text, buttons) = run(ctx, &cmd, &by).await;
    control::reply(ctx, i, &text, &buttons).await;
}

/// Reject on a review card: ask why first (it can be left empty), saying
/// who reads it: the member who sent it, or (a partner's) nobody.
pub async fn reject_form(ctx: &Ctx, i: &Interaction, id: i64) {
    if !ctx.is_admin(i.member.as_ref()) {
        return ctx.respond(i, &ui::ephemeral(&admins_only(&ctx.cfg()))).await;
    }
    let app = ctx.db().application(id).await.ok().flatten();
    let kind = app.as_ref().map_or(chipsoc_admin::AppKind::Position, |a| a.kind);
    let title =
        app.as_ref().map_or_else(|| "Reject".to_owned(), |a| format!("Reject: {}", chipsoc_admin::what(&ctx.cfg(), a)));
    let label = chipsoc_admin::reason_label(kind);
    let reason = ui::input("reason", label, TextInputStyle::Paragraph, false, None);
    let form = vec![ui::described(reason, chipsoc_admin::reject_says(kind))];
    ctx.respond(i, &ui::modal(&CustomId::RejectForm(id).to_string(), &title, form)).await;
}

/// The reason form sent: reject, tell them why, and mark the card.
pub async fn reject_submit(ctx: &Ctx, i: &Interaction, id: i64, reason: Option<&String>) {
    let Some(by) = actor(i).filter(|_| ctx.is_admin(i.member.as_ref())) else {
        return ctx.respond(i, &ui::ephemeral(&admins_only(&ctx.cfg()))).await;
    };
    let app = match chipsoc_admin::claim(ctx, id, false, &by).await {
        Ok(app) => app,
        Err(why) => return ctx.respond(i, &ui::ephemeral(&why.markdown())).await,
    };
    ctx.respond(i, &crate::apps::deferred(&app)).await;
    let decided = chipsoc_admin::settle_with(ctx, &app, false, reason.map(String::as_str), &by).await;
    crate::apps::show_decision(ctx, i, &app, &decided, &by).await;
}

/// Do a parsed command; the reply and its buttons.
async fn run(ctx: &Ctx, cmd: &Cmd, by: &Actor) -> (String, Vec<Component>) {
    if let Some(op) = op(cmd) {
        let (mut text, buttons) = edit(ctx, &op, by).await;
        // Closing a position closes its window: everyone who applied is listed now.
        if let Cmd::PositionOpen { open: false, .. } = cmd {
            match chipsoc_admin::rounds::post_closed(ctx).await {
                Ok(0) => {}
                Ok(n) => text.push_str(&format!(
                    "\nIts applicants are listed in the review channel ({}).",
                    fmt::count(n, "card", "cards")
                )),
                Err(e) => text.push_str(&format!("\n{}", failed("list its applicants", &e))),
            }
        }
        return (text, buttons);
    }
    let text = match cmd {
        Cmd::Queue => queue(ctx).await,
        Cmd::History => history(ctx).await,
        Cmd::Changes(user) => changes(ctx, *user).await,
        Cmd::Undo { commit, how } => return undo(ctx, commit, *how, by).await,
        Cmd::Approvals => crate::trust::approvals(ctx).await,
        Cmd::Decide { id, accept, reason } => {
            return match chipsoc_admin::decide_with(ctx, *id, *accept, reason.as_deref(), by).await {
                Ok(d) if d.pending => (
                    format!("⚠️ Accepting #{id} didn't go through:{} It's waiting again.", d.outcome.spaced()),
                    Vec::new(),
                ),
                Ok(d) => {
                    let mark = if d.accepted { "✅ Accepted" } else { "❌ Rejected" };
                    let told = if d.told { " They were told by DM." } else { "" };
                    let buttons = if d.lead {
                        let lead = CustomId::MakeLead(*id).to_string();
                        vec![ui::row(vec![ui::button(&lead, "Make lead", ButtonStyle::Primary)])]
                    } else {
                        Vec::new()
                    };
                    (format!("{mark} #{id}.{told}{}", d.outcome.spaced()), buttons)
                }
                Err(why) => (why.markdown(), Vec::new()),
            };
        }
        Cmd::Feeds => feeds(ctx).await,
        Cmd::AddFeed(url) => match chipsoc_admin::add_feed(ctx, url, by).await {
            Ok(title) => format!("{} Now following **{title}**.", fmt::DONE),
            Err(why) => format!("{} **Couldn't follow it:** {}", fmt::ATTENTION, why.markdown()),
        },
        Cmd::PauseFeed { id, paused } => match chipsoc_admin::pause_feed(ctx, *id, *paused, by).await {
            Ok(true) => format!("Feed #{id} {}.", if *paused { "paused" } else { "resumed" }),
            Ok(false) => "No such feed; `/server feed list` shows them.".into(),
            Err(e) => failed(&format!("{} feed #{id}", if *paused { "pause" } else { "resume" }), &e),
        },
        Cmd::RemoveFeed(id) => match chipsoc_admin::remove_feed(ctx, *id, by).await {
            Ok(true) => format!("{} Stopped following feed #{id}.", fmt::DONE),
            Ok(false) => "No such feed; `/server feed list` shows them.".into(),
            Err(e) => failed(&format!("stop following feed #{id}"), &e),
        },
        Cmd::Resources => resources(ctx).await,
        Cmd::AddResource(r) => match chipsoc_admin::post_resource(ctx, r, by).await {
            Ok(p) => format!("{} Shared **{}**: {}", fmt::DONE, p.title, p.link),
            Err(e) => failed("share it", &e),
        },
        Cmd::ArchiveResource(id) => match chipsoc_admin::archive_resource(ctx, *id, by).await {
            Ok(true) => format!("{} Archived <#{id}>: closed and locked, still readable.", fmt::DONE),
            Ok(false) => "That isn't a post in the resources forum.".into(),
            Err(e) => failed(&format!("archive <#{id}>"), &e),
        },
        Cmd::CloseIdea { post, reason } => match chipsoc_admin::close_idea(ctx, *post, reason.as_deref(), by).await {
            Ok(Some(closed)) => format!("{} {}", fmt::DONE, closed.said(*post)),
            Ok(None) => {
                "That isn't a post in the ideas forum: paste the link of an idea's post (right-click it → Copy Link)."
                    .into()
            }
            Err(e) => failed(&format!("close <#{post}>"), &e),
        },
        Cmd::Members => members(ctx).await,
        Cmd::About(user) => match chipsoc_admin::join::about(ctx, *user).await {
            Ok(a) => about(&a),
            Err(e) => failed(&format!("read what <@{user}> told the bot"), &e),
        },
        Cmd::Appoint { role, user } => match chipsoc_admin::appoint(ctx, role, *user, by).await {
            Ok(said) => format!("{} **<@{user}> now holds {}.**{}", fmt::DONE, ctx.role_name(role), said.spaced()),
            Err(why) => {
                format!("{} **Couldn't appoint <@{user}> as {}:**{}", fmt::ATTENTION, ctx.role_name(role), why.spaced())
            }
        },
        Cmd::ProjectLead { slug, user } => match chipsoc_admin::add_project_lead(ctx, slug, *user, by).await {
            Ok(said) => {
                format!("{} **<@{user}> now leads {}.**{}", fmt::DONE, project(&ctx.cfg(), slug), said.spaced())
            }
            Err(why) => format!("{} **Couldn't make <@{user}> a lead:**{}", fmt::ATTENTION, why.spaced()),
        },
        Cmd::Unappoint { role, user } => match chipsoc_admin::unappoint(ctx, role, *user, by).await {
            Ok(said) => {
                format!("{} **<@{user}> no longer holds {}.**{}", fmt::DONE, ctx.role_name(role), said.spaced())
            }
            Err(why) => {
                format!("{} **Couldn't take {} from <@{user}>:**{}", fmt::ATTENTION, ctx.role_name(role), why.spaced())
            }
        },
        Cmd::Basic(_)
        | Cmd::Share { .. }
        | Cmd::JoinNotify
        | Cmd::ProjectStatus { .. }
        | Cmd::Applications { .. }
        | Cmd::RenameProject { .. }
        | Cmd::PositionOpen { .. } => String::new(),
    };
    (text, Vec::new())
}

/// `/server history [@admin]`.
async fn changes(ctx: &Ctx, user: Option<Snowflake>) -> String {
    match chipsoc_admin::history_of(ctx, user, SHOWN).await {
        Ok(done) => {
            let lines = done
                .iter()
                .map(|d| {
                    let who = d.who_id.map_or_else(|| ui::escape(&d.who), |id| format!("<@{id}>"));
                    let id = d.commit.as_deref().map(|c| format!("`{c}` ")).unwrap_or_default();
                    let who = if user.is_some() { String::new() } else { format!(" · {who}") };
                    format!("{id}<t:{}:R>{who} · {}", d.at, ui::escape(&ui::truncate(&d.what, 120)))
                })
                .collect();
            match user {
                Some(u) => list(
                    &ctx.cfg(),
                    &format!("**What <@{u}> did this week** (config changes by id; `/server revert <id>` undoes one):"),
                    lines,
                    &format!("<@{u}> hasn't done anything through the bot this week."),
                ),
                None => list(
                    &ctx.cfg(),
                    "**Latest config changes** (`/server revert <id>` undoes one; the server owner only):",
                    lines,
                    "No config history here: the config isn't a git checkout.",
                ),
            }
        }
        Err(e) => failed("read the history", &e),
    }
}

/// `/server revert <change> [rewind]`: saved as a new commit, then shown as a plan to apply.
async fn undo(ctx: &Ctx, commit: &str, how: Undo, by: &Actor) -> (String, Vec<Component>) {
    let made = chipsoc_admin::undo(ctx, commit, how, by).await;
    let said = chipsoc_admin::undo_said(&made).markdown();
    match made {
        Ok(Some(_)) => control::planned(ctx, &format!("{} {said}", fmt::DONE), false).await,
        Ok(None) => (said, Vec::new()),
        Err(_) => (format!("{} {said}", fmt::ATTENTION), Vec::new()),
    }
}

/// Make a config edit as `by` (checked first; refused if it would break the
/// config), save it, then show what applying would do.
async fn edit(ctx: &Ctx, op: &Op, by: &Actor) -> (String, Vec<Component>) {
    if let Some(why) = ctx.unwritable() {
        return (format!("{} **Not saved:** {why}.", fmt::ATTENTION), Vec::new());
    }
    let saved = match control::begin(ctx).await {
        Ok(mut c) => {
            let made = c.engine.edit(op, &by.by(false)).await;
            c.end().await;
            made
        }
        Err(e) => Err(e),
    };
    match saved {
        Ok(change) if change.files.is_empty() => ("Nothing to change: it's already like that.".into(), Vec::new()),
        Ok(change) => {
            let warnings = change.warnings.iter().map(|w| format!("\n{} {w}", fmt::ATTENTION)).collect::<String>();
            let planned =
                control::planned(ctx, &format!("{} **Saved:** {}.{warnings}", fmt::DONE, change.summary), false).await;
            // With something to apply, the bot log gets the plan soon, headed
            // by the commit ("… by ada"): one post, not two.
            if planned.1.is_empty() || crate::reload::last_commit(&ctx.cfg().dir).await.is_none() {
                ctx.log_bot(&format!("✏️ {} changed the config: {}.", by.mention(), change.summary)).await;
            }
            planned
        }
        Err(e) => {
            let text = match e.downcast_ref::<chipsoc_config::authoring::Problems>() {
                Some(p) => ui::fit(
                    &format!("{} **Not saved:** the config would have problems:", fmt::ATTENTION),
                    &fmt::bullets(&p.0, 15),
                ),
                None => {
                    tracing::warn!("a config edit failed: {e:#}");
                    format!("{} **Not saved:** {}", fmt::ATTENTION, fmt::error(&e))
                }
            };
            (text, Vec::new())
        }
    }
}

/// "⚠️ **Couldn't {what}:** {why}", with the details in the process log.
fn failed(what: &str, e: &anyhow::Error) -> String {
    tracing::warn!("couldn't {what}: {e:#}");
    format!("{} **Couldn't {what}:** {}", fmt::ATTENTION, fmt::error(e))
}

/// Where to see a whole list: the dashboard's address, if there is one.
fn dashboard(cfg: &Config) -> String {
    cfg.bot.web.as_ref().map_or_else(|| "the web dashboard".to_owned(), |w| format!("the dashboard ({})", w.url))
}

/// `head`, then up to [`SHOWN`] lines, then how many more.
fn list(cfg: &Config, head: &str, lines: Vec<String>, empty: &str) -> String {
    if lines.is_empty() {
        return empty.to_owned();
    }
    let more = lines.len().saturating_sub(SHOWN);
    let mut shown: Vec<String> = lines.into_iter().take(SHOWN).collect();
    if more > 0 {
        shown.push(format!("…and {more} more ({} lists them all).", dashboard(cfg)));
    }
    ui::fit(head, &shown)
}

async fn queue(ctx: &Ctx) -> String {
    let cfg = ctx.cfg();
    if cfg.bot.review.is_none() {
        return "The review queue is off. Ask whoever runs the bot to turn it on (bot.toml, `[review]`).".into();
    }
    match chipsoc_admin::pending(ctx).await {
        Ok(q) => list(
            &cfg,
            "**Waiting for a decision** (`/server queue accept|reject <id>`):",
            q.iter()
                .map(|p| {
                    let who = chipsoc_admin::submitted_by(&cfg, &p.user);
                    let kind = chipsoc_admin::AppKind::label_of(&p.kind);
                    format!("`#{}` {kind} · {} · {who}", p.id, chipsoc_admin::fmt::plain(&p.what, 100))
                })
                .collect(),
            "Nothing is waiting.",
        ),
        Err(e) => failed("read the queue", &e),
    }
}

async fn history(ctx: &Ctx) -> String {
    match chipsoc_admin::history(ctx, SHOWN as i64).await {
        Ok(past) => list(
            &ctx.cfg(),
            "**Latest decisions:**",
            past.iter()
                .map(|p| {
                    let by = p.reviewed_by.as_deref().map(|r| format!(" by <@{r}>")).unwrap_or_default();
                    let why = p.reason.as_deref().map(|r| format!(": {}", ui::truncate(r, 80))).unwrap_or_default();
                    let who = chipsoc_admin::submitted_by(&ctx.cfg(), &p.user);
                    let what = chipsoc_admin::fmt::plain(&p.what, 100);
                    let decision = if p.status == "accepted" { "Accepted" } else { "Rejected" };
                    let kind = chipsoc_admin::AppKind::label_of(&p.kind);
                    format!("`#{}` {kind} · {what} · {who} · {decision}{by}{why}", p.id)
                })
                .collect(),
            "No decisions yet.",
        ),
        Err(e) => failed("read the history", &e),
    }
}

async fn feeds(ctx: &Ctx) -> String {
    match chipsoc_admin::feeds(ctx).await {
        Ok(f) => list(
            &ctx.cfg(),
            "**Feeds followed:**",
            f.iter()
                .map(|f| {
                    let paused = if f.paused { " (paused)" } else { "" };
                    match f.title.as_deref().filter(|t| !t.is_empty()) {
                        Some(t) => format!("`#{}` **{}** <{}> → #{}{paused}", f.id, fmt::escape(t), f.url, f.channel),
                        None => format!("`#{}` <{}> → #{}{paused}", f.id, f.url, f.channel),
                    }
                })
                .collect(),
            "No feeds yet: `/server feed add <url>`.",
        ),
        Err(e) => failed("read the feeds", &e),
    }
}

async fn resources(ctx: &Ctx) -> String {
    match chipsoc_admin::resources(ctx, SHOWN).await {
        Ok(posts) => list(
            &ctx.cfg(),
            "**Latest resources:**",
            posts
                .iter()
                .map(|p| format!("{} {}{}", p.link, p.title, if p.archived { " (archived)" } else { "" }))
                .collect(),
            "No resources yet: `/server resource add`.",
        ),
        Err(e) => failed("read the resources", &e),
    }
}

async fn members(ctx: &Ctx) -> String {
    let r = match chipsoc_admin::roster(ctx).await {
        Ok(r) => r,
        Err(e) => return failed("read who holds what", &e),
    };
    let names = |h: &chipsoc_admin::Holding| {
        let who: Vec<String> = h.people.iter().map(|p| format!("<@{}>", p.id)).collect();
        format!("**{}**: {}", h.name, if who.is_empty() { "nobody".into() } else { who.join(", ") })
    };
    let mut lines: Vec<String> = r.holders.iter().map(names).collect();
    lines.extend(r.leads.iter().filter(|h| !h.people.is_empty()).map(names));
    for f in &r.former {
        let roles: Vec<&str> = f.roles.iter().map(|n| n.name.as_str()).collect();
        lines.push(format!("Left: <@{}> (was {})", f.person.id, roles.join(", ")));
    }
    list(
        &ctx.cfg(),
        "**Who holds what:**",
        lines,
        "**Nobody is appointed to anything yet.** Add a position on the dashboard's Positions page (or in bot.toml), \
         and the bot keeps who holds it in step with the config.",
    )
}

/// What someone told the bot, for `/server member info`.
fn about(a: &chipsoc_admin::join::About) -> String {
    let who = format!("<@{}>", a.id);
    if a.is_empty() {
        return format!(
            "{who} hasn't been through the join flow or set a profile, so the bot knows nothing about them."
        );
    }
    let mut head = format!("**About {who}**");
    if a.shown.is_empty() {
        head.push_str(" (not in the server now)");
    }
    let mut lines = Vec::new();
    let name = [a.name.as_str(), a.surname.as_str()].join(" ").trim().to_owned();
    if !name.is_empty() {
        lines.push(format!("Name: **{}**", ui::escape(&name)));
    }
    if !a.company.is_empty() {
        lines.push(format!("Company: **{}**", ui::escape(&a.company)));
    }
    if !a.answers.is_empty() {
        let when = a.finished.map(|t| format!(" (finished <t:{t}:R>)")).unwrap_or_default();
        lines.push(format!("Join flow{when}:"));
        for s in &a.answers {
            let said = if s.answer.is_empty() { "—".to_owned() } else { ui::escape(&s.answer) };
            lines.push(format!("• {} **{said}**", s.question));
        }
    }
    ui::fit(&head, &lines)
}

#[cfg(test)]
mod tests {
    use twilight_model::id::Id;

    use super::*;

    fn opt(name: &str, value: CommandOptionValue) -> CommandDataOption {
        CommandDataOption { name: name.into(), value }
    }

    fn sub(group: &str, name: &str, args: Vec<CommandDataOption>) -> Vec<CommandDataOption> {
        vec![opt(group, CommandOptionValue::SubCommandGroup(vec![opt(name, CommandOptionValue::SubCommand(args))]))]
    }

    fn s(v: &str) -> CommandOptionValue {
        CommandOptionValue::String(v.into())
    }

    #[test]
    fn member_info_shows_the_profile_and_answers_as_written() {
        use chipsoc_admin::join::{About, Said};
        let nobody = About { id: Snowflake(7), ..About::default() };
        assert!(about(&nobody).contains("knows nothing about them"));
        let a = About {
            id: Snowflake(7),
            shown: "Ada L.".into(),
            name: "Ada".into(),
            surname: "Lovelace".into(),
            company: "**Arm**".into(),
            answers: vec![Said { question: "Other work?".into(), answer: "Yes, Where?: Graphcore".into() }],
            finished: Some(5),
        };
        let text = about(&a);
        assert!(text.starts_with("**About <@7>**\nName: **Ada Lovelace**"), "{text}");
        assert!(text.contains("Company: **\\*\\*Arm\\*\\***"), "their words are escaped: {text}");
        assert!(text.contains("Join flow (finished <t:5:R>):\n• Other work? **Yes, Where?: Graphcore**"), "{text}");
        let gone = About { shown: String::new(), ..a };
        assert!(about(&gone).contains("(not in the server now)"));
    }

    #[test]
    fn options_parse_into_commands() {
        let basic = vec![opt(
            "pull",
            CommandOptionValue::SubCommand(vec![opt("permissions", CommandOptionValue::Boolean(true))]),
        )];
        assert_eq!(parse(&basic), Ok(Cmd::Basic(Basic::Pull { permissions: true })));
        assert_eq!(parse(&[]), Ok(Cmd::Basic(Basic::Status)));
        let reject = sub(
            "queue",
            "reject",
            vec![opt("id", CommandOptionValue::Integer(4)), opt("reason", s("  Full this year "))],
        );
        assert_eq!(parse(&reject), Ok(Cmd::Decide { id: 4, accept: false, reason: Some("Full this year".into()) }));
        let blank = sub("queue", "reject", vec![opt("id", CommandOptionValue::Integer(4)), opt("reason", s(" "))]);
        assert_eq!(parse(&blank), Ok(Cmd::Decide { id: 4, accept: false, reason: None }));
        let status = sub("project", "status", vec![opt("project", s("golem")), opt("status", s("past"))]);
        assert_eq!(parse(&status), Ok(Cmd::ProjectStatus { slug: "golem".into(), status: ProjectStatus::Past }));
        let bad = sub("project", "status", vec![opt("project", s("golem")), opt("status", s("done"))]);
        assert!(parse(&bad).is_err());
        let rename = sub("project", "rename", vec![opt("project", s("golem")), opt("to", s(" robot "))]);
        let rename = parse(&rename).unwrap();
        assert_eq!(op(&rename), Some(Op::RenameProject { from: "golem".into(), to: "robot".into() }));
        let appoint = sub(
            "member",
            "appoint",
            vec![opt("role", s("treasurer")), opt("user", CommandOptionValue::User(Id::new(7)))],
        );
        assert_eq!(parse(&appoint), Ok(Cmd::Appoint { role: "treasurer".into(), user: Snowflake(7) }));
        let lead =
            sub("project", "lead", vec![opt("project", s("golem")), opt("user", CommandOptionValue::User(Id::new(7)))]);
        assert_eq!(parse(&lead), Ok(Cmd::ProjectLead { slug: "golem".into(), user: Snowflake(7) }));
        assert_eq!(op(&parse(&lead).unwrap()), None, "not a config op of its own: it's recorded, given and DMed");
        let info = sub("member", "info", vec![opt("user", CommandOptionValue::User(Id::new(7)))]);
        assert_eq!(parse(&info), Ok(Cmd::About(Snowflake(7))));
        let close = sub("idea", "close", vec![opt("thread", s("<#55>")), opt("reason", s(" Done elsewhere "))]);
        assert_eq!(parse(&close), Ok(Cmd::CloseIdea { post: Snowflake(55), reason: Some("Done elsewhere".into()) }));
        let close = sub("idea", "close", vec![opt("post", s("https://discord.com/channels/1/56"))]);
        assert_eq!(parse(&close), Ok(Cmd::CloseIdea { post: Snowflake(56), reason: None }));
        let add = sub(
            "resource",
            "add",
            vec![opt("title", s("Book")), opt("url", s("https://example.org")), opt("tags", s("books, papers"))],
        );
        let Ok(Cmd::AddResource(r)) = parse(&add) else { panic!("{:?}", parse(&add)) };
        assert_eq!((r.title.as_str(), r.tags.clone()), ("Book", vec!["books".to_owned(), "papers".to_owned()]));
        let link = sub("resource", "archive", vec![opt("post", s("https://discord.com/channels/1/22/"))]);
        assert_eq!(parse(&link), Ok(Cmd::ArchiveResource(Snowflake(22))));
        assert!(parse(&sub("resource", "archive", vec![opt("post", s("nope"))])).is_err());
        assert!(parse(&sub("feed", "launch", vec![])).is_err());
        assert!(parse(&sub("queue", "accept", vec![])).is_err(), "the id is needed");
        let top = |name: &str, args| vec![opt(name, CommandOptionValue::SubCommand(args))];
        assert_eq!(parse(&top("history", vec![])), Ok(Cmd::Changes(None)));
        let theirs = top("history", vec![opt("admin", CommandOptionValue::User(Id::new(7)))]);
        assert_eq!(parse(&theirs), Ok(Cmd::Changes(Some(Snowflake(7)))));
        let revert = top("revert", vec![opt("change", s(" abc1234 "))]);
        assert_eq!(parse(&revert), Ok(Cmd::Undo { commit: "abc1234".into(), how: Undo::Revert }));
        let rewind = top("revert", vec![opt("change", s("abc1234")), opt("rewind", CommandOptionValue::Boolean(true))]);
        assert_eq!(parse(&rewind), Ok(Cmd::Undo { commit: "abc1234".into(), how: Undo::Rewind }));
        assert!(parse(&top("revert", vec![])).is_err(), "which change is needed");
    }

    #[test]
    fn help_says_what_admins_can_do_and_where() {
        let mut cfg = Config::default();
        cfg.bot.name = "ChipSoc".into();
        cfg.bot.web = Some(chipsoc_config::bot::Web { url: "https://dash.example".into(), share_device: None });
        let text = help(&cfg);
        assert!(text.starts_with("**Running ChipSoc from Discord.**"), "{text}");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[1], "**Dashboard:** https://dash.example (`/server web` DMs you a way in).", "first: {text}");
        assert!(lines[2].starts_with("• the bot log: plans to **Apply**"), "{text}");
        assert!(text.contains("• `/server status|drift|plan|apply|pull`"), "{text}");
        assert!(text.contains("`/server member list|info|appoint|remove`"), "{text}");
        assert!(text.contains("`chipsoc-bot login --server https://dash.example`"), "{text}");
        assert!(!text.contains("/server queue") && !text.contains("/server project"), "only what's on: {text}");
        cfg.bot.review = Some(chipsoc_config::bot::Review { channel: "review".into() });
        cfg.bot.events = Some(Default::default());
        cfg.join = Some(Default::default());
        cfg.projects.push(toml::from_str("slug = \"golem\"\nname = \"Golem\"").unwrap());
        let text = help(&cfg);
        assert!(text.contains("• `/server queue list|accept|reject|history`"), "{text}");
        assert!(
            text.contains("• #review: applications and suggestions; press **Accept**"),
            "new admins find #review: {text}"
        );
        for cmd in
            ["member list|info", "project status", "/server web", "/server join notify", "/event", "drift", "approvals"]
        {
            assert!(text.contains(cmd), "{cmd} missing: {text}");
        }
        assert_eq!(parse(&[opt("help", CommandOptionValue::SubCommand(vec![]))]), Ok(Cmd::Basic(Basic::Help)));
    }

    /// Every feature on, with long names: still one message, nothing cut.
    #[test]
    fn help_fits_one_message_with_everything_on() {
        let mut cfg = Config::default();
        cfg.bot.name = "A Rather Long Community Name For Testing".into();
        cfg.bot.admin_role = Some("committee".into());
        cfg.desired.roles.roles.push(chipsoc_config::schema::Role {
            key: "committee".into(),
            name: "The Committee Of This Society".into(),
            ..Default::default()
        });
        for (k, id) in [("bot-log", 1_234_567_890_123_456_789), ("review", 1_234_567_890_123_456_780)] {
            cfg.desired.lock.channels.insert(k.into(), Snowflake(id));
        }
        cfg.bot.log.bot = Some("bot-log".into());
        cfg.bot.web = Some(chipsoc_config::bot::Web {
            url: "https://a-long-dashboard-name.tailnet-name-here.ts.net".into(),
            share_device: None,
        });
        cfg.bot.review = Some(chipsoc_config::bot::Review { channel: "review".into() });
        cfg.bot.positions.push(Default::default());
        cfg.join = Some(Default::default());
        cfg.projects.push(toml::from_str("slug = \"golem\"\nname = \"Golem\"").unwrap());
        cfg.bot.feeds = Some(toml::from_str("channel = \"news\"").unwrap());
        cfg.bot.resources = Some(toml::from_str("forum = \"res\"").unwrap());
        cfg.bot.ideas = Some(toml::from_str("forum = \"ideas\"").unwrap());
        cfg.bot.events = Some(Default::default());
        cfg.bot.recording = Some(toml::from_str("channel = \"committee\"").unwrap());
        cfg.federation.peers.push(chipsoc_config::schema::Peer {
            name: "Them".into(),
            id: "1".into(),
            send: vec!["announcements".into(), "events".into()],
            ..Default::default()
        });
        assert!(shares_on_demand(&cfg));
        let text = help(&cfg);
        assert!(text.chars().count() <= ui::MAX_CONTENT, "{} characters: {text}", text.chars().count());
        assert!(text.ends_with("then any command."), "nothing cut: {text}");
        for cmd in ["/record start|stop", "/server feed", "/server idea close", "/server resource", "approvals"] {
            assert!(text.contains(cmd), "{cmd} missing: {text}");
        }
    }

    #[test]
    fn config_commands_are_ops_and_risky_ones_ask_first() {
        let open = Cmd::Applications { slug: "golem".into(), open: true };
        assert_eq!(op(&open), Some(Op::SetProject { slug: "golem".into(), set: ProjectField::Applications(true) }));
        let close = Cmd::PositionOpen { role: "chair".into(), open: false };
        assert_eq!(op(&close), Some(Op::SetPosition { role: "chair".into(), set: PositionField::Open(false) }));
        assert_eq!(op(&Cmd::Queue), None);
        assert!(sure(&open).is_none() && sure(&Cmd::AddFeed("x".into())).is_none());
        let past = Cmd::ProjectStatus { slug: "golem".into(), status: ProjectStatus::Past };
        let proposed = Cmd::ProjectStatus { slug: "golem".into(), status: ProjectStatus::Proposed };
        assert!(sure(&past).is_some() && sure(&proposed).is_some(), "every change of status asks first");
        let mut cfg = Config::default();
        cfg.projects.push(chipsoc_config::Project {
            slug: "golem".into(),
            name: "Golem".into(),
            status: ProjectStatus::Active,
            ..Default::default()
        });
        let q = sure(&proposed).unwrap().question(&cfg, None);
        let feed = Sure::RemoveFeed(3).question(&cfg, Some("Chip news"));
        assert_eq!(feed, "Stop following **Chip news** (#3)?");
        assert!(q.starts_with("Make **Golem** proposed? Its channels move to the archive"), "{q}");
        assert!(q.contains("Its applications close."), "{q}");
        for s in [
            sure(&past).unwrap(),
            Sure::RemoveFeed(3),
            Sure::ArchiveResource(Snowflake(99)),
            Sure::Unappoint { role: "proj-a:b".into(), user: Snowflake(5) },
            Sure::Undo { commit: "a".repeat(40), how: Undo::Rewind },
            Sure::Undo { commit: "abc1234".into(), how: Undo::Revert },
            Sure::CloseIdea { post: Snowflake(u64::MAX), reason: reason_key(Some("a reason")) },
            Sure::CloseIdea { post: Snowflake(5), reason: String::new() },
        ] {
            let id = CustomId::Sure(s.encode()).to_string();
            assert!(id.len() <= 100, "{id}");
            let Some(CustomId::Sure(back)) = CustomId::parse(&id) else { panic!("{id}") };
            assert_eq!(Sure::decode(&back), Some(s));
        }
        assert_eq!(Sure::decode("status:done:golem"), None);
        assert_eq!(Sure::decode(CANCEL), None, "Cancel is never taken for a change");
    }

    /// Revert, rewind and closing an idea ask first; the reason is kept for
    /// Confirm and shown as it will be posted.
    #[test]
    fn going_back_and_closing_ideas_ask_first() {
        let cfg = Config::default();
        let rewind = Cmd::Undo { commit: "abc1234".into(), how: Undo::Rewind };
        let q = sure(&rewind).unwrap().question(&cfg, None);
        assert!(
            q.starts_with("Put the config back as it was right after `abc1234`?")
                && q.contains("Every later change is undone"),
            "{q}"
        );
        let revert = sure(&Cmd::Undo { commit: "abc1234".into(), how: Undo::Revert }).unwrap().question(&cfg, None);
        assert!(revert.starts_with("Undo change `abc1234`? Only that change is undone"), "{revert}");

        let close = Cmd::CloseIdea { post: Snowflake(55), reason: Some("Merged into Golem.".into()) };
        let asked = sure(&close).unwrap();
        let Sure::CloseIdea { reason: key, .. } = &asked else { panic!() };
        assert_eq!(kept_reason(key), None, "kept only once asked");
        keep_reason(Some("Merged into Golem."));
        assert_eq!(kept_reason(key).as_deref(), Some("Merged into Golem."));
        let q = asked.question(&cfg, None);
        assert_eq!(
            q,
            "Close the idea <#55>? It's locked and archived (never deleted), and this is posted in it:\n> 🔒 **Closed:** Merged into Golem.\n> It stays here to read."
        );
        let none = sure(&Cmd::CloseIdea { post: Snowflake(55), reason: None }).unwrap().question(&cfg, None);
        assert!(none.ends_with("> 🔒 **Closed.** It stays here to read."), "{none}");

        let bad = vec![opt("revert", CommandOptionValue::SubCommand(vec![opt("change", s("--help"))]))];
        assert!(parse(&bad).unwrap_err().contains("change's id"));
    }

    /// Discord refuses the whole command set if any limit is broken.
    #[test]
    fn the_command_fits_discords_limits() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        let write = |f: &str, body: &str| {
            std::fs::create_dir_all(p.join(f).parent().unwrap()).unwrap();
            std::fs::write(p.join(f), body).unwrap();
        };
        write("server.toml", "[guild]\nname = \"T\"\n");
        write(
            "roles.toml",
            "[[role]]\nkey = \"chair\"\nname = \"Chair\"\n\n[[role]]\nkey = \"everyone\"\nname = \"@everyone\"\n",
        );
        write(
            "categories/10-main.toml",
            "[category]\nkey = \"main\"\nname = \"Main\"\n\n[[channel]]\nkey = \"general\"\nname = \"general\"\n\n\
             [[channel]]\nkey = \"res\"\ntype = \"forum\"\nname = \"res\"\n",
        );
        write(
            "bot.toml",
            "name = \"T\"\n[[position]]\nrole = \"chair\"\n[feeds]\nchannel = \"general\"\n[resources]\nforum = \"res\"\n",
        );
        write("members.toml", "chair = []\n");
        let projects: String = (0..30).map(|n| format!("[[project]]\nslug = \"p{n}\"\nname = \"P{n}\"\n\n")).collect();
        write("projects.toml", &projects);
        let cfg = Config::load(p).unwrap();
        let c = command(&cfg);
        let subs = &c.options;
        assert!(subs.len() <= 25 && c.description.len() <= 100);
        let names: Vec<&str> = subs.iter().map(|o| o.name.as_str()).collect();
        for want in
            ["help", "status", "drift", "plan", "apply", "queue", "member", "project", "position", "feed", "resource"]
        {
            assert!(names.contains(&want), "{names:?}");
        }
        fn check(o: &CommandOption) {
            assert!(o.name.len() <= 32 && o.name == o.name.to_lowercase(), "{}", o.name);
            assert!(!o.description.is_empty() && o.description.chars().count() <= 100, "{}", o.description);
            assert!(o.choices.as_ref().is_none_or(|c| c.len() <= 25), "{}", o.name);
            let inner = o.options.as_deref().unwrap_or_default();
            assert!(inner.len() <= 25, "{}", o.name);
            // Required options come before optional ones.
            let first_optional = inner.iter().position(|x| {
                x.required != Some(true)
                    && x.kind != CommandOptionType::SubCommand
                    && x.kind != CommandOptionType::SubCommandGroup
            });
            if let Some(at) = first_optional {
                assert!(inner[at..].iter().all(|x| x.required != Some(true)), "{}", o.name);
            }
            inner.iter().for_each(check);
        }
        subs.iter().for_each(check);
        let project = subs.iter().find(|o| o.name == "project").unwrap();
        let status = &project.options.as_ref().unwrap()[0].options.as_ref().unwrap()[0];
        assert!(status.choices.is_none() && status.required == Some(true), "30 projects: typed, not listed");
    }
}

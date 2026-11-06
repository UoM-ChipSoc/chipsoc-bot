//! Slash commands (registered only for features that are on) and the routing
//! of every command, button, menu and form to its handler.

use std::fmt;

use twilight_model::application::command::{
    Command, CommandOption, CommandOptionChoice, CommandOptionChoiceValue, CommandOptionType, CommandType,
};
use twilight_model::application::interaction::application_command::CommandOptionValue;
use twilight_model::application::interaction::{Interaction, InteractionData};
use twilight_model::gateway::payload::incoming::InteractionCreate;
use twilight_model::guild::Permissions;
use twilight_model::id::Id;
use twilight_model::id::marker::GuildMarker;

use crate::review::{self, Form};
use crate::{Ctx, apps, control, fedrecv, fedshare, help, join, manage, profile, record, roster, trust, ui};

/// What a button, menu or form is for. Encoded in Discord's `custom_id`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CustomId {
    /// A button the config put on a post (`{button:…}`), e.g. `volunteer`, `apply:golem`.
    Do(String),
    /// The position picker after pressing "volunteer".
    PickPosition,
    VolunteerForm(String),
    ApplyForm(String),
    IdeaForm,
    ResourceForm,
    EventForm,
    FeedForm,
    ProfileForm,
    Review {
        accept: bool,
        id: i64,
    },
    /// Accept on a card showing one version of the entry (see
    /// [`chipsoc_admin::shares::version`]): refused once it has changed.
    ReviewSeen {
        id: i64,
        seen: String,
    },
    /// Apply a partner's held sync: the batch an admin was asked about.
    AllowSync {
        peer: String,
        batch: String,
    },
    /// Drop a partner's held sync instead (the partner isn't told).
    RefuseSync {
        peer: String,
        batch: String,
    },
    /// Apply or dismiss a posted plan (its id in [`crate::plans::Plans`]).
    Plan {
        apply: bool,
        id: String,
    },
    /// Make an accepted project applicant a lead (application id).
    MakeLead(i64),
    /// Restore or ignore the old roles of someone who rejoined (user id).
    Rejoined {
        restore: bool,
        user: u64,
    },
    /// A step of the join flow (see [`crate::join`]).
    Join(String),
    /// Confirm or Cancel under a `/server` question (see [`crate::manage::Sure`]).
    Sure(String),
    /// The form asking why, after Reject on a review card (application id).
    RejectForm(i64),
    /// Approve or reject held-back config changes (see [`crate::trust`]).
    Trust {
        approve: bool,
        hash: String,
    },
    /// "Edit and resend" under a refused form: the form (its id) again, filled in.
    Resend(String),
    /// "Reject the rest…" on a window's card (the position's role key).
    RejectRest(String),
    /// The form asking why, after "Reject the rest…" (role key).
    RestForm(String),
    /// Partners picked under `/server share` (what's shared, see [`crate::fedshare`]).
    ShareTo(String),
}

impl fmt::Display for CustomId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Do(a) => write!(f, "do:{a}"),
            Self::PickPosition => f.write_str("pick-position"),
            Self::VolunteerForm(r) => write!(f, "vol:{r}"),
            Self::ApplyForm(p) => write!(f, "app:{p}"),
            Self::IdeaForm => f.write_str("idea"),
            Self::ResourceForm => f.write_str("resource"),
            Self::EventForm => f.write_str("event"),
            Self::FeedForm => f.write_str("feed"),
            Self::ProfileForm => f.write_str("profile"),
            Self::Review { accept, id } => write!(f, "rev:{}:{id}", if *accept { "accept" } else { "reject" }),
            Self::ReviewSeen { id, seen } => write!(f, "rev:accept:{id}:{seen}"),
            Self::AllowSync { peer, batch } => write!(f, "fedok:{peer}:{batch}"),
            Self::RefuseSync { peer, batch } => write!(f, "fedno:{peer}:{batch}"),
            Self::Plan { apply, id } => write!(f, "plan:{}:{id}", if *apply { "apply" } else { "dismiss" }),
            Self::MakeLead(id) => write!(f, "lead:{id}"),
            Self::Rejoined { restore, user } => {
                write!(f, "back:{}:{user}", if *restore { "restore" } else { "ignore" })
            }
            Self::Join(a) => write!(f, "join:{a}"),
            Self::Sure(what) => write!(f, "sure:{what}"),
            Self::RejectForm(id) => write!(f, "rej:{id}"),
            Self::Trust { approve, hash } => write!(f, "trust:{}:{hash}", if *approve { "ok" } else { "no" }),
            Self::Resend(form) => write!(f, "again:{form}"),
            Self::RejectRest(role) => write!(f, "rest:{role}"),
            Self::RestForm(role) => write!(f, "restform:{role}"),
            Self::ShareTo(what) => write!(f, "shareto:{what}"),
        }
    }
}

impl CustomId {
    /// The id for a button or form in a DM: it carries the server (see
    /// [`chipsoc_admin::in_dm`]), so the press finds its way back.
    pub fn in_dm(&self, guild: Id<GuildMarker>) -> String {
        chipsoc_admin::in_dm(&self.to_string(), chipsoc_config::Snowflake(guild.get()))
    }

    /// [`CustomId::in_dm`] when `i` came from a DM, else as it is.
    pub fn for_reply(&self, ctx: &Ctx, i: &Interaction) -> String {
        if i.guild_id.is_some() { self.to_string() } else { self.in_dm(ctx.guild) }
    }

    pub fn parse(s: &str) -> Option<Self> {
        // The server of a DM's button (see `in_dm`): routing has already used it.
        let s = chipsoc_admin::server_in_dm(s).map_or(s, |(id, _)| id);
        let (head, rest) = s.split_once(':').unwrap_or((s, ""));
        Some(match (head, rest) {
            ("do", a) => Self::Do(a.to_owned()),
            ("pick-position", _) => Self::PickPosition,
            ("vol", r) => Self::VolunteerForm(r.to_owned()),
            ("app", p) => Self::ApplyForm(p.to_owned()),
            ("idea", _) => Self::IdeaForm,
            ("resource", _) => Self::ResourceForm,
            ("event", _) => Self::EventForm,
            ("feed", _) => Self::FeedForm,
            ("profile", _) => Self::ProfileForm,
            ("rev", r) => {
                let (decision, id) = r.split_once(':')?;
                match id.split_once(':') {
                    Some((id, seen)) if decision == "accept" => {
                        Self::ReviewSeen { id: id.parse().ok()?, seen: seen.to_owned() }
                    }
                    Some(_) => return None,
                    None => Self::Review { accept: yes_no(decision, "accept", "reject")?, id: id.parse().ok()? },
                }
            }
            ("fedok", r) => {
                // Buttons from before batches had none: they're stale.
                let (peer, batch) = r.rsplit_once(':').unwrap_or((r, ""));
                Self::AllowSync { peer: peer.to_owned(), batch: batch.to_owned() }
            }
            ("fedno", r) => {
                let (peer, batch) = r.rsplit_once(':')?;
                Self::RefuseSync { peer: peer.to_owned(), batch: batch.to_owned() }
            }
            ("plan", r) => {
                let (what, id) = r.split_once(':')?;
                Self::Plan { apply: yes_no(what, "apply", "dismiss")?, id: id.to_owned() }
            }
            ("lead", id) => Self::MakeLead(id.parse().ok()?),
            ("back", r) => {
                let (what, user) = r.split_once(':')?;
                Self::Rejoined { restore: yes_no(what, "restore", "ignore")?, user: user.parse().ok()? }
            }
            ("join", a) => Self::Join(a.to_owned()),
            ("sure", what) => Self::Sure(what.to_owned()),
            ("rej", id) => Self::RejectForm(id.parse().ok()?),
            ("trust", r) => {
                let (what, hash) = r.split_once(':')?;
                Self::Trust { approve: yes_no(what, "ok", "no")?, hash: hash.to_owned() }
            }
            ("again", form) if !form.is_empty() => Self::Resend(form.to_owned()),
            ("rest", role) if !role.is_empty() => Self::RejectRest(role.to_owned()),
            ("restform", role) if !role.is_empty() => Self::RestForm(role.to_owned()),
            ("shareto", what) if !what.is_empty() => Self::ShareTo(what.to_owned()),
            _ => return None,
        })
    }
}

/// `yes` as true, `no` as false; anything else (a malformed id) isn't either.
fn yes_no(word: &str, yes: &str, no: &str) -> Option<bool> {
    match word {
        w if w == yes => Some(true),
        w if w == no => Some(false),
        _ => None,
    }
}

/// The server a DM'd button belongs to (`…@<guild id>`).
pub fn guild_in_custom_id(i: &InteractionCreate) -> Option<Id<GuildMarker>> {
    let id = match &i.data {
        Some(InteractionData::MessageComponent(c)) => c.custom_id.as_str(),
        Some(InteractionData::ModalSubmit(m)) => m.custom_id.as_str(),
        _ => return None,
    };
    chipsoc_admin::server_in_dm(id).and_then(|(_, g)| Id::new_checked(g.0))
}

pub async fn interaction(ctx: &Ctx, i: &Interaction) {
    // A DM carries its server only in the custom id, which the sender controls.
    if i.guild_id.is_none() && !member_of(ctx, i).await {
        return ctx.respond(i, &ui::ephemeral("This is only for members of that server.")).await;
    }
    match &i.data {
        Some(InteractionData::ApplicationCommand(cmd)) => {
            let opt = |name: &str| {
                cmd.options.iter().find(|o| o.name == name).and_then(|o| match &o.value {
                    CommandOptionValue::String(s) => Some(s.clone()),
                    _ => None,
                })
            };
            match cmd.name.as_str() {
                "volunteer" => apps::volunteer(ctx, i, opt("position")).await,
                "apply" => apps::apply(ctx, i, opt("project")).await,
                "propose-project" => review::open(ctx, i, Form::Idea, None).await,
                "share-resource" => review::open(ctx, i, Form::Resource, None).await,
                "event" => review::open(ctx, i, Form::Event, None).await,
                "suggest-feed" => review::open(ctx, i, Form::Feed, None).await,
                "solved" => help::solved(ctx, i).await,
                "partners" => fedrecv::partners(ctx, i).await,
                "profile" => profile::open(ctx, i).await,
                "server" => manage::server(ctx, i, &cmd.options).await,
                "record" => record::handle(ctx, i, &cmd.options).await,
                _ => unknown(ctx, i).await,
            }
        }
        Some(InteractionData::MessageComponent(c)) => match CustomId::parse(&c.custom_id) {
            Some(CustomId::Do(action)) => button(ctx, i, &action).await,
            Some(CustomId::PickPosition) => apps::volunteer(ctx, i, c.values.first().cloned()).await,
            // Rejecting asks for a reason first.
            Some(CustomId::Review { accept: false, id }) => manage::reject_form(ctx, i, id).await,
            Some(CustomId::Review { accept, id }) => apps::review(ctx, i, accept, id, None).await,
            Some(CustomId::ReviewSeen { id, seen }) => apps::review(ctx, i, true, id, Some(&seen)).await,
            Some(CustomId::AllowSync { peer, batch }) => fedrecv::allow(ctx, i, &peer, &batch).await,
            Some(CustomId::RefuseSync { peer, batch }) => fedrecv::refuse_button(ctx, i, &peer, &batch).await,
            Some(CustomId::Plan { apply, id }) => control::plan_button(ctx, i, apply, &id).await,
            Some(CustomId::MakeLead(id)) => apps::make_lead(ctx, i, id).await,
            Some(CustomId::Rejoined { restore, user }) => roster::rejoined(ctx, i, restore, user).await,
            Some(CustomId::Join(a)) => join::component(ctx, i, &a, &c.values).await,
            Some(CustomId::Sure(what)) => manage::confirmed(ctx, i, &what).await,
            Some(CustomId::Trust { approve, hash }) => trust::button(ctx, i, approve, &hash).await,
            Some(CustomId::Resend(form)) => review::resend(ctx, i, &form).await,
            Some(CustomId::RejectRest(role)) => apps::reject_rest_form(ctx, i, &role).await,
            Some(CustomId::ShareTo(what)) => fedshare::picked(ctx, i, &what, &c.values).await,
            _ => unknown(ctx, i).await,
        },
        Some(InteractionData::ModalSubmit(m)) => {
            let v = ui::modal_values(m);
            let id = CustomId::parse(&m.custom_id);
            if let Some(form) = id.as_ref().and_then(Form::of) {
                return review::submit(ctx, i, form, v).await;
            }
            match id {
                Some(CustomId::ProfileForm) => profile::submit(ctx, i, &v.texts()).await,
                Some(CustomId::Join(a)) => join::modal(ctx, i, &a, &v.texts()).await,
                Some(CustomId::RejectForm(id)) => manage::reject_submit(ctx, i, id, Some(&v.text("reason"))).await,
                Some(CustomId::RestForm(role)) => apps::reject_rest(ctx, i, &role, &v.text("reason")).await,
                _ => unknown(ctx, i).await,
            }
        }
        _ => {}
    }
}

/// A command, button or form this version doesn't know (left from an older
/// one, say): said so, rather than left for Discord to call a failure.
pub(crate) async fn unknown(ctx: &Ctx, i: &Interaction) {
    tracing::info!(id = ?i.data, "an interaction nothing handles");
    let text = "The bot doesn't know that one any more (it may be from an older message). Try the command again.";
    ctx.respond(i, &ui::ephemeral(text)).await;
}

/// Whether whoever sent a DM interaction is a member of `ctx`'s server
/// (asked of Discord; false if it can't be told).
async fn member_of(ctx: &Ctx, i: &Interaction) -> bool {
    let Some(user) = i.user.as_ref() else { return false };
    ctx.get(&format!("guilds/{}/members/{}", ctx.guild, user.id)).await.is_ok()
}

/// A `{button:…}` from a managed post.
async fn button(ctx: &Ctx, i: &Interaction, action: &str) {
    use chipsoc_config::template::ButtonAction as A;
    match A::parse(action) {
        Some(A::Apply(slug)) => apps::apply(ctx, i, Some(slug)).await,
        Some(A::Join(_)) => join::start(ctx, i, None).await,
        Some(A::Volunteer) => apps::volunteer(ctx, i, None).await,
        Some(A::ProposeProject) => review::open(ctx, i, Form::Idea, None).await,
        Some(A::SuggestFeed) => review::open(ctx, i, Form::Feed, None).await,
        Some(A::ShareResource) => review::open(ctx, i, Form::Resource, None).await,
        Some(A::Event) => review::open(ctx, i, Form::Event, None).await,
        Some(A::Profile) => profile::open(ctx, i).await,
        None => ctx.respond(i, &ui::ephemeral("That button isn't set up here any more.")).await,
    }
}

/// A slash command. `perms` limits who Discord shows it to; handlers check
/// admins themselves either way.
#[allow(deprecated)] // dm_permission is required by the struct; left None
pub(crate) fn command(
    name: &str,
    description: &str,
    options: Vec<CommandOption>,
    perms: Option<Permissions>,
) -> Command {
    Command {
        application_id: None,
        contexts: None,
        default_member_permissions: perms,
        description: description.to_owned(),
        description_localizations: None,
        dm_permission: None,
        guild_id: None,
        id: None,
        integration_types: None,
        kind: CommandType::ChatInput,
        name: name.to_owned(),
        name_localizations: None,
        nsfw: None,
        options,
        version: Id::new(1),
    }
}

pub(crate) fn option(kind: CommandOptionType, name: &str, description: &str) -> CommandOption {
    CommandOption {
        autocomplete: None,
        channel_types: None,
        choices: None,
        description: description.to_owned(),
        description_localizations: None,
        kind,
        max_length: None,
        max_value: None,
        min_length: None,
        min_value: None,
        name: name.to_owned(),
        name_localizations: None,
        options: None,
        file_types: None,
        required: None,
    }
}

pub(crate) fn choice(name: &str, description: &str, choices: Vec<(String, String)>) -> CommandOption {
    let choices = choices
        .into_iter()
        .take(25)
        .map(|(name, value)| CommandOptionChoice {
            name: ui::truncate(&name, 100),
            name_localizations: None,
            value: CommandOptionChoiceValue::String(value),
        })
        .collect();
    CommandOption {
        choices: Some(choices),
        required: Some(true),
        ..option(CommandOptionType::String, name, description)
    }
}

pub(crate) fn subcommand(name: &str, description: &str, options: Vec<CommandOption>) -> CommandOption {
    CommandOption { options: Some(options), ..option(CommandOptionType::SubCommand, name, description) }
}

/// The commands for the features `bot.toml` turns on.
pub fn commands(ctx: &Ctx) -> Vec<Command> {
    let cfg = ctx.cfg();
    let b = &cfg.bot;
    let name = ctx.name();
    let mut v = Vec::new();
    if b.review.is_some() && !b.positions.is_empty() {
        let positions: Vec<(String, String)> =
            apps::positions(ctx).into_iter().map(|(key, label, _)| (label, key)).collect();
        // With nothing open there's nothing to pick (Discord refuses an empty
        // list, and with it every command): it says so instead. With one,
        // there's nothing to choose: the form opens.
        let options = if positions.len() <= 1 { vec![] } else { vec![choice("position", "Which position", positions)] };
        v.push(command("volunteer", &format!("Apply for a position helping run {name}"), options, None));
    }
    // Only active projects take members, so only they are offered.
    let open: Vec<(String, String)> =
        cfg.projects.iter().filter(|p| apps::taking(&cfg, p)).map(|p| (p.display(), p.slug.clone())).collect();
    if !open.is_empty() {
        v.push(command("apply", "Apply to join a project team", vec![choice("project", "Which project", open)], None));
    }
    if b.ideas.is_some() && b.review.is_some() {
        v.push(command("propose-project", "Propose a project", vec![], None));
    }
    if b.resources.is_some() && b.review.is_some() {
        v.push(command("share-resource", "Share a useful link: a guide, book, tool or paper", vec![], None));
    }
    if b.events.is_some() {
        let about = if b.event_suggestions() {
            "Suggest an event (admins' events go straight on the calendar)"
        } else {
            "Create an event (admins)"
        };
        v.push(command("event", about, vec![], None));
    }
    if b.feeds.is_some() && b.review.is_some() {
        v.push(command("suggest-feed", "Suggest a blog, newsletter or channel to follow", vec![], None));
    }
    if let Some(h) = &b.help {
        v.push(command("solved", &format!("Mark this #{} post solved", h.forum), vec![], None));
    }
    if cfg.federation.peers.iter().any(|p| !p.id.is_empty()) {
        v.push(command("partners", "Projects shared by partner communities", vec![], None));
    }
    if cfg.join.is_some() {
        v.push(command("profile", "Update your details: the welcome questions and your name", vec![], None));
    } else if b.profile.is_some() {
        v.push(command("profile", "Set the name people see here", vec![], None));
    }
    if b.recording.is_some() {
        v.push(record::definition());
    }
    v.push(manage::command(&cfg));
    v
}

pub async fn register(ctx: &Ctx) {
    let cmds = commands(ctx);
    let path = format!("applications/{}/guilds/{}/commands", ctx.app, ctx.guild);
    let set = async { ctx.call(chipsoc_discord::Method::Put, &path, Some(serde_json::to_value(&cmds)?)).await };
    match set.await {
        Ok(_) => tracing::info!(guild = %ctx.guild, count = cmds.len(), "slash commands registered"),
        Err(e) => {
            tracing::warn!(?e, guild = %ctx.guild, "registering slash commands failed");
            ctx.log_bot(&format!(
                "⚠️ **Discord refused the slash commands**, so `/server` and the others may be missing or out of date: {}",
                chipsoc_admin::fmt::error(&e)
            ))
            .await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::CustomId;

    #[test]
    fn custom_ids_round_trip() {
        for id in [
            CustomId::Do("apply:golem".into()),
            CustomId::PickPosition,
            CustomId::VolunteerForm("treasurer".into()),
            CustomId::Review { accept: false, id: 7 },
            CustomId::ReviewSeen { id: 7, seen: "0123456789abcdef".into() },
            CustomId::ProfileForm,
            CustomId::IdeaForm,
            CustomId::ResourceForm,
            CustomId::AllowSync { peer: "sharc".into(), batch: "0123456789abcdef".into() },
            CustomId::RefuseSync { peer: "sharc".into(), batch: "0123456789abcdef".into() },
            CustomId::Sure("cancel".into()),
            CustomId::Plan { apply: true, id: "0123456789".into() },
            CustomId::Plan { apply: false, id: "ab".into() },
            CustomId::MakeLead(3),
            CustomId::Rejoined { restore: true, user: 42 },
            CustomId::Rejoined { restore: false, user: 42 },
            CustomId::Join("form:name".into()),
            CustomId::Sure("status:past:golem".into()),
            CustomId::RejectForm(12),
            CustomId::Resend("vol:treasurer".into()),
            CustomId::Resend("idea".into()),
            CustomId::RejectRest("treasurer".into()),
            CustomId::RestForm("treasurer".into()),
            CustomId::EventForm,
        ] {
            assert_eq!(CustomId::parse(&id.to_string()), Some(id));
        }
        assert_eq!(CustomId::parse("do:profile@123"), Some(CustomId::Do("profile".into())));
        assert_eq!(CustomId::parse("rev:accept:x"), None);
        assert_eq!(CustomId::parse("rev:maybe:3"), None, "not read as a reject");
        assert_eq!(CustomId::parse("rev:reject:3:ab"), None);
        assert_eq!(
            CustomId::parse(&chipsoc_admin::rounds::review_seen(5, "ab")),
            Some(CustomId::ReviewSeen { id: 5, seen: "ab".into() })
        );
        assert_eq!(CustomId::parse("plan:x:ab"), None);
        assert_eq!(CustomId::parse("trust:yes:ab"), None);
        let old = CustomId::AllowSync { peer: "sharc".into(), batch: String::new() };
        assert_eq!(CustomId::parse("fedok:sharc"), Some(old), "an Allow button from before batches: stale");
        assert_eq!(CustomId::parse("back:restore:x"), None);
        // The ids the shared code puts on a round's card.
        assert_eq!(
            CustomId::parse(&chipsoc_admin::rounds::review_button(false, 4)),
            Some(CustomId::Review { accept: false, id: 4 })
        );
        assert_eq!(
            CustomId::parse(&chipsoc_admin::rounds::rest_button("chair")),
            Some(CustomId::RejectRest("chair".into()))
        );
        let again = CustomId::parse("again:vol:treasurer@1000").unwrap();
        assert_eq!(again, CustomId::Resend("vol:treasurer".into()), "from a DM");
    }
}

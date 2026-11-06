//! Member forms that go to the review queue (`/volunteer`, `/apply`,
//! `/propose-project`, `/share-resource`, `/suggest-feed`, `/event`), all
//! one way: checked when the form opens and again when it's sent (the
//! feature is set up, it's still open, they needn't wait); answered at once
//! ("thinking…"), so Discord never calls it a failure; checked; filed as a
//! card in the review channel; and always answered. A refusal they can fix
//! (a wait, a bad link, a duplicate, an unreadable time) keeps what they
//! typed for a while, with an **Edit and resend** button that opens the form
//! again filled in.

use chipsoc_admin::Answer;
use chipsoc_config::Snowflake;
use chipsoc_store::{AppKind, Round};
use twilight_model::application::interaction::Interaction;
use twilight_model::channel::message::component::{ButtonStyle, Component};
use twilight_model::http::interaction::InteractionResponse;
use twilight_model::user::User;

use crate::apps::{invoker, post_review};
use crate::commands::CustomId;
use crate::limits::Kind;
use crate::ui::{self, Values};
use crate::{Ctx, apps, events, feeds, ideas, now, resources};

/// How long what someone typed into a refused form is kept for "Edit and resend".
pub const DRAFT_LIFE: i64 = 30 * 60;

/// Which member form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Form {
    /// A position (role key).
    Volunteer(String),
    /// A project team (slug).
    Apply(String),
    Idea,
    Resource,
    Feed,
    /// Suggested by a member, or made at once by an admin.
    Event,
}

impl Form {
    /// The form a modal id is for.
    pub fn of(id: &CustomId) -> Option<Self> {
        Some(match id {
            CustomId::VolunteerForm(r) => Self::Volunteer(r.clone()),
            CustomId::ApplyForm(s) => Self::Apply(s.clone()),
            CustomId::IdeaForm => Self::Idea,
            CustomId::ResourceForm => Self::Resource,
            CustomId::FeedForm => Self::Feed,
            CustomId::EventForm => Self::Event,
            _ => return None,
        })
    }

    /// Its modal's id (and the key its drafts are kept under).
    pub fn id(&self) -> CustomId {
        match self {
            Self::Volunteer(r) => CustomId::VolunteerForm(r.clone()),
            Self::Apply(s) => CustomId::ApplyForm(s.clone()),
            Self::Idea => CustomId::IdeaForm,
            Self::Resource => CustomId::ResourceForm,
            Self::Feed => CustomId::FeedForm,
            Self::Event => CustomId::EventForm,
        }
    }

    /// How often one person may send it; admins making events aren't limited.
    fn limit(&self, ctx: &Ctx, i: &Interaction) -> Option<Kind> {
        Some(match self {
            Self::Volunteer(_) => Kind::Volunteer,
            Self::Apply(_) => Kind::Apply,
            Self::Idea => Kind::Idea,
            Self::Resource => Kind::Resource,
            Self::Feed => Kind::Feed,
            Self::Event if ctx.is_admin(i.member.as_ref()) => return None,
            Self::Event => Kind::Event,
        })
    }

    /// Why it can't be used now (the feature isn't set up, it's closed, they
    /// hold it already), as a reply. Asked when it opens and when it's sent.
    async fn ready(&self, ctx: &Ctx, i: &Interaction, user: &User) -> Result<(), String> {
        match self {
            Self::Volunteer(role) => apps::volunteer_ready(ctx, role, user).await,
            Self::Apply(slug) => apps::apply_ready(ctx, slug, user).await,
            Self::Idea => ideas::ready(ctx),
            Self::Resource => resources::ready(ctx),
            Self::Feed => feeds::ready(ctx),
            Self::Event => events::ready(ctx, i),
        }
    }

    /// The form: its title and boxes.
    fn modal(&self, ctx: &Ctx, i: &Interaction) -> Option<(String, Vec<Component>)> {
        match self {
            Self::Volunteer(role) => apps::volunteer_form(ctx, role),
            Self::Apply(slug) => apps::apply_form(ctx, slug),
            Self::Idea => Some(ideas::form(ctx)),
            Self::Resource => Some(resources::form(ctx)),
            Self::Feed => Some(feeds::form()),
            Self::Event => Some(events::form(ctx, i)),
        }
    }

    /// Checks on what they typed that cost nothing (a typo): one refused
    /// here gives the turn back.
    fn precheck(&self, v: &Values) -> Result<(), Refusal> {
        match self {
            Self::Resource => resources::precheck(v),
            Self::Feed => feeds::precheck(v),
            _ => Ok(()),
        }
    }

    /// Whether checking it makes the bot work for it (a forum read, a
    /// fetch): past [`Form::precheck`] the turn counts, refused or not.
    fn costs(&self) -> bool {
        matches!(self, Self::Resource | Self::Feed)
    }

    /// Check what they sent: what to file for review, or done already.
    async fn vet(&self, ctx: &Ctx, i: &Interaction, user: &User, v: &Values) -> Result<Vetted, Refusal> {
        match self {
            Self::Volunteer(role) => apps::vet_volunteer(ctx, role, user, v),
            Self::Apply(slug) => apps::vet_apply(ctx, slug, user, v),
            Self::Idea => Ok(ideas::vet(ctx, v)),
            Self::Resource => resources::vet(ctx, v).await,
            Self::Feed => feeds::vet(ctx, v).await,
            Self::Event => events::vet(ctx, i, user, v).await,
        }
    }
}

/// What a checked form files in the review queue.
pub struct Filing {
    pub kind: AppKind,
    /// Role key, project slug, feed address, idea name, link or event title.
    pub target: String,
    /// The card's title, e.g. "Position · Treasurer" (escaped where they wrote it).
    pub title: String,
    pub answers: Vec<Answer>,
    /// They'd like to lead it.
    pub lead: bool,
    /// A line at the end of the card.
    pub extra: Option<String>,
    /// Show who they are (their join answers) under their name.
    pub who: bool,
    /// What they're told once it's in.
    pub thanks: String,
    /// Waiting for a window to close (a line in the review channel, not a card).
    pub round: Round,
}

impl Filing {
    /// A filing with a card of its own, no lead wish or extra line, and nothing about who they are.
    pub fn new(kind: AppKind, target: &str, title: String, answers: Vec<Answer>, thanks: String) -> Self {
        Self {
            kind,
            target: target.to_owned(),
            title,
            answers,
            lead: false,
            extra: None,
            who: false,
            thanks,
            round: Round::Single,
        }
    }
}

/// How a form checked out.
pub enum Vetted {
    /// To file for review.
    Review(Filing),
    /// Done already (an admin's event): what to tell them, and buttons or menus to go with it.
    Done(String, Vec<Component>),
}

/// Why a form was turned away.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub text: String,
    /// They can fix it and send it again: what they typed is kept, with "Edit and resend".
    pub fixable: bool,
}

impl Refusal {
    /// Something they can fix.
    pub fn fix(text: impl Into<String>) -> Self {
        Self { text: text.into(), fixable: true }
    }

    /// Something sending it again won't change.
    pub fn last(text: impl Into<String>) -> Self {
        Self { text: text.into(), fixable: false }
    }
}

const NOBODY: &str = "Couldn't tell who sent that; please try again.";

/// The "Edit and resend" button for `form`.
fn resend_button(ctx: &Ctx, i: &Interaction, form: &Form) -> Component {
    resend_row(ctx, i, &form.id())
}

/// The "Edit and resend" button for the form with id `form`.
pub fn resend_row(ctx: &Ctx, i: &Interaction, form: &CustomId) -> Component {
    let id = CustomId::Resend(form.to_string()).for_reply(ctx, i);
    ui::row(vec![ui::button(&id, "Edit and resend", ButtonStyle::Primary)])
}

/// Open `form`, filled in with `filled` if they're sending it again.
pub async fn open(ctx: &Ctx, i: &Interaction, form: Form, filled: Option<Values>) {
    let Some(user) = invoker(i) else { return ctx.respond(i, &ui::ephemeral(NOBODY)).await };
    if let Err(why) = form.ready(ctx, i, user).await {
        return ctx.respond(i, &ui::ephemeral(&why)).await;
    }
    // Told before they type, not after.
    if let Some(secs) = form.limit(ctx, i).and_then(|k| ctx.limits.wait(k, user.id.get())) {
        let again = if filled.is_some() { vec![resend_button(ctx, i, &form)] } else { Vec::new() };
        return ctx.respond(i, &ui::ephemeral_with(&crate::limits::waiting(secs), again)).await;
    }
    let Some((title, mut fields)) = form.modal(ctx, i) else {
        return ctx.respond(i, &ui::ephemeral("That form isn't set up here any more.")).await;
    };
    if let Some(v) = &filled {
        ui::fill(&mut fields, v);
    }
    ctx.respond(i, &modal(ctx, i, &form, &title, fields)).await;
}

fn modal(ctx: &Ctx, i: &Interaction, form: &Form, title: &str, fields: Vec<Component>) -> InteractionResponse {
    ui::modal(&form.id().for_reply(ctx, i), title, fields)
}

/// "Edit and resend": the form again, filled in with what they typed.
pub async fn resend(ctx: &Ctx, i: &Interaction, form_id: &str) {
    let id = CustomId::parse(form_id);
    if id == Some(CustomId::ProfileForm) {
        // The profile form keeps what they sent itself.
        return crate::profile::open(ctx, i).await;
    }
    let Some(form) = id.as_ref().and_then(Form::of) else {
        return ctx.respond(i, &ui::ephemeral("That button isn't set up here any more.")).await;
    };
    let Some(user) = invoker(i) else { return ctx.respond(i, &ui::ephemeral(NOBODY)).await };
    let draft = match ctx.db().form_draft(&user.id.to_string(), &form.id().to_string(), now() - DRAFT_LIFE).await {
        Ok(d) => d.and_then(|d| serde_json::from_str::<Values>(&d).ok()),
        Err(e) => {
            tracing::warn!(?e, "reading a form draft failed");
            None
        }
    };
    // Gone (it was a while ago): the form opens empty rather than not at all.
    open(ctx, i, form, Some(draft.unwrap_or_default())).await;
}

async fn keep_draft(ctx: &Ctx, user: u64, form: &Form, v: &Values) {
    let Ok(json) = serde_json::to_string(v) else { return };
    let (user, form) = (user.to_string(), form.id().to_string());
    let saved = ctx.db().set_form_draft(&user, &form, &json, now(), now() - DRAFT_LIFE).await;
    ctx.log_err(saved, "keeping a form draft");
}

/// Turn the form away; one they can fix keeps what they typed, with "Edit and resend".
async fn refuse(ctx: &Ctx, i: &Interaction, user: u64, form: &Form, v: &Values, r: &Refusal) {
    if !r.fixable {
        return ctx.edit_response(i, &r.text, &[]).await;
    }
    keep_draft(ctx, user, form, v).await;
    ctx.edit_response(i, &r.text, &[resend_button(ctx, i, form)]).await;
}

/// A member form sent: checked, filed for review (or done), and answered.
pub async fn submit(ctx: &Ctx, i: &Interaction, form: Form, v: Values) {
    // Checking asks Discord (and may fetch a feed); Discord waits only 3 seconds.
    ctx.respond(i, &ui::defer()).await;
    let Some(user) = invoker(i) else { return ctx.edit_response(i, NOBODY, &[]).await };
    let uid = user.id.get();
    // Anyone can send a form with any id, so it's checked again here.
    if let Err(why) = form.ready(ctx, i, user).await {
        return ctx.edit_response(i, &why, &[]).await;
    }
    // The turn is taken before any work; only a slip found for free gives it back.
    let mut turn = match form.limit(ctx, i).map(|k| ctx.limits.turn(k, uid)) {
        None => None,
        Some(Ok(t)) => Some(t),
        Some(Err(wait)) => return refuse(ctx, i, uid, &form, &v, &Refusal::fix(wait)).await,
    };
    if let Err(r) = form.precheck(&v) {
        return refuse(ctx, i, uid, &form, &v, &r).await;
    }
    if form.costs() {
        // Past here the bot works for it, so it counts, refused or not.
        spend(turn.take());
    }
    let filing = match form.vet(ctx, i, user, &v).await {
        Ok(Vetted::Review(f)) => f,
        Ok(Vetted::Done(text, components)) => {
            spend(turn);
            forget_draft(ctx, uid, &form).await;
            return ctx.edit_response(i, &text, &components).await;
        }
        Err(r) => return refuse(ctx, i, uid, &form, &v, &r).await,
    };
    let Some(id) = record(ctx, user, &filing).await else {
        let again = Refusal::fix("Sorry, that didn't save. Please try again.");
        return refuse(ctx, i, uid, &form, &v, &again).await;
    };
    spend(turn);
    forget_draft(ctx, uid, &form).await;
    file(ctx, id, user, &filing).await;
    ctx.edit_response(i, &filing.thanks, &[]).await;
}

/// Keep the turn taken (if one was): it went through, or is about to cost real work.
fn spend(turn: Option<crate::limits::Turn<'_>>) {
    if let Some(t) = turn {
        t.used();
    }
}

async fn forget_draft(ctx: &Ctx, user: u64, form: &Form) {
    let forgot = ctx.db().forget_form_draft(&user.to_string(), &form.id().to_string()).await;
    ctx.log_err(forgot, "forgetting a form draft");
}

async fn record(ctx: &Ctx, user: &User, f: &Filing) -> Option<i64> {
    let json = Answer::to_json(&f.answers);
    let added =
        ctx.db().add_application_in(f.kind, &f.target, &user.id.to_string(), &json, f.lead, now(), f.round).await;
    match added {
        Ok(id) => Some(id),
        Err(e) => {
            tracing::warn!(?e, "storing an application failed");
            None
        }
    }
}

/// Put it in front of the admins: a card, or in a window, a line.
async fn file(ctx: &Ctx, id: i64, user: &User, f: &Filing) {
    let me = Snowflake(user.id.get());
    if f.round == Round::Waiting {
        return chipsoc_admin::rounds::noted(ctx, &f.target, me).await;
    }
    let who = if f.who { chipsoc_admin::join::summary(ctx, me).await } else { None };
    post_review(ctx, id, &f.title, user, who.as_deref(), &f.answers, f.extra.as_deref()).await;
}

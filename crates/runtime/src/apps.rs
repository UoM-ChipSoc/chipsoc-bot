//! `/volunteer` and `/apply`: forms that become review cards. Deciding is in
//! `chipsoc_admin::queue`. A card says who the applicant is in a line from
//! their join answers, so the forms don't ask again. One pending application
//! per person and target. Accepted project applicants who asked to lead can
//! be made leads.

use chipsoc_admin::{Actor, Answer};
use chipsoc_config::Snowflake;
use chipsoc_config::authoring::MAX_FORM_FIELDS;
use chipsoc_config::bot::{Position, Question};
use chipsoc_store::{AppKind, Round};
use twilight_model::application::interaction::Interaction;
use twilight_model::channel::message::component::{ButtonStyle, Component, Label, TextInput, TextInputStyle};
use twilight_model::user::User;

use crate::commands::CustomId;
use crate::review::{self, Filing, Form, Refusal, Vetted};
use crate::ui::Values;
use crate::{Ctx, now, ui};

pub fn invoker(i: &Interaction) -> Option<&User> {
    i.member.as_ref().and_then(|m| m.user.as_ref()).or(i.user.as_ref())
}

/// Answer an interaction that came without anyone (see [`invoker`]).
pub async fn nobody(ctx: &Ctx, i: &Interaction) {
    ctx.respond(i, &ui::ephemeral("Discord didn't say who pressed this; try again.")).await
}

/// Positions taking applications now (open, and any window not closed yet):
/// (role key, label, also grants the admin role).
pub fn positions(ctx: &Ctx) -> Vec<(String, String, bool)> {
    let cfg = ctx.cfg();
    cfg.bot
        .positions
        .iter()
        .filter(|p| p.taking(&cfg.bot.timezone, now()))
        .map(|p| (p.role.clone(), p.label.clone().unwrap_or_else(|| ctx.role_name(&p.role)), p.admin))
        .collect()
}

/// A modal field showing a long question in full (labels are capped at 45 characters).
#[allow(deprecated)] // TextInput::label must be None inside a Label
fn question(id: &str, n: usize, text: &str) -> Component {
    Component::Label(Label {
        component: Box::new(Component::TextInput(TextInput {
            custom_id: id.to_owned(),
            id: None,
            label: None,
            max_length: Some(1000),
            min_length: None,
            placeholder: None,
            required: Some(true),
            style: TextInputStyle::Paragraph,
            value: None,
        })),
        description: Some(ui::truncate(text, 100)),
        id: None,
        label: ui::truncate(&format!("Question {n}: {text}"), 45),
    })
}

/// What `/volunteer` and its button say while no position is open.
pub const NONE_OPEN: &str = "No positions are open for applications right now. When one opens, `/volunteer` lists it; keep an eye on the announcements.";

const NO_REVIEW: &str = "Applications aren't set up here.";

/// `/volunteer`, or its button: the form for `role`, or a menu to pick one.
pub async fn volunteer(ctx: &Ctx, i: &Interaction, role: Option<String>) {
    if ctx.cfg().bot.review.is_none() {
        return ctx.respond(i, &ui::ephemeral(NO_REVIEW)).await;
    }
    let positions = positions(ctx);
    if positions.is_empty() {
        return ctx.respond(i, &ui::ephemeral(NONE_OPEN)).await;
    }
    // With one open, there's nothing to choose.
    let only = (positions.len() == 1).then(|| positions[0].0.clone());
    let Some(role) = role.or(only) else {
        let options: Vec<(String, String)> = positions.iter().map(|(k, l, _)| (l.clone(), k.clone())).collect();
        let menu = ui::menu(&CustomId::PickPosition.to_string(), "Choose a position", &options);
        return ctx.respond(i, &ui::ephemeral_with("Which position would you like to apply for?", vec![menu])).await;
    };
    review::open(ctx, i, Form::Volunteer(role), None).await
}

/// Whether `user` can apply for the position `role` now: it's a position,
/// it takes applications, and they neither hold it nor have applied already.
pub async fn volunteer_ready(ctx: &Ctx, role: &str, user: &User) -> Result<(), String> {
    let cfg = ctx.cfg();
    if cfg.bot.review.is_none() {
        return Err(NO_REVIEW.into());
    }
    let p = cfg.bot.positions.iter().find(|p| p.role == role && p.open);
    let Some(p) = p else { return Err("That position isn't open for applications.".into()) };
    if !p.taking(&cfg.bot.timezone, now()) {
        let label = chipsoc_admin::rounds::label(&cfg, role);
        let day = p.closes_label().unwrap_or_default();
        return Err(format!("Applications for **{label}** closed on {day}. You'll hear if it opens again."));
    }
    match chipsoc_admin::already(ctx, AppKind::Position, role, Snowflake(user.id.get())).await {
        Some(why) => Err(why),
        None => Ok(()),
    }
}

/// The position form: "Apply: {label}", the intro if any, then its questions.
pub fn volunteer_form(ctx: &Ctx, role: &str) -> Option<(String, Vec<Component>)> {
    let cfg = ctx.cfg();
    let p = cfg.bot.positions.iter().find(|p| p.role == role)?;
    let mut fields: Vec<Component> = p.intro.iter().map(|t| ui::text(t)).collect();
    fields.extend(asked(p).enumerate().map(|(n, q)| ask(&format!("q{n}"), q)));
    Some((format!("Apply: {}", chipsoc_admin::rounds::label(&cfg, role)), fields))
}

/// The questions a position's form has room for (the intro takes a box).
fn asked(p: &Position) -> impl Iterator<Item = &Question> {
    p.questions.iter().take(MAX_FORM_FIELDS - usize::from(p.intro.is_some()))
}

/// A form box for a configured question; long questions show in full under the label.
pub(crate) fn ask(id: &str, q: &Question) -> Component {
    let style = if q.long { TextInputStyle::Paragraph } else { TextInputStyle::Short };
    let field = ui::input(id, &q.ask, style, !q.optional, q.hint.as_deref());
    if q.ask.chars().count() > ui::MAX_LABEL { ui::described(field, &q.ask) } else { field }
}

/// A position's form, sent: its answers, on a card of their own, or in a
/// window, waiting for the card that lists everyone who applied.
pub fn vet_volunteer(ctx: &Ctx, role: &str, _user: &User, v: &Values) -> Result<Vetted, Refusal> {
    let cfg = ctx.cfg();
    let Some(p) = cfg.bot.positions.iter().find(|p| p.role == role) else {
        return Err(Refusal::last("That position isn't open for applications any more."));
    };
    let label = chipsoc_admin::rounds::label(&cfg, role);
    let answers: Vec<Answer> =
        asked(p).enumerate().map(|(n, q)| Answer::new(&q.ask, Some(&v.text(&format!("q{n}"))))).collect();
    let mut f = Filing::new(
        AppKind::Position,
        role,
        format!("{} · {label}", AppKind::Position.label()),
        answers,
        format!("Thanks! Your application for **{label}** is in. You'll get a DM once it's reviewed."),
    );
    f.who = true;
    if chipsoc_admin::rounds::in_window(&cfg, role, now()) {
        f.round = Round::Waiting;
        let day = p.closes_label().unwrap_or_default();
        f.thanks = format!(
            "Thanks! Your application for **{label}** is in. Applications close on {day}; they're all looked at then, and you'll get a DM once yours is reviewed."
        );
    }
    Ok(Vetted::Review(f))
}

/// Whether a project takes applications now: they're open, it's active, and
/// there's a review queue for them.
pub fn taking(cfg: &chipsoc_config::Config, p: &chipsoc_config::Project) -> bool {
    p.application.open && p.is_live() && cfg.bot.review.is_some()
}

/// How the lead question is asked (the same words as on the idea form; see
/// [`chipsoc_admin::Lead`]).
const LEAD_QUESTION: &str = chipsoc_admin::forums::IDEA_LEAD;
const JUST_JOIN: &str = "Just join";

/// How many of a project's own questions its form has room for: all but
/// the "why" box and the lead question.
fn room(p: &chipsoc_config::Project) -> usize {
    MAX_FORM_FIELDS - 1 - usize::from(p.application.lead_question)
}

/// `/apply`, or an `apply:` button.
pub async fn apply(ctx: &Ctx, i: &Interaction, slug: Option<String>) {
    match slug {
        Some(slug) => review::open(ctx, i, Form::Apply(slug), None).await,
        None => ctx.respond(i, &ui::ephemeral("That project isn't taking applications right now.")).await,
    }
}

/// Whether `user` can apply to the project `slug` now: it takes
/// applications, and they're neither on its team nor waiting already.
pub async fn apply_ready(ctx: &Ctx, slug: &str, user: &User) -> Result<(), String> {
    let cfg = ctx.cfg();
    if !cfg.projects.iter().any(|p| p.slug == slug && taking(&cfg, p)) {
        return Err("That project isn't taking applications right now.".into());
    }
    match chipsoc_admin::already(ctx, AppKind::Project, slug, Snowflake(user.id.get())).await {
        Some(why) => Err(why),
        None => Ok(()),
    }
}

/// The project form: why, whether they'd lead it, and the project's own questions.
pub fn apply_form(ctx: &Ctx, slug: &str) -> Option<(String, Vec<Component>)> {
    let cfg = ctx.cfg();
    let p = cfg.projects.iter().find(|p| p.slug == slug)?;
    let mut fields = vec![ui::input("why", "Why this project?", TextInputStyle::Paragraph, true, None)];
    if p.application.lead_question {
        let options =
            [(JUST_JOIN.to_owned(), "no".to_owned()), (chipsoc_admin::Lead::Lead.label().to_owned(), "yes".to_owned())];
        fields.push(ui::choice("lead", LEAD_QUESTION, &options));
    }
    for (n, q) in p.application.questions.iter().take(room(p)).enumerate() {
        fields.push(question(&format!("q{n}"), n + 1, q));
    }
    Some((format!("Apply: {}", p.name), fields))
}

/// A project team's form, sent.
pub fn vet_apply(ctx: &Ctx, slug: &str, _user: &User, v: &Values) -> Result<Vetted, Refusal> {
    let cfg = ctx.cfg();
    let Some(p) = cfg.projects.iter().find(|p| p.slug == slug) else {
        return Err(Refusal::last("That project isn't taking applications any more."));
    };
    let as_lead = v.pick("lead").is_some_and(|x| x == "yes");
    let mut answers = vec![Answer::new("Why", Some(&v.text("why")))];
    for (n, q) in p.application.questions.iter().take(room(p)).enumerate() {
        answers.push(Answer::new(q, Some(&v.text(&format!("q{n}")))));
    }
    let thanks = format!("Thanks! Your application to **{}** is in. You'll get a DM once it's reviewed.", p.name);
    let mut f =
        Filing::new(AppKind::Project, slug, format!("{} · {}", AppKind::Project.label(), p.display()), answers, thanks);
    f.lead = as_lead;
    f.extra = as_lead.then(|| chipsoc_admin::LEAD_WISH.to_owned());
    f.who = true;
    Ok(Vetted::Review(f))
}

/// A card in the review channel with Accept and Reject. `who`: a line about
/// the applicant (their join answers), shown under their name.
pub async fn post_review(
    ctx: &Ctx,
    id: i64,
    title: &str,
    user: &User,
    who: Option<&str>,
    answers: &[Answer],
    extra: Option<&str>,
) {
    let Some(channel) = ctx.cfg().bot.review.as_ref().and_then(|r| ctx.channel(&r.channel)) else {
        tracing::warn!("no review channel set up; application {id} kept in the database only");
        return unposted(ctx, id, "there's no review channel ([review] in bot.toml)").await;
    };
    let mut body = card_head(&format!("#{id} · {title}"), user.id.get(), who);
    for a in answers.iter().filter(|a| !a.answer.is_empty()) {
        // Answers are the applicant's words: quoted and escaped, never markdown.
        body.push_str(&format!("\n**{}**\n{}\n", a.question, ui::quoted(&ui::truncate(&a.answer, 400))));
    }
    if let Some(x) = extra {
        body.push_str(&format!("\n{x}"));
    }
    let buttons = [ui::row(vec![
        ui::button(&CustomId::Review { accept: true, id }.to_string(), "Accept", ButtonStyle::Success),
        ui::button(&CustomId::Review { accept: false, id }.to_string(), "Reject", ButtonStyle::Danger),
    ])];
    match ctx.post(channel, &body, &buttons).await {
        Ok(m) => {
            let saved = ctx.db().set_card(id, &channel.to_string(), &chipsoc_discord::wire::s(&m["id"])).await;
            ctx.log_err(saved, "remembering the review card");
        }
        Err(e) => {
            tracing::warn!("posting review card failed: {e:#}");
            unposted(ctx, id, &chipsoc_admin::fmt::error(&e)).await;
        }
    }
}

/// The bot log hears of a submission nobody would otherwise see.
async fn unposted(ctx: &Ctx, id: i64, why: &str) {
    ctx.log_bot(&format!(
        "{} **A new submission (#{id}) couldn't be posted for review:** {why}. It's in `/server queue list`.",
        chipsoc_admin::fmt::ATTENTION
    ))
    .await;
}

/// The top of a review card: what it is, who sent it, and (escaped: they
/// wrote it) the line about them.
fn card_head(title: &str, user: u64, who: Option<&str>) -> String {
    match who.map(str::trim).filter(|w| !w.is_empty()) {
        Some(w) => format!("**{title}**\nfrom <@{user}> · {}\n", ui::escape(&ui::truncate(w, 300))),
        None => format!("**{title}**\nfrom <@{user}>\n"),
    }
}

/// Who pressed the button, as the shared admin code knows people.
pub(crate) fn actor(user: &User) -> Actor {
    Actor { id: Some(Snowflake(user.id.get())), name: user.name.clone() }
}

/// Accept or Reject on a review card.
/// Accept or Reject on a review card; `seen`: the version of the entry the
/// card showed (partners' entries), so a changed one isn't accepted unseen.
pub async fn review(ctx: &Ctx, i: &Interaction, accept: bool, id: i64, seen: Option<&str>) {
    if !ctx.is_admin(i.member.as_ref()) {
        return ctx.respond(i, &ui::ephemeral(&crate::manage::admins_only(&ctx.cfg()))).await;
    }
    let Some(reviewer) = invoker(i).map(actor) else { return nobody(ctx, i).await };
    let app = match chipsoc_admin::claim_seen(ctx, id, accept, seen, &reviewer).await {
        Ok(app) => app,
        Err(why) => return ctx.respond(i, &ui::ephemeral(&why.markdown())).await,
    };
    ctx.respond(i, &deferred(&app)).await;
    let decided = chipsoc_admin::settle(ctx, &app, accept, &reviewer).await;
    if decided.pending && app.kind.is_partner() {
        // The card was drawn again as the entry is now (it may have changed).
        let note = decided.note(&reviewer);
        return chipsoc_admin::shares::redraw(ctx, app.id, &note).await;
    }
    show_decision(ctx, i, &app, &decided, &reviewer).await;
}

/// How to answer a decision on `app`'s card while it's made: the card itself
/// is edited, but a card listing a window's round is drawn again, and the
/// reviewer told how it went.
pub fn deferred(app: &chipsoc_store::Application) -> twilight_model::http::interaction::InteractionResponse {
    if chipsoc_admin::rounds::on_round_card(app) { ui::defer() } else { ui::defer_update() }
}

/// A decision, shown on the card (see [`deferred`]).
pub async fn show_decision(
    ctx: &Ctx,
    i: &Interaction,
    app: &chipsoc_store::Application,
    decided: &chipsoc_admin::Decided,
    by: &Actor,
) {
    let note = decided.note(by);
    if chipsoc_admin::rounds::on_round_card(app) {
        if let Some((ch, msg)) = &app.card {
            ctx.log_err(chipsoc_admin::rounds::redraw(ctx, ch, msg).await, "drawing a round's card again");
        }
        return ctx.edit_response(i, note.trim_start(), &[]).await;
    }
    let original = i.message.as_ref().map(|m| m.content.clone()).unwrap_or_default();
    let buttons = if decided.pending {
        // It didn't go through: keep the card's buttons to try again.
        i.message.as_ref().map(|m| m.components.clone()).unwrap_or_default()
    } else if decided.lead {
        vec![ui::row(vec![ui::button(&CustomId::MakeLead(app.id).to_string(), "Make lead", ButtonStyle::Primary)])]
    } else {
        Vec::new()
    };
    ctx.edit_response(i, &chipsoc_admin::fmt::with_note(&original, &note), &buttons).await;
}

/// "Reject the rest…" on a round's card: asks for one reason for them all.
pub async fn reject_rest_form(ctx: &Ctx, i: &Interaction, role: &str) {
    if !ctx.is_admin(i.member.as_ref()) {
        return ctx.respond(i, &ui::ephemeral(&crate::manage::admins_only(&ctx.cfg()))).await;
    }
    let reason = ui::input("reason", "Reason (optional, sent to them)", TextInputStyle::Paragraph, false, None);
    let form = vec![ui::described(reason, "Sent to everyone still waiting, and kept with each application.")];
    let label = chipsoc_admin::rounds::label(&ctx.cfg(), role);
    let title = format!("Reject the rest: {label}");
    ctx.respond(i, &ui::modal(&CustomId::RestForm(role.to_owned()).to_string(), &title, form)).await;
}

/// The reason sent: everyone still waiting for `role` is rejected.
pub async fn reject_rest(ctx: &Ctx, i: &Interaction, role: &str, reason: &str) {
    let Some(by) = invoker(i).filter(|_| ctx.is_admin(i.member.as_ref())).map(actor) else {
        return ctx.respond(i, &ui::ephemeral(&crate::manage::admins_only(&ctx.cfg()))).await;
    };
    ctx.respond(i, &ui::defer()).await;
    let why = Some(reason).filter(|r| !r.trim().is_empty());
    let text = match chipsoc_admin::rounds::reject_rest(ctx, role, why, &by).await {
        Ok(0) => "Nobody was still waiting.".to_owned(),
        Ok(n) => format!(
            "❌ Rejected {} for **{}**; each was told{}.",
            chipsoc_admin::fmt::count(n, "applicant", "applicants"),
            chipsoc_admin::rounds::label(&ctx.cfg(), role),
            if why.is_some() { " why" } else { "" }
        ),
        Err(why) => why.markdown(),
    };
    ctx.edit_response(i, &text, &[]).await;
}

/// "Make lead" on an accepted project application.
pub async fn make_lead(ctx: &Ctx, i: &Interaction, id: i64) {
    let Some(admin) = invoker(i).filter(|_| ctx.is_admin(i.member.as_ref())).map(actor) else {
        return ctx.respond(i, &ui::ephemeral(&crate::manage::admins_only(&ctx.cfg()))).await;
    };
    let (app, key) = match chipsoc_admin::claim_lead(ctx, id).await {
        Ok(found) => found,
        Err(why) => return ctx.respond(i, &ui::ephemeral(&why.markdown())).await,
    };
    ctx.respond(i, &ui::defer_update()).await;
    let text = match chipsoc_admin::lead(ctx, &app, &key, &admin).await {
        Ok(text) => text,
        // The card keeps its Make lead button, to try again once fixed.
        Err(why) => return ctx.follow_up(i, &why.markdown()).await,
    };
    let original = i.message.as_ref().map(|m| m.content.clone()).unwrap_or_default();
    ctx.edit_response(i, &chipsoc_admin::fmt::with_note(&original, &chipsoc_admin::made_lead(&admin, &text)), &[])
        .await;
}

/// Watch application windows: once one closes, post the card listing
/// everyone who applied, and offer `/volunteer` only what's still open.
pub async fn windows_loop(ctx: std::sync::Arc<Ctx>) {
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(60));
    let mut taking: Option<Vec<String>> = None;
    loop {
        tick.tick().await;
        if let Err(e) = chipsoc_admin::rounds::post_closed(&*ctx).await {
            tracing::warn!("posting a closed window's applicants failed: {e:#}");
        }
        let now_taking: Vec<String> = positions(&ctx).into_iter().map(|p| p.0).collect();
        if taking.as_ref().is_some_and(|t| *t != now_taking) {
            crate::commands::register(&ctx).await;
        }
        taking = Some(now_taking);
    }
}

#[cfg(test)]
mod tests {
    use super::card_head;

    #[test]
    fn the_card_says_who_they_are_in_their_own_words_escaped() {
        let head = card_head("Position · Treasurer", 7, Some("Manchester · EEE · Year 3 · [x](https://evil.example)"));
        assert_eq!(
            head,
            "**Position · Treasurer**\nfrom <@7> · Manchester · EEE · Year 3 · \\[x\\]\\(https://evil.example)\n"
        );
        assert_eq!(card_head("Idea · X", 7, Some("  ")), "**Idea · X**\nfrom <@7>\n");
    }
}

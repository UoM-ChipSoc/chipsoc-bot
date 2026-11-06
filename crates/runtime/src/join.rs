//! The join flow in Discord (`join.toml`): one question at a time in a reply
//! only they can see, with menus, Back, forms for text, and a summary to
//! confirm. Answers in progress are kept in the store for a day, so a
//! restart doesn't lose them. On
//! confirm the answers' roles are given (and the flow's old ones taken), the
//! nickname is set as `/profile` does, and the answers are kept so pressing
//! the button again starts from them. With the gate on, new members who
//! haven't finished get one reminder and the bot log hears who's waiting;
//! members already here hear about it only through `/server join notify`.

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;
use std::time::Duration;

use chipsoc_admin::join as effects;
use chipsoc_config::Snowflake;
use chipsoc_config::join::{self as flow, Answer, Answers, Field, Join, Step};
use chipsoc_store::Profile;
use twilight_model::application::interaction::Interaction;
use twilight_model::channel::message::EmojiReactionType;
use twilight_model::channel::message::component::{
    ActionRow, Button, ButtonStyle, Component, SelectMenu, SelectMenuOption, SelectMenuType, TextInputStyle,
};
use twilight_model::http::interaction::{InteractionResponse, InteractionResponseData, InteractionResponseType};
use twilight_model::id::Id;
use twilight_model::id::marker::UserMarker;

use crate::apps::invoker;
use crate::commands::CustomId;
use crate::limits::Kind;
use crate::{Ctx, members, now, profile, ui};

/// How long answers in progress are kept (seconds).
const DRAFT_LIFE: i64 = 24 * 3600;
/// How often reminders and the yearly refresh are looked at.
const EVERY: Duration = Duration::from_secs(10 * 60);
/// Between DMs, so the bot doesn't look like spam.
const PACE: Duration = Duration::from_secs(2);
const DMS_PER_ROUND: usize = 30;
/// Store notes (in `prompted`, user `*`): when the gate and the yearly refresh were first seen.
const GATE_SEEN: &str = "join:gate";
const YEARLY_SEEN: &str = "join:yearly";
const GRANDFATHERED: &str = "join:grandfather";

/// Answers in progress, until confirmed: kept in the store (see [`draft`]).
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
struct Draft {
    answers: Answers,
    /// The step shown; `None` is the summary.
    at: Option<String>,
    /// Where Back goes.
    back: Vec<Option<String>>,
    /// Changing one answer from the summary: once every answer on the path
    /// is in again, Next goes straight back to the summary.
    #[serde(default)]
    review: bool,
}

/// Someone's answers in progress, if they have any from the last [`DRAFT_LIFE`].
async fn draft(ctx: &Ctx, user: u64) -> Option<Draft> {
    match ctx.db().join_draft(&user.to_string(), now() - DRAFT_LIFE).await {
        Ok(d) => d.and_then(|d| serde_json::from_str(&d).ok()),
        Err(e) => {
            tracing::warn!(?e, "reading a join draft failed");
            None
        }
    }
}

async fn keep_draft(ctx: &Ctx, user: u64, d: &Draft) {
    let json = serde_json::to_string(d).unwrap_or_default();
    let saved = ctx.db().set_join_draft(&user.to_string(), &json, now(), now() - DRAFT_LIFE).await;
    ctx.log_err(saved, "saving a join draft");
}

/// A component id for the flow; in DMs it carries the server.
fn cid(ctx: &Ctx, i: &Interaction, action: &str) -> String {
    CustomId::Join(action.to_owned()).for_reply(ctx, i)
}

fn button(id: String, label: &str, style: ButtonStyle, disabled: bool) -> Component {
    Component::Button(Button {
        custom_id: Some(id),
        disabled,
        emoji: None,
        id: None,
        label: Some(label.to_owned()),
        sku_id: None,
        style,
        url: None,
    })
}

/// Replace the message the flow is on.
fn update(text: &str, components: Vec<Component>) -> InteractionResponse {
    InteractionResponse {
        kind: InteractionResponseType::UpdateMessage,
        data: Some(InteractionResponseData {
            content: Some(ui::truncate(text, ui::MAX_CONTENT - 1)),
            components: Some(components),
            ..Default::default()
        }),
    }
}

const TIMED_OUT: &str =
    "This was left unfinished for over a day. Press **Get started** (or use `/profile`) to go through it again.";

/// Start (or restart) the flow, from `from` if it's on their path. In
/// `place`, it replaces the message the button is on (the yearly DM, whose
/// buttons then go) rather than coming as a new reply.
pub async fn start(ctx: &Ctx, i: &Interaction, from: Option<&str>) {
    start_in(ctx, i, from, false).await;
}

async fn start_in(ctx: &Ctx, i: &Interaction, from: Option<&str>, place: bool) {
    let Some(user) = invoker(i) else { return };
    let Some(j) = ctx.cfg().join.clone() else {
        let gone = "That isn't set up here any more.";
        return ctx.respond(i, &if place { ui::update(gone) } else { ui::ephemeral(gone) }).await;
    };
    let (answers, finished) = saved(ctx, user.id).await;
    let walk = j.walk(&answers);
    let d = match from.and_then(|f| walk.done.iter().position(|d| d == f)) {
        Some(n) => jump(&walk.done, n, answers),
        // Been through it before: their answers, to change one or confirm.
        None if finished && walk.at.is_none() => {
            Draft { back: walk.done.iter().cloned().map(Some).collect(), answers, at: None, review: true }
        }
        None => Draft { answers, at: j.first().map(str::to_owned), back: Vec::new(), review: false },
    };
    let (text, rows) = render(ctx, i, &j, &d, None);
    keep_draft(ctx, user.id.get(), &d).await;
    ctx.respond(i, &if place { update(&text, rows) } else { ui::ephemeral_with(&text, rows) }).await;
}

/// The draft for changing the `n`th answer on the path `done`.
fn jump(done: &[String], n: usize, answers: Answers) -> Draft {
    Draft { answers, at: Some(done[n].clone()), back: done[..n].iter().cloned().map(Some).collect(), review: true }
}

/// What they answered last time (and whether they finished); else what the
/// roles they hold say.
async fn saved(ctx: &Ctx, user: Id<UserMarker>) -> (Answers, bool) {
    match ctx.db().join_record(&user.to_string()).await {
        Ok(Some(r)) => return (Answers::parse(&r.answers), r.finished_at.is_some()),
        Ok(None) => {}
        Err(e) => tracing::warn!(?e, "reading join answers failed"),
    }
    let Some(j) = ctx.cfg().join.clone() else { return Default::default() };
    match effects::held(ctx, Snowflake(user.get())).await {
        Ok(held) => (j.guess(&held), false),
        Err(e) => {
            tracing::warn!("reading roles failed; the flow starts empty: {e:#}");
            Default::default()
        }
    }
}

fn render(ctx: &Ctx, i: &Interaction, j: &Join, d: &Draft, note: Option<&str>) -> (String, Vec<Component>) {
    let back = button(cid(ctx, i, "back"), "Back", ButtonStyle::Secondary, d.back.is_empty());
    let mut text = String::new();
    let Some(step) = d.at.as_deref().and_then(|k| j.step(k)) else {
        return summary(ctx, i, j, d, note, back);
    };
    if d.back.is_empty()
        && let Some(intro) = &j.intro
    {
        text.push_str(&format!("{intro}\n\n"));
    }
    text.push_str(&format!("**{}**", step.question));
    if let Some(h) = &step.help {
        text.push_str(&format!("\n{h}"));
    }
    let empty = Answer::default();
    let a = d.answers.0.get(&step.key).unwrap_or(&empty);
    for f in step.boxes(a) {
        if let Some(v) = a.fields.get(&f.key) {
            text.push_str(&format!("\n{}: **{v}**", f.label));
        }
    }
    if let Some(n) = note {
        text.push_str(&format!("\n\n⚠️ {n}"));
    }
    let mut rows = Vec::new();
    if !step.is_text() {
        rows.push(menu(cid(ctx, i, &format!("pick:{}", step.key)), step, &a.options));
    }
    let mut buttons = vec![back];
    if !step.boxes(a).is_empty() {
        let label = if step.is_text() { "Fill in" } else { "Tell us more" };
        buttons.push(button(cid(ctx, i, &format!("fill:{}", step.key)), label, ButtonStyle::Primary, false));
    }
    if step.accepts(a) {
        buttons.push(button(cid(ctx, i, "next"), "Next", ButtonStyle::Success, false));
    }
    rows.push(ui::row(buttons));
    (text, rows)
}

fn menu(id: String, step: &Step, picked: &[String]) -> Component {
    let options: Vec<SelectMenuOption> = step
        .options
        .iter()
        .take(25)
        .map(|o| SelectMenuOption {
            default: picked.contains(&o.key()),
            description: o.description.as_ref().map(|d| ui::truncate(d, 99)),
            emoji: o.emoji.clone().map(|name| EmojiReactionType::Unicode { name }),
            label: ui::truncate(&o.label, 99),
            value: o.key(),
        })
        .collect();
    let n = options.len() as u8;
    Component::ActionRow(ActionRow {
        id: None,
        components: vec![Component::SelectMenu(SelectMenu {
            channel_types: None,
            custom_id: id,
            default_values: None,
            disabled: false,
            id: None,
            kind: SelectMenuType::Text,
            max_values: Some(if step.multi { n } else { 1 }),
            min_values: Some(u8::from(!(step.multi && step.optional))),
            options: Some(options),
            placeholder: Some(if step.multi { "Pick any that fit" } else { "Pick one" }.to_owned()),
            required: None,
        })],
    })
}

/// Each answer on the way, as a line.
fn lines(j: &Join, answers: &Answers) -> Vec<String> {
    let line = |(q, said): (String, String)| format!("• {q} **{}**", if said.is_empty() { "—" } else { &said });
    j.said(answers).into_iter().map(line).collect()
}

fn summary(
    ctx: &Ctx,
    i: &Interaction,
    j: &Join,
    d: &Draft,
    note: Option<&str>,
    back: Component,
) -> (String, Vec<Component>) {
    let Some(out) = j.outcome(&d.answers) else {
        return ("Something changed; please start again.".into(), vec![ui::row(vec![back])]);
    };
    let mut tail = Vec::new();
    if let Some(you_get) = you_get(j, &out.give.iter().map(|k| ctx.role_name(k)).collect::<Vec<_>>()) {
        tail.push(format!("\n{you_get}"));
    }
    if let Some(n) = note {
        tail.push(format!("\n⚠️ {n}"));
    }
    let mut body = lines(j, &d.answers);
    body.extend(tail);
    let head = ui::fit("**Check your answers**, change one, or Confirm.", &body);
    let done = j.walk(&d.answers).done;
    let mut rows = Vec::new();
    if !done.is_empty() {
        rows.push(change_menu(cid(ctx, i, "jump"), j, &done));
    }
    let confirm = button(cid(ctx, i, "ok"), "Confirm", ButtonStyle::Success, false);
    rows.push(ui::row(vec![back, confirm]));
    (head, rows)
}

/// "You'll get" on the summary: the roles their answers give. The access
/// role is left out (its name means nothing to them); with the gate on, the
/// rest of the server is what it gives.
fn you_get(j: &Join, roles: &[String]) -> Option<String> {
    let mut roles = roles.to_vec();
    roles.dedup();
    let opens = j.gate && j.access_role.is_some();
    match (roles.is_empty(), opens) {
        (true, false) => None,
        (true, true) => Some("Then the rest of the server opens.".into()),
        (false, false) => Some(format!("You'll get: {}.", roles.join(", "))),
        (false, true) => Some(format!("You'll get: {}, and the rest of the server.", roles.join(", "))),
    }
}

/// The summary's "Change an answer…" menu: one entry per question on their path.
fn change_menu(id: String, j: &Join, done: &[String]) -> Component {
    let options = done
        .iter()
        .filter_map(|k| j.step(k))
        .take(25)
        .map(|s| SelectMenuOption {
            default: false,
            description: None,
            emoji: None,
            label: ui::truncate(&s.question, 99),
            value: s.key.clone(),
        })
        .collect();
    Component::ActionRow(ActionRow {
        id: None,
        components: vec![Component::SelectMenu(SelectMenu {
            channel_types: None,
            custom_id: id,
            default_values: None,
            disabled: false,
            id: None,
            kind: SelectMenuType::Text,
            max_values: Some(1),
            min_values: Some(1),
            options: Some(options),
            placeholder: Some("Change an answer…".to_owned()),
            required: None,
        })],
    })
}

/// A menu pick or button in the flow (and the admin's notify buttons).
pub async fn component(ctx: &Ctx, i: &Interaction, action: &str, values: &[String]) {
    let (verb, arg) = action.split_once(':').unwrap_or((action, ""));
    match verb {
        "notify" => return notify_button(ctx, i, arg == "send").await,
        // The yearly DM's Update: the flow takes the DM's place, so its buttons go.
        "from" => return start_in(ctx, i, Some(arg), i.guild_id.is_none() && i.message.is_some()).await,
        "keep" => return keep(ctx, i).await,
        _ => {}
    }
    let Some(user) = invoker(i).map(|u| u.id.get()) else { return };
    let Some(j) = ctx.cfg().join.clone() else {
        return ctx.respond(i, &ui::update("That isn't set up here any more.")).await;
    };
    let Some(mut d) = draft(ctx, user).await else { return ctx.respond(i, &ui::update(TIMED_OUT)).await };
    let mut note = None;
    match verb {
        "pick" if d.at.as_deref() == Some(arg) => {
            let Some(step) = j.step(arg) else { return ctx.respond(i, &ui::update(TIMED_OUT)).await };
            let a = d.answers.0.entry(arg.to_owned()).or_default();
            a.options = values.iter().filter(|v| step.option(v).is_some()).cloned().collect();
            // Straight on, unless there's more to ask about the pick.
            if !step.multi && step.accepts(a) && step.boxes(a).is_empty() {
                advance(&j, &mut d);
            }
        }
        "next" => match d.at.as_deref().and_then(|k| j.step(k)) {
            Some(step) if step.accepts(d.answers.0.get(&step.key).unwrap_or(&Answer::default())) => advance(&j, &mut d),
            _ => note = Some("Answer this one first."),
        },
        "back" => {
            if let Some(prev) = d.back.pop() {
                d.at = prev;
            }
        }
        "jump" if d.at.is_none() => {
            let done = j.walk(&d.answers).done;
            match values.first().and_then(|v| done.iter().position(|k| k == v)) {
                Some(n) => d = jump(&done, n, d.answers),
                None => note = Some("That question isn't on your path any more."),
            }
        }
        "fill" if d.at.as_deref() == Some(arg) => return form(ctx, i, &j, &d, arg).await,
        "ok" if d.at.is_none() => return confirm(ctx, i, &j, d).await,
        // A button from an older message.
        _ => note = Some("That button was from an earlier step; here's where you are."),
    }
    let (text, rows) = render(ctx, i, &j, &d, note);
    keep_draft(ctx, user, &d).await;
    ctx.respond(i, &update(&text, rows)).await;
}

fn advance(j: &Join, d: &mut Draft) {
    let Some(step) = d.at.as_deref().and_then(|k| j.step(k)) else { return };
    let a = d.answers.0.get(&step.key).cloned().unwrap_or_default();
    let next = j.next_after(step, &a).map(str::to_owned);
    let walk = j.walk(&d.answers);
    if d.review && walk.at.is_none() {
        // Changing one answer: the rest still stand, so back to the summary.
        d.back = walk.done.into_iter().map(Some).collect();
        d.at = None;
        return;
    }
    d.back.push(d.at.take());
    d.at = next;
}

/// The form for a step's text boxes, filled in with what they gave before.
async fn form(ctx: &Ctx, i: &Interaction, j: &Join, d: &Draft, key: &str) {
    let Some(step) = j.step(key) else { return };
    let empty = Answer::default();
    let a = d.answers.0.get(key).unwrap_or(&empty);
    let saved = match invoker(i) {
        Some(u) => ctx.db().profile(&u.id.to_string()).await.ok().flatten().unwrap_or_default(),
        None => Profile::default(),
    };
    let fields: Vec<Component> = step
        .boxes(a)
        .into_iter()
        .map(|f| {
            let known = match f.key.as_str() {
                flow::NAME => Some(&saved.name),
                flow::SURNAME => Some(&saved.surname),
                flow::COMPANY => Some(&saved.company),
                _ => None,
            };
            let value = a.fields.get(&f.key).or(known.filter(|v| !v.is_empty()));
            text_box(f, value)
        })
        .collect();
    if fields.is_empty() {
        let (text, rows) = render(ctx, i, j, d, None);
        return ctx.respond(i, &update(&text, rows)).await;
    }
    let title = if step.is_text() { step.question.clone() } else { "A bit more".to_owned() };
    ctx.respond(i, &ui::modal(&cid(ctx, i, &format!("form:{key}")), &title, fields)).await;
}

fn text_box(f: &Field, value: Option<&String>) -> Component {
    let style = if f.long { TextInputStyle::Paragraph } else { TextInputStyle::Short };
    let mut c = ui::input(&f.key, &f.label, style, f.required, f.hint.as_deref());
    if let Some(h) = &f.help {
        c = ui::described(c, h);
    }
    if let Component::Label(l) = &mut c
        && let Component::TextInput(t) = l.component.as_mut()
    {
        t.value = value.cloned();
    }
    c
}

/// A step's form sent.
pub async fn modal(ctx: &Ctx, i: &Interaction, action: &str, v: &HashMap<String, String>) {
    let Some(key) = action.strip_prefix("form:") else { return };
    let Some(user) = invoker(i).map(|u| u.id.get()) else { return };
    let Some(j) = ctx.cfg().join.clone() else { return };
    let Some(mut d) = draft(ctx, user).await else { return ctx.respond(i, &ui::update(TIMED_OUT)).await };
    let Some(step) = j.step(key).filter(|_| d.at.as_deref() == Some(key)) else {
        return ctx.respond(i, &ui::update(TIMED_OUT)).await;
    };
    let a = d.answers.0.entry(key.to_owned()).or_default();
    let mut note = None;
    for f in step.boxes(&a.clone()) {
        let text = profile::clean(v.get(&f.key).map_or("", String::as_str));
        if f.key == flow::NAME && !text.chars().any(char::is_alphanumeric) {
            note = Some("Please give a name (letters or digits).");
        }
        if text.is_empty() {
            a.fields.remove(&f.key);
        } else {
            a.fields.insert(f.key.clone(), text);
        }
    }
    if note.is_none() && step.accepts(a) {
        advance(&j, &mut d);
    }
    let (text, rows) = render(ctx, i, &j, &d, note);
    keep_draft(ctx, user, &d).await;
    ctx.respond(i, &update(&text, rows)).await;
}

/// The profile the answers give, on top of what was saved: boxes the flow
/// has but they weren't asked (an old branch) are cleared.
fn profile_from(j: &Join, user: &str, fields: &std::collections::BTreeMap<String, String>, saved: Profile) -> Profile {
    let asks: BTreeSet<&str> = j
        .steps
        .iter()
        .flat_map(|s| s.fields.iter().chain(s.options.iter().flat_map(|o| &o.ask)))
        .map(|f| f.key.as_str())
        .collect();
    let take = |key: &str, old: String| {
        if asks.contains(key) { profile::clean(fields.get(key).map_or("", String::as_str)) } else { old }
    };
    Profile {
        user_id: user.to_owned(),
        name: take(flow::NAME, saved.name),
        surname: take(flow::SURNAME, saved.surname),
        company: take(flow::COMPANY, saved.company),
    }
}

async fn confirm(ctx: &Ctx, i: &Interaction, j: &Join, d: Draft) {
    let Some(user) = invoker(i).map(|u| u.id) else { return };
    // Reading the member list and giving roles cost: past the free checks, the turn counts.
    let turn = match ctx.limits.turn(Kind::Profile, user.get()) {
        Ok(t) => t,
        Err(wait) => return ctx.respond(i, &ui::ephemeral(&wait)).await,
    };
    ctx.respond(i, &ui::defer_update()).await;
    let back = || button(cid(ctx, i, "back"), "Back", ButtonStyle::Secondary, false);
    let Some(out) = j.outcome(&d.answers) else {
        return ctx
            .edit_response(i, "Some answers are missing; please go back through them.", &[ui::row(vec![back()])])
            .await;
    };
    turn.used();
    let saved = ctx.db().profile(&user.to_string()).await.ok().flatten().unwrap_or_default();
    let p = profile_from(j, &user.to_string(), &out.fields, saved);
    let nick = if out.fields.contains_key(flow::NAME) {
        match profile::choose(ctx, user, &p).await {
            Ok(Some(nick)) => Some(nick),
            Ok(None) => {
                let text = format!(
                    "Someone here already goes by **{}**. Go back and add your surname (or more of it) so people can tell you apart.",
                    p.name
                );
                return ctx.edit_response(i, &text, &[ui::row(vec![back()])]).await;
            }
            Err(()) => {
                let again = button(cid(ctx, i, "ok"), "Try again", ButtonStyle::Success, false);
                let text = "Couldn't check your name against others just now; please try again.";
                return ctx.edit_response(i, text, &[ui::row(vec![back(), again])]).await;
            }
        }
    } else {
        None
    };
    let done = match effects::confirm(ctx, Snowflake(user.get()), &d.answers).await {
        Ok(done) => done,
        Err(why) => return ctx.edit_response(i, &why, &[ui::row(vec![back()])]).await,
    };
    ctx.log_err(ctx.db().forget_join_draft(&user.to_string()).await, "forgetting a join draft");
    let mut text = "✅ All done".to_owned();
    let mut yourself = None;
    if let Some(nick) = &nick {
        let renamed = profile::rename(ctx, user, nick).await;
        yourself = profile::set_it_yourself(&renamed, nick);
        if renamed == profile::Renamed::Done {
            text.push_str(&format!(": you're **{nick}** here"));
        }
    }
    if !p.name.is_empty() || !p.company.is_empty() {
        ctx.log_err(ctx.db().set_profile(&p, now()).await, "saving profile");
    }
    text.push('.');
    if let Some(why) = yourself {
        text.push_str(&format!("\n{why}"));
    }
    let access = j.access_role.as_deref().map(|k| ctx.role_name(k));
    let added: Vec<&String> = done.added.iter().filter(|r| Some(*r) != access.as_ref()).collect();
    if !added.is_empty() {
        text.push_str(&format!("\nGave you: {}.", added.iter().map(|r| r.as_str()).collect::<Vec<_>>().join(", ")));
    }
    if !done.removed.is_empty() {
        text.push_str(&format!("\nTook away: {}.", done.removed.join(", ")));
    }
    for problem in &done.problems {
        text.push_str(&format!("\n⚠️ {problem}"));
    }
    let let_in = access.is_some_and(|a| done.added.contains(&a));
    if let Some(next) = done_message(j, let_in) {
        text.push_str(&format!("\n{next}"));
    }
    text.push_str("\nTo change anything later, press the button again or use `/profile`.");
    let detail = format!("+{} -{}", done.added.join(","), done.removed.join(","));
    ctx.audit(Some(&user.to_string()), "join_flow", None, Some(&detail)).await;
    ctx.edit_response(i, &text, &[]).await;
}

/// What to do next, after Confirm: `done_message`, else (when the gate
/// just let them in) that the server is open.
fn done_message(j: &Join, let_in: bool) -> Option<String> {
    j.done_message.clone().or_else(|| (j.gate && let_in).then(|| "The rest of the server is open now.".to_owned()))
}

/// "Still right" on the yearly DM.
async fn keep(ctx: &Ctx, i: &Interaction) {
    let Some(user) = invoker(i).map(|u| u.id.to_string()) else { return };
    let kept = match ctx.db().join_record(&user).await {
        Ok(Some(r)) => ctx.db().set_join_answers(&user, &r.answers, true, now()).await.is_ok(),
        _ => false,
    };
    // Once it's noted, the DM's buttons go; if it isn't, they stay to try again.
    let reply = if kept {
        ui::update("Thanks, noted. 👍")
    } else {
        ui::ephemeral("Sorry, that didn't save; please try again.")
    };
    ctx.respond(i, &reply).await;
}

/// `/server join notify`: how many members would get the DM, with Send.
pub async fn notify_command(ctx: &Ctx, i: &Interaction) {
    if !ctx.is_admin(i.member.as_ref()) {
        return ctx.respond(i, &ui::ephemeral(&crate::manage::admins_only(&ctx.cfg()))).await;
    }
    if ctx.cfg().join.is_none() {
        return ctx.respond(i, &ui::ephemeral("There's no join.toml here.")).await;
    }
    ctx.respond(i, &ui::defer()).await;
    let targets = match effects::notify_targets(ctx).await {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!("listing members to notify failed: {e:#}");
            let text = format!("⚠️ **Couldn't list members:** {}", chipsoc_admin::fmt::error(&e));
            return ctx.edit_response(i, &text, &[]).await;
        }
    };
    if targets.is_empty() {
        return ctx.edit_response(i, "Nobody to tell: everyone has been through the flow or had the DM.", &[]).await;
    }
    let text = effects::notice(ctx);
    let minutes = (targets.len() as u64 * PACE.as_secs()).div_ceil(60);
    let body = format!(
        "This would DM **{}** who {} been through the join flow (once each, ever), one every {} s (about {minutes} min):\n>>> {text}",
        chipsoc_admin::fmt::count(targets.len(), "member", "members"),
        if targets.len() == 1 { "hasn't" } else { "haven't" },
        PACE.as_secs()
    );
    let rows = vec![ui::row(vec![
        button(cid(ctx, i, "notify:send"), &format!("Send to {}", targets.len()), ButtonStyle::Danger, false),
        button(cid(ctx, i, "notify:cancel"), "Cancel", ButtonStyle::Secondary, false),
    ])];
    ctx.edit_response(i, &body, &rows).await;
}

async fn notify_button(ctx: &Ctx, i: &Interaction, send: bool) {
    if !ctx.is_admin(i.member.as_ref()) {
        return ctx.respond(i, &ui::ephemeral(&crate::manage::admins_only(&ctx.cfg()))).await;
    }
    if !send {
        return ctx.respond(i, &ui::update("Cancelled; nobody was sent anything.")).await;
    }
    let Some(by) = invoker(i).map(crate::apps::actor) else { return };
    ctx.respond(i, &ui::update("Sending; the bot log hears when it's done.")).await;
    if let Err(e) = effects::send_notice(ctx, &by, PACE).await {
        tracing::warn!("sending the join flow notice failed: {e:#}");
        ctx.log_bot(&format!(
            "⚠️ **The join flow notice from {} wasn't sent:** {}",
            by.mention(),
            chipsoc_admin::fmt::error(&e)
        ))
        .await;
    }
}

/// Every few minutes: with the gate on, remind new members who haven't
/// finished and tell the bot log who's waiting; on the yearly date, ask about
/// `yearly` steps.
pub async fn watch(ctx: Arc<Ctx>) {
    let mut tick = tokio::time::interval(EVERY);
    loop {
        tick.tick().await;
        let j = ctx.cfg().join.clone().unwrap_or_default();
        let mut budget = DMS_PER_ROUND;
        // Turned off, it counts afresh from when it's next turned on.
        if j.gate {
            gated(&ctx, &j, &mut budget).await;
        } else {
            ctx.log_err(ctx.db().forget_prompt("*", GATE_SEEN).await, "noting the join gate is off");
        }
        if j.yearly_refresh.is_some() {
            yearly(&ctx, &j, &mut budget).await;
        } else {
            ctx.log_err(ctx.db().forget_prompt("*", YEARLY_SEEN).await, "noting the yearly refresh is off");
        }
    }
}

/// When we first saw `kind` (noting now if never).
async fn first_seen(ctx: &Ctx, kind: &str) -> Option<i64> {
    let db = ctx.db();
    match db.prompted_at("*", kind).await {
        Ok(Some(t)) => Some(t),
        Ok(None) => db.first_prompt("*", kind, now()).await.ok().map(|_| now()),
        Err(e) => {
            tracing::warn!(?e, "reading join state failed");
            None
        }
    }
}

/// Whether `user` has been noted for `kind` already.
async fn done(ctx: &Ctx, user: &str, kind: &str) -> bool {
    ctx.db().prompted_at(user, kind).await.ok().flatten().is_some()
}

async fn first(ctx: &Ctx, user: &str, kind: &str) -> bool {
    ctx.db().first_prompt(user, kind, now()).await.unwrap_or_else(|e| {
        tracing::warn!(?e, "noting a prompt failed");
        false
    })
}

async fn gated(ctx: &Ctx, j: &Join, budget: &mut usize) {
    let Some(since) = first_seen(ctx, GATE_SEEN).await else { return };
    let Some(access) = j.access_role.as_deref().and_then(|k| ctx.role(k)) else { return };
    let members = match ctx.members().await {
        Ok(m) => m,
        Err(e) => return tracing::warn!(?e, "listing members for the join flow failed"),
    };
    let finished: BTreeSet<String> = match ctx.db().join_records().await {
        Ok(r) => r.into_iter().filter(|r| r.finished_at.is_some()).map(|r| r.user_id).collect(),
        Err(e) => return tracing::warn!(?e, "reading join answers failed"),
    };
    let wait = i64::from(j.remind_after_hours) * 3600;
    let text = j.reminder_message.clone().unwrap_or_else(|| reminder_text(&ctx.name()));
    let mut waiting = Vec::new();
    for m in members.iter().filter(|m| !members::is_bot(m) && !members::roles(m).contains(&access)) {
        let Some(id) = members::user(m) else { continue };
        let user = id.to_string();
        let joined = members::joined(m);
        if joined < since {
            if j.grandfather && !done(ctx, &user, GRANDFATHERED).await {
                let path = format!("guilds/{}/members/{user}/roles/{access}", ctx.guild);
                match ctx.call(chipsoc_discord::Method::Put, &path, None).await {
                    // Noted once given, so a failed grant is tried again next round.
                    Ok(_) => drop(first(ctx, &user, GRANDFATHERED).await),
                    Err(e) => tracing::warn!(?e, %user, "letting in a member who was here before the gate failed"),
                }
            }
            continue;
        }
        if finished.contains(&user) || j.remind_after_hours == 0 || now() - joined < wait {
            continue;
        }
        let mut reminded = ctx.db().prompted_at(&user, effects::REMINDED).await.ok().flatten().is_some();
        if !reminded && *budget > 0 && first(ctx, &user, effects::REMINDED).await {
            *budget -= 1;
            reminded = true;
            if let Err(why) = effects::dm_start(ctx, Snowflake(id), &text).await {
                tracing::info!(%user, %why, "join reminder not sent");
            }
            tokio::time::sleep(PACE).await;
        }
        if reminded && first(ctx, &user, effects::WAITING).await {
            waiting.push(format!("<@{user}>"));
        }
    }
    if !waiting.is_empty() {
        let head = format!(
            "⏳ Joined over {} h ago and haven't finished the join flow (a reminder DM was tried; give them **{}** by hand if they need help):",
            j.remind_after_hours,
            j.access_role.as_deref().map(|k| ctx.role_name(k)).unwrap_or_default()
        );
        ctx.log_bot(&ui::fit(&head, &waiting)).await;
    }
}

fn reminder_text(name: &str) -> String {
    format!(
        "Hi! You joined {name} a while ago but haven't answered the welcome questions yet, so most channels are still hidden. It takes a minute:"
    )
}

async fn yearly(ctx: &Ctx, j: &Join, budget: &mut usize) {
    use chrono::{Datelike, NaiveDate};
    let Some((month, day)) = j.yearly_refresh.as_deref().and_then(flow::month_day) else { return };
    let tz: chrono_tz::Tz = ctx.cfg().bot.timezone.parse().unwrap_or(chrono_tz::UTC);
    let today = chrono::Utc::now().with_timezone(&tz).date_naive();
    let date = NaiveDate::from_ymd_opt(today.year(), month, day).or(NaiveDate::from_ymd_opt(today.year(), month, 28));
    let Some(date) = date.filter(|d| today >= *d) else { return };
    let Some(moment) = date.and_hms_opt(0, 0, 0).and_then(|t| t.and_local_timezone(tz).earliest()) else { return };
    let moment = moment.timestamp();
    // A date already past when this was turned on waits for next year.
    if first_seen(ctx, YEARLY_SEEN).await.is_none_or(|seen| seen > moment) {
        return;
    }
    let records = match ctx.db().join_records().await {
        Ok(r) => r,
        Err(e) => return tracing::warn!(?e, "reading join answers failed"),
    };
    let due: Vec<_> = records.into_iter().filter(|r| r.finished_at.is_some() && r.ts < moment).collect();
    if due.is_empty() {
        return;
    }
    let here: BTreeSet<String> = match ctx.members().await {
        Ok(m) => m.iter().filter_map(members::user).map(|u| u.to_string()).collect(),
        Err(e) => return tracing::warn!(?e, "listing members for the yearly refresh failed"),
    };
    let kind = format!("join:yearly:{}", today.year());
    for r in due.into_iter().filter(|r| here.contains(&r.user_id)) {
        let answers = Answers::parse(&r.answers);
        let done = j.walk(&answers).done;
        let steps: Vec<&Step> = done.iter().filter_map(|k| j.step(k)).filter(|s| s.yearly).collect();
        let Some(first_step) = steps.first() else { continue };
        if *budget == 0 {
            return;
        }
        if !first(ctx, &r.user_id, &kind).await {
            continue;
        }
        *budget -= 1;
        let only: Vec<String> = lines(j, &answers)
            .into_iter()
            .zip(&done)
            .filter(|(_, k)| steps.iter().any(|s| &s.key == *k))
            .map(|(l, _)| l)
            .collect();
        let text = ui::fit(&format!("A new year at {}: is this still right?", ctx.name()), &only);
        let g = ctx.guild;
        let rows = [ui::row(vec![
            button(CustomId::Join("keep".into()).in_dm(g), "Still right", ButtonStyle::Success, false),
            button(CustomId::Join(format!("from:{}", first_step.key)).in_dm(g), "Update", ButtonStyle::Primary, false),
        ])];
        if let Some(user) = r.user_id.parse::<u64>().ok().and_then(Id::<UserMarker>::new_checked)
            && let Err(why) = ctx.dm_with(user, &text, &rows).await
        {
            tracing::info!(%user, %why, "yearly join DM not sent");
        }
        tokio::time::sleep(PACE).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flow() -> Join {
        toml::from_str(
            "[[step]]\nkey = \"work\"\nquestion = \"Working?\"\n\
             [[step.option]]\nlabel = \"Yes\"\nnext = \"name\"\n[[step.option.ask]]\nkey = \"company\"\nlabel = \"Where?\"\nrequired = false\n\
             [[step.option]]\nlabel = \"No\"\nnext = \"name\"\n\
             [[step]]\nkey = \"name\"\nquestion = \"Name?\"\n[[step.field]]\nkey = \"name\"\nlabel = \"Name\"\n\
             [[step.field]]\nkey = \"surname\"\nlabel = \"Surname\"\nrequired = false\n",
        )
        .unwrap()
    }

    #[test]
    fn the_profile_comes_from_the_boxes_and_old_answers_are_cleared() {
        let j = flow();
        let saved = Profile { user_id: "1".into(), name: "Old".into(), surname: "Name".into(), company: "Arm".into() };
        let fields = [("name".to_owned(), "A\u{200B}da ".to_owned())].into();
        let p = profile_from(&j, "1", &fields, saved);
        assert_eq!(p.name, "Ada", "cleaned as /profile does");
        assert_eq!(p.surname, "", "asked and left blank");
        assert_eq!(p.company, "", "back from the job: the company goes");
        assert_eq!(profile::nickname(&p.name, &p.surname, &["Ada".into()], &[]), None, "and named as /profile names");
    }

    #[test]
    fn a_draft_is_kept_as_json() {
        let mut answers = Answers::default();
        answers.0.entry("work".into()).or_default().options.push("yes".into());
        let d = Draft { answers, at: None, back: vec![Some("work".into())], review: true };
        let json = serde_json::to_string(&d).unwrap();
        assert_eq!(serde_json::from_str::<Draft>(&json).unwrap(), d);
    }

    fn answered() -> Answers {
        let mut a = Answers::default();
        a.0.entry("work".into()).or_default().options.push("no".into());
        a.0.entry("name".into()).or_default().fields.insert("name".into(), "Ada".into());
        a
    }

    #[test]
    fn changing_one_answer_from_the_summary_goes_back_to_it() {
        let j = flow();
        let done = j.walk(&answered()).done;
        assert_eq!(done, ["work", "name"]);
        let mut d = jump(&done, 1, answered());
        assert_eq!((d.at.as_deref(), d.back.clone()), (Some("name"), vec![Some("work".to_owned())]));
        advance(&j, &mut d);
        assert_eq!(d.at, None, "straight back to the summary, not through every step");
        assert_eq!(d.back, [Some("work".to_owned()), Some("name".to_owned())], "Back still walks the path");

        // A new branch is asked before the summary.
        let mut d = jump(&done, 0, answered());
        d.answers.0.get_mut("work").unwrap().options = vec!["yes".into()];
        d.answers.0.remove("name");
        advance(&j, &mut d);
        assert_eq!(d.at.as_deref(), Some("name"), "an unanswered step on the new path is asked");
    }

    #[test]
    fn the_summary_leaves_the_access_role_out_and_the_end_says_what_next() {
        let mut j = flow();
        j.access_role = Some("joined".into());
        assert_eq!(you_get(&j, &["EEE".into()]).as_deref(), Some("You'll get: EEE."));
        assert_eq!(you_get(&j, &[]), None);
        assert_eq!(done_message(&j, true), None, "no gate, no message set: nothing to say");
        j.gate = true;
        assert_eq!(you_get(&j, &["EEE".into()]).as_deref(), Some("You'll get: EEE, and the rest of the server."));
        assert_eq!(done_message(&j, true).as_deref(), Some("The rest of the server is open now."));
        assert_eq!(done_message(&j, false), None, "they were in already");
        j.done_message = Some("Say hi in #introductions.".into());
        assert_eq!(done_message(&j, false).as_deref(), Some("Say hi in #introductions."));
    }

    #[test]
    fn flow_ids_survive_a_dm() {
        let id = format!("{}@123", CustomId::Join("pick:uni".into()));
        assert_eq!(CustomId::parse(&id), Some(CustomId::Join("pick:uni".into())));
        let start = effects::start_button(Snowflake(123), "Go");
        let id = start[0]["components"][0]["custom_id"].as_str().unwrap();
        assert_eq!(CustomId::parse(id), Some(CustomId::Do("join".into())), "the shared button opens the flow");
    }
}

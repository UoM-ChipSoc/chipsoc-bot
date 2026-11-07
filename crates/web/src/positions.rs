//! Committee positions (bot.toml `[[position]]`): what people can apply for
//! with `/volunteer`, the form they fill in, and whether it's taking
//! applications. Who holds each one is on the Members page.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::response::{Html, IntoResponse, Response};
use axum::{Extension, Form};
use chipsoc_config::bot::{Position, Question};
use chipsoc_config::ops::{Op, PositionField, Schema};
use chipsoc_discord::Rest;
use dioxus::prelude::*;

use crate::App;
use crate::auth::Session;
use crate::form::{self, Fields, Shown, area, check, confirm, edit, need_confirm, select, shown, text, wrong};
use crate::view::{self, Tab, csrf, empty};

const HERE: &str = "/positions";
/// Blank question rows offered after the existing ones.
const SPARE: usize = 2;

pub(crate) async fn page<R: Rest + 'static>(
    State(app): State<Arc<App<R>>>,
    Extension(s): Extension<Session>,
) -> Response {
    view(&app, &s, &Shown::none()).await
}

async fn view<R: Rest>(app: &App<R>, s: &Session, sh: &Shown) -> Response {
    match app.schema().await {
        Ok(cfg) => shown(sh, render(s, &cfg, sh)),
        Err(e) => view::broken(Tab::Positions, s, &e).into_response(),
    }
}

pub(crate) async fn act<R: Rest + Clone + 'static>(
    State(app): State<Arc<App<R>>>,
    Extension(s): Extension<Session>,
    Path(action): Path<String>,
    Form(f): Form<Vec<(String, String)>>,
) -> Response {
    let f = Fields(f);
    let cfg = match app.schema().await {
        Ok(c) => c,
        Err(e) => return view::broken(Tab::Positions, &s, &e).into_response(),
    };
    let built = build(&action, &f, &cfg);
    let closes = built.as_ref().is_ok_and(|(_, ops)| chipsoc_admin::rounds::closes_window(ops));
    match edit(&app, &s, HERE, f, built).await {
        Ok(done) => {
            if closes {
                close_windows(&app, &s).await;
            }
            done
        }
        Err(sh) => view(&app, &s, &sh).await,
    }
}

/// Closing a position closes its window: the card listing who applied is
/// posted now (the bot would within a minute anyway). Here and for the API
/// (the CLI and TUI signed in to this server).
pub(crate) async fn close_windows<R: Rest + Clone + 'static>(app: &App<R>, s: &Session) {
    let posted = match app.desk(s).await {
        Ok(d) => chipsoc_admin::rounds::post_closed(&d).await,
        Err(e) => Err(e),
    };
    if let Err(e) = posted {
        tracing::warn!("posting a closed window's applicants failed: {e:#}");
    }
}

fn build(action: &str, f: &Fields, cfg: &Schema) -> Result<(String, Vec<Op>), Shown> {
    let role = f.get("role").to_owned();
    let form = format!("pos-{role}");
    let set = |set| Op::SetPosition { role: role.clone(), set };
    match action {
        "add" => {
            let form = "pos-add";
            if role.is_empty() {
                return Err(wrong(form, f, "role", "Pick the role it gives."));
            }
            let position = Position { role: role.clone(), label: f.opt("label"), ..Default::default() };
            Ok((form.into(), vec![Op::AddPosition { position }]))
        }
        "edit" => {
            let Some(p) = cfg.bot.positions.iter().find(|p| p.role == role) else {
                return Err(form::refuse(&form, f, "No such position."));
            };
            let questions = questions(f);
            if questions.is_empty() {
                return Err(wrong(&form, f, "q0-ask", "Ask at least one question."));
            }
            let mut ops = Vec::new();
            if f.opt("label") != p.label {
                ops.push(set(PositionField::Label(f.opt("label"))));
            }
            if f.has("admin") != p.admin {
                ops.push(set(PositionField::Admin(f.has("admin"))));
            }
            if f.has("open") != p.open {
                ops.push(set(PositionField::Open(f.has("open"))));
            }
            let intro = Some(f.raw("intro").trim().to_owned()).filter(|i| !i.is_empty());
            if intro != p.intro {
                ops.push(set(PositionField::Intro(intro)));
            }
            if questions != p.questions {
                ops.push(set(PositionField::Questions(questions)));
            }
            Ok((form, ops))
        }
        "open" => Ok((form.clone(), vec![set(PositionField::Open(f.get("open") == "true"))])),
        "move" => Ok((form.clone(), vec![Op::MovePosition { role: role.clone(), to: form::direction(f) }])),
        "remove" => {
            need_confirm(&form, f)?;
            Ok((form.clone(), vec![Op::RemovePosition { role: role.clone() }]))
        }
        _ => Err(form::refuse("", f, "Unknown action.")),
    }
}

/// The question rows: `q{i}-ask`, `-long`, `-optional`, `-hint`; blank ones left out.
fn questions(f: &Fields) -> Vec<Question> {
    let n: usize = f.get("count").parse().unwrap_or(0);
    (0..n.min(25))
        .filter_map(|i| {
            let at = |name: &str| format!("q{i}-{name}");
            let ask = f.get(&at("ask"));
            (!ask.is_empty()).then(|| Question {
                ask: ask.to_owned(),
                long: f.has(&at("long")),
                optional: f.has(&at("optional")),
                hint: f.opt(&at("hint")),
            })
        })
        .collect()
}

/// The admin role's real name ("Committee"), else "admin".
fn admin_name(cfg: &Schema) -> String {
    cfg.bot.admin_role.as_deref().map_or_else(|| "admin".to_owned(), |k| role_name(cfg, k))
}

fn role_name(cfg: &Schema, key: &str) -> String {
    cfg.roles.iter().find(|r| r.key == key).map_or_else(|| key.to_owned(), |r| r.name.clone())
}

fn render(s: &Session, cfg: &Schema, sh: &Shown) -> Html<String> {
    let n = cfg.bot.positions.len();
    view::page(
        Tab::Positions,
        s,
        rsx! {
            h1 { "Positions" }
            p { class: "hint",
                "Roles people can apply for with /volunteer. Accepting an application gives them the role and lists them on the Members page."
            }
            if cfg.bot.review.is_none() {
                p { class: "note bad", role: "alert",
                    "Applications are off. Ask whoever runs the bot to turn them on (bot.toml, [review])."
                }
            }
            if cfg.bot.positions.is_empty() {
                {empty("No positions yet. Add one below to start taking applications.")}
            }
            for (i, p) in cfg.bot.positions.iter().enumerate() {
                {card(s, cfg, p, i, n, sh)}
            }
            {add_form(s, cfg, sh)}
        },
    )
}

fn card(s: &Session, cfg: &Schema, p: &Position, i: usize, n: usize, sh: &Shown) -> Element {
    let form = format!("pos-{}", p.role);
    let role = p.role.as_str();
    let name = role_name(cfg, role);
    let title = p.label.clone().unwrap_or_else(|| name.clone());
    let label = p.label.clone().unwrap_or_default();
    let intro = p.intro.clone().unwrap_or_default();
    let open_value = (!p.open).to_string();
    let mut rows: Vec<Question> = p.questions.clone();
    if sh.open(&form) {
        rows = questions(&sh.fields);
    }
    rows.extend(std::iter::repeat_n(Question::new(""), SPARE));
    let count = rows.len().to_string();
    let admin = admin_name(cfg);
    rsx! {
        section { class: "card", id: "{form}",
            div { class: "item",
                h2 {
                    "{title}"
                    if p.admin {
                        span { class: "tag", "also gives {admin}" }
                    }
                    span { class: "tag", if p.open { "taking applications" } else { "not taking applications" } }
                }
                div { class: "actions",
                    {form::button(s, "/positions/open", &[("role", role), ("open", &open_value)], if p.open { "Close applications" } else { "Open applications" }, "quiet small")}
                    if i > 0 {
                        {form::mover(s, "/positions/move", &[("role", role)], true, &title)}
                    }
                    if i + 1 < n {
                        {form::mover(s, "/positions/move", &[("role", role)], false, &title)}
                    }
                    {confirm(s, "/positions/remove", &[("role", role)], "Remove", &format!("Yes, remove {title}"), "People can't apply for it any more. Whoever holds the role keeps it.")}
                }
            }
            p { class: "hint", "Gives the role {name}. {view::count(p.questions.len(), \"question\")} on its form." }
            details { class: "edit", open: sh.open(&form),
                summary { "Edit the position and its form" }
                form { method: "post", action: "/positions/edit",
                    {csrf(s)}
                    {form::hidden(&[("role", role), ("count", &count)])}
                    {sh.problems(&form)}
                    {text(sh, &form, "label", "Label", &label, &format!("Shown in the /volunteer menu; empty uses the role's name ({name})."))}
                    {check(sh, &form, "admin", &format!("Holders also get the {admin} role (dashboard and /server access)"), p.admin)}
                    {check(sh, &form, "open", "Taking applications", p.open)}
                    {area(sh, &form, "intro", "Intro", &intro, 3, "Shown above the questions. Discord forms hold 5 boxes; the intro takes one.")}
                    fieldset {
                        legend { "Questions" }
                        p { class: "hint", "Leave a question empty to take it out." }
                        for (i, q) in rows.iter().enumerate() {
                            {question_row(sh, &form, i, q)}
                        }
                    }
                    button { "Save {title}" }
                }
            }
        }
    }
}

fn question_row(sh: &Shown, form: &str, i: usize, q: &Question) -> Element {
    let hint = q.hint.clone().unwrap_or_default();
    let fresh = Shown::none();
    rsx! {
        div { class: "question",
            {text(sh, form, &format!("q{i}-ask"), &format!("Question {}", i + 1), &q.ask, "")}
            {text(&fresh, form, &format!("q{i}-hint"), "Example shown in the empty box (optional)", &hint, "")}
            {check(&fresh, form, &format!("q{i}-long"), "Long answer (a paragraph box)", q.long)}
            {check(&fresh, form, &format!("q{i}-optional"), "Can be left blank", q.optional)}
        }
    }
}

fn add_form(s: &Session, cfg: &Schema, sh: &Shown) -> Element {
    let form = "pos-add";
    let free: Vec<(String, String)> = std::iter::once((String::new(), "Pick a role".to_owned()))
        .chain(
            cfg.roles
                .iter()
                .filter(|r| !r.managed && r.key != "everyone" && !cfg.bot.positions.iter().any(|p| p.role == r.key))
                .map(|r| (r.key.clone(), r.name.clone())),
        )
        .collect();
    rsx! {
        h2 { "Add a position" }
        form { method: "post", action: "/positions/add", class: "card", id: "{form}",
            {csrf(s)}
            {sh.problems(form)}
            {select(sh, form, "role", "Role it gives", &free, "")}
            {text(sh, form, "label", "Label (optional)", "", "Shown in the /volunteer menu.")}
            p { class: "hint", "It starts open, with the usual questions; edit them after adding. Make the role on the Roles page first if it's new." }
            button { "Add position" }
        }
    }
}

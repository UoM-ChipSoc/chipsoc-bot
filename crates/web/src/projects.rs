//! Projects: add one, edit its details, brief and application questions,
//! change its status (confirming the big steps), open or close applications,
//! and its leads. Each edit is an `Op` on projects.toml (keeping its
//! comments); Discord follows on apply. Leads go through `chipsoc_admin`
//! (on the Members routes), so their Lead role follows straight away.

use std::sync::Arc;

use anyhow::{Result, bail};
use axum::extract::{Path, Query, State};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::{Extension, Form};
use chipsoc_config::ops::{Op, ProjectField, Schema};
use chipsoc_config::{Config, Project, ProjectStatus, slug};
use chipsoc_discord::Rest;
use dioxus::prelude::*;
use serde::Deserialize;

use crate::App;
use crate::auth::Session;
use crate::form::{self, Fields, Shown, area, check, confirm, confirm_calm, edit, text, wrong};
use crate::people::{ID_HINT, Names};
// What the add form asks for; kept to refill the form after a problem.
use crate::remote::NewProject as Draft;
use crate::remote::SetApplications;
use crate::view::{self, Tab, csrf, done, empty, trouble};

/// Extra application questions Discord's form has room for.
const MAX_QUESTIONS: usize = 3;

#[derive(Deserialize)]
pub(crate) struct Notice {
    /// The project just saved, to confirm it.
    saved: Option<String>,
}

pub(crate) async fn page<R: Rest + Clone + 'static>(
    State(app): State<Arc<App<R>>>,
    Extension(s): Extension<Session>,
    Query(q): Query<Notice>,
) -> Response {
    let saved = q.saved.unwrap_or_default();
    show(&app, &s, Shows { saved: &saved, ..Shows::default() }, &Shown::none()).await
}

/// What the page shows besides the projects: a notice in the project just
/// saved, a problem at the top, and the add form as it was sent.
#[derive(Default)]
struct Shows<'a> {
    saved: &'a str,
    problem: Option<String>,
    draft: Draft,
    /// Why the add form was refused, shown beside it.
    add_problem: Option<String>,
}

/// The page, with `sh` shown again.
async fn show<R: Rest + Clone>(app: &App<R>, s: &Session, shows: Shows<'_>, sh: &Shown) -> Response {
    let loaded = async { anyhow::Ok((app.config().await?, app.schema().await?)) };
    match loaded.await {
        Ok((cfg, schema)) => {
            let leads: Vec<String> =
                cfg.projects.iter().flat_map(|p| p.leads.iter().map(ToString::to_string)).collect();
            let leads: Vec<&str> = leads.iter().map(String::as_str).collect();
            let names = match app.desk(s).await {
                Ok(d) => Names::of(&d, &leads).await,
                Err(_) => Names::default(),
            };
            form::shown(sh, render(s, &cfg, &schema, &shows, &names, sh))
        }
        Err(e) => view::page(Tab::Projects, s, trouble(&format!("{e:#}"))).into_response(),
    }
}

/// The status form: every change of status needs `confirm`.
#[derive(Deserialize)]
pub(crate) struct StatusForm {
    status: ProjectStatus,
    #[serde(default)]
    confirm: String,
}

pub(crate) async fn set_status<R: Rest + Clone + 'static>(
    State(app): State<Arc<App<R>>>,
    Extension(s): Extension<Session>,
    Path(slug): Path<String>,
    Form(f): Form<StatusForm>,
) -> Response {
    if f.confirm != "yes" {
        let e = anyhow::anyhow!("Press the “Yes” button to confirm the change of status.");
        return problem(&app, &s, e).await;
    }
    match save_status(&app, &s, &slug, f.status).await {
        Ok(()) => Redirect::to(&saved_url(&slug)).into_response(),
        Err(e) => problem(&app, &s, e).await,
    }
}

pub(crate) async fn save_status<R: Rest>(app: &App<R>, s: &Session, slug: &str, status: ProjectStatus) -> Result<()> {
    let op = Op::SetProject { slug: slug.to_owned(), set: ProjectField::Status(status) };
    app.edit(s, &op).await.map(drop)
}

pub(crate) async fn set_applications<R: Rest + Clone + 'static>(
    State(app): State<Arc<App<R>>>,
    Extension(s): Extension<Session>,
    Path(slug): Path<String>,
    Form(f): Form<SetApplications>,
) -> Response {
    match save_applications(&app, &s, &slug, f.open).await {
        Ok(()) => Redirect::to(&saved_url(&slug)).into_response(),
        Err(e) => problem(&app, &s, e).await,
    }
}

pub(crate) async fn save_applications<R: Rest>(app: &App<R>, s: &Session, slug: &str, open: bool) -> Result<()> {
    let op = Op::SetProject { slug: slug.to_owned(), set: ProjectField::Applications(open) };
    app.edit(s, &op).await.map(drop)
}

pub(crate) async fn add<R: Rest + Clone + 'static>(
    State(app): State<Arc<App<R>>>,
    Extension(s): Extension<Session>,
    Form(d): Form<Draft>,
) -> Response {
    match save_new(&app, &s, &d).await {
        Ok(added) => Redirect::to(&saved_url(&added)).into_response(),
        Err(e) => {
            let shows = Shows { draft: d, add_problem: Some(format!("{e:#}")), ..Shows::default() };
            let mut res = show(&app, &s, shows, &Shown::none()).await;
            *res.status_mut() = axum::http::StatusCode::UNPROCESSABLE_ENTITY;
            res
        }
    }
}

/// Add the project `d` describes; its slug.
pub(crate) async fn save_new<R: Rest>(app: &App<R>, s: &Session, d: &Draft) -> Result<String> {
    if d.name.trim().is_empty() {
        bail!("Give the project a name.");
    }
    let op = Op::AddProject { name: d.name.clone(), blurb: d.blurb.clone(), area: d.area.clone(), status: d.status };
    app.edit(s, &op).await?;
    Ok(slug(d.name.trim()))
}

/// `POST /projects/{slug}/{edit|brief|questions}`: the other edits.
pub(crate) async fn act<R: Rest + Clone + 'static>(
    State(app): State<Arc<App<R>>>,
    Extension(s): Extension<Session>,
    Path((slug, action)): Path<(String, String)>,
    Form(f): Form<Vec<(String, String)>>,
) -> Response {
    let f = Fields(f);
    let cfg = match app.config().await {
        Ok(c) => c,
        Err(e) => return problem(&app, &s, e).await,
    };
    let built = build(&slug, &action, &f, &cfg);
    let here = if action == "rename" { saved_url(f.get("slug").trim()) } else { saved_url(&slug) };
    match edit(&app, &s, &here, f, built).await {
        Ok(done) => done,
        Err(sh) => show(&app, &s, Shows::default(), &sh).await,
    }
}

fn build(slug: &str, action: &str, f: &Fields, cfg: &Config) -> Result<(String, Vec<Op>), Shown> {
    let form = format!("{action}-{slug}");
    let Some(p) = cfg.projects.iter().find(|p| p.slug == slug) else {
        return Err(form::refuse(&form, f, "No such project."));
    };
    let set = |set| Op::SetProject { slug: slug.to_owned(), set };
    let mut ops = Vec::new();
    match action {
        "edit" => {
            if f.get("name").is_empty() {
                return Err(wrong(&form, f, "name", "A project needs a name."));
            }
            let github = f.opt("github");
            if github.as_deref().is_some_and(|g| !g.starts_with("https://")) {
                return Err(wrong(&form, f, "github", "Give the whole address, starting https://."));
            }
            if f.get("name") != p.name {
                ops.push(set(ProjectField::Name(f.get("name").to_owned())));
            }
            if f.get("blurb") != p.blurb {
                ops.push(set(ProjectField::Blurb(f.get("blurb").to_owned())));
            }
            if f.get("area") != p.kind {
                ops.push(set(ProjectField::Area(f.get("area").to_owned())));
            }
            if f.opt("stage") != p.stage {
                ops.push(set(ProjectField::Stage(f.opt("stage"))));
            }
            if github != p.github {
                ops.push(set(ProjectField::Github(github)));
            }
            if f.list("tags") != p.tags {
                ops.push(set(ProjectField::Tags(f.list("tags"))));
            }
        }
        "rename" => {
            let to = f.get("slug").trim();
            if !chipsoc_config::is_key(to) {
                return Err(wrong(&form, f, "slug", "Use lower-case letters, digits and dashes."));
            }
            if to != slug {
                ops.push(Op::RenameProject { from: slug.to_owned(), to: to.to_owned() });
            }
        }
        "brief" => ops.push(Op::SetBrief { slug: slug.to_owned(), body: f.raw("body").replace("\r\n", "\n") }),
        "questions" => {
            let questions: Vec<String> =
                f.raw("questions").lines().map(str::trim).filter(|q| !q.is_empty()).map(str::to_owned).collect();
            if questions.len() > MAX_QUESTIONS {
                return Err(wrong(&form, f, "questions", "Discord's form has room for 3 extra questions."));
            }
            if f.has("lead_question") != p.application.lead_question {
                ops.push(set(ProjectField::LeadQuestion(f.has("lead_question"))));
            }
            if questions != p.application.questions {
                ops.push(set(ProjectField::Questions(questions)));
            }
        }
        _ => return Err(form::refuse(&form, f, "Unknown action.")),
    }
    Ok((form, ops))
}

/// The list again with what went wrong at the top.
async fn problem<R: Rest + Clone>(app: &App<R>, s: &Session, e: anyhow::Error) -> Response {
    show(app, s, Shows { problem: Some(format!("{e:#}")), ..Shows::default() }, &Shown::none()).await
}

/// Only clean slugs go into the URL; anything else just returns to the list.
fn saved_url(project: &str) -> String {
    if project == slug(project) { format!("/projects?saved={project}#{project}") } else { "/projects".into() }
}

fn render(s: &Session, cfg: &Config, schema: &Schema, shows: &Shows, names: &Names, sh: &Shown) -> Html<String> {
    view::page(
        Tab::Projects,
        s,
        rsx! {
            h1 { "Projects" }
            p { class: "hint", "Changes reach Discord when you apply them. Leads change straight away." }
            if let Some(p) = &shows.problem {
                {trouble(p)}
            }
            if cfg.projects.is_empty() {
                {empty("No projects yet. Add the first one below.")}
            }
            for p in &cfg.projects {
                {card(s, cfg, schema, p, shows.saved == p.slug, names, sh)}
            }
            {add_form(s, cfg, &shows.draft, shows.add_problem.as_deref())}
        },
    )
}

fn card(s: &Session, cfg: &Config, schema: &Schema, p: &Project, saved: bool, names: &Names, sh: &Shown) -> Element {
    let open = p.application.open;
    let slug = p.slug.as_str();
    let brief = schema.briefs.get(slug).map(|b| b.body.clone()).unwrap_or_default();
    let brief_hint = schema
        .briefs
        .get(slug)
        .map_or_else(|| "Saved as a new file under content/projects.".to_owned(), |b| format!("Kept in {}.", b.file));
    let questions = p.application.questions.join("\n");
    let status_url = format!("/projects/{slug}/status");
    let (brief_form, questions_form) = (format!("brief-{slug}"), format!("questions-{slug}"));
    rsx! {
        section { class: "card", id: "{p.slug}",
            h2 {
                "{p.name}"
                span { class: "tag", "{status_label(p.status)}" }
            }
            if saved {
                {done(&format!("Saved {}. {}", p.name, form::APPLY_HINT))}
            }
            if !p.blurb.is_empty() {
                p { "{p.blurb}" }
            }
            p { class: "hint",
                if !p.kind.is_empty() {
                    "Area: {p.kind}. "
                }
                if let Some(st) = &p.stage {
                    "Stage: {st}. "
                }
                if let Some(g) = &p.github {
                    a { href: "{g}", rel: "noreferrer", "Code repository" }
                }
            }
            div { class: "choices", role: "group", "aria-label": "Status",
                for st in ProjectStatus::ALL {
                    if st == p.status {
                        button { class: "current", disabled: true, "{status_label(st)}" }
                    } else {
                        {confirm_calm(s, &status_url, &[("status", st.as_str())], status_label(st), &format!("Yes, make {} {}", p.name, status_label(st).to_lowercase()), &change_hint(p.status, st))}
                    }
                }
            }
            p { class: "hint", "Now: {status_hint(p.status)}." }
            form { method: "post", action: "/projects/{p.slug}/applications", class: "row",
                {csrf(s)}
                input { r#type: "hidden", name: "open", value: "{!open}" }
                span { if open { "Taking applications" } else { "Not taking applications" } }
                button { class: "quiet", if open { "Close applications" } else { "Open applications" } }
            }
            {leads(s, p, names, sh)}
            {details_form(s, cfg, p, sh)}
            {rename_form(s, p, sh)}
            details { class: "edit", open: sh.open(&brief_form),
                summary { "Post text" }
                form { method: "post", action: "/projects/{slug}/brief",
                    {csrf(s)}
                    {sh.problems(&brief_form)}
                    {area(sh, &brief_form, "body", "The project's post (markdown)", &brief, 8, &brief_hint)}
                    button { "Save post text" }
                }
            }
            details { class: "edit", open: sh.open(&questions_form),
                summary { "Application form" }
                form { method: "post", action: "/projects/{slug}/questions",
                    {csrf(s)}
                    {sh.problems(&questions_form)}
                    {check(sh, &questions_form, "lead_question", "Ask whether they'd like to lead it", p.application.lead_question)}
                    {area(sh, &questions_form, "questions", "Extra questions (one per line, up to 3)", &questions, 3, "")}
                    button { "Save form" }
                }
            }
        }
    }
}

fn leads(s: &Session, p: &Project, names: &Names, sh: &Shown) -> Element {
    let slug = p.slug.as_str();
    let form = format!("lead-{slug}");
    if !p.lead_role {
        return rsx! { p { class: "hint", "This project has no Lead role." } };
    }
    rsx! {
        div { class: "row",
            span {
                "Leads: "
                if p.leads.is_empty() {
                    span { class: "muted", "none yet" }
                }
                for l in &p.leads {
                    span { class: "tag", {names.who(&l.to_string())} }
                }
            }
            a { href: "/members#holders-{p.lead_role_key()}", "Names on the Members page" }
        }
        if !p.leads.is_empty() {
            details { class: "edit",
                summary { "Remove a lead" }
                for l in &p.leads {
                    {
                        let name = names.name(&l.to_string());
                        confirm(s, "/members/unlead", &[("slug", slug), ("user", &l.to_string()), ("back", "/projects")], &format!("Remove {name}"), &format!("Yes, take {name} off {}'s leads", p.name), "They lose the Lead role straight away.")
                    }
                }
            }
        }
        details { class: "edit", open: sh.open(&form),
            summary { "Add a lead" }
            form { method: "post", action: "/members/lead",
                {csrf(s)}
                {form::hidden(&[("slug", slug), ("back", "/projects")])}
                {sh.problems(&form)}
                {text(sh, &form, "user", "Who", "", ID_HINT)}
                {form::pick(sh, &form)}
                button { "Make lead" }
            }
        }
    }
}

fn details_form(s: &Session, cfg: &Config, p: &Project, sh: &Shown) -> Element {
    let form = format!("edit-{}", p.slug);
    let areas = cfg.bot.projects.as_ref().map(|p| p.areas.clone()).unwrap_or_default();
    let area_hint = if areas.is_empty() { String::new() } else { format!("Usual areas: {}.", areas.join(", ")) };
    let stage = p.stage.clone().unwrap_or_default();
    let github = p.github.clone().unwrap_or_default();
    let tags = p.tags.join(", ");
    rsx! {
        details { class: "edit", open: sh.open(&form),
            summary { "Details" }
            form { method: "post", action: "/projects/{p.slug}/edit",
                {csrf(s)}
                {sh.problems(&form)}
                {text(sh, &form, "name", "Name", &p.name, "")}
                {text(sh, &form, "blurb", "One-line description", &p.blurb, "")}
                {text(sh, &form, "area", "Area", &p.kind, &area_hint)}
                {text(sh, &form, "stage", "Stage (optional)", &stage, "Where it's at, shown on its post, e.g. “taping out”.")}
                {text(sh, &form, "github", "Code link (optional)", &github, "A link to its repository.")}
                {text(sh, &form, "tags", "Tags (optional)", &tags, "Separated by commas.")}
                button { "Save details" }
            }
        }
    }
}

fn rename_form(s: &Session, p: &Project, sh: &Shown) -> Element {
    let form = format!("rename-{}", p.slug);
    rsx! {
        details { class: "edit", open: sh.open(&form),
            summary { "Short name (slug)" }
            form { method: "post", action: "/projects/{p.slug}/rename",
                {csrf(s)}
                {sh.problems(&form)}
                {text(sh, &form, "slug", "Short name (slug)", &p.slug, "Used in its roles', channels' and buttons' keys. Renaming keeps its roles, channels and post.")}
                button { "Rename" }
            }
        }
    }
}

fn add_form(s: &Session, cfg: &Config, d: &Draft, problem: Option<&str>) -> Element {
    let areas = cfg.bot.projects.as_ref().map(|p| p.areas.clone()).unwrap_or_default();
    rsx! {
        h2 { "Add a project" }
        form { method: "post", action: "/projects", class: "card", id: "add",
            {csrf(s)}
            if let Some(p) = problem {
                {trouble(p)}
            }
            label { r#for: "name", "Name" }
            input { id: "name", name: "name", r#type: "text", required: true, value: "{d.name}" }
            label { r#for: "blurb", "One-line description" }
            input { id: "blurb", name: "blurb", r#type: "text", value: "{d.blurb}" }
            label { r#for: "area", "Area" }
            input { id: "area", name: "area", r#type: "text", list: "areas", value: "{d.area}" }
            datalist { id: "areas",
                for a in areas {
                    option { value: "{a}" }
                }
            }
            label { r#for: "status", "Status" }
            select { id: "status", name: "status",
                for st in ProjectStatus::ALL {
                    option { value: st.as_str(), selected: st == d.status,
                        "{status_label(st)}: {status_hint(st)}"
                    }
                }
            }
            p { class: "hint", "It starts with the usual channels and team roles. Nothing changes in Discord until you apply." }
            button { "Add project" }
        }
    }
}

fn status_label(s: ProjectStatus) -> &'static str {
    match s {
        ProjectStatus::Proposed => "Proposed",
        ProjectStatus::Active => "Active",
        ProjectStatus::Past => "Past",
    }
}

/// What changing status does, as a sentence.
fn change_hint(from: ProjectStatus, to: ProjectStatus) -> String {
    let what = chipsoc_config::projects::status_change(from, to).unwrap_or_default();
    let mut c = what.chars();
    c.next().map(|f| f.to_uppercase().chain(c).chain(['.']).collect()).unwrap_or_default()
}

fn status_hint(s: ProjectStatus) -> &'static str {
    match s {
        ProjectStatus::Proposed => "an idea; a forum post, no roles or channels yet",
        ProjectStatus::Active => "running; it has its own channels",
        ProjectStatus::Past => "done; its channels are archived, history kept",
    }
}

//! Who holds what: positions and the roles the bot keeps up to date, and
//! each project's leads, by name; appoint and remove (the role follows
//! straight away), and give people who left and came back their old roles.
//! People are typed as an @username, a name or an id.

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::response::{Html, IntoResponse, Response};
use axum::{Extension, Form};
use chipsoc_admin::join::About;
use chipsoc_admin::{Holding, Roster};
use chipsoc_discord::Rest;
use dioxus::prelude::*;

use crate::App;
use crate::auth::Session;
use crate::form::{self, Fields, Shown, confirm, said_back, select, shown, spawned, text, user_id, when, wrong};
use crate::people::{ID_HINT, resolve};
use crate::view::{self, Tab, csrf, empty};

const HERE: &str = "/members";

/// `?about=<who>`: show what they told the bot (an @username, a name or an id).
#[derive(Debug, Default, serde::Deserialize)]
pub(crate) struct Lookup {
    #[serde(default)]
    about: String,
}

pub(crate) async fn page<R: Rest + Clone + 'static>(
    State(app): State<Arc<App<R>>>,
    Extension(s): Extension<Session>,
    Query(q): Query<Lookup>,
) -> Response {
    let about = match q.about.trim() {
        "" => None,
        typed => {
            let found = async {
                let desk = app.desk(&s).await.map_err(|e| Looked::Failed(format!("{e:#}")))?;
                let user = resolve(&desk, typed).await.map_err(|(why, found)| {
                    Looked::Unsure(why, found.iter().map(|m| (m.id.to_string(), m.label())).collect())
                })?;
                chipsoc_admin::join::about(&desk, user).await.map_err(|e| Looked::Failed(format!("{e:#}")))
            };
            Some(found.await)
        }
    };
    show(&app, &s, &Shown::none(), about.as_ref(), &q.about).await
}

async fn view<R: Rest + Clone>(app: &App<R>, s: &Session, sh: &Shown) -> Response {
    show(app, s, sh, None, "").await
}

/// Why someone looked up can't be shown.
enum Looked {
    /// Not one person: why, and who it could be (id, label).
    Unsure(String, Vec<(String, String)>),
    Failed(String),
}

async fn show<R: Rest + Clone>(
    app: &App<R>,
    s: &Session,
    sh: &Shown,
    about: Option<&Result<About, Looked>>,
    typed: &str,
) -> Response {
    let loaded = async {
        let desk = app.desk(s).await?;
        let r = chipsoc_admin::roster(&desk).await?;
        // Who's in the server now, so Restore is only offered to people who came back.
        let back = chipsoc_admin::members_of(&desk, r.former.iter().map(|f| f.person.id)).await;
        anyhow::Ok((r, back.into_keys().collect::<Vec<_>>()))
    };
    match loaded.await {
        Ok((r, back)) => shown(sh, render(s, &r, &back, sh, about, typed)),
        Err(e) => view::broken(Tab::Members, s, &e).into_response(),
    }
}

pub(crate) async fn act<R: Rest + Clone + 'static>(
    State(app): State<Arc<App<R>>>,
    Extension(s): Extension<Session>,
    Path(action): Path<String>,
    Form(f): Form<Vec<(String, String)>>,
) -> Response {
    let f = Fields(f);
    // The Projects page sends its lead forms here and wants to go back there.
    let here = if f.get("back") == "/projects" { "/projects" } else { HERE };
    let role = f.get("role").to_owned();
    let slug = f.get("slug").to_owned();
    // Appointing: who they typed is found first, and comes back with what's wrong.
    let form = match action.as_str() {
        "appoint" => "appoint".to_owned(),
        "lead" => format!("lead-{slug}"),
        _ => String::new(),
    };
    let user = if form.is_empty() {
        let Some(user) = user_id(f.get("user")) else { return said_back(here, Err("Which person?".into())) };
        user
    } else {
        let typed = if f.has("pick") { f.get("pick") } else { f.get("user") };
        let found = async { resolve(&app.desk(&s).await.map_err(|e| (format!("{e:#}"), Vec::new()))?, typed).await };
        match found.await {
            Ok(user) => user,
            Err((why, found)) => {
                let mut sh = wrong(&form, &f, "user", &why);
                sh.choices = found.iter().map(|m| (m.id.to_string(), m.label())).collect();
                return view(&app, &s, &sh).await;
            }
        }
    };
    let needs_confirm = matches!(action.as_str(), "unappoint" | "unlead" | "forget" | "restore");
    if needs_confirm && !f.confirmed() {
        return said_back(here, Err(crate::form::CONFIRM_FIRST.into()));
    }
    let done = match action.as_str() {
        "appoint" => spawned!(app, s, |d| chipsoc_admin::appoint(d, &role, user, &d.actor).await),
        "unappoint" => spawned!(app, s, |d| chipsoc_admin::unappoint(d, &role, user, &d.actor).await),
        "lead" => spawned!(app, s, |d| chipsoc_admin::add_project_lead(d, &slug, user, &d.actor).await),
        "unlead" => spawned!(app, s, |d| chipsoc_admin::remove_lead(d, &slug, user, &d.actor).await),
        "restore" => {
            spawned!(app, s, |d| chipsoc_admin::restore(d, user, &d.actor)
                .await
                .map(|t| chipsoc_admin::Said::new().text("Restored.").then(t)))
        }
        "forget" => spawned!(app, s, |d| {
            match chipsoc_admin::forget_former(d, user, &d.actor).await {
                Ok(true) => Ok("Their old roles won't be offered back.".to_owned()),
                Ok(false) => Err("Already dealt with.".to_owned()),
                Err(e) => Err(format!("{e:#}")),
            }
        }),
        _ => Err("Unknown action.".to_owned()),
    };
    match done {
        Err(why) if !form.is_empty() => view(&app, &s, &form::refuse(&form, &f, &why)).await,
        done => said_back(here, done),
    }
}

fn render(
    s: &Session,
    r: &Roster,
    back: &[chipsoc_config::Snowflake],
    sh: &Shown,
    about: Option<&Result<About, Looked>>,
    typed: &str,
) -> Html<String> {
    view::page(
        Tab::Members,
        s,
        rsx! {
            h1 { "Members" }
            p { class: "hint",
                "Who holds each committee role and who leads each project. Changes here happen in Discord straight away."
            }
            {lookup(about, typed)}
            h2 { "Positions and roles" }
            if r.holders.is_empty() {
                {empty("Nobody is appointed to anything yet. Add a position on the Positions page, and who holds it shows here.")}
            } else {
                for h in &r.holders {
                    {holding(s, h, "unappoint", &[("role", &h.role)])}
                }
            }
            if !r.holders.is_empty() || sh.open("appoint") {
                {appoint_form(s, r, sh)}
            }
            h2 { "Project leads" }
            if r.leads.is_empty() {
                {empty("No projects yet; add them on the Projects page.")}
            }
            for h in &r.leads {
                {lead_card(s, h, sh)}
            }
            h2 { "People who left" }
            if r.former.is_empty() {
                {empty("Nobody who held a role has left. If they do, they're listed here so you can give their roles back when they return.")}
            } else {
                {view::table(&["Who", "Held", "Left", ""], rsx! {
                    for f in &r.former {
                        {former(s, f, back.contains(&f.person.id))}
                    }
                })}
            }
        },
    )
}

/// Someone who left holding roles: give them back (once they're back) or forget.
fn former(s: &Session, f: &chipsoc_admin::FormerMember, back: bool) -> Element {
    let user = f.person.id.to_string();
    let held = f.roles.iter().map(|r| r.name.as_str()).collect::<Vec<_>>().join(", ");
    let name = &f.person.name;
    rsx! {
        tr {
            td { "{name}" if back { span { class: "tag", "back" } } }
            td { class: "wrap", "{held}" }
            td { "{when(f.since)}" }
            td { class: "actions",
                if back {
                    {form::confirm_calm(s, "/members/restore", &[("user", &user)], "Restore", &format!("Yes, give {name} back {held}"), "They get these roles in Discord straight away.")}
                } else {
                    button { class: "small quiet", disabled: true, title: "They aren't in the server now", "Restore" }
                }
                {confirm(s, "/members/forget", &[("user", &user)], "Forget", &format!("Yes, don't offer {name} their roles back"), "Their old roles won't be offered back if they rejoin.")}
            }
        }
    }
}

/// Look someone up: their profile and join-flow answers.
fn lookup(about: Option<&Result<About, Looked>>, typed: &str) -> Element {
    rsx! {
        form { method: "get", action: "/members", class: "card", id: "about",
            h3 { "Look someone up" }
            p { class: "hint", "What they told the bot: their name, where they work, and their join-flow answers." }
            label { r#for: "about-user", "Who" }
            input { id: "about-user", name: "about", value: "{typed}", "aria-describedby": "about-user-hint" }
            p { class: "hint", id: "about-user-hint", "{ID_HINT}" }
            button { "Show" }
            match about {
                None => rsx! {},
                Some(Err(Looked::Failed(why))) => rsx! { p { class: "note bad", role: "alert", {view::plain(why)} } },
                Some(Err(Looked::Unsure(why, found))) => rsx! {
                    div { class: "note bad", role: "alert",
                        p { "{why}" }
                        if !found.is_empty() {
                            ul {
                                for (id, label) in found {
                                    li { a { href: "/members?about={id}#about", "{label}" } }
                                }
                            }
                        }
                    }
                },
                Some(Ok(a)) => about_card(a),
            }
        }
    }
}

fn about_card(a: &About) -> Element {
    let who = if a.shown.is_empty() {
        format!("{} (not in the server now)", a.id)
    } else {
        format!("{} ({})", a.shown, a.id)
    };
    let name = format!("{} {}", a.name, a.surname).trim().to_owned();
    rsx! {
        h4 { "{who}" }
        if a.is_empty() {
            p { class: "muted", "They haven't been through the join flow or set a profile." }
        }
        dl {
            if !name.is_empty() {
                dt { "Name" }
                dd { "{name}" }
            }
            if !a.company.is_empty() {
                dt { "Company" }
                dd { "{a.company}" }
            }
            for s in &a.answers {
                dt { "{s.question}" }
                dd { if s.answer.is_empty() { "Not set" } else { "{s.answer}" } }
            }
        }
        if let Some(t) = a.finished {
            p { class: "hint", "Finished the join flow {when(t)}." }
        }
    }
}

fn holding(s: &Session, h: &Holding, remove: &str, ids: &[(&str, &str)]) -> Element {
    let action = format!("/members/{remove}");
    rsx! {
        section { class: "card", id: "holders-{h.role}",
            h3 {
                "{h.name}"
                if let Some(p) = &h.position {
                    if p != &h.name {
                        span { class: "tag", "position: {p}" }
                    }
                }
            }
            if h.people.is_empty() {
                p { class: "muted", "Nobody yet." }
            }
            ul { class: "plain",
                for p in &h.people {
                    li { class: "item",
                        span {
                            a { href: "/members?about={p.id}#about", title: "Discord user id {p.id}: what they told the bot", "{p.name}" }
                        }
                        {
                            let user = p.id.to_string();
                            let mut fields = ids.to_vec();
                            fields.push(("user", &user));
                            confirm(s, &action, &fields, &format!("Remove {}", p.name), &format!("Yes, take {} away from {}", h.name, p.name), "They lose the role in Discord straight away.")
                        }
                    }
                }
            }
        }
    }
}

fn lead_card(s: &Session, h: &Holding, sh: &Shown) -> Element {
    let slug = h.project.clone().unwrap_or_default();
    let form = format!("lead-{slug}");
    rsx! {
        {holding(s, h, "unlead", &[("slug", &slug)])}
        details { class: "edit", open: sh.open(&form),
            summary { "Add a lead" }
            form { method: "post", action: "/members/lead",
                {csrf(s)}
                {form::hidden(&[("slug", &slug)])}
                {sh.problems(&form)}
                {text(sh, &form, "user", "Who", "", ID_HINT)}
                {form::pick(sh, &form)}
                button { "Make lead" }
            }
        }
    }
}

fn appoint_form(s: &Session, r: &Roster, sh: &Shown) -> Element {
    let form = "appoint";
    let roles: Vec<(String, String)> = r.holders.iter().map(|h| (h.role.clone(), h.name.clone())).collect();
    rsx! {
        form { method: "post", action: "/members/appoint", class: "card", id: "{form}",
            h3 { "Appoint someone" }
            {csrf(s)}
            {sh.problems(form)}
            {select(sh, form, "role", "Which position or role", &roles, "")}
            {text(sh, form, "user", "Who", "", ID_HINT)}
            {form::pick(sh, form)}
            button { "Appoint" }
        }
    }
}

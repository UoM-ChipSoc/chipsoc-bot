//! Roles: list, add, edit (name, colour, shown separately, mentionable,
//! permissions), reorder and remove. Each change is an `Op` on roles.toml.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::response::{IntoResponse, Response};
use axum::{Extension, Form};
use chipsoc_config::Role;
use chipsoc_config::ops::{Op, RoleField, Schema};
use chipsoc_discord::Rest;
use dioxus::prelude::*;

use crate::App;
use crate::auth::Session;
use crate::form::{
    self, Fields, Shown, check, confirm, edit, is_colour, is_key, key_from, need_confirm, shown, text, wrong,
};
use crate::view::{self, Tab, csrf, empty, permission};

const HERE: &str = "/roles";

pub(crate) async fn page<R: Rest + 'static>(
    State(app): State<Arc<App<R>>>,
    Extension(s): Extension<Session>,
) -> Response {
    view(&app, &s, &Shown::none()).await
}

async fn view<R: Rest>(app: &App<R>, s: &Session, sh: &Shown) -> Response {
    match app.schema().await {
        Ok(cfg) => shown(sh, render(s, &cfg, sh)),
        Err(e) => view::broken(Tab::Roles, s, &e).into_response(),
    }
}

pub(crate) async fn act<R: Rest + 'static>(
    State(app): State<Arc<App<R>>>,
    Extension(s): Extension<Session>,
    Path(action): Path<String>,
    Form(f): Form<Vec<(String, String)>>,
) -> Response {
    let f = Fields(f);
    let cfg = match app.schema().await {
        Ok(c) => c,
        Err(e) => return view::broken(Tab::Roles, &s, &e).into_response(),
    };
    let built = build(&action, &f, &cfg);
    match edit(&app, &s, HERE, f, built).await {
        Ok(done) => done,
        Err(sh) => view(&app, &s, &sh).await,
    }
}

fn build(action: &str, f: &Fields, cfg: &Schema) -> Result<(String, Vec<Op>), Shown> {
    let key = f.get("key").to_owned();
    match action {
        "add" => {
            let form = "role-add";
            let name = f.get("name");
            if name.is_empty() {
                return Err(wrong(form, f, "name", "Give the role a name."));
            }
            let key = key_from(f.get("key"), name);
            if !is_key(&key) {
                return Err(wrong(form, f, "key", "Use lower-case letters, digits and dashes."));
            }
            let color = f.opt("color");
            if color.as_deref().is_some_and(|c| !is_colour(c)) {
                return Err(wrong(form, f, "color", "Give a colour like #2d6a5a, or leave it empty."));
            }
            let role = Role { key, name: name.to_owned(), color, ..Default::default() };
            Ok((form.into(), vec![Op::AddRole { role }]))
        }
        "edit" => {
            let form = format!("role-{key}");
            let Some(r) = cfg.roles.iter().find(|r| r.key == key) else {
                return Err(form::refuse(&form, f, "No such role."));
            };
            let mut ops = Vec::new();
            let set = |set| Op::SetRole { key: key.clone(), set };
            if key != "everyone" {
                let name = f.get("name");
                if name.is_empty() {
                    return Err(wrong(&form, f, "name", "A role needs a name."));
                }
                let color = f.opt("color");
                if color.as_deref().is_some_and(|c| !is_colour(c)) {
                    return Err(wrong(&form, f, "color", "Give a colour like #2d6a5a, or leave it empty."));
                }
                if name != r.name {
                    ops.push(set(RoleField::Name(name.to_owned())));
                }
                if color != r.color {
                    ops.push(set(RoleField::Color(color)));
                }
                if f.has("hoist") != r.hoist {
                    ops.push(set(RoleField::Hoist(f.has("hoist"))));
                }
                if f.has("mentionable") != r.mentionable {
                    ops.push(set(RoleField::Mentionable(f.has("mentionable"))));
                }
            }
            let perms: Vec<String> = f.all("perm").into_iter().map(str::to_owned).collect();
            if perms != r.permissions {
                ops.push(set(RoleField::Permissions(perms)));
            }
            Ok((form, ops))
        }
        "move" => Ok((format!("role-{key}"), vec![Op::MoveRole { key, to: form::direction(f) }])),
        "remove" => {
            let form = format!("role-{key}");
            need_confirm(&form, f)?;
            Ok((form, vec![Op::RemoveRole { key }]))
        }
        _ => Err(form::refuse("", f, "Unknown action.")),
    }
}

fn render(s: &Session, cfg: &Schema, sh: &Shown) -> axum::response::Html<String> {
    let roles = &cfg.roles;
    view::page(
        Tab::Roles,
        s,
        rsx! {
            h1 { "Roles" }
            p { class: "hint",
                "Top of the list is top of Discord's role list. Changes reach Discord when you apply them. "
                "Project roles are made from each project; edit them on the Projects page."
            }
            if roles.is_empty() {
                {empty("No roles yet. Add the first one below.")}
            }
            ul { class: "plain card",
                for (i, r) in roles.iter().enumerate() {
                    li { id: "role-{r.key}", {row(s, r, i, roles.len(), sh)} }
                }
            }
            {add_form(s, sh)}
        },
    )
}

fn row(s: &Session, r: &Role, i: usize, n: usize, sh: &Shown) -> Element {
    let form = format!("role-{}", r.key);
    let key = r.key.as_str();
    let everyone = key == "everyone";
    let colour = r.color.clone().unwrap_or_default();
    let perms: Vec<String> = r.permissions.iter().map(|p| permission(p)).collect();
    rsx! {
        div { class: "item",
            div {
                svg { class: "swatch", width: "12", height: "12", view_box: "0 0 12 12",
                    circle { cx: "6", cy: "6", r: "6", fill: r.color.as_deref().unwrap_or("#c9c5bc") }
                }
                strong { "{r.name}" }
                span { class: "tag", "{r.key}" }
                if r.managed {
                    span { class: "tag", "managed by Discord" }
                }
                if r.hoist {
                    span { class: "tag", "shown separately" }
                }
                if r.mentionable {
                    span { class: "tag", "mentionable" }
                }
                if !perms.is_empty() {
                    div { class: "hint", "{perms.join(\", \")}" }
                }
            }
            if !r.managed {
                div { class: "actions",
                    if !everyone && i > 0 {
                        {form::mover(s, "/roles/move", &[("key", key)], true, &r.name)}
                    }
                    if !everyone && i + 2 < n {
                        {form::mover(s, "/roles/move", &[("key", key)], false, &r.name)}
                    }
                    if !everyone {
                        {
                            confirm(
                                s,
                                "/roles/remove",
                                &[("key", key)],
                                "Remove",
                                &format!("Yes, remove {}", r.name),
                                "Taken out of the config. The role stays in Discord; delete it there if you want it gone.",
                            )
                        }
                    }
                }
            }
        }
        if !r.managed {
            details { class: "edit", open: sh.open(&form),
                summary { "Edit" }
                form { method: "post", action: "/roles/edit",
                    {csrf(s)}
                    {form::hidden(&[("key", key)])}
                    {sh.problems(&form)}
                    if !everyone {
                        {text(sh, &form, "name", "Name", &r.name, "")}
                        {text(sh, &form, "color", "Colour", &colour, "Like #2d6a5a; empty for none.")}
                        {check(sh, &form, "hoist", "Show members separately in the member list", r.hoist)}
                        {check(sh, &form, "mentionable", "Anyone can @mention it", r.mentionable)}
                    }
                    {permissions(sh, &form, &r.permissions)}
                    button { "Save {r.name}" }
                }
            }
        }
    }
}

/// A checkbox per permission, ticked for those given.
pub(crate) fn permissions(sh: &Shown, form: &str, given: &[String]) -> Element {
    let all = view::permissions_in_order();
    let sent: Vec<String> =
        if sh.open(form) { sh.fields.all("perm").into_iter().map(str::to_owned).collect() } else { given.to_vec() };
    rsx! {
        fieldset { class: "perms",
            legend { "Permissions" }
            for p in all {
                label { class: "check",
                    input { r#type: "checkbox", name: "perm", value: "{p}", checked: sent.iter().any(|g| g == p) }
                    "{permission(p)}"
                    if let Some(w) = view::permission_warning(p) {
                        span { class: "hint", " {w}" }
                    }
                }
            }
        }
    }
}

fn add_form(s: &Session, sh: &Shown) -> Element {
    let form = "role-add";
    rsx! {
        h2 { "Add a role" }
        form { method: "post", action: "/roles/add", class: "card", id: "{form}",
            {csrf(s)}
            {sh.problems(form)}
            {text(sh, form, "name", "Name", "", "")}
            {text(sh, form, "key", "Key (optional)", "", "A permanent id for the config; made from the name if left empty.")}
            {text(sh, form, "color", "Colour (optional)", "", "Like #2d6a5a.")}
            p { class: "hint", "It goes at the bottom of the list; move it up as needed. Nothing changes in Discord until you apply." }
            button { "Add role" }
        }
    }
}

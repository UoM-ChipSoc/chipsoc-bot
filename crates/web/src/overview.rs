//! The front page: what the server is, how big, and whether the config is healthy.

use std::sync::Arc;

use axum::Extension;
use axum::extract::State;
use axum::response::Html;
use chipsoc_admin::Host as _;
use chipsoc_admin::join::Progress;
use chipsoc_config::authoring::{Report, check_config};
use chipsoc_config::{Config, ProjectStatus};
use chipsoc_discord::Rest;
use dioxus::prelude::*;

use crate::App;
use crate::auth::Session;
use crate::people::Names;
use crate::view::{self, Tab, count, done, notes};

pub(crate) async fn page<R: Rest + Clone + 'static>(
    State(app): State<Arc<App<R>>>,
    Extension(s): Extension<Session>,
) -> Html<String> {
    let (cfg, report) = match app.config().await {
        Ok(cfg) => {
            let report = check_config(&cfg);
            (Some(cfg), report)
        }
        Err(e) => (None, Report { errors: vec![format!("{e:#}")], warnings: Vec::new() }),
    };
    // Applications waiting and the join flow's progress, if the bot's database can be read.
    let (queue, join, names) = match app.desk(&s).await {
        Ok(desk) => {
            let join = match cfg.as_ref().and_then(|c| c.join.as_ref()) {
                Some(_) => chipsoc_admin::join::progress(&desk).await.ok(),
                None => None,
            };
            let waiting: Vec<String> = join.iter().flat_map(|p| p.waiting.iter().map(ToString::to_string)).collect();
            let names = Names::of(&desk, &waiting.iter().map(String::as_str).collect::<Vec<_>>()).await;
            (desk.db().pending().await.ok(), join, names)
        }
        Err(_) => (None, None, Names::default()),
    };
    view::page(Tab::Overview, &s, overview(cfg.as_deref(), &report, queue, join.as_ref(), &names))
}

fn overview(
    cfg: Option<&Config>,
    report: &Report,
    queue: Option<i64>,
    join: Option<&Progress>,
    names: &Names,
) -> Element {
    let name = cfg.map_or("Your server", |c| c.desired.server.guild.name.as_str());
    rsx! {
        h1 { "{name}" }
        {health(report)}
        if let Some(n) = queue.filter(|n| *n > 0) {
            p { class: "note warn", role: "status",
                a { href: "/queue", "{waiting_text(n)}" }
            }
        }
        if let Some(cfg) = cfg {
            {stats(cfg, queue)}
            if let Some(p) = join {
                {join_card(cfg, p, names)}
            }
            {link_state(cfg)}
            {quick_links(cfg)}
        }
    }
}

/// "3 waiting in the review queue: …", for the link at the top.
fn waiting_text(n: i64) -> String {
    format!("{n} waiting in the review queue: applications and suggestions to Accept or Reject")
}

/// Where to go for the usual jobs.
fn quick_links(cfg: &Config) -> Element {
    let links = [
        ("/queue", "Review applications and suggestions"),
        ("/members", "See who holds what"),
        ("/projects", "Add or update a project"),
        ("/positions", "Open or close a position"),
        ("/channels", "Add or tidy channels"),
        ("/onboarding", "Edit what newcomers see"),
        ("/activity", "See what's been done"),
    ];
    rsx! {
        section { class: "card",
            h2 { "Jump to" }
            ul { class: "links",
                for (href, text) in links {
                    li { a { href: "{href}", "{text}" } }
                }
            }
            if cfg.bot.review.is_none() {
                p { class: "hint", "Applications are off. Ask whoever runs the bot to turn them on (bot.toml, [review])." }
            }
        }
    }
}

fn health(report: &Report) -> Element {
    if report.errors.is_empty() && report.warnings.is_empty() {
        return done("All good: the config files have no problems.");
    }
    rsx! {
        if !report.errors.is_empty() {
            p { class: "note bad", role: "alert",
                "The config has {count(report.errors.len(), \"problem\")} to fix before changes can be applied."
            }
        }
        {notes("Needs fixing", &report.errors)}
        {notes("Worth a look", &report.warnings)}
    }
}

fn stats(cfg: &Config, queue: Option<i64>) -> Element {
    let snap = &cfg.desired;
    let projects = |status| cfg.projects.iter().filter(|p| p.status == status).count();
    rsx! {
        div { class: "stats",
            {stat("Roles", snap.roles.roles.len(), "")}
            {stat("Positions", cfg.bot.positions.len(), "")}
            if let Some(n) = queue {
                {stat("Waiting for review", n as usize, "")}
            }
            {stat("Categories", snap.categories.len(), "")}
            {stat("Channels", snap.channels().count(), "")}
            {
                stat(
                    "Projects",
                    cfg.projects.len(),
                    &format!(
                        "{} active · {} proposed · {} past",
                        projects(ProjectStatus::Active),
                        projects(ProjectStatus::Proposed),
                        projects(ProjectStatus::Past),
                    ),
                )
            }
        }
    }
}

fn stat(label: &str, n: usize, detail: &str) -> Element {
    rsx! {
        div { class: "card stat",
            b { "{n}" }
            "{label}"
            if !detail.is_empty() {
                span { class: "hint", "{detail}" }
            }
        }
    }
}

/// The join flow: who's finished, who's waiting.
fn join_card(cfg: &Config, p: &Progress, names: &Names) -> Element {
    let gate = cfg.join.as_ref().is_some_and(|j| j.gate);
    let gate_text = if gate {
        "Newcomers see most channels only after they finish."
    } else {
        "The gate is off: everyone sees everything; the flow is optional."
    };
    let waiting: Vec<String> = p.waiting.iter().map(|id| names.name(&id.to_string())).collect();
    rsx! {
        section { class: "card",
            h2 { "Join flow" }
            p { "{count(p.finished, \"member\")} finished · {p.notified} DMed about it by the bot. {gate_text}" }
            if !waiting.is_empty() {
                p { class: "note warn", "Still to finish, though the bot reminded them: {waiting.join(\", \")}" }
            }
            p { class: "hint", "Tell members who haven't been through it with /server join notify or `chipsoc-bot join notify`." }
        }
    }
}

fn link_state(cfg: &Config) -> Element {
    let text = if cfg.guild().is_some() {
        "This config is linked to the Discord server. Edits reach Discord when you apply them."
    } else {
        "This config hasn't been applied to Discord yet."
    };
    rsx! {
        section { class: "card",
            p { "{text}" }
            a { class: "button", href: "/changes", "Review and apply" }
        }
    }
}

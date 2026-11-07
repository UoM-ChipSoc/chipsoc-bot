//! The Apply page: make Discord match the config. See the plan, confirm,
//! apply it exactly as shown; or pull changes made in Discord back into the
//! config.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use axum::extract::State;
use axum::response::Html;
use axum::{Extension, Form};
use chipsoc_config::Config;
use chipsoc_discord::Rest;
use chipsoc_discord::exec::Report;
use chipsoc_engine::{Options, Pulled};
use chipsoc_model::{Action, Field, ObjectKind, OrderScope, Plan};
use dioxus::prelude::*;
use serde::Deserialize;

use crate::App;
use crate::auth::Session;
use crate::view::{self, Tab, count, csrf, done, notes, plain, trouble};

/// Longest old/new value shown in a step; post bodies can be long.
const SHOWN_VALUE: usize = 280;

/// The button that makes a plan. It's a form (POST, with the CSRF token):
/// a plan is kept on the server for applying, so following a link mustn't make one.
pub(crate) fn plan_button(s: &Session, label: &str, class: &str) -> Element {
    rsx! {
        form { method: "post", action: "/changes/plan", class: "inline",
            {csrf(s)}
            button { class: "{class}", "{label}" }
        }
    }
}

pub(crate) async fn page(Extension(s): Extension<Session>) -> Html<String> {
    view::page(Tab::Changes, &s, landing(&s))
}

pub(crate) async fn plan<R: Rest + 'static>(
    State(app): State<Arc<App<R>>>,
    Extension(s): Extension<Session>,
) -> Html<String> {
    let content = plan_section(&app, &s).await;
    view::page(Tab::Changes, &s, content)
}

/// A fresh plan, kept for `s` to apply, with its Apply form.
pub(crate) async fn plan_section<R: Rest>(app: &App<R>, s: &Session) -> Element {
    match planned(app, s).await {
        Ok(planned) => plan_view(s, &planned),
        Err(e) => rsx! {
            h1 { "Plan: what would change" }
            {trouble(&format!("Couldn't work out the plan: {e:#}"))}
        },
    }
}

/// A plan as the page shows it.
struct Planned {
    plan: Plan,
    /// Where the server keeps it for this session to apply.
    id: String,
    config: String,
    names: Names,
    drift: chipsoc_admin::Drift,
}

async fn planned<R: Rest>(app: &App<R>, s: &Session) -> Result<Planned> {
    let e = app.engine().await?;
    // A config with errors fails to plan (the page says what to fix).
    let plan = e.plan(Options::default()).await?;
    let (config, names) = (config_hash(&e.cfg), Names::of(&e.cfg));
    drop(e);
    let id = app.keep(plan.clone(), s);
    let drift = chipsoc_admin::drift(&plan);
    Ok(Planned { plan, id, config, names, drift })
}

/// What the apply form sends back: which plan the server showed (it keeps the
/// plan itself, so the form can't change it) and the config it came from.
#[derive(Deserialize)]
pub(crate) struct ApplyForm {
    plan_id: String,
    config: String,
    confirm: Option<String>,
}

pub(crate) async fn apply<R: Rest + 'static>(
    State(app): State<Arc<App<R>>>,
    Extension(s): Extension<Session>,
    Form(f): Form<ApplyForm>,
) -> Html<String> {
    tracing::info!(actor = s.actor().unwrap_or("-"), "web UI apply");
    // Spawned so closing the tab can't stop an apply halfway.
    let result = tokio::spawn(run_apply(app, f, s.clone())).await.unwrap_or_else(|e| Err(e.into()));
    let content = match result {
        Ok((report, names)) => results(&s, &report, &names),
        Err(e) => refused(&s, &e),
    };
    view::page(Tab::Changes, &s, content)
}

async fn run_apply<R: Rest>(app: Arc<App<R>>, f: ApplyForm, s: Session) -> Result<(Report, Names)> {
    if f.confirm.as_deref() != Some("yes") {
        bail!("Tick the box to confirm you've read the changes.");
    }
    let plan = app.take(&f.plan_id, &s).context("This page is out of date, or its plan was already applied.")?;
    let mut e = app.engine().await?;
    if config_hash(&e.cfg) != f.config {
        bail!("The config files changed since you looked (perhaps someone else edited them).");
    }
    let report = e.apply_by(&plan, &s.by(), &mut |_: &Action, _: &Result<(), String>| {}).await?;
    Ok((report, Names::of(&e.cfg)))
}

#[derive(Deserialize)]
pub(crate) struct AdoptForm {
    permissions: Option<String>,
    confirm: Option<String>,
}

pub(crate) async fn adopt<R: Rest + 'static>(
    State(app): State<Arc<App<R>>>,
    Extension(s): Extension<Session>,
    Form(f): Form<AdoptForm>,
) -> Html<String> {
    if f.confirm.as_deref() != Some("yes") {
        let content = rsx! {
            h1 { "Nothing was pulled" }
            {trouble("Press “Yes, copy Discord's version into the config” to confirm.")}
        };
        return view::page(Tab::Changes, &s, content);
    }
    let permissions = f.permissions.is_some();
    tracing::info!(actor = s.actor().unwrap_or("-"), permissions, "web UI pull from Discord");
    let by = s.by();
    let pull = async move { app.engine().await?.adopt_by(permissions, &by).await };
    let content = match tokio::spawn(pull).await.unwrap_or_else(|e| Err(e.into())) {
        Ok(notes) => pulled(&s, &notes),
        Err(e) => rsx! {
            h1 { "Nothing was pulled" }
            {trouble(&format!("{e:#}"))}
        },
    };
    view::page(Tab::Changes, &s, content)
}

/// Spots config edits between showing a plan and applying it.
fn config_hash(cfg: &Config) -> String {
    chipsoc_model::stable_hash(serde_json::to_string(&cfg.desired).unwrap_or_default())
}

/// What people call the things a plan names by key.
struct Names(HashMap<(ObjectKind, String), String>);

impl Names {
    fn of(cfg: &Config) -> Self {
        let snap = &cfg.desired;
        let named = |kind, key: &str, name: &str| ((kind, key.to_owned()), name.to_owned());
        let roles = snap.roles.roles.iter().map(|r| named(ObjectKind::Role, &r.key, &r.name));
        let cats = snap.categories.iter().map(|c| named(ObjectKind::Category, &c.category.key, &c.category.name));
        let chans = snap.channels().flat_map(|(c, _)| {
            [named(ObjectKind::Channel, &c.key, &c.name), named(ObjectKind::Post, &c.key, &format!("in {}", c.name))]
        });
        let projects = cfg.projects.iter().map(|p| named(ObjectKind::ProjectPost, &p.slug, &p.name));
        let threads = snap.channels().flat_map(|(c, _)| {
            let key = |t: &chipsoc_config::schema::ForumThread| chipsoc_config::schema::thread_key(&c.key, &t.key);
            c.threads.iter().map(move |t| named(ObjectKind::Thread, &key(t), &format!("{} in {}", t.title, c.name)))
        });
        let rules = snap.settings.automod.iter().map(|r| named(ObjectKind::AutoMod, &r.key, &r.name));
        let server = named(ObjectKind::Server, "server", &snap.server.guild.name);
        Self(roles.chain(cats).chain(chans).chain(projects).chain(threads).chain(rules).chain([server]).collect())
    }

    fn get<'a>(&'a self, kind: ObjectKind, key: &'a str) -> &'a str {
        self.0.get(&(kind, key.to_owned())).map_or(key, String::as_str)
    }

    fn list(&self, kind: ObjectKind, keys: &[String]) -> String {
        keys.iter().map(|k| self.get(kind, k)).collect::<Vec<_>>().join(", ")
    }
}

fn landing(s: &Session) -> Element {
    rsx! {
        h1 { "Apply changes to Discord" }
        section { class: "card",
            h2 { "Plan, then apply" }
            p {
                "Edits made here or in the config files reach Discord only when you apply them. "
                "The plan shows every change first; nothing happens until you confirm."
            }
            {plan_button(s, "See the plan", "button")}
        }
        section { class: "card",
            h2 { "History" }
            p {
                "Who changed the config, and when. The server owner can revert any change from there."
            }
            a { class: "button quiet", href: "/changes/history", "See the config's history" }
        }
        section { class: "card",
            h2 { "Pull changes made in Discord" }
            p {
                "If someone renamed things, edited topics or reordered channels in Discord's own settings, "
                "copy those changes into the config files so they're kept."
            }
            details { class: "confirm-box",
                summary { class: "button quiet danger",
                    span { class: "closed", "Pull changes made in Discord…" }
                    span { class: "opened", "Cancel" }
                }
                form { method: "post", action: "/changes/adopt",
                    {csrf(s)}
                    input { r#type: "hidden", name: "confirm", value: "yes" }
                    label { class: "check",
                        input { r#type: "checkbox", name: "permissions", value: "yes" }
                        "Also copy role permissions"
                    }
                    p { class: "hint",
                        "Where Discord and the config disagree, the config files take Discord's version: "
                        "edits made here that aren't applied yet are replaced. See the plan first to know what differs."
                    }
                    button { class: "danger", "Yes, copy Discord's version into the config" }
                }
            }
        }
    }
}

fn plan_view(s: &Session, planned: &Planned) -> Element {
    let Planned { plan, id, config, names, drift } = planned;
    let steps: Vec<&Action> = plan.actions.iter().filter(|a| a.mutates()).collect();
    let warnings: Vec<String> = plan.warnings.iter().map(ToString::to_string).collect();
    let skipped: Vec<&Action> = plan.actions.iter().filter(|a| !a.mutates()).collect();
    rsx! {
        h1 { "Plan: what would change" }
        if steps.is_empty() {
            {done("Discord already matches the config. Nothing to do.")}
        } else {
            p { "Applying makes {count(steps.len(), \"change\")} in Discord, in this order:" }
            ol { class: "steps card",
                for a in &steps {
                    li { {step(names, a)} }
                }
            }
        }
        {who_changed(drift)}
        {notes("Good to know", &warnings)}
        if !skipped.is_empty() {
            h2 { "Left alone" }
            ul { class: "plain card",
                for a in skipped {
                    li { {step(names, a)} }
                }
            }
        }
        if !steps.is_empty() {
            {apply_form(s, plan, id, config)}
        }
    }
}

/// The drift report: the same steps, by who changed what since the last apply.
fn who_changed(d: &chipsoc_admin::Drift) -> Element {
    if d.discord.is_empty() && d.person.is_empty() {
        return rsx! {};
    }
    let lines = |l: &[chipsoc_admin::Line]| l.iter().map(|l| l.text.clone()).collect::<Vec<_>>();
    rsx! {
        h2 { "Who changed what" }
        div { class: "stats",
            div { class: "card stat", b { "{d.waiting.len()}" } "Changed in the config" span { class: "hint", "Applying sends these." } }
            div { class: "card stat", b { "{d.discord.len()}" } "Changed in Discord" span { class: "hint", "Applying undoes these; pulling keeps them." } }
            div { class: "card stat", b { "{d.person.len()}" } "Needs a person" span { class: "hint", "Changed on both sides, or not known: choose by hand." } }
        }
        {notes("Changed in Discord", &lines(&d.discord))}
        {notes("Needs a person", &lines(&d.person))}
    }
}

fn apply_form(s: &Session, plan: &Plan, id: &str, config: &str) -> Element {
    let n = count(plan.changes(), "change");
    rsx! {
        form { method: "post", action: "/changes/apply", class: "card confirm",
            {csrf(s)}
            input { r#type: "hidden", name: "plan_id", value: "{id}" }
            input { r#type: "hidden", name: "config", value: "{config}" }
            label { class: "check",
                input { r#type: "checkbox", name: "confirm", value: "yes", required: true }
                "I've read the {n} above and want Discord to match."
            }
            button { "Apply {n} to Discord" }
            p { class: "hint",
                "If Discord or the config changes before you press this, nothing is applied and you'll be asked to look again."
            }
        }
    }
}

/// One step in plain words.
fn step(names: &Names, a: &Action) -> Element {
    let headline = |class, verb, kind: &ObjectKind, key| headline(class, verb, *kind, names.get(*kind, key));
    match a {
        Action::Create { kind, key } => headline("add", "Add", kind, key),
        Action::Edit { kind, key, changes, .. } => rsx! {
            {headline("edit", "Change", kind, key)}
            ul { class: "changes",
                for c in changes {
                    li {
                        "{field(c.field)}: "
                        del { class: "value", "{shown(&c.from)}" }
                        " → "
                        ins { class: "value", "{shown(&c.to)}" }
                    }
                }
            }
        },
        Action::Reorder { scope, to, .. } => rsx! {
            span { class: "verb edit", "Reorder" }
            "{reorder(names, scope, to)}"
        },
        Action::Archive { kind, key, .. } => headline("remove", "Archive", kind, key),
        Action::Delete { kind, key, .. } => headline("remove", "Delete", kind, key),
        Action::Skip { kind, key, reason } => rsx! {
            {headline("", "Skip", kind, key)}
            " ({reason})"
        },
    }
}

/// "Add role Admin"; just "Change onboarding" when the name says nothing more.
fn headline(class: &str, verb: &str, kind: ObjectKind, name: &str) -> Element {
    let kind = kind.to_string();
    rsx! {
        span { class: "verb {class}", "{verb}" }
        "{kind}"
        if name != kind {
            " "
            strong { "{name}" }
        }
    }
}

/// "channels in Main: general, projects".
fn reorder(names: &Names, scope: &OrderScope, to: &[String]) -> String {
    match scope {
        OrderScope::Roles => format!("roles: {}", names.list(ObjectKind::Role, to)),
        OrderScope::Categories => format!("categories: {}", names.list(ObjectKind::Category, to)),
        OrderScope::Channels(Some(c)) => {
            format!("channels in {}: {}", names.get(ObjectKind::Category, c), names.list(ObjectKind::Channel, to))
        }
        OrderScope::Channels(None) => format!("channels outside categories: {}", names.list(ObjectKind::Channel, to)),
    }
}

/// A field as Discord's settings call it.
fn field(f: Field) -> String {
    match f {
        Field::Hoist => "shown separately".into(),
        Field::Mentionable => "anyone can mention it".into(),
        Field::Overwrites => "permissions".into(),
        Field::Parent => "category".into(),
        Field::Nsfw => "age-restricted".into(),
        Field::Slowmode => "slow mode".into(),
        Field::Color => "colour".into(),
        Field::Content => "text".into(),
        Field::Holders => "who holds it".into(),
        Field::AfkChannel => "inactive channel".into(),
        Field::ContentFilter => "explicit media filter".into(),
        Field::Notifications => "default notifications".into(),
        other => other.to_string(),
    }
}

fn shown(value: &str) -> String {
    if value.is_empty() {
        return "(nothing)".into();
    }
    if value.chars().count() <= SHOWN_VALUE {
        return value.into();
    }
    value.chars().take(SHOWN_VALUE).chain(['…']).collect()
}

fn results(s: &Session, report: &Report, names: &Names) -> Element {
    let failed = report.failed().count();
    let total = report.outcomes.len();
    let summary = if failed == 0 {
        done(&format!("Done: {} applied.", count(total, "change")))
    } else {
        trouble(&format!(
            "{failed} of {} didn't work; the others went ahead. The reasons are below.",
            count(total, "change")
        ))
    };
    rsx! {
        h1 { "Applied" }
        {summary}
        ol { class: "steps card",
            for o in &report.outcomes {
                li {
                    if o.result.is_ok() {
                        span { class: "mark ok", "aria-hidden": "true", title: "done", "✓" }
                        span { class: "hidden", "Done: " }
                    } else {
                        span { class: "mark fail", "aria-hidden": "true", title: "failed", "✗" }
                        span { class: "hidden", "Failed: " }
                    }
                    {step(names, &o.action)}
                    if let Err(why) = &o.result {
                        p { class: "why", {plain(why)} }
                    }
                }
            }
        }
        {plan_button(s, "See the plan now", "button quiet")}
    }
}

fn refused(s: &Session, e: &anyhow::Error) -> Element {
    rsx! {
        h1 { "Nothing was applied" }
        {trouble(&format!("{e:#}"))}
        {plan_button(s, "Look again", "button")}
    }
}

fn pulled(s: &Session, results: &[Pulled]) -> Element {
    let (mut copied, mut by_hand) = (Vec::new(), Vec::new());
    for p in results {
        match p {
            Pulled::Adopted(s) => copied.push(s.clone()),
            Pulled::ByHand(s) => by_hand.push(s.clone()),
        }
    }
    rsx! {
        h1 { "Pulled from Discord" }
        if results.is_empty() {
            {done("Nothing to pull: the config already has everything changed in Discord.")}
        }
        {notes("Copied into the config files", &copied)}
        {notes("Needs doing by hand", &by_hand)}
        if !by_hand.is_empty() {
            p { class: "hint",
                "These can't be copied automatically. Change them in the config files, "
                "or apply to put Discord back the way the config says."
            }
        }
        {plan_button(s, "See the plan now", "button quiet")}
    }
}

//! The review queue: applications and suggestions waiting for Accept or
//! Reject, with their answers; accept, reject (optionally saying why), make
//! a lead; and the decisions made so far. Everything goes through `chipsoc_admin`, as the
//! review card's buttons in Discord do.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::response::{Html, IntoResponse, Response};
use axum::{Extension, Form};
use chipsoc_admin::{Past, Pending};
use chipsoc_discord::Rest;
use dioxus::prelude::*;

use crate::App;
use crate::auth::Session;
use crate::form::{self, Fields, said_back, spawned, when};
use crate::people::Names;
use crate::view::{self, Tab, csrf, empty};

const HERE: &str = "/queue";

pub(crate) async fn page<R: Rest + Clone + 'static>(
    State(app): State<Arc<App<R>>>,
    Extension(s): Extension<Session>,
) -> Response {
    let loaded = async {
        let desk = app.desk(&s).await?;
        let on = chipsoc_admin::Host::cfg(&desk).bot.review.is_some();
        let (pending, past) = (chipsoc_admin::pending(&desk).await?, chipsoc_admin::queue_history(&desk, 50).await?);
        let mut ids: Vec<&str> = pending.iter().map(|p| p.user.as_str()).collect();
        for p in &past {
            ids.extend([Some(p.user.as_str()), p.reviewed_by.as_deref()].into_iter().flatten());
        }
        let names = Names::of(&desk, &ids).await;
        anyhow::Ok((pending, past, on, names))
    };
    match loaded.await {
        Ok((pending, past, on, names)) => render(&s, &pending, &past, on, &names).into_response(),
        Err(e) => view::broken(Tab::Queue, &s, &e).into_response(),
    }
}

pub(crate) async fn act<R: Rest + Clone + 'static>(
    State(app): State<Arc<App<R>>>,
    Extension(s): Extension<Session>,
    Path(action): Path<String>,
    Form(f): Form<Vec<(String, String)>>,
) -> Response {
    let f = Fields(f);
    let Ok(id) = f.get("id").parse::<i64>() else { return said_back(HERE, Err("Which application?".into())) };
    let reason = f.opt("reason");
    // The version the page showed: a partner's entry changed since isn't accepted unseen.
    let seen = f.opt("seen");
    let done = match action.as_str() {
        "accept" | "reject" => {
            let accept = action == "accept";
            spawned!(app, s, |d| {
                let r = chipsoc_admin::decide_seen(d, id, accept, reason.as_deref(), seen.as_deref(), &d.actor).await;
                r.and_then(said)
            })
        }
        "lead" => spawned!(app, s, |d| {
            chipsoc_admin::make_lead(d, id, &d.actor)
                .await
                .map(|t| chipsoc_admin::Said::new().text("Made a lead.").then(t))
        }),
        _ => Err("Unknown action.".to_owned()),
    };
    said_back(HERE, done)
}

/// What a decision did, for the flash. A failed Accept is no decision: it's
/// still waiting, so it's said as a problem.
fn said(d: chipsoc_admin::Decided) -> Result<chipsoc_admin::Said, chipsoc_admin::Refusal> {
    use chipsoc_admin::Said;
    if d.pending {
        return Err(Said::new().text("Accept didn't go through, so it's still waiting.").then(d.outcome).into());
    }
    let head = match (d.accepted, d.told) {
        (true, true) => "Accepted; they've been told by DM.",
        (true, false) => "Accepted.",
        (false, true) => "Rejected; they've been told by DM.",
        (false, false) => "Rejected.",
    };
    Ok(Said::new().text(head).then(d.outcome))
}

fn render(s: &Session, pending: &[Pending], past: &[Past], on: bool, names: &Names) -> Html<String> {
    view::page(
        Tab::Queue,
        s,
        rsx! {
            h1 { "Review queue" }
            p { class: "hint",
                "Applications, project ideas, resources, feed and event suggestions, and partners' shares waiting for Accept or Reject. Deciding here also updates the card in Discord and tells by DM whoever here sent it (a partner isn't told)."
            }
            h2 { "Waiting ({pending.len()})" }
            if !on {
                p { class: "note bad", role: "alert",
                    "Applications and suggestions are off. Ask whoever runs the bot to turn them on (bot.toml, [review])."
                }
            } else if pending.is_empty() {
                {empty("Nothing waiting. New ones appear here and in the review channel.")}
            }
            for p in pending {
                {card(s, p, names)}
            }
            h2 { "Decided" }
            if past.is_empty() {
                {empty("No decisions yet.")}
            } else {
                {history(s, past, names)}
            }
        },
    )
}

fn card(s: &Session, p: &Pending, names: &Names) -> Element {
    let id = p.id.to_string();
    let kind = chipsoc_store::AppKind::label_of(&p.kind);
    let form = format!("app-{}", p.id);
    let (accepting, rejecting, reason) = chipsoc_admin::says_of(&p.kind);
    rsx! {
        section { class: "card", id: "{form}",
            h3 {
                "{p.what}"
                span { class: "tag", "{kind}" }
                if p.as_lead {
                    span { class: "tag", "would like to lead" }
                }
            }
            p { class: "hint", "From " {names.who(&p.user)} ", {when(p.created_at)}." }
            if !p.answers.is_empty() {
                dl { class: "answers",
                    for a in &p.answers {
                        dt { "{a.question}" }
                        dd { if a.answer.is_empty() { "(left blank)" } else { "{a.answer}" } }
                    }
                }
            }
            div { class: "actions",
                {form::button(s, "/queue/accept", &[("id", &id), ("seen", &p.version)], "Accept", "")}
                details { class: "confirm-box",
                    summary { class: "button quiet danger",
                        span { class: "closed", "Reject…" }
                        span { class: "opened", "Cancel" }
                    }
                    form { method: "post", action: "/queue/reject",
                        {csrf(s)}
                        {form::hidden(&[("id", &id)])}
                        label { r#for: "{form}-reason", "{reason}" }
                        textarea { id: "{form}-reason", name: "reason", rows: "3" }
                        p { class: "hint", "{rejecting}" }
                        button { class: "danger", "Reject" }
                    }
                }
            }
            p { class: "hint", "{accepting}" }
        }
    }
}

fn history(s: &Session, past: &[Past], names: &Names) -> Element {
    view::table(
        &["When", "For", "Who", "Decision", "By", "Reason", ""],
        rsx! {
            for p in past {
                tr {
                    td { "{p.reviewed_at.map(when).unwrap_or_default()}" }
                    td { class: "wrap", "{p.what}" }
                    td { {names.who(&p.user)} }
                    td { class: if p.status == "accepted" { "good-ink" } else { "bad-ink" }, "{decision(&p.status)}" }
                    td { {names.who(p.reviewed_by.as_deref().unwrap_or_default())} }
                    td { class: "wrap", "{p.reason.clone().unwrap_or_default()}" }
                    td {
                        if p.kind == "project" && p.status == "accepted" && p.as_lead {
                            {form::button(s, "/queue/lead", &[("id", &p.id.to_string())], "Make lead", "quiet small")}
                        }
                    }
                }
            }
        },
    )
}

/// `accepted` as "Accepted".
fn decision(status: &str) -> String {
    let mut c = status.chars();
    c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use chipsoc_admin::Decided;

    #[test]
    fn a_failed_accept_is_said_as_a_problem_not_a_rejection() {
        let failed = Decided {
            id: 1,
            accepted: false,
            outcome: " Didn't give **Treasurer**.".into(),
            lead: false,
            reason: None,
            pending: true,
            told: false,
        };
        let said = super::said(failed).unwrap_err();
        assert!(said.starts_with("Accept didn't go through, so it's still waiting.") && !said.contains("Rejected"));
        assert_eq!(
            said.plain(None),
            "Accept didn't go through, so it's still waiting. Didn't give Treasurer.",
            "plain text"
        );
        let mut rejected = Decided {
            id: 1,
            accepted: false,
            outcome: Default::default(),
            lead: false,
            reason: None,
            pending: false,
            told: false,
        };
        assert_eq!(super::said(rejected.clone()).unwrap(), "Rejected.", "a partner's: nobody told");
        rejected.told = true;
        assert_eq!(super::said(rejected).unwrap(), "Rejected; they've been told by DM.");
    }
}

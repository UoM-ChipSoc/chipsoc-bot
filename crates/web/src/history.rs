//! The config's history: who changed what and when (or what one admin did
//! this week), with Revert and Rewind for the server owner. Going back is a
//! new commit, shown as a plan to apply like any other change.

use std::sync::Arc;

use axum::extract::{Query, State};
use axum::response::{IntoResponse, Response};
use axum::{Extension, Form};
use chipsoc_config::Snowflake;
use chipsoc_config::git::Undo;
use chipsoc_discord::Rest;
use dioxus::prelude::*;
use serde::Deserialize;

use crate::App;
use crate::auth::Session;
use crate::form::when;
use crate::view::{self, Tab, csrf, empty};

/// Changes listed.
const SHOWN: usize = 100;

#[derive(Deserialize)]
pub(crate) struct Whose {
    user: Option<Snowflake>,
}

pub(crate) async fn page<R: Rest + Clone + 'static>(
    State(app): State<Arc<App<R>>>,
    Extension(s): Extension<Session>,
    Query(q): Query<Whose>,
) -> Response {
    let loaded = async {
        let desk = app.desk(&s).await?;
        let done = chipsoc_admin::history_of(&desk, q.user, SHOWN).await?;
        anyhow::Ok((done, chipsoc_admin::owner_id(&desk).await.ok()))
    };
    let (done, owner) = match loaded.await {
        Ok(found) => found,
        Err(e) => return view::broken(Tab::Changes, &s, &e).into_response(),
    };
    let owns = s.user.as_ref().is_some_and(|u| Some(u.id) == owner);
    let content = rsx! {
        h1 {
            match q.user {
                Some(u) => {
                    let name = done.iter().find(|d| d.who_id == Some(u)).map_or_else(|| u.to_string(), |d| d.who.clone());
                    rsx! { "What {name} did this week" }
                }
                None => rsx! { "Config history" },
            }
        }
        p { class: "hint",
            "Every change to the config, newest first, with who made it (from the bot's signature on the commit, "
            "else git's author). "
            if q.user.is_some() {
                a { href: "/changes/history", "Show everyone's changes." }
            }
        }
        if owns {
            p { class: "hint",
                "Revert undoes one change; Rewind puts the whole config back as it was right after it. Either is "
                "saved as a new change (nothing is rewritten), then shown as a plan to apply: what it removes in "
                "Discord is archived, not deleted."
            }
        } else {
            p { class: "hint", "Only the server owner can revert or rewind changes." }
        }
        if done.is_empty() {
            {empty("Nothing here yet.")}
        } else {
            {view::table(&["When", "Who", "Change", ""], rsx! {
                for d in &done {
                    tr {
                        td { "{when(d.at)}" }
                        td {
                            match d.who_id {
                                Some(id) if q.user.is_none() => rsx! { a { href: "/changes/history?user={id}", "{d.who}" } },
                                _ => rsx! { "{d.who}" },
                            }
                        }
                        td { class: "wrap",
                            if let Some(c) = &d.commit {
                                code { "{c}" }
                                " "
                            }
                            "{d.what}"
                        }
                        td {
                            if let (true, Some(c), true) = (owns, &d.commit, d.revertible) {
                                {undo_button(&s, c, &d.what, Undo::Revert)}
                                {undo_button(&s, c, &d.what, Undo::Rewind)}
                            }
                        }
                    }
                }
            })}
        }
    };
    view::page(Tab::Changes, &s, content).into_response()
}

/// Revert or Rewind, asking first (the question every front end asks).
fn undo_button(s: &Session, commit: &str, subject: &str, how: Undo) -> Element {
    let (value, label, yes) = match how {
        Undo::Revert => ("revert", "Revert…", "Yes, revert it"),
        Undo::Rewind => ("rewind", "Rewind to here…", "Yes, rewind"),
    };
    let question = chipsoc_admin::undo_question(commit, Some(subject), how).plain(None);
    rsx! {
        details { class: "confirm-box",
            summary { class: "button quiet",
                span { class: "closed", "{label}" }
                span { class: "opened", "Cancel" }
            }
            form { method: "post", action: "/changes/undo", class: "inline",
                {csrf(s)}
                input { r#type: "hidden", name: "commit", value: "{commit}" }
                input { r#type: "hidden", name: "how", value: "{value}" }
                p { class: "hint", "{question}" }
                button { class: "danger", "{yes}" }
            }
        }
    }
}

#[derive(Deserialize)]
pub(crate) struct UndoForm {
    commit: String,
    how: Undo,
}

/// `POST /changes/undo`: go back on a change, then send them to the Apply
/// page saying what happened (so reloading it doesn't go back again).
pub(crate) async fn undo<R: Rest + Clone + 'static>(
    State(app): State<Arc<App<R>>>,
    Extension(s): Extension<Session>,
    Form(f): Form<UndoForm>,
) -> Response {
    let (a, who) = (app.clone(), s.clone());
    // Spawned so closing the tab can't stop it halfway.
    let run = async move {
        let desk = a.desk(&who).await?;
        anyhow::Ok(chipsoc_admin::undo(&desk, &f.commit, f.how, &desk.actor).await)
    };
    let made = tokio::spawn(run).await.unwrap_or_else(|e| Err(e.into())).unwrap_or_else(|e| Err(format!("{e:#}")));
    let head = chipsoc_admin::undo_said(&made).plain(None);
    match made {
        Ok(Some(_)) => crate::flash::redirect("/changes", &format!("{head} Press **See the plan** to apply it."), true),
        Ok(None) => crate::flash::redirect("/changes/history", &head, true),
        Err(_) => crate::flash::redirect("/changes/history", &head, false),
    }
}

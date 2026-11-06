//! The audit log: who did what, newest first, from every front end.

use std::sync::Arc;

use axum::Extension;
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use chipsoc_discord::Rest;
use dioxus::prelude::*;

use crate::App;
use crate::auth::Session;
use crate::form::when;
use crate::people::Names;
use crate::view::{self, Tab, empty};

/// Entries shown.
const SHOWN: i64 = 200;

pub(crate) async fn page<R: Rest + Clone + 'static>(
    State(app): State<Arc<App<R>>>,
    Extension(s): Extension<Session>,
) -> Response {
    let loaded = async {
        let desk = app.desk(&s).await?;
        let entries = chipsoc_admin::activity(&desk, SHOWN).await?;
        let ids: Vec<&str> = entries
            .iter()
            .flat_map(|e| [e.actor.as_deref(), e.target.as_deref(), e.detail.as_deref()].into_iter().flatten())
            .collect();
        let names = Names::of(&desk, &ids).await;
        anyhow::Ok((entries, names))
    };
    let (entries, names) = match loaded.await {
        Ok(e) => e,
        Err(e) => return view::broken(Tab::Activity, &s, &e).into_response(),
    };
    let content = rsx! {
        h1 { "Activity" }
        p { class: "hint",
            "Decisions and changes made through the bot, this site, the terminal and Discord buttons, newest first. "
            "Config edits are also in the config repo's history."
        }
        if entries.is_empty() {
            {empty("Nothing yet. Decisions on applications, feeds and the like show up here.")}
        } else {
            {view::table(&["When", "Who", "Did", "To", "Detail"], rsx! {
                for e in &entries {
                    tr {
                        td { "{when(e.ts)}" }
                        td { {names.who(e.actor.as_deref().unwrap_or_default())} }
                        td { "{did(&e.action)}" }
                        td { class: "wrap", {names.who(e.target.as_deref().unwrap_or_default())} }
                        td { class: "wrap", {names.who(e.detail.as_deref().unwrap_or_default())} }
                    }
                }
            })}
        }
    };
    view::page(Tab::Activity, &s, content).into_response()
}

/// An audit-log action as a sentence (see [`chipsoc_admin::fmt::did`]).
fn did(action: &str) -> String {
    chipsoc_admin::fmt::did(action)
}

#[cfg(test)]
mod tests {
    #[test]
    fn actions_read_as_sentences() {
        assert_eq!(super::did("accepted"), "Accepted an application or suggestion");
        assert_eq!(super::did("some_new_thing"), "Some new thing");
    }
}

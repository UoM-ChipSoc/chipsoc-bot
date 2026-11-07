//! Feeds the bot follows (new items are posted in the feeds channel): add,
//! pause or resume, stop following.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::response::{Html, IntoResponse, Response};
use axum::{Extension, Form};
use chipsoc_admin::FeedInfo;
use chipsoc_discord::Rest;
use dioxus::prelude::*;

use crate::App;
use crate::auth::Session;
use crate::form::{self, Fields, Shown, confirm, input_of, said_back, shown, spawned, when, wrong};
use crate::view::{self, Tab, csrf, empty};

const HERE: &str = "/feeds";

pub(crate) async fn page<R: Rest + Clone + 'static>(
    State(app): State<Arc<App<R>>>,
    Extension(s): Extension<Session>,
) -> Response {
    view(&app, &s, &Shown::none()).await
}

async fn view<R: Rest + Clone>(app: &App<R>, s: &Session, sh: &Shown) -> Response {
    let loaded = async {
        let desk = app.desk(s).await?;
        let on = chipsoc_admin::Host::cfg(&desk).bot.feeds.as_ref().map(|f| f.channel.clone());
        anyhow::Ok((chipsoc_admin::feeds(&desk).await?, on))
    };
    match loaded.await {
        Ok((feeds, on)) => shown(sh, render(s, &feeds, on.as_deref(), sh)),
        Err(e) => view::broken(Tab::Feeds, s, &e).into_response(),
    }
}

pub(crate) async fn act<R: Rest + Clone + 'static>(
    State(app): State<Arc<App<R>>>,
    Extension(s): Extension<Session>,
    Path(action): Path<String>,
    Form(f): Form<Vec<(String, String)>>,
) -> Response {
    let f = Fields(f);
    if action == "add" {
        let url = f.get("url").to_owned();
        if !(url.starts_with("https://") || url.starts_with("http://")) {
            return view(&app, &s, &wrong("feed-add", &f, "url", "Give the feed's web address, starting https://."))
                .await;
        }
        let done = spawned!(app, s, |d| chipsoc_admin::add_feed(d, &url, &d.actor).await);
        return match done {
            Ok(title) => said_back(HERE, Ok(format!("Now following {title}."))),
            Err(why) => view(&app, &s, &wrong("feed-add", &f, "url", &why)).await,
        };
    }
    let Ok(id) = f.get("id").parse::<i64>() else { return said_back(HERE, Err("Which feed?".into())) };
    let done = match action.as_str() {
        "pause" | "resume" => {
            let paused = action == "pause";
            spawned!(app, s, |d| match chipsoc_admin::pause_feed(d, id, paused, &d.actor).await {
                Ok(true) => Ok(if paused { "Paused." } else { "Resumed." }.to_owned()),
                Ok(false) => Err("No such feed.".into()),
                Err(e) => Err(format!("{e:#}")),
            })
        }
        "remove" if !f.confirmed() => Err(crate::form::CONFIRM_FIRST.into()),
        "remove" => spawned!(app, s, |d| match chipsoc_admin::remove_feed(d, id, &d.actor).await {
            Ok(true) => Ok("Stopped following it.".to_owned()),
            Ok(false) => Err("No such feed.".into()),
            Err(e) => Err(format!("{e:#}")),
        }),
        _ => Err("Unknown action.".into()),
    };
    said_back(HERE, done)
}

fn render(s: &Session, feeds: &[FeedInfo], on: Option<&str>, sh: &Shown) -> Html<String> {
    let form = "feed-add";
    view::page(
        Tab::Feeds,
        s,
        rsx! {
            h1 { "Feeds" }
            if let Some(ch) = on {
                p { class: "hint", "New items are posted in #{ch}. Members can suggest feeds with /suggest-feed." }
            } else {
                p { class: "note bad", role: "alert",
                    "Feeds are off. Ask whoever runs the bot to turn them on (bot.toml, [feeds])."
                }
            }
            if feeds.is_empty() && on.is_none() {
                {empty("Not following any feeds.")}
            } else if feeds.is_empty() {
                {empty("Not following any feeds yet. Follow one below, or accept a suggestion in the review queue.")}
            } else {
                {view::table(&["Feed", "Posts in", "Added", "", ""], rsx! {
                    for f in feeds {
                        tr { class: if f.paused { "muted" },
                            td { class: "wrap",
                                if let Some(t) = f.title.as_deref().filter(|t| !t.is_empty()) {
                                    strong { "{t}" }
                                    br {}
                                }
                                "{f.url}"
                                if f.paused { span { class: "tag", "paused" } }
                            }
                            td { "#{f.channel}" }
                            td { "{when(f.added_at)}" }
                            td {
                                if f.paused {
                                    {form::button(s, "/feeds/resume", &[("id", &f.id.to_string())], "Resume", "quiet small")}
                                } else {
                                    {form::button(s, "/feeds/pause", &[("id", &f.id.to_string())], "Pause", "quiet small")}
                                }
                            }
                            td {
                                {confirm(s, "/feeds/remove", &[("id", &f.id.to_string())], "Remove", "Yes, stop following", &format!("Stop following {}? Nothing already posted is removed.", chipsoc_admin::feeds::shown(f)))}
                            }
                        }
                    }
                })}
            }
            if on.is_some() {
                h2 { "Follow a feed" }
                form { method: "post", action: "/feeds/add", class: "card", id: "{form}",
                    {csrf(s)}
                    {sh.problems(form)}
                    {input_of(sh, form, "url", "Feed or website address", "", "An RSS or Atom feed, or a website that has one (it's found from the page). Items already there aren't posted.", "url")}
                    button { "Follow" }
                }
            }
        },
    )
}

//! Resources members share (posts in the resources forum): share a link,
//! take one down (closed and locked, never deleted); and the ideas forum.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::response::{Html, IntoResponse, Response};
use axum::{Extension, Form};
use chipsoc_admin::{NewResource, Post};
use chipsoc_config::Snowflake;
use chipsoc_discord::Rest;
use dioxus::prelude::*;

use crate::App;
use crate::auth::Session;
use crate::form::{Fields, Shown, area, confirm, input_of, said_back, shown, spawned, text, wrong};
use crate::view::{self, Tab, csrf, empty};

const HERE: &str = "/resources";

pub(crate) async fn page<R: Rest + Clone + 'static>(
    State(app): State<Arc<App<R>>>,
    Extension(s): Extension<Session>,
) -> Response {
    view(&app, &s, &Shown::none()).await
}

/// A forum's posts, or why there aren't any to show.
type Listed = Result<Vec<Post>, String>;

async fn view<R: Rest + Clone>(app: &App<R>, s: &Session, sh: &Shown) -> Response {
    let desk = match app.desk(s).await {
        Ok(d) => d,
        Err(e) => return view::broken(Tab::Resources, s, &e).into_response(),
    };
    let cfg = chipsoc_admin::Host::cfg(&desk);
    let resources: Listed = match &cfg.bot.resources {
        Some(_) => chipsoc_admin::resources(&desk, 30).await.map_err(|e| format!("{e:#}")),
        None => Err("Resources are off. Ask whoever runs the bot to turn them on (bot.toml, [resources]).".into()),
    };
    let ideas: Listed = match &cfg.bot.ideas {
        Some(_) => chipsoc_admin::ideas(&desk, 30).await.map_err(|e| format!("{e:#}")),
        None => Err("The ideas forum is off. Ask whoever runs the bot to turn it on (bot.toml, [ideas]).".into()),
    };
    let tags = cfg
        .bot
        .resources
        .as_ref()
        .and_then(|r| cfg.desired.channel(&r.forum))
        .map(|c| c.tags.clone())
        .unwrap_or_default();
    shown(sh, render(s, &resources, &ideas, &tags, sh))
}

pub(crate) async fn act<R: Rest + Clone + 'static>(
    State(app): State<Arc<App<R>>>,
    Extension(s): Extension<Session>,
    Path(action): Path<String>,
    Form(f): Form<Vec<(String, String)>>,
) -> Response {
    let f = Fields(f);
    match action.as_str() {
        "add" => {
            let form = "res-add";
            if f.get("title").is_empty() {
                return view(&app, &s, &wrong(form, &f, "title", "Give it a title.")).await;
            }
            if !f.get("url").starts_with("https://") && !f.get("url").starts_with("http://") {
                return view(&app, &s, &wrong(form, &f, "url", "Give a web address, starting https://.")).await;
            }
            let r = NewResource {
                title: f.get("title").to_owned(),
                url: f.get("url").to_owned(),
                description: f.raw("description").trim().to_owned(),
                tags: f.list("tags"),
            };
            let done = spawned!(app, s, |d| chipsoc_admin::post_resource(d, &r, &d.actor)
                .await
                .map(|p| format!("Shared {}.", p.title))
                .map_err(|e| format!("{e:#}")));
            match done {
                Ok(t) => said_back(HERE, Ok(t)),
                Err(why) => view(&app, &s, &crate::form::refuse(form, &f, &why)).await,
            }
        }
        "archive" => {
            let Ok(id) = f.get("id").parse::<Snowflake>() else { return said_back(HERE, Err("Which post?".into())) };
            if !f.confirmed() {
                return said_back(HERE, Err(crate::form::CONFIRM_FIRST.into()));
            }
            let done = spawned!(app, s, |d| match chipsoc_admin::archive_resource(d, id, &d.actor).await {
                Ok(true) => Ok("Archived: closed and locked, still there to read.".to_owned()),
                Ok(false) => Err("That isn't a post in the resources forum.".into()),
                Err(e) => Err(format!("{e:#}")),
            });
            said_back(HERE, done)
        }
        _ => said_back(HERE, Err("Unknown action.".into())),
    }
}

fn render(s: &Session, resources: &Listed, ideas: &Listed, tags: &[String], sh: &Shown) -> Html<String> {
    let form = "res-add";
    let tag_hint = if tags.is_empty() { String::new() } else { format!("The forum's tags: {}.", tags.join(", ")) };
    view::page(
        Tab::Resources,
        s,
        rsx! {
            h1 { "Resources" }
            match resources {
                Err(why) => rsx! { p { class: "note bad", role: "alert", {view::plain(why)} } },
                Ok(posts) => rsx! {
                    if posts.is_empty() {
                        {empty("Nothing shared yet. Share the first link below.")}
                    }
                    {posts_list(s, posts, true)}
                    h2 { "Share a link" }
                    form { method: "post", action: "/resources/add", class: "card", id: "{form}",
                        {csrf(s)}
                        {sh.problems(form)}
                        {text(sh, form, "title", "Title", "", "")}
                        {input_of(sh, form, "url", "Address", "", "", "url")}
                        {area(sh, form, "description", "Description (optional)", "", 3, "")}
                        {text(sh, form, "tags", "Tags (optional)", "", &format!("Separated by commas. {tag_hint}"))}
                        button { "Share" }
                    }
                },
            }
            h2 { "Project ideas" }
            p { class: "hint", "Ideas accepted from the review queue, as posted in the ideas forum." }
            match ideas {
                Err(why) => rsx! { p { class: "muted", {view::plain(why)} } },
                Ok(posts) if posts.is_empty() => empty("No project ideas yet. Members propose them with /propose-project; accepted ones are posted here."),
                Ok(posts) => posts_list(s, posts, false),
            }
        },
    )
}

fn posts_list(s: &Session, posts: &[Post], can_archive: bool) -> Element {
    rsx! {
        ul { class: "plain card",
            for p in posts {
                li { class: "item",
                    div {
                        a { href: "{p.link}", rel: "noreferrer", "{p.title}" }
                        for t in &p.tags {
                            span { class: "tag", "{t}" }
                        }
                        if p.archived {
                            span { class: "tag", "archived" }
                        }
                        if !p.content.is_empty() {
                            div { class: "hint clip", "{p.content}" }
                        }
                    }
                    if can_archive && !p.archived {
                        {confirm(s, "/resources/archive", &[("id", &p.id.to_string())], "Archive", "Yes, archive it", "Archiving closes and locks the post; it stays readable in Discord and is never deleted.")}
                    }
                }
            }
        }
    }
}

//! The page frame and small pieces every page uses.

use axum::response::Html;
use dioxus::prelude::*;

use crate::auth::Session;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tab {
    Overview,
    Roles,
    Channels,
    Projects,
    Positions,
    Queue,
    Members,
    Feeds,
    Resources,
    Onboarding,
    Activity,
    /// Meetings recorded with `/record`.
    Recordings,
    /// Plan and apply: make Discord match the config.
    Changes,
}

impl Tab {
    /// In the order of the usual jobs: the queue first, the server's setup last.
    const ALL: [Self; 13] = [
        Self::Overview,
        Self::Queue,
        Self::Members,
        Self::Projects,
        Self::Positions,
        Self::Feeds,
        Self::Resources,
        Self::Onboarding,
        Self::Channels,
        Self::Roles,
        Self::Recordings,
        Self::Activity,
        Self::Changes,
    ];

    pub(crate) fn href(self) -> &'static str {
        match self {
            Self::Overview => "/",
            Self::Roles => "/roles",
            Self::Channels => "/channels",
            Self::Projects => "/projects",
            Self::Positions => "/positions",
            Self::Queue => "/queue",
            Self::Members => "/members",
            Self::Feeds => "/feeds",
            Self::Resources => "/resources",
            Self::Onboarding => "/onboarding",
            Self::Activity => "/activity",
            Self::Recordings => "/recordings",
            Self::Changes => "/changes",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Overview => "Overview",
            Self::Roles => "Roles",
            Self::Channels => "Channels",
            Self::Projects => "Projects",
            Self::Positions => "Positions",
            Self::Queue => "Queue",
            Self::Members => "Members",
            Self::Feeds => "Feeds",
            Self::Resources => "Resources",
            Self::Onboarding => "Onboarding",
            Self::Activity => "Activity",
            Self::Recordings => "Recordings",
            Self::Changes => "Apply",
        }
    }
}

/// A full page with the navigation bar, `tab` highlighted.
pub(crate) fn page(tab: Tab, s: &Session, content: Element) -> Html<String> {
    frame(Some(tab), tab.label(), tab.href(), s, content)
}

/// A full page with the navigation bar; `here` is where choosing a theme comes back to.
pub(crate) fn frame(
    tab: Option<Tab>,
    title: &'static str,
    here: &'static str,
    s: &Session,
    content: Element,
) -> Html<String> {
    let theme = |mode: &str| format!("/theme/{mode}?back={here}");
    let body = dioxus_ssr::render_element(rsx! {
        header {
            nav { "aria-label": "Sections",
                div { class: "tabs",
                    for t in Tab::ALL {
                        a { href: t.href(), "aria-current": if Some(t) == tab { "page" }, "{t.label()}" }
                    }
                }
                if s.signed_in {
                    div { class: "who",
                        match &s.user {
                            Some(u) => rsx! {
                                a { href: "/account", title: "Your sign-ins: revoke any you don't recognise", "{u.name} · Sign-ins" }
                            },
                            None => rsx! {
                                span { title: "Signed in with the shared code", "Shared code" }
                            },
                        }
                        form { method: "post", action: "/logout",
                            {csrf(s)}
                            button { class: "link", "Sign out" }
                        }
                    }
                }
            }
        }
        main { {content} }
        footer {
            nav { class: "themes", "aria-label": "Theme",
                "Theme:"
                a { class: "theme-auto", href: theme("auto"), "Auto" }
                a { class: "theme-light", href: theme("light"), "Light" }
                a { class: "theme-dark", href: theme("dark"), "Dark" }
            }
        }
    });
    document(title, &body)
}

/// A page without navigation, for signing in.
pub(crate) fn bare(title: &'static str, content: Element) -> Html<String> {
    document(title, &dioxus_ssr::render_element(rsx! { main { class: "narrow", {content} } }))
}

/// What the tab title says after the page's name until [`name_server`] puts the server's there.
const TITLE_TAIL: &str = " · Server settings</title>";

fn document(title: &'static str, body: &str) -> Html<String> {
    Html(format!(
        "<!DOCTYPE html><html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <title>{title}{TITLE_TAIL}<link rel=\"stylesheet\" href=\"/style.css\">\
         </head><body>{body}</body></html>"
    ))
}

/// Pages' tab titles name the server: "Queue · Test", not "Queue · Server settings".
pub(crate) async fn name_server<R: chipsoc_discord::Rest + 'static>(
    axum::extract::State(app): axum::extract::State<std::sync::Arc<crate::App<R>>>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use axum::body::{Body, to_bytes};
    let res = next.run(req).await;
    let is_page = res
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("text/html"));
    let Some(name) = app.config().await.ok().map(|c| c.desired.server.guild.name.clone()) else { return res };
    if !is_page || name.trim().is_empty() {
        return res;
    }
    let (mut parts, body) = res.into_parts();
    let Ok(bytes) = to_bytes(body, 4 * 1024 * 1024).await else {
        return axum::response::Response::from_parts(parts, Body::empty());
    };
    let tail = format!(" · {}</title>", escape(&name));
    let page = String::from_utf8_lossy(&bytes).replacen(TITLE_TAIL, &tail, 1);
    parts.headers.remove(axum::http::header::CONTENT_LENGTH);
    axum::response::Response::from_parts(parts, Body::from(page))
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// The hidden field every POST form needs.
pub(crate) fn csrf(s: &Session) -> Element {
    rsx! { input { r#type: "hidden", name: "csrf", value: "{s.csrf}" } }
}

/// Text from the engine, with its `backticked` names shown as code.
pub(crate) fn plain(text: &str) -> Element {
    rsx! {
        for (i, part) in text.split('`').enumerate() {
            if i % 2 == 1 {
                code { "{part}" }
            } else {
                "{part}"
            }
        }
    }
}

pub(crate) fn done(text: &str) -> Element {
    rsx! { p { class: "note good", role: "status", {plain(text)} } }
}

pub(crate) fn trouble(text: &str) -> Element {
    rsx! { p { class: "note bad", role: "alert", {plain(text)} } }
}

/// A titled list of notes, or nothing when empty.
pub(crate) fn notes(title: &str, items: &[String]) -> Element {
    if items.is_empty() {
        return rsx! {};
    }
    rsx! {
        h2 { "{title}" }
        ul { class: "plain card",
            for item in items {
                li { {plain(item)} }
            }
        }
    }
}

/// "1 change", "3 changes".
pub(crate) fn count(n: usize, noun: &str) -> String {
    if n == 1 { format!("1 {noun}") } else { format!("{n} {noun}s") }
}

/// A table that scrolls sideways on narrow screens rather than squashing.
/// An empty heading is a column of buttons ("Actions" to screen readers).
pub(crate) fn table(head: &[&str], rows: Element) -> Element {
    rsx! {
        div { class: "table-wrap",
            table {
                thead {
                    tr {
                        for h in head {
                            if h.is_empty() {
                                th { scope: "col", span { class: "hidden", "Actions" } }
                            } else {
                                th { scope: "col", "{h}" }
                            }
                        }
                    }
                }
                tbody { {rows} }
            }
        }
    }
}

/// An empty state: what's missing and what to do about it.
pub(crate) fn empty(text: &str) -> Element {
    rsx! { p { class: "empty", {plain(text)} } }
}

/// The page for `tab` saying why it can't be shown.
pub(crate) fn broken(tab: Tab, s: &Session, e: &anyhow::Error) -> Html<String> {
    page(tab, s, trouble(&format!("{e:#}")))
}

/// A permission as Discord's settings name it: `MANAGE_GUILD` as "Manage Server".
pub(crate) fn permission(name: &str) -> String {
    let label = match name {
        "CREATE_INSTANT_INVITE" => "Create Invite",
        "MANAGE_GUILD" => "Manage Server",
        "STREAM" => "Video",
        "VIEW_CHANNEL" => "View Channels",
        "SEND_TTS_MESSAGES" => "Send Text-to-Speech Messages",
        "MENTION_EVERYONE" => "Mention @everyone, @here and All Roles",
        "USE_EXTERNAL_EMOJIS" => "Use External Emoji",
        "VIEW_GUILD_INSIGHTS" => "View Server Insights",
        "USE_VAD" => "Use Voice Activity",
        "MANAGE_GUILD_EXPRESSIONS" => "Manage Expressions",
        "CREATE_GUILD_EXPRESSIONS" => "Create Expressions",
        "MANAGE_THREADS" => "Manage Threads and Posts",
        "SEND_MESSAGES_IN_THREADS" => "Send Messages in Threads and Posts",
        "USE_EMBEDDED_ACTIVITIES" => "Use Activities",
        "MODERATE_MEMBERS" => "Timeout Members",
        "VIEW_CREATOR_MONETIZATION_ANALYTICS" => "View Server Subscription Insights",
        "SEND_POLLS" => "Create Polls",
        // The rest read right as words: KICK_MEMBERS is "Kick Members".
        _ => "",
    };
    if !label.is_empty() {
        return label.to_owned();
    }
    let small = ["in", "to", "and", "of"];
    let words: Vec<String> = name
        .to_lowercase()
        .split('_')
        .map(|w| {
            if small.contains(&w) {
                return w.to_owned();
            }
            let mut c = w.chars();
            c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default()
        })
        .collect();
    words.join(" ")
}

/// Every permission, Administrator first (it gives all the others).
pub(crate) fn permissions_in_order() -> Vec<&'static str> {
    let mut all = chipsoc_model::perms::all();
    all.sort_by_key(|p| *p != "ADMINISTRATOR");
    all
}

/// What to know before ticking it, for the few that need saying.
pub(crate) fn permission_warning(name: &str) -> Option<&'static str> {
    (name == "ADMINISTRATOR").then_some("Gives every permission; avoid.")
}

#[cfg(test)]
mod tests {
    use super::permission;

    #[test]
    fn permissions_read_as_discord_names_them() {
        assert_eq!(permission("MANAGE_GUILD"), "Manage Server");
        assert_eq!(permission("USE_VAD"), "Use Voice Activity");
        assert_eq!(permission("MODERATE_MEMBERS"), "Timeout Members");
        assert_eq!(permission("KICK_MEMBERS"), "Kick Members");
        assert_eq!(permission("REQUEST_TO_SPEAK"), "Request to Speak");
    }
}

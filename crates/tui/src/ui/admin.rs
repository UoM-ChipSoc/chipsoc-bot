//! The admin screens: the review queue, members, feeds and resources.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Stylize;
use ratatui::text::{Line, Span};

use super::tabs::{list_and_details, text};
use super::{field, when};
use crate::app::{App, Loaded};
use crate::lists::{self, Shared, Who};

/// Data that isn't here yet, or couldn't be had.
fn missing<T>(f: &mut Frame, area: Rect, title: &str, data: &Loaded<T>) -> bool {
    let line = match data {
        Loaded::Got(_) => return false,
        Loaded::No => Line::from("Loading… (press r if this stays)").italic(),
        Loaded::Failed(e) => Line::from(e.clone()).red(),
    };
    text(f, area, title, vec![line], 0);
    true
}

pub fn queue(f: &mut Frame, area: Rect, app: &App) {
    if app.show_history {
        if missing(f, area, "History", &app.history) {
            return;
        }
        let Some(h) = app.history.get() else { return };
        if h.is_empty() {
            return text(f, area, "History", vec![Line::from("Nothing decided yet. Press h for the queue.")], 0);
        }
        let labels = h
            .iter()
            .map(|p| {
                let mark = if p.status == "accepted" { Span::from("✓ ").green() } else { Span::from("✗ ").red() };
                Line::from(vec![mark, Span::raw(format!("{} — {}", lists::sender(&p.user, &p.from), p.what))])
            })
            .collect();
        let mut details = Vec::new();
        if let Some(p) = h.get(app.sel()) {
            let decision = if p.status == "accepted" { "Accepted" } else { "Rejected" };
            details = vec![
                Line::from(format!("{}: {}", lists::sender(&p.user, &p.from), lists::item(&p.kind, &p.what))).bold(),
                field("Decision", decision),
                field("By", p.by.clone().or_else(|| p.reviewed_by.clone()).unwrap_or_else(|| "-".into())),
                field("When", p.reviewed_at.map(when).unwrap_or_default()),
                field("Reason", p.reason.clone().unwrap_or_else(|| "none given".into())),
                field("Sent", when(p.created_at)),
                field("Discord id", p.user.clone()).italic(),
            ];
        }
        return list_and_details(
            f,
            area,
            app,
            ("History (h: back to the queue)", labels, app.sel()),
            ("Details", details),
        );
    }
    if missing(f, area, "Review queue", &app.queue) {
        return;
    }
    let Some(q) = app.queue.get() else { return };
    if q.is_empty() && !app.review_on() {
        let lines = vec![
            Line::from("The review queue is off. To turn it on, add [review] to bot.toml (5, then e opens it).")
                .italic(),
        ];
        return text(f, area, "Review queue", lines, 0);
    }
    if q.is_empty() {
        let lines = vec![Line::from(
            "✓ Nothing waiting. Applications and suggestions (positions, projects, ideas, feeds, links) wait here \
             for Accept or Reject.",
        )];
        return text(f, area, "Review queue", lines, 0);
    }
    let labels = q.iter().map(|p| Line::from(format!("{} — {}", lists::sender(&p.user, &p.from), p.what))).collect();
    let mut details = Vec::new();
    if let Some(p) = q.get(app.sel()) {
        details
            .push(Line::from(format!("{}: {}", lists::sender(&p.user, &p.from), lists::item(&p.kind, &p.what))).bold());
        details.push(field("Kind", lists::kind_label(&p.kind)));
        details.push(field("Sent", when(p.created_at)));
        details.push(field("Discord id", p.user.clone()).italic());
        if p.as_lead {
            details.push(Line::from("They'd like to lead it.").cyan());
        }
        for a in &p.answers {
            details.push(Line::default());
            details.push(Line::from(a.question.clone()).italic());
            details.push(Line::from(if a.answer.is_empty() { "(left blank)".to_owned() } else { a.answer.clone() }));
        }
    }
    list_and_details(f, area, app, ("Waiting for Accept or Reject", labels, app.sel()), ("Details", details));
}

pub fn members(f: &mut Frame, area: Rect, app: &App) {
    if missing(f, area, "Members", &app.roster) {
        return;
    }
    let Some(r) = app.roster.get() else { return };
    let rows = lists::people(r);
    if rows.is_empty() {
        let lines = vec![Line::from(
            "No roles are managed yet. Add a position (5, then n) to keep track of who holds its role.",
        )];
        return text(f, area, "Members", lines, 0);
    }
    let holding = |role: &str| r.holders.iter().find(|h| h.role == role);
    let leads = |slug: &str| r.leads.iter().find(|h| h.project.as_deref() == Some(slug));
    let labels = rows
        .iter()
        .map(|w| match w {
            Who::Role(k) => {
                let h = holding(k);
                let pos = if h.and_then(|h| h.position.as_ref()).is_some() { " (position)" } else { "" };
                Line::from(format!("▾ {}{pos}", h.map_or(k.as_str(), |h| h.name.as_str()))).bold()
            }
            Who::Project(s) => Line::from(format!("▾ {}", leads(s).map_or(s.as_str(), |h| h.name.as_str()))).bold(),
            Who::Holder(_, u) | Who::Lead(_, u) => Line::from(format!("    {}", lists::name_of(r, *u))),
            Who::Left => Line::from("▾ Left the server").bold(),
            Who::Former(u) => Line::from(format!("    {}", lists::name_of(r, *u))).italic(),
        })
        .collect();
    let details = match rows.get(app.sel()) {
        Some(Who::Role(k)) => {
            let h = holding(k);
            let n = h.map_or(0, |h| h.people.len());
            vec![
                Line::from(h.map_or(k.clone(), |h| h.name.clone())).bold(),
                field("Held by", if n == 0 { "nobody yet (n to appoint)".into() } else { n.to_string() }),
                Line::from("Only the people listed here hold this role; the bot keeps it that way.").italic(),
            ]
        }
        Some(Who::Holder(k, u)) => vec![
            Line::from(lists::name_of(r, *u)).bold(),
            field("Holds", holding(k).map_or(k.clone(), |h| h.name.clone())),
            field("Discord id", u.to_string()).italic(),
        ],
        Some(Who::Project(s)) => vec![
            Line::from(leads(s).map_or(s.clone(), |h| h.name.clone())).bold(),
            Line::from("Leads get the project's Lead role. Press l to add one.").italic(),
        ],
        Some(Who::Lead(s, u)) => vec![
            Line::from(lists::name_of(r, *u)).bold(),
            field("Leads", s.clone()),
            field("Discord id", u.to_string()).italic(),
        ],
        Some(Who::Left) => vec![Line::from("People who left holding roles. When they come back, g restores them.")],
        Some(Who::Former(u)) => match r.former.iter().find(|x| x.person.id == *u) {
            Some(x) => vec![
                Line::from(x.person.name.clone()).bold(),
                field("Held", x.roles.iter().map(|n| n.name.clone()).collect::<Vec<_>>().join(", ")),
                field("Left", when(x.since)),
                Line::from("g restores their roles (once they're back); f forgets them.").italic(),
            ],
            None => Vec::new(),
        },
        None => Vec::new(),
    };
    list_and_details(f, area, app, ("Who holds what", labels, app.sel()), ("Details", details));
}

pub fn feeds(f: &mut Frame, area: Rect, app: &App) {
    if missing(f, area, "Feeds", &app.feeds) {
        return;
    }
    let Some(fs) = app.feeds.get() else { return };
    if fs.is_empty() && !app.feeds_on() {
        let lines =
            vec![Line::from("Feeds are off. To turn them on, add [feeds] to bot.toml (5, then e opens it).").italic()];
        return text(f, area, "Feeds", lines, 0);
    }
    if fs.is_empty() {
        let lines = vec![Line::from("Not following any feeds. Press n to follow a blog or newsletter's feed.")];
        return text(f, area, "Feeds", lines, 0);
    }
    let labels = fs
        .iter()
        .map(|x| {
            let state = if x.paused { Span::from(" paused").bold() } else { Span::raw("") };
            Line::from(vec![Span::raw(x.url.clone()), state])
        })
        .collect();
    let mut details = Vec::new();
    if let Some(x) = fs.get(app.sel()) {
        details = vec![
            Line::from(x.url.clone()).bold(),
            field("Posts in", format!("#{}", x.channel)),
            field("State", if x.paused { "paused" } else { "following" }),
            field("Added by", x.added_by_name.clone().or_else(|| x.added_by.clone()).unwrap_or_else(|| "-".into())),
            field("Added", when(x.added_at)),
        ];
    }
    list_and_details(f, area, app, ("Feeds followed", labels, app.sel()), ("Details", details));
}

pub fn resources(f: &mut Frame, area: Rect, app: &App) {
    if !app.resources_on() {
        let lines = vec![
            Line::from("Resources are off. To turn them on, add [resources] to bot.toml (5, then e opens it).")
                .italic(),
        ];
        return text(f, area, "Resources", lines, 0);
    }
    if missing(f, area, "Resources", &app.resources) {
        return;
    }
    let Some((res, ideas)) = app.resources.get() else { return };
    let no_ideas = Vec::new();
    let idea_list = ideas.as_ref().unwrap_or(&no_ideas);
    let rows = lists::shared(res, idea_list);
    if rows.is_empty() {
        let mut lines = vec![Line::from("Nothing shared yet. Press n to share a link.")];
        if let Err(e) = ideas {
            lines.push(Line::from(format!("Ideas: {e}")).italic());
        }
        return text(f, area, "Resources", lines, 0);
    }
    let labels = rows
        .iter()
        .map(|r| match r {
            Shared::Resource(i) => Line::from(format!("🔗 {}", res[*i].title)),
            Shared::Idea(i) => Line::from(format!("💡 {}", idea_list[*i].title)).cyan(),
        })
        .collect();
    let post = rows.get(app.sel()).map(|r| match r {
        Shared::Resource(i) => &res[*i],
        Shared::Idea(i) => &idea_list[*i],
    });
    let mut details = Vec::new();
    if let Some(p) = post {
        details.push(Line::from(p.title.clone()).bold());
        details.extend((!p.tags.is_empty()).then(|| field("Tags", p.tags.join(", "))));
        details.extend(p.archived.then(|| field("State", "archived")));
        details.push(field("Link", p.link.clone()).italic());
        details.push(Line::default());
        details.extend(p.content.lines().map(|l| Line::from(l.to_owned())));
    }
    list_and_details(f, area, app, ("Resources 🔗 and ideas 💡", labels, app.sel()), ("Post", details));
}

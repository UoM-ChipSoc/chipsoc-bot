//! Projects (projects.toml) expand into roles, a category of channels, and a
//! front-page post in the projects forum.
//!
//! - active projects get `<Name> Team` and `<Name> Lead` roles; when a project
//!   finishes they stay with their holders as a record, renamed
//!   `<Name> · Contributor` and `<Name> · Lead (past)`;
//! - active projects get a category, placed right after the one holding the
//!   projects forum, in projects.toml order, channels in the order listed;
//! - past projects' channels move to the archive category, history kept;
//! - proposed projects (ideas) get a post only.

use crate::schema::{Category, CategoryFile, Channel, ChannelKind, Lock, Project, ProjectStatus, Role, Snapshot};
use crate::template::{self, Button, Rendered};
use crate::{BotConfig, slug};

/// What moving a project from `from` to `to` does, for every front end to
/// ask about first: none if it stays the same.
pub fn status_change(from: ProjectStatus, to: ProjectStatus) -> Option<&'static str> {
    use ProjectStatus::{Active, Past, Proposed};
    Some(match (from, to) {
        _ if from == to => return None,
        (_, Past) => {
            "its channels move to the archive (history kept) and its roles are renamed when you apply; \
                      its applications close"
        }
        (Active, Proposed) => {
            "its channels move to the archive when you apply and its roles are no longer managed; \
                               making it active again starts new channels. Its applications close"
        }
        (_, Active) => "it gets its roles and its own channels when you apply",
        (_, Proposed) => "it's an idea again: its roles are no longer managed",
    })
}

pub fn expand(snap: &mut Snapshot, projects: &[Project], bot: &BotConfig) {
    let mut new_cats = Vec::new();
    for p in projects {
        if p.status == ProjectStatus::Proposed {
            continue; // an idea: just its post
        }
        let mut add_role = |key: String, name: String, mentionable: bool| {
            if snap.role(&key).is_none() {
                snap.roles.roles.push(Role { key, name, mentionable, ..Default::default() });
            }
        };
        let live = p.status == ProjectStatus::Active;
        if p.team_role {
            let name = if live { format!("{} Team", p.name) } else { format!("{} · Contributor", p.name) };
            add_role(p.team_role_key(), name, live);
        }
        if p.lead_role {
            let name = if live { format!("{} Lead", p.name) } else { format!("{} · Lead (past)", p.name) };
            add_role(p.lead_role_key(), name, live);
        }

        let channels = channels(p);
        match p.status {
            ProjectStatus::Active => new_cats.push(CategoryFile {
                category: Category { key: p.category_key(), name: p.name.clone(), overwrites: Vec::new() },
                channels,
            }),
            ProjectStatus::Past => {
                if let Some(archive) = bot.archive.as_deref().and_then(|k| category_mut(snap, k)) {
                    archive.channels.extend(channels);
                }
            }
            ProjectStatus::Proposed => {}
        }
    }
    let forum = bot.projects.as_ref().map(|p| p.forum.as_str());
    let at = forum
        .and_then(|f| snap.categories.iter().position(|c| c.channels.iter().any(|ch| ch.key == f)))
        .map(|i| i + 1)
        .or_else(|| bot.archive.as_deref().and_then(|a| snap.categories.iter().position(|c| c.category.key == a)))
        .unwrap_or(snap.categories.len());
    snap.categories.splice(at..at, new_cats);
}

fn category_mut<'a>(snap: &'a mut Snapshot, key: &str) -> Option<&'a mut CategoryFile> {
    snap.categories.iter_mut().find(|c| c.category.key == key)
}

/// A project's channels: text first (Discord lists voice last anyway), each
/// group in the order written.
fn channels(p: &Project) -> Vec<Channel> {
    let (voice, text): (Vec<&String>, Vec<&String>) = p.channels.iter().partition(|c| is_voice(c));
    text.into_iter()
        .chain(voice)
        .map(|c| {
            let voice = is_voice(c);
            Channel {
                key: p.channel_key(c),
                kind: if voice { ChannelKind::Voice } else { ChannelKind::Text },
                name: if voice { format!("{} {}", p.name, title_case(c)) } else { format!("{}-{}", p.slug, slug(c)) },
                topic: (!voice).then(|| format!("{} · {c}", p.display())),
                ..Default::default()
            }
        })
        .collect()
}

fn is_voice(name: &str) -> bool {
    name == "voice" || name.ends_with("-voice") || name.ends_with(" voice")
}

fn title_case(s: &str) -> String {
    let mut c = s.chars();
    c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default()
}

/// The project's forum post: title, text (one message per part), buttons and tags.
pub struct Post {
    pub title: String,
    /// Its messages: the first starts the thread; buttons go under the last.
    pub parts: Vec<Rendered>,
    /// Forum tag names to apply (matched case-insensitively).
    pub tags: Vec<String>,
}

/// One string for comparing a project post with Discord: title, tags, text, buttons.
pub fn signature(title: &str, tags: &[String], parts: &[Rendered]) -> String {
    let mut tags: Vec<String> = tags.iter().map(|t| t.to_lowercase()).collect();
    tags.sort();
    tags.dedup();
    format!("{title}\n[{}]\n{}", tags.join(", "), template::signature(parts))
}

/// What a pinned post's [`signature`] starts with.
const PINNED: &str = "📌\n";

/// `sig` (a [`signature`]) for a post that's pinned, if it is.
pub fn pinned(sig: String, pinned: bool) -> String {
    if pinned { format!("{PINNED}{sig}") } else { sig }
}

/// Whether a [`pinned`] signature is pinned, and the signature without it.
pub fn unpinned(sig: &str) -> (bool, &str) {
    match sig.strip_prefix(PINNED) {
        Some(rest) => (true, rest),
        None => (false, sig),
    }
}

pub fn post(p: &Project, lock: &Lock, brief: Option<&str>) -> Post {
    let (text, buttons) = text(p, lock, brief);
    let mut parts: Vec<Rendered> = template::split(&text)
        .iter()
        .map(|part| {
            let r = template::render(part, lock);
            Rendered { content: truncate(&r.content, template::MAX_MESSAGE), buttons: Vec::new() }
        })
        .collect();
    if let Some(last) = parts.last_mut() {
        last.buttons = buttons;
    }
    let mut tags: Vec<String> = p.tags.clone();
    tags.push(p.status.as_str().into());
    if !p.kind.is_empty() {
        tags.push(p.kind.clone());
    }
    Post { title: p.display(), parts, tags }
}

/// The post's source, one per message (see [`template::split`]), for `check`.
pub fn parts(p: &Project, lock: &Lock, brief: Option<&str>) -> Vec<String> {
    template::split(&text(p, lock, brief).0)
}

/// The post's text, as a template, and its buttons.
fn text(p: &Project, lock: &Lock, brief: Option<&str>) -> (String, Vec<Button>) {
    let status = match p.status {
        ProjectStatus::Proposed => "💡 Proposed",
        ProjectStatus::Active => "🟢 Active",
        ProjectStatus::Past => "🏁 Finished",
    };
    let mut s = format!("# {}\n**Status:** {status}", p.display());
    if let Some(stage) = &p.stage {
        s.push_str(&format!(" · {stage}"));
    }
    if !p.kind.is_empty() {
        s.push_str(&format!(" · **Area:** {}", p.kind));
    }
    if let Some(gh) = &p.github {
        s.push_str(&format!("\n**Code:** <{gh}>"));
    }
    let chans: Vec<String> =
        p.channels.iter().filter_map(|c| lock.channel(&p.channel_key(c))).map(|id| format!("<#{id}>")).collect();
    if !chans.is_empty() {
        s.push_str(&format!("\n**Channels:** {}", chans.join(" ")));
    }
    if let Some(brief) = brief {
        // The brief's own button lines aren't used: the post's buttons are its own.
        s.push_str(&format!("\n\n{}", template::without_buttons(brief, |_| true).trim()));
    }
    let mut buttons = Vec::new();
    if p.application.open && p.is_live() {
        buttons.push(Button { action: format!("apply:{}", p.slug), label: "Apply to join".into() });
    } else if p.is_live() {
        s.push_str("\n\n_Not taking applications right now; ask in the channels above if you'd like to help._");
    }
    (s, buttons)
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max { s.to_owned() } else { s.chars().take(max - 1).chain(['…']).collect() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bot::ProjectsConf;

    fn project(slug: &str, status: ProjectStatus) -> Project {
        Project {
            slug: slug.into(),
            name: slug.to_uppercase(),
            status,
            team_role: true,
            lead_role: true,
            channels: vec!["voice".into(), "general".into(), "rtl".into()],
            ..Default::default()
        }
    }

    fn cat(key: &str, channels: &[&str]) -> CategoryFile {
        CategoryFile {
            category: Category { key: key.into(), name: key.into(), overwrites: vec![] },
            channels: channels
                .iter()
                .map(|c| Channel { key: (*c).into(), name: (*c).into(), ..Default::default() })
                .collect(),
        }
    }

    #[test]
    fn expands_by_status_and_places_after_the_projects_forum() {
        let mut snap = Snapshot {
            categories: vec![cat("meta", &[]), cat("work", &["projects"]), cat("voice", &[]), cat("archive", &[])],
            ..Default::default()
        };
        let bot = BotConfig {
            archive: Some("archive".into()),
            projects: Some(ProjectsConf { forum: "projects".into(), ..Default::default() }),
            ..Default::default()
        };
        let projects = [
            project("golem", ProjectStatus::Active),
            project("old", ProjectStatus::Past),
            project("idea", ProjectStatus::Proposed),
            project("wyrm", ProjectStatus::Active),
        ];
        expand(&mut snap, &projects, &bot);

        let order: Vec<&str> = snap.categories.iter().map(|c| c.category.key.as_str()).collect();
        assert_eq!(order, ["meta", "work", "proj-golem", "proj-wyrm", "voice", "archive"]);
        let golem = snap.category("proj-golem").unwrap();
        let chans: Vec<&str> = golem.channels.iter().map(|c| c.key.as_str()).collect();
        assert_eq!(chans, ["proj-golem-general", "proj-golem-rtl", "proj-golem-voice"], "text first, spec order");
        assert_eq!(snap.category("archive").unwrap().channels.len(), 3, "past project channels archived");
        assert!(snap.role("proj-golem-lead").is_some());
        assert_eq!(snap.role("proj-old-team").unwrap().name, "OLD · Contributor", "finished projects keep a record");
        assert!(snap.role("proj-idea-team").is_none(), "ideas get no roles");
        assert!(snap.category("proj-idea").is_none(), "proposed gets no channels");
    }

    #[test]
    fn post_has_apply_button_when_open() {
        let mut p = project("golem", ProjectStatus::Active);
        p.application.open = true;
        let post = post(&p, &Lock::default(), None);
        assert_eq!(post.parts[0].buttons[0].action, "apply:golem");
        assert!(post.tags.contains(&"active".to_owned()));
    }
}

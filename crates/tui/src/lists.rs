//! What each list tab holds, row by row, from the config's [`Schema`] or the
//! admin data loaded for it. Shared by key handling and drawing.

use chipsoc_client::remote::{Post, Roster};
use chipsoc_config::Snowflake;
use chipsoc_config::ops::Schema;
use chipsoc_config::schema::Overwrite;
use chipsoc_store::AppKind;

/// A row on the Channels tab.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Spot {
    Category(String),
    /// A channel, and the category it's in (none: outside any).
    Channel(String, Option<String>),
}

/// Channels outside any category, then each category and its channels.
pub fn spots(s: &Schema) -> Vec<Spot> {
    let mut v: Vec<Spot> = s.top_level.iter().map(|c| Spot::Channel(c.key.clone(), None)).collect();
    for cf in &s.categories {
        let cat = &cf.category.key;
        v.push(Spot::Category(cat.clone()));
        v.extend(cf.channels.iter().map(|c| Spot::Channel(c.key.clone(), Some(cat.clone()))));
    }
    v
}

/// A row on the Members tab.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Who {
    /// A role the bot keeps up to date (a position's, or one listed in members.toml).
    Role(String),
    Holder(String, Snowflake),
    /// A project's leads.
    Project(String),
    Lead(String, Snowflake),
    Left,
    Former(Snowflake),
}

pub fn people(r: &Roster) -> Vec<Who> {
    let mut v = Vec::new();
    for h in &r.holders {
        v.push(Who::Role(h.role.clone()));
        v.extend(h.people.iter().map(|p| Who::Holder(h.role.clone(), p.id)));
    }
    for l in &r.leads {
        let Some(slug) = &l.project else { continue };
        v.push(Who::Project(slug.clone()));
        v.extend(l.people.iter().map(|p| Who::Lead(slug.clone(), p.id)));
    }
    if !r.former.is_empty() {
        v.push(Who::Left);
        v.extend(r.former.iter().map(|f| Who::Former(f.person.id)));
    }
    v
}

/// Someone's name in the roster, else who they are by id.
pub fn name_of(r: &Roster, id: Snowflake) -> String {
    let all = r.holders.iter().chain(&r.leads).flat_map(|h| &h.people).chain(r.former.iter().map(|f| &f.person));
    member(&all.into_iter().find(|p| p.id == id).map_or_else(|| id.to_string(), |p| p.name.clone()))
}

/// Who sent a queue entry: the name the backend found (`from`), else by id.
pub fn sender(user: &str, from: &str) -> String {
    member(if from.is_empty() { user } else { from })
}

/// A name as shown; a bare id (no name could be found) as "unknown member (id …1111)".
pub fn member(name: &str) -> String {
    if name.is_empty() || !name.bytes().all(|b| b.is_ascii_digit()) {
        return name.to_owned();
    }
    let tail = &name[name.len().saturating_sub(4)..];
    format!("unknown member (id …{tail})")
}

/// What a queue entry is, in words: "application for Treasurer", "project idea Golem".
pub fn item(kind: &str, what: &str) -> String {
    match kind {
        "position" => format!("application for {what}"),
        "project" => format!("application to join {what}"),
        "idea" => format!("project idea {what}"),
        "feed" => format!("feed suggestion {what}"),
        "resource" => format!("resource {what}"),
        "event" => format!("event suggestion {what}"),
        k if k.starts_with("partner-") => format!("{} {what}", kind_label(k).to_lowercase()),
        _ => what.to_owned(),
    }
}

/// A queue entry's kind as a heading (see [`AppKind::label`]).
pub fn kind_label(kind: &str) -> String {
    AppKind::label_of(kind)
}

/// `MANAGE_GUILD` as Discord shows it: "Manage Server".
pub fn permission(name: &str) -> String {
    const DISCORD: [(&str, &str); 12] = [
        ("ADMINISTRATOR", "Administrator (every permission)"),
        ("MANAGE_GUILD", "Manage Server"),
        ("USE_VAD", "Use Voice Activity"),
        ("MODERATE_MEMBERS", "Timeout Members"),
        ("VIEW_CHANNEL", "View Channels"),
        ("SEND_MESSAGES", "Send Messages"),
        ("MANAGE_NICKNAMES", "Manage Nicknames"),
        ("CHANGE_NICKNAME", "Change Nickname"),
        ("VIEW_GUILD_INSIGHTS", "View Server Insights"),
        ("MANAGE_GUILD_EXPRESSIONS", "Manage Expressions"),
        ("SEND_TTS_MESSAGES", "Send Text-to-Speech Messages"),
        ("USE_EXTERNAL_EMOJIS", "Use External Emoji"),
    ];
    if let Some((_, label)) = DISCORD.iter().find(|(n, _)| *n == name) {
        return (*label).to_owned();
    }
    name.split('_')
        .map(|w| {
            let w = w.to_lowercase();
            let mut c = w.chars();
            c.next().map(|f| f.to_uppercase().chain(c).collect::<String>()).unwrap_or_default()
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// A row on the Resources tab.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shared {
    Resource(usize),
    Idea(usize),
}

pub fn shared(resources: &[Post], ideas: &[Post]) -> Vec<Shared> {
    (0..resources.len()).map(Shared::Resource).chain((0..ideas.len()).map(Shared::Idea)).collect()
}

/// A role's name in the config, else its key.
pub fn role_name<'a>(s: &'a Schema, key: &'a str) -> &'a str {
    s.roles.iter().find(|r| r.key == key).map_or(key, |r| r.name.as_str())
}

/// Who can see a channel or category, in words.
pub fn audience(s: &Schema, ows: &[Overwrite]) -> String {
    let view = |list: &[String]| list.iter().any(|p| p == "VIEW_CHANNEL");
    let everyone = |o: &&Overwrite| o.role.as_deref() == Some("everyone");
    if !ows.iter().filter(everyone).any(|o| view(&o.deny)) {
        return "everyone".into();
    }
    let names: Vec<&str> = ows
        .iter()
        .filter(|o| !everyone(o) && view(&o.allow))
        .filter_map(|o| o.role.as_deref())
        .map(|k| role_name(s, k))
        .collect();
    if names.is_empty() { "only admins".into() } else { format!("only {}", names.join(", ")) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn people_and_permissions_read_as_words() {
        assert_eq!(member("Ada"), "Ada");
        assert_eq!(member("123456781111"), "unknown member (id …1111)");
        assert_eq!(permission("MANAGE_GUILD"), "Manage Server");
        assert_eq!(permission("KICK_MEMBERS"), "Kick Members");
        assert_eq!(item("idea", "Golem"), "project idea Golem");
    }
}

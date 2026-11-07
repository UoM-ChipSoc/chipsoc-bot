//! What admin actions say back, kept as parts (names, people, channels,
//! times, commands) so each front end draws it its own way: Discord
//! markdown in the bot, plain text with people's names in the web UI, the
//! CLI and the TUI.

use std::collections::HashMap;

use chipsoc_config::{Config, Snowflake};
use chipsoc_engine::OwnerOnly;
use serde::{Deserialize, Serialize};

use crate::Host;

/// Words for people, as parts (see the module docs). Sentences added with
/// [`Said::then`] are spaced for you. Sent as JSON (the web API), it's its
/// plain text: name people first ([`Said::named`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Said(pub Vec<Part>);

impl Serialize for Said {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.plain(None))
    }
}

impl<'de> Deserialize<'de> for Said {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(Said::new().text(String::deserialize(d)?))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Part {
    Text(String),
    /// A name to stand out: a role, a project, a feed.
    Strong(String),
    /// Someone, with their name once looked up (see [`Said::named`]).
    User {
        id: Snowflake,
        name: Option<String>,
    },
    Channel(Snowflake),
    /// A moment, in Unix seconds.
    Time(i64),
    /// A command or a file: `/server plan`.
    Code(String),
}

impl Said {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn text(mut self, s: impl Into<String>) -> Self {
        let s = s.into();
        if !s.is_empty() {
            self.0.push(Part::Text(s));
        }
        self
    }

    pub fn strong(mut self, s: impl Into<String>) -> Self {
        self.0.push(Part::Strong(s.into()));
        self
    }

    pub fn user(mut self, id: Snowflake) -> Self {
        self.0.push(Part::User { id, name: None });
        self
    }

    pub fn channel(mut self, id: Snowflake) -> Self {
        self.0.push(Part::Channel(id));
        self
    }

    pub fn time(mut self, ts: i64) -> Self {
        self.0.push(Part::Time(ts));
        self
    }

    pub fn code(mut self, s: impl Into<String>) -> Self {
        self.0.push(Part::Code(s.into()));
        self
    }

    /// `next` after this, with a space between unless one is already there.
    /// With its first letter lowercase, to go mid-sentence ("…: the config doesn't say").
    pub fn mid_sentence(mut self) -> Self {
        if let Some(Part::Text(t)) = self.0.first_mut() {
            let mut c = t.chars();
            if let Some(f) = c.next() {
                *t = f.to_lowercase().chain(c).collect();
            }
        }
        self
    }

    pub fn then(mut self, next: impl Into<Said>) -> Self {
        let mut next = next.into();
        if let Some(Part::Text(t)) = next.0.first_mut() {
            *t = t.trim_start_matches(' ').to_owned();
            if t.is_empty() {
                next.0.remove(0);
            }
        }
        if next.is_empty() {
            return self;
        }
        let ends_spaced = match self.0.last() {
            None => true,
            Some(Part::Text(t)) => t.ends_with(char::is_whitespace),
            Some(_) => false,
        };
        let starts_tight = match next.0.first() {
            Some(Part::Text(t)) => t.starts_with(['\n', '.', ',', ';', ':', ')']),
            _ => false,
        };
        if !ends_spaced && !starts_tight {
            self.0.push(Part::Text(" ".into()));
        }
        self.0.extend(next.0);
        self
    }

    pub fn is_empty(&self) -> bool {
        self.0.iter().all(|p| matches!(p, Part::Text(t) if t.trim().is_empty()))
    }

    /// [`Said::markdown`] after a space, to follow a sentence (nothing if it's empty).
    pub fn spaced(&self) -> String {
        if self.is_empty() { String::new() } else { format!(" {}", self.markdown()) }
    }

    /// Whether its markdown has `s` in it.
    pub fn contains(&self, s: &str) -> bool {
        self.markdown().contains(s)
    }

    /// Whether its markdown starts with `s`.
    pub fn starts_with(&self, s: &str) -> bool {
        self.markdown().starts_with(s)
    }

    /// The people it mentions.
    pub fn users(&self) -> impl Iterator<Item = Snowflake> + '_ {
        self.0.iter().filter_map(|p| match p {
            Part::User { id, .. } => Some(*id),
            _ => None,
        })
    }

    /// As Discord shows it: names in bold, people and channels as mentions.
    pub fn markdown(&self) -> String {
        self.0
            .iter()
            .map(|p| match p {
                Part::Text(t) => t.clone(),
                Part::Strong(s) => format!("**{}**", crate::fmt::escape(s)),
                Part::User { id, .. } => format!("<@{id}>"),
                Part::Channel(id) => format!("<#{id}>"),
                Part::Time(ts) => format!("<t:{ts}:F>"),
                Part::Code(c) => format!("`{c}`"),
            })
            .collect()
    }

    /// As plain text: people by name (once looked up, else their id),
    /// channels by `cfg`'s names, times in its time zone (UTC without one).
    /// Commands keep their backticks.
    pub fn plain(&self, cfg: Option<&Config>) -> String {
        self.0
            .iter()
            .map(|p| match p {
                Part::Text(t) | Part::Strong(t) => t.clone(),
                Part::User { name: Some(n), .. } => n.clone(),
                Part::User { id, name: None } => format!("user {id}"),
                Part::Channel(id) => channel_name(cfg, *id),
                Part::Time(ts) => {
                    cfg.map_or_else(|| format!("{} UTC", chipsoc_model::utc_minute(*ts)), |c| c.bot.local_minute(*ts))
                }
                Part::Code(c) => format!("`{c}`"),
            })
            .collect::<String>()
            .trim()
            .to_owned()
    }

    /// With people's names filled in from `names`.
    pub fn with_names(mut self, names: &HashMap<Snowflake, String>) -> Self {
        for p in &mut self.0 {
            if let Part::User { id, name } = p
                && let Some(n) = names.get(id)
            {
                *name = Some(n.clone());
            }
        }
        self
    }

    /// With people's names filled in from the server's member list, and
    /// channels' from the config: ready for [`Said::plain`] anywhere.
    pub async fn named<H: Host>(self, h: &H) -> Self {
        let cfg = h.cfg();
        let mut said = self;
        for p in &mut said.0 {
            if let Part::Channel(id) = p {
                *p = Part::Text(channel_name(Some(&cfg), *id));
            }
        }
        let users: Vec<Snowflake> = said.users().collect();
        if users.is_empty() {
            return said;
        }
        let names = crate::members_of(h, users).await.into_iter().map(|(id, m)| (id, m.name)).collect();
        said.with_names(&names)
    }

    /// Lifts Discord markdown as older code wrote it (`**name**`, `<@id>`,
    /// `<#id>`, `<t:ts:F>`, `` `code` ``) into parts, so it reads plainly
    /// in the web UI and the CLI too.
    pub fn md(s: &str) -> Self {
        let mut out = Said::new();
        let mut rest = s;
        while !rest.is_empty() {
            let next = [rest.find("**"), rest.find('<'), rest.find('`')].into_iter().flatten().min();
            let Some(at) = next else {
                out = out.text(rest);
                break;
            };
            out = out.text(&rest[..at]);
            let tail = &rest[at..];
            if let Some(inner) = tail.strip_prefix("**")
                && let Some(end) = inner.find("**")
            {
                out = out.strong(unescape(&inner[..end]));
                rest = &inner[end + 2..];
            } else if let Some(inner) = tail.strip_prefix('`')
                && let Some(end) = inner.find('`')
            {
                out = out.code(&inner[..end]);
                rest = &inner[end + 1..];
            } else if let Some((part, len)) = mention(tail) {
                out.0.push(part);
                rest = &tail[len..];
            } else {
                let n = tail.chars().next().map_or(1, char::len_utf8);
                out = out.text(&tail[..n]);
                rest = &tail[n..];
            }
        }
        out.merged()
    }

    /// Neighbouring texts as one.
    fn merged(self) -> Self {
        let mut out: Vec<Part> = Vec::new();
        for p in self.0 {
            match (out.last_mut(), p) {
                (Some(Part::Text(a)), Part::Text(b)) => a.push_str(&b),
                (_, p) => out.push(p),
            }
        }
        Self(out)
    }
}

/// `<@id>`, `<@!id>`, `<#id>` or `<t:ts…>` at the start of `s`, and its length.
fn mention(s: &str) -> Option<(Part, usize)> {
    let end = s.find('>')?;
    let inner = &s[1..end];
    let part = if let Some(id) = inner.strip_prefix("@!").or_else(|| inner.strip_prefix('@')) {
        Part::User { id: Snowflake(id.parse().ok()?), name: None }
    } else if let Some(id) = inner.strip_prefix('#') {
        Part::Channel(Snowflake(id.parse().ok()?))
    } else {
        Part::Time(inner.strip_prefix("t:")?.split(':').next()?.parse().ok()?)
    };
    Some((part, end + 1))
}

/// [`crate::fmt::escape`] undone.
fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => out.extend(chars.next()),
            c => out.push(c),
        }
    }
    out
}

fn channel_name(cfg: Option<&Config>, id: Snowflake) -> String {
    let named = cfg.and_then(|cfg| {
        let key = cfg.desired.lock.channel_key(id)?;
        Some(cfg.desired.channel(key).map_or_else(|| key.to_owned(), |c| c.name.clone()))
    });
    format!("#{}", named.unwrap_or_else(|| id.to_string()))
}

impl From<&str> for Said {
    /// Older prose, lifted (see [`Said::md`]).
    fn from(s: &str) -> Self {
        Self::md(s)
    }
}

impl From<String> for Said {
    fn from(s: String) -> Self {
        Self::md(&s)
    }
}

impl PartialEq<&str> for Said {
    /// Its markdown is `other`.
    fn eq(&self, other: &&str) -> bool {
        self.markdown() == *other
    }
}

impl std::fmt::Display for Said {
    /// As plain text (people as their names if looked up).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.plain(None))
    }
}

/// Why admin work wasn't done: words for whoever asked, not a failure to
/// log. The one refusal type of the shared admin code, so every front end
/// draws it as it draws a [`Said`] and can tell who could do it instead
/// (the web API answers 403 for the owner's, 409 for the rest).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// Only the server owner may do it.
    OwnerOnly(OwnerOnly),
    /// Only the server owner may do it, and Discord couldn't say just now who that is.
    OwnerUnknown,
    /// Anything else, in words.
    Said(Said),
}

impl Refusal {
    /// In words, as for anything else said.
    pub fn said(&self) -> Said {
        match self {
            Self::OwnerOnly(o) => Said::md(&format!("{o}.")),
            Self::OwnerUnknown => Said::new().text(chipsoc_engine::OWNER_UNKNOWN),
            Self::Said(s) => s.clone(),
        }
    }

    /// Whether only the server owner could do it.
    pub fn owner_only(&self) -> bool {
        matches!(self, Self::OwnerOnly(_) | Self::OwnerUnknown)
    }

    /// See [`Said::markdown`].
    pub fn markdown(&self) -> String {
        self.said().markdown()
    }

    /// See [`Said::plain`].
    pub fn plain(&self, cfg: Option<&Config>) -> String {
        self.said().plain(cfg)
    }

    /// See [`Said::spaced`].
    pub fn spaced(&self) -> String {
        self.said().spaced()
    }

    /// See [`Said::contains`].
    pub fn contains(&self, s: &str) -> bool {
        self.said().contains(s)
    }

    /// See [`Said::starts_with`].
    pub fn starts_with(&self, s: &str) -> bool {
        self.said().starts_with(s)
    }

    /// See [`Said::named`].
    pub async fn named<H: Host>(self, h: &H) -> Said {
        self.said().named(h).await
    }
}

impl From<OwnerOnly> for Refusal {
    fn from(o: OwnerOnly) -> Self {
        Self::OwnerOnly(o)
    }
}

impl From<Said> for Refusal {
    fn from(s: Said) -> Self {
        Self::Said(s)
    }
}

impl From<&str> for Refusal {
    /// Older prose, lifted (see [`Said::md`]).
    fn from(s: &str) -> Self {
        Self::Said(s.into())
    }
}

impl From<String> for Refusal {
    fn from(s: String) -> Self {
        Self::Said(s.into())
    }
}

impl PartialEq<&str> for Refusal {
    /// Its markdown is `other`.
    fn eq(&self, other: &&str) -> bool {
        self.markdown() == *other
    }
}

impl std::fmt::Display for Refusal {
    /// As plain text (see [`Said`]'s).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.said().fmt(f)
    }
}

impl std::error::Error for Refusal {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_front_end_draws_it_its_own_way() {
        let said = Said::new()
            .text("Gave")
            .user(Snowflake(7))
            .strong("Chair*")
            .then(".")
            .then(Said::md(" Try `/server plan`."));
        assert_eq!(said.markdown(), "Gave<@7>**Chair\\***. Try `/server plan`.");
        let named = said.with_names(&HashMap::from([(Snowflake(7), "Ada".to_owned())]));
        assert_eq!(named.plain(None), "GaveAdaChair*. Try `/server plan`.");
    }

    #[test]
    fn sentences_are_spaced_once() {
        let s = Said::from("Recorded.").then(" Gave them **X**.").then(Said::new()).then("(Note.)");
        assert_eq!(s.markdown(), "Recorded. Gave them **X**. (Note.)");
        assert_eq!(Said::new().then(" Hi.").markdown(), "Hi.");
    }

    #[test]
    fn older_markdown_is_lifted_into_parts() {
        let s = Said::md("⚠️ **Sec\\_retary** for <@!9> in <#5> at <t:60:F>, see `x` <b> 2 < 3 **open");
        assert_eq!(
            s.0,
            vec![
                Part::Text("⚠️ ".into()),
                Part::Strong("Sec_retary".into()),
                Part::Text(" for ".into()),
                Part::User { id: Snowflake(9), name: None },
                Part::Text(" in ".into()),
                Part::Channel(Snowflake(5)),
                Part::Text(" at ".into()),
                Part::Time(60),
                Part::Text(", see ".into()),
                Part::Code("x".into()),
                Part::Text(" <b> 2 < 3 **open".into()),
            ]
        );
        assert_eq!(s.plain(None), "⚠️ Sec_retary for user 9 in #5 at 1970-01-01 00:01 UTC, see `x` <b> 2 < 3 **open");
    }
}

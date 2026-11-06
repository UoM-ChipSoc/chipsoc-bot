//! Finding people by name: who "@ada" or "Ada Lovelace" is, and the names
//! of ids, from the server's member list. Front ends show names, not ids.

use std::collections::{BTreeSet, HashMap};

use anyhow::Result;
use chipsoc_config::Snowflake;
use chipsoc_discord::Rest;
use chipsoc_discord::wire::{id, names, s};
use serde::{Deserialize, Serialize};

use crate::Host;

/// Most people offered for one search.
const MOST_FOUND: usize = 10;

/// Someone in the server, as its member list shows them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Member {
    pub id: Snowflake,
    /// Their Discord username (`ada`), without the @.
    pub username: String,
    /// Nickname here, else display name, else username.
    pub name: String,
}

impl Member {
    fn of(m: &serde_json::Value) -> Option<Self> {
        let id = id(&m["user"]["id"])?;
        let username = s(&m["user"]["username"]);
        let name = chipsoc_discord::wire::shown_name(m).unwrap_or_default();
        Some(Self { id, username, name })
    }

    /// "Ada L. (@ada)", or just one of them when that's all there is.
    pub fn label(&self) -> String {
        match (self.name.as_str(), self.username.as_str()) {
            ("", "") => self.id.to_string(),
            (n, "") => n.to_owned(),
            (n, u) if n.is_empty() || n == u => format!("@{u}"),
            (n, u) => format!("{n} (@{u})"),
        }
    }

    /// `q` (lower case) is their username, nickname or display name.
    fn is(&self, q: &str, m: &serde_json::Value) -> bool {
        names(m).any(|n| n.to_lowercase() == q)
    }

    /// One of their names contains `q` (lower case).
    fn partly(m: &serde_json::Value, q: &str) -> bool {
        names(m).any(|n| n.to_lowercase().contains(q))
    }
}

/// Go through the member list (see [`Rest::members`]) until `each` says
/// stop (returns false). A failed read ends it.
async fn scan<H: Host>(h: &H, mut each: impl FnMut(&serde_json::Value) -> bool) -> Result<()> {
    for m in h.rest().members(h.guild()).await?.iter() {
        if !each(m) {
            break;
        }
    }
    Ok(())
}

/// Those of `ids` who are in the server now, with their names. Anyone
/// missing (left, or the list couldn't be read) is left out.
pub async fn members_of<H: Host>(h: &H, ids: impl IntoIterator<Item = Snowflake>) -> HashMap<Snowflake, Member> {
    let ids: BTreeSet<Snowflake> = ids.into_iter().collect();
    let mut out = HashMap::new();
    if ids.is_empty() {
        return out;
    }
    let read = scan(h, |m| {
        if let Some(found) = Member::of(m).filter(|f| ids.contains(&f.id)) {
            out.insert(found.id, found);
        }
        out.len() < ids.len()
    });
    if let Err(e) = read.await {
        tracing::warn!("reading the member list failed: {e:#}");
    }
    out
}

/// Who a search found.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Found {
    pub members: Vec<Member>,
    /// Their username, nickname or display name is the query itself (else
    /// one of them only contains it).
    pub exact: bool,
}

impl Found {
    /// The one person `query` means, if it's clear: the only exact match, or
    /// the one whose username it is (nicknames repeat, usernames don't).
    pub fn one(&self, query: &str) -> Result<Snowflake, Unresolved> {
        let q = query.trim().trim_start_matches('@').to_lowercase();
        if q.is_empty() {
            return Err(Unresolved::Blank);
        }
        let by_username: Vec<&Member> = self.members.iter().filter(|m| m.username.to_lowercase() == q).collect();
        match (self.members.as_slice(), by_username.as_slice()) {
            ([], _) => Err(Unresolved::Nobody(query.trim().to_owned())),
            (_, [one]) => Ok(one.id),
            ([one], _) if self.exact => Ok(one.id),
            _ => Err(Unresolved::Several(self.members.clone())),
        }
    }
}

/// People whose username, nickname or display name is `query` (with or
/// without the @, any case); if nobody's is, those whose name contains it.
pub async fn find_members<H: Host>(h: &H, query: &str) -> Result<Found> {
    let q = query.trim().trim_start_matches('@').to_lowercase();
    if q.is_empty() {
        return Ok(Found::default());
    }
    let (mut exact, mut partly) = (Vec::new(), Vec::new());
    scan(h, |m| {
        if let Some(found) = Member::of(m) {
            if found.is(&q, m) {
                exact.push(found);
            } else if partly.len() < MOST_FOUND && Member::partly(m, &q) {
                partly.push(found);
            }
        }
        true
    })
    .await?;
    let is_exact = !exact.is_empty();
    let mut members = if is_exact { exact } else { partly };
    members.truncate(MOST_FOUND);
    Ok(Found { members, exact: is_exact })
}

/// Why someone typed couldn't be pinned down to one person.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unresolved {
    /// Nobody given.
    Blank,
    /// Nobody in the server is called that.
    Nobody(String),
    /// Several people could be meant: pick one.
    Several(Vec<Member>),
    /// The member list couldn't be read.
    Failed(String),
}

impl std::fmt::Display for Unresolved {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Blank => write!(f, "Who? Give their @username or name."),
            Self::Nobody(q) => {
                write!(f, "Nobody in the server is called “{q}”. Check the spelling, or give their @username.")
            }
            Self::Several(m) => {
                let who: Vec<String> = m.iter().map(Member::label).collect();
                write!(f, "Which one? {}", who.join(", "))
            }
            Self::Failed(e) => write!(f, "Couldn't read the member list to find them: {e}"),
        }
    }
}

/// Who `input` means: an id or mention as it is, else the one member whose
/// username, nickname or display name it is. A name that only partly
/// matches is never taken on its own: it's offered to pick.
pub async fn resolve_user<H: Host>(h: &H, input: &str) -> Result<Snowflake, Unresolved> {
    if let Some(id) = Snowflake::parse_mention(input) {
        return Ok(id);
    }
    if input.trim().trim_start_matches('@').is_empty() {
        return Err(Unresolved::Blank);
    }
    find_members(h, input).await.map_err(|e| Unresolved::Failed(format!("{e:#}")))?.one(input)
}

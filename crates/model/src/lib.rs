//! Shared vocabulary: Discord ids, object kinds, and the reconcile [`Plan`].
//! No I/O; every other crate builds on it.

use std::fmt;

use serde::{Deserialize, Serialize};

pub mod perms;

/// A Discord id. Stored as a string in TOML/JSON, like Discord does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct Snowflake(pub u64);

impl fmt::Display for Snowflake {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl std::str::FromStr for Snowflake {
    type Err = std::num::ParseIntError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        s.parse().map(Snowflake)
    }
}

impl Snowflake {
    /// A user id as people type or paste it: `123`, `<@123>` or `<@!123>`,
    /// digits only.
    pub fn parse_mention(s: &str) -> Option<Self> {
        let s = s.trim();
        let id = s.strip_prefix("<@").and_then(|m| m.strip_suffix('>')).map_or(s, |m| m.strip_prefix('!').unwrap_or(m));
        if id.is_empty() || !id.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        id.parse().ok()
    }
}

impl Serialize for Snowflake {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Snowflake {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            S(String),
            N(u64),
        }
        match Raw::deserialize(d)? {
            Raw::S(s) => s.parse().map_err(serde::de::Error::custom),
            Raw::N(n) => Ok(Snowflake(n)),
        }
    }
}

/// What a plan step acts on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ObjectKind {
    Server,
    Role,
    Category,
    Channel,
    /// A bot-managed message in a channel.
    Post,
    /// A project's front page in the projects forum.
    ProjectPost,
    Onboarding,
    Screening,
    AutoMod,
    /// Who holds a role.
    Holders,
}

impl ObjectKind {
    /// As written in files and plan hashes (`project-post`, `auto-mod`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Server => "server",
            Self::Role => "role",
            Self::Category => "category",
            Self::Channel => "channel",
            Self::Post => "post",
            Self::ProjectPost => "project-post",
            Self::Onboarding => "onboarding",
            Self::Screening => "screening",
            Self::AutoMod => "auto-mod",
            Self::Holders => "holders",
        }
    }
}

impl fmt::Display for ObjectKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Server => "server",
            Self::Role => "role",
            Self::Category => "category",
            Self::Channel => "channel",
            Self::Post => "post",
            Self::ProjectPost => "project post",
            Self::Onboarding => "onboarding",
            Self::Screening => "screening",
            Self::AutoMod => "automod rule",
            Self::Holders => "holders of",
        })
    }
}

/// A field that can differ between config and Discord.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Field {
    Name,
    Description,
    Icon,
    Banner,
    VerificationLevel,
    ContentFilter,
    Notifications,
    SystemChannel,
    RulesChannel,
    UpdatesChannel,
    AfkChannel,
    Color,
    Hoist,
    Mentionable,
    Permissions,
    Overwrites,
    Type,
    Parent,
    Topic,
    Slowmode,
    Nsfw,
    UserLimit,
    Tags,
    /// Message text, onboarding prompts, rule settings: the object's body.
    Content,
    /// Who holds a role.
    Holders,
}

impl fmt::Display for Field {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = serde_json::to_value(self).ok().and_then(|v| v.as_str().map(str::to_owned)).unwrap_or_default();
        f.write_str(&s.replace('_', " "))
    }
}

/// One field that differs: Discord has `from`, config wants `to`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Change {
    pub field: Field,
    pub from: String,
    pub to: String,
}

/// Which side moved since the last apply, for an edit.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Cause {
    /// The config changed; this is the edit apply exists for.
    Config,
    /// Someone changed it in Discord; config is as last applied.
    Discord,
    /// Both changed: someone has to choose.
    Both,
    /// Not known (never applied with this version).
    #[default]
    Unknown,
}

/// Which list a [`Action::Reorder`] sorts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OrderScope {
    Roles,
    Categories,
    /// Channels inside one category (`None`: channels outside any category).
    Channels(Option<String>),
}

/// One step of a reconcile plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "kebab-case")]
pub enum Action {
    /// In config, not yet in Discord.
    Create { kind: ObjectKind, key: String },
    /// In both; `changes` lists what differs. Apply sends only those fields.
    Edit {
        kind: ObjectKind,
        key: String,
        id: Snowflake,
        changes: Vec<Change>,
        #[serde(default)]
        cause: Cause,
    },
    /// Sort a list into config order (keys, top first).
    Reorder { scope: OrderScope, from: Vec<String>, to: Vec<String> },
    /// Removed from config: move to the archive category, history kept.
    Archive { kind: ObjectKind, key: String, id: Snowflake },
    /// Removed for real (only with `--delete`).
    Delete { kind: ObjectKind, key: String, id: Snowflake },
    /// Can't be managed (e.g. a role above the bot's own).
    Skip { kind: ObjectKind, key: String, reason: String },
}

impl Action {
    /// Whether applying this step changes anything.
    pub fn mutates(&self) -> bool {
        !matches!(self, Self::Skip { .. })
    }

    /// The step's first line, without its details.
    pub fn headline(&self) -> String {
        self.to_string().lines().next().unwrap_or_default().to_owned()
    }

    /// The object a step is about, as `kind:key` (reorders have none).
    pub fn object(&self) -> Option<String> {
        match self {
            Self::Create { kind, key }
            | Self::Edit { kind, key, .. }
            | Self::Archive { kind, key, .. }
            | Self::Delete { kind, key, .. } => Some(object_key(*kind, key)),
            Self::Reorder { .. } | Self::Skip { .. } => None,
        }
    }

    /// Whether `self` asks for no more than `full` does (same step, a subset of its changes).
    pub fn within(&self, full: &Self) -> bool {
        match (self, full) {
            (
                Self::Edit { kind, key, id, changes, .. },
                Self::Edit { kind: k2, key: key2, id: id2, changes: c2, .. },
            ) => kind == k2 && key == key2 && id == id2 && changes.iter().all(|c| c2.contains(c)),
            _ => self == full,
        }
    }
}

/// How an object is named in [`Plan::hashes`] and the lockfile's `applied`.
pub fn object_key(kind: ObjectKind, key: &str) -> String {
    format!("{}:{key}", kind.as_str())
}

impl fmt::Display for Action {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Create { kind, key } => write!(f, "+ create {kind} {key}"),
            Self::Edit { kind, key, changes, cause, .. } => {
                write!(f, "~ edit {kind} {key}")?;
                match cause {
                    Cause::Discord => write!(f, " (changed in Discord)")?,
                    Cause::Both => write!(f, " (changed in Discord AND in config: check which to keep)")?,
                    Cause::Config | Cause::Unknown => {}
                }
                for c in changes {
                    write!(f, "\n    {}: {} → {}", c.field, c.from, c.to)?;
                }
                Ok(())
            }
            Self::Reorder { scope, from, to } => {
                let what = match scope {
                    OrderScope::Roles => "roles".to_owned(),
                    OrderScope::Categories => "categories".to_owned(),
                    OrderScope::Channels(Some(c)) => format!("channels in {c}"),
                    OrderScope::Channels(None) => "top-level channels".to_owned(),
                };
                let moves = moves(from, to);
                if moves.is_empty() {
                    return write!(f, "↕ reorder {what}: put new ones in place");
                }
                let shown: Vec<String> = moves.iter().take(MOVES_SHOWN).map(|m| m.to_string()).collect();
                write!(f, "↕ reorder {what}: {}", shown.join(", "))?;
                if moves.len() > MOVES_SHOWN {
                    write!(f, " (+{} more)", moves.len() - MOVES_SHOWN)?;
                }
                Ok(())
            }
            Self::Archive { kind, key, .. } => write!(f, "→ archive {kind} {key}"),
            Self::Delete { kind, key, .. } => write!(f, "- delete {kind} {key}"),
            Self::Skip { kind, key, reason } => write!(f, ". skip {kind} {key} ({reason})"),
        }
    }
}

/// Moves named in a reorder's one-line summary.
const MOVES_SHOWN: usize = 3;

/// One item a reorder moves, and where it lands: just above `above`, or
/// (when it ends up last among those that stay put) just below `below`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Move {
    pub key: String,
    pub above: Option<String>,
    pub below: Option<String>,
}

impl fmt::Display for Move {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (&self.above, &self.below) {
            (Some(a), _) => write!(f, "{} above {a}", self.key),
            (None, Some(b)) => write!(f, "{} below {b}", self.key),
            (None, None) => write!(f, "{} to the top", self.key),
        }
    }
}

/// What a reorder from `from` to `to` (top first) moves: the fewest items
/// that, moved, put the rest in order. Items only one side has are left out
/// (new ones are placed as they're made). Each lands next to an item that
/// stays put.
pub fn moves(from: &[String], to: &[String]) -> Vec<Move> {
    let shared: Vec<&String> = to.iter().filter(|k| from.contains(k)).collect();
    let rank: Vec<usize> = shared.iter().filter_map(|k| from.iter().position(|f| f == *k)).collect();
    // The longest run already in order (as ranks in `from`) stays put.
    let n = rank.len();
    let (mut len, mut prev) = (vec![1usize; n], vec![usize::MAX; n]);
    for i in 0..n {
        for j in 0..i {
            if rank[j] < rank[i] && len[j] + 1 > len[i] {
                len[i] = len[j] + 1;
                prev[i] = j;
            }
        }
    }
    let mut stays = vec![false; n];
    let mut at = (0..n).max_by_key(|&i| (len[i], std::cmp::Reverse(i)));
    while let Some(i) = at {
        stays[i] = true;
        at = (prev[i] != usize::MAX).then_some(prev[i]);
    }
    (0..n)
        .filter(|&i| !stays[i])
        .map(|i| Move {
            key: shared[i].clone(),
            above: (i + 1..n).find(|&j| stays[j]).map(|j| shared[j].clone()),
            below: (0..i).rev().find(|&j| stays[j]).map(|j| shared[j].clone()),
        })
        .map(|m| if m.above.is_some() { Move { below: None, ..m } } else { m })
        .collect()
}

/// Something the operator should know that isn't a step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "warning", rename_all = "kebab-case")]
pub enum Warning {
    /// In Discord, never in config. Left alone unless `--sweep`.
    Unmanaged {
        kind: ObjectKind,
        name: String,
        id: Snowflake,
    },
    /// The lockfile points at something that no longer exists; it will be recreated.
    Missing {
        kind: ObjectKind,
        key: String,
        id: Snowflake,
    },
    /// A role removed from config. Roles can't be archived, so it stays until `--delete`.
    RoleRemoved {
        key: String,
        id: Snowflake,
    },
    /// Discord won't let a bot manage roles at or above its own.
    BotRoleTooLow {
        bot_role: String,
        /// Names of the roles at or above it that it would manage.
        #[serde(default)]
        above: Vec<String>,
    },
    Note {
        text: String,
    },
}

impl fmt::Display for Warning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unmanaged { kind, name, .. } => write!(f, "{kind} {name} isn't in config; left as is"),
            Self::Missing { kind, key, .. } => write!(f, "{kind} {key} was deleted in Discord; it will be recreated"),
            Self::RoleRemoved { key, .. } => {
                write!(f, "role {key} was removed from config; kept (use --delete to remove it)")
            }
            Self::BotRoleTooLow { bot_role, above } => {
                let which = match above.as_slice() {
                    [] => "some roles".to_owned(),
                    [one] => one.clone(),
                    [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
                };
                write!(
                    f,
                    "the bot's role ({bot_role}) is below {which}, so the bot can't edit or move {}: Discord only lets \
                     a bot manage roles below its own. The server owner (or an admin above it) can drag the bot's \
                     role to the top in Server Settings → Roles",
                    if above.len() == 1 { "it" } else { "them" }
                )
            }
            Self::Note { text } => f.write_str(text),
        }
    }
}

/// Output of the diff: ordered steps plus warnings.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Plan {
    pub actions: Vec<Action>,
    pub warnings: Vec<Warning>,
    /// Hash of the Discord state and config the plan was made from; apply refuses a stale plan.
    #[serde(default)]
    pub fingerprint: String,
    /// What the plan was asked to do with orphans.
    #[serde(default)]
    pub delete: bool,
    #[serde(default)]
    pub sweep: bool,
    /// Each configured object's comparable state (`kind:key` → hash), recorded
    /// in the lockfile once applied, so later plans can tell who changed what.
    #[serde(default)]
    pub hashes: std::collections::BTreeMap<String, String>,
    /// The server's owner and the bot's own role when the plan was made, for
    /// what only the owner may apply.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<Snowflake>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bot_role: Option<Snowflake>,
    /// The bot's own role's key in the config, if it has one there.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bot_role_key: Option<String>,
}

impl Plan {
    /// Steps that change something.
    pub fn changes(&self) -> usize {
        self.actions.iter().filter(|a| a.mutates()).count()
    }

    pub fn is_empty(&self) -> bool {
        self.changes() == 0
    }
}

/// The whole SHA-256 of `data`, in hex: for secrets, where [`stable_hash`]'s
/// 64 bits would be too few.
pub fn sha256_hex(data: impl AsRef<[u8]>) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(data.as_ref()).iter().map(|b| format!("{b:02x}")).collect()
}

/// Short content hash that is stable across Rust versions and platforms
/// (unlike `DefaultHasher`), so it is safe to commit.
pub fn stable_hash(data: impl AsRef<[u8]>) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(data.as_ref())[..8].iter().map(|b| format!("{b:02x}")).collect()
}

/// Things kept under an id until used or `ttl` old, at most `max` at once:
/// plans waiting for their Apply button, and the like.
#[derive(Debug)]
pub struct Expiring<V> {
    ttl: std::time::Duration,
    max: usize,
    items: std::collections::HashMap<String, (std::time::Instant, V)>,
}

impl<V> Expiring<V> {
    pub fn new(ttl: std::time::Duration, max: usize) -> Self {
        Self { ttl, max, items: std::collections::HashMap::new() }
    }

    /// Keep `v` as `id` from `now`, first dropping what's expired and, while
    /// full, the oldest.
    pub fn put(&mut self, id: String, v: V, now: std::time::Instant) {
        self.make_room(now, |_| true, self.max);
        self.items.insert(id, (now, v));
    }

    /// Drop what's expired, then the oldest of those `among` picks while
    /// there are `cap` or more of them.
    pub fn make_room(&mut self, now: std::time::Instant, among: impl Fn(&V) -> bool, cap: usize) {
        let ttl = self.ttl;
        self.items.retain(|_, (at, _)| now.duration_since(*at) < ttl);
        while self.items.values().filter(|(_, v)| among(v)).count() >= cap.max(1) {
            let oldest = self.items.iter().filter(|(_, (_, v))| among(v)).min_by_key(|(_, (at, _))| *at);
            let Some(id) = oldest.map(|(id, _)| id.clone()) else { break };
            self.items.remove(&id);
        }
    }

    /// What's kept as `id`, if it hasn't expired, leaving it there.
    pub fn peek(&self, id: &str, now: std::time::Instant) -> Option<&V> {
        self.items.get(id).filter(|(at, _)| now.duration_since(*at) < self.ttl).map(|(_, v)| v)
    }

    /// What's kept as `id`, once, if it hasn't expired.
    pub fn take(&mut self, id: &str, now: std::time::Instant) -> Option<V> {
        let (at, v) = self.items.remove(id)?;
        (now.duration_since(at) < self.ttl).then_some(v)
    }
}

/// A Unix time (seconds) as `2026-11-04 13:05`, in UTC.
pub fn utc_minute(ts: i64) -> String {
    let (days, secs) = (ts.div_euclid(86_400), ts.rem_euclid(86_400));
    // Civil date from days since 1970-01-01 (Howard Hinnant's method).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02}", secs / 3600, secs % 3600 / 60)
}

#[cfg(test)]
mod tests {
    #[test]
    fn expiring_things_are_taken_once_and_make_room() {
        use std::time::{Duration, Instant};
        let t = Instant::now();
        let mut e = super::Expiring::new(Duration::from_secs(60), 2);
        e.put("a".into(), 1, t);
        e.put("b".into(), 2, t + Duration::from_secs(1));
        e.put("c".into(), 3, t + Duration::from_secs(2));
        assert_eq!(e.take("a", t), None, "the oldest made room");
        assert_eq!(e.peek("b", t), Some(&2));
        assert_eq!(e.take("b", t), Some(2));
        assert_eq!(e.take("b", t), None, "once");
        assert_eq!(e.take("c", t + Duration::from_secs(62)), None, "expired");
        e.put("x".into(), 10, t);
        e.put("y".into(), 11, t + Duration::from_secs(1));
        e.make_room(t, |v| *v == 10, 1);
        assert_eq!((e.peek("x", t), e.peek("y", t)), (None, Some(&11)), "only among those picked");
    }

    #[test]
    fn times_read_as_utc_dates() {
        assert_eq!(super::utc_minute(0), "1970-01-01 00:00");
        assert_eq!(super::utc_minute(951_782_400 + 3_660), "2000-02-29 01:01");
        assert_eq!(super::utc_minute(1_793_797_500), "2026-11-04 13:05");
        assert_eq!(super::utc_minute(-60), "1969-12-31 23:59");
    }

    #[test]
    fn user_ids_parse_as_typed_or_pasted() {
        for s in ["123", " 123 ", "<@123>", "<@!123>"] {
            assert_eq!(super::Snowflake::parse_mention(s), Some(super::Snowflake(123)), "{s}");
        }
        for s in ["", "ada", "<@>", "<@&123>", "+123", "<@123", "12 3"] {
            assert_eq!(super::Snowflake::parse_mention(s), None, "{s}");
        }
    }

    use super::*;

    #[test]
    fn snowflake_reads_strings_and_numbers() {
        #[derive(Deserialize, Serialize)]
        struct T {
            id: Snowflake,
        }
        assert_eq!(toml::from_str::<T>("id = \"42\"").unwrap().id, Snowflake(42));
        assert_eq!(toml::from_str::<T>("id = 42").unwrap().id, Snowflake(42));
        assert_eq!(toml::to_string(&T { id: Snowflake(7) }).unwrap().trim(), "id = \"7\"");
    }

    #[test]
    fn actions_render_for_humans() {
        let a = Action::Edit {
            kind: ObjectKind::Channel,
            key: "general".into(),
            id: Snowflake(1),
            changes: vec![Change { field: Field::UserLimit, from: "0".into(), to: "5".into() }],
            cause: Cause::Discord,
        };
        assert_eq!(a.to_string(), "~ edit channel general (changed in Discord)\n    user limit: 0 → 5");
        let r = Action::Reorder { scope: OrderScope::Roles, from: vec![], to: vec!["a".into(), "b".into()] };
        assert_eq!(r.to_string(), "↕ reorder roles: put new ones in place");
        let k = |s: &str| s.split(' ').map(str::to_owned).collect::<Vec<_>>();
        let r = Action::Reorder { scope: OrderScope::Roles, from: k("a b c d e"), to: k("e a b c d") };
        assert_eq!(r.to_string(), "↕ reorder roles: e above a");
        let r = Action::Reorder { scope: OrderScope::Roles, from: k("a b c d e f"), to: k("f e d c b a") };
        assert_eq!(r.to_string(), "↕ reorder roles: e below f, d below f, c below f (+2 more)");
    }

    #[test]
    fn moves_are_the_fewest_that_sort_the_rest() {
        let k = |s: &str| s.split(' ').map(str::to_owned).collect::<Vec<_>>();
        let m = moves(&k("chair treasurer secretary member"), &k("chair secretary treasurer member"));
        assert_eq!(m.len(), 1, "{m:?}");
        assert_eq!(m[0].to_string(), "treasurer above member");
        let m = moves(&k("a b c"), &k("b c a"));
        assert_eq!(m.iter().map(ToString::to_string).collect::<Vec<_>>(), ["a below c"]);
        assert!(moves(&k("a b"), &k("a new b")).is_empty(), "new items aren't moves");
        assert!(moves(&k("a b"), &k("a b")).is_empty());
    }

    #[test]
    fn plan_round_trips_as_json() {
        let p = Plan {
            actions: vec![Action::Create { kind: ObjectKind::Role, key: "member".into() }],
            warnings: vec![Warning::Note { text: "hi".into() }],
            fingerprint: "f".into(),
            ..Default::default()
        };
        let back: Plan = serde_json::from_str(&serde_json::to_string(&p).unwrap()).unwrap();
        assert_eq!(back, p);
    }
}

#[cfg(test)]
mod kind_tests {
    use super::*;

    #[test]
    fn as_str_is_how_serde_writes_it() {
        use ObjectKind::*;
        for k in [Server, Role, Category, Channel, Post, ProjectPost, Onboarding, Screening, AutoMod, Holders] {
            assert_eq!(serde_json::to_value(k).unwrap(), k.as_str(), "{k:?}");
        }
    }
}

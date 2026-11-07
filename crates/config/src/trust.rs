//! What the bot trusts from a config repo anyone with push access can change.
//!
//! - `lock.toml` maps keys to Discord ids and records what was last applied,
//!   which is how drift watch tells a change made in Discord from one made in
//!   the files. The bot keeps its own copy (in its database) and uses that:
//!   the repo's may only add keys for objects nobody else claims. A changed id
//!   is refused, and a removed key waits for an admin. A long post's later
//!   parts are only ever the bot's own record, never taken from the repo.
//! - Some settings decide who is an admin or where the bot reports
//!   ([`Sensitive`]). A pushed change to them waits for an admin to approve
//!   it; until then the bot keeps the ones approved last.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::bot::{DriftMode, Web};
use crate::recording::Recording;
use crate::{Config, Lock, Peer, Snowflake};

/// The settings a pushed commit can't change by itself.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Sensitive {
    pub admin_role: Option<String>,
    /// Roles of positions with `admin = true`.
    pub admin_positions: BTreeSet<String>,
    pub drift: DriftMode,
    pub web: Option<Web>,
    pub log_people: Option<String>,
    pub log_bot: Option<String>,
    pub key_env: String,
    pub peers: Vec<Peer>,
    /// members.toml for the admin role and admin positions' roles.
    pub holders: BTreeMap<String, Vec<Snowflake>>,
    /// `[recording]`: whether calls may be recorded, by whom, and what
    /// people in them are told. Left out when off, so older approvals match.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recording: Option<crate::recording::Recording>,
}

impl Sensitive {
    /// The sensitive settings `cfg` asks for.
    pub fn of(cfg: &Config) -> Self {
        let admin_positions: BTreeSet<String> =
            cfg.bot.positions.iter().filter(|p| p.admin).map(|p| p.role.clone()).collect();
        let mut s = Self {
            admin_role: cfg.bot.admin_role.clone(),
            admin_positions,
            drift: cfg.bot.drift.mode,
            web: cfg.bot.web.clone(),
            log_people: cfg.bot.log.people.clone(),
            log_bot: cfg.bot.log.bot.clone(),
            key_env: cfg.federation.key_env.clone(),
            peers: cfg.federation.peers.clone(),
            holders: BTreeMap::new(),
            recording: cfg.bot.recording.clone(),
        };
        for k in s.admin_keys() {
            if let Some(ids) = cfg.members.get(&k) {
                s.holders.insert(k, ids.clone());
            }
        }
        s
    }

    /// Role keys whose holders are admins: the admin role and admin positions' roles.
    fn admin_keys(&self) -> BTreeSet<String> {
        self.admin_role.iter().chain(&self.admin_positions).cloned().collect()
    }

    /// A short fingerprint, to tie an approval to what was shown.
    pub fn hash(&self) -> String {
        chipsoc_model::stable_hash(serde_json::to_string(self).unwrap_or_default())
    }

    /// Make `cfg` use these settings instead of its own. A position the
    /// approved settings don't list as admin isn't one; holders of admin roles
    /// are as approved.
    pub fn impose(&self, cfg: &mut Config) {
        cfg.bot.admin_role = self.admin_role.clone();
        for p in &mut cfg.bot.positions {
            p.admin = p.admin && self.admin_positions.contains(&p.role);
        }
        cfg.bot.drift.mode = self.drift;
        cfg.bot.web = self.web.clone();
        cfg.bot.log.people = self.log_people.clone();
        cfg.bot.log.bot = self.log_bot.clone();
        cfg.federation.key_env = self.key_env.clone();
        cfg.federation.peers = self.peers.clone();
        cfg.bot.recording = self.recording.clone();
        let mut keys = self.admin_keys();
        keys.extend(Self::of(cfg).admin_keys());
        for k in keys {
            match self.holders.get(&k) {
                Some(ids) => cfg.members.insert(k.clone(), ids.clone()),
                None => cfg.members.remove(&k),
            };
        }
        cfg.desired.holders.clear();
        crate::holders(&mut cfg.desired, &cfg.members, &cfg.projects, &cfg.bot);
    }

    /// What `new` changes, one line each, for an admin to approve; keys as written.
    pub fn changes(&self, new: &Self) -> Vec<String> {
        let key = |k: &str| format!("`{k}`");
        self.changes_named(new, &key, &key)
    }

    /// [`Sensitive::changes`], with roles and channels named by `role` and `channel`.
    pub fn changes_named(
        &self,
        new: &Self,
        role: &dyn Fn(&str) -> String,
        channel: &dyn Fn(&str) -> String,
    ) -> Vec<String> {
        let mut out = Vec::new();
        let named = |o: &Option<String>, f: &dyn Fn(&str) -> String| o.as_deref().map_or("(none)".to_owned(), f);
        if self.admin_role != new.admin_role {
            out.push(format!(
                "admin role (`admin_role`): {} → {}",
                named(&self.admin_role, role),
                named(&new.admin_role, role)
            ));
        }
        for r in new.admin_positions.difference(&self.admin_positions) {
            out.push(format!("{} becomes an admin position", role(r)));
        }
        for r in self.admin_positions.difference(&new.admin_positions) {
            out.push(format!("{} stops being an admin position", role(r)));
        }
        if self.drift != new.drift {
            out.push(format!("[drift] mode: {:?} → {:?}", self.drift, new.drift).to_lowercase());
        }
        if self.web != new.web {
            let w = |w: &Option<Web>| {
                w.as_ref().map_or("(none)".to_owned(), |w| {
                    format!("url {}, share_device {}", w.url, w.share_device.as_deref().unwrap_or("(none)"))
                })
            };
            out.push(format!("[web]: {} → {}", w(&self.web), w(&new.web)));
        }
        if self.log_people != new.log_people {
            out.push(format!(
                "people log (`log.people`): {} → {}",
                named(&self.log_people, channel),
                named(&new.log_people, channel)
            ));
        }
        if self.log_bot != new.log_bot {
            out.push(format!(
                "bot log (`log.bot`): {} → {}",
                named(&self.log_bot, channel),
                named(&new.log_bot, channel)
            ));
        }
        if self.key_env != new.key_env {
            out.push(format!("federation key_env: `{}` → `{}`", self.key_env, new.key_env));
        }
        let peer = |ps: &[Peer], name: &str| ps.iter().find(|p| p.name == name).cloned();
        let names: BTreeSet<&str> = self.peers.iter().chain(&new.peers).map(|p| p.name.as_str()).collect();
        for n in names {
            match (peer(&self.peers, n), peer(&new.peers, n)) {
                (None, Some(_)) => out.push(format!("partner **{n}** added")),
                (Some(_), None) => out.push(format!("partner **{n}** removed")),
                (Some(a), Some(b)) if a != b => {
                    let mut what = Vec::new();
                    if a.id != b.id {
                        what.push("id");
                    }
                    if a.keys().ne(b.keys()) {
                        what.push("keys");
                    }
                    if a.role_map != b.role_map {
                        what.push("role_map");
                    }
                    if a.send != b.send {
                        what.push("send");
                    }
                    if a.accept != b.accept {
                        what.push("accept");
                    }
                    if what.is_empty() {
                        what.push("settings");
                    }
                    out.push(format!("partner **{n}**: {} changed", what.join(", ")));
                }
                _ => {}
            }
        }
        if let Some(line) = recording_change(self.recording.as_ref(), new.recording.as_ref(), role, channel) {
            out.push(line);
        }
        let keys: BTreeSet<&String> = self.holders.keys().chain(new.holders.keys()).collect();
        for k in keys {
            let (a, b) =
                (self.holders.get(k).cloned().unwrap_or_default(), new.holders.get(k).cloned().unwrap_or_default());
            let added: Vec<String> = b.iter().filter(|u| !a.contains(u)).map(|u| format!("<@{u}>")).collect();
            let gone: Vec<String> = a.iter().filter(|u| !b.contains(u)).map(|u| format!("<@{u}>")).collect();
            let mut what = Vec::new();
            if !added.is_empty() {
                what.push(format!("give to {}", added.join(", ")));
            }
            if !gone.is_empty() {
                what.push(format!("take from {}", gone.join(", ")));
            }
            if !what.is_empty() {
                out.push(format!("{} (members.toml): {}", role(k), what.join("; ")));
            }
        }
        out
    }
}

/// What a change to `[recording]` does, in a line.
fn recording_change(
    old: Option<&Recording>,
    new: Option<&Recording>,
    role: &dyn Fn(&str) -> String,
    channel: &dyn Fn(&str) -> String,
) -> Option<String> {
    let who = |r: &Recording| {
        let roles: Vec<String> = r.who.iter().map(|k| role(k)).collect();
        if roles.is_empty() { "admins".to_owned() } else { format!("admins and {}", roles.join(", ")) }
    };
    match (old, new) {
        (None, None) => None,
        (None, Some(r)) => {
            let mut line =
                format!("[recording] turned on: {} may record calls; summaries in {}", who(r), channel(&r.channel));
            if Recording::transcription(Some(r)) != Recording::transcription(None) {
                line.push_str(&format!("; transcripts {}", Recording::transcription_text(Some(r))));
            }
            Some(line)
        }
        (Some(_), None) => Some("[recording] turned off".to_owned()),
        (Some(a), Some(b)) if a == b => None,
        (Some(a), Some(b)) => {
            let mut what = Vec::new();
            if a.who != b.who {
                what.push(format!("who may record: {} → {}", who(a), who(b)));
            }
            if a.channel != b.channel {
                what.push(format!("summaries: {} → {}", channel(&a.channel), channel(&b.channel)));
            }
            if (&a.notice, &a.join_notice) != (&b.notice, &b.join_notice) {
                what.push("what people in the call are told".to_owned());
            }
            if Recording::transcription(Some(a)) != Recording::transcription(Some(b)) {
                what.push(format!(
                    "transcripts made {} (were made {})",
                    Recording::transcription_text(Some(b)),
                    Recording::transcription_text(Some(a))
                ));
            }
            if a.language != b.language {
                let l = |r: &Recording| r.language.clone().unwrap_or_else(|| "detected".to_owned());
                what.push(format!("language: {} → {}", l(a), l(b)));
            }
            if (a.max_hours, a.max_gb, a.min_free_gb, a.max_total_gb)
                != (b.max_hours, b.max_gb, b.min_free_gb, b.max_total_gb)
            {
                what.push("limits".to_owned());
            }
            Some(format!("[recording]: {}", what.join("; ")))
        }
    }
}

/// One `lock.toml` entry, as `section.key`.
pub type Entry = String;

/// The lock the bot uses, and what it didn't take from the repo's.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LockReview {
    pub lock: Lock,
    /// Entries whose id the repo changed: the bot's is kept.
    pub refused: Vec<String>,
    /// Entries the repo removed: kept until an admin approves.
    pub unlinks: Vec<Entry>,
}

impl LockReview {
    pub fn is_clean(&self) -> bool {
        self.refused.is_empty() && self.unlinks.is_empty()
    }
}

/// The lock to use: `trusted` (the bot's own), plus keys the repo's lock
/// added for ids no key already has. What was last applied (and from which
/// commit) is only ever `trusted`'s.
pub fn review(repo: &Lock, trusted: &Lock) -> LockReview {
    let mut r = LockReview { lock: trusted.clone(), ..Default::default() };
    if repo.guild.is_some() && repo.guild != trusted.guild && trusted.guild.is_some() {
        r.refused.push("guild".into());
    }
    if r.lock.guild.is_none() {
        r.lock.guild = repo.guild;
    }
    section(&mut r, "roles", &repo.roles, |l| &mut l.roles);
    section(&mut r, "categories", &repo.categories, |l| &mut l.categories);
    section(&mut r, "channels", &repo.channels, |l| &mut l.channels);
    section(&mut r, "posts", &repo.posts, |l| &mut l.posts);
    section(&mut r, "project_posts", &repo.project_posts, |l| &mut l.project_posts);
    section(&mut r, "automod", &repo.automod, |l| &mut l.automod);
    section(&mut r, "assets", &repo.assets, |l| &mut l.assets);
    section(&mut r, "archived", &repo.archived, |l| &mut l.archived);
    r
}

/// A `lock.toml` entry, as [`review`] weighs the repo's copy against the bot's.
trait Pinned: Clone + PartialEq {
    /// Whether `repo` says this is the same Discord object. A long post's
    /// later parts don't count: only the bot records them.
    fn same(&self, repo: &Self) -> bool {
        self == repo
    }
    /// Whether `repo` (a new key's entry) names an object this one is or holds.
    fn claims(&self, repo: &Self) -> bool {
        self == repo
    }
    /// What's taken from the repo's copy for a key the bot hasn't: never a
    /// post's later parts (the bot edits and deletes those; it finds its own
    /// again by their text).
    fn as_taken(&self) -> Self {
        self.clone()
    }
}

impl Pinned for Snowflake {}
impl Pinned for crate::AssetRef {}

impl Pinned for crate::PostRef {
    fn same(&self, repo: &Self) -> bool {
        self.message == repo.message
    }
    fn claims(&self, repo: &Self) -> bool {
        self.message == repo.message || self.parts.contains(&repo.message)
    }
    fn as_taken(&self) -> Self {
        Self { message: self.message, parts: Vec::new() }
    }
}

impl Pinned for crate::ThreadRef {
    fn same(&self, repo: &Self) -> bool {
        (self.thread, self.message) == (repo.thread, repo.message)
    }
    fn claims(&self, repo: &Self) -> bool {
        let ours = [self.thread, self.message];
        [repo.thread, repo.message].iter().any(|id| ours.contains(id) || self.parts.contains(id))
    }
    fn as_taken(&self) -> Self {
        Self { thread: self.thread, message: self.message, parts: Vec::new() }
    }
}

fn section<V: Pinned>(
    r: &mut LockReview,
    name: &str,
    repo: &BTreeMap<String, V>,
    of: impl Fn(&mut Lock) -> &mut BTreeMap<String, V>,
) {
    let ours = of(&mut r.lock).clone();
    for (k, v) in repo {
        match ours.get(k) {
            Some(o) if !o.same(v) => r.refused.push(format!("{name}.{k}")),
            Some(_) => {}
            None if ours.values().any(|o| o.claims(v)) => r.refused.push(format!("{name}.{k}")),
            None => {
                of(&mut r.lock).insert(k.clone(), v.as_taken());
            }
        }
    }
    r.unlinks.extend(ours.keys().filter(|k| !repo.contains_key(*k)).map(|k| format!("{name}.{k}")));
}

/// Forget the entries an admin approved unlinking.
pub fn unlink(lock: &mut Lock, entries: &[Entry]) {
    for e in entries {
        let Some((section, key)) = e.split_once('.') else { continue };
        match section {
            "roles" => drop(lock.roles.remove(key)),
            "categories" => drop(lock.categories.remove(key)),
            "channels" => drop(lock.channels.remove(key)),
            "posts" => drop(lock.posts.remove(key)),
            "project_posts" => drop(lock.project_posts.remove(key)),
            "automod" => drop(lock.automod.remove(key)),
            "assets" => drop(lock.assets.remove(key)),
            "archived" => drop(lock.archived.remove(key)),
            _ => {}
        }
    }
}

/// What the bot keeps about a config, from its own database.
#[derive(Debug, Clone, Default)]
pub struct Trusted {
    /// The lock as the bot last saved it; `None` before it ever did.
    pub lock: Option<Lock>,
    /// The sensitive settings an admin approved last; `None` before any were.
    pub approved: Option<Sensitive>,
}

/// What [`Trusted::vet`] changed from the files.
#[derive(Debug, Clone, Default)]
pub struct Vetted {
    pub lock: LockReview,
    /// The files' sensitive settings, when they aren't the approved ones.
    pub held: Option<Sensitive>,
}

impl Trusted {
    /// Make `cfg` (as loaded from the files) use the trusted lock and the
    /// approved sensitive settings.
    pub fn vet(&self, cfg: &mut Config) -> Vetted {
        let mut out = Vetted::default();
        if let Some(t) = &self.lock {
            out.lock = review(&cfg.desired.lock, t);
            cfg.desired.lock = out.lock.lock.clone();
        }
        if let Some(a) = &self.approved {
            let new = Sensitive::of(cfg);
            if &new != a {
                a.impose(cfg);
                out.held = Some(new);
            }
        }
        out
    }

    /// Plan warnings for what was held back.
    pub fn notes(v: &Vetted) -> Vec<String> {
        let mut out = Vec::new();
        if !v.lock.refused.is_empty() {
            out.push(format!(
                "lock.toml in the repo changes the Discord id of {}; the bot keeps its own",
                v.lock.refused.join(", ")
            ));
        }
        if !v.lock.unlinks.is_empty() {
            out.push(format!(
                "lock.toml in the repo drops {}; kept until an admin approves it in the bot log",
                v.lock.unlinks.join(", ")
            ));
        }
        if v.held.is_some() {
            out.push(
                "the config changes who's an admin, partners, logs, drift watch or call recording; the approved \
                 settings are used until an admin approves them in the bot log"
                    .into(),
            );
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bot::Position;

    fn lock(roles: &[(&str, u64)]) -> Lock {
        let mut l = Lock::default();
        for (k, v) in roles {
            l.roles.insert((*k).into(), Snowflake(*v));
        }
        l.applied.hashes.insert("role:member".into(), "ours".into());
        l
    }

    #[test]
    fn a_pushed_lock_cant_repoint_ids_or_forge_what_was_applied() {
        let trusted = lock(&[("member", 10), ("admin", 11)]);
        let mut repo = lock(&[("member", 11), ("admin", 11), ("new", 12), ("dup", 10)]);
        repo.applied.hashes.insert("role:member".into(), "forged".into());
        repo.applied.commit = Some("abc".into());
        let r = review(&repo, &trusted);
        assert_eq!(r.lock.roles["member"], Snowflake(10), "a changed id is refused");
        assert_eq!(r.lock.roles["new"], Snowflake(12), "a new key for an unclaimed id is taken");
        assert!(!r.lock.roles.contains_key("dup"), "a new key for a claimed id isn't");
        assert_eq!(r.refused, ["roles.dup", "roles.member"]);
        assert_eq!(r.lock.applied, trusted.applied, "applied hashes are only ever the bot's");
        assert!(r.unlinks.is_empty());

        let gone = review(&lock(&[("member", 10)]), &trusted);
        assert_eq!(gone.unlinks, ["roles.admin"]);
        assert_eq!(gone.lock.roles["admin"], Snowflake(11), "kept until approved");
        let mut approved = gone.lock.clone();
        unlink(&mut approved, &gone.unlinks);
        assert!(!approved.roles.contains_key("admin"));
    }

    #[test]
    fn a_posts_later_parts_are_only_ever_the_bots() {
        use crate::{PostRef, ThreadRef};
        let t = |thread, parts: &[u64]| ThreadRef {
            thread: Snowflake(thread),
            message: Snowflake(thread),
            parts: parts.iter().copied().map(Snowflake).collect(),
        };
        let mut trusted = Lock::default();
        trusted.project_posts.insert("cpu".into(), t(20, &[21, 22]));
        trusted.posts.insert("rules".into(), PostRef { message: Snowflake(30), parts: vec![Snowflake(31)] });

        // The bot's own commit after a long post's apply: the same entries, parts and all.
        assert!(review(&trusted.clone(), &trusted).is_clean());
        // Parts differing alone (the repo's lags, or was edited) say nothing; the bot's are kept.
        let mut repo = trusted.clone();
        repo.project_posts.insert("cpu".into(), t(20, &[]));
        repo.posts.get_mut("rules").unwrap().parts = vec![Snowflake(99)];
        let r = review(&repo, &trusted);
        assert!(r.is_clean(), "{r:?}");
        assert_eq!(r.lock.project_posts["cpu"], trusted.project_posts["cpu"]);
        assert_eq!(r.lock.posts["rules"].parts, [Snowflake(31)]);
        // A new key's parts from the repo aren't taken, nor one claiming a message the bot holds.
        repo.project_posts.insert("new".into(), t(40, &[41]));
        repo.project_posts.insert("steal".into(), t(22, &[]));
        let r = review(&repo, &trusted);
        assert_eq!(r.lock.project_posts["new"], t(40, &[]));
        assert_eq!(r.refused, ["project_posts.steal"]);

        // Changing a post's thread or first message is still refused.
        let mut repo = trusted.clone();
        repo.project_posts.get_mut("cpu").unwrap().message = Snowflake(23);
        repo.posts.get_mut("rules").unwrap().message = Snowflake(32);
        let r = review(&repo, &trusted);
        assert_eq!(r.refused, ["posts.rules", "project_posts.cpu"]);
        assert_eq!(r.lock.project_posts["cpu"], trusted.project_posts["cpu"]);
        assert_eq!(r.lock.posts["rules"], trusted.posts["rules"]);
    }

    fn cfg() -> Config {
        let mut c = Config::default();
        c.bot.admin_role = Some("admin".into());
        c.bot.positions = vec![Position { role: "chair".into(), admin: true, ..Default::default() }];
        c.members.insert("chair".into(), vec![Snowflake(1)]);
        c.members.insert("mod".into(), vec![Snowflake(5)]);
        c
    }

    #[test]
    fn pushed_sensitive_settings_wait_for_approval() {
        let approved = Sensitive::of(&cfg());
        let t = Trusted { lock: None, approved: Some(approved.clone()) };
        let mut pushed = cfg();
        pushed.bot.admin_role = Some("member".into());
        pushed.bot.positions.push(Position { role: "mod".into(), admin: true, ..Default::default() });
        pushed.members.get_mut("chair").unwrap().push(Snowflake(9));
        pushed.bot.drift.mode = DriftMode::Undo;
        let v = t.vet(&mut pushed);
        let held = v.held.expect("held back");
        assert_eq!(pushed.bot.admin_role.as_deref(), Some("admin"));
        assert!(!pushed.bot.positions[1].admin, "a new admin position isn't one yet");
        assert_eq!(pushed.members["chair"], [Snowflake(1)]);
        assert_eq!(pushed.desired.holders["admin"], [Snowflake(1)].into(), "the admin role goes to approved people");
        assert_eq!(pushed.bot.drift.mode, DriftMode::Report);
        let lines = approved.changes(&held);
        assert!(lines.iter().any(|l| l.contains("admin role (`admin_role`): `admin` → `member`")), "{lines:?}");
        assert!(lines.iter().any(|l| l.contains("`mod` becomes an admin position")), "{lines:?}");
        assert!(lines.iter().any(|l| l.contains("give to <@9>")), "{lines:?}");
        assert!(lines.iter().any(|l| l.contains("[drift] mode: report → undo")), "{lines:?}");

        let mut same = cfg();
        same.members.get_mut("mod").unwrap().push(Snowflake(6));
        assert!(t.vet(&mut same).held.is_none(), "other changes go ahead");
        assert_eq!(same.members["mod"].len(), 2);
    }

    #[test]
    fn recording_waits_for_approval_and_older_approvals_still_match() {
        use crate::recording::Recording;
        let approved = Sensitive::of(&cfg());
        assert!(!serde_json::to_string(&approved).unwrap().contains("recording"), "off: as before");
        let t = Trusted { lock: None, approved: Some(approved.clone()) };
        let mut pushed = cfg();
        pushed.bot.recording =
            Some(Recording { who: vec!["member".into()], channel: "c".into(), ..Default::default() });
        let held = t.vet(&mut pushed).held.expect("held back");
        assert!(pushed.bot.recording.is_none(), "not on until approved");
        let lines = approved.changes(&held);
        assert_eq!(lines, ["[recording] turned on: admins and `member` may record calls; summaries in `c`"]);
        let mut changed = held.clone();
        changed.recording.as_mut().unwrap().notice = Some("Not recorded.".into());
        assert_eq!(held.changes(&changed), ["[recording]: what people in the call are told"]);
        // The card names exactly what would run.
        let mut whisper = held.clone();
        let r = whisper.recording.as_mut().unwrap();
        r.transcribe = Some(crate::recording::Engine::WhisperCpp);
        r.whisper = Some("/usr/local/bin/whisper-cli".into());
        r.model = Some("/usr/local/share/whisper/ggml-base.bin".into());
        let line = held.changes(&whisper).join("\n");
        assert!(
            line.contains(
                "running `/usr/local/bin/whisper-cli` with the model `/usr/local/share/whisper/ggml-base.bin`"
            ),
            "{line}"
        );
        let on = approved.changes(&whisper).join("\n");
        assert!(on.contains("turned on") && on.contains("`/usr/local/bin/whisper-cli`"), "{on}");
    }

    #[test]
    fn sharing_with_a_partner_waits_for_approval() {
        let mut base = cfg();
        base.federation.peers.push(Peer { name: "S".into(), id: "s".into(), ..Default::default() });
        let approved = Sensitive::of(&base);
        let t = Trusted { lock: None, approved: Some(approved.clone()) };
        let mut pushed = base.clone();
        pushed.federation.peers[0].send = vec!["announcements".into()];
        pushed.federation.peers[0].accept.events = Some(crate::EventLanding::On(true));
        let held = t.vet(&mut pushed).held.expect("held back");
        assert!(pushed.federation.peers[0].send.is_empty() && pushed.federation.peers[0].accept.is_empty());
        assert_eq!(approved.changes(&held), ["partner **S**: send, accept changed"]);
        // Peers without the new keys hash as before, so nothing approved earlier asks again.
        assert_eq!(serde_json::to_value(&base.federation.peers[0]).unwrap().as_object().unwrap().len(), 2);
    }
}

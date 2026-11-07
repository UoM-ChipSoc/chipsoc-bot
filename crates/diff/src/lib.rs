//! The reconcile kernel: compare what config wants with what Discord has and
//! list the steps to get there. Pure: no network, no files.
//!
//! Objects match by Discord id through the lockfile, so a rename is an edit,
//! never a delete and recreate. Order is compared as lists, not numbers.

use std::collections::{BTreeMap, BTreeSet};

use chipsoc_config::schema::*;
use chipsoc_config::template;
use chipsoc_config::{Config, projects, resolved_onboarding};
use chipsoc_model::{
    Action, Cause, Change, Field, ObjectKind, OrderScope, Plan, Snowflake, Warning, object_key, perms, stable_hash,
};
use serde_json::json;

#[cfg(test)]
mod tests;

/// How to treat objects that config doesn't (or no longer) describe.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Options {
    /// Delete removed objects instead of archiving them.
    pub delete: bool,
    /// Also archive channels config has never managed.
    pub sweep: bool,
    /// The bot's own role, to check it sits above the roles it manages.
    pub bot_role: Option<Snowflake>,
}

pub fn plan(cfg: &Config, live: &Snapshot, opts: Options) -> Plan {
    let plan = Plan { delete: opts.delete, sweep: opts.sweep, ..Plan::default() };
    let mut d = Diff { want: &cfg.desired, live, plan, opts, admin: cfg.bot.admin_role.as_deref() };
    d.roles();
    d.categories();
    d.channels();
    d.server();
    d.holders();
    d.orphans(cfg.bot.archive.as_deref());
    d.posts();
    d.project_posts(cfg);
    d.threads();
    d.settings();
    d.join_gate(cfg);
    d.unreadable();
    d.plan
}

/// Only who holds the roles in `keys` (those config manages): what applying
/// a change to who holds them needs, without a whole export. `live` needs
/// only its `members` and `holders`.
pub fn holders_plan(cfg: &Config, live: &Snapshot, keys: &[String]) -> Plan {
    let mut d = Diff {
        want: &cfg.desired,
        live,
        plan: Plan::default(),
        opts: Options::default(),
        admin: cfg.bot.admin_role.as_deref(),
    };
    d.holders_of(Some(keys));
    d.plan
}

struct Diff<'a> {
    want: &'a Snapshot,
    live: &'a Snapshot,
    plan: Plan,
    opts: Options,
    /// `admin_role`: taking it away is called out, and never from everyone.
    admin: Option<&'a str>,
}

fn ch(field: Field, from: impl ToString, to: impl ToString) -> Change {
    Change { field, from: from.to_string(), to: to.to_string() }
}

fn opt<T: ToString>(v: &Option<T>) -> String {
    v.as_ref().map_or_else(|| "(none)".into(), ToString::to_string)
}

/// Overwrites as comparable (target, allow bits, deny bits). Targets are ids;
/// a role not created yet is its key, so it always differs.
fn ow_set(ows: &[Overwrite], lock: &Lock) -> BTreeSet<(String, u64, u64)> {
    ows.iter()
        .map(|o| {
            let target = match (&o.role, o.member) {
                (Some(k), _) => lock.role(k).map_or_else(|| format!("new:{k}"), |i| i.to_string()),
                (None, Some(m)) => m.to_string(),
                _ => String::new(),
            };
            (target, perms::bits_or_all(&o.allow), perms::bits_or_all(&o.deny))
        })
        .filter(|(_, a, d)| *a != 0 || *d != 0)
        .collect()
}

fn ow_text(ows: &[Overwrite]) -> String {
    if ows.is_empty() {
        return "(none)".into();
    }
    let one = |o: &Overwrite| {
        let mut s = o.role.clone().or_else(|| o.member.map(|m| format!("member {m}"))).unwrap_or_default();
        for (sign, list) in [('+', &o.allow), ('-', &o.deny)] {
            for p in list {
                s.push_str(&format!(" {sign}{p}"));
            }
        }
        s
    };
    ows.iter().map(one).collect::<Vec<_>>().join("; ")
}

/// The first line that differs between two texts, shortened, as (old, new).
fn first_difference(old: &str, new: &str) -> (String, String) {
    let short = |s: &str| {
        let s = s.trim();
        if s.chars().count() > 60 {
            format!("{}…", s.chars().take(59).collect::<String>())
        } else {
            format!("\"{s}\"")
        }
    };
    let (mut a, mut b) = (old.lines(), new.lines());
    loop {
        match (a.next(), b.next()) {
            (Some(x), Some(y)) if x == y => {}
            (Some(x), Some(y)) => return (short(x), short(y)),
            (Some(x), None) => return (short(x), "(removed)".into()),
            (None, Some(y)) => return ("(nothing)".into(), short(y)),
            (None, None) => return ("…".into(), "…".into()),
        }
    }
}

/// What differs between two posts, by their signatures: the first line that
/// differs, part by part when either has more than one.
fn part_changes(have: &str, want: &str) -> Vec<Change> {
    let (h, w) = (template::signature_parts(have), template::signature_parts(want));
    if h.len() == 1 && w.len() == 1 {
        let (from, to) = first_difference(have, want);
        return vec![ch(Field::Content, from, to)];
    }
    let mut changes = Vec::new();
    if h.len() != w.len() {
        changes.push(ch(Field::Content, format!("{} parts", h.len()), format!("{} parts", w.len())));
    }
    for i in 0..h.len().max(w.len()) {
        let (a, b) = (h.get(i).copied().unwrap_or_default(), w.get(i).copied().unwrap_or_default());
        if a != b {
            let (from, to) = first_difference(a, b);
            changes.push(ch(Field::Content, format!("part {} changed: {from}", i + 1), to));
        }
    }
    changes
}

/// [`part_changes`] for project posts, with being pinned (or not) said on its own.
fn pinned_changes(have: &str, want: &str) -> Vec<Change> {
    let ((was, have), (is, want)) = (projects::unpinned(have), projects::unpinned(want));
    let mut changes = if have == want { Vec::new() } else { part_changes(have, want) };
    if was != is {
        let say = |p: bool| if p { "pinned" } else { "not pinned" };
        changes.push(ch(Field::Pinned, say(was), say(is)));
    }
    changes
}

/// A forum post's title as Discord keeps it (at most 100 characters).
pub fn thread_title(title: &str) -> String {
    title.chars().take(100).collect()
}

/// The tags of `names` that `forum` has (matched case-insensitively), as named.
fn forum_tags(forum: &Channel) -> impl Fn(&[String]) -> Vec<String> + '_ {
    let have: BTreeSet<String> = forum.tags.iter().map(|t| t.to_lowercase()).collect();
    move |names| names.iter().filter(|t| have.contains(&t.to_lowercase())).cloned().collect()
}

/// Who moved: compare both sides with what was last applied.
fn cause(applied: Option<&str>, want: &str, have: &str) -> Cause {
    match applied {
        Some(a) if a == have => Cause::Config,
        Some(a) if a == want => Cause::Discord,
        Some(_) => Cause::Both,
        None => Cause::Unknown,
    }
}

fn hash(v: &serde_json::Value) -> String {
    stable_hash(v.to_string())
}

/// Hash any serialisable value (for settings).
mod erased {
    pub trait Ser {
        fn json(&self) -> serde_json::Value;
    }
    impl<T: serde::Serialize> Ser for T {
        fn json(&self) -> serde_json::Value {
            serde_json::to_value(self).unwrap_or_default()
        }
    }
}

/// The parts of a role apply manages, comparable across config and Discord.
fn role_state(r: &Role) -> String {
    let color = r.color.as_ref().map(|c| c.to_lowercase());
    hash(&json!([r.name, color, r.hoist, r.mentionable, perms::bits_or_all(&r.permissions)]))
}

fn category_state(c: &Category, lock: &Lock) -> String {
    hash(&json!([c.name, ow_set(&c.overwrites, lock)]))
}

/// `sort`: the forum's sort order, when config sets it (else it isn't part of
/// the state, so hashes from before it could be set still match).
fn channel_state(c: &Channel, parent: Option<String>, lock: &Lock, sort: Option<Option<ForumSort>>) -> String {
    let text = !c.kind.is_voice();
    let mut state = json!([
        c.kind,
        parent,
        c.name,
        text.then_some(&c.topic),
        text.then_some(c.slowmode),
        c.nsfw,
        (!text).then(|| c.user_limit.unwrap_or(0)),
        c.kind.has_tags().then_some(&c.tags),
        ow_set(&c.overwrites, lock),
    ]);
    if let (Some(sort), Some(list)) = (sort, state.as_array_mut()) {
        list.push(json!(sort));
    }
    hash(&state)
}

/// The forum sort order config manages for `c`, if any.
fn managed_sort(c: &Channel) -> Option<ForumSort> {
    c.sort.filter(|_| c.kind.has_tags())
}

/// Text ↔ announcement is the only type change Discord allows in place.
fn convertible(a: ChannelKind, b: ChannelKind) -> bool {
    use ChannelKind::{Announcement, Text};
    matches!((a, b), (Text, Announcement) | (Announcement, Text))
}

/// Channel keys in the order Discord shows them: text-like first, then voice.
fn keys_in_order(chans: &[Channel]) -> Vec<String> {
    let (voice, text): (Vec<&Channel>, Vec<&Channel>) = chans.iter().partition(|c| c.kind.is_voice());
    text.into_iter().chain(voice).map(|c| c.key.clone()).collect()
}

impl Diff<'_> {
    fn push(&mut self, a: Action) {
        self.plan.actions.push(a);
    }

    fn note(&mut self, text: String) {
        self.plan.warnings.push(Warning::Note { text });
    }

    /// Record an object's wanted state, and queue an edit if Discord's differs.
    fn edit(&mut self, kind: ObjectKind, key: &str, id: Snowflake, changes: Vec<Change>, want: &str, have: &str) {
        let obj = object_key(kind, key);
        self.plan.hashes.insert(obj.clone(), want.to_owned());
        if !changes.is_empty() {
            let cause = cause(self.want.lock.applied.hashes.get(&obj).map(String::as_str), want, have);
            self.push(Action::Edit { kind, key: key.to_owned(), id, changes, cause });
        }
    }

    /// Record the wanted state of an object being created.
    fn wanted(&mut self, kind: ObjectKind, key: &str, want: &str) {
        self.plan.hashes.insert(object_key(kind, key), want.to_owned());
    }

    /// Match a desired key to a live object: `Some(id)` to compare, `None` once a create is queued.
    fn matched(&mut self, kind: ObjectKind, key: &str, id: Option<Snowflake>, exists: bool) -> Option<Snowflake> {
        match id {
            Some(id) if exists => return Some(id),
            Some(id) => self.plan.warnings.push(Warning::Missing { kind, key: key.to_owned(), id }),
            None => {}
        }
        self.push(Action::Create { kind, key: key.to_owned() });
        None
    }

    /// Reorder if the items both sides have are out of order, or new ones are coming.
    fn order(&mut self, scope: OrderScope, want: Vec<String>, live: Vec<String>, adding: bool) {
        let shared: Vec<&String> = want.iter().filter(|k| live.contains(k)).collect();
        let live_shared: Vec<&String> = live.iter().filter(|k| want.contains(k)).collect();
        if shared != live_shared || adding {
            self.push(Action::Reorder { scope, from: live, to: want });
        }
    }

    /// A live key translated to the desired lockfile's key for the same id.
    fn want_key(&self, id: Option<Snowflake>, of: impl Fn(&Lock, Snowflake) -> Option<&str>) -> Option<String> {
        id.and_then(|i| of(&self.want.lock, i)).map(str::to_owned)
    }

    fn roles(&mut self) {
        let live_order: Vec<Snowflake> =
            self.live.roles.roles.iter().filter_map(|r| self.live.lock.role(&r.key)).collect();
        let live_roles: BTreeMap<Snowflake, &Role> =
            self.live.roles.roles.iter().filter_map(|r| Some((self.live.lock.role(&r.key)?, r))).collect();
        let bot_rank = self.opts.bot_role.and_then(|b| live_order.iter().position(|i| *i == b));
        // Only roles it could manage count: another bot's role above it is fine.
        let manageable = |i: &Snowflake| live_roles.get(i).is_some_and(|r| !r.managed);
        // Roles at or above the bot's: Discord won't let it edit or move them.
        let above: Vec<Snowflake> =
            bot_rank.map(|r| live_order[..r].iter().copied().filter(manageable).collect()).unwrap_or_default();
        if !above.is_empty() {
            let name = self.opts.bot_role.and_then(|b| live_roles.get(&b)).map_or("bot".into(), |r| r.name.clone());
            let above = above.iter().filter_map(|i| live_roles.get(i)).map(|r| r.name.clone()).collect();
            self.plan.warnings.push(Warning::BotRoleTooLow { bot_role: name, above });
        }
        let mut created = false;
        for r in &self.want.roles.roles {
            let id = self.want.lock.role(&r.key);
            if r.managed {
                continue; // Discord owns it (the bot's role, boosters, integrations)
            }
            let exists = id.is_some_and(|i| live_roles.contains_key(&i));
            let want_state = role_state(r);
            let Some(id) = self.matched(ObjectKind::Role, &r.key, id, exists) else {
                self.wanted(ObjectKind::Role, &r.key, &want_state);
                created = true;
                continue;
            };
            let above_bot =
                live_order.iter().position(|i| *i == id).zip(bot_rank).is_some_and(|(rank, bot)| rank < bot);
            if above_bot {
                let reason = "above the bot's role, so Discord won't let it edit this".to_owned();
                self.push(Action::Skip { kind: ObjectKind::Role, key: r.key.clone(), reason });
                continue;
            }
            let l = live_roles[&id];
            let mut v = Vec::new();
            if r.name != l.name {
                v.push(ch(Field::Name, &l.name, &r.name));
            }
            let lower = |c: &Option<String>| c.as_ref().map(|c| c.to_lowercase());
            if lower(&r.color) != lower(&l.color) {
                v.push(ch(Field::Color, opt(&l.color), opt(&r.color)));
            }
            if r.hoist != l.hoist {
                v.push(ch(Field::Hoist, l.hoist, r.hoist));
            }
            if r.mentionable != l.mentionable {
                v.push(ch(Field::Mentionable, l.mentionable, r.mentionable));
            }
            let (want, have) = (perms::bits_or_all(&r.permissions), perms::bits_or_all(&l.permissions));
            if want != have {
                let list = |b| perms::names(b).join(" ");
                v.push(ch(Field::Permissions, format!("-{}", list(have & !want)), format!("+{}", list(want & !have))));
            }
            self.edit(ObjectKind::Role, &r.key, id, v, &want_state, &role_state(l));
        }
        // Discord-managed roles (the bot's own, boosters) can't be moved, so
        // they're left out, and so are roles above the bot's (the warning says so).
        let not_everyone = |k: &String| k != "everyone";
        let above_keys: Vec<&str> = above.iter().filter_map(|i| self.want.lock.role_key(*i)).collect();
        let movable = |k: &String| not_everyone(k) && !above_keys.contains(&k.as_str());
        let live_keys: Vec<String> = live_order
            .iter()
            .filter(|i| live_roles.get(i).is_some_and(|r| !r.managed))
            .filter_map(|i| self.want.lock.role_key(*i).map(str::to_owned))
            .filter(movable)
            .collect();
        let want_keys: Vec<String> =
            self.want.roles.roles.iter().filter(|r| !r.managed).map(|r| r.key.clone()).filter(movable).collect();
        self.order(OrderScope::Roles, want_keys, live_keys, created);
    }

    fn categories(&mut self) {
        let live: BTreeMap<Snowflake, &Category> = self
            .live
            .categories
            .iter()
            .filter_map(|c| Some((self.live.lock.category(&c.category.key)?, &c.category)))
            .collect();
        let mut created = false;
        for cf in &self.want.categories {
            let c = &cf.category;
            let id = self.want.lock.category(&c.key);
            let want_state = category_state(c, &self.want.lock);
            let Some(id) = self.matched(ObjectKind::Category, &c.key, id, id.is_some_and(|i| live.contains_key(&i)))
            else {
                self.wanted(ObjectKind::Category, &c.key, &want_state);
                created = true;
                continue;
            };
            let l = live[&id];
            let mut v = Vec::new();
            if c.name != l.name {
                v.push(ch(Field::Name, &l.name, &c.name));
            }
            if ow_set(&c.overwrites, &self.want.lock) != ow_set(&l.overwrites, &self.live.lock) {
                v.push(ch(Field::Overwrites, ow_text(&l.overwrites), ow_text(&c.overwrites)));
            }
            let have = category_state(l, &self.live.lock);
            self.edit(ObjectKind::Category, &c.key, id, v, &want_state, &have);
        }
        let want: Vec<String> = self.want.categories.iter().map(|c| c.category.key.clone()).collect();
        let live_keys: Vec<String> = self
            .live
            .categories
            .iter()
            .filter_map(|c| {
                let id = self.live.lock.category(&c.category.key);
                self.want_key(id, |l, i| l.categories.iter().find(|(_, v)| **v == i).map(|(k, _)| k.as_str()))
            })
            .collect();
        self.order(OrderScope::Categories, want, live_keys, created);
    }

    fn channels(&mut self) {
        let live: BTreeMap<Snowflake, (&Channel, Option<Snowflake>)> = self
            .live
            .channels()
            .filter_map(|(c, cat)| {
                let parent = cat.and_then(|x| self.live.lock.category(&x.key));
                Some((self.live.lock.channel(&c.key)?, (c, parent)))
            })
            .collect();
        let mut adding: BTreeSet<Option<String>> = BTreeSet::new();
        for (c, cat) in self.want.channels() {
            let cat_key = cat.map(|x| x.key.clone());
            let id = self.want.lock.channel(&c.key);
            let want_parent = cat_key.as_deref().and_then(|k| self.want.lock.category(k));
            let parent_text =
                want_parent.map(|p| p.to_string()).or_else(|| cat_key.as_ref().map(|k| format!("new:{k}")));
            let sort = managed_sort(c).map(Some);
            let want_state = channel_state(c, parent_text, &self.want.lock, sort);
            let Some(id) = self.matched(ObjectKind::Channel, &c.key, id, id.is_some_and(|i| live.contains_key(&i)))
            else {
                self.wanted(ObjectKind::Channel, &c.key, &want_state);
                adding.insert(cat_key);
                continue;
            };
            let (l, live_parent) = live[&id];
            if c.kind != l.kind && !convertible(c.kind, l.kind) {
                self.note(format!(
                    "channel {} changes from {} to {}, which Discord can't do in place: the old one is archived and a new one made",
                    c.key, l.kind, c.kind
                ));
                self.push(Action::Archive { kind: ObjectKind::Channel, key: c.key.clone(), id });
                self.push(Action::Create { kind: ObjectKind::Channel, key: c.key.clone() });
                self.wanted(ObjectKind::Channel, &c.key, &want_state);
                adding.insert(cat_key);
                continue;
            }
            let mut v = Vec::new();
            if c.kind != l.kind {
                v.push(ch(Field::Type, l.kind, c.kind));
            }
            if want_parent != live_parent || (cat_key.is_some() && want_parent.is_none()) {
                let from = self
                    .want_key(live_parent, |l, i| l.categories.iter().find(|(_, v)| **v == i).map(|(k, _)| k.as_str()));
                v.push(ch(Field::Parent, opt(&from), opt(&cat_key)));
                adding.insert(cat_key.clone());
            }
            if c.name != l.name {
                v.push(ch(Field::Name, &l.name, &c.name));
            }
            if !c.kind.is_voice() {
                if c.topic != l.topic {
                    v.push(ch(Field::Topic, opt(&l.topic), opt(&c.topic)));
                }
                if c.slowmode != l.slowmode {
                    v.push(ch(Field::Slowmode, l.slowmode, c.slowmode));
                }
            }
            if c.nsfw != l.nsfw {
                v.push(ch(Field::Nsfw, l.nsfw, c.nsfw));
            }
            if c.kind.is_voice() && c.user_limit.unwrap_or(0) != l.user_limit.unwrap_or(0) {
                v.push(ch(Field::UserLimit, l.user_limit.unwrap_or(0), c.user_limit.unwrap_or(0)));
            }
            if c.kind.has_tags() && c.tags != l.tags {
                v.push(ch(Field::Tags, l.tags.join(", "), c.tags.join(", ")));
            }
            if let Some(want) = managed_sort(c)
                && l.sort != Some(want)
            {
                v.push(ch(Field::Sort, l.sort.map_or("(Discord's default)", ForumSort::as_str), want.as_str()));
            }
            if ow_set(&c.overwrites, &self.want.lock) != ow_set(&l.overwrites, &self.live.lock) {
                v.push(ch(Field::Overwrites, ow_text(&l.overwrites), ow_text(&c.overwrites)));
            }
            let live_parent_text = live_parent.map(|p| p.to_string());
            let mut have = channel_state(l, live_parent_text.clone(), &self.live.lock, sort.map(|_| l.sort));
            // Sort newly set in config: what was applied before had none, and still matches without it.
            let obj = object_key(ObjectKind::Channel, &c.key);
            let unsorted = channel_state(l, live_parent_text, &self.live.lock, None);
            if sort.is_some() && self.want.lock.applied.hashes.get(&obj) == Some(&unsorted) {
                have = unsorted;
            }
            self.edit(ObjectKind::Channel, &c.key, id, v, &want_state, &have);
        }

        let mut scopes: Vec<(Option<String>, Vec<String>)> = self
            .want
            .categories
            .iter()
            .map(|cf| (Some(cf.category.key.clone()), keys_in_order(&cf.channels)))
            .collect();
        scopes.push((None, keys_in_order(&self.want.top_level.channels)));
        for (cat, want) in scopes {
            if want.is_empty() {
                continue;
            }
            let live_list: &[Channel] = match &cat {
                None => &self.live.top_level.channels,
                Some(k) => {
                    let cid = self.want.lock.category(k);
                    self.live
                        .categories
                        .iter()
                        .find(|c| cid.is_some() && self.live.lock.category(&c.category.key) == cid)
                        .map_or(&[], |c| c.channels.as_slice())
                }
            };
            let live_keys: Vec<String> = live_list
                .iter()
                .filter_map(|c| self.want_key(self.live.lock.channel(&c.key), |l, i| l.channel_key(i)))
                .collect();
            let adding_here = adding.contains(&cat);
            self.order(OrderScope::Channels(cat), want, live_keys, adding_here);
        }
    }

    fn server(&mut self) {
        let (w, l) = (&self.want.server.guild, &self.live.server.guild);
        let mut v = Vec::new();
        if w.name != l.name {
            v.push(ch(Field::Name, &l.name, &w.name));
        }
        if w.description.is_some() && w.description != l.description {
            v.push(ch(Field::Description, opt(&l.description), opt(&w.description)));
        }
        for (f, a, b) in [
            (Field::VerificationLevel, w.verification_level, l.verification_level),
            (Field::ContentFilter, w.explicit_content_filter, l.explicit_content_filter),
            (Field::Notifications, w.default_message_notifications, l.default_message_notifications),
        ] {
            if a.is_some() && a != b {
                v.push(ch(f, opt(&b), opt(&a)));
            }
        }
        for (f, a, b) in [
            (Field::SystemChannel, &w.system_channel, &l.system_channel),
            (Field::RulesChannel, &w.rules_channel, &l.rules_channel),
            (Field::UpdatesChannel, &w.updates_channel, &l.updates_channel),
            (Field::AfkChannel, &w.afk_channel, &l.afk_channel),
        ] {
            if a.is_some() && a != b {
                v.push(ch(f, opt(b), opt(a)));
            }
        }
        for (f, name, file) in [(Field::Icon, "icon", &w.icon), (Field::Banner, "banner", &w.banner)] {
            let Some(file) = file else { continue };
            let applied = self.want.lock.assets.get(name);
            let same_file = applied.map(|a| &a.file) == self.want.assets.get(name);
            let untouched = applied.map(|a| &a.discord) == self.live.assets.get(name);
            if !(same_file && untouched) {
                v.push(ch(f, if untouched { "an older file" } else { "a different image" }, file));
            }
        }
        // Compare only the settings config sets.
        let state = |g: &Guild| {
            let set = |x: bool| x.then_some(());
            hash(&json!([
                g.name,
                set(w.description.is_some()).map(|()| &g.description),
                set(w.verification_level.is_some()).map(|()| g.verification_level),
                set(w.explicit_content_filter.is_some()).map(|()| g.explicit_content_filter),
                set(w.default_message_notifications.is_some()).map(|()| g.default_message_notifications),
                set(w.system_channel.is_some()).map(|()| &g.system_channel),
                set(w.rules_channel.is_some()).map(|()| &g.rules_channel),
                set(w.updates_channel.is_some()).map(|()| &g.updates_channel),
                set(w.afk_channel.is_some()).map(|()| &g.afk_channel),
            ]))
        };
        let (want, have) = (state(w), state(l));
        let id = self.want.lock.guild.unwrap_or_default();
        self.edit(ObjectKind::Server, "server", id, v, &want, &have);
    }

    /// Who holds the roles config manages holders for.
    fn holders(&mut self) {
        self.holders_of(None);
    }

    /// [`Diff::holders`] for `only` these roles, if given.
    fn holders_of(&mut self, only: Option<&[String]>) {
        let want = self.want;
        for (key, people) in &want.holders {
            if only.is_some_and(|o| !o.contains(key)) {
                continue;
            }
            let want_set = people.clone();
            let Some(id) = want.lock.role(key) else { continue }; // being created: holders on the next run
            let have = self.live.holders.get(key).cloned().unwrap_or_default();
            for gone in want_set.iter().filter(|u| !self.live.members.is_empty() && !self.live.members.contains(u)) {
                self.note(format!("role {key}: <@{gone}> isn't in the server, so can't be given it"));
            }
            let list = |s: &BTreeSet<Snowflake>| {
                if s.is_empty() {
                    "(nobody)".to_owned()
                } else {
                    s.iter().map(|u| format!("<@{u}>")).collect::<Vec<_>>().join(" ")
                }
            };
            let want_set: BTreeSet<Snowflake> = want_set
                .into_iter()
                .filter(|u| self.live.members.is_empty() || self.live.members.contains(u))
                .collect();
            if self.admin == Some(key.as_str()) && !have.is_empty() {
                if want_set.is_empty() {
                    self.note(format!(
                        "⚠️ role {key} is the admin role and the config gives it to nobody here, so it's left with {}; list someone in members.toml",
                        list(&have)
                    ));
                    continue;
                }
                let losing: BTreeSet<Snowflake> = have.difference(&want_set).copied().collect();
                if !losing.is_empty() {
                    self.note(format!(
                        "⚠️ this takes the admin role ({key}) from {}: they hold no admin position and \
                         members.toml doesn't list them under `{key}`; add them there to keep it",
                        list(&losing)
                    ));
                }
            }
            let changes =
                if want_set == have { vec![] } else { vec![ch(Field::Holders, list(&have), list(&want_set))] };
            let (ws, hs) = (hash(&json!(want_set)), hash(&json!(have)));
            self.edit(ObjectKind::Holders, key, id, changes, &ws, &hs);
        }
    }

    /// Live objects no desired key accounts for: removed from config (archived,
    /// or deleted with `--delete`) or never managed (reported, or archived with `--sweep`).
    fn orphans(&mut self, archive: Option<&str>) {
        let wanted_channels: BTreeSet<&str> = self.want.channels().map(|(c, _)| c.key.as_str()).collect();
        let wanted_cats: BTreeSet<&str> = self.want.categories.iter().map(|c| c.category.key.as_str()).collect();
        let wanted_roles: BTreeSet<&str> = self.want.roles.roles.iter().map(|r| r.key.as_str()).collect();
        let archived: BTreeSet<Snowflake> = self.want.lock.archived.values().copied().collect();
        let archive_id = archive.and_then(|a| self.want.lock.category(a));
        let recreated: BTreeSet<Snowflake> = self
            .plan
            .actions
            .iter()
            .filter_map(|a| match a {
                Action::Archive { id, .. } => Some(*id),
                _ => None,
            })
            .collect();

        let mut found = Vec::new();
        for (c, cat) in self.live.channels() {
            let Some(id) = self.live.lock.channel(&c.key) else { continue };
            let in_archive = archive_id.is_some() && cat.and_then(|x| self.live.lock.category(&x.key)) == archive_id;
            if archived.contains(&id) || in_archive || recreated.contains(&id) {
                continue;
            }
            match self.want.lock.channel_key(id) {
                Some(k) if wanted_channels.contains(k) => {}
                Some(k) => found.push((ObjectKind::Channel, k.to_owned(), id, true)),
                None => found.push((ObjectKind::Channel, c.name.clone(), id, false)),
            }
        }
        for cf in &self.live.categories {
            let Some(id) = self.live.lock.category(&cf.category.key) else { continue };
            if archived.contains(&id) || Some(id) == archive_id {
                continue;
            }
            match self.want.lock.categories.iter().find(|(_, v)| **v == id).map(|(k, _)| k.as_str()) {
                Some(k) if wanted_cats.contains(k) => {}
                Some(k) => found.push((ObjectKind::Category, k.to_owned(), id, true)),
                None => found.push((ObjectKind::Category, cf.category.name.clone(), id, false)),
            }
        }
        for (kind, key, id, managed) in found {
            match (managed, self.opts.delete, self.opts.sweep) {
                (true, true, _) => self.push(Action::Delete { kind, key, id }),
                (true, false, _) | (false, _, true) => self.push(Action::Archive { kind, key, id }),
                (false, _, false) => self.plan.warnings.push(Warning::Unmanaged { kind, name: key, id }),
            }
        }
        for r in &self.live.roles.roles {
            let Some(id) = self.live.lock.role(&r.key) else { continue };
            if r.managed {
                continue;
            }
            match self.want.lock.role_key(id) {
                Some(k) if wanted_roles.contains(k) => {}
                Some(k) if self.opts.delete => {
                    self.push(Action::Delete { kind: ObjectKind::Role, key: k.to_owned(), id })
                }
                Some(k) => self.plan.warnings.push(Warning::RoleRemoved { key: k.to_owned(), id }),
                None => {
                    self.plan.warnings.push(Warning::Unmanaged { kind: ObjectKind::Role, name: r.name.clone(), id })
                }
            }
        }
    }

    /// Keys of roles and channels this plan creates: text mentioning them must be re-sent after.
    fn creating(&self) -> BTreeSet<(char, String)> {
        self.plan
            .actions
            .iter()
            .filter_map(|a| match a {
                Action::Create { kind: ObjectKind::Role, key } => Some(('@', key.clone())),
                Action::Create { kind: ObjectKind::Channel, key } => Some(('#', key.clone())),
                _ => None,
            })
            .collect()
    }

    fn posts(&mut self) {
        let creating = self.creating();
        for (key, src) in &self.want.posts {
            if self.live.unreadable.posts.contains(key) {
                continue;
            }
            let want = template::signature(&template::render_parts(src, &self.want.lock));
            let want_state = stable_hash(&want);
            self.wanted(ObjectKind::Post, key, &want_state);
            let mentions_new = ['@', '#']
                .into_iter()
                .any(|sigil| template::refs(src, sigil).into_iter().any(|k| creating.contains(&(sigil, k))));
            match (self.want.lock.posts.get(key), self.live.posts.get(key)) {
                (Some(p), Some(have)) => {
                    if *have != want || mentions_new {
                        let changes = if *have == want {
                            let (from, to) = ("mentions things this run makes", "sent again once they exist");
                            vec![ch(Field::Content, from, to)]
                        } else {
                            part_changes(have, &want)
                        };
                        let have_state = stable_hash(have);
                        self.edit(ObjectKind::Post, key, p.message, changes, &want_state, &have_state);
                    }
                }
                (Some(p), None) => {
                    self.plan.warnings.push(Warning::Missing {
                        kind: ObjectKind::Post,
                        key: key.clone(),
                        id: p.message,
                    });
                    self.push(Action::Create { kind: ObjectKind::Post, key: key.clone() });
                }
                (None, _) => self.push(Action::Create { kind: ObjectKind::Post, key: key.clone() }),
            }
        }
    }

    fn project_posts(&mut self, cfg: &Config) {
        let Some(forum) = cfg.bot.projects.as_ref().and_then(|p| self.want.channel(&p.forum)) else { return };
        let pinned = cfg.bot.projects.as_ref().and_then(|p| p.pinned.as_deref());
        let tagged = forum_tags(forum);
        // Discord pins one post per forum: the pinned one's step comes last,
        // after the one it takes over from is unpinned.
        let (last, first): (Vec<&Project>, Vec<&Project>) =
            cfg.projects.iter().partition(|p| Some(p.slug.as_str()) == pinned);
        for p in first.into_iter().chain(last).filter(|p| !self.live.unreadable.project_posts.contains(&p.slug)) {
            let post = projects::post(p, &self.want.lock, self.want.project_posts.get(&p.slug).map(String::as_str));
            let pin = Some(p.slug.as_str()) == pinned;
            let want = projects::pinned(projects::signature(&post.title, &tagged(&post.tags), &post.parts), pin);
            let want_state = stable_hash(&want);
            self.wanted(ObjectKind::ProjectPost, &p.slug, &want_state);
            match (self.want.lock.project_posts.get(&p.slug), self.live.project_posts.get(&p.slug)) {
                (Some(t), Some(have)) => {
                    if *have != want {
                        let changes = pinned_changes(have, &want);
                        self.edit(ObjectKind::ProjectPost, &p.slug, t.thread, changes, &want_state, &stable_hash(have));
                    }
                }
                _ => self.push(Action::Create { kind: ObjectKind::ProjectPost, key: p.slug.clone() }),
            }
        }
    }

    /// Forums' `[[channel.thread]]` posts: started, kept in step, and
    /// archived once taken out of the config.
    fn threads(&mut self) {
        let creating = self.creating();
        let mut wanted = BTreeSet::new();
        for (forum, _) in self.want.channels().filter(|(c, _)| c.kind.has_tags()) {
            let tagged = forum_tags(forum);
            for t in &forum.threads {
                let key = thread_key(&forum.key, &t.key);
                wanted.insert(key.clone());
                let Some(src) = self.want.threads.get(&key) else { continue };
                if self.live.unreadable.posts.contains(&key) {
                    continue;
                }
                let parts = template::render_parts(src, &self.want.lock);
                let want = projects::signature(&thread_title(&t.title), &tagged(&t.tags), &parts);
                let want_state = stable_hash(&want);
                self.wanted(ObjectKind::Thread, &key, &want_state);
                let mentions_new = ['@', '#']
                    .into_iter()
                    .any(|sigil| template::refs(src, sigil).into_iter().any(|k| creating.contains(&(sigil, k))));
                match (self.want.lock.posts.get(&key), self.live.threads.get(&key)) {
                    (Some(p), Some(have)) => {
                        if *have != want || mentions_new {
                            let changes = if *have == want {
                                let (from, to) = ("mentions things this run makes", "sent again once they exist");
                                vec![ch(Field::Content, from, to)]
                            } else {
                                part_changes(have, &want)
                            };
                            self.edit(ObjectKind::Thread, &key, p.message, changes, &want_state, &stable_hash(have));
                        }
                    }
                    (Some(p), None) => {
                        self.plan.warnings.push(Warning::Missing {
                            kind: ObjectKind::Thread,
                            key: key.clone(),
                            id: p.message,
                        });
                        self.push(Action::Create { kind: ObjectKind::Thread, key });
                    }
                    (None, _) => self.push(Action::Create { kind: ObjectKind::Thread, key }),
                }
            }
        }
        // Taken out of the config (still there in Discord): archived, never deleted.
        let gone: Vec<(String, Snowflake)> = self
            .want
            .lock
            .posts
            .iter()
            .filter(|(k, _)| k.contains('/') && !wanted.contains(*k))
            .filter(|(k, _)| self.live.threads.contains_key(*k))
            .map(|(k, p)| (k.clone(), p.message))
            .collect();
        for (key, id) in gone {
            self.push(Action::Archive { kind: ObjectKind::Thread, key, id });
        }
    }

    /// Say what the plan left out because this token couldn't read it.
    fn unreadable(&mut self) {
        let u = &self.live.unreadable;
        let mut gaps = Vec::new();
        if u.automod && !self.want.settings.automod.is_empty() {
            gaps.push("AutoMod rules (reading them needs Manage Server)".to_owned());
        }
        if u.onboarding && self.want.settings.onboarding.is_some() {
            gaps.push("onboarding".to_owned());
        }
        if u.screening && self.want.settings.screening.is_some() {
            gaps.push("the rules screen (member verification)".to_owned());
        }
        let posts: Vec<String> = u
            .posts
            .iter()
            .filter(|k| self.want.posts.contains_key(*k))
            .map(|k| format!("#{k}"))
            .chain(u.project_posts.iter().map(|s| format!("project {s}")))
            .chain(u.posts.iter().filter(|k| self.want.threads.contains_key(*k)).map(|k| format!("the forum post {k}")))
            .collect();
        if !posts.is_empty() {
            gaps.push(format!(
                "the bot's posts in {} (reading them needs View Channel and Read Message History there)",
                posts.join(", ")
            ));
        }
        if !gaps.is_empty() {
            self.plan
                .warnings
                .push(Warning::Note { text: format!("left out, as this token can't read them: {}", gaps.join("; ")) });
        }
    }

    /// Say so when this plan turns the join gate on: who stops seeing what.
    fn join_gate(&mut self, cfg: &Config) {
        let Some(j) = cfg.join.as_ref().filter(|j| j.gate) else { return };
        let Some(access) = &j.access_role else { return };
        let view = 1u64 << chipsoc_model::perms::VIEW_CHANNEL;
        let sees = |s: &Snapshot| s.role("everyone").is_some_and(|r| perms::bits_or_all(&r.permissions) & view != 0);
        if sees(self.live) && !sees(self.want) {
            let open: Vec<String> = j.open_channels.iter().map(|k| format!("#{k}")).collect();
            let after = if j.grandfather {
                "the bot gives it to members already here within minutes"
            } else {
                "members already here need it too: announce the flow, then run `/server join notify`"
            };
            self.plan.warnings.push(Warning::Note {
                text: format!(
                    "join gate: @everyone will only see {}; the rest needs `{access}`, given when someone finishes the join flow ({after})",
                    if open.is_empty() { "nothing".to_owned() } else { open.join(", ") }
                ),
            });
        }
    }

    fn settings(&mut self) {
        let (w, l) = (&self.want.settings, &self.live.settings);
        let automod_unreadable = self.live.unreadable.automod;
        let guild = self.want.lock.guild.unwrap_or_default();
        let state = |v: &dyn erased::Ser| hash(&v.json());
        let unreadable = &self.live.unreadable;
        let (onboarding, screening) = (
            w.onboarding.as_ref().filter(|_| !unreadable.onboarding),
            w.screening.as_ref().filter(|_| !unreadable.screening),
        );
        if let Some(o) = onboarding {
            let want = resolved_onboarding(o, self.want);
            let mut have = l.onboarding.clone().unwrap_or_default();
            have.default_channels.sort();
            // New or archived channels change "every public channel", so resend after them.
            let creating = self.creating();
            let mentions_new = o.prompts.iter().flat_map(|p| &p.options).any(|opt| {
                creating.contains(&('@', opt.role.clone()))
                    || opt.channels.iter().any(|c| creating.contains(&('#', c.clone())))
            });
            let channels_move = mentions_new
                || o.default_channels.is_empty()
                    && self.plan.actions.iter().any(|a| {
                        matches!(
                            a,
                            Action::Create { kind: ObjectKind::Channel, .. }
                                | Action::Archive { kind: ObjectKind::Channel, .. }
                        )
                    });
            let (ws, hs) = (state(&want), state(&have));
            let say = |o: &Onboarding| {
                format!("{} questions, {} default channels", o.prompts.len(), o.default_channels.len())
            };
            let changes =
                if want != have || channels_move { vec![ch(Field::Content, say(&have), say(&want))] } else { vec![] };
            self.edit(ObjectKind::Onboarding, "onboarding", guild, changes, &ws, &hs);
        }
        if let Some(sc) = screening {
            let say = |s: Option<&Screening>| format!("{} rules", s.map_or(0, |s| s.rules.len()));
            let changes = if l.screening.as_ref() == Some(sc) {
                vec![]
            } else {
                vec![ch(Field::Content, say(l.screening.as_ref()), say(Some(sc)))]
            };
            let (ws, hs) = (state(sc), state(&l.screening));
            self.edit(ObjectKind::Screening, "screening", guild, changes, &ws, &hs);
        }
        // Order of presets and actions doesn't matter to Discord.
        let norm = |r: &AutoModRule| {
            let mut r = r.clone();
            r.key.clear();
            r.presets.sort();
            r.actions.sort();
            r
        };
        for r in w.automod.iter().filter(|_| !automod_unreadable) {
            let ws = state(&norm(r));
            // By id when known; otherwise take over a rule with the same name.
            let by_id = self
                .want
                .lock
                .automod
                .get(&r.key)
                .and_then(|id| l.automod.iter().find(|x| self.live.lock.automod.get(&x.key) == Some(id)));
            match by_id.or_else(|| l.automod.iter().find(|x| x.name == r.name)) {
                Some(h) => {
                    let id = self.live.lock.automod.get(&h.key).copied().unwrap_or_default();
                    let changes = if norm(r) == norm(h) {
                        vec![]
                    } else {
                        vec![ch(Field::Content, "different settings", "config")]
                    };
                    self.edit(ObjectKind::AutoMod, &r.key, id, changes, &ws, &state(&norm(h)));
                }
                None => {
                    self.wanted(ObjectKind::AutoMod, &r.key, &ws);
                    self.push(Action::Create { kind: ObjectKind::AutoMod, key: r.key.clone() });
                }
            }
        }
    }
}

//! Carry out a [`Plan`]: each step sends only what it changes. The lockfile is
//! saved after every step that changes something, and a failed step doesn't
//! stop the rest; the report says what happened to each.

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use chipsoc_config::schema::*;
use std::collections::BTreeSet;

use chipsoc_config::template::{self, FORUM_POST_TITLE, Rendered};
use chipsoc_config::{Config, projects, resolved_onboarding};
use chipsoc_model::{Action, Change, Field, ObjectKind, OrderScope, Plan};
use serde_json::{Map, Value, json};

use crate::wire::{self, id, s};
use crate::{ARCHIVED_THREAD, ApiError, Method, Rest};

use crate::export::PINNED_THREAD;

/// What a long post's part no longer needed says, if the bot can't delete it.
const MOVED_UP: &str = "(moved up)";

/// Discord's message type for "… changed the post title", which a thread
/// rename leaves in the thread, authored by whoever renamed it.
pub const NAME_CHANGE: u64 = 4;

/// Delete the rename notices `me` (the bot) left among `thread`'s newest 100
/// messages. How many went.
pub async fn clear_rename_notices(rest: &impl Rest, thread: Snowflake, me: Snowflake) -> Result<usize> {
    let recent = rest.call(Method::Get, &format!("channels/{thread}/messages?limit=100"), None).await?;
    let notices: Vec<Snowflake> = recent
        .as_array()
        .into_iter()
        .flatten()
        .filter(|m| m["type"].as_u64() == Some(NAME_CHANGE) && id(&m["author"]["id"]) == Some(me))
        .filter_map(|m| id(&m["id"]))
        .collect();
    let mut gone = 0;
    for m in notices {
        match rest.call(Method::Delete, &format!("channels/{thread}/messages/{m}"), None).await {
            Ok(_) => gone += 1,
            Err(e) if ApiError::status_of(&e) == Some(404) => {}
            Err(e) => return Err(e),
        }
    }
    Ok(gone)
}

/// [`clear_rename_notices`] in every forum post the lockfile holds whose
/// title the bot sets: projects' and `[[channel.thread]]`s. For the notices
/// left by renames made before the bot deleted its own.
pub async fn clear_all_rename_notices(rest: &impl Rest, lock: &Lock) -> Result<usize> {
    let me = id(&rest.call(Method::Get, "users/@me", None).await?["id"]).context("no id for the bot")?;
    let threads = lock.project_posts.values().map(|t| t.thread);
    let threads = threads.chain(lock.posts.iter().filter(|(k, _)| is_thread_key(k)).map(|(_, p)| p.message));
    let mut gone = 0;
    for t in threads.collect::<BTreeSet<_>>() {
        match clear_rename_notices(rest, t, me).await {
            Ok(n) => gone += n,
            Err(e) if matches!(ApiError::status_of(&e), Some(403 | 404)) => {}
            Err(e) => return Err(e),
        }
    }
    Ok(gone)
}

/// Which post's lockfile entry: a channel's (by key), a project's (by slug)
/// or a forum's `[[channel.thread]]` (by `forum/key`, among the posts).
#[derive(Clone, Copy)]
enum Entry<'k> {
    Post(&'k str),
    Project(&'k str),
    Thread(&'k str),
}

/// Pages of a forum's archived posts read when looking for a lost one.
const ARCHIVED_PAGES: usize = 10;

/// The first message's part of `have` (what a post's first part said when
/// planned): a forum post's starts with its title and tags (and a pin mark).
fn first_of<'s>(post: Entry, have: &'s str) -> &'s str {
    match post {
        Entry::Post(_) => have,
        Entry::Project(_) | Entry::Thread(_) => projects::unpinned(have).1.splitn(3, '\n').nth(2).unwrap_or_default(),
    }
}

/// Each part's signature.
fn signatures(parts: &[Rendered]) -> Vec<String> {
    parts.iter().map(Rendered::signature).collect()
}

/// What happened to one step.
#[derive(Debug)]
pub struct Outcome {
    pub action: Action,
    pub result: Result<(), String>,
}

#[derive(Debug, Default)]
pub struct Report {
    pub outcomes: Vec<Outcome>,
    /// The audit-log entry the apply wrote, if it wrote one.
    pub audit_id: Option<i64>,
}

impl Report {
    pub fn failed(&self) -> impl Iterator<Item = &Outcome> {
        self.outcomes.iter().filter(|o| o.result.is_err())
    }
}

/// Where the executor reports progress and saves the lockfile.
pub trait Sink {
    fn saved(&mut self, lock: &Lock) -> impl std::future::Future<Output = Result<()>> + Send;
    fn step(&mut self, _action: &Action, _result: &Result<(), String>) {}
}

/// Run `plan`. `live` is the export it was planned from: who holds managed
/// roles is read from it, not paged through again for each role.
pub async fn execute(
    rest: &impl Rest,
    cfg: &Config,
    plan: &Plan,
    live: Option<&Snapshot>,
    lock: &mut Lock,
    sink: &mut impl Sink,
) -> Report {
    let mut report = Report::default();
    // Objects already in sync count as applied as they are.
    let touched: std::collections::BTreeSet<String> = plan.actions.iter().filter_map(Action::object).collect();
    for (obj, h) in &plan.hashes {
        if !touched.contains(obj) {
            lock.applied.hashes.insert(obj.clone(), h.clone());
        }
    }
    let me = std::sync::OnceLock::new();
    for action in plan.actions.iter().filter(|a| a.mutates()) {
        let mut ex = Exec { rest, cfg, lock, live, me: &me };
        let result = ex.run(action).await.map_err(|e| format!("{e:#}"));
        if result.is_ok()
            && let Some(obj) = action.object()
        {
            match (action, plan.hashes.get(&obj)) {
                (Action::Archive { .. } | Action::Delete { .. }, _) => {
                    lock.applied.hashes.remove(&obj);
                }
                (_, Some(h)) => {
                    lock.applied.hashes.insert(obj, h.clone());
                }
                _ => {}
            }
        }
        let result = match (result, sink.saved(lock).await) {
            (Ok(()), Err(e)) => Err(format!("done in Discord, but saving the lockfile failed: {e:#}")),
            (r, _) => r,
        };
        sink.step(action, &result);
        report.outcomes.push(Outcome { action: action.clone(), result });
    }
    report
}

struct Exec<'a, R> {
    rest: &'a R,
    cfg: &'a Config,
    lock: &'a mut Lock,
    live: Option<&'a Snapshot>,
    /// The bot's own user id, once asked.
    me: &'a std::sync::OnceLock<Snowflake>,
}

/// Every key's id, or an error naming the ones that don't exist yet.
fn resolve(keys: &[String], id_of: impl Fn(&str) -> Option<Snowflake>) -> Result<Vec<Snowflake>> {
    let missing: Vec<&str> = keys.iter().map(String::as_str).filter(|k| id_of(k).is_none()).collect();
    if !missing.is_empty() {
        bail!("not created yet: {}", missing.join(", "));
    }
    Ok(keys.iter().filter_map(|k| id_of(k)).collect())
}

/// "a", "a and b", "a, b and c".
fn and_list(items: &[String]) -> String {
    match items {
        [] => String::new(),
        [one] => one.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

fn has(changes: &[Change], f: Field) -> bool {
    changes.iter().any(|c| c.field == f)
}

fn guild(lock: &Lock) -> Result<Snowflake> {
    lock.guild.context("the lockfile doesn't say which server this is; run `export` or pass --guild")
}

fn want_role<'a>(cfg: &'a Config, key: &str) -> Result<&'a Role> {
    cfg.desired.role(key).with_context(|| format!("role `{key}` isn't in config"))
}

impl<R: Rest> Exec<'_, R> {
    async fn call(&self, method: Method, path: String, body: Option<Value>) -> Result<Value> {
        self.rest.call(method, &path, body).await
    }

    async fn run(&mut self, a: &Action) -> Result<()> {
        let g = guild(self.lock)?;
        match a {
            Action::Create { kind, key } => self.create(g, *kind, key).await,
            Action::Edit { kind, key, id, changes, .. } => self.edit(g, *kind, key, *id, changes).await,
            Action::Reorder { scope, to, .. } => self.reorder(g, scope, to).await,
            Action::Archive { kind, key, id } => self.archive(*kind, key, *id).await,
            Action::Delete { kind, key, id } => {
                match kind {
                    ObjectKind::Role => {
                        self.call(Method::Delete, format!("guilds/{g}/roles/{id}"), None).await?;
                        self.lock.roles.retain(|_, v| v != id);
                    }
                    ObjectKind::Channel | ObjectKind::Category => {
                        self.call(Method::Delete, format!("channels/{id}"), None).await?;
                        self.lock.channels.retain(|_, v| v != id);
                        self.lock.categories.retain(|_, v| v != id);
                    }
                    other => bail!("can't delete a {other} ({key})"),
                }
                Ok(())
            }
            Action::Skip { .. } => Ok(()),
        }
    }

    /// Give and take a role so exactly the configured people hold it.
    async fn holders(&mut self, g: Snowflake, key: &str, role: Snowflake) -> Result<()> {
        let d = &self.cfg.desired;
        let known = self.live.filter(|l| !l.members.is_empty()).and_then(|l| Some((l.holders.get(key)?, &l.members)));
        let (have, present) = match known {
            Some((have, present)) => (have.clone(), present.clone()),
            None => {
                let members = crate::export::members(self.rest, g).await?;
                let have = members.iter().filter(|(_, roles)| roles.contains(&role)).map(|(u, _)| *u).collect();
                (have, members.iter().map(|(u, _)| *u).collect())
            }
        };
        let want = d.holders.get(key).cloned().unwrap_or_default();
        if self.cfg.bot.admin_role.as_deref() == Some(key) && !want.iter().any(|u| present.contains(u)) {
            bail!("the config gives the admin role to nobody in the server, so it isn't taken from anyone");
        }
        let mut failed = Vec::new();
        for u in want.difference(&have).filter(|u| present.contains(u)) {
            if let Err(e) = self.call(Method::Put, format!("guilds/{g}/members/{u}/roles/{role}"), None).await {
                failed.push(format!("give <@{u}>: {e:#}"));
            }
        }
        for u in have.difference(&want) {
            if let Err(e) = self.call(Method::Delete, format!("guilds/{g}/members/{u}/roles/{role}"), None).await {
                failed.push(format!("take from <@{u}>: {e:#}"));
            }
        }
        if !failed.is_empty() {
            bail!("{}", failed.join("; "));
        }
        Ok(())
    }

    /// The one object at `list` with this name (and type, and parent) that no
    /// key claims: a create that reached Discord but not the lockfile, or a
    /// lockfile that was lost. Several such is refused rather than guessed, or
    /// made into one more.
    async fn unclaimed(
        &self,
        list: String,
        name: &str,
        kind: Option<u8>,
        parent: Option<Option<Snowflake>>,
    ) -> Result<Option<Snowflake>> {
        let items = self.call(Method::Get, list, None).await?;
        let claimed: std::collections::BTreeSet<Snowflake> =
            [&self.lock.roles, &self.lock.categories, &self.lock.channels]
                .into_iter()
                .flat_map(|m| m.values().copied())
                .chain(self.lock.archived.values().copied())
                .collect();
        let found: Vec<Snowflake> = items
            .as_array()
            .into_iter()
            .flatten()
            .filter(|x| x["name"] == name && kind.is_none_or(|k| x["type"].as_u64() == Some(k.into())))
            .filter(|x| parent.is_none_or(|p| id(&x["parent_id"]) == p))
            .filter_map(|x| id(&x["id"]))
            .filter(|i| !claimed.contains(i))
            .collect();
        match found[..] {
            [] => Ok(None),
            [one] => Ok(Some(one)),
            _ => bail!(
                "{} things named `{name}` are already there and lock.toml claims none of them; \
                 put the right one's id in lock.toml (or restore lock.toml.bak), or rename the others",
                found.len()
            ),
        }
    }

    async fn create(&mut self, g: Snowflake, kind: ObjectKind, key: &str) -> Result<()> {
        let want = &self.cfg.desired;
        match kind {
            ObjectKind::Role => {
                let r = want_role(self.cfg, key)?;
                let body = wire::role_json(r).map_err(|e| anyhow!(e))?;
                let made = match self.unclaimed(format!("guilds/{g}/roles"), &r.name, None, None).await? {
                    // Probably ours from a run that crashed before saving: take it over.
                    Some(existing) => {
                        self.call(Method::Patch, format!("guilds/{g}/roles/{existing}"), Some(body)).await?
                    }
                    None => self.call(Method::Post, format!("guilds/{g}/roles"), Some(body)).await?,
                };
                self.lock.roles.insert(key.to_owned(), id(&made["id"]).context("no id in reply")?);
            }
            ObjectKind::Category => {
                let c = &want.category(key).context("category not in config")?.category;
                let ows = wire::overwrites_json(&c.overwrites, self.lock).map_err(|e| anyhow!(e))?;
                let body = json!({ "name": c.name, "type": 4, "permission_overwrites": ows });
                let made = match self.unclaimed(format!("guilds/{g}/channels"), &c.name, Some(4), None).await? {
                    Some(existing) => self.call(Method::Patch, format!("channels/{existing}"), Some(body)).await?,
                    None => self.call(Method::Post, format!("guilds/{g}/channels"), Some(body)).await?,
                };
                self.lock.categories.insert(key.to_owned(), id(&made["id"]).context("no id in reply")?);
            }
            ObjectKind::Channel => {
                let (c, cat) = want.channels().find(|(c, _)| c.key == key).context("channel not in config")?;
                let parent = match cat {
                    Some(cat) => Some(
                        self.lock
                            .category(&cat.key)
                            .with_context(|| format!("category `{}` doesn't exist yet", cat.key))?,
                    ),
                    None => None,
                };
                let body = wire::channel_json(c, parent, self.lock, &Value::Null).map_err(|e| anyhow!(e))?;
                let kind = Some(c.kind.discord());
                let made = match self.unclaimed(format!("guilds/{g}/channels"), &c.name, kind, Some(parent)).await? {
                    Some(existing) => self.call(Method::Patch, format!("channels/{existing}"), Some(body)).await?,
                    None => self.call(Method::Post, format!("guilds/{g}/channels"), Some(body)).await?,
                };
                self.lock.channels.insert(key.to_owned(), id(&made["id"]).context("no id in reply")?);
            }
            ObjectKind::Post => self.post(key, None).await?,
            ObjectKind::ProjectPost => self.project_post(key, None).await?,
            ObjectKind::Thread => self.thread(key, None).await?,
            ObjectKind::Onboarding | ObjectKind::Screening | ObjectKind::Server | ObjectKind::Holders => {
                self.edit(g, kind, key, g, &[]).await?
            }
            ObjectKind::AutoMod => {
                let r = want.settings.automod.iter().find(|r| r.key == key).context("rule not in config")?;
                let body = wire::automod_json(r, self.lock).map_err(|e| anyhow!(e))?;
                let made = self.call(Method::Post, format!("guilds/{g}/auto-moderation/rules"), Some(body)).await?;
                self.lock.automod.insert(key.to_owned(), id(&made["id"]).context("no id in reply")?);
            }
        }
        Ok(())
    }

    async fn edit(
        &mut self,
        g: Snowflake,
        kind: ObjectKind,
        key: &str,
        oid: Snowflake,
        changes: &[Change],
    ) -> Result<()> {
        let want = &self.cfg.desired;
        match kind {
            ObjectKind::Role => {
                let r = want_role(self.cfg, key)?;
                let full = wire::role_json(r).map_err(|e| anyhow!(e))?;
                let mut patch = Map::new();
                for (f, k) in [
                    (Field::Name, "name"),
                    (Field::Color, "color"),
                    (Field::Hoist, "hoist"),
                    (Field::Mentionable, "mentionable"),
                    (Field::Permissions, "permissions"),
                ] {
                    if has(changes, f) {
                        patch.insert(k.into(), full[k].clone());
                    }
                }
                self.call(Method::Patch, format!("guilds/{g}/roles/{oid}"), Some(Value::Object(patch))).await?;
            }
            ObjectKind::Category => {
                let c = &want.category(key).context("category not in config")?.category;
                let mut patch = Map::new();
                if has(changes, Field::Name) {
                    patch.insert("name".into(), json!(c.name));
                }
                if has(changes, Field::Overwrites) {
                    patch.insert(
                        "permission_overwrites".into(),
                        wire::overwrites_json(&c.overwrites, self.lock).map_err(|e| anyhow!(e))?,
                    );
                }
                self.call(Method::Patch, format!("channels/{oid}"), Some(Value::Object(patch))).await?;
            }
            ObjectKind::Channel => {
                let (c, cat) = want.channels().find(|(c, _)| c.key == key).context("channel not in config")?;
                let parent = match cat {
                    Some(cat) => Some(
                        self.lock
                            .category(&cat.key)
                            .with_context(|| format!("category `{}` doesn't exist yet", cat.key))?,
                    ),
                    None => None,
                };
                let live_tags = if has(changes, Field::Tags) {
                    self.call(Method::Get, format!("channels/{oid}"), None).await?["available_tags"].clone()
                } else {
                    Value::Null
                };
                let full = wire::channel_json(c, parent, self.lock, &live_tags).map_err(|e| anyhow!(e))?;
                let mut patch = Map::new();
                for (f, k) in [
                    (Field::Name, "name"),
                    (Field::Type, "type"),
                    (Field::Topic, "topic"),
                    (Field::Slowmode, "rate_limit_per_user"),
                    (Field::Nsfw, "nsfw"),
                    (Field::UserLimit, "user_limit"),
                    (Field::Tags, "available_tags"),
                    (Field::Sort, "default_sort_order"),
                    (Field::Overwrites, "permission_overwrites"),
                    (Field::Parent, "parent_id"),
                ] {
                    if has(changes, f) {
                        patch.insert(k.into(), full[k].clone());
                    }
                }
                self.call(Method::Patch, format!("channels/{oid}"), Some(Value::Object(patch))).await?;
            }
            ObjectKind::Server => self.server(g, changes).await?,
            ObjectKind::Post => self.post(key, Some(oid)).await?,
            ObjectKind::ProjectPost => {
                let existing = self.lock.project_posts.get(key).cloned();
                self.project_post(key, existing).await?;
            }
            ObjectKind::Thread => {
                let existing = self.thread_ref(Entry::Thread(key));
                self.thread(key, existing).await?;
            }
            ObjectKind::Onboarding => {
                let o = want.settings.onboarding.as_ref().context("no onboarding in config")?;
                let body = wire::onboarding_json(&resolved_onboarding(o, want), self.lock);
                self.call(Method::Put, format!("guilds/{g}/onboarding"), Some(body)).await?;
            }
            ObjectKind::Screening => {
                let sc = want.settings.screening.as_ref().context("no screening in config")?;
                self.call(Method::Patch, format!("guilds/{g}/member-verification"), Some(wire::screening_json(sc)))
                    .await?;
            }
            ObjectKind::Holders => self.holders(g, key, oid).await?,
            ObjectKind::AutoMod => {
                let r = want.settings.automod.iter().find(|r| r.key == key).context("rule not in config")?;
                let body = wire::automod_json(r, self.lock).map_err(|e| anyhow!(e))?;
                match self.call(Method::Patch, format!("guilds/{g}/auto-moderation/rules/{oid}"), Some(body)).await {
                    Err(e) if ApiError::status_of(&e) == Some(404) => {
                        bail!(
                            "Discord doesn't let bots change its built-in rules; edit it in Server Settings → AutoMod"
                        )
                    }
                    r => r?,
                };
                self.lock.automod.insert(key.to_owned(), oid);
            }
        }
        Ok(())
    }

    async fn server(&mut self, g: Snowflake, changes: &[Change]) -> Result<()> {
        let w = &self.cfg.desired.server.guild;
        let mut patch = Map::new();
        let chan = |k: &Option<String>| -> Result<Option<String>> {
            match k.as_deref() {
                None => Ok(None),
                Some(k) => Ok(Some(
                    self.lock.channel(k).with_context(|| format!("channel `{k}` doesn't exist yet"))?.to_string(),
                )),
            }
        };
        for c in changes {
            let (k, v) = match c.field {
                Field::Name => ("name", json!(w.name)),
                Field::Description => ("description", json!(w.description)),
                Field::VerificationLevel => ("verification_level", json!(w.verification_level)),
                Field::ContentFilter => ("explicit_content_filter", json!(w.explicit_content_filter)),
                Field::Notifications => ("default_message_notifications", json!(w.default_message_notifications)),
                Field::SystemChannel => ("system_channel_id", json!(chan(&w.system_channel)?)),
                Field::RulesChannel => ("rules_channel_id", json!(chan(&w.rules_channel)?)),
                Field::UpdatesChannel => ("public_updates_channel_id", json!(chan(&w.updates_channel)?)),
                Field::AfkChannel => ("afk_channel_id", json!(chan(&w.afk_channel)?)),
                Field::Icon => ("icon", json!(self.image(&w.icon)?)),
                Field::Banner => ("banner", json!(self.image(&w.banner)?)),
                _ => continue,
            };
            patch.insert(k.into(), v);
        }
        let reply = self.call(Method::Patch, format!("guilds/{g}"), Some(Value::Object(patch))).await?;
        for name in ["icon", "banner"] {
            if has(changes, if name == "icon" { Field::Icon } else { Field::Banner })
                && let (Some(file), Some(discord)) = (self.cfg.desired.assets.get(name), reply[name].as_str())
            {
                self.lock.assets.insert(name.into(), AssetRef { file: file.clone(), discord: discord.into() });
            }
        }
        Ok(())
    }

    /// An image file as the data URI Discord wants.
    fn image(&self, file: &Option<String>) -> Result<String> {
        let f = file.as_deref().context("no image file set")?;
        let bytes = std::fs::read(chipsoc_config::inside(&self.cfg.dir, f)?).with_context(|| format!("reading {f}"))?;
        let mime = match f.rsplit('.').next().map(str::to_lowercase).as_deref() {
            Some("png") => "image/png",
            Some("jpg" | "jpeg") => "image/jpeg",
            Some("gif") => "image/gif",
            Some("webp") => "image/webp",
            _ => bail!("{f}: use a .png, .jpg, .gif or .webp image"),
        };
        Ok(format!("data:{mime};base64,{}", base64::engine::general_purpose::STANDARD.encode(bytes)))
    }

    /// The bot's own user id.
    async fn me(&self) -> Result<Snowflake> {
        if let Some(me) = self.me.get() {
            return Ok(*me);
        }
        let me = id(&self.call(Method::Get, "users/@me".into(), None).await?["id"]).context("no id for the bot")?;
        Ok(*self.me.get_or_init(|| me))
    }

    /// Every message and thread the lockfile holds for posts, every part.
    fn claimed(&self) -> BTreeSet<Snowflake> {
        let posts = self.lock.posts.values().flat_map(|p| std::iter::once(p.message).chain(p.parts.iter().copied()));
        let projects = self
            .lock
            .project_posts
            .values()
            .flat_map(|t| [t.thread, t.message].into_iter().chain(t.parts.iter().copied()));
        posts.chain(projects).collect()
    }

    /// A message the bot posted in `ch` that says exactly `want`: the managed
    /// post, when the lockfile lost track of it.
    async fn find_post(&self, ch: Snowflake, want: &Rendered) -> Result<Option<Snowflake>> {
        let me = self.me().await?;
        let recent = self.call(Method::Get, format!("channels/{ch}/messages?limit=100"), None).await?;
        let claimed = self.claimed();
        Ok(recent
            .as_array()
            .into_iter()
            .flatten()
            .filter(|m| id(&m["author"]["id"]) == Some(me) && wire::rendered_from(m) == *want)
            .filter_map(|m| id(&m["id"]))
            .find(|m| !claimed.contains(m)))
    }

    /// The bot's messages in `ch` after `first` that say `want`, in order: a
    /// long post's later parts, when the lockfile lost track of them. Stops
    /// at the first part not found (it and the rest are posted again).
    async fn find_later(&self, ch: Snowflake, first: Snowflake, want: &[Rendered]) -> Result<Vec<Snowflake>> {
        if want.is_empty() {
            return Ok(Vec::new());
        }
        let me = self.me().await?;
        let claimed = self.claimed();
        let after = self.call(Method::Get, format!("channels/{ch}/messages?after={first}&limit=100"), None).await?;
        let mut mine: Vec<(Snowflake, Rendered)> = after
            .as_array()
            .into_iter()
            .flatten()
            .filter(|m| id(&m["author"]["id"]) == Some(me))
            .filter_map(|m| Some((id(&m["id"])?, wire::rendered_from(m))))
            .filter(|(m, _)| !claimed.contains(m) && *m > first)
            .collect();
        mine.sort_by_key(|(m, _)| *m);
        let mut ids = Vec::new();
        let mut from = 0;
        for w in want {
            let Some(j) = mine[from..].iter().position(|(_, r)| r == w) else { break };
            ids.push(mine[from + j].0);
            from += j + 1;
        }
        Ok(ids)
    }

    /// What Discord had in each part of a post when this plan was made (as
    /// signatures), if it was read with the message ids the lockfile has now.
    fn have(&self, post: Entry) -> Vec<String> {
        let Some(live) = self.live else { return Vec::new() };
        let (sig, same) = match post {
            Entry::Post(k) => (live.posts.get(k), live.lock.posts.get(k) == self.lock.posts.get(k)),
            Entry::Project(s) => {
                (live.project_posts.get(s), live.lock.project_posts.get(s) == self.lock.project_posts.get(s))
            }
            Entry::Thread(k) => (live.threads.get(k), live.lock.posts.get(k) == self.lock.posts.get(k)),
        };
        match sig {
            Some(sig) if same => template::signature_parts(sig).into_iter().map(str::to_owned).collect(),
            _ => Vec::new(),
        }
    }

    /// A post's later parts as the lockfile has them.
    fn parts_of(&self, post: Entry) -> Vec<Snowflake> {
        match post {
            Entry::Post(k) | Entry::Thread(k) => self.lock.posts.get(k).map(|p| p.parts.clone()),
            Entry::Project(s) => self.lock.project_posts.get(s).map(|t| t.parts.clone()),
        }
        .unwrap_or_default()
    }

    /// A forum post's entry as a [`ThreadRef`] (a `[[channel.thread]]`'s first
    /// message has its thread's id).
    fn thread_ref(&self, post: Entry) -> Option<ThreadRef> {
        match post {
            Entry::Project(s) => self.lock.project_posts.get(s).cloned(),
            Entry::Post(k) | Entry::Thread(k) => self.lock.posts.get(k).map(|p| ThreadRef {
                thread: p.message,
                message: p.message,
                parts: p.parts.clone(),
            }),
        }
    }

    fn set_thread_ref(&mut self, post: Entry, t: ThreadRef) {
        match post {
            Entry::Project(s) => drop(self.lock.project_posts.insert(s.to_owned(), t)),
            Entry::Post(k) | Entry::Thread(k) => {
                drop(self.lock.posts.insert(k.to_owned(), PostRef { message: t.thread, parts: t.parts }))
            }
        }
    }

    /// Bring a post's later parts (the bot's messages in `ch` after the
    /// first) in line with `want`, and keep their ids in the lockfile.
    /// `old` are the ones there; `have`, what each said, when known.
    async fn finish(
        &mut self,
        post: Entry<'_>,
        ch: Snowflake,
        old: &[Snowflake],
        want: &[Rendered],
        have: &[String],
    ) -> Result<()> {
        let (ids, result) = self.later(ch, old, want, have).await;
        match post {
            Entry::Post(k) | Entry::Thread(k) => self.lock.posts.get_mut(k).map(|p| p.parts = ids),
            Entry::Project(s) => self.lock.project_posts.get_mut(s).map(|t| t.parts = ids),
        };
        result
    }

    /// Edit the later parts there in place (those unchanged left alone), post
    /// the ones missing after them, and take away the ones no longer needed.
    /// One found deleted is posted again with those after it, to keep the
    /// order. Returns the ids now in use, with what stopped it, if anything.
    async fn later(
        &self,
        ch: Snowflake,
        old: &[Snowflake],
        want: &[Rendered],
        have: &[String],
    ) -> (Vec<Snowflake>, Result<()>) {
        let mut ids = Vec::new();
        for (i, (m, w)) in old.iter().zip(want).enumerate() {
            if have.get(i) == Some(&w.signature()) {
                ids.push(*m);
                continue;
            }
            let path = format!("channels/{ch}/messages/{m}");
            match self.call(Method::Patch, path, Some(wire::message_json(w))).await {
                Ok(_) => ids.push(*m),
                Err(e) if ApiError::status_of(&e) == Some(404) => break,
                Err(e) => return ([ids, old[i..].to_vec()].concat(), Err(e)),
            }
        }
        let kept = ids.len();
        for (i, m) in old.iter().enumerate().skip(kept) {
            if let Err(e) = self.take_away(ch, *m).await {
                let left = old[i..].to_vec();
                return ([ids, left].concat(), Err(e.context(format!("taking away part {} of the post", i + 2))));
            }
        }
        for w in &want[kept..] {
            let made = self.call(Method::Post, format!("channels/{ch}/messages"), Some(wire::message_json(w))).await;
            match made.and_then(|m| id(&m["id"]).context("no id in reply")) {
                Ok(m) => ids.push(m),
                Err(e) => return (ids, Err(e)),
            }
        }
        (ids, Ok(()))
    }

    /// Take away a later part no longer needed: deleted if the bot wrote it
    /// (a message it didn't is only forgotten), or failing that, edited to
    /// say so.
    async fn take_away(&self, ch: Snowflake, m: Snowflake) -> Result<()> {
        let path = format!("channels/{ch}/messages/{m}");
        let msg = match self.call(Method::Get, path.clone(), None).await {
            Ok(msg) => msg,
            Err(e) if ApiError::status_of(&e) == Some(404) => return Ok(()),
            Err(e) => return Err(e),
        };
        if id(&msg["author"]["id"]) != Some(self.me().await?) {
            return Ok(());
        }
        match self.call(Method::Delete, path.clone(), None).await {
            Ok(_) => Ok(()),
            Err(e) if ApiError::status_of(&e) == Some(404) => Ok(()),
            Err(e) => {
                let moved = Rendered { content: MOVED_UP.into(), buttons: Vec::new() };
                self.call(Method::Patch, path, Some(wire::message_json(&moved))).await.map(|_| ()).map_err(|_| e)
            }
        }
    }

    /// Take away all of a post's old later parts before posting it again from
    /// the start (they'd come before it). Best effort: the first error, if any.
    async fn clear(&self, ch: Snowflake, old: &[Snowflake]) -> Option<anyhow::Error> {
        let mut first = None;
        for m in old {
            if let Err(e) = self.take_away(ch, *m).await {
                first.get_or_insert(e);
            }
        }
        first
    }

    /// Create or update a channel's managed post (re-posting if it was deleted).
    async fn post(&mut self, key: &str, existing: Option<Snowflake>) -> Result<()> {
        let src = self.cfg.desired.posts.get(key).context("no post for this channel in config")?;
        let ch = self.lock.channel(key).with_context(|| format!("channel `{key}` doesn't exist yet"))?;
        let parts = template::render_parts(src, self.lock);
        let entry = Entry::Post(key);
        if self.cfg.desired.channel(key).is_some_and(|c| c.kind.has_tags()) {
            // A forum's post: a pinned thread titled FORUM_POST_TITLE, kept by
            // its id, which is also its first message's.
            let existing = existing.map(|t| ThreadRef { thread: t, message: t, parts: self.parts_of(entry) });
            return self.forum_thread(entry, ch, FORUM_POST_TITLE, None, &parts, existing, Some(true)).await;
        }
        let (first, later) = parts.split_first().context("the post has no text")?;
        let (have, old) = (self.have(entry), self.parts_of(entry));
        // Looking is best effort: without message history, post as before.
        if existing.is_none()
            && let Ok(Some(m)) = self.find_post(ch, first).await
        {
            let found = self.find_later(ch, m, later).await.unwrap_or_default();
            let have = signatures(&later[..found.len()]);
            self.lock.posts.insert(key.to_owned(), PostRef { message: m, parts: found.clone() });
            return self.finish(entry, ch, &found, later, &have).await;
        }
        if let Some(m) = existing {
            match self.edit_first(entry, ch, m, first, &have).await {
                Ok(()) => return self.finish(entry, ch, &old, later, have.get(1..).unwrap_or_default()).await,
                Err(e) if ApiError::status_of(&e) == Some(404) => {} // deleted in Discord: post again
                Err(e) => return Err(e),
            }
        }
        let cleared = self.clear(ch, &old).await;
        let made = self.call(Method::Post, format!("channels/{ch}/messages"), Some(wire::message_json(first))).await?;
        let message = id(&made["id"]).context("no id in reply")?;
        self.lock.posts.insert(key.to_owned(), PostRef { message, parts: Vec::new() });
        self.finish(entry, ch, &[], later, &[]).await?;
        cleared.map_or(Ok(()), |e| Err(e.context("posted again, but the old parts are still there")))
    }

    /// Edit a post's first message `m` in `ch` to say `first`, unless `have`
    /// (what each part said when planned) shows it already does.
    async fn edit_first(
        &self,
        post: Entry<'_>,
        ch: Snowflake,
        m: Snowflake,
        first: &Rendered,
        have: &[String],
    ) -> Result<()> {
        if have.first().is_some_and(|h| first_of(post, h) == first.signature()) {
            return Ok(());
        }
        self.call(Method::Patch, format!("channels/{ch}/messages/{m}"), Some(wire::message_json(first))).await?;
        Ok(())
    }

    async fn project_post(&mut self, slug: &str, existing: Option<ThreadRef>) -> Result<()> {
        let p = self.cfg.projects.iter().find(|p| p.slug == slug).context("project not in config")?;
        let conf = self.cfg.bot.projects.as_ref().context("no [projects] in bot.toml")?;
        let forum_key = &conf.forum;
        let forum = self.lock.channel(forum_key).with_context(|| format!("forum `{forum_key}` doesn't exist yet"))?;
        let post = projects::post(p, self.lock, self.cfg.desired.project_posts.get(slug).map(String::as_str));
        // Flags are sent only to pin, or to unpin one that was.
        let want_pin = conf.pinned.as_deref() == Some(slug);
        let was_pinned = self.live.and_then(|l| l.project_posts.get(slug)).is_some_and(|sig| projects::unpinned(sig).0);
        let pin = (want_pin || was_pinned).then_some(want_pin);
        self.forum_thread(Entry::Project(slug), forum, &post.title, Some(&post.tags), &post.parts, existing, pin).await
    }

    /// A forum's `[[channel.thread]]` post, keyed `forum/key`. Never pinned.
    async fn thread(&mut self, key: &str, existing: Option<ThreadRef>) -> Result<()> {
        let (forum_key, own) = key.split_once('/').context("not a forum post's key")?;
        let ch = self.cfg.desired.channel(forum_key).with_context(|| format!("forum `{forum_key}` not in config"))?;
        let t = ch.threads.iter().find(|t| t.key == own).context("forum post not in config")?;
        let src = self.cfg.desired.threads.get(key).context("no text for this forum post")?;
        let forum = self.lock.channel(forum_key).with_context(|| format!("forum `{forum_key}` doesn't exist yet"))?;
        let parts = template::render_parts(src, self.lock);
        self.forum_thread(Entry::Thread(key), forum, &t.title, Some(&t.tags), &parts, existing, None).await
    }

    /// The ids of `forum`'s tags named in `names` (matched case-insensitively), at most 5.
    async fn tag_ids(&self, forum: Snowflake, names: &[String]) -> Result<Vec<String>> {
        let available = self.call(Method::Get, format!("channels/{forum}"), None).await?["available_tags"].clone();
        let wanted: Vec<String> = names.iter().map(|t| t.to_lowercase()).collect();
        Ok(available
            .as_array()
            .into_iter()
            .flatten()
            .filter(|t| wanted.contains(&s(&t["name"]).to_lowercase()))
            .filter_map(|t| id(&t["id"]).map(|i| i.to_string()))
            .take(5)
            .collect())
    }

    /// A forum post the bot keeps (a forum's own post, a project's, or a
    /// `[[channel.thread]]`): its title, tags (unless `tags` is `None`) and
    /// parts edited in place, opened again first if Discord archived it;
    /// found again if the lockfile lost track of it; posted again if it was
    /// deleted. `pin`: pin it (or unpin it), else its pin is left as it is.
    #[allow(clippy::too_many_arguments)]
    async fn forum_thread(
        &mut self,
        entry: Entry<'_>,
        forum: Snowflake,
        title: &str,
        tags: Option<&[String]>,
        parts: &[Rendered],
        existing: Option<ThreadRef>,
        pin: Option<bool>,
    ) -> Result<()> {
        let (first, later) = parts.split_first().context("the post has no text")?;
        let mut have = self.have(entry);
        let tag_ids = match tags {
            Some(tags) => Some(self.tag_ids(forum, tags).await?),
            None => None,
        };
        let title: String = title.chars().take(100).collect();
        let found = if existing.is_none() {
            self.find_thread(forum, &title, first, self.claimed()).await.ok().flatten()
        } else {
            None
        };
        let found = match found {
            Some(mut t) => {
                t.parts = self.find_later(t.thread, t.message, later).await.unwrap_or_default();
                // The first part is sent again below with the title and tags.
                have = [vec![String::new()], signatures(&later[..t.parts.len()])].concat();
                self.set_thread_ref(entry, t.clone());
                Some(t)
            }
            None => None,
        };
        let old = self.thread_ref(entry);
        if let Some(t) = existing.or(found) {
            match self.keep_thread(entry, &t, &title, tag_ids.as_deref(), pin, first, &have).await {
                Ok(renamed) => {
                    // A new title leaves a notice in the thread.
                    let notices = match renamed {
                        true => match self.me().await {
                            Ok(me) => clear_rename_notices(self.rest, t.thread, me).await.map(drop),
                            Err(e) => Err(e),
                        },
                        false => Ok(()),
                    };
                    self.finish(entry, t.thread, &t.parts, later, have.get(1..).unwrap_or_default()).await?;
                    return notices.context("renamed, but couldn't delete Discord's rename notice in it");
                }
                Err(e) if ApiError::status_of(&e) == Some(404) => {} // deleted in Discord: post again
                Err(e) => return Err(e),
            }
        }
        let cleared = match old {
            Some(t) => self.clear(t.thread, &t.parts).await,
            None => None,
        };
        let mut body = json!({ "name": title, "message": wire::message_json(first) });
        if let Some(ids) = &tag_ids {
            body["applied_tags"] = json!(ids);
        }
        let made = self.call(Method::Post, format!("channels/{forum}/threads"), Some(body)).await?;
        let thread = id(&made["id"]).context("no id in reply")?;
        let message = id(&made["message"]["id"]).unwrap_or(thread);
        self.set_thread_ref(entry, ThreadRef { thread, message, parts: Vec::new() });
        let rest = self.finish(entry, thread, &[], later, &[]).await;
        if pin == Some(true) {
            self.call(Method::Patch, format!("channels/{thread}"), Some(json!({ "flags": PINNED_THREAD })))
                .await
                .context("posted, but couldn't pin it (the bot needs Manage Threads there)")?;
        }
        rest?;
        cleared.map_or(Ok(()), |e| Err(e.context("posted again, but the old parts are still there")))
    }

    /// Bring a kept forum post's thread (title, tags, pin; opened if
    /// archived) and first message in line, sending only what differs.
    /// Whether it was renamed. A 404 means it's gone.
    #[allow(clippy::too_many_arguments)]
    async fn keep_thread(
        &self,
        entry: Entry<'_>,
        t: &ThreadRef,
        title: &str,
        tag_ids: Option<&[String]>,
        pin: Option<bool>,
        first: &Rendered,
        have: &[String],
    ) -> Result<bool> {
        // As Discord has it now; if it can't be read, everything is sent.
        let live = match self.call(Method::Get, format!("channels/{}", t.thread), None).await {
            Ok(c) => Some(c),
            Err(e) if ApiError::status_of(&e) == Some(404) => return Err(e),
            Err(_) => None,
        };
        let mut patch = Map::new();
        let renamed = live.as_ref().is_some_and(|c| c["name"].as_str() != Some(title));
        if live.is_none() || renamed {
            patch.insert("name".into(), json!(title));
        }
        if let Some(ids) = tag_ids {
            let now: BTreeSet<Snowflake> =
                live.as_ref().map(|c| wire::ids(&c["applied_tags"])).unwrap_or_default().into_iter().collect();
            let want: BTreeSet<Snowflake> = ids.iter().filter_map(|i| i.parse().ok().map(Snowflake)).collect();
            if live.is_none() || now != want {
                patch.insert("applied_tags".into(), json!(ids));
            }
        }
        if let Some(on) = pin {
            // Only the pinned bit: the thread's other flags stay as they are.
            let flags = live.as_ref().and_then(|c| c["flags"].as_u64()).unwrap_or(0);
            let want = if on { flags | PINNED_THREAD } else { flags & !PINNED_THREAD };
            if live.is_none() || want != flags {
                patch.insert("flags".into(), json!(want));
            }
        }
        // An archived post takes no edits or new parts until it's opened
        // again. It's left open: a kept post isn't one to hide.
        if live.as_ref().is_some_and(|c| c["thread_metadata"]["archived"] == true) {
            patch.insert("archived".into(), json!(false));
        }
        if !patch.is_empty() {
            self.call(Method::Patch, format!("channels/{}", t.thread), Some(Value::Object(patch))).await?;
        }
        self.edit_first(entry, t.thread, t.message, first, have).await?;
        Ok(renamed)
    }

    /// A forum post the bot started in `forum` with this title and first
    /// message, that the lockfile lost track of (none of `claimed`).
    async fn find_thread(
        &self,
        forum: Snowflake,
        title: &str,
        want: &Rendered,
        claimed: BTreeSet<Snowflake>,
    ) -> Result<Option<ThreadRef>> {
        let me = self.me().await?;
        let g = guild(self.lock)?;
        let mut lists = vec![self.call(Method::Get, format!("guilds/{g}/threads/active"), None).await?];
        // Archived posts come a page at a time, newest archived first.
        let mut before: Option<String> = None;
        for _ in 0..ARCHIVED_PAGES {
            let path = match &before {
                Some(b) => {
                    format!("channels/{forum}/threads/archived/public?limit=100&before={}", b.replace('+', "%2B"))
                }
                None => format!("channels/{forum}/threads/archived/public?limit=100"),
            };
            let page = self.call(Method::Get, path, None).await?;
            let more = page["has_more"] == true;
            before = page["threads"]
                .as_array()
                .and_then(|l| l.last())
                .and_then(|t| t["thread_metadata"]["archive_timestamp"].as_str())
                .map(str::to_owned);
            lists.push(page);
            if !more || before.is_none() {
                break;
            }
        }
        let candidates: Vec<Snowflake> = lists
            .iter()
            .flat_map(|l| l["threads"].as_array().into_iter().flatten())
            .filter(|t| id(&t["parent_id"]) == Some(forum) && id(&t["owner_id"]) == Some(me) && t["name"] == title)
            .filter_map(|t| id(&t["id"]))
            .filter(|t| !claimed.contains(t))
            .collect();
        for thread in candidates {
            // A forum post's first message has the thread's id.
            let Ok(first) = self.call(Method::Get, format!("channels/{thread}/messages/{thread}"), None).await else {
                continue;
            };
            if wire::rendered_from(&first) == *want {
                return Ok(Some(ThreadRef { thread, message: thread, parts: Vec::new() }));
            }
        }
        Ok(None)
    }

    async fn reorder(&mut self, g: Snowflake, scope: &OrderScope, to: &[String]) -> Result<()> {
        match scope {
            OrderScope::Roles => {
                let g_json = self.call(Method::Get, format!("guilds/{g}"), None).await?;
                let me = self.call(Method::Get, "users/@me".into(), None).await?;
                let bot = crate::bot_role_in(&g_json, &me["id"]).map(|(b, _)| b);
                let pos = |r: &Value| r["position"].as_i64().unwrap_or(0);
                let mut live: Vec<&Value> =
                    g_json["roles"].as_array().into_iter().flatten().filter(|r| id(&r["id"]) != Some(g)).collect();
                live.sort_by_key(|r| (-pos(r), id(&r["id"])));
                let ordered = resolve(to, |k| self.lock.role(k))?;
                // Discord refuses the whole request if it names a role at or
                // above the bot's own: only those below it are sent.
                let bot_pos = bot.and_then(|b| live.iter().find(|r| id(&r["id"]) == Some(b))).map(|r| pos(r));
                let below = |r: &&Value| bot_pos.is_none_or(|b| pos(r) < b) && id(&r["id"]) != bot;
                let stuck: Vec<String> = to
                    .iter()
                    .zip(&ordered)
                    .filter(|(_, i)| live.iter().find(|r| id(&r["id"]) == Some(**i)).is_some_and(|r| !below(r)))
                    .map(|(k, i)| {
                        let r = live.iter().find(|r| id(&r["id"]) == Some(*i));
                        r.map_or_else(|| k.clone(), |r| s(&r["name"]))
                    })
                    .collect();
                let live: Vec<&Value> = live.into_iter().filter(below).collect();
                // Roles config doesn't list keep the top, in their current order.
                let mut all: Vec<&Value> =
                    live.iter().copied().filter(|r| !id(&r["id"]).is_some_and(|i| ordered.contains(&i))).collect();
                all.extend(ordered.iter().filter_map(|i| live.iter().copied().find(|r| id(&r["id"]) == Some(*i))));
                let n = all.len() as i64;
                let body: Vec<Value> = all
                    .iter()
                    .enumerate()
                    .filter(|(_, r)| !r["managed"].as_bool().unwrap_or(false))
                    .map(|(i, r)| json!({ "id": r["id"], "position": n - i as i64 }))
                    .collect();
                if !body.is_empty() {
                    self.call(Method::Patch, format!("guilds/{g}/roles"), Some(json!(body))).await?;
                }
                if !stuck.is_empty() {
                    bail!(
                        "moved the others, but not {}: {} above the bot's role, and Discord only lets a bot move \
                         roles below its own. Ask the server owner to drag the bot's role to the top in Server \
                         Settings → Roles, then apply again",
                        and_list(&stuck),
                        if stuck.len() == 1 { "it's" } else { "they're" }
                    );
                }
            }
            OrderScope::Categories => {
                let body: Vec<Value> = resolve(to, |k| self.lock.category(k))?
                    .into_iter()
                    .enumerate()
                    .map(|(i, c)| json!({ "id": c.to_string(), "position": i }))
                    .collect();
                self.call(Method::Patch, format!("guilds/{g}/channels"), Some(json!(body))).await?;
            }
            OrderScope::Channels(_) => {
                let body: Vec<Value> = resolve(to, |k| self.lock.channel(k))?
                    .into_iter()
                    .enumerate()
                    .map(|(i, c)| json!({ "id": c.to_string(), "position": i }))
                    .collect();
                self.call(Method::Patch, format!("guilds/{g}/channels"), Some(json!(body))).await?;
            }
        }
        Ok(())
    }

    /// Move to the archive category, visible to admins only, read-only. A
    /// forum post is archived and locked where it is.
    async fn archive(&mut self, kind: ObjectKind, key: &str, oid: Snowflake) -> Result<()> {
        if kind == ObjectKind::Thread {
            let path = format!("channels/{oid}");
            let body = json!({ "archived": true, "locked": true });
            let mut done = self.call(Method::Patch, path.clone(), Some(body.clone())).await;
            // Discord archived it already, so it takes no lock: open it, then close and lock it.
            if done.as_ref().err().is_some_and(|e| ApiError::code_of(e) == Some(ARCHIVED_THREAD)) {
                self.call(Method::Patch, path.clone(), Some(json!({ "archived": false }))).await?;
                done = self.call(Method::Patch, path, Some(body)).await;
            }
            match done {
                Err(e) if ApiError::status_of(&e) != Some(404) => return Err(e),
                _ => {}
            }
            self.lock.posts.remove(key);
            self.lock.archived.insert(format!("{key}-{oid}"), oid);
            return Ok(());
        }
        let mut ows = vec![
            json!({ "id": guild(self.lock)?.to_string(), "type": 0, "allow": "0", "deny": (1u64 << 10).to_string() }),
        ];
        if let Some(admin) = self.cfg.bot.admin_role.as_deref().and_then(|k| self.lock.role(k)) {
            let send = (1u64 << 11) | (1u64 << 38);
            ows.push(json!({ "id": admin.to_string(), "type": 0, "allow": (1u64 << 10).to_string(), "deny": send.to_string() }));
        }
        let mut patch = json!({ "permission_overwrites": ows });
        if kind == ObjectKind::Channel {
            let cat = self
                .cfg
                .bot
                .archive
                .as_deref()
                .context("set `archive` (a category) in bot.toml to archive channels")?;
            let cat_id =
                self.lock.category(cat).with_context(|| format!("archive category `{cat}` doesn't exist yet"))?;
            patch["parent_id"] = json!(cat_id.to_string());
        }
        self.call(Method::Patch, format!("channels/{oid}"), Some(patch)).await?;
        self.lock.channels.retain(|_, v| *v != oid);
        self.lock.categories.retain(|_, v| *v != oid);
        self.lock.archived.insert(format!("{key}-{oid}"), oid);
        Ok(())
    }
}

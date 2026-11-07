//! The one entry point every front end (CLI, TUI, web, the bot) uses to plan,
//! apply, export and adopt, and to edit the config. Nothing here prints;
//! progress goes to a callback. Git, file locks and copying the config run
//! off the async runtime's threads.

mod adopt;
pub mod git;
mod owner;

pub use owner::{Ask, By, OWNER_UNKNOWN, OwnerOnly, check as owner_check, owner_of, owner_only};

use std::borrow::Cow;
use std::collections::BTreeSet;
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::{Arc, OnceLock};

use anyhow::{Result, bail};
use chipsoc_config::authoring;
pub use chipsoc_config::authoring::Problems;
use chipsoc_config::ops::{self, Change, Op};
use chipsoc_config::schema::*;
use chipsoc_config::trust::{Trusted, Vetted};
use chipsoc_config::{Config, write_snapshot};
use chipsoc_config::{FileLock, WriteLock, blocking};
pub use chipsoc_diff::Options;
use chipsoc_discord::exec::{self, Report};
use chipsoc_discord::{Method, Rest, export};
use chipsoc_model::{Action, ObjectKind, Plan, Warning};
use git::Upstream;

#[cfg(test)]
mod tests;

/// Told about each step as `apply` runs.
pub trait Progress {
    fn step(&mut self, action: &Action, result: &Result<(), String>);
}

impl<F: FnMut(&Action, &Result<(), String>)> Progress for F {
    fn step(&mut self, action: &Action, result: &Result<(), String>) {
        self(action, result);
    }
}

pub struct Engine<R> {
    pub rest: R,
    /// The config as last loaded (vetted, when there's a [`Baseline`]); shared, so handing it out is cheap.
    pub cfg: Arc<Config>,
    /// Whether edits and applies commit and push; off unless the front end
    /// says (see [`Engine::autosave`] and [`autosaves`]).
    autosave: bool,
    /// The bot's user id, asked of Discord once.
    me: OnceLock<serde_json::Value>,
    /// What the bot trusts instead of the repo's lock.toml and sensitive settings.
    trust: Option<(Arc<dyn Baseline>, Trusted)>,
    /// What the trusted state changed from the files, for plan warnings.
    pub vetted: Vetted,
}

/// A future a [`Baseline`] returns.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Where the bot keeps what it trusts about a config (its own lock.toml and
/// the sensitive settings an admin approved), away from the repo anyone with
/// push access can change.
pub trait Baseline: Send + Sync {
    fn load(&self) -> BoxFuture<'_, Result<Trusted>>;
    fn save_lock<'a>(&'a self, lock: &'a Lock) -> BoxFuture<'a, Result<()>>;

    /// Changes whenever what [`Baseline::load`] gives does (by this program or
    /// another), so a copy can be checked without loading it all. By
    /// default, a hash of it all.
    fn revision(&self) -> BoxFuture<'_, Result<String>> {
        Box::pin(async {
            let t = self.load().await?;
            Ok(chipsoc_model::stable_hash(serde_json::to_string(&(&t.lock, &t.approved))?))
        })
    }
    /// Project `from` in server `guild` is now `to`, its keys moved as `keys`
    /// (see [`Op::RenameProject`]): what's kept by slug or role key follows.
    fn renamed<'a>(
        &'a self,
        _guild: Option<Snowflake>,
        _from: &'a str,
        _to: &'a str,
        _keys: &'a ops::Renames,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async { Ok(()) })
    }

    /// Note that the person with Discord id `by` made the config commit
    /// with this [`git::fingerprint`] (through the bot, the web UI or the
    /// CLI on the bot's machine), so approvals know who made a change from
    /// the bot's own records, never from what a pushed commit claims.
    fn made<'a>(&'a self, _fingerprint: &'a str, _by: Snowflake) -> BoxFuture<'a, Result<()>> {
        Box::pin(async { Ok(()) })
    }

    /// Note in the audit log of server `guild` that `by` (an id, else a
    /// name) did `action`; `detail` says what. The entry's id, if it has one.
    fn audit<'a>(
        &'a self,
        _guild: Snowflake,
        _by: &'a str,
        _action: &'a str,
        _detail: &'a str,
    ) -> BoxFuture<'a, Result<Option<i64>>> {
        Box::pin(async { Ok(None) })
    }
}

/// An apply refused before it changed anything, because something else moved
/// first: planning again (in a moment) is the fix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refused {
    /// The server or the config changed since the plan was made.
    Stale,
    /// Another apply holds the [`Lease`].
    Busy,
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Stale => "the server or the config changed since this plan was made; plan again",
            Self::Busy => "another apply is running on this config; try again in a moment",
        })
    }
}

impl std::error::Error for Refused {}

/// Held while applying, so two applies (CLI, bot, web) never interleave.
pub struct Lease(#[allow(dead_code)] FileLock);

impl Lease {
    pub fn take(dir: &Path) -> Result<Self> {
        match FileLock::try_take(&dir.join(".apply.lock"))? {
            Some(l) => Ok(Self(l)),
            None => Err(Refused::Busy.into()),
        }
    }

    /// Whether an apply holds the lease now (here or in another program): a
    /// hint, out of date as soon as it's said (see [`FileLock::held`]). An
    /// apply starting in the instant it looks waits it out (see [`Lease::take_soon`]).
    pub fn busy(dir: &Path) -> bool {
        FileLock::held(&dir.join(".apply.lock"))
    }

    /// [`Lease::take`], trying again for a moment first: a lease just let go
    /// can stay held a little longer by a program being started meanwhile
    /// (it shares the lock until it starts running).
    pub async fn take_soon(dir: &Path) -> Result<Self> {
        for _ in 0..20 {
            if let Ok(l) = Self::take(dir) {
                return Ok(l);
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        Self::take(dir)
    }
}

/// What a plan is checked for: planning on the bot's own copy only notes
/// that it's behind (it catches up by itself); applying refuses, saying why
/// catching up failed if it did.
#[derive(Debug, Clone, PartialEq, Eq)]
enum For {
    Plan,
    Apply { sync_error: Option<String> },
}

/// What [`Engine::apply_holders`] did about one role.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Held {
    pub role: String,
    /// Given (else taken away).
    pub gave: bool,
    pub result: Result<(), String>,
}

impl<R: Rest> Engine<R> {
    pub fn load(rest: R, dir: &Path) -> Result<Self> {
        Ok(Self::with(rest, Config::load(dir)?))
    }

    /// Use the bot's own lock and approved settings from `baseline` instead of
    /// the files'. The first time, the files' lock is what's trusted.
    pub async fn trusting(mut self, baseline: Arc<dyn Baseline>) -> Result<Self> {
        self.set_baseline(baseline);
        self.refresh().await?;
        Ok(self)
    }

    /// [`Engine::trusting`], read from `baseline` at the next [`Engine::refresh`].
    pub fn set_baseline(&mut self, baseline: Arc<dyn Baseline>) {
        self.trust = Some((baseline, Trusted::default()));
    }

    /// [`Engine::reload`], off the async runtime's threads, reading what's
    /// trusted again too (another program, such as the bot, may have applied
    /// or approved meanwhile).
    pub async fn refresh(&mut self) -> Result<()> {
        let dir = self.cfg.dir.clone();
        // Nothing changes unless all of it worked: never the files' settings unvetted.
        let mut cfg = blocking(move || Config::load(&dir)).await?;
        if let Some((baseline, _)) = self.trust.clone() {
            let (t, vetted) = vet_with(&*baseline, &mut cfg).await?;
            self.vetted = vetted;
            self.trust = Some((baseline, t));
        }
        self.cfg = Arc::new(cfg);
        Ok(())
    }

    /// Where this engine reads what's trusted, if anywhere.
    pub fn baseline(&self) -> Option<Arc<dyn Baseline>> {
        self.trust.as_ref().map(|(b, _)| b.clone())
    }

    fn vet(&mut self, cfg: &mut Config) {
        if let Some((_, t)) = &self.trust {
            self.vetted = t.vet(cfg);
        }
    }

    /// An engine whose config isn't loaded yet (call [`Engine::reload`] before use),
    /// so a front end can start even while a file has a mistake in it.
    pub fn unloaded(rest: R, dir: &Path) -> Self {
        Self::with(rest, Config { dir: dir.to_owned(), ..Default::default() })
    }

    /// A copy of this engine to plan with, on the config as loaded now, so
    /// a plan (a whole export) needn't hold this one: edits and applies can
    /// go ahead meanwhile (an apply checks its plan afresh anyway).
    pub fn planner(&self) -> Self
    where
        R: Clone,
    {
        Self {
            rest: self.rest.clone(),
            cfg: self.cfg.clone(),
            autosave: self.autosave,
            me: self.me.clone(),
            trust: self.trust.clone(),
            vetted: self.vetted.clone(),
        }
    }

    fn with(rest: R, cfg: Config) -> Self {
        Self { rest, cfg: Arc::new(cfg), autosave: false, me: OnceLock::new(), trust: None, vetted: Vetted::default() }
    }

    /// Commit and push edits and applies (the bot and the web UI, when
    /// [`autosaves`] says so), or leave that to a person (the default).
    pub fn autosave(mut self, on: bool) -> Self {
        self.autosave = on;
        self
    }

    /// Whether this engine commits and pushes what it changes.
    pub fn saves(&self) -> bool {
        self.autosave
    }

    pub fn reload(&mut self) -> Result<()> {
        let mut cfg = Config::load(&self.cfg.dir)?;
        self.vet(&mut cfg);
        self.cfg = Arc::new(cfg);
        Ok(())
    }

    /// Commit and push the config as `by`, if this engine saves. Returns whether it committed.
    async fn commit(&self, message: &str, by: &By) -> Result<bool> {
        if !self.autosave {
            return Ok(false);
        }
        let held = WriteLock::take_async(&self.cfg.dir).await?;
        let before = git::head_full(&self.cfg.dir);
        let saved = git::save_locked_async(&self.cfg.dir, message, &held).await;
        let fingerprint = committed_since(&self.cfg.dir, before.as_deref());
        drop(held);
        self.made(fingerprint, by).await;
        saved
    }

    /// Keep who made the commit with `fingerprint` (see [`Baseline::made`]).
    async fn made(&self, fingerprint: Option<String>, by: &By) {
        let (Some((baseline, _)), Some(id), Some(fp)) = (&self.trust, by.id, fingerprint) else { return };
        if let Err(e) = baseline.made(&fp, id).await {
            tracing::warn!("noting who made a config commit failed: {e:#}");
        }
    }

    /// Edit the config with `op` (tried on a copy and checked first, see
    /// [`ops::apply_op`]), commit it saying who, and reload. The whole time
    /// the config's write lock is held, shared by every program writing this
    /// directory, so nothing commits or pulls in between.
    pub async fn edit(&mut self, op: &Op, by: &By) -> Result<Change> {
        self.edit_ops(std::slice::from_ref(op), by).await
    }

    /// [`Engine::edit`] for several ops made as one change (one check, one
    /// commit, all or nothing). An error means nothing was edited: once the
    /// files are written, a step after that which fails (committing, or the
    /// trusted lock following a project rename) is told in the change's
    /// warnings instead, as the edit stands.
    pub async fn edit_ops(&mut self, list: &[Op], by: &By) -> Result<Change> {
        let renames = self.project_renames(list);
        let list = list.to_vec();
        let (mut change, saved) = self
            .edit_with(
                move |dir| {
                    let change = ops::apply_ops(dir, &list)?;
                    let message = (!change.files.is_empty()).then(|| change.summary.clone());
                    Ok((change, message))
                },
                by,
            )
            .await?;
        if let Err(e) = saved {
            tracing::warn!("an edit is in the files but committing it failed: {e:#}");
            change.warnings.push(format!("The edit is made, but committing it to the config repo failed: {e:#}"));
        }
        if !change.files.is_empty()
            && !renames.is_empty()
            && let Err(e) = self.renamed(&renames).await
        {
            tracing::warn!("after a project rename, updating what the bot trusts failed: {e:#}");
            change.warnings.push(format!(
                "The rename is made, but the bot's trusted copy of lock.toml didn't follow it ({e:#}): \
                 until it does, the bot may offer to make the project's roles and channels anew. Check before applying."
            ));
        }
        Ok(change)
    }

    /// The project renames among `list`, with the keys each moves in the lock in use.
    fn project_renames(&self, list: &[Op]) -> Vec<(String, String, ops::Renames)> {
        let cfg = &self.cfg;
        list.iter()
            .filter_map(|op| match op {
                Op::RenameProject { from, to } => {
                    let p = cfg.projects.iter().find(|p| &p.slug == from)?;
                    let others: Vec<Project> = cfg.projects.iter().filter(|q| &q.slug != from).cloned().collect();
                    let keys = ops::project_renames(p, to, &others, &cfg.desired.lock);
                    Some((from.clone(), to.clone(), keys))
                }
                _ => None,
            })
            .collect()
    }

    /// After a project rename: the lock the bot trusts follows the one in the
    /// files (it would otherwise refuse the moved keys and make the project
    /// anew), and so does what it keeps by key elsewhere.
    async fn renamed(&mut self, renames: &[(String, String, ops::Renames)]) -> Result<()> {
        let Some((baseline, trusted)) = self.trust.clone() else { return Ok(()) };
        if let Some(mut lock) = trusted.lock.clone() {
            for (from, to, keys) in renames {
                ops::rename_in_lock(&mut lock, from, to, keys);
            }
            baseline.save_lock(&lock).await?;
        }
        for (from, to, keys) in renames {
            baseline.renamed(self.cfg.guild(), from, to, keys).await?;
        }
        self.refresh().await
    }

    /// Edit the config files with `f` (which says what to commit, if
    /// anything), holding the write lock; commit as `by`, and reload. The
    /// outer error means nothing was edited; the inner one that the edit
    /// stands but saving it failed. A config that doesn't load after it is
    /// left as it was here (the next load shows the problem).
    pub async fn edit_with<T: Send + 'static>(
        &mut self,
        f: impl FnOnce(&Path) -> Result<(T, Option<String>)> + Send + 'static,
        by: &By,
    ) -> Result<(T, Result<bool>)> {
        let held = WriteLock::take_async(&self.cfg.dir).await?;
        let (dir, autosave, signer) = (self.cfg.dir.clone(), self.autosave, by.clone());
        let (value, saved, message, fingerprint) = blocking(move || {
            let (value, message) = f(&dir)?;
            let before = git::head_full(&dir);
            let saved = match &message {
                Some(m) if autosave => git::save_locked(&dir, &signed(m.clone(), &signer)),
                _ => Ok(false),
            };
            Ok((value, saved, message, committed_since(&dir, before.as_deref())))
        })
        .await?;
        drop(held);
        self.made(fingerprint, by).await;
        if let Err(e) = self.refresh().await {
            tracing::warn!("the config doesn't load after an edit: {e:#}");
        }
        if let Some(m) = message {
            self.audit(by, "config_edit", &m).await;
        }
        Ok((value, saved))
    }

    /// Note what `by` did in the audit log, if there's one (see [`Baseline::audit`]) and they're a person.
    /// The entry's id, if one was written.
    async fn audit(&self, by: &By, action: &str, detail: &str) -> Option<i64> {
        let (Some((baseline, _)), Some(who)) = (&self.trust, by.key()) else { return None };
        let g = self.guild().await.ok()?;
        match baseline.audit(g, &who, action, detail).await {
            Ok(id) => id,
            Err(e) => {
                tracing::warn!("audit log failed: {e:#}");
                None
            }
        }
    }

    /// The server's owner, as Discord says now (see [`owner::owner_of`]).
    pub async fn owner(&self) -> Result<Snowflake> {
        owner::owner_of(&self.rest, self.guild().await?).await
    }

    /// Go back on config commit `commit` as `how` says (see
    /// [`git::undo_locked`]): the server owner only. The result is checked
    /// like any edit, committed saying who, and pushed; applying it is a
    /// separate step, like any config change. `None`: nothing to change.
    pub async fn undo(&mut self, commit: &str, how: git::Undo, by: &By) -> Result<Option<git::Undid>> {
        owner::check(by, &Ask::Undo, Some(self.owner().await?))?;
        let held = WriteLock::take_async(&self.cfg.dir).await?;
        let (dir, push, signer, commit) = (self.cfg.dir.clone(), self.autosave, by.clone(), commit.to_owned());
        let made = blocking(move || {
            if push && let Err(e) = git::sync_locked(&dir, true) {
                tracing::warn!("catching up before going back on a commit failed: {e:#}");
            }
            let before = authoring::check(&dir).map(|r| r.errors).unwrap_or_default();
            let message = |subject: &str, short: &str| {
                let m = match how {
                    git::Undo::Revert => format!("Revert \"{subject}\" ({short})"),
                    git::Undo::Rewind => format!("Rewind the config to {short} (\"{subject}\")"),
                };
                signed(m, &signer)
            };
            let check = |d: &Path| {
                let after = authoring::check(d)?;
                let new: Vec<String> = after.errors.into_iter().filter(|e| !before.contains(e)).collect();
                if new.is_empty() { Ok(()) } else { Err(Problems(new).into()) }
            };
            let before = git::head_full(&dir);
            let made = git::undo_locked(&dir, &commit, how, message, check, push);
            made.map(|m| (m, committed_since(&dir, before.as_deref())))
        })
        .await;
        drop(held);
        let made = match made {
            Ok((m, fingerprint)) => {
                self.made(fingerprint, by).await;
                Ok(m)
            }
            Err(e) => Err(e),
        };
        if let Err(e) = self.refresh().await {
            tracing::warn!("the config doesn't load after going back on a commit: {e:#}");
        }
        let made = made?;
        if let Some(m) = &made {
            let action = match how {
                git::Undo::Revert => "revert",
                git::Undo::Rewind => "rewind",
            };
            self.audit(by, action, &m.subject).await;
        }
        Ok(made)
    }

    /// The server this config manages: from the lockfile, else the bot's only one.
    pub async fn guild(&self) -> Result<Snowflake> {
        match self.cfg.guild() {
            Some(g) => Ok(g),
            None => Ok(chipsoc_discord::sole_guild(&self.rest).await?.0),
        }
    }

    pub async fn live(&self) -> Result<Snapshot> {
        let g = self.guild().await?;
        export::export(&self.rest, g, &self.cfg.desired.lock).await
    }

    /// Ids of the roles whose holders config manages.
    fn held_roles(&self) -> Vec<Snowflake> {
        let d = &self.cfg.desired;
        d.holders.keys().filter_map(|k| d.lock.role(k)).collect()
    }

    /// What would change, for a config without errors.
    pub async fn plan(&self, opts: Options) -> Result<Plan> {
        self.blockers()?;
        let notes = self.checkout(For::Plan).await?;
        let g = self.guild().await?;
        let mut plan = self.plan_at(g, opts).await?.0;
        sweep_guard(&self.cfg, &plan)?;
        plan.warnings.splice(0..0, notes);
        let held = Trusted::notes(&self.vetted).into_iter().map(|text| Warning::Note { text });
        plan.warnings.splice(0..0, held);
        Ok(plan)
    }

    /// Before changing anything: the bot's own copy of the config catches up
    /// with its remote (a person pulls anywhere else), is reloaded, and must
    /// pass [`Engine::checkout`].
    async fn catch_up(&mut self) -> Result<()> {
        let mut sync_error = None;
        if self.autosave
            && let Err(e) = git::sync_async(&self.cfg.dir, true).await
        {
            tracing::warn!("catching up with the config's remote failed: {e:#}");
            sync_error = Some(format!("{e:#}"));
        }
        self.refresh().await?;
        self.checkout(For::Apply { sync_error }).await?;
        Ok(())
    }

    /// Whether this copy of the config is fit to plan or apply from (see [`checkout`]).
    async fn checkout(&self, purpose: For) -> Result<Vec<Warning>> {
        let (cfg, autosave) = (self.cfg.clone(), self.autosave);
        blocking(move || checkout(&cfg, &purpose, autosave)).await
    }

    fn blockers(&self) -> Result<()> {
        let r = authoring::check_config(&self.cfg);
        if r.errors.is_empty() { Ok(()) } else { Err(Problems(r.errors).into()) }
    }

    /// The bot's own role and whether it's on top, from a guild already fetched.
    async fn bot_role(&self, guild: &serde_json::Value) -> Result<Option<(Snowflake, bool)>> {
        let me = match self.me.get() {
            Some(me) => me,
            None => {
                let me = self.rest.call(Method::Get, "users/@me", None).await?["id"].clone();
                self.me.get_or_init(|| me)
            }
        };
        Ok(chipsoc_discord::bot_role_in(guild, me))
    }

    async fn plan_at(&self, g: Snowflake, opts: Options) -> Result<(Plan, Snapshot)> {
        let (live, guild) =
            export::export_with_holders(&self.rest, g, &self.cfg.desired.lock, &self.held_roles()).await?;
        let bot = self.bot_role(&guild).await?;
        let cfg = if self.cfg.desired.lock.guild == Some(g) {
            Cow::Borrowed(&*self.cfg)
        } else {
            let mut c = (*self.cfg).clone();
            c.desired.lock.guild = Some(g);
            Cow::Owned(c)
        };
        let mut plan = chipsoc_diff::plan(&cfg, &live, Options { bot_role: bot.map(|b| b.0), ..opts });
        if let Some((id, false)) = bot {
            // Named from Discord's own list: it has roles the lockfile doesn't.
            let above = chipsoc_discord::roles_above(&guild, id);
            plan.warnings.retain(|w| !matches!(w, Warning::BotRoleTooLow { .. }));
            let name = live
                .roles
                .roles
                .iter()
                .find(|r| live.lock.role(&r.key) == Some(id))
                .map_or("bot".into(), |r| r.name.clone());
            plan.warnings.insert(0, Warning::BotRoleTooLow { bot_role: name, above });
        }
        plan.fingerprint = fingerprint(&live, &self.cfg, opts);
        plan.owner = chipsoc_discord::wire::id(&guild["owner_id"]);
        plan.bot_role = bot.map(|b| b.0);
        plan.bot_role_key =
            bot.and_then(|(id, _)| self.cfg.desired.lock.roles.iter().find(|(_, v)| **v == id).map(|(k, _)| k.clone()));
        Ok((plan, live))
    }

    /// Apply a plan made by [`Engine::plan`] (or part of one). It's checked
    /// against a fresh plan: refused if Discord or the config changed since, or
    /// if it asks for a step the fresh plan doesn't.
    pub async fn apply(&mut self, plan: &Plan, progress: &mut (impl Progress + Send)) -> Result<Report> {
        self.apply_by(plan, &By::bot(), progress).await
    }

    /// [`Engine::apply`] for `by`: said in the commit message and the audit
    /// log, and refused if the plan holds something only the server owner
    /// may apply (see [`owner_only`]).
    pub async fn apply_by(&mut self, plan: &Plan, by: &By, progress: &mut (impl Progress + Send)) -> Result<Report> {
        let _lease = Lease::take_soon(&self.cfg.dir).await?;
        self.catch_up().await?;
        self.blockers()?;
        let g = self.guild().await?;
        let (fresh, live) =
            self.plan_at(g, Options { delete: plan.delete, sweep: plan.sweep, ..Default::default() }).await?;
        sweep_guard(&self.cfg, &fresh)?;
        if fresh.fingerprint != plan.fingerprint {
            return Err(Refused::Stale.into());
        }
        if let Some(extra) = plan.actions.iter().find(|a| !fresh.actions.iter().any(|f| a.within(f))) {
            bail!("this plan asks for a step the config doesn't call for: {}", extra.headline());
        }
        // Objects with steps left out stay unrecorded, so later plans still
        // say who changed them.
        let left_out: BTreeSet<String> =
            fresh.actions.iter().filter(|f| !plan.actions.contains(f)).filter_map(Action::object).collect();
        let mut checked = Plan { actions: plan.actions.clone(), ..fresh };
        owner::check(by, &Ask::Apply(&checked), None)?;
        checked.hashes.retain(|obj, _| !left_out.contains(obj));
        let mut lock = self.cfg.desired.lock.clone();
        lock.guild = Some(g);
        let dir = self.cfg.dir.clone();
        if let Some(head) = blocking(move || Ok(git::head_full(&dir))).await? {
            lock.applied.commit = Some(head);
        }
        // The bot's own copy first, so lock.toml never has ids it doesn't
        // (another program reloading meanwhile would take them for a pusher's).
        struct Saver<'a, P> {
            cfg: &'a Config,
            trust: Option<Arc<dyn Baseline>>,
            progress: &'a mut P,
        }
        impl<P: Progress + Send> exec::Sink for Saver<'_, P> {
            async fn saved(&mut self, lock: &Lock) -> Result<()> {
                let trusted = match &self.trust {
                    Some(b) => b.save_lock(lock).await,
                    None => Ok(()),
                };
                let file = self.cfg.save_lock_async(lock).await;
                trusted.and(file)
            }
            fn step(&mut self, a: &Action, r: &Result<(), String>) {
                self.progress.step(a, r);
            }
        }
        let cfg = self.cfg.clone();
        let mut saver = Saver { cfg: &cfg, trust: self.baseline(), progress };
        let before = lock.clone();
        let mut report = exec::execute(&self.rest, &cfg, &checked, Some(&live), &mut lock, &mut saver).await;
        if let Err(e) = self.record_applied(g, &mut lock, &before).await {
            tracing::warn!("recording applied state failed: {e:#}");
        }
        if let Err(e) = self.trust_lock(&lock).await {
            tracing::warn!("keeping the applied lock in the bot's database failed: {e:#}");
        }
        if let Err(e) = self.cfg.save_lock_async(&lock).await {
            tracing::warn!("saving the lockfile after apply failed: {e:#}");
        }
        let done = report.outcomes.iter().filter(|o| o.result.is_ok()).count();
        if let Err(e) = self.commit(&signed(format!("Apply {done} change(s)"), by), by).await {
            tracing::warn!("saving the config after apply failed: {e:#}");
        }
        let failed = report.outcomes.len() - done;
        let detail =
            if failed == 0 { format!("{done} change(s)") } else { format!("{done} change(s), {failed} failed") };
        report.audit_id = self.audit(by, "apply", &detail).await;
        if let Err(e) = self.refresh().await {
            // The report still stands; the next load shows the problem.
            tracing::warn!("reloading the config after apply failed: {e:#}");
        }
        Ok(report)
    }

    /// Apply just the steps of a fresh plan that `keep` picks (none: nothing to do).
    pub async fn apply_only(
        &mut self,
        keep: impl Fn(&Action) -> bool,
        progress: &mut (impl Progress + Send),
    ) -> Result<Report> {
        self.refresh().await?;
        let plan = self.plan(Options::default()).await?;
        let actions: Vec<Action> = plan.actions.iter().filter(|a| a.mutates() && keep(a)).cloned().collect();
        if actions.is_empty() {
            return Ok(Report::default());
        }
        self.apply(&Plan { actions, ..plan }, progress).await
    }

    /// Keep `lock` as the one the bot trusts, if it has a [`Baseline`].
    async fn trust_lock(&mut self, lock: &Lock) -> Result<()> {
        let Some((b, t)) = &mut self.trust else { return Ok(()) };
        t.lock = Some(lock.clone());
        b.save_lock(lock).await
    }

    /// Record in `lock` what's in step after an apply (the caller saves it);
    /// `before` is the lock it started from.
    async fn record_applied(&mut self, g: Snowflake, lock: &mut Lock, before: &Lock) -> Result<()> {
        let mut cfg = (*self.cfg).clone();
        cfg.desired.lock = lock.clone();
        self.cfg = Arc::new(cfg);
        // Objects made during this run were hashed before they existed (or
        // what they point at did), so plan again to hash them as they are.
        // With no new ids or posts, the plan's hashes (kept as it ran) are those.
        let ids = |l: &Lock| Lock { applied: Default::default(), ..l.clone() };
        if ids(lock) == ids(before) {
            return Ok(());
        }
        let (after, _) = self.plan_at(g, Options::default()).await?;
        let pending: BTreeSet<String> = after.actions.iter().filter_map(Action::object).collect();
        for (obj, h) in after.hashes {
            if !pending.contains(&obj) {
                lock.applied.hashes.insert(obj, h);
            }
        }
        Ok(())
    }

    /// Bring one person's holding of `roles` in line with the config, now:
    /// the quick way to apply an edit about them (an appointment, a lead, a
    /// restore). Only their roles change, so nobody else gains or loses one
    /// (the admin role included; a full plan shows any other difference).
    /// Roles not created yet, or whose holders the config doesn't manage, are
    /// left alone. One member listing, no export.
    pub async fn apply_holders(&mut self, user: Snowflake, roles: &[String]) -> Result<Vec<Held>> {
        let _lease = Lease::take_soon(&self.cfg.dir).await?;
        self.refresh().await?;
        let cfg = self.cfg.clone();
        let d = &cfg.desired;
        let g = self.guild().await?;
        let ids: Vec<Snowflake> =
            roles.iter().filter(|k| d.holders.contains_key(*k)).filter_map(|k| d.lock.role(k)).collect();
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let mut live = Snapshot { lock: d.lock.clone(), ..Default::default() };
        export::holders_into(&mut live, &self.rest.members(g).await?, &ids);
        let plan = chipsoc_diff::holders_plan(&cfg, &live, roles);
        let mut out = Vec::new();
        let mut lock = d.lock.clone();
        for key in roles {
            let (Some(id), Some(want)) = (d.lock.role(key), d.holders.get(key)) else { continue };
            let mut have = live.holders.get(key).cloned().unwrap_or_default();
            let (gives, holds) = (want.contains(&user), have.contains(&user));
            if gives != holds && (holds || live.members.contains(&user)) {
                let result = if !gives && cfg.bot.admin_role.as_deref() == Some(key) && want.is_empty() {
                    Err("the config gives the admin role to nobody, so it isn't taken from anyone".to_owned())
                } else {
                    let method = if gives { Method::Put } else { Method::Delete };
                    let path = format!("guilds/{g}/members/{user}/roles/{id}");
                    self.rest.call(method, &path, None).await.map(drop).map_err(|e| format!("{e:#}"))
                };
                if result.is_ok() {
                    if gives {
                        have.insert(user)
                    } else {
                        have.remove(&user)
                    };
                }
                out.push(Held { role: key.clone(), gave: gives, result });
            }
            // In step with the config now (this person's change may have
            // been the only one): record it, as a full apply would.
            let obj = chipsoc_model::object_key(ObjectKind::Holders, key);
            let in_sync = plan.actions.iter().all(|a| a.object().as_deref() != Some(obj.as_str()))
                || want.iter().filter(|u| live.members.contains(u)).copied().collect::<BTreeSet<_>>() == have;
            if let Some(h) = plan.hashes.get(&obj).filter(|_| in_sync) {
                lock.applied.hashes.insert(obj, h.clone());
            }
        }
        if lock != d.lock {
            lock.guild = Some(g);
            self.trust_lock(&lock).await?;
            cfg.save_lock_async(&lock).await?;
            let mut fresh = (*cfg).clone();
            fresh.desired.lock = lock;
            self.cfg = Arc::new(fresh);
        }
        Ok(out)
    }

    /// Copy changes made in Discord's UI back into the config files. Only
    /// things changed in Discord alone are copied; the rest needs a person.
    pub async fn adopt(&mut self, permissions: bool) -> Result<Vec<Pulled>> {
        self.adopt_by(permissions, &By::bot()).await
    }

    /// [`Engine::adopt`], saying who in the commit message. Holds the apply
    /// lease, so it never interleaves with an apply (here or in another process).
    pub async fn adopt_by(&mut self, permissions: bool, by: &By) -> Result<Vec<Pulled>> {
        let _lease = Lease::take_soon(&self.cfg.dir).await?;
        self.catch_up().await?;
        let g = self.guild().await?;
        let (plan, live) = self.plan_at(g, Options::default()).await?;
        let (list, notes) = adopt::ops(&self.cfg, &live, &plan, permissions);
        if !list.is_empty() {
            self.edit_with(
                move |dir| {
                    let change = ops::apply_ops(dir, &list)?;
                    Ok(((), (!change.files.is_empty()).then(|| "Pull changes made in Discord".to_owned())))
                },
                by,
            )
            .await?
            .1?;
        }
        Ok(notes)
    }
}

/// Read what `baseline` trusts and impose it on `cfg` (the first time, the
/// files' lock is what's trusted). Also what it trusts, and what it changed.
pub async fn vet_with(baseline: &dyn Baseline, cfg: &mut Config) -> Result<(Trusted, Vetted)> {
    let mut t = baseline.load().await?;
    let lock = &cfg.desired.lock;
    if t.lock.is_none() && (lock.guild.is_some() || lock.known() > 0) {
        baseline.save_lock(lock).await?;
        t.lock = Some(lock.clone());
    }
    let vetted = t.vet(cfg);
    Ok((t, vetted))
}

/// Whether this copy of the config is fit to plan or apply from: a git
/// checkout must have what its remote has, and the commit last applied
/// (else someone else's applied change looks like one made in Discord, and
/// gets undone). Offline, or not a checkout, it goes ahead with a note.
fn checkout(cfg: &Config, purpose: &For, autosave: bool) -> Result<Vec<Warning>> {
    let dir = &cfg.dir;
    let sync_error = match purpose {
        For::Apply { sync_error } => sync_error.as_deref(),
        For::Plan => None,
    };
    let strict = matches!(purpose, For::Apply { .. }) || !autosave;
    let note = |text: String| Warning::Note { text };
    let mut notes = Vec::new();
    let stuck = sync_error.map(|e| format!(" (catching up failed: {e})")).unwrap_or_default();
    match git::upstream_state(dir) {
        Upstream::None | Upstream::Current => {}
        Upstream::Unknown(why) => notes.push(note(format!(
            "couldn't check the config's remote for newer commits, so this plan is from this copy as it is ({why})"
        ))),
        Upstream::Behind(n, up) if !strict => notes.push(note(format!(
            "the bot's copy of the config is {n} commit(s) behind {up}; it catches up within 2 minutes, and before applying"
        ))),
        Upstream::Behind(n, up) => bail!(
            "this copy of the config is {n} commit(s) behind {up}{stuck}: pull first (git pull). \
             Planning from an old copy would undo changes others have applied"
        ),
    }
    if let Some(c) = cfg.desired.lock.applied.commit.as_deref()
        && dir.join(".git").exists()
        && !git::contains(dir, c)
    {
        let short = &c[..c.len().min(10)];
        let why = format!(
            "lock.toml says config commit {short} was applied last, and this copy doesn't have it{stuck}: pull first. \
             (If that commit is gone for good, delete the \"git:commit\" line under [applied] in lock.toml.)"
        );
        if strict {
            bail!("{why}");
        }
        notes.push(note(why));
    }
    if !autosave {
        let changed = git::uncommitted(dir);
        if !changed.is_empty() {
            notes.push(note(format!(
                "not committed yet: {}. Commit and push after applying, or others' plans will undo it",
                changed.join(", ")
            )));
        }
    }
    if let Some(kept) = chipsoc_config::fuller_backup(dir, &cfg.desired.lock) {
        notes.push(note(format!(
            "lock.toml knows {} Discord objects but lock.toml.bak knows {}: if lock.toml was lost or emptied, \
             restore it (cp lock.toml.bak lock.toml) before applying",
            cfg.desired.lock.known(),
            kept.known()
        )));
    }
    Ok(notes)
}

/// Sweeping with a lockfile that doesn't know most of what's there would
/// archive the very things the config describes (and then make them again):
/// refused, saying how to recover.
fn sweep_guard(cfg: &Config, plan: &Plan) -> Result<()> {
    if !plan.sweep {
        return Ok(());
    }
    let d = &cfg.desired;
    let known: std::collections::BTreeSet<Snowflake> =
        d.lock.roles.values().chain(d.lock.categories.values()).chain(d.lock.channels.values()).copied().collect();
    let swept: std::collections::BTreeSet<&str> = plan
        .actions
        .iter()
        .filter_map(|a| match a {
            Action::Archive { key, id, .. } if !known.contains(id) => Some(key.as_str()),
            _ => None,
        })
        .collect();
    if swept.is_empty() {
        return Ok(());
    }
    let clashes: Vec<&str> = plan
        .actions
        .iter()
        .filter_map(|a| match a {
            Action::Create { kind: ObjectKind::Category, key } => d.category(key).map(|c| c.category.name.as_str()),
            Action::Create { kind: ObjectKind::Channel, key } => d.channel(key).map(|c| c.name.as_str()),
            _ => None,
        })
        .filter(|name| swept.contains(name))
        .collect();
    let wanted = d.roles.roles.len() + d.categories.len() + d.channels().count();
    let matched = d.lock.roles.len() + d.lock.categories.len() + d.lock.channels.len();
    if clashes.is_empty() && matched * 2 >= wanted {
        return Ok(());
    }
    let mut why = Vec::new();
    if matched * 2 < wanted {
        why.push(format!("lock.toml knows only {matched} of the config's {wanted} roles, categories and channels"));
    }
    if !clashes.is_empty() {
        why.push(format!("it would archive things named like ones the config makes ({})", clashes.join(", ")));
    }
    let why = format!("refusing to sweep: {}", why.join(", and "));
    bail!(
        "{why}. If lock.toml was lost, restore it (cp lock.toml.bak lock.toml, or from git history); \
         otherwise apply without --sweep first, so what's already there is matched by name, then sweep"
    )
}

/// The [`git::fingerprint`] of HEAD if it moved from `before` (a commit
/// was made here, maybe rebased onto the remote's since: then it's still
/// HEAD).
fn committed_since(dir: &Path, before: Option<&str>) -> Option<String> {
    let now = git::head_full(dir)?;
    (Some(now.as_str()) != before).then(|| git::fingerprint(dir, &now)).flatten()
}

/// A commit message, with `by <who>` when someone is named, and their
/// name and Discord id in [`git::NAME_TRAILER`] and [`git::BY_TRAILER`] lines
/// (so history reads who from those, never from the subject alone).
pub fn signed(message: String, by: &By) -> String {
    let who = by.name.as_deref().map(|w| w.replace(['\r', '\n'], " "));
    let mut m = match &who {
        Some(who) => format!("{message} by {who}"),
        None => message,
    };
    let trailers: Vec<String> =
        [who.map(|w| format!("{}: {w}", git::NAME_TRAILER)), by.id.map(|id| format!("{}: {id}", git::BY_TRAILER))]
            .into_iter()
            .flatten()
            .collect();
    if !trailers.is_empty() {
        m.push_str(&format!("\n\n{}", trailers.join("\n")));
    }
    m
}

/// What `plan` shows a person: warnings, skipped steps, then each change.
pub fn plan_lines(p: &Plan) -> Vec<String> {
    let warnings = p.warnings.iter().map(|w| format!("! {w}"));
    let skipped = p.actions.iter().filter(|a| !a.mutates()).map(ToString::to_string);
    // A step's detail lines are indented under it.
    let steps = p.actions.iter().filter(|a| a.mutates()).flat_map(|a| {
        let text = a.to_string();
        let lines = text.lines().enumerate();
        lines.map(|(i, l)| if i == 0 { l.to_owned() } else { format!("  {l}") }).collect::<Vec<_>>()
    });
    warnings.chain(skipped).chain(steps).collect()
}

/// Whether the bot and the web UI should commit and push their changes to
/// this config (see [`Engine::autosave`]): [`autosave_on`] and it's a synced
/// git checkout. Runs git; ask once.
pub fn autosaves(dir: &Path) -> bool {
    autosave_on() && git::is_synced(dir)
}

/// Whether `CONFIG_AUTOSAVE` asks for changes to be committed and pushed.
pub fn autosave_on() -> bool {
    std::env::var("CONFIG_AUTOSAVE").is_ok_and(|v| v == "1" || v == "true")
}

/// One result of [`Engine::adopt`].
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", content = "text", rename_all = "snake_case")]
pub enum Pulled {
    /// Copied into the config files.
    Adopted(String),
    /// Differs, but has to be edited by hand (or undone with apply).
    ByHand(String),
}

impl std::fmt::Display for Pulled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Adopted(s) => write!(f, "✓ {s}"),
            Self::ByHand(s) => write!(f, "! {s}"),
        }
    }
}

/// Hash of what Discord looked like, what the config said and what was asked
/// for (`--delete`, `--sweep`), to spot a stale or altered plan.
pub fn fingerprint(live: &Snapshot, cfg: &Config, opts: Options) -> String {
    let asked = serde_json::json!({ "delete": opts.delete, "sweep": opts.sweep });
    let inputs = serde_json::json!([live, cfg.desired, cfg.bot, cfg.projects, cfg.federation, asked]);
    chipsoc_model::stable_hash(inputs.to_string())
}

/// Export a server into an empty directory as a new config.
pub async fn export_to(rest: &impl Rest, guild: Snowflake, dir: &Path) -> Result<Snapshot> {
    if dir.join("server.toml").exists() {
        bail!("{} already has a config; export into an empty directory", dir.display());
    }
    std::fs::create_dir_all(dir)?;
    let snap = export::export(rest, guild, &Lock::default()).await?;
    let dir = dir.to_owned();
    blocking(move || write_snapshot(&dir, &snap).map(|()| snap)).await
}

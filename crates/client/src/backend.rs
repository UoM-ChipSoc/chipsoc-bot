//! One way for the CLI and TUI to do admin work, wherever it runs: through
//! the bot's web server after `login` ([`Client`]), or here, on the config
//! files, the bot's token and its database. Both go through `chipsoc_admin`
//! and `ops`, as the web pages and the bot do.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use chipsoc_admin::{Actor, Decided, Done, Drift, Entry, FeedInfo, Host, NewResource, Past, Pending, Post, Roster};
use chipsoc_config::ops::{self, Change, Op, Schema};
use chipsoc_config::template::{self, Rendered};
use chipsoc_config::{Config, Snowflake, authoring};
use chipsoc_discord::Rest;
use chipsoc_engine::git::{Undid, Undo};
use chipsoc_engine::{Engine, Options, Progress, Pulled};
use chipsoc_store::Store;
use tokio::sync::OnceCell;

use crate::remote::{Client, Planned, Status, Step};

/// Where admin work goes.
pub enum Backend<R> {
    /// Through the bot's web server, on its copy of the config.
    Remote(Client),
    /// Here: these files, this Discord client, the bot's database if there is one.
    Local(Local<R>),
}

/// Admin work on this machine.
pub struct Local<R> {
    dir: PathBuf,
    rest: R,
    /// The bot's database, if `DATABASE_URL` says where (or the default file exists).
    db: Option<String>,
    store: OnceCell<Store>,
    actor: Actor,
}

impl<R: Rest + Clone> Local<R> {
    /// On the config in `dir`, the database `DATABASE_URL` names (or the
    /// default one, if it's there), acting as this machine's user.
    pub fn new(dir: &Path, rest: R) -> Self {
        let db = std::env::var("DATABASE_URL")
            .ok()
            .or_else(|| Path::new("data/bot.db").exists().then(|| "sqlite:data/bot.db?mode=rw".to_owned()));
        let who = std::env::var("USER").unwrap_or_else(|_| "someone".into());
        Self::with_db(dir, rest, db, &format!("{who} (on this machine)"))
    }

    /// With the database at `db` (none: work that needs it says so), acting as `name`.
    pub fn with_db(dir: &Path, rest: R, db: Option<String>, name: &str) -> Self {
        let actor = Actor { id: None, name: name.to_owned() };
        Self { dir: dir.to_owned(), rest, db, store: OnceCell::new(), actor }
    }

    fn cfg(&self) -> Result<Config> {
        Config::load(&self.dir).with_context(|| format!("loading config from {}", self.dir.display()))
    }

    /// An engine on these files; a person commits what it changes. With the
    /// bot's database, it uses the lock and approved settings the bot trusts
    /// (see [`chipsoc_config::trust`]), as the bot and the web UI do, and keeps
    /// them up to date.
    async fn engine(&self) -> Result<Engine<R>> {
        match self.store().await? {
            Some(store) => {
                let trusted = chipsoc_admin::trust::StoreBaseline::new(store.clone(), &self.dir);
                Engine::unloaded(self.rest.clone(), &self.dir).trusting(trusted).await
            }
            None => Engine::load(self.rest.clone(), &self.dir),
        }
    }

    /// The bot's database, if there is one here.
    async fn store(&self) -> Result<Option<&Store>> {
        let Some(url) = &self.db else { return Ok(None) };
        let store = self.store.get_or_try_init(|| Store::open(url)).await.context("opening the bot's database")?;
        Ok(Some(store))
    }

    /// Config edits, checked as one, written to the files (not committed).
    /// With the bot's database, through [`Local::engine`], so a project rename
    /// moves what the bot keeps under the old name too; a config that doesn't
    /// load can still be edited (to fix it) straight in the files.
    async fn edit(&self, list: &[Op]) -> Result<Change> {
        if self.db.is_some() && self.cfg().is_ok() {
            return self.engine().await?.edit_ops(list, &self.actor.by(true)).await;
        }
        // The bot or web UI may share this checkout: don't write while they do.
        let (dir, list) = (self.dir.clone(), list.to_vec());
        chipsoc_config::blocking(move || {
            let _held = chipsoc_config::WriteLock::take(&dir)?;
            ops::apply_ops(&dir, &list)
        })
        .await
    }

    /// The config directory this works on.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// A [`Host`] for one piece of work.
    async fn desk(&self) -> Result<Desk<'_, R>> {
        let Some(url) = &self.db else {
            bail!(
                "the bot's database isn't on this machine: sign in to the bot's web server \
                 (`chipsoc-bot login --server https://…`), or set DATABASE_URL"
            )
        };
        let store = self.store.get_or_try_init(|| Store::open(url)).await.context("opening the bot's database")?;
        // The settings the bot trusts, not only the files' (see `engine`).
        let e = self.engine().await?;
        let guild = e.guild().await?;
        Ok(Desk { local: self, store, cfg: e.cfg.clone(), guild, gkey: guild.to_string() })
    }
}

struct Desk<'a, R> {
    local: &'a Local<R>,
    store: &'a Store,
    cfg: Arc<Config>,
    guild: Snowflake,
    gkey: String,
}

impl<R: Rest + Clone> Host for Desk<'_, R> {
    type Rest = R;

    fn rest(&self) -> &R {
        &self.local.rest
    }

    fn db(&self) -> chipsoc_store::Guild<'_> {
        self.store.guild(&self.gkey)
    }

    fn cfg(&self) -> Arc<Config> {
        self.cfg.clone()
    }

    fn guild(&self) -> Snowflake {
        self.guild
    }

    /// Here the files are edited in place; committing them is up to the person.
    fn saves(&self) -> bool {
        false
    }

    fn unwritable(&self) -> Option<String> {
        None
    }

    async fn record(&self, ops: &[Op], user: Snowflake, roles: &[String]) -> Result<chipsoc_admin::Recorded> {
        let mut e = self.local.engine().await?;
        chipsoc_admin::record_with(&mut e, &self.local.actor.by(true), ops, user, roles).await
    }

    /// Here: the bot's token, on its own machine.
    fn trusted(&self) -> bool {
        true
    }

    async fn undo(&self, commit: &str, how: Undo, by: &chipsoc_engine::By) -> Result<Option<Undid>> {
        self.local.engine().await?.undo(commit, how, by).await
    }
}

/// What the shared admin code said, as plain text with people's names (an error if it refused).
async fn spoken<H: Host>(h: &H, r: Result<chipsoc_admin::Said, chipsoc_admin::Refusal>) -> Result<String> {
    match r {
        Ok(s) => Ok(plain(h, s).await),
        Err(why) => Err(anyhow::Error::msg(plain(h, why.said()).await)),
    }
}

async fn plain<H: Host>(h: &H, s: chipsoc_admin::Said) -> String {
    s.named(h).await.plain(Some(&h.cfg()))
}

impl<R: Rest + Clone> Backend<R> {
    /// Who's acting, for messages.
    pub fn who(&self) -> String {
        match self {
            Self::Remote(c) => format!("{} on {}", c.login.user, c.login.server),
            Self::Local(l) => l.actor.name.clone(),
        }
    }

    pub fn is_remote(&self) -> bool {
        matches!(self, Self::Remote(_))
    }

    /// What applying would change, with the id to apply it by. The web server
    /// keeps its plans under their ids; here the plan is checked again
    /// against a fresh one when it's applied.
    pub async fn plan(&self, opts: Options) -> Result<Planned> {
        match self {
            Self::Remote(c) => c.plan(opts.delete, opts.sweep).await,
            Self::Local(l) => {
                let plan = l.engine().await?.plan(opts).await?;
                Ok(Planned { id: plan.fingerprint.clone(), lines: chipsoc_engine::plan_lines(&plan), plan })
            }
        }
    }

    /// Apply all of a plan from [`Backend::plan`], telling `progress` about each step.
    pub async fn apply(&self, p: &Planned, progress: &mut (impl Progress + Send)) -> Result<Vec<Step>> {
        match self {
            Self::Remote(c) => {
                let steps = c.apply(&p.id).await?;
                for s in &steps {
                    progress.step(&s.action, &s.error.clone().map_or(Ok(()), Err));
                }
                Ok(steps)
            }
            Self::Local(l) => {
                let report = l.engine().await?.apply_by(&p.plan, &l.actor.by(true), progress).await?;
                Ok(report.outcomes.into_iter().map(|o| Step { action: o.action, error: o.result.err() }).collect())
            }
        }
    }

    /// Copy changes made in Discord into the config (the server's copy, through it).
    pub async fn pull(&self, permissions: bool) -> Result<Vec<Pulled>> {
        match self {
            Self::Remote(c) => c.pull(permissions).await,
            Self::Local(l) => l.engine().await?.adopt_by(permissions, &l.actor.by(true)).await,
        }
    }

    /// The config's latest changes, or what `user` did in the last week (see [`chipsoc_admin::history_of`]).
    pub async fn changes(&self, user: Option<Snowflake>, limit: usize) -> Result<Vec<Done>> {
        match self {
            Self::Remote(c) => c.changes(user, limit).await,
            Self::Local(l) => chipsoc_admin::history_of(&l.desk().await?, user, limit).await,
        }
    }

    /// Go back on a config commit (the server owner only, through the server).
    /// What it came to, in words (see [`chipsoc_admin::undo_said`]): `Err` if
    /// nothing was changed because it was refused. Apply to send it to Discord.
    pub async fn undo(&self, commit: &str, how: Undo) -> Result<String> {
        let made = match self {
            Self::Remote(c) => c.undo(commit, how).await.map_err(|e| format!("{e:#}").into()),
            Self::Local(l) => chipsoc_admin::undo(&l.desk().await?, commit, how, &l.actor).await,
        };
        let text = chipsoc_admin::undo_said(&made).plain(None);
        if made.is_err() { Err(anyhow::Error::msg(text)) } else { Ok(text) }
    }

    pub async fn status(&self) -> Result<Status> {
        match self {
            Self::Remote(c) => c.status().await,
            Self::Local(l) => {
                let cfg = l.cfg()?;
                let queue = match l.desk().await {
                    Ok(d) => d.db().pending().await.unwrap_or_default(),
                    Err(_) => 0,
                };
                Ok(Status::of(&cfg, queue))
            }
        }
    }

    pub async fn drift(&self) -> Result<Drift> {
        match self {
            Self::Remote(c) => c.drift().await,
            Self::Local(l) => {
                let plan = l.engine().await?.plan(Options::default()).await?;
                Ok(chipsoc_admin::drift(&plan))
            }
        }
    }

    pub async fn schema(&self) -> Result<Schema> {
        match self {
            Self::Remote(c) => c.schema().await,
            Self::Local(l) => ops::schema(&l.dir),
        }
    }

    /// One config edit, checked first (here: written to the files, not committed).
    pub async fn op(&self, op: &Op) -> Result<Change> {
        match self {
            Self::Remote(c) => c.op(op).await,
            Self::Local(l) => l.edit(std::slice::from_ref(op)).await,
        }
    }

    /// Several config edits made as one change (one check, one commit: all or none).
    pub async fn ops(&self, list: &[Op]) -> Result<Change> {
        match self {
            Self::Remote(c) => c.ops(list).await,
            Self::Local(l) => l.edit(list).await,
        }
    }

    pub async fn preview(&self, body: &str) -> Result<Rendered> {
        match self {
            Self::Remote(c) => c.preview(body).await,
            Self::Local(l) => {
                let cfg = l.cfg()?;
                Ok(template::render(&cfg.fill_lists(body), &cfg.desired.lock))
            }
        }
    }

    pub async fn check(&self) -> Result<authoring::Report> {
        match self {
            Self::Remote(c) => {
                let r = c.check().await?;
                Ok(authoring::Report { errors: r.errors, warnings: r.warnings })
            }
            Self::Local(l) => authoring::check(&l.dir),
        }
    }

    pub async fn queue(&self) -> Result<Vec<Pending>> {
        match self {
            Self::Remote(c) => c.queue().await,
            Self::Local(l) => chipsoc_admin::pending(&l.desk().await?).await,
        }
    }

    pub async fn history(&self, limit: usize) -> Result<Vec<Past>> {
        match self {
            Self::Remote(c) => c.history(limit).await,
            Self::Local(l) => chipsoc_admin::history(&l.desk().await?, limit as i64).await,
        }
    }

    pub async fn decide(&self, id: i64, accept: bool, reason: Option<&str>) -> Result<Decided> {
        match self {
            Self::Remote(c) => c.decide(id, accept, reason).await,
            Self::Local(l) => {
                let d = l.desk().await?;
                match chipsoc_admin::decide_with(&d, id, accept, reason, &l.actor).await {
                    Ok(mut decided) => {
                        decided.outcome = decided.outcome.named(&d).await;
                        Ok(decided)
                    }
                    Err(why) => Err(anyhow::Error::msg(plain(&d, why.said()).await)),
                }
            }
        }
    }

    pub async fn make_lead(&self, id: i64) -> Result<String> {
        match self {
            Self::Remote(c) => c.make_lead(id).await,
            Self::Local(l) => {
                let d = l.desk().await?;
                spoken(&d, chipsoc_admin::make_lead(&d, id, &l.actor).await).await
            }
        }
    }

    pub async fn feeds(&self) -> Result<Vec<FeedInfo>> {
        match self {
            Self::Remote(c) => c.feeds().await,
            Self::Local(l) => chipsoc_admin::feeds(&l.desk().await?).await,
        }
    }

    pub async fn add_feed(&self, url: &str) -> Result<String> {
        match self {
            Self::Remote(c) => c.add_feed(url).await,
            Self::Local(l) => {
                let title = chipsoc_admin::add_feed(&l.desk().await?, url, &l.actor).await?;
                Ok(format!("Now following {title}."))
            }
        }
    }

    pub async fn pause_feed(&self, id: i64, paused: bool) -> Result<()> {
        match self {
            Self::Remote(c) => c.pause_feed(id, paused).await,
            Self::Local(l) => found(chipsoc_admin::pause_feed(&l.desk().await?, id, paused, &l.actor).await?, "feed"),
        }
    }

    pub async fn remove_feed(&self, id: i64) -> Result<()> {
        match self {
            Self::Remote(c) => c.remove_feed(id).await,
            Self::Local(l) => found(chipsoc_admin::remove_feed(&l.desk().await?, id, &l.actor).await?, "feed"),
        }
    }

    pub async fn resources(&self, limit: usize) -> Result<Vec<Post>> {
        match self {
            Self::Remote(c) => c.resources(limit).await,
            Self::Local(l) => chipsoc_admin::resources(&l.desk().await?, limit).await,
        }
    }

    pub async fn add_resource(&self, r: &NewResource) -> Result<Post> {
        match self {
            Self::Remote(c) => c.add_resource(r).await,
            Self::Local(l) => chipsoc_admin::post_resource(&l.desk().await?, r, &l.actor).await,
        }
    }

    pub async fn archive_resource(&self, id: Snowflake) -> Result<()> {
        match self {
            Self::Remote(c) => c.archive_resource(id).await,
            Self::Local(l) => {
                let done = chipsoc_admin::archive_resource(&l.desk().await?, id, &l.actor).await?;
                found(done, "post in the resources forum")
            }
        }
    }

    /// Share an announcement or event with partners; whom it went to.
    pub async fn share(&self, r: &crate::remote::ShareRequest) -> Result<Vec<String>> {
        match self {
            Self::Remote(c) => c.share(r).await.map(|s| s.partners),
            Self::Local(l) => {
                let pick = chipsoc_admin::shares::Pick::parse(&r.what, &r.item)?;
                chipsoc_admin::shares::share_now(&l.desk().await?, pick, &r.partners, &l.actor).await
            }
        }
    }

    pub async fn ideas(&self, limit: usize) -> Result<Vec<Post>> {
        match self {
            Self::Remote(c) => c.ideas(limit).await,
            Self::Local(l) => chipsoc_admin::ideas(&l.desk().await?, limit).await,
        }
    }

    pub async fn activity(&self, limit: usize) -> Result<Vec<Entry>> {
        match self {
            Self::Remote(c) => c.activity(limit).await,
            Self::Local(l) => chipsoc_admin::activity(&l.desk().await?, limit as i64).await,
        }
    }

    /// How the join flow is going.
    pub async fn join(&self) -> Result<chipsoc_admin::join::Progress> {
        match self {
            Self::Remote(c) => c.join().await,
            Self::Local(l) => chipsoc_admin::join::progress(&l.desk().await?).await,
        }
    }

    /// Who the join flow's one-off notice would go to, and what it says.
    pub async fn join_notice(&self) -> Result<chipsoc_admin::join::Notice> {
        match self {
            Self::Remote(c) => c.join_notice().await,
            Self::Local(l) => chipsoc_admin::join::preview(&l.desk().await?).await,
        }
    }

    /// Send the notice (once per member, ever); says how it went or that it's under way.
    pub async fn send_join_notice(&self) -> Result<String> {
        match self {
            Self::Remote(c) => {
                let n = c.send_join_notice().await?;
                Ok(format!("Sending to {} members; the bot log hears when it's done.", n.members))
            }
            Self::Local(l) => {
                let d = l.desk().await?;
                let n = chipsoc_admin::join::send_notice(&d, &l.actor, chipsoc_admin::join::NOTICE_PACE).await?;
                Ok(format!("Sent {}, {} with DMs closed, {} failed.", n.sent, n.closed, n.failed))
            }
        }
    }

    pub async fn members(&self) -> Result<Roster> {
        match self {
            Self::Remote(c) => c.members().await,
            Self::Local(l) => chipsoc_admin::roster(&l.desk().await?).await,
        }
    }

    /// Who a username or name (`@ada`, `Ada Lovelace`) could be.
    pub async fn find_members(&self, query: &str) -> Result<chipsoc_admin::Found> {
        match self {
            Self::Remote(c) => c.find_members(query).await,
            Self::Local(l) => chipsoc_admin::find_members(&l.desk().await?, query).await,
        }
    }

    /// Who `typed` means: an id or mention as it is, else the one member
    /// with that @username or name. Several or nobody: an error saying who.
    pub async fn user(&self, typed: &str) -> Result<Snowflake> {
        if let Some(id) = Snowflake::parse_mention(typed) {
            return Ok(id);
        }
        let found = self.find_members(typed).await?;
        found.one(typed).map_err(|e| match e {
            chipsoc_admin::Unresolved::Several(m) => {
                let who: Vec<String> = m.iter().map(|m| format!("{} ({})", m.label(), m.id)).collect();
                anyhow::anyhow!("More than one person matches: {}. Give their @username or id.", who.join(", "))
            }
            e => anyhow::anyhow!("{e}"),
        })
    }

    /// What `user` told the bot: their profile and join answers.
    pub async fn about(&self, user: Snowflake) -> Result<chipsoc_admin::join::About> {
        match self {
            Self::Remote(c) => c.about(user).await,
            Self::Local(l) => chipsoc_admin::join::about(&l.desk().await?, user).await,
        }
    }

    pub async fn appoint(&self, role: &str, user: Snowflake) -> Result<String> {
        match self {
            Self::Remote(c) => c.appoint(role, user).await,
            Self::Local(l) => {
                let d = l.desk().await?;
                spoken(&d, chipsoc_admin::appoint(&d, role, user, &l.actor).await).await
            }
        }
    }

    pub async fn unappoint(&self, role: &str, user: Snowflake) -> Result<String> {
        match self {
            Self::Remote(c) => c.unappoint(role, user).await,
            Self::Local(l) => {
                let d = l.desk().await?;
                spoken(&d, chipsoc_admin::unappoint(&d, role, user, &l.actor).await).await
            }
        }
    }

    pub async fn add_lead(&self, slug: &str, user: Snowflake) -> Result<String> {
        match self {
            Self::Remote(c) => c.add_lead(slug, user).await,
            Self::Local(l) => {
                let d = l.desk().await?;
                spoken(&d, chipsoc_admin::add_project_lead(&d, slug, user, &l.actor).await).await
            }
        }
    }

    pub async fn remove_lead(&self, slug: &str, user: Snowflake) -> Result<String> {
        match self {
            Self::Remote(c) => c.remove_lead(slug, user).await,
            Self::Local(l) => {
                let d = l.desk().await?;
                spoken(&d, chipsoc_admin::remove_lead(&d, slug, user, &l.actor).await).await
            }
        }
    }

    pub async fn restore(&self, user: Snowflake) -> Result<String> {
        match self {
            Self::Remote(c) => c.restore(user).await,
            Self::Local(l) => {
                let d = l.desk().await?;
                spoken(&d, chipsoc_admin::restore(&d, user, &l.actor).await).await
            }
        }
    }

    pub async fn forget(&self, user: Snowflake) -> Result<()> {
        match self {
            Self::Remote(c) => c.forget(user).await,
            Self::Local(l) => found(chipsoc_admin::forget_former(&l.desk().await?, user, &l.actor).await?, "person"),
        }
    }
}

fn found(done: bool, what: &str) -> Result<()> {
    if done { Ok(()) } else { bail!("no such {what}") }
}

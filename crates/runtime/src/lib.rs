//! The bot: slash commands, buttons and forms, the review queue, logs, feeds,
//! reminders, drift watch and partner sync. One process can serve several
//! servers, each with its own config directory.

mod alarm;
mod apps;
mod commands;
mod control;
mod debounce;
mod drift;
mod events;
mod federation;
mod fedrecv;
mod fedshare;
mod fedsync;
mod feeds;
mod gitsync;
mod help;
mod ideas;
mod join;
mod limits;
mod manage;
mod members;
mod modlog;
mod plans;
mod profile;
mod queue;
mod record;
mod reload;
mod resources;
mod review;
mod roster;
mod trust;
mod ui;
mod webaccess;

#[cfg(test)]
mod tests;

pub use fedrecv::{Revoked, revoke_partner};

use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use chipsoc_config::Config;
use chipsoc_store::Store;
use futures_util::StreamExt as _;
use twilight_gateway::{CloseFrame, Event, EventTypeFlags, Intents, Message, Shard, ShardId, StreamExt};
use twilight_model::application::interaction::Interaction;
use twilight_model::channel::message::{AllowedMentions, Component};
use twilight_model::guild::{PartialMember, Permissions};
use twilight_model::http::interaction::InteractionResponse;
use twilight_model::id::Id;
use twilight_model::id::marker::{ApplicationMarker, ChannelMarker, GuildMarker, RoleMarker, UserMarker};

use crate::queue::Queue;

/// Events waiting for a server's ordered worker.
const EVENT_QUEUE: usize = 1000;

pub(crate) use chipsoc_admin::now;

/// Discord's REST API: the live client, or in tests the fake.
#[derive(Clone)]
pub(crate) enum Api {
    Live(chipsoc_discord::Http),
    #[cfg(test)]
    Fake(Arc<chipsoc_discord::fake::FakeDiscord>),
}

impl chipsoc_discord::Rest for Api {
    async fn call(
        &self,
        method: chipsoc_discord::Method,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> Result<serde_json::Value> {
        match self {
            Self::Live(h) => h.call(method, path, body).await,
            #[cfg(test)]
            Self::Fake(f) => f.call(method, path, body).await,
        }
    }
}

/// Discord's REST API as JSON (the fake in tests), for one server: its
/// member list is the one kept from the gateway (see [`members`]).
#[derive(Clone)]
pub(crate) struct Discord {
    pub api: Api,
    guild: chipsoc_config::Snowflake,
    pub members: Arc<members::Members>,
}

impl chipsoc_discord::Rest for Discord {
    async fn call(
        &self,
        method: chipsoc_discord::Method,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> Result<serde_json::Value> {
        self.api.call(method, path, body).await
    }

    async fn members(&self, guild: chipsoc_config::Snowflake) -> Result<Arc<Vec<serde_json::Value>>> {
        if guild == self.guild { self.members.get(&self.api, guild).await } else { self.api.members(guild).await }
    }
}

/// Everything a handler needs for one server.
pub(crate) struct Ctx {
    /// Discord's REST API as JSON (the fake in tests).
    pub rest: Discord,
    pub app: Id<ApplicationMarker>,
    pub me: Id<UserMarker>,
    pub guild: Id<GuildMarker>,
    /// The guild id as text, for the store.
    pub gkey: String,
    pub store: Store,
    /// The config and the federation routes resolved from it, swapped together.
    loaded: RwLock<(Arc<Config>, Arc<federation::Routes>)>,
    pub fed: fedsync::State,
    /// Message and member events, handled in order by `ordered_worker`.
    events: Queue<Event>,
    /// Plans posted with an Apply button.
    pub plans: plans::Plans,
    /// Discord events that may mean drift, waiting for a check.
    pub drift: drift::Trigger,
    /// Held while the bot changes the config or Discord, so changes never interleave.
    pub changing: tokio::sync::Mutex<()>,
    /// The fingerprint of the last plan posted in the bot log for a config
    /// change, so each change is announced once.
    pub announced: tokio::sync::Mutex<Option<String>>,
    /// The bot's own applies in the audit log: it closes plan posts for those
    /// itself, so [`control::close_applied`] skips them.
    pub own_applies: std::sync::Mutex<std::collections::BTreeSet<i64>>,
    /// How often each member may send forms.
    pub limits: limits::Limits,
    /// The resources forum's posts, kept between members' submits.
    pub seen_posts: chipsoc_admin::forums::SeenPosts,
    /// Whether config changes are committed and pushed (see [`chipsoc_engine::autosaves`]).
    pub autosave: bool,
    /// Who the admins are, until a role, the server or the config changes.
    pub admins: webaccess::AdminCache,
    /// `/record`: who's in which call, and the recording if one is running.
    pub rec: record::State,
}

impl Ctx {
    /// The current config (reloaded when its files change, and by the drift watch).
    pub fn cfg(&self) -> Arc<Config> {
        self.loaded.read().unwrap_or_else(|e| e.into_inner()).0.clone()
    }

    /// Which channels are published and relayed to partners, for the current config.
    pub fn routes(&self) -> Arc<federation::Routes> {
        self.loaded.read().unwrap_or_else(|e| e.into_inner()).1.clone()
    }

    fn set_cfg(&self, c: Arc<Config>) {
        let routes = Arc::new(federation::Routes::resolve(&c));
        *self.loaded.write().unwrap_or_else(|e| e.into_inner()) = (c, routes);
        // The admin role may be another now.
        self.admins.forget();
    }

    pub fn db(&self) -> chipsoc_store::Guild<'_> {
        self.store.guild(&self.gkey)
    }

    pub fn channel(&self, key: &str) -> Option<Id<ChannelMarker>> {
        self.cfg().desired.lock.channel(key).and_then(|s| Id::new_checked(s.0))
    }

    pub fn role(&self, key: &str) -> Option<Id<RoleMarker>> {
        self.cfg().desired.lock.role(key).and_then(|s| Id::new_checked(s.0))
    }

    pub fn role_name(&self, key: &str) -> String {
        self.cfg().desired.role(key).map_or_else(|| key.to_owned(), |r| r.name.clone())
    }

    /// The community's name, for messages.
    pub fn name(&self) -> String {
        self.cfg().bot.name.clone()
    }

    /// Server admins, and members of `admin_role`.
    pub fn is_admin(&self, member: Option<&PartialMember>) -> bool {
        let Some(m) = member else { return false };
        let perms = m.permissions.unwrap_or(Permissions::empty());
        let admin_role = self.cfg().bot.admin_role.clone();
        perms.intersects(Permissions::ADMINISTRATOR | Permissions::MANAGE_GUILD)
            || admin_role.and_then(|k| self.role(&k)).is_some_and(|r| m.roles.contains(&r))
    }

    /// Send `body` to Discord, as JSON.
    async fn send(&self, method: chipsoc_discord::Method, path: &str, body: serde_json::Value) -> Result<()> {
        self.call(method, path, Some(body)).await.map(drop)
    }

    /// Discord's REST API, as JSON in and out (see [`chipsoc_discord::Rest`]).
    pub async fn call(
        &self,
        method: chipsoc_discord::Method,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> Result<serde_json::Value> {
        use chipsoc_discord::Rest as _;
        self.rest.call(method, path, body).await
    }

    /// `GET path`.
    pub async fn get(&self, path: &str) -> Result<serde_json::Value> {
        self.call(chipsoc_discord::Method::Get, path, None).await
    }

    /// Everyone in the server (see [`members`]). Errors abort: a partial list
    /// would read as "these people left".
    pub async fn members(&self) -> Result<Arc<Vec<serde_json::Value>>> {
        self.rest.members.get(&self.rest.api, chipsoc_config::Snowflake(self.guild.get())).await
    }

    pub async fn respond(&self, i: &Interaction, resp: &InteractionResponse) {
        let sent = async {
            let path = format!("interactions/{}/{}/callback", i.id, i.token);
            self.send(chipsoc_discord::Method::Post, &path, serde_json::to_value(resp)?).await
        };
        if let Err(e) = sent.await {
            tracing::warn!(?e, "interaction response failed");
        }
    }

    /// A message's JSON: `text` cut to fit, buttons if any, pinging nobody.
    fn message(text: &str, components: Option<&[Component]>) -> Result<serde_json::Value> {
        let mut m = serde_json::json!({
            "content": ui::truncate(text, ui::MAX_CONTENT),
            "allowed_mentions": AllowedMentions::default(),
        });
        if let Some(c) = components {
            m["components"] = serde_json::to_value(c)?;
        }
        Ok(m)
    }

    /// Post without pinging anyone.
    pub async fn say(&self, channel: Id<ChannelMarker>, text: &str) {
        let sent = async {
            self.send(
                chipsoc_discord::Method::Post,
                &format!("channels/{channel}/messages"),
                Self::message(text, None)?,
            )
            .await
        };
        if let Err(e) = sent.await {
            tracing::warn!(?e, %channel, "post failed");
        }
    }

    /// Post with buttons, without pinging anyone; the message as Discord made it.
    pub async fn post(
        &self,
        channel: Id<ChannelMarker>,
        text: &str,
        components: &[Component],
    ) -> Result<serde_json::Value> {
        let path = format!("channels/{channel}/messages");
        self.call(chipsoc_discord::Method::Post, &path, Some(Self::message(text, Some(components))?)).await
    }

    /// Post with buttons, without pinging anyone.
    pub async fn say_with(&self, channel: Id<ChannelMarker>, text: &str, components: &[Component]) {
        let body = Self::message(text, Some(components));
        let path = format!("channels/{channel}/messages");
        let sent = async { self.send(chipsoc_discord::Method::Post, &path, body?).await };
        self.log_err(sent.await, "post with buttons");
    }

    /// Fill in a deferred response (or the message under a deferred button).
    pub async fn edit_response(&self, i: &Interaction, text: &str, components: &[Component]) {
        let body = Self::message(text, Some(components));
        let path = format!("webhooks/{}/{}/messages/@original", self.app, i.token);
        let edit = async { self.send(chipsoc_discord::Method::Patch, &path, body?).await };
        self.log_err(edit.await, "editing a response");
    }

    /// Post to a channel named in config, if it's set up.
    pub async fn say_in(&self, key: Option<&str>, text: &str) {
        if let Some(ch) = key.and_then(|k| self.channel(k)) {
            self.say(ch, text).await;
        }
    }

    /// The people log: edits, deletes, joins, leaves, decisions.
    pub async fn log_people(&self, text: &str) {
        self.say_in(self.cfg().bot.log.people.as_deref(), text).await;
    }

    /// The bot log channel (or the people log if there's no bot log).
    pub fn bot_log(&self) -> Option<Id<ChannelMarker>> {
        let cfg = self.cfg();
        cfg.bot.log.bot.as_deref().or(cfg.bot.log.people.as_deref()).and_then(|k| self.channel(k))
    }

    /// The bot log: drift, partner sync, problems.
    pub async fn log_bot(&self, text: &str) {
        if let Some(ch) = self.bot_log() {
            self.say(ch, text).await;
        }
    }

    /// DM someone.
    pub async fn dm(&self, user: Id<UserMarker>, text: &str) -> Result<(), chipsoc_admin::DmError> {
        chipsoc_admin::dm(self, chipsoc_config::Snowflake(user.get()), text, None).await
    }

    /// DM someone with buttons, whose ids carry this server (see [`commands::CustomId::in_dm`]).
    pub async fn dm_with(
        &self,
        user: Id<UserMarker>,
        text: &str,
        components: &[Component],
    ) -> Result<(), chipsoc_admin::DmError> {
        let rows = serde_json::to_value(components).map_err(|e| chipsoc_admin::DmError::Failed(e.to_string()))?;
        chipsoc_admin::dm(self, chipsoc_config::Snowflake(user.get()), text, Some(rows)).await
    }

    pub async fn audit(&self, actor: Option<&str>, action: &str, target: Option<&str>, detail: Option<&str>) {
        self.log_err(self.db().audit(now(), actor, action, target, detail).await, "audit log");
    }

    /// Log a failed best-effort operation instead of dropping it.
    pub fn log_err<T, E: std::fmt::Debug>(&self, r: Result<T, E>, what: &str) {
        if let Err(e) = r {
            tracing::warn!(?e, guild = %self.guild, "{what} failed");
        }
    }
}

#[cfg(test)]
impl Ctx {
    /// A server's context on the fake Discord, with an in-memory store, for
    /// handler tests. Its member list is read afresh each time (the fake
    /// sends no member events).
    pub(crate) async fn on_fake(cfg: Config, fake: Arc<chipsoc_discord::fake::FakeDiscord>, autosave: bool) -> Self {
        Self {
            rest: Discord {
                api: Api::Fake(fake),
                guild: chipsoc_config::Snowflake(1000),
                members: Arc::new(members::Members::new(Duration::ZERO)),
            },
            app: Id::new(1),
            me: Id::new(42),
            guild: Id::new(1000),
            gkey: "1000".into(),
            store: Store::open("sqlite::memory:").await.unwrap(),
            fed: fedsync::State::new(&cfg),
            loaded: {
                let routes = Arc::new(federation::Routes::resolve(&cfg));
                RwLock::new((Arc::new(cfg), routes))
            },
            events: Queue::new("server events", EVENT_QUEUE),
            plans: Default::default(),
            drift: Default::default(),
            changing: Default::default(),
            announced: Default::default(),
            own_applies: Default::default(),
            limits: Default::default(),
            seen_posts: Default::default(),
            autosave,
            admins: Default::default(),
            rec: Default::default(),
        }
    }
}

/// Run a background job forever, restarting it (after a pause) if it panics.
fn supervise<F, Fut>(name: &'static str, job: F)
where
    F: Fn() -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    tokio::spawn(async move {
        loop {
            match tokio::spawn(job()).await {
                Err(e) => tracing::error!(name, ?e, "background job crashed; restarting in 30 s"),
                Ok(()) => tracing::warn!(name, "background job stopped; restarting in 30 s"),
            }
            tokio::time::sleep(Duration::from_secs(30)).await;
        }
    });
}

/// Run `work` on its own task; if it panics, that's logged like any other
/// error (not only printed to stderr).
fn spawn_logged(what: &'static str, work: impl Future<Output = ()> + Send + 'static) {
    tokio::spawn(unless_it_panics(what, work));
}

/// Run `work` on its own task: what it returns, or `None` if it panicked
/// (logged like any other error).
async fn unless_it_panics<T: Send + 'static>(
    what: &'static str,
    work: impl Future<Output = T> + Send + 'static,
) -> Option<T> {
    match tokio::spawn(work).await {
        Ok(v) => Some(v),
        Err(e) => {
            tracing::error!(what, ?e, "crashed");
            None
        }
    }
}

/// A server's config as the files have it, and its guild id from the lockfile.
fn load_server(dir: &Path) -> Result<(Config, Id<GuildMarker>)> {
    let cfg = Config::load(dir).with_context(|| format!("loading {}", dir.display()))?;
    let guild = cfg.guild().and_then(|g| Id::new_checked(g.0));
    let guild = guild
        .with_context(|| format!("{} has no server id in lock.toml yet; run `apply` once first", dir.display()))?;
    Ok((cfg, guild))
}

/// What every server's [`Ctx`] shares.
#[derive(Clone)]
struct Shared {
    /// One client, so rate limits are shared too.
    http: chipsoc_discord::Http,
    app: Id<ApplicationMarker>,
    me: Id<UserMarker>,
    store: Store,
    /// The gateway, for joining calls to record them.
    voice: Arc<dyn record::Voice>,
}

impl Shared {
    /// A server's config with the settings the bot trusts (see [`trust`]),
    /// and its guild id. One the bot can't vet, or with problems `check`
    /// finds, isn't used, as when the files don't load.
    async fn load(&self, dir: &Path) -> Result<(Config, Id<GuildMarker>)> {
        let d = dir.to_owned();
        let (mut cfg, guild) = chipsoc_config::git::blocking(move || load_server(&d)).await?;
        chipsoc_admin::trust::vet(&self.store, &self.http, dir, &mut cfg)
            .await
            .with_context(|| format!("reading what the bot trusts about {}", dir.display()))?;
        let problems = reload::problems(&cfg);
        if !problems.is_empty() {
            return Err(chipsoc_config::authoring::Problems(problems).into());
        }
        Ok((cfg, guild))
    }

    /// Start serving a server (its config from [`Shared::load`]): its commands and background jobs.
    async fn start(&self, dir: &Path, cfg: Config, guild: Id<GuildMarker>) -> Arc<Ctx> {
        let d = dir.to_owned();
        let autosave = chipsoc_config::git::blocking(move || Ok(chipsoc_engine::autosaves(&d))).await.unwrap_or(false);
        let ctx = Arc::new(Ctx {
            autosave,
            rest: Discord {
                api: Api::Live(self.http.clone()),
                guild: chipsoc_config::Snowflake(guild.get()),
                members: Arc::new(members::Members::new(members::FRESH)),
            },
            app: self.app,
            me: self.me,
            guild,
            gkey: guild.to_string(),
            store: self.store.clone(),
            fed: fedsync::State::new(&cfg),
            loaded: {
                let routes = Arc::new(federation::Routes::resolve(&cfg));
                RwLock::new((Arc::new(cfg), routes))
            },
            events: Queue::new("server events", EVENT_QUEUE),
            plans: Default::default(),
            drift: Default::default(),
            changing: Default::default(),
            announced: Default::default(),
            own_applies: Default::default(),
            limits: Default::default(),
            seen_posts: Default::default(),
            admins: Default::default(),
            rec: record::State::new(self.voice.clone()),
        });
        commands::register(&ctx).await;
        trust::review(&ctx, dir).await;
        start_jobs(&ctx, dir);
        ctx
    }
}

/// A server whose config didn't load, now loading.
type Loaded = (PathBuf, Config, Id<GuildMarker>);

/// Run the bot for one or more servers until stopped. A server whose config
/// doesn't load is kept waiting (and said in its logs): its checkout keeps
/// syncing, so a fixed push starts it. Fails only if no config was given.
pub async fn run(dirs: &[PathBuf]) -> Result<()> {
    if dirs.is_empty() {
        bail!("no config directory given");
    }
    // A rustls crypto provider must be installed before the gateway opens its socket.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let token = std::env::var("DISCORD_TOKEN").context("DISCORD_TOKEN isn't set; put it in .env")?;
    let app = std::env::var("APPLICATION_ID").context("APPLICATION_ID isn't set; put it in .env")?.parse()?;
    let store = Store::from_env().await?;
    // Messages ping nobody unless they say whom: see `chipsoc_discord::Http`.
    let http = chipsoc_discord::Http::new(token.clone());
    let me = chipsoc_discord::Rest::call(&http, chipsoc_discord::Method::Get, "users/@me", None).await?;
    let me = chipsoc_discord::wire::id(&me["id"])
        .and_then(|id| Id::new_checked(id.0))
        .context("reading the bot's own user")?;
    let intents = Intents::GUILDS
        | Intents::GUILD_MEMBERS
        | Intents::GUILD_MESSAGES
        | Intents::GUILD_VOICE_STATES
        | Intents::MESSAGE_CONTENT
        | Intents::DIRECT_MESSAGES
        | Intents::GUILD_SCHEDULED_EVENTS
        | Intents::AUTO_MODERATION_CONFIGURATION;
    let mut shard = Shard::new(ShardId::ONE, token, intents);
    let voice = Arc::new(record::Live::new(shard.sender()));
    let shared = Shared { http, app, me, store, voice };

    let mut guilds: HashMap<Id<GuildMarker>, Arc<Ctx>> = HashMap::new();
    let (loaded_tx, mut loaded) = tokio::sync::mpsc::unbounded_channel::<Loaded>();
    let mut waiting = 0;
    for dir in dirs {
        // A rebase an older version left stuck would leave lock.toml unreadable.
        if let Err(e) = chipsoc_config::git::recover(dir) {
            tracing::warn!(dir = %dir.display(), "checking the config checkout failed: {e:#}");
        }
        let (cfg, guild) = match shared.load(dir).await {
            Ok(x) => x,
            Err(e) => {
                tracing::error!("{e:#}; this server waits until its config loads");
                waiting += 1;
                tokio::spawn(wait_for_config(shared.clone(), dir.clone(), loaded_tx.clone()));
                continue;
            }
        };
        if guilds.contains_key(&guild) {
            tracing::error!(dir = %dir.display(), %guild, "another config already runs this server; skipped");
            continue;
        }
        guilds.insert(guild, shared.start(dir, cfg, guild).await);
    }
    // Servers whose config loads later start on their own task, so the
    // gateway is read meanwhile (a slow start would miss its heartbeats).
    // Each says how its start went: the server, or `None` if starting it crashed.
    let (started_tx, mut started) =
        tokio::sync::mpsc::unbounded_channel::<(PathBuf, Id<GuildMarker>, Option<Arc<Ctx>>)>();
    let mut starting: std::collections::HashSet<Id<GuildMarker>> = Default::default();
    if guilds.is_empty() {
        tracing::error!("no server's config loads yet; waiting for a fixed config");
    }

    tracing::info!(servers = guilds.len(), waiting, "connected; bot running");
    let mut stop = std::pin::pin!(stopped());
    loop {
        let item = tokio::select! {
            item = shard.next_event(EventTypeFlags::all()) => item,
            Some((dir, cfg, guild)) = loaded.recv() => {
                if guilds.contains_key(&guild) || !starting.insert(guild) {
                    tracing::error!(dir = %dir.display(), %guild, "another config already runs this server; skipped");
                } else {
                    let (shared, tx) = (shared.clone(), started_tx.clone());
                    tokio::spawn(async move {
                        let (s, d) = (shared.clone(), dir.clone());
                        let ctx = unless_it_panics("starting a server", async move { s.start(&d, cfg, guild).await });
                        let _ = tx.send((dir, guild, ctx.await));
                    });
                }
                continue;
            }
            Some((dir, guild, ctx)) = started.recv() => {
                starting.remove(&guild);
                match ctx {
                    Some(ctx) => {
                        tracing::info!(dir = %dir.display(), %guild, "the config loads now; server started");
                        guilds.insert(guild, ctx);
                    }
                    None => {
                        // Waits as a config that doesn't load does, and tries again.
                        tracing::error!(dir = %dir.display(), %guild, "starting the server crashed; trying again soon");
                        let (shared, tx) = (shared.clone(), loaded_tx.clone());
                        tokio::spawn(async move {
                            tokio::time::sleep(gitsync::EVERY).await;
                            wait_for_config(shared, dir, tx).await;
                        });
                    }
                }
                continue;
            }
            _ = &mut stop => break,
        };
        let Some(item) = item else { break };
        match item {
            Ok(event) => route(&guilds, event),
            Err(e) => tracing::warn!(?e, "gateway receive error"),
        }
    }
    tracing::info!("shutting down");
    close(&mut shard).await;
    Ok(())
}

/// A server whose config doesn't load: keep syncing its checkout (and
/// retrying, for configs copied in by hand), say what's wrong every hour, and
/// hand it over to start once it loads.
async fn wait_for_config(shared: Shared, dir: PathBuf, loaded: tokio::sync::mpsc::UnboundedSender<Loaded>) {
    let mut tick = tokio::time::interval(gitsync::EVERY);
    let mut alarm = alarm::Alarm::default();
    loop {
        tick.tick().await;
        let d = dir.clone();
        let synced = match tokio::task::spawn_blocking(move || gitsync::sync(&d).err()).await {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(?e, "syncing a waiting config crashed");
                continue;
            }
        };
        let e = match shared.load(&dir).await {
            Ok((cfg, guild)) => {
                let _ = loaded.send((dir, cfg, guild));
                return;
            }
            Err(e) => e,
        };
        if !alarm.failed(std::time::Instant::now()) {
            continue;
        }
        tracing::error!(dir = %dir.display(), "the config doesn't load: {e:#}");
        let why = ui::truncate(&format!("{e:#}").replace("```", "'''"), 1000);
        let mut text = format!(
            "⚠️ **The bot isn't running this server: its config doesn't load.**\n```\n{why}\n```\n\
             Push a fix and it starts within 2 minutes; this repeats every hour until then."
        );
        if let Some(s) = synced {
            text.push_str(&format!("\n{}", gitsync::stuck(&s)));
        }
        let channel = chipsoc_config::log_channel(&dir).and_then(|c| Id::<ChannelMarker>::new_checked(c.0));
        if let Some(ch) = channel {
            let body = serde_json::json!({ "content": ui::truncate(&text, ui::MAX_CONTENT) });
            let path = format!("channels/{ch}/messages");
            if let Err(e) =
                chipsoc_discord::Rest::call(&shared.http, chipsoc_discord::Method::Post, &path, Some(body)).await
            {
                tracing::warn!(?e, "posting to the bot log failed");
            }
        }
    }
}

/// Ctrl-C, or SIGTERM (what systemd sends).
async fn stopped() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = term.recv() => {}
                }
                return;
            }
            Err(e) => tracing::warn!(?e, "can't listen for SIGTERM; only Ctrl-C stops the bot"),
        }
    }
    let _ = tokio::signal::ctrl_c().await;
}

/// Tell Discord we're leaving, so the bot shows offline at once. Waits a few seconds at most.
async fn close(shard: &mut Shard) {
    shard.close(CloseFrame::NORMAL);
    let closed = async {
        while let Some(Ok(m)) = shard.next().await {
            if matches!(m, Message::Close(_)) {
                break;
            }
        }
    };
    let _ = tokio::time::timeout(Duration::from_secs(5), closed).await;
}

fn start_jobs(ctx: &Arc<Ctx>, dir: &Path) {
    let c = ctx.clone();
    tokio::spawn(async move { control::close_stale_posts(&c).await });
    let (c, d) = (ctx.clone(), dir.to_path_buf());
    tokio::spawn(async move { clear_rename_notices(&c, &d).await });
    let c = ctx.clone();
    supervise("server events", move || ordered_worker(c.clone()));
    let c = ctx.clone();
    supervise("partner inbox", move || fedrecv::worker(c.clone()));
    let c = ctx.clone();
    supervise("feeds", move || feeds::poll_loop(c.clone()));
    let c = ctx.clone();
    supervise("reminders", move || events::reminder_loop(c.clone()));
    let c = ctx.clone();
    supervise("application windows", move || apps::windows_loop(c.clone()));
    let c = ctx.clone();
    supervise("message retention", move || modlog::retention_loop(c.clone()));
    let c = ctx.clone();
    supervise("partner sync", move || fedsync::full_sync_loop(c.clone()));
    let c = ctx.clone();
    supervise("partner deltas", move || fedsync::flush_loop(c.clone()));
    let c = ctx.clone();
    supervise("partner shares", move || fedshare::send_loop(c.clone()));
    let (c, d) = (ctx.clone(), dir.to_path_buf());
    supervise("config reload", move || reload::watch(c.clone(), d.clone()));
    let c = ctx.clone();
    supervise("drift watch", move || drift::watch(c.clone()));
    let c = ctx.clone();
    supervise("web access sweep", move || webaccess::sweep_loop(c.clone()));
    let c = ctx.clone();
    supervise("join flow", move || join::watch(c.clone()));
    let (c, d) = (ctx.clone(), dir.to_path_buf());
    supervise("config sync", move || gitsync::watch(c.clone(), d.clone()));
    let c = ctx.clone();
    supervise("recording limits", move || record::watch(c.clone()));
    let c = ctx.clone();
    supervise("transcripts", move || record::transcripts(c.clone()));
}

/// Kept (per config) once [`clear_rename_notices`] has run.
const NOTICES_SWEPT: &str = "rename-notices-swept";

/// Discord's "changed the post title" notices the bot's renames left in the
/// forum posts it keeps, from before it deleted them as it renamed. Once per
/// config (tried again at the next start if it fails): renames now delete
/// their own.
async fn clear_rename_notices(ctx: &Ctx, dir: &Path) {
    let scope = chipsoc_admin::trust::scope(dir);
    if !matches!(ctx.store.trusted(&scope, NOTICES_SWEPT).await, Ok(None)) {
        return;
    }
    let lock = ctx.cfg().desired.lock.clone();
    match chipsoc_discord::exec::clear_all_rename_notices(&ctx.rest, &lock).await {
        Ok(n) => {
            if n > 0 {
                tracing::info!(guild = %ctx.guild, "deleted {n} old thread rename notice(s)");
            }
            ctx.log_err(ctx.store.set_trusted(&scope, NOTICES_SWEPT, "1").await, "remembering the rename notices went");
        }
        Err(e) => tracing::warn!("deleting old thread rename notices failed: {e:#}"),
    }
}

/// Message and member events for one server, handled one at a time in order,
/// so an edit never races the create it follows.
async fn ordered_worker(ctx: Arc<Ctx>) {
    loop {
        let e = ctx.events.pop().await;
        ordered(&ctx, e).await;
    }
}

/// Interactions run at once; partner inbox messages go to their own worker;
/// everything else for a server joins its ordered queue.
fn route(guilds: &HashMap<Id<GuildMarker>, Arc<Ctx>>, event: Event) {
    if let Event::InteractionCreate(i) = event {
        // Buttons in DMs carry their server in the custom id.
        let gid = i.guild_id.or_else(|| commands::guild_in_custom_id(&i));
        if let Some(ctx) = gid.and_then(|g| guilds.get(&g)).cloned() {
            spawn_logged("interaction", async move { commands::interaction(&ctx, &i.0).await });
        }
        return;
    }
    if let Event::Ready(_) = event {
        // A new session: member events since the last one are lost.
        for ctx in guilds.values() {
            ctx.rest.members.forget();
        }
        return;
    }
    let Some(ctx) = event.guild_id().and_then(|g| guilds.get(&g)) else { return };
    seen_member(ctx, &event);
    record::on_event(ctx, &event);
    if drift::matters(ctx, &event) {
        ctx.drift.poke();
    }
    if matches!(event, Event::RoleCreate(_) | Event::RoleUpdate(_) | Event::RoleDelete(_) | Event::GuildUpdate(_)) {
        ctx.admins.forget();
    }
    match event {
        Event::MessageCreate(m) if fedrecv::is_inbox(ctx, m.channel_id) => {
            // Saved before it waits for the worker, so a full queue can't lose it.
            let ctx = ctx.clone();
            spawn_logged("saving a partner message", async move {
                fedrecv::arrived(&*ctx, m.id.get(), &m.content).await;
            });
        }
        e if is_ordered(&e) => {
            ctx.events.push(e);
        }
        _ => {}
    }
}

/// A member event, for the member list kept (see [`members`]).
fn seen_member(ctx: &Ctx, event: &Event) {
    use members::Seen;
    let seen = match event {
        Event::MemberAdd(m) => serde_json::to_value(&m.member).map(Seen::Joined),
        Event::MemberUpdate(m) => serde_json::to_value(&**m).map(Seen::Changed),
        Event::MemberRemove(m) => Ok(Seen::Left(m.user.id.get())),
        Event::GuildCreate(_) => return ctx.rest.members.forget(),
        _ => return,
    };
    match seen {
        Ok(s) => ctx.rest.members.seen(s),
        Err(e) => {
            tracing::warn!(?e, "reading a member event failed; the member list is read again");
            ctx.rest.members.forget();
        }
    }
}

/// The events `ordered` handles; others aren't queued.
fn is_ordered(e: &Event) -> bool {
    matches!(
        e,
        Event::MessageCreate(_)
            | Event::MessageUpdate(_)
            | Event::MessageDelete(_)
            | Event::MessageDeleteBulk(_)
            | Event::MemberAdd(_)
            | Event::MemberUpdate(_)
            | Event::MemberRemove(_)
            | Event::ThreadDelete(_)
            | Event::GuildScheduledEventUpdate(_)
            | Event::GuildScheduledEventDelete(_)
    )
}

async fn ordered(ctx: &Arc<Ctx>, event: Event) {
    match event {
        Event::MessageCreate(m) => {
            modlog::on_create(ctx, &m.message).await;
            federation::on_message(ctx, &m.message).await;
            fedshare::on_message(ctx, &m.message).await;
        }
        Event::MessageUpdate(m) => {
            modlog::on_update(ctx, &m.0).await;
            fedshare::on_update(ctx, &m.0).await;
        }
        Event::MessageDelete(d) => {
            modlog::on_delete(ctx, d.channel_id, d.id).await;
            fedshare::on_delete(ctx, d.id.get()).await;
        }
        Event::ThreadDelete(t) => fedshare::on_delete(ctx, t.id.get()).await,
        Event::GuildScheduledEventUpdate(e) => fedshare::on_event(ctx, &e.0).await,
        Event::GuildScheduledEventDelete(e) => fedshare::on_event_gone(ctx, e.0.id.get()).await,
        Event::MessageDeleteBulk(d) => {
            for id in d.ids {
                modlog::on_delete(ctx, d.channel_id, id).await;
                fedshare::on_delete(ctx, id.get()).await;
            }
        }
        Event::MemberAdd(m) => {
            modlog::on_join(ctx, &m.member.user).await;
            roster::on_join(ctx, &m.member.user).await;
            fedsync::on_member(ctx, m.member.user.id, &m.member.roles, m.member.user.bot).await;
            if !m.member.user.bot {
                fedrecv::on_join(ctx, m.member.user.id.get());
            }
        }
        Event::MemberUpdate(m) => {
            fedsync::on_member(ctx, m.user.id, &m.roles, m.user.bot).await;
            profile::on_member_update(ctx, &m).await;
            webaccess::on_roles(ctx, m.user.id, &m.roles, m.user.bot).await;
        }
        Event::MemberRemove(m) => {
            modlog::on_leave(ctx, &m.user).await;
            roster::on_leave(ctx, &m.user).await;
            webaccess::on_leave(ctx, m.user.id).await;
            fedsync::on_member(ctx, m.user.id, &[], m.user.bot).await;
        }
        _ => {}
    }
}

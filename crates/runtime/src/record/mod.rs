//! `/record`: recording meetings in voice and stage channels (audio only;
//! Discord gives bots no video). Everyone in the call is told in its chat,
//! and whoever joins later by DM; there's no opt-out button: people who don't
//! want to be recorded leave the call. It stops on `/record stop`, when the
//! call has been empty for two minutes, or at `max_hours`; a transcript is
//! made afterwards and the committee gets a summary with links. Recordings
//! are kept until an admin deletes one in the web dashboard.

mod live;
#[cfg(test)]
mod tests;

pub(crate) use live::Live;

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use chipsoc_config::recording::{self as conf, Recording};
use chipsoc_discord::{Method, Rest as _};
use chipsoc_voice::capture::{Feed, Input, OggFiles, Recorder, Running};
use chipsoc_voice::store::{self, Meta, Person, Transcript};
use chipsoc_voice::transcribe::{self, Engine};
use twilight_gateway::Event;
use twilight_model::application::command::{Command, CommandOption, CommandOptionType};
use twilight_model::application::interaction::Interaction;
use twilight_model::application::interaction::application_command::{CommandDataOption, CommandOptionValue};
use twilight_model::channel::ChannelType;
use twilight_model::gateway::payload::incoming::GuildCreate;
use twilight_model::guild::PartialMember;
use twilight_model::id::Id;
use twilight_model::id::marker::{ChannelMarker, GuildMarker, UserMarker};
use twilight_model::voice::VoiceState;

use crate::apps::invoker;
use crate::commands::{option, subcommand};
use crate::{Ctx, now, ui};

pub(crate) type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// What the voice server needs to let the bot in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Conn {
    pub guild: u64,
    pub channel: u64,
    pub me: u64,
    pub session: String,
    pub endpoint: String,
    pub token: String,
}

/// Joining calls and hearing them: the live path ([`Live`]), or a pretend one in tests.
pub(crate) trait Voice: Send + Sync {
    /// Ask Discord to put the bot in `channel`, or take it out of any with `None`.
    fn move_to(&self, guild: Id<GuildMarker>, channel: Option<Id<ChannelMarker>>);
    /// Connect to a call's voice server, handing what's heard to `feed`.
    fn connect(&self, conn: Conn, feed: Feed) -> BoxFuture<'static, Result<Box<dyn Call>>>;
}

/// A connected call.
pub(crate) trait Call: Send {
    fn leave(&mut self);
}

/// No gateway to join calls with.
struct NoVoice;

impl Voice for NoVoice {
    fn move_to(&self, _: Id<GuildMarker>, _: Option<Id<ChannelMarker>>) {}
    fn connect(&self, _: Conn, _: Feed) -> BoxFuture<'static, Result<Box<dyn Call>>> {
        Box::pin(async { bail!("the bot isn't connected to Discord's gateway") })
    }
}

/// What a voice channel's status says while it records.
pub(crate) const STATUS: &str = "🔴 Recording";
/// How long an empty call keeps recording.
const EMPTY_FOR: Duration = Duration::from_secs(120);
/// How often a recording's limits are checked.
const CHECK_EVERY: Duration = Duration::from_secs(5);
/// How long Discord has to let the bot into a call.
const JOIN_TIMEOUT: Duration = Duration::from_secs(15);
/// Stop when the disk has less than this free.
const MIN_FREE_WHILE: u64 = 200_000_000;
/// How often waiting transcripts are looked for (besides when one stops).
const TRANSCRIPTS_EVERY: Duration = Duration::from_secs(60);
const GB: f64 = 1e9;

/// Someone in a voice channel.
#[derive(Debug, Clone)]
struct InVoice {
    channel: u64,
    name: Option<String>,
    bot: bool,
}

/// The bot's own voice connection, as the gateway tells it.
#[derive(Debug, Default)]
struct Mine {
    session: Option<String>,
    channel: Option<u64>,
    /// Endpoint and token, from the latest voice server update.
    server: Option<(String, String)>,
}

/// The recording now, for the gateway events that feed it.
struct Now {
    id: String,
    channel: u64,
    channel_name: String,
    feed: Feed,
    /// Who has been told it's recording.
    told: HashSet<u64>,
    /// The text channel `/record start` was run in, if not the call's own
    /// chat: where a late joiner is told if neither a DM nor the call's chat works.
    asked_in: Option<u64>,
}

/// A recording in progress.
struct Session {
    id: String,
    dir: PathBuf,
    channel: Id<ChannelMarker>,
    running: Running,
    call: Box<dyn Call>,
    since: Instant,
    max: Duration,
    max_gb: f64,
    /// `recording.max_total_gb`, when that's the nearer limit for this one.
    total_gb: Option<f64>,
    /// Who started it: they (or an admin) may stop it.
    started_by: u64,
    empty_since: Option<Instant>,
    /// Whether the channel's voice status was set (so it's cleared after).
    status: bool,
}

/// One server's recording state.
pub(crate) struct State {
    link: RwLock<Arc<dyn Voice>>,
    voice: Mutex<HashMap<u64, InVoice>>,
    mine: Mutex<Mine>,
    mine_changed: tokio::sync::Notify,
    session: tokio::sync::Mutex<Option<Session>>,
    now: Mutex<Option<Now>>,
    /// Poked when a recording stops, so its transcript starts at once.
    stopped: tokio::sync::Notify,
    /// Where recordings go instead of [`store::root`] (tests).
    root: RwLock<Option<PathBuf>>,
}

impl Default for State {
    fn default() -> Self {
        Self::new(Arc::new(NoVoice))
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl State {
    pub fn new(link: Arc<dyn Voice>) -> Self {
        Self {
            link: RwLock::new(link),
            voice: Default::default(),
            mine: Default::default(),
            mine_changed: Default::default(),
            session: Default::default(),
            now: Default::default(),
            stopped: Default::default(),
            root: Default::default(),
        }
    }

    /// Where recordings are kept.
    pub fn root(&self) -> PathBuf {
        self.root.read().unwrap_or_else(|e| e.into_inner()).clone().unwrap_or_else(store::root)
    }

    #[cfg(test)]
    pub fn set_root(&self, root: &std::path::Path) {
        *self.root.write().unwrap_or_else(|e| e.into_inner()) = Some(root.to_owned());
    }

    fn link(&self) -> Arc<dyn Voice> {
        self.link.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    #[cfg(test)]
    pub fn set_link(&self, link: Arc<dyn Voice>) {
        *self.link.write().unwrap_or_else(|e| e.into_inner()) = link;
    }

    /// The people (not bots) in `channel`.
    fn humans_in(&self, channel: u64) -> Vec<(u64, Option<String>)> {
        let v = lock(&self.voice);
        let mut out: Vec<_> =
            v.iter().filter(|(_, s)| s.channel == channel && !s.bot).map(|(u, s)| (*u, s.name.clone())).collect();
        out.sort();
        out
    }

    /// The call `user` is in.
    fn channel_of(&self, user: u64) -> Option<u64> {
        lock(&self.voice).get(&user).map(|s| s.channel)
    }

    /// Whether a recording is running.
    pub fn recording(&self) -> Option<u64> {
        lock(&self.now).as_ref().map(|n| n.channel)
    }
}

/// `/record`, when bot.toml has `[recording]`.
pub fn definition() -> Command {
    let channel = CommandOption {
        channel_types: Some(vec![ChannelType::GuildVoice, ChannelType::GuildStageVoice]),
        ..option(CommandOptionType::Channel, "channel", "The call (default: the one you're in)")
    };
    crate::commands::command(
        "record",
        "Record a meeting in a voice channel (everyone in it is told)",
        vec![
            subcommand("start", "Start recording a call; everyone in it is told", vec![channel]),
            subcommand("stop", "Stop recording; the transcript is made and the committee told", vec![]),
        ],
        None,
    )
}

/// Whether `member` may start and stop recordings: admins, and holders of `who`.
fn may_record(ctx: &Ctx, member: Option<&PartialMember>, rec: &Recording) -> bool {
    ctx.is_admin(member)
        || member.is_some_and(|m| rec.who.iter().filter_map(|k| ctx.role(k)).any(|r| m.roles.contains(&r)))
}

/// A member's name as the server shows it.
fn shown(nick: Option<&str>, global: Option<&str>, user: &str) -> String {
    nick.or(global).filter(|n| !n.is_empty()).unwrap_or(user).to_owned()
}

/// `/record start` or `/record stop`.
pub async fn handle(ctx: &Ctx, i: &Interaction, options: &[CommandDataOption]) {
    let Some(rec) = ctx.cfg().bot.recording.clone() else {
        return ctx.respond(i, &ui::ephemeral("Recording isn't set up here (bot.toml, [recording]).")).await;
    };
    if !may_record(ctx, i.member.as_ref(), &rec) {
        let roles: Vec<String> = rec.who.iter().map(|k| ctx.role_name(k)).collect();
        let who = if roles.is_empty() { "admins".to_owned() } else { format!("admins and {}", roles.join(", ")) };
        return ctx.respond(i, &ui::ephemeral(&format!("Only {who} can start or stop recordings."))).await;
    }
    let Some(sub) = options.first() else { return };
    let by = match invoker(i) {
        Some(u) => {
            let m = i.member.as_ref();
            Person { id: u.id.get(), name: shown(m.and_then(|m| m.nick.as_deref()), u.global_name.as_deref(), &u.name) }
        }
        None => return,
    };
    match sub.name.as_str() {
        "start" => {
            let picked = match &sub.value {
                CommandOptionValue::SubCommand(o) => o.iter().find_map(|o| match o.value {
                    CommandOptionValue::Channel(c) => Some(c),
                    _ => None,
                }),
                _ => None,
            };
            start(ctx, i, &rec, picked, by).await;
        }
        "stop" => {
            let starter = ctx.rec.session.lock().await.as_ref().map(|s| s.started_by);
            if let Some(starter) = starter
                && starter != by.id
                && !ctx.is_admin(i.member.as_ref())
            {
                let text = format!("<@{starter}> started this recording, so only they or an admin can stop it.");
                return ctx.respond(i, &ui::ephemeral(&text)).await;
            }
            ctx.respond(i, &ui::defer()).await;
            let text = match stop(ctx, &format!("stopped by {}", by.name)).await {
                Ok(Some(m)) => format!(
                    "⏹️ Stopped recording after {}. The committee has the summary.",
                    chipsoc_voice::duration(m.length_ms)
                ),
                Ok(None) => "Nothing is being recorded.".to_owned(),
                Err(e) => format!("Couldn't stop the recording: {}", chipsoc_admin::fmt::error(&e)),
            };
            ctx.edit_response(i, &text, &[]).await;
        }
        _ => {}
    }
}

/// A voice or stage channel's name, or why it isn't one, or why `member`
/// (who is `user`) may not record it: they must be able to see the call and
/// join it themselves, as Discord says now.
async fn voice_channel(
    ctx: &Ctx,
    channel: Id<ChannelMarker>,
    user: u64,
    member: Option<&PartialMember>,
) -> Result<String, String> {
    let c = ctx.rest.call(Method::Get, &format!("channels/{channel}"), None).await.map_err(|e| {
        tracing::warn!(?e, "reading a channel failed");
        "Couldn't read that channel.".to_owned()
    })?;
    let kind = c["type"].as_u64().unwrap_or(0);
    if kind != u64::from(u8::from(ChannelType::GuildVoice)) && kind != u64::from(u8::from(ChannelType::GuildStageVoice))
    {
        return Err("That isn't a voice or stage channel.".to_owned());
    }
    let guild = ctx.rest.call(Method::Get, &format!("guilds/{}", ctx.guild), None).await.map_err(|e| {
        tracing::warn!(?e, "reading the server failed");
        "Couldn't check who may join that call; try again.".to_owned()
    })?;
    let roles: Vec<u64> = member.map(|m| m.roles.iter().map(|r| r.get()).collect()).unwrap_or_default();
    let can = permissions_in(&guild, &c, user, &roles);
    if can & (VIEW_CHANNEL | CONNECT) != VIEW_CHANNEL | CONNECT {
        return Err(format!("You can't join <#{channel}> yourself, so you can't record it."));
    }
    Ok(c["name"].as_str().unwrap_or("call").to_owned())
}

const ADMINISTRATOR: u64 = 1 << 3;
const VIEW_CHANNEL: u64 = 1 << 10;
const CONNECT: u64 = 1 << 20;

/// What `user`, holding `roles`, may do in `channel`, by Discord's rules:
/// the server's roles (`@everyone`'s too), then the channel's overwrites for
/// `@everyone`, their roles and them. The owner and administrators may do anything.
fn permissions_in(guild: &serde_json::Value, channel: &serde_json::Value, user: u64, roles: &[u64]) -> u64 {
    use chipsoc_discord::wire::id;
    let everyone = id(&guild["id"]).map(|g| g.0);
    if id(&guild["owner_id"]).map(|o| o.0) == Some(user) {
        return u64::MAX;
    }
    let bits = |v: &serde_json::Value| v.as_str().and_then(|p| p.parse::<u64>().ok()).unwrap_or(0);
    let held = |r: u64| Some(r) == everyone || roles.contains(&r);
    let mut p = guild["roles"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|r| id(&r["id"]).is_some_and(|i| held(i.0)))
        .fold(0, |p, r| p | bits(&r["permissions"]));
    if p & ADMINISTRATOR != 0 {
        return u64::MAX;
    }
    let ows: Vec<&serde_json::Value> = channel["permission_overwrites"].as_array().into_iter().flatten().collect();
    let target = |o: &serde_json::Value| id(&o["id"]).map(|i| i.0);
    let member = |o: &serde_json::Value| o["type"].as_u64() == Some(1) || o["type"].as_str() == Some("1");
    if let Some(o) = ows.iter().find(|o| !member(o) && target(o) == everyone) {
        p = (p & !bits(&o["deny"])) | bits(&o["allow"]);
    }
    let (mut allow, mut deny) = (0, 0);
    for o in ows.iter().filter(|o| !member(o) && target(o) != everyone && target(o).is_some_and(|r| roles.contains(&r)))
    {
        allow |= bits(&o["allow"]);
        deny |= bits(&o["deny"]);
    }
    p = (p & !deny) | allow;
    if let Some(o) = ows.iter().find(|o| member(o) && target(o) == Some(user)) {
        p = (p & !bits(&o["deny"])) | bits(&o["allow"]);
    }
    p
}

async fn start(ctx: &Ctx, i: &Interaction, rec: &Recording, picked: Option<Id<ChannelMarker>>, by: Person) {
    let here = i.channel.as_ref().filter(|c| matches!(c.kind, ChannelType::GuildVoice | ChannelType::GuildStageVoice));
    let channel = picked.or_else(|| ctx.rec.channel_of(by.id).and_then(Id::new_checked)).or(here.map(|c| c.id));
    let Some(channel) = channel else {
        let text = "Join the call first, or say which: `/record start channel:…`.";
        return ctx.respond(i, &ui::ephemeral(text)).await;
    };
    let name = match voice_channel(ctx, channel, by.id, i.member.as_ref()).await {
        Ok(n) => n,
        Err(why) => return ctx.respond(i, &ui::ephemeral(&why)).await,
    };
    let mut slot = ctx.rec.session.lock().await;
    if let Some(s) = slot.as_ref() {
        let text = format!("Already recording <#{}>. `/record stop` first.", s.channel);
        return ctx.respond(i, &ui::ephemeral(&text)).await;
    }
    let root = ctx.rec.root();
    let need = (rec.min_free_gb * GB) as u64;
    if let Some(free) = store::free_bytes(&root)
        && free < need
    {
        let text = format!(
            "Not enough disk space to record: {:.1} GB free, and recording.min_free_gb asks for {:.1} GB. Delete old recordings in the web dashboard, or free some space.",
            free as f64 / GB,
            rec.min_free_gb
        );
        return ctx.respond(i, &ui::ephemeral(&text)).await;
    }
    // All of this server's recordings together stay under `max_total_gb`.
    let cap = (rec.max_total_gb * GB) as u64;
    let used = {
        let (r, g) = (root.clone(), ctx.guild.get());
        tokio::task::spawn_blocking(move || store::total(&r, g)).await.unwrap_or(u64::MAX)
    };
    if used >= cap {
        let text = format!(
            "Recordings here already take {:.1} GB, and recording.max_total_gb allows {:.1} GB, so this one won't start. Delete old recordings in the web dashboard first.",
            used as f64 / GB,
            rec.max_total_gb
        );
        ctx.log_bot(&format!("🎙️ <@{}> couldn't start a recording: {text}", by.id)).await;
        return ctx.respond(i, &ui::ephemeral(&text)).await;
    }
    let room = cap - used;
    ctx.respond(i, &ui::defer()).await;
    let asked_in = i.channel.as_ref().map(|c| c.id.get()).filter(|c| *c != channel.get());
    match begin(ctx, rec, root, channel, &name, by, Started { room, asked_in }).await {
        Ok(s) => {
            *slot = Some(s);
            let text = format!(
                "🔴 Recording <#{channel}>. Everyone in it has been told, and anyone who joins will be. `/record stop` to stop."
            );
            ctx.edit_response(i, &text, &[]).await;
        }
        Err(e) => {
            tracing::warn!("starting a recording failed: {e:#}");
            ctx.edit_response(i, &format!("Couldn't start recording: {}", chipsoc_admin::fmt::error(&e)), &[]).await;
        }
    }
}

/// The bot's connection details for `channel`, once the gateway has given them all.
fn ready(ctx: &Ctx, channel: u64) -> Option<Conn> {
    let m = lock(&ctx.rec.mine);
    let (endpoint, token) = m.server.clone()?;
    (m.channel == Some(channel)).then_some(())?;
    Some(Conn { guild: ctx.guild.get(), channel, me: ctx.me.get(), session: m.session.clone()?, endpoint, token })
}

/// Wait for [`ready`].
async fn wait_ready(ctx: &Ctx, channel: u64) -> Option<Conn> {
    let wait = async {
        loop {
            let changed = ctx.rec.mine_changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if let Some(c) = ready(ctx, channel) {
                return c;
            }
            changed.await;
        }
    };
    tokio::time::timeout(JOIN_TIMEOUT, wait).await.ok()
}

/// More about a recording being started.
struct Started {
    /// Bytes left under `recording.max_total_gb`.
    room: u64,
    /// See [`Now::asked_in`].
    asked_in: Option<u64>,
}

/// Join `channel`, tell everyone, then start hearing it: nothing is heard
/// before the notice is up.
async fn begin(
    ctx: &Ctx,
    rec: &Recording,
    root: PathBuf,
    channel: Id<ChannelMarker>,
    name: &str,
    by: Person,
    started: Started,
) -> Result<Session> {
    let link = ctx.rec.link();
    let stale = {
        let mut m = lock(&ctx.rec.mine);
        m.server = None;
        m.channel.is_some()
    };
    if stale {
        // Left over from before: out first, so Discord sends fresh details.
        link.move_to(ctx.guild, None);
        tokio::time::sleep(Duration::from_secs(1)).await;
        lock(&ctx.rec.mine).server = None;
    }
    link.move_to(ctx.guild, Some(channel));
    let Some(conn) = wait_ready(ctx, channel.get()).await else {
        link.move_to(ctx.guild, None);
        bail!(
            "Discord didn't let the bot into the call within {} seconds; may it connect there?",
            JOIN_TIMEOUT.as_secs()
        );
    };
    let started_at = now();
    let guild = ctx.guild.get();
    let channel_name = format!("#{name}");
    let (id, dir) = {
        let (r, n) = (root.clone(), name.to_owned());
        tokio::task::spawn_blocking(move || store::create(&r, guild, started_at, &n)).await??
    };
    let meta = Meta::new(id.clone(), guild, channel.get(), &channel_name, started_at, by.clone());
    let own = (rec.max_gb * GB) as u64;
    let total_gb = (started.room < own).then_some(rec.max_total_gb);
    let recorder = Recorder::new(&dir, meta, Box::new(OggFiles::new(&dir)), own.min(started.room))
        .with_context(|| format!("writing in {}", dir.display()))?;
    let running = Running::start(recorder);
    let feed = running.feed();
    // Nothing was recorded if it goes no further: no folder is left behind.
    let give_up = |running: Running, dir: PathBuf| async move {
        lock(&ctx.rec.now).take();
        ctx.rec.link().move_to(ctx.guild, None);
        let _ = tokio::task::spawn_blocking(move || {
            let _ = running.finish(now(), "it didn't start");
            std::fs::remove_dir_all(dir)
        })
        .await;
    };
    // Nobody is recorded without being told: the status and the notice go
    // up before the bot hears anything.
    let there: HashSet<u64> = ctx.rec.humans_in(channel.get()).into_iter().map(|(u, _)| u).collect();
    let status = set_status(ctx, channel, Some(STATUS)).await;
    let mut notice = rec.notice_for(&ctx.name());
    if !status {
        notice = format!("**{STATUS} now.** {notice}");
    }
    if let Err(e) = ctx.post(channel, &notice, &[]).await {
        tracing::warn!(?e, "posting the recording notice failed");
        if status {
            set_status(ctx, channel, None).await;
        }
        give_up(running, dir).await;
        bail!(
            "the notice couldn't be posted in the call's chat, so it isn't recording; may the bot send messages there?"
        );
    }
    let n = Now {
        id: id.clone(),
        channel: channel.get(),
        channel_name: channel_name.clone(),
        feed: feed.clone(),
        told: there.clone(),
        asked_in: started.asked_in,
    };
    *lock(&ctx.rec.now) = Some(n);
    // Whoever came in while the notice went up is told as a late joiner;
    // from here on, joiners are told as they join (see `voice_state`).
    let mut late = Vec::new();
    for (user, known) in ctx.rec.humans_in(channel.get()) {
        let name = match known {
            Some(n) => n,
            None => member_name(ctx, user).await,
        };
        feed.send(Input::Joined { user, name });
        if lock(&ctx.rec.now).as_mut().is_some_and(|n| n.told.insert(user)) {
            late.push(user);
        }
    }
    for user in late {
        if !tell_joiner(ctx, user, channel.get(), &channel_name, started.asked_in).await {
            if status {
                set_status(ctx, channel, None).await;
            }
            give_up(running, dir).await;
            ctx.say(channel, "⏹️ Recording didn't start after all; nothing was recorded.").await;
            bail!("<@{user}> came into the call and couldn't be told it's recording, so it isn't");
        }
    }
    let call = match link.connect(conn, feed.clone()).await {
        Ok(c) => c,
        Err(e) => {
            if status {
                set_status(ctx, channel, None).await;
            }
            give_up(running, dir).await;
            ctx.say(channel, "⏹️ Recording didn't start after all; nothing was recorded.").await;
            return Err(e);
        }
    };
    ctx.audit(
        Some(&by.id.to_string()),
        chipsoc_admin::AuditAction::RecordingStarted,
        Some(&channel.to_string()),
        Some(&id),
    )
    .await;
    Ok(Session {
        id,
        dir,
        channel,
        running,
        call,
        since: Instant::now(),
        max: Duration::from_secs_f64(rec.max_hours * 3600.0),
        max_gb: rec.max_gb,
        total_gb,
        started_by: by.id,
        empty_since: None,
        status,
    })
}

/// Someone's name in the server, asked of Discord (their id if that fails).
async fn member_name(ctx: &Ctx, user: u64) -> String {
    let name = match ctx.rest.call(Method::Get, &format!("guilds/{}/members/{user}", ctx.guild), None).await {
        Ok(m) => chipsoc_discord::wire::shown_name(&m).unwrap_or_default(),
        Err(_) => String::new(),
    };
    if name.trim().is_empty() { format!("User {user}") } else { name }
}

/// Set (or with `None` clear) a voice channel's status; false if Discord won't.
async fn set_status(ctx: &Ctx, channel: Id<ChannelMarker>, text: Option<&str>) -> bool {
    let body = serde_json::json!({ "status": text.unwrap_or("") });
    match ctx.rest.call(Method::Put, &format!("channels/{channel}/voice-status"), Some(body)).await {
        Ok(_) => true,
        Err(e) => {
            tracing::info!(?e, "setting the voice channel's status failed");
            false
        }
    }
}

/// Stop the recording, if there is one: leave the call, close the files,
/// say so in the call and tell the committee. `why` ends "It stopped: …".
pub(crate) async fn stop(ctx: &Ctx, why: &str) -> Result<Option<Meta>> {
    let Some(mut s) = ctx.rec.session.lock().await.take() else { return Ok(None) };
    lock(&ctx.rec.now).take();
    s.call.leave();
    ctx.rec.link().move_to(ctx.guild, None);
    if s.status {
        set_status(ctx, s.channel, None).await;
    }
    let (running, why_owned) = (s.running, why.to_owned());
    let meta = tokio::task::spawn_blocking(move || running.finish(now(), &why_owned)).await??;
    ctx.say(s.channel, "⏹️ Recording stopped.").await;
    let detail = format!("{}: {why}", s.id);
    ctx.audit(None, chipsoc_admin::AuditAction::RecordingStopped, Some(&s.channel.to_string()), Some(&detail)).await;
    tracing::info!(id = s.id, dir = %s.dir.display(), "recording stopped: {why}");
    summary(ctx, &meta).await;
    ctx.rec.stopped.notify_one();
    Ok(Some(meta))
}

/// Where the committee can listen to a recording.
fn link_to(ctx: &Ctx, meta: &Meta) -> String {
    match ctx.cfg().bot.web.as_ref() {
        Some(w) => format!("{}/recordings/{}", w.url.trim_end_matches('/'), meta.id),
        None => format!("the web dashboard's Recordings page (`{}`)", meta.id),
    }
}

/// Whether a transcript can be made, and how, for a config in `dir`.
/// whisper.cpp's program and model must be installed on the server, out
/// of the config's reach (see [`transcribe::installed`]).
fn engine(rec: &Recording, dir: &std::path::Path) -> Result<Engine, String> {
    let api = Engine::api_from_env(rec.language.clone());
    match rec.engine(api.is_some()) {
        conf::Engine::None if rec.transcribe == Some(conf::Engine::None) => {
            Err("transcripts are turned off (recording.transcribe = \"none\")".into())
        }
        conf::Engine::None => {
            Err("no transcription engine is set up (bot.toml recording.transcribe, or TRANSCRIBE_URL)".into())
        }
        conf::Engine::Api => api.ok_or_else(|| "recording.transcribe is \"api\" but TRANSCRIBE_URL isn't set".into()),
        conf::Engine::WhisperCpp => {
            let model = rec.model.clone().unwrap_or_default();
            let program = match &rec.whisper {
                Some(p) => p.clone(),
                None => conf::WHISPER_DIRS
                    .iter()
                    .map(|d| format!("{d}/whisper-cli"))
                    .find(|p| std::fs::symlink_metadata(p).is_ok())
                    .ok_or_else(|| {
                        format!(
                            "whisper.cpp's whisper-cli isn't in {} (recording.whisper)",
                            conf::WHISPER_DIRS.join(", ")
                        )
                    })?,
            };
            for (what, path) in [("model", &model), ("whisper", &program)] {
                conf::outside(dir, path).map_err(|why| format!("recording.{what}: {why}"))?;
            }
            Engine::whisper_cpp(program.as_ref(), model.as_ref(), rec.language.clone())
                .map_err(|why| format!("{why} (recording.model, recording.whisper)"))
        }
    }
}

/// The committee's summary of a finished recording.
pub(crate) fn summary_text(meta: &Meta, link: &str, transcript: Result<(), &str>) -> String {
    // Names are whatever people chose: escaped.
    let people: Vec<String> = meta.names().into_iter().map(chipsoc_voice::md_name).collect();
    let people = if people.is_empty() { "nobody".to_owned() } else { people.join(", ") };
    let why = chipsoc_voice::md_name(meta.ended_because.as_deref().unwrap_or("stopped"));
    let mut t = format!(
        "🎙️ **Recording of {} ended** ({why}) after {}.\nPeople: {people}.\nListen or download: {link}\n",
        channel_shown(&meta.channel_name),
        chipsoc_voice::duration(meta.length_ms),
    );
    match transcript {
        Ok(()) => t.push_str("📝 A transcript is being made; it'll be posted here."),
        Err(why) => t.push_str(&format!("📝 No transcript: {why}. The audio is kept.")),
    }
    t
}

/// A call's name (`#meeting`), escaped for a message.
fn channel_shown(name: &str) -> String {
    match name.strip_prefix('#') {
        Some(n) => format!("#{}", chipsoc_voice::md_name(n)),
        None => chipsoc_voice::md_name(name),
    }
}

async fn summary(ctx: &Ctx, meta: &Meta) {
    let Some(rec) = ctx.cfg().bot.recording.clone() else { return };
    let e = engine(&rec, &ctx.cfg().dir);
    let text = summary_text(meta, &link_to(ctx, meta), e.as_ref().map(drop).map_err(String::as_str));
    ctx.say_in(Some(&rec.channel), &text).await;
}

/// Whether a recording can go on writing its files.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Files {
    Fine,
    /// At its size limit.
    Full,
    /// Writing them failed.
    Broken,
}

impl Files {
    fn of(r: &Running) -> Self {
        if r.broken() {
            Self::Broken
        } else if r.full() {
            Self::Full
        } else {
            Self::Fine
        }
    }
}

/// Why a recording should stop now, if it should.
fn reason(
    humans: usize,
    empty_for: Option<Duration>,
    ran: Duration,
    max: Duration,
    files: Files,
    free: Option<u64>,
    (max_gb, total_gb): (f64, Option<f64>),
) -> Option<String> {
    if humans == 0 && empty_for.is_some_and(|d| d >= EMPTY_FOR) {
        return Some(format!("the call was empty for {} minutes", EMPTY_FOR.as_secs() / 60));
    }
    if ran >= max {
        let h = max.as_secs_f64() / 3600.0;
        return Some(format!("it reached the {h}-hour limit (recording.max_hours)"));
    }
    if files == Files::Broken {
        return Some("writing the files failed (see the bot's log)".to_owned());
    }
    if files == Files::Full {
        return Some(match total_gb {
            Some(t) => format!("all the recordings here reached their size limit, {t} GB (recording.max_total_gb)"),
            None => format!("it reached its size limit, {max_gb} GB (recording.max_gb)"),
        });
    }
    if free.is_some_and(|f| f < MIN_FREE_WHILE) {
        return Some("the disk is nearly full".to_owned());
    }
    None
}

/// Stop recordings that hit a limit (see [`reason`]).
pub(crate) async fn watch(ctx: Arc<Ctx>) {
    let mut tick = tokio::time::interval(CHECK_EVERY);
    loop {
        tick.tick().await;
        let why = {
            let mut g = ctx.rec.session.lock().await;
            let Some(s) = g.as_mut() else { continue };
            let humans = ctx.rec.humans_in(s.channel.get()).len();
            if humans == 0 {
                s.empty_since.get_or_insert_with(Instant::now);
            } else {
                s.empty_since = None;
            }
            let free = store::free_bytes(&s.dir);
            reason(
                humans,
                s.empty_since.map(|t| t.elapsed()),
                s.since.elapsed(),
                s.max,
                Files::of(&s.running),
                free,
                (s.max_gb, s.total_gb),
            )
        };
        if let Some(why) = why {
            ctx.log_err(stop(&ctx, &why).await, "stopping a recording");
        }
    }
}

/// Gateway events about calls: who's in which, and the bot's own connection.
pub(crate) fn on_event(ctx: &Arc<Ctx>, event: &Event) {
    match event {
        Event::GuildCreate(g) => {
            if let GuildCreate::Available(g) = &**g {
                let names: HashMap<u64, String> = g
                    .members
                    .iter()
                    .map(|m| (m.user.id.get(), shown(m.nick.as_deref(), m.user.global_name.as_deref(), &m.user.name)))
                    .collect();
                let mut v = lock(&ctx.rec.voice);
                v.clear();
                for s in &g.voice_states {
                    if s.user_id == ctx.me {
                        let mut m = lock(&ctx.rec.mine);
                        m.channel = s.channel_id.map(Id::get);
                        m.session = Some(s.session_id.clone());
                        continue;
                    }
                    if let Some(c) = s.channel_id {
                        let bot = s.member.as_ref().is_some_and(|m| m.user.bot);
                        let name = names.get(&s.user_id.get()).cloned();
                        v.insert(s.user_id.get(), InVoice { channel: c.get(), name, bot });
                    }
                }
            }
        }
        Event::VoiceStateUpdate(vs) => voice_state(ctx, &vs.0),
        Event::VoiceServerUpdate(s) => {
            if let Some(endpoint) = &s.endpoint {
                voice_server(ctx, endpoint, &s.token);
            }
        }
        _ => {}
    }
}

/// The voice server for the bot's call (again, if Discord moved it).
fn voice_server(ctx: &Arc<Ctx>, endpoint: &str, token: &str) {
    let moved = {
        let mut m = lock(&ctx.rec.mine);
        let moved = m.server.is_some();
        m.server = Some((endpoint.to_owned(), token.to_owned()));
        moved
    };
    ctx.rec.mine_changed.notify_waiters();
    if moved && ctx.rec.recording().is_some() {
        let ctx = ctx.clone();
        crate::spawn_logged("reconnecting a recording", async move { reconnect(&ctx).await });
    }
}

/// Connect a running recording to the call's new voice server.
async fn reconnect(ctx: &Ctx) {
    let mut g = ctx.rec.session.lock().await;
    let Some(s) = g.as_mut() else { return };
    let Some(conn) = ready(ctx, s.channel.get()) else { return };
    match ctx.rec.link().connect(conn, s.running.feed()).await {
        Ok(call) => {
            let mut old = std::mem::replace(&mut s.call, call);
            old.leave();
            tracing::info!("the recording moved to the call's new voice server");
        }
        Err(e) => tracing::warn!("reconnecting the recording failed: {e:#}"),
    }
}

/// Someone joined, left or moved call; or the bot did.
fn voice_state(ctx: &Arc<Ctx>, s: &VoiceState) {
    let user = s.user_id.get();
    if s.user_id == ctx.me {
        {
            let mut m = lock(&ctx.rec.mine);
            m.session = Some(s.session_id.clone());
            m.channel = s.channel_id.map(Id::get);
        }
        ctx.rec.mine_changed.notify_waiters();
        if let Some(rec) = ctx.rec.recording()
            && s.channel_id.map(Id::get) != Some(rec)
        {
            let ctx = ctx.clone();
            crate::spawn_logged("stopping a recording", async move {
                ctx.log_err(stop(&ctx, "the bot was disconnected from the call").await, "stopping a recording");
            });
        }
        return;
    }
    let member = s.member.as_ref();
    let bot = member.is_some_and(|m| m.user.bot);
    let name = member.map(|m| shown(m.nick.as_deref(), m.user.global_name.as_deref(), &m.user.name));
    let before = {
        let mut v = lock(&ctx.rec.voice);
        let before = v.get(&user).map(|x| x.channel);
        match s.channel_id {
            Some(c) => {
                v.insert(user, InVoice { channel: c.get(), name: name.clone(), bot });
            }
            None => {
                v.remove(&user);
            }
        }
        before
    };
    let after = s.channel_id.map(Id::get);
    if bot || before == after {
        return;
    }
    let mut now = lock(&ctx.rec.now);
    let Some(n) = now.as_mut() else { return };
    if after == Some(n.channel) {
        let name = name.unwrap_or_else(|| format!("User {user}"));
        n.feed.send(Input::Joined { user, name });
        if n.told.insert(user) {
            let (ctx, channel, channel_name, asked_in) = (ctx.clone(), n.channel, n.channel_name.clone(), n.asked_in);
            crate::spawn_logged("telling someone the call is recorded", async move {
                if !tell_joiner(&ctx, user, channel, &channel_name, asked_in).await {
                    let why = "someone who joined couldn't be told it's recording";
                    ctx.log_err(stop(&ctx, why).await, "stopping a recording");
                }
            });
        }
    } else if before == Some(n.channel) {
        n.feed.send(Input::Left { user });
    }
}

/// Tell someone who joined a call being recorded: by DM; if their DMs are
/// closed, to them in the call's chat; failing that, in the channel the
/// recording was started from. False (and said in the bot log) if none
/// worked: the caller stops recording, as nobody is recorded without being told.
async fn tell_joiner(ctx: &Ctx, user: u64, channel: u64, channel_name: &str, asked_in: Option<u64>) -> bool {
    let Some(rec) = ctx.cfg().bot.recording.clone() else { return true };
    let text = rec.join_notice_for(&ctx.name(), channel_name);
    let Some(uid) = Id::<UserMarker>::new_checked(user) else { return true };
    if ctx.dm(uid, &text).await.is_ok() {
        return true;
    }
    for at in std::iter::once(channel).chain(asked_in) {
        let body = serde_json::json!({
            "content": ui::truncate(&format!("<@{user}> {text}"), ui::MAX_CONTENT),
            "allowed_mentions": { "parse": [], "users": [user.to_string()] },
        });
        match ctx.rest.call(Method::Post, &format!("channels/{at}/messages"), Some(body)).await {
            Ok(_) => return true,
            Err(e) => tracing::warn!("telling someone in <#{at}> it's recording failed: {e:#}"),
        }
    }
    ctx.log_bot(&format!(
        "{} **The recording of {} stops:** <@{user}> joined the call and couldn't be told it's being recorded \
         (no DM, and the bot couldn't post in the call's chat{}).",
        chipsoc_admin::fmt::ATTENTION,
        chipsoc_admin::fmt::escape(channel_name),
        asked_in.map(|c| format!(" or <#{c}>")).unwrap_or_default()
    ))
    .await;
    false
}

/// Finish recordings the bot was making when it stopped, and pick up
/// transcripts it was making; then make each waiting transcript in turn.
pub(crate) async fn transcripts(ctx: Arc<Ctx>) {
    recover(&ctx).await;
    loop {
        if let Some(rec) = ctx.cfg().bot.recording.clone() {
            let waiting = {
                let root = ctx.rec.root();
                let g = ctx.guild.get();
                tokio::task::spawn_blocking(move || {
                    let mut v: Vec<Meta> = store::list(&root, g)
                        .into_iter()
                        .filter(|m| m.ended_at.is_some() && m.transcript == Transcript::Waiting)
                        .collect();
                    v.reverse();
                    v
                })
                .await
                .unwrap_or_default()
            };
            for meta in waiting {
                make_transcript(&ctx, &rec, &meta).await;
            }
        }
        tokio::select! {
            _ = ctx.rec.stopped.notified() => {}
            _ = tokio::time::sleep(TRANSCRIPTS_EVERY) => {}
        }
    }
}

/// Recordings left unfinished by a stop or crash: closed as they are (what
/// was saved is kept) and their transcript queued; the committee is told.
async fn recover(ctx: &Ctx) {
    let root = ctx.rec.root();
    let guild = ctx.guild.get();
    let running = lock(&ctx.rec.now).as_ref().map(|n| n.id.clone());
    let left = tokio::task::spawn_blocking(move || {
        let mut out = Vec::new();
        let all = store::list(&root, guild).into_iter().filter(|m| Some(&m.id) != running.as_ref());
        for (dir, mut m) in all.filter_map(|m| store::find(&root, guild, &m.id)) {
            let unfinished = m.ended_at.is_none();
            if !unfinished && m.transcript != Transcript::Making {
                continue;
            }
            if unfinished {
                let saved = std::fs::metadata(dir.join(store::META)).and_then(|f| f.modified()).ok();
                let at = saved.and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_secs() as i64);
                m.ended_at = Some(at.unwrap_or(m.started_at + (m.length_ms / 1000) as i64));
                m.ended_because = Some("the bot stopped while recording".into());
                for p in &mut m.participants {
                    for s in &mut p.spans {
                        s.to_ms.get_or_insert(m.length_ms);
                    }
                }
                m.bytes = store::size(&dir);
            }
            m.transcript = Transcript::Waiting;
            if store::save(&dir, &m).is_ok() && unfinished {
                out.push(m);
            }
        }
        out
    })
    .await
    .unwrap_or_default();
    for m in left {
        if let Some(c) = Id::<ChannelMarker>::new_checked(m.channel) {
            set_status(ctx, c, None).await;
        }
        summary(ctx, &m).await;
    }
}

async fn make_transcript(ctx: &Ctx, rec: &Recording, meta: &Meta) {
    let root = ctx.rec.root();
    let Some((dir, _)) = store::find(&root, ctx.guild.get(), &meta.id) else { return };
    let engine = match engine(rec, &ctx.cfg().dir) {
        Ok(e) => e,
        Err(why) => {
            ctx.log_err(transcribe::set_state(&dir, Transcript::Skipped { why }), "saving a recording's state");
            return;
        }
    };
    if let Err(e) = transcribe::set_state(&dir, Transcript::Making) {
        return ctx.log_err(Err::<(), _>(e), "saving a recording's state");
    }
    let link = link_to(ctx, meta);
    let (state, text) = match transcribe::transcribe(&dir, meta, &engine).await {
        Ok(_) => (
            Transcript::Done,
            format!(
                "📝 **Transcript ready** for the recording of {} ({}): {link}",
                channel_shown(&meta.channel_name),
                when(meta.started_at)
            ),
        ),
        Err(e) => {
            let why = chipsoc_admin::fmt::error(&e);
            tracing::warn!(id = meta.id, "transcribing failed: {e:#}");
            (
                Transcript::Failed { why: why.clone() },
                format!(
                    "⚠️ The transcript of the recording of {} ({}) couldn't be made: {why}. The audio is kept: {link}",
                    channel_shown(&meta.channel_name),
                    when(meta.started_at)
                ),
            )
        }
    };
    ctx.log_err(transcribe::set_state(&dir, state), "saving a recording's state");
    ctx.say_in(Some(&rec.channel), &text).await;
}

/// `ts` as "2026-11-06 18:00 UTC".
fn when(ts: i64) -> String {
    format!("{} UTC", chipsoc_model::utc_minute(ts))
}

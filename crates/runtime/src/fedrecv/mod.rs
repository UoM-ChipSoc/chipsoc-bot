//! Receiving partner sync: catalogues and role attestations posted into our
//! inbox, handled on a per-server worker so a big full sync never delays
//! the people log.
//!
//! Every inbox message is saved as it arrives, before it waits for the
//! worker, and marked done once handled, so none is lost to a restart, an
//! error or a flood: on start the worker reads the inbox history it missed
//! (ignoring messages edited since they were posted) and handles whatever
//! isn't done, oldest first. A message counts as received when Discord says
//! it was posted, so one handled again later is judged the same way.
//!
//! Roles change only through the agreed `role_map`, never for a role too
//! powerful to take a partner's word on, and never touching a role the member
//! already had. Each member's state applies only if it's newer than what we
//! applied for them, and a full sync counts as of when the partner took it, so
//! a late full sync never undoes a later change; a seq from the future is
//! refused. A partner giving roles to, or taking them from, unusually many
//! members within a day is held for an admin, however many messages it's
//! spread over; each question's "Allow" applies just what it describes.
//! Updates that fail part-way (Discord or the database erroring) are retried
//! every few minutes.
//!
//! A partner removed from `federation.toml`, a key removed from its list, or
//! a role removed from its `role_map` (or now too powerful) has the roles its
//! word gave taken back, checked again every few minutes until done.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use chipsoc_config::{Config, Peer, RoleMap, Snapshot};
use chipsoc_discord::{ApiError, Method, Rest};
use chipsoc_fedproto::{self as proto, Body, Member};
use chipsoc_store::{InboxState, Store};
use serde::Deserialize;
use tokio::sync::Notify;
use twilight_model::application::interaction::Interaction;
use twilight_model::channel::message::component::ButtonStyle;
use twilight_model::channel::message::{AllowedMentions, MessageFlags};
use twilight_model::id::Id;
use twilight_model::id::marker::{ChannelMarker, UserMarker};

use crate::apps::invoker;
use crate::commands::CustomId;
use crate::fedsync::{self, peer_keys};
use crate::{Ctx, now, ui};
use chipsoc_admin::{AuditAction, fmt};

mod catch_up;
mod checks;
mod grants;
mod held;
mod partners;
mod receive;
mod shared;
#[cfg(test)]
mod tests;

use catch_up::*;
use checks::*;
pub use checks::{Revoked, revoke_partner};
use grants::*;
use held::*;
pub use held::{allow, refuse_button};
pub use partners::partners;
use partners::*;
use receive::*;
use shared::*;

const RETRY_EVERY: Duration = Duration::from_secs(5 * 60);
/// Retries of one member's update, or one inbox message, before giving up (about an hour).
const MAX_TRIES: u32 = 12;
/// The mass-change guards count a partner's grants and revocations over this long (seconds).
const GRANT_WINDOW: i64 = 24 * 3600;
/// Notes about one partner posted to the bot log per hour.
const MAX_LOGS_PER_HOUR: u32 = 20;
/// Largest catalogue kept from a partner (bytes of JSON).
const MAX_CATALOG: usize = 256 * 1024;

/// What receiving needs from a server: Discord, the store and the bot log.
/// [`Ctx`] in the bot; a fake in tests.
pub(crate) trait Env: chipsoc_admin::Host {
    fn store(&self) -> &Store;
    fn gkey(&self) -> &str;
    fn state(&self) -> &fedsync::State;
    fn log_bot(&self, text: &str) -> impl Future<Output = ()> + Send;
    fn audit(&self, action: AuditAction, target: &str, detail: &str) -> impl Future<Output = ()> + Send;
    /// Post `text` to the bot log with an "Allow" button carrying `button` (and "Refuse" beside it).
    fn ask(&self, text: &str, button: &str) -> impl Future<Output = ()> + Send;
    /// Post a sealed envelope into `peer`'s inbox. Whether it went.
    fn deliver(&self, peer: &Peer, content: &str) -> impl Future<Output = bool> + Send;
}

impl Env for Ctx {
    fn store(&self) -> &Store {
        &self.store
    }

    fn gkey(&self) -> &str {
        &self.gkey
    }

    fn state(&self) -> &fedsync::State {
        &self.fed
    }

    fn log_bot(&self, text: &str) -> impl Future<Output = ()> + Send {
        Ctx::log_bot(self, text)
    }

    fn audit(&self, action: AuditAction, target: &str, detail: &str) -> impl Future<Output = ()> + Send {
        Ctx::audit(self, None, action, Some(target), Some(detail))
    }

    async fn ask(&self, text: &str, button: &str) {
        let Some(channel) = self.bot_log() else { return tracing::warn!("{text}") };
        let mut buttons = vec![ui::button(button, "Allow", ButtonStyle::Primary)];
        if let Some(CustomId::AllowSync { peer, batch }) = CustomId::parse(button) {
            let refuse = CustomId::RefuseSync { peer, batch }.to_string();
            buttons.push(ui::button(&refuse, "Refuse", ButtonStyle::Danger));
        }
        self.say_with(channel, text, &[ui::row(buttons)]).await;
    }

    fn deliver(&self, peer: &Peer, content: &str) -> impl Future<Output = bool> + Send {
        fedsync::deliver(self, peer, content)
    }
}

fn db<E: Env>(e: &E) -> chipsoc_store::Fed<'_> {
    e.store().fed(e.gkey())
}

/// Log a failed best-effort store write.
fn warn_err<T>(r: Result<T>, what: &str) {
    if let Err(e) = r {
        tracing::warn!(?e, "{what} failed");
    }
}

/// An inbox message for the worker. These may be dropped if the worker falls
/// far behind: saved ones are picked up again from the store.
pub struct Job {
    msg: u64,
    content: String,
    /// It's in the store (saving can fail; then it's handled anyway).
    saved: bool,
}

/// Admin decisions and config changes, for the worker. Kept apart from inbox
/// messages so a flood of those can never push one out, and small: each
/// batch is listed once, and "check partners" is a flag.
#[derive(Default)]
pub struct Control {
    todo: std::sync::Mutex<Todo>,
    ready: Notify,
}

#[derive(Default)]
struct Todo {
    peers: bool,
    /// Held batches an admin allowed: (partner, batch).
    release: Vec<(String, String)>,
    /// Members who just joined, to give back what partners still vouch for.
    joined: Vec<u64>,
}

/// Joins kept for the worker at once; more (a raid) wait for the partner's next update.
const MAX_JOINED: usize = 500;

impl Control {
    fn push(&self, f: impl FnOnce(&mut Todo)) {
        f(&mut self.todo.lock().unwrap_or_else(|e| e.into_inner()));
        self.ready.notify_one();
    }

    fn take(&self) -> Todo {
        std::mem::take(&mut *self.todo.lock().unwrap_or_else(|e| e.into_inner()))
    }

    async fn next(&self) -> Todo {
        loop {
            let todo = self.take();
            if todo.peers || !todo.release.is_empty() || !todo.joined.is_empty() {
                return todo;
            }
            self.ready.notified().await;
        }
    }
}

/// Uses of something limited per hour.
#[derive(Debug, Default, Clone, Copy)]
struct Budget {
    from: i64,
    used: u32,
}

#[derive(Debug, PartialEq)]
enum Spent {
    Ok,
    /// The first one over the limit this hour.
    JustOver,
    Over,
}

impl Budget {
    fn spend(&mut self, max: u32, now: i64) -> Spent {
        if now - self.from >= 3600 {
            *self = Budget { from: now, used: 0 };
        }
        self.used = self.used.saturating_add(1);
        match self.used.cmp(&(max + 1)) {
            std::cmp::Ordering::Less => Spent::Ok,
            std::cmp::Ordering::Equal => Spent::JustOver,
            std::cmp::Ordering::Greater => Spent::Over,
        }
    }
}

/// Inbound state, keyed by partner id.
#[derive(Default)]
pub struct Inbox {
    roles: HashMap<String, Batch<Member>>,
    catalogs: HashMap<String, Batch<proto::Project>>,
    /// Updates waiting for an admin, oldest first.
    held: HashMap<String, Vec<Held>>,
    /// When an admin was last asked about each partner's held updates.
    asked: HashMap<String, i64>,
    /// When an admin was last asked (or reminded) about each held batch.
    batches: HashMap<String, i64>,
    /// Envelopes taken from each partner this hour.
    rate: HashMap<String, Budget>,
    /// Notes about each partner posted this hour.
    logs: HashMap<String, Budget>,
    /// Revocations that failed, already said so in the log.
    failing: HashSet<String>,
    last_reject_log: i64,
    /// Shared items coming in parts, by (partner, batch).
    shares: HashMap<(String, String), Gathering>,
    /// Shared items of each kind taken from each partner this hour.
    shared: HashMap<(String, &'static str), Budget>,
    /// Inbox messages that failed: not tried again before then.
    retry_at: HashMap<u64, i64>,
    /// Messages given up on since the log last said so, and when it did.
    given_up: u32,
    last_give_up_log: i64,
    /// Kinds a partner sent that we don't take, said once in the log.
    not_taken: HashSet<(String, &'static str)>,
}

impl Inbox {
    /// Drop everything in progress or held from a partner. Returns the inbox
    /// messages it was in, to mark done.
    fn forget(&mut self, peer: &str) -> Vec<u64> {
        let mut msgs = Vec::new();
        msgs.extend(self.roles.remove(peer).map(|b| b.msgs).unwrap_or_default());
        msgs.extend(self.catalogs.remove(peer).map(|b| b.msgs).unwrap_or_default());
        for h in self.held.remove(peer).unwrap_or_default() {
            if let Some(b) = h.batch {
                self.batches.remove(&b);
            }
            msgs.extend(h.update.msgs);
        }
        self.asked.remove(peer);
        let parts: Vec<(String, String)> = self.shares.keys().filter(|(p, _)| p == peer).cloned().collect();
        for k in parts {
            msgs.extend(self.shares.remove(&k).map(|g| g.msgs).unwrap_or_default());
        }
        msgs
    }
}

/// What becomes of inbox messages.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Outcome {
    Done,
    /// Part of a sync still coming in.
    Waiting,
    Held,
    /// Something failed that may work later.
    Retry,
}

/// Whether the mass-change guards apply.
#[derive(Debug, Clone, PartialEq)]
enum Gate {
    Check,
    /// An admin allowed it.
    Allowed,
    /// It was held before a restart: hold it again, in the batch an admin
    /// was asked about (if they were).
    Held(Option<String>),
}

pub fn is_inbox(ctx: &Ctx, channel: Id<ChannelMarker>) -> bool {
    inbox_channel(ctx) == Some(channel)
}

fn inbox_channel(ctx: &Ctx) -> Option<Id<ChannelMarker>> {
    ctx.cfg().federation.inbox.as_deref().and_then(|k| ctx.channel(k))
}

/// The config changed: check for removed partners, keys and roles soon.
pub fn peers_changed(ctx: &Ctx) {
    ctx.fed.control.push(|t| t.peers = true);
}

/// Someone joined: if partners vouched for them before they left, the
/// worker gives those roles back.
pub fn on_join(ctx: &Ctx, user: u64) {
    if ctx.cfg().federation.peers.is_empty() {
        return;
    }
    ctx.fed.control.push(|t| {
        if t.joined.len() < MAX_JOINED {
            t.joined.push(user);
        }
    });
}

/// A new message in the inbox: save it, then queue it for the worker.
pub(crate) async fn arrived<E: Env>(e: &E, msg: u64, content: &str) {
    if !proto::is_envelope(content) {
        return;
    }
    let saved = match db(e).inbox_add(msg, content, now()).await {
        Ok(true) => true,
        Ok(false) => return, // caught up on already
        Err(err) => {
            tracing::warn!(?err, "saving a partner message failed; handling it anyway");
            false
        }
    };
    e.state().jobs.push(Job { msg, content: content.to_owned(), saved });
}

/// Catch up on the inbox, then handle inbox messages and admin decisions in
/// order, and every few minutes retry what failed, check partners and ask
/// about held updates.
pub async fn worker(ctx: Arc<Ctx>) {
    start(&*ctx).await;
    catch_up(&ctx).await;
    replay(&*ctx, false).await;
    let mut tick = tokio::time::interval(RETRY_EVERY);
    tick.tick().await;
    loop {
        tokio::select! {
            job = ctx.fed.jobs.pop() => run_job(&*ctx, job).await,
            todo = ctx.fed.control.next() => control(&*ctx, todo).await,
            _ = tick.tick() => every_few_minutes(&*ctx).await,
        }
    }
}

/// Before reading the inbox: start afresh (what was in progress is read
/// again from the store), tidy up, and finish what a stop cut short.
async fn start<E: Env>(e: &E) {
    {
        let mut inbox = e.state().inbox.lock().await;
        let failing = std::mem::take(&mut inbox.failing);
        *inbox = Inbox { failing, ..Inbox::default() };
    }
    prune(e).await;
    reconcile(e).await;
    check_peers(e).await;
}

async fn every_few_minutes<E: Env>(e: &E) {
    check_peers(e).await;
    reconcile(e).await;
    retry_pending(e).await;
    replay(e, true).await;
    let cfg = e.cfg();
    let waiting: Vec<String> = e.state().inbox.lock().await.held.keys().cloned().collect();
    for peer in cfg.federation.peers.iter().filter(|p| waiting.contains(&p.id)) {
        ask(e, peer).await;
    }
    prune(e).await;
}

async fn control<E: Env>(e: &E, todo: Todo) {
    if todo.peers {
        check_peers(e).await;
    }
    for (peer, batch) in todo.release {
        release(e, &peer, &batch).await;
    }
    for user in todo.joined {
        rejoined(e, user).await;
    }
}

async fn run_job<E: Env>(e: &E, job: Job) {
    if job.saved {
        match db(e).inbox_state(job.msg).await {
            Ok(Some(InboxState::New)) => {}
            Ok(_) => return, // handled meanwhile, from the store
            Err(err) => tracing::warn!(?err, "reading the partner inbox failed; handling the message anyway"),
        }
    }
    handle(e, job.msg, &job.content, Gate::Check, true, 0).await;
}

/// Forget what's too old to matter: inbox messages (except held ones), seqs
/// older than any envelope still accepted or from the future, revocations
/// outside the guard's window.
async fn prune<E: Env>(e: &E) {
    let fed = db(e);
    let now = now();
    e.state().inbox.lock().await.retry_at.retain(|_, at| *at > now - 3600);
    warn_err(fed.inbox_prune(now - 2 * proto::MAX_SKEW).await, "pruning the partner inbox");
    let ms = |t: i64| t.max(0) as u64 * 1000;
    let seqs = fed.prune_seqs(ms(now - 2 * proto::MAX_SKEW), ms(now + proto::MAX_SEQ_AHEAD)).await;
    warn_err(seqs, "pruning partner seqs");
    warn_err(fed.prune_revokes(now - GRANT_WINDOW).await, "pruning partner revocations");
}

/// Post to the bot log about a partner, at most [`MAX_LOGS_PER_HOUR`] times
/// an hour, so a partner can't flood it.
async fn log_about<E: Env>(e: &E, peer: &Peer, text: &str) {
    let spent = e.state().inbox.lock().await.logs.entry(peer.id.clone()).or_default().spend(MAX_LOGS_PER_HOUR, now());
    match spent {
        Spent::Ok => e.log_bot(text).await,
        Spent::JustOver => {
            let more =
                format!("(That's {MAX_LOGS_PER_HOUR} notes about {} this hour; the rest wait an hour.)", peer.name);
            e.log_bot(&format!("{text}\n{more}")).await;
        }
        Spent::Over => tracing::info!(peer = peer.name, "{text}"),
    }
}

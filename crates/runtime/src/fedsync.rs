//! Signed sync with partner servers: project catalogues and role attestations,
//! posted by webhook into each other's private inbox channel. This is the
//! sending side; `fedrecv` receives.
//!
//! The catalogue goes out when it changes; role changes stream as small
//! batched deltas; a daily full sync catches anything missed. Each envelope is
//! packed to fit Discord's message limit once sealed, so big catalogues and
//! syncs go in several parts. A full sync is a snapshot marked with the seq it
//! was taken at, so the receiver never lets it undo a later delta. Each
//! partner backs off on its own after failed sends, so one partner that's down
//! doesn't hold up or re-send to the others; a change that keeps failing is
//! dropped (the next full sync carries it).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chipsoc_config::Peer;
use chipsoc_fedproto::{self as proto, Body, Envelope, PublicKey};
use twilight_model::id::Id;
use twilight_model::id::marker::{RoleMarker, UserMarker};

use crate::queue::Queue;
use crate::{Ctx, federation, fedrecv, fedshare, members, now, ui};

const FULL_SYNC_EVERY: Duration = Duration::from_secs(24 * 3600);
/// How often to look for partners due a full sync.
const FULL_SYNC_CHECK: Duration = Duration::from_secs(60);
const FLUSH_EVERY: Duration = Duration::from_secs(30);
/// Longest project name and blurb sent, so one project always fits a message.
const MAX_NAME: usize = 100;
const MAX_BLURB: usize = 400;
const MAX_BACKOFF: Duration = Duration::from_secs(30 * 60);
/// Failed sends of one change before it's dropped.
const MAX_TRIES: u32 = 10;
/// Inbox messages waiting for the worker. Each is saved first, so one
/// dropped when it's full is read back from the store later.
const INBOX_QUEUE: usize = 1000;

/// A role change for one member, not yet sent.
#[derive(Debug, Clone, PartialEq)]
struct Delta {
    roles: Vec<String>,
    tries: u32,
}

/// What a partner last heard about a member, and in which envelope.
#[derive(Debug, Clone, PartialEq)]
struct Sent {
    roles: Vec<String>,
    seq: u64,
}

/// When a partner's next full sync is due.
#[derive(Debug, Clone, Copy)]
struct Due {
    at: Instant,
    failures: u32,
    /// A full sync has gone through since we started.
    synced: bool,
}

/// Outbound state: what each partner last heard, and changes not yet sent.
/// Keyed by partner id, then user id.
#[derive(Default)]
pub struct Outbox {
    sent: HashMap<String, HashMap<u64, Sent>>,
    queued: HashMap<String, HashMap<u64, Delta>>,
    /// Partners whose last delta failed: (failures in a row, don't retry before).
    backoff: HashMap<String, (u32, Instant)>,
    /// Each partner's full sync schedule; a partner not listed is due now.
    full: HashMap<String, Due>,
}

/// One server's sync state.
pub struct State {
    /// Our signing key, from the variable `key_env` names.
    pub key: Option<proto::SecretKey>,
    pub seq: proto::Seq,
    pub out: tokio::sync::Mutex<Outbox>,
    pub inbox: tokio::sync::Mutex<fedrecv::Inbox>,
    /// Inbox messages, for `fedrecv::worker`.
    pub jobs: Queue<fedrecv::Job>,
    /// Admin decisions and config changes, for `fedrecv::worker`; never dropped.
    pub control: fedrecv::Control,
    /// Shared content waiting to go, for `fedshare::send_loop`.
    pub shares: fedshare::Outgoing,
}

impl State {
    pub fn new(cfg: &chipsoc_config::Config) -> Self {
        let var = &cfg.federation.key_env;
        let key = std::env::var(var).ok().and_then(|k| match proto::signing_key(&k) {
            Ok(k) => Some(k),
            Err(e) => {
                tracing::warn!(var, e, "invalid signing key; signed sync is off");
                None
            }
        });
        Self::with_key(key)
    }

    /// Signing with `key` (none: signed sync is off).
    pub fn with_key(key: Option<proto::SecretKey>) -> Self {
        Self {
            key,
            seq: Default::default(),
            out: Default::default(),
            inbox: Default::default(),
            jobs: Queue::new("partner inbox", INBOX_QUEUE),
            control: Default::default(),
            shares: Default::default(),
        }
    }
}

/// Each partner's public keys. Bad keys are skipped with a warning.
pub fn peer_keys(peers: &[Peer]) -> HashMap<String, Vec<PublicKey>> {
    let mut out = HashMap::new();
    for p in peers.iter().filter(|p| !p.id.is_empty()) {
        let keys: Vec<PublicKey> = p
            .keys()
            .filter_map(|k| {
                proto::verifying_key(k).map_err(|e| tracing::warn!(peer = p.name, e, "bad peer public key")).ok()
            })
            .collect();
        if !keys.is_empty() {
            out.insert(p.id.clone(), keys);
        }
    }
    out
}

pub(crate) fn enabled(ctx: &Ctx) -> bool {
    ctx.fed.key.is_some() && !ctx.cfg().federation.name.is_empty()
}

pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

/// Seal and send one envelope; its seq if it went.
async fn send(ctx: &Ctx, peer: &Peer, body: Body) -> Option<u64> {
    let (content, seq) = seal_for(&ctx.fed, &ctx.cfg().federation.name, peer, body)?;
    deliver(ctx, peer, &content).await.then_some(seq)
}

/// `body` sealed for `peer` as `from`, with the next seq: the message and
/// its seq. None if we have no key, or it's too big for a message.
pub(crate) fn seal_for(state: &State, from: &str, peer: &Peer, body: Body) -> Option<(String, u64)> {
    let sk = state.key.as_ref()?;
    let env = match Envelope::new(from, &peer.id, state.seq.next(now_ms()), now(), body) {
        Ok(e) => e,
        Err(e) => {
            tracing::error!(e, "can't make an envelope");
            return None;
        }
    };
    let content = proto::seal(sk, &env);
    if content.chars().count() > ui::MAX_CONTENT {
        tracing::warn!(peer = peer.name, "envelope too large; not sent");
        return None;
    }
    Some((content, env.seq))
}

/// Post a sealed envelope into `peer`'s inbox, by its webhook. Whether it went.
pub(crate) async fn deliver(ctx: &Ctx, peer: &Peer, content: &str) -> bool {
    let Some((hook, token)) = std::env::var(&peer.webhook_env).ok().as_deref().and_then(federation::parse_webhook)
    else {
        tracing::warn!(peer = peer.name, var = peer.webhook_env, "partner webhook not set");
        return false;
    };
    // Our project names, posts and blurbs are in it: never ping anyone in their inbox.
    match federation::by_webhook(ctx, hook, &token, &ctx.cfg().federation.name, content).await {
        Ok(()) => true,
        Err(e) => {
            tracing::warn!(?e, peer = peer.name, "sending envelope failed");
            false
        }
    }
}

/// Whether `body` fits in one message once sealed. Seq, time and nonce are
/// measured at the width real ones have.
pub(crate) fn fits(sk: &proto::SecretKey, from: &str, to: &str, body: Body) -> bool {
    let env = Envelope { from: from.into(), to: to.into(), seq: now_ms(), ts: now(), nonce: "x".repeat(16), body };
    proto::sealed_len(sk, &env) <= ui::MAX_CONTENT
}

/// Split `items` into runs that each `fits`, in order. Items too big to fit
/// even alone come back separately.
fn pack<T>(items: Vec<T>, mut fits: impl FnMut(&[T]) -> bool) -> (Vec<Vec<T>>, Vec<T>) {
    let (mut chunks, mut skipped, mut cur) = (Vec::new(), Vec::new(), Vec::new());
    for item in items {
        cur.push(item);
        if fits(&cur) {
            continue;
        }
        if cur.len() > 1 {
            let last = cur.split_off(cur.len() - 1);
            chunks.push(std::mem::replace(&mut cur, last));
            if fits(&cur) {
                continue;
            }
        }
        skipped.append(&mut cur);
    }
    if !cur.is_empty() {
        chunks.push(cur);
    }
    (chunks, skipped)
}

/// How long to wait after `failures` failed sends in a row.
pub(crate) fn backoff(failures: u32) -> Duration {
    let factor = 1u32.checked_shl(failures.saturating_sub(1)).unwrap_or(u32::MAX);
    FLUSH_EVERY.saturating_mul(factor).min(MAX_BACKOFF)
}

/// The role keys we attest to `peer` for a member holding `roles`.
fn attest(ctx: &Ctx, peer: &Peer, roles: &[Id<RoleMarker>]) -> Vec<String> {
    peer.send_roles.iter().filter(|k| ctx.role(k).is_some_and(|id| roles.contains(&id))).cloned().collect()
}

fn member(id: u64, roles: &[String]) -> proto::Member {
    proto::Member { id: id.to_string(), roles: roles.to_vec() }
}

/// Queue a member's current roles for every partner we attest to.
/// Called on member updates, joins and leaves (`roles` empty).
pub async fn on_member(ctx: &Ctx, user: Id<UserMarker>, roles: &[Id<RoleMarker>], bot: bool) {
    let cfg = ctx.cfg();
    if !enabled(ctx) || bot {
        return;
    }
    let mut out = ctx.fed.out.lock().await;
    for peer in cfg.federation.peers.iter().filter(|p| !p.send_roles.is_empty()) {
        let now = attest(ctx, peer, roles);
        let before =
            out.sent.get(&peer.id).and_then(|m| m.get(&user.get())).map(|s| s.roles.clone()).unwrap_or_default();
        let queue = out.queued.entry(peer.id.clone()).or_default();
        if now == before {
            queue.remove(&user.get());
        } else {
            queue.insert(user.get(), Delta { roles: now, tries: 0 });
        }
    }
}

/// Send queued role changes every 30 seconds, skipping partners in backoff.
pub async fn flush_loop(ctx: Arc<Ctx>) {
    let mut tick = tokio::time::interval(FLUSH_EVERY);
    loop {
        tick.tick().await;
        if !enabled(&ctx) {
            continue;
        }
        let cfg = ctx.cfg();
        for (peer_id, changes) in take_due(&ctx).await {
            if let Some(peer) = cfg.federation.peers.iter().find(|p| p.id == peer_id) {
                flush_peer(&ctx, peer, changes).await;
            }
        }
    }
}

async fn take_due(ctx: &Ctx) -> Vec<(String, HashMap<u64, Delta>)> {
    let mut out = ctx.fed.out.lock().await;
    let out = &mut *out;
    let now = Instant::now();
    let due: Vec<String> = out
        .queued
        .iter()
        .filter(|(p, c)| !c.is_empty() && out.backoff.get(*p).is_none_or(|(_, until)| *until <= now))
        .map(|(p, _)| p.clone())
        .collect();
    due.into_iter().filter_map(|p| out.queued.remove(&p).map(|c| (p, c))).collect()
}

fn delta_body(chunk: &[(u64, Delta)]) -> Body {
    Body::RolesDelta { members: chunk.iter().map(|(u, d)| member(*u, &d.roles)).collect() }
}

async fn flush_peer(ctx: &Ctx, peer: &Peer, changes: HashMap<u64, Delta>) {
    let Some(sk) = &ctx.fed.key else { return };
    let from = ctx.cfg().federation.name.clone();
    let (chunks, skipped) = pack(changes.into_iter().collect(), |c| fits(sk, &from, &peer.id, delta_body(c)));
    for (user, _) in skipped {
        tracing::warn!(peer = peer.name, user, "a member's roles don't fit in one message; not sent");
    }
    let mut chunks = chunks.into_iter();
    while let Some(chunk) = chunks.next() {
        let Some(seq) = send(ctx, peer, delta_body(&chunk)).await else {
            let failed = chunk.into_iter().chain(chunks.flatten()).collect();
            return failed_send(ctx, peer, failed).await;
        };
        let mut out = ctx.fed.out.lock().await;
        let sent = out.sent.entry(peer.id.clone()).or_default();
        sent.extend(chunk.into_iter().map(|(u, d)| (u, Sent { roles: d.roles, seq })));
    }
    ctx.fed.out.lock().await.backoff.remove(&peer.id);
}

/// Back off from the partner and queue the changes again with a try counted.
async fn failed_send(ctx: &Ctx, peer: &Peer, failed: Vec<(u64, Delta)>) {
    let dropped = {
        let mut out = ctx.fed.out.lock().await;
        let out = &mut *out;
        let failures = out.backoff.get(&peer.id).map_or(0, |(n, _)| *n) + 1;
        out.backoff.insert(peer.id.clone(), (failures, Instant::now() + backoff(failures)));
        requeue(out.queued.entry(peer.id.clone()).or_default(), failed)
    };
    if dropped > 0 {
        let msg = format!(
            "⚠️ **Gave up sending {dropped} role change(s) to {}** after {MAX_TRIES} tries. The next daily sync sends everyone's current roles.",
            peer.name
        );
        ctx.log_bot(&msg).await;
    }
}

/// Put failed changes back unless something newer was queued meanwhile.
/// Returns how many were dropped for failing too often.
fn requeue(queue: &mut HashMap<u64, Delta>, failed: Vec<(u64, Delta)>) -> usize {
    let mut dropped = 0;
    for (user, d) in failed {
        if queue.contains_key(&user) {
            continue;
        }
        if d.tries + 1 >= MAX_TRIES {
            dropped += 1;
        } else {
            queue.insert(user, Delta { roles: d.roles, tries: d.tries + 1 });
        }
    }
    dropped
}

/// Full syncs for every partner that's due: the catalogue if it changed (or
/// always, after the first), then everyone's roles. Each partner is scheduled
/// on its own: daily after a success, sooner with backoff after a failure.
async fn sync_due(ctx: &Ctx) {
    let cfg = ctx.cfg();
    let now = Instant::now();
    let due: Vec<(&Peer, Due)> = {
        let out = ctx.fed.out.lock().await;
        cfg.federation
            .peers
            .iter()
            .filter(|p| !p.id.is_empty() && (p.sends("projects") || !p.send_roles.is_empty()))
            .map(|p| (p, out.full.get(&p.id).copied().unwrap_or(Due { at: now, failures: 0, synced: false })))
            .filter(|(_, d)| d.at <= now)
            .collect()
    };
    if due.is_empty() {
        return;
    }
    // The snapshot's seq is taken before reading members, so any delta sent
    // after it has a higher one and wins at the partner.
    let as_of = ctx.fed.seq.next(now_ms());
    let members = if due.iter().any(|(p, _)| !p.send_roles.is_empty()) {
        match ctx.members().await {
            Ok(m) => Some(m),
            Err(e) => {
                tracing::warn!(?e, "listing members failed; full role sync skipped");
                None
            }
        }
    } else {
        Some(Arc::new(Vec::new()))
    };
    for (peer, d) in due {
        let mut ok = !peer.sends("projects") || send_catalogue(ctx, peer, d.synced).await;
        if !peer.send_roles.is_empty() {
            ok &= match &members {
                Some(m) => sync_roles(ctx, peer, m, as_of).await,
                None => false,
            };
        }
        let next = if ok {
            Due { at: Instant::now() + FULL_SYNC_EVERY, failures: 0, synced: true }
        } else {
            let failures = d.failures + 1;
            tracing::warn!(peer = peer.name, failures, "full sync failed; trying again later");
            Due { at: Instant::now() + backoff(failures), failures, synced: d.synced }
        };
        ctx.fed.out.lock().await.full.insert(peer.id.clone(), next);
    }
}

/// Send the catalogue now to partners that haven't had this version. A
/// partner it fails for is retried by the full sync loop, with backoff.
pub async fn send_new_catalogue(ctx: &Ctx) {
    if !enabled(ctx) {
        return;
    }
    let cfg = ctx.cfg();
    for peer in cfg.federation.peers.iter().filter(|p| p.sends("projects") && !p.id.is_empty()) {
        if !send_catalogue(ctx, peer, false).await {
            tracing::warn!(peer = peer.name, "sending the changed catalogue failed; trying again later");
            let mut out = ctx.fed.out.lock().await;
            let d = out.full.entry(peer.id.clone()).or_insert(Due { at: Instant::now(), failures: 0, synced: false });
            d.failures += 1;
            d.at = d.at.min(Instant::now() + backoff(d.failures));
        }
    }
}

pub fn catalogue_hash(ctx: &Ctx) -> String {
    hash_of(&catalogue(ctx))
}

fn hash_of(body: &Body) -> String {
    chipsoc_model::stable_hash(serde_json::to_string(body).unwrap_or_default())
}

/// The catalogue in parts that each fit a message. A project too big even
/// alone is left out.
fn catalogue_parts(
    sk: &proto::SecretKey,
    from: &str,
    to: &str,
    projects: Vec<proto::Project>,
    batch: &str,
) -> Vec<Body> {
    let part = |projects: &[proto::Project], part, parts| Body::Catalog {
        projects: projects.to_vec(),
        batch: batch.to_owned(),
        part,
        parts,
    };
    let widest = projects.len().max(1) as u64;
    let (mut chunks, skipped) = pack(projects, |c| fits(sk, from, to, part(c, widest, widest)));
    for p in skipped {
        tracing::warn!(project = p.slug, "a project doesn't fit in one message; left out of the catalogue");
    }
    if chunks.is_empty() {
        chunks.push(Vec::new()); // an empty catalogue still says "none"
    }
    let parts = chunks.len() as u64;
    chunks.iter().enumerate().map(|(i, c)| part(c, i as u64, parts)).collect()
}

/// Send one partner the catalogue unless it already has this version (or
/// `force`). False if a part failed to send.
async fn send_catalogue(ctx: &Ctx, peer: &Peer, force: bool) -> bool {
    let Some(sk) = &ctx.fed.key else { return false };
    let Body::Catalog { projects, .. } = catalogue(ctx) else { return false };
    let hash = catalogue_hash(ctx);
    let fed = ctx.store.fed(&ctx.gkey);
    let last = match fed.sent_hash(&peer.id, "catalog").await {
        Ok(h) => h,
        Err(e) => {
            tracing::warn!(?e, "reading the catalogue hash failed; sending anyway");
            None
        }
    };
    if !force && last.as_deref() == Some(hash.as_str()) {
        return true;
    }
    let from = ctx.cfg().federation.name.clone();
    let batch = chipsoc_model::stable_hash(format!("{}{hash}{}", peer.id, now_ms()));
    for (i, body) in catalogue_parts(sk, &from, &peer.id, projects, &batch).into_iter().enumerate() {
        if send(ctx, peer, body).await.is_none() {
            tracing::warn!(peer = peer.name, part = i, "sending the catalogue stopped at a failed send");
            return false;
        }
    }
    ctx.log_err(fed.set_sent_hash(&peer.id, "catalog", &hash).await, "saving catalogue hash");
    true
}

fn roles_part(batch: &str, part: u64, parts: u64, total: u64, members: &[proto::Member], as_of: u64) -> Body {
    Body::Roles { batch: batch.to_owned(), part, parts, total, members: members.to_vec(), as_of }
}

/// One partner's full role sync, a snapshot of `members` taken at seq `as_of`.
/// Stops at the first part that fails.
async fn sync_roles(ctx: &Ctx, peer: &Peer, members: &[serde_json::Value], as_of: u64) -> bool {
    let Some(sk) = &ctx.fed.key else { return false };
    let mut state: HashMap<u64, Vec<String>> = members
        .iter()
        .filter(|m| !members::is_bot(m))
        .filter_map(|m| Some((members::user(m)?, attest(ctx, peer, &members::roles(m)))))
        .filter(|(_, r)| !r.is_empty())
        .collect();
    let list: Vec<proto::Member> = state.iter().map(|(u, r)| member(*u, r)).collect();
    let batch = chipsoc_model::stable_hash(format!("{}{}", peer.id, now_ms()));
    let from = ctx.cfg().federation.name.clone();
    // Measured with part numbers as wide as they can get.
    let widest = list.len().max(1) as u64;
    let (mut chunks, skipped) =
        pack(list, |c| fits(sk, &from, &peer.id, roles_part(&batch, widest, widest, widest, c, as_of)));
    for m in skipped {
        tracing::warn!(peer = peer.name, user = m.id, "a member's roles don't fit in one message; left out");
        state.retain(|u, _| u.to_string() != m.id);
    }
    if chunks.is_empty() {
        chunks.push(Vec::new()); // an empty sync still says "nobody"
    }
    let (parts, total) = (chunks.len() as u64, state.len() as u64);
    for (i, chunk) in chunks.iter().enumerate() {
        if send(ctx, peer, roles_part(&batch, i as u64, parts, total, chunk, as_of)).await.is_none() {
            tracing::warn!(peer = peer.name, part = i, parts, "full role sync stopped at a failed send");
            return false;
        }
    }
    let mut out = ctx.fed.out.lock().await;
    merge_snapshot(out.sent.entry(peer.id.clone()).or_default(), state, as_of);
    true
}

/// What a partner heard, after a full sync taken at `as_of`: the snapshot,
/// except for members a delta sent after it told them about since.
fn merge_snapshot(sent: &mut HashMap<u64, Sent>, snapshot: HashMap<u64, Vec<String>>, as_of: u64) {
    sent.retain(|_, s| s.seq > as_of);
    for (user, roles) in snapshot {
        sent.entry(user).or_insert(Sent { roles, seq: as_of });
    }
}

fn catalogue(ctx: &Ctx) -> Body {
    let projects = ctx
        .cfg()
        .projects
        .iter()
        .map(|p| proto::Project {
            slug: p.slug.clone(),
            name: ui::truncate(&p.name, MAX_NAME),
            blurb: ui::truncate(&p.blurb, MAX_BLURB),
            kind: p.kind.clone(),
            status: p.status.to_string(),
            github: p.github.clone(),
            open: p.application.open,
        })
        .collect();
    Body::Catalog { projects, batch: String::new(), part: 0, parts: 1 }
}

/// Daily full syncs, each partner on its own schedule. After the first, the
/// catalogue is resent even if unchanged, in case a partner lost state.
pub async fn full_sync_loop(ctx: Arc<Ctx>) {
    tokio::time::sleep(Duration::from_secs(120)).await;
    let mut tick = tokio::time::interval(FULL_SYNC_CHECK);
    loop {
        tick.tick().await;
        if enabled(&ctx) {
            sync_due(&ctx).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> proto::SecretKey {
        proto::signing_key(&proto::generate().unwrap().0).unwrap()
    }

    fn attested(n: usize, roles: usize) -> Vec<proto::Member> {
        let roles: Vec<String> = (0..roles).map(|r| format!("role-number-{r}")).collect();
        (0..n).map(|i| member(100_000_000_000_000_000 + i as u64, &roles)).collect()
    }

    #[test]
    fn chunks_fit_once_sealed() {
        let sk = key();
        let list = attested(200, 3);
        let body = |c: &[proto::Member]| roles_part("0123456789abcdef", 200, 200, 200, c, now_ms());
        let (chunks, skipped) = pack(list.clone(), |c| fits(&sk, "us", "them", body(c)));
        assert!(skipped.is_empty());
        assert!(chunks.len() > 1, "200 members need several messages");
        assert_eq!(chunks.concat(), list, "nothing lost or reordered");
        for c in &chunks {
            let env = Envelope::new("us", "them", now_ms(), now(), body(c)).unwrap();
            assert!(proto::seal(&sk, &env).chars().count() <= ui::MAX_CONTENT);
        }
    }

    #[test]
    fn a_member_too_big_for_any_message_is_skipped() {
        let sk = key();
        let mut list = attested(3, 1);
        list.insert(1, attested(1, 200).remove(0));
        let delta = |c: &[proto::Member]| Body::RolesDelta { members: c.to_vec() };
        let (chunks, skipped) = pack(list.clone(), |c| fits(&sk, "us", "them", delta(c)));
        assert_eq!(skipped, vec![list[1].clone()]);
        assert_eq!(chunks.concat(), vec![list[0].clone(), list[2].clone(), list[3].clone()]);
    }

    #[test]
    fn pack_keeps_order_and_splits_on_the_limit() {
        let (chunks, skipped) = pack(vec![1, 2, 3, 9, 4], |c: &[i32]| c.iter().sum::<i32>() <= 6);
        assert_eq!(chunks, vec![vec![1, 2, 3], vec![4]]);
        assert_eq!(skipped, vec![9]);
        assert_eq!(pack(Vec::<i32>::new(), |_| true), (vec![], vec![]));
    }

    fn project(i: usize) -> proto::Project {
        proto::Project {
            slug: format!("project-{i}"),
            name: format!("Project number {i}"),
            blurb: "A fairly long description of what this project does and who it is for. ".repeat(4),
            kind: "hardware".into(),
            status: "active".into(),
            github: Some(format!("https://github.com/example/project-{i}")),
            open: true,
        }
    }

    #[test]
    fn a_big_catalogue_goes_in_parts_that_each_fit() {
        let sk = key();
        let projects: Vec<proto::Project> = (0..30).map(project).collect();
        let whole = Body::Catalog { projects: projects.clone(), batch: String::new(), part: 0, parts: 1 };
        assert!(!fits(&sk, "us", "them", whole), "30 projects don't fit in one message");
        let parts = catalogue_parts(&sk, "us", "them", projects.clone(), "0123456789abcdef");
        assert!(parts.len() > 1);
        let mut got = Vec::new();
        for (i, body) in parts.iter().enumerate() {
            let Body::Catalog { projects, part, parts: n, .. } = body else { panic!() };
            assert_eq!((*part, *n), (i as u64, parts.len() as u64));
            got.extend(projects.clone());
            let env = Envelope::new("us", "them", now_ms(), now(), body.clone()).unwrap();
            assert!(proto::seal(&sk, &env).chars().count() <= ui::MAX_CONTENT);
        }
        assert_eq!(got, projects, "nothing lost or reordered");
        assert_eq!(catalogue_parts(&sk, "us", "them", vec![], "b").len(), 1, "an empty catalogue is still sent");
    }

    #[test]
    fn a_snapshot_never_hides_a_later_delta() {
        let sent = |roles: &[&str], seq| Sent { roles: roles.iter().map(|r| r.to_string()).collect(), seq };
        // Users 1 and 2 were told about before the snapshot (seq 10), user 3 after it.
        let mut out = HashMap::from([(1, sent(&["old"], 5)), (2, sent(&["gone"], 6)), (3, sent(&["newer"], 12))]);
        let snapshot = HashMap::from([(1, vec!["a".into()]), (3, vec!["stale".into()]), (4, vec!["b".into()])]);
        merge_snapshot(&mut out, snapshot, 10);
        assert_eq!(out[&1], sent(&["a"], 10));
        assert!(!out.contains_key(&2), "not in the snapshot: they heard nobody has roles");
        assert_eq!(out[&3], sent(&["newer"], 12), "the later delta stands");
        assert_eq!(out[&4], sent(&["b"], 10));
    }

    #[test]
    fn backoff_doubles_up_to_half_an_hour() {
        assert_eq!(backoff(1), Duration::from_secs(30));
        assert_eq!(backoff(2), Duration::from_secs(60));
        assert_eq!(backoff(4), Duration::from_secs(240));
        assert_eq!(backoff(7), MAX_BACKOFF);
        assert_eq!(backoff(200), MAX_BACKOFF);
    }

    #[test]
    fn requeue_counts_tries_and_gives_up() {
        let d = |roles: &[&str], tries| Delta { roles: roles.iter().map(|r| r.to_string()).collect(), tries };
        let mut queue = HashMap::from([(2, d(&["newer"], 0))]);
        let failed = vec![(1, d(&["a"], 0)), (2, d(&["older"], 3)), (3, d(&["b"], MAX_TRIES - 1))];
        assert_eq!(requeue(&mut queue, failed), 1, "user 3 has failed too often");
        assert_eq!(queue[&1], d(&["a"], 1));
        assert_eq!(queue[&2], d(&["newer"], 0), "a newer change wins");
        assert!(!queue.contains_key(&3));
    }

    #[test]
    fn peer_keys_take_every_valid_key() {
        let (_, a) = proto::generate().unwrap();
        let (_, b) = proto::generate().unwrap();
        let peer = |id: &str, public_key: &str, public_keys: Vec<String>| Peer {
            id: id.into(),
            public_key: public_key.into(),
            public_keys,
            ..Peer::default()
        };
        let keys = peer_keys(&[peer("p", &a, vec![b, "junk".into()]), peer("q", "", vec![]), peer("", &a, vec![])]);
        assert_eq!(keys.len(), 1);
        assert_eq!(keys["p"].len(), 2);
    }
}

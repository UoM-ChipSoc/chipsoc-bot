//! Verifying inbox messages and putting multi-part syncs back together.

use super::*;

/// Post rejected envelopes to the log at most this often (seconds).
pub(super) const REJECT_LOG_EVERY: i64 = 600;
/// Envelopes taken from one partner per hour as they arrive; more are dropped.
pub(super) const MAX_PER_HOUR: u32 = 1000;
/// Most parts a catalogue may come in (each fits one message).
pub(super) const MAX_CATALOG_PARTS: u64 = 200;

/// Role state from one delta, or from every part of a full sync.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Update {
    pub(super) members: Vec<Member>,
    pub(super) seq: u64,
    pub(super) full: bool,
    /// The key that signed it.
    pub(super) kid: String,
    /// The inbox messages it came in.
    pub(super) msgs: Vec<u64>,
}

/// One part of a catalogue or full role sync.
pub(super) struct Part<T> {
    pub(super) batch: String,
    pub(super) as_of: u64,
    pub(super) part: u64,
    pub(super) parts: u64,
    pub(super) seq: u64,
    pub(super) msg: u64,
    pub(super) kid: String,
    pub(super) items: Vec<T>,
}

/// A catalogue or full role sync being received.
pub(super) struct Batch<T> {
    pub(super) id: String,
    pub(super) as_of: u64,
    pub(super) parts: u64,
    pub(super) got: BTreeMap<u64, Vec<T>>,
    /// The lowest envelope seq among the parts.
    pub(super) seq: u64,
    pub(super) msgs: Vec<u64>,
    pub(super) kid: String,
}

#[derive(Debug, PartialEq)]
pub(super) enum Added<T> {
    /// More parts to come.
    Waiting,
    /// A part of an older sync than the one being received.
    Stale,
    /// Every part is in. `seq` is when it counts from: the snapshot's `as_of`,
    /// or (from older senders) its first envelope.
    Complete { items: Vec<T>, seq: u64, kid: String, msgs: Vec<u64> },
}

/// Add one part to a partner's batch. Also returns the messages of an older
/// batch this one replaced, which won't complete now.
pub(super) fn add_part<T>(batches: &mut HashMap<String, Batch<T>>, peer: &str, p: Part<T>) -> (Added<T>, Vec<u64>) {
    let mut replaced = Vec::new();
    if let Some(b) = batches.get(peer)
        && (b.id != p.batch || b.parts != p.parts)
    {
        if p.as_of != 0 && p.as_of < b.as_of {
            return (Added::Stale, replaced);
        }
        replaced = batches.remove(peer).map(|b| b.msgs).unwrap_or_default();
    }
    let b = batches.entry(peer.to_owned()).or_insert_with(|| Batch {
        id: p.batch,
        as_of: p.as_of,
        parts: p.parts,
        got: BTreeMap::new(),
        seq: p.seq,
        msgs: Vec::new(),
        kid: String::new(),
    });
    b.seq = b.seq.min(p.seq);
    if !b.msgs.contains(&p.msg) {
        b.msgs.push(p.msg);
    }
    b.kid = p.kid;
    b.got.entry(p.part).or_insert(p.items);
    if (b.got.len() as u64) < b.parts {
        return (Added::Waiting, replaced);
    }
    let Some(b) = batches.remove(peer) else { return (Added::Waiting, replaced) };
    let seq = if b.as_of != 0 { b.as_of } else { b.seq };
    (Added::Complete { items: b.got.into_values().flatten().collect(), seq, kid: b.kid, msgs: b.msgs }, replaced)
}

/// Handle saved inbox messages that aren't done, oldest first; with
/// `new_only`, just those not handled yet (or that failed).
pub(super) async fn replay<E: Env>(e: &E, new_only: bool) {
    let fed = db(e);
    let pending = match fed.inbox_pending(new_only).await {
        Ok(p) => p,
        Err(err) => return tracing::warn!(?err, "reading the partner inbox failed"),
    };
    for m in pending {
        if m.tries >= MAX_TRIES {
            tracing::warn!(msg = m.msg, "a partner message kept failing; giving up on it");
            give_up_note(e).await;
            warn_err(fed.inbox_set(m.msg, InboxState::Done).await, "updating the partner inbox");
            continue;
        }
        let not_yet = e.state().inbox.lock().await.retry_at.get(&m.msg).is_some_and(|at| *at > now());
        if m.tries > 0 && not_yet {
            continue; // backing off
        }
        let gate = if m.state == InboxState::Held { Gate::Held(m.batch) } else { Gate::Check };
        handle(e, m.msg, &m.content, gate, false, m.tries).await;
    }
}

/// Say in the log that partner messages were given up on, at most every
/// [`REJECT_LOG_EVERY`] (with how many since).
async fn give_up_note<E: Env>(e: &E) {
    let mut inbox = e.state().inbox.lock().await;
    inbox.given_up += 1;
    if now() - inbox.last_give_up_log < REJECT_LOG_EVERY {
        return;
    }
    let n = std::mem::take(&mut inbox.given_up);
    inbox.last_give_up_log = now();
    drop(inbox);
    let what = if n == 1 { "a partner message".to_owned() } else { format!("{n} partner messages") };
    e.log_bot(&format!("⚠️ **Gave up on {what} after {MAX_TRIES} tries.** Their next daily sync covers roles; shared items they send again are taken then.")).await;
}

/// How long a message that failed `tries` times waits before the next try (seconds).
pub(super) fn retry_after(tries: u32) -> i64 {
    (RETRY_EVERY.as_secs() as i64).saturating_mul(1 << tries.saturating_sub(1).min(4)).min(3600)
}

pub(super) async fn mark<E: Env>(e: &E, msgs: &[u64], outcome: Outcome) {
    let fed = db(e);
    for &m in msgs {
        let r = match outcome {
            Outcome::Done => fed.inbox_set(m, InboxState::Done).await,
            Outcome::Waiting => fed.inbox_set(m, InboxState::Waiting).await,
            Outcome::Held => fed.inbox_set(m, InboxState::Held).await,
            Outcome::Retry => match fed.inbox_failed(m).await {
                Ok(tries) => {
                    e.state().inbox.lock().await.retry_at.insert(m, now() + retry_after(tries));
                    Ok(())
                }
                Err(err) => Err(err),
            },
        };
        warn_err(r, "updating the partner inbox");
    }
}

/// Verify and act on one inbox message. `live`: it just arrived (rather than
/// being read back from the store); `tries`: how often it failed before.
pub(super) async fn handle<E: Env>(e: &E, msg: u64, content: &str, gate: Gate, live: bool, tries: u32) {
    let cfg = e.cfg();
    let at = posted_at(msg);
    let opened = match proto::verify(content, &peer_keys(&cfg.federation.peers), &cfg.federation.name, at) {
        Ok(o) => o,
        Err(err) => {
            reject(e, &err.to_string()).await;
            return mark(e, &[msg], Outcome::Done).await;
        }
    };
    let (env, kid) = (opened.envelope, opened.kid);
    let fed = db(e);
    match fed.fresh_nonce(&env.from, &env.nonce, env.ts, now() - 2 * proto::MAX_SKEW, msg).await {
        Ok(true) => {}
        Ok(false) => {
            reject(e, "replayed envelope").await;
            return mark(e, &[msg], Outcome::Done).await;
        }
        Err(err) => {
            tracing::warn!(?err, "nonce check failed; will retry");
            return mark(e, &[msg], Outcome::Retry).await;
        }
    }
    let Some(peer) = cfg.federation.peers.iter().find(|p| p.id == env.from) else {
        return mark(e, &[msg], Outcome::Done).await;
    };
    if live {
        let spent = e.state().inbox.lock().await.rate.entry(peer.id.clone()).or_default().spend(MAX_PER_HOUR, now());
        if spent != Spent::Ok {
            if spent == Spent::JustOver {
                let text = format!(
                    "⚠️ **{} sent over {MAX_PER_HOUR} messages this hour**, so the bot is ignoring more until the hour's up. Their next full sync covers what's missed; check with them.",
                    peer.name
                );
                e.log_bot(&text).await;
            }
            return mark(e, &[msg], Outcome::Done).await;
        }
    }
    // A part of a held sync, read back after a restart, stays held.
    let waiting = if matches!(gate, Gate::Held(_)) { Outcome::Held } else { Outcome::Waiting };
    match env.body {
        Body::Catalog { projects, batch, part, parts } => {
            if parts > MAX_CATALOG_PARTS {
                reject(e, &format!("a catalogue in {parts} parts")).await;
                return mark(e, &[msg], Outcome::Done).await;
            }
            let p = Part { batch, as_of: 0, part, parts, seq: env.seq, msg, kid, items: projects };
            let (added, replaced) = add_part(&mut e.state().inbox.lock().await.catalogs, &peer.id, p);
            mark(e, &replaced, Outcome::Done).await;
            match added {
                Added::Waiting => mark(e, &[msg], Outcome::Waiting).await,
                Added::Stale => mark(e, &[msg], Outcome::Done).await,
                Added::Complete { items, seq, msgs, .. } => {
                    let outcome = keep_catalogue(e, peer, items, env.ts, seq).await;
                    mark(e, &msgs, outcome).await;
                }
            }
        }
        Body::RolesDelta { members } => {
            let u = Update { members, seq: env.seq, full: false, kid, msgs: vec![msg] };
            settle(e, peer, u, gate).await;
        }
        Body::Share(item) => {
            let (gathered, expired) = {
                let shares = &mut e.state().inbox.lock().await.shares;
                let expired = expire(shares, &peer.id, now());
                (gather(shares, &peer.id, item, env.seq, msg, now()), expired)
            };
            if !expired.is_empty() {
                mark(e, &expired, Outcome::Done).await;
                let text = format!(
                    "{} **Some of what {} shared came in parts that never all arrived**, so it was left out. If it matters, ask them to share it again.",
                    fmt::ATTENTION,
                    peer.name
                );
                log_about(e, peer, &text).await;
            }
            match gathered {
                Gathered::Waiting => mark(e, &[msg], Outcome::Waiting).await,
                Gathered::Dropped => {
                    let text = format!(
                        "{} **{} is sending too many things in parts at once**, so the bot left one out. Their next share of it is taken.",
                        fmt::ATTENTION,
                        peer.name
                    );
                    log_about(e, peer, &text).await;
                    mark(e, &[msg], Outcome::Done).await;
                }
                Gathered::Complete { item, seq, msgs } => {
                    let outcome = take_share(e, peer, *item, seq, tries == 0).await;
                    mark(e, &msgs, outcome).await;
                }
            }
        }
        Body::Unshare { what, origin } => {
            let outcome = match chipsoc_admin::shares::take_down(e, peer, what, &origin, env.seq).await {
                Ok(_) => Outcome::Done,
                Err(err) => {
                    tracing::warn!(?err, peer = peer.name, "taking down a partner's item failed; will retry");
                    Outcome::Retry
                }
            };
            mark(e, &[msg], outcome).await;
        }
        Body::Unknown(kind) => {
            // From a newer version: nothing here knows what to do with it.
            tracing::info!(peer = peer.name, kind, "ignored a partner message of a kind this version doesn't know");
            mark(e, &[msg], Outcome::Done).await;
        }
        Body::Roles { batch, part, parts, members, as_of, .. } => {
            let p = Part { batch, as_of, part, parts, seq: env.seq, msg, kid, items: members };
            let (added, replaced) = add_part(&mut e.state().inbox.lock().await.roles, &peer.id, p);
            mark(e, &replaced, Outcome::Done).await;
            match added {
                Added::Waiting => mark(e, &[msg], waiting).await,
                Added::Stale => mark(e, &[msg], Outcome::Done).await,
                Added::Complete { items, seq, kid, msgs } => {
                    settle(e, peer, Update { members: items, seq, full: true, kid, msgs }, gate).await;
                }
            }
        }
    }
}

/// Store a partner's complete catalogue, unless it's too big; says so in
/// the log only when it changed.
pub(super) async fn keep_catalogue<E: Env>(
    e: &E,
    peer: &Peer,
    items: Vec<proto::Project>,
    ts: i64,
    seq: u64,
) -> Outcome {
    let n = items.len();
    let body = serde_json::json!({ "projects": items }).to_string();
    if body.len() > MAX_CATALOG {
        let text = format!(
            "⚠️ **{}'s project catalogue is over {} KB**, so the bot didn't keep it.",
            peer.name,
            MAX_CATALOG / 1024
        );
        log_about(e, peer, &text).await;
        return Outcome::Done;
    }
    let fed = db(e);
    let before = fed.catalogs().await.ok().and_then(|c| c.into_iter().find(|c| c.peer == peer.id)).map(|c| c.body);
    if let Err(err) = fed.set_catalog(&peer.id, &body, ts, seq).await {
        tracing::warn!(?err, "saving a partner catalogue failed; will retry");
        return Outcome::Retry;
    }
    if before.as_deref() != Some(body.as_str()) {
        log_about(e, peer, &format!("📚 **{} shared their projects** ({n}). See `/partners`.", peer.name)).await;
    }
    Outcome::Done
}

pub(super) async fn reject<E: Env>(e: &E, why: &str) {
    tracing::warn!(why, "rejected federation envelope");
    let mut inbox = e.state().inbox.lock().await;
    if now() - inbox.last_reject_log >= REJECT_LOG_EVERY {
        inbox.last_reject_log = now();
        drop(inbox);
        e.log_bot(&format!(
            "{} **Ignored a partner message that didn't check out:** {}. If this keeps happening, check the partner's keys in federation.toml.",
            fmt::ATTENTION,
            plain(why, 300)
        ))
        .await;
    }
}

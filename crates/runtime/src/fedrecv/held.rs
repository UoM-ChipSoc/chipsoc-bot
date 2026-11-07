//! The mass-change guards, and updates held for an admin to allow.

use super::*;

/// A partner may take its grants from at most this share of members a day.
pub(super) const MAX_REVOKE_SHARE: f64 = 0.5;
/// Updates held for an admin per partner; more are dropped (their next full sync carries them).
pub(super) const MAX_HELD: usize = 20;
/// Ask about a partner's newly held updates at most this often (seconds).
pub(super) const ASK_EVERY: i64 = 600;
/// Remind about a held batch nobody has decided on this often (seconds).
pub(super) const REMIND_EVERY: i64 = 24 * 3600;

/// An update held for an admin.
#[derive(Debug, Clone)]
pub(super) struct Held {
    pub(super) update: Update,
    /// The batch an admin was asked about, once asked.
    pub(super) batch: Option<String>,
}

/// A partner granting to this many members within the window is more likely
/// a bug on their side (or a stolen key) than reality: `recent` members gained
/// a role from them in the window (counting this update), on top of
/// `established` granted before it.
pub(super) fn too_many_grants(recent: usize, established: usize) -> bool {
    recent > 5 && 2 * recent > established
}

/// Whether an update giving roles to `gaining` members must wait for an
/// admin, given `recent` and `established` from [`chipsoc_store::Fed::grant_window`].
pub(super) fn must_hold(gaining: usize, recent: usize, established: usize) -> bool {
    gaining > 0 && too_many_grants(recent + gaining, established)
}

/// Taking grants from most of a partner's members within a day is more
/// likely a partner-side bug than reality.
pub(super) fn too_many_revokes(losing: usize, granted: usize) -> bool {
    losing > 3 && losing as f64 > granted as f64 * MAX_REVOKE_SHARE
}

/// Whether an update taking grants from `losing` members must wait for an
/// admin, given the members the partner took grants from in the window
/// (`recent`) and those still holding one (`granted`).
pub(super) fn must_hold_revokes(losing: usize, recent: usize, granted: usize) -> bool {
    losing > 0 && too_many_revokes(losing + recent, granted + recent)
}

/// How many members an update would give a partner role they don't have
/// from this partner yet, and how many it would take one from.
pub(super) async fn changes<E: Env>(e: &E, peer: &Peer, targets: &[RoleMap], u: &Update) -> Result<(usize, usize)> {
    // One read for the lot, however many members it lists.
    let grants = db(e).peer_grants(&peer.id).await?;
    let mut granted: HashMap<&str, BTreeSet<String>> = HashMap::new();
    for g in &grants {
        granted.entry(g.user_id.as_str()).or_default().insert(g.role_key.clone());
    }
    let (mut gaining, mut losing) = (0, 0);
    let none = BTreeSet::new();
    for m in &u.members {
        let want = wanted(targets, m);
        let has = granted.get(m.id.as_str()).unwrap_or(&none);
        gaining += usize::from(want.iter().any(|k| !has.contains(k)));
        losing += usize::from(has.iter().any(|k| !want.contains(k)));
    }
    if u.full {
        let vouched: HashSet<&str> = u.members.iter().map(|m| m.id.as_str()).collect();
        losing += granted.keys().filter(|g| !vouched.contains(*g)).count();
    }
    Ok((gaining, losing))
}

/// Keep an update for an admin to allow, and ask them (see [`ask`]).
/// `batch`: it was held before a restart, in this batch.
pub(super) async fn hold<E: Env>(e: &E, peer: &Peer, u: Update, batch: Option<String>) {
    let msgs = u.msgs.clone();
    let kept = {
        let mut inbox = e.state().inbox.lock().await;
        let inbox = &mut *inbox;
        let held = inbox.held.entry(peer.id.clone()).or_default();
        if held.iter().any(|h| h.update.msgs == u.msgs) {
            true
        } else if held.len() >= MAX_HELD {
            false
        } else {
            if let Some(b) = &batch {
                inbox.batches.entry(b.clone()).or_insert_with(now);
            }
            held.push(Held { update: u, batch });
            true
        }
    };
    if !kept {
        tracing::warn!(peer = peer.name, "too many partner updates held; dropped one");
        let text = format!(
            "⚠️ {MAX_HELD} updates from {} are already waiting for an admin, so I dropped a newer one. Their next full sync after you decide carries it.",
            peer.name
        );
        log_about(e, peer, &text).await;
        return mark(e, &msgs, Outcome::Done).await;
    }
    mark(e, &msgs, Outcome::Held).await;
    ask(e, peer).await;
}

/// A held batch's id, from the messages it's made of.
pub(super) fn batch_id(peer: &str, msgs: &[u64]) -> String {
    let list: Vec<String> = msgs.iter().map(u64::to_string).collect();
    chipsoc_model::stable_hash(format!("{peer}:{}", list.join(",")))
}

/// Ask an admin about a partner's held updates nobody has been asked about
/// yet, bundled, at most every [`ASK_EVERY`]; and remind them about batches
/// still waiting after a day. Each question's "Allow" applies just the
/// updates it describes.
pub(super) async fn ask<E: Env>(e: &E, peer: &Peer) {
    let now = now();
    let (new, remind) = {
        let mut inbox = e.state().inbox.lock().await;
        let inbox = &mut *inbox;
        let Some(held) = inbox.held.get_mut(&peer.id) else { return };
        let mut new = None;
        if now - inbox.asked.get(&peer.id).copied().unwrap_or(0) >= ASK_EVERY {
            let unasked: Vec<&mut Held> = held.iter_mut().filter(|h| h.batch.is_none()).collect();
            if !unasked.is_empty() {
                let mut msgs: Vec<u64> = unasked.iter().flat_map(|h| h.update.msgs.iter().copied()).collect();
                msgs.sort_unstable();
                let id = batch_id(&peer.id, &msgs);
                let updates: Vec<Update> = unasked
                    .into_iter()
                    .map(|h| {
                        h.batch = Some(id.clone());
                        h.update.clone()
                    })
                    .collect();
                inbox.asked.insert(peer.id.clone(), now);
                inbox.batches.insert(id.clone(), now);
                new = Some((id, updates, msgs));
            }
        }
        let mut remind = Vec::new();
        let ids: BTreeSet<String> = held.iter().filter_map(|h| h.batch.clone()).collect();
        for id in ids {
            let at = inbox.batches.entry(id.clone()).or_insert(now);
            if now - *at >= REMIND_EVERY {
                *at = now;
                let updates: Vec<Update> =
                    held.iter().filter(|h| h.batch.as_ref() == Some(&id)).map(|h| h.update.clone()).collect();
                remind.push((id, updates));
            }
        }
        (new, remind)
    };
    if let Some((id, updates, msgs)) = new {
        let fed = db(e);
        for m in msgs {
            warn_err(fed.inbox_set_batch(m, Some(&id)).await, "naming a held batch");
        }
        question(e, peer, &id, &updates, false).await;
    }
    for (id, updates) in remind {
        question(e, peer, &id, &updates, true).await;
    }
}

pub(super) async fn question<E: Env>(e: &E, peer: &Peer, batch: &str, updates: &[Update], again: bool) {
    let targets = grantable(&e.cfg(), peer);
    let (mut gaining, mut losing, mut listed) = (0, 0, 0);
    for u in updates {
        if let Ok((g, l)) = changes(e, peer, &targets, u).await {
            gaining += g;
            losing += l;
        }
        listed += u.members.len();
    }
    let text = held_note(&peer.name, gaining, losing, updates.len(), listed, again);
    let button = CustomId::AllowSync { peer: peer.id.clone(), batch: batch.to_owned() }.to_string();
    e.ask(&text, &button).await;
}

/// "Allow" on a held sync: applies the batch the question was about, if it's
/// still held.
pub async fn allow(ctx: &Ctx, i: &Interaction, peer: &str, batch: &str) {
    if !ctx.is_admin(i.member.as_ref()) {
        return ctx.respond(i, &ui::ephemeral(&crate::manage::admins_only(&ctx.cfg()))).await;
    }
    let Some(user) = invoker(i) else { return crate::apps::nobody(ctx, i).await };
    let n = {
        let inbox = ctx.fed.inbox.lock().await;
        let held = inbox.held.get(peer).map(Vec::as_slice).unwrap_or_default();
        held.iter().filter(|h| !batch.is_empty() && h.batch.as_deref() == Some(batch)).count()
    };
    if n == 0 {
        let text =
            "That's no longer held: it was allowed already, or replaced. Anything held since has its own question.";
        return ctx.respond(i, &ui::ephemeral(text)).await;
    }
    ctx.fed.control.push(|t| {
        let item = (peer.to_owned(), batch.to_owned());
        if !t.release.contains(&item) {
            t.release.push(item);
        }
    });
    ctx.audit(
        Some(&user.id.to_string()),
        chipsoc_admin::AuditAction::AllowPartnerSync,
        Some(peer),
        Some(&format!("{batch}: {n} held")),
    )
    .await;
    let original = i.message.as_ref().map(|m| m.content.as_str()).unwrap_or_default();
    let note = format!("\n{} **Allowed by <@{}>** ({}).", fmt::DONE, user.id, fmt::count(n, "update", "updates"));
    ctx.respond(i, &ui::update(&fmt::with_note(original, &note))).await;
}

/// "Refuse" on a held sync: drops the batch the question was about, if it's
/// still held. The partner isn't told; the audit log keeps who refused it.
pub async fn refuse_button(ctx: &Ctx, i: &Interaction, peer: &str, batch: &str) {
    if !ctx.is_admin(i.member.as_ref()) {
        return ctx.respond(i, &ui::ephemeral(&crate::manage::admins_only(&ctx.cfg()))).await;
    }
    let Some(user) = invoker(i) else { return crate::apps::nobody(ctx, i).await };
    let n = refuse(ctx, peer, batch).await;
    if n == 0 {
        let text = "That's no longer held: it was allowed or refused already, or replaced. Anything held since has its own question.";
        return ctx.respond(i, &ui::ephemeral(text)).await;
    }
    ctx.audit(
        Some(&user.id.to_string()),
        chipsoc_admin::AuditAction::RefusePartnerSync,
        Some(peer),
        Some(&format!("{batch}: {n} held")),
    )
    .await;
    let original = i.message.as_ref().map(|m| m.content.as_str()).unwrap_or_default();
    let note = format!(
        "\n{} **Refused by <@{}>** ({}): dropped, and the partner isn't told. Their next full sync is checked afresh.",
        fmt::FAILED,
        user.id,
        fmt::count(n, "update", "updates")
    );
    ctx.respond(i, &ui::update(&fmt::with_note(original, &note))).await;
}

/// Drop one held batch from a partner: nothing in it is applied, and it
/// isn't asked about again. Says how many updates it had (0: not held).
pub(super) async fn refuse<E: Env>(e: &E, peer_id: &str, batch: &str) -> usize {
    let dropped: Vec<Held> = {
        let mut inbox = e.state().inbox.lock().await;
        let Some(list) = inbox.held.get_mut(peer_id) else { return 0 };
        let (go, stay): (Vec<Held>, Vec<Held>) =
            std::mem::take(list).into_iter().partition(|h| !batch.is_empty() && h.batch.as_deref() == Some(batch));
        *list = stay;
        if !go.is_empty() {
            inbox.batches.remove(batch);
        }
        go
    };
    for h in &dropped {
        mark(e, &h.update.msgs, Outcome::Done).await;
    }
    dropped.len()
}

/// Apply one held batch from a partner, then count what it has granted so
/// far as established, so the guard starts afresh.
pub(super) async fn release<E: Env>(e: &E, peer_id: &str, batch: &str) {
    let held: Vec<Update> = {
        let mut inbox = e.state().inbox.lock().await;
        inbox.batches.remove(batch);
        let Some(list) = inbox.held.get_mut(peer_id) else { return };
        let (go, stay): (Vec<Held>, Vec<Held>) =
            std::mem::take(list).into_iter().partition(|h| h.batch.as_deref() == Some(batch));
        *list = stay;
        go.into_iter().map(|h| h.update).collect()
    };
    if held.is_empty() {
        return;
    }
    let cfg = e.cfg();
    let Some(peer) = cfg.federation.peers.iter().find(|p| p.id == peer_id) else {
        for u in held {
            mark(e, &u.msgs, Outcome::Done).await;
        }
        return;
    };
    for u in held {
        settle(e, peer, u, Gate::Allowed).await;
    }
    warn_err(db(e).reset_grant_window(peer_id, now()).await, "resetting the partner grant window");
}

/// The question under a held sync, with its Allow button.
pub(super) fn held_note(
    peer: &str,
    gaining: usize,
    losing: usize,
    updates: usize,
    listed: usize,
    again: bool,
) -> String {
    let head = if again {
        format!("⏳ Still waiting: **{peer}'s role sync is held for an admin.**")
    } else {
        format!("{} **{peer}'s role sync is held for an admin.**", fmt::ATTENTION)
    };
    format!(
        "{head}\n\
         • It would give partner roles to {gaining} more member(s) and take them from {losing} ({}, {listed} member(s) listed).\n\
         • That's more than usual in a day, so it waits in case it's a mistake on their side.\n\
         Check with {peer}, then press **Allow** to apply just these, or **Refuse** to drop them ({peer} isn't told). Anything held later asks again.",
        fmt::count(updates, "update", "updates"),
    )
}

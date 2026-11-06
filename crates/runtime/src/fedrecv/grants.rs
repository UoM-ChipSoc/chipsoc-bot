//! Giving and taking back the roles a partner's word gives, one member at a time.

use chipsoc_admin::{LivePowers, Withheld};

use super::*;

/// What a role sync did to one member, for its note in the log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Logged {
    /// `back`: given back after they left and rejoined.
    Gave {
        role: String,
        user: u64,
        back: bool,
    },
    Took {
        role: String,
        user: u64,
    },
    NotGiven {
        user: u64,
        why: Withheld,
    },
}

/// What a role sync did (see [`sync_note`]), and the roles' live
/// permissions, asked of Discord once, when the first grant needs them.
#[derive(Debug, Default)]
pub(super) struct Log {
    pub(super) lines: Vec<Logged>,
    powers: Option<LivePowers>,
}

impl Log {
    fn push(&mut self, l: Logged) {
        self.lines.push(l);
    }

    /// Why role `key` mustn't be given now, if it mustn't: it has moderator
    /// or admin permissions in Discord (see [`chipsoc_admin::powerful_now`]).
    async fn withheld<E: Env>(&mut self, e: &E, key: &str) -> Option<Withheld> {
        if self.powers.is_none() {
            // Not kept when it fails: the next grant asks again.
            self.powers = LivePowers::fetch(e).await.map_err(|err| tracing::warn!(?err)).ok();
        }
        let cfg = e.cfg();
        let powerful = self.powers.as_ref().map(|p| p.powerful(&cfg, key)).context("couldn't read the roles");
        Withheld::from_check(&cfg, key, &powerful)
    }
}

/// Give a member who left and came back the roles partners still vouch for.
/// Their grants are kept while they're away, so the partner's next update
/// matches them and changes nothing; without this they'd never get the roles
/// back. Only partners still in federation.toml, through role_map entries
/// still there, and never a role members.toml manages.
pub(super) async fn rejoined<E: Env>(e: &E, user: u64) {
    let cfg = e.cfg();
    let fed = db(e);
    let id = user.to_string();
    let mut roles = None;
    for peer in cfg.federation.peers.iter().filter(|p| !p.id.is_empty()) {
        let grants = match fed.grants(&peer.id, &id).await {
            Ok(g) if g.is_empty() => continue,
            Ok(g) => g,
            Err(err) => return tracing::warn!(?err, "reading partner grants failed"),
        };
        if roles.is_none() {
            roles = match member_roles(e, user).await {
                Ok(Some(r)) => Some(r),
                Ok(None) => return, // gone again
                Err(err) => return tracing::warn!(?err, "reading a member's roles failed"),
            };
        }
        let have = roles.as_deref().unwrap_or_default();
        let targets = grantable(&cfg, peer);
        let mut log = Log::default();
        for key in grants {
            if !targets.iter().any(|t| t.ours == key) || cfg.desired.holders.contains_key(&key) {
                continue;
            }
            let Some(role) = cfg.desired.lock.role(&key).map(|r| r.0).filter(|r| !have.contains(r)) else { continue };
            if let Some(why) = log.withheld(e, &key).await {
                log.push(Logged::NotGiven { user, why });
                continue;
            }
            match e.rest().call(Method::Put, &role_path(e, user, role), None).await {
                Ok(_) => log.push(Logged::Gave { role: role_name(&cfg, &key), user, back: true }),
                Err(err) => tracing::warn!(?err, "giving back a partner role failed"),
            }
        }
        post_log(e, peer, &log).await;
    }
}

/// The partner's roles we map to ours, minus any too powerful to grant.
pub(super) fn grantable(cfg: &Config, peer: &Peer) -> Vec<RoleMap> {
    let admin = cfg.bot.admin_role.as_deref();
    peer.role_map.iter().filter(|m| !m.too_powerful(&cfg.desired, admin)).cloned().collect()
}

pub(super) fn wanted(targets: &[RoleMap], m: &Member) -> BTreeSet<String> {
    targets.iter().filter(|t| m.roles.contains(&t.theirs)).map(|t| t.ours.clone()).collect()
}

pub(super) fn role_name(cfg: &Config, key: &str) -> String {
    cfg.desired.role(key).map_or_else(|| key.to_owned(), |r| r.name.clone())
}

/// A Discord user id, as partners must send them.
pub(super) fn valid_id(id: &str) -> bool {
    id.parse::<Id<UserMarker>>().is_ok()
}

/// Apply an update, unless the partner has given or taken roles from too
/// many members lately and no admin has allowed it.
pub(super) async fn settle<E: Env>(e: &E, peer: &Peer, mut u: Update, gate: Gate) {
    let fed = db(e);
    let before = u.members.len();
    u.members.retain(|m| valid_id(&m.id));
    if u.members.len() < before {
        tracing::warn!(peer = peer.name, dropped = before - u.members.len(), "partner sent bad member ids");
    }
    let record = match fed.peer(&peer.id).await {
        Ok(r) => r.unwrap_or_default(),
        Err(err) => {
            tracing::warn!(?err, "reading partner state failed; will retry");
            return mark(e, &u.msgs, Outcome::Retry).await;
        }
    };
    if u.full && record.snapshot_seq >= u.seq {
        tracing::info!(peer = peer.name, "ignored a full sync older than one already applied");
        return mark(e, &u.msgs, Outcome::Done).await;
    }
    let granted = match fed.granted_users(&peer.id).await {
        Ok(g) => g,
        Err(err) => {
            tracing::warn!(?err, "reading grants failed; will retry");
            return mark(e, &u.msgs, Outcome::Retry).await;
        }
    };
    let targets = grantable(&e.cfg(), peer);
    match gate {
        Gate::Allowed => {}
        Gate::Held(batch) => return hold(e, peer, u, batch).await,
        Gate::Check => {
            let since = record.window_from.max(now() - GRANT_WINDOW);
            let counts = async {
                let (gaining, losing) = changes(e, peer, &targets, &u).await?;
                let (recent, established) = fed.grant_window(&peer.id, since).await?;
                let revoked = fed.revoke_window(&peer.id, since).await?;
                anyhow::Ok((gaining, losing, recent, established, revoked))
            };
            let (gaining, losing, recent, established, revoked) = match counts.await {
                Ok(c) => c,
                Err(err) => {
                    tracing::warn!(?err, "reading grants failed; will retry");
                    return mark(e, &u.msgs, Outcome::Retry).await;
                }
            };
            if must_hold(gaining, recent, established) || must_hold_revokes(losing, revoked, granted.len()) {
                return hold(e, peer, u, None).await;
            }
        }
    }
    let mut log = Log::default();
    for m in &u.members {
        receive(e, peer, &targets, m, u.seq, &u.kid, &mut log).await;
    }
    if u.full {
        let vouched: HashSet<&str> = u.members.iter().map(|m| m.id.as_str()).collect();
        for id in granted.iter().filter(|g| !vouched.contains(g.as_str())) {
            let none = Member { id: id.clone(), roles: Vec::new() };
            receive(e, peer, &targets, &none, u.seq, &u.kid, &mut log).await;
        }
        // Only now: a full sync cut short is applied again when it's read back.
        if let Err(err) = fed.claim_snapshot(&peer.id, u.seq).await {
            tracing::warn!(?err, "recording a full sync failed");
        }
    }
    post_log(e, peer, &log).await;
    mark(e, &u.msgs, Outcome::Done).await;
}

/// One member's attested roles, unless we've applied newer state for them.
/// The seq is claimed first so late envelopes can't undo newer state; if the
/// roles then fail to apply, the update is kept to retry (where a newer
/// update for the same member replaces it).
pub(super) async fn receive<E: Env>(
    e: &E,
    peer: &Peer,
    targets: &[RoleMap],
    m: &Member,
    seq: u64,
    kid: &str,
    log: &mut Log,
) {
    let fed = db(e);
    match fed.claim_seq(&peer.id, &m.id, seq).await {
        Ok(true) => {}
        Ok(false) => return,
        Err(err) => return tracing::warn!(?err, "seq check failed; the partner's next sync retries it"),
    }
    match apply_member(e, peer, targets, m, kid, log).await {
        Ok(()) => warn_err(fed.retry_drop(&peer.id, &m.id).await, "clearing a partner retry"),
        Err(err) => {
            tracing::warn!(?err, peer = peer.name, user = m.id, "partner role update failed; will retry");
            let json = serde_json::to_string(m).unwrap_or_default();
            warn_err(fed.retry_put(&peer.id, &m.id, &json, kid, 1).await, "saving a partner retry");
        }
    }
}

pub(super) async fn retry_pending<E: Env>(e: &E) {
    let fed = db(e);
    let list = match fed.retries().await {
        Ok(l) => l,
        Err(err) => return tracing::warn!(?err, "reading partner retries failed"),
    };
    let cfg = e.cfg();
    for r in list {
        let peer = cfg.federation.peers.iter().find(|p| p.id == r.peer);
        let (Some(peer), Ok(member)) = (peer, serde_json::from_str::<Member>(&r.member)) else {
            warn_err(fed.retry_drop(&r.peer, &r.user_id).await, "clearing a partner retry");
            continue;
        };
        let mut log = Log::default();
        let result = apply_member(e, peer, &grantable(&cfg, peer), &member, &r.kid, &mut log).await;
        post_log(e, peer, &log).await;
        let tries = r.tries.max(0) as u32;
        match result {
            Ok(()) => warn_err(fed.retry_drop(&r.peer, &r.user_id).await, "clearing a partner retry"),
            Err(err) if tries >= MAX_TRIES => {
                tracing::warn!(?err, peer = peer.name, user = r.user_id, "partner role update failed; giving up");
                let msg = format!(
                    "⚠️ Couldn't update <@{}>'s roles from {} after {MAX_TRIES} tries. Their next sync tries again.",
                    r.user_id, peer.name
                );
                log_about(e, peer, &msg).await;
                warn_err(fed.retry_drop(&r.peer, &r.user_id).await, "clearing a partner retry");
            }
            Err(_) => {
                let put = fed.retry_put(&r.peer, &r.user_id, &r.member, &r.kid, tries + 1).await;
                warn_err(put, "saving a partner retry");
            }
        }
    }
}

/// A member's role ids, or None if they're not in the server.
pub(super) async fn member_roles<E: Env>(e: &E, user: u64) -> Result<Option<Vec<u64>>> {
    match e.rest().call(Method::Get, &format!("guilds/{}/members/{user}", e.guild().0), None).await {
        Ok(m) => Ok(Some(chipsoc_discord::wire::ids(&m["roles"]).into_iter().map(|s| s.0).collect())),
        Err(err) if ApiError::status_of(&err) == Some(404) => Ok(None),
        Err(err) => Err(err),
    }
}

pub(super) fn role_path<E: Env>(e: &E, user: u64, role: u64) -> String {
    format!("guilds/{}/members/{user}/roles/{role}", e.guild().0)
}

/// Bring one member's partner-granted roles in line with what the partner
/// attests, on a message signed by `kid`. Safe to repeat after a failure
/// part-way. A grant is recorded before its role is given, so a stop in
/// between leaves a grant to check (see [`reconcile`]), never a partner's
/// role nobody can take back. A role another partner also gave stays when
/// this one takes it back, as does one members.toml now manages.
pub(super) async fn apply_member<E: Env>(
    e: &E,
    peer: &Peer,
    targets: &[RoleMap],
    m: &Member,
    kid: &str,
    log: &mut Log,
) -> Result<()> {
    let Ok(user) = m.id.parse::<u64>() else { return Ok(()) };
    let cfg = e.cfg();
    let fed = db(e);
    let want = wanted(targets, m);
    let granted: BTreeSet<String> = fed.grants(&peer.id, &m.id).await?.into_iter().collect();
    if want == granted {
        if !want.is_empty() {
            fed.set_grant_kid(&peer.id, &m.id, kid).await?;
        }
        return Ok(());
    }
    let Some(roles) = member_roles(e, user).await? else {
        // They've left: no roles to manage, so just forget the grants.
        for key in &granted {
            fed.revoke(&peer.id, &m.id, key).await?;
        }
        return Ok(());
    };
    for key in want.difference(&granted) {
        let Some(role) = cfg.desired.lock.role(key).map(|r| r.0) else { continue };
        if roles.contains(&role) {
            // They had it already: not ours to manage, unless another partner gave it.
            if fed.granted_elsewhere(&peer.id, &m.id, key).await? {
                fed.grant(&peer.id, &m.id, key, kid, now()).await?;
            }
            continue;
        }
        // `check` vets role_map, but the role may have been changed in Discord since.
        if let Some(why) = log.withheld(e, key).await {
            log.push(Logged::NotGiven { user, why });
            continue;
        }
        fed.grant_pending(&peer.id, &m.id, key, kid, now()).await?;
        if let Err(err) = e.rest().call(Method::Put, &role_path(e, user, role), None).await {
            warn_err(fed.revoke(&peer.id, &m.id, key).await, "forgetting a failed partner grant");
            return Err(err);
        }
        fed.confirm_grant(&peer.id, &m.id, key).await?;
        log.push(Logged::Gave { role: role_name(&cfg, key), user, back: false });
    }
    let mut took = false;
    for key in granted.difference(&want) {
        let managed = cfg.desired.holders.contains_key(key);
        if !managed
            && !fed.granted_elsewhere(&peer.id, &m.id, key).await?
            && let Some(role) = cfg.desired.lock.role(key).map(|r| r.0).filter(|r| roles.contains(r))
        {
            e.rest().call(Method::Delete, &role_path(e, user, role), None).await?;
            log.push(Logged::Took { role: role_name(&cfg, key), user });
        }
        fed.revoke(&peer.id, &m.id, key).await?;
        took = true;
    }
    if took {
        fed.record_revoke(&peer.id, &m.id, now()).await?;
    }
    fed.set_grant_kid(&peer.id, &m.id, kid).await?;
    Ok(())
}

/// Settle grants recorded just before giving their role, in case the bot
/// stopped in between: kept if the member has the role, forgotten if not
/// (the partner's update is read back and applied again).
pub(super) async fn reconcile<E: Env>(e: &E) {
    let fed = db(e);
    let pending = match fed.pending_grants().await {
        Ok(p) => p,
        Err(err) => return tracing::warn!(?err, "reading pending partner grants failed"),
    };
    let cfg = e.cfg();
    for g in pending {
        let Ok(user) = g.user_id.parse::<u64>() else { continue };
        let role = cfg.desired.lock.role(&g.role_key).map(|r| r.0);
        let r = match member_roles(e, user).await {
            Ok(Some(roles)) if role.is_some_and(|r| roles.contains(&r)) => {
                fed.confirm_grant(&g.peer, &g.user_id, &g.role_key).await
            }
            Ok(_) => fed.revoke(&g.peer, &g.user_id, &g.role_key).await,
            Err(err) => {
                tracing::warn!(?err, "checking a pending partner grant failed; will try again");
                continue;
            }
        };
        warn_err(r, "settling a pending partner grant");
    }
}

pub(super) async fn post_log<E: Env>(e: &E, peer: &Peer, log: &Log) {
    if !log.lines.is_empty() {
        log_about(e, peer, &sync_note(&peer.name, &log.lines)).await;
    }
}

/// A role sync's log as one note: roles given and taken, each with who, cut short.
pub(super) fn sync_note(peer: &str, log: &[Logged]) -> String {
    let mut gave: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut took: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut other = Vec::new();
    for l in log {
        match l {
            Logged::Gave { role, user, back } => {
                let again = if *back { " (back after rejoining)" } else { "" };
                gave.entry(role.clone()).or_default().push(format!("<@{user}>{again}"));
            }
            Logged::Took { role, user } => took.entry(role.clone()).or_default().push(format!("<@{user}>")),
            Logged::NotGiven { user, why } => other.push(format!("<@{user}>: {why}")),
        }
    }
    let (g, t): (usize, usize) = (gave.values().map(Vec::len).sum(), took.values().map(Vec::len).sum());
    let mut head = Vec::new();
    if g > 0 {
        head.push(format!("{g} given"));
    }
    if t > 0 {
        head.push(format!("{t} taken back"));
    }
    if head.is_empty() {
        head.push(format!("{} not given", other.len()));
    }
    let mut lines = Vec::new();
    lines.extend(gave.iter().map(|(role, who)| format!("Gave **{role}** to {}", fmt::collapse(who, 8))));
    lines.extend(took.iter().map(|(role, who)| format!("Took **{role}** from {}", fmt::collapse(who, 8))));
    lines.extend(other.iter().map(|o| format!("Not given: {o}")));
    let mut out = vec![format!("🔗 **Role sync from {peer}:** {}.", head.join(", "))];
    out.extend(fmt::bullets(&lines, fmt::SHOWN));
    out.join("\n")
}

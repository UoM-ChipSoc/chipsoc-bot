//! Taking back what removed partners, keys and roles gave.

use super::*;

/// What taking back a partner's grants did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Revoked {
    /// Roles taken off members.
    pub removed: usize,
    /// Grants forgotten but the role kept, as another partner also gave it
    /// (or members.toml manages it now).
    pub kept: usize,
    /// Roles Discord wouldn't remove; their grants stay, to try again.
    pub failed: usize,
    pub members: usize,
}

impl std::fmt::Display for Revoked {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} role(s) removed from {} member(s)", self.removed, self.members)?;
        if self.kept > 0 {
            write!(f, ", {} kept as another partner or members.toml also gives them", self.kept)?;
        }
        if self.failed > 0 {
            write!(f, ", {} couldn't be removed and will be tried again", self.failed)?;
        }
        Ok(())
    }
}

/// Which of a partner's grants to take back.
#[derive(Debug, Clone, Copy)]
pub enum Which<'a> {
    /// All of them, and forget the partner's catalogue and seqs too.
    All,
    /// Those given on messages signed by one of these keys, or by an unrecorded one.
    Kids(&'a [String]),
    /// Those of these roles.
    Roles(&'a [String]),
}

/// Take back roles a partner's word gave (see [`Which`]).
pub async fn revoke_grants(
    rest: &impl Rest,
    store: &Store,
    guild: &str,
    desired: &Snapshot,
    peer: &str,
    which: Which<'_>,
) -> Result<Revoked> {
    let fed = store.fed(guild);
    let mut r = Revoked::default();
    let mut members = BTreeSet::new();
    for g in fed.peer_grants(peer).await? {
        let skip = match which {
            Which::All => false,
            Which::Kids(ks) => g.kid.as_ref().is_some_and(|k| !ks.contains(k)),
            Which::Roles(rs) => !rs.contains(&g.role_key),
        };
        if skip {
            continue;
        }
        let user = g.user_id.parse::<u64>().ok();
        if desired.holders.contains_key(&g.role_key) || fed.granted_elsewhere(peer, &g.user_id, &g.role_key).await? {
            r.kept += 1;
        } else if let (Some(user), Some(role)) = (user, desired.lock.role(&g.role_key)) {
            let path = format!("guilds/{guild}/members/{user}/roles/{}", role.0);
            match rest.call(Method::Delete, &path, None).await {
                Ok(_) => r.removed += 1,
                // They've left, or the role is gone.
                Err(e) if ApiError::status_of(&e) == Some(404) => {}
                Err(e) => {
                    tracing::warn!(?e, peer, user, role = g.role_key, "taking back a partner role failed");
                    r.failed += 1;
                    continue;
                }
            }
        }
        members.insert(g.user_id.clone());
        fed.revoke(peer, &g.user_id, &g.role_key).await?;
    }
    r.members = members.len();
    if matches!(which, Which::All) {
        fed.forget_peer(peer).await?;
    }
    Ok(r)
}

/// Each configured partner's valid key ids.
pub(super) fn configured_kids(cfg: &Config) -> HashMap<String, Vec<String>> {
    let keys = peer_keys(&cfg.federation.peers);
    cfg.federation
        .peers
        .iter()
        .filter(|p| !p.id.is_empty())
        .map(|p| (p.id.clone(), keys.get(&p.id).map(|ks| ks.iter().map(proto::kid).collect()).unwrap_or_default()))
        .collect()
}

/// Whether to say how a revocation went: when it did something, or the first
/// time it fails (it's tried again every few minutes).
pub(super) async fn worth_saying<E: Env>(e: &E, what: &str, r: &Revoked) -> bool {
    let mut inbox = e.state().inbox.lock().await;
    if r.failed == 0 {
        inbox.failing.remove(what);
        return r.members > 0;
    }
    inbox.failing.insert(what.to_owned()) || r.members > 0
}

/// Take back what partners no longer in the config gave, what was given on
/// messages signed by keys no longer listed, and grants of roles no longer
/// in a partner's `role_map` or too powerful now. Runs on start, on config
/// changes and every few minutes, so what failed is tried again.
pub(super) async fn check_peers<E: Env>(e: &E) {
    let cfg = e.cfg();
    let fed = db(e);
    let configured = configured_kids(&cfg);
    let known = match fed.peers_with_state().await {
        Ok(p) => p,
        Err(err) => return tracing::warn!(?err, "reading partner state failed"),
    };
    for id in known.iter().filter(|id| !configured.contains_key(id.as_str())) {
        let dropped = e.state().inbox.lock().await.forget(id);
        mark(e, &dropped, Outcome::Done).await;
        // What they shared is archived here and forgotten, so adding them back starts afresh.
        let shared = match chipsoc_admin::shares::forget_partner(e, id).await {
            Ok(n) => n,
            Err(err) => {
                tracing::warn!(?err, peer = id, "archiving what a removed partner shared failed");
                0
            }
        };
        let archived = if shared > 0 {
            format!(", archived the {} they'd shared here", fmt::count(shared, "thing", "things"))
        } else {
            String::new()
        };
        match revoke_grants(e.rest(), e.store(), e.gkey(), &cfg.desired, id, Which::All).await {
            Ok(r) if shared > 0 || worth_saying(e, id, &r).await => {
                let text = format!(
                    "🔒 **`{}` is no longer a partner** in federation.toml, so the bot took back the roles their word gave ({r}){archived} and deleted their shared projects.",
                    plain(id, 100)
                );
                e.log_bot(&text).await;
                e.audit(AuditAction::RevokePartner, id, &r.to_string()).await;
            }
            Ok(_) => {}
            Err(err) => tracing::warn!(?err, peer = id, "taking back a removed partner's roles failed"),
        }
    }
    for (id, kids) in &configured {
        let before = match fed.peer(id).await {
            Ok(r) => r.map(|r| r.kids).unwrap_or_default(),
            Err(err) => {
                tracing::warn!(?err, "reading partner state failed");
                continue;
            }
        };
        let removed: Vec<String> = before.into_iter().filter(|k| !kids.contains(k)).collect();
        if !removed.is_empty() {
            // What's held or half-received may be signed by that key: their next full sync resends it.
            let dropped = e.state().inbox.lock().await.forget(id);
            mark(e, &dropped, Outcome::Done).await;
            let name = cfg.federation.peers.iter().find(|p| &p.id == id).map_or(id.as_str(), |p| p.name.as_str());
            // Seqs that key signed may be from the future: start afresh.
            warn_err(fed.forget_seqs(id).await, "forgetting a partner's seqs");
            match revoke_grants(e.rest(), e.store(), e.gkey(), &cfg.desired, id, Which::Kids(&removed)).await {
                Ok(r) => {
                    if worth_saying(e, &format!("{id}/keys"), &r).await {
                        let text = format!(
                            "🔒 **A key was removed from {name}'s public keys**, so the bot took back the roles given on messages it signed ({r}). Their next full sync gives back what they still vouch for (held for an admin if it's many)."
                        );
                        e.log_bot(&text).await;
                        e.audit(AuditAction::RevokePartnerKey, id, &removed.join(",")).await;
                    }
                    if r.failed > 0 {
                        continue; // keep the old keys on record, so the next check tries again
                    }
                }
                Err(err) => {
                    tracing::warn!(?err, peer = id, "taking back a removed key's roles failed");
                    continue;
                }
            }
        }
        warn_err(fed.set_peer_kids(id, kids).await, "recording partner keys");
    }
    for peer in cfg.federation.peers.iter().filter(|p| !p.id.is_empty()) {
        check_roles(e, &cfg, peer).await;
    }
}

/// Take back a partner's grants of roles it may no longer give: gone from
/// its `role_map`, refused by the config, or with moderator or admin
/// permissions in Discord now.
pub(super) async fn check_roles<E: Env>(e: &E, cfg: &Config, peer: &Peer) {
    let grants = match db(e).peer_grants(&peer.id).await {
        Ok(g) => g,
        Err(err) => return tracing::warn!(?err, "reading partner grants failed"),
    };
    let keys: BTreeSet<String> = grants.into_iter().map(|g| g.role_key).collect();
    let mapped: BTreeSet<String> = grantable(cfg, peer).into_iter().map(|m| m.ours).collect();
    // Asked of Discord once, if any role needs it.
    let mut powers = None;
    for key in keys {
        let name = role_name(cfg, &key);
        let why = if !mapped.contains(&key) {
            format!("{}'s `role_map` no longer gives **{name}** (or the config won't let a partner give it)", peer.name)
        } else {
            if powers.is_none() {
                match chipsoc_admin::LivePowers::fetch(e).await {
                    Ok(p) => powers = Some(p),
                    Err(err) => return tracing::warn!(?err, "checking partner roles' permissions failed"),
                }
            }
            if !powers.as_ref().is_some_and(|p| p.powerful(cfg, &key)) {
                continue;
            }
            format!("**{name}** has moderator or admin permissions in Discord now")
        };
        let only = [key.clone()];
        match revoke_grants(e.rest(), e.store(), e.gkey(), &cfg.desired, &peer.id, Which::Roles(&only)).await {
            Ok(r) if worth_saying(e, &format!("{}/role/{key}", peer.id), &r).await => {
                e.log_bot(&format!("🔒 **Took back roles {}'s word gave:** {why} ({r}).", peer.name)).await;
                e.audit(AuditAction::RevokePartnerRole, &peer.id, &format!("{key}: {r}")).await;
            }
            Ok(_) => {}
            Err(err) => tracing::warn!(?err, peer = peer.name, role = key, "taking back a partner role failed"),
        }
    }
}

/// `fed revoke`: take back every role a partner's word gave (or, with `kid`,
/// those given on messages that key signed) and delete their shared projects.
/// `peer` is their id or name. Works on the bot's database and Discord.
pub async fn revoke_partner(rest: &impl Rest, cfg: &Config, peer: &str, kid: Option<&str>) -> Result<String> {
    let guild = cfg.guild().context("lock.toml has no server id; run apply first")?.0.to_string();
    let id = cfg.federation.peers.iter().find(|p| p.id == peer || p.name == peer).map_or(peer, |p| p.id.as_str());
    let store = Store::from_env().await?;
    let fed = store.fed(&guild);
    if !fed.peers_with_state().await?.iter().any(|p| p == id) {
        bail!("nothing is recorded from a partner `{id}`");
    }
    let kids = kid.map(|k| vec![k.to_owned()]);
    let which = kids.as_deref().map_or(Which::All, Which::Kids);
    let r = revoke_grants(rest, &store, &guild, &cfg.desired, id, which).await?;
    if kid.is_some() {
        // What that key signed may have been from the future.
        fed.forget_seqs(id).await?;
    }
    let what = kid.map_or_else(|| "all".to_owned(), |k| format!("key {k}"));
    store
        .guild(&guild)
        .audit(now(), Some("cli"), AuditAction::RevokePartner.as_str(), Some(id), Some(&format!("{what}: {r}")))
        .await?;
    Ok(format!("Took back what `{id}` gave ({what}): {r}."))
}

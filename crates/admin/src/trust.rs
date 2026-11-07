//! The bot's own copy of what it trusts about a config ([`Trusted`]), kept in
//! its database rather than the repo: see [`chipsoc_config::trust`].

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use chipsoc_config::ops::Renames;
use chipsoc_config::trust::{Sensitive, Trusted, Vetted};
use chipsoc_config::{Config, Lock, Snowflake};
use chipsoc_discord::{Method, Rest, wire};
use chipsoc_engine::{Baseline, BoxFuture};
use chipsoc_store::Store;

const LOCK: &str = "lock";
const APPROVED: &str = "approved";
/// The commit the approved settings were approved at.
const APPROVED_AT: &str = "approved_at";
/// What the last approval card in the bot log was about.
const PROMPTED: &str = "prompted";

/// The key a config's trusted state is kept under: its directory.
pub fn scope(dir: &Path) -> String {
    std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_owned()).display().to_string()
}

/// A [`Baseline`] in the bot's database.
pub struct StoreBaseline {
    store: Store,
    scope: String,
}

impl StoreBaseline {
    pub fn new(store: Store, dir: &Path) -> Arc<Self> {
        Arc::new(Self { store, scope: scope(dir) })
    }
}

impl Baseline for StoreBaseline {
    fn load(&self) -> BoxFuture<'_, Result<Trusted>> {
        Box::pin(load(&self.store, &self.scope))
    }

    fn save_lock<'a>(&'a self, lock: &'a Lock) -> BoxFuture<'a, Result<()>> {
        Box::pin(save_lock(&self.store, &self.scope, lock))
    }

    fn renamed<'a>(
        &'a self,
        guild: Option<Snowflake>,
        from: &'a str,
        to: &'a str,
        keys: &'a Renames,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let Some(g) = guild.map(|g| g.to_string()) else { return Ok(()) };
            let pairs: Vec<(&str, &str)> = keys.iter().map(|(o, n)| (o.as_str(), n.as_str())).collect();
            self.store.guild(&g).rename_project(from, to, &pairs).await?;
            for (old, new) in &pairs {
                self.store.fed(&g).rename_role(old, new).await?;
            }
            Ok(())
        })
    }

    fn audit<'a>(
        &'a self,
        guild: Snowflake,
        by: &'a str,
        action: &'a str,
        detail: &'a str,
    ) -> BoxFuture<'a, Result<Option<i64>>> {
        Box::pin(async move {
            let g = guild.to_string();
            Ok(Some(self.store.guild(&g).audit(crate::now(), Some(by), action, None, Some(detail)).await?))
        })
    }
}

pub async fn load(store: &Store, scope: &str) -> Result<Trusted> {
    let lock = match store.trusted(scope, LOCK).await? {
        Some(b) => Some(serde_json::from_str(&b)?),
        None => None,
    };
    let approved = match store.trusted(scope, APPROVED).await? {
        Some(b) => Some(serde_json::from_str(&b)?),
        None => None,
    };
    Ok(Trusted { lock, approved })
}

pub async fn save_lock(store: &Store, scope: &str, lock: &Lock) -> Result<()> {
    store.set_trusted(scope, LOCK, &serde_json::to_string(lock)?).await
}

/// Approve `s`, as of config commit `at`.
pub async fn approve(store: &Store, scope: &str, s: &Sensitive, at: Option<&str>) -> Result<()> {
    store.set_trusted(scope, APPROVED, &serde_json::to_string(s)?).await?;
    store.set_trusted(scope, APPROVED_AT, at.unwrap_or_default()).await
}

/// The commit the approved settings date from, if known.
pub async fn approved_at(store: &Store, scope: &str) -> Result<Option<String>> {
    Ok(store.trusted(scope, APPROVED_AT).await?.filter(|c| !c.is_empty()))
}

pub async fn prompted(store: &Store, scope: &str) -> Result<Option<String>> {
    store.trusted(scope, PROMPTED).await
}

pub async fn set_prompted(store: &Store, scope: &str, hash: &str) -> Result<()> {
    store.set_trusted(scope, PROMPTED, hash).await
}

/// Make `cfg`, as loaded from `dir`, use what the bot trusts. The first time
/// (nothing kept yet), the files' lock is what's trusted, and their sensitive
/// settings too if who they make admins is who holds those roles in Discord
/// now ([`agrees_with_discord`]). Otherwise (a database lost or not copied
/// over, say, while the repo was changed) they wait for the server owner's
/// Approve in the bot log, and until then the bot uses only the files' logs
/// and web link: no admin role, partners or recording, and drift is only
/// reported.
pub async fn vet<R: Rest>(store: &Store, rest: &R, dir: &Path, cfg: &mut Config) -> Result<Vetted> {
    let scope = scope(dir);
    let mut t = load(store, &scope).await?;
    let lock = &cfg.desired.lock;
    if t.lock.is_none() && (lock.guild.is_some() || lock.known() > 0) {
        save_lock(store, &scope, lock).await?;
        t.lock = Some(lock.clone());
    }
    if t.approved.is_none() {
        let s = Sensitive::of(cfg);
        let agrees = agrees_with_discord(rest, cfg, &s).await.unwrap_or_else(|e| {
            tracing::warn!("checking the config's admins against Discord failed: {e:#}");
            false
        });
        let first = if agrees {
            s
        } else {
            tracing::warn!("first run: the config's admins aren't who Discord says; its settings wait for the owner");
            held_at_first(&s)
        };
        approve(store, &scope, &first, chipsoc_config::git::head_full(dir).as_deref()).await?;
        t.approved = Some(first);
    }
    Ok(t.vet(cfg))
}

/// What a first run uses while the files' settings `s` wait: their logs
/// and web link (so the approval card has somewhere to go), nothing else.
fn held_at_first(s: &Sensitive) -> Sensitive {
    Sensitive {
        web: s.web.clone(),
        log_people: s.log_people.clone(),
        log_bot: s.log_bot.clone(),
        drift: chipsoc_config::bot::DriftMode::Report,
        ..Sensitive::default()
    }
}

/// Whether the admins `s` makes are those Discord has now: each admin role
/// (the admin role and admin positions' roles) is in the lock and in
/// Discord, and whoever members.toml lists for it, of those in the server,
/// is exactly who holds it there.
pub async fn agrees_with_discord<R: Rest>(rest: &R, cfg: &Config, s: &Sensitive) -> Result<bool> {
    let keys: BTreeSet<&String> = s.admin_role.iter().chain(&s.admin_positions).collect();
    if keys.is_empty() {
        return Ok(true);
    }
    let Some(g) = cfg.desired.lock.guild else { return Ok(false) };
    let guild = rest.call(Method::Get, &format!("guilds/{g}"), None).await?;
    let live: BTreeSet<Snowflake> =
        guild["roles"].as_array().into_iter().flatten().filter_map(|r| wire::id(&r["id"])).collect();
    let members = rest.members(g).await?;
    let here: BTreeSet<Snowflake> = members.iter().filter_map(|m| wire::id(&m["user"]["id"])).collect();
    for k in keys {
        let Some(id) = cfg.desired.lock.role(k).filter(|id| live.contains(id)) else { return Ok(false) };
        let Some(listed) = s.holders.get(k) else { continue };
        let want: BTreeSet<Snowflake> = listed.iter().filter(|u| here.contains(u)).copied().collect();
        let have: BTreeSet<Snowflake> = members
            .iter()
            .filter(|m| wire::ids(&m["roles"]).contains(&id))
            .filter_map(|m| wire::id(&m["user"]["id"]))
            .collect();
        if want != have {
            return Ok(false);
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    use chipsoc_discord::fake::FakeDiscord;

    /// A server where member 7 holds the admin role (1001's sibling 1500), and a config saying so.
    fn server(dir: &Path) -> (FakeDiscord, Config) {
        let fake = FakeDiscord::new();
        fake.join(7);
        fake.join(8);
        {
            let mut st = fake.state.lock().unwrap();
            st.roles.push(serde_json::json!({ "id": "1500", "name": "Admin", "position": 1, "permissions": "0" }));
            st.members[0]["roles"] = serde_json::json!(["1500"]);
        }
        let mut cfg = Config { dir: dir.to_owned(), ..Default::default() };
        cfg.bot.admin_role = Some("admin".into());
        cfg.desired.lock.guild = Some(Snowflake(1000));
        cfg.desired.lock.roles.insert("admin".into(), Snowflake(1500));
        cfg.members.insert("admin".into(), vec![Snowflake(7)]);
        (fake, cfg)
    }

    #[tokio::test]
    async fn the_first_run_approves_then_pushes_wait() {
        let store = Store::open("sqlite::memory:").await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        let (fake, cfg) = server(dir.path());
        let first = vet(&store, &fake, dir.path(), &mut cfg.clone()).await.unwrap();
        assert!(first.held.is_none(), "first run, and Discord agrees: approved");

        let mut pushed = cfg.clone();
        pushed.bot.admin_role = Some("member".into());
        pushed.desired.lock.guild = Some(Snowflake(2));
        let v = vet(&store, &fake, dir.path(), &mut pushed).await.unwrap();
        assert!(v.held.is_some());
        assert_eq!(pushed.bot.admin_role.as_deref(), Some("admin"), "the approved admin role is used");
        assert_eq!(pushed.desired.lock.guild, Some(Snowflake(1000)), "and the bot's own lock");

        let new = v.held.unwrap();
        approve(&store, &scope(dir.path()), &new, None).await.unwrap();
        let mut again = cfg.clone();
        again.bot.admin_role = Some("member".into());
        assert!(vet(&store, &fake, dir.path(), &mut again).await.unwrap().held.is_none());
        assert_eq!(again.bot.admin_role.as_deref(), Some("member"));
    }

    /// A lost database while the repo made someone else an admin: nothing is
    /// approved on the spot; it waits for the owner.
    #[tokio::test]
    async fn a_first_run_that_disagrees_with_discord_waits_for_the_owner() {
        let dir = tempfile::tempdir().unwrap();
        let (fake, cfg) = server(dir.path());
        let mut cases = Vec::new();
        let mut more = cfg.clone();
        more.members.insert("admin".into(), vec![Snowflake(7), Snowflake(8)]);
        cases.push(("someone made an admin", more));
        let mut gone = cfg.clone();
        gone.desired.lock.roles.insert("admin".into(), Snowflake(9999));
        cases.push(("an admin role Discord hasn't got", gone));
        let mut unlocked = cfg.clone();
        unlocked.desired.lock.roles.clear();
        cases.push(("an admin role not in the lock", unlocked));
        for (what, files) in cases {
            let store = Store::open("sqlite::memory:").await.unwrap();
            let mut used = files.clone();
            let v = vet(&store, &fake, dir.path(), &mut used).await.unwrap();
            let held = v.held.unwrap_or_else(|| panic!("{what}: held back"));
            assert_eq!(held, Sensitive::of(&files), "{what}");
            assert_eq!(used.bot.admin_role, None, "{what}: no admin role until approved");
            assert_eq!(used.bot.drift.mode, chipsoc_config::bot::DriftMode::Report, "{what}");
            let approved = load(&store, &scope(dir.path())).await.unwrap().approved.unwrap();
            let ask = chipsoc_engine::Ask::Approve { old: &approved, new: &held };
            assert_eq!(chipsoc_engine::owner_only(&ask, Some(Snowflake(1))), Some(chipsoc_engine::OwnerOnly::Trust));
        }
        // Discord not answering is a disagreement too.
        let store = Store::open("sqlite::memory:").await.unwrap();
        fake.fail("GET guilds/1000", 500, false);
        assert!(vet(&store, &fake, dir.path(), &mut cfg.clone()).await.unwrap().held.is_some());
        // No admin roles at all: nothing to disagree about.
        let store = Store::open("sqlite::memory:").await.unwrap();
        let plain = Config { dir: dir.path().to_owned(), ..Default::default() };
        assert!(vet(&store, &fake, dir.path(), &mut plain.clone()).await.unwrap().held.is_none());
    }
}

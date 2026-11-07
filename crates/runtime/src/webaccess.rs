//! Web UI access for admins over Tailscale: when someone becomes an admin
//! (the admin role, Administrator or Manage Server, or owning the server, as
//! everywhere else) they get a DM with a one-time link that shares the web
//! UI's machine with them (the tailnet policy limits sharees to port 443); the
//! share is revoked when they stop being one or leave, and an hourly sweep
//! catches any change the bot missed (while it was down, or a role's
//! permissions changing): it revokes, and offers to admins without a share.
//!
//! Revoking deletes the device invite, the API's only handle on a share.
//! Tailscale's docs say revoking an accepted share's invite (Machines → Share
//! → Revoke) ends the recipient's access; the API reference doesn't say so of
//! DELETE on an accepted invite, so docs/deploy.md asks operators to check the
//! machine's Share list once. Sign-in still checks admin status either way.
//!
//! A missing or refused `TAILSCALE_API_KEY` (Tailscale's keys expire) is said
//! once in the bot log, and so is every share that couldn't be revoked.
//!
//! Every new admin also gets a short welcome DM saying what they can do and
//! where, Tailscale or not (with the link step only when it's set up).

use std::collections::HashSet;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use twilight_model::id::Id;
use twilight_model::id::marker::{RoleMarker, UserMarker};

use chipsoc_admin::{Admins, fmt};
use chipsoc_config::Snowflake;

use crate::{Ctx, now};

const API: &str = "https://api.tailscale.com/api/v2";

fn key() -> Option<String> {
    std::env::var("TAILSCALE_API_KEY").ok().filter(|k| !k.is_empty())
}

const NO_KEY: &str = "TAILSCALE_API_KEY isn't set on the bot's server";
const KEY_REFUSED: &str = "Tailscale refused TAILSCALE_API_KEY (it has probably expired)";

/// A Tailscale reply as an error when it failed, saying so when it was the key.
fn checked(r: reqwest::Response) -> Result<reqwest::Response> {
    match r.status() {
        s if s.is_success() => Ok(r),
        s => Err(Refusal(s).into()),
    }
}

/// Tailscale answered with an error status.
#[derive(Debug)]
struct Refusal(reqwest::StatusCode);

impl Refusal {
    /// Whether it was the API key it refused.
    fn of_key(&self) -> bool {
        matches!(self.0, reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN)
    }

    /// Whether `e` is (or was caused by) Tailscale refusing the key.
    fn key_in(e: &anyhow::Error) -> bool {
        e.chain().any(|c| c.downcast_ref::<Self>().is_some_and(Self::of_key))
    }
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.of_key() {
            write!(f, "{KEY_REFUSED}: Tailscale said {}", self.0)
        } else {
            write!(f, "Tailscale said {}", self.0)
        }
    }
}

impl std::error::Error for Refusal {}

/// Servers (by guild id) whose bot log has heard the key is missing or refused.
static TOLD: std::sync::Mutex<Vec<(u64, &'static str)>> = std::sync::Mutex::new(Vec::new());

/// Whether to tell this server's bot log about `problem` now: once per run.
fn first_time(guild: u64, problem: &'static str) -> bool {
    let mut told = TOLD.lock().unwrap_or_else(|e| e.into_inner());
    if told.contains(&(guild, problem)) {
        return false;
    }
    told.push((guild, problem));
    true
}

/// Say once in the bot log that the key is missing or refused, if `e` (or its absence) says so.
async fn about_the_key(ctx: &Ctx, e: Option<&anyhow::Error>) {
    let (problem, fix) = match e {
        None => (NO_KEY, "Admins won't get dashboard links and shares can't be revoked until it's set."),
        Some(e) if Refusal::key_in(e) => (
            KEY_REFUSED,
            "Make a new API key in the Tailscale admin console, put it in the bot's .env and restart the bot; \
             until then shares can't be given or revoked.",
        ),
        Some(_) => return,
    };
    if first_time(ctx.guild.get(), problem) {
        ctx.log_bot(&format!("⚠️ Web access over Tailscale: {problem}. {fix}")).await;
    }
}

/// One client for every call, sharing its connections and TLS setup.
fn client() -> Result<reqwest::Client> {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    if let Some(c) = CLIENT.get() {
        return Ok(c.clone());
    }
    let c = reqwest::Client::builder().timeout(Duration::from_secs(20)).build()?;
    Ok(CLIENT.get_or_init(|| c).clone())
}

/// The machine to share, by its Tailscale name, if this is set up.
fn device_name(ctx: &Ctx) -> Option<String> {
    ctx.cfg().bot.web.as_ref()?.share_device.clone()
}

async fn device_id(key: &str, name: &str) -> Result<String> {
    let v: Value = client()?
        .get(format!("{API}/tailnet/-/devices"))
        .basic_auth(key, Some(""))
        .send()
        .await
        .map(checked)??
        .json()
        .await?;
    v["devices"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|d| d["hostname"] == name || d["name"].as_str().is_some_and(|n| n.split('.').next() == Some(name)))
        .and_then(|d| d["id"].as_str().map(str::to_owned))
        .with_context(|| format!("no Tailscale machine called `{name}`"))
}

/// A one-time invite to the machine: (invite id, link).
async fn create_invite(key: &str, device: &str) -> Result<(String, String)> {
    let body = json!([{ "multiUse": false, "allowExitNode": false }]);
    let v: Value = client()?
        .post(format!("{API}/device/{device}/device-invites"))
        .basic_auth(key, Some(""))
        .json(&body)
        .send()
        .await
        .map(checked)??
        .json()
        .await?;
    let invite = v.as_array().and_then(|a| a.first()).context("Tailscale returned no invite")?;
    match (invite["id"].as_str(), invite["inviteUrl"].as_str()) {
        (Some(id), Some(url)) => Ok((id.to_owned(), url.to_owned())),
        _ => bail!("Tailscale's invite had no link"),
    }
}

async fn delete_invite(key: &str, invite: &str) -> Result<()> {
    let r = client()?.delete(format!("{API}/device-invites/{invite}")).basic_auth(key, Some("")).send().await?;
    if r.status().is_success() || r.status() == reqwest::StatusCode::NOT_FOUND {
        Ok(())
    } else {
        Err(Refusal(r.status()).into())
    }
}

/// How often every share is checked against who the admins are.
const SWEEP_EVERY: Duration = Duration::from_secs(3600);

/// [`Admins`] as last read from Discord, until something it depends on
/// changes (a role, the server's owner, the config's admin role), so a
/// member update needn't ask Discord again.
#[derive(Default)]
pub struct AdminCache(std::sync::Mutex<(u64, Option<Admins>)>);

impl AdminCache {
    fn lock(&self) -> std::sync::MutexGuard<'_, (u64, Option<Admins>)> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Something they depend on changed: read them again next time.
    pub fn forget(&self) {
        let mut c = self.lock();
        *c = (c.0.wrapping_add(1), None);
    }

    /// The admins kept, else what `fetch` reads (kept unless forgotten meanwhile).
    async fn get_or(&self, fetch: impl Future<Output = Result<Admins>>) -> Result<Admins> {
        let generation = {
            let c = self.lock();
            if let Some(a) = &c.1 {
                return Ok(a.clone());
            }
            c.0
        };
        let a = fetch.await?;
        let mut c = self.lock();
        // Forgotten while reading: what was read may be from before the change.
        if c.0 == generation {
            c.1 = Some(a.clone());
        }
        Ok(a)
    }
}

/// Who the admins are, by the one rule shared with sign-in and `/server`.
async fn admins(ctx: &Ctx) -> Result<Admins> {
    let cfg = ctx.cfg();
    let admin_role = cfg.bot.admin_role.as_deref().and_then(|k| cfg.desired.lock.role(k));
    ctx.admins.get_or(Admins::fetch(&ctx.rest, Snowflake(ctx.guild.get()), admin_role)).await
}

fn ids(roles: &[Id<RoleMarker>]) -> Vec<Snowflake> {
    roles.iter().map(|r| Snowflake(r.get())).collect()
}

/// Who has had the welcome DM (in the store's `prompted` list).
const WELCOMED: &str = "admin-welcome";

/// Welcome new admins, and give or take back web access to match whether they're one.
pub async fn on_roles(ctx: &Ctx, user: Id<UserMarker>, roles: &[Id<RoleMarker>], bot: bool) {
    if bot {
        return;
    }
    let shared = ctx.db().web_share(&user.to_string()).await.ok().flatten();
    let welcomed = ctx.db().prompted_at(&user.to_string(), WELCOMED).await.ok().flatten().is_some();
    if key().is_none() && (shared.is_some() || device_name(ctx).is_some()) {
        about_the_key(ctx, None).await;
    }
    let admins = match admins(ctx).await {
        Ok(a) => a,
        Err(e) => return tracing::warn!("reading roles for web access failed: {e:#}"),
    };
    let admin = admins.include(Snowflake(user.get()), &ids(roles));
    if !admin && welcomed {
        // Welcomed again if they're made an admin again.
        ctx.log_err(ctx.db().forget_prompt(&user.to_string(), WELCOMED).await, "forgetting a welcome");
    }
    let result = match (admin, shared) {
        (true, None) if !welcomed => {
            welcome(ctx, user).await;
            Ok(())
        }
        (true, None) if device_name(ctx).is_some() && key().is_some() => offer(ctx, user).await,
        (false, Some(invite)) => revoke(ctx, user, &invite).await,
        _ => Ok(()),
    };
    if let Err(e) = result {
        about_the_key(ctx, Some(&e)).await;
        tracing::warn!("web access for {user} failed: {e:#}");
        ctx.log_bot(&format!("⚠️ **Web access for <@{user}> didn't work:** {}", fmt::error(&e))).await;
    }
}

/// The welcome DM for a new admin: what they can do and where. `link` is a
/// one-time Tailscale link to the dashboard's machine, when that's set up.
pub fn welcome_text(cfg: &chipsoc_config::Config, log: Option<String>, link: Option<&str>) -> String {
    let name = if cfg.bot.name.is_empty() { "the server" } else { cfg.bot.name.as_str() };
    let log = log.unwrap_or_else(|| "the bot's log channel".to_owned());
    let mut out = vec![
        format!("👋 **You're an admin of {name} now.** Here's what you can do, and where:"),
        format!("• {log}: plans to **Apply**, changes to **Approve**, drift and problems."),
    ];
    if let Some(r) = &cfg.bot.review {
        let review =
            cfg.desired.lock.channel(&r.channel).map_or_else(|| format!("#{}", r.channel), |c| format!("<#{c}>"));
        out.push(format!("• {review}: applications and suggestions; press **Accept** or **Reject** on each."));
    }
    out.push(
        "• `/server help`: everything admins can do from Discord: appoint people, open positions, review applications, apply the config.".to_owned(),
    );
    if cfg.bot.recording.is_some() {
        out.push(
            "• `/record start` / `stop`: record a meeting in a call (everyone in it is told); recordings are on the dashboard's Recordings page.".to_owned(),
        );
    }
    if let Some(w) = &cfg.bot.web {
        out.push(format!("• Dashboard: {} (sign in with Discord).", w.url));
    }
    out.push(
        match cfg.bot.drift.mode {
            chipsoc_config::bot::DriftMode::Undo => {
                "Changes made only in Discord are put back; make them through these instead."
            }
            chipsoc_config::bot::DriftMode::Report => {
                "Changes made only in Discord show up in the bot log as drift; make them through these instead."
            }
        }
        .to_owned(),
    );
    if let (Some(link), Some(w)) = (link, &cfg.bot.web) {
        out.push(format!(
            "\n**To open the dashboard from your device:**\n\
             1. Install Tailscale and sign in (free): https://tailscale.com/download\n\
             2. Open this one-time link to add the dashboard: {link}\n\
             3. Then visit {} and sign in with Discord.",
            w.url
        ));
    }
    out.join("\n")
}

/// DM a new admin [`welcome_text`] (with a Tailscale link if that's set up),
/// once: they're remembered as welcomed even if the DM couldn't be sent.
async fn welcome(ctx: &Ctx, user: Id<UserMarker>) {
    match ctx.db().first_prompt(&user.to_string(), WELCOMED, now()).await {
        Ok(true) => {}
        Ok(false) => return,
        Err(e) => return tracing::warn!("recording a welcome failed: {e:#}"),
    }
    let link = match (device_name(ctx), key()) {
        (Some(_), Some(_)) => match invite(ctx, user).await {
            Ok(link) => Some(link),
            Err(e) => {
                about_the_key(ctx, Some(&e)).await;
                tracing::warn!("making a dashboard link failed: {e:#}");
                ctx.log_bot(&format!("⚠️ **Couldn't make <@{user}> a dashboard link:** {}", fmt::error(&e))).await;
                None
            }
        },
        _ => None,
    };
    let log = ctx.bot_log().map(|c| format!("<#{c}>"));
    match ctx.dm(user, &welcome_text(&ctx.cfg(), log, link.as_deref())).await {
        Ok(()) => {
            ctx.log_bot(&format!(
                "👋 Sent <@{user}>, a new admin, a welcome DM{}.",
                if link.is_some() { " with a dashboard link" } else { "" }
            ))
            .await
        }
        Err(why) => {
            ctx.log_bot(&format!(
                "⚠️ **Couldn't send <@{user}>, a new admin, a welcome DM:** {why}. `/server help` tells them the same."
            ))
            .await
        }
    }
}

/// Every hour: revoke the share of anyone no longer an admin (or no longer here).
pub async fn sweep_loop(ctx: Arc<Ctx>) {
    let mut tick = tokio::time::interval(SWEEP_EVERY);
    loop {
        tick.tick().await;
        if key().is_none() && device_name(&ctx).is_some() {
            about_the_key(&ctx, None).await;
        }
        if let Err(e) = sweep(&ctx).await {
            tracing::warn!("checking web access failed: {e:#}");
            about_the_key(&ctx, Some(&e)).await;
        }
    }
}

async fn sweep(ctx: &Ctx) -> Result<()> {
    let shares = ctx.db().web_shares().await?;
    // The first time (as this version starts), today's admins count as welcomed already.
    let seed = ctx.db().prompted(WELCOMED).await?.is_empty();
    let offering = device_name(ctx).is_some() && key().is_some();
    // Hourly, whatever is set up: new admins by a role's permissions are welcomed here.
    // Read afresh, in case an event was missed.
    ctx.admins.forget();
    let admins = admins(ctx).await?;
    let members = ctx.members().await?;
    let still: HashSet<String> = members
        .iter()
        .filter(|m| !crate::members::is_bot(m))
        .filter_map(|m| Some((crate::members::user(m)?, crate::members::roles(m))))
        .filter(|(user, roles)| admins.include(Snowflake(*user), &ids(roles)))
        .map(|(user, _)| user.to_string())
        .collect();
    if seed {
        for u in still.iter().chain(shares.iter().map(|(u, _)| u)) {
            ctx.log_err(ctx.db().first_prompt(u, WELCOMED, now()).await, "recording a welcome");
        }
    } else {
        // Admins by a role's permissions get no member event: welcome them here.
        let welcomed = ctx.db().prompted(WELCOMED).await?;
        let new: Vec<&String> = still.iter().filter(|u| !welcomed.contains(*u)).collect();
        if sends_offers(new.len()) {
            for u in new {
                if let Ok(id) = u.parse::<Id<UserMarker>>() {
                    welcome(ctx, id).await;
                }
            }
        }
    }
    if offering {
        let shares = ctx.db().web_shares().await?;
        let shared: HashSet<&str> = shares.iter().map(|(u, _)| u.as_str()).collect();
        offer_missing(ctx, still.iter().filter(|u| !shared.contains(u.as_str())).collect()).await;
    }
    let (mut failed, mut why) = (Vec::new(), None);
    for (user, invite) in shares.into_iter().filter(|(u, _)| !still.contains(u)) {
        let Ok(id) = user.parse::<Id<UserMarker>>() else { continue };
        if let Err(e) = revoke(ctx, id, &invite).await {
            about_the_key(ctx, Some(&e)).await;
            tracing::warn!("revoking web access for {user} failed: {e:#}");
            failed.push(format!("<@{user}>"));
            why = Some(fmt::error(&e));
        }
    }
    // One message a sweep (every hour) while shares stay that should go.
    if let Some(why) = why {
        ctx.log_bot(&format!(
            "⚠️ **Couldn't revoke web access for {}**, who aren't admins any more: {why}",
            fmt::collapse(&failed, 10)
        ))
        .await;
    }
    Ok(())
}

/// Most links one sweep sends: more new admins at once means a role (or
/// @everyone) was given Administrator or Manage Server by mistake.
const MAX_OFFERS: usize = 5;

/// Offer the dashboard to admins who don't have it: those who became admins
/// without a member event, such as through a role's permissions changing
/// (the same rule as `/server` and sign-in).
async fn offer_missing(ctx: &Ctx, mut users: Vec<&String>) {
    users.sort();
    if !sends_offers(users.len()) {
        if first_time(ctx.guild.get(), TOO_MANY) {
            ctx.log_bot(&format!(
                "⚠️ **Web access over Tailscale: {} people are admins (by role permissions) without dashboard access**, \
                 so the bot didn't send that many links. Check which roles have Administrator or Manage Server; \
                 `/server web` sends one person a link.",
                users.len()
            ))
            .await;
        }
        return;
    }
    for user in users {
        let Ok(id) = user.parse::<Id<UserMarker>>() else { continue };
        if let Err(e) = offer(ctx, id).await {
            // Likely the same for the rest (the key, the machine): tried again next sweep.
            tracing::warn!("offering web access failed: {e:#}");
            about_the_key(ctx, Some(&e)).await;
            break;
        }
    }
}

const TOO_MANY: &str = "too many admins without access";

/// Whether a sweep should send links to `n` admins without one, or ask a person to look.
fn sends_offers(n: usize) -> bool {
    n <= MAX_OFFERS
}

/// They left the server: revoke any share.
pub async fn on_leave(ctx: &Ctx, user: Id<UserMarker>) {
    if let Ok(Some(invite)) = ctx.db().web_share(&user.to_string()).await
        && let Err(e) = revoke(ctx, user, &invite).await
    {
        about_the_key(ctx, Some(&e)).await;
        tracing::warn!("revoking web access for {user} failed: {e:#}");
        ctx.log_bot(&format!("⚠️ **Couldn't revoke web access for <@{user}>:** {}", fmt::error(&e))).await;
    }
}

/// A fresh one-time link to the dashboard's machine (replacing any earlier one), recorded.
async fn invite(ctx: &Ctx, user: Id<UserMarker>) -> Result<String> {
    let key = key().context(NO_KEY)?;
    let name = device_name(ctx).context("set [web] share_device in bot.toml")?;
    if let Ok(Some(old)) = ctx.db().web_share(&user.to_string()).await {
        delete_invite(&key, &old).await?;
    }
    let device = device_id(&key, &name).await?;
    let (invite, link) = create_invite(&key, &device).await?;
    ctx.db().set_web_share(&user.to_string(), Some(&invite), now()).await?;
    Ok(link)
}

/// Send a fresh one-time link by DM (replacing any earlier one).
pub async fn offer(ctx: &Ctx, user: Id<UserMarker>) -> Result<()> {
    if key().is_none() || device_name(ctx).is_none() {
        bail!(
            "the dashboard link isn't set up on this server (no Tailscale sharing); ask the maintainer (docs/deploy.md: Web dashboard over Tailscale)"
        );
    }
    let link = invite(ctx, user).await?;
    let url = ctx.cfg().bot.web.as_ref().map(|w| w.url.clone()).unwrap_or_default();
    let text = format!(
        "🔒 **Your link to the {} dashboard:**\n\
         1. Install Tailscale and sign in (free): https://tailscale.com/download\n\
         2. Open this one-time link to add the dashboard: {link}\n\
         3. Then visit {url} and sign in with Discord.",
        ctx.name(),
    );
    ctx.dm(user, &text).await.map_err(|e| anyhow::anyhow!("couldn't DM you: {e}"))?;
    ctx.log_bot(&format!("🔒 Sent <@{user}> a link to the web dashboard.")).await;
    Ok(())
}

async fn revoke(ctx: &Ctx, user: Id<UserMarker>, invite: &str) -> Result<()> {
    let key = key().context(NO_KEY)?;
    delete_invite(&key, invite).await?;
    ctx.db().set_web_share(&user.to_string(), None, now()).await?;
    ctx.log_bot(&format!("🔒 Removed <@{user}>'s access to the web dashboard (not an admin any more).")).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use reqwest::StatusCode;

    use super::*;

    #[test]
    fn the_welcome_says_what_and_where_with_tailscale_only_when_set_up() {
        let mut cfg = chipsoc_config::Config::default();
        cfg.bot.name = "ChipSoc".into();
        let plain = welcome_text(&cfg, Some("<#5>".into()), None);
        assert_eq!(
            plain,
            "👋 **You're an admin of ChipSoc now.** Here's what you can do, and where:
• <#5>: plans to **Apply**, changes to **Approve**, drift and problems.
• `/server help`: everything admins can do from Discord: appoint people, open positions, review applications, apply the config.
Changes made only in Discord show up in the bot log as drift; make them through these instead."
        );
        cfg.bot.drift.mode = chipsoc_config::bot::DriftMode::Undo;
        assert!(
            welcome_text(&cfg, None, None)
                .ends_with("Changes made only in Discord are put back; make them through these instead.")
        );
        cfg.bot.review = Some(chipsoc_config::bot::Review { channel: "review".into() });
        let text = welcome_text(&cfg, Some("<#5>".into()), None);
        assert!(
            text.contains("• #review: applications and suggestions; press **Accept** or **Reject** on each."),
            "{text}"
        );
        cfg.bot.web =
            Some(chipsoc_config::bot::Web { url: "https://dash.example".into(), share_device: Some("vps".into()) });
        let text = welcome_text(&cfg, Some("<#5>".into()), Some("https://login.tailscale.com/x"));
        assert!(text.contains("• Dashboard: https://dash.example (sign in with Discord)."), "{text}");
        assert!(
            text.contains("2. Open this one-time link to add the dashboard: https://login.tailscale.com/x"),
            "{text}"
        );
        assert!(!welcome_text(&cfg, None, None).contains("Tailscale"), "no link step without a link");
        assert!(!text.contains("/record"), "no recording line while recording is off");
        cfg.bot.recording = Some(toml::from_str("channel = \"committee\"").unwrap());
        assert!(welcome_text(&cfg, None, None).contains("• `/record start` / `stop`: record a meeting"));
    }

    #[tokio::test]
    async fn the_admins_are_read_again_only_after_a_change() {
        let cache = AdminCache::default();
        let reads = std::sync::atomic::AtomicUsize::new(0);
        let read = || async {
            reads.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Ok(Admins::default())
        };
        cache.get_or(read()).await.unwrap();
        cache.get_or(read()).await.unwrap();
        assert_eq!(reads.load(std::sync::atomic::Ordering::Relaxed), 1, "kept");
        cache.forget();
        cache.get_or(read()).await.unwrap();
        assert_eq!(reads.load(std::sync::atomic::Ordering::Relaxed), 2, "read again after a change");
        // A change while reading: what was read isn't kept.
        cache.forget();
        cache
            .get_or(async {
                cache.forget();
                Ok(Admins::default())
            })
            .await
            .unwrap();
        assert!(cache.lock().1.is_none());
    }

    #[test]
    fn a_refused_key_says_so_once() {
        assert!(Refusal(StatusCode::UNAUTHORIZED).to_string().starts_with(KEY_REFUSED));
        assert!(Refusal(StatusCode::FORBIDDEN).to_string().starts_with(KEY_REFUSED));
        assert!(!Refusal(StatusCode::INTERNAL_SERVER_ERROR).to_string().contains("TAILSCALE_API_KEY"));
        let wrapped = anyhow::Error::from(Refusal(StatusCode::UNAUTHORIZED)).context("sharing the machine");
        assert!(Refusal::key_in(&wrapped), "found under context");
        assert!(!Refusal::key_in(&anyhow::anyhow!("{KEY_REFUSED}: but only in words")));
        assert!(first_time(7, KEY_REFUSED));
        assert!(!first_time(7, KEY_REFUSED), "said once");
        assert!(first_time(7, NO_KEY), "another problem is said too");
        assert!(first_time(8, KEY_REFUSED), "and on another server");
    }

    #[test]
    fn a_sweep_offers_a_few_new_admins_not_everyone() {
        assert!(sends_offers(1) && sends_offers(MAX_OFFERS));
        assert!(!sends_offers(MAX_OFFERS + 1), "@everyone with Manage Server would DM the whole server");
    }
}

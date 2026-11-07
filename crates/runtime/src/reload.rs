//! Reload a server's config soon after its files change (a pull, a deploy),
//! not only on the drift watch's 20-minute tick. The one place a changed
//! config is picked up.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chipsoc_admin::fmt;
use chipsoc_config::Config;

use crate::{Ctx, commands, control, fedrecv, fedsync};

const EVERY: Duration = Duration::from_secs(30);
/// How often approval cards are checked for (see [`crate::trust::review`]).
const REVIEW_EVERY: Duration = crate::gitsync::EVERY;

pub use chipsoc_config::fingerprint;

/// Every config change (a merged commit, an edit from the web UI or
/// `/server`, a deploy) is reloaded, and its plan posted in the bot log if
/// there's anything to apply. Commits the git sync just `pulled` are looked
/// at straight away. Every [`REVIEW_EVERY`], an approval card that couldn't be
/// posted (or was deleted) goes up again, whatever the config comes from.
pub async fn watch(ctx: Arc<Ctx>, dir: PathBuf, pulled: Arc<tokio::sync::Notify>) {
    let mut seen = changed(&dir).await;
    // Plan posts from before now were closed at start (see `control::close_stale_posts`).
    let mut applied = ctx.db().last_audit_id().await.unwrap_or_default();
    let mut tick = tokio::time::interval(EVERY);
    // The cards were looked at as the server started.
    let mut reviewed = Instant::now();
    // New commits not looked at yet: announced even if only lock.toml changed.
    let mut pull = false;
    loop {
        tokio::select! {
            _ = tick.tick() => {}
            () = pulled.notified() => pull = true,
        }
        // Applied by the web UI or the CLI: the bot log's plans are moot.
        control::close_applied(&ctx, &mut applied).await;
        // Mid-apply, lock.toml changes step by step; read it once the apply is done.
        if applying(&ctx, &dir) {
            continue;
        }
        if reviewed.elapsed() >= REVIEW_EVERY {
            crate::trust::review(&ctx, &dir).await;
            reviewed = Instant::now();
        }
        let now = changed(&dir).await;
        if now.0 != seen.0 && reload(&ctx, &dir).await {
            reviewed = Instant::now();
            if now.1 != seen.1 || pull {
                let intro = crate::gitsync::headline(&last_commit(&dir).await.unwrap_or_default());
                // Busy applying: try again on the next tick.
                if !control::announce(&ctx, &intro).await {
                    continue;
                }
            }
        }
        (seen, pull) = (now, false);
    }
}

/// Whether the bot, or another program on this config (the web UI, the
/// CLI), is changing things now.
pub fn applying(ctx: &Ctx, dir: &Path) -> bool {
    ctx.changing.try_lock().is_err() || chipsoc_engine::Lease::busy(dir)
}

/// Fingerprints of every file, and of the ones people edit (not lock.toml,
/// which every apply writes).
async fn changed(dir: &Path) -> (u64, u64) {
    let dir = dir.to_owned();
    let both = move || Ok((fingerprint(&dir), chipsoc_config::edits_fingerprint(&dir)));
    chipsoc_config::blocking(both).await.unwrap_or_default()
}

/// The newest commit, as "abc1234 its message", if the config is a git checkout.
pub(crate) async fn last_commit(dir: &Path) -> Option<String> {
    let dir = dir.to_owned();
    chipsoc_config::blocking(move || Ok(chipsoc_engine::git::last_commit(&dir))).await.ok().flatten()
}

/// Load the config again and use it. False (and said in the bot log) if it doesn't load.
pub async fn reload(ctx: &Ctx, dir: &Path) -> bool {
    let path = dir.to_owned();
    let mut cfg = match chipsoc_config::blocking(move || Config::load(&path)).await {
        Ok(c) => c,
        Err(e) => {
            ctx.log_bot(&doesnt_load(&e)).await;
            return false;
        }
    };
    // Sensitive settings and lock.toml ids are the trusted ones until an admin approves (see `trust`).
    if let Err(e) = chipsoc_admin::trust::vet(&ctx.store, &ctx.rest, dir, &mut cfg).await {
        tracing::warn!("reading the trusted settings failed: {e:#}");
        ctx.log_bot(&format!(
            "{} **The config changed, but the bot couldn't read what it trusts**, so it keeps the old one: {}",
            fmt::ATTENTION,
            fmt::error(&e)
        ))
        .await;
        return false;
    }
    if cfg.guild().map(|g| g.0) != Some(ctx.guild.get()) {
        ctx.log_bot(&format!(
            "{} **The config now names a different server**, so the bot keeps the old one. Restart the bot to switch servers.",
            fmt::ATTENTION
        ))
        .await;
        return false;
    }
    let problems = problems(&cfg);
    if !problems.is_empty() {
        ctx.log_bot(&has_problems(&problems)).await;
        return false;
    }
    use_checked(ctx, Arc::new(cfg)).await;
    crate::trust::review(ctx, dir).await;
    tracing::info!(guild = %ctx.guild, "config reloaded");
    true
}

/// The bot log's note when the config doesn't load. What's wrong with the
/// files is the point here, so it's shown as written, in a code block.
pub fn doesnt_load(e: &anyhow::Error) -> String {
    let why = fmt::truncate(&format!("{e:#}").replace("```", "'''"), 1200);
    format!(
        "{} **The config doesn't load**, so the bot keeps using the last one that did.\n```\n{why}\n```\nFix it in the \
         config repo; the bot picks the fix up within 2 minutes.",
        fmt::ATTENTION
    )
}

/// What `check` (and so apply) finds wrong with `cfg`. A config with any is
/// never used: anyone who can push could otherwise get round what the checks
/// refuse (such as a join flow handing out the admin role).
pub fn problems(cfg: &Config) -> Vec<String> {
    chipsoc_config::authoring::check_config(cfg).errors
}

/// The bot log's note when the config loads but has [`problems`], shown as
/// written (like [`doesnt_load`]'s), in a code block.
pub fn has_problems(problems: &[String]) -> String {
    let list: Vec<String> = problems.iter().map(|p| format!("- {p}")).collect();
    let why = fmt::truncate(&list.join("\n").replace("```", "'''"), 1200);
    format!(
        "{} **The config has {}**, so the bot keeps using the last one that didn't.\n```\n{why}\n```\nFix {} in the \
         config repo (`check` lists them too); the bot picks the fix up within 2 minutes.",
        fmt::ATTENTION,
        fmt::count(problems.len(), "problem", "problems"),
        if problems.len() == 1 { "it" } else { "them" },
    )
}

/// Start using `cfg`: re-register the commands if they changed, send
/// partners the project catalogue if that changed, and take back what removed
/// partners or keys gave. A config with [`problems`] isn't used (false).
pub async fn use_config(ctx: &Ctx, cfg: Arc<Config>) -> bool {
    let problems = problems(&cfg);
    if !problems.is_empty() {
        tracing::warn!(guild = %ctx.guild, "not using a config with problems: {}", problems.join("; "));
        return false;
    }
    use_checked(ctx, cfg).await;
    true
}

/// [`use_config`] for one already found to have no [`problems`].
async fn use_checked(ctx: &Ctx, cfg: Arc<Config>) {
    let (commands, catalogue) = (commands::commands(ctx), fedsync::catalogue_hash(ctx));
    ctx.set_cfg(cfg);
    if commands::commands(ctx) != commands {
        commands::register(ctx).await;
    }
    if fedsync::catalogue_hash(ctx) != catalogue {
        fedsync::send_new_catalogue(ctx).await;
    }
    fedrecv::peers_changed(ctx);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_follows_file_changes_but_not_dot_files() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir(d.path().join("categories")).unwrap();
        std::fs::write(d.path().join("bot.toml"), "a").unwrap();
        let first = fingerprint(d.path());
        assert_eq!(fingerprint(d.path()), first, "stable");
        std::fs::write(d.path().join(".bot.toml.XyZ"), "partial").unwrap();
        assert_eq!(fingerprint(d.path()), first, "temporary files ignored");
        std::fs::write(d.path().join("categories/10-a.toml"), "x").unwrap();
        let second = fingerprint(d.path());
        assert_ne!(second, first, "new file in a subdirectory");
        std::fs::write(d.path().join("bot.toml"), "ab").unwrap();
        assert_ne!(fingerprint(d.path()), second, "edited file");
    }
}

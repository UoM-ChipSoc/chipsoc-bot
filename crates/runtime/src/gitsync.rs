//! A config that is a git checkout: pull every 2 minutes, and when new commits
//! arrive, use them and post what they'd change, with an Apply button (the
//! reload watcher does the same for every other change to the files). Commits
//! made here that didn't get pushed are rebased and pushed (a clash in
//! `lock.toml` is merged). GitHub refuses now and then for a moment, so a
//! failure is said in the bot log only after [`SAY_AFTER`] in a row (about 6
//! minutes), then every hour while it lasts, and once when it syncs again.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use chipsoc_config::git;

use chipsoc_admin::fmt;

use crate::alarm::Alarm;
use crate::{Ctx, control, reload};

pub const EVERY: Duration = Duration::from_secs(120);
/// Failed syncs in a row before the bot log hears of it; fewer are hiccups.
pub const SAY_AFTER: u32 = 3;

pub async fn watch(ctx: Arc<Ctx>, dir: PathBuf) {
    let mut tick = tokio::time::interval(EVERY);
    let mut alarm = Alarm::after(SAY_AFTER);
    loop {
        tick.tick().await;
        match pull(&ctx, &dir).await {
            Ok(head) => {
                if alarm.ok() {
                    ctx.log_bot(&format!("{} **The config syncs with its repo again.**", fmt::DONE)).await;
                }
                if let Some(head) = head
                    && reload::reload(&ctx, &dir).await
                {
                    // Busy: the reload watcher posts it once the files settle.
                    let commit = reload::last_commit(&dir).await;
                    control::announce(&ctx, &headline(commit.as_deref().unwrap_or(&head))).await;
                }
            }
            Err(e) => {
                tracing::warn!("syncing the config failed: {e:#}");
                if alarm.failed(Instant::now()) {
                    ctx.log_bot(&stuck(&e)).await;
                }
            }
        }
    }
}

/// The heading of a config change's plan in the bot log, from "abc1234 its
/// subject": "📥 **Config changed: its subject** (abc1234)".
pub fn headline(commit: &str) -> String {
    match commit.split_once(' ') {
        Some((hash, subject)) => format!("{} **Config changed: {subject}** ({hash})", fmt::NEW_CONFIG),
        None if commit.is_empty() => format!("{} **Config changed**", fmt::NEW_CONFIG),
        None => format!("{} **Config changed** ({commit})", fmt::NEW_CONFIG),
    }
}

/// What the bot log says while the config can't sync.
pub fn stuck(e: &anyhow::Error) -> String {
    format!(
        "{} **The config can't sync with its repo.**\n\
         • Why: {}\n\
         • Meanwhile the bot keeps the config it has: new commits aren't used, and changes made here wait to be pushed.\n\
         • This repeats every hour until it syncs again.",
        fmt::ATTENTION,
        fmt::error(e)
    )
}

/// Sync, as one of the bot's changes. The new commit, if any arrived.
async fn pull(ctx: &Ctx, dir: &Path) -> Result<Option<String>> {
    let _one = ctx.changing.lock().await;
    let dir = dir.to_owned();
    git::blocking(move || sync(&dir)).await
}

/// Pull (and, where the bot saves its changes, push). The new commit, if any
/// arrived. Skipped while an apply runs (here or in the web UI): it catches up
/// itself, and lock.toml mustn't change under it.
pub fn sync(dir: &Path) -> Result<Option<String>> {
    if !git::is_synced(dir) {
        return Ok(None);
    }
    let Ok(_no_apply) = chipsoc_engine::Lease::take(dir) else { return Ok(None) };
    if !git::sync(dir, chipsoc_engine::autosave_on())? {
        return Ok(None);
    }
    git::head(dir).map(Some)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    #[test]
    fn one_heading_for_a_config_change() {
        assert_eq!(super::headline("abc1234 Open Treasurer"), "📥 **Config changed: Open Treasurer** (abc1234)");
        assert_eq!(super::headline(""), "📥 **Config changed**");
    }
    use std::process::Command;

    fn git(dir: &Path, args: &[&str]) {
        let out = Command::new("git").arg("-C").arg(dir).args(args).output().unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    }

    /// While an apply holds the lease, the two-minute sync leaves the
    /// checkout alone (lock.toml mustn't change under it), and catches up after.
    #[test]
    fn sync_waits_for_an_apply() {
        let root = tempfile::tempdir().unwrap();
        let (origin, bot, laptop) =
            (root.path().join("origin.git"), root.path().join("bot"), root.path().join("laptop"));
        git(root.path(), &["init", "-q", "--bare", "-b", "main", origin.to_str().unwrap()]);
        for c in [&bot, &laptop] {
            git(root.path(), &["clone", "-q", origin.to_str().unwrap(), c.to_str().unwrap()]);
            git(c, &["config", "user.email", "t@t"]);
            git(c, &["config", "user.name", "t"]);
        }
        std::fs::write(laptop.join("server.toml"), "[guild]\nname = \"T\"\n").unwrap();
        git(&laptop, &["add", "."]);
        git(&laptop, &["commit", "-qm", "config"]);
        git(&laptop, &["push", "-q", "-u", "origin", "HEAD"]);

        let lease = chipsoc_engine::Lease::take(&bot).unwrap();
        assert_eq!(super::sync(&bot).unwrap(), None, "skipped during an apply");
        assert!(!bot.join("server.toml").exists());
        drop(lease);
        assert!(super::sync(&bot).unwrap().is_some(), "caught up after");
        assert!(bot.join("server.toml").exists());
    }

    #[test]
    fn the_alert_says_why_in_plain_words() {
        let e = anyhow::anyhow!("git@github.com: Permission denied (publickey).").context("git fetch origin failed");
        let text = super::stuck(&e);
        assert!(
            text.starts_with(
                "⚠️ **The config can't sync with its repo.**\n• Why: GitHub turned the bot's deploy key away"
            ),
            "{text}"
        );
        assert!(!text.contains("git fetch"), "no error chain: {text}");
    }
}

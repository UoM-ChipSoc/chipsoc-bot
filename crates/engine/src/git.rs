//! The config directory as a git checkout: pull what others pushed, and
//! commit and push what this machine changed (lockfile, web edits).
//!
//! Every write to a config directory (by the bot, the web UI, the CLI) holds
//! its [`WriteLock`], so a commit never picks up half of someone else's edit
//! and a pull never runs under a write. Only config files are ever committed.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use chipsoc_config::files::take_written;
use chipsoc_config::{LOCK_FILE, WriteLock, blocking};

/// How long a fetch or push may take before it's given up.
const NETWORK: Duration = Duration::from_secs(30);
/// A fetch this recent is good enough for checking whether we're behind.
const FRESH: Duration = Duration::from_secs(30);

/// Whether git may see an environment variable: what it needs to run and
/// reach the remote, never the bot's secrets (tokens, signing keys, webhooks).
fn passed_to_git(name: &str) -> bool {
    const KEEP: &[&str] = &[
        "PATH",
        "HOME",
        "USER",
        "LOGNAME",
        "LANG",
        "LC_ALL",
        "TZ",
        "TMPDIR",
        "SSH_AUTH_SOCK",
        "SSL_CERT_FILE",
        "SSL_CERT_DIR",
    ];
    KEEP.contains(&name)
        || name.starts_with("GIT_")
        || name.starts_with("XDG_")
        || name.starts_with("LC_")
        || name.to_ascii_lowercase().ends_with("_proxy")
}

fn command(dir: &Path, args: &[&str]) -> Command {
    let mut c = Command::new("git");
    c.env_clear().envs(std::env::vars_os().filter(|(k, _)| k.to_str().is_some_and(passed_to_git)));
    // Never wait for a password or an editor: there's nobody to type one.
    c.arg("-C").arg(dir).args(args).env("GIT_TERMINAL_PROMPT", "0").env("GIT_EDITOR", "true");
    c.stdin(Stdio::null());
    c
}

fn checked(args: &[&str], out: Output) -> Result<String> {
    if !out.status.success() {
        bail!("git {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

fn git(dir: &Path, args: &[&str]) -> Result<String> {
    checked(args, command(dir, args).output().context("running git")?)
}

fn succeeds(dir: &Path, args: &[&str]) -> bool {
    command(dir, args).stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success())
}

/// [`git`] for commands that talk to the remote: killed if it hangs.
fn git_net(dir: &Path, args: &[&str]) -> Result<String> {
    let mut child = command(dir, args).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().context("running git")?;
    let start = Instant::now();
    while child.try_wait()?.is_none() {
        if start.elapsed() > NETWORK {
            let _ = child.kill();
            let _ = child.wait();
            bail!("git {}: no answer from the remote after {} s", args.join(" "), NETWORK.as_secs());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    checked(args, child.wait_with_output()?)
}

/// [`sync`] off the async runtime's threads.
pub async fn sync_async(dir: &Path, push: bool) -> Result<bool> {
    let w = WriteLock::take_async(dir).await?;
    let dir = dir.to_owned();
    blocking(move || {
        let _w = w;
        sync_locked(&dir, push)
    })
    .await
}

/// [`save_locked`] off the async runtime's threads, for a caller holding the [`WriteLock`].
pub async fn save_locked_async(dir: &Path, message: &str, _held: &WriteLock) -> Result<bool> {
    let (dir, message) = (dir.to_owned(), message.to_owned());
    blocking(move || save_locked(&dir, &message)).await
}

/// Whether `dir` is a git checkout with a remote to sync with.
pub fn is_synced(dir: &Path) -> bool {
    dir.join(".git").exists() && git(dir, &["remote"]).is_ok_and(|r| !r.is_empty())
}

/// The current commit, short.
pub fn head(dir: &Path) -> Result<String> {
    git(dir, &["rev-parse", "--short", "HEAD"])
}

/// The newest commit as "abc1234 its message", if `dir` is a git checkout with one.
pub fn last_commit(dir: &Path) -> Option<String> {
    if !dir.join(".git").exists() {
        return None;
    }
    git(dir, &["log", "-1", "--format=%h %s"]).ok().filter(|s| !s.is_empty())
}

/// The current commit, in full, if `dir` is a git checkout with one.
pub fn head_full(dir: &Path) -> Option<String> {
    if !dir.join(".git").exists() {
        return None;
    }
    git(dir, &["rev-parse", "HEAD"]).ok()
}

/// Whether the checkout's HEAD contains `commit` (false if it's unknown here).
pub fn contains(dir: &Path, commit: &str) -> bool {
    // Only an id ever reaches git: anything else could be read as an option.
    commit.bytes().all(|b| b.is_ascii_hexdigit())
        && !commit.is_empty()
        && succeeds(dir, &["merge-base", "--is-ancestor", commit, "HEAD"])
}

fn upstream(dir: &Path) -> Option<String> {
    git(dir, &["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{upstream}"]).ok()
}

/// Commits (ahead, behind) the upstream.
fn counts(dir: &Path) -> Result<(usize, usize)> {
    if !succeeds(dir, &["rev-parse", "--verify", "-q", "HEAD"]) {
        // A fresh clone of what was an empty repository.
        return Ok((0, git(dir, &["rev-list", "--count", "@{upstream}"])?.parse().unwrap_or(0)));
    }
    let out = git(dir, &["rev-list", "--left-right", "--count", "HEAD...@{upstream}"])?;
    let mut n = out.split_whitespace().map(|x| x.parse::<usize>().unwrap_or(0));
    Ok((n.next().unwrap_or(0), n.next().unwrap_or(0)))
}

fn last_fetch() -> &'static Mutex<HashMap<PathBuf, Instant>> {
    static LAST: std::sync::OnceLock<Mutex<HashMap<PathBuf, Instant>>> = std::sync::OnceLock::new();
    LAST.get_or_init(Default::default)
}

fn fetch(dir: &Path) -> Result<()> {
    git_net(dir, &["fetch", "--quiet"])?;
    last_fetch().lock().unwrap_or_else(|e| e.into_inner()).insert(dir.to_owned(), Instant::now());
    Ok(())
}

/// Fetch, unless that was done moments ago.
fn fetch_recent(dir: &Path) -> Result<()> {
    let at = last_fetch().lock().unwrap_or_else(|e| e.into_inner()).get(dir).copied();
    if at.is_some_and(|t| t.elapsed() < FRESH) { Ok(()) } else { fetch(dir) }
}

/// Where a checkout stands against its remote, for deciding whether to plan from it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Upstream {
    /// Not a git checkout, no remote, or no upstream branch: nothing to compare.
    None,
    /// Has everything the remote has.
    Current,
    /// The remote branch (named) has this many commits the checkout doesn't.
    Behind(usize, String),
    /// The remote couldn't be reached (offline?): the reason.
    Unknown(String),
}

/// Fetch (if not done in the last half minute) and compare with the upstream branch.
pub fn upstream_state(dir: &Path) -> Upstream {
    if !is_synced(dir) {
        return Upstream::None;
    }
    let Some(up) = upstream(dir) else { return Upstream::None };
    if let Err(e) = fetch_recent(dir) {
        return Upstream::Unknown(format!("{e:#}"));
    }
    match counts(dir) {
        Ok((_, 0)) => Upstream::Current,
        Ok((_, n)) => Upstream::Behind(n, up),
        Err(e) => Upstream::Unknown(format!("{e:#}")),
    }
}

/// Fetch and fast-forward. Returns true if new commits arrived.
pub fn pull(dir: &Path) -> Result<bool> {
    let _w = WriteLock::take(dir)?;
    sync_locked(dir, false)
}

/// Bring the checkout up to date with its remote, never leaving it mid-rebase:
/// a rebase or merge left stuck (by a crash, or an older version) is aborted
/// first. With `push`, commits made here are rebased onto the remote's (a
/// clash in `lock.toml` is merged key by key) and pushed. Returns true if
/// commits arrived from the remote.
pub fn sync(dir: &Path, push: bool) -> Result<bool> {
    let _w = WriteLock::take(dir)?;
    sync_locked(dir, push)
}

/// [`sync`] for a caller holding the [`WriteLock`].
pub fn sync_locked(dir: &Path, push: bool) -> Result<bool> {
    if recover(dir)? {
        tracing::warn!(dir = %dir.display(), "the config checkout was stuck mid-rebase or merge; aborted it");
    }
    fetch(dir)?;
    let Some(up) = upstream(dir) else {
        if push && succeeds(dir, &["rev-parse", "--verify", "-q", "HEAD"]) {
            git_net(dir, &["push", "--quiet", "--set-upstream", "origin", "HEAD"])?; // first push
        }
        return Ok(false);
    };
    let (ahead, behind) = counts(dir)?;
    if behind > 0 {
        if ahead == 0 {
            git(dir, &["merge", "--ff-only", "--quiet", "@{upstream}"])?;
        } else if push {
            rebase_merging_lock(dir)?;
        } else {
            bail!(
                "{} has {ahead} commit(s) of its own and {up} has {behind} it doesn't; \
                 merge them by hand (git pull --rebase)",
                dir.display()
            );
        }
    }
    if push && counts(dir)?.0 > 0 {
        git_net(dir, &["push", "--quiet"])?;
    }
    Ok(behind > 0)
}

/// Abort a rebase or merge left in progress. True if there was one.
pub fn recover(dir: &Path) -> Result<bool> {
    if !dir.join(".git").exists() {
        return Ok(false);
    }
    let path = |p: &str| git(dir, &["rev-parse", "--git-path", p]).map(|r| dir.join(r));
    if path("rebase-merge")?.exists() || path("rebase-apply")?.exists() {
        git(dir, &["rebase", "--abort"])?;
        return Ok(true);
    }
    if path("MERGE_HEAD")?.exists() {
        git(dir, &["merge", "--abort"])?;
        return Ok(true);
    }
    if path("REVERT_HEAD")?.exists() || path("sequencer")?.exists() {
        // A revert of the bot's own, cut short: it changed nothing pushed.
        let _ = git(dir, &["revert", "--abort"]);
        let _ = git(dir, &["revert", "--quit"]);
        return Ok(true);
    }
    Ok(false)
}

/// Rebase this checkout's commits onto its upstream. Where both changed
/// `lock.toml`, the two are merged key by key ([`chipsoc_config::merge_lock`]); a clash
/// in any other file aborts the rebase, leaving the checkout as it was.
fn rebase_merging_lock(dir: &Path) -> Result<()> {
    let mut step = git(dir, &["rebase", "--quiet", "@{upstream}"]);
    for _ in 0..1000 {
        let Err(e) = step else { return Ok(()) };
        let conflicted = git(dir, &["diff", "--name-only", "--diff-filter=U"]).unwrap_or_default();
        let conflicted: Vec<&str> = conflicted.lines().collect();
        if conflicted != [LOCK_FILE] {
            let _ = git(dir, &["rebase", "--abort"]);
            if conflicted.is_empty() {
                return Err(e);
            }
            bail!(
                "this copy and the remote both changed {}; nothing was lost, but a person has to merge them \
                 (git pull --rebase in {})",
                conflicted.join(", "),
                dir.display()
            );
        }
        // While rebasing, stage 2 is the remote's side and 3 is ours being replayed.
        let stage = |n: u8| git(dir, &["show", &format!(":{n}:{LOCK_FILE}")]).unwrap_or_default();
        let merged = match chipsoc_config::merge_lock(&stage(1), &stage(3), &stage(2)) {
            Ok(m) => m,
            Err(err) => {
                let _ = git(dir, &["rebase", "--abort"]);
                return Err(err.context("merging lock.toml"));
            }
        };
        chipsoc_config::write_atomic(&dir.join(LOCK_FILE), &merged)?;
        git(dir, &["add", "--", LOCK_FILE])?;
        // Nothing of ours left in this commit: drop it.
        step = if succeeds(dir, &["diff", "--cached", "--quiet"]) {
            git(dir, &["rebase", "--skip"])
        } else {
            git(dir, &["rebase", "--continue"])
        };
    }
    let _ = git(dir, &["rebase", "--abort"]);
    bail!("rebasing the config didn't finish")
}

/// Top-level files and folders that make up a config.
const CONFIG_FILES: &[&str] = &[
    "server.toml",
    "roles.toml",
    "channels.toml",
    "bot.toml",
    "onboarding.toml",
    "join.toml",
    "screening.toml",
    "automod.toml",
    "projects.toml",
    "federation.toml",
    "members.toml",
    LOCK_FILE,
];
const CONFIG_DIRS: &[&str] = &["categories", "content"];
/// Posts, briefs and images can live anywhere in the config.
const CONFIG_EXTENSIONS: &[&str] = &["md", "png", "jpg", "jpeg", "gif", "webp"];

/// Whether a path (relative to the config) is a config file the bot may commit:
/// never a dot-file such as `.env`, nor anything that isn't config.
pub fn is_config_path(rel: &str) -> bool {
    let parts: Vec<&str> = rel.split('/').collect();
    if parts.iter().any(|p| p.is_empty() || p.starts_with('.')) {
        return false;
    }
    let ext = rel.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
    match parts.as_slice() {
        [file] => CONFIG_FILES.contains(file) || CONFIG_EXTENSIONS.contains(&ext.as_str()),
        [top, ..] if CONFIG_DIRS.contains(top) => true,
        _ => CONFIG_EXTENSIONS.contains(&ext.as_str()),
    }
}

/// Files here that look like secrets and that git would commit if someone ran
/// `git add -A`: they belong in `.gitignore`.
pub fn unignored_secrets(dir: &Path) -> Vec<String> {
    let Ok(out) = git(dir, &["ls-files", "--others", "--exclude-standard"]) else { return Vec::new() };
    out.lines()
        .filter(|f| {
            let name = f.rsplit('/').next().unwrap_or(f);
            name.starts_with(".env") || name == "deploy.env" || name.ends_with(".pem") || name.ends_with(".key")
        })
        .map(str::to_owned)
        .collect()
}

/// Config files changed here and not yet committed.
pub fn uncommitted(dir: &Path) -> Vec<String> {
    if !dir.join(".git").exists() {
        return Vec::new();
    }
    let Ok(out) = git(dir, &["status", "--porcelain", "--untracked-files=all"]) else { return Vec::new() };
    out.lines().filter_map(|l| l.get(3..)).filter(|f| is_config_path(f)).map(str::to_owned).collect()
}

/// Commit `lock.toml` and the config files this program wrote (if any changed)
/// with `message`. Never anything else, even a tracked file someone edited
/// on this machine. Returns true if it committed.
fn commit_config(dir: &Path, message: &str) -> Result<bool> {
    for f in unignored_secrets(dir) {
        tracing::warn!(dir = %dir.display(), "{f} looks like a secret and isn't in .gitignore; add it there");
    }
    let mut paths = take_written(dir);
    paths.push(LOCK_FILE.to_owned());
    paths.sort();
    paths.dedup();
    let tracked = git(dir, &["ls-files"])?;
    let tracked: std::collections::HashSet<&str> = tracked.lines().collect();
    let paths: Vec<&str> = paths
        .iter()
        .map(String::as_str)
        .filter(|p| is_config_path(p) && (tracked.contains(p) || dir.join(p).exists()))
        .collect();
    if !paths.is_empty() {
        let mut args = vec!["add", "--all", "--"];
        args.extend(paths);
        git(dir, &args)?;
    }
    if succeeds(dir, &["diff", "--cached", "--quiet"]) {
        return Ok(false);
    }
    // A commit of only lock.toml (ids and hashes after an apply) changes no
    // config, so CI has nothing new to check.
    let staged = git(dir, &["diff", "--cached", "--name-only"])?;
    let message =
        if staged.lines().all(|f| f == LOCK_FILE) { format!("{message}\n\n{SKIP_CI}") } else { message.to_owned() };
    git(dir, &["commit", "--quiet", "-m", &message])?;
    Ok(true)
}

/// Marks a commit CI needn't run for (GitHub Actions, GitLab and others skip it).
pub const SKIP_CI: &str = "[skip ci]";

/// Commit and push in one go, if there's anything to save.
pub fn save(dir: &Path, message: &str) -> Result<bool> {
    let _w = WriteLock::take(dir)?;
    save_locked(dir, message)
}

/// [`save`] for a caller already holding the [`WriteLock`].
pub fn save_locked(dir: &Path, message: &str) -> Result<bool> {
    let committed = commit_config(dir, message)?;
    if committed && is_synced(dir) {
        sync_locked(dir, true)?;
    }
    Ok(committed)
}

/// The trailer naming who made a commit by Discord id (`… by <name>` names them in the subject).
pub const BY_TRAILER: &str = "Discord-User";

/// The trailer naming who made a commit through the bot, by name.
pub const NAME_TRAILER: &str = "Made-By";

/// One commit in the config's history.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Commit {
    pub id: String,
    pub short: String,
    /// When it was made (Unix seconds).
    pub at: i64,
    pub subject: String,
    /// Who: the name the bot signed it with (see [`NAME_TRAILER`]), else git's author.
    pub who: String,
    /// Their Discord id, when the bot knew it.
    pub who_id: Option<chipsoc_config::Snowflake>,
    /// Whether it changed config files other than `lock.toml` (so reverting it changes something).
    pub config: bool,
}

const LOG_FORMAT: &str = "--format=%x1e%H%x1f%h%x1f%ct%x1f%an%x1f%s%x1f%b%x1f";

/// The latest `limit` commits, newest first (none if `dir` isn't a git checkout).
pub fn log(dir: &Path, limit: usize) -> Vec<Commit> {
    if !dir.join(".git").exists() {
        return Vec::new();
    }
    let n = format!("-n{}", limit.clamp(1, 1000));
    let Ok(out) = git(dir, &["log", &n, LOG_FORMAT, "--name-only", "--no-merges"]) else { return Vec::new() };
    out.split('\x1e').filter_map(commit).collect()
}

/// What tells a commit apart through rebases: its author time and whole
/// message, hashed (a rebase keeps both; its id changes). The bot keeps
/// this for each commit it makes, with who asked for it, so who made a
/// change is known from its own records, never from what a commit says.
pub fn fingerprint(dir: &Path, commit: &str) -> Option<String> {
    if commit != "HEAD" && !(commit.bytes().all(|b| b.is_ascii_hexdigit()) && !commit.is_empty()) {
        return None;
    }
    let out = git(dir, &["log", "-1", "--format=%at%x1f%B", commit, "--"]).ok()?;
    Some(chipsoc_model::stable_hash(out.trim()))
}

/// The commits that changed any of `paths`, as [`fingerprint`]s: those after
/// `since` when this checkout has it, else just the latest. At most 50.
pub fn changed(dir: &Path, since: Option<&str>, paths: &[&str]) -> Vec<String> {
    let range = since.filter(|c| contains(dir, c)).map(|c| format!("{c}..HEAD"));
    let mut args = vec!["log", "--format=%x1e%at%x1f%B", if range.is_some() { "-n50" } else { "-n1" }];
    args.extend(range.as_deref());
    args.push("--");
    args.extend(paths);
    let out = git(dir, &args).unwrap_or_default();
    out.split('\x1e').map(str::trim).filter(|r| !r.is_empty()).map(chipsoc_model::stable_hash).collect()
}

fn commit(record: &str) -> Option<Commit> {
    let f: Vec<&str> = record.split('\x1f').collect();
    let [id, short, at, author, subject, body, files] = f.as_slice() else { return None };
    let trailer =
        |key: &str| body.lines().find_map(|l| Some(l.strip_prefix(key)?.strip_prefix(':')?.trim().to_owned()));
    let who_id = trailer(BY_TRAILER).and_then(|id| id.parse().ok());
    // "… by ada" names who only in a commit the bot signed: "Sort roles by year" was made by its author.
    let signed = || who_id.is_some().then(|| subject.rsplit_once(" by ").map(|(_, w)| w.trim().to_owned())).flatten();
    let who = trailer(NAME_TRAILER).filter(|w| !w.is_empty()).or_else(signed).unwrap_or_else(|| author.to_string());
    let config = files.lines().map(str::trim).any(|f| !f.is_empty() && f != LOCK_FILE && is_config_path(f));
    Some(Commit {
        id: id.trim().to_owned(),
        short: short.to_string(),
        at: at.parse().unwrap_or(0),
        subject: subject.to_string(),
        who,
        who_id,
        config,
    })
}

/// How to go back on a config commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Undo {
    /// A new commit undoing just that one; later changes stay.
    Revert,
    /// A new commit putting the config back as it was right after it.
    Rewind,
}

/// Go back on config commit `commit` (as [`Undo`] says), committing the
/// result with `message(subject, short)` and pushing it when `push`.
/// Never rewrites history: the old commits stay, and a revert can itself be
/// reverted. `lock.toml` (the bot's ids) and files that aren't config stay
/// as they are. `check` vets the result first; nothing changes if it fails,
/// if the checkout has unsaved changes, if `commit` isn't in this branch's
/// history, or (reverting) if later changes clash with it. Returns the new
/// commit, or `None` if there was nothing to change. Once committed it
/// stands: a push that fails then is a warning on it, not an error. For a
/// caller holding the [`WriteLock`].
pub fn undo_locked(
    dir: &Path,
    commit: &str,
    how: Undo,
    message: impl FnOnce(&str, &str) -> String,
    check: impl FnOnce(&Path) -> Result<()>,
    push: bool,
) -> Result<Option<Undid>> {
    let commit = commit.trim();
    // Only an id: anything else could be taken for one of git's options.
    if !(4..=40).contains(&commit.len()) || !commit.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("`{commit}` isn't a commit id (4 to 40 letters a-f and digits)");
    }
    if !dir.join(".git").exists() {
        bail!("the config here isn't a git checkout, so it has no history to go back to");
    }
    let Ok(full) = git(dir, &["rev-parse", "--verify", "--quiet", &format!("{commit}^{{commit}}")]) else {
        bail!("no commit `{commit}` in the config's history");
    };
    if !contains(dir, &full) {
        bail!("commit `{commit}` isn't in this branch's history");
    }
    if !git(dir, &["status", "--porcelain", "--untracked-files=no"])?.is_empty() {
        bail!("the config here has changes that aren't saved yet; try again once they are (or the bot has synced)");
    }
    let parents = git(dir, &["rev-list", "--parents", "-n1", &full])?.split_whitespace().count() - 1;
    let (subject, short) =
        (git(dir, &["log", "-1", "--format=%s", &full])?, git(dir, &["rev-parse", "--short", &full])?);
    let undone = || {
        let _ = git(dir, &["revert", "--quit"]);
        let _ = git(dir, &["reset", "--hard", "--quiet", "HEAD"]);
    };
    match how {
        Undo::Revert => {
            if parents != 1 {
                bail!("`{short}` is a merge (or the first commit); rewind to the commit before it instead");
            }
            if let Err(e) = git(dir, &["revert", "--no-commit", &full]) {
                let _ = git(dir, &["revert", "--abort"]);
                undone();
                tracing::info!("reverting {short} clashed: {e:#}");
                bail!(
                    "later changes touch the same lines as `{short}`, so it can't be undone on its own; \
                     rewind to the commit before it, or change the files by hand"
                );
            }
            // Keep the bot's ids and anything that isn't config as they are now.
            let staged = git(dir, &["diff", "--cached", "--name-only"])?;
            for f in staged.lines().filter(|f| *f == LOCK_FILE || !is_config_path(f)) {
                git(dir, &["restore", "--source=HEAD", "--staged", "--worktree", "--", f])?;
            }
        }
        Undo::Rewind => {
            // Without renames: a file renamed since is listed under both names, so the old one comes back.
            let changed = git(dir, &["diff", "--no-renames", "--name-only", &full, "HEAD"])?;
            for f in changed.lines().filter(|f| *f != LOCK_FILE && is_config_path(f)) {
                if succeeds(dir, &["cat-file", "-e", &format!("{full}:{f}")]) {
                    git(dir, &["checkout", &full, "--", f])?;
                } else {
                    git(dir, &["rm", "--quiet", "--", f])?;
                }
            }
        }
    }
    if succeeds(dir, &["diff", "--cached", "--quiet"]) {
        undone();
        return Ok(None);
    }
    if let Err(e) = check(dir) {
        undone();
        return Err(e);
    }
    let message = message(&subject, &short);
    git(dir, &["commit", "--quiet", "--no-verify", "-m", &message])?;
    let subject = message.lines().next().unwrap_or_default().to_owned();
    let mut warning = None;
    if push
        && is_synced(dir)
        && let Err(e) = sync_locked(dir, true)
    {
        tracing::warn!("going back on {short} is committed, but pushing it failed: {e:#}");
        warning = Some(format!(
            "Saved, but pushing it to the config repo failed: {}. The bot pushes it on its next sync.",
            e.root_cause().to_string().trim_end_matches('.')
        ));
    }
    Ok(Some(Undid { subject, warning }))
}

/// What going back on a commit made (see [`undo_locked`]).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Undid {
    /// The new commit's subject.
    pub subject: String,
    /// Something that went wrong after it was committed (pushing it), which doesn't undo it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(args: &[&str]) {
        let out = Command::new("git").args(args).output().unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    }

    /// A bare origin and two clones of it.
    fn repos() -> (tempfile::TempDir, PathBuf, PathBuf, PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let (origin, a, b) = (root.path().join("origin.git"), root.path().join("a"), root.path().join("b"));
        run(&["init", "-q", "--bare", "-b", "main", origin.to_str().unwrap()]);
        for c in [&a, &b] {
            run(&["clone", "-q", origin.to_str().unwrap(), c.to_str().unwrap()]);
            run(&["-C", c.to_str().unwrap(), "config", "user.email", "t@t"]);
            run(&["-C", c.to_str().unwrap(), "config", "user.name", "t"]);
        }
        (root, origin, a, b)
    }

    fn read(p: &Path) -> String {
        std::fs::read_to_string(p).unwrap()
    }

    /// Write a file as the bot does, so its next save commits it.
    fn put(p: PathBuf, body: impl AsRef<str>) {
        chipsoc_config::write_atomic(&p, body.as_ref()).unwrap();
    }

    #[test]
    fn commits_and_pulls_between_clones() {
        let (_root, _, a, b) = repos();
        put(a.join("server.toml"), "x");
        assert!(save(&a, "first").unwrap());
        assert!(!save(&a, "nothing new").unwrap());
        assert!(pull(&b).unwrap());
        put(a.join("server.toml"), "y");
        save(&a, "second").unwrap();
        last_fetch().lock().unwrap().clear();
        assert_eq!(upstream_state(&b), Upstream::Behind(1, "origin/main".into()));
        assert!(pull(&b).unwrap());
        assert_eq!(read(&b.join("server.toml")), "y");
        assert!(!pull(&b).unwrap());
        assert_eq!(upstream_state(&b), Upstream::Current);
    }

    /// A commit's fingerprint stays through a rebase (its id doesn't), and
    /// `changed` lists what touched the given files since a commit.
    #[test]
    fn fingerprints_survive_a_rebase() {
        let (_root, _, a, b) = repos();
        put(a.join("server.toml"), "x");
        save(&a, "first").unwrap();
        pull(&b).unwrap();
        put(b.join("roles.toml"), "r");
        // Committed, not pushed yet (offline, say)...
        assert!(commit_config(&b, "Roles by ada\n\nDiscord-User: 7").unwrap());
        let (id, fp) = (head_full(&b).unwrap(), fingerprint(&b, "HEAD").unwrap());
        // ...while a's checkout pushes: b's next save rebases its commit.
        put(a.join("server.toml"), "y");
        save(&a, "second").unwrap();
        put(b.join("members.toml"), "m");
        save(&b, "Members by ada").unwrap();
        let rebased = git(&b, &["log", "--format=%H", "--grep=Roles by ada"]).unwrap();
        assert_ne!(rebased.trim(), id, "rebased");
        assert_eq!(fingerprint(&b, rebased.trim()).as_deref(), Some(fp.as_str()));
        assert!(changed(&b, None, &["roles.toml"]).contains(&fp));
        let first = git(&b, &["rev-list", "--max-parents=0", "HEAD"]).unwrap();
        let since = changed(&b, Some(first.trim()), &["roles.toml", "members.toml"]);
        assert_eq!(since.len(), 2, "{since:?}");
        assert_eq!(fingerprint(&b, "--all"), None, "only ids reach git");
    }

    /// Commit `files` in `a` as the bot would, signed by `who`.
    fn commit_as(a: &Path, files: &[(&str, &str)], message: &str) -> String {
        for (f, body) in files {
            put(a.join(f), body);
        }
        assert!(save(a, message).unwrap());
        git(a, &["rev-parse", "--short", "HEAD"]).unwrap()
    }

    fn undo(a: &Path, c: &str, how: Undo) -> Result<Option<String>> {
        let made = undo_locked(a, c, how, |s, short| format!("Undo \"{s}\" ({short}) by olive"), |_| Ok(()), true)?;
        Ok(made.map(|m| {
            assert_eq!(m.warning, None, "pushed");
            m.subject
        }))
    }

    #[test]
    fn a_revert_undoes_one_commit_and_a_rewind_restores_the_tree() {
        let (_root, _, a, b) = repos();
        commit_as(&a, &[("server.toml", "one"), ("roles.toml", "r1"), ("lock.toml", "l1")], "start");
        let ada =
            commit_as(&a, &[("server.toml", "two"), ("lock.toml", "l2")], "Server: name by ada\n\nDiscord-User: 7");
        commit_as(&a, &[("roles.toml", "r2"), ("members.toml", "m")], "Roles by max\n\nMade-By: max");
        let made = log(&a, 10);
        assert_eq!(made[1].short, ada);
        assert_eq!((made[1].who.as_str(), made[1].who_id), ("ada", Some(chipsoc_config::Snowflake(7))));
        assert_eq!(made[0].who, "max");
        commit_as(&a, &[("roles.toml", "r3")], "Sort roles by year");
        assert_eq!(log(&a, 1)[0].who, "t", "\"by year\" isn't a person: the bot didn't sign it");
        let sorted = log(&a, 1)[0].short.clone();
        undo(&a, &sorted, Undo::Revert).unwrap().unwrap();
        assert!(made.iter().all(|c| c.config));

        // Reverting ada's commit puts server.toml back; the later commit stays,
        // and the bot's lock.toml is left alone.
        let made = undo(&a, &ada, Undo::Revert).unwrap().unwrap();
        assert!(made.starts_with("Undo \"Server: name by ada\""), "{made}");
        assert_eq!((read(&a.join("server.toml")), read(&a.join("roles.toml"))), ("one".into(), "r2".into()));
        assert_eq!(read(&a.join("lock.toml")), "l2", "the bot's ids stay");
        assert!(pull(&b).unwrap(), "pushed");
        assert_eq!(read(&b.join("server.toml")), "one");
        assert_eq!(undo(&a, &ada, Undo::Revert).unwrap(), None, "already undone");

        // Rewinding to ada's commit restores the tree as it was then: no members.toml.
        let first = log(&a, 10).last().unwrap().short.clone();
        undo(&a, &ada, Undo::Rewind).unwrap().unwrap();
        assert_eq!((read(&a.join("server.toml")), read(&a.join("roles.toml"))), ("two".into(), "r1".into()));
        assert!(!a.join("members.toml").exists(), "added since: removed");
        assert_eq!(read(&a.join("lock.toml")), "l2");
        // Nothing is rewritten: every commit is still there, and the first can't be reverted alone.
        assert_eq!(log(&a, 10).len(), 7);
        assert!(undo(&a, &first, Undo::Revert).unwrap_err().to_string().contains("first commit"));
    }

    #[test]
    fn going_back_refuses_clashes_odd_ids_and_failed_checks_cleanly() {
        let (_root, _, a, _) = repos();
        commit_as(&a, &[("server.toml", "one\n")], "start");
        let c = commit_as(&a, &[("server.toml", "two\n")], "two");
        commit_as(&a, &[("server.toml", "three\n")], "three");
        let head = git(&a, &["rev-parse", "HEAD"]).unwrap();
        let err = undo(&a, &c, Undo::Revert).unwrap_err().to_string();
        assert!(err.contains("rewind"), "{err}");
        assert_eq!(git(&a, &["rev-parse", "HEAD"]).unwrap(), head, "nothing committed");
        assert_eq!(read(&a.join("server.toml")), "three\n", "and nothing left half done");
        assert!(git(&a, &["status", "--porcelain"]).unwrap().is_empty());

        for bad in ["--upload-pack=touch /tmp/x", "HEAD~1", "zz", "abc"] {
            assert!(undo(&a, bad, Undo::Rewind).unwrap_err().to_string().contains("isn't a commit id"), "{bad}");
        }
        let refused = undo_locked(&a, &c, Undo::Rewind, |s, _| s.to_owned(), |_| bail!("the config would break"), true);
        assert!(refused.unwrap_err().to_string().contains("would break"));
        assert_eq!(read(&a.join("server.toml")), "three\n");
        assert!(git(&a, &["status", "--porcelain"]).unwrap().is_empty(), "put back as it was");

        std::fs::write(a.join("server.toml"), "edited here").unwrap();
        assert!(undo(&a, &c, Undo::Rewind).unwrap_err().to_string().contains("aren't saved"));
    }

    #[test]
    fn a_rewind_brings_back_a_file_renamed_since() {
        let (_root, _, a, _) = repos();
        std::fs::create_dir_all(a.join("categories")).unwrap();
        let body = "[category]\nkey = \"a\"\nname = \"A\"\n# a few lines, so git sees a rename\n";
        let start = commit_as(&a, &[("server.toml", "s"), ("categories/10-a.toml", body)], "start");
        std::fs::rename(a.join("categories/10-a.toml"), a.join("categories/20-a.toml")).unwrap();
        run(&["-C", a.to_str().unwrap(), "add", "-A"]);
        run(&["-C", a.to_str().unwrap(), "commit", "-q", "-m", "move a"]);
        undo(&a, &start, Undo::Rewind).unwrap().unwrap();
        assert_eq!(read(&a.join("categories/10-a.toml")), body, "the old name is back");
        assert!(!a.join("categories/20-a.toml").exists(), "and the new one gone");
    }

    #[test]
    fn going_back_whose_push_fails_still_stands_with_a_warning() {
        let (root, origin, a, _) = repos();
        commit_as(&a, &[("server.toml", "one")], "start");
        let two = commit_as(&a, &[("server.toml", "two")], "two");
        // The origin goes away: the commit is made, pushing it isn't.
        std::fs::rename(&origin, root.path().join("gone.git")).unwrap();
        let made =
            undo_locked(&a, &two, Undo::Revert, |s, _| format!("Revert {s}"), |_| Ok(()), true).unwrap().unwrap();
        assert_eq!(made.subject, "Revert two");
        let warning = made.warning.expect("the push failure is told");
        assert!(warning.starts_with("Saved, but pushing it to the config repo failed:"), "{warning}");
        assert_eq!(read(&a.join("server.toml")), "one", "the revert stands");
        assert_eq!(log(&a, 1)[0].subject, "Revert two");
    }

    #[test]
    fn git_never_sees_the_bots_secrets() {
        for k in ["FED_SIGNING_KEY", "DISCORD_TOKEN", "MY_PARTNER_KEY", "WEB_TOKEN", "TAILSCALE_API_KEY"] {
            assert!(!passed_to_git(k), "{k}");
        }
        for k in ["PATH", "HOME", "GIT_SSH_COMMAND", "SSH_AUTH_SOCK", "https_proxy"] {
            assert!(passed_to_git(k), "{k}");
        }
    }

    #[test]
    fn saves_only_config_files() {
        let (_root, _, a, _) = repos();
        std::fs::create_dir_all(a.join("categories")).unwrap();
        for f in ["server.toml", "categories/10-a.toml", ".env", "notes.txt", "deploy.env", "lock.toml"] {
            put(a.join(f), "x");
        }
        assert!(save(&a, "first").unwrap());
        let files = git(&a, &["ls-files"]).unwrap();
        assert_eq!(files.lines().collect::<Vec<_>>(), ["categories/10-a.toml", "lock.toml", "server.toml"]);
        assert!(unignored_secrets(&a).contains(&".env".to_owned()));
        assert!(!save(&a, "only junk changed").unwrap(), "stray files never make a commit");
        // A tracked file edited on this machine by someone else isn't the bot's to commit.
        std::fs::write(a.join("server.toml"), "edited in place").unwrap();
        assert!(!save(&a, "autosave").unwrap(), "only what the bot wrote is committed");
        put(a.join("lock.toml"), "y");
        assert!(save(&a, "autosave").unwrap());
        assert_eq!(git(&a, &["show", "--name-only", "--format="]).unwrap(), "lock.toml");
        // Only lock.toml changed: CI skips it. The subject stays as it was.
        assert_eq!(git(&a, &["log", "-1", "--format=%s|%b"]).unwrap(), "autosave|[skip ci]");
        put(a.join("lock.toml"), "z");
        put(a.join("server.toml"), "edited by the bot");
        assert!(save(&a, "edit").unwrap());
        assert!(!git(&a, &["log", "-1", "--format=%B"]).unwrap().contains(SKIP_CI), "config changes are checked");
        for (p, ok) in [("content/x.md", true), ("img/logo.png", true), (".env", false), ("a/.env", false)] {
            assert_eq!(is_config_path(p), ok, "{p}");
        }
    }

    const BASE: &str = "guild = \"1\"\n\n[roles]\nadmin = \"10\"\n";

    /// The server's autosave races a push from a laptop: both change lock.toml.
    #[test]
    fn a_lock_clash_is_merged_not_left_mid_rebase() {
        let (_root, _, server, laptop) = repos();
        put(server.join("lock.toml"), BASE);
        put(server.join("server.toml"), "x");
        save(&server, "first").unwrap();
        pull(&laptop).unwrap();

        // The laptop applies and pushes: a new role.
        put(laptop.join("lock.toml"), format!("{BASE}member = \"11\"\n"));
        put(laptop.join("server.toml"), "y");
        save(&laptop, "laptop apply").unwrap();
        // Meanwhile the server made a channel, and saves without having pulled.
        put(server.join("lock.toml"), format!("{BASE}\n[channels]\ngeneral = \"20\"\n"));
        assert!(save(&server, "server apply").unwrap());

        let lock: chipsoc_config::Lock = toml::from_str(&read(&server.join("lock.toml"))).unwrap();
        assert_eq!(lock.roles.len(), 2, "the laptop's role is kept: {lock:?}");
        assert_eq!(lock.channels["general"].0, 20, "the server's channel is kept");
        assert_eq!(read(&server.join("server.toml")), "y");
        assert!(!recover(&server).unwrap(), "not left mid-rebase");
        assert_eq!(counts(&server).unwrap(), (0, 0), "pushed");
        pull(&laptop).unwrap();
        assert_eq!(read(&laptop.join("lock.toml")), read(&server.join("lock.toml")));
    }

    #[test]
    fn a_checkout_stuck_mid_rebase_recovers() {
        let (_root, _, server, laptop) = repos();
        put(server.join("bot.toml"), "a");
        save(&server, "first").unwrap();
        pull(&laptop).unwrap();
        put(laptop.join("bot.toml"), "laptop");
        save(&laptop, "laptop").unwrap();
        std::fs::write(server.join("bot.toml"), "server").unwrap();
        git(&server, &["commit", "-qam", "server"]).unwrap();
        // What an older version did: rebase, and stop at the clash.
        assert!(git(&server, &["pull", "--rebase", "--quiet"]).is_err());
        assert!(read(&server.join("bot.toml")).contains("<<<<<<<"));

        let err = sync(&server, true).unwrap_err().to_string();
        assert!(err.contains("bot.toml") && err.contains("a person has to merge"), "{err}");
        assert_eq!(read(&server.join("bot.toml")), "server", "back as it was, not mid-rebase");
        assert!(!recover(&server).unwrap());
    }

    #[test]
    fn offline_is_unknown_not_an_error() {
        let (_root, origin, a, _) = repos();
        put(a.join("server.toml"), "x");
        save(&a, "first").unwrap();
        std::fs::rename(&origin, origin.with_extension("gone")).unwrap();
        last_fetch().lock().unwrap().clear();
        assert!(matches!(upstream_state(&a), Upstream::Unknown(_)));
    }
}

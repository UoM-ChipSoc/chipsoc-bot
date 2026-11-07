//! Changing a config directory's files alongside other programs (the bot, the
//! web UI and the CLI share a checkout): the file locks they take turns by,
//! the files this program wrote (for the next commit), and running such
//! blocking work off the async runtime's threads.

use std::collections::BTreeSet;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

/// A lock on a file, held across processes until dropped: what every lock
/// on a config directory is made of ([`WriteLock`], and the engine's apply lease).
pub struct FileLock(#[allow(dead_code)] File);

/// How often a waiting [`FileLock::take`] tries again.
const STEP: Duration = Duration::from_millis(100);

impl FileLock {
    fn open(path: &Path) -> Result<File> {
        File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path)
            .with_context(|| format!("opening {}", path.display()))
    }

    /// The lock on `path`, if nobody holds it now.
    pub fn try_take(path: &Path) -> Result<Option<Self>> {
        let f = Self::open(path)?;
        Ok(f.try_lock().is_ok().then_some(Self(f)))
    }

    /// Whether someone holds the lock on `path` now: a hint, out of date as
    /// soon as it's said. Looking takes a shared lock for an instant, which
    /// never stops another look.
    pub fn held(path: &Path) -> bool {
        let Ok(f) = Self::open(path) else { return false };
        f.try_lock_shared().is_err()
    }

    /// Wait up to `wait` for the lock on `path`; after that, fail saying `busy`.
    pub fn take(path: &Path, wait: Duration, busy: &str) -> Result<Self> {
        let (f, until) = (Self::open(path)?, Instant::now() + wait);
        while !got(&f, until, busy)? {
            std::thread::sleep(STEP);
        }
        Ok(Self(f))
    }

    /// [`FileLock::take`] for async code: waits without holding up a thread.
    pub async fn take_async(path: &Path, wait: Duration, busy: &str) -> Result<Self> {
        let (f, until) = (Self::open(path)?, Instant::now() + wait);
        while !got(&f, until, busy)? {
            tokio::time::sleep(STEP).await;
        }
        Ok(Self(f))
    }
}

/// One try for `f`'s lock: whether it's held here now; an error once `until` has passed.
fn got(f: &File, until: Instant, busy: &str) -> Result<bool> {
    if f.try_lock().is_ok() {
        return Ok(true);
    }
    if Instant::now() > until {
        bail!("{busy}");
    }
    Ok(false)
}

/// Held while changing a config directory's files or its git state, across
/// processes (the bot and the web UI share a checkout). Kept inside `.git`
/// when there is one, so it's never committed.
pub struct WriteLock(#[allow(dead_code)] FileLock);

/// How long to wait for another writer to finish.
const WAIT: Duration = Duration::from_secs(60);

const BUSY: &str = "something else has been changing this config for a minute; try again later";

impl WriteLock {
    fn path(dir: &Path) -> PathBuf {
        if dir.join(".git").is_dir() { dir.join(".git/config-write.lock") } else { dir.join(".write.lock") }
    }

    /// Wait (up to a minute) for anyone else writing to `dir` to finish.
    pub fn take(dir: &Path) -> Result<Self> {
        FileLock::take(&Self::path(dir), WAIT, BUSY).map(Self)
    }

    /// [`WriteLock::take`] for async code: waits without holding up a thread.
    pub async fn take_async(dir: &Path) -> Result<Self> {
        FileLock::take_async(&Self::path(dir), WAIT, BUSY).await.map(Self)
    }
}

/// Run blocking work (git, file locks, copying the config) on a thread kept
/// for that, so it never holds up the async runtime's own threads.
pub async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T> + Send + 'static) -> Result<T> {
    tokio::task::spawn_blocking(f).await.context("a background task stopped")?
}

/// Files this program wrote or removed in config directories, waiting to be committed.
fn written() -> &'static Mutex<BTreeSet<PathBuf>> {
    static W: OnceLock<Mutex<BTreeSet<PathBuf>>> = OnceLock::new();
    W.get_or_init(Default::default)
}

/// Note that this program wrote (or removed) `path`, so the next save of its
/// config commits it. Nothing else but `lock.toml` is ever committed.
pub fn wrote(path: &Path) {
    if let Ok(p) = std::path::absolute(path) {
        written().lock().unwrap_or_else(|e| e.into_inner()).insert(p);
    }
}

/// The files under `dir` this program wrote since the last save (relative,
/// `/`-separated), forgetting them; also forgets ones in directories gone since
/// (scratch copies).
pub fn take_written(dir: &Path) -> Vec<String> {
    let Ok(root) = std::path::absolute(dir) else { return Vec::new() };
    let mut all = written().lock().unwrap_or_else(|e| e.into_inner());
    let mine: Vec<PathBuf> = all.iter().filter(|p| p.starts_with(&root)).cloned().collect();
    all.retain(|p| !p.starts_with(&root) && p.parent().is_some_and(Path::exists));
    mine.iter()
        .filter_map(|p| p.strip_prefix(&root).ok())
        .map(|r| r.components().map(|c| c.as_os_str().to_string_lossy()).collect::<Vec<_>>().join("/"))
        .collect()
}

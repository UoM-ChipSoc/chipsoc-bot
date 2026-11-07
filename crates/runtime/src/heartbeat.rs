//! A file the bot writes about once a minute while Discord's gateway talks to
//! it (its heartbeat acks come every 40 s or so), so something outside the
//! bot can tell when it's down: `deploy.sh monitor-setup`'s timer. Off
//! unless `HEARTBEAT_FILE` is set (the unit sets it).

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// The file is written at most this often (seconds).
const EVERY: u64 = 60;

static LAST: Beat = Beat::new();

/// The gateway delivered an event: note it in `HEARTBEAT_FILE`, if set and
/// a minute has passed since the last time.
pub fn beat() {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    if let Some(path) = std::env::var_os("HEARTBEAT_FILE") {
        LAST.at(Path::new(&path), now);
    }
}

struct Beat {
    last: AtomicU64,
    warned: AtomicBool,
}

impl Beat {
    const fn new() -> Self {
        Self { last: AtomicU64::new(0), warned: AtomicBool::new(false) }
    }

    /// Write the time `now` (seconds since 1970) to `path` unless it was
    /// written less than [`EVERY`] ago. Whether it wrote.
    fn at(&self, path: &Path, now: u64) -> bool {
        let last = self.last.load(Ordering::Relaxed);
        if now < last.saturating_add(EVERY)
            || self.last.compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed).is_err()
        {
            return false;
        }
        match std::fs::write(path, format!("{now}\n")) {
            Ok(()) => true,
            Err(e) => {
                if !self.warned.swap(true, Ordering::Relaxed) {
                    tracing::warn!(path = %path.display(), "can't write the heartbeat file: {e}");
                }
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_at_most_once_a_minute() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("heartbeat");
        let b = Beat::new();
        assert!(b.at(&f, 1_000));
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "1000\n");
        assert!(!b.at(&f, 1_059));
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "1000\n");
        assert!(b.at(&f, 1_060));
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "1060\n");
    }

    #[test]
    fn a_folder_it_cant_write_is_no_error() {
        let dir = tempfile::tempdir().unwrap();
        let b = Beat::new();
        assert!(!b.at(&dir.path().join("missing/heartbeat"), 1_000));
        // Not tried again within the minute either.
        assert!(!b.at(&dir.path().join("heartbeat"), 1_030));
    }
}

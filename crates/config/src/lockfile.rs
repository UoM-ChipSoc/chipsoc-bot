//! Keeping `lock.toml` safe: a backup before each save, and merging two
//! machines' versions when both changed it at once.

use std::path::Path;

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use toml::{Table, Value};

use crate::{Applied, LOCK_FILE, Lock, git, write_atomic};

/// The copy of `lock.toml` kept from before the last save. Not committed.
pub const LOCK_BACKUP: &str = "lock.toml.bak";

/// `[applied]` entry naming the config commit last applied from.
const APPLIED_COMMIT: &str = "git:commit";

impl Lock {
    /// How many Discord objects it knows (roles, categories, channels, posts).
    pub fn known(&self) -> usize {
        self.roles.len() + self.categories.len() + self.channels.len() + self.posts.len() + self.project_posts.len()
    }
}

// `[applied]` is one table on disk, the commit among the hashes, so a lock.toml
// (or the copy the bot keeps in its database) written now still reads in an
// older version of the bot, which rejects keys it doesn't know.
impl Serialize for Applied {
    fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        let mut all: BTreeMap<&str, &str> = self.hashes.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        if let Some(c) = &self.commit {
            all.insert(APPLIED_COMMIT, c);
        }
        all.serialize(s)
    }
}

impl<'de> Deserialize<'de> for Applied {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        let mut hashes = BTreeMap::<String, String>::deserialize(d)?;
        let commit = hashes.remove(APPLIED_COMMIT);
        Ok(Self { hashes, commit })
    }
}

/// Write `lock.toml`, first copying the one there to `lock.toml.bak`, unless
/// the one there looks lost (far emptier than the backup).
pub fn save(dir: &Path, lock: &Lock) -> Result<()> {
    let _w = git::WriteLock::take(dir)?;
    backup(dir);
    let body = toml::to_string_pretty(lock).context("serialising lock.toml")?;
    write_atomic(&dir.join(LOCK_FILE), &body)
}

fn read_lock(path: &Path) -> Option<(String, Lock)> {
    let body = std::fs::read_to_string(path).ok()?;
    let lock = toml::from_str(&body).ok()?;
    Some((body, lock))
}

fn backup(dir: &Path) {
    let Some((body, now)) = read_lock(&dir.join(LOCK_FILE)) else { return };
    let kept = read_lock(&dir.join(LOCK_BACKUP)).map_or(0, |(_, l)| l.known());
    if now.known() * 2 < kept {
        return;
    }
    if let Err(e) = write_atomic(&dir.join(LOCK_BACKUP), &body) {
        tracing::warn!("backing up lock.toml failed: {e:#}");
    }
}

/// The backup, if it knows many more objects than `lock`: a sign that
/// `lock.toml` was lost or emptied and should be restored.
pub fn fuller_backup(dir: &Path, lock: &Lock) -> Option<Lock> {
    let (_, kept) = read_lock(&dir.join(LOCK_BACKUP))?;
    (kept.known() >= 4 && lock.known() * 2 < kept.known()).then_some(kept)
}

/// Merge two versions of `lock.toml` that both started from `base`, key by
/// key (`[roles] admin`, `[posts] rules`, …): a key only one side changed takes
/// that side's value. Where both changed it, `mine` (this machine, the bot's
/// server) wins: its ids and what it applied are the ones it trusts.
pub fn merge_lock(base: &str, mine: &str, theirs: &str) -> Result<String> {
    let parse = |s: &str, side: &str| -> Result<Table> {
        if s.trim().is_empty() { Ok(Table::new()) } else { toml::from_str(s).with_context(|| format!("{side} side")) }
    };
    let (b, m, t) = (parse(base, "base")?, parse(mine, "this machine's")?, parse(theirs, "the remote's")?);
    let mut out = Table::new();
    for key in keys(&b, &m, &t) {
        let (bv, mv, tv) = (b.get(&key), m.get(&key), t.get(&key));
        let tables = [bv, mv, tv].into_iter().flatten().all(Value::is_table);
        let merged = if tables {
            let empty = Table::new();
            let tab = |v: Option<&Value>| v.and_then(Value::as_table).cloned().unwrap_or_else(|| empty.clone());
            let (bt, mt, tt) = (tab(bv), tab(mv), tab(tv));
            let mut inner = Table::new();
            for k in keys(&bt, &mt, &tt) {
                if let Some(v) = pick(bt.get(&k), mt.get(&k), tt.get(&k)) {
                    inner.insert(k, v);
                }
            }
            Some(Value::Table(inner))
        } else {
            pick(bv, mv, tv)
        };
        if let Some(v) = merged {
            out.insert(key, v);
        }
    }
    let lock: Lock = Value::Table(out).try_into().context("the merged lock.toml")?;
    toml::to_string_pretty(&lock).context("serialising lock.toml")
}

fn keys(a: &Table, b: &Table, c: &Table) -> std::collections::BTreeSet<String> {
    a.keys().chain(b.keys()).chain(c.keys()).cloned().collect()
}

fn pick(base: Option<&Value>, mine: Option<&Value>, theirs: Option<&Value>) -> Option<Value> {
    let v = if mine == theirs || theirs == base {
        mine
    } else if mine == base {
        theirs
    } else {
        mine // both changed it: keep the one this machine is using
    };
    v.cloned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Snowflake;

    #[test]
    fn long_posts_parts_read_in_old_and_new_versions() {
        // A lock.toml from before parts reads as one-part posts.
        let old = "[posts.rules]\nmessage = \"5\"\n\n[project_posts.golem]\nthread = \"6\"\nmessage = \"6\"\n";
        let lock: Lock = toml::from_str(old).unwrap();
        assert!(lock.posts["rules"].parts.is_empty() && lock.project_posts["golem"].parts.is_empty());
        assert!(!toml::to_string_pretty(&lock).unwrap().contains("parts"), "and is written as before");

        // One with parts still reads in an older bot, as these were: `parts` is ignored.
        #[derive(serde::Deserialize)]
        struct OldLock {
            posts: BTreeMap<String, OldPost>,
        }
        #[derive(serde::Deserialize)]
        struct OldPost {
            message: Snowflake,
        }
        let mut lock = Lock::default();
        let part = |n| crate::PostRef { message: Snowflake(n), parts: vec![Snowflake(n + 1), Snowflake(n + 2)] };
        lock.posts.insert("rules".into(), part(5));
        let body = toml::to_string_pretty(&lock).unwrap();
        assert!(body.contains("parts = ["), "{body}");
        let older: OldLock = toml::from_str(&body).unwrap();
        assert_eq!(older.posts["rules"].message, Snowflake(5));
        assert_eq!(toml::from_str::<Lock>(&body).unwrap(), lock);
    }

    #[test]
    fn merges_key_by_key() {
        let base = "guild = \"1\"\n[roles]\na = \"10\"\nb = \"11\"\nc = \"12\"\n[applied]\n\"role:a\" = \"h1\"\n";
        // Here: made d, archived c, re-made b. There: made e, edited a's hash, made d too.
        let mine = "guild = \"1\"\n[roles]\na = \"10\"\nb = \"21\"\nd = \"30\"\n[applied]\n\"role:a\" = \"h1\"\n";
        let theirs = "guild = \"1\"\n[roles]\na = \"10\"\nb = \"11\"\nc = \"12\"\nd = \"31\"\ne = \"40\"\n\
                      [applied]\n\"role:a\" = \"h2\"\n";
        let lock: Lock = toml::from_str(&merge_lock(base, mine, theirs).unwrap()).unwrap();
        let ids: Vec<(&str, u64)> = lock.roles.iter().map(|(k, v)| (k.as_str(), v.0)).collect();
        assert_eq!(ids, [("a", 10), ("b", 21), ("d", 30), ("e", 40)]);
        assert_eq!(lock.applied.hashes["role:a"], "h2");
        assert_eq!(lock.guild, Some(Snowflake(1)));
    }

    #[test]
    fn both_changing_an_existing_id_keeps_ours() {
        let base = "[channels]\nx = \"1\"\n";
        let lock: Lock =
            toml::from_str(&merge_lock(base, "[channels]\nx = \"2\"\n", "[channels]\nx = \"3\"\n").unwrap()).unwrap();
        assert_eq!(lock.channels["x"].0, 2);
    }

    /// The commit stays an `[applied]` entry on disk, as older versions read
    /// it, but never counts as an object's hash.
    #[test]
    fn the_applied_commit_reads_and_writes_where_it_always_was() {
        let body = "[applied]\n\"git:commit\" = \"abc\"\n\"role:a\" = \"h1\"\n";
        let lock: Lock = toml::from_str(body).unwrap();
        assert_eq!(lock.applied.commit.as_deref(), Some("abc"));
        assert_eq!(lock.applied.hashes.keys().collect::<Vec<_>>(), ["role:a"]);
        assert!(toml::to_string_pretty(&lock).unwrap().trim_end().ends_with(body.trim()));
        let json = serde_json::to_value(&lock).unwrap();
        assert_eq!(json["applied"]["git:commit"], "abc");
        assert_eq!(serde_json::from_value::<Lock>(json).unwrap(), lock);
    }

    #[test]
    fn backup_survives_a_lost_lock() {
        let d = tempfile::tempdir().unwrap();
        let mut full = Lock::default();
        for i in 0..6 {
            full.roles.insert(format!("r{i}"), Snowflake(i + 10));
        }
        save(d.path(), &full).unwrap();
        save(d.path(), &full).unwrap();
        let kept: Lock = toml::from_str(&std::fs::read_to_string(d.path().join(LOCK_BACKUP)).unwrap()).unwrap();
        assert_eq!(kept, full);
        // lock.toml is lost; saving the near-empty one must not replace the backup.
        std::fs::remove_file(d.path().join(LOCK_FILE)).unwrap();
        save(d.path(), &Lock::default()).unwrap();
        save(d.path(), &Lock::default()).unwrap();
        assert_eq!(fuller_backup(d.path(), &Lock::default()), Some(full));
    }
}

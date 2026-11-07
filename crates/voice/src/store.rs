//! Where recordings are kept and what's known about each: one folder per
//! recording, `<root>/<guild>/<date>-<time>-<channel>/`, holding a track per
//! speaker, the mixed track, `meta.json` and (once made) `transcript.md`.
//! Nothing here deletes a recording except [`delete`], which an admin asks for.

use std::io;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// The metadata file in each recording's folder.
pub const META: &str = "meta.json";
/// The mixed track.
pub const MIX: &str = "mix.ogg";
/// The transcript, once made.
pub const TRANSCRIPT: &str = "transcript.md";
/// Scratch files for transcribing, removed afterwards.
pub const WORK: &str = ".work";

/// Where recordings are kept: `RECORDINGS_DIR`, else `recordings` beside the
/// bot's database (`DATABASE_URL`, so the bot's state directory under
/// systemd), else `data/recordings`.
pub fn root() -> PathBuf {
    root_for(&std::env::var("DATABASE_URL").unwrap_or_else(|_| "sqlite:data/bot.db".into()))
}

/// [`root`] for the database at `db_url` (`RECORDINGS_DIR` still comes first).
pub fn root_for(db_url: &str) -> PathBuf {
    match std::env::var_os("RECORDINGS_DIR").filter(|d| !d.is_empty()) {
        Some(d) => PathBuf::from(d),
        None => root_beside(db_url),
    }
}

/// [`root`] for a database at `url`.
fn root_beside(url: &str) -> PathBuf {
    let db =
        url.strip_prefix("sqlite:").and_then(|p| p.split('?').next()).filter(|p| !p.is_empty() && *p != ":memory:");
    match db.and_then(|p| Path::new(p).parent()) {
        Some(dir) if !dir.as_os_str().is_empty() => dir.join("recordings"),
        _ => PathBuf::from("data/recordings"),
    }
}

/// One server's recordings.
pub fn guild_dir(root: &Path, guild: u64) -> PathBuf {
    root.join(guild.to_string())
}

/// Someone, as the recording names them.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Person {
    pub id: u64,
    pub name: String,
}

/// From when to when someone was in the call, in ms from the start.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Span {
    pub from_ms: u64,
    /// `None` while they're still there.
    pub to_ms: Option<u64>,
}

/// Someone who was in the call.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Participant {
    pub id: u64,
    /// Their name in the server when they joined.
    pub name: String,
    pub spans: Vec<Span>,
}

/// One speaker's audio: as long as the whole recording, silent where they were.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Track {
    /// Who, if known (audio Discord never said the sender of has none).
    pub user: Option<u64>,
    pub name: String,
    /// File name in the recording's folder.
    pub file: String,
    pub bytes: u64,
    /// When they spoke: `[from, to]` in ms from the start.
    #[serde(default)]
    pub speech: Vec<[u64; 2]>,
}

/// Whether there's a transcript.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "kebab-case")]
pub enum Transcript {
    /// Recording, or waiting to be transcribed.
    #[default]
    Waiting,
    /// Being made now.
    Making,
    Done,
    /// Not made, and why (no engine set up, say). The audio is kept.
    Skipped {
        why: String,
    },
    Failed {
        why: String,
    },
}

/// What `meta.json` says.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Meta {
    /// The folder's name, which is the recording's id.
    pub id: String,
    pub guild: u64,
    pub channel: u64,
    pub channel_name: String,
    /// Unix seconds.
    pub started_at: i64,
    pub ended_at: Option<i64>,
    /// Why it stopped: "stopped by Ada", "the call was empty", ….
    pub ended_because: Option<String>,
    pub started_by: Person,
    /// Length of the audio, in ms.
    #[serde(default)]
    pub length_ms: u64,
    pub participants: Vec<Participant>,
    pub tracks: Vec<Track>,
    /// The mixed track's file, once there is one.
    pub mix: Option<String>,
    /// All its files' size.
    #[serde(default)]
    pub bytes: u64,
    #[serde(default)]
    pub transcript: Transcript,
}

impl Meta {
    /// A new recording's metadata.
    pub fn new(id: String, guild: u64, channel: u64, channel_name: &str, started_at: i64, by: Person) -> Self {
        Self {
            id,
            guild,
            channel,
            channel_name: channel_name.to_owned(),
            started_at,
            ended_at: None,
            ended_because: None,
            started_by: by,
            length_ms: 0,
            participants: Vec::new(),
            tracks: Vec::new(),
            mix: None,
            bytes: 0,
            transcript: Transcript::Waiting,
        }
    }

    /// The files someone may download: tracks, the mix and the transcript.
    pub fn files(&self) -> Vec<&str> {
        let mut v: Vec<&str> = self.tracks.iter().map(|t| t.file.as_str()).collect();
        v.extend(self.mix.as_deref());
        if self.transcript == Transcript::Done {
            v.push(TRANSCRIPT);
        }
        v
    }

    /// Everyone's names, in the order they joined.
    pub fn names(&self) -> Vec<&str> {
        self.participants.iter().map(|p| p.name.as_str()).collect()
    }

    /// The name of the speaker `user`, from the participants (or the track).
    pub fn name_of(&self, user: u64) -> Option<&str> {
        self.participants.iter().find(|p| p.id == user).map(|p| p.name.as_str())
    }
}

/// Whether `s` can be a recording's id or file name: no paths, nothing hidden.
pub fn is_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 120
        && !s.starts_with('.')
        && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        && !s.contains("..")
}

/// A channel name as part of a folder name.
fn slug(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
    }
    let out = out.trim_end_matches('-');
    let out: String = out.chars().take(40).collect();
    if out.is_empty() { "call".into() } else { out }
}

/// Make a new recording's folder for `channel` started at `at` (Unix
/// seconds): `<date>-<time>-<channel>`, with `-2`… if that's taken.
pub fn create(root: &Path, guild: u64, at: i64, channel: &str) -> io::Result<(String, PathBuf)> {
    let when = chrono::DateTime::from_timestamp(at, 0).unwrap_or_default().format("%Y-%m-%d-%H%M");
    let base = format!("{when}-{}", slug(channel));
    let dir = guild_dir(root, guild);
    std::fs::create_dir_all(&dir)?;
    for n in 1..1000 {
        let id = if n == 1 { base.clone() } else { format!("{base}-{n}") };
        let p = dir.join(&id);
        match std::fs::create_dir(&p) {
            Ok(()) => return Ok((id, p)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::other("too many recordings with the same name"))
}

/// Write `meta` into `dir`, whole or not at all.
pub fn save(dir: &Path, meta: &Meta) -> io::Result<()> {
    let tmp = dir.join(".meta.json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(meta)?)?;
    std::fs::rename(&tmp, dir.join(META))
}

/// The metadata in `dir`.
pub fn load(dir: &Path) -> Result<Meta> {
    let p = dir.join(META);
    let text = std::fs::read(&p).with_context(|| format!("reading {}", p.display()))?;
    serde_json::from_slice(&text).with_context(|| format!("reading {}", p.display()))
}

/// A server's recordings, newest first. Folders without readable metadata are skipped.
pub fn list(root: &Path, guild: u64) -> Vec<Meta> {
    let Ok(entries) = std::fs::read_dir(guild_dir(root, guild)) else { return Vec::new() };
    let mut out: Vec<Meta> = entries
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_str().is_some_and(is_name))
        .filter_map(|e| match load(&e.path()) {
            Ok(m) => Some(m),
            Err(err) => {
                tracing::warn!("skipping a recording: {err:#}");
                None
            }
        })
        .collect();
    out.sort_by(|a, b| b.started_at.cmp(&a.started_at).then_with(|| b.id.cmp(&a.id)));
    out
}

/// A recording's folder and metadata, by id.
pub fn find(root: &Path, guild: u64, id: &str) -> Option<(PathBuf, Meta)> {
    if !is_name(id) {
        return None;
    }
    let dir = guild_dir(root, guild).join(id);
    let meta = load(&dir).ok()?;
    Some((dir, meta))
}

/// The path of one of a recording's files, if it has that file.
pub fn file(root: &Path, guild: u64, id: &str, name: &str) -> Option<PathBuf> {
    let (dir, meta) = find(root, guild, id)?;
    meta.files().contains(&name).then(|| dir.join(name)).filter(|p| p.is_file())
}

/// Delete a recording and all its files. False if there's no such
/// recording; refused while its transcript is being made (from its files).
pub fn delete(root: &Path, guild: u64, id: &str) -> io::Result<bool> {
    let Some((dir, meta)) = find(root, guild, id) else { return Ok(false) };
    if meta.transcript == Transcript::Making {
        return Err(io::Error::other("its transcript is being made"));
    }
    std::fs::remove_dir_all(dir)?;
    Ok(true)
}

/// Recordings the bot was still making when it stopped.
pub fn unfinished(root: &Path, guild: u64) -> Vec<Meta> {
    list(root, guild).into_iter().filter(|m| m.ended_at.is_none()).collect()
}

/// Bytes free for the bot at `path` (or the nearest folder above it that exists).
pub fn free_bytes(path: &Path) -> Option<u64> {
    let mut p = Some(path);
    while let Some(dir) = p {
        if dir.exists() {
            let s = rustix::fs::statvfs(dir).ok()?;
            return Some(s.f_bavail.saturating_mul(s.f_frsize));
        }
        p = dir.parent();
    }
    None
}

/// How much all of a server's recordings take.
pub fn total(root: &Path, guild: u64) -> u64 {
    let Ok(entries) = std::fs::read_dir(guild_dir(root, guild)) else { return 0 };
    entries.filter_map(Result::ok).filter(|e| e.file_type().is_ok_and(|t| t.is_dir())).map(|e| size(&e.path())).sum()
}

/// The size of the files in `dir`.
pub fn size(dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else { return 0 };
    entries.filter_map(Result::ok).filter_map(|e| e.metadata().ok()).filter(|m| m.is_file()).map(|m| m.len()).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(id: &str, at: i64) -> Meta {
        Meta::new(id.into(), 1, 2, "meeting", at, Person { id: 7, name: "Ada".into() })
    }

    #[test]
    fn root_is_beside_the_database() {
        assert_eq!(root_beside("sqlite:/var/lib/bot/bot.db?mode=rwc"), PathBuf::from("/var/lib/bot/recordings"));
        assert_eq!(root_beside("sqlite:data/bot.db?mode=rwc"), PathBuf::from("data/recordings"));
        assert_eq!(root_beside("sqlite::memory:"), PathBuf::from("data/recordings"));
        assert_eq!(root_beside(""), PathBuf::from("data/recordings"));
    }

    #[test]
    fn folders_are_dated_and_never_reused() {
        let t = tempfile::tempdir().unwrap();
        let at = 1_780_000_000; // 2026-05-28 20:26 UTC
        let (a, pa) = create(t.path(), 1, at, "Weekly Meeting 🎙️").unwrap();
        let (b, _) = create(t.path(), 1, at, "Weekly Meeting 🎙️").unwrap();
        assert_eq!(a, "2026-05-28-2026-weekly-meeting");
        assert_eq!(b, format!("{a}-2"));
        assert_eq!(pa, t.path().join("1").join(&a));
        assert!(is_name(&a));
    }

    #[test]
    fn metadata_round_trips_and_lists_newest_first() {
        let t = tempfile::tempdir().unwrap();
        for (name, at) in [("old", 100), ("new", 200)] {
            let (id, dir) = create(t.path(), 1, at, name).unwrap();
            let mut m = meta(&id, at);
            m.tracks.push(Track {
                user: Some(7),
                name: "Ada".into(),
                file: "7.ogg".into(),
                bytes: 3,
                speech: vec![[0, 20]],
            });
            m.mix = Some(MIX.into());
            save(&dir, &m).unwrap();
            assert_eq!(load(&dir).unwrap(), m);
        }
        std::fs::create_dir_all(t.path().join("1").join("junk")).unwrap();
        let all = list(t.path(), 1);
        let ids: Vec<&str> = all.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["1970-01-01-0003-new", "1970-01-01-0001-old"], "newest first; junk skipped");
        assert!(list(t.path(), 2).is_empty(), "another server's are apart");
        assert_eq!(unfinished(t.path(), 1).len(), 2);
    }

    #[test]
    fn only_listed_files_are_served_and_deleting_is_by_id() {
        let t = tempfile::tempdir().unwrap();
        let (id, dir) = create(t.path(), 1, 0, "c").unwrap();
        let mut m = meta(&id, 0);
        m.tracks.push(Track { user: Some(7), name: "Ada".into(), file: "7.ogg".into(), bytes: 0, speech: vec![] });
        save(&dir, &m).unwrap();
        std::fs::write(dir.join("7.ogg"), b"x").unwrap();
        std::fs::write(dir.join(TRANSCRIPT), b"x").unwrap();
        assert!(file(t.path(), 1, &id, "7.ogg").is_some());
        assert!(file(t.path(), 1, &id, META).is_none(), "not a listed file");
        assert!(file(t.path(), 1, &id, TRANSCRIPT).is_none(), "not done yet");
        assert!(file(t.path(), 1, "../1", "7.ogg").is_none());
        assert!(find(t.path(), 1, "..").is_none());
        assert!(!is_name("a/b") && !is_name(".work") && !is_name("a..b"));
        assert!(!delete(t.path(), 1, "nope").unwrap());
        assert!(delete(t.path(), 1, &id).unwrap());
        assert!(!dir.exists());
    }

    #[test]
    fn all_of_a_servers_recordings_are_counted() {
        let t = tempfile::tempdir().unwrap();
        assert_eq!(total(t.path(), 1), 0);
        let (_, a) = create(t.path(), 1, 0, "a").unwrap();
        let (_, b) = create(t.path(), 1, 60, "b").unwrap();
        let (_, other) = create(t.path(), 2, 0, "a").unwrap();
        std::fs::write(a.join("7.ogg"), [0u8; 100]).unwrap();
        std::fs::write(b.join("8.ogg"), [0u8; 50]).unwrap();
        std::fs::write(other.join("8.ogg"), [0u8; 1000]).unwrap();
        assert_eq!(total(t.path(), 1), 150, "this server's only");
    }

    #[test]
    fn free_space_is_known() {
        let t = tempfile::tempdir().unwrap();
        assert!(free_bytes(&t.path().join("not/yet")).is_some_and(|b| b > 0));
    }
}

//! Reading and writing a server's config directory. Every front end (CLI, TUI,
//! web, the bot) goes through here.
//!
//! ```text
//! server.toml  roles.toml  categories/NN-name.toml  channels.toml
//! bot.toml  onboarding.toml  screening.toml  automod.toml  join.toml
//! projects.toml  federation.toml  content/*.md  lock.toml
//! ```

pub mod authoring;
pub mod bot;
pub mod git;
pub mod join;
mod lockfile;
pub mod ops;
pub mod projects;
pub mod recording;
pub mod schema;
pub mod template;
pub mod trust;

use std::collections::BTreeMap;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

pub use bot::BotConfig;
pub use join::Join;
pub use lockfile::{LOCK_BACKUP, fuller_backup, merge_lock};
pub use schema::*;

pub const LOCK_FILE: &str = "lock.toml";

/// A loaded config directory.
#[derive(Debug, Clone, Default)]
pub struct Config {
    pub dir: PathBuf,
    /// What Discord should look like, projects expanded.
    pub desired: Snapshot,
    pub bot: BotConfig,
    pub projects: Vec<Project>,
    pub federation: Federation,
    /// Things worth a warning, found while loading.
    pub notes: Vec<String>,
    /// members.toml as written (role key → user ids, repeats and all).
    pub members: BTreeMap<String, Vec<Snowflake>>,
    /// Channels whose name Discord would change: (key, as written, as Discord has it).
    pub renamed: Vec<(String, String, String)>,
    /// The bot's join flow (`join.toml`).
    pub join: Option<Join>,
}

/// What `{positions}` and `{projects:open}` list: positions open in
/// `/volunteer` (label, first line of the intro) and projects taking
/// applications (name, blurb). Kept in step with bot.toml and projects.toml.
fn open_lists(bot: &BotConfig, projects: &[Project], desired: &Snapshot) -> (Vec<template::Item>, Vec<template::Item>) {
    let positions = bot
        .positions
        .iter()
        .filter(|p| p.open)
        .map(|p| {
            let name = p.label.clone().or_else(|| desired.role(&p.role).map(|r| r.name.clone()));
            let mut about = p.intro.as_deref().map(template::short_line).unwrap_or_default();
            if let Some(day) = p.closes_label() {
                let closes = format!("applications close {day}");
                about = if about.is_empty() { closes } else { format!("{about} ({closes})") };
            }
            (name.unwrap_or_else(|| p.role.clone()), about)
        })
        .collect();
    let projects = projects
        .iter()
        .filter(|p| p.application.open && p.is_live())
        .map(|p| (p.name.clone(), template::short_line(&p.blurb)))
        .collect();
    (positions, projects)
}

impl Config {
    /// Whether admins decide who holds `role`: the admin role, a position's
    /// role, one members.toml lists, or a project's lead role. Nothing a member
    /// picks for themselves (the join flow, onboarding) may give it.
    pub fn admins_decide(&self, role: &str) -> bool {
        self.bot.admin_role.as_deref() == Some(role)
            || self.bot.positions.iter().any(|p| p.role == role)
            || self.desired.holders.contains_key(role)
            || self.projects.iter().any(|p| p.lead_role && p.lead_role_key() == role)
    }

    /// A post's text with `{positions}` and `{projects:open}` filled in, as
    /// [`Config::load`] does for managed posts (for previews).
    pub fn fill_lists(&self, body: &str) -> String {
        let (positions, projects) = open_lists(&self.bot, &self.projects, &self.desired);
        template::lists(body, &positions, &projects)
    }

    pub fn load(dir: &Path) -> Result<Self> {
        no_symlinks(dir)?;
        let bot: BotConfig = read_or_default(&dir.join("bot.toml"))?;
        let projects = read_or_default::<Projects>(&dir.join("projects.toml"))?.projects;
        let federation = read_or_default(&dir.join("federation.toml"))?;
        let join: Option<Join> = read_optional(&dir.join("join.toml"))?;
        let mut desired = load_snapshot(dir)?;
        let mut onboarding: Option<Onboarding> = read_optional(&dir.join("onboarding.toml"))?;
        let mut notes = Vec::new();
        if join.is_some() && onboarding.is_none() {
            // Discord's questions would be asked as well as the flow's: turn them off.
            onboarding = Some(Onboarding::default());
            notes.push("join.toml: Discord's onboarding questions are off; the join flow asks instead.".to_owned());
        }
        desired.settings = Settings {
            onboarding,
            screening: read_optional(&dir.join("screening.toml"))?,
            automod: read_or_default::<AutoMod>(&dir.join("automod.toml"))?.rules,
        };
        let post_files: Vec<(String, String)> =
            desired.channels().filter_map(|(ch, _)| Some((ch.key.clone(), ch.post.clone()?))).collect();
        // An apply button for a project that isn't taking applications (closed,
        // or not active) is left off until it is.
        let closed: Vec<String> = projects
            .iter()
            .filter(|p| !(p.application.open && p.is_live()))
            .map(|p| format!("apply:{}", p.slug))
            .collect();
        let (open_positions, open_projects) = open_lists(&bot, &projects, &desired);
        for (key, path) in post_files {
            let body = read_text(&inside(dir, &path)?)?.with_context(|| format!("post file {path} not found"))?;
            let body = template::lists(&body, &open_positions, &open_projects);
            for a in template::button_actions(&body).into_iter().filter(|a| closed.contains(a)) {
                notes.push(format!("{path}: the `{a}` button is left off while the project isn't taking applications"));
            }
            desired.posts.insert(key, template::without_buttons(&body, |a| closed.iter().any(|c| c == a)));
        }
        for p in &projects {
            if let Some(brief) = &p.brief {
                let body = read_text(&inside(dir, brief)?)?.with_context(|| format!("brief {brief} not found"))?;
                desired.project_posts.insert(p.slug.clone(), body);
            }
        }
        for (name, file) in [("icon", &desired.server.guild.icon), ("banner", &desired.server.guild.banner)] {
            if let Some(f) = file {
                let bytes = fs::read(inside(dir, f)?).with_context(|| format!("reading {f}"))?;
                desired.assets.insert(name.into(), chipsoc_model::stable_hash(bytes));
            }
        }
        let members: BTreeMap<String, Vec<Snowflake>> = read_or_default(&dir.join("members.toml"))?;
        projects::expand(&mut desired, &projects, &bot);
        holders(&mut desired, &members, &projects, &bot);
        resolve_inheritance(&mut desired);
        if let Some(j) = &join {
            join::gate(&mut desired, j);
        }
        let renamed = normalise_names(&mut desired);
        Ok(Self { dir: dir.to_owned(), desired, bot, projects, federation, notes, members, renamed, join })
    }

    /// The guild this config manages, once it has been exported or applied.
    pub fn guild(&self) -> Option<Snowflake> {
        self.desired.lock.guild
    }

    /// Write `lock.toml` (keeping the last one as `lock.toml.bak`).
    pub fn save_lock(&self, lock: &Lock) -> Result<()> {
        lockfile::save(&self.dir, lock)
    }

    /// [`Config::save_lock`] off the async runtime's threads (it may wait for the write lock).
    pub async fn save_lock_async(&self, lock: &Lock) -> Result<()> {
        let (dir, lock) = (self.dir.clone(), lock.clone());
        git::blocking(move || lockfile::save(&dir, &lock)).await
    }
}

/// A hash of every file's path, size and modification time: it changes when
/// any file does. Dot-files are skipped (`.git`, and rsync's temporary files).
pub fn fingerprint(dir: &Path) -> u64 {
    fingerprint_where(dir, |_| true)
}

/// [`fingerprint`] of what people write: everything but `lock.toml` (and its
/// backup), which applies change.
pub fn edits_fingerprint(dir: &Path) -> u64 {
    let (lock, backup) = (dir.join(LOCK_FILE), dir.join(LOCK_BACKUP));
    fingerprint_where(dir, |p| p != lock && p != backup)
}

fn fingerprint_where(dir: &Path, keep: impl Fn(&Path) -> bool) -> u64 {
    use std::hash::{Hash, Hasher};
    type Seen = Vec<(PathBuf, u64, Option<std::time::SystemTime>)>;
    fn collect(dir: &Path, keep: &dyn Fn(&Path) -> bool, out: &mut Seen) {
        let Ok(entries) = fs::read_dir(dir) else { return };
        for e in entries.flatten() {
            if e.file_name().to_string_lossy().starts_with('.') {
                continue;
            }
            let Ok(meta) = e.metadata() else { continue };
            if meta.is_dir() {
                collect(&e.path(), keep, out);
            } else if keep(&e.path()) {
                out.push((e.path(), meta.len(), meta.modified().ok()));
            }
        }
    }
    let mut files = Vec::new();
    collect(dir, &keep, &mut files);
    files.sort();
    let mut h = std::collections::hash_map::DefaultHasher::new();
    files.hash(&mut h);
    h.finish()
}

/// Where to report trouble with a config that doesn't load: its bot log
/// channel (or people log), if `bot.toml` and `lock.toml` still read.
pub fn log_channel(dir: &Path) -> Option<Snowflake> {
    let bot: BotConfig = read_or_default(&dir.join("bot.toml")).ok()?;
    let lock: Lock = read_or_default(&dir.join(LOCK_FILE)).ok()?;
    lock.channel(bot.log.bot.as_deref().or(bot.log.people.as_deref())?)
}

/// Who should hold which roles: members.toml, project leads, and the admin
/// role for anyone in an admin position. Only roles named here are managed;
/// the admin role only when members.toml lists it or a position has
/// `admin = true`, so listing other roles never takes it from anyone.
fn holders(snap: &mut Snapshot, members: &BTreeMap<String, Vec<Snowflake>>, projects: &[Project], bot: &BotConfig) {
    for (role, ids) in members {
        snap.holders.entry(role.clone()).or_default().extend(ids.iter().copied());
    }
    if let Some(admin) = &bot.admin_role
        && (members.contains_key(admin) || bot.positions.iter().any(|p| p.admin))
    {
        let admins: Vec<Snowflake> = bot
            .positions
            .iter()
            .filter(|p| p.admin)
            .flat_map(|p| members.get(&p.role).into_iter().flatten().copied())
            .collect();
        snap.holders.entry(admin.clone()).or_default().extend(admins);
    }
    for p in projects {
        // Leads hold the Lead role while active, and keep it as a record after.
        if p.status != ProjectStatus::Proposed && p.lead_role && !p.leads.is_empty() {
            snap.holders.entry(p.lead_role_key()).or_default().extend(p.leads.iter().copied());
        }
    }
}

/// Turn each channel's overwrites into what Discord should have.
fn resolve_inheritance(snap: &mut Snapshot) {
    for cf in &mut snap.categories {
        for ch in &mut cf.channels {
            ch.overwrites = effective_overwrites(Some(&cf.category), ch);
            ch.inherit = false;
        }
    }
}

/// A channel name as Discord keeps it: text-like channels (text,
/// announcement, forum, media) are lower case with dashes for spaces.
pub fn discord_channel_name(kind: ChannelKind, name: &str) -> String {
    if kind.is_voice() {
        return name.to_owned();
    }
    name.split_whitespace().collect::<Vec<_>>().join("-").to_lowercase()
}

/// Write channel names as Discord will have them, so a name Discord changes
/// isn't drift; returns what changed.
fn normalise_names(snap: &mut Snapshot) -> Vec<(String, String, String)> {
    let mut renamed = Vec::new();
    let all = snap.categories.iter_mut().flat_map(|c| c.channels.iter_mut()).chain(snap.top_level.channels.iter_mut());
    for ch in all {
        let name = discord_channel_name(ch.kind, &ch.name);
        if name != ch.name {
            renamed.push((ch.key.clone(), std::mem::replace(&mut ch.name, name.clone()), name));
        }
    }
    renamed
}

/// The structure files and lockfile, without settings or projects.
pub fn load_snapshot(dir: &Path) -> Result<Snapshot> {
    let mut categories = Vec::new();
    for path in category_paths(dir)? {
        categories.push(read_toml::<CategoryFile>(&path)?);
    }
    Ok(Snapshot {
        server: read_toml(&dir.join("server.toml"))?,
        roles: read_or_default(&dir.join("roles.toml"))?,
        categories,
        top_level: read_or_default(&dir.join("channels.toml"))?,
        lock: read_or_default(&dir.join(LOCK_FILE))?,
        ..Default::default()
    })
}

/// Category files, sorted by name (which is their order in Discord).
pub fn category_paths(dir: &Path) -> Result<Vec<PathBuf>> {
    let cats = dir.join("categories");
    let entries = match fs::read_dir(&cats) {
        Ok(e) => e,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", cats.display())),
    };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e == "toml"))
        .collect();
    paths.sort();
    Ok(paths)
}

/// Channel keys @everyone can view: not denied on the channel, nor on its
/// category unless the channel allows it back. If the @everyone role can't
/// view channels (the join gate), only those allowing it.
pub fn public_channels(snap: &Snapshot) -> Vec<String> {
    let says = |ows: &[Overwrite], allow: bool| {
        ows.iter().any(|o| {
            o.role.as_deref() == Some("everyone")
                && if allow { &o.allow } else { &o.deny }.iter().any(|p| p == "VIEW_CHANNEL")
        })
    };
    let base =
        snap.role("everyone").is_none_or(|r| r.permissions.iter().any(|p| p == "VIEW_CHANNEL" || p == "ADMINISTRATOR"));
    snap.channels()
        .filter(|(c, cat)| {
            if says(&c.overwrites, false) {
                return false;
            }
            let by_cat = |allow: bool| cat.is_some_and(|cat| says(&cat.overwrites, allow));
            says(&c.overwrites, true) || !by_cat(false) && (base || by_cat(true))
        })
        .map(|(c, _)| c.key.clone())
        .collect()
}

/// Onboarding with its default channels spelled out and sorted, for comparing and sending.
pub fn resolved_onboarding(o: &Onboarding, snap: &Snapshot) -> Onboarding {
    let mut o = o.clone();
    if o.default_channels.is_empty() {
        o.default_channels = public_channels(snap);
    }
    o.default_channels.sort();
    o
}

/// Write an exported snapshot as a config directory.
pub fn write_snapshot(dir: &Path, snap: &Snapshot) -> Result<()> {
    let cats = dir.join("categories");
    fs::create_dir_all(&cats).with_context(|| format!("creating {}", cats.display()))?;
    write_toml(&dir.join("server.toml"), &snap.server)?;
    write_toml(&dir.join("roles.toml"), &snap.roles)?;
    for (i, cat) in snap.categories.iter().enumerate() {
        let mut cat = cat.clone();
        for ch in &mut cat.channels {
            (ch.overwrites, ch.inherit) = own_overwrites(Some(&cat.category), &ch.overwrites);
        }
        let name = format!("{:02}-{}.toml", (i + 1) * 10, slug(&cat.category.name));
        write_toml(&cats.join(name), &cat)?;
    }
    if !snap.top_level.channels.is_empty() {
        write_toml(&dir.join("channels.toml"), &snap.top_level)?;
    }
    let s = &snap.settings;
    if let Some(o) = &s.onboarding {
        write_toml(&dir.join("onboarding.toml"), o)?;
    }
    if let Some(sc) = &s.screening {
        write_toml(&dir.join("screening.toml"), sc)?;
    }
    if !s.automod.is_empty() {
        write_toml(&dir.join("automod.toml"), &AutoMod { rules: s.automod.clone() })?;
    }
    write_toml(&dir.join(LOCK_FILE), &snap.lock)
}

pub(crate) fn read_text(path: &Path) -> Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(s) => Ok(Some(s)),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

fn read_toml<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    let body = read_text(path)?.with_context(|| format!("{} not found", path.display()))?;
    toml::from_str(&body).with_context(|| format!("in {}", path.display()))
}

fn read_optional<T: serde::de::DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    match read_text(path)? {
        Some(body) => toml::from_str(&body).map(Some).with_context(|| format!("in {}", path.display())),
        None => Ok(None),
    }
}

fn read_or_default<T: serde::de::DeserializeOwned + Default>(path: &Path) -> Result<T> {
    Ok(read_optional(path)?.unwrap_or_default())
}

/// A file named in config, which must stay inside the config directory: no
/// absolute paths, no `..`, and no symlinks anywhere along the way (a config
/// change must never make the bot publish files from elsewhere).
pub fn inside(dir: &Path, rel: &str) -> Result<PathBuf> {
    use std::path::Component;
    let p = Path::new(rel);
    if rel.trim().is_empty() || p.components().any(|c| !matches!(c, Component::Normal(_) | Component::CurDir)) {
        anyhow::bail!("`{rel}` must be a path inside the config folder (no `/` at the start, no `..`)");
    }
    let mut at = dir.to_path_buf();
    for c in p.components() {
        at.push(c);
        if fs::symlink_metadata(&at).is_ok_and(|m| m.file_type().is_symlink()) {
            anyhow::bail!("`{rel}` goes through a symlink, which isn't allowed");
        }
    }
    Ok(at)
}

/// Refuse a config folder with a symlink anywhere in it (hidden ones like
/// `.git` aside). Git carries symlinks, and one pushed to the repo could make
/// the bot read a file from elsewhere (and post it, or quote it in an error)
/// or write through it, into `.git` say.
pub fn no_symlinks(dir: &Path) -> Result<()> {
    fn walk(dir: &Path, rel: &Path) -> Result<()> {
        let entries = match fs::read_dir(dir.join(rel)) {
            Ok(e) => e,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e).with_context(|| format!("reading {}", dir.join(rel).display())),
        };
        for entry in entries {
            let entry = entry?;
            if entry.file_name().to_string_lossy().starts_with('.') {
                continue;
            }
            let path = rel.join(entry.file_name());
            let kind = entry.file_type()?;
            if kind.is_symlink() {
                anyhow::bail!(
                    "`{}` is a symlink, which isn't allowed in the config; replace it with the file itself",
                    path.display()
                );
            }
            if kind.is_dir() {
                walk(dir, &path)?;
            }
        }
        Ok(())
    }
    walk(dir, Path::new(""))
}

/// Write `bytes` via a temporary file and rename, so a crash never leaves
/// half a file. The temporary file is made new (never opened through a
/// symlink someone left there).
pub fn write_new(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let tmp = path.with_extension("tmp");
    match fs::remove_file(&tmp) {
        Err(e) if e.kind() != ErrorKind::NotFound => {
            return Err(e).with_context(|| format!("removing {}", tmp.display()));
        }
        _ => {}
    }
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)
        .with_context(|| format!("writing {}", tmp.display()))?;
    f.write_all(bytes).with_context(|| format!("writing {}", tmp.display()))?;
    drop(f);
    fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))?;
    git::wrote(path);
    Ok(())
}

/// Write via a temporary file and rename, so a crash never leaves half a file.
pub fn write_atomic(path: &Path, body: &str) -> Result<()> {
    write_new(path, body.as_bytes())
}

fn write_toml<T: serde::Serialize>(path: &Path, value: &T) -> Result<()> {
    let body = toml::to_string_pretty(value).with_context(|| format!("serialising {}", path.display()))?;
    write_atomic(path, &body)
}

/// What a role, category or channel key may be made of, for messages.
pub const KEY_RULE: &str = "lower-case letters, digits and dashes";

/// Whether `s` is a key: [`KEY_RULE`], not empty. Keys end up in URLs and
/// Discord custom ids, so `check` refuses anything else.
pub fn is_key(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// Lower-case ASCII slug of a name, for file names and default keys.
pub fn slug(name: &str) -> String {
    let mut s = String::new();
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            s.push(c.to_ascii_lowercase());
        } else if !s.is_empty() && !s.ends_with('-') {
            s.push('-');
        }
    }
    let s = s.trim_end_matches('-').to_string();
    if s.is_empty() { "item".into() } else { s }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn named_files_stay_inside_the_config() {
        let d = std::path::Path::new("/c");
        assert_eq!(inside(d, "content/a.md").unwrap(), d.join("content/a.md"));
        for bad in ["../x.md", "/etc/passwd", "content/../../x", ""] {
            assert!(inside(d, bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn channel_names_as_discord_keeps_them() {
        assert_eq!(discord_channel_name(ChannelKind::Text, "General  Chat"), "general-chat");
        assert_eq!(discord_channel_name(ChannelKind::Forum, "Help"), "help");
        assert_eq!(discord_channel_name(ChannelKind::Voice, "Lounge Voice"), "Lounge Voice");
    }

    #[test]
    fn slugify() {
        assert_eq!(slug("Help & Resources"), "help-resources");
        assert_eq!(slug("🦀"), "item");
    }

    #[test]
    fn category_file_round_trips_and_rejects_typos() {
        let cf = CategoryFile {
            category: Category {
                key: "welcome".into(),
                name: "Welcome".into(),
                overwrites: vec![Overwrite {
                    role: Some("everyone".into()),
                    deny: vec!["VIEW_CHANNEL".into()],
                    ..Default::default()
                }],
            },
            channels: vec![Channel {
                key: "rules".into(),
                name: "rules".into(),
                overwrites: vec![Overwrite {
                    role: Some("member".into()),
                    allow: vec!["SEND_MESSAGES".into()],
                    ..Default::default()
                }],
                ..Default::default()
            }],
        };
        let s = toml::to_string_pretty(&cf).unwrap();
        assert_eq!(toml::from_str::<CategoryFile>(&s).unwrap(), cf);
        let typo = s.replace("name = \"rules\"", "nmae = \"rules\"");
        assert!(toml::from_str::<CategoryFile>(&typo).is_err(), "unknown fields are errors");
    }

    #[test]
    fn channels_inherit_category_overwrites() {
        let ow = |role: &str, allow: &[&str], deny: &[&str]| Overwrite {
            role: Some(role.into()),
            allow: allow.iter().map(|s| s.to_string()).collect(),
            deny: deny.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        };
        let cat = Category {
            overwrites: vec![ow("everyone", &[], &["VIEW_CHANNEL"]), ow("committee", &["VIEW_CHANNEL"], &[])],
            ..Default::default()
        };
        let inbox =
            Channel { overwrites: vec![ow("committee", &["VIEW_CHANNEL"], &["SEND_MESSAGES"])], ..Default::default() };
        let eff = effective_overwrites(Some(&cat), &inbox);
        assert_eq!(
            eff,
            vec![ow("everyone", &[], &["VIEW_CHANNEL"]), ow("committee", &["VIEW_CHANNEL"], &["SEND_MESSAGES"])]
        );
        assert_eq!(
            own_overwrites(Some(&cat), &eff),
            (inbox.overwrites.clone(), true),
            "export writes only the difference"
        );
        assert!(
            effective_overwrites(Some(&cat), &Channel::default()).len() == 2,
            "plain channels are as private as their category"
        );
        let open = Channel { inherit: false, ..Default::default() };
        assert!(effective_overwrites(Some(&cat), &open).is_empty());
        assert_eq!(
            own_overwrites(Some(&cat), &[]),
            (vec![], false),
            "dropping the category's entries needs inherit = false"
        );
    }

    #[test]
    fn public_channels_follow_overwrites() {
        let deny = |allow: bool| Overwrite {
            role: Some("everyone".into()),
            allow: if allow { vec!["VIEW_CHANNEL".into()] } else { vec![] },
            deny: if allow { vec![] } else { vec!["VIEW_CHANNEL".into()] },
            ..Default::default()
        };
        let ch = |k: &str, ows: Vec<Overwrite>| Channel {
            key: k.into(),
            name: k.into(),
            overwrites: ows,
            ..Default::default()
        };
        let snap = Snapshot {
            categories: vec![
                CategoryFile {
                    category: Category::default(),
                    channels: vec![ch("open", vec![]), ch("shut", vec![deny(false)])],
                },
                CategoryFile {
                    category: Category { overwrites: vec![deny(false)], ..Default::default() },
                    channels: vec![ch("private", vec![]), ch("window", vec![deny(true)])],
                },
            ],
            ..Default::default()
        };
        assert_eq!(public_channels(&snap), ["open", "window"]);
    }
}

#[cfg(test)]
mod holder_tests {
    use super::*;

    fn dir(bot: &str) -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        let p = d.path();
        fs::write(p.join("server.toml"), "[guild]\nname = \"T\"\n").unwrap();
        fs::write(p.join("bot.toml"), format!("name = \"T\"\nadmin_role = \"admin\"\n{bot}")).unwrap();
        fs::write(
            p.join("roles.toml"),
            "[[role]]\nkey = \"admin\"\nname = \"Admin\"\n\n[[role]]\nkey = \"chair\"\nname = \"Chair\"\n",
        )
        .unwrap();
        fs::write(p.join("members.toml"), "chair = [\"7\"]\n").unwrap();
        d
    }

    #[test]
    fn listing_other_roles_leaves_the_admin_role_alone() {
        let d = dir("");
        let c = Config::load(d.path()).unwrap();
        assert!(!c.desired.holders.contains_key("admin"), "nobody would be left an admin: {:?}", c.desired.holders);

        let d = dir("[[position]]\nrole = \"chair\"\nadmin = true\n");
        let c = Config::load(d.path()).unwrap();
        assert_eq!(c.desired.holders["admin"], [Snowflake(7)].into(), "admin positions manage it");

        let d = dir("");
        fs::write(d.path().join("members.toml"), "chair = [\"7\"]\nadmin = [\"8\"]\n").unwrap();
        let c = Config::load(d.path()).unwrap();
        assert_eq!(c.desired.holders["admin"], [Snowflake(8)].into(), "or listing it");
    }
}

#[cfg(test)]
mod path_tests {
    use super::inside;

    #[test]
    fn config_files_stay_inside_the_folder() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("content")).unwrap();
        std::fs::write(d.path().join("content/ok.md"), "hi").unwrap();
        assert!(inside(d.path(), "content/ok.md").is_ok());
        for bad in ["/etc/hostname", "../secret.env", "content/../../x", "/proc/self/environ"] {
            assert!(inside(d.path(), bad).is_err(), "{bad} must be refused");
        }
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("/etc/hostname", d.path().join("content/link.md")).unwrap();
            assert!(inside(d.path(), "content/link.md").unwrap_err().to_string().contains("symlink"));
            std::os::unix::fs::symlink("/etc", d.path().join("linkdir")).unwrap();
            assert!(inside(d.path(), "linkdir/hostname").is_err(), "symlinked folders too");
        }
    }

    /// Red team: a symlink pushed to the repo (`members.toml` → the bot's
    /// environment) would have the bot read it, and quote it in the error it
    /// posts; one in `content/` (→ `.git/config`) would have an edit write
    /// through it, and git config can run commands.
    #[cfg(unix)]
    #[test]
    fn a_config_with_symlinks_is_never_read_or_written_through() {
        use std::os::unix::fs::symlink;
        let d = tempfile::tempdir().unwrap();
        let p = d.path();
        std::fs::write(p.join("server.toml"), "[guild]\nname = \"T\"\n").unwrap();
        std::fs::create_dir_all(p.join(".git")).unwrap();
        std::fs::write(p.join(".git/config"), "[core]\n").unwrap();
        assert!(super::Config::load(p).is_ok());
        symlink("/proc/self/environ", p.join("members.toml")).unwrap();
        let err = super::Config::load(p).unwrap_err().to_string();
        assert!(err.contains("members.toml") && err.contains("symlink"), "{err}");
        std::fs::remove_file(p.join("members.toml")).unwrap();

        std::fs::create_dir_all(p.join("content")).unwrap();
        symlink("../.git/config", p.join("content/welcome.tmp")).unwrap();
        assert!(super::Config::load(p).is_err());
        let op = crate::ops::Op::SetServer { set: crate::ops::ServerField::Name("[core]\nfsmonitor = evil".into()) };
        assert!(crate::ops::apply_op(p, &op).unwrap_err().to_string().contains("symlink"));
        assert_eq!(std::fs::read_to_string(p.join(".git/config")).unwrap(), "[core]\n", "nothing written through it");

        // Writes never go through a link left where the temporary file goes.
        std::fs::remove_file(p.join("content/welcome.tmp")).unwrap();
        symlink("../.git/config", p.join("content/welcome.tmp")).unwrap();
        super::write_atomic(&p.join("content/welcome.md"), "hello").unwrap();
        assert_eq!(std::fs::read_to_string(p.join(".git/config")).unwrap(), "[core]\n");
        assert_eq!(std::fs::read_to_string(p.join("content/welcome.md")).unwrap(), "hello");
    }
}

#[cfg(test)]
mod broken_tests {
    #[test]
    fn a_broken_config_still_names_its_log_channel() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("server.toml"), "not toml [").unwrap();
        std::fs::write(d.path().join("bot.toml"), "name = \"T\"\n[log]\nbot = \"bot-log\"\n").unwrap();
        std::fs::write(d.path().join("lock.toml"), "[channels]\nbot-log = \"77\"\n").unwrap();
        assert!(super::Config::load(d.path()).is_err());
        assert_eq!(super::log_channel(d.path()), Some(super::Snowflake(77)));
    }
}

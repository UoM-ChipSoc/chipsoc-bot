//! Edits that keep the files' comments and layout (toml_edit or append-only).

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use toml_edit::{Array, ArrayOfTables, DocumentMut, Item, Table, value};

use crate::schema::{CategoryFile, Channel, Project, Projects, Role, Roles};
use crate::{category_paths, read_text, write_atomic};

fn read_doc(path: &Path) -> Result<DocumentMut> {
    let text = read_text(path)?.unwrap_or_default();
    text.parse().with_context(|| format!("parsing {}", path.display()))
}

fn append(path: &Path, block: &str) -> Result<()> {
    let mut text = read_text(path)?.unwrap_or_default();
    if !text.is_empty() && !text.ends_with("\n\n") {
        text.push_str(if text.ends_with('\n') { "\n" } else { "\n\n" });
    }
    text.push_str(block);
    write_atomic(path, &text)
}

/// A new project with the usual defaults: team and lead roles, and the
/// starting channels from bot.toml (else general + voice).
pub fn new_project(
    name: &str,
    blurb: &str,
    area: &str,
    status: crate::schema::ProjectStatus,
    bot: &crate::BotConfig,
) -> Project {
    let defaults = bot.projects.as_ref().map(|p| p.default_channels.clone()).unwrap_or_default();
    let channels = if defaults.is_empty() { vec!["general".into(), "voice".into()] } else { defaults };
    Project {
        slug: crate::slug(name.trim()),
        name: name.trim().to_owned(),
        blurb: blurb.trim().to_owned(),
        kind: area.trim().to_owned(),
        status,
        team_role: true,
        lead_role: true,
        channels,
        ..Default::default()
    }
}

pub fn add_project(dir: &Path, p: &Project) -> Result<()> {
    let path = dir.join("projects.toml");
    let existing: Projects = toml::from_str(&read_text(&path)?.unwrap_or_default())?;
    if existing.projects.iter().any(|x| x.slug == p.slug) {
        bail!("a project `{}` already exists", p.slug);
    }
    append(&path, &toml::to_string_pretty(&Projects { projects: vec![p.clone()] })?)
}

fn project_table<'a>(doc: &'a mut DocumentMut, slug: &str) -> Result<&'a mut Table> {
    doc.get_mut("project")
        .and_then(Item::as_array_of_tables_mut)
        .and_then(|a| a.iter_mut().find(|t| t.get("slug").and_then(|s| s.as_str()) == Some(slug)))
        .with_context(|| format!("no project `{slug}`"))
}

/// The list of user ids at `key`, made empty if it isn't there.
fn id_list<'a>(t: &'a mut Table, key: &str) -> Result<&'a mut Array> {
    if !t.contains_key(key) {
        t.insert(key, value(Array::new()));
    }
    t[key].as_array_mut().with_context(|| format!("`{key}` isn't a list"))
}

/// Where `user` is in a list of ids (written as strings or numbers).
pub(crate) fn find_id(list: &Array, user: u64) -> Option<usize> {
    list.iter().position(|v| {
        let id = v.as_str().and_then(|s| s.parse().ok()).or_else(|| v.as_integer().and_then(|i| u64::try_from(i).ok()));
        id == Some(user)
    })
}

/// Add someone to a list of ids; false if they're already on it.
fn add_id(list: &mut Array, user: u64) -> bool {
    let new = find_id(list, user).is_none();
    if new {
        list.push(user.to_string());
    }
    new
}

/// Give someone a role in members.toml. Returns false if they already hold it there.
pub fn add_holder(dir: &Path, role: &str, user: u64) -> Result<bool> {
    let path = dir.join("members.toml");
    let mut doc = read_doc(&path)?;
    let added = add_id(id_list(doc.as_table_mut(), role)?, user);
    if added {
        write_atomic(&path, &doc.to_string())?;
    }
    Ok(added)
}

/// Take a role off someone in members.toml. Returns false if they weren't listed.
/// An emptied list stays: the role is still managed, held by nobody.
pub fn remove_holder(dir: &Path, role: &str, user: u64) -> Result<bool> {
    let path = dir.join("members.toml");
    let mut doc = read_doc(&path)?;
    let Some(list) = doc.get_mut(role).and_then(Item::as_array_mut) else { return Ok(false) };
    let Some(i) = find_id(list, user) else { return Ok(false) };
    list.remove(i);
    write_atomic(&path, &doc.to_string())?;
    Ok(true)
}

/// Add someone to a project's `leads`. Returns false if they're already a lead.
pub fn add_project_lead(dir: &Path, slug: &str, user: u64) -> Result<bool> {
    let path = dir.join("projects.toml");
    let mut doc = read_doc(&path)?;
    let added = add_id(id_list(project_table(&mut doc, slug)?, "leads")?, user);
    if added {
        write_atomic(&path, &doc.to_string())?;
    }
    Ok(added)
}

/// Where someone is listed in the config.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Listings {
    /// members.toml role keys.
    pub roles: Vec<String>,
    /// Slugs of the projects they lead.
    pub leads: Vec<String>,
}

impl Listings {
    pub fn is_empty(&self) -> bool {
        self.roles.is_empty() && self.leads.is_empty()
    }
}

/// Where someone is listed in members.toml and project leads.
pub fn listings(dir: &Path, user: u64) -> Result<Listings> {
    unlist_in(dir, user, false)
}

/// Take someone out of members.toml and every project's leads; says where they were.
pub fn unlist(dir: &Path, user: u64) -> Result<Listings> {
    unlist_in(dir, user, true)
}

fn unlist_in(dir: &Path, user: u64, write: bool) -> Result<Listings> {
    let mut out = Listings::default();
    let path = dir.join("members.toml");
    let mut doc = read_doc(&path)?;
    for (role, item) in doc.as_table_mut().iter_mut() {
        if let Some(list) = item.as_array_mut()
            && let Some(i) = find_id(list, user)
        {
            list.remove(i);
            out.roles.push(role.to_string());
        }
    }
    if write && !out.roles.is_empty() {
        write_atomic(&path, &doc.to_string())?;
    }
    let path = dir.join("projects.toml");
    let mut doc = read_doc(&path)?;
    for t in doc.get_mut("project").and_then(Item::as_array_of_tables_mut).into_iter().flat_map(|a| a.iter_mut()) {
        let slug = t.get("slug").and_then(|s| s.as_str()).unwrap_or_default().to_owned();
        if let Some(list) = t.get_mut("leads").and_then(Item::as_array_mut)
            && let Some(i) = find_id(list, user)
        {
            list.remove(i);
            out.leads.push(slug);
        }
    }
    if write && !out.leads.is_empty() {
        write_atomic(&path, &doc.to_string())?;
    }
    Ok(out)
}

pub fn add_role(dir: &Path, role: &Role) -> Result<()> {
    let path = dir.join("roles.toml");
    let roles: Roles = toml::from_str(&read_text(&path)?.unwrap_or_default())?;
    if roles.roles.iter().any(|r| r.key == role.key) {
        bail!("a role `{}` already exists", role.key);
    }
    append(&path, &toml::to_string_pretty(&Roles { roles: vec![role.clone()] })?)
}

/// Category files as (key, name, path), in order.
pub fn category_files(dir: &Path) -> Result<Vec<(String, String, PathBuf)>> {
    let mut out = Vec::new();
    for path in category_paths(dir)? {
        let cf: CategoryFile =
            toml::from_str(&read_text(&path)?.unwrap_or_default()).with_context(|| format!("in {}", path.display()))?;
        out.push((cf.category.key, cf.category.name, path));
    }
    Ok(out)
}

pub fn add_channel(dir: &Path, category: Option<&str>, ch: &Channel) -> Result<()> {
    let snap = crate::load_snapshot(dir)?;
    if snap.channel(&ch.key).is_some() {
        bail!("a channel `{}` already exists", ch.key);
    }
    let path = match category {
        Some(cat) => {
            category_files(dir)?.into_iter().find(|(k, ..)| k == cat).with_context(|| format!("no category `{cat}`"))?.2
        }
        None => dir.join("channels.toml"),
    };
    #[derive(serde::Serialize)]
    struct One<'a> {
        channel: [&'a Channel; 1],
    }
    append(&path, &toml::to_string_pretty(&One { channel: [ch] })?)
}

/// Things `open` can jump to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Server,
    Bot,
    Roles,
    Role,
    Category,
    Channel,
    Onboarding,
    Join,
    Screening,
    Automod,
    Projects,
    Project,
    Federation,
    Content,
}

impl Kind {
    pub const ALL: [(&'static str, Self); 14] = [
        ("server", Self::Server),
        ("bot", Self::Bot),
        ("roles", Self::Roles),
        ("role", Self::Role),
        ("category", Self::Category),
        ("channel", Self::Channel),
        ("onboarding", Self::Onboarding),
        ("join", Self::Join),
        ("screening", Self::Screening),
        ("automod", Self::Automod),
        ("projects", Self::Projects),
        ("project", Self::Project),
        ("federation", Self::Federation),
        ("content", Self::Content),
    ];

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.iter().find(|(n, _)| *n == s).map(|(_, k)| *k)
    }

    /// Whether this kind names one object (needs a key).
    pub fn keyed(self) -> bool {
        matches!(self, Self::Role | Self::Category | Self::Channel | Self::Project | Self::Content)
    }
}

/// Where something is defined: (file, 1-based line).
pub fn locate(dir: &Path, kind: Kind, key: Option<&str>) -> Result<(PathBuf, usize)> {
    let line_of = |path: &Path, needle: &str| -> Result<Option<usize>> {
        Ok(read_text(path)?.and_then(|t| t.lines().position(|l| l.trim() == needle)).map(|i| i + 1))
    };
    let need = || key.with_context(|| "which one? give its key".to_owned());
    let whole = |f: &str| Ok((dir.join(f), 1));
    match kind {
        Kind::Server => whole("server.toml"),
        Kind::Bot => whole("bot.toml"),
        Kind::Roles => whole("roles.toml"),
        Kind::Onboarding => whole("onboarding.toml"),
        Kind::Join => whole("join.toml"),
        Kind::Screening => whole("screening.toml"),
        Kind::Automod => whole("automod.toml"),
        Kind::Projects => whole("projects.toml"),
        Kind::Federation => whole("federation.toml"),
        Kind::Content => Ok((dir.join("content").join(format!("{}.md", need()?)), 1)),
        Kind::Role | Kind::Project => {
            let (file, field) = if kind == Kind::Role { ("roles.toml", "key") } else { ("projects.toml", "slug") };
            let k = need()?;
            let path = dir.join(file);
            let line =
                line_of(&path, &format!("{field} = \"{k}\""))?.with_context(|| format!("no {file} entry `{k}`"))?;
            Ok((path, line))
        }
        Kind::Category | Kind::Channel => {
            let k = need()?;
            let mut files: Vec<PathBuf> = category_paths(dir)?;
            files.push(dir.join("channels.toml"));
            for path in files {
                if let Some(line) = line_of(&path, &format!("key = \"{k}\""))? {
                    return Ok((path, line));
                }
            }
            bail!("no category or channel `{k}`")
        }
    }
}

/// Reorder `[[section]]` entries in a file to follow `keys`; others keep their place at the end.
fn reorder_tables(path: &Path, section: &str, keys: &[String]) -> Result<()> {
    let mut doc = read_doc(path)?;
    let Some(arr) = doc.get_mut(section).and_then(Item::as_array_of_tables_mut) else { return Ok(()) };
    let mut tables: Vec<Table> = arr.iter().cloned().collect();
    let rank = |t: &Table| {
        let k = t.get("key").and_then(|k| k.as_str()).unwrap_or_default();
        keys.iter().position(|x| x == k).unwrap_or(usize::MAX)
    };
    // toml_edit writes tables by their document position, so hand out the
    // existing positions in the new order.
    let mut slots: Vec<Option<isize>> = tables.iter().map(Table::position).collect();
    slots.sort();
    tables.sort_by_key(rank); // stable: unknown keys keep their relative order
    let mut sorted = ArrayOfTables::new();
    for (mut t, pos) in tables.into_iter().zip(slots) {
        t.set_position(pos);
        sorted.push(t);
    }
    *arr = sorted;
    write_atomic(path, &doc.to_string())
}

/// Put roles in this order (top of the hierarchy first).
pub fn reorder_roles(dir: &Path, keys: &[String]) -> Result<()> {
    reorder_tables(&dir.join("roles.toml"), "role", keys)
}

/// Put a category's channels (or top-level ones, for `None`) in this order.
pub fn reorder_channels(dir: &Path, category: Option<&str>, keys: &[String]) -> Result<()> {
    let path = match category {
        Some(cat) => {
            category_files(dir)?.into_iter().find(|(k, ..)| k == cat).with_context(|| format!("no category `{cat}`"))?.2
        }
        None => dir.join("channels.toml"),
    };
    reorder_tables(&path, "channel", keys)
}

/// Rename category files so they sort in this order (`10-…`, `20-…`).
/// Categories not listed (generated ones) are skipped.
pub fn reorder_categories(dir: &Path, keys: &[String]) -> Result<()> {
    let files = category_files(dir)?;
    let mut n = 0;
    let mut moves = Vec::new();
    for key in keys {
        let Some((_, _, path)) = files.iter().find(|(k, ..)| k == key) else { continue };
        n += 10;
        let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or(key);
        let rest = stem
            .split_once('-')
            .map_or(stem, |(prefix, rest)| if prefix.chars().all(|c| c.is_ascii_digit()) { rest } else { stem });
        moves.push((path.clone(), path.with_file_name(format!("{n:02}-{rest}.toml"))));
    }
    // Two passes so a rename never overwrites a file still waiting to move.
    let tmp: Vec<(PathBuf, PathBuf)> =
        moves.iter().map(|(from, to)| (from.with_extension("moving"), to.clone())).collect();
    for ((from, _), (t, _)) in moves.iter().zip(&tmp) {
        fs::rename(from, t)?;
        crate::git::wrote(from);
    }
    for (t, to) in tmp {
        fs::rename(t, &to)?;
        crate::git::wrote(&to);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::ProjectStatus;

    fn dir() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        let p = d.path();
        fs::create_dir_all(p.join("categories")).unwrap();
        fs::write(p.join("server.toml"), "[guild]\nname = \"T\"\n").unwrap();
        fs::write(
            p.join("roles.toml"),
            "# roles\n[[role]]\nkey = \"a\"\nname = \"A\"\n\n[[role]]\nkey = \"b\"\nname = \"B\"\n",
        )
        .unwrap();
        fs::write(p.join("categories/10-one.toml"), "[category]\nkey = \"one\"\nname = \"One\"\n\n[[channel]]\nkey = \"x\"\nname = \"x\"\n\n[[channel]]\nkey = \"y\"\nname = \"y\"\n").unwrap();
        fs::write(p.join("categories/20-two.toml"), "[category]\nkey = \"two\"\nname = \"Two\"\n").unwrap();
        d
    }

    #[test]
    fn project_add_keeps_comments() {
        let d = dir();
        fs::write(d.path().join("projects.toml"), "# keep me\n").unwrap();
        let p = Project { slug: "golem".into(), name: "Golem".into(), ..Default::default() };
        add_project(d.path(), &p).unwrap();
        assert!(add_project(d.path(), &p).is_err(), "duplicate rejected");
        let op = crate::ops::Op::SetProject {
            slug: "golem".into(),
            set: crate::ops::ProjectField::Status(ProjectStatus::Active),
        };
        crate::ops::apply_op(d.path(), &op).unwrap();
        let c = crate::Config::load(d.path()).unwrap();
        assert_eq!(c.projects[0].status, ProjectStatus::Active);
        assert!(fs::read_to_string(d.path().join("projects.toml")).unwrap().starts_with("# keep me"));
    }

    #[test]
    fn holders_and_leads_keep_comments() {
        let d = dir();
        let p = d.path();
        fs::write(p.join("members.toml"), "# who holds what\nchair = [\"7\"] # elected\n").unwrap();
        assert!(add_holder(p, "chair", 8).unwrap());
        assert!(!add_holder(p, "chair", 7).unwrap(), "already listed");
        assert!(add_holder(p, "a", 9).unwrap(), "a new list");
        assert!(remove_holder(p, "chair", 7).unwrap());
        assert!(!remove_holder(p, "chair", 7).unwrap());
        assert!(!remove_holder(p, "ghost", 7).unwrap());
        let text = fs::read_to_string(p.join("members.toml")).unwrap();
        assert!(text.starts_with("# who holds what") && text.contains("# elected"), "{text}");
        let c = crate::Config::load(p).unwrap();
        assert_eq!(c.desired.holders["chair"].iter().map(|s| s.0).collect::<Vec<_>>(), [8]);
        assert_eq!(c.desired.holders["a"].iter().map(|s| s.0).collect::<Vec<_>>(), [9]);

        fs::write(p.join("projects.toml"), "# keep me\n[[project]]\nslug = \"golem\"\nname = \"Golem\"\nleads = [5]\n")
            .unwrap();
        assert!(add_project_lead(p, "golem", 6).unwrap());
        assert!(!add_project_lead(p, "golem", 5).unwrap(), "numbers count too");
        assert!(add_project_lead(p, "ghost", 6).is_err());
        let c = crate::Config::load(p).unwrap();
        assert_eq!(c.projects[0].leads.iter().map(|s| s.0).collect::<Vec<_>>(), [5, 6]);
        assert!(fs::read_to_string(p.join("projects.toml")).unwrap().starts_with("# keep me"));
    }

    #[test]
    fn unlisting_a_leaver() {
        let d = dir();
        let p = d.path();
        fs::write(p.join("members.toml"), "# c\na = [\"7\", \"8\"]\nb = [\"8\"]\n").unwrap();
        fs::write(p.join("projects.toml"), "[[project]]\nslug = \"golem\"\nname = \"Golem\"\nleads = [\"7\"]\n")
            .unwrap();
        let want = Listings { roles: vec!["a".into()], leads: vec!["golem".into()] };
        assert_eq!(listings(p, 7).unwrap(), want);
        assert_eq!(listings(p, 7).unwrap(), want, "listings changes nothing");
        assert_eq!(unlist(p, 7).unwrap(), want);
        assert!(listings(p, 7).unwrap().is_empty());
        assert_eq!(listings(p, 8).unwrap().roles, ["a", "b"]);
        assert!(fs::read_to_string(p.join("members.toml")).unwrap().starts_with("# c"));
        assert!(unlist(p, 9).unwrap().is_empty());
    }

    #[test]
    fn add_channel_then_locate() {
        let d = dir();
        let ch = Channel { key: "rtl".into(), name: "rtl".into(), ..Default::default() };
        add_channel(d.path(), Some("two"), &ch).unwrap();
        let (path, line) = locate(d.path(), Kind::Channel, Some("rtl")).unwrap();
        assert!(path.ends_with("20-two.toml") && line > 3);
        assert!(add_channel(d.path(), Some("one"), &ch).is_err(), "duplicate rejected");
    }

    #[test]
    fn reorder() {
        let d = dir();
        let p = d.path();
        reorder_roles(p, &["b".into(), "a".into()]).unwrap();
        reorder_channels(p, Some("one"), &["y".into(), "x".into()]).unwrap();
        reorder_categories(p, &["two".into(), "one".into()]).unwrap();
        let s = crate::load_snapshot(p).unwrap();
        assert_eq!(s.roles.roles.iter().map(|r| r.key.as_str()).collect::<Vec<_>>(), ["b", "a"]);
        assert_eq!(s.categories.iter().map(|c| c.category.key.as_str()).collect::<Vec<_>>(), ["two", "one"]);
        assert_eq!(s.categories[1].channels[0].key, "y");
        assert!(fs::read_to_string(p.join("roles.toml")).unwrap().contains("# roles"));
    }
}

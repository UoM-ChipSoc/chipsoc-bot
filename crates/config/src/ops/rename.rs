//! Renaming a project's slug. Every key a project makes comes from its slug
//! (`proj-<slug>-team`, `proj-<slug>`, …), so the rename moves those keys in
//! lock.toml and wherever the config names them, and Discord keeps the same
//! roles, category, channels and forum post.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use toml_edit::{DocumentMut, Item, Value};

use crate::schema::{Lock, Project, Projects};
use crate::{KEY_RULE, LOCK_FILE, is_key, read_or_default, write_atomic};

/// Old key → new key, for every key a project's slug makes.
pub type Renames = BTreeMap<String, String>;

/// The keys `p` makes, and those `lock` knows that are its (channels it had
/// once included), each mapped to the key it gets under slug `to`. `others`
/// are the other projects: one whose slug starts `<slug>-` owns its own keys.
pub fn project_renames(p: &Project, to: &str, others: &[Project], lock: &Lock) -> Renames {
    let (from_cat, to_cat) = (format!("proj-{}", p.slug), format!("proj-{to}"));
    let prefix = format!("{from_cat}-");
    let longer: Vec<&Project> = others.iter().filter(|q| q.slug.starts_with(&format!("{}-", p.slug))).collect();
    let theirs = |k: &str| longer.iter().any(|q| k == q.category_key() || k.starts_with(&format!("proj-{}-", q.slug)));
    let mut keys: Vec<String> = vec![p.team_role_key(), p.lead_role_key(), p.category_key()];
    keys.extend(p.channels.iter().map(|c| p.channel_key(c)));
    keys.extend(
        lock_keys(lock).filter(|k| (*k == from_cat || k.starts_with(&prefix)) && !theirs(k)).map(str::to_owned),
    );
    keys.into_iter().map(|k| (k.clone(), format!("{to_cat}{}", &k[from_cat.len()..]))).collect()
}

fn lock_keys(lock: &Lock) -> impl Iterator<Item = &str> {
    lock.roles
        .keys()
        .chain(lock.categories.keys())
        .chain(lock.channels.keys())
        .chain(lock.posts.keys())
        .chain(lock.archived.keys())
        .map(String::as_str)
}

/// Move a project's entries in `lock` (ids, posts, what was last applied) to their new keys.
pub fn rename_in_lock(lock: &mut Lock, from: &str, to: &str, renames: &Renames) {
    fn moved<V>(map: &mut BTreeMap<String, V>, renames: &Renames) {
        for (old, new) in renames {
            if let Some(v) = map.remove(old) {
                map.insert(new.clone(), v);
            }
        }
    }
    moved(&mut lock.roles, renames);
    moved(&mut lock.categories, renames);
    moved(&mut lock.channels, renames);
    moved(&mut lock.posts, renames);
    moved(&mut lock.archived, renames);
    if let Some(t) = lock.project_posts.remove(from) {
        lock.project_posts.insert(to.to_owned(), t);
    }
    // What was last applied, `kind:key`: the project post is keyed by slug.
    let applied = std::mem::take(&mut lock.applied.hashes);
    lock.applied.hashes = applied
        .into_iter()
        .map(|(obj, hash)| {
            let renamed = obj.split_once(':').and_then(|(kind, key)| {
                let new = if kind == "project-post" {
                    (key == from).then(|| to.to_owned())
                } else {
                    renames.get(key).cloned()
                };
                new.map(|n| format!("{kind}:{n}"))
            });
            (renamed.unwrap_or(obj), hash)
        })
        .collect();
}

/// Rename project `from` to `to` in the config at `dir`: projects.toml, lock.toml,
/// and every other file naming one of its keys (members.toml, overwrites,
/// partner role maps, `{#key}`/`{@key}` mentions and `apply:` buttons in posts).
pub(super) fn rename_project(dir: &Path, from: &str, to: &str) -> Result<()> {
    if !is_key(to) {
        bail!("a project's slug is made of {KEY_RULE}");
    }
    if from == to {
        bail!("`{from}` already has that slug");
    }
    let projects: Projects = read_or_default(&dir.join("projects.toml"))?;
    let Some(p) = projects.projects.iter().find(|p| p.slug == from) else { bail!("no project `{from}`") };
    if projects.projects.iter().any(|p| p.slug == to) {
        bail!("there's already a project `{to}`");
    }
    let lock_path = dir.join(LOCK_FILE);
    let mut lock: Lock = read_or_default(&lock_path)?;
    let others: Vec<Project> = projects.projects.iter().filter(|q| q.slug != from).cloned().collect();
    let renames = project_renames(p, to, &others, &lock);
    let taken = |k: &str| lock_keys(&lock).any(|l| l == k) || lock.project_posts.contains_key(to);
    if let Some(new) = renames.values().find(|k| taken(k)) {
        bail!("`{new}` is already in lock.toml, so `{to}` can't be used");
    }

    super::write::set_slug(dir, from, to)?;
    if lock_path.exists() {
        rename_in_lock(&mut lock, from, to, &renames);
        write_atomic(&lock_path, &toml::to_string_pretty(&lock).context("serialising lock.toml")?)?;
    }
    let mut files = Vec::new();
    walk(dir, Path::new(""), &mut files)?;
    for rel in files {
        let path = dir.join(&rel);
        match rel.extension().and_then(|e| e.to_str()) {
            Some("toml") if rel != Path::new(LOCK_FILE) => in_toml(&path, &renames)?,
            Some("md") => in_markdown(&path, from, to, &renames)?,
            _ => {}
        }
    }
    Ok(())
}

fn walk(dir: &Path, rel: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    for entry in fs::read_dir(dir.join(rel)).with_context(|| format!("reading {}", dir.join(rel).display()))? {
        let entry = entry?;
        if entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        let path = rel.join(entry.file_name());
        if entry.file_type()?.is_dir() { walk(dir, &path, out)? } else { out.push(path) }
    }
    Ok(())
}

/// Every string that is exactly an old key, and every table key that is
/// (members.toml lists holders under role keys).
fn in_toml(path: &Path, renames: &Renames) -> Result<()> {
    let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    if !renames.keys().any(|k| text.contains(k.as_str())) {
        return Ok(());
    }
    let mut doc: DocumentMut = text.parse().with_context(|| format!("parsing {}", path.display()))?;
    in_table(doc.as_table_mut(), renames);
    write_atomic(path, &doc.to_string())
}

fn in_table(t: &mut dyn toml_edit::TableLike, renames: &Renames) {
    let keys: Vec<String> = t.iter().map(|(k, _)| k.to_owned()).collect();
    for k in keys {
        if let Some(item) = t.get_mut(&k) {
            in_item(item, renames);
        }
        if let Some(new) = renames.get(&k)
            && !t.contains_key(new)
            && let Some(item) = t.remove(&k)
        {
            t.insert(new, item);
        }
    }
}

fn in_item(item: &mut Item, renames: &Renames) {
    match item {
        Item::Value(v) => in_value(v, renames),
        Item::Table(t) => in_table(t, renames),
        Item::ArrayOfTables(a) => a.iter_mut().for_each(|t| in_table(t, renames)),
        Item::None => {}
    }
}

fn in_value(v: &mut Value, renames: &Renames) {
    match v {
        Value::String(s) => {
            if let Some(new) = renames.get(s.value()) {
                let decor = s.decor().clone();
                *s = toml_edit::Formatted::new(new.clone());
                *s.decor_mut() = decor;
            }
        }
        Value::Array(a) => a.iter_mut().for_each(|v| in_value(v, renames)),
        Value::InlineTable(t) => in_table(t, renames),
        _ => {}
    }
}

/// `{#key}`, `{@key}` and `{button:apply:<slug>…}` in a post or brief.
fn in_markdown(path: &Path, from: &str, to: &str, renames: &Renames) -> Result<()> {
    let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let mut out = text.clone();
    for (old, new) in renames {
        for sigil in ['#', '@'] {
            out = out.replace(&format!("{{{sigil}{old}}}"), &format!("{{{sigil}{new}}}"));
        }
    }
    for end in ['|', '}'] {
        out = out.replace(&format!("{{button:apply:{from}{end}"), &format!("{{button:apply:{to}{end}"));
    }
    if out != text { write_atomic(path, &out) } else { Ok(()) }
}

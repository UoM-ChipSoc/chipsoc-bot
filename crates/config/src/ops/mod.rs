//! Typed edits to the config files, shared by every front end (web, TUI, CLI,
//! the bot). Each [`Op`] is one change a person can make; [`apply_op`] makes
//! it on a copy first and refuses it if the config would stop checking clean,
//! then writes the files it changed, keeping their comments and order.

mod rename;
mod schema;
mod write;

#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

pub use rename::{Renames, project_renames, rename_in_lock};
pub use schema::{Choices, PostFile, Schema, schema};

use crate::authoring::{Problems, check};
use crate::bot::{
    DriftMode, Events, Feeds, Help, Ideas, Position, Profile, ProjectsConf, Question, Resources, Review, Web,
};
use crate::schema::{
    AutoModRule, Channel, ChannelKind, OnbOption, OnbPrompt, Overwrite, Peer, ProjectStatus, Role, Snowflake,
};

/// One change to the config.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Op {
    /// server.toml's `[guild]`.
    SetServer {
        set: ServerField,
    },

    AddRole {
        role: Role,
    },
    SetRole {
        key: String,
        set: RoleField,
    },
    MoveRole {
        key: String,
        to: Move,
    },
    /// Out of the config; Discord keeps it until an apply with `--delete`.
    RemoveRole {
        key: String,
    },
    /// Put roles in this order (top first); ones not named keep their place after.
    OrderRoles {
        keys: Vec<String>,
    },
    /// Put categories in this order; ones not named (made by projects) are left alone.
    OrderCategories {
        keys: Vec<String>,
    },
    /// Put a category's channels (none: the top-level ones) in this order.
    OrderChannels {
        category: Option<String>,
        keys: Vec<String>,
    },

    /// A new, empty category file after the others.
    AddCategory {
        key: String,
        name: String,
    },
    RenameCategory {
        key: String,
        name: String,
    },
    MoveCategory {
        key: String,
        to: Move,
    },
    /// Who can see and do what in the category; its channels inherit these.
    SetCategoryOverwrites {
        key: String,
        overwrites: Vec<Overwrite>,
    },
    /// Only an empty one; apply archives it.
    RemoveCategory {
        key: String,
    },

    /// At the end of `category` (none: outside any category).
    AddChannel {
        category: Option<String>,
        channel: Channel,
    },
    SetChannel {
        key: String,
        set: ChannelField,
    },
    /// Within its category.
    MoveChannel {
        key: String,
        to: Move,
    },
    /// To the end of another category (none: outside any).
    MoveChannelTo {
        key: String,
        category: Option<String>,
    },
    /// Out of the config; apply moves it to the archive.
    RemoveChannel {
        key: String,
    },

    AddPosition {
        position: Position,
    },
    SetPosition {
        role: String,
        set: PositionField,
    },
    MovePosition {
        role: String,
        to: Move,
    },
    RemovePosition {
        role: String,
    },

    /// members.toml: `user` holds `role`.
    AddHolder {
        role: String,
        user: Snowflake,
    },
    RemoveHolder {
        role: String,
        user: Snowflake,
    },

    /// With the usual channels and roles (see `authoring::new_project`).
    AddProject {
        name: String,
        #[serde(default)]
        blurb: String,
        #[serde(default)]
        area: String,
        #[serde(default)]
        status: ProjectStatus,
    },
    SetProject {
        slug: String,
        set: ProjectField,
    },
    /// A new slug, keeping its roles, channels and post: their keys move in
    /// lock.toml and wherever the config names them.
    RenameProject {
        from: String,
        to: String,
    },
    AddLead {
        slug: String,
        user: Snowflake,
    },
    RemoveLead {
        slug: String,
        user: Snowflake,
    },

    /// The markdown kept posted in a channel (made `content/<key>.md` if it has none).
    SetPost {
        channel: String,
        body: String,
    },
    /// A project's brief (made `content/projects/<slug>.md` if it has none).
    SetBrief {
        slug: String,
        body: String,
    },

    SetOnboarding {
        set: OnboardingField,
    },
    AddPrompt {
        prompt: OnbPrompt,
    },
    /// Prompts and options are counted from 0, in file order.
    SetPrompt {
        prompt: usize,
        set: PromptField,
    },
    MovePrompt {
        prompt: usize,
        to: Move,
    },
    RemovePrompt {
        prompt: usize,
    },
    AddOption {
        prompt: usize,
        option: OnbOption,
    },
    SetOption {
        prompt: usize,
        option: usize,
        set: OptionField,
    },
    MoveOption {
        prompt: usize,
        option: usize,
        to: Move,
    },
    RemoveOption {
        prompt: usize,
        option: usize,
    },

    SetScreening {
        set: ScreeningField,
    },

    AddRule {
        rule: AutoModRule,
    },
    /// Replaces the rule with the same key.
    SetRule {
        rule: AutoModRule,
    },
    RemoveRule {
        key: String,
    },

    SetBot {
        set: BotField,
    },
    /// Turn a bot.toml feature on (with its settings) or off.
    SetSection {
        section: Section,
    },

    SetFederation {
        set: FederationField,
    },
    /// Names of environment variables only; secrets never go in the config.
    AddPeer {
        peer: Peer,
    },
    /// Replaces the peer with the same name.
    SetPeer {
        peer: Peer,
    },
    RemovePeer {
        name: String,
    },
}

/// Where to move something in its list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Move {
    /// Earlier: higher in the role list, nearer the top of the channel list.
    Up,
    Down,
    /// To this place, counting from 0.
    To(usize),
}

/// A field of server.toml's `[guild]`. Channels are channel keys; none unsets it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServerField {
    Name(String),
    Description(Option<String>),
    SystemChannel(Option<String>),
    RulesChannel(Option<String>),
    UpdatesChannel(Option<String>),
    AfkChannel(Option<String>),
    VerificationLevel(Option<u8>),
    ExplicitContentFilter(Option<u8>),
    DefaultMessageNotifications(Option<u8>),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoleField {
    Name(String),
    /// `#rrggbb`, or none.
    Color(Option<String>),
    Hoist(bool),
    Mentionable(bool),
    Permissions(Vec<String>),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChannelField {
    Name(String),
    Topic(Option<String>),
    #[serde(rename = "type")]
    Kind(ChannelKind),
    /// Seconds; 0 is off.
    Slowmode(u16),
    Nsfw(bool),
    UserLimit(Option<u16>),
    Tags(Vec<String>),
    /// Added to the category's; an entry for the same role replaces the category's.
    Overwrites(Vec<Overwrite>),
    /// Off: ignore the category's overwrites.
    Inherit(bool),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PositionField {
    Label(Option<String>),
    Admin(bool),
    Open(bool),
    Intro(Option<String>),
    Questions(Vec<Question>),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectField {
    Name(String),
    Blurb(String),
    /// The `kind` field.
    Area(String),
    Status(ProjectStatus),
    Stage(Option<String>),
    Github(Option<String>),
    Channels(Vec<String>),
    Tags(Vec<String>),
    TeamRole(bool),
    LeadRole(bool),
    /// Taking applications.
    Applications(bool),
    LeadQuestion(bool),
    Questions(Vec<String>),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OnboardingField {
    Enabled(bool),
    Advanced(bool),
    /// None given: every channel @everyone can see.
    DefaultChannels(Vec<String>),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptField {
    Title(String),
    SingleSelect(bool),
    Required(bool),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OptionField {
    Title(String),
    Description(Option<String>),
    Emoji(Option<String>),
    /// Role key granted; empty for none.
    Role(String),
    Channels(Vec<String>),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScreeningField {
    Enabled(bool),
    Description(String),
    Label(String),
    Rules(Vec<String>),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BotField {
    Name(String),
    Timezone(String),
    AdminRole(Option<String>),
    Archive(Option<String>),
    LogPeople(Option<String>),
    LogBot(Option<String>),
    KeepDays(u32),
}

/// A bot.toml section: its settings, or none to turn it off.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Section {
    Review(Option<Review>),
    Events(Option<Events>),
    Feeds(Option<Feeds>),
    Ideas(Option<Ideas>),
    Help(Option<Help>),
    Projects(Option<ProjectsConf>),
    Profile(Option<Profile>),
    Resources(Option<Resources>),
    Web(Option<Web>),
    Drift(DriftMode),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FederationField {
    Name(String),
    Inbox(Option<String>),
    Publish(Vec<String>),
}

/// What [`apply_op`] did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Change {
    /// [`Op::describe`], or that nothing needed changing.
    pub summary: String,
    /// Files written or removed, relative to the config directory.
    pub files: Vec<String>,
    /// New warnings from `check`, and any step after the edit that failed
    /// (the edit stands; see `Engine::edit_ops`).
    pub warnings: Vec<String>,
}

impl Op {
    /// One line for people and commit messages.
    pub fn describe(&self) -> String {
        match self {
            Self::SetServer { set } => format!("Server: {}", field_text(set)),
            Self::AddRole { role } => format!("Add role {} ({})", role.name, role.key),
            Self::SetRole { key, set } => format!("Role {key}: {}", field_text(set)),
            Self::MoveRole { key, to } => format!("Move role {key} {}", move_text(*to)),
            Self::RemoveRole { key } => format!("Remove role {key}"),
            Self::OrderRoles { .. } => "Reorder roles".into(),
            Self::OrderCategories { .. } => "Reorder categories".into(),
            Self::OrderChannels { category, .. } => {
                format!("Reorder channels in {}", category.as_deref().unwrap_or("top level"))
            }
            Self::AddCategory { key, name } => format!("Add category {name} ({key})"),
            Self::RenameCategory { key, name } => format!("Rename category {key} to {name:?}"),
            Self::MoveCategory { key, to } => format!("Move category {key} {}", move_text(*to)),
            Self::SetCategoryOverwrites { key, overwrites } => {
                format!("Category {key}: permissions ({} entr{})", overwrites.len(), plural_y(overwrites.len()))
            }
            Self::RemoveCategory { key } => format!("Remove category {key}"),
            Self::AddChannel { category, channel } => {
                format!(
                    "Add {} channel {} to {}",
                    channel.kind,
                    channel.key,
                    category.as_deref().unwrap_or("top level")
                )
            }
            Self::SetChannel { key, set } => format!("Channel {key}: {}", field_text(set)),
            Self::MoveChannel { key, to } => format!("Move channel {key} {}", move_text(*to)),
            Self::MoveChannelTo { key, category } => {
                format!("Move channel {key} to {}", category.as_deref().unwrap_or("top level"))
            }
            Self::RemoveChannel { key } => format!("Remove channel {key}"),
            Self::AddPosition { position } => format!("Add position {}", position.role),
            Self::SetPosition { role, set } => format!("Position {role}: {}", field_text(set)),
            Self::MovePosition { role, to } => format!("Move position {role} {}", move_text(*to)),
            Self::RemovePosition { role } => format!("Remove position {role}"),
            Self::AddHolder { role, user } => format!("Give {role} to {user}"),
            Self::RemoveHolder { role, user } => format!("Take {role} from {user}"),
            Self::AddProject { name, .. } => format!("Add project {}", name.trim()),
            Self::SetProject { slug, set } => format!("Project {slug}: {}", field_text(set)),
            Self::RenameProject { from, to } => format!("Rename project {from} to {to}"),
            Self::AddLead { slug, user } => format!("Make {user} a lead of {slug}"),
            Self::RemoveLead { slug, user } => format!("Remove {user} from {slug}'s leads"),
            Self::SetPost { channel, .. } => format!("Edit the post in {channel}"),
            Self::SetBrief { slug, .. } => format!("Edit {slug}'s brief"),
            Self::SetOnboarding { set } => format!("Onboarding: {}", field_text(set)),
            Self::AddPrompt { prompt } => format!("Add onboarding question {:?}", prompt.title),
            Self::SetPrompt { prompt, set } => format!("Onboarding question {}: {}", prompt + 1, field_text(set)),
            Self::MovePrompt { prompt, to } => format!("Move onboarding question {} {}", prompt + 1, move_text(*to)),
            Self::RemovePrompt { prompt } => format!("Remove onboarding question {}", prompt + 1),
            Self::AddOption { prompt, option } => {
                format!("Add option {:?} to onboarding question {}", option.title, prompt + 1)
            }
            Self::SetOption { prompt, option, set } => {
                format!("Onboarding question {} option {}: {}", prompt + 1, option + 1, field_text(set))
            }
            Self::MoveOption { prompt, option, to } => {
                format!("Move option {} of onboarding question {} {}", option + 1, prompt + 1, move_text(*to))
            }
            Self::RemoveOption { prompt, option } => {
                format!("Remove option {} of onboarding question {}", option + 1, prompt + 1)
            }
            Self::SetScreening { set } => format!("Screening: {}", field_text(set)),
            Self::AddRule { rule } => format!("Add AutoMod rule {}", rule.key),
            Self::SetRule { rule } => format!("Edit AutoMod rule {}", rule.key),
            Self::RemoveRule { key } => format!("Remove AutoMod rule {key}"),
            Self::SetBot { set } => format!("Bot: {}", field_text(set)),
            Self::SetSection { section } => section_text(section),
            Self::SetFederation { set } => format!("Federation: {}", field_text(set)),
            Self::AddPeer { peer } => format!("Add partner {}", peer.name),
            Self::SetPeer { peer } => format!("Edit partner {}", peer.name),
            Self::RemovePeer { name } => format!("Remove partner {name}"),
        }
    }
}

fn plural_y(n: usize) -> &'static str {
    if n == 1 { "y" } else { "ies" }
}

fn move_text(to: Move) -> String {
    match to {
        Move::Up => "up".into(),
        Move::Down => "down".into(),
        Move::To(i) => format!("to place {}", i + 1),
    }
}

/// `name to "X"`, `hoist on`, `topic none`, from a one-entry field enum.
fn field_text<T: Serialize>(field: &T) -> String {
    let v = serde_json::to_value(field).unwrap_or_default();
    let Some((name, value)) = v.as_object().and_then(|m| m.iter().next()) else {
        return v.as_str().unwrap_or_default().replace('_', " ");
    };
    let name = name.replace('_', " ");
    match value {
        serde_json::Value::Null => format!("{name} none"),
        serde_json::Value::Bool(b) => format!("{name} {}", if *b { "on" } else { "off" }),
        serde_json::Value::Array(a) if a.iter().all(|x| x.is_string()) => {
            let items: Vec<&str> = a.iter().filter_map(|x| x.as_str()).collect();
            format!("{name} to [{}]", short(&items.join(", ")))
        }
        serde_json::Value::Array(a) => format!("{name} to {} item(s)", a.len()),
        serde_json::Value::Object(_) => format!("{name} to {}", short(&value.to_string())),
        other => format!("{name} to {}", short(&other.to_string())),
    }
}

/// `Turn on review (…)`, `Turn off feeds`, `Drift: report`.
fn section_text(s: &Section) -> String {
    let v = serde_json::to_value(s).unwrap_or_default();
    let Some((name, value)) = v.as_object().and_then(|m| m.iter().next()) else { return String::new() };
    match (s, value) {
        (Section::Drift(_), v) => format!("Drift: {}", v.as_str().unwrap_or_default()),
        (_, serde_json::Value::Null) => format!("Turn off {name}"),
        (_, v) => format!("Turn on {name} ({})", short(&v.to_string())),
    }
}

fn short(s: &str) -> String {
    if s.chars().count() <= 60 { s.to_owned() } else { s.chars().take(59).chain(['…']).collect() }
}

/// Make `op` on the config in `dir`: tried on a copy and checked first, so a
/// change that would break the config is refused (with the problems) and
/// nothing is written. Problems the config already had don't count.
pub fn apply_op(dir: &Path, op: &Op) -> Result<Change> {
    apply_ops(dir, std::slice::from_ref(op))
}

/// Make several ops as one change, on one scratch copy with one check: all
/// of them, or (if any fails, or the result wouldn't check clean) none.
pub fn apply_ops(dir: &Path, ops: &[Op]) -> Result<Change> {
    let before = check(dir)?;
    let tmp = tempfile::tempdir().context("making a scratch copy of the config")?;
    copy_dir(dir, tmp.path())?;
    let old = read_all(tmp.path())?;
    let mut closed = Vec::new();
    for op in ops {
        if let Op::SetProject { slug, set: ProjectField::Status(s) } = op
            && *s != ProjectStatus::Active
            && write::takes_applications(tmp.path(), slug)?
        {
            closed.push(slug.as_str());
        }
        write::edit(tmp.path(), op)?;
    }
    let new = read_all(tmp.path())?;
    let after = check(tmp.path())?;
    let here = |e: &String| e.replace(&tmp.path().display().to_string(), &dir.display().to_string());
    let errors: Vec<String> = after.errors.iter().map(here).filter(|e| !before.errors.contains(e)).collect();
    if !errors.is_empty() {
        return Err(Problems(errors).into());
    }
    let warnings = after.warnings.iter().map(here).filter(|w| !before.warnings.contains(w)).collect();
    let paths: BTreeSet<&PathBuf> = old.keys().chain(new.keys()).collect();
    let changed: Vec<&PathBuf> = paths.into_iter().filter(|p| old.get(*p) != new.get(*p)).collect();
    if changed.is_empty() {
        return Ok(Change { summary: "Nothing to change.".into(), files: Vec::new(), warnings });
    }
    for rel in &changed {
        if fs::read(dir.join(rel)).ok().as_ref() != old.get(*rel) {
            bail!("{} changed meanwhile; try again", rel.display());
        }
    }
    for rel in &changed {
        match new.get(*rel) {
            Some(bytes) => write_file(&dir.join(rel), bytes)?,
            None => fs::remove_file(dir.join(rel)).with_context(|| format!("removing {}", rel.display()))?,
        }
        crate::git::wrote(&dir.join(rel));
    }
    let files = changed.iter().map(|p| p.display().to_string()).collect();
    let mut summary = match ops {
        [one] => one.describe(),
        many => {
            format!("{} changes: {}", many.len(), short(&many.iter().map(Op::describe).collect::<Vec<_>>().join("; ")))
        }
    };
    if !closed.is_empty() {
        summary
            .push_str(&format!(" (applications to {} closed: only active projects take members)", closed.join(", ")));
    }
    Ok(Change { summary, files, warnings })
}

/// The config directory's files (not hidden ones like `.git`), recursively.
fn walk(dir: &Path, rel: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    for entry in fs::read_dir(dir.join(rel)).with_context(|| format!("reading {}", dir.join(rel).display()))? {
        let entry = entry?;
        if entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        let path = rel.join(entry.file_name());
        let kind = entry.file_type()?;
        if kind.is_symlink() {
            bail!(
                "`{}` is a symlink, which isn't allowed in the config; replace it with the file itself",
                path.display()
            );
        }
        if kind.is_dir() {
            walk(dir, &path, out)?;
        } else {
            out.push(path);
        }
    }
    Ok(())
}

/// Copy a config directory's files (not hidden ones like `.git` or `.env`) into `to`.
pub fn copy_dir(from: &Path, to: &Path) -> Result<()> {
    let mut files = Vec::new();
    walk(from, Path::new(""), &mut files)?;
    for rel in files {
        if let Some(parent) = to.join(&rel).parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(from.join(&rel), to.join(&rel)).with_context(|| format!("copying {}", rel.display()))?;
    }
    Ok(())
}

fn read_all(dir: &Path) -> Result<BTreeMap<PathBuf, Vec<u8>>> {
    let mut files = Vec::new();
    walk(dir, Path::new(""), &mut files)?;
    files.into_iter().map(|rel| Ok((rel.clone(), fs::read(dir.join(&rel))?))).collect()
}

/// Write via a temporary file and rename, making the directory if needed.
fn write_file(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    crate::write_new(path, bytes)
}

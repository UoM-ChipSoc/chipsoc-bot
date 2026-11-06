//! What [`Op`](super::Op)s can edit, with the current values: for front ends to build forms from.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::authoring::BUTTON_ACTIONS;
use crate::schema::{
    AutoModRule, CategoryFile, Channel, ChannelKind, Onboarding, Project, ProjectStatus, Role, Screening,
};
use crate::{BotConfig, Config, Federation, Snowflake, load_snapshot, read_or_default};

/// The config as written in its files (project roles and channels aren't
/// here: projects.toml makes them).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Schema {
    /// roles.toml, top of the hierarchy first.
    pub roles: Vec<Role>,
    /// Category files in order, each with its channels.
    pub categories: Vec<CategoryFile>,
    /// channels.toml.
    pub top_level: Vec<Channel>,
    /// Channel key → its managed post.
    pub posts: BTreeMap<String, PostFile>,
    /// Project slug → its brief.
    pub briefs: BTreeMap<String, PostFile>,
    pub bot: BotConfig,
    /// members.toml: role key → who holds it.
    pub holders: BTreeMap<String, Vec<Snowflake>>,
    pub projects: Vec<Project>,
    pub onboarding: Option<Onboarding>,
    pub screening: Option<Screening>,
    pub automod: Vec<AutoModRule>,
    pub federation: Federation,
    pub choices: Choices,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PostFile {
    pub file: String,
    pub body: String,
}

/// The values fields that take one of a set can have.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Choices {
    pub channel_types: Vec<String>,
    pub statuses: Vec<String>,
    pub permissions: Vec<String>,
    /// For `{button:action|Label}` lines in posts.
    pub button_actions: Vec<String>,
    pub drift_modes: Vec<String>,
}

pub fn schema(dir: &Path) -> Result<Schema> {
    let cfg = Config::load(dir).with_context(|| format!("loading config from {}", dir.display()))?;
    let snap = load_snapshot(dir)?;
    let file_of = |file: &Option<String>, body: Option<&String>| {
        Some(PostFile { file: file.clone()?, body: body.cloned().unwrap_or_default() })
    };
    // As written, with any buttons loading leaves off (see `Config::notes`).
    let raw = |file: &Option<String>| -> Option<String> {
        crate::read_text(&crate::inside(dir, file.as_deref()?).ok()?).ok().flatten()
    };
    let posts =
        snap.channels().filter_map(|(c, _)| Some((c.key.clone(), file_of(&c.post, raw(&c.post).as_ref())?))).collect();
    let briefs = cfg
        .projects
        .iter()
        .filter_map(|p| Some((p.slug.clone(), file_of(&p.brief, cfg.desired.project_posts.get(&p.slug))?)))
        .collect();
    let open = cfg.projects.iter().filter(|p| p.application.open).map(|p| format!("apply:{}", p.slug));
    let choices = Choices {
        channel_types: ChannelKind::ALL.iter().map(|k| k.as_str().to_owned()).collect(),
        statuses: ProjectStatus::ALL.iter().map(|s| s.as_str().to_owned()).collect(),
        permissions: chipsoc_model::perms::all().into_iter().map(str::to_owned).collect(),
        button_actions: BUTTON_ACTIONS.iter().map(|a| (*a).to_owned()).chain(open).collect(),
        drift_modes: vec!["undo".into(), "report".into()],
    };
    let settings = cfg.desired.settings;
    Ok(Schema {
        roles: snap.roles.roles,
        categories: snap.categories,
        top_level: snap.top_level.channels,
        posts,
        briefs,
        bot: cfg.bot,
        holders: read_or_default(&dir.join("members.toml"))?,
        projects: cfg.projects,
        onboarding: settings.onboarding,
        screening: settings.screening,
        automod: settings.automod,
        federation: cfg.federation,
        choices,
    })
}

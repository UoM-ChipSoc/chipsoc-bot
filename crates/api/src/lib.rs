//! The web server's JSON API (`/api/v1`): what the CLI and TUI send and get
//! back. Both ends use these types: the web server (`chipsoc_web`) and its
//! client (`chipsoc_client`), so neither depends on the other.

pub use chipsoc_admin::{Decided, Done, Drift, Entry, FeedInfo, NewResource, Past, Pending, Post, Roster};
use chipsoc_config::ops::Op;
use chipsoc_config::{Project, ProjectStatus, Snowflake};
use chipsoc_model::{Action, Plan};
use serde::{Deserialize, Serialize};

/// A plan the server made and keeps under `id` for a short while.
#[derive(Serialize, Deserialize)]
pub struct Planned {
    pub id: String,
    pub plan: Plan,
    /// The plan as `plan` prints it.
    pub lines: Vec<String>,
}

#[derive(Serialize, Deserialize)]
pub struct ApplyRequest {
    pub plan_id: String,
    /// Only these steps (indexes into the plan's actions); all of them when absent.
    #[serde(default)]
    pub steps: Option<Vec<usize>>,
}

/// How one applied step went.
#[derive(Serialize, Deserialize)]
pub struct Step {
    pub action: Action,
    pub error: Option<String>,
}

#[derive(Serialize, Deserialize, Default)]
pub struct PullRequest {
    /// Also copy role permissions.
    #[serde(default)]
    pub permissions: bool,
}

#[derive(Serialize, Deserialize)]
pub struct Checked {
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
}

#[derive(Serialize, Deserialize)]
pub struct ProjectList {
    pub projects: Vec<Project>,
    /// The areas the config suggests for new projects.
    pub areas: Vec<String>,
}

/// A project to add; also the web form for it.
#[derive(Serialize, Deserialize, Default)]
#[serde(default)]
pub struct NewProject {
    pub name: String,
    pub blurb: String,
    pub area: String,
    pub status: ProjectStatus,
}

#[derive(Serialize, Deserialize)]
pub struct SetStatus {
    pub status: ProjectStatus,
}

#[derive(Serialize, Deserialize)]
pub struct SetApplications {
    pub open: bool,
}

/// The project a change was saved to.
#[derive(Serialize, Deserialize)]
pub struct Saved {
    pub slug: String,
}

/// Accept or reject an application in the review queue.
#[derive(Serialize, Deserialize)]
pub struct Decision {
    pub accept: bool,
    /// Told to the applicant and kept with the application.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// The version of the entry that was shown ([`chipsoc_admin::Pending::version`]):
    /// Accept is refused if it changed since.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seen: Option<String>,
}

/// An announcement or event to share with partners.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShareRequest {
    /// `announcement` or `event`.
    pub what: String,
    /// The message's link, or the event's link or id.
    pub item: String,
    /// Partners' names or ids; empty: every partner we send that kind to.
    #[serde(default)]
    pub partners: Vec<String>,
}

/// Whom something was shared with.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Shared {
    pub partners: Vec<String>,
}

/// A feed to follow.
#[derive(Serialize, Deserialize)]
pub struct NewFeed {
    pub url: String,
}

#[derive(Serialize, Deserialize)]
pub struct Pause {
    pub paused: bool,
}

/// `POST /ideas/{id}/close`: why, posted in the idea (optional).
#[derive(Serialize, Deserialize)]
pub struct CloseIdea {
    #[serde(default)]
    pub reason: Option<String>,
}

/// Someone to give a role members.toml lists, or make a project lead.
#[derive(Serialize, Deserialize)]
pub struct Appoint {
    /// The role key (ignored for leads).
    #[serde(default)]
    pub role: String,
    pub user: Snowflake,
}

/// The server at a glance, without asking Discord.
#[derive(Serialize, Deserialize, Default)]
pub struct Status {
    pub name: String,
    /// Whether the config has been applied to a server.
    pub linked: bool,
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
    /// Applications waiting for a decision.
    pub queue: i64,
    pub roles: usize,
    pub channels: usize,
    pub projects: usize,
    pub positions: usize,
}

impl Status {
    /// For `cfg`, with `queue` applications waiting.
    pub fn of(cfg: &chipsoc_config::Config, queue: i64) -> Self {
        let report = chipsoc_config::authoring::check_config(cfg);
        Self {
            name: cfg.desired.server.guild.name.clone(),
            linked: cfg.guild().is_some(),
            errors: report.errors,
            warnings: report.warnings,
            queue,
            roles: cfg.desired.roles.roles.len(),
            channels: cfg.desired.channels().count(),
            projects: cfg.projects.len(),
            positions: cfg.bot.positions.len(),
        }
    }
}

/// What `POST /ops` takes: one op, or several made as one change.
#[derive(Serialize, Deserialize)]
#[serde(untagged)]
pub enum Ops {
    Many(Vec<Op>),
    One(Box<Op>),
}

impl Ops {
    pub fn into_vec(self) -> Vec<Op> {
        match self {
            Self::Many(v) => v,
            Self::One(op) => vec![*op],
        }
    }
}

/// Markdown to render as a managed post would be.
#[derive(Serialize, Deserialize)]
pub struct Preview {
    pub body: String,
}

/// What an action did, in a sentence or two.
#[derive(Serialize, Deserialize)]
pub struct Note {
    pub text: String,
}

/// `POST /undo`: go back on a config commit.
#[derive(Serialize, Deserialize)]
pub struct UndoRequest {
    pub commit: String,
    pub how: chipsoc_engine::git::Undo,
}

/// What `POST /undo` made: the new commit's subject, if anything changed,
/// and what went wrong after it was saved (pushing it), if anything.
#[derive(Serialize, Deserialize)]
pub struct Undone {
    pub subject: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
}

#[derive(Serialize, Deserialize)]
pub struct Me {
    pub user: String,
    /// The community the server runs (bot.toml `name`).
    #[serde(default)]
    pub server: String,
}

/// A server's current setup as config files (path → text), for `export`.
#[derive(Serialize, Deserialize)]
pub struct Exported {
    pub files: std::collections::BTreeMap<String, String>,
}

/// An API token, traded for a sign-in's one-time code.
#[derive(Serialize, Deserialize)]
pub struct Token {
    pub token: String,
}

/// A command-line sign-in, as its owner sees it in the list (never the token).
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct TokenInfo {
    /// What to revoke it by.
    pub id: String,
    /// The device it was made on, and the day.
    pub name: String,
    pub created: String,
    pub expires: String,
    /// The token asking.
    #[serde(default)]
    pub current: bool,
}

/// Why the server said no.
#[derive(Serialize, Deserialize)]
pub struct Refusal {
    pub error: String,
    /// Config mistakes that stop a plan.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub problems: Vec<String>,
}

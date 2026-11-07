//! The config files' schema and the lockfile.
//!
//! Order is meaningful: roles are listed top of the hierarchy first, categories
//! sort by file name, and channels appear in the order they're written.
//! Unknown fields are errors, so typos fail `check` instead of being ignored.

use std::collections::{BTreeMap, BTreeSet};

pub use chipsoc_model::Snowflake;
use serde::{Deserialize, Serialize};

fn is_false(b: &bool) -> bool {
    !*b
}
fn is_zero(n: &u16) -> bool {
    *n == 0
}
fn default_true() -> bool {
    true
}
fn key_env() -> String {
    "FED_SIGNING_KEY".into()
}
fn is_default_key_env(s: &String) -> bool {
    *s == key_env()
}

/// `server.toml`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Server {
    /// The `[guild]` table: the server's own settings.
    pub guild: Guild,
}

/// Server-wide settings. Unset fields are left as they are in Discord.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Guild {
    /// The server's name.
    pub name: String,
    /// Shown in server discovery and invites.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Image file (relative to the config dir) for the server icon.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    /// Image file for the server banner (needs boost level 2).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub banner: Option<String>,
    /// Discord's number: 0 none, 1 low, 2 medium, 3 high, 4 very high.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verification_level: Option<u8>,
    /// Discord's number: 0 off, 1 members without roles, 2 everyone.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub explicit_content_filter: Option<u8>,
    /// Discord's number: 0 all messages, 1 mentions only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_message_notifications: Option<u8>,
    /// Channel key for Discord's join and boost messages.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_channel: Option<String>,
    /// Channel key of the rules channel (community servers).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rules_channel: Option<String>,
    /// Channel key for Discord's community updates (`public_updates_channel_id`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updates_channel: Option<String>,
    /// Voice channel key idle members are moved to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub afk_channel: Option<String>,
}

/// `roles.toml`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Roles {
    /// `[[role]]` entries, top of the hierarchy first.
    #[serde(default, rename = "role")]
    pub roles: Vec<Role>,
}

/// A role.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Role {
    /// How the config refers to it; `everyone` is the @everyone role.
    pub key: String,
    /// The name shown in Discord.
    pub name: String,
    /// Hex colour, e.g. `"#5865f2"`; unset is Discord's default (none).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    /// Shown separately in the member list.
    #[serde(default, skip_serializing_if = "is_false")]
    pub hoist: bool,
    /// Anyone may @mention it.
    #[serde(default, skip_serializing_if = "is_false")]
    pub mentionable: bool,
    /// Discord permission names, e.g. `["VIEW_CHANNEL", "SEND_MESSAGES"]`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub permissions: Vec<String>,
    /// Owned by Discord (the bot's own role, boosters, integrations): never edited.
    #[serde(default, skip_serializing_if = "is_false")]
    pub managed: bool,
}

/// One `categories/*.toml` file: a category and its channels.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CategoryFile {
    /// The `[category]` table.
    pub category: Category,
    /// `[[channel]]` entries, in display order.
    #[serde(default, rename = "channel")]
    pub channels: Vec<Channel>,
}

/// A category.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Category {
    /// How the config refers to it.
    pub key: String,
    /// The name shown in Discord.
    pub name: String,
    /// Permission overwrites; its channels inherit them unless `inherit = false`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub overwrites: Vec<Overwrite>,
}

/// `channels.toml`: channels outside any category.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChannelsFile {
    /// `[[channel]]` entries, in display order.
    #[serde(default, rename = "channel")]
    pub channels: Vec<Channel>,
}

/// A channel's `type`: `text` (default), `voice`, `announcement`, `stage`,
/// `forum` or `media`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ChannelKind {
    #[default]
    Text,
    Voice,
    Announcement,
    Stage,
    Forum,
    Media,
}

impl ChannelKind {
    pub const ALL: [Self; 6] = [Self::Text, Self::Voice, Self::Announcement, Self::Stage, Self::Forum, Self::Media];

    /// Discord lists voice channels after text ones in a category.
    pub fn is_voice(self) -> bool {
        matches!(self, Self::Voice | Self::Stage)
    }

    /// Forum-like channels hold posts with tags.
    pub fn has_tags(self) -> bool {
        matches!(self, Self::Forum | Self::Media)
    }

    pub fn discord(self) -> u8 {
        match self {
            Self::Text => 0,
            Self::Voice => 2,
            Self::Announcement => 5,
            Self::Stage => 13,
            Self::Forum => 15,
            Self::Media => 16,
        }
    }

    pub fn from_discord(t: u8) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.discord() == t)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Voice => "voice",
            Self::Announcement => "announcement",
            Self::Stage => "stage",
            Self::Forum => "forum",
            Self::Media => "media",
        }
    }
}

impl std::str::FromStr for ChannelKind {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        Self::ALL.into_iter().find(|k| k.as_str() == s).ok_or_else(|| {
            let all: Vec<&str> = Self::ALL.iter().map(|k| k.as_str()).collect();
            format!("`{s}`: the channel types are {}", all.join(", "))
        })
    }
}

impl std::fmt::Display for ChannelKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A channel.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Channel {
    /// How the config refers to it.
    pub key: String,
    /// TOML key `type`; default `text`.
    #[serde(rename = "type", default)]
    pub kind: ChannelKind,
    /// The name shown in Discord.
    pub name: String,
    /// Text-like channels only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub topic: Option<String>,
    /// Seconds between messages per member (0 = off); text-like channels only.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub slowmode: u16,
    /// Age-restricted.
    #[serde(default, skip_serializing_if = "is_false")]
    pub nsfw: bool,
    /// Voice only; 0 or unset = no limit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_limit: Option<u16>,
    /// Tag names, for forum and media channels.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Added to the category's overwrites; an entry for the same role or member replaces it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub overwrites: Vec<Overwrite>,
    /// Set false to ignore the category's overwrites.
    #[serde(default = "default_true", skip_serializing_if = "Clone::clone")]
    pub inherit: bool,
    /// Markdown file (relative to the config dir) the bot keeps posted here.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub post: Option<String>,
    /// Forums: how posts are listed (`activity` or `creation`); unset = left as Discord has it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sort: Option<ForumSort>,
    /// Forums: `[[channel.thread]]` posts the bot starts and keeps in step.
    #[serde(default, rename = "thread", skip_serializing_if = "Vec::is_empty")]
    pub threads: Vec<ForumThread>,
}

/// A forum's default sort order (Discord's `default_sort_order`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ForumSort {
    /// Latest activity first.
    Activity,
    /// Newest post first.
    Creation,
}

impl ForumSort {
    pub fn discord(self) -> u8 {
        match self {
            Self::Activity => 0,
            Self::Creation => 1,
        }
    }

    pub fn from_discord(n: u64) -> Option<Self> {
        match n {
            0 => Some(Self::Activity),
            1 => Some(Self::Creation),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Activity => "activity",
            Self::Creation => "creation",
        }
    }
}

/// A forum post the config describes: started by the bot, its title, text
/// and tags kept in step. Removed from config, it's archived (never deleted).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ForumThread {
    /// How the config refers to it (unique in its forum).
    pub key: String,
    /// The post's title (at most 100 characters).
    pub title: String,
    /// Markdown file (relative to the config dir): its text, in parts like other posts.
    pub post: String,
    /// Forum tag names to apply (at most 5; each one of the forum's `tags`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
}

/// How a forum thread is keyed in `lock.toml` `[posts]` and in plans: `forum/key`.
/// (Channel keys have no `/`, so older versions of the bot pass these by.)
pub fn thread_key(forum: &str, key: &str) -> String {
    format!("{forum}/{key}")
}

/// Whether a `[posts]` key is a forum thread's ([`thread_key`]), not a channel's.
pub fn is_thread_key(key: &str) -> bool {
    key.contains('/')
}

impl Overwrite {
    /// Who it applies to, for matching category and channel entries.
    pub fn target(&self) -> (Option<&str>, Option<Snowflake>) {
        (self.role.as_deref(), self.member)
    }
}

/// The overwrites Discord should have on a channel: the category's, with the
/// channel's own entries replacing any for the same target.
pub fn effective_overwrites(cat: Option<&Category>, ch: &Channel) -> Vec<Overwrite> {
    let mut out: Vec<Overwrite> = match cat {
        Some(c) if ch.inherit => c.overwrites.clone(),
        _ => Vec::new(),
    };
    for o in &ch.overwrites {
        match out.iter_mut().find(|x| x.target() == o.target()) {
            Some(x) => *x = o.clone(),
            None => out.push(o.clone()),
        }
    }
    out
}

/// The reverse, for export: the shortest channel entry that gives `live`.
pub fn own_overwrites(cat: Option<&Category>, live: &[Overwrite]) -> (Vec<Overwrite>, bool) {
    let inherited = cat.map(|c| c.overwrites.as_slice()).unwrap_or_default();
    let same_targets = inherited.iter().all(|o| live.iter().any(|l| l.target() == o.target()));
    if !same_targets {
        return (live.to_vec(), false);
    }
    (live.iter().filter(|l| !inherited.contains(l)).cloned().collect(), true)
}

impl Default for Channel {
    fn default() -> Self {
        Self {
            key: String::new(),
            kind: ChannelKind::default(),
            name: String::new(),
            topic: None,
            slowmode: 0,
            nsfw: false,
            user_limit: None,
            tags: Vec::new(),
            overwrites: Vec::new(),
            inherit: true,
            post: None,
            sort: None,
            threads: Vec::new(),
        }
    }
}

/// A permission overwrite on a category or channel, for one role or one member.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Overwrite {
    /// Role key (or `everyone`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// A member's id; members aren't managed, but their overwrites are kept.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub member: Option<Snowflake>,
    /// Permission names granted, e.g. `["VIEW_CHANNEL"]`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow: Vec<String>,
    /// Permission names taken away.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deny: Vec<String>,
}

// `parts` is new: these two take no `deny_unknown_fields`, so a lock.toml
// with it still reads in older versions of the bot (they ignore it).

/// A bot-managed message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PostRef {
    /// The message's id (in a forum, its thread's).
    pub message: Snowflake,
    /// A long post's later parts, in order: the bot's messages after the first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parts: Vec<Snowflake>,
}

/// A project's front-page forum post.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadRef {
    /// The post's thread id.
    pub thread: Snowflake,
    /// Its first message, which the bot edits.
    pub message: Snowflake,
    /// A long post's later parts, in order: the bot's messages in the thread after the first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parts: Vec<Snowflake>,
}

/// An uploaded image: the file's hash, and Discord's hash for what we uploaded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssetRef {
    /// Hash of the image file in the config.
    pub file: String,
    /// Discord's image hash after the upload.
    pub discord: String,
}

/// `lock.toml`: which Discord object each config key is. Generated; commit it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Lock {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub guild: Option<Snowflake>,
    #[serde(default)]
    pub roles: BTreeMap<String, Snowflake>,
    #[serde(default)]
    pub categories: BTreeMap<String, Snowflake>,
    #[serde(default)]
    pub channels: BTreeMap<String, Snowflake>,
    /// Channel key → its managed post.
    #[serde(default)]
    pub posts: BTreeMap<String, PostRef>,
    /// Project slug → its forum post.
    #[serde(default)]
    pub project_posts: BTreeMap<String, ThreadRef>,
    #[serde(default)]
    pub automod: BTreeMap<String, Snowflake>,
    /// `icon` / `banner` → what we last uploaded.
    #[serde(default)]
    pub assets: BTreeMap<String, AssetRef>,
    /// Removed from config and moved to the archive: key → id.
    #[serde(default)]
    pub archived: BTreeMap<String, Snowflake>,
    /// What was last applied, to tell changes made in Discord from changes made in config.
    #[serde(default)]
    pub applied: Applied,
}

/// What was last applied: each object's hash, and the config commit it came
/// from. In `lock.toml` both are `[applied]` entries, the commit as
/// `"git:commit"`, where older versions of the bot read it (see `lockfile.rs`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Applied {
    /// `kind:key` → hash of the object as applied.
    pub hashes: BTreeMap<String, String>,
    /// The config commit last applied from, if the config was a git checkout.
    pub commit: Option<String>,
}

impl Lock {
    /// A role's id. `everyone` is always the server's own id.
    pub fn role(&self, key: &str) -> Option<Snowflake> {
        self.roles.get(key).copied().or_else(|| self.guild.filter(|_| key == "everyone"))
    }
    pub fn category(&self, key: &str) -> Option<Snowflake> {
        self.categories.get(key).copied()
    }
    pub fn channel(&self, key: &str) -> Option<Snowflake> {
        self.channels.get(key).copied()
    }

    /// The key a role id is known by.
    pub fn role_key(&self, id: Snowflake) -> Option<&str> {
        let found = self.roles.iter().find(|(_, v)| **v == id).map(|(k, _)| k.as_str());
        found.or_else(|| (self.guild == Some(id)).then_some("everyone"))
    }
    pub fn channel_key(&self, id: Snowflake) -> Option<&str> {
        self.channels.iter().find(|(_, v)| **v == id).map(|(k, _)| k.as_str())
    }
}

/// Native Discord onboarding: questions that assign roles and show channels,
/// asked on join and editable later under Channels & Roles.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Onboarding {
    /// Whether Discord shows onboarding to new members (default false).
    #[serde(default)]
    pub enabled: bool,
    /// `advanced` also counts question channels toward Discord's minimum.
    #[serde(default, skip_serializing_if = "is_false")]
    pub advanced: bool,
    /// Channels everyone sees; unset = every channel @everyone can view.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub default_channels: Vec<String>,
    /// `[[prompt]]` entries: the questions, in order.
    #[serde(default, rename = "prompt")]
    pub prompts: Vec<OnbPrompt>,
}

/// An onboarding question.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OnbPrompt {
    /// The question.
    pub title: String,
    /// Only one option may be picked (default true).
    #[serde(default = "default_true")]
    pub single_select: bool,
    /// Must be answered to finish onboarding.
    #[serde(default, skip_serializing_if = "is_false")]
    pub required: bool,
    /// `[[prompt.option]]` entries: the answers.
    #[serde(default, rename = "option")]
    pub options: Vec<OnbOption>,
}

/// An answer to an onboarding question.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OnbOption {
    /// The answer's text.
    pub title: String,
    /// A line under the title.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// A Unicode emoji shown with it, e.g. `"🔧"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub emoji: Option<String>,
    /// Role key granted when picked.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub role: String,
    /// Channel keys shown when picked.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub channels: Vec<String>,
}

/// `projects.toml`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Projects {
    /// `[[project]]` entries; active ones get categories in this order.
    #[serde(default, rename = "project")]
    pub projects: Vec<Project>,
}

/// A project's `status`: `proposed` (default), `active` or `past`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProjectStatus {
    /// An idea: a post in the projects forum, no roles or channels. Taking
    /// applications needs `active`.
    #[default]
    Proposed,
    /// Running: gets its own category.
    Active,
    /// Finished: channels move to the archive; roles are kept, renamed.
    Past,
}

impl ProjectStatus {
    pub const ALL: [Self; 3] = [Self::Proposed, Self::Active, Self::Past];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Proposed => "proposed",
            Self::Active => "active",
            Self::Past => "past",
        }
    }
}

impl std::str::FromStr for ProjectStatus {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        Self::ALL.into_iter().find(|x| x.as_str() == s).ok_or_else(|| format!("`{s}`: use proposed, active or past"))
    }
}

impl std::fmt::Display for ProjectStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A project.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Project {
    /// Its key: role, channel and button keys are made from it. Editing it
    /// here makes a new project; rename with `project rename` instead.
    pub slug: String,
    /// Display name; also names its roles and category.
    pub name: String,
    /// One line after the name, e.g. "a RISC-V CPU".
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub blurb: String,
    /// Free label for the area (e.g. a domain or team); shown on its post and added as a tag.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub kind: String,
    /// Default `proposed`.
    #[serde(default)]
    pub status: ProjectStatus,
    /// Free label for where it's at, shown on its post (e.g. "taping out").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stage: Option<String>,
    /// Link to its code, shown on its post.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub github: Option<String>,
    /// Markdown file (relative to the config dir) added to its post.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub brief: Option<String>,
    /// Give it a `<Name> Team` role (default true).
    #[serde(default = "default_true")]
    pub team_role: bool,
    /// Give it a `<Name> Lead` role (default true).
    #[serde(default = "default_true")]
    pub lead_role: bool,
    /// Channel names, in order; `voice`, or a name ending in `-voice` or
    /// ` voice`, makes a voice channel.
    #[serde(default)]
    pub channels: Vec<String>,
    /// Forum tags for its post, besides its status and `kind`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// `[project.application]`: joining through `/apply`.
    #[serde(default)]
    pub application: ProjectApplication,
    /// Discord user ids of its leads (given the Lead role while active).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub leads: Vec<Snowflake>,
}

/// How people apply to join a project.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectApplication {
    /// Taking applications: adds an apply button to its post and lists it in `/apply`.
    #[serde(default)]
    pub open: bool,
    /// Ask applicants whether they'd like to lead.
    #[serde(default, skip_serializing_if = "is_false")]
    pub lead_question: bool,
    /// Extra questions (up to 3) shown to applicants.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub questions: Vec<String>,
}

impl Project {
    pub fn display(&self) -> String {
        if self.blurb.is_empty() { self.name.clone() } else { format!("{} — {}", self.name, self.blurb) }
    }
    pub fn is_live(&self) -> bool {
        self.status == ProjectStatus::Active
    }
    pub fn team_role_key(&self) -> String {
        format!("proj-{}-team", self.slug)
    }
    pub fn lead_role_key(&self) -> String {
        format!("proj-{}-lead", self.slug)
    }
    pub fn category_key(&self) -> String {
        format!("proj-{}", self.slug)
    }
    pub fn channel_key(&self, channel: &str) -> String {
        format!("proj-{}-{}", self.slug, crate::slug(channel))
    }
}

/// `automod.toml`: Discord's AutoMod rules.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AutoMod {
    /// `[[rule]]` entries.
    #[serde(default, rename = "rule")]
    pub rules: Vec<AutoModRule>,
}

/// What a rule looks for: `keyword` (default), `keyword_preset`, `spam` or `mention_spam`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Trigger {
    #[default]
    Keyword,
    KeywordPreset,
    Spam,
    MentionSpam,
}

impl Trigger {
    pub fn discord(self) -> u8 {
        match self {
            Self::Keyword => 1,
            Self::Spam => 3,
            Self::KeywordPreset => 4,
            Self::MentionSpam => 5,
        }
    }
    pub fn from_discord(n: u8) -> Option<Self> {
        [Self::Keyword, Self::Spam, Self::KeywordPreset, Self::MentionSpam].into_iter().find(|t| t.discord() == n)
    }
}

/// Discord's word lists: `profanity`, `sexual_content` or `slurs`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Preset {
    Profanity,
    SexualContent,
    Slurs,
}

impl Preset {
    pub fn discord(self) -> u8 {
        self as u8 + 1
    }
    pub fn from_discord(n: u8) -> Option<Self> {
        [Self::Profanity, Self::SexualContent, Self::Slurs].into_iter().find(|p| p.discord() == n)
    }
}

/// What a rule does when it matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModAction {
    /// Stop the message being sent.
    Block,
    /// Post about it in `alert_channel`.
    Alert,
    /// Time the member out for `timeout_seconds`.
    Timeout,
}

/// An AutoMod rule.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AutoModRule {
    /// How the config refers to it.
    pub key: String,
    /// The name shown in Discord.
    pub name: String,
    /// What it looks for.
    pub trigger: Trigger,
    /// Default true.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// For `keyword`: words to catch; `*` is a wildcard, e.g. `"cat*"`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keywords: Vec<String>,
    /// For `keyword`: regular expressions to catch (Rust syntax).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub regex: Vec<String>,
    /// For `keyword` and `keyword_preset`: words never caught.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow: Vec<String>,
    /// For `keyword_preset`: which of Discord's lists.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub presets: Vec<Preset>,
    /// For `mention_spam`: mentions in one message that trigger it (default 10).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mention_limit: Option<u8>,
    /// For `mention_spam`: also catch mention raids.
    #[serde(default, skip_serializing_if = "is_false")]
    pub raid_protection: bool,
    /// What it does, e.g. `["block", "alert"]`.
    pub actions: Vec<ModAction>,
    /// Channel key for `alert`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alert_channel: Option<String>,
    /// How long `timeout` lasts, in seconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_seconds: Option<u32>,
    /// Shown to the member when `block` stops their message.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub block_message: Option<String>,
    /// Role keys the rule ignores.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exempt_roles: Vec<String>,
    /// Channel keys the rule ignores.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exempt_channels: Vec<String>,
}

/// `screening.toml`: rules members must agree to before talking (Discord's
/// membership screening).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Screening {
    /// Whether new members must agree (default false).
    #[serde(default)]
    pub enabled: bool,
    /// Intro shown above the rules.
    #[serde(default)]
    pub description: String,
    /// The checkbox text, e.g. "I have read and agree to the rules".
    #[serde(default)]
    pub label: String,
    /// The rules, one per entry.
    #[serde(default)]
    pub rules: Vec<String>,
}

/// `federation.toml`: sharing with partner servers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Federation {
    /// Our id, as partners know us.
    #[serde(default)]
    pub name: String,
    /// Environment variable holding our signing key.
    #[serde(default = "key_env", skip_serializing_if = "is_default_key_env")]
    pub key_env: String,
    /// Private channel partners' bots post signed messages into.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inbox: Option<String>,
    /// Announcement channel keys to publish, so partners can Follow them.
    #[serde(default)]
    pub publish: Vec<String>,
    /// `[[peer]]` entries: the partner servers.
    #[serde(default, rename = "peer")]
    pub peers: Vec<Peer>,
}

impl Default for Federation {
    fn default() -> Self {
        Self { name: String::new(), key_env: key_env(), inbox: None, publish: Vec::new(), peers: Vec::new() }
    }
}

/// A partner server.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Peer {
    /// What we call them, in checks and the bot log.
    pub name: String,
    /// Environment variable with the webhook into their sync inbox (secret, not in git).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub webhook_env: String,
    /// Our channel keys whose new posts are copied to them…
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub share: Vec<String>,
    /// …through the webhook in this environment variable (a channel they chose).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub share_webhook_env: String,
    /// Their id (signed sync).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub id: String,
    /// Their public key (signed sync).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub public_key: String,
    /// More public keys, e.g. old and new while they rotate.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub public_keys: Vec<String>,
    /// Send them our project catalogue.
    #[serde(default, skip_serializing_if = "is_false")]
    pub catalog: bool,
    /// Our role keys we vouch for, for our members who hold them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub send_roles: Vec<String>,
    /// `[[peer.role_map]]`: their vouched roles → ours.
    #[serde(default, rename = "role_map", skip_serializing_if = "Vec::is_empty")]
    pub role_map: Vec<RoleMap>,
    /// What else we send them signed (see [`SEND_KINDS`]): `projects` (like
    /// `catalog`), and `resources`, `feeds`, `questions` as they're posted;
    /// `events` and `announcements` when an admin shares one.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub send: Vec<String>,
    /// What we take from them, and where it lands.
    #[serde(default, skip_serializing_if = "Accept::is_empty")]
    pub accept: Accept,
}

/// What a peer's `send` may list.
pub const SEND_KINDS: &[&str] = &["projects", "resources", "feeds", "questions", "events", "announcements"];

/// What we take from a partner (`accept = { … }` in a `[[peer]]`): each a
/// channel key (forums for resources and questions), or `{ to = "…",
/// review = false }` to post without review. Unlisted kinds are ignored.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Accept {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<Landing>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub questions: Option<Landing>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub feeds: Option<Landing>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub announcements: Option<Landing>,
    /// `true`, or `{ review = false }`: their events become events here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub events: Option<EventLanding>,
}

impl Accept {
    pub fn is_empty(&self) -> bool {
        self == &Self::default()
    }

    /// Where a kind (`resources`, `events`…) lands (none for events), and
    /// whether it waits for review; `None` if it isn't taken.
    pub fn landing(&self, kind: &str) -> Option<(Option<&str>, bool)> {
        fn to(l: &Option<Landing>) -> Option<(Option<&str>, bool)> {
            l.as_ref().map(|l| (Some(l.to()), l.review()))
        }
        match kind {
            "resources" => to(&self.resources),
            "questions" => to(&self.questions),
            "feeds" => to(&self.feeds),
            "announcements" => to(&self.announcements),
            "events" => match &self.events {
                Some(EventLanding::On(true)) => Some((None, true)),
                Some(EventLanding::Table(t)) => Some((None, t.review)),
                _ => None,
            },
            _ => None,
        }
    }

    /// Whether a partner's edits of something of `kind` already posted wait
    /// for review (see [`Edits`]).
    pub fn edits_reviewed(&self, kind: &str) -> bool {
        let l = match kind {
            "resources" => &self.resources,
            "questions" => &self.questions,
            "feeds" => &self.feeds,
            "announcements" => &self.announcements,
            "events" => {
                return match &self.events {
                    Some(EventLanding::Table(t)) => t.edits.map_or(t.review, |e| e == Edits::Review),
                    _ => true,
                };
            }
            _ => return true,
        };
        l.as_ref().is_none_or(Landing::edits_reviewed)
    }

    /// The channel keys things land in, with their kind.
    pub fn channels(&self) -> impl Iterator<Item = (&'static str, &str)> {
        [
            ("resources", &self.resources),
            ("questions", &self.questions),
            ("feeds", &self.feeds),
            ("announcements", &self.announcements),
        ]
        .into_iter()
        .filter_map(|(k, l)| l.as_ref().map(|l| (k, l.to())))
    }
}

/// Where one kind lands: a channel key, or a table.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Landing {
    /// Waits for review.
    To(String),
    Table(LandingTable),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LandingTable {
    pub to: String,
    /// Waits in the review queue first (the default).
    #[serde(default = "default_true")]
    pub review: bool,
    /// What their later edits of something posted do (see [`Edits`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edits: Option<Edits>,
}

/// What a partner's edit of something already posted here does: waits for
/// review like something new (`"review"`), or is applied (`"apply"`). The
/// default is `"review"` for a kind that's reviewed, `"apply"` for one
/// posted without review.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Edits {
    Review,
    Apply,
}

impl Landing {
    pub fn to(&self) -> &str {
        match self {
            Self::To(k) | Self::Table(LandingTable { to: k, .. }) => k,
        }
    }

    pub fn review(&self) -> bool {
        match self {
            Self::To(_) => true,
            Self::Table(t) => t.review,
        }
    }

    /// Whether their edits wait for review (see [`Edits`]).
    pub fn edits_reviewed(&self) -> bool {
        match self {
            Self::To(_) => true,
            Self::Table(t) => t.edits.map_or(t.review, |e| e == Edits::Review),
        }
    }
}

/// Whether a partner's events are taken: `true`, or `{ review = false }`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum EventLanding {
    On(bool),
    Table(EventTable),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EventTable {
    #[serde(default = "default_true")]
    pub review: bool,
    /// What their later edits of an event posted here do (see [`Edits`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edits: Option<Edits>,
}

impl Peer {
    /// Whether we send them `kind` (one of [`SEND_KINDS`]).
    pub fn sends(&self, kind: &str) -> bool {
        self.send.iter().any(|k| k == kind) || (kind == "projects" && self.catalog)
    }

    /// Every public key given, `public_key` first.
    pub fn keys(&self) -> impl Iterator<Item = &str> {
        std::iter::once(self.public_key.as_str())
            .chain(self.public_keys.iter().map(String::as_str))
            .filter(|k| !k.is_empty())
    }
}

/// A partner's vouched role, given here as one of ours.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoleMap {
    /// Their role key, as in their `send_roles`.
    pub theirs: String,
    /// Our role key to give its holders.
    pub ours: String,
}

impl RoleMap {
    /// Our role is too powerful to hand out on a partner's word: `everyone`,
    /// the admin role, a role not in config or owned by Discord, one whose
    /// holders the config manages, or one that [`Snapshot::grants_power`].
    pub fn too_powerful(&self, desired: &Snapshot, admin_role: Option<&str>) -> bool {
        let key = self.ours.as_str();
        key == "everyone"
            || admin_role == Some(key)
            || desired.role(key).is_none_or(|r| r.managed)
            || desired.holders.contains_key(key)
            || desired.grants_power(key)
    }
}

/// A permission list as bits (see [`chipsoc_model::perms::bits_or_all`]).
fn perm_bits(p: &[String]) -> u64 {
    chipsoc_model::perms::bits_or_all(p)
}

/// Server settings managed alongside the structure.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Settings {
    /// `onboarding.toml`; none = not managed.
    pub onboarding: Option<Onboarding>,
    /// `screening.toml`; none = not managed.
    pub screening: Option<Screening>,
    /// `automod.toml`'s rules.
    pub automod: Vec<AutoModRule>,
}

/// One server's whole state: what config wants, or what Discord has.
///
/// `posts` (channel key) and `project_posts` (slug) hold each managed message:
/// the template source in config, and Discord's rendered text when live.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Snapshot {
    /// The server's own settings (`server.toml`).
    pub server: Server,
    /// Roles, top first (`roles.toml`, plus project roles).
    pub roles: Roles,
    /// Categories and their channels (`categories/*.toml`, plus active projects').
    pub categories: Vec<CategoryFile>,
    /// Channels outside any category (`channels.toml`).
    pub top_level: ChannelsFile,
    /// Onboarding, screening and AutoMod.
    pub settings: Settings,
    /// Channel key → its post.
    pub posts: BTreeMap<String, String>,
    /// Project slug → its forum post.
    pub project_posts: BTreeMap<String, String>,
    /// `forum/key` ([`thread_key`]) → a `[[channel.thread]]` post's text.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub threads: BTreeMap<String, String>,
    /// `icon` / `banner`: the file's hash (config) or Discord's image hash (live).
    pub assets: BTreeMap<String, String>,
    /// Who holds each role whose holders config manages (role key → user ids).
    pub holders: BTreeMap<String, std::collections::BTreeSet<Snowflake>>,
    /// Live only: everyone in the server.
    pub members: std::collections::BTreeSet<Snowflake>,
    /// Config key → Discord id: `lock.toml` (config), or as read (live).
    pub lock: Lock,
    /// Live only: what Discord wouldn't let this token read, so plans leave it out.
    #[serde(skip_serializing_if = "Unreadable::is_empty")]
    pub unreadable: Unreadable,
    /// Live only: forums and the bot's forum posts (threads) as Discord sent
    /// them, by id, so applying right after doesn't read them again.
    #[serde(skip)]
    pub forums_read: BTreeMap<Snowflake, serde_json::Value>,
}

/// Parts of a live server the token couldn't read (403). A read-only bot
/// planning in CI may lack Manage Server (AutoMod rules) or access to a
/// channel (the bot's posts there); the plan says so instead of guessing.
/// Discord documents no permission for reading onboarding, and doesn't
/// document the member-verification (screening) endpoint at all, so those are
/// treated the same way should it refuse.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Unreadable {
    /// AutoMod rules (needs Manage Server).
    pub automod: bool,
    /// Onboarding.
    pub onboarding: bool,
    /// Membership screening.
    pub screening: bool,
    /// Channel keys of posts.
    pub posts: BTreeSet<String>,
    /// Slugs of project posts.
    pub project_posts: BTreeSet<String>,
}

impl Unreadable {
    pub fn is_empty(&self) -> bool {
        !self.automod && !self.onboarding && !self.screening && self.posts.is_empty() && self.project_posts.is_empty()
    }
}

impl Snapshot {
    /// Every channel with its category (if any), in display order.
    pub fn channels(&self) -> impl Iterator<Item = (&Channel, Option<&Category>)> {
        self.categories
            .iter()
            .flat_map(|cf| cf.channels.iter().map(move |c| (c, Some(&cf.category))))
            .chain(self.top_level.channels.iter().map(|c| (c, None)))
    }

    pub fn channel(&self, key: &str) -> Option<&Channel> {
        self.channels().map(|(c, _)| c).find(|c| c.key == key)
    }

    pub fn role(&self, key: &str) -> Option<&Role> {
        self.roles.roles.iter().find(|r| r.key == key)
    }

    pub fn category(&self, key: &str) -> Option<&CategoryFile> {
        self.categories.iter().find(|c| c.category.key == key)
    }

    /// Whether holding this role gives dangerous permissions (see
    /// [`chipsoc_model::perms::DANGEROUS`]), from the role itself or any
    /// category or channel overwrite, or lets someone see a private channel.
    pub fn grants_power(&self, key: &str) -> bool {
        self.dangerous_role(key) || self.sees_private(key)
    }

    /// Whether the role, or an overwrite for it anywhere, allows a dangerous
    /// permission. Permissions that don't parse count as dangerous.
    pub fn dangerous_role(&self, key: &str) -> bool {
        use chipsoc_model::perms::dangerous;
        self.role(key).is_some_and(|r| dangerous(perm_bits(&r.permissions)))
            || self.overwrites_for(key).any(|o| dangerous(perm_bits(&o.allow)))
    }

    /// The permission bits the config gives a role: its own, and every
    /// category or channel overwrite allowing it something.
    pub fn power_bits(&self, key: &str) -> u64 {
        let own = self.role(key).map_or(0, |r| perm_bits(&r.permissions));
        self.overwrites_for(key).fold(own, |b, o| b | perm_bits(&o.allow))
    }

    /// Whether an overwrite lets the role see a channel or category that
    /// @everyone can't.
    pub fn sees_private(&self, key: &str) -> bool {
        let view = 1u64 << chipsoc_model::perms::VIEW_CHANNEL;
        let lets_see =
            |ows: &[Overwrite]| ows.iter().any(|o| o.role.as_deref() == Some(key) && perm_bits(&o.allow) & view != 0);
        let hides = |ows: &[Overwrite]| {
            ows.iter().any(|o| o.role.as_deref() == Some("everyone") && perm_bits(&o.deny) & view != 0)
        };
        let public: std::collections::BTreeSet<String> = crate::public_channels(self).into_iter().collect();
        self.categories.iter().any(|cf| lets_see(&cf.category.overwrites) && hides(&cf.category.overwrites))
            || self.channels().any(|(c, cat)| !public.contains(&c.key) && lets_see(&effective_overwrites(cat, c)))
    }

    /// Every category and channel overwrite for a role.
    fn overwrites_for<'a>(&'a self, key: &'a str) -> impl Iterator<Item = &'a Overwrite> + 'a {
        self.categories
            .iter()
            .flat_map(|c| &c.category.overwrites)
            .chain(self.channels().flat_map(|(c, _)| &c.overwrites))
            .filter(move |o| o.role.as_deref() == Some(key))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partner_edits_are_reviewed_unless_said_otherwise() {
        let a: Accept = toml::from_str(
            "resources = \"r\"\nquestions = { to = \"q\", review = false }\nfeeds = { to = \"f\", edits = \"apply\" }\n\
             announcements = { to = \"a\", review = false, edits = \"review\" }\nevents = true\n",
        )
        .unwrap();
        assert!(a.edits_reviewed("resources"), "reviewed kinds: edits too");
        assert!(!a.edits_reviewed("questions"), "posted without review: so are edits");
        assert!(!a.edits_reviewed("feeds"));
        assert!(a.edits_reviewed("announcements"));
        assert!(a.edits_reviewed("events"));
        let e: Accept = toml::from_str("events = { review = true, edits = \"apply\" }\n").unwrap();
        assert!(!e.edits_reviewed("events"));
    }
}

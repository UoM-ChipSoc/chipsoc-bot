//! Per-guild state: applications, audit log, message archive, feeds, reminders, profiles.

use anyhow::Result;
use sqlx::SqlitePool;

/// Feeds unique per guild (not globally); member profiles; one-off prompts.
pub(crate) const SCHEMA_V3: &str = r#"
CREATE TABLE feeds_v3 (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    guild_id TEXT NOT NULL, url TEXT NOT NULL, channel_key TEXT NOT NULL, added_by TEXT, ts INTEGER NOT NULL,
    UNIQUE (guild_id, url)
);
INSERT INTO feeds_v3 (id, guild_id, url, channel_key, added_by, ts) SELECT id, guild_id, url, channel_key, added_by, ts FROM feeds;
DROP TABLE feeds;
ALTER TABLE feeds_v3 RENAME TO feeds;
DROP TABLE IF EXISTS role_assignments;
CREATE TABLE profiles (
    guild_id TEXT NOT NULL, user_id TEXT NOT NULL,
    name TEXT NOT NULL, surname TEXT NOT NULL, company TEXT NOT NULL, ts INTEGER NOT NULL,
    PRIMARY KEY (guild_id, user_id)
);
CREATE TABLE prompted (
    guild_id TEXT NOT NULL, user_id TEXT NOT NULL, kind TEXT NOT NULL, ts INTEGER NOT NULL,
    PRIMARY KEY (guild_id, user_id, kind)
);
"#;

/// Tailscale share invites given to admins for the web UI.
pub(crate) const SCHEMA_V5: &str = r#"
CREATE TABLE web_shares (
    guild_id TEXT NOT NULL, user_id TEXT NOT NULL, invite_id TEXT NOT NULL, ts INTEGER NOT NULL,
    PRIMARY KEY (guild_id, user_id)
);
"#;

/// Where each application's review card is, so any front end can mark it decided.
pub(crate) const SCHEMA_V6: &str = r#"
ALTER TABLE applications ADD COLUMN card_channel TEXT;
ALTER TABLE applications ADD COLUMN card_message TEXT;
"#;

/// Which scheduled event a reminder is for, so reminders for deleted events are dropped.
pub(crate) const SCHEMA_V8: &str = r#"
ALTER TABLE reminders ADD COLUMN event_id TEXT;
"#;

/// Why an application was rejected (told to the applicant); feeds can be paused.
pub(crate) const SCHEMA_V9: &str = r#"
ALTER TABLE applications ADD COLUMN reason TEXT;
ALTER TABLE feeds ADD COLUMN paused INTEGER NOT NULL DEFAULT 0;
"#;

/// What members answered in the join flow, and when they finished it.
pub(crate) const SCHEMA_V10: &str = r#"
CREATE TABLE join_answers (
    guild_id TEXT NOT NULL, user_id TEXT NOT NULL,
    answers TEXT NOT NULL, finished_at INTEGER, ts INTEGER NOT NULL,
    PRIMARY KEY (guild_id, user_id)
);
"#;

/// Join-flow answers in progress, so they survive a restart (each kept for a while).
pub(crate) const SCHEMA_V13: &str = r#"
CREATE TABLE join_drafts (
    guild_id TEXT NOT NULL, user_id TEXT NOT NULL, draft TEXT NOT NULL, ts INTEGER NOT NULL,
    PRIMARY KEY (guild_id, user_id)
);
"#;

/// Forms members sent that were refused, kept a while so "Edit and resend"
/// can open them again filled in; and applications taken while a position's
/// window is open, which wait for one card listing them all.
pub(crate) const SCHEMA_V14: &str = r#"
CREATE TABLE form_drafts (
    guild_id TEXT NOT NULL, user_id TEXT NOT NULL, form TEXT NOT NULL, draft TEXT NOT NULL, ts INTEGER NOT NULL,
    PRIMARY KEY (guild_id, user_id, form)
);
ALTER TABLE applications ADD COLUMN round INTEGER NOT NULL DEFAULT 0;
"#;

/// Plans posted with live Apply buttons, so an older one can be closed when a
/// newer one is posted or any is applied.
pub(crate) const SCHEMA_V15: &str = r#"
CREATE TABLE plan_posts (
    guild_id TEXT NOT NULL, channel_id TEXT NOT NULL, message_id TEXT NOT NULL, plan_id TEXT NOT NULL,
    ts INTEGER NOT NULL,
    PRIMARY KEY (guild_id, message_id)
);
"#;

/// A shared resource's tags as a list (`items`), not only as "a, b": older
/// rows are split on commas, as they were read.
pub(crate) const SCHEMA_V19: &str = r#"
UPDATE applications SET answers = (
    SELECT json_group_array(CASE
        WHEN json_extract(a.value, '$.question') = 'Tags' AND json_extract(a.value, '$.items') IS NULL
            AND json_valid(a.split) THEN json_set(a.value, '$.items', (
                SELECT json_group_array(trim(t.value)) FROM json_each(a.split) AS t WHERE trim(t.value) != ''
            ))
        ELSE json(a.value)
    END)
    FROM (
        SELECT value, '["' || replace(replace(replace(
            coalesce(json_extract(value, '$.answer'), ''), '\', '\\'), '"', '\"'), ',', '","') || '"]' AS split
        FROM json_each(applications.answers)
    ) AS a
)
WHERE kind = 'resource' AND json_valid(answers) AND json_type(answers) = 'array';
"#;

/// A feed's title, as it last read, so people see it rather than its address.
pub(crate) const SCHEMA_V21: &str = r#"
ALTER TABLE feeds ADD COLUMN title TEXT;
"#;

/// Positions people held when they left, offered back if they return.
pub(crate) const SCHEMA_V4: &str = r#"
CREATE TABLE former_roles (
    guild_id TEXT NOT NULL, user_id TEXT NOT NULL, role_key TEXT NOT NULL, since INTEGER NOT NULL,
    PRIMARY KEY (guild_id, user_id, role_key)
);
"#;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppKind {
    Position,
    Project,
    Feed,
    /// A project idea, posted in the ideas forum once accepted.
    Idea,
    /// A link, posted in the resources forum once accepted.
    Resource,
    /// An event a member suggested, put on the calendar once accepted.
    Event,
    /// What a partner shared, posted here once accepted.
    PartnerResource,
    PartnerQuestion,
    PartnerFeed,
    PartnerEvent,
    PartnerAnnouncement,
}

impl AppKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Position => "position",
            Self::Project => "project",
            Self::Feed => "feed",
            Self::Idea => "idea",
            Self::Resource => "resource",
            Self::Event => "event",
            Self::PartnerResource => "partner-resource",
            Self::PartnerQuestion => "partner-question",
            Self::PartnerFeed => "partner-feed",
            Self::PartnerEvent => "partner-event",
            Self::PartnerAnnouncement => "partner-announcement",
        }
    }
    fn parse(s: &str) -> Self {
        match s {
            "project" => Self::Project,
            "event" => Self::Event,
            "feed" => Self::Feed,
            "idea" => Self::Idea,
            "resource" => Self::Resource,
            "partner-resource" => Self::PartnerResource,
            "partner-question" => Self::PartnerQuestion,
            "partner-feed" => Self::PartnerFeed,
            "partner-event" => Self::PartnerEvent,
            "partner-announcement" => Self::PartnerAnnouncement,
            _ => Self::Position,
        }
    }

    /// What it is, as a heading: the same on review cards, the web, the TUI and the CLI.
    pub fn label(self) -> &'static str {
        match self {
            Self::Position => "Position",
            Self::Project => "Project",
            Self::Idea => "Project idea",
            Self::Resource => "Resource",
            Self::Feed => "Feed",
            Self::Event => "Event suggestion",
            Self::PartnerResource => "Partner resource",
            Self::PartnerQuestion => "Partner question",
            Self::PartnerFeed => "Partner feed",
            Self::PartnerEvent => "Partner event",
            Self::PartnerAnnouncement => "Partner announcement",
        }
    }

    /// A kind as stored (`"partner-resource"`), if it's one this knows.
    pub fn of(kind: &str) -> Option<Self> {
        let k = Self::parse(kind);
        (k.as_str() == kind).then_some(k)
    }

    /// [`AppKind::label`] for a kind as stored (`"partner-resource"`); one this doesn't know reads as it is.
    pub fn label_of(kind: &str) -> String {
        let k = Self::parse(kind);
        if k.as_str() == kind { k.label().to_owned() } else { kind.to_owned() }
    }

    /// Something a partner shared (its `user` is `partner:<their id>`, nobody here).
    pub fn is_partner(self) -> bool {
        matches!(
            self,
            Self::PartnerResource
                | Self::PartnerQuestion
                | Self::PartnerFeed
                | Self::PartnerEvent
                | Self::PartnerAnnouncement
        )
    }
}

/// How an application reaches the review channel.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Round {
    /// On a card of its own, as soon as it's sent.
    #[default]
    Single,
    /// Sent while its position's window was open: waiting for the window to close.
    Waiting,
    /// On the card listing everyone who applied in the window.
    Posted,
}

impl Round {
    fn as_int(self) -> i64 {
        match self {
            Self::Single => 0,
            Self::Waiting => 1,
            Self::Posted => 2,
        }
    }

    fn parse(n: i64) -> Self {
        match n {
            1 => Self::Waiting,
            2 => Self::Posted,
            _ => Self::Single,
        }
    }
}

/// Where an application stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppStatus {
    Pending,
    Accepted,
    Rejected,
}

impl AppStatus {
    /// As stored (and shown).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Accepted => "accepted",
            Self::Rejected => "rejected",
        }
    }

    fn parse(s: &str) -> Self {
        match s {
            "accepted" => Self::Accepted,
            "rejected" => Self::Rejected,
            _ => Self::Pending,
        }
    }
}

impl std::fmt::Display for AppStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Application {
    pub id: i64,
    pub kind: AppKind,
    /// Role key, project slug, feed URL, idea name or resource link.
    pub target: String,
    pub user: String,
    pub answers: String,
    pub as_lead: bool,
    pub status: AppStatus,
    pub created_at: i64,
    /// The review card, as (channel id, message id).
    pub card: Option<(String, String)>,
    /// Whether it has a card of its own or is one of a window's round.
    pub round: Round,
}

/// An application row as stored.
type AppRow = (i64, String, String, String, String, bool, String, i64, Option<String>, Option<String>, i64);

const APP_COLUMNS: &str =
    "id, kind, target, user_id, answers, as_lead, status, created_at, card_channel, card_message, round";

fn application((id, kind, target, user, answers, as_lead, status, created_at, ch, msg, round): AppRow) -> Application {
    let card = ch.zip(msg);
    let status = AppStatus::parse(&status);
    let (kind, round) = (AppKind::parse(&kind), Round::parse(round));
    Application { id, kind, target, user, answers, as_lead, status, created_at, card, round }
}

/// A row of [`APP_COLUMNS`] then reviewed_by, reviewed_at and reason.
fn decision(r: &sqlx::sqlite::SqliteRow) -> Result<Decision> {
    use sqlx::Row as _;
    let g = |i| r.try_get::<Option<String>, _>(i);
    let app: AppRow = (
        r.try_get(0)?,
        r.try_get(1)?,
        r.try_get(2)?,
        r.try_get(3)?,
        r.try_get(4)?,
        r.try_get(5)?,
        r.try_get(6)?,
        r.try_get(7)?,
        g(8)?,
        g(9)?,
        r.try_get(10)?,
    );
    Ok(Decision { app: application(app), reviewed_by: g(11)?, reviewed_at: r.try_get(12)?, reason: g(13)? })
}

/// A decided application: who decided, when, and why (if they said).
#[derive(Debug, Clone, PartialEq)]
pub struct Decision {
    pub app: Application,
    pub reviewed_by: Option<String>,
    pub reviewed_at: Option<i64>,
    pub reason: Option<String>,
}

/// Someone who left holding roles, which they can be given back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Former {
    pub user: String,
    pub roles: Vec<String>,
    /// When they left.
    pub since: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct Archived {
    pub id: String,
    pub channel_id: String,
    pub author_id: String,
    pub author_name: String,
    pub content: String,
    pub ts: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct Feed {
    pub id: i64,
    pub url: String,
    pub channel_key: String,
    pub added_by: Option<String>,
    pub ts: i64,
    /// Not checked for new items while paused.
    pub paused: bool,
    /// Its title, as it last read (none before it was first read).
    pub title: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct AuditEntry {
    pub ts: i64,
    pub actor: Option<String>,
    pub action: String,
    pub target: Option<String>,
    pub detail: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct Reminder {
    pub id: i64,
    pub channel_id: String,
    pub due: i64,
    pub text: String,
    /// The Discord scheduled event it's for, if any.
    pub event_id: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, sqlx::FromRow)]
pub struct Profile {
    pub user_id: String,
    pub name: String,
    pub surname: String,
    pub company: String,
}

/// Someone's join-flow answers (JSON, as `chipsoc_config::join::Answers`).
#[derive(Debug, Clone, Default, PartialEq, Eq, sqlx::FromRow)]
pub struct JoinRecord {
    pub user_id: String,
    pub answers: String,
    pub finished_at: Option<i64>,
    /// Last changed.
    pub ts: i64,
}

/// An apply in the audit log: who applied, and when.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct Applied {
    pub id: i64,
    pub ts: i64,
    pub actor: Option<String>,
}

/// A plan posted with live Apply buttons.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct PlanPost {
    pub channel_id: String,
    pub message_id: String,
    pub plan_id: String,
}

pub struct Guild<'a> {
    pub(crate) pool: &'a SqlitePool,
    pub(crate) guild: &'a str,
}

impl Guild<'_> {
    /// Federation state for the same guild.
    pub fn fed(&self) -> crate::Fed<'_> {
        crate::Fed { pool: self.pool, guild: self.guild }
    }

    /// Replace what a pending application says (a partner updated what it shared).
    pub async fn set_answers(&self, id: i64, answers: &str) -> Result<()> {
        sqlx::query("UPDATE applications SET answers = ? WHERE guild_id = ? AND id = ? AND status = 'pending'")
            .bind(answers)
            .bind(self.guild)
            .bind(id)
            .execute(self.pool)
            .await?;
        Ok(())
    }

    pub async fn add_application(
        &self,
        kind: AppKind,
        target: &str,
        user: &str,
        answers: &str,
        as_lead: bool,
        now: i64,
    ) -> Result<i64> {
        self.add_application_in(kind, target, user, answers, as_lead, now, Round::Single).await
    }

    /// [`Guild::add_application`], in a window's round ([`Round::Waiting`]) or not.
    #[allow(clippy::too_many_arguments)]
    pub async fn add_application_in(
        &self,
        kind: AppKind,
        target: &str,
        user: &str,
        answers: &str,
        as_lead: bool,
        now: i64,
        round: Round,
    ) -> Result<i64> {
        Ok(sqlx::query_scalar(
            "INSERT INTO applications (guild_id, kind, target, user_id, answers, as_lead, created_at, round)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?) RETURNING id",
        )
        .bind(self.guild)
        .bind(kind.as_str())
        .bind(target)
        .bind(user)
        .bind(answers)
        .bind(as_lead)
        .bind(now)
        .bind(round.as_int())
        .fetch_one(self.pool)
        .await?)
    }

    /// Applications for `target` waiting for its window to close, oldest first.
    pub async fn waiting_round(&self, kind: AppKind, target: &str) -> Result<Vec<Application>> {
        let sql = format!(
            "SELECT {APP_COLUMNS} FROM applications WHERE guild_id = ? AND kind = ? AND target = ?
             AND status = 'pending' AND round = 1 ORDER BY id"
        );
        let rows: Vec<AppRow> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
            .bind(self.guild)
            .bind(kind.as_str())
            .bind(target)
            .fetch_all(self.pool)
            .await?;
        Ok(rows.into_iter().map(application).collect())
    }

    /// Take the applications waiting in `target`'s round to post them on a
    /// card: each is taken once, however many callers ask at the same time.
    pub async fn take_round(&self, kind: AppKind, target: &str) -> Result<Vec<Application>> {
        let sql = format!(
            "UPDATE applications SET round = 2 WHERE guild_id = ? AND kind = ? AND target = ?
             AND status = 'pending' AND round = 1 RETURNING {APP_COLUMNS}"
        );
        let rows: Vec<AppRow> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
            .bind(self.guild)
            .bind(kind.as_str())
            .bind(target)
            .fetch_all(self.pool)
            .await?;
        let mut apps: Vec<Application> = rows.into_iter().map(application).collect();
        apps.sort_by_key(|a| a.id);
        Ok(apps)
    }

    /// Put taken applications back to wait (their card couldn't be posted).
    pub async fn untake_round(&self, ids: &[i64]) -> Result<()> {
        for id in ids {
            sqlx::query("UPDATE applications SET round = 1 WHERE guild_id = ? AND id = ? AND round = 2")
                .bind(self.guild)
                .bind(id)
                .execute(self.pool)
                .await?;
        }
        Ok(())
    }

    /// Every application on one review card (a round's card lists several),
    /// with how each was decided, oldest first.
    pub async fn on_card(&self, channel: &str, message: &str) -> Result<Vec<Decision>> {
        let sql = format!(
            "SELECT {APP_COLUMNS}, reviewed_by, reviewed_at, reason FROM applications
             WHERE guild_id = ? AND card_channel = ? AND card_message = ? ORDER BY id"
        );
        let rows: Vec<sqlx::sqlite::SqliteRow> = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(self.guild)
            .bind(channel)
            .bind(message)
            .fetch_all(self.pool)
            .await?;
        rows.iter().map(decision).collect()
    }

    /// Applications in rounds already posted for `target` and still waiting for a decision.
    pub async fn posted_round(&self, kind: AppKind, target: &str) -> Result<Vec<Application>> {
        let sql = format!(
            "SELECT {APP_COLUMNS} FROM applications WHERE guild_id = ? AND kind = ? AND target = ?
             AND status = 'pending' AND round = 2 ORDER BY id"
        );
        let rows: Vec<AppRow> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
            .bind(self.guild)
            .bind(kind.as_str())
            .bind(target)
            .fetch_all(self.pool)
            .await?;
        Ok(rows.into_iter().map(application).collect())
    }

    /// Positions with applications waiting in a window's round.
    pub async fn rounds_waiting(&self) -> Result<Vec<String>> {
        Ok(sqlx::query_scalar(
            "SELECT DISTINCT target FROM applications WHERE guild_id = ? AND kind = 'position'
             AND status = 'pending' AND round = 1 ORDER BY target",
        )
        .bind(self.guild)
        .fetch_all(self.pool)
        .await?)
    }

    /// A form `user` sent that was refused, if it's newer than `after`.
    pub async fn form_draft(&self, user: &str, form: &str, after: i64) -> Result<Option<String>> {
        Ok(sqlx::query_scalar(
            "SELECT draft FROM form_drafts WHERE guild_id = ? AND user_id = ? AND form = ? AND ts > ?",
        )
        .bind(self.guild)
        .bind(user)
        .bind(form)
        .bind(after)
        .fetch_optional(self.pool)
        .await?)
    }

    /// Keep what `user` typed into `form`, replacing any earlier one, and
    /// clear out everyone's from before `expired`.
    pub async fn set_form_draft(&self, user: &str, form: &str, draft: &str, now: i64, expired: i64) -> Result<()> {
        sqlx::query("DELETE FROM form_drafts WHERE guild_id = ? AND ts <= ?")
            .bind(self.guild)
            .bind(expired)
            .execute(self.pool)
            .await?;
        sqlx::query(
            "INSERT INTO form_drafts (guild_id, user_id, form, draft, ts) VALUES (?, ?, ?, ?, ?)
             ON CONFLICT (guild_id, user_id, form) DO UPDATE SET draft = excluded.draft, ts = excluded.ts",
        )
        .bind(self.guild)
        .bind(user)
        .bind(form)
        .bind(draft)
        .bind(now)
        .execute(self.pool)
        .await?;
        Ok(())
    }

    /// Forget a kept form (it went through).
    pub async fn forget_form_draft(&self, user: &str, form: &str) -> Result<()> {
        sqlx::query("DELETE FROM form_drafts WHERE guild_id = ? AND user_id = ? AND form = ?")
            .bind(self.guild)
            .bind(user)
            .bind(form)
            .execute(self.pool)
            .await?;
        Ok(())
    }

    pub async fn application(&self, id: i64) -> Result<Option<Application>> {
        let sql = format!("SELECT {APP_COLUMNS} FROM applications WHERE guild_id = ? AND id = ?");
        let row: Option<AppRow> =
            sqlx::query_as(sqlx::AssertSqlSafe(sql)).bind(self.guild).bind(id).fetch_optional(self.pool).await?;
        Ok(row.map(application))
    }

    /// Applications waiting for a decision, oldest first.
    pub async fn pending_applications(&self) -> Result<Vec<Application>> {
        let sql =
            format!("SELECT {APP_COLUMNS} FROM applications WHERE guild_id = ? AND status = 'pending' ORDER BY id");
        let rows: Vec<AppRow> = sqlx::query_as(sqlx::AssertSqlSafe(sql)).bind(self.guild).fetch_all(self.pool).await?;
        Ok(rows.into_iter().map(application).collect())
    }

    /// Remember where an application's review card was posted.
    pub async fn set_card(&self, id: i64, channel: &str, message: &str) -> Result<()> {
        sqlx::query("UPDATE applications SET card_channel = ?, card_message = ? WHERE guild_id = ? AND id = ?")
            .bind(channel)
            .bind(message)
            .bind(self.guild)
            .bind(id)
            .execute(self.pool)
            .await?;
        Ok(())
    }

    /// Accept or reject a pending application. False if someone already decided it.
    pub async fn decide(&self, id: i64, accept: bool, reviewer: &str, now: i64) -> Result<bool> {
        let status = if accept { AppStatus::Accepted } else { AppStatus::Rejected }.as_str();
        let r = sqlx::query(
            "UPDATE applications SET status = ?, reviewed_by = ?, reviewed_at = ?
             WHERE guild_id = ? AND id = ? AND status = 'pending'",
        )
        .bind(status)
        .bind(reviewer)
        .bind(now)
        .bind(self.guild)
        .bind(id)
        .execute(self.pool)
        .await?;
        Ok(r.rows_affected() == 1)
    }

    /// Say why an application was decided as it was.
    pub async fn set_reason(&self, id: i64, reason: &str) -> Result<()> {
        sqlx::query("UPDATE applications SET reason = ? WHERE guild_id = ? AND id = ?")
            .bind(reason)
            .bind(self.guild)
            .bind(id)
            .execute(self.pool)
            .await?;
        Ok(())
    }

    /// Decided applications, latest decision first.
    pub async fn decided_applications(&self, limit: i64) -> Result<Vec<Decision>> {
        let sql = format!(
            "SELECT {APP_COLUMNS}, reviewed_by, reviewed_at, reason FROM applications
             WHERE guild_id = ? AND status <> 'pending' ORDER BY reviewed_at DESC, id DESC LIMIT ?"
        );
        let rows: Vec<sqlx::sqlite::SqliteRow> =
            sqlx::query(sqlx::AssertSqlSafe(sql)).bind(self.guild).bind(limit).fetch_all(self.pool).await?;
        rows.iter().map(decision).collect()
    }

    /// Put an accepted application back in the queue (accepting it didn't go through).
    pub async fn undecide(&self, id: i64) -> Result<()> {
        sqlx::query(
            "UPDATE applications SET status = 'pending', reviewed_by = NULL, reviewed_at = NULL
             WHERE guild_id = ? AND id = ? AND status = 'accepted'",
        )
        .bind(self.guild)
        .bind(id)
        .execute(self.pool)
        .await?;
        Ok(())
    }

    pub async fn pending(&self) -> Result<i64> {
        Ok(sqlx::query_scalar("SELECT COUNT(*) FROM applications WHERE guild_id = ? AND status = 'pending'")
            .bind(self.guild)
            .fetch_one(self.pool)
            .await?)
    }

    /// Note what `actor` did; the entry's id.
    pub async fn audit(
        &self,
        now: i64,
        actor: Option<&str>,
        action: &str,
        target: Option<&str>,
        detail: Option<&str>,
    ) -> Result<i64> {
        let r = sqlx::query(
            "INSERT INTO audit_log (guild_id, ts, actor, action, target, detail) VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(self.guild)
        .bind(now)
        .bind(actor)
        .bind(action)
        .bind(target)
        .bind(detail)
        .execute(self.pool)
        .await?;
        Ok(r.last_insert_rowid())
    }

    /// What `actor` (as the audit log has them: an id, or a name) did since
    /// `since`, newest first, at most `limit`.
    pub async fn audit_by(&self, actor: &str, since: i64, limit: i64) -> Result<Vec<AuditEntry>> {
        Ok(sqlx::query_as(
            "SELECT ts, actor, action, target, detail FROM audit_log WHERE guild_id = ? AND actor = ? AND ts >= ? \
             ORDER BY id DESC LIMIT ?",
        )
        .bind(self.guild)
        .bind(actor)
        .bind(since)
        .bind(limit)
        .fetch_all(self.pool)
        .await?)
    }

    /// The latest audit-log entries, newest first.
    pub async fn recent_audit(&self, limit: i64) -> Result<Vec<AuditEntry>> {
        Ok(sqlx::query_as(
            "SELECT ts, actor, action, target, detail FROM audit_log WHERE guild_id = ? ORDER BY id DESC LIMIT ?",
        )
        .bind(self.guild)
        .bind(limit)
        .fetch_all(self.pool)
        .await?)
    }

    pub async fn archive_message(&self, m: &Archived) -> Result<()> {
        sqlx::query(
            "INSERT INTO message_archive (id, guild_id, channel_id, author_id, author_name, content, ts) VALUES (?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT (id) DO UPDATE SET content = excluded.content",
        )
        .bind(&m.id)
        .bind(self.guild)
        .bind(&m.channel_id)
        .bind(&m.author_id)
        .bind(&m.author_name)
        .bind(&m.content)
        .bind(m.ts)
        .execute(self.pool)
        .await?;
        Ok(())
    }

    pub async fn archived(&self, id: &str) -> Result<Option<Archived>> {
        Ok(sqlx::query_as(
            "SELECT id, channel_id, author_id, author_name, content, ts FROM message_archive WHERE guild_id = ? AND id = ?",
        )
        .bind(self.guild)
        .bind(id)
        .fetch_optional(self.pool)
        .await?)
    }

    pub async fn mark_deleted(&self, id: &str) -> Result<()> {
        sqlx::query("UPDATE message_archive SET deleted = 1 WHERE guild_id = ? AND id = ?")
            .bind(self.guild)
            .bind(id)
            .execute(self.pool)
            .await?;
        Ok(())
    }

    /// Forget archived messages older than `before`. Returns how many.
    pub async fn prune_messages(&self, before: i64) -> Result<u64> {
        let r = sqlx::query("DELETE FROM message_archive WHERE guild_id = ? AND ts < ?")
            .bind(self.guild)
            .bind(before)
            .execute(self.pool)
            .await?;
        Ok(r.rows_affected())
    }

    /// Register a feed; returns its id (the existing one if already known).
    pub async fn add_feed(&self, url: &str, channel_key: &str, by: &str, now: i64) -> Result<i64> {
        sqlx::query("INSERT OR IGNORE INTO feeds (guild_id, url, channel_key, added_by, ts) VALUES (?, ?, ?, ?, ?)")
            .bind(self.guild)
            .bind(url)
            .bind(channel_key)
            .bind(by)
            .bind(now)
            .execute(self.pool)
            .await?;
        Ok(sqlx::query_scalar("SELECT id FROM feeds WHERE guild_id = ? AND url = ?")
            .bind(self.guild)
            .bind(url)
            .fetch_one(self.pool)
            .await?)
    }

    pub async fn feeds(&self) -> Result<Vec<Feed>> {
        Ok(sqlx::query_as(
            "SELECT id, url, channel_key, added_by, ts, paused, title FROM feeds WHERE guild_id = ? ORDER BY id",
        )
        .bind(self.guild)
        .fetch_all(self.pool)
        .await?)
    }

    pub async fn feed(&self, id: i64) -> Result<Option<Feed>> {
        Ok(sqlx::query_as(
            "SELECT id, url, channel_key, added_by, ts, paused, title FROM feeds WHERE guild_id = ? AND id = ?",
        )
        .bind(self.guild)
        .bind(id)
        .fetch_optional(self.pool)
        .await?)
    }

    /// Keep a feed's title, as it reads now.
    pub async fn set_feed_title(&self, id: i64, title: &str) -> Result<()> {
        sqlx::query("UPDATE feeds SET title = ? WHERE guild_id = ? AND id = ? AND title IS NOT ?")
            .bind(title)
            .bind(self.guild)
            .bind(id)
            .bind(title)
            .execute(self.pool)
            .await?;
        Ok(())
    }

    /// Stop following a feed and forget its items. False if there was no such feed.
    pub async fn delete_feed(&self, id: i64) -> Result<bool> {
        let mut tx = self.pool.begin().await?;
        let r = sqlx::query("DELETE FROM feeds WHERE guild_id = ? AND id = ?")
            .bind(self.guild)
            .bind(id)
            .execute(&mut *tx)
            .await?;
        if r.rows_affected() == 1 {
            sqlx::query("DELETE FROM feed_items WHERE feed_id = ?").bind(id).execute(&mut *tx).await?;
        }
        tx.commit().await?;
        Ok(r.rows_affected() == 1)
    }

    /// Pause or resume a feed. False if there was no such feed.
    pub async fn set_feed_paused(&self, id: i64, paused: bool) -> Result<bool> {
        let r = sqlx::query("UPDATE feeds SET paused = ? WHERE guild_id = ? AND id = ?")
            .bind(paused)
            .bind(self.guild)
            .bind(id)
            .execute(self.pool)
            .await?;
        Ok(r.rows_affected() == 1)
    }

    /// Record a feed item as seen; true if it's new.
    pub async fn mark_item(&self, feed: i64, item: &str) -> Result<bool> {
        let r = sqlx::query("INSERT OR IGNORE INTO feed_items (feed_id, item_id) VALUES (?, ?)")
            .bind(feed)
            .bind(item)
            .execute(self.pool)
            .await?;
        Ok(r.rows_affected() == 1)
    }

    /// Forget that an item was seen, so it's tried again (its post failed).
    pub async fn unmark_item(&self, feed: i64, item: &str) -> Result<()> {
        sqlx::query("DELETE FROM feed_items WHERE feed_id = ? AND item_id = ?")
            .bind(feed)
            .bind(item)
            .execute(self.pool)
            .await?;
        Ok(())
    }

    pub async fn add_reminder(&self, channel: &str, due: i64, text: &str, event: Option<&str>) -> Result<()> {
        sqlx::query("INSERT INTO reminders (guild_id, channel_id, due, text, event_id) VALUES (?, ?, ?, ?, ?)")
            .bind(self.guild)
            .bind(channel)
            .bind(due)
            .bind(text)
            .bind(event)
            .execute(self.pool)
            .await?;
        Ok(())
    }

    /// Reminders due now. Call [`Guild::done_reminder`] once each is sent.
    pub async fn due_reminders(&self, now: i64) -> Result<Vec<Reminder>> {
        Ok(sqlx::query_as("SELECT id, channel_id, due, text, event_id FROM reminders WHERE guild_id = ? AND due <= ?")
            .bind(self.guild)
            .bind(now)
            .fetch_all(self.pool)
            .await?)
    }

    pub async fn done_reminder(&self, id: i64) -> Result<()> {
        sqlx::query("DELETE FROM reminders WHERE guild_id = ? AND id = ?")
            .bind(self.guild)
            .bind(id)
            .execute(self.pool)
            .await?;
        Ok(())
    }

    pub async fn profile(&self, user: &str) -> Result<Option<Profile>> {
        Ok(sqlx::query_as("SELECT user_id, name, surname, company FROM profiles WHERE guild_id = ? AND user_id = ?")
            .bind(self.guild)
            .bind(user)
            .fetch_optional(self.pool)
            .await?)
    }

    pub async fn set_profile(&self, p: &Profile, now: i64) -> Result<()> {
        sqlx::query(
            "INSERT INTO profiles (guild_id, user_id, name, surname, company, ts) VALUES (?, ?, ?, ?, ?, ?)
             ON CONFLICT (guild_id, user_id) DO UPDATE SET name = excluded.name, surname = excluded.surname,
             company = excluded.company, ts = excluded.ts",
        )
        .bind(self.guild)
        .bind(&p.user_id)
        .bind(&p.name)
        .bind(&p.surname)
        .bind(&p.company)
        .bind(now)
        .execute(self.pool)
        .await?;
        Ok(())
    }

    /// A project's slug changed from `from` to `to`, and its role keys as
    /// `keys` (old, new): its applications and the roles people held when they
    /// left follow.
    pub async fn rename_project(&self, from: &str, to: &str, keys: &[(&str, &str)]) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("UPDATE applications SET target = ? WHERE guild_id = ? AND kind = 'project' AND target = ?")
            .bind(to)
            .bind(self.guild)
            .bind(from)
            .execute(&mut *tx)
            .await?;
        for (old, new) in keys {
            sqlx::query("UPDATE OR IGNORE former_roles SET role_key = ? WHERE guild_id = ? AND role_key = ?")
                .bind(new)
                .bind(self.guild)
                .bind(old)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Remember the roles someone held when they left.
    pub async fn remember_roles(&self, user: &str, roles: &[String], now: i64) -> Result<()> {
        for role in roles {
            sqlx::query("INSERT OR REPLACE INTO former_roles (guild_id, user_id, role_key, since) VALUES (?, ?, ?, ?)")
                .bind(self.guild)
                .bind(user)
                .bind(role)
                .bind(now)
                .execute(self.pool)
                .await?;
        }
        Ok(())
    }

    /// The roles someone held when they last left.
    pub async fn former_roles(&self, user: &str) -> Result<Vec<String>> {
        Ok(sqlx::query_scalar(
            "SELECT role_key FROM former_roles WHERE guild_id = ? AND user_id = ? ORDER BY since, role_key",
        )
        .bind(self.guild)
        .bind(user)
        .fetch_all(self.pool)
        .await?)
    }

    /// Everyone remembered as having left with roles, latest first.
    pub async fn former_members(&self) -> Result<Vec<Former>> {
        let rows: Vec<(String, String, i64)> = sqlx::query_as(
            "SELECT user_id, role_key, since FROM former_roles WHERE guild_id = ? ORDER BY since DESC, user_id, role_key",
        )
        .bind(self.guild)
        .fetch_all(self.pool)
        .await?;
        let mut out: Vec<Former> = Vec::new();
        for (user, role, since) in rows {
            match out.iter_mut().find(|f| f.user == user) {
                Some(f) => {
                    f.roles.push(role);
                    f.since = f.since.max(since);
                }
                None => out.push(Former { user, roles: vec![role], since }),
            }
        }
        Ok(out)
    }

    pub async fn forget_roles(&self, user: &str) -> Result<()> {
        sqlx::query("DELETE FROM former_roles WHERE guild_id = ? AND user_id = ?")
            .bind(self.guild)
            .bind(user)
            .execute(self.pool)
            .await?;
        Ok(())
    }

    /// Other profiles giving this name (ignoring case).
    pub async fn profiles_named(&self, name: &str) -> Result<Vec<Profile>> {
        Ok(sqlx::query_as(
            "SELECT user_id, name, surname, company FROM profiles WHERE guild_id = ? AND name = ? COLLATE NOCASE",
        )
        .bind(self.guild)
        .bind(name)
        .fetch_all(self.pool)
        .await?)
    }

    pub async fn web_share(&self, user: &str) -> Result<Option<String>> {
        Ok(sqlx::query_scalar("SELECT invite_id FROM web_shares WHERE guild_id = ? AND user_id = ?")
            .bind(self.guild)
            .bind(user)
            .fetch_optional(self.pool)
            .await?)
    }

    /// Everyone given web access: (user id, invite id).
    pub async fn web_shares(&self) -> Result<Vec<(String, String)>> {
        Ok(sqlx::query_as("SELECT user_id, invite_id FROM web_shares WHERE guild_id = ? ORDER BY user_id")
            .bind(self.guild)
            .fetch_all(self.pool)
            .await?)
    }

    pub async fn set_web_share(&self, user: &str, invite: Option<&str>, now: i64) -> Result<()> {
        match invite {
            Some(i) => sqlx::query(
                "INSERT INTO web_shares (guild_id, user_id, invite_id, ts) VALUES (?, ?, ?, ?)
                 ON CONFLICT (guild_id, user_id) DO UPDATE SET invite_id = excluded.invite_id, ts = excluded.ts",
            )
            .bind(self.guild)
            .bind(user)
            .bind(i)
            .bind(now),
            None => {
                sqlx::query("DELETE FROM web_shares WHERE guild_id = ? AND user_id = ?").bind(self.guild).bind(user)
            }
        }
        .execute(self.pool)
        .await?;
        Ok(())
    }

    pub async fn join_record(&self, user: &str) -> Result<Option<JoinRecord>> {
        Ok(sqlx::query_as(
            "SELECT user_id, answers, finished_at, ts FROM join_answers WHERE guild_id = ? AND user_id = ?",
        )
        .bind(self.guild)
        .bind(user)
        .fetch_optional(self.pool)
        .await?)
    }

    /// Everyone's join-flow answers.
    pub async fn join_records(&self) -> Result<Vec<JoinRecord>> {
        Ok(sqlx::query_as("SELECT user_id, answers, finished_at, ts FROM join_answers WHERE guild_id = ?")
            .bind(self.guild)
            .fetch_all(self.pool)
            .await?)
    }

    /// Save someone's answers; `finished` marks the flow done (the first time is kept).
    pub async fn set_join_answers(&self, user: &str, answers: &str, finished: bool, now: i64) -> Result<()> {
        sqlx::query(
            "INSERT INTO join_answers (guild_id, user_id, answers, finished_at, ts) VALUES (?, ?, ?, ?, ?)
             ON CONFLICT (guild_id, user_id) DO UPDATE SET answers = excluded.answers, ts = excluded.ts,
             finished_at = COALESCE(join_answers.finished_at, excluded.finished_at)",
        )
        .bind(self.guild)
        .bind(user)
        .bind(answers)
        .bind(finished.then_some(now))
        .bind(now)
        .execute(self.pool)
        .await?;
        Ok(())
    }

    /// Someone's join-flow draft (JSON, the runtime's), if one was saved since `after`.
    pub async fn join_draft(&self, user: &str, after: i64) -> Result<Option<String>> {
        Ok(sqlx::query_scalar("SELECT draft FROM join_drafts WHERE guild_id = ? AND user_id = ? AND ts > ?")
            .bind(self.guild)
            .bind(user)
            .bind(after)
            .fetch_optional(self.pool)
            .await?)
    }

    /// Save someone's join-flow draft, and forget everyone's saved before `expired`.
    pub async fn set_join_draft(&self, user: &str, draft: &str, now: i64, expired: i64) -> Result<()> {
        sqlx::query("DELETE FROM join_drafts WHERE guild_id = ? AND ts <= ?")
            .bind(self.guild)
            .bind(expired)
            .execute(self.pool)
            .await?;
        sqlx::query(
            "INSERT INTO join_drafts (guild_id, user_id, draft, ts) VALUES (?, ?, ?, ?)
             ON CONFLICT (guild_id, user_id) DO UPDATE SET draft = excluded.draft, ts = excluded.ts",
        )
        .bind(self.guild)
        .bind(user)
        .bind(draft)
        .bind(now)
        .execute(self.pool)
        .await?;
        Ok(())
    }

    /// Forget someone's join-flow draft (they confirmed it).
    pub async fn forget_join_draft(&self, user: &str) -> Result<()> {
        sqlx::query("DELETE FROM join_drafts WHERE guild_id = ? AND user_id = ?")
            .bind(self.guild)
            .bind(user)
            .execute(self.pool)
            .await?;
        Ok(())
    }

    /// Note a plan posted with live buttons.
    pub async fn add_plan_post(&self, p: &PlanPost, now: i64) -> Result<()> {
        sqlx::query(
            "INSERT OR REPLACE INTO plan_posts (guild_id, channel_id, message_id, plan_id, ts) VALUES (?, ?, ?, ?, ?)",
        )
        .bind(self.guild)
        .bind(&p.channel_id)
        .bind(&p.message_id)
        .bind(&p.plan_id)
        .bind(now)
        .execute(self.pool)
        .await?;
        Ok(())
    }

    /// Plans posted whose buttons are still live, oldest first.
    pub async fn plan_posts(&self) -> Result<Vec<PlanPost>> {
        Ok(sqlx::query_as(
            "SELECT channel_id, message_id, plan_id FROM plan_posts WHERE guild_id = ? ORDER BY ts, rowid",
        )
        .bind(self.guild)
        .fetch_all(self.pool)
        .await?)
    }

    /// Plans posted no later than `ts` whose buttons are still live, oldest first.
    pub async fn plan_posts_until(&self, ts: i64) -> Result<Vec<PlanPost>> {
        Ok(sqlx::query_as(
            "SELECT channel_id, message_id, plan_id FROM plan_posts WHERE guild_id = ? AND ts <= ? ORDER BY ts, rowid",
        )
        .bind(self.guild)
        .bind(ts)
        .fetch_all(self.pool)
        .await?)
    }

    /// Applies in the audit log after entry `after` (by any program sharing
    /// this database: the bot, the web server, the CLI), oldest first.
    pub async fn applies_after(&self, after: i64) -> Result<Vec<Applied>> {
        Ok(sqlx::query_as(
            "SELECT id, ts, actor FROM audit_log WHERE guild_id = ? AND action = 'apply' AND id > ? ORDER BY id",
        )
        .bind(self.guild)
        .bind(after)
        .fetch_all(self.pool)
        .await?)
    }

    /// The newest audit-log entry's id (0 if there's none), to read [`Guild::applies_after`] from.
    /// Any server's: ids are one sequence for all, so it's a fine start for each.
    pub async fn last_audit_id(&self) -> Result<i64> {
        Ok(sqlx::query_scalar("SELECT COALESCE(MAX(id), 0) FROM audit_log").fetch_one(self.pool).await?)
    }

    /// Its buttons are gone (used, dismissed or replaced).
    pub async fn close_plan_post(&self, message: &str) -> Result<()> {
        sqlx::query("DELETE FROM plan_posts WHERE guild_id = ? AND message_id = ?")
            .bind(self.guild)
            .bind(message)
            .execute(self.pool)
            .await?;
        Ok(())
    }

    /// When we first prompted a user about `kind`, if we have.
    pub async fn prompted_at(&self, user: &str, kind: &str) -> Result<Option<i64>> {
        Ok(sqlx::query_scalar("SELECT ts FROM prompted WHERE guild_id = ? AND user_id = ? AND kind = ?")
            .bind(self.guild)
            .bind(user)
            .bind(kind)
            .fetch_optional(self.pool)
            .await?)
    }

    /// Forget that we prompted a user about `kind`.
    pub async fn forget_prompt(&self, user: &str, kind: &str) -> Result<()> {
        sqlx::query("DELETE FROM prompted WHERE guild_id = ? AND user_id = ? AND kind = ?")
            .bind(self.guild)
            .bind(user)
            .bind(kind)
            .execute(self.pool)
            .await?;
        Ok(())
    }

    /// Everyone prompted about `kind`.
    pub async fn prompted(&self, kind: &str) -> Result<std::collections::BTreeSet<String>> {
        let users: Vec<String> = sqlx::query_scalar("SELECT user_id FROM prompted WHERE guild_id = ? AND kind = ?")
            .bind(self.guild)
            .bind(kind)
            .fetch_all(self.pool)
            .await?;
        Ok(users.into_iter().collect())
    }

    /// Record that we prompted a user about `kind`; false if we already had.
    pub async fn first_prompt(&self, user: &str, kind: &str, now: i64) -> Result<bool> {
        let r = sqlx::query("INSERT OR IGNORE INTO prompted (guild_id, user_id, kind, ts) VALUES (?, ?, ?, ?)")
            .bind(self.guild)
            .bind(user)
            .bind(kind)
            .bind(now)
            .execute(self.pool)
            .await?;
        Ok(r.rows_affected() == 1)
    }
}

#[cfg(test)]
mod tests {
    use crate::{AppKind, AppStatus, Archived, PlanPost, Profile, Round, Store};

    #[test]
    fn every_kind_has_one_label() {
        assert_eq!(AppKind::label_of("resource"), "Resource");
        assert_eq!(AppKind::label_of("partner-announcement"), "Partner announcement");
        assert_eq!(AppKind::label_of("event"), "Event suggestion");
        assert_eq!(AppKind::label_of("something-new"), "something-new", "unknown kinds read as stored");
    }

    #[tokio::test]
    async fn plan_posts_are_kept_until_closed() {
        let s = Store::open("sqlite::memory:").await.unwrap();
        let g = s.guild("g1");
        let post = |m: &str| PlanPost { channel_id: "5".into(), message_id: m.into(), plan_id: format!("p{m}") };
        g.add_plan_post(&post("11"), 1).await.unwrap();
        g.add_plan_post(&post("12"), 2).await.unwrap();
        assert_eq!(g.plan_posts().await.unwrap(), [post("11"), post("12")], "oldest first");
        assert!(s.guild("g2").plan_posts().await.unwrap().is_empty(), "scoped by guild");
        g.close_plan_post("11").await.unwrap();
        assert_eq!(g.plan_posts().await.unwrap(), [post("12")]);
    }

    #[tokio::test]
    async fn applies_are_read_from_the_audit_log_in_order() {
        let s = Store::open("sqlite::memory:").await.unwrap();
        let g = s.guild("g1");
        let start = g.last_audit_id().await.unwrap();
        let first = g.audit(5, Some("7"), "apply", None, Some("1 change(s)")).await.unwrap();
        g.audit(6, Some("7"), "appoint", None, None).await.unwrap();
        s.guild("g2").audit(7, Some("8"), "apply", None, None).await.unwrap();
        g.audit(8, Some("ada"), "apply", None, None).await.unwrap();
        let all = g.applies_after(start).await.unwrap();
        let who: Vec<_> = all.iter().map(|a| (a.ts, a.actor.as_deref().unwrap())).collect();
        assert_eq!(who, [(5, "7"), (8, "ada")], "applies only, this guild's, oldest first");
        assert!(g.applies_after(all[1].id).await.unwrap().is_empty());
        assert_eq!(all[0].id, first, "an entry's id is what noting it said");
        g.add_plan_post(&PlanPost { channel_id: "5".into(), message_id: "1".into(), plan_id: "a".into() }, 5)
            .await
            .unwrap();
        g.add_plan_post(&PlanPost { channel_id: "5".into(), message_id: "2".into(), plan_id: "b".into() }, 9)
            .await
            .unwrap();
        let until: Vec<_> = g.plan_posts_until(8).await.unwrap().into_iter().map(|p| p.message_id).collect();
        assert_eq!(until, ["1"], "posted after the apply: left open");
    }

    #[tokio::test]
    async fn applications_are_decided_once() {
        let s = Store::open("sqlite::memory:").await.unwrap();
        let g = s.guild("g1");
        let id = g.add_application(AppKind::Position, "treasurer", "u1", "{}", false, 100).await.unwrap();
        assert_eq!(g.pending().await.unwrap(), 1);
        assert!(s.guild("g2").application(id).await.unwrap().is_none(), "scoped by guild");
        assert!(g.decide(id, true, "mod1", 200).await.unwrap());
        assert!(!g.decide(id, false, "mod2", 201).await.unwrap(), "a second click loses");
        g.undecide(id).await.unwrap();
        assert_eq!(g.pending().await.unwrap(), 1, "back in the queue");
        assert!(g.decide(id, true, "mod1", 202).await.unwrap());
        let a = g.application(id).await.unwrap().unwrap();
        assert_eq!((a.kind, a.status, a.created_at), (AppKind::Position, AppStatus::Accepted, 100));
    }

    #[tokio::test]
    async fn pending_applications_carry_their_card() {
        let s = Store::open("sqlite::memory:").await.unwrap();
        let g = s.guild("g1");
        let a = g.add_application(AppKind::Project, "golem", "u1", "[]", true, 1).await.unwrap();
        let b = g.add_application(AppKind::Feed, "https://x", "u2", "[]", false, 2).await.unwrap();
        g.set_card(a, "10", "11").await.unwrap();
        g.decide(b, false, "m", 3).await.unwrap();
        let pending = g.pending_applications().await.unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!((pending[0].id, pending[0].card.clone()), (a, Some(("10".into(), "11".into()))));
        assert!(s.guild("g2").pending_applications().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn feeds_are_removed_with_their_items_and_audit_is_listed() {
        let s = Store::open("sqlite::memory:").await.unwrap();
        let g = s.guild("g1");
        let f = g.add_feed("https://x/feed", "feeds", "u", 7).await.unwrap();
        g.mark_item(f, "i").await.unwrap();
        assert_eq!(g.feed(f).await.unwrap().map(|f| (f.added_by, f.ts)), Some((Some("u".into()), 7)));
        assert!(!s.guild("g2").delete_feed(f).await.unwrap(), "scoped by guild");
        assert!(g.delete_feed(f).await.unwrap());
        assert!(g.feeds().await.unwrap().is_empty());
        let f = g.add_feed("https://x/feed", "feeds", "u", 8).await.unwrap();
        assert!(g.mark_item(f, "i").await.unwrap(), "items forgotten");

        g.audit(1, Some("a"), "first", None, None).await.unwrap();
        g.audit(2, None, "second", Some("t"), Some("d")).await.unwrap();
        let log = g.recent_audit(1).await.unwrap();
        assert_eq!((log.len(), log[0].action.as_str(), log[0].target.as_deref()), (1, "second", Some("t")));
    }

    #[tokio::test]
    async fn messages_feeds_reminders_profiles() {
        let s = Store::open("sqlite::memory:").await.unwrap();
        let g = s.guild("g1");
        let m = Archived {
            id: "m".into(),
            channel_id: "c".into(),
            author_id: "a".into(),
            author_name: "A".into(),
            content: "hi".into(),
            ts: 10,
        };
        g.archive_message(&m).await.unwrap();
        assert_eq!(g.archived("m").await.unwrap(), Some(m));
        assert_eq!(g.prune_messages(11).await.unwrap(), 1);

        let f = g.add_feed("https://x/feed", "feeds", "u", 1).await.unwrap();
        assert_eq!(g.add_feed("https://x/feed", "feeds", "u", 2).await.unwrap(), f);
        assert_ne!(s.guild("g2").add_feed("https://x/feed", "feeds", "u", 2).await.unwrap(), f, "per guild");
        assert!(g.mark_item(f, "i").await.unwrap() && !g.mark_item(f, "i").await.unwrap());

        g.add_reminder("c", 5, "go", Some("77")).await.unwrap();
        let due = g.due_reminders(6).await.unwrap();
        assert_eq!((due.len(), due[0].event_id.as_deref()), (1, Some("77")));
        g.done_reminder(due[0].id).await.unwrap();
        assert!(g.due_reminders(6).await.unwrap().is_empty());

        let p = Profile { user_id: "u".into(), name: "Ada".into(), surname: "Lovelace".into(), company: String::new() };
        g.set_profile(&p, 1).await.unwrap();
        assert_eq!(g.profile("u").await.unwrap(), Some(p));
        assert!(g.first_prompt("u", "profile", 1).await.unwrap());
        assert!(!g.first_prompt("u", "profile", 2).await.unwrap());
        assert_eq!(g.profiles_named("ADA").await.unwrap().len(), 1);
        assert!(s.guild("g2").profiles_named("Ada").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn join_answers_keep_the_first_finish() {
        let s = Store::open("sqlite::memory:").await.unwrap();
        let g = s.guild("g");
        g.set_join_answers("u", "{}", false, 1).await.unwrap();
        assert_eq!(g.join_record("u").await.unwrap().unwrap().finished_at, None);
        g.set_join_answers("u", "{\"a\":{}}", true, 2).await.unwrap();
        g.set_join_answers("u", "{\"b\":{}}", true, 3).await.unwrap();
        let r = g.join_record("u").await.unwrap().unwrap();
        assert_eq!((r.answers.as_str(), r.finished_at, r.ts), ("{\"b\":{}}", Some(2), 3));
        assert!(s.guild("g2").join_records().await.unwrap().is_empty());
        assert_eq!(g.prompted_at("u", "join:notify").await.unwrap(), None);
        assert!(g.first_prompt("u", "join:notify", 5).await.unwrap());
        assert_eq!(g.prompted_at("u", "join:notify").await.unwrap(), Some(5));
        assert_eq!(g.prompted("join:notify").await.unwrap(), ["u".to_string()].into());
        g.forget_prompt("u", "join:notify").await.unwrap();
        assert!(g.first_prompt("u", "join:notify", 6).await.unwrap());
    }

    #[tokio::test]
    async fn web_shares_are_listed_per_guild() {
        let s = Store::open("sqlite::memory:").await.unwrap();
        let g = s.guild("g1");
        g.set_web_share("u2", Some("i2"), 1).await.unwrap();
        g.set_web_share("u1", Some("i1"), 1).await.unwrap();
        s.guild("g2").set_web_share("u3", Some("i3"), 1).await.unwrap();
        assert_eq!(g.web_shares().await.unwrap(), [("u1".into(), "i1".into()), ("u2".into(), "i2".into())]);
        g.set_web_share("u1", None, 2).await.unwrap();
        assert_eq!(g.web_shares().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn former_roles_are_remembered_until_forgotten() {
        let s = Store::open("sqlite::memory:").await.unwrap();
        let g = s.guild("g1");
        g.remember_roles("u", &["chair".into(), "proj-golem-lead".into()], 5).await.unwrap();
        g.remember_roles("u", &["chair".into()], 6).await.unwrap();
        assert_eq!(g.former_roles("u").await.unwrap(), ["proj-golem-lead", "chair"]);
        assert!(s.guild("g2").former_roles("u").await.unwrap().is_empty());
        g.remember_roles("v", &["treasurer".into()], 9).await.unwrap();
        let former = g.former_members().await.unwrap();
        let listed: Vec<(&str, usize, i64)> =
            former.iter().map(|f| (f.user.as_str(), f.roles.len(), f.since)).collect();
        assert_eq!(listed, [("v", 1, 9), ("u", 2, 6)], "latest first, roles together");
        g.forget_roles("u").await.unwrap();
        assert!(g.former_roles("u").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn decisions_keep_their_reason_and_feeds_pause() {
        let s = Store::open("sqlite::memory:").await.unwrap();
        let g = s.guild("g1");
        let a = g.add_application(AppKind::Position, "chair", "u1", "[]", false, 1).await.unwrap();
        let b = g.add_application(AppKind::Project, "golem", "u2", "[]", false, 2).await.unwrap();
        g.add_application(AppKind::Project, "golem", "u3", "[]", false, 3).await.unwrap();
        g.decide(a, false, "m1", 10).await.unwrap();
        g.set_reason(a, "Full this year").await.unwrap();
        g.decide(b, true, "m2", 20).await.unwrap();
        let past = g.decided_applications(10).await.unwrap();
        let seen: Vec<(i64, &str, Option<&str>)> =
            past.iter().map(|d| (d.app.id, d.app.status.as_str(), d.reason.as_deref())).collect();
        assert_eq!(
            seen,
            [(b, "accepted", None), (a, "rejected", Some("Full this year"))],
            "latest first, pending left out"
        );
        assert_eq!((past[1].reviewed_by.as_deref(), past[1].reviewed_at), (Some("m1"), Some(10)));
        assert!(s.guild("g2").decided_applications(10).await.unwrap().is_empty());

        let f = g.add_feed("https://x/feed", "feeds", "u", 1).await.unwrap();
        assert!(!g.feed(f).await.unwrap().unwrap().paused);
        assert!(g.set_feed_paused(f, true).await.unwrap());
        assert!(g.feeds().await.unwrap()[0].paused);
        assert_eq!(g.feed(f).await.unwrap().unwrap().title, None, "not read yet");
        g.set_feed_title(f, "Chip news").await.unwrap();
        assert_eq!(g.feeds().await.unwrap()[0].title.as_deref(), Some("Chip news"));
        assert!(!s.guild("g2").set_feed_paused(f, false).await.unwrap(), "scoped by guild");
    }

    #[tokio::test]
    async fn join_drafts_are_kept_per_member_until_they_expire() {
        let s = Store::open("sqlite::memory:").await.unwrap();
        let g = s.guild("g1");
        g.set_join_draft("1", "{\"a\":1}", 100, 0).await.unwrap();
        g.set_join_draft("2", "{}", 150, 0).await.unwrap();
        assert_eq!(g.join_draft("1", 50).await.unwrap().as_deref(), Some("{\"a\":1}"));
        assert_eq!(g.join_draft("1", 100).await.unwrap(), None, "too old to use");
        assert_eq!(s.guild("g2").join_draft("1", 0).await.unwrap(), None, "per server");
        g.set_join_draft("1", "{\"a\":2}", 200, 120).await.unwrap();
        assert_eq!(g.join_draft("1", 0).await.unwrap().as_deref(), Some("{\"a\":2}"), "replaced");
        assert_eq!(g.join_draft("2", 0).await.unwrap().as_deref(), Some("{}"));
        g.set_join_draft("3", "{}", 300, 160).await.unwrap();
        assert_eq!(g.join_draft("2", 0).await.unwrap(), None, "expired ones are cleared out");
        g.forget_join_draft("1").await.unwrap();
        assert_eq!(g.join_draft("1", 0).await.unwrap(), None);
    }

    #[tokio::test]
    async fn a_window_s_round_is_taken_once_and_its_card_lists_them() {
        let s = Store::open("sqlite::memory:").await.unwrap();
        let g = s.guild("g1");
        let a = g.add_application_in(AppKind::Position, "chair", "1", "[]", false, 1, Round::Waiting).await.unwrap();
        let b = g.add_application_in(AppKind::Position, "chair", "2", "[]", false, 2, Round::Waiting).await.unwrap();
        g.add_application(AppKind::Position, "chair", "3", "[]", false, 3).await.unwrap();
        assert_eq!(g.rounds_waiting().await.unwrap(), ["chair"]);
        assert_eq!(g.waiting_round(AppKind::Position, "chair").await.unwrap().len(), 2);
        let (first, second) =
            tokio::join!(g.take_round(AppKind::Position, "chair"), g.take_round(AppKind::Position, "chair"));
        let taken: Vec<i64> = first.unwrap().iter().chain(&second.unwrap()).map(|a| a.id).collect();
        assert_eq!(taken, [a, b], "each taken once");
        assert!(g.rounds_waiting().await.unwrap().is_empty());
        for id in [a, b] {
            g.set_card(id, "c", "m").await.unwrap();
        }
        g.decide(a, false, "9", 5).await.unwrap();
        g.set_reason(a, "Not this year").await.unwrap();
        let card = g.on_card("c", "m").await.unwrap();
        assert_eq!(card.len(), 2);
        assert_eq!(card[0].app.round, Round::Posted);
        assert_eq!((card[0].reviewed_by.as_deref(), card[0].reason.as_deref()), (Some("9"), Some("Not this year")));
        assert_eq!(g.posted_round(AppKind::Position, "chair").await.unwrap()[0].id, b);
        g.untake_round(&[b]).await.unwrap();
        assert_eq!(g.waiting_round(AppKind::Position, "chair").await.unwrap()[0].id, b, "back to waiting");
        let e = g.add_application(AppKind::Event, "Social", "4", "[]", false, 6).await.unwrap();
        assert_eq!(g.application(e).await.unwrap().unwrap().kind, AppKind::Event);
    }

    #[tokio::test]
    async fn form_drafts_are_kept_per_member_and_form_for_a_while() {
        let s = Store::open("sqlite::memory:").await.unwrap();
        let g = s.guild("g1");
        g.set_form_draft("1", "idea", "{\"name\":\"A\"}", 100, 0).await.unwrap();
        g.set_form_draft("1", "feed", "{}", 100, 0).await.unwrap();
        assert_eq!(g.form_draft("1", "idea", 50).await.unwrap().as_deref(), Some("{\"name\":\"A\"}"));
        assert_eq!(g.form_draft("1", "idea", 100).await.unwrap(), None, "too old");
        assert_eq!(g.form_draft("2", "idea", 0).await.unwrap(), None, "someone else's");
        g.forget_form_draft("1", "idea").await.unwrap();
        assert_eq!(g.form_draft("1", "idea", 0).await.unwrap(), None);
        assert_eq!(g.form_draft("1", "feed", 0).await.unwrap().as_deref(), Some("{}"), "other forms kept");
        g.set_form_draft("2", "idea", "{}", 300, 200).await.unwrap();
        assert_eq!(g.form_draft("1", "feed", 0).await.unwrap(), None, "expired ones are cleared out");
    }
}

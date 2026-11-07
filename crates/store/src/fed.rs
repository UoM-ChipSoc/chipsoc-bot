//! Federation state: replay nonces, partners' catalogues, roles we granted on a
//! partner's word (and which of their keys signed it), what we last sent each
//! partner, the keys we last knew each partner by, and inbox messages not yet
//! fully handled.

use anyhow::Result;
use sqlx::SqlitePool;

/// Federation tables, scoped by guild. Replaces the v1 tables (never used with a real peer).
pub(crate) const SCHEMA_V2: &str = r#"
DROP TABLE IF EXISTS fed_nonces;
DROP TABLE IF EXISTS fed_catalog;
DROP TABLE IF EXISTS fed_grants;
CREATE TABLE fed_nonces (
    guild_id TEXT NOT NULL, peer TEXT NOT NULL, nonce TEXT NOT NULL, ts INTEGER NOT NULL,
    PRIMARY KEY (guild_id, peer, nonce)
);
CREATE TABLE fed_catalog (
    guild_id TEXT NOT NULL, peer TEXT NOT NULL, body TEXT NOT NULL, ts INTEGER NOT NULL, seq INTEGER NOT NULL,
    PRIMARY KEY (guild_id, peer)
);
CREATE TABLE fed_grants (
    guild_id TEXT NOT NULL, peer TEXT NOT NULL, user_id TEXT NOT NULL, role_key TEXT NOT NULL,
    PRIMARY KEY (guild_id, peer, user_id, role_key)
);
-- Newest seq applied per partner member, so late envelopes can't undo newer state.
CREATE TABLE fed_seen (
    guild_id TEXT NOT NULL, peer TEXT NOT NULL, user_id TEXT NOT NULL, seq INTEGER NOT NULL,
    PRIMARY KEY (guild_id, peer, user_id)
);
-- Hash of what we last sent a partner, per kind, to send only changes.
CREATE TABLE fed_sent (
    guild_id TEXT NOT NULL, peer TEXT NOT NULL, kind TEXT NOT NULL, hash TEXT NOT NULL,
    PRIMARY KEY (guild_id, peer, kind)
);
"#;

/// Inbox messages kept until handled, so none is lost to a restart or an error;
/// the key that signed each grant, so a removed key's grants can be taken back;
/// when each grant was made, for the mass-grant guard; and what we know of each
/// partner.
pub(crate) const SCHEMA_V7: &str = r#"
CREATE TABLE fed_inbox (
    guild_id TEXT NOT NULL, msg_id INTEGER NOT NULL, content TEXT NOT NULL, received INTEGER NOT NULL,
    state TEXT NOT NULL DEFAULT 'new', tries INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (guild_id, msg_id)
);
ALTER TABLE fed_nonces ADD COLUMN msg_id INTEGER;
ALTER TABLE fed_grants ADD COLUMN kid TEXT;
ALTER TABLE fed_grants ADD COLUMN granted_at INTEGER NOT NULL DEFAULT 0;
CREATE TABLE fed_peers (
    guild_id TEXT NOT NULL, peer TEXT NOT NULL, kids TEXT NOT NULL DEFAULT '',
    snapshot_seq INTEGER NOT NULL DEFAULT 0, window_from INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (guild_id, peer)
);
"#;

/// The held batch each held inbox message is in (what an admin's "Allow"
/// button names); whether a grant's role may not have reached Discord yet;
/// partner-word revocations, for the mass-revoke guard; and member updates
/// that failed part-way, to retry.
pub(crate) const SCHEMA_V11: &str = r#"
ALTER TABLE fed_inbox ADD COLUMN batch TEXT;
ALTER TABLE fed_grants ADD COLUMN pending INTEGER NOT NULL DEFAULT 0;
CREATE TABLE fed_revokes (
    guild_id TEXT NOT NULL, peer TEXT NOT NULL, user_id TEXT NOT NULL, at INTEGER NOT NULL
);
CREATE INDEX fed_revokes_by_peer ON fed_revokes (guild_id, peer, at);
CREATE TABLE fed_retry (
    guild_id TEXT NOT NULL, peer TEXT NOT NULL, user_id TEXT NOT NULL, member TEXT NOT NULL,
    kid TEXT NOT NULL, tries INTEGER NOT NULL,
    PRIMARY KEY (guild_id, peer, user_id)
);
"#;

/// Shared content: what we've shared with each partner (to send, and to
/// follow up when it's edited or taken down), and what partners shared with
/// us (to skip repeats, and to update or archive what we posted of it).
pub(crate) const SCHEMA_V17: &str = r#"
CREATE TABLE fed_share_out (
    guild_id TEXT NOT NULL, peer TEXT NOT NULL, what TEXT NOT NULL, origin TEXT NOT NULL,
    body TEXT NOT NULL, removed INTEGER NOT NULL DEFAULT 0, pending INTEGER NOT NULL DEFAULT 1,
    tries INTEGER NOT NULL DEFAULT 0, changed INTEGER NOT NULL,
    PRIMARY KEY (guild_id, peer, what, origin)
);
CREATE TABLE fed_mirrors (
    guild_id TEXT NOT NULL, peer TEXT NOT NULL, what TEXT NOT NULL, origin TEXT NOT NULL,
    seq INTEGER NOT NULL, state TEXT NOT NULL, body TEXT NOT NULL, app_id INTEGER,
    channel_id TEXT, message_id TEXT, at INTEGER NOT NULL,
    PRIMARY KEY (guild_id, peer, what, origin)
);
CREATE INDEX fed_mirrors_by_app ON fed_mirrors (guild_id, app_id);
"#;

/// A partner's edit of something posted here, waiting for review (its body
/// and queue entry); who shared each of our items (only they or an admin
/// may change it at partners), and when it was first shared (for the caps).
pub(crate) const SCHEMA_V18: &str = r#"
ALTER TABLE fed_mirrors ADD COLUMN edit_body TEXT;
ALTER TABLE fed_mirrors ADD COLUMN edit_app INTEGER;
CREATE INDEX fed_mirrors_by_edit ON fed_mirrors (guild_id, edit_app);
ALTER TABLE fed_share_out ADD COLUMN author TEXT;
ALTER TABLE fed_share_out ADD COLUMN shared_at INTEGER NOT NULL DEFAULT 0;
"#;

/// Something shared with a partner, sent or waiting to be.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct OutShare {
    pub peer: String,
    pub what: String,
    pub origin: String,
    /// The item, as JSON.
    pub body: String,
    /// Taken down: tell them so.
    pub removed: bool,
    pub pending: bool,
    pub tries: i64,
    /// When it last changed: a send counts only if nothing changed since.
    pub changed: i64,
}

/// Where something a partner shared is up to here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MirrorState {
    /// Waiting in the review queue.
    Review,
    Posted,
    /// Rejected here: later updates are ignored.
    Refused,
    /// They took it down, or what we posted of it was deleted here.
    Removed,
}

impl MirrorState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Review => "review",
            Self::Posted => "posted",
            Self::Refused => "refused",
            Self::Removed => "removed",
        }
    }

    fn parse(s: &str) -> Self {
        match s {
            "posted" => Self::Posted,
            "refused" => Self::Refused,
            "removed" => Self::Removed,
            _ => Self::Review,
        }
    }
}

/// Something a partner shared, as we have it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mirror {
    pub peer: String,
    pub what: String,
    pub origin: String,
    /// The newest envelope applied, so a late one never undoes a newer.
    pub seq: u64,
    pub state: MirrorState,
    /// The item, as JSON.
    pub body: String,
    /// Its review-queue entry.
    pub app_id: Option<i64>,
    /// Where it's posted: a channel and message (a forum post's are the same id), or an event (no channel).
    pub channel_id: Option<String>,
    pub message_id: Option<String>,
    /// Their edit of what's posted, waiting for review (the item, as JSON).
    pub edit_body: Option<String>,
    /// That edit's review-queue entry.
    pub edit_app: Option<i64>,
}

type MirrorRow = (
    String,
    String,
    String,
    i64,
    String,
    String,
    Option<i64>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<i64>,
);

fn mirror(
    (peer, what, origin, seq, state, body, app_id, channel_id, message_id, edit_body, edit_app): MirrorRow,
) -> Mirror {
    Mirror {
        peer,
        what,
        origin,
        seq: seq as u64,
        state: MirrorState::parse(&state),
        body,
        app_id,
        channel_id,
        message_id,
        edit_body,
        edit_app,
    }
}

const MIRROR_COLUMNS: &str =
    "peer, what, origin, seq, state, body, app_id, channel_id, message_id, edit_body, edit_app";

/// Where an inbox message is up to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InboxState {
    /// Not handled yet, or to be retried.
    New,
    /// Handled; waiting for the rest of its sync.
    Waiting,
    /// Held for an admin to allow.
    Held,
    Done,
}

impl InboxState {
    fn as_str(self) -> &'static str {
        match self {
            Self::New => "new",
            Self::Waiting => "wait",
            Self::Held => "held",
            Self::Done => "done",
        }
    }

    fn parse(s: &str) -> Self {
        match s {
            "wait" => Self::Waiting,
            "held" => Self::Held,
            "done" => Self::Done,
            _ => Self::New,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InboxMessage {
    pub msg: u64,
    pub content: String,
    pub state: InboxState,
    pub tries: u32,
    /// The held batch it's in, once an admin has been asked about it.
    pub batch: Option<String>,
}

/// A member update that failed part-way, to try again.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct Retry {
    pub peer: String,
    pub user_id: String,
    /// The member's attested state, as JSON.
    pub member: String,
    pub kid: String,
    pub tries: i64,
}

/// A grant whose role may not have reached Discord (the bot stopped between
/// recording it and giving the role).
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct PendingGrant {
    pub peer: String,
    pub user_id: String,
    pub role_key: String,
}

#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct Grant {
    pub user_id: String,
    pub role_key: String,
    /// The partner key that signed it; None if granted before keys were recorded.
    pub kid: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PeerRecord {
    /// Key ids we last knew them by.
    pub kids: Vec<String>,
    /// `as_of` of the newest full sync applied.
    pub snapshot_seq: u64,
    /// Grants made before this time count as established for the mass-grant guard.
    pub window_from: i64,
}

#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
pub struct Catalog {
    pub peer: String,
    pub body: String,
    pub ts: i64,
}

pub struct Fed<'a> {
    pub(crate) pool: &'a SqlitePool,
    pub(crate) guild: &'a str,
}

impl Fed<'_> {
    /// Remember a nonce, seen in inbox message `msg`; false if another message
    /// already used it (handling the same message again is fine). Forgets
    /// nonces older than `expire_before`.
    pub async fn fresh_nonce(&self, peer: &str, nonce: &str, ts: i64, expire_before: i64, msg: u64) -> Result<bool> {
        sqlx::query("DELETE FROM fed_nonces WHERE guild_id = ? AND ts < ?")
            .bind(self.guild)
            .bind(expire_before)
            .execute(self.pool)
            .await?;
        let r =
            sqlx::query("INSERT OR IGNORE INTO fed_nonces (guild_id, peer, nonce, ts, msg_id) VALUES (?, ?, ?, ?, ?)")
                .bind(self.guild)
                .bind(peer)
                .bind(nonce)
                .bind(ts)
                .bind(msg as i64)
                .execute(self.pool)
                .await?;
        if r.rows_affected() == 1 {
            return Ok(true);
        }
        let by: Option<i64> =
            sqlx::query_scalar("SELECT msg_id FROM fed_nonces WHERE guild_id = ? AND peer = ? AND nonce = ?")
                .bind(self.guild)
                .bind(peer)
                .bind(nonce)
                .fetch_optional(self.pool)
                .await?
                .flatten();
        Ok(by == Some(msg as i64))
    }

    /// Store a partner's catalogue unless we already have a newer one.
    pub async fn set_catalog(&self, peer: &str, body: &str, ts: i64, seq: u64) -> Result<()> {
        sqlx::query(
            "INSERT INTO fed_catalog (guild_id, peer, body, ts, seq) VALUES (?, ?, ?, ?, ?)
             ON CONFLICT (guild_id, peer) DO UPDATE SET body = excluded.body, ts = excluded.ts, seq = excluded.seq
             WHERE excluded.seq > fed_catalog.seq",
        )
        .bind(self.guild)
        .bind(peer)
        .bind(body)
        .bind(ts)
        .bind(seq as i64)
        .execute(self.pool)
        .await?;
        Ok(())
    }

    pub async fn catalogs(&self) -> Result<Vec<Catalog>> {
        Ok(sqlx::query_as("SELECT peer, body, ts FROM fed_catalog WHERE guild_id = ? ORDER BY peer")
            .bind(self.guild)
            .fetch_all(self.pool)
            .await?)
    }

    /// Claim `seq` as the newest state for a partner member; false if we've
    /// applied newer. The same seq can be claimed again, so a message handled
    /// again after a restart still applies.
    pub async fn claim_seq(&self, peer: &str, user: &str, seq: u64) -> Result<bool> {
        let r = sqlx::query(
            "INSERT INTO fed_seen (guild_id, peer, user_id, seq) VALUES (?, ?, ?, ?)
             ON CONFLICT (guild_id, peer, user_id) DO UPDATE SET seq = excluded.seq WHERE excluded.seq >= fed_seen.seq",
        )
        .bind(self.guild)
        .bind(peer)
        .bind(user)
        .bind(seq as i64)
        .execute(self.pool)
        .await?;
        Ok(r.rows_affected() == 1)
    }

    /// Forget the seqs and the full sync applied from a partner, so newer
    /// state from it applies again (after one of its keys was removed).
    pub async fn forget_seqs(&self, peer: &str) -> Result<()> {
        sqlx::query("DELETE FROM fed_seen WHERE guild_id = ? AND peer = ?")
            .bind(self.guild)
            .bind(peer)
            .execute(self.pool)
            .await?;
        sqlx::query("UPDATE fed_peers SET snapshot_seq = 0 WHERE guild_id = ? AND peer = ?")
            .bind(self.guild)
            .bind(peer)
            .execute(self.pool)
            .await?;
        Ok(())
    }

    /// Forget seqs above `max` (from the future, so they'd block every real
    /// update) or below `min` (older than any envelope still accepted).
    pub async fn prune_seqs(&self, min: u64, max: u64) -> Result<()> {
        let (min, max) = (min.min(i64::MAX as u64) as i64, max.min(i64::MAX as u64) as i64);
        sqlx::query("DELETE FROM fed_seen WHERE guild_id = ? AND (seq < ? OR seq > ?)")
            .bind(self.guild)
            .bind(min)
            .bind(max)
            .execute(self.pool)
            .await?;
        sqlx::query(
            "UPDATE fed_peers SET snapshot_seq = 0 WHERE guild_id = ? AND (snapshot_seq < 0 OR snapshot_seq > ?)",
        )
        .bind(self.guild)
        .bind(max)
        .execute(self.pool)
        .await?;
        Ok(())
    }

    /// A role's key changed (a project was renamed): partners' grants of it follow.
    pub async fn rename_role(&self, old: &str, new: &str) -> Result<()> {
        sqlx::query("UPDATE OR IGNORE fed_grants SET role_key = ? WHERE guild_id = ? AND role_key = ?")
            .bind(new)
            .bind(self.guild)
            .bind(old)
            .execute(self.pool)
            .await?;
        Ok(())
    }

    /// Role keys granted to a user on a partner's word.
    pub async fn grants(&self, peer: &str, user: &str) -> Result<Vec<String>> {
        Ok(sqlx::query_scalar("SELECT role_key FROM fed_grants WHERE guild_id = ? AND peer = ? AND user_id = ?")
            .bind(self.guild)
            .bind(peer)
            .bind(user)
            .fetch_all(self.pool)
            .await?)
    }

    /// Users holding any grant from a partner.
    pub async fn granted_users(&self, peer: &str) -> Result<Vec<String>> {
        Ok(sqlx::query_scalar("SELECT DISTINCT user_id FROM fed_grants WHERE guild_id = ? AND peer = ?")
            .bind(self.guild)
            .bind(peer)
            .fetch_all(self.pool)
            .await?)
    }

    /// Record a grant, made at `now` on a message signed by key `kid`.
    pub async fn grant(&self, peer: &str, user: &str, role: &str, kid: &str, now: i64) -> Result<()> {
        sqlx::query(
            "INSERT INTO fed_grants (guild_id, peer, user_id, role_key, kid, granted_at) VALUES (?, ?, ?, ?, ?, ?)
             ON CONFLICT (guild_id, peer, user_id, role_key) DO UPDATE SET kid = excluded.kid",
        )
        .bind(self.guild)
        .bind(peer)
        .bind(user)
        .bind(role)
        .bind(kid)
        .bind(now)
        .execute(self.pool)
        .await?;
        Ok(())
    }

    /// Record a grant before giving the role, so a crash in between leaves a
    /// grant to check rather than a role nobody knows a partner gave.
    pub async fn grant_pending(&self, peer: &str, user: &str, role: &str, kid: &str, now: i64) -> Result<()> {
        sqlx::query(
            "INSERT INTO fed_grants (guild_id, peer, user_id, role_key, kid, granted_at, pending) VALUES (?, ?, ?, ?, ?, ?, 1)
             ON CONFLICT (guild_id, peer, user_id, role_key) DO UPDATE SET kid = excluded.kid",
        )
        .bind(self.guild)
        .bind(peer)
        .bind(user)
        .bind(role)
        .bind(kid)
        .bind(now)
        .execute(self.pool)
        .await?;
        Ok(())
    }

    /// The role of a pending grant reached Discord.
    pub async fn confirm_grant(&self, peer: &str, user: &str, role: &str) -> Result<()> {
        sqlx::query(
            "UPDATE fed_grants SET pending = 0 WHERE guild_id = ? AND peer = ? AND user_id = ? AND role_key = ?",
        )
        .bind(self.guild)
        .bind(peer)
        .bind(user)
        .bind(role)
        .execute(self.pool)
        .await?;
        Ok(())
    }

    /// Grants whose role may not have reached Discord.
    pub async fn pending_grants(&self) -> Result<Vec<PendingGrant>> {
        Ok(sqlx::query_as(
            "SELECT peer, user_id, role_key FROM fed_grants WHERE guild_id = ? AND pending = 1 ORDER BY peer, user_id",
        )
        .bind(self.guild)
        .fetch_all(self.pool)
        .await?)
    }

    /// The partner vouched again for a user's grants, with key `kid`.
    pub async fn set_grant_kid(&self, peer: &str, user: &str, kid: &str) -> Result<()> {
        sqlx::query("UPDATE fed_grants SET kid = ? WHERE guild_id = ? AND peer = ? AND user_id = ? AND kid IS NOT ?")
            .bind(kid)
            .bind(self.guild)
            .bind(peer)
            .bind(user)
            .bind(kid)
            .execute(self.pool)
            .await?;
        Ok(())
    }

    /// Every grant from a partner.
    pub async fn peer_grants(&self, peer: &str) -> Result<Vec<Grant>> {
        Ok(sqlx::query_as(
            "SELECT user_id, role_key, kid FROM fed_grants WHERE guild_id = ? AND peer = ? ORDER BY user_id, role_key",
        )
        .bind(self.guild)
        .bind(peer)
        .fetch_all(self.pool)
        .await?)
    }

    /// Whether a partner other than `peer` also granted this role to this user.
    pub async fn granted_elsewhere(&self, peer: &str, user: &str, role: &str) -> Result<bool> {
        let n: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM fed_grants WHERE guild_id = ? AND peer != ? AND user_id = ? AND role_key = ?",
        )
        .bind(self.guild)
        .bind(peer)
        .bind(user)
        .bind(role)
        .fetch_one(self.pool)
        .await?;
        Ok(n > 0)
    }

    /// Users given a grant by a partner after `since`, and users granted at or before it.
    pub async fn grant_window(&self, peer: &str, since: i64) -> Result<(usize, usize)> {
        let (recent, before): (i64, i64) = sqlx::query_as(
            "SELECT COUNT(DISTINCT CASE WHEN granted_at > ? THEN user_id END),
                    COUNT(DISTINCT CASE WHEN granted_at <= ? THEN user_id END)
             FROM fed_grants WHERE guild_id = ? AND peer = ?",
        )
        .bind(since)
        .bind(since)
        .bind(self.guild)
        .bind(peer)
        .fetch_one(self.pool)
        .await?;
        Ok((recent as usize, before as usize))
    }

    pub async fn revoke(&self, peer: &str, user: &str, role: &str) -> Result<()> {
        sqlx::query("DELETE FROM fed_grants WHERE guild_id = ? AND peer = ? AND user_id = ? AND role_key = ?")
            .bind(self.guild)
            .bind(peer)
            .bind(user)
            .bind(role)
            .execute(self.pool)
            .await?;
        Ok(())
    }

    /// A partner's word took a role from a user at `now`, for the mass-revoke guard.
    pub async fn record_revoke(&self, peer: &str, user: &str, now: i64) -> Result<()> {
        sqlx::query("INSERT INTO fed_revokes (guild_id, peer, user_id, at) VALUES (?, ?, ?, ?)")
            .bind(self.guild)
            .bind(peer)
            .bind(user)
            .bind(now)
            .execute(self.pool)
            .await?;
        Ok(())
    }

    /// Users a partner's word took roles from after `since`.
    pub async fn revoke_window(&self, peer: &str, since: i64) -> Result<usize> {
        let n: i64 = sqlx::query_scalar(
            "SELECT COUNT(DISTINCT user_id) FROM fed_revokes WHERE guild_id = ? AND peer = ? AND at > ?",
        )
        .bind(self.guild)
        .bind(peer)
        .bind(since)
        .fetch_one(self.pool)
        .await?;
        Ok(n as usize)
    }

    /// Forget revocations made before `before`.
    pub async fn prune_revokes(&self, before: i64) -> Result<()> {
        sqlx::query("DELETE FROM fed_revokes WHERE guild_id = ? AND at < ?")
            .bind(self.guild)
            .bind(before)
            .execute(self.pool)
            .await?;
        Ok(())
    }

    /// Keep a member update to try again (replacing an older one for them).
    pub async fn retry_put(&self, peer: &str, user: &str, member: &str, kid: &str, tries: u32) -> Result<()> {
        sqlx::query(
            "INSERT INTO fed_retry (guild_id, peer, user_id, member, kid, tries) VALUES (?, ?, ?, ?, ?, ?)
             ON CONFLICT (guild_id, peer, user_id) DO UPDATE SET member = excluded.member, kid = excluded.kid,
             tries = excluded.tries",
        )
        .bind(self.guild)
        .bind(peer)
        .bind(user)
        .bind(member)
        .bind(kid)
        .bind(tries)
        .execute(self.pool)
        .await?;
        Ok(())
    }

    pub async fn retry_drop(&self, peer: &str, user: &str) -> Result<()> {
        sqlx::query("DELETE FROM fed_retry WHERE guild_id = ? AND peer = ? AND user_id = ?")
            .bind(self.guild)
            .bind(peer)
            .bind(user)
            .execute(self.pool)
            .await?;
        Ok(())
    }

    /// Member updates waiting to be tried again.
    pub async fn retries(&self) -> Result<Vec<Retry>> {
        Ok(sqlx::query_as(
            "SELECT peer, user_id, member, kid, tries FROM fed_retry WHERE guild_id = ? ORDER BY peer, user_id",
        )
        .bind(self.guild)
        .fetch_all(self.pool)
        .await?)
    }

    /// Partners we hold anything from or of: grants, a catalogue, a record,
    /// what they shared or what we shared with them.
    pub async fn peers_with_state(&self) -> Result<Vec<String>> {
        Ok(sqlx::query_scalar(
            "SELECT peer FROM fed_grants WHERE guild_id = ?1
             UNION SELECT peer FROM fed_catalog WHERE guild_id = ?1
             UNION SELECT peer FROM fed_peers WHERE guild_id = ?1
             UNION SELECT peer FROM fed_mirrors WHERE guild_id = ?1
             UNION SELECT peer FROM fed_share_out WHERE guild_id = ?1 ORDER BY peer",
        )
        .bind(self.guild)
        .fetch_all(self.pool)
        .await?)
    }

    pub async fn peer(&self, peer: &str) -> Result<Option<PeerRecord>> {
        let row: Option<(String, i64, i64)> =
            sqlx::query_as("SELECT kids, snapshot_seq, window_from FROM fed_peers WHERE guild_id = ? AND peer = ?")
                .bind(self.guild)
                .bind(peer)
                .fetch_optional(self.pool)
                .await?;
        Ok(row.map(|(kids, snapshot_seq, window_from)| PeerRecord {
            kids: kids.split(',').filter(|k| !k.is_empty()).map(str::to_owned).collect(),
            snapshot_seq: snapshot_seq as u64,
            window_from,
        }))
    }

    /// Remember the key ids a partner has now.
    pub async fn set_peer_kids(&self, peer: &str, kids: &[String]) -> Result<()> {
        sqlx::query(
            "INSERT INTO fed_peers (guild_id, peer, kids) VALUES (?, ?, ?)
             ON CONFLICT (guild_id, peer) DO UPDATE SET kids = excluded.kids",
        )
        .bind(self.guild)
        .bind(peer)
        .bind(kids.join(","))
        .execute(self.pool)
        .await?;
        Ok(())
    }

    /// Record `seq` as the newest full sync applied from a partner (once it's
    /// been applied, so one cut short is applied again); false if one as new
    /// or newer was applied.
    pub async fn claim_snapshot(&self, peer: &str, seq: u64) -> Result<bool> {
        let r = sqlx::query(
            "INSERT INTO fed_peers (guild_id, peer, snapshot_seq) VALUES (?, ?, ?)
             ON CONFLICT (guild_id, peer) DO UPDATE SET snapshot_seq = excluded.snapshot_seq
             WHERE excluded.snapshot_seq > fed_peers.snapshot_seq",
        )
        .bind(self.guild)
        .bind(peer)
        .bind(seq as i64)
        .execute(self.pool)
        .await?;
        Ok(r.rows_affected() == 1)
    }

    /// An admin allowed what was held: grants so far count as established.
    pub async fn reset_grant_window(&self, peer: &str, now: i64) -> Result<()> {
        sqlx::query(
            "INSERT INTO fed_peers (guild_id, peer, window_from) VALUES (?, ?, ?)
             ON CONFLICT (guild_id, peer) DO UPDATE SET window_from = excluded.window_from",
        )
        .bind(self.guild)
        .bind(peer)
        .bind(now)
        .execute(self.pool)
        .await?;
        Ok(())
    }

    /// Forget a partner's catalogue, seqs, record, retries and revocations
    /// (not its grants: those are taken back one by one).
    pub async fn forget_peer(&self, peer: &str) -> Result<()> {
        for table in ["fed_catalog", "fed_seen", "fed_peers", "fed_retry", "fed_revokes"] {
            // The table names are our own constants, not input.
            let sql = format!("DELETE FROM {table} WHERE guild_id = ? AND peer = ?");
            sqlx::query(sqlx::AssertSqlSafe(sql)).bind(self.guild).bind(peer).execute(self.pool).await?;
        }
        Ok(())
    }

    /// Keep an inbox message until it's handled; false if we already have it.
    pub async fn inbox_add(&self, msg: u64, content: &str, now: i64) -> Result<bool> {
        let r =
            sqlx::query("INSERT OR IGNORE INTO fed_inbox (guild_id, msg_id, content, received) VALUES (?, ?, ?, ?)")
                .bind(self.guild)
                .bind(msg as i64)
                .bind(content)
                .bind(now)
                .execute(self.pool)
                .await?;
        Ok(r.rows_affected() == 1)
    }

    pub async fn inbox_set(&self, msg: u64, state: InboxState) -> Result<()> {
        sqlx::query("UPDATE fed_inbox SET state = ? WHERE guild_id = ? AND msg_id = ?")
            .bind(state.as_str())
            .bind(self.guild)
            .bind(msg as i64)
            .execute(self.pool)
            .await?;
        Ok(())
    }

    /// Name the held batch messages are in, once an admin is asked about it.
    pub async fn inbox_set_batch(&self, msg: u64, batch: Option<&str>) -> Result<()> {
        sqlx::query("UPDATE fed_inbox SET batch = ? WHERE guild_id = ? AND msg_id = ?")
            .bind(batch)
            .bind(self.guild)
            .bind(msg as i64)
            .execute(self.pool)
            .await?;
        Ok(())
    }

    pub async fn inbox_state(&self, msg: u64) -> Result<Option<InboxState>> {
        let s: Option<String> = sqlx::query_scalar("SELECT state FROM fed_inbox WHERE guild_id = ? AND msg_id = ?")
            .bind(self.guild)
            .bind(msg as i64)
            .fetch_optional(self.pool)
            .await?;
        Ok(s.as_deref().map(InboxState::parse))
    }

    /// Count a failed try at handling a message; returns the tries so far.
    pub async fn inbox_failed(&self, msg: u64) -> Result<u32> {
        let n: Option<i64> = sqlx::query_scalar(
            "UPDATE fed_inbox SET tries = tries + 1, state = 'new' WHERE guild_id = ? AND msg_id = ? RETURNING tries",
        )
        .bind(self.guild)
        .bind(msg as i64)
        .fetch_optional(self.pool)
        .await?;
        Ok(n.unwrap_or_default() as u32)
    }

    /// Messages not yet done, oldest first; with `new_only`, just those not
    /// handled yet or that failed.
    pub async fn inbox_pending(&self, new_only: bool) -> Result<Vec<InboxMessage>> {
        let rows: Vec<(i64, String, String, i64, Option<String>)> = sqlx::query_as(
            "SELECT msg_id, content, state, tries, batch FROM fed_inbox
             WHERE guild_id = ? AND state != 'done' AND (? = 0 OR state = 'new') ORDER BY msg_id",
        )
        .bind(self.guild)
        .bind(new_only)
        .fetch_all(self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(msg, content, state, tries, batch)| InboxMessage {
                msg: msg as u64,
                content,
                state: InboxState::parse(&state),
                tries: tries as u32,
                batch,
            })
            .collect())
    }

    /// The newest inbox message we have, to read on from after a restart.
    pub async fn inbox_last(&self) -> Result<Option<u64>> {
        let n: Option<i64> = sqlx::query_scalar("SELECT MAX(msg_id) FROM fed_inbox WHERE guild_id = ?")
            .bind(self.guild)
            .fetch_one(self.pool)
            .await?;
        Ok(n.map(|n| n as u64))
    }

    /// Forget inbox messages received before `before` (too old to accept
    /// anyway), except those held for an admin to decide on.
    pub async fn inbox_prune(&self, before: i64) -> Result<()> {
        sqlx::query("DELETE FROM fed_inbox WHERE guild_id = ? AND received < ? AND state != 'held'")
            .bind(self.guild)
            .bind(before)
            .execute(self.pool)
            .await?;
        Ok(())
    }

    /// Share an item with a partner (again, if it was): sent by the share
    /// loop. `author`: who shared it, the only member (besides admins) whose
    /// changes to it follow.
    pub async fn share_out(
        &self,
        peer: &str,
        what: &str,
        origin: &str,
        body: &str,
        author: Option<&str>,
        now: i64,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO fed_share_out (guild_id, peer, what, origin, body, changed, author, shared_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT (guild_id, peer, what, origin) DO UPDATE SET body = excluded.body, removed = 0,
             pending = 1, tries = 0, changed = excluded.changed, author = excluded.author",
        )
        .bind(self.guild)
        .bind(peer)
        .bind(what)
        .bind(origin)
        .bind(body)
        .bind(now)
        .bind(author)
        .bind(now)
        .execute(self.pool)
        .await?;
        Ok(())
    }

    /// Who shared an item (see [`Fed::share_out`]), if anyone is recorded.
    pub async fn out_author(&self, what: &str, origin: &str) -> Result<Option<String>> {
        let rows: Vec<Option<String>> =
            sqlx::query_scalar("SELECT author FROM fed_share_out WHERE guild_id = ? AND what = ? AND origin = ?")
                .bind(self.guild)
                .bind(what)
                .bind(origin)
                .fetch_all(self.pool)
                .await?;
        Ok(rows.into_iter().flatten().next())
    }

    /// Items of a kind first shared since `since` (by `author`, if given).
    pub async fn shared_since(&self, what: &str, author: Option<&str>, since: i64) -> Result<i64> {
        Ok(sqlx::query_scalar(
            "SELECT COUNT(DISTINCT origin) FROM fed_share_out
             WHERE guild_id = ? AND what = ? AND shared_at >= ? AND (?4 IS NULL OR author = ?4)",
        )
        .bind(self.guild)
        .bind(what)
        .bind(since)
        .bind(author)
        .fetch_one(self.pool)
        .await?)
    }

    /// Forget everything shared with or by a partner: what we shared with
    /// them, and what they shared with us (returned, to archive).
    pub async fn forget_shares(&self, peer: &str) -> Result<Vec<Mirror>> {
        let sql = format!("SELECT {MIRROR_COLUMNS} FROM fed_mirrors WHERE guild_id = ? AND peer = ?");
        let rows: Vec<MirrorRow> =
            sqlx::query_as(sqlx::AssertSqlSafe(sql)).bind(self.guild).bind(peer).fetch_all(self.pool).await?;
        for table in ["fed_mirrors", "fed_share_out"] {
            // The table names are our own constants, not input.
            let sql = format!("DELETE FROM {table} WHERE guild_id = ? AND peer = ?");
            sqlx::query(sqlx::AssertSqlSafe(sql)).bind(self.guild).bind(peer).execute(self.pool).await?;
        }
        Ok(rows.into_iter().map(mirror).collect())
    }

    /// The partners an item is shared with (and not taken down).
    pub async fn shared_with(&self, what: &str, origin: &str) -> Result<Vec<String>> {
        Ok(sqlx::query_scalar(
            "SELECT peer FROM fed_share_out WHERE guild_id = ? AND what = ? AND origin = ? AND removed = 0 ORDER BY peer",
        )
        .bind(self.guild)
        .bind(what)
        .bind(origin)
        .fetch_all(self.pool)
        .await?)
    }

    /// What we sent of an item, to each partner that has it.
    pub async fn out_bodies(&self, what: &str, origin: &str) -> Result<Vec<String>> {
        Ok(sqlx::query_scalar(
            "SELECT body FROM fed_share_out WHERE guild_id = ? AND what = ? AND origin = ? AND removed = 0",
        )
        .bind(self.guild)
        .bind(what)
        .bind(origin)
        .fetch_all(self.pool)
        .await?)
    }

    /// An item shared already changed: send it again to whoever has it (not
    /// if it's what was sent already). How many that is.
    pub async fn update_out(&self, what: &str, origin: &str, body: &str, now: i64) -> Result<u64> {
        let r = sqlx::query(
            "UPDATE fed_share_out SET body = ?, pending = 1, tries = 0, changed = ?
             WHERE guild_id = ? AND what = ? AND origin = ? AND removed = 0 AND body != ?",
        )
        .bind(body)
        .bind(now)
        .bind(self.guild)
        .bind(what)
        .bind(origin)
        .bind(body)
        .execute(self.pool)
        .await?;
        Ok(r.rows_affected())
    }

    /// An item shared was taken down: tell whoever has it. How many that is.
    pub async fn remove_out(&self, what: &str, origin: &str, now: i64) -> Result<u64> {
        let r = sqlx::query(
            "UPDATE fed_share_out SET removed = 1, pending = 1, tries = 0, changed = ?
             WHERE guild_id = ? AND what = ? AND origin = ? AND removed = 0",
        )
        .bind(now)
        .bind(self.guild)
        .bind(what)
        .bind(origin)
        .execute(self.pool)
        .await?;
        Ok(r.rows_affected())
    }

    /// What's waiting to be sent: take-downs, announcements and events
    /// first, then oldest change first.
    pub async fn out_pending(&self) -> Result<Vec<OutShare>> {
        Ok(sqlx::query_as(
            "SELECT peer, what, origin, body, removed, pending, tries, changed FROM fed_share_out
             WHERE guild_id = ? AND pending = 1
             ORDER BY removed DESC, what IN ('announcement', 'event') DESC, changed, peer",
        )
        .bind(self.guild)
        .fetch_all(self.pool)
        .await?)
    }

    /// It went (or is given up on): done, unless it changed again since `changed`.
    pub async fn out_done(&self, s: &OutShare) -> Result<()> {
        sqlx::query(
            "UPDATE fed_share_out SET pending = 0 WHERE guild_id = ? AND peer = ? AND what = ? AND origin = ? AND changed = ?",
        )
        .bind(self.guild)
        .bind(&s.peer)
        .bind(&s.what)
        .bind(&s.origin)
        .bind(s.changed)
        .execute(self.pool)
        .await?;
        Ok(())
    }

    /// Count a failed send.
    pub async fn out_failed(&self, s: &OutShare) -> Result<()> {
        sqlx::query(
            "UPDATE fed_share_out SET tries = tries + 1 WHERE guild_id = ? AND peer = ? AND what = ? AND origin = ?",
        )
        .bind(self.guild)
        .bind(&s.peer)
        .bind(&s.what)
        .bind(&s.origin)
        .execute(self.pool)
        .await?;
        Ok(())
    }

    /// Something a partner shared, if we've had it.
    pub async fn mirror(&self, peer: &str, what: &str, origin: &str) -> Result<Option<Mirror>> {
        let sql = format!(
            "SELECT {MIRROR_COLUMNS} FROM fed_mirrors WHERE guild_id = ? AND peer = ? AND what = ? AND origin = ?"
        );
        let row: Option<MirrorRow> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
            .bind(self.guild)
            .bind(peer)
            .bind(what)
            .bind(origin)
            .fetch_optional(self.pool)
            .await?;
        Ok(row.map(mirror))
    }

    /// What a review-queue entry holds of a partner's: the item, or an edit of it.
    pub async fn mirror_of_app(&self, app: i64) -> Result<Option<Mirror>> {
        let sql =
            format!("SELECT {MIRROR_COLUMNS} FROM fed_mirrors WHERE guild_id = ?1 AND (app_id = ?2 OR edit_app = ?2)");
        let row: Option<MirrorRow> =
            sqlx::query_as(sqlx::AssertSqlSafe(sql)).bind(self.guild).bind(app).fetch_optional(self.pool).await?;
        Ok(row.map(mirror))
    }

    /// How many of a partner's items of one kind (or their edits) wait in the review queue.
    pub async fn mirrors_in_review(&self, peer: &str, what: &str) -> Result<i64> {
        Ok(sqlx::query_scalar(
            "SELECT COUNT(*) FROM fed_mirrors WHERE guild_id = ? AND peer = ? AND what = ?
             AND (state = 'review' OR edit_app IS NOT NULL)",
        )
        .bind(self.guild)
        .bind(peer)
        .bind(what)
        .fetch_one(self.pool)
        .await?)
    }

    pub async fn set_mirror(&self, m: &Mirror, now: i64) -> Result<()> {
        sqlx::query(
            "INSERT INTO fed_mirrors (guild_id, peer, what, origin, seq, state, body, app_id, channel_id, message_id,
             at, edit_body, edit_app)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT (guild_id, peer, what, origin) DO UPDATE SET seq = excluded.seq, state = excluded.state,
             body = excluded.body, app_id = excluded.app_id, channel_id = excluded.channel_id,
             message_id = excluded.message_id, at = excluded.at, edit_body = excluded.edit_body,
             edit_app = excluded.edit_app",
        )
        .bind(self.guild)
        .bind(&m.peer)
        .bind(&m.what)
        .bind(&m.origin)
        .bind(m.seq as i64)
        .bind(m.state.as_str())
        .bind(&m.body)
        .bind(m.app_id)
        .bind(&m.channel_id)
        .bind(&m.message_id)
        .bind(now)
        .bind(&m.edit_body)
        .bind(m.edit_app)
        .execute(self.pool)
        .await?;
        Ok(())
    }

    pub async fn sent_hash(&self, peer: &str, kind: &str) -> Result<Option<String>> {
        Ok(sqlx::query_scalar("SELECT hash FROM fed_sent WHERE guild_id = ? AND peer = ? AND kind = ?")
            .bind(self.guild)
            .bind(peer)
            .bind(kind)
            .fetch_optional(self.pool)
            .await?)
    }

    pub async fn set_sent_hash(&self, peer: &str, kind: &str, hash: &str) -> Result<()> {
        sqlx::query(
            "INSERT INTO fed_sent (guild_id, peer, kind, hash) VALUES (?, ?, ?, ?)
             ON CONFLICT (guild_id, peer, kind) DO UPDATE SET hash = excluded.hash",
        )
        .bind(self.guild)
        .bind(peer)
        .bind(kind)
        .bind(hash)
        .execute(self.pool)
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Store;

    #[tokio::test]
    async fn shares_out_and_mirrors() {
        let s = Store::open("sqlite::memory:").await.unwrap();
        let g = s.guild("g");
        let f = g.fed();
        f.share_out("p", "resource", "1", "{a}", Some("7"), 10).await.unwrap();
        f.share_out("q", "resource", "1", "{a}", Some("7"), 10).await.unwrap();
        assert_eq!(f.out_author("resource", "1").await.unwrap().as_deref(), Some("7"));
        assert_eq!(f.shared_since("resource", Some("7"), 10).await.unwrap(), 1, "one item, to two partners");
        assert_eq!(f.shared_since("resource", Some("8"), 0).await.unwrap(), 0);
        assert_eq!(f.shared_since("resource", None, 11).await.unwrap(), 0);
        let out = f.out_pending().await.unwrap();
        assert_eq!(out.len(), 2);
        f.out_done(&out[0]).await.unwrap();
        assert_eq!(f.out_pending().await.unwrap().len(), 1);
        assert_eq!(f.update_out("resource", "1", "{b}", 11).await.unwrap(), 2);
        let out = f.out_pending().await.unwrap();
        assert!(out.iter().all(|o| o.body == "{b}" && !o.removed));
        // Changed again while it was being sent: still pending.
        f.update_out("resource", "1", "{c}", 12).await.unwrap();
        f.out_done(&out[0]).await.unwrap();
        assert_eq!(f.out_pending().await.unwrap().len(), 2);
        assert_eq!(f.remove_out("resource", "1", 13).await.unwrap(), 2);
        assert!(f.shared_with("resource", "1").await.unwrap().is_empty());
        assert_eq!(f.remove_out("resource", "1", 14).await.unwrap(), 0, "once");
        assert_eq!(f.update_out("resource", "2", "{}", 14).await.unwrap(), 0, "never shared");

        let mut m = Mirror {
            peer: "p".into(),
            what: "resource".into(),
            origin: "1".into(),
            seq: 5,
            state: MirrorState::Review,
            body: "{}".into(),
            app_id: Some(3),
            channel_id: None,
            message_id: None,
            edit_body: None,
            edit_app: None,
        };
        f.set_mirror(&m, 1).await.unwrap();
        assert_eq!(f.mirrors_in_review("p", "resource").await.unwrap(), 1);
        assert_eq!(f.mirrors_in_review("p", "question").await.unwrap(), 0, "counted per kind");
        m.state = MirrorState::Posted;
        m.channel_id = Some("9".into());
        f.set_mirror(&m, 2).await.unwrap();
        assert_eq!(f.mirror("p", "resource", "1").await.unwrap(), Some(m.clone()));
        assert_eq!(f.mirror_of_app(3).await.unwrap(), Some(m.clone()));
        assert_eq!(f.mirrors_in_review("p", "resource").await.unwrap(), 0);
        // An edit waiting for review: found by its entry, and counted.
        (m.edit_body, m.edit_app) = (Some("{e}".into()), Some(4));
        f.set_mirror(&m, 3).await.unwrap();
        assert_eq!(f.mirror_of_app(4).await.unwrap(), Some(m.clone()));
        assert_eq!(f.mirrors_in_review("p", "resource").await.unwrap(), 1);
        assert_eq!(f.peers_with_state().await.unwrap(), ["p", "q"]);
        assert_eq!(f.forget_shares("p").await.unwrap(), [m]);
        assert_eq!(f.mirror("p", "resource", "1").await.unwrap(), None);
        assert_eq!(f.peers_with_state().await.unwrap(), ["q"]);
    }

    #[tokio::test]
    async fn nonces_seq_catalog_grants() {
        let s = Store::open("sqlite::memory:").await.unwrap();
        let f = s.fed("g");
        assert!(f.fresh_nonce("p", "n1", 100, 0, 1).await.unwrap());
        assert!(!f.fresh_nonce("p", "n1", 100, 0, 2).await.unwrap(), "replay rejected");
        assert!(f.fresh_nonce("p", "n1", 100, 0, 1).await.unwrap(), "the same message, handled again");
        assert!(f.fresh_nonce("p", "n2", 300, 200, 3).await.unwrap(), "old nonces pruned");
        assert!(f.fresh_nonce("p", "n1", 300, 200, 4).await.unwrap(), "n1 expired so accepted again");

        assert!(f.claim_seq("p", "u", 5).await.unwrap());
        assert!(!f.claim_seq("p", "u", 4).await.unwrap(), "older state ignored");
        assert!(f.claim_seq("p", "u", 5).await.unwrap(), "the same state again");
        assert!(f.claim_seq("p", "u", 6).await.unwrap());

        f.set_catalog("p", "new", 1, 10).await.unwrap();
        f.set_catalog("p", "old", 1, 9).await.unwrap();
        assert_eq!(f.catalogs().await.unwrap()[0].body, "new");
        assert!(s.fed("other").catalogs().await.unwrap().is_empty(), "scoped by guild");

        f.grant("p", "u", "partner", "k1", 50).await.unwrap();
        assert_eq!(f.grants("p", "u").await.unwrap(), ["partner"]);
        assert_eq!(f.granted_users("p").await.unwrap(), ["u"]);
        f.set_grant_kid("p", "u", "k2").await.unwrap();
        let g = f.peer_grants("p").await.unwrap();
        assert_eq!(g[0].kid.as_deref(), Some("k2"), "vouched again with the new key");
        assert!(!f.granted_elsewhere("p", "u", "partner").await.unwrap());
        f.grant("q", "u", "partner", "kq", 60).await.unwrap();
        assert!(f.granted_elsewhere("p", "u", "partner").await.unwrap());
        f.revoke("p", "u", "partner").await.unwrap();
        assert!(f.grants("p", "u").await.unwrap().is_empty());

        assert_eq!(f.peers_with_state().await.unwrap(), ["p", "q"]);
        f.forget_peer("p").await.unwrap();
        assert!(f.catalogs().await.unwrap().is_empty(), "catalogue wiped");
        assert!(f.claim_seq("p", "u", 1).await.unwrap(), "seqs forgotten");

        assert_eq!(f.sent_hash("p", "catalog").await.unwrap(), None);
        f.set_sent_hash("p", "catalog", "h").await.unwrap();
        assert_eq!(f.sent_hash("p", "catalog").await.unwrap().as_deref(), Some("h"));
    }

    #[tokio::test]
    async fn grant_window_and_snapshots() {
        let s = Store::open("sqlite::memory:").await.unwrap();
        let f = s.fed("g");
        for (u, at) in [("a", 10), ("b", 10), ("c", 100), ("c", 100), ("d", 200)] {
            f.grant("p", u, &format!("r{at}"), "k", at).await.unwrap();
        }
        assert_eq!(f.grant_window("p", 50).await.unwrap(), (2, 2));
        assert_eq!(f.grant_window("p", 200).await.unwrap(), (0, 4));

        assert_eq!(f.peer("p").await.unwrap(), None);
        assert!(f.claim_snapshot("p", 10).await.unwrap());
        assert!(!f.claim_snapshot("p", 9).await.unwrap(), "an older full sync");
        assert!(!f.claim_snapshot("p", 10).await.unwrap());
        f.set_peer_kids("p", &["k1".into(), "k2".into()]).await.unwrap();
        f.reset_grant_window("p", 77).await.unwrap();
        let rec = f.peer("p").await.unwrap().unwrap();
        assert_eq!(rec, PeerRecord { kids: vec!["k1".into(), "k2".into()], snapshot_seq: 10, window_from: 77 });
    }

    #[tokio::test]
    async fn inbox_keeps_messages_until_done() {
        let s = Store::open("sqlite::memory:").await.unwrap();
        let f = s.fed("g");
        assert_eq!(f.inbox_last().await.unwrap(), None);
        assert!(f.inbox_add(5, "five", 100).await.unwrap());
        assert!(!f.inbox_add(5, "five", 100).await.unwrap(), "already kept");
        assert!(f.inbox_add(3, "three", 100).await.unwrap());
        assert!(f.inbox_add(9, "nine", 500).await.unwrap());
        assert_eq!(f.inbox_last().await.unwrap(), Some(9));
        let ids = |v: Vec<InboxMessage>| v.into_iter().map(|m| m.msg).collect::<Vec<_>>();
        assert_eq!(ids(f.inbox_pending(false).await.unwrap()), [3, 5, 9], "oldest first");

        f.inbox_set(3, InboxState::Done).await.unwrap();
        f.inbox_set(9, InboxState::Held).await.unwrap();
        assert_eq!(f.inbox_failed(5).await.unwrap(), 1);
        assert_eq!(f.inbox_failed(5).await.unwrap(), 2);
        assert_eq!(ids(f.inbox_pending(true).await.unwrap()), [5], "only failed ones to retry");
        let all = f.inbox_pending(false).await.unwrap();
        assert_eq!((all[1].state, all[1].tries), (InboxState::Held, 0));

        f.inbox_prune(200).await.unwrap();
        assert_eq!(ids(f.inbox_pending(false).await.unwrap()), [9]);
        assert!(s.fed("other").inbox_pending(false).await.unwrap().is_empty(), "scoped by guild");
    }

    #[tokio::test]
    async fn held_messages_outlive_the_prune() {
        // Red-team PoC: pruning used to delete held rows, losing what an admin hadn't decided on.
        let s = Store::open("sqlite::memory:").await.unwrap();
        let f = s.fed("g");
        f.inbox_add(1, "held", 100).await.unwrap();
        f.inbox_add(2, "waiting", 100).await.unwrap();
        f.inbox_set(1, InboxState::Held).await.unwrap();
        f.inbox_set(2, InboxState::Waiting).await.unwrap();
        f.inbox_set_batch(1, Some("b1")).await.unwrap();
        f.inbox_prune(1000).await.unwrap();
        let left = f.inbox_pending(false).await.unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!((left[0].msg, left[0].state, left[0].batch.as_deref()), (1, InboxState::Held, Some("b1")));
        assert_eq!(f.inbox_state(1).await.unwrap(), Some(InboxState::Held));
        assert_eq!(f.inbox_state(2).await.unwrap(), None);

        f.inbox_add(3, "unhandled", 2000).await.unwrap();
        f.inbox_add(4, "parts", 2000).await.unwrap();
        f.inbox_set(4, InboxState::Waiting).await.unwrap();
        let new: Vec<u64> = f.inbox_pending(true).await.unwrap().into_iter().map(|m| m.msg).collect();
        assert_eq!(new, [3], "waiting parts aren't handled again until a restart");
    }

    #[tokio::test]
    async fn seqs_from_the_future_can_be_cleared() {
        // Red-team PoC: a stolen key sent seq = i64::MAX and froze the member for good.
        let s = Store::open("sqlite::memory:").await.unwrap();
        let f = s.fed("g");
        assert!(f.claim_seq("p", "v", i64::MAX as u64).await.unwrap());
        assert!(f.claim_snapshot("p", i64::MAX as u64).await.unwrap());
        assert!(!f.claim_seq("p", "v", 1_000).await.unwrap(), "frozen");
        f.prune_seqs(0, 10_000).await.unwrap();
        assert!(f.claim_seq("p", "v", 1_000).await.unwrap(), "honest updates apply again");
        assert_eq!(f.peer("p").await.unwrap().unwrap().snapshot_seq, 0);
        assert!(f.claim_seq("p", "w", 5).await.unwrap());
        f.prune_seqs(100, 10_000).await.unwrap();
        assert!(f.claim_seq("p", "w", 4).await.unwrap(), "old rows forgotten");

        f.claim_snapshot("p", 50).await.unwrap();
        f.forget_seqs("p").await.unwrap();
        assert_eq!(f.peer("p").await.unwrap().unwrap().snapshot_seq, 0);
        assert!(f.claim_seq("p", "v", 1).await.unwrap(), "a removed key's seqs are forgotten");
    }

    #[tokio::test]
    async fn pending_grants_revokes_and_retries() {
        let s = Store::open("sqlite::memory:").await.unwrap();
        let f = s.fed("g");
        f.grant_pending("p", "u", "r", "k", 10).await.unwrap();
        assert_eq!(f.grants("p", "u").await.unwrap(), ["r"], "a pending grant is still ours to take back");
        let pending = f.pending_grants().await.unwrap();
        assert_eq!(pending, [PendingGrant { peer: "p".into(), user_id: "u".into(), role_key: "r".into() }]);
        f.confirm_grant("p", "u", "r").await.unwrap();
        assert!(f.pending_grants().await.unwrap().is_empty());

        for (u, at) in [("a", 10), ("a", 20), ("b", 30), ("c", 5)] {
            f.record_revoke("p", u, at).await.unwrap();
        }
        assert_eq!(f.revoke_window("p", 6).await.unwrap(), 2);
        f.prune_revokes(25).await.unwrap();
        assert_eq!(f.revoke_window("p", 0).await.unwrap(), 1);

        f.retry_put("p", "u", "{}", "k", 1).await.unwrap();
        f.retry_put("p", "u", "{\"id\":\"u\"}", "k2", 2).await.unwrap();
        let r = f.retries().await.unwrap();
        assert_eq!((r.len(), r[0].tries, r[0].kid.as_str()), (1, 2, "k2"), "a newer update replaces the older");
        f.retry_drop("p", "u").await.unwrap();
        assert!(f.retries().await.unwrap().is_empty());
        f.retry_put("p", "u", "{}", "k", 1).await.unwrap();
        f.forget_peer("p").await.unwrap();
        assert!(f.retries().await.unwrap().is_empty(), "a removed partner's retries go");
        assert_eq!(f.revoke_window("p", 0).await.unwrap(), 0);
    }
}

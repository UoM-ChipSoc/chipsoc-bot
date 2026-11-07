//! The bot's state in SQLite: applications, the audit and message logs,
//! feeds, reminders, member profiles and federation. Migrations run on open.

use std::future::Future;
use std::path::Path;
use std::str::FromStr;
use std::time::Duration;

use anyhow::{Context, Result};
use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};

mod fed;
mod guild;
mod signins;
mod trusted;
pub use fed::{
    Catalog, Fed, Grant, InboxMessage, InboxState, Mirror, MirrorState, OutShare, PeerRecord, PendingGrant, Retry,
};
pub use guild::*;
pub use signins::SignIn;

#[derive(Clone)]
pub struct Store {
    pool: SqlitePool,
}

impl Store {
    /// Open (or create) the store, e.g. `sqlite:bot.db?mode=rwc` or `sqlite::memory:`.
    ///
    /// The bot and the web UI share the file, so it's opened for two
    /// processes: write-ahead logging (readers don't wait for a writer), and
    /// a writer waits up to `BUSY` (10 s) for another rather than failing.
    pub async fn open(url: &str) -> Result<Self> {
        // An in-memory database exists per connection, so keep just one.
        let memory = url.contains(":memory:");
        let mut opts = SqliteConnectOptions::from_str(url)?.busy_timeout(BUSY);
        if !memory {
            opts = opts.journal_mode(SqliteJournalMode::Wal).synchronous(SqliteSynchronous::Normal);
        }
        let mut pools = SqlitePoolOptions::new().max_connections(if memory { 1 } else { 4 });
        if memory {
            // Its one connection is the database: never let it go idle or old.
            pools = pools.min_connections(1).idle_timeout(None).max_lifetime(None);
        }
        // Turning on write-ahead logging needs the file to itself, and SQLite
        // fails at once (not after the busy timeout) when two connections
        // ask together: wait for the other instead.
        let pool = when_free(|| pools.clone().connect_with(opts.clone())).await?;
        migrate(&pool).await?;
        Ok(Self { pool })
    }

    /// The bot's database: `DATABASE_URL`, default `sqlite:data/bot.db?mode=rwc`
    /// (its directory made if needed). The bot and the web UI share it.
    pub async fn from_env() -> Result<Self> {
        let url = Self::env_url()?;
        Self::open(&url).await.with_context(|| format!("opening {url}"))
    }

    /// Where [`Store::from_env`] opens, its directory made.
    pub fn env_url() -> Result<String> {
        let url = std::env::var("DATABASE_URL").unwrap_or_else(|_| "sqlite:data/bot.db?mode=rwc".into());
        if let Some(path) = url.strip_prefix("sqlite:").and_then(|p| p.split('?').next())
            && let Some(dir) = Path::new(path).parent()
            && !dir.as_os_str().is_empty()
        {
            std::fs::create_dir_all(dir)?;
        }
        Ok(url)
    }

    /// State for one guild.
    pub fn guild<'a>(&'a self, guild: &'a str) -> Guild<'a> {
        Guild { pool: &self.pool, guild }
    }

    /// Federation state for one guild.
    pub fn fed<'a>(&'a self, guild: &'a str) -> Fed<'a> {
        Fed { pool: &self.pool, guild }
    }
}

/// How long a write waits for the other process's to finish.
const BUSY: Duration = Duration::from_secs(10);

/// Run `f` again while the database is busy (SQLite's `SQLITE_BUSY`, which
/// some steps return without waiting the busy timeout), for up to [`BUSY`].
async fn when_free<T, F: Future<Output = sqlx::Result<T>>>(mut f: impl FnMut() -> F) -> sqlx::Result<T> {
    let deadline = tokio::time::Instant::now() + BUSY;
    loop {
        match f().await {
            Err(e) if is_busy(&e) && tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            r => return r,
        }
    }
}

/// `SQLITE_BUSY`, or one of its extended codes (busy recovering, busy snapshot…).
fn is_busy(e: &sqlx::Error) -> bool {
    let code = e.as_database_error().and_then(sqlx::error::DatabaseError::code).and_then(|c| c.parse::<i32>().ok());
    code.is_some_and(|c| c & 0xff == 5)
}

/// Applies migrations newer than the database's `user_version`, in order.
/// Each runs in a write transaction taken up front that reads the version
/// again inside it, so two processes starting on an old database (after a
/// deploy) never both run one: the second waits, then finds it done.
async fn migrate(pool: &SqlitePool) -> Result<()> {
    loop {
        let mut tx = when_free(|| pool.begin_with("BEGIN IMMEDIATE")).await?;
        let version: i64 = sqlx::query_scalar("PRAGMA user_version").fetch_one(&mut *tx).await?;
        let Some(sql) = MIGRATIONS.get(version as usize) else {
            tx.rollback().await?;
            return Ok(());
        };
        sqlx::raw_sql(*sql).execute(&mut *tx).await.with_context(|| format!("migrating to version {}", version + 1))?;
        // The version is our own integer, not input.
        let bump = format!("PRAGMA user_version = {}", version + 1);
        sqlx::raw_sql(sqlx::AssertSqlSafe(bump)).execute(&mut *tx).await?;
        tx.commit().await?;
    }
}

const MIGRATIONS: &[&str] = &[
    SCHEMA_V1,
    fed::SCHEMA_V2,
    guild::SCHEMA_V3,
    guild::SCHEMA_V4,
    guild::SCHEMA_V5,
    guild::SCHEMA_V6,
    fed::SCHEMA_V7,
    guild::SCHEMA_V8,
    guild::SCHEMA_V9,
    guild::SCHEMA_V10,
    fed::SCHEMA_V11,
    trusted::SCHEMA_V12,
    guild::SCHEMA_V13,
    guild::SCHEMA_V14,
    guild::SCHEMA_V15,
    signins::SCHEMA_V16,
    fed::SCHEMA_V17,
    fed::SCHEMA_V18,
    guild::SCHEMA_V19,
    guild::SCHEMA_V20,
];

const SCHEMA_V1: &str = r#"
CREATE TABLE IF NOT EXISTS applications (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    guild_id    TEXT NOT NULL,
    kind        TEXT NOT NULL,           -- 'project' | 'position'
    target      TEXT NOT NULL,           -- project slug or role key
    user_id     TEXT NOT NULL,
    answers     TEXT NOT NULL,           -- json
    as_lead     INTEGER NOT NULL DEFAULT 0,
    status      TEXT NOT NULL DEFAULT 'pending',
    reviewed_by TEXT,
    created_at  INTEGER NOT NULL,
    reviewed_at INTEGER
);
CREATE TABLE IF NOT EXISTS audit_log (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    guild_id TEXT NOT NULL,
    ts INTEGER NOT NULL,
    actor TEXT,
    action TEXT NOT NULL,
    target TEXT,
    detail TEXT
);
CREATE TABLE IF NOT EXISTS message_archive (
    id TEXT PRIMARY KEY,
    guild_id TEXT NOT NULL,
    channel_id TEXT NOT NULL,
    author_id TEXT NOT NULL,
    author_name TEXT NOT NULL,
    content TEXT NOT NULL,
    ts INTEGER NOT NULL,
    deleted INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS feeds (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    guild_id TEXT NOT NULL,
    url TEXT NOT NULL UNIQUE,
    channel_key TEXT NOT NULL,
    added_by TEXT,
    ts INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS feed_items (
    feed_id INTEGER NOT NULL,
    item_id TEXT NOT NULL,
    PRIMARY KEY (feed_id, item_id)
);
CREATE TABLE IF NOT EXISTS reminders (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    guild_id TEXT NOT NULL,
    channel_id TEXT NOT NULL,
    due INTEGER NOT NULL,
    text TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS fed_nonces (
    peer TEXT NOT NULL,
    nonce TEXT NOT NULL,
    ts INTEGER NOT NULL,
    PRIMARY KEY (peer, nonce)
);
CREATE TABLE IF NOT EXISTS fed_catalog (
    peer TEXT PRIMARY KEY,
    body TEXT NOT NULL,
    ts INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS fed_grants (
    peer TEXT NOT NULL,
    user_id TEXT NOT NULL,
    role_key TEXT NOT NULL,
    PRIMARY KEY (peer, user_id, role_key)
);
CREATE TABLE IF NOT EXISTS role_assignments (
    guild_id TEXT NOT NULL,
    user_id  TEXT NOT NULL,
    role_key TEXT NOT NULL,
    source   TEXT NOT NULL,              -- 'self' | 'admin' | 'sync' | 'application'
    ts INTEGER NOT NULL,
    PRIMARY KEY (guild_id, user_id, role_key)
);
"#;

#[cfg(test)]
mod tests {
    use super::*;

    /// A database from the first release, with an application and an audit
    /// entry, upgrades to the latest version keeping both, even with two
    /// processes (here: two pools) opening it at once.
    #[tokio::test]
    async fn an_old_database_upgrades_keeping_its_data() {
        let d = tempfile::tempdir().unwrap();
        let url = format!("sqlite:{}?mode=rwc", d.path().join("bot.db").display());
        {
            let pool = SqlitePoolOptions::new().max_connections(1).connect(&url).await.unwrap();
            sqlx::raw_sql(SCHEMA_V1).execute(&pool).await.unwrap();
            sqlx::raw_sql("PRAGMA user_version = 1").execute(&pool).await.unwrap();
            sqlx::raw_sql(
                "INSERT INTO applications (guild_id, kind, target, user_id, answers, created_at) \
                 VALUES ('1', 'position', 'chair', '7', '{}', 5); \
                 INSERT INTO audit_log (guild_id, ts, actor, action) VALUES ('1', 3, 'ada', 'appoint');",
            )
            .execute(&pool)
            .await
            .unwrap();
            pool.close().await;
        }
        let (a, b) = tokio::join!(Store::open(&url), Store::open(&url));
        let (a, _b) = (a.unwrap(), b.unwrap());
        let version: i64 = sqlx::query_scalar("PRAGMA user_version").fetch_one(&a.pool).await.unwrap();
        assert_eq!(version as usize, MIGRATIONS.len());
        let mode: String = sqlx::query_scalar("PRAGMA journal_mode").fetch_one(&a.pool).await.unwrap();
        assert_eq!(mode, "wal");
        let g = a.guild("1");
        let apps = g.pending_applications().await.unwrap();
        assert_eq!((apps.len(), apps[0].target.as_str(), apps[0].user.as_str()), (1, "chair", "7"));
        assert_eq!(g.recent_audit(5).await.unwrap()[0].action, "appoint");
    }

    /// Shared resources' tags, kept as "a, b" before version 19, are a list
    /// after it; nothing else changes.
    #[tokio::test]
    async fn resource_tags_become_a_list() {
        let d = tempfile::tempdir().unwrap();
        let url = format!("sqlite:{}?mode=rwc", d.path().join("bot.db").display());
        let tags = |answer: &str| serde_json::json!([{ "question": "Title", "answer": "A" }, { "question": "Tags", "answer": answer }]);
        let rows = [
            ("resource", tags("cpu, rust ,")),
            ("resource", tags(r#"say "hi", back\slash"#)),
            ("resource", tags("")),
            ("resource", tags("bad\u{1}")),
            ("position", tags("a, b")),
        ];
        {
            let pool = SqlitePoolOptions::new().max_connections(1).connect(&url).await.unwrap();
            for sql in &MIGRATIONS[..18] {
                sqlx::raw_sql(*sql).execute(&pool).await.unwrap();
            }
            sqlx::raw_sql("PRAGMA user_version = 18").execute(&pool).await.unwrap();
            for (kind, answers) in &rows {
                sqlx::query(
                    "INSERT INTO applications (guild_id, kind, target, user_id, answers, created_at) \
                     VALUES ('1', ?, 'x', '7', ?, 5)",
                )
                .bind(kind)
                .bind(answers.to_string())
                .execute(&pool)
                .await
                .unwrap();
            }
            pool.close().await;
        }
        let s = Store::open(&url).await.unwrap();
        let after: Vec<String> =
            sqlx::query_scalar("SELECT answers FROM applications ORDER BY id").fetch_all(&s.pool).await.unwrap();
        let items = |i: usize| {
            let v: serde_json::Value = serde_json::from_str(&after[i]).unwrap();
            assert_eq!(v[0], serde_json::json!({ "question": "Title", "answer": "A" }), "{v}");
            v[1].get("items").cloned()
        };
        assert_eq!(items(0), Some(serde_json::json!(["cpu", "rust"])));
        assert_eq!(items(1), Some(serde_json::json!([r#"say "hi""#, r"back\slash"])));
        assert_eq!(items(2), Some(serde_json::json!([])));
        assert_eq!(items(3), None, "not readable as a list: left alone");
        assert_eq!(items(4), None, "not a resource");
    }
}

//! What the bot trusts about a config instead of its repo: its own copy of
//! `lock.toml` and the settings an admin approved, keyed by config directory.

use anyhow::Result;

use crate::Store;

/// One value per config directory and kind.
pub(crate) const SCHEMA_V12: &str = r#"
CREATE TABLE trusted (
    scope TEXT NOT NULL, kind TEXT NOT NULL, body TEXT NOT NULL,
    PRIMARY KEY (scope, kind)
);
"#;

/// How many times each value changed, so a reader can tell it's out of date
/// without reading them all (see [`Store::trusted_rev`]).
pub(crate) const SCHEMA_V19: &str = r#"
ALTER TABLE trusted ADD COLUMN rev INTEGER NOT NULL DEFAULT 0;
"#;

impl Store {
    /// What was kept for `scope` (a config directory) under `kind`.
    pub async fn trusted(&self, scope: &str, kind: &str) -> Result<Option<String>> {
        let row: Option<(String,)> = sqlx::query_as("SELECT body FROM trusted WHERE scope = ? AND kind = ?")
            .bind(scope)
            .bind(kind)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|r| r.0))
    }

    pub async fn set_trusted(&self, scope: &str, kind: &str, body: &str) -> Result<()> {
        sqlx::query(
            "INSERT INTO trusted (scope, kind, body, rev) VALUES (?, ?, ?, 1) ON CONFLICT(scope, kind) \
             DO UPDATE SET body = excluded.body, rev = rev + (body IS NOT excluded.body)",
        )
        .bind(scope)
        .bind(kind)
        .bind(body)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// A number that grows whenever anything kept for `scope` changes (by
    /// this program or another sharing the database).
    pub async fn trusted_rev(&self, scope: &str) -> Result<i64> {
        let rev = sqlx::query_scalar("SELECT COALESCE(SUM(rev), 0) FROM trusted WHERE scope = ?")
            .bind(scope)
            .fetch_one(&self.pool)
            .await?;
        Ok(rev)
    }
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn kept_per_scope_and_kind() {
        let s = crate::Store::open("sqlite::memory:").await.unwrap();
        assert_eq!(s.trusted("/a", "lock").await.unwrap(), None);
        s.set_trusted("/a", "lock", "1").await.unwrap();
        s.set_trusted("/a", "lock", "2").await.unwrap();
        s.set_trusted("/b", "lock", "3").await.unwrap();
        assert_eq!(s.trusted("/a", "lock").await.unwrap().as_deref(), Some("2"));
        assert_eq!(s.trusted("/a", "approved").await.unwrap(), None);
    }

    #[tokio::test]
    async fn the_revision_grows_with_each_change() {
        let s = crate::Store::open("sqlite::memory:").await.unwrap();
        let rev = || s.trusted_rev("/a");
        let none = rev().await.unwrap();
        s.set_trusted("/a", "lock", "1").await.unwrap();
        let one = rev().await.unwrap();
        assert!(one > none, "a new value");
        s.set_trusted("/a", "lock", "1").await.unwrap();
        assert_eq!(rev().await.unwrap(), one, "the same value again");
        s.set_trusted("/a", "approved", "x").await.unwrap();
        let two = rev().await.unwrap();
        assert!(two > one, "another kind");
        s.set_trusted("/a", "lock", "2").await.unwrap();
        assert!(rev().await.unwrap() > two, "a changed value");
        s.set_trusted("/b", "lock", "3").await.unwrap();
        assert_eq!(s.trusted_rev("/b").await.unwrap(), 1, "per scope");
    }
}

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
        sqlx::query("INSERT INTO trusted (scope, kind, body) VALUES (?, ?, ?) ON CONFLICT(scope, kind) DO UPDATE SET body = excluded.body")
            .bind(scope)
            .bind(kind)
            .bind(body)
            .execute(&self.pool)
            .await?;
        Ok(())
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
}

//! Sign-ins to the web UI and its API, so a restart (every deploy) signs
//! nobody out. Only a hash of each token or cookie is kept: the database
//! alone signs nobody in.

use anyhow::Result;

use crate::Store;

pub(crate) const SCHEMA_V16: &str = r#"
CREATE TABLE web_signins (
    hash TEXT PRIMARY KEY,     -- SHA-256 of the bearer token or session cookie, hex
    id TEXT NOT NULL,          -- what it's listed and revoked as
    api INTEGER NOT NULL,      -- 1: a command-line sign-in, 0: a browser's
    user_id TEXT NOT NULL, user_name TEXT NOT NULL, device TEXT NOT NULL,
    csrf TEXT NOT NULL,
    created INTEGER NOT NULL, expires INTEGER NOT NULL
);
CREATE INDEX web_signins_user ON web_signins (user_id, api);
CREATE TABLE web_secrets (name TEXT PRIMARY KEY, value TEXT NOT NULL);
"#;

/// One sign-in, as kept.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct SignIn {
    /// SHA-256 of the token (or cookie), hex.
    pub hash: String,
    pub id: String,
    pub api: bool,
    pub user_id: String,
    pub user_name: String,
    /// The computer it's for, as its owner sees it listed.
    pub device: String,
    pub csrf: String,
    /// Unix seconds.
    pub created: i64,
    pub expires: i64,
}

const COLUMNS: &str = "hash, id, api, user_id, user_name, device, csrf, created, expires";

impl Store {
    /// The web UI's secret `name` (its cookie key), made from `new` the first
    /// time: whichever process asks first, they all get the same one.
    pub async fn web_secret(&self, name: &str, new: &str) -> Result<String> {
        sqlx::query("INSERT OR IGNORE INTO web_secrets (name, value) VALUES (?, ?)")
            .bind(name)
            .bind(new)
            .execute(&self.pool)
            .await?;
        Ok(sqlx::query_scalar("SELECT value FROM web_secrets WHERE name = ?").bind(name).fetch_one(&self.pool).await?)
    }

    /// Keep a new sign-in (dropping any that have expired).
    pub async fn put_signin(&self, s: &SignIn, now: i64) -> Result<()> {
        sqlx::query("DELETE FROM web_signins WHERE expires <= ?").bind(now).execute(&self.pool).await?;
        sqlx::query(
            "INSERT OR REPLACE INTO web_signins (hash, id, api, user_id, user_name, device, csrf, created, expires) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&s.hash)
        .bind(&s.id)
        .bind(s.api)
        .bind(&s.user_id)
        .bind(&s.user_name)
        .bind(&s.device)
        .bind(&s.csrf)
        .bind(s.created)
        .bind(s.expires)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// The live sign-in whose token hashes to `hash`.
    pub async fn signin(&self, hash: &str, now: i64) -> Result<Option<SignIn>> {
        let q = format!("SELECT {COLUMNS} FROM web_signins WHERE hash = ? AND expires > ?");
        Ok(sqlx::query_as(sqlx::AssertSqlSafe(q)).bind(hash).bind(now).fetch_optional(&self.pool).await?)
    }

    /// `user`'s live command-line sign-ins, newest first.
    pub async fn signins_of(&self, user: &str, now: i64) -> Result<Vec<SignIn>> {
        let q = format!(
            "SELECT {COLUMNS} FROM web_signins WHERE user_id = ? AND api = 1 AND expires > ? ORDER BY created DESC, id"
        );
        Ok(sqlx::query_as(sqlx::AssertSqlSafe(q)).bind(user).bind(now).fetch_all(&self.pool).await?)
    }

    /// Forget the sign-in whose token hashes to `hash` (signing out).
    pub async fn end_signin(&self, hash: &str) -> Result<()> {
        sqlx::query("DELETE FROM web_signins WHERE hash = ?").bind(hash).execute(&self.pool).await?;
        Ok(())
    }

    /// Revoke `user`'s command-line sign-in listed as `id`: its hash, if there was one.
    pub async fn revoke_signin(&self, user: &str, id: &str) -> Result<Option<String>> {
        Ok(sqlx::query_scalar("DELETE FROM web_signins WHERE user_id = ? AND id = ? AND api = 1 RETURNING hash")
            .bind(user)
            .bind(id)
            .fetch_optional(&self.pool)
            .await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn sign_ins_are_kept_listed_and_revoked() {
        let s = Store::open("sqlite::memory:").await.unwrap();
        let one = |hash: &str, id: &str, api: bool, expires: i64| SignIn {
            hash: hash.into(),
            id: id.into(),
            api,
            user_id: "7".into(),
            user_name: "ada".into(),
            device: "laptop".into(),
            csrf: "c".into(),
            created: 10,
            expires,
        };
        s.put_signin(&one("h1", "a", true, 100), 0).await.unwrap();
        s.put_signin(&one("h2", "b", false, 100), 0).await.unwrap();
        s.put_signin(&one("h3", "c", true, 20), 0).await.unwrap();
        assert_eq!(s.signin("h1", 50).await.unwrap().unwrap().device, "laptop");
        assert_eq!(s.signin("h3", 50).await.unwrap(), None, "expired");
        let listed: Vec<String> = s.signins_of("7", 50).await.unwrap().into_iter().map(|t| t.id).collect();
        assert_eq!(listed, ["a"], "command-line sign-ins only, live ones");
        assert_eq!(s.revoke_signin("8", "a").await.unwrap(), None, "only their own");
        assert_eq!(s.revoke_signin("7", "a").await.unwrap().as_deref(), Some("h1"));
        assert_eq!(s.signin("h1", 50).await.unwrap(), None);
        s.end_signin("h2").await.unwrap();
        assert_eq!(s.signin("h2", 50).await.unwrap(), None);

        assert_eq!(s.web_secret("key", "first").await.unwrap(), "first");
        assert_eq!(s.web_secret("key", "second").await.unwrap(), "first", "made once");
    }
}

//! The web server as an admin [`Host`]: its Discord client, the bot's
//! database, and config edits made while holding the engine.

use std::sync::Arc;

use anyhow::{Context, Result};
use chipsoc_admin::{Actor, Host};
use chipsoc_config::ops::Op;
use chipsoc_config::{Config, Snowflake};
use chipsoc_discord::Rest;
use chipsoc_store::Store;

use crate::App;
use crate::auth::Session;

/// One request's view of the server, as `actor`.
pub(crate) struct Desk<'a, R> {
    app: &'a App<R>,
    store: &'a Store,
    cfg: Arc<Config>,
    guild: Snowflake,
    gkey: String,
    pub actor: Actor,
}

impl<R: Rest + Clone> App<R> {
    /// The config as it is now, the server it manages and the database, for `s`'s request.
    pub(crate) async fn desk(&self, s: &Session) -> Result<Desk<'_, R>> {
        let cfg = self.config().await?;
        let guild = self.guild(Some(&cfg)).await?;
        let store = self.store.get_or_try_init(|| Store::open(&self.db)).await.context("opening the bot's database")?;
        let actor = match &s.user {
            Some(u) => Actor { id: Some(u.id), name: u.name.clone() },
            None => Actor { id: None, name: "the web UI".into() },
        };
        Ok(Desk { app: self, store, cfg, guild, gkey: guild.to_string(), actor })
    }
}

impl<R: Rest + Clone> Host for Desk<'_, R> {
    type Rest = R;

    fn rest(&self) -> &R {
        &self.app.rest
    }

    fn db(&self) -> chipsoc_store::Guild<'_> {
        self.store.guild(&self.gkey)
    }

    fn cfg(&self) -> Arc<Config> {
        self.cfg.clone()
    }

    fn guild(&self) -> Snowflake {
        self.guild
    }

    fn saves(&self) -> bool {
        self.app.saves
    }

    async fn record(&self, ops: &[Op], user: Snowflake, roles: &[String]) -> Result<chipsoc_admin::Recorded> {
        let mut e = self.app.engine.lock().await;
        let by = if self.actor.id.is_some() { self.actor.by(false) } else { chipsoc_engine::By::named(None) };
        let r = chipsoc_admin::record_with(&mut e, &by, ops, user, roles).await;
        self.app.forget_config();
        r
    }

    async fn undo(
        &self,
        commit: &str,
        how: chipsoc_config::git::Undo,
        by: &chipsoc_engine::By,
    ) -> Result<Option<chipsoc_config::git::Undid>> {
        let mut e = self.app.engine.lock().await;
        let r = e.undo(commit, how, by).await;
        self.app.forget_config();
        r
    }
}

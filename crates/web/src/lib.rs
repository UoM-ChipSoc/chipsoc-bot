//! A calm web UI for committee members: see the server as configured, edit
//! projects, and send the config to Discord. Pages are rendered on the server
//! with Dioxus SSR and every action is a plain HTML form, so it needs no
//! JavaScript and deploys with one `cargo build`. The CLI and TUI use the same
//! things through a JSON API (`chipsoc_client` is their client).

mod activity;
mod api;
mod auth;
mod changes;
mod channels;
mod feeds;
mod flash;
mod form;
mod history;
mod host;
mod members;
mod oauth;
mod onboarding;
mod overview;
mod people;
mod positions;
mod projects;
mod queue;
mod recordings;
mod resources;
mod roles;
mod theme;
mod view;

/// The JSON API's wire types live with their client.
pub use chipsoc_client::remote;

#[cfg(any(test, feature = "testing"))]
pub mod testing;
#[cfg(test)]
mod tests;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use chipsoc_config::Snowflake;
use chipsoc_config::git::blocking;

use anyhow::{Context, Result, bail};
use axum::Router;
use axum::http::HeaderValue;
use axum::http::header::{
    CACHE_CONTROL, CONTENT_SECURITY_POLICY, CONTENT_TYPE, REFERRER_POLICY, X_CONTENT_TYPE_OPTIONS,
};
use axum::middleware::{from_fn_with_state, map_response};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use chipsoc_config::Config;
use chipsoc_discord::{Http, Rest};
use chipsoc_engine::Engine;
use tokio::sync::{Mutex, MutexGuard};

use auth::{Auth, Session, SignIn};
use oauth::{BoxFuture, Discord, Members};

/// Serve the UI for the config in `config_dir`, talking to Discord with
/// `DISCORD_TOKEN` and sharing the bot's database (`DATABASE_URL`). People sign in with Discord when `DISCORD_CLIENT_SECRET`
/// and `WEB_URL` are set, else with `WEB_TOKEN` when that is; with neither,
/// only a loopback address is allowed.
pub async fn serve(config_dir: PathBuf, addr: SocketAddr) -> Result<()> {
    let sign_in = match Discord::from_env()? {
        Some(d) => {
            if std::env::var_os("WEB_TOKEN").is_some() {
                tracing::warn!("WEB_TOKEN is ignored: people sign in with Discord");
            }
            SignIn::Discord(d)
        }
        None => auth::web_token()?.map_or(SignIn::Open, SignIn::Code),
    };
    auth::check_bind(addr, &sign_in)?;
    if !config_dir.join("server.toml").exists() {
        bail!("{} has no server.toml; point at the config directory", config_dir.display());
    }
    let token = std::env::var("DISCORD_TOKEN").context("set DISCORD_TOKEN to the bot's token")?;
    // Loaded per request, so a broken file shows up as a problem on the page.
    let autosave = blocking({
        let d = config_dir.clone();
        move || Ok(chipsoc_engine::autosaves(&d))
    })
    .await?;
    let mut engine = Engine::unloaded(Http::new(token), &config_dir).autosave(autosave);
    // The lock and sensitive settings the bot trusts, not just the repo's (see `chipsoc_config::trust`).
    let db = chipsoc_store::Store::env_url()?;
    let store = chipsoc_store::Store::open(&db).await.with_context(|| format!("opening {db}"))?;
    engine.set_baseline(chipsoc_admin::trust::StoreBaseline::new(store, &config_dir));
    let listener = tokio::net::TcpListener::bind(addr).await.with_context(|| format!("listening on {addr}"))?;
    match &sign_in {
        SignIn::Open => tracing::warn!("WEB_TOKEN is not set: anyone on this machine can use the web UI"),
        SignIn::Code(_) => {}
        SignIn::Discord(d) => tracing::info!("signing in with Discord; redirect URI {}", d.redirect_uri()),
    }
    tracing::info!("web UI at http://{addr}");
    run(listener, router(engine, db, sign_in)).await
}

/// How long a connection may take to send a request's headers, and stay idle between requests.
const HEADER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);
/// How long a request's body may take to arrive.
const BODY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
/// The largest request body; forms and API calls are far smaller.
const MAX_BODY: usize = 1 << 20;
/// Connections served at once; more wait to be accepted.
const MAX_CONNECTIONS: usize = 256;
/// The longest any one connection stays open.
const MAX_CONNECTION: std::time::Duration = std::time::Duration::from_secs(30 * 60);

/// Serve `app` on `listener` (HTTP/1.1), so slow or idle clients can't hold
/// connections: headers within [`HEADER_TIMEOUT`], bodies within
/// [`BODY_TIMEOUT`] and [`MAX_BODY`], at most [`MAX_CONNECTIONS`] at once.
pub(crate) async fn run(listener: tokio::net::TcpListener, app: Router) -> Result<()> {
    use hyper_util::rt::{TokioIo, TokioTimer};
    let app = app.layer(axum::middleware::from_fn(limit_body));
    let slots = Arc::new(tokio::sync::Semaphore::new(MAX_CONNECTIONS));
    loop {
        let slot = slots.clone().acquire_owned().await.context("web server stopped")?;
        let (stream, _) = match listener.accept().await {
            Ok(conn) => conn,
            Err(e) => {
                tracing::warn!("accepting a web connection failed: {e}");
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                continue;
            }
        };
        let service = hyper_util::service::TowerToHyperService::new(app.clone());
        tokio::spawn(async move {
            let conn = hyper::server::conn::http1::Builder::new()
                .timer(TokioTimer::new())
                .header_read_timeout(HEADER_TIMEOUT)
                .serve_connection(TokioIo::new(stream), service);
            let _ = tokio::time::timeout(MAX_CONNECTION, conn).await;
            drop(slot);
        });
    }
}

/// Read the whole body up front, within [`BODY_TIMEOUT`] and [`MAX_BODY`].
async fn limit_body(req: axum::extract::Request, next: axum::middleware::Next) -> Response {
    use axum::http::StatusCode;
    let (parts, body) = req.into_parts();
    let bytes = match tokio::time::timeout(BODY_TIMEOUT, axum::body::to_bytes(body, MAX_BODY)).await {
        Ok(Ok(bytes)) => bytes,
        Ok(Err(_)) => return (StatusCode::PAYLOAD_TOO_LARGE, "That request is too large.").into_response(),
        Err(_) => return (StatusCode::REQUEST_TIMEOUT, "That request took too long to send.").into_response(),
    };
    next.run(axum::extract::Request::from_parts(parts, axum::body::Body::from(bytes))).await
}

/// What every page shares. Pages read the config (cached until its files
/// change); the engine is held only to plan, apply, pull or edit.
struct App<R> {
    engine: Mutex<Engine<R>>,
    /// The engine's Discord client, for work that doesn't need the engine.
    rest: R,
    /// The config directory.
    dir: PathBuf,
    /// The server, once known: from the lockfile, else asked of Discord.
    guild: tokio::sync::OnceCell<Snowflake>,
    /// Where what the bot trusts is kept (its lock and approved settings),
    /// imposed on every config read here.
    baseline: Option<Arc<dyn chipsoc_engine::Baseline>>,
    /// The config as last loaded and vetted.
    loaded: std::sync::Mutex<Option<Loaded>>,
    /// Plans made through the API, by id, waiting to be applied.
    plans: std::sync::Mutex<chipsoc_model::Expiring<api::Kept>>,
    /// The bot's database, opened when first needed.
    db: String,
    store: tokio::sync::OnceCell<chipsoc_store::Store>,
    /// Whether the engine commits and pushes config changes.
    saves: bool,
    /// Where the bot keeps recordings (beside its database).
    recordings: PathBuf,
}

/// A config read by [`App::config`], and what it was read from.
#[derive(Clone)]
struct Loaded {
    /// The files' fingerprint.
    files: u64,
    /// What was trusted, as JSON.
    trusted: String,
    cfg: Arc<Config>,
}

impl<R: Rest> App<R> {
    /// The engine, with the config re-read from disk: for planning, applying and pulling.
    async fn engine(&self) -> Result<MutexGuard<'_, Engine<R>>> {
        let mut e = self.engine.lock().await;
        e.refresh().await?;
        self.forget_config();
        Ok(e)
    }

    /// The config, vetted against what the bot trusts: read again only when
    /// its files, or what's trusted, have changed.
    async fn config(&self) -> Result<Arc<Config>> {
        let dir = self.dir.clone();
        let files = blocking(move || Ok(chipsoc_config::fingerprint(&dir))).await?;
        let trusted = match &self.baseline {
            Some(b) => {
                let t = b.load().await?;
                serde_json::to_string(&(&t.lock, &t.approved))?
            }
            None => String::new(),
        };
        if let Some(c) = self.cached()
            && (c.files, &c.trusted) == (files, &trusted)
        {
            return Ok(c.cfg);
        }
        let dir = self.dir.clone();
        let mut cfg = blocking(move || Config::load(&dir)).await?;
        if let Some(b) = &self.baseline {
            chipsoc_engine::vet_with(&**b, &mut cfg).await?;
        }
        crate::form::set_zone(&cfg.bot.timezone);
        let cfg = Arc::new(cfg);
        *self.loaded.lock().unwrap_or_else(|e| e.into_inner()) = Some(Loaded { files, trusted, cfg: cfg.clone() });
        Ok(cfg)
    }

    fn cached(&self) -> Option<Loaded> {
        self.loaded.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// After a change made here: read the files again next time, even if a
    /// change came too quickly for the fingerprint to see.
    fn forget_config(&self) {
        if let Some(c) = self.loaded.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
            c.files = 0;
        }
    }

    /// The server this config manages.
    async fn guild(&self, cfg: Option<&Config>) -> Result<Snowflake> {
        let known = cfg.and_then(Config::guild);
        let g = self
            .guild
            .get_or_try_init(|| async {
                match known {
                    Some(g) => Ok(g),
                    None => Ok::<_, anyhow::Error>(chipsoc_discord::sole_guild(&self.rest).await?.0),
                }
            })
            .await?;
        Ok(*g)
    }

    /// Make one config edit as `s` (see [`Engine::edit`]): checked first,
    /// committed saying who, with no apply or pull in between.
    async fn edit(&self, s: &Session, op: &chipsoc_config::ops::Op) -> Result<chipsoc_config::ops::Change> {
        self.edit_ops(s, std::slice::from_ref(op)).await
    }

    /// Make config edits as `s` as one change (see [`Engine::edit_ops`]):
    /// one check, one commit, all or nothing.
    async fn edit_ops(&self, s: &Session, ops: &[chipsoc_config::ops::Op]) -> Result<chipsoc_config::ops::Change> {
        // Not reloaded first: an edit may be what fixes a config that doesn't load.
        let mut e = self.engine.lock().await;
        let change = e.edit_ops(ops, &s.by()).await;
        self.forget_config();
        let change = change?;
        if !change.files.is_empty() {
            tracing::info!(actor = s.actor().unwrap_or("-"), "web UI edit: {}", change.summary);
        }
        Ok(change)
    }
}

impl<R: Rest + 'static> Members for App<R> {
    /// Asked again every few minutes per session, so it never waits for the
    /// engine (a plan or apply can hold it for a while).
    fn may_use(&self, user: Snowflake) -> BoxFuture<'_, Result<bool>> {
        Box::pin(async move {
            // A broken config still has its last good admin role and server id,
            // and owners and Administrators can always sign in to fix it.
            let cfg = match self.config().await {
                Ok(c) => Some(c),
                Err(err) => {
                    tracing::warn!("checking web UI access with a config that doesn't load: {err:#}");
                    self.cached().map(|c| c.cfg)
                }
            };
            let cfg = cfg.as_deref();
            let admin_role = cfg.and_then(|c| c.desired.lock.role(c.bot.admin_role.as_deref()?));
            let g = self.guild(cfg).await?;
            chipsoc_admin::is_admin(&self.rest, g, admin_role, user).await
        })
    }

    fn who_may_use(&self) -> BoxFuture<'_, String> {
        Box::pin(async move {
            let cfg = self.config().await.ok().or_else(|| self.cached().map(|c| c.cfg));
            match cfg.as_deref().and_then(|c| Some((c, c.bot.admin_role.as_deref()?))) {
                Some((c, k)) => format!("members of {}, or anyone with Manage Server", chipsoc_admin::role_name(c, k)),
                None => "anyone with Manage Server".to_owned(),
            }
        })
    }

    fn note(&self, text: String) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            let log = match self.config().await {
                Ok(cfg) => cfg.bot.log.bot.as_deref().and_then(|k| cfg.desired.lock.channel(k)),
                Err(e) => {
                    tracing::warn!("not telling the bot log: the config doesn't load: {e:#}");
                    None
                }
            };
            let Some(ch) = log else { return };
            let body = serde_json::json!({ "content": text, "allowed_mentions": { "parse": [] } });
            let path = format!("channels/{ch}/messages");
            if let Err(e) = self.rest.call(chipsoc_discord::Method::Post, &path, Some(body)).await {
                tracing::warn!("posting to the bot log failed: {e:#}");
            }
        })
    }
}

/// The whole site, on `engine`'s config and Discord client, with the bot's database at `db`.
fn router<R: Rest + Clone + 'static>(engine: Engine<R>, db: String, sign_in: SignIn) -> Router {
    let (rest, dir, baseline, saves) = (engine.rest.clone(), engine.cfg.dir.clone(), engine.baseline(), engine.saves());
    let app = Arc::new(App {
        engine: Mutex::new(engine),
        rest,
        dir,
        guild: Default::default(),
        baseline,
        loaded: Default::default(),
        plans: api::kept_plans(),
        recordings: chipsoc_voice::store::root_for(&db),
        db,
        store: Default::default(),
        saves,
    });
    let auth = Arc::new(Auth::new(sign_in, app.clone(), Some(app.db.clone())));
    let pages = Router::new()
        .route("/", get(overview::page::<R>))
        // The old combined page: roles and channels have their own now.
        .route("/server", get(|| async { axum::response::Redirect::permanent("/channels") }))
        .route("/roles", get(roles::page::<R>))
        .route("/roles/{action}", post(roles::act::<R>))
        .route("/activity", get(activity::page::<R>))
        .route("/onboarding", get(onboarding::page::<R>))
        .route("/onboarding/{action}", post(onboarding::act::<R>))
        .route("/feeds", get(feeds::page::<R>))
        .route("/feeds/{action}", post(feeds::act::<R>))
        .route("/resources", get(resources::page::<R>))
        .route("/resources/{action}", post(resources::act::<R>))
        .route("/members", get(members::page::<R>))
        .route("/members/{action}", post(members::act::<R>))
        .route("/queue", get(queue::page::<R>))
        .route("/queue/{action}", post(queue::act::<R>))
        .route("/positions", get(positions::page::<R>))
        .route("/positions/{action}", post(positions::act::<R>))
        .route("/channels", get(channels::page::<R>))
        .route("/channels/{action}", post(channels::act::<R>))
        .route("/channels/permissions/{kind}/{key}", get(channels::permissions::<R>))
        .route("/projects", get(projects::page::<R>).post(projects::add::<R>))
        .route("/projects/{slug}/status", post(projects::set_status::<R>))
        .route("/projects/{slug}/applications", post(projects::set_applications::<R>))
        .route("/projects/{slug}/{action}", post(projects::act::<R>))
        .route("/changes", get(changes::page))
        .route("/changes/plan", post(changes::plan::<R>))
        .route("/changes/apply", post(changes::apply::<R>))
        .route("/changes/adopt", post(changes::adopt::<R>))
        .route("/changes/history", get(history::page::<R>))
        .route("/recordings", get(recordings::page::<R>))
        .route("/recordings/delete", post(recordings::delete::<R>))
        .route("/recordings/{id}", get(recordings::one::<R>))
        .route("/recordings/{id}/files/{file}", get(recordings::file::<R>))
        .route("/changes/undo", post(history::undo::<R>))
        .with_state(app.clone())
        .merge(
            Router::new()
                .route("/logout", post(auth::logout))
                .route("/account", get(auth::account))
                .route("/account/revoke", post(auth::account_revoke))
                .with_state(auth.clone()),
        )
        .layer(from_fn_with_state(app.clone(), view::name_server::<R>))
        .layer(from_fn_with_state(auth.clone(), auth::guard));
    let api = Router::new()
        .route("/me", get(api::me::<R>))
        .route("/export", get(api::export::<R>))
        .route("/plan", get(api::plan::<R>))
        .route("/apply", post(api::apply::<R>))
        .route("/pull", post(api::pull::<R>))
        .route("/check", get(api::check::<R>))
        .route("/projects", get(api::projects::<R>).post(api::add_project::<R>))
        .route("/projects/{slug}/status", post(api::set_status::<R>))
        .route("/projects/{slug}/applications", post(api::set_applications::<R>))
        .route("/ops", post(api::op::<R>))
        .route("/ops/schema", get(api::schema::<R>))
        .route("/ops/preview", post(api::preview::<R>))
        .route("/queue", get(api::queue::<R>))
        .route("/queue/{id}", post(api::decide::<R>))
        .route("/queue/{id}/lead", post(api::make_lead::<R>))
        .route("/feeds", get(api::feeds::<R>))
        .route("/feeds/{id}", delete(api::remove_feed::<R>))
        .route("/resources", get(api::resources::<R>).post(api::post_resource::<R>))
        .route("/ideas", get(api::ideas::<R>))
        .route("/ideas/{id}/close", post(api::close_idea::<R>))
        .route("/share", post(api::share::<R>))
        .route("/activity", get(api::activity::<R>))
        .route("/changes", get(api::changes::<R>))
        .route("/undo", post(api::undo::<R>))
        .route("/join", get(api::join::<R>))
        .route("/join/notify", get(api::join_notice::<R>).post(api::send_join_notice::<R>))
        .route("/status", get(api::status::<R>))
        .route("/drift", get(api::drift::<R>))
        .route("/queue/history", get(api::history::<R>))
        .route("/feeds", post(api::add_feed::<R>))
        .route("/feeds/{id}/pause", post(api::pause_feed::<R>))
        .route("/resources/{id}", delete(api::archive_resource::<R>))
        .route("/members", get(api::members::<R>).post(api::appoint::<R>))
        .route("/members/{role}/{user}", delete(api::unappoint::<R>))
        .route("/members/about/{user}", get(api::about::<R>))
        .route("/members/find", get(api::find_members::<R>))
        .route("/members/former/{user}", delete(api::forget::<R>))
        .route("/members/former/{user}/restore", post(api::restore::<R>))
        .route("/projects/{slug}/leads", post(api::add_lead::<R>))
        .route("/projects/{slug}/leads/{user}", delete(api::remove_lead::<R>))
        .with_state(app)
        .merge(
            Router::new()
                .route("/tokens", get(auth::api_tokens))
                .route("/tokens/{id}", delete(auth::api_revoke))
                .with_state(auth.clone()),
        )
        .layer(from_fn_with_state(auth.clone(), auth::api_guard));
    Router::new()
        .route("/login", get(auth::login_page).post(auth::login))
        .route("/auth/discord", get(auth::discord_callback))
        .route("/auth/cli", get(auth::cli_login))
        .route("/auth/cli/confirm", post(auth::cli_confirm))
        .route("/auth/cli/token", post(auth::cli_token))
        .route("/auth/cli/done", get(auth::cli_done))
        .route("/auth/cli/failed", get(auth::cli_failed))
        .route("/api/v1/logout", post(auth::api_logout))
        .with_state(auth)
        .route("/style.css", get(style))
        .route("/theme/{mode}", get(theme::set))
        .nest("/api/v1", api)
        .merge(pages)
        .layer(axum::middleware::from_fn(flash::stamp))
        .layer(axum::middleware::from_fn(theme::stamp))
        .layer(map_response(harden))
}

/// The policy every page gets unless it sets its own.
pub(crate) const CSP: &str =
    "default-src 'none'; style-src 'self'; form-action 'self'; frame-ancestors 'none'; base-uri 'none'";

async fn style() -> Response {
    let headers = [(CONTENT_TYPE, "text/css; charset=utf-8"), (CACHE_CONTROL, "max-age=3600")];
    (headers, include_str!("style.css")).into_response()
}

/// No scripts, no framing, no leaking URLs; pages aren't cached. A page may
/// set its own policy (the CLI sign-in's, to hand back to 127.0.0.1).
async fn harden(mut res: Response) -> Response {
    let h = res.headers_mut();
    h.entry(CONTENT_SECURITY_POLICY).or_insert(HeaderValue::from_static(CSP));
    h.insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    h.insert(REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    h.entry(CACHE_CONTROL).or_insert(HeaderValue::from_static("no-store"));
    res
}

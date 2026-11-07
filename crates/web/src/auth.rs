//! Sign-in, sessions (kept in the bot's database, hashed, so a restart signs
//! nobody out), and a per-session CSRF token checked on every POST. Cookies
//! are encrypted with a key kept in the database (`axum-extra`'s private
//! cookies). Sign-in is with Discord (see [`crate::oauth`]) when it's set up,
//! else a shared code (`WEB_TOKEN`, for trying the UI out on this machine),
//! else none at all on loopback. With Discord, `login` in the CLI gets a
//! bearer token for the API the same way, once the person confirms on this
//! server that it's their terminal asking.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Result, bail};
use axum::body::{Body, to_bytes};
use axum::extract::{Path, Query, Request, State};
use axum::http::header::{AUTHORIZATION, CONTENT_SECURITY_POLICY, HOST};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::{Extension, Form, Json};
use axum_extra::extract::PrivateCookieJar;
use axum_extra::extract::cookie::{Cookie, Key, SameSite};
use chipsoc_store::Store;
use chrono::{DateTime, Utc};
use dioxus::prelude::*;
use oauth2::{CsrfToken, PkceCodeChallenge, PkceCodeVerifier};
use serde::{Deserialize, Serialize};
use subtle::ConstantTimeEq;
use tokio::time::Instant;

use crate::api::failure;
use crate::oauth::{Discord, Members, User};
use crate::remote::TokenInfo;
use crate::view;

const COOKIE_NAME: &str = "session";
/// The name the cookie key is kept under in the database.
const COOKIE_KEY: &str = "cookie_key";
/// Ties Discord's redirect back to the browser that started signing in.
const LOGIN_COOKIE: &str = "login";
const SESSION_TTL: Duration = Duration::from_secs(12 * 60 * 60);
/// How long a CLI sign-in lasts.
pub(crate) const API_TTL: Duration = Duration::from_secs(30 * 24 * 60 * 60);
const LOGIN_TTL: Duration = Duration::from_secs(10 * 60);
/// How long the CLI has to trade its one-time code for a token.
const CODE_TTL: Duration = Duration::from_secs(5 * 60);
/// CLI sign-ins waiting to be confirmed, and codes waiting for the CLI, held
/// at once (the oldest goes first). Only people Discord says may use the UI
/// make these; sign-ins still at Discord are kept in the browser's cookie.
const MAX_PENDING: usize = 1000;
/// Sessions held at once (the oldest goes first).
const MAX_SESSIONS: usize = 10_000;
/// How long a Discord check stands before a change asks again, so someone
/// taken off the committee loses access soon.
pub(crate) const RECHECK: Duration = Duration::from_secs(5 * 60);
const MIN_SECRET: usize = 12;
/// Forms are small: plans stay on the server and only their id comes back.
const MAX_FORM: usize = 64 << 10;
/// Wrong codes wait this long, one at a time, so guessing is slow.
const FAILED_LOGIN_DELAY: Duration = Duration::from_secs(1);
/// The longest device name kept from the CLI.
const MAX_DEVICE: usize = 64;

/// How people sign in.
pub(crate) enum SignIn {
    /// Nobody does; only loopback is served.
    Open,
    /// With the shared `WEB_TOKEN`.
    Code(String),
    Discord(Discord),
}

/// The signed-in visitor, handed to every page.
#[derive(Clone)]
pub(crate) struct Session {
    pub csrf: String,
    /// False when nobody needs to sign in (nothing to sign out of).
    pub signed_in: bool,
    /// Who, when they signed in with Discord.
    pub user: Option<User>,
}

/// Who signed in with the shared code, in logs and commits: it has no name.
pub(crate) const SHARED: &str = "shared token";

impl Session {
    /// Who, for commit messages and logs: the Discord username, or
    /// [`SHARED`] for the shared code.
    pub fn actor(&self) -> Option<&str> {
        match &self.user {
            Some(u) => Some(u.name.as_str()),
            None if self.signed_in => Some(SHARED),
            None => None,
        }
    }

    /// Who, as the engine knows them (see [`chipsoc_engine::By`]).
    pub fn by(&self) -> chipsoc_engine::By {
        match &self.user {
            Some(u) => chipsoc_engine::By::person(u.id, &u.name),
            None => chipsoc_engine::By::named(self.actor()),
        }
    }
}

struct Entry {
    session: Session,
    expires: Instant,
    /// When Discord last said this user may use the UI (none: not since this server started).
    checked: Option<Instant>,
    /// Set for a bearer token for the API, not a browser's cookie.
    api: Option<Named>,
}

/// What a CLI sign-in is listed as, so its owner can see and revoke it.
struct Named {
    /// Shown and used to revoke it; never the token itself.
    id: String,
    name: String,
    created: DateTime<Utc>,
    expires: DateTime<Utc>,
}

/// A sign-in started by the CLI: where its one-time code goes once Discord
/// says who it is and they confirm.
#[derive(Clone, Serialize, Deserialize)]
struct CliLogin {
    /// Its listener on 127.0.0.1; none shows the code to paste instead.
    port: Option<u16>,
    state: String,
    /// The PKCE S256 challenge of a secret only the CLI knows: the code is
    /// worth a token only with that secret, so whoever else sees it gets nothing.
    challenge: String,
    /// The name the CLI gave for its computer.
    device: Option<String>,
}

/// A sign-in on its way through Discord, kept in the browser's login cookie
/// (encrypted by this server): nothing is stored here until Discord says who
/// it is, so a flood of sign-ins can't push out anyone else's.
#[derive(Serialize, Deserialize)]
struct Ticket {
    /// The OAuth2 `state` Discord carries back.
    state: String,
    /// The PKCE secret whose hash went to Discord with the sign-in.
    verifier: String,
    /// When it started (seconds since 1970).
    at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cli: Option<CliLogin>,
}

/// A CLI sign-in Discord has confirmed, waiting for the person to press
/// Continue on this server's page.
struct Confirm {
    user: User,
    cli: CliLogin,
    expires: Instant,
}

/// A one-time code from a CLI sign-in, waiting to be traded for a token.
struct Code {
    user: User,
    challenge: String,
    device: Option<String>,
    expires: Instant,
}

pub(crate) struct Auth {
    sign_in: SignIn,
    members: Arc<dyn Members>,
    /// The bot's database, where sign-ins and the cookie key are kept so a
    /// restart signs nobody out; opened when first needed.
    db: Option<String>,
    store: tokio::sync::OnceCell<Store>,
    /// Encrypts and signs cookies (see [`PrivateCookieJar`]).
    key: tokio::sync::OnceCell<Key>,
    /// Sessions seen since this server started, by token: those without a
    /// Discord user live only here; the rest are also in the database.
    sessions: std::sync::Mutex<HashMap<String, Entry>>,
    /// CLI sign-ins waiting to be confirmed, by a random id on their page.
    confirms: std::sync::Mutex<HashMap<String, Confirm>>,
    /// CLI sign-ins done, by one-time code.
    codes: std::sync::Mutex<HashMap<String, Code>>,
    attempts: tokio::sync::Mutex<()>,
}

/// Make room in a map of things that expire: drop the expired, then the
/// oldest while it's full.
fn make_room<V>(all: &mut HashMap<String, V>, max: usize, expires: impl Fn(&V) -> Instant) {
    let now = Instant::now();
    all.retain(|_, v| expires(v) > now);
    while all.len() >= max {
        let Some(oldest) = all.iter().min_by_key(|(_, v)| expires(v)).map(|(k, _)| k.clone()) else { break };
        all.remove(&oldest);
    }
}

fn lock<T>(m: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// How a token is kept in the database: never the token itself.
fn hashed(token: &str) -> String {
    chipsoc_model::sha256_hex(token.as_bytes())
}

/// Whether two secrets match, in constant time.
fn same(a: &str, b: &str) -> bool {
    a.as_bytes().ct_eq(b.as_bytes()).into()
}

impl Auth {
    /// Sign-in as `sign_in` says, asking `members` who may; sign-ins kept in
    /// the database at `db` (none: in memory only).
    pub fn new(sign_in: SignIn, members: Arc<dyn Members>, db: Option<String>) -> Self {
        Self {
            sign_in,
            members,
            db,
            store: Default::default(),
            key: Default::default(),
            sessions: Default::default(),
            confirms: Default::default(),
            codes: Default::default(),
            attempts: Default::default(),
        }
    }

    /// The bot's database, if it opens (else sign-ins last until a restart).
    /// A failed open is tried again next time: only an open database is kept.
    async fn store(&self) -> Option<Store> {
        let url = self.db.as_deref()?;
        let opened = self
            .store
            .get_or_try_init(|| async {
                Store::open(url).await.inspect_err(|e| {
                    tracing::warn!("opening the database for sign-ins failed, so they're kept in memory: {e:#}")
                })
            })
            .await;
        opened.ok().cloned()
    }

    /// The cookie key: kept in the database, so cookies outlive a restart.
    async fn key(&self) -> Key {
        if let Some(k) = self.key.get() {
            return k.clone();
        }
        let fresh = Key::generate();
        let kept = match self.store().await {
            Some(store) => store.web_secret(COOKIE_KEY, &chipsoc_client::remote::hex(fresh.master())).await.ok(),
            None => None,
        };
        let key = match kept.as_deref().and_then(unhex) {
            Some(bytes) if bytes.len() >= 64 => Key::from(&bytes),
            _ => {
                tracing::warn!("couldn't keep the cookie key in the database; sign-ins last until a restart");
                fresh
            }
        };
        let _ = self.key.set(key);
        self.key.get().cloned().unwrap_or_else(Key::generate)
    }

    /// This request's cookies, readable only if this server made them.
    async fn jar(&self, headers: &HeaderMap) -> PrivateCookieJar {
        PrivateCookieJar::from_headers(headers, self.key().await)
    }

    fn sessions(&self) -> std::sync::MutexGuard<'_, HashMap<String, Entry>> {
        lock(&self.sessions)
    }

    /// A new session and its cookie id.
    async fn start(&self, signed_in: bool, user: Option<User>) -> (String, Session) {
        let session = Session { csrf: random_token(), signed_in, user };
        (self.insert(session.clone(), None).await, session)
    }

    /// A new API token for `user`, listed as `name`.
    async fn mint(&self, user: User, name: String) -> String {
        let created = Utc::now();
        let expires = created + chrono::Duration::from_std(API_TTL).unwrap_or_default();
        let named = Named { id: random_token()[..16].to_owned(), name, created, expires };
        self.insert(Session { csrf: random_token(), signed_in: true, user: Some(user) }, Some(named)).await
    }

    async fn insert(&self, session: Session, api: Option<Named>) -> String {
        let token = random_token();
        let ttl = if api.is_some() { API_TTL } else { SESSION_TTL };
        if let (Some(user), Some(store)) = (&session.user, self.store().await) {
            let now = unix_now() as i64;
            let kept = chipsoc_store::SignIn {
                hash: hashed(&token),
                id: api.as_ref().map_or_else(|| random_token()[..16].to_owned(), |n| n.id.clone()),
                api: api.is_some(),
                user_id: user.id.to_string(),
                user_name: user.name.clone(),
                device: api.as_ref().map(|n| n.name.clone()).unwrap_or_default(),
                csrf: session.csrf.clone(),
                created: api.as_ref().map_or(now, |n| n.created.timestamp()),
                expires: now + ttl.as_secs() as i64,
            };
            if let Err(e) = store.put_signin(&kept, now).await {
                tracing::warn!("keeping a sign-in in the database failed; it lasts until a restart: {e:#}");
            }
        }
        let mut all = self.sessions();
        let now = Instant::now();
        make_room(&mut all, MAX_SESSIONS, |e| e.expires);
        all.insert(token.clone(), Entry { session, expires: now + ttl, checked: Some(now), api });
        token
    }

    /// A live session (`api`: a token, else a cookie): as this server has
    /// seen it since it started, else as the database kept it.
    async fn lookup(&self, token: &str, api: bool) -> Option<Session> {
        let seen = self.sessions().get(token).map(|e| (e.api.is_some(), e.expires, e.session.clone()));
        if let Some((is_api, expires, session)) = seen {
            if expires > Instant::now() {
                return (is_api == api).then_some(session);
            }
            self.forget(token).await;
            return None;
        }
        let store = self.store().await?;
        let now = unix_now() as i64;
        let kept = match store.signin(&hashed(token), now).await {
            Ok(k) => k?,
            Err(e) => {
                tracing::warn!("reading sign-ins from the database failed: {e:#}");
                return None;
            }
        };
        let user = User { id: kept.user_id.parse().ok()?, name: kept.user_name.clone() };
        let session = Session { csrf: kept.csrf.clone(), signed_in: true, user: Some(user) };
        let at = |t: i64| DateTime::from_timestamp(t, 0).unwrap_or_default();
        let named = kept.api.then(|| Named {
            id: kept.id.clone(),
            name: kept.device.clone(),
            created: at(kept.created),
            expires: at(kept.expires),
        });
        let left = Duration::from_secs(kept.expires.saturating_sub(now).max(0) as u64);
        let mut all = self.sessions();
        make_room(&mut all, MAX_SESSIONS, |e| e.expires);
        // Not checked with Discord since this server started: asked again before any change.
        all.insert(
            token.to_owned(),
            Entry { session: session.clone(), expires: Instant::now() + left, checked: None, api: named },
        );
        (kept.api == api).then_some(session)
    }

    /// Sign a session or token out, here and in the database.
    async fn forget(&self, token: &str) {
        self.sessions().remove(token);
        if let Some(store) = self.store().await
            && let Err(e) = store.end_signin(&hashed(token)).await
        {
            tracing::warn!("forgetting a sign-in in the database failed: {e:#}");
        }
    }

    /// `user`'s CLI sign-ins, newest first; `current` is the token asking, if one is.
    async fn tokens_of(&self, user: &User, current: Option<&str>) -> Vec<TokenInfo> {
        let current = current.map(hashed);
        let info = |id: &str, name: &str, created: DateTime<Utc>, expires: DateTime<Utc>, hash: &str| TokenInfo {
            id: id.to_owned(),
            name: name.to_owned(),
            created: created.format("%Y-%m-%d %H:%M UTC").to_string(),
            expires: expires.format("%Y-%m-%d").to_string(),
            current: current.as_deref() == Some(hash),
        };
        if let Some(store) = self.store().await {
            let at = |t: i64| DateTime::from_timestamp(t, 0).unwrap_or_default();
            match store.signins_of(&user.id.to_string(), unix_now() as i64).await {
                Ok(kept) => {
                    return kept
                        .iter()
                        .map(|k| info(&k.id, &k.device, at(k.created), at(k.expires), &k.hash))
                        .collect();
                }
                Err(e) => tracing::warn!("reading sign-ins from the database failed: {e:#}"),
            }
        }
        let now = Instant::now();
        let mut out: Vec<(DateTime<Utc>, TokenInfo)> = self
            .sessions()
            .iter()
            .filter(|(_, e)| e.expires > now && e.session.user.as_ref().is_some_and(|u| u.id == user.id))
            .filter_map(|(token, e)| {
                let n = e.api.as_ref()?;
                Some((n.created, info(&n.id, &n.name, n.created, n.expires, &hashed(token))))
            })
            .collect();
        out.sort_by_key(|t| std::cmp::Reverse(t.0));
        out.into_iter().map(|(_, t)| t).collect()
    }

    /// Revoke `user`'s CLI sign-in listed as `id`; whether there was one.
    async fn revoke(&self, user: &User, id: &str) -> bool {
        let mine = |e: &Entry| {
            e.session.user.as_ref().is_some_and(|u| u.id == user.id) && e.api.as_ref().is_some_and(|n| same(&n.id, id))
        };
        let seen = self.sessions().iter().find(|(_, e)| mine(e)).map(|(t, _)| t.clone());
        let kept = match self.store().await {
            Some(store) => store.revoke_signin(&user.id.to_string(), id).await.unwrap_or_else(|e| {
                tracing::warn!("revoking a sign-in in the database failed: {e:#}");
                None
            }),
            None => None,
        };
        let mut all = self.sessions();
        if let Some(hash) = &kept {
            all.retain(|t, _| hashed(t) != *hash);
        }
        if let Some(t) = &seen {
            all.remove(t);
        }
        let found = kept.is_some() || seen.is_some();
        if found {
            tracing::info!(user = %user.name, "revoked a command-line sign-in");
        }
        found
    }

    /// The browser's session cookie: its id and session.
    async fn find(&self, headers: &HeaderMap) -> Option<(String, Session)> {
        let id = self.jar(headers).await.get(COOKIE_NAME)?.value().to_owned();
        let s = self.lookup(&id, false).await?;
        Some((id, s))
    }

    async fn end(&self, headers: &HeaderMap) {
        if let Some(c) = self.jar(headers).await.get(COOKIE_NAME) {
            self.forget(c.value()).await;
        }
    }

    /// The visitor's session id and session (new: needs its cookie set), or where to send them.
    async fn resolve(&self, headers: &HeaderMap) -> Result<(String, Session, bool), Refused> {
        let open = matches!(self.sign_in, SignIn::Open);
        if open && !loopback_host(headers) {
            return Err(Refused::NotLocal);
        }
        match self.find(headers).await {
            Some((id, s)) => Ok((id, s, false)),
            None if open => {
                let (id, s) = self.start(false, None).await;
                Ok((id, s, true))
            }
            None => Err(Refused::SignIn),
        }
    }

    /// Before a change: ask Discord again whether a Discord user still may,
    /// if it's been [`RECHECK`] since it last said so.
    async fn recheck(&self, id: &str, s: &Session) -> Result<(), Refused> {
        let Some(user) = &s.user else { return Ok(()) };
        let checked = self.sessions().get(id).and_then(|e| e.checked);
        if checked.is_some_and(|at| at.elapsed() < RECHECK) {
            return Ok(());
        }
        match self.members.may_use(user.id).await {
            Ok(true) => {
                if let Some(e) = self.sessions().get_mut(id) {
                    e.checked = Some(Instant::now());
                }
                Ok(())
            }
            Ok(false) => {
                tracing::info!(user = %user.name, "signed out: no longer allowed to use the web UI");
                self.forget(id).await;
                Err(Refused::Revoked)
            }
            Err(e) => {
                tracing::warn!(user = %user.name, "checking web UI access with Discord failed: {e:#}");
                Err(Refused::Unchecked)
            }
        }
    }

    /// The sign-in in this browser's login cookie, if this server made it and it's still live.
    fn ticket(jar: &PrivateCookieJar) -> Option<Ticket> {
        let t: Ticket = serde_json::from_str(jar.get(LOGIN_COOKIE)?.value()).ok()?;
        let now = unix_now();
        (t.at <= now + 60 && now.saturating_sub(t.at) < LOGIN_TTL.as_secs()).then_some(t)
    }

    /// A new sign-in with Discord: its login cookie, and Discord's page to send the browser to.
    fn begin(&self, d: &Discord, cli: Option<CliLogin>) -> Result<(Cookie<'static>, String), String> {
        let verifier = PkceCodeVerifier::new(chipsoc_client::remote::random_hex(32));
        let t = Ticket {
            state: CsrfToken::new_random().secret().clone(),
            verifier: verifier.secret().clone(),
            at: unix_now(),
            cli,
        };
        self.sealed(d, &t)
    }

    /// `t` as the login cookie, and Discord's page for it.
    fn sealed(&self, d: &Discord, t: &Ticket) -> Result<(Cookie<'static>, String), String> {
        let challenge = PkceCodeChallenge::from_code_verifier_sha256(&PkceCodeVerifier::new(t.verifier.clone()));
        let url = d.authorize_url(&CsrfToken::new(t.state.clone()), challenge).map_err(|e| {
            tracing::warn!("making Discord's sign-in link failed: {e:#}");
            "Couldn't make the link to Discord; check WEB_URL.".to_owned()
        })?;
        let body = serde_json::to_string(t).unwrap_or_default();
        Ok((self.cookie(LOGIN_COOKIE, body, "/auth", LOGIN_TTL), url))
    }

    /// The login cookie and Discord's page for this browser's sign-in: the
    /// one it started, if it has a while to go, so reloading keeps it.
    fn pending(&self, d: &Discord, jar: &PrivateCookieJar) -> Result<(Cookie<'static>, String), String> {
        if let Some(t) = Self::ticket(jar)
            && t.cli.is_none()
            && unix_now().saturating_sub(t.at) < LOGIN_TTL.as_secs() / 2
        {
            return self.sealed(d, &t);
        }
        self.begin(d, None)
    }

    /// Finish signing in with Discord: who, and the CLI that asked if one did; or why not.
    async fn finish(
        &self,
        d: &Discord,
        jar: &PrivateCookieJar,
        q: Callback,
    ) -> Result<(User, Option<CliLogin>), (StatusCode, String)> {
        let Some(t) = Self::ticket(jar) else {
            return Err((StatusCode::FORBIDDEN, "This sign-in expired or was started in another browser.".into()));
        };
        if !q.state.as_deref().is_some_and(|s| same(s, &t.state)) {
            tracing::warn!("refused a Discord sign-in whose state didn't match");
            return Err((StatusCode::FORBIDDEN, "This sign-in didn't start on this page, so it was refused.".into()));
        }
        if q.error.is_some() {
            return Err((StatusCode::UNAUTHORIZED, "Signing in with Discord was cancelled.".into()));
        }
        let Some(code) = q.code else {
            return Err((StatusCode::BAD_REQUEST, "Discord didn't send a sign-in code.".into()));
        };
        let user = d.user(code, PkceCodeVerifier::new(t.verifier)).await.map_err(|e| {
            tracing::warn!("Discord sign-in failed: {e:#}");
            (StatusCode::BAD_GATEWAY, "Discord didn't confirm the sign-in.".to_owned())
        })?;
        match self.members.may_use(user.id).await {
            Ok(true) => {
                let to = if t.cli.is_some() { "the command line (to confirm)" } else { "the web UI" };
                tracing::info!(user = %user.name, id = %user.id, "signed in to {to} with Discord");
                Ok((user, t.cli))
            }
            Ok(false) => {
                tracing::info!(user = %user.name, id = %user.id, "refused web UI sign-in: not an admin");
                let who = self.members.who_may_use().await;
                Err((
                    StatusCode::FORBIDDEN,
                    format!(
                        "{NOT_COMMITTEE} {}. Only {who} can use this. \
                         Wrong account? Sign out of Discord in this browser first.",
                        user.name
                    ),
                ))
            }
            Err(e) => {
                tracing::warn!(user = %user.name, "checking web UI access with Discord failed: {e:#}");
                Err((StatusCode::BAD_GATEWAY, "Couldn't check your roles with Discord.".into()))
            }
        }
    }

    /// A cookie for this way of signing in. Discord's redirect back is a
    /// cross-site navigation, so those cookies are Lax (CSRF tokens still guard POSTs).
    fn cookie(&self, name: &'static str, value: String, path: &'static str, max_age: Duration) -> Cookie<'static> {
        let (same_site, secure) = match &self.sign_in {
            SignIn::Discord(d) => (SameSite::Lax, d.secure()),
            _ => (SameSite::Strict, false),
        };
        Cookie::build((name, value))
            .path(path)
            .http_only(true)
            .same_site(same_site)
            .secure(secure)
            .max_age(cookie::time::Duration::seconds(max_age.as_secs() as i64))
            .build()
    }

    /// The cookie that removes `name`'s.
    fn removal(&self, name: &'static str, path: &'static str) -> Cookie<'static> {
        self.cookie(name, String::new(), path, Duration::ZERO)
    }
}

/// How the refusal for someone who isn't on the committee starts.
const NOT_COMMITTEE: &str = "You're signed in to Discord as";

/// Why a request was turned away.
enum Refused {
    NotLocal,
    SignIn,
    TooLarge,
    Expired,
    Revoked,
    Unchecked,
}

impl IntoResponse for Refused {
    fn into_response(self) -> Response {
        let (status, text) = match self {
            Self::SignIn => return Redirect::to("/login").into_response(),
            Self::NotLocal => (StatusCode::FORBIDDEN, "Open this page through localhost."),
            Self::TooLarge => (StatusCode::PAYLOAD_TOO_LARGE, "That form is too large."),
            Self::Expired => (StatusCode::FORBIDDEN, "This form has expired. Go back, reload the page and try again."),
            Self::Revoked => {
                let text = "Your Discord account no longer has access, so nothing was changed. \
                            Sign in again if your roles have changed.";
                return (StatusCode::FORBIDDEN, problem_page(text)).into_response();
            }
            Self::Unchecked => (
                StatusCode::SERVICE_UNAVAILABLE,
                "Couldn't check your access with Discord just now, so nothing was changed. Try again in a moment.",
            ),
        };
        (status, text).into_response()
    }
}

impl Refused {
    /// The same, as a page with the navigation bar, for someone signed in
    /// whose form didn't go through: what they typed is still in the page
    /// they came from.
    fn page(self, s: &Session) -> Response {
        let (status, text) = match self {
            Self::Expired => (
                StatusCode::FORBIDDEN,
                "Not saved: this page was open too long. Go back, reload and try again (copy any long text first).",
            ),
            Self::Unchecked => (
                StatusCode::SERVICE_UNAVAILABLE,
                "Not saved: couldn't check your access with Discord just now. Go back and try again in a moment.",
            ),
            Self::TooLarge => (StatusCode::PAYLOAD_TOO_LARGE, "Not saved: that form is too large."),
            other => return other.into_response(),
        };
        let content = rsx! {
            h1 { "Not saved" }
            {view::trouble(text)}
        };
        (status, view::frame(None, "Not saved", "/", s, content)).into_response()
    }

    /// The same, as JSON for the API.
    fn api(self) -> Response {
        let (status, text) = match self {
            Self::Revoked => (
                StatusCode::FORBIDDEN,
                "Your Discord account no longer has access. Sign in again if your roles have changed.",
            ),
            Self::Unchecked => (
                StatusCode::SERVICE_UNAVAILABLE,
                "Couldn't check your access with Discord just now. Try again in a moment.",
            ),
            _ => (StatusCode::UNAUTHORIZED, "Not signed in."),
        };
        failure(status, text).into_response()
    }
}

/// `WEB_TOKEN`, if set; too short a code is an error rather than a weak lock.
pub(crate) fn web_token() -> Result<Option<String>> {
    let code = match std::env::var("WEB_TOKEN") {
        Err(std::env::VarError::NotPresent) => return Ok(None),
        Err(e) => bail!("WEB_TOKEN: {e}"),
        Ok(t) => t.trim().to_owned(),
    };
    if code.chars().count() < MIN_SECRET {
        bail!("WEB_TOKEN is too short; use at least {MIN_SECRET} characters");
    }
    Ok(Some(code))
}

/// Off loopback, only Discord sign-in over https. Without sign-in anyone could
/// use it; the shared code has no names and no revoking, and its cookie
/// isn't marked https-only, so it's kept to this machine (an https proxy here
/// can still publish it).
pub(crate) fn check_bind(addr: SocketAddr, sign_in: &SignIn) -> Result<()> {
    if addr.ip().is_loopback() {
        return Ok(());
    }
    match sign_in {
        SignIn::Open => {
            bail!("set up sign-in with Discord before listening on {addr}; without it only 127.0.0.1 is allowed")
        }
        SignIn::Code(_) => bail!(
            "WEB_TOKEN sign-in is for trying the UI out and only listens on 127.0.0.1; \
             sign in with Discord (DISCORD_CLIENT_SECRET and an https WEB_URL) to listen on {addr}"
        ),
        SignIn::Discord(d) if !d.secure() => bail!(
            "WEB_URL is plain http, so sign-in would cross the network unencrypted; \
             use an https WEB_URL or listen on 127.0.0.1 instead of {addr}"
        ),
        SignIn::Discord(_) => Ok(()),
    }
}

/// In front of every page: needs a session, and a matching CSRF token on POSTs.
/// Discord users also need Discord to still agree they may (asked again
/// every [`RECHECK`]), to see pages as well as to change things.
pub(crate) async fn guard(State(auth): State<Arc<Auth>>, req: Request, next: Next) -> Response {
    let (id, session, new) = match auth.resolve(req.headers()).await {
        Ok(found) => found,
        Err(refused) => return refused.into_response(),
    };
    let mut req = if req.method() == Method::POST {
        match csrf_checked(req, &session).await {
            Ok(req) => req,
            Err(refused) => return refused.page(&session),
        }
    } else {
        req
    };
    if req.uri().path() != "/logout"
        && let Err(refused) = auth.recheck(&id, &session).await
    {
        return refused.page(&session);
    }
    req.extensions_mut().insert(session);
    let res = next.run(req).await;
    if new {
        let jar = auth.jar(&HeaderMap::new()).await.add(auth.cookie(COOKIE_NAME, id, "/", SESSION_TTL));
        return (jar, res).into_response();
    }
    res
}

/// In front of the API: a live bearer token, whose owner Discord still says may
/// use it (asked again every [`RECHECK`]). No cookies, so no CSRF token either.
pub(crate) async fn api_guard(State(auth): State<Arc<Auth>>, mut req: Request, next: Next) -> Response {
    let Some(token) = bearer(req.headers()).map(str::to_owned) else {
        let text = "Not signed in: run `chipsoc-bot login --server <this server>`.";
        return failure(StatusCode::UNAUTHORIZED, text).into_response();
    };
    let Some(session) = auth.lookup(&token, true).await else {
        return failure(StatusCode::UNAUTHORIZED, "This sign-in has expired or was signed out.").into_response();
    };
    if let Err(refused) = auth.recheck(&token, &session).await {
        return refused.api();
    }
    req.extensions_mut().insert(session);
    next.run(req).await
}

/// `POST /api/v1/logout`: the token stops working.
pub(crate) async fn api_logout(State(auth): State<Arc<Auth>>, headers: HeaderMap) -> StatusCode {
    if let Some(token) = bearer(&headers)
        && auth.lookup(token, true).await.is_some()
    {
        auth.forget(token).await;
    }
    StatusCode::NO_CONTENT
}

/// `GET /api/v1/tokens`: the caller's command-line sign-ins.
pub(crate) async fn api_tokens(
    State(auth): State<Arc<Auth>>,
    Extension(s): Extension<Session>,
    headers: HeaderMap,
) -> Json<Vec<TokenInfo>> {
    let tokens = match &s.user {
        Some(u) => auth.tokens_of(u, bearer(&headers)).await,
        None => Vec::new(),
    };
    Json(tokens)
}

/// `DELETE /api/v1/tokens/{id}`: revoke one of the caller's own sign-ins.
pub(crate) async fn api_revoke(
    State(auth): State<Arc<Auth>>,
    Extension(s): Extension<Session>,
    Path(id): Path<String>,
) -> Response {
    match &s.user {
        Some(user) if auth.revoke(user, &id).await => StatusCode::NO_CONTENT.into_response(),
        _ => failure(StatusCode::NOT_FOUND, "You have no sign-in with that id.").into_response(),
    }
}

/// `/account`: the visitor's command-line sign-ins, each with a Revoke button.
pub(crate) async fn account(State(auth): State<Arc<Auth>>, Extension(s): Extension<Session>) -> Html<String> {
    account_page(&auth, &s).await
}

#[derive(Deserialize)]
pub(crate) struct RevokeForm {
    id: String,
}

/// `POST /account/revoke`.
pub(crate) async fn account_revoke(
    State(auth): State<Arc<Auth>>,
    Extension(s): Extension<Session>,
    Form(f): Form<RevokeForm>,
) -> Response {
    let done = match &s.user {
        Some(u) => auth.revoke(u, &f.id).await,
        None => false,
    };
    let note = if done { "Revoked: that command line is signed out." } else { "That sign-in was already gone." };
    crate::flash::redirect("/account", note, done)
}

async fn account_page(auth: &Auth, s: &Session) -> Html<String> {
    let tokens = match &s.user {
        Some(u) => auth.tokens_of(u, None).await,
        None => Vec::new(),
    };
    let content = rsx! {
        h1 { "Your sign-ins" }
        section { class: "card",
            h2 { "Command line" }
            if s.user.is_none() {
                p { "Only people signed in with Discord have command-line sign-ins." }
            } else if tokens.is_empty() {
                p {
                    "None. "
                    code { "chipsoc-bot login" }
                    " adds one, after you confirm it here."
                }
            } else {
                p { class: "hint", "Each lasts 30 days. Revoke any you don't recognise, and tell the rest of the committee." }
                {view::table(&["Device", "Signed in", "Expires", ""], rsx! {
                    for t in tokens {
                        tr {
                            td { class: "wrap", "{t.name}" }
                            td { "{t.created}" }
                            td { "{t.expires}" }
                            td {
                                form { method: "post", action: "/account/revoke",
                                    {view::csrf(s)}
                                    input { r#type: "hidden", name: "id", value: "{t.id}" }
                                    button { class: "danger", "Revoke" }
                                }
                            }
                        }
                    }
                })}
            }
        }
    };
    view::frame(None, "Sign-ins", "/account", s, content)
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    let v = headers.get(AUTHORIZATION)?.to_str().ok()?;
    v.strip_prefix("Bearer ").map(str::trim).filter(|t| !t.is_empty())
}

/// The request again, if its form carries this session's CSRF token.
async fn csrf_checked(req: Request, session: &Session) -> Result<Request, Refused> {
    let (parts, body) = req.into_parts();
    let bytes = to_bytes(body, MAX_FORM).await.map_err(|_| Refused::TooLarge)?;
    let fields: Vec<(String, String)> = serde_urlencoded::from_bytes(&bytes).unwrap_or_default();
    let ok = fields.iter().any(|(k, v)| k == "csrf" && same(v, &session.csrf));
    if !ok {
        return Err(Refused::Expired);
    }
    Ok(Request::from_parts(parts, Body::from(bytes)))
}

pub(crate) async fn login_page(State(auth): State<Arc<Auth>>, headers: HeaderMap) -> Response {
    match &auth.sign_in {
        SignIn::Open => Redirect::to("/").into_response(),
        SignIn::Code(_) => login_form(None).into_response(),
        SignIn::Discord(_) if auth.find(&headers).await.is_some() => Redirect::to("/").into_response(),
        SignIn::Discord(d) => {
            let jar = auth.jar(&headers).await;
            match auth.pending(d, &jar) {
                Ok((cookie, url)) => (jar.add(cookie), discord_form(&url)).into_response(),
                Err(why) => (StatusCode::INTERNAL_SERVER_ERROR, problem_page(&why)).into_response(),
            }
        }
    }
}

#[derive(Deserialize)]
pub(crate) struct Login {
    code: String,
}

pub(crate) async fn login(State(auth): State<Arc<Auth>>, Form(f): Form<Login>) -> Response {
    let secret = match &auth.sign_in {
        SignIn::Code(secret) => secret,
        SignIn::Open => return Redirect::to("/").into_response(),
        SignIn::Discord(_) => return Redirect::to("/login").into_response(),
    };
    let _turn = auth.attempts.lock().await;
    if !same(f.code.trim(), secret) {
        tokio::time::sleep(FAILED_LOGIN_DELAY).await;
        tracing::info!("refused a wrong WEB_TOKEN code");
        let res = login_form(Some("That code isn't right. Check it and try again."));
        return (StatusCode::UNAUTHORIZED, res).into_response();
    }
    tracing::info!(actor = SHARED, "signed in with the shared WEB_TOKEN code");
    let (id, _) = auth.start(true, None).await;
    let jar = auth.jar(&HeaderMap::new()).await.add(auth.cookie(COOKIE_NAME, id, "/", SESSION_TTL));
    (jar, Redirect::to("/")).into_response()
}

/// What `login` in the CLI opens.
#[derive(Deserialize)]
pub(crate) struct CliStart {
    port: Option<u16>,
    state: String,
    /// SHA-256 (hex) of the CLI's secret verifier.
    challenge: Option<String>,
    /// The CLI's computer, as it names itself.
    device: Option<String>,
}

/// A device name from the CLI, kept to plain characters: it's shown on pages
/// and in the bot log, and whoever made the link chose it.
fn device_name(raw: &str) -> Option<String> {
    let kept: String = raw
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_' | ' '))
        .take(MAX_DEVICE)
        .collect();
    let kept = kept.trim();
    (!kept.is_empty()).then(|| kept.to_owned())
}

/// `/auth/cli`: sign in with Discord, confirm here that it's your terminal,
/// then hand the CLI an API token.
pub(crate) async fn cli_login(
    State(auth): State<Arc<Auth>>,
    headers: HeaderMap,
    Query(q): Query<CliStart>,
) -> Response {
    let SignIn::Discord(d) = &auth.sign_in else {
        let text = "This server doesn't sign in with Discord, so the command line can't sign in to it.";
        return (StatusCode::NOT_FOUND, problem_page(text)).into_response();
    };
    let state_ok = (16..=128).contains(&q.state.len())
        && q.state.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    // PKCE S256: 32 bytes of SHA-256, base64url without padding.
    let s256 = |c: &String| c.len() == 43 && c.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    let Some(challenge) = q.challenge.filter(s256) else {
        let text = "This sign-in link is from an older version of the command line; update it and run login again.";
        return (StatusCode::BAD_REQUEST, problem_page(text)).into_response();
    };
    if q.port == Some(0) || !state_ok {
        return (StatusCode::BAD_REQUEST, problem_page("This sign-in link is broken; run login again."))
            .into_response();
    }
    let device = q.device.as_deref().and_then(device_name);
    match auth.begin(d, Some(CliLogin { port: q.port, state: q.state, challenge, device })) {
        Ok((cookie, url)) => (auth.jar(&headers).await.add(cookie), Redirect::to(&url)).into_response(),
        Err(why) => (StatusCode::INTERNAL_SERVER_ERROR, problem_page(&why)).into_response(),
    }
}

/// After Discord: ask whether it's really their terminal before any code is made.
fn ask_to_confirm(auth: &Auth, user: User, cli: CliLogin) -> Response {
    let id = random_token();
    let page = confirm_page(&id, &user, &cli, None);
    let mut all = lock(&auth.confirms);
    make_room(&mut all, MAX_PENDING, |c| c.expires);
    all.insert(id, Confirm { user, cli, expires: Instant::now() + LOGIN_TTL });
    // Continue redirects to the CLI on 127.0.0.1; browsers hold a form's
    // redirect to its page's form-action, so this page allows exactly that.
    let csp = crate::CSP.replace("form-action 'self'", "form-action 'self' http://127.0.0.1:*");
    let csp = HeaderValue::from_str(&csp).expect("the policy is plain ASCII");
    ([(CONTENT_SECURITY_POLICY, csp)], page).into_response()
}

/// What the confirmation page sends.
#[derive(Deserialize)]
pub(crate) struct Confirmed {
    id: String,
    cancel: Option<String>,
}

/// `POST /auth/cli/confirm`: Continue hands the CLI its one-time code.
///
/// No code to type: the one-time code goes to `127.0.0.1` on the machine
/// running this browser, and only trades for a token with the PKCE verifier
/// the CLI that started the sign-in holds. So a link someone else started
/// signs nobody in for them, unless they talk you into sending them the
/// address the browser lands on. `--paste` mode shows the code instead, with
/// a warning. The page's random id is the only way in, so another site can't
/// post this.
pub(crate) async fn cli_confirm(State(auth): State<Arc<Auth>>, Form(f): Form<Confirmed>) -> Response {
    let mut all = lock(&auth.confirms);
    let Some(_) = all.get(&f.id).filter(|c| c.expires > Instant::now()) else {
        drop(all);
        let text = "This sign-in expired or was already used; run login again in your terminal.";
        return (StatusCode::FORBIDDEN, problem_page(text)).into_response();
    };
    if f.cancel.is_some() {
        let c = all.remove(&f.id);
        drop(all);
        if let Some(c) = c {
            tracing::warn!(user = %c.user.name, "a command-line sign-in was cancelled at the confirmation step");
        }
        return (StatusCode::OK, problem_page("Cancelled: nothing was signed in.")).into_response();
    }
    let Some(c) = all.remove(&f.id) else { return StatusCode::FORBIDDEN.into_response() };
    drop(all);
    to_cli(&auth, c.user, c.cli)
}

/// Why the terminal says signing in failed, for [`cli_failed`]: a fixed
/// list, so a link can't put words on this server's page.
#[derive(Deserialize)]
pub(crate) struct Failed {
    why: Option<String>,
}

/// `/auth/cli/done`: where the terminal sends the browser once it's signed in.
pub(crate) async fn cli_done() -> Html<String> {
    view::bare(
        "Signed in",
        rsx! {
            h1 { "✓ Signed in" }
            section { class: "card",
                p { "You can close this tab and go back to the terminal." }
            }
        },
    )
}

/// `/auth/cli/failed`: where the terminal sends the browser when signing in didn't finish.
pub(crate) async fn cli_failed(Query(f): Query<Failed>) -> Html<String> {
    let text = match f.why.as_deref() {
        Some("expired") => "The sign-in code expired or was already used.",
        Some("refused") => "The dashboard refused this sign-in.",
        _ => "Signing in didn't finish.",
    };
    cli_problem_page(text)
}

/// A command-line sign-in didn't work: why, and what to do next.
fn cli_problem_page(text: &str) -> Html<String> {
    view::bare(
        "Not signed in",
        rsx! {
            h1 { "Not signed in" }
            {view::trouble(text)}
            section { class: "card",
                p { "Go back to the terminal and run " code { "chipsoc-bot login" } " again." }
            }
        },
    )
}

/// A one-time code for the CLI: to its listener, or on a page to paste from.
/// The CLI trades it, with its verifier, for a token at `/auth/cli/token`.
fn to_cli(auth: &Auth, user: User, cli: CliLogin) -> Response {
    let code = random_token();
    {
        let mut all = lock(&auth.codes);
        make_room(&mut all, MAX_PENDING, |c| c.expires);
        let expires = Instant::now() + CODE_TTL;
        all.insert(code.clone(), Code { user, challenge: cli.challenge, device: cli.device, expires });
    }
    let Some(port) = cli.port else { return paste_page(&code).into_response() };
    let query = serde_urlencoded::to_string([("code", &code), ("state", &cli.state)]).unwrap_or_default();
    // Always this machine's loopback, whatever else the request said.
    Redirect::to(&format!("http://127.0.0.1:{port}/callback?{query}")).into_response()
}

/// What the CLI sends to trade its one-time code for a token.
#[derive(Deserialize)]
pub(crate) struct Trade {
    code: String,
    verifier: String,
}

/// `POST /auth/cli/token`: a one-time code and the verifier whose hash the
/// sign-in started with, for an API token. A wrong verifier uses the code up.
/// The bot log hears of every new command-line sign-in.
pub(crate) async fn cli_token(State(auth): State<Arc<Auth>>, Json(t): Json<Trade>) -> Response {
    let found = lock(&auth.codes).remove(&t.code);
    let Some(code) = found.filter(|c| c.expires > Instant::now()) else {
        return failure(StatusCode::UNAUTHORIZED, "That sign-in code has expired or was used; run login again.")
            .into_response();
    };
    // RFC 7636 verifiers are 43 to 128 characters; anything else matches nothing.
    let fits = (43..=128).contains(&t.verifier.len());
    let made = fits.then(|| PkceCodeChallenge::from_code_verifier_sha256(&PkceCodeVerifier::new(t.verifier.clone())));
    if !made.is_some_and(|c| same(c.as_str(), &code.challenge)) {
        tracing::warn!(user = %code.user.name, "refused a CLI sign-in code sent without its verifier");
        return failure(StatusCode::UNAUTHORIZED, "That sign-in code isn't yours; run login again.").into_response();
    }
    let device = code.device.unwrap_or_else(|| "an unnamed device".into());
    let name = format!("{device} · {}", Utc::now().format("%Y-%m-%d"));
    tracing::info!(user = %code.user.name, sign_in = %name, "new command-line sign-in");
    let text = format!(
        "🔑 <@{}> signed in the command line on `{device}`. If that wasn't them, they can revoke it \
         under Sign-ins on the web UI.",
        code.user.id
    );
    let token = auth.mint(code.user, name).await;
    if tokio::time::timeout(Duration::from_secs(5), auth.members.note(text)).await.is_err() {
        tracing::warn!("telling the bot log about a command-line sign-in took too long");
    }
    Json(crate::remote::Token { token }).into_response()
}

/// What Discord's redirect back carries.
#[derive(Deserialize)]
pub(crate) struct Callback {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

/// `/auth/discord`, where Discord sends people after they agree to sign in.
pub(crate) async fn discord_callback(
    State(auth): State<Arc<Auth>>,
    headers: HeaderMap,
    Query(q): Query<Callback>,
) -> Response {
    let SignIn::Discord(d) = &auth.sign_in else { return Redirect::to("/login").into_response() };
    let jar = auth.jar(&headers).await;
    let finished = auth.finish(d, &jar, q).await;
    // The ticket is spent, whichever way it went.
    let jar = jar.add(auth.removal(LOGIN_COOKIE, "/auth"));
    match finished {
        Ok((user, None)) => {
            let (id, _) = auth.start(true, Some(user)).await;
            (jar.add(auth.cookie(COOKIE_NAME, id, "/", SESSION_TTL)), Redirect::to("/")).into_response()
        }
        Ok((user, Some(cli))) => (jar, ask_to_confirm(&auth, user, cli)).into_response(),
        Err((status, text)) if text.starts_with(NOT_COMMITTEE) => {
            (jar, (status, problem_page_with(&text, "Sign in with another account"))).into_response()
        }
        Err((status, text)) => (jar, (status, problem_page(&text))).into_response(),
    }
}

pub(crate) async fn logout(State(auth): State<Arc<Auth>>, headers: HeaderMap) -> Response {
    auth.end(&headers).await;
    let jar = auth.jar(&headers).await.add(auth.removal(COOKIE_NAME, "/"));
    (jar, Redirect::to("/login")).into_response()
}

fn login_form(problem: Option<&str>) -> Html<String> {
    view::bare(
        "Sign in",
        rsx! {
            h1 { "Sign in" }
            if let Some(p) = problem {
                {view::trouble(p)}
            }
            form { method: "post", action: "/login", class: "card",
                label { r#for: "code", "Access code" }
                input {
                    id: "code",
                    r#type: "password",
                    name: "code",
                    autocomplete: "current-password",
                    required: true,
                    autofocus: true,
                }
                p { class: "hint", "Ask whoever set up the dashboard." }
                button { "Sign in" }
            }
        },
    )
}

fn discord_form(url: &str) -> Html<String> {
    view::bare(
        "Sign in",
        rsx! {
            h1 { "Sign in" }
            section { class: "card",
                p { "Use the Discord account you have on the server. This is for the committee." }
                a { class: "button", href: "{url}", "Sign in with Discord" }
                p { class: "hint", "Discord only shares your username with this page." }
            }
        },
    )
}

/// Before a CLI sign-in gets anything: whose, on which computer, and the
/// code its terminal shows, typed in to prove it's theirs.
fn confirm_page(id: &str, user: &User, cli: &CliLogin, problem: Option<&str>) -> Html<String> {
    let device = cli.device.as_deref().unwrap_or("a computer that didn't give its name");
    view::bare(
        "Let a terminal sign in?",
        rsx! {
            h1 { "Let a terminal sign in as you?" }
            if let Some(p) = problem {
                {view::trouble(p)}
            }
            form { method: "post", action: "/auth/cli/confirm", class: "card",
                p {
                    "The command-line tool on "
                    strong { "{device}" }
                    " would be signed in as "
                    strong { "{user.name}" }
                    ". It can do everything you can do here, for 30 days. Sign it out any time under Sign-ins."
                }
                p {
                    strong {
                        "Only continue if you just ran "
                        code { "chipsoc-bot login" }
                        " yourself."
                    }
                    " If someone sent you this link, close this page."
                }
                if cli.port.is_none() {
                    p { class: "hint", "The next page shows a code: paste it only into a terminal you started yourself." }
                }
                input { r#type: "hidden", name: "id", value: "{id}" }
                button { "Yes, sign it in" }
                button {
                    class: "quiet",
                    name: "cancel",
                    value: "yes",
                    formnovalidate: true,
                    "Cancel"
                }
            }
        },
    )
}

/// The CLI's one-time code, for `login --paste`.
fn paste_page(code: &str) -> Html<String> {
    view::bare(
        "Your sign-in code",
        rsx! {
            h1 { "Your sign-in code" }
            section { class: "card",
                p { "Paste this code into the terminal where you ran login, and nowhere else:" }
                p { code { "{code}" } }
                p { class: "hint", "Only paste this into a terminal you started yourself. It works once, for the next 5 minutes." }
            }
        },
    )
}

/// Signing in didn't work: why, and a way to try again.
fn problem_page(text: &str) -> Html<String> {
    problem_page_with(text, "Try again")
}

/// [`problem_page`] with its own label on the button back to signing in.
fn problem_page_with(text: &str, again: &str) -> Html<String> {
    view::bare(
        "Sign in",
        rsx! {
            h1 { "Not signed in" }
            {view::trouble(text)}
            a { class: "button", href: "/login", "{again}" }
        },
    )
}

/// Whether the browser asked for localhost, so DNS rebinding can't reach an unlocked UI.
fn loopback_host(headers: &HeaderMap) -> bool {
    let Some(host) = headers.get(HOST).and_then(|h| h.to_str().ok()) else { return false };
    let name = match host.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or_default(),
        None => host.split(':').next().unwrap_or_default(),
    };
    name.eq_ignore_ascii_case("localhost") || name.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

pub(crate) fn random_token() -> String {
    chipsoc_client::remote::random_hex(32)
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn waiting_codes_are_bounded() {
        let mut all: HashMap<String, Instant> = HashMap::new();
        for n in 0..MAX_PENDING + 50 {
            make_room(&mut all, MAX_PENDING, |at| *at);
            all.insert(n.to_string(), Instant::now() + LOGIN_TTL + Duration::from_secs(n as u64));
        }
        assert_eq!(all.len(), MAX_PENDING, "can't fill memory");
        assert!(!all.contains_key("0") && all.contains_key(&(MAX_PENDING + 49).to_string()), "the oldest went");
        tokio::time::advance(LOGIN_TTL * 3).await;
        make_room(&mut all, MAX_PENDING, |at| *at);
        assert!(all.is_empty(), "expired ones go");
    }

    #[test]
    fn device_names_are_kept_plain() {
        assert_eq!(device_name("ada-laptop.local").as_deref(), Some("ada-laptop.local"));
        assert_eq!(device_name("  `<@&1>` [x](https://evil)\n").as_deref(), Some("1 xhttpsevil"));
        assert_eq!(device_name("***").as_deref(), None);
        assert_eq!(device_name(&"a".repeat(500)).map(|d| d.len()), Some(MAX_DEVICE));
    }
}

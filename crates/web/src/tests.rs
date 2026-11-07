//! The routes, driven like a browser would, against a fake Discord and a temp config.

use std::collections::HashMap;
use std::fs;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use anyhow::Result;
use axum::body::{Body, to_bytes};
use axum::http::header::{AUTHORIZATION, CONTENT_TYPE, COOKIE, HOST, LOCATION, SET_COOKIE};
use axum::http::{Request, StatusCode, request};
use chipsoc_discord::fake::FakeDiscord;
use chipsoc_model::Plan;
use serde_json::{Value, json};
use tower::ServiceExt;

use super::*;
use crate::oauth::OAuth;
use crate::testing::*;

const CODE: &str = "correct horse battery";

/// The fake OAuth, noting the codes traded (and their PKCE verifiers) so
/// tests can see none were.
struct Recorder {
    oauth: FakeOAuth,
    traded: StdMutex<Vec<String>>,
    verifiers: StdMutex<Vec<String>>,
}

impl OAuth for Arc<Recorder> {
    fn send(&self, req: oauth::HttpRequest) -> oauth::BoxFuture<'_, Result<oauth::HttpResponse>> {
        let form = token_form(&req);
        if let Some(code) = form.get("code") {
            self.traded.lock().unwrap().push(code.clone());
            self.verifiers.lock().unwrap().push(form.get("code_verifier").cloned().unwrap_or_default());
        }
        self.oauth.send(req)
    }
}

/// A database beside the config, hidden so config edits leave it alone.
fn db_url(dir: &std::path::Path) -> String {
    format!("sqlite:{}?mode=rwc", dir.join(".bot.db").display())
}

fn fixture() -> tempfile::TempDir {
    let d = tempfile::tempdir().unwrap();
    write_config(d.path());
    d
}

/// Keeps cookies between requests, like a browser.
struct Browser {
    app: Router,
    discord: Arc<FakeDiscord>,
    cookies: HashMap<String, String>,
    dir: tempfile::TempDir,
    oauth: Option<Arc<Recorder>>,
}

struct Page {
    status: StatusCode,
    location: Option<String>,
    set_cookie: Option<String>,
    csp: Option<String>,
    body: String,
}

impl Browser {
    fn new(secret: Option<&str>) -> Self {
        let sign_in = secret.map_or(SignIn::Open, |s| SignIn::Code(s.into()));
        Self::with(fixture(), sign_in, None)
    }

    fn with(dir: tempfile::TempDir, sign_in: SignIn, oauth: Option<Arc<Recorder>>) -> Self {
        Self::on(dir, Arc::new(FakeDiscord::new()), sign_in, oauth)
    }

    fn on(dir: tempfile::TempDir, discord: Arc<FakeDiscord>, sign_in: SignIn, oauth: Option<Arc<Recorder>>) -> Self {
        let engine = Engine::load(discord.clone(), dir.path()).unwrap();
        Self { app: router(engine, db_url(dir.path()), sign_in), discord, cookies: HashMap::new(), dir, oauth }
    }

    /// The bot's database, as the server sees it.
    async fn store(&self) -> chipsoc_store::Store {
        chipsoc_store::Store::open(&db_url(self.dir.path())).await.unwrap()
    }

    /// Signing in with Discord, on the [`committee`]'s server.
    fn discord() -> Self {
        let dir = fixture();
        let discord = Arc::new(FakeDiscord::new());
        let oauth = Arc::new(Recorder {
            oauth: committee(dir.path(), &discord),
            traded: Default::default(),
            verifiers: Default::default(),
        });
        Self::on(dir, discord, discord_sign_in(Box::new(oauth.clone())), Some(oauth))
    }

    /// Start signing in with Discord: the `state` the login page sends there.
    async fn start_discord(&mut self) -> String {
        let page = self.get("/login").await;
        let start = "href=\"https://discord.com/oauth2/authorize?";
        let at = page.body.find(start).unwrap_or_else(|| panic!("no Discord link in {}", page.body)) + start.len();
        let query = unescape(&page.body[at..at + page.body[at..].find('"').unwrap()]);
        let fields: HashMap<String, String> = serde_urlencoded::from_str(&query).unwrap();
        assert_eq!(fields["scope"], "identify");
        assert_eq!(fields["client_id"], "777");
        assert_eq!(fields["redirect_uri"], "https://ui.example/auth/discord");
        fields["state"].clone()
    }

    /// The whole round trip through Discord, as `code`'s user.
    async fn sign_in_with_discord(&mut self, code: &str) -> Page {
        let state = self.start_discord().await;
        self.get(&format!("/auth/discord?code={code}&state={state}")).await
    }

    /// Start a CLI sign-in and come back from Discord as `code`'s user: the
    /// page asking to confirm it (or why not).
    async fn cli_discord(&mut self, start: &str, code: &str) -> Page {
        let to_discord = self.get(start).await.location.expect("sent to Discord");
        let state = &query(&to_discord)["state"];
        self.get(&format!("/auth/discord?code={code}&state={state}")).await
    }

    /// Sign in the CLI's way, as `code`'s user, pressing Continue: where the
    /// token is sent.
    async fn cli_sign_in(&mut self, start: &str, code: &str) -> Page {
        let page = self.cli_discord(start, code).await;
        if page.status != StatusCode::OK {
            return page;
        }
        self.post("/auth/cli/confirm", &[("id", &field(&page.body, "id"))]).await
    }

    /// A CLI token for `code`'s user, checking its one-time code comes back
    /// to the CLI's listener and is traded with the CLI's verifier.
    async fn token(&mut self, code: &str) -> String {
        let page = self.cli_sign_in(&cli_start(Some(4567)), code).await;
        let to = page.location.unwrap_or_else(|| panic!("not sent back: {}", page.body));
        let fields = query(to.strip_prefix("http://127.0.0.1:4567/callback").expect("to the CLI's listener"));
        assert_eq!(fields["state"], CLI_STATE);
        assert!(!fields.contains_key("token"), "only a code goes through the browser");
        let (status, body) = self.trade(&fields["code"], CLI_VERIFIER).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        body["token"].as_str().unwrap().to_owned()
    }

    /// Trade a CLI sign-in's one-time code for a token.
    async fn trade(&self, code: &str, verifier: &str) -> (StatusCode, Value) {
        self.api("POST", "/auth/cli/token", None, Some(json!({ "code": code, "verifier": verifier }))).await
    }

    /// An API call, with no cookies.
    async fn api(&self, method: &str, path: &str, token: Option<&str>, body: Option<Value>) -> (StatusCode, Value) {
        let mut req = Request::builder().method(method).uri(path).header(HOST, "127.0.0.1:8080");
        if let Some(t) = token {
            req = req.header(AUTHORIZATION, format!("Bearer {t}"));
        }
        if body.is_some() {
            req = req.header(CONTENT_TYPE, "application/json");
        }
        let body = body.map_or_else(Body::empty, |v| Body::from(v.to_string()));
        let res = self.app.clone().oneshot(req.body(body).unwrap()).await.unwrap();
        let status = res.status();
        let bytes = to_bytes(res.into_body(), usize::MAX).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }

    fn traded(&self) -> Vec<String> {
        self.oauth.as_ref().unwrap().traded.lock().unwrap().clone()
    }

    fn set_roles(&self, user: u64, roles: &[&str]) {
        let mut st = self.discord.state.lock().unwrap();
        let m = st.members.iter_mut().find(|m| m["user"]["id"].as_str().and_then(|s| s.parse().ok()) == Some(user));
        let m = m.unwrap();
        m["roles"] = json!(roles);
    }

    async fn get(&mut self, path: &str) -> Page {
        self.send(Request::get(path), Body::empty()).await
    }

    async fn post(&mut self, path: &str, fields: &[(&str, &str)]) -> Page {
        let form = serde_urlencoded::to_string(fields).unwrap();
        let req = Request::post(path).header(CONTENT_TYPE, "application/x-www-form-urlencoded");
        self.send(req, Body::from(form)).await
    }

    /// Post a form the way a page offers it: with the session's CSRF token.
    async fn submit(&mut self, path: &str, fields: &[(&str, &str)]) -> Page {
        let csrf = field(&self.get("/changes").await.body, "csrf");
        let mut all = vec![("csrf", csrf.as_str())];
        all.extend_from_slice(fields);
        self.post(path, &all).await
    }

    async fn send(&mut self, req: request::Builder, body: Body) -> Page {
        let mut req = req.header(HOST, "127.0.0.1:8080");
        if !self.cookies.is_empty() {
            let jar: Vec<String> = self.cookies.iter().map(|(k, v)| format!("{k}={v}")).collect();
            req = req.header(COOKIE, jar.join("; "));
        }
        let res = self.app.clone().oneshot(req.body(body).unwrap()).await.unwrap();
        let location = res.headers().get(LOCATION).map(|v| v.to_str().unwrap().to_owned());
        let set: Vec<String> =
            res.headers().get_all(SET_COOKIE).iter().map(|v| v.to_str().unwrap().to_owned()).collect();
        for c in &set {
            let (name, value) = c.split(';').next().unwrap().split_once('=').unwrap();
            if value.is_empty() || c.contains("Max-Age=0") {
                self.cookies.remove(name);
            } else {
                self.cookies.insert(name.into(), value.into());
            }
        }
        let set_cookie = (!set.is_empty()).then(|| set.join("\n"));
        let csp = res.headers().get("content-security-policy").map(|v| v.to_str().unwrap().to_owned());
        let status = res.status();
        let body = String::from_utf8(to_bytes(res.into_body(), usize::MAX).await.unwrap().to_vec()).unwrap();
        Page { status, location, set_cookie, csp, body }
    }

    fn projects_toml(&self) -> String {
        fs::read_to_string(self.dir.path().join("projects.toml")).unwrap()
    }
}

const CLI_STATE: &str = "state-from-the-cli-0123";
/// The CLI's secret; the sign-in link carries only its hash.
const CLI_VERIFIER: &str = "the-cli-keeps-this-to-itself-and-it-is-long-enough-for-pkce";

/// The PKCE S256 challenge for `verifier`.
fn s256(verifier: &str) -> String {
    oauth2::PkceCodeChallenge::from_code_verifier_sha256(&oauth2::PkceCodeVerifier::new(verifier.into()))
        .as_str()
        .into()
}

/// Where the CLI starts signing in, with or without its listener's port.
fn cli_start(port: Option<u16>) -> String {
    let challenge = s256(CLI_VERIFIER);
    let port = port.map(|p| format!("port={p}&")).unwrap_or_default();
    format!("/auth/cli?{port}state={CLI_STATE}&challenge={challenge}")
}

/// The query fields of a URL.
fn query(url: &str) -> HashMap<String, String> {
    serde_urlencoded::from_str(url.split_once('?').map_or("", |(_, q)| q)).unwrap()
}

/// A form field's value from a rendered page.
fn field(html: &str, name: &str) -> String {
    let start = format!("name=\"{name}\" value=\"");
    let at = html.find(&start).unwrap_or_else(|| panic!("no field {name} in {html}")) + start.len();
    unescape(&html[at..at + html[at..].find('"').unwrap()])
}

/// Undo the `&#NN;` escapes the renderer writes.
fn unescape(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(i) = rest.find("&#") {
        out.push_str(&rest[..i]);
        let end = rest[i..].find(';').unwrap() + i;
        out.push(char::from_u32(rest[i + 2..end].parse().unwrap()).unwrap());
        rest = &rest[end + 1..];
    }
    out + rest
}

#[tokio::test]
async fn sign_in_is_required_when_a_code_is_set() {
    let mut b = Browser::new(Some(CODE));
    let page = b.get("/").await;
    assert_eq!((page.status, page.location.as_deref()), (StatusCode::SEE_OTHER, Some("/login")));
    assert_eq!(b.post("/projects/golem/status", &[("status", "past")]).await.location.as_deref(), Some("/login"));

    let wrong = b.post("/login", &[("code", "guess")]).await;
    assert_eq!(wrong.status, StatusCode::UNAUTHORIZED);
    assert!(wrong.body.contains("That code isn") && !wrong.body.contains(CODE));

    let ok = b.post("/login", &[("code", CODE)]).await;
    let cookie = ok.set_cookie.unwrap();
    assert!(cookie.contains("HttpOnly") && cookie.contains("SameSite=Strict"), "{cookie}");
    let home = b.get("/").await;
    assert_eq!(home.status, StatusCode::OK);
    assert!(home.body.contains("Test") && home.body.contains("Sign out"));

    let forged = b.post("/projects/golem/status", &[("status", "past")]).await;
    assert_eq!(forged.status, StatusCode::FORBIDDEN, "a POST without the CSRF token is refused");
    assert!(b.projects_toml().contains("status = \"active\""));

    let out = b.submit("/logout", &[]).await;
    assert_eq!(out.location.as_deref(), Some("/login"));
    assert_eq!(b.get("/").await.location.as_deref(), Some("/login"));
}

#[tokio::test]
async fn without_a_code_only_localhost_is_served() {
    let b = Browser::new(None);
    let req = Request::get("/").header(HOST, "evil.example:8080").body(Body::empty()).unwrap();
    assert_eq!(b.app.clone().oneshot(req).await.unwrap().status(), StatusCode::FORBIDDEN, "DNS rebinding");
    assert!(auth::check_bind("0.0.0.0:8080".parse().unwrap(), &SignIn::Open).is_err());
    assert!(auth::check_bind("127.0.0.1:8080".parse().unwrap(), &SignIn::Open).is_ok());
    let shared = SignIn::Code(CODE.into());
    assert!(auth::check_bind("0.0.0.0:8080".parse().unwrap(), &shared).is_err(), "the shared code stays local");
    assert!(auth::check_bind("127.0.0.1:8080".parse().unwrap(), &shared).is_ok());
}

#[tokio::test]
async fn overview_reports_config_problems() {
    let mut b = Browser::new(None);
    assert!(b.get("/").await.body.contains("All good"));
    write(b.dir.path(), "bot.toml", "name = \"Test\"\nadmin_role = \"ghost\"\n");
    let page = b.get("/").await;
    assert!(page.body.contains("Needs fixing") && page.body.contains("<code>ghost</code>"), "{}", page.body);
}

#[tokio::test]
async fn plan_page_lists_steps() {
    let mut b = Browser::new(None);
    let page = b.submit("/changes/plan", &[]).await;
    assert_eq!(page.status, StatusCode::OK);
    for want in ["channel <strong>general</strong>", "role <strong>Golem Team</strong>", "Apply 1", "name=\"plan_id\""]
    {
        assert!(page.body.contains(want), "missing {want:?} in {}", page.body);
    }
}

#[tokio::test]
async fn apply_runs_the_plan_shown() {
    let mut b = Browser::new(None);
    let page = b.submit("/changes/plan", &[]).await;
    let (plan, config) = (field(&page.body, "plan_id"), field(&page.body, "config"));
    let done = b.submit("/changes/apply", &[("plan_id", &plan), ("config", &config), ("confirm", "yes")]).await;
    assert!(done.body.contains("Done:") && done.body.contains("✓"), "{}", done.body);
    assert!(b.submit("/changes/plan", &[]).await.body.contains("already matches"));
    let again = b.submit("/changes/apply", &[("plan_id", &plan), ("config", &config), ("confirm", "yes")]).await;
    assert!(again.body.contains("Nothing was applied"), "a plan is used once: {}", again.body);
}

#[tokio::test]
async fn the_apply_form_carries_no_plan_to_forge() {
    let mut b = Browser::new(None);
    let page = b.submit("/changes/plan", &[]).await;
    assert!(!page.body.contains("name=\"plan\""), "{}", page.body);
    let config = field(&page.body, "config");
    let forged = serde_json::to_string(&Plan { delete: true, ..Plan::default() }).unwrap();
    let refused = b.submit("/changes/apply", &[("plan_id", &forged), ("config", &config), ("confirm", "yes")]).await;
    assert!(refused.body.contains("Nothing was applied"), "{}", refused.body);
    assert!(b.discord.take_calls().is_empty(), "nothing sent to Discord");
}

#[tokio::test]
async fn stale_plans_are_refused() {
    let mut b = Browser::new(None);
    let page = b.submit("/changes/plan", &[]).await;
    let (plan, config) = (field(&page.body, "plan_id"), field(&page.body, "config"));
    let form = [("plan_id", plan.as_str()), ("config", config.as_str()), ("confirm", "yes")];

    b.discord.state.lock().unwrap().guild.insert("name".into(), json!("Renamed in Discord"));
    let refused = b.submit("/changes/apply", &form).await;
    assert!(refused.body.contains("Nothing was applied") && refused.body.contains("changed"), "{}", refused.body);
    assert!(b.discord.take_calls().is_empty(), "nothing sent to Discord");

    // Config edited after looking: refused too, even though Discord didn't change.
    let page = b.submit("/changes/plan", &[]).await;
    let (plan, config) = (field(&page.body, "plan_id"), field(&page.body, "config"));
    b.submit("/projects/golem/status", &[("status", "past"), ("confirm", "yes")]).await;
    let refused = b.submit("/changes/apply", &[("plan_id", &plan), ("config", &config), ("confirm", "yes")]).await;
    assert!(refused.body.contains("config files changed"), "{}", refused.body);
    assert!(b.discord.take_calls().is_empty());
}

#[tokio::test]
async fn status_edit_writes_the_file() {
    let mut b = Browser::new(None);
    let res = b.submit("/projects/golem/status", &[("status", "past"), ("confirm", "yes")]).await;
    assert_eq!(res.location.as_deref(), Some("/projects?saved=golem#golem"));
    let file = b.projects_toml();
    assert!(file.contains("status = \"past\"") && file.starts_with("# Keep these"), "comments kept: {file}");
    assert!(b.get("/projects?saved=golem").await.body.contains("Saved Golem."));

    b.submit("/projects/golem/applications", &[("open", "true")]).await;
    assert!(b.projects_toml().contains("open = true"), "{}", b.projects_toml());
}

#[tokio::test]
async fn renaming_a_project_writes_the_new_slug() {
    let mut b = Browser::new(None);
    let bad = b.submit("/projects/golem/rename", &[("slug", "Not A Slug")]).await;
    assert!(bad.body.contains("lower-case letters"), "{}", bad.body);
    let res = b.submit("/projects/golem/rename", &[("slug", "robot")]).await;
    assert_eq!(res.location.as_deref(), Some("/projects?saved=robot#robot"));
    assert!(b.projects_toml().contains("slug = \"robot\""), "{}", b.projects_toml());
}

#[tokio::test]
async fn adding_a_project_writes_it_and_reports_clashes() {
    let mut b = Browser::new(None);
    let form = [("name", "Wyrm GPU"), ("blurb", "a GPU"), ("area", "gpu"), ("status", "proposed")];
    assert_eq!(b.submit("/projects", &form).await.location.as_deref(), Some("/projects?saved=wyrm-gpu#wyrm-gpu"));
    assert!(b.projects_toml().contains("slug = \"wyrm-gpu\""));
    let again = b.submit("/projects", &form).await;
    assert!(again.body.contains("already exists") && again.body.contains("value=\"a GPU\""), "{}", again.body);
}

#[tokio::test]
async fn pulling_copies_discord_edits_into_the_config() {
    let mut b = Browser::new(None);
    let page = b.submit("/changes/plan", &[]).await;
    let (plan, config) = (field(&page.body, "plan_id"), field(&page.body, "config"));
    b.submit("/changes/apply", &[("plan_id", &plan), ("config", &config), ("confirm", "yes")]).await;
    b.discord.edit_channel("general", |c| c["topic"] = json!("set in Discord"));
    let unconfirmed = b.submit("/changes/adopt", &[]).await;
    assert!(unconfirmed.body.contains("Nothing was pulled"), "pulling can replace edits: it asks first");
    let page = b.submit("/changes/adopt", &[("confirm", "yes")]).await;
    assert!(page.body.contains("Copied into the config files"), "{}", page.body);
    let main = fs::read_to_string(b.dir.path().join("categories/10-main.toml")).unwrap();
    assert!(main.contains("topic = \"set in Discord\""), "{main}");
}

#[tokio::test]
async fn discord_sign_in_refuses_a_state_that_does_not_match() {
    let mut b = Browser::discord();
    assert_eq!(b.get("/").await.location.as_deref(), Some("/login"));
    let state = b.start_discord().await;
    assert_eq!(b.start_discord().await, state, "reloading the page keeps the same sign-in");

    let forged = b.get("/auth/discord?code=code-ada&state=forged").await;
    assert_eq!(forged.status, StatusCode::FORBIDDEN);
    assert!(forged.body.contains("Try again"), "{}", forged.body);
    // The state is single-use, so the real one doesn't work after a forgery either.
    let late = b.get(&format!("/auth/discord?code=code-ada&state={state}")).await;
    assert_eq!(late.status, StatusCode::FORBIDDEN);

    // Someone else's browser (no login cookie) with a real state: refused.
    let state = b.start_discord().await;
    b.cookies.clear();
    let other = b.get(&format!("/auth/discord?code=code-ada&state={state}")).await;
    assert_eq!(other.status, StatusCode::FORBIDDEN);

    assert!(b.traded().is_empty(), "no code is traded without a matching state");
    assert_eq!(b.get("/").await.location.as_deref(), Some("/login"), "not signed in");
}

#[tokio::test]
async fn discord_sign_in_refuses_people_who_do_not_run_the_server() {
    for code in ["code-mel", "code-sam"] {
        let mut b = Browser::discord();
        let page = b.sign_in_with_discord(code).await;
        assert_eq!(page.status, StatusCode::FORBIDDEN, "{code}");
        assert!(page.body.contains("Only the committee") && page.body.contains("another account"), "{}", page.body);
        assert!(!b.cookies.contains_key("session"), "{code} got no session");
        assert_eq!(b.get("/").await.location.as_deref(), Some("/login"));
    }
    let mut b = Browser::discord();
    let page = b.sign_in_with_discord("code-unknown").await;
    assert_eq!(page.status, StatusCode::BAD_GATEWAY, "a code Discord won't trade");
    assert_eq!(b.get("/").await.location.as_deref(), Some("/login"));
}

#[tokio::test]
async fn discord_sign_in_lets_admins_in() {
    // The admin role (by its lockfile id), Manage Server, and the owner.
    for (code, name) in [("code-ada", "ada"), ("code-max", "max"), ("code-olive", "olive")] {
        let mut b = Browser::discord();
        let page = b.sign_in_with_discord(code).await;
        assert_eq!((page.status, page.location.as_deref()), (StatusCode::SEE_OTHER, Some("/")), "{}", page.body);
        let cookie = page.set_cookie.unwrap();
        let session = cookie.lines().find(|c| c.starts_with("session=")).unwrap();
        for want in ["HttpOnly", "SameSite=Lax", "Secure"] {
            assert!(session.contains(want), "{session}");
        }
        assert!(b.cookies.contains_key("session") && !b.cookies.contains_key("login"), "login cookie cleared");
        let home = b.get("/").await;
        assert!(home.body.contains(name) && home.body.contains("Sign out"), "{}", home.body);
        assert_eq!(b.get("/login").await.location.as_deref(), Some("/"));
        assert_eq!(b.traded(), [code]);

        let saved = b.submit("/projects/golem/status", &[("status", "past"), ("confirm", "yes")]).await;
        assert_eq!(saved.location.as_deref(), Some("/projects?saved=golem#golem"));
        assert!(b.projects_toml().contains("status = \"past\""));
    }
}

#[tokio::test(start_paused = true)]
async fn discord_access_is_checked_again_after_a_while() {
    let mut b = Browser::discord();
    b.sign_in_with_discord("code-ada").await;
    b.set_roles(ADMIN, &[]);
    // Within the cache: Discord isn't asked again.
    let saved = b.submit("/projects/golem/status", &[("status", "past"), ("confirm", "yes")]).await;
    assert_eq!(saved.location.as_deref(), Some("/projects?saved=golem#golem"));

    tokio::time::advance(auth::RECHECK + Duration::from_secs(1)).await;
    let refused = b.get("/").await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN, "viewing is checked too");
    assert!(refused.body.contains("no longer has access"), "{}", refused.body);
    assert_eq!(b.get("/").await.location.as_deref(), Some("/login"), "signed out");

    let mut b = Browser::discord();
    b.sign_in_with_discord("code-ada").await;
    let csrf = field(&b.get("/changes").await.body, "csrf");
    b.set_roles(ADMIN, &[]);
    tokio::time::advance(auth::RECHECK + Duration::from_secs(1)).await;
    let refused = b.post("/projects/golem/status", &[("csrf", &csrf), ("status", "active"), ("confirm", "yes")]).await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN);
    assert!(refused.body.contains("no longer has access"), "{}", refused.body);
    assert!(b.projects_toml().contains("status = \"active\""), "nothing changed");

    // Still on the committee: the check passes and is good for another while.
    let mut b = Browser::discord();
    b.sign_in_with_discord("code-ada").await;
    tokio::time::advance(auth::RECHECK + Duration::from_secs(1)).await;
    let saved = b.submit("/projects/golem/status", &[("status", "past"), ("confirm", "yes")]).await;
    assert_eq!(saved.location.as_deref(), Some("/projects?saved=golem#golem"));
    b.set_roles(ADMIN, &[]);
    tokio::time::advance(auth::RECHECK / 2).await;
    let saved = b.submit("/projects/golem/status", &[("status", "active"), ("confirm", "yes")]).await;
    assert_eq!(saved.location.as_deref(), Some("/projects?saved=golem#golem"), "checked recently");
}

#[tokio::test]
async fn discord_sign_in_needs_https_off_loopback() {
    let settings = |secret: Option<&str>, url: Option<&str>, app: Option<&str>| {
        Discord::settings(secret.map(Into::into), url.map(Into::into), app.map(Into::into))
    };
    assert!(settings(None, None, Some("777")).unwrap().is_none(), "off without both");
    assert!(settings(Some("s"), None, Some("777")).is_err());
    assert!(settings(None, Some("https://ui.example"), Some("777")).is_err());
    assert!(settings(Some("s"), Some("https://ui.example"), None).is_err(), "needs APPLICATION_ID");
    assert!(settings(Some("s"), Some("ui.example"), Some("777")).is_err());
    let (_, url, _) = settings(Some("s"), Some("https://ui.example/"), Some("777")).unwrap().unwrap();
    assert_eq!(url, "https://ui.example");

    let any: SocketAddr = "0.0.0.0:8080".parse().unwrap();
    let local: SocketAddr = "127.0.0.1:8080".parse().unwrap();
    let at = |base_url: &str| {
        let oauth = Box::new(FakeOAuth { users: HashMap::new() });
        SignIn::Discord(Discord {
            client_id: "777".into(),
            client_secret: "s".into(),
            base_url: base_url.into(),
            oauth,
        })
    };
    let plain = at("http://ui.example");
    assert!(auth::check_bind(any, &plain).is_err(), "OAuth over plain http off loopback");
    assert!(auth::check_bind(local, &plain).is_ok());
    let tls = at("https://ui.example");
    assert!(auth::check_bind(any, &tls).is_ok());
}

#[tokio::test]
async fn code_sign_in_ignores_the_discord_callback() {
    let mut b = Browser::new(Some(CODE));
    let page = b.get("/auth/discord?code=x&state=y").await;
    assert_eq!(page.location.as_deref(), Some("/login"));
}

#[tokio::test]
async fn api_needs_a_live_token() {
    let mut b = Browser::discord();
    let (status, body) = b.api("GET", "/api/v1/plan", None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(body["error"].as_str().unwrap().contains("login"), "{body}");
    assert_eq!(b.api("GET", "/api/v1/me", Some("guess"), None).await.0, StatusCode::UNAUTHORIZED);

    // A browser's session isn't an API token, nor the other way round.
    b.sign_in_with_discord("code-ada").await;
    let session = b.cookies["session"].clone();
    assert_eq!(b.api("GET", "/api/v1/me", Some(&session), None).await.0, StatusCode::UNAUTHORIZED);
    let token = b.token("code-ada").await;
    b.cookies.insert("session".into(), token.clone());
    assert_eq!(b.get("/").await.location.as_deref(), Some("/login"));

    let (status, me) = b.api("GET", "/api/v1/me", Some(&token), None).await;
    assert_eq!((status, me["user"].as_str()), (StatusCode::OK, Some("ada")));
    assert_eq!(b.api("POST", "/api/v1/logout", Some(&token), None).await.0, StatusCode::NO_CONTENT);
    assert_eq!(b.api("GET", "/api/v1/me", Some(&token), None).await.0, StatusCode::UNAUTHORIZED, "revoked");
}

#[tokio::test(start_paused = true)]
async fn api_refuses_people_who_do_not_run_the_server() {
    let mut b = Browser::discord();
    let page = b.cli_sign_in(&cli_start(Some(4567)), "code-mel").await;
    assert_eq!((page.status, page.location), (StatusCode::FORBIDDEN, None), "no token for a non-admin");

    let token = b.token("code-ada").await;
    b.set_roles(ADMIN, &[]);
    assert_eq!(b.api("GET", "/api/v1/me", Some(&token), None).await.0, StatusCode::OK, "checked recently");
    tokio::time::advance(auth::RECHECK + Duration::from_secs(1)).await;
    let (status, body) = b.api("GET", "/api/v1/projects", Some(&token), None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(body["error"].as_str().unwrap().contains("no longer has access"), "{body}");
    assert_eq!(b.api("GET", "/api/v1/me", Some(&token), None).await.0, StatusCode::UNAUTHORIZED, "signed out");

    // The token lasts 30 days, not forever.
    let token = b.token("code-max").await;
    tokio::time::advance(auth::API_TTL).await;
    assert_eq!(b.api("GET", "/api/v1/me", Some(&token), None).await.0, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn cli_sign_in_only_goes_back_to_loopback() {
    let mut b = Browser::discord();
    let challenge = s256(CLI_VERIFIER);
    for bad in [
        format!("port=0&state={CLI_STATE}&challenge={challenge}"),
        format!("port=70000&state={CLI_STATE}&challenge={challenge}"),
        format!("port=80&state=short&challenge={challenge}"),
        format!("port=4567&state={CLI_STATE}"),
        format!("port=4567&state={CLI_STATE}&challenge=abc"),
    ] {
        let page = b.get(&format!("/auth/cli?{bad}")).await;
        assert_eq!((page.status, page.location), (StatusCode::BAD_REQUEST, None), "{bad}");
    }
    let start = format!("{}&redirect_uri=https://evil.example/", cli_start(Some(4567)));
    let page = b.cli_sign_in(&start, "code-ada").await;
    let to = page.location.unwrap();
    assert!(to.starts_with("http://127.0.0.1:4567/callback?"), "{to}");

    // Without a port, the page shows a one-time code to paste.
    let page = b.cli_sign_in(&cli_start(None), "code-olive").await;
    assert_eq!((page.status, page.location), (StatusCode::OK, None));
    let code = page.body.split("<code>").nth(1).and_then(|s| s.split("</code>").next()).unwrap().to_owned();
    assert_eq!(b.api("GET", "/api/v1/me", Some(&code), None).await.0, StatusCode::UNAUTHORIZED, "not a token");
    let (status, token) = b.trade(&code, CLI_VERIFIER).await;
    assert_eq!(status, StatusCode::OK, "{token}");
    let (status, me) = b.api("GET", "/api/v1/me", token["token"].as_str(), None).await;
    assert_eq!((status, me["user"].as_str()), (StatusCode::OK, Some("olive")));
    assert_eq!(b.trade(&code, CLI_VERIFIER).await.0, StatusCode::UNAUTHORIZED, "used once");

    let mut b = Browser::new(Some(CODE));
    assert_eq!(b.get(&cli_start(Some(4567))).await.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_cli_code_is_worthless_without_the_cli_s_secret() {
    let mut b = Browser::discord();
    let page = b.cli_sign_in(&cli_start(Some(4567)), "code-ada").await;
    let fields = query(&page.location.unwrap());
    // Another program on the machine caught the redirect, but doesn't know the verifier.
    let (status, body) = b.trade(&fields["code"], "a-guess").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    assert_eq!(b.trade(&fields["code"], CLI_VERIFIER).await.0, StatusCode::UNAUTHORIZED, "and the code is spent");
}

#[tokio::test]
async fn api_applies_only_plans_it_made() {
    let mut b = Browser::discord();
    let (ada, max) = (b.token("code-ada").await, b.token("code-max").await);
    let (status, planned) = b.api("GET", "/api/v1/plan", Some(&ada), None).await;
    assert_eq!(status, StatusCode::OK, "{planned}");
    let id = planned["id"].as_str().unwrap();
    let plan: Plan = serde_json::from_value(planned["plan"].clone()).unwrap();
    assert!(plan.changes() > 1 && planned["lines"].as_array().unwrap().len() >= plan.changes(), "{planned}");

    // Only an id goes back, so there's no plan to forge; unknown ids and other people's are refused.
    let apply = |id: &str, steps: Value| json!({ "plan_id": id, "steps": steps, "plan": { "actions": [] } });
    assert_eq!(
        b.api("POST", "/api/v1/apply", Some(&ada), Some(apply("made-up", Value::Null))).await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(b.api("POST", "/api/v1/apply", Some(&max), Some(apply(id, Value::Null))).await.0, StatusCode::NOT_FOUND);
    assert!(b.discord.take_calls().is_empty(), "nothing sent to Discord");

    // Just the first step, of a fresh plan.
    let (_, planned) = b.api("GET", "/api/v1/plan", Some(&ada), None).await;
    let id = planned["id"].as_str().unwrap();
    let first = plan.actions.iter().position(|a| a.mutates()).unwrap();
    let (status, steps) = b.api("POST", "/api/v1/apply", Some(&ada), Some(apply(id, json!([first])))).await;
    assert_eq!(status, StatusCode::OK, "{steps}");
    assert_eq!(steps.as_array().unwrap().len(), 1, "{steps}");
    assert!(steps[0]["error"].is_null(), "{steps}");
    assert!(!b.discord.take_calls().is_empty());
    let (status, _) = b.api("POST", "/api/v1/apply", Some(&ada), Some(apply(id, Value::Null))).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "a plan is used once");

    let (_, planned) = b.api("GET", "/api/v1/plan", Some(&ada), None).await;
    let left: Plan = serde_json::from_value(planned["plan"].clone()).unwrap();
    assert_eq!(left.changes(), plan.changes() - 1);
    let id = planned["id"].as_str().unwrap();
    let (status, _) = b.api("POST", "/api/v1/apply", Some(&ada), Some(apply(id, json!([999])))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "no such step");
}

#[tokio::test]
async fn api_edits_projects_checks_and_pulls() {
    let mut b = Browser::discord();
    let ada = b.token("code-ada").await;
    let new = json!({ "name": "Wyrm GPU", "blurb": "a GPU", "area": "gpu", "status": "proposed" });
    let (status, saved) = b.api("POST", "/api/v1/projects", Some(&ada), Some(new.clone())).await;
    assert_eq!((status, saved["slug"].as_str()), (StatusCode::OK, Some("wyrm-gpu")));
    let (status, again) = b.api("POST", "/api/v1/projects", Some(&ada), Some(new)).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(again["error"].as_str().unwrap().contains("already exists"), "{again}");

    let status = json!({ "status": "past" });
    assert_eq!(b.api("POST", "/api/v1/projects/golem/status", Some(&ada), Some(status)).await.0, StatusCode::OK);
    let open = json!({ "open": true });
    assert_eq!(b.api("POST", "/api/v1/projects/golem/applications", Some(&ada), Some(open)).await.0, StatusCode::OK);
    let (_, list) = b.api("GET", "/api/v1/projects", Some(&ada), None).await;
    let golem = &list["projects"][0];
    assert_eq!((golem["status"].as_str(), golem["application"]["open"].as_bool()), (Some("past"), Some(true)));
    assert_eq!(list["projects"][1]["slug"], "wyrm-gpu");

    let (status, checked) = b.api("GET", "/api/v1/check", Some(&ada), None).await;
    assert_eq!((status, checked["errors"].as_array().map(Vec::len)), (StatusCode::OK, Some(0)), "{checked}");
    write(b.dir.path(), "bot.toml", "name = \"Test\"\nadmin_role = \"ghost\"\n");
    let (_, checked) = b.api("GET", "/api/v1/check", Some(&ada), None).await;
    assert!(checked["errors"][0].as_str().unwrap().contains("ghost"), "{checked}");
    let (status, refused) = b.api("GET", "/api/v1/plan", Some(&ada), None).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(refused["problems"][0].as_str().unwrap().contains("ghost"), "{refused}");

    write(b.dir.path(), "bot.toml", "name = \"Test\"\nadmin_role = \"admin\"\n");
    let (status, pulled) = b.api("POST", "/api/v1/pull", Some(&ada), Some(json!({}))).await;
    assert_eq!(status, StatusCode::OK, "{pulled}");
    assert!(pulled.is_array(), "{pulled}");
}

#[tokio::test]
async fn theme_choice_is_remembered_and_stamped() {
    use axum::http::Request;
    use tower::ServiceExt;
    let d = tempfile::tempdir().unwrap();
    std::fs::write(d.path().join("server.toml"), "[guild]\nname = \"T\"\n").unwrap();
    let app = theme_test_router();
    let set = app
        .clone()
        .oneshot(
            Request::get("/theme/dark")
                .header("referer", "https://x.test/projects")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(set.headers()["location"], "/projects", "goes back to the page, on this site only");
    assert!(set.headers()["set-cookie"].to_str().unwrap().starts_with("theme=dark"));
    let evil = app
        .clone()
        .oneshot(
            Request::get("/theme/dark")
                .header("referer", "https://evil.test//evil.test/x")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(evil.headers()["location"], "/");
    let page = app
        .oneshot(Request::get("/page").header("cookie", "theme=dark").body(axum::body::Body::empty()).unwrap())
        .await
        .unwrap();
    let body = axum::body::to_bytes(page.into_body(), 1 << 20).await.unwrap();
    assert!(String::from_utf8_lossy(&body).contains("data-theme=\"dark\""));
}

#[tokio::test]
async fn api_edits_the_config_with_ops_and_refuses_breaking_ones() {
    let mut b = Browser::discord();
    let ada = b.token("code-ada").await;
    for (method, path) in [("GET", "/api/v1/ops/schema"), ("POST", "/api/v1/ops"), ("POST", "/api/v1/ops/preview")] {
        assert_eq!(b.api(method, path, None, None).await.0, StatusCode::UNAUTHORIZED, "{method} {path}");
    }
    let (status, schema) = b.api("GET", "/api/v1/ops/schema", Some(&ada), None).await;
    assert_eq!(status, StatusCode::OK, "{schema}");
    assert!(schema["roles"].as_array().unwrap().iter().any(|r| r["key"] == "admin"), "{schema}");

    let open = json!({ "op": "set_project", "slug": "golem", "set": { "applications": true } });
    let (status, change) = b.api("POST", "/api/v1/ops", Some(&ada), Some(open)).await;
    assert_eq!(status, StatusCode::OK, "{change}");
    assert_eq!(change["files"], json!(["projects.toml"]));
    assert_eq!(change["summary"], "Project golem: applications on");
    let projects = b.projects_toml();
    assert!(projects.starts_with("# Keep these") && projects.contains("open = true"), "{projects}");

    let before = b.projects_toml();
    let breaking = json!({ "op": "remove_role", "key": "admin" });
    let (status, refused) = b.api("POST", "/api/v1/ops", Some(&ada), Some(breaking)).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(refused["problems"][0].as_str().unwrap().contains("unknown role `admin`"), "{refused}");
    assert_eq!(b.projects_toml(), before);
    let (status, _) = b.api("POST", "/api/v1/ops", Some(&ada), Some(json!({ "op": "launch" }))).await;
    assert!(status.is_client_error(), "unknown ops are refused");

    let body = json!({ "body": "Hi {#general}\n{button:volunteer|Volunteer}" });
    let (status, shown) = b.api("POST", "/api/v1/ops/preview", Some(&ada), Some(body)).await;
    assert_eq!(status, StatusCode::OK, "{shown}");
    assert_eq!(
        (shown["content"].as_str(), &shown["buttons"][0]["action"]),
        (Some("Hi {#general}"), &json!("volunteer"))
    );
}

#[tokio::test]
async fn api_runs_the_review_queue_feeds_resources_and_activity() {
    let mut b = Browser::discord();
    let ada = b.token("code-ada").await;
    let p = b.dir.path();
    write(
        p,
        "bot.toml",
        "name = \"Test\"\nadmin_role = \"admin\"\n[[position]]\nrole = \"admin\"\n\n[resources]\nforum = \"projects\"\n",
    );
    write(
        p,
        "lock.toml",
        &format!("guild = \"1000\"\n[roles]\nadmin = \"{ADMIN_ROLE}\"\n[channels]\nprojects = \"1700\"\n"),
    );
    let forum =
        json!({ "id": "1700", "type": 15, "name": "projects", "available_tags": [{ "id": "1702", "name": "active" }] });
    b.discord.state.lock().unwrap().channels.push(forum);
    let store = b.store().await;
    let db = store.guild("1000");
    let answers = r#"[{"question":"Why?","answer":"To help"}]"#;
    let member = MEMBER.to_string();
    let id = db.add_application(chipsoc_store::AppKind::Position, "admin", &member, answers, false, 1).await.unwrap();
    for (method, path) in [
        ("GET", "/api/v1/queue"),
        ("POST", "/api/v1/queue/1"),
        ("POST", "/api/v1/queue/1/lead"),
        ("GET", "/api/v1/feeds"),
        ("DELETE", "/api/v1/feeds/1"),
        ("GET", "/api/v1/resources"),
        ("POST", "/api/v1/resources"),
        ("GET", "/api/v1/ideas"),
        ("GET", "/api/v1/activity"),
        ("GET", "/api/v1/join"),
        ("GET", "/api/v1/join/notify"),
        ("POST", "/api/v1/join/notify"),
    ] {
        assert_eq!(b.api(method, path, None, None).await.0, StatusCode::UNAUTHORIZED, "{method} {path}");
    }

    let (status, queue) = b.api("GET", "/api/v1/queue", Some(&ada), None).await;
    assert_eq!(status, StatusCode::OK, "{queue}");
    assert_eq!((&queue[0]["what"], &queue[0]["answers"][0]["answer"]), (&json!("Admin"), &json!("To help")));
    let accept = json!({ "accept": true });
    let decide = format!("/api/v1/queue/{id}");
    let (status, decided) = b.api("POST", &decide, Some(&ada), Some(accept.clone())).await;
    assert_eq!(status, StatusCode::OK, "{decided}");
    assert!(decided["outcome"].as_str().unwrap().contains("Gave them Admin."), "plain text: {decided}");
    assert!(
        b.discord.roles_of(MEMBER).contains(&ADMIN_ROLE.to_owned()),
        "given directly: this config doesn't autosave"
    );
    let (status, again) = b.api("POST", &decide, Some(&ada), Some(accept)).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(again["error"].as_str().unwrap().contains("already accepted"), "{again}");
    assert_eq!(b.api("GET", "/api/v1/queue", Some(&ada), None).await.1, json!([]));

    let feed = db.add_feed("https://example.org/feed", "general", "ada", 1).await.unwrap();
    let (_, feeds) = b.api("GET", "/api/v1/feeds", Some(&ada), None).await;
    assert_eq!(feeds[0]["url"], "https://example.org/feed", "{feeds}");
    let remove = format!("/api/v1/feeds/{feed}");
    assert_eq!(b.api("DELETE", &remove, Some(&ada), None).await.0, StatusCode::NO_CONTENT);
    assert_eq!(b.api("DELETE", &remove, Some(&ada), None).await.0, StatusCode::NOT_FOUND);

    let link = json!({ "title": "Verilog guide", "url": "https://example.org/v", "description": "Start here", "tags": ["Active"] });
    let (status, posted) = b.api("POST", "/api/v1/resources", Some(&ada), Some(link)).await;
    assert_eq!((status, &posted["tags"]), (StatusCode::OK, &json!(["active"])), "{posted}");
    let (_, listed) = b.api("GET", "/api/v1/resources?limit=5", Some(&ada), None).await;
    assert_eq!(listed[0]["title"], "Verilog guide", "{listed}");
    assert!(listed[0]["content"].as_str().unwrap().contains("https://example.org/v"), "{listed}");
    let (status, ideas) = b.api("GET", "/api/v1/ideas", Some(&ada), None).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(ideas["error"].as_str().unwrap().contains("ideas forum"), "{ideas}");

    let (_, activity) = b.api("GET", "/api/v1/activity", Some(&ada), None).await;
    let actions: Vec<&str> = activity.as_array().unwrap().iter().filter_map(|e| e["action"].as_str()).collect();
    assert_eq!(actions, ["post_resource", "remove_feed", "accepted"], "newest first");
    assert_eq!(activity[2]["actor"], ADMIN.to_string(), "who decided");
}

#[tokio::test]
async fn the_remote_client_reaches_every_endpoint() {
    use crate::remote::{Client, Login};
    use chipsoc_config::Snowflake;
    use chipsoc_config::ops::{Op, RoleField};

    let dir = tempfile::tempdir().unwrap();
    let server = crate::testing::spawn(dir.path()).await.unwrap();
    let token = crate::testing::token(&server.url, "code-ada").await.unwrap();
    let c = Client::new(Login { server: server.url.clone(), token, user: "ada".into() }).unwrap();

    let st = c.status().await.unwrap();
    assert_eq!((st.name.as_str(), st.linked, st.queue, st.projects), ("Test", true, 0, 1));
    assert!(st.errors.is_empty(), "{:?}", st.errors);
    let drift = c.drift().await.unwrap();
    assert!(drift.waiting.iter().any(|l| l.text.contains("general")), "{drift:?}");

    let schema = c.schema().await.unwrap();
    assert!(schema.roles.iter().any(|r| r.key == "admin") && schema.projects[0].slug == "golem");
    let change = c.op(&Op::SetRole { key: "admin".into(), set: RoleField::Hoist(true) }).await.unwrap();
    assert_eq!(change.files, ["roles.toml"]);
    let bad = c.op(&Op::RemoveRole { key: "admin".into() }).await.unwrap_err();
    assert!(bad.downcast_ref::<chipsoc_engine::Problems>().is_some(), "{bad:#}");
    assert_eq!(c.preview("Hi\n{button:volunteer|Help}").await.unwrap().buttons[0].action, "volunteer");

    // The server's own database: reach it through the API alone.
    assert!(c.queue().await.unwrap().is_empty() && c.history(5).await.unwrap().is_empty());
    let err = c.decide(1, false, Some("no")).await.unwrap_err().to_string();
    assert!(err.contains("no longer exists"), "{err}");
    let err = c.add_feed("http://127.0.0.1/feed").await.unwrap_err().to_string();
    assert!(err.contains("feeds aren't set up"), "{err}");
    assert!(c.feeds().await.unwrap().is_empty());
    assert!(c.pause_feed(1, true).await.unwrap_err().to_string().contains("No such feed"));
    assert!(c.remove_feed(1).await.unwrap_err().to_string().contains("No such feed"));
    assert!(c.archive_resource(Snowflake(1)).await.unwrap_err().to_string().contains("resources aren't set up"));
    assert!(c.ideas(5).await.unwrap_err().to_string().contains("ideas forum"));

    let roster = c.members().await.unwrap();
    assert_eq!((roster.holders.len(), roster.leads[0].project.as_deref()), (0, Some("golem")));
    let err = c.appoint("admin", Snowflake(MEMBER)).await.unwrap_err().to_string();
    assert!(err.contains("doesn't say who holds"), "{err}");
    let err = c.unappoint("admin", Snowflake(MEMBER)).await.unwrap_err().to_string();
    assert!(err.contains("synced git checkout"), "this config doesn't save: {err}");
    let err = c.add_lead("golem", Snowflake(MEMBER)).await.unwrap_err().to_string();
    assert!(err.contains("synced git checkout"), "{err}");
    assert!(c.remove_lead("nope", Snowflake(MEMBER)).await.unwrap_err().to_string().contains("No project"));
    assert_eq!(c.restore(Snowflake(MEMBER)).await.unwrap_err().to_string(), "Already dealt with.");
    assert!(c.forget(Snowflake(MEMBER)).await.unwrap_err().to_string().contains("Nobody"));
    assert!(c.activity(5).await.unwrap().is_empty(), "nothing was done");
}

#[tokio::test]
async fn api_decides_with_reasons_pauses_feeds_and_restores_people() {
    let mut b = Browser::discord();
    let ada = b.token("code-ada").await;
    let p = b.dir.path();
    write(p, "bot.toml", "name = \"Test\"\nadmin_role = \"admin\"\n[[position]]\nrole = \"admin\"\n");
    let store = b.store().await;
    let db = store.guild("1000");
    let member = MEMBER.to_string();
    let id = db.add_application(chipsoc_store::AppKind::Position, "admin", &member, "[]", false, 1).await.unwrap();
    for (method, path) in [
        ("GET", "/api/v1/status"),
        ("GET", "/api/v1/drift"),
        ("GET", "/api/v1/queue/history"),
        ("POST", "/api/v1/feeds"),
        ("POST", "/api/v1/feeds/1/pause"),
        ("DELETE", "/api/v1/resources/1"),
        ("GET", "/api/v1/members"),
        ("POST", "/api/v1/members"),
        ("DELETE", "/api/v1/members/admin/1"),
        ("DELETE", "/api/v1/members/former/1"),
        ("POST", "/api/v1/members/former/1/restore"),
        ("POST", "/api/v1/projects/golem/leads"),
        ("DELETE", "/api/v1/projects/golem/leads/1"),
    ] {
        assert_eq!(b.api(method, path, None, None).await.0, StatusCode::UNAUTHORIZED, "{method} {path}");
    }

    let reject = json!({ "accept": false, "reason": "Elections are in May." });
    let (status, d) = b.api("POST", &format!("/api/v1/queue/{id}"), Some(&ada), Some(reject)).await;
    assert_eq!((status, &d["reason"]), (StatusCode::OK, &json!("Elections are in May.")), "{d}");
    assert!(b.discord.dms_to(MEMBER)[0].contains("Elections are in May."));
    let (_, past) = b.api("GET", "/api/v1/queue/history", Some(&ada), None).await;
    assert_eq!((&past[0]["status"], &past[0]["reason"]), (&json!("rejected"), &json!("Elections are in May.")));

    let feed = db.add_feed("https://example.org/feed", "general", "ada", 1).await.unwrap();
    let pause = format!("/api/v1/feeds/{feed}/pause");
    let (status, _) = b.api("POST", &pause, Some(&ada), Some(json!({ "paused": true }))).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(b.api("GET", "/api/v1/feeds", Some(&ada), None).await.1[0]["paused"], true);

    db.remember_roles(&member, &["admin".into()], 5).await.unwrap();
    let (_, roster) = b.api("GET", "/api/v1/members", Some(&ada), None).await;
    assert_eq!(roster["former"][0]["person"]["id"], member, "{roster}");
    let (status, done) = b.api("POST", &format!("/api/v1/members/former/{MEMBER}/restore"), Some(&ada), None).await;
    assert_eq!(status, StatusCode::OK, "{done}");
    assert!(done["text"].as_str().unwrap().contains("Not recorded in the config"), "{done}");
    assert!(b.discord.roles_of(MEMBER).contains(&ADMIN_ROLE.to_owned()));
    let (_, roster) = b.api("GET", "/api/v1/members", Some(&ada), None).await;
    assert_eq!(roster["former"], json!([]));

    let (_, activity) = b.api("GET", "/api/v1/activity", Some(&ada), None).await;
    let actions: Vec<&str> = activity.as_array().unwrap().iter().filter_map(|e| e["action"].as_str()).collect();
    assert_eq!(actions, ["restore", "pause_feed", "rejected"]);
}

/// Signed in with Discord as ada, an admin, on the [`committee`]'s server.
async fn ada() -> Browser {
    let mut b = Browser::discord();
    let page = b.sign_in_with_discord("code-ada").await;
    assert_eq!(page.location.as_deref(), Some("/"), "{}", page.body);
    b
}

impl Browser {
    fn file(&self, name: &str) -> String {
        fs::read_to_string(self.dir.path().join(name)).unwrap_or_default()
    }

    /// Follow a redirect to its page.
    async fn follow(&mut self, p: Page) -> Page {
        let to = p.location.unwrap_or_else(|| panic!("not redirected ({}): {}", p.status, p.body));
        self.get(&to).await
    }
}

#[tokio::test]
async fn every_page_renders_signed_in() {
    let mut b = ada().await;
    for (path, want) in [
        ("/", "Jump to"),
        ("/roles", "Add a role"),
        ("/channels", "Add a channel"),
        ("/channels/permissions/category/main", "Permissions: Main"),
        ("/channels/permissions/channel/general", "Also use the category"),
        ("/projects", "Golem"),
        ("/positions", "Add a position"),
        ("/queue", "Applications and suggestions are off"),
        ("/members", "Project leads"),
        ("/feeds", "Feeds are off"),
        ("/resources", "Resources are off"),
        ("/onboarding", "Add a question"),
        ("/activity", "Nothing yet"),
        ("/recordings", "Recording is off"),
        ("/changes", "Apply changes to Discord"),
        ("/account", "Command line"),
    ] {
        let page = b.get(path).await;
        assert_eq!(page.status, StatusCode::OK, "{path}: {}", page.body);
        assert!(page.body.contains(want), "{path} lacks {want:?}: {}", page.body);
        let untabbed = path.starts_with("/channels/permissions") || path == "/account";
        assert!(page.body.contains("aria-current=\"page\"") || untabbed, "{path}");
    }
    assert!(b.submit("/changes/plan", &[]).await.body.contains("Plan: what would change"));
    assert_eq!(b.get("/server").await.location.as_deref(), Some("/channels"), "the old page moved");
    assert_eq!(b.get("/channels/permissions/channel/ghost").await.status, StatusCode::OK);
}

#[tokio::test]
async fn new_forms_still_need_the_csrf_token() {
    let mut b = ada().await;
    for path in [
        "/roles/add",
        "/channels/add-channel",
        "/positions/add",
        "/projects/golem/edit",
        "/queue/reject",
        "/members/appoint",
        "/feeds/pause",
        "/resources/archive",
        "/onboarding/post",
    ] {
        let page = b.post(path, &[("name", "x")]).await;
        assert_eq!(page.status, StatusCode::FORBIDDEN, "{path}");
    }
}

#[tokio::test]
async fn roles_are_edited_through_ops_with_a_flash_once() {
    let mut b = ada().await;
    let roles = format!("# our roles\n{}", b.file("roles.toml"));
    write(b.dir.path(), "roles.toml", &roles);

    let added = b.submit("/roles/add", &[("name", "Mods"), ("color", "#336699")]).await;
    assert_eq!(added.location.as_deref(), Some("/roles"));
    let page = b.follow(added).await;
    assert!(
        page.body.contains("class=\"note good flash\"") && page.body.contains("Add role Mods (mods)"),
        "{}",
        page.body
    );
    assert!(!b.get("/roles").await.body.contains("flash"), "shown once");
    let file = b.file("roles.toml");
    assert!(file.starts_with("# our roles") && file.contains("key = \"mods\"") && file.contains("#336699"), "{file}");

    let before = b.file("roles.toml");
    let bad = b.submit("/roles/add", &[("name", "Helpers"), ("color", "blue")]).await;
    assert_eq!(bad.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(bad.body.contains("class=\"field-error\"") && bad.body.contains("aria-invalid=\"true\""), "{}", bad.body);
    assert!(bad.body.contains("value=\"Helpers\""), "what they typed is kept");
    assert_eq!(b.file("roles.toml"), before);

    let edit =
        [("key", "mods"), ("name", "Moderators"), ("color", "#336699"), ("hoist", "yes"), ("perm", "KICK_MEMBERS")];
    b.submit("/roles/edit", &edit).await;
    let file = b.file("roles.toml");
    assert!(file.contains("Moderators") && file.contains("hoist = true") && file.contains("KICK_MEMBERS"), "{file}");

    // Removing needs the "Yes"; and the admin role can't go while bot.toml uses it.
    b.submit("/roles/remove", &[("key", "mods")]).await;
    assert!(b.file("roles.toml").contains("mods"), "not without confirming");
    let refused = b.submit("/roles/remove", &[("key", "admin"), ("confirm", "yes")]).await;
    assert_eq!(refused.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(refused.body.contains("unknown role") && refused.body.contains("role=\"alert\""), "{}", refused.body);
    b.submit("/roles/remove", &[("key", "mods"), ("confirm", "yes")]).await;
    assert!(!b.file("roles.toml").contains("mods"));
}

/// A form's edits are one change: when one is refused, none is saved.
#[tokio::test]
async fn a_form_saves_all_its_edits_or_none() {
    let mut b = ada().await;
    b.submit("/roles/add", &[("name", "Mods")]).await;
    let before = b.file("roles.toml");
    let edit = [("key", "mods"), ("name", "Moderators"), ("hoist", "yes"), ("perm", "NOT_A_PERMISSION")];
    let refused = b.submit("/roles/edit", &edit).await;
    assert_eq!(refused.status, StatusCode::UNPROCESSABLE_ENTITY, "{}", refused.body);
    assert_eq!(b.file("roles.toml"), before, "the name and hoist weren't saved without the permissions");
}

#[tokio::test]
async fn channels_and_their_permissions_are_edited() {
    let mut b = ada().await;
    let main = "categories/10-main.toml";
    b.submit("/channels/add-channel", &[("category", "main"), ("name", "Help desk"), ("type", "forum")]).await;
    assert!(b.file(main).contains("key = \"help-desk\""), "{}", b.file(main));
    let topic = [("key", "general"), ("name", "general"), ("topic", "Say hi"), ("type", "text"), ("slowmode", "5")];
    b.submit("/channels/edit-channel", &topic).await;
    let file = b.file(main);
    assert!(file.contains("topic = \"Say hi\"") && file.contains("slowmode = 5"), "{file}");
    let bad = b
        .submit(
            "/channels/edit-channel",
            &[("key", "general"), ("name", "general"), ("type", "text"), ("slowmode", "soon")],
        )
        .await;
    assert_eq!(bad.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(bad.body.contains("chan-general-slowmode-error"), "{}", bad.body);

    let perms = [("key", "main"), ("count", "1"), ("ow0-role", "everyone"), ("ow0-VIEW_CHANNEL", "deny")];
    b.submit("/channels/category-perms", &perms).await;
    assert!(b.file(main).contains("VIEW_CHANNEL"), "{}", b.file(main));
    assert!(b.get("/channels").await.body.contains("hidden"), "who can see it is shown");
    let ghost = [("key", "main"), ("count", "1"), ("ow0-role", "ghost"), ("ow0-VIEW_CHANNEL", "allow")];
    let refused = b.submit("/channels/category-perms", &ghost).await;
    assert_eq!(refused.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(refused.body.contains("Permissions: Main") && refused.body.contains("ghost"), "{}", refused.body);

    b.submit("/channels/archive-channel", &[("key", "help-desk")]).await;
    assert!(b.file(main).contains("help-desk"), "not without confirming");
    b.submit("/channels/archive-channel", &[("key", "help-desk"), ("confirm", "yes")]).await;
    assert!(!b.file(main).contains("help-desk"));
}

#[tokio::test]
async fn positions_and_their_forms_are_edited() {
    let mut b = ada().await;
    // The admin role itself can't be a position, so a committee role is.
    let roles = b.file("roles.toml").replacen(
        "[[role]]\nkey = \"everyone\"",
        "[[role]]\nkey = \"chair\"\nname = \"Chair\"\n\n[[role]]\nkey = \"everyone\"",
        1,
    );
    write(b.dir.path(), "roles.toml", &roles);
    assert!(b.file("roles.toml").contains("chair"));
    b.submit("/positions/add", &[("role", "chair"), ("label", "Chair")]).await;
    assert!(b.file("bot.toml").contains("[[position]]") && b.file("bot.toml").contains("Chair"));
    let questions = [
        ("role", "chair"),
        ("count", "3"),
        ("label", "Chair"),
        ("open", "yes"),
        ("intro", "Thanks for your interest."),
        ("q0-ask", "Why you?"),
        ("q0-long", "yes"),
        ("q1-ask", ""),
        ("q2-ask", "Year?"),
    ];
    b.submit("/positions/edit", &questions).await;
    let bot = b.file("bot.toml");
    assert!(bot.contains("Why you?") && bot.contains("Year?") && bot.contains("Thanks for your interest."), "{bot}");
    let none = b.submit("/positions/edit", &[("role", "chair"), ("count", "1"), ("q0-ask", "")]).await;
    assert_eq!(none.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(none.body.contains("Ask at least one question"));
    b.submit("/positions/open", &[("role", "chair"), ("open", "false")]).await;
    assert!(b.file("bot.toml").contains("open = false"), "{}", b.file("bot.toml"));
    b.submit("/positions/remove", &[("role", "chair"), ("confirm", "yes")]).await;
    assert!(!b.file("bot.toml").contains("[[position]]"));
}

#[tokio::test]
async fn projects_details_brief_form_and_confirmed_status() {
    let mut b = ada().await;
    b.submit("/projects/golem/edit", &[("name", "Golem"), ("blurb", "a CPU"), ("stage", "taping out")]).await;
    let file = b.projects_toml();
    assert!(file.starts_with("# Keep these") && file.contains("stage = \"taping out\""), "{file}");
    b.submit("/projects/golem/brief", &[("body", "We build a CPU.")]).await;
    assert!(fs::read_to_string(b.dir.path().join("content/projects/golem.md")).unwrap().contains("We build a CPU."));
    let many = b.submit("/projects/golem/questions", &[("questions", "a\nb\nc\nd")]).await;
    assert_eq!(many.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(many.body.contains("room for 3"), "{}", many.body);

    let unconfirmed = b.submit("/projects/golem/status", &[("status", "past")]).await;
    assert!(unconfirmed.body.contains("confirm"), "{}", unconfirmed.body);
    assert!(b.projects_toml().contains("status = \"active\""), "a change of status needs a Yes");
    let unconfirmed = b.submit("/projects/golem/status", &[("status", "proposed")]).await;
    assert!(unconfirmed.body.contains("confirm"), "back to an idea archives its channels too");
    b.submit("/projects/golem/status", &[("status", "proposed"), ("confirm", "yes")]).await;
    assert!(b.projects_toml().contains("status = \"proposed\""));
}

#[tokio::test]
async fn the_queue_rejects_with_a_reason_and_keeps_history() {
    let mut b = ada().await;
    write(b.dir.path(), "bot.toml", "name = \"Test\"\nadmin_role = \"admin\"\n[[position]]\nrole = \"admin\"\n");
    let store = b.store().await;
    let db = store.guild("1000");
    let answers = r#"[{"question":"Why?","answer":"To help"}]"#;
    let id = db
        .add_application(chipsoc_store::AppKind::Position, "admin", &MEMBER.to_string(), answers, false, 1)
        .await
        .unwrap();
    let page = b.get("/queue").await;
    assert!(page.body.contains("To help") && page.body.contains("Reject"), "{}", page.body);

    let done = b.submit("/queue/reject", &[("id", &id.to_string()), ("reason", "Elections are in May.")]).await;
    let page = b.follow(done).await;
    assert!(page.body.contains("Rejected; they've been told by DM."), "{}", page.body);
    assert!(
        page.body.contains("Elections are in May.") && page.body.contains("Rejected"),
        "in the history: {}",
        page.body
    );
    assert!(b.discord.dms_to(MEMBER)[0].contains("Elections are in May."));
    let again = b.submit("/queue/accept", &[("id", &id.to_string())]).await;
    assert!(b.follow(again).await.body.contains("already rejected"));
    assert!(b.get("/activity").await.body.contains("Rejected an application"));
}

#[tokio::test]
async fn members_appointing_explains_refusals() {
    let mut b = ada().await;
    let bad = b.submit("/members/appoint", &[("role", "admin"), ("user", "nobody")]).await;
    assert_eq!(bad.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(bad.body.contains("Nobody in the server is called"), "{}", bad.body);
    let refused = b.submit("/members/appoint", &[("role", "admin"), ("user", &MEMBER.to_string())]).await;
    assert!(
        refused.body.contains("doesn&#39;t say who holds") && refused.body.contains("value=\"502\""),
        "{}",
        refused.body
    );
    let unconfirmed = b.submit("/members/unappoint", &[("role", "admin"), ("user", &MEMBER.to_string())]).await;
    assert!(b.follow(unconfirmed).await.body.contains("confirm"));

    b.store().await.guild("1000").remember_roles(&MEMBER.to_string(), &["admin".into()], 5).await.unwrap();
    assert!(b.get("/members").await.body.contains("Restore"));
    let unconfirmed = b.submit("/members/restore", &[("user", &MEMBER.to_string())]).await;
    assert!(b.follow(unconfirmed).await.body.contains("confirm"), "giving roles back asks first");
    let restored = b.submit("/members/restore", &[("user", &MEMBER.to_string()), ("confirm", "yes")]).await;
    let page = b.follow(restored).await;
    assert!(
        page.body.contains("Restored.") && page.body.contains("Gave them Admin.") && !page.body.contains("**"),
        "no Discord markdown: {}",
        page.body
    );
    assert!(b.discord.roles_of(MEMBER).contains(&ADMIN_ROLE.to_owned()));
}

#[tokio::test]
async fn feeds_pause_resume_and_need_confirming_to_remove() {
    let mut b = ada().await;
    write(b.dir.path(), "bot.toml", "name = \"Test\"\nadmin_role = \"admin\"\n[feeds]\nchannel = \"general\"\n");
    let feed = b.store().await.guild("1000").add_feed("https://example.org/feed", "general", "ada", 1).await.unwrap();
    let id = feed.to_string();
    b.submit("/feeds/pause", &[("id", &id)]).await;
    let page = b.get("/feeds").await;
    assert!(page.body.contains("paused") && page.body.contains("Resume"), "{}", page.body);
    b.submit("/feeds/resume", &[("id", &id)]).await;
    assert!(!b.get("/feeds").await.body.contains(">paused<"));
    b.submit("/feeds/remove", &[("id", &id)]).await;
    assert!(b.get("/feeds").await.body.contains("example.org/feed"), "not without confirming");
    let bad = b.submit("/feeds/add", &[("url", "example.org")]).await;
    assert_eq!(bad.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(bad.body.contains("feed-add-url-error"));
    b.submit("/feeds/remove", &[("id", &id), ("confirm", "yes")]).await;
    assert!(!b.get("/feeds").await.body.contains("example.org/feed"));
}

#[tokio::test]
async fn onboarding_questions_and_posts_with_a_preview() {
    let mut b = ada().await;
    let preview = b
        .submit(
            "/onboarding/post",
            &[("channel", "general"), ("body", "Hi\n{button:volunteer|Help out}"), ("preview", "yes")],
        )
        .await;
    assert_eq!(preview.status, StatusCode::OK);
    assert!(preview.body.contains("Help out"), "{}", preview.body);
    assert!(b.file("content/general.md").is_empty(), "previewed, not saved");
    b.submit("/onboarding/post", &[("channel", "general"), ("body", "Hi\n{button:volunteer|Help out}")]).await;
    assert!(b.file("content/general.md").contains("Help out"));
    let bad = b.submit("/onboarding/post", &[("channel", "general"), ("body", "{button:launch|Go}")]).await;
    assert!(bad.body.contains("unknown button action"), "{}", bad.body);

    let none = b.submit("/onboarding/add-prompt", &[("title", "What do you study?")]).await;
    assert!(none.body.contains("prompt-add-answer-error"), "{}", none.body);
    b.submit(
        "/onboarding/add-prompt",
        &[("title", "What do you study?"), ("answer", "Physics"), ("single_select", "yes")],
    )
    .await;
    assert!(b.file("onboarding.toml").contains("What do you study?"), "{}", b.file("onboarding.toml"));
    let added =
        b.submit("/onboarding/add-option", &[("prompt", "0"), ("title", "Engineering"), ("role", "admin")]).await;
    assert!(b.file("onboarding.toml").contains("Engineering"), "{}", added.body);
    b.submit("/onboarding/remove-prompt", &[("prompt", "0"), ("confirm", "yes")]).await;
    assert!(!b.file("onboarding.toml").contains("What do you study?"));
}

#[tokio::test]
async fn resources_are_shared_and_taken_down_only_when_confirmed() {
    let mut b = ada().await;
    let p = b.dir.path();
    write(p, "bot.toml", "name = \"Test\"\nadmin_role = \"admin\"\n[resources]\nforum = \"projects\"\n");
    write(
        p,
        "lock.toml",
        &format!("guild = \"1000\"\n[roles]\nadmin = \"{ADMIN_ROLE}\"\n[channels]\nprojects = \"1700\"\n"),
    );
    let forum =
        json!({ "id": "1700", "type": 15, "name": "projects", "available_tags": [{ "id": "1702", "name": "active" }] });
    b.discord.state.lock().unwrap().channels.push(forum);

    let bad = b.submit("/resources/add", &[("title", "Guide"), ("url", "example.org")]).await;
    assert_eq!(bad.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(bad.body.contains("res-add-url-error") && bad.body.contains("value=\"Guide\""), "{}", bad.body);
    let shared = b
        .submit("/resources/add", &[("title", "Verilog guide"), ("url", "https://example.org/v"), ("tags", "active")])
        .await;
    let page = b.follow(shared).await;
    assert!(page.body.contains("Shared Verilog guide.") && page.body.contains("Archive"), "{}", page.body);
    let id = b.discord.state.lock().unwrap().channels.iter().find(|c| c["name"] == "Verilog guide").unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    b.submit("/resources/archive", &[("id", &id)]).await;
    let locked =
        |b: &Browser| b.discord.state.lock().unwrap().channels.iter().any(|c| c["id"] == id && c["locked"] == true);
    assert!(!locked(&b), "not without confirming");
    b.submit("/resources/archive", &[("id", &id), ("confirm", "yes")]).await;
    assert!(locked(&b));
    assert!(b.get("/resources").await.body.contains("The ideas forum is off"));
}

/// A tiny router with just the theme parts.
fn theme_test_router() -> axum::Router {
    use axum::routing::get;
    axum::Router::new()
        .route("/theme/{mode}", get(crate::theme::set))
        .route("/page", get(|| async { axum::response::Html("<!DOCTYPE html><html lang=\"en\"><body></body></html>") }))
        .layer(axum::middleware::from_fn(crate::theme::stamp))
}

/// The CLI's and TUI's local backend does the same work as the server.
mod local_backend {
    use std::sync::Arc;

    use chipsoc_client::{Backend, Local};
    use chipsoc_config::ops::{Op, RoleField};
    use chipsoc_discord::fake::FakeDiscord;
    use chipsoc_engine::{Engine, Options};

    use crate::testing::write_config;

    #[tokio::test]
    async fn local_work_goes_through_the_same_code() {
        let dir = tempfile::tempdir().unwrap();
        write_config(dir.path());
        let fake = Arc::new(FakeDiscord::new());
        let none = Backend::Local(Local::with_db(dir.path(), fake.clone(), None, "me"));
        let err = none.queue().await.unwrap_err().to_string();
        assert!(err.contains("login --server"), "{err}");
        assert_eq!(none.status().await.unwrap().queue, 0, "status works without the database");

        let url = format!("sqlite:{}?mode=rwc", dir.path().join(".bot.db").display());
        let b = Backend::Local(Local::with_db(dir.path(), fake.clone(), Some(url), "me"));
        let change = b.op(&Op::SetRole { key: "admin".into(), set: RoleField::Hoist(true) }).await.unwrap();
        assert_eq!(change.files, ["roles.toml"]);
        assert!(b.schema().await.unwrap().roles.iter().any(|r| r.key == "admin" && r.hoist));
        // Applied first, so the server is linked and Discord can be asked.
        let mut e = Engine::load(fake.clone(), dir.path()).unwrap();
        let plan = e.plan(Options::default()).await.unwrap();
        e.apply(&plan, &mut |_: &chipsoc_model::Action, _: &Result<(), String>| {}).await.unwrap();
        assert!(b.drift().await.unwrap().is_clean());
        assert!(b.queue().await.unwrap().is_empty());
        assert!(b.pause_feed(1, true).await.unwrap_err().to_string().contains("no such feed"));
        assert!(b.members().await.unwrap().leads[0].project.as_deref() == Some("golem"));
        // The edit above is in the audit log, as whoever made it here.
        let done = b.activity(5).await.unwrap();
        assert_eq!(done.len(), 1, "{done:?}");
        assert_eq!((done[0].action.as_str(), done[0].actor.as_deref()), ("config_edit", Some("me")));
    }

    /// Plan, apply and pull go through the backend too, here as on the server.
    #[tokio::test]
    async fn local_plans_apply_and_pull() {
        let dir = tempfile::tempdir().unwrap();
        write_config(dir.path());
        let fake = Arc::new(FakeDiscord::new());
        let b = Backend::Local(Local::with_db(dir.path(), fake.clone(), None, "me"));
        let p = b.plan(Options::default()).await.unwrap();
        assert!(!p.plan.is_empty() && !p.lines.is_empty());
        let mut seen = 0;
        let steps = b.apply(&p, &mut |_: &chipsoc_model::Action, _: &Result<(), String>| seen += 1).await.unwrap();
        assert!(steps.iter().all(|s| s.error.is_none()) && seen == steps.len(), "{seen}");
        // Created this time; who holds roles follows on the next.
        let again = b.plan(Options::default()).await.unwrap();
        b.apply(&again, &mut |_: &chipsoc_model::Action, _: &Result<(), String>| {}).await.unwrap();
        assert!(b.plan(Options::default()).await.unwrap().plan.is_empty());
        assert!(b.apply(&p, &mut |_: &chipsoc_model::Action, _: &Result<(), String>| {}).await.is_err(), "stale");
        assert!(b.pull(false).await.unwrap().is_empty());
    }
}

/// Signing in, and each session's re-check, never wait behind a plan or apply.
#[tokio::test]
async fn sign_in_checks_dont_wait_for_the_engine() {
    use chipsoc_engine::Engine;
    let dir = tempfile::tempdir().unwrap();
    write_config(dir.path());
    write(dir.path(), "bot.toml", "name = \"Test\"\nadmin_role = \"admin\"\narchive = \"archive\"\n");
    let fake = Arc::new(FakeDiscord::new());
    let mut e = Engine::load(fake.clone(), dir.path()).unwrap();
    let plan = e.plan(chipsoc_engine::Options::default()).await.unwrap();
    e.apply(&plan, &mut |_: &chipsoc_model::Action, _: &Result<(), String>| {}).await.unwrap();
    fake.join(ADMIN);
    let admin = e.cfg.desired.lock.role("admin").unwrap();
    fake.call(chipsoc_discord::Method::Put, &format!("guilds/1000/members/{ADMIN}/roles/{admin}"), None).await.unwrap();
    let app = App {
        engine: tokio::sync::Mutex::new(e),
        rest: fake.clone(),
        dir: dir.path().to_owned(),
        guild: Default::default(),
        baseline: None,
        loaded: Default::default(),
        plans: crate::api::kept_plans(),
        db: "sqlite::memory:".into(),
        store: Default::default(),
        saves: false,
        recordings: dir.path().join("recordings"),
    };
    let _planning = app.engine.lock().await;
    let check = |user| tokio::time::timeout(Duration::from_secs(5), oauth::Members::may_use(&app, user));
    assert!(check(chipsoc_config::Snowflake(ADMIN)).await.expect("didn't wait").unwrap());
    assert!(!check(chipsoc_config::Snowflake(MEMBER)).await.expect("didn't wait").unwrap());
    // The config is read once, then again only when its files change.
    let first = app.config().await.unwrap();
    assert!(Arc::ptr_eq(&first, &app.config().await.unwrap()), "cached");
    write(dir.path(), "server.toml", "[guild]\nname = \"Renamed\"\n");
    assert_eq!(app.config().await.unwrap().desired.server.guild.name, "Renamed");
    // A mistake in the files doesn't lock admins out of the page that fixes it.
    write(dir.path(), "server.toml", "[guild\n");
    assert!(app.config().await.is_err());
    assert!(check(chipsoc_config::Snowflake(ADMIN)).await.expect("didn't wait").unwrap());
    assert!(!check(chipsoc_config::Snowflake(MEMBER)).await.expect("didn't wait").unwrap());
}

/// Where the theme links send someone back to.
async fn theme_back(query: &str) -> String {
    let req = Request::get(format!("/theme/light{query}")).body(Body::empty()).unwrap();
    let res = theme_test_router().oneshot(req).await.unwrap();
    res.headers()[LOCATION].to_str().unwrap().to_owned()
}

#[tokio::test]
async fn theme_links_go_back_to_their_page_on_this_site_only() {
    assert_eq!(theme_back("?back=/roles").await, "/roles");
    for bad in
        ["?back=//evil.example/x", "?back=https://evil.example/", "?back=/%5Cevil.example", "?back=/theme/dark", ""]
    {
        assert_eq!(theme_back(bad).await, "/", "{bad}");
    }
    let mut b = ada().await;
    let page = b.get("/roles").await;
    assert!(page.body.contains("href=\"/theme/dark?back=/roles\""), "{}", page.body);
}

#[tokio::test]
async fn the_shared_code_signs_as_shared_token() {
    let s = auth::Session { csrf: String::new(), signed_in: true, user: None };
    assert_eq!(s.actor(), Some(auth::SHARED));
    let open = auth::Session { signed_in: false, ..s };
    assert_eq!(open.actor(), None, "nobody signed in on loopback without sign-in");

    let mut b = Browser::new(Some(CODE));
    b.post("/login", &[("code", CODE)]).await;
    assert!(b.get("/").await.body.contains("Shared code"));
}

/// Red team M1: someone makes their own `login` link (their own challenge)
/// and gets an admin to open it. Continue sends the one-time code only to
/// 127.0.0.1 on the admin's own machine, and it trades for nothing without
/// the attacker's verifier... which only helps someone holding both.
#[tokio::test]
async fn an_attacker_started_cli_sign_in_lands_on_the_admins_own_machine() {
    let mut b = Browser::discord();
    let attacker = crate::remote::Verifier::new();
    let start =
        format!("/auth/cli?port=4567&state={CLI_STATE}&challenge={}&device=evil%20%60box%60", attacker.challenge);
    let page = b.cli_discord(&start, "code-ada").await;
    assert_eq!(page.status, StatusCode::OK);
    for want in ["Let a terminal sign in as you?", "evil box", "ada", "Only continue if you just ran"] {
        assert!(page.body.contains(want), "{want}: {}", page.body);
    }
    assert!(!page.body.contains("name=\"code\""), "nothing to type");
    let id = field(&page.body, "id");
    let done = b.post("/auth/cli/confirm", &[("id", &id)]).await;
    let to = done.location.unwrap_or_else(|| panic!("not sent back: {}", done.body));
    assert!(to.starts_with("http://127.0.0.1:4567/callback"), "only to this machine: {to}");
    let fields = query(to.strip_prefix("http://127.0.0.1:4567/callback").unwrap());
    assert!(!fields.contains_key("token"), "only a code goes through the browser");
    let (status, _) = b.trade(&fields["code"], "a-verifier-nobody-holds").await;
    assert_ne!(status, StatusCode::OK, "the code is useless without the CLI's verifier");

    // Cancelling issues nothing, and a used or made-up id gets nowhere.
    let page = b.cli_discord(&start, "code-ada").await;
    let id = field(&page.body, "id");
    let cancelled = b.post("/auth/cli/confirm", &[("id", &id), ("cancel", "yes")]).await;
    assert!(cancelled.body.contains("Cancelled"), "{}", cancelled.body);
    assert_eq!(b.post("/auth/cli/confirm", &[("id", &id)]).await.status, StatusCode::FORBIDDEN);
    assert_eq!(b.post("/auth/cli/confirm", &[("id", "guess")]).await.status, StatusCode::FORBIDDEN);
}

/// `--paste` mode: the code is shown, with a warning to paste it only into
/// a terminal you started yourself.
#[tokio::test]
async fn paste_mode_warns_before_and_with_the_code() {
    let mut b = Browser::discord();
    let page = b.cli_discord(&cli_start(None), "code-ada").await;
    assert!(page.body.contains("paste it only into a terminal you started yourself"), "{}", page.body);
    let shown = b.post("/auth/cli/confirm", &[("id", &field(&page.body, "id"))]).await;
    assert!(shown.body.contains("Only paste this into a terminal you started yourself"), "{}", shown.body);
}

#[tokio::test]
async fn cli_sign_ins_are_named_listed_revocable_and_logged() {
    let mut b = Browser::discord();
    write(b.dir.path(), "bot.toml", "name = \"Test\"\nadmin_role = \"admin\"\n[log]\nbot = \"general\"\n");
    write(
        b.dir.path(),
        "lock.toml",
        &format!("guild = \"1000\"\n[roles]\nadmin = \"{ADMIN_ROLE}\"\n[channels]\ngeneral = \"2000\"\n"),
    );
    let start = format!("{}&device=ada-laptop", cli_start(Some(4567)));
    let page = b.cli_sign_in(&start, "code-ada").await;
    let fields = query(page.location.as_deref().unwrap());
    let (status, body) = b.trade(&fields["code"], CLI_VERIFIER).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let ada = body["token"].as_str().unwrap().to_owned();
    let max = b.token("code-max").await;

    let logged: Vec<Value> = b.discord.state.lock().unwrap().messages.clone();
    let note = logged.iter().find(|m| m["channel_id"] == "2000").expect("the bot log heard");
    let text = note["content"].as_str().unwrap();
    assert!(text.contains(&format!("<@{ADMIN}>")) && text.contains("`ada-laptop`"), "{text}");
    assert_eq!(note["allowed_mentions"], json!({ "parse": [] }), "pings nobody");

    let (status, list) = b.api("GET", "/api/v1/tokens", Some(&ada), None).await;
    assert_eq!(status, StatusCode::OK, "{list}");
    let list = list.as_array().unwrap();
    assert_eq!(list.len(), 1, "only ada's own: {list:?}");
    assert!(list[0]["name"].as_str().unwrap().starts_with("ada-laptop · "), "{list:?}");
    assert_eq!(list[0]["current"], true);
    assert!(!serde_json::to_string(list).unwrap().contains(&ada), "the token itself is never shown");
    let id = list[0]["id"].as_str().unwrap().to_owned();

    // Someone else can't revoke it, or see it.
    assert_eq!(b.api("DELETE", &format!("/api/v1/tokens/{id}"), Some(&max), None).await.0, StatusCode::NOT_FOUND);
    assert_eq!(b.api("GET", "/api/v1/me", Some(&ada), None).await.0, StatusCode::OK);

    // On the web page too, and revoking there signs that command line out.
    b.sign_in_with_discord("code-ada").await;
    let account = b.get("/account").await;
    assert!(account.body.contains("ada-laptop") && account.body.contains("Revoke"), "{}", account.body);
    let revoked = b.submit("/account/revoke", &[("id", &id)]).await;
    assert!(b.follow(revoked).await.body.contains("Revoked"), "reloading doesn't send it again");
    assert_eq!(b.api("GET", "/api/v1/me", Some(&ada), None).await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(b.api("GET", "/api/v1/me", Some(&max), None).await.0, StatusCode::OK, "max's is untouched");

    // And with the API, for your own.
    let (_, list) = b.api("GET", "/api/v1/tokens", Some(&max), None).await;
    let id = list[0]["id"].as_str().unwrap();
    assert_eq!(b.api("DELETE", &format!("/api/v1/tokens/{id}"), Some(&max), None).await.0, StatusCode::NO_CONTENT);
    assert_eq!(b.api("GET", "/api/v1/me", Some(&max), None).await.0, StatusCode::UNAUTHORIZED);
}

/// Red team L2: cookieless sign-in starts used to push real sign-ins out.
#[tokio::test]
async fn a_flood_of_sign_ins_cannot_push_out_a_real_one() {
    let mut b = Browser::discord();
    let state = b.start_discord().await;
    let flood = b.app.clone();
    for _ in 0..1100 {
        let req = Request::get("/login").header(HOST, "127.0.0.1:8080").body(Body::empty()).unwrap();
        assert_eq!(flood.clone().oneshot(req).await.unwrap().status(), StatusCode::OK);
    }
    let page = b.get(&format!("/auth/discord?code=code-ada&state={state}")).await;
    assert_eq!(page.location.as_deref(), Some("/"), "{}", page.body);

    // A login cookie this server didn't sign is no sign-in at all.
    let mut b = Browser::discord();
    let state = b.start_discord().await;
    // One byte flipped at a fixed place: always a change, whatever the cookie holds.
    let mut forged = b.cookies["login"].clone().into_bytes();
    let i = forged.len() / 2;
    forged[i] = if forged[i] == b'A' { b'B' } else { b'A' };
    b.cookies.insert("login".into(), String::from_utf8(forged).unwrap());
    assert_eq!(b.get(&format!("/auth/discord?code=code-ada&state={state}")).await.status, StatusCode::FORBIDDEN);
    assert!(b.traded().is_empty());
}

/// Red team L2: plans were made on GET (so a link could make them) and one
/// session's plans could push out everyone else's.
#[tokio::test]
async fn one_admin_s_plans_cannot_push_out_another_s() {
    let mut b = Browser::discord();
    let (ada, max) = (b.token("code-ada").await, b.token("code-max").await);
    let (_, planned) = b.api("GET", "/api/v1/plan", Some(&ada), None).await;
    let id = planned["id"].as_str().unwrap().to_owned();
    for _ in 0..250 {
        assert_eq!(b.api("GET", "/api/v1/plan", Some(&max), None).await.0, StatusCode::OK);
    }
    let apply = json!({ "plan_id": id });
    let (status, steps) = b.api("POST", "/api/v1/apply", Some(&ada), Some(apply)).await;
    assert_eq!(status, StatusCode::OK, "ada's plan is still there: {steps}");

    // Someone else's id doesn't use up the owner's plan.
    let (_, planned) = b.api("GET", "/api/v1/plan", Some(&ada), None).await;
    let id = planned["id"].as_str().unwrap().to_owned();
    assert_eq!(
        b.api("POST", "/api/v1/apply", Some(&max), Some(json!({ "plan_id": id }))).await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(b.api("POST", "/api/v1/apply", Some(&ada), Some(json!({ "plan_id": id }))).await.0, StatusCode::OK);

    // In the browser, planning is a form with the CSRF token, not a link.
    let mut b = ada_browser().await;
    assert_eq!(b.get("/changes/plan").await.status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(b.post("/changes/plan", &[]).await.status, StatusCode::FORBIDDEN, "no CSRF token");
    assert!(b.submit("/changes/plan", &[]).await.body.contains("Plan: what would change"));
}

async fn ada_browser() -> Browser {
    let mut b = Browser::discord();
    b.sign_in_with_discord("code-ada").await;
    b
}

/// Red team L9: a client that never finishes its headers is cut off, and
/// bodies are limited.
#[tokio::test(start_paused = true)]
async fn slow_and_huge_requests_are_cut_off() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let dir = tempfile::tempdir().unwrap();
    let server = crate::testing::spawn(dir.path()).await.unwrap();
    let addr = server.url.strip_prefix("http://").unwrap().to_owned();

    let mut slow = tokio::net::TcpStream::connect(&addr).await.unwrap();
    slow.write_all(b"GET /login HTTP/1.1\r\nHost: 127.0.0.1\r\n").await.unwrap();
    let mut buf = Vec::new();
    let read = tokio::time::timeout(Duration::from_secs(60), slow.read_to_end(&mut buf)).await;
    assert!(read.is_ok(), "the connection was closed, not left open");

    let mut big = tokio::net::TcpStream::connect(&addr).await.unwrap();
    let head = format!(
        "POST /auth/cli/token HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n",
        2 << 20
    );
    big.write_all(head.as_bytes()).await.unwrap();
    let _ = big.write_all(&vec![b' '; 2 << 20]).await;
    let mut reply = Vec::new();
    let _ = big.read_to_end(&mut reply).await;
    let reply = String::from_utf8_lossy(&reply);
    assert!(reply.starts_with("HTTP/1.1 413"), "{reply}");
}

#[tokio::test]
async fn the_browser_hears_how_a_command_line_sign_in_went() {
    let mut b = Browser::discord();
    let done = b.get("/auth/cli/done").await;
    assert_eq!(done.status, StatusCode::OK);
    assert!(done.body.contains("✓ Signed in") && done.body.contains("close this tab"), "{}", done.body);
    let failed = b.get("/auth/cli/failed?why=expired").await;
    assert!(failed.body.contains("expired") && failed.body.contains("chipsoc-bot login"), "{}", failed.body);
    let odd = b.get("/auth/cli/failed?why=%3Cb%3Eyou%20were%20hacked%3C%2Fb%3E").await;
    assert!(odd.body.contains("Signing in didn") && !odd.body.contains("hacked"), "a link can't put words here");
}

#[tokio::test]
async fn what_members_told_the_bot_is_shown_on_the_members_page_and_api() {
    let mut b = ada().await;
    let p = chipsoc_store::Profile {
        user_id: MEMBER.to_string(),
        name: "Ada".into(),
        surname: "Byron".into(),
        company: "Arm".into(),
    };
    b.store().await.guild("1000").set_profile(&p, 1).await.unwrap();
    let page = b.get(&format!("/members?about={MEMBER}")).await;
    assert!(page.body.contains("Look someone up") && page.body.contains("Ada Byron"), "{}", page.body);
    assert!(page.body.contains("<dd>Arm</dd>"), "{}", page.body);
    assert!(b.get("/members?about=nope").await.body.contains("Nobody in the server is called"));
    let token = b.token("code-ada").await;
    let (status, a) = b.api("GET", &format!("/api/v1/members/about/{MEMBER}"), Some(&token), None).await;
    assert_eq!((status, &a["company"]), (StatusCode::OK, &json!("Arm")), "{a}");
}

/// Continue redirects the browser to the CLI on 127.0.0.1, so the page's
/// policy must allow that form target (browsers hold redirects to it).
#[tokio::test]
async fn the_cli_confirmation_page_may_hand_back_to_loopback() {
    let mut b = Browser::discord();
    let csp = b.cli_discord(&cli_start(Some(4567)), "code-ada").await.csp.unwrap();
    assert!(csp.contains("form-action 'self' http://127.0.0.1:*"), "{csp}");
    assert!(csp.contains("default-src 'none'"), "{csp}");
}

impl Browser {
    /// The web server restarted (a deploy): a new app on the same config, database and Discord.
    fn restart(&mut self) {
        let engine = Engine::load(self.discord.clone(), self.dir.path()).unwrap();
        let oauth = self.oauth.clone().expect("signing in with Discord");
        self.app = router(engine, db_url(self.dir.path()), discord_sign_in(Box::new(oauth)));
    }
}

/// Every deploy used to sign everyone out: sessions and command-line
/// sign-ins were kept in memory only. Now they're in the database, hashed.
#[tokio::test]
async fn sign_ins_survive_a_restart_and_so_do_revocations() {
    let mut b = ada().await;
    let token = b.token("code-ada").await;
    let other = b.token("code-ada").await;
    b.restart();
    let home = b.get("/").await;
    assert!(home.body.contains("ada") && home.body.contains("Sign out"), "the browser is still signed in");
    let (status, me) = b.api("GET", "/api/v1/me", Some(&token), None).await;
    assert_eq!((status, me["user"].as_str()), (StatusCode::OK, Some("ada")), "and the command line");
    let (_, listed) = b.api("GET", "/api/v1/tokens", Some(&token), None).await;
    let listed = listed.as_array().unwrap().clone();
    assert_eq!(listed.len(), 2, "{listed:?}");
    assert_eq!(listed.iter().filter(|t| t["current"] == true).count(), 1, "{listed:?}");
    let saved = b.submit("/projects/golem/status", &[("status", "past"), ("confirm", "yes")]).await;
    assert_eq!(saved.location.as_deref(), Some("/projects?saved=golem#golem"), "forms still carry a good CSRF token");

    // Only hashes are kept: the database alone signs nobody in.
    for f in [".bot.db", ".bot.db-wal"] {
        let bytes = fs::read(b.dir.path().join(f)).unwrap_or_default();
        let text = String::from_utf8_lossy(&bytes);
        assert!(!text.contains(&token) && !text.contains(&other), "{f} holds a token");
    }

    // Revoked or signed out stays so after another restart.
    let id = listed.iter().find(|t| t["current"] == false).unwrap()["id"].as_str().unwrap().to_owned();
    assert_eq!(b.api("DELETE", &format!("/api/v1/tokens/{id}"), Some(&token), None).await.0, StatusCode::NO_CONTENT);
    let cookie = b.cookies["session"].clone();
    b.submit("/logout", &[]).await;
    b.restart();
    assert_eq!(b.api("GET", "/api/v1/me", Some(&other), None).await.0, StatusCode::UNAUTHORIZED, "revoked");
    b.cookies.insert("session".into(), cookie);
    assert_eq!(b.get("/").await.location.as_deref(), Some("/login"), "signed out");
    assert_eq!(b.api("GET", "/api/v1/me", Some(&token), None).await.0, StatusCode::OK);
}

/// The browser's sign-in uses PKCE too: what's sent to Discord matches the link's challenge.
#[tokio::test]
async fn browser_sign_in_uses_pkce() {
    let mut b = Browser::discord();
    let page = b.get("/login").await;
    let start = "href=\"https://discord.com/oauth2/authorize?";
    let at = page.body.find(start).unwrap() + start.len();
    let fields = query(&format!("?{}", unescape(&page.body[at..at + page.body[at..].find('"').unwrap()])));
    assert_eq!(fields["code_challenge_method"], "S256");
    let state = &fields["state"];
    let done = b.get(&format!("/auth/discord?code=code-ada&state={state}")).await;
    assert_eq!(done.location.as_deref(), Some("/"), "{}", done.body);
    let sent = b.oauth.as_ref().unwrap().verifiers.lock().unwrap().clone();
    assert_eq!(sent.len(), 1);
    assert_eq!(s256(&sent[0]), fields["code_challenge"]);
}

/// Deleting for real takes the server owner, whichever admin plans it.
#[tokio::test]
async fn only_the_owner_applies_a_plan_that_deletes() {
    let mut b = Browser::discord();
    let (ada, olive) = (b.token("code-ada").await, b.token("code-olive").await);
    for (who, ok) in [(&ada, false), (&olive, true)] {
        let (_, planned) = b.api("GET", "/api/v1/plan?delete=true", Some(who), None).await;
        let id = planned["id"].as_str().unwrap();
        let (status, body) =
            b.api("POST", "/api/v1/apply", Some(who), Some(json!({ "plan_id": id, "plan": { "actions": [] } }))).await;
        if ok {
            assert_eq!(status, StatusCode::OK, "{body}");
        } else {
            assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
            assert!(body["error"].as_str().unwrap().starts_with("Only the server owner can delete"), "{body}");
        }
    }
}

fn git_in(dir: &std::path::Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git").arg("-C").arg(dir).args(args).output().unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

/// The config's history says who changed what; only the owner reverts, and
/// it shows the plan to apply.
#[tokio::test]
async fn the_history_names_who_and_only_the_owner_reverts() {
    let mut b = Browser::discord();
    let dir = b.dir.path().to_owned();
    fs::write(dir.join(".gitignore"), ".bot.db*\n").unwrap();
    for args in [&["init", "-q", "-b", "main"][..], &["config", "user.email", "t@t"], &["config", "user.name", "t"]] {
        git_in(&dir, args);
    }
    git_in(&dir, &["add", "-A"]);
    git_in(&dir, &["commit", "-qm", "start"]);
    let (ada, olive) = (b.token("code-ada").await, b.token("code-olive").await);
    let past = json!({ "op": "set_project", "slug": "golem", "set": { "status": "past" } });
    assert_eq!(b.api("POST", "/api/v1/ops", Some(&ada), Some(past)).await.0, StatusCode::OK);
    // Here a person commits (the bot does it itself where it autosaves), signed as the bot signs.
    git_in(
        &dir,
        &[
            "commit",
            "-qam",
            &format!("Project golem: status past by ada\n\n{}: {ADMIN}", chipsoc_config::git::BY_TRAILER),
        ],
    );
    assert!(b.projects_toml().contains("status = \"past\""));

    let (_, changes) = b.api("GET", "/api/v1/changes", Some(&ada), None).await;
    let theirs = &changes[0];
    assert_eq!((theirs["who"].as_str(), theirs["who_id"].as_str()), (Some("ada"), Some(ADMIN.to_string().as_str())));
    let id = theirs["commit"].as_str().unwrap().to_owned();
    let (_, mine) = b.api("GET", &format!("/api/v1/changes?user={ADMIN}"), Some(&ada), None).await;
    assert!(mine.as_array().unwrap().iter().any(|d| d["commit"] == id.as_str()), "{mine}");

    let undo = json!({ "commit": id, "how": "revert" });
    let (status, body) = b.api("POST", "/api/v1/undo", Some(&ada), Some(undo.clone())).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert!(body["error"].as_str().unwrap().starts_with("Only the server owner can revert"), "{body}");
    assert!(b.projects_toml().contains("status = \"past\""), "unchanged");
    let (status, body) = b.api("POST", "/api/v1/undo", Some(&olive), Some(undo)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body["subject"].as_str().unwrap().starts_with("Revert \"Project golem"), "{body}");
    assert!(b.projects_toml().contains("status = \"active\""), "back as it was");
    let last = git_in(&dir, &["log", "-1", "--format=%s%n%b"]);
    assert!(
        last.contains("by olive") && last.contains(&format!("{}: {OWNER}", chipsoc_config::git::BY_TRAILER)),
        "{last}"
    );

    // The page: a Revert button for the owner only, and the plan after reverting.
    b.sign_in_with_discord("code-ada").await;
    let page = b.get("/changes/history").await;
    assert!(
        page.body.contains("Only the server owner can revert") && !page.body.contains("Rewind to here"),
        "{}",
        page.body
    );
    b.cookies.clear();
    b.sign_in_with_discord("code-olive").await;
    let page = b.get("/changes/history").await;
    assert!(page.body.contains("Revert…") && page.body.contains("Rewind to here…"), "{}", page.body);
    assert!(
        page.body.contains("Yes, rewind") && page.body.contains("Every later change is undone."),
        "both ask first: {}",
        page.body
    );
    let latest = git_in(&dir, &["rev-parse", "--short", "HEAD"]);
    let done = b.submit("/changes/undo", &[("commit", &latest), ("how", "revert")]).await;
    assert_eq!(done.location.as_deref(), Some("/changes"), "redirected, so a reload doesn't go back again");
    let next = b.get("/changes").await;
    assert!(next.body.contains("Saved as a new change: Revert") && next.body.contains("See the plan"), "{}", next.body);
    assert!(b.projects_toml().contains("status = \"past\""), "the revert reverted");
}

#[tokio::test]
async fn people_are_named_and_can_be_appointed_by_name() {
    let mut b = ada().await;
    write(b.dir.path(), "members.toml", "admin = []\n");
    {
        let mut st = b.discord.state.lock().unwrap();
        for m in st.members.iter_mut() {
            let id = m["user"]["id"].as_str().unwrap_or_default().to_owned();
            if id == MEMBER.to_string() {
                m["user"]["username"] = json!("mel");
                m["user"]["global_name"] = json!("Melody");
            } else if id == MANAGER.to_string() {
                m["user"]["username"] = json!("max");
                m["nick"] = json!("Max Melton");
            }
        }
    }
    // An @username is found; the config here can't be saved, so it's refused for that, not for who.
    let found = b.submit("/members/appoint", &[("role", "admin"), ("user", "@mel")]).await;
    assert!(found.body.contains("synced git checkout") && !found.body.contains("Nobody in"), "{}", found.body);
    // Part of two names: never a guess; both are offered to pick.
    let unsure = b.submit("/members/appoint", &[("role", "admin"), ("user", "el")]).await;
    assert_eq!(unsure.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(unsure.body.contains("Which one?") && unsure.body.contains("Melody (@mel)"), "{}", unsure.body);
    assert!(unsure.body.contains("Max Melton (@max)") && unsure.body.contains("name=\"pick\""), "{}", unsure.body);
    let picked =
        b.submit("/members/appoint", &[("role", "admin"), ("user", "el"), ("pick", &MEMBER.to_string())]).await;
    assert!(picked.body.contains("synced git checkout"), "{}", picked.body);

    // Looking someone up by name, and the queue naming who applied.
    assert!(b.get("/members?about=melody").await.body.contains("Melody"), "found by display name");
    let unsure = b.get("/members?about=el").await;
    assert!(unsure.body.contains(&format!("href=\"/members?about={MEMBER}#about\"")), "{}", unsure.body);
    let db = b.store().await;
    db.guild("1000")
        .add_application(chipsoc_store::AppKind::Position, "admin", &MEMBER.to_string(), "[]", false, 1)
        .await
        .unwrap();
    let queue = b.get("/queue").await;
    assert!(queue.body.contains("From <span title=\"Discord user id 502\">Melody</span>"), "{}", queue.body);

    // The API finds people for the CLI and TUI.
    let token = b.token("code-ada").await;
    let (status, found) = b.api("GET", "/api/v1/members/find?q=%40max", Some(&token), None).await;
    assert_eq!(status, StatusCode::OK, "{found}");
    assert_eq!((found["exact"].as_bool(), found["members"][0]["name"].as_str()), (Some(true), Some("Max Melton")));
}

#[tokio::test]
async fn pages_name_the_server_and_mark_the_theme() {
    let mut b = ada().await;
    let page = b.get("/queue").await;
    assert!(page.body.contains("<title>Queue · Test</title>"), "{}", page.body);
    assert!(page.body.contains("class=\"theme-auto\" aria-current=\"true\""), "{}", page.body);
    assert!(page.body.contains("aria-label=\"Theme\""), "{}", page.body);
}

#[tokio::test]
async fn recordings_are_listed_played_and_deleted_only_on_purpose() {
    use chipsoc_voice::store::{self, Meta, Participant, Person, Span, Track, Transcript};
    let mut b = ada().await;
    let root = b.dir.path().join("recordings");
    let (id, dir) = store::create(&root, 1000, 1_780_000_000, "meeting").unwrap();
    let mut m = Meta::new(id.clone(), 1000, 5, "#meeting", 1_780_000_000, Person { id: ADMIN, name: "Ada".into() });
    m.ended_at = Some(1_780_000_065);
    m.ended_because = Some("stopped by Ada".into());
    m.length_ms = 65_000;
    m.participants.push(Participant {
        id: ADMIN,
        name: "Ada".into(),
        spans: vec![Span { from_ms: 0, to_ms: Some(65_000) }],
    });
    m.tracks.push(Track { user: Some(ADMIN), name: "Ada".into(), file: "501.ogg".into(), bytes: 4, speech: vec![] });
    m.mix = Some(store::MIX.into());
    m.transcript = Transcript::Done;
    fs::write(dir.join("501.ogg"), "OggS").unwrap();
    fs::write(dir.join(store::MIX), "OggSmix").unwrap();
    fs::write(dir.join(store::TRANSCRIPT), "# Transcript: #meeting\n\nAda [00:01]: Hello there.\n").unwrap();
    store::save(&dir, &m).unwrap();

    let list = b.get("/recordings").await;
    for want in ["#meeting", "1m 05s", "Ada", "ready", &format!("/recordings/{id}")] {
        assert!(list.body.contains(want), "{want}: {}", list.body);
    }
    let one = b.get(&format!("/recordings/{id}")).await;
    assert_eq!(one.status, StatusCode::OK);
    assert!(one.body.contains("Ada [00:01]: Hello there."), "{}", one.body);
    assert!(one.body.contains("<audio"), "{}", one.body);
    assert!(one.csp.unwrap().contains("media-src 'self'"), "the player may load the audio");

    let track = b.get(&format!("/recordings/{id}/files/501.ogg")).await;
    assert_eq!((track.status, track.body.as_str()), (StatusCode::OK, "OggS"));
    let part = b
        .send(Request::get(format!("/recordings/{id}/files/{}", store::MIX)).header("range", "bytes=4-"), Body::empty())
        .await;
    assert_eq!((part.status, part.body.as_str()), (StatusCode::PARTIAL_CONTENT, "mix"), "players can seek");
    for path in [
        format!("/recordings/{id}/files/meta.json"),
        format!("/recordings/{id}/files/..%2Fmeta.json"),
        "/recordings/nope/files/501.ogg".into(),
    ] {
        assert_eq!(b.get(&path).await.status, StatusCode::NOT_FOUND, "{path}");
    }

    // Not while its transcript is being made (review 4 #25).
    store::save(&dir, &Meta { transcript: Transcript::Making, ..m.clone() }).unwrap();
    let page = b.submit("/recordings/delete", &[("id", &id), ("confirm", "yes")]).await;
    let shown = b.follow(page).await;
    assert!(shown.body.contains("transcript is being made"), "{}", shown.body);
    assert!(dir.exists());
    assert!(store::delete(&root, 1000, &id).is_err(), "nor by anything else");
    store::save(&dir, &m).unwrap();

    // Deleting needs the confirmation, and is logged.
    let page = b.submit("/recordings/delete", &[("id", &id)]).await;
    assert!(page.location.is_some());
    assert!(dir.exists(), "not without the Yes");
    let page = b.submit("/recordings/delete", &[("id", &id), ("confirm", "yes")]).await;
    let shown = b.follow(page).await;
    assert!(shown.body.contains("Deleted the recording of #meeting"), "{}", shown.body);
    assert!(!dir.exists());
    let audit = b.store().await.guild("1000").recent_audit(5).await.unwrap();
    let e = audit.iter().find(|a| a.action == "recording_deleted").expect("audited");
    assert_eq!(e.target.as_deref(), Some(id.as_str()));
    assert_eq!(e.actor.as_deref(), Some(ADMIN.to_string().as_str()));
}

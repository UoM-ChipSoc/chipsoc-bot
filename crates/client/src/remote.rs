//! The other end of the web server's JSON API (`/api/v1`): what the CLI and
//! TUI send and get back, a client for it, and the sign-in `login` keeps in
//! the user's config dir.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
pub use chipsoc_admin::{Decided, Done, Drift, Entry, FeedInfo, NewResource, Past, Pending, Post, Roster};
use chipsoc_config::ops::{Change, Op, Schema};
use chipsoc_config::template::Rendered;
use chipsoc_config::{Project, ProjectStatus, Snowflake};
use chipsoc_engine::{Problems, Pulled};
use chipsoc_model::{Action, Plan};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

/// How long a read may take before it's given up.
const QUICK: Duration = Duration::from_secs(60);
/// How long a plan may take (it exports the whole server).
const SLOW: Duration = Duration::from_secs(300);

/// A plan the server made and keeps under `id` for a short while.
#[derive(Serialize, Deserialize)]
pub struct Planned {
    pub id: String,
    pub plan: Plan,
    /// The plan as `plan` prints it.
    pub lines: Vec<String>,
}

#[derive(Serialize, Deserialize)]
pub struct ApplyRequest {
    pub plan_id: String,
    /// Only these steps (indexes into the plan's actions); all of them when absent.
    #[serde(default)]
    pub steps: Option<Vec<usize>>,
}

/// How one applied step went.
#[derive(Serialize, Deserialize)]
pub struct Step {
    pub action: Action,
    pub error: Option<String>,
}

#[derive(Serialize, Deserialize, Default)]
pub struct PullRequest {
    /// Also copy role permissions.
    #[serde(default)]
    pub permissions: bool,
}

#[derive(Serialize, Deserialize)]
pub struct Checked {
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
}

#[derive(Serialize, Deserialize)]
pub struct ProjectList {
    pub projects: Vec<Project>,
    /// The areas the config suggests for new projects.
    pub areas: Vec<String>,
}

/// A project to add; also the web form for it.
#[derive(Serialize, Deserialize, Default)]
#[serde(default)]
pub struct NewProject {
    pub name: String,
    pub blurb: String,
    pub area: String,
    pub status: ProjectStatus,
}

#[derive(Serialize, Deserialize)]
pub struct SetStatus {
    pub status: ProjectStatus,
}

#[derive(Serialize, Deserialize)]
pub struct SetApplications {
    pub open: bool,
}

/// The project a change was saved to.
#[derive(Serialize, Deserialize)]
pub struct Saved {
    pub slug: String,
}

/// Accept or reject an application in the review queue.
#[derive(Serialize, Deserialize)]
pub struct Decision {
    pub accept: bool,
    /// Told to the applicant and kept with the application.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// The version of the entry that was shown ([`chipsoc_admin::Pending::version`]):
    /// Accept is refused if it changed since.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seen: Option<String>,
}

/// An announcement or event to share with partners.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShareRequest {
    /// `announcement` or `event`.
    pub what: String,
    /// The message's link, or the event's link or id.
    pub item: String,
    /// Partners' names or ids; empty: every partner we send that kind to.
    #[serde(default)]
    pub partners: Vec<String>,
}

/// Whom something was shared with.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Shared {
    pub partners: Vec<String>,
}

/// A feed to follow.
#[derive(Serialize, Deserialize)]
pub struct NewFeed {
    pub url: String,
}

#[derive(Serialize, Deserialize)]
pub struct Pause {
    pub paused: bool,
}

/// Someone to give a role members.toml lists, or make a project lead.
#[derive(Serialize, Deserialize)]
pub struct Appoint {
    /// The role key (ignored for leads).
    #[serde(default)]
    pub role: String,
    pub user: Snowflake,
}

/// The server at a glance, without asking Discord.
#[derive(Serialize, Deserialize, Default)]
pub struct Status {
    pub name: String,
    /// Whether the config has been applied to a server.
    pub linked: bool,
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
    /// Applications waiting for a decision.
    pub queue: i64,
    pub roles: usize,
    pub channels: usize,
    pub projects: usize,
    pub positions: usize,
}

impl Status {
    /// For `cfg`, with `queue` applications waiting.
    pub fn of(cfg: &chipsoc_config::Config, queue: i64) -> Self {
        let report = chipsoc_config::authoring::check_config(cfg);
        Self {
            name: cfg.desired.server.guild.name.clone(),
            linked: cfg.guild().is_some(),
            errors: report.errors,
            warnings: report.warnings,
            queue,
            roles: cfg.desired.roles.roles.len(),
            channels: cfg.desired.channels().count(),
            projects: cfg.projects.len(),
            positions: cfg.bot.positions.len(),
        }
    }
}

/// What `POST /ops` takes: one op, or several made as one change.
#[derive(Serialize, Deserialize)]
#[serde(untagged)]
pub enum Ops {
    Many(Vec<Op>),
    One(Box<Op>),
}

impl Ops {
    pub fn into_vec(self) -> Vec<Op> {
        match self {
            Self::Many(v) => v,
            Self::One(op) => vec![*op],
        }
    }
}

/// Markdown to render as a managed post would be.
#[derive(Serialize, Deserialize)]
pub struct Preview {
    pub body: String,
}

/// What an action did, in a sentence or two.
#[derive(Serialize, Deserialize)]
pub struct Note {
    pub text: String,
}

/// `POST /undo`: go back on a config commit.
#[derive(Serialize, Deserialize)]
pub struct UndoRequest {
    pub commit: String,
    pub how: chipsoc_config::git::Undo,
}

/// What `POST /undo` made: the new commit's subject, if anything changed,
/// and what went wrong after it was saved (pushing it), if anything.
#[derive(Serialize, Deserialize)]
pub struct Undone {
    pub subject: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
}

#[derive(Serialize, Deserialize)]
pub struct Me {
    pub user: String,
    /// The community the server runs (bot.toml `name`).
    #[serde(default)]
    pub server: String,
}

/// A server's current setup as config files (path → text), for `export`.
#[derive(Serialize, Deserialize)]
pub struct Exported {
    pub files: std::collections::BTreeMap<String, String>,
}

/// An API token, traded for a sign-in's one-time code.
#[derive(Serialize, Deserialize)]
pub struct Token {
    pub token: String,
}

/// A command-line sign-in, as its owner sees it in the list (never the token).
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct TokenInfo {
    /// What to revoke it by.
    pub id: String,
    /// The device it was made on, and the day.
    pub name: String,
    pub created: String,
    pub expires: String,
    /// The token asking.
    #[serde(default)]
    pub current: bool,
}

/// What this computer calls itself, for the sign-in page and the list of sign-ins.
pub fn this_device() -> String {
    let from_file = |p: &str| std::fs::read_to_string(p).ok();
    let from_cmd = || {
        let out = std::process::Command::new("hostname").output().ok()?;
        out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
    };
    from_file("/proc/sys/kernel/hostname")
        .or_else(|| from_file("/etc/hostname"))
        .or_else(|| std::env::var("COMPUTERNAME").ok())
        .or_else(from_cmd)
        .map(|h| h.trim().to_owned())
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "a computer".into())
}

/// The CLI's half of a sign-in (PKCE, RFC 7636): a secret verifier kept
/// here, and its S256 challenge, which goes in the sign-in link.
pub struct Verifier {
    pub secret: String,
    pub challenge: String,
}

/// `bytes` random bytes from the OS, as hex: for tokens, states and secrets.
pub fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    getrandom::fill(&mut buf).expect("the OS random number generator failed");
    hex(&buf)
}

/// `bytes` as lower-case hex.
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

impl Verifier {
    pub fn new() -> Self {
        let (challenge, secret) = oauth2::PkceCodeChallenge::new_random_sha256();
        Self { secret: secret.secret().clone(), challenge: challenge.as_str().to_owned() }
    }
}

impl Default for Verifier {
    fn default() -> Self {
        Self::new()
    }
}

/// Trade a sign-in's one-time code, with the verifier it started with, for an API token.
pub async fn trade(server: &str, code: &str, verifier: &Verifier) -> Result<String> {
    let http = reqwest::Client::builder().connect_timeout(Duration::from_secs(15)).user_agent("chipsoc-bot").build()?;
    let body = serde_json::json!({ "code": code.trim(), "verifier": verifier.secret });
    let res = http
        .post(format!("{server}/auth/cli/token"))
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(body.to_string())
        .send()
        .await
        .with_context(|| format!("reaching {server}"))?;
    let status = res.status();
    let bytes = res.bytes().await.with_context(|| format!("reading {server}'s answer"))?;
    if !status.is_success() {
        let r: Refusal = serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| Refusal { error: format!("{server} said {status}"), problems: Vec::new() });
        bail!("{}", r.error);
    }
    Ok(serde_json::from_slice::<Token>(&bytes).with_context(|| format!("reading {server}'s answer"))?.token)
}

/// Why the server said no.
#[derive(Serialize, Deserialize)]
pub struct Refusal {
    pub error: String,
    /// Config mistakes that stop a plan.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub problems: Vec<String>,
}

/// Signed in to a bot's web server, as saved by `login`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Login {
    pub server: String,
    pub token: String,
    /// The Discord username, for display.
    pub user: String,
}

impl Login {
    /// `$XDG_CONFIG_HOME/chipsoc-bot/remote.toml`, else `~/.config/…`.
    pub fn path() -> Option<PathBuf> {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
        Some(base.join("chipsoc-bot").join("remote.toml"))
    }

    pub fn load(path: &Path) -> Result<Option<Self>> {
        match std::fs::read_to_string(path) {
            Ok(s) => Ok(Some(toml::from_str(&s).with_context(|| format!("reading {}", path.display()))?)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    /// Write it where only this user can read it: it's as good as a password.
    pub fn save(&self, path: &Path) -> Result<()> {
        let dir = path.parent().context("no directory for the sign-in file")?;
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        let tmp = path.with_extension("tmp");
        let mut open = std::fs::File::options();
        open.write(true).create(true).truncate(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut open, 0o600);
        let mut f = open.open(&tmp).with_context(|| format!("writing {}", tmp.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // In case the file was already there with looser permissions.
            f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        std::io::Write::write_all(&mut f, toml::to_string(self)?.as_bytes())?;
        std::fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))
    }
}

/// The server's address without a trailing slash; plain http only to this machine.
pub fn server_url(server: &str) -> Result<String> {
    let url = server.trim().trim_end_matches('/');
    let host = |rest: &str| rest.split(['/', '?', '#']).next().unwrap_or_default().to_owned();
    let host = match (url.strip_prefix("https://"), url.strip_prefix("http://")) {
        (Some(rest), _) => host(rest),
        (_, Some(rest)) => {
            let h = host(rest);
            let name = if h.starts_with('[') { h.split_inclusive(']').next() } else { h.split(':').next() };
            let name = name.unwrap_or_default();
            if !matches!(name, "localhost" | "127.0.0.1" | "[::1]") {
                bail!("{url} isn't https, so the sign-in would cross the network unencrypted");
            }
            h
        }
        _ => bail!("give the server as an address like https://bot.example.ts.net"),
    };
    if host.is_empty() {
        bail!("give the server as an address like https://bot.example.ts.net");
    }
    Ok(url.to_owned())
}

/// Talks to the API as whoever `login` signed in.
pub struct Client {
    pub login: Login,
    http: reqwest::Client,
}

impl Client {
    pub fn new(login: Login) -> Result<Self> {
        // No overall timeout: an apply can take a while.
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .user_agent("chipsoc-bot")
            .build()
            .context("setting up HTTPS")?;
        Ok(Self { login, http })
    }

    pub async fn me(&self) -> Result<String> {
        Ok(self.get::<Me>("me").await?.user)
    }

    /// Who this sign-in is, and on which community.
    pub async fn whoami(&self) -> Result<Me> {
        self.get("me").await
    }

    /// The server's current setup, as a new config's files.
    pub async fn export(&self) -> Result<Exported> {
        self.get("export").await
    }

    pub async fn plan(&self, delete: bool, sweep: bool) -> Result<Planned> {
        self.get(&format!("plan?delete={delete}&sweep={sweep}")).await
    }

    /// Apply all of the plan the server keeps as `plan_id`.
    pub async fn apply(&self, plan_id: &str) -> Result<Vec<Step>> {
        self.post("apply", &ApplyRequest { plan_id: plan_id.into(), steps: None }).await
    }

    pub async fn pull(&self, permissions: bool) -> Result<Vec<Pulled>> {
        self.post("pull", &PullRequest { permissions }).await
    }

    pub async fn check(&self) -> Result<Checked> {
        self.get("check").await
    }

    pub async fn projects(&self) -> Result<ProjectList> {
        self.get("projects").await
    }

    pub async fn add_project(&self, p: &NewProject) -> Result<String> {
        Ok(self.post::<Saved>("projects", p).await?.slug)
    }

    pub async fn set_status(&self, slug: &str, status: ProjectStatus) -> Result<()> {
        self.post::<Saved>(&format!("projects/{slug}/status"), &SetStatus { status }).await.map(drop)
    }

    pub async fn set_applications(&self, slug: &str, open: bool) -> Result<()> {
        self.post::<Saved>(&format!("projects/{slug}/applications"), &SetApplications { open }).await.map(drop)
    }

    /// The server at a glance (no Discord calls).
    pub async fn status(&self) -> Result<Status> {
        self.get("status").await
    }

    /// How Discord differs from the config, by who changed what.
    pub async fn drift(&self) -> Result<Drift> {
        self.get("drift").await
    }

    /// What can be edited, with the current values.
    pub async fn schema(&self) -> Result<Schema> {
        self.get("ops/schema").await
    }

    /// One config edit, checked first; refused with the problems if it would break the config.
    pub async fn op(&self, op: &Op) -> Result<Change> {
        self.post("ops", op).await
    }

    /// Config edits made as one change: one check, one commit, all or nothing.
    pub async fn ops(&self, list: &[Op]) -> Result<Change> {
        self.post("ops", &list).await
    }

    /// A post's markdown as Discord gets it.
    pub async fn preview(&self, body: &str) -> Result<Rendered> {
        self.post("ops/preview", &Preview { body: body.into() }).await
    }

    pub async fn queue(&self) -> Result<Vec<Pending>> {
        self.get("queue").await
    }

    /// How the join flow is going.
    pub async fn join(&self) -> Result<chipsoc_admin::join::Progress> {
        self.get("join").await
    }

    /// Who the join flow's one-off notice would go to.
    pub async fn join_notice(&self) -> Result<chipsoc_admin::join::Notice> {
        self.get("join/notify").await
    }

    /// Send it; the server sends in the background and tells its bot log.
    pub async fn send_join_notice(&self) -> Result<chipsoc_admin::join::Notice> {
        self.post("join/notify", &serde_json::json!({})).await
    }

    pub async fn history(&self, limit: usize) -> Result<Vec<Past>> {
        self.get(&format!("queue/history?limit={limit}")).await
    }

    /// Accept or reject an application, telling them `reason` if given.
    pub async fn decide(&self, id: i64, accept: bool, reason: Option<&str>) -> Result<Decided> {
        self.post(&format!("queue/{id}"), &Decision { accept, reason: reason.map(str::to_owned), seen: None }).await
    }

    pub async fn make_lead(&self, id: i64) -> Result<String> {
        Ok(self.post::<Note>(&format!("queue/{id}/lead"), &serde_json::json!({})).await?.text)
    }

    pub async fn feeds(&self) -> Result<Vec<FeedInfo>> {
        self.get("feeds").await
    }

    /// Follow a feed; what happened.
    pub async fn add_feed(&self, url: &str) -> Result<String> {
        Ok(self.post::<Note>("feeds", &NewFeed { url: url.into() }).await?.text)
    }

    pub async fn pause_feed(&self, id: i64, paused: bool) -> Result<()> {
        self.post::<serde::de::IgnoredAny>(&format!("feeds/{id}/pause"), &Pause { paused }).await.map(drop)
    }

    pub async fn remove_feed(&self, id: i64) -> Result<()> {
        self.delete(&format!("feeds/{id}")).await
    }

    pub async fn resources(&self, limit: usize) -> Result<Vec<Post>> {
        self.get(&format!("resources?limit={limit}")).await
    }

    pub async fn add_resource(&self, r: &NewResource) -> Result<Post> {
        self.post("resources", r).await
    }

    /// Close and lock a resource's post (it isn't deleted).
    pub async fn archive_resource(&self, id: Snowflake) -> Result<()> {
        self.delete(&format!("resources/{id}")).await
    }

    /// Share an announcement or event with partners; whom it went to.
    pub async fn share(&self, r: &ShareRequest) -> Result<Shared> {
        self.post("share", r).await
    }

    pub async fn ideas(&self, limit: usize) -> Result<Vec<Post>> {
        self.get(&format!("ideas?limit={limit}")).await
    }

    pub async fn activity(&self, limit: usize) -> Result<Vec<Entry>> {
        self.get(&format!("activity?limit={limit}")).await
    }

    /// The config's latest changes, or what `user` did this week.
    pub async fn changes(&self, user: Option<Snowflake>, limit: usize) -> Result<Vec<Done>> {
        let user = user.map(|u| format!("&user={u}")).unwrap_or_default();
        self.get(&format!("changes?limit={limit}{user}")).await
    }

    /// Go back on a config commit (the server owner only).
    pub async fn undo(
        &self,
        commit: &str,
        how: chipsoc_config::git::Undo,
    ) -> Result<Option<chipsoc_config::git::Undid>> {
        let req = UndoRequest { commit: commit.to_owned(), how };
        let made = self.post::<Undone>("undo", &req).await?;
        Ok(made.subject.map(|subject| chipsoc_config::git::Undid { subject, warning: made.warning }))
    }

    /// Who holds what, and who left holding something.
    pub async fn members(&self) -> Result<Roster> {
        self.get("members").await
    }

    /// What `user` told the bot: their profile and join answers.
    pub async fn about(&self, user: Snowflake) -> Result<chipsoc_admin::join::About> {
        self.get(&format!("members/about/{user}")).await
    }

    /// Who a username or name (`@ada`, `Ada Lovelace`) could be.
    pub async fn find_members(&self, query: &str) -> Result<chipsoc_admin::Found> {
        let q: String = query
            .bytes()
            .map(|b| match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
                _ => format!("%{b:02X}"),
            })
            .collect();
        self.get(&format!("members/find?q={q}")).await
    }

    /// Give `user` a role members.toml lists; what happened.
    pub async fn appoint(&self, role: &str, user: Snowflake) -> Result<String> {
        Ok(self.post::<Note>("members", &Appoint { role: role.into(), user }).await?.text)
    }

    /// Take `user` out of members.toml for `role`; what happened.
    pub async fn unappoint(&self, role: &str, user: Snowflake) -> Result<String> {
        Ok(self.delete_with::<Note>(&format!("members/{role}/{user}")).await?.text)
    }

    pub async fn add_lead(&self, slug: &str, user: Snowflake) -> Result<String> {
        Ok(self.post::<Note>(&format!("projects/{slug}/leads"), &Appoint { role: String::new(), user }).await?.text)
    }

    pub async fn remove_lead(&self, slug: &str, user: Snowflake) -> Result<String> {
        Ok(self.delete_with::<Note>(&format!("projects/{slug}/leads/{user}")).await?.text)
    }

    /// Give someone who came back their old roles; what happened.
    pub async fn restore(&self, user: Snowflake) -> Result<String> {
        Ok(self.post::<Note>(&format!("members/former/{user}/restore"), &serde_json::json!({})).await?.text)
    }

    /// Stop offering someone's old roles back.
    pub async fn forget(&self, user: Snowflake) -> Result<()> {
        self.delete(&format!("members/former/{user}")).await
    }

    /// This user's command-line sign-ins.
    pub async fn tokens(&self) -> Result<Vec<TokenInfo>> {
        self.get("tokens").await
    }

    /// Revoke this sign-in's token on the server.
    pub async fn logout(&self) -> Result<()> {
        let res = self.http.post(self.url("logout")).bearer_auth(&self.login.token).send().await?;
        if !res.status().is_success() {
            bail!("the server said {}", res.status());
        }
        Ok(())
    }

    fn url(&self, path: &str) -> String {
        format!("{}/api/v1/{path}", self.login.server)
    }

    /// Reads answer quickly or not at all; only planning (a whole export) may take a while.
    async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        let wait = if path.starts_with("plan") { SLOW } else { QUICK };
        self.send(self.http.get(self.url(path)).timeout(wait)).await
    }

    async fn post<T: DeserializeOwned>(&self, path: &str, body: &impl Serialize) -> Result<T> {
        let req = self.http.post(self.url(path)).header(reqwest::header::CONTENT_TYPE, "application/json");
        self.send(req.body(serde_json::to_vec(body)?)).await
    }

    /// A DELETE answered with no body.
    async fn delete(&self, path: &str) -> Result<()> {
        self.send::<serde::de::IgnoredAny>(self.http.delete(self.url(path))).await.map(drop)
    }

    async fn delete_with<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        self.send(self.http.delete(self.url(path))).await
    }

    async fn send<T: DeserializeOwned>(&self, req: reqwest::RequestBuilder) -> Result<T> {
        let server = &self.login.server;
        let res = req.bearer_auth(&self.login.token).send().await.with_context(|| format!("reaching {server}"))?;
        let status = res.status();
        let bytes = res.bytes().await.with_context(|| format!("reading {server}'s answer"))?;
        if status.is_success() {
            // 204 No Content: read as JSON `null`.
            let bytes: &[u8] = if bytes.is_empty() { b"null" } else { &bytes };
            return serde_json::from_slice(bytes).with_context(|| format!("reading {server}'s answer"));
        }
        let r: Refusal = serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| Refusal { error: format!("{server} said {status}"), problems: Vec::new() });
        if !r.problems.is_empty() {
            return Err(Problems(r.problems).into());
        }
        if status == reqwest::StatusCode::UNAUTHORIZED {
            bail!("{} Sign in again: `chipsoc-bot login --server {server}`", r.error);
        }
        bail!("{}", r.error)
    }
}

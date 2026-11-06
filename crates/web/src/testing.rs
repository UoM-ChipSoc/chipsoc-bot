//! The web server on a pretend Discord, with a pretend Discord sign-in: for
//! this crate's tests and, with the `testing` feature, other crates'.

use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use chipsoc_config::Snowflake;
use chipsoc_discord::fake::FakeDiscord;
use chipsoc_engine::Engine;
use serde_json::json;

use crate::auth::SignIn;
use crate::oauth::{BoxFuture, Discord, HttpRequest, HttpResponse, OAuth, User};

/// Discord's OAuth endpoints: each code belongs to a user.
pub(crate) struct FakeOAuth {
    pub users: HashMap<String, User>,
}

impl OAuth for FakeOAuth {
    fn send(&self, req: HttpRequest) -> BoxFuture<'_, Result<HttpResponse>> {
        Box::pin(async move { Ok(self.answer(&req)) })
    }
}

/// A token request's form fields (empty for any other request).
pub(crate) fn token_form(req: &HttpRequest) -> HashMap<String, String> {
    if !req.uri().path().ends_with("/oauth2/token") {
        return HashMap::new();
    }
    serde_urlencoded::from_bytes(req.body()).unwrap_or_default()
}

impl FakeOAuth {
    /// What Discord says to `req`: a token for a known code (sent with the
    /// app's id and secret, and a PKCE verifier), and who a token is.
    fn answer(&self, req: &HttpRequest) -> HttpResponse {
        let reply = |status: u16, body: serde_json::Value| {
            http::Response::builder()
                .status(status)
                .header("content-type", "application/json")
                .body(body.to_string().into_bytes())
                .unwrap()
        };
        if req.uri().path().ends_with("/oauth2/token") {
            let f = token_form(req);
            let field = |k: &str| f.get(k).map(String::as_str).unwrap_or_default();
            assert_eq!((field("client_id"), field("client_secret")), ("777", "client secret"));
            assert_eq!(field("redirect_uri"), "https://ui.example/auth/discord");
            assert_eq!(field("grant_type"), "authorization_code");
            assert!(field("code_verifier").len() >= 43, "PKCE is used: {f:?}");
            if !self.users.contains_key(field("code")) {
                return reply(400, json!({ "error": "invalid_grant" }));
            }
            return reply(200, json!({ "access_token": format!("token-{}", field("code")), "token_type": "Bearer" }));
        }
        let auth = req.headers().get("authorization").and_then(|v| v.to_str().ok()).unwrap_or_default();
        let code = auth.strip_prefix("Bearer token-").unwrap_or_default();
        match self.users.get(code) {
            Some(u) if req.uri().path().ends_with("/users/@me") => {
                reply(200, json!({ "id": u.id.to_string(), "username": u.name }))
            }
            _ => reply(401, json!({ "message": "401: Unauthorized" })),
        }
    }
}

pub(crate) const ADMIN: u64 = 501;
pub(crate) const MEMBER: u64 = 502;
pub(crate) const MANAGER: u64 = 503;
pub(crate) const OWNER: u64 = 504;
pub(crate) const STRANGER: u64 = 505;
/// The `admin` role's id in the lockfile.
pub(crate) const ADMIN_ROLE: &str = "1500";

pub(crate) fn write(dir: &Path, file: &str, body: &str) {
    let p = dir.join(file);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, body).unwrap();
}

/// A small config: a few roles and channels, and one active project.
pub(crate) fn write_config(p: &Path) {
    write(p, "server.toml", "[guild]\nname = \"Test\"\n");
    write(p, "bot.toml", "name = \"Test\"\narchive = \"archive\"\n[projects]\nforum = \"projects\"\n");
    write(
        p,
        "roles.toml",
        "[[role]]\nkey = \"bot\"\nname = \"Bot\"\nmanaged = true\n\n[[role]]\nkey = \"admin\"\nname = \"Admin\"\n\n\
         [[role]]\nkey = \"everyone\"\nname = \"@everyone\"\npermissions = [\"VIEW_CHANNEL\"]\n",
    );
    write(
        p,
        "categories/10-main.toml",
        "[category]\nkey = \"main\"\nname = \"Main\"\n\n[[channel]]\nkey = \"general\"\nname = \"general\"\n\n\
         [[channel]]\nkey = \"projects\"\ntype = \"forum\"\nname = \"projects\"\ntags = [\"active\", \"past\"]\n",
    );
    write(p, "categories/90-archive.toml", "[category]\nkey = \"archive\"\nname = \"Archive\"\n");
    write(
        p,
        "projects.toml",
        "# Keep these in the order they started.\n[[project]]\nslug = \"golem\"\nname = \"Golem\"\nblurb = \"a CPU\"\n\
         status = \"active\"\nchannels = [\"general\"]\n",
    );
}

/// A server where `admin` is the admin role: ada holds it, max has Manage
/// Server, olive owns it, and mel and sam can't sign in. Codes are `code-<name>`.
pub(crate) fn committee(dir: &Path, discord: &FakeDiscord) -> FakeOAuth {
    write(dir, "bot.toml", "name = \"Test\"\nadmin_role = \"admin\"\n");
    write(dir, "lock.toml", &format!("guild = \"1000\"\n[roles]\nadmin = \"{ADMIN_ROLE}\"\n"));
    let mut st = discord.state.lock().unwrap();
    st.guild.insert("owner_id".into(), json!(OWNER.to_string()));
    // Mods have Manage Server, below the bot's own role so it can still manage the rest.
    st.roles.iter_mut().filter(|r| r["managed"] == true).for_each(|r| r["position"] = json!(2));
    st.roles.push(json!({ "id": "1600", "name": "Mods", "position": 1, "permissions": "32", "managed": false }));
    let member = |id: u64, roles: &[&str]| json!({ "user": { "id": id.to_string() }, "roles": roles });
    st.members.extend([
        member(ADMIN, &[ADMIN_ROLE]),
        member(MEMBER, &[]),
        member(MANAGER, &["1600"]),
        member(OWNER, &[]),
    ]);
    let user = |id: u64, name: &str| (format!("code-{name}"), User { id: Snowflake(id), name: name.into() });
    let users =
        [user(ADMIN, "ada"), user(MEMBER, "mel"), user(MANAGER, "max"), user(OWNER, "olive"), user(STRANGER, "sam")];
    FakeOAuth { users: users.into_iter().collect() }
}

pub(crate) fn discord_sign_in(oauth: Box<dyn OAuth>) -> SignIn {
    let base_url = "https://ui.example".into();
    SignIn::Discord(Discord { client_id: "777".into(), client_secret: "client secret".into(), base_url, oauth })
}

/// A running test server.
pub struct Server {
    pub url: String,
    pub discord: Arc<FakeDiscord>,
}

/// Serve a small config in `dir` on a random loopback port, against a fresh
/// pretend Discord, signing in as in [`committee`].
pub async fn spawn(dir: &Path) -> Result<Server> {
    write_config(dir);
    let discord = Arc::new(FakeDiscord::new());
    let oauth = committee(dir, &discord);
    let engine = Engine::load(discord.clone(), dir)?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}", listener.local_addr()?);
    let app = crate::router(engine, "sqlite::memory:".into(), discord_sign_in(Box::new(oauth)));
    tokio::spawn(crate::run(listener, app));
    Ok(Server { url, discord })
}

/// What a browser does with `login`'s link when signed in to Discord with
/// `code`: follow the redirects, through Discord and back, confirm the code
/// the terminal shows, and on to the CLI. The last page, which shows the
/// code to paste when the link has no port.
pub async fn browse(link: &str, code: &str) -> Result<String> {
    let http = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build()?;
    let header = |res: &reqwest::Response, name: &str| -> Result<String> {
        let v = res.headers().get(name).with_context(|| format!("no {name} from {}", res.url()))?;
        Ok(v.to_str()?.to_owned())
    };
    let res = http.get(link).send().await?;
    let (cookie, to_discord) = (header(&res, "set-cookie")?, header(&res, "location")?);
    let cookie = cookie.split(';').next().unwrap_or_default();
    let query = to_discord.split_once('?').context("no query for Discord")?.1;
    let fields: HashMap<String, String> = serde_urlencoded::from_str(query)?;
    let base = link.split("/auth/cli").next().unwrap_or_default();
    let back = format!("{base}/auth/discord?code={code}&state={}", fields["state"]);
    let page = http.get(back).header("cookie", cookie).send().await?.text().await?;
    let id = page.split("name=\"id\" value=\"").nth(1).and_then(|s| s.split('"').next());
    let id = id.with_context(|| format!("no confirmation on {page}"))?;
    let form = serde_urlencoded::to_string([("id", id)])?;
    let confirm =
        http.post(format!("{base}/auth/cli/confirm")).header("content-type", "application/x-www-form-urlencoded");
    let res = confirm.body(form).send().await?;
    let res = match res.headers().get("location") {
        Some(to) => http.get(to.to_str()?).send().await?,
        None => res,
    };
    Ok(res.text().await?)
}

/// An API token for `code`'s user, as `login --paste` gets one.
pub async fn token(server: &str, code: &str) -> Result<String> {
    let verifier = crate::remote::Verifier::new();
    let link = format!("{server}/auth/cli?state=0123456789abcdef&challenge={}", verifier.challenge);
    let page = browse(&link, code).await?;
    let once = page.split("<code>").nth(1).and_then(|s| s.split("</code>").next());
    let once = once.with_context(|| format!("no code on {page}"))?;
    crate::remote::trade(server, once, &verifier).await
}

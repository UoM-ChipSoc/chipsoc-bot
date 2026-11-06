//! Talking to Discord for the reconcile engine: one small REST seam ([`Rest`]),
//! a live implementation on twilight (which handles rate limits), an in-memory
//! [`fake::FakeDiscord`] for tests, export to a [`Snapshot`](chipsoc_config::Snapshot), and the executor.

pub mod exec;
pub mod export;
pub mod fake;
pub mod wire;

use std::future::Future;

use anyhow::Result;
use chipsoc_config::Snowflake;
use serde_json::Value;
pub use twilight_http::request::Method;

/// An error response from Discord, kept so callers can match on the status.
#[derive(Debug, Clone, thiserror::Error)]
#[error("Discord said {status}: {message}")]
pub struct ApiError {
    pub status: u16,
    pub message: String,
    /// Discord's own error code, when it sent one (50007: can't DM this user).
    pub code: Option<u64>,
}

impl ApiError {
    /// The status of an [`ApiError`] inside an `anyhow` chain.
    pub fn status_of(e: &anyhow::Error) -> Option<u16> {
        e.downcast_ref::<Self>().map(|a| a.status)
    }

    /// Discord's error code of an [`ApiError`] inside an `anyhow` chain.
    pub fn code_of(e: &anyhow::Error) -> Option<u64> {
        e.downcast_ref::<Self>().and_then(|a| a.code)
    }
}

/// Discord's error code for a DM to someone who doesn't take them.
pub const CANNOT_DM: u64 = 50007;

/// Discord's REST API, as JSON in and out. Paths are relative to `/api/v10/`.
pub trait Rest: Send + Sync {
    fn call(&self, method: Method, path: &str, body: Option<Value>) -> impl Future<Output = Result<Value>> + Send;

    /// Everyone in `guild`, as Discord lists members (see
    /// [`export::fetch_members`]). A client that keeps the list up to date
    /// itself (the bot, from the gateway) gives its copy instead.
    fn members(&self, guild: Snowflake) -> impl Future<Output = Result<std::sync::Arc<Vec<Value>>>> + Send {
        async move { export::fetch_members(self, guild).await.map(std::sync::Arc::new) }
    }
}

/// A shared client: lets a test keep a handle on a fake it hands over.
impl<T: Rest> Rest for std::sync::Arc<T> {
    fn call(&self, method: Method, path: &str, body: Option<Value>) -> impl Future<Output = Result<Value>> + Send {
        (**self).call(method, path, body)
    }

    fn members(&self, guild: Snowflake) -> impl Future<Output = Result<std::sync::Arc<Vec<Value>>>> + Send {
        (**self).members(guild)
    }
}

/// The real thing, through twilight's client (shared, so rate limits are too).
#[derive(Clone)]
pub struct Http {
    client: std::sync::Arc<twilight_http::Client>,
}

impl Http {
    pub fn new(token: String) -> Self {
        Self::shared(std::sync::Arc::new(twilight_http::Client::new(token)))
    }

    /// With no token, for what Discord shows anyone (public templates).
    pub fn anonymous() -> Self {
        Self::shared(std::sync::Arc::new(twilight_http::Client::builder().build()))
    }

    pub fn shared(client: std::sync::Arc<twilight_http::Client>) -> Self {
        Self { client }
    }
}

impl Rest for Http {
    async fn call(&self, method: Method, path: &str, body: Option<Value>) -> Result<Value> {
        use twilight_http::error::ErrorType;
        let mut req = twilight_http::request::RequestBuilder::raw(method, path.to_owned());
        if by_token(path) {
            // As twilight's own interaction and webhook requests: their token
            // is in the path, and a refusal (an expired interaction) mustn't
            // count against the bot's token, which twilight would then stop using.
            req = req.use_authorization_token(false);
        }
        if let Some(mut b) = body {
            no_pings_unless_asked(&mut b);
            req = req.json(&b);
        }
        match self.client.request::<Value>(req.build()?).await {
            Ok(resp) => {
                let bytes = resp.bytes().await?;
                Ok(if bytes.is_empty() { Value::Null } else { serde_json::from_slice(&bytes)? })
            }
            Err(e) => match e.kind() {
                ErrorType::Response { status, body, .. } => {
                    let v: Value = serde_json::from_slice(body).unwrap_or_default();
                    let message = v["message"].as_str().unwrap_or("no details").to_owned();
                    Err(ApiError { status: status.get(), message, code: v["code"].as_u64() }.into())
                }
                _ => Err(e.into()),
            },
        }
    }
}

/// Discord with the bot's token if there is one; without, a stand-in whose
/// every call fails saying how to set one (`why`), so the files can still be
/// read and edited.
#[derive(Clone)]
pub enum MaybeHttp {
    Http(Http),
    NoToken { why: String },
}

impl MaybeHttp {
    /// With `DISCORD_TOKEN` if it's set.
    pub fn from_env(why: impl Into<String>) -> Self {
        std::env::var("DISCORD_TOKEN").map_or_else(|_| Self::NoToken { why: why.into() }, |t| Self::Http(Http::new(t)))
    }
}

impl Rest for MaybeHttp {
    async fn call(&self, method: Method, path: &str, body: Option<Value>) -> Result<Value> {
        match self {
            Self::Http(h) => h.call(method, path, body).await,
            Self::NoToken { why } => anyhow::bail!("{why}"),
        }
    }
}

/// Member and feed text is untrusted: a message pings nobody unless it says
/// whom (`allowed_mentions`). Covers a message's own body and one sent inside
/// an interaction reply (`data`) or a new forum post (`message`).
pub(crate) fn no_pings_unless_asked(body: &mut Value) {
    let Some(obj) = body.as_object_mut() else { return };
    for inner in ["data", "message"] {
        if let Some(v) = obj.get_mut(inner) {
            no_pings_unless_asked(v);
        }
    }
    // A modal (a form: `custom_id` + `title`) isn't a message and takes no mentions.
    let modal = obj.contains_key("custom_id") && obj.contains_key("title");
    let says_something = !modal && ["content", "embeds", "components"].iter().any(|k| obj.contains_key(*k));
    if says_something && !obj.contains_key("allowed_mentions") {
        obj.insert("allowed_mentions".into(), serde_json::json!({ "parse": [] }));
    }
}

/// Whether a path carries its own token (an interaction's, or a webhook's)
/// and so is sent without the bot's.
fn by_token(path: &str) -> bool {
    let parts: Vec<&str> = path.split('?').next().unwrap_or_default().split('/').collect();
    matches!(parts.as_slice(), ["interactions" | "webhooks", _, _, ..])
}

/// The bot's own role in a server (Discord makes one per bot), and whether it
/// sits strictly above every other role, which the bot needs to manage them all.
pub async fn bot_role(rest: &impl Rest, guild: Snowflake) -> Result<Option<(Snowflake, bool)>> {
    let me = rest.call(Method::Get, "users/@me", None).await?;
    let g = rest.call(Method::Get, &format!("guilds/{guild}"), None).await?;
    Ok(bot_role_in(&g, &me["id"]))
}

/// [`bot_role`] from a guild already fetched, for the bot whose user id is `me`.
pub fn bot_role_in(g: &Value, me: &Value) -> Option<(Snowflake, bool)> {
    let roles: Vec<&Value> = g["roles"].as_array().into_iter().flatten().collect();
    let mine = roles.iter().find(|r| r["tags"]["bot_id"] == *me)?;
    let pos = |r: &Value| r["position"].as_i64().unwrap_or(0);
    let on_top = roles
        .iter()
        .filter(|r| r["id"] != mine["id"] && !r["managed"].as_bool().unwrap_or(false))
        .all(|r| pos(r) < pos(mine));
    wire::id(&mine["id"]).map(|id| (id, on_top))
}

/// The names of the roles at or above `bot`'s role that a bot could manage
/// (not other bots' or integrations'), top first: Discord won't let it touch them.
pub fn roles_above(g: &Value, bot: Snowflake) -> Vec<String> {
    let roles: Vec<&Value> = g["roles"].as_array().into_iter().flatten().collect();
    let pos = |r: &Value| r["position"].as_i64().unwrap_or(0);
    let Some(mine) = roles.iter().find(|r| wire::id(&r["id"]) == Some(bot)).map(|r| pos(r)) else {
        return Vec::new();
    };
    let mut above: Vec<&&Value> = roles
        .iter()
        .filter(|r| wire::id(&r["id"]) != Some(bot) && !r["managed"].as_bool().unwrap_or(false) && pos(r) >= mine)
        .collect();
    above.sort_by_key(|r| -pos(r));
    above.iter().map(|r| wire::s(&r["name"])).collect()
}

/// The guilds the bot is in, as (id, name).
pub async fn guilds(rest: &impl Rest) -> Result<Vec<(Snowflake, String)>> {
    let v = rest.call(Method::Get, "users/@me/guilds", None).await?;
    Ok(v.as_array().into_iter().flatten().filter_map(|g| Some((wire::id(&g["id"])?, wire::s(&g["name"])))).collect())
}

/// The one guild the bot is in, or an error listing them.
pub async fn sole_guild(rest: &impl Rest) -> Result<(Snowflake, String)> {
    match guilds(rest).await?.as_slice() {
        [g] => Ok(g.clone()),
        [] => anyhow::bail!("the bot isn't in any server yet; invite it first"),
        many => {
            let list: Vec<String> = many.iter().map(|(id, name)| format!("{name} ({id})")).collect();
            anyhow::bail!("the bot is in several servers; say which with --guild: {}", list.join(", "))
        }
    }
}

/// A public template (`discord.new/<code>`) as a config snapshot: roles and
/// channels only. Template ids are local (from 0), so the lockfile is dropped.
pub async fn template(rest: &impl Rest, code: &str) -> Result<chipsoc_config::Snapshot> {
    let code = code.trim().trim_start_matches("https://").trim_start_matches("discord.new/");
    let t = rest.call(Method::Get, &format!("guilds/templates/{code}"), None).await?;
    let g = t["serialized_source_guild"].clone();
    let stand_in = Static { guild: g.clone(), channels: g["channels"].clone() };
    let mut snap = export::export(&stand_in, Snowflake(0), &chipsoc_config::Lock::default()).await?;
    snap.lock = chipsoc_config::Lock::default();
    Ok(snap)
}

/// Serves a template's guild and channels as if they were live.
struct Static {
    guild: Value,
    channels: Value,
}

impl Rest for Static {
    async fn call(&self, method: Method, path: &str, _: Option<Value>) -> Result<Value> {
        match (method, path) {
            (Method::Get, "guilds/0") => Ok(self.guild.clone()),
            (Method::Get, "guilds/0/channels") => Ok(self.channels.clone()),
            _ => Err(ApiError { status: 404, message: "not in a template".into(), code: None }.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn interaction_and_webhook_tokens_go_in_the_path_only() {
        assert!(super::by_token("interactions/1/abc/callback"));
        assert!(super::by_token("webhooks/1/abc/messages/@original"));
        assert!(!super::by_token("webhooks/1"), "managing a webhook needs the bot's token");
        assert!(!super::by_token("channels/1/messages"));
    }
}

#[cfg(test)]
mod ping_tests {
    use super::no_pings_unless_asked;
    use serde_json::json;

    #[test]
    fn messages_ping_nobody_unless_they_say_whom() {
        let mut reply = json!({ "type": 4, "data": { "content": "@everyone hi" } });
        no_pings_unless_asked(&mut reply);
        assert_eq!(reply["data"]["allowed_mentions"], json!({ "parse": [] }));
        let mut post = json!({ "name": "t", "message": { "content": "x" } });
        no_pings_unless_asked(&mut post);
        assert_eq!(post["message"]["allowed_mentions"], json!({ "parse": [] }));
        let mut asked = json!({ "content": "<@1>", "allowed_mentions": { "users": ["1"] } });
        no_pings_unless_asked(&mut asked);
        assert_eq!(asked["allowed_mentions"], json!({ "users": ["1"] }), "kept as asked");
        let mut modal = json!({ "type": 9, "data": { "custom_id": "f", "title": "Form", "components": [] } });
        no_pings_unless_asked(&mut modal);
        assert!(modal["data"].get("allowed_mentions").is_none(), "forms take none");
        let mut role = json!({ "name": "Role" });
        no_pings_unless_asked(&mut role);
        assert!(role.get("allowed_mentions").is_none(), "not a message");
    }
}

/// [`Http`] against a real HTTP server standing in for Discord (twilight
/// aimed at it as a proxy): what goes out, and what comes back.
#[cfg(test)]
mod http_tests {
    use std::sync::{Arc, Mutex};

    use serde_json::json;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;

    /// A request as the server got it: "METHOD /path", its Authorization header, its body.
    type Got = (String, Option<String>, String);

    /// Answers by path: canned refusals, an empty 204, else `{}`.
    fn answer(path: &str) -> (u16, String) {
        match path {
            "/api/v10/channels/1/messages" => {
                (403, json!({ "message": "Cannot send messages to this user", "code": 50007 }).to_string())
            }
            "/api/v10/channels/2" => (404, json!({ "message": "Unknown Channel", "code": 10003 }).to_string()),
            "/api/v10/users/@me" => (200, json!({ "id": "42" }).to_string()),
            _ => (204, String::new()),
        }
    }

    async fn serve(listener: tokio::net::TcpListener, got: Arc<Mutex<Vec<Got>>>) {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else { return };
            let got = got.clone();
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                let head_end = loop {
                    let n = sock.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        break i + 4;
                    }
                };
                let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
                let header = |name: &str| {
                    head.lines().find_map(|l| {
                        let (k, v) = l.split_once(':')?;
                        k.eq_ignore_ascii_case(name).then(|| v.trim().to_owned())
                    })
                };
                let len: usize = header("content-length").and_then(|l| l.parse().ok()).unwrap_or(0);
                while buf.len() < head_end + len {
                    let n = sock.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                }
                let line: Vec<&str> = head.lines().next().unwrap_or_default().split(' ').collect();
                let (method, path) = (line[0].to_owned(), line.get(1).copied().unwrap_or_default().to_owned());
                let body = String::from_utf8_lossy(&buf[head_end..]).to_string();
                got.lock().unwrap().push((format!("{method} {path}"), header("authorization"), body));
                let (status, reply) = answer(&path);
                let resp = format!(
                    "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{reply}",
                    reply.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await;
            });
        }
    }

    #[tokio::test]
    async fn errors_are_mapped_empty_answers_are_null_and_tokens_go_only_where_due() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let got: Arc<Mutex<Vec<Got>>> = Arc::default();
        tokio::spawn(serve(listener, got.clone()));
        let client = twilight_http::Client::builder()
            .token("secret".into())
            .proxy(addr, true)
            .timeout(std::time::Duration::from_secs(5))
            .build();
        let http = Http::shared(Arc::new(client));

        let me = http.call(Method::Get, "users/@me", None).await.unwrap();
        assert_eq!(me["id"], "42");
        let e = http.call(Method::Post, "channels/1/messages", Some(json!({ "content": "hi" }))).await.unwrap_err();
        assert_eq!((ApiError::status_of(&e), ApiError::code_of(&e)), (Some(403), Some(CANNOT_DM)));
        assert!(e.to_string().contains("Cannot send messages to this user"), "{e}");
        let e = http.call(Method::Get, "channels/2", None).await.unwrap_err();
        assert_eq!((ApiError::status_of(&e), ApiError::code_of(&e)), (Some(404), Some(10003)));
        let gone = http.call(Method::Delete, "channels/3/messages/4", None).await.unwrap();
        assert_eq!(gone, Value::Null, "204: nothing to read");
        let reply = json!({ "type": 4, "data": { "content": "@everyone" } });
        assert_eq!(http.call(Method::Post, "interactions/5/tok/callback", Some(reply)).await.unwrap(), Value::Null);
        http.call(Method::Post, "webhooks/6/tok", Some(json!({ "content": "x" }))).await.unwrap();

        let got = got.lock().unwrap().clone();
        let find = |call: &str| got.iter().find(|g| g.0 == call).unwrap_or_else(|| panic!("{call} in {got:?}")).clone();
        assert_eq!(find("GET /api/v10/users/@me").1.as_deref(), Some("Bot secret"));
        assert_eq!(find("POST /api/v10/channels/1/messages").1.as_deref(), Some("Bot secret"));
        let (_, auth, body) = find("POST /api/v10/interactions/5/tok/callback");
        assert_eq!(auth, None, "an interaction's token is in its path");
        let body: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(body["data"]["allowed_mentions"], json!({ "parse": [] }), "pings nobody");
        assert_eq!(find("POST /api/v10/webhooks/6/tok").1, None, "nor a webhook's");
    }
}

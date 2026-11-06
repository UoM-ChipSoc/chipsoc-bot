//! "Sign in with Discord": the OAuth2 authorization-code flow with PKCE
//! (scope `identify`, through the `oauth2` crate), then the bot's own view of the server decides who may use the
//! UI: holders of `admin_role`, Administrator or Manage Server, or the owner.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use chipsoc_config::Snowflake;
use oauth2::basic::BasicClient;
use oauth2::{
    AuthType, AuthUrl, AuthorizationCode, ClientId, ClientSecret, CsrfToken, PkceCodeChallenge, PkceCodeVerifier,
    RedirectUrl, RequestTokenError, Scope, TokenResponse, TokenUrl,
};
use serde::Deserialize;

pub(crate) type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Whoever signed in with Discord.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct User {
    pub id: Snowflake,
    pub name: String,
}

/// One request to Discord's OAuth2 token endpoint or its API, and the answer.
pub(crate) type HttpRequest = oauth2::HttpRequest;
pub(crate) type HttpResponse = oauth2::HttpResponse;

/// The way to Discord, behind a seam so tests can stand in for it. The
/// OAuth2 flow itself (the authorize link, PKCE, trading the code) is the
/// `oauth2` crate's.
pub(crate) trait OAuth: Send + Sync {
    fn send(&self, req: HttpRequest) -> BoxFuture<'_, Result<HttpResponse>>;
}

/// A failed request to Discord, as `oauth2` wants errors.
#[derive(Debug)]
pub(crate) struct SendError(String);

impl std::fmt::Display for SendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for SendError {}

/// [`OAuth`] as `oauth2`'s HTTP client.
struct Via<'a>(&'a dyn OAuth);

impl<'c> oauth2::AsyncHttpClient<'c> for Via<'_> {
    type Error = SendError;
    type Future = BoxFuture<'c, Result<HttpResponse, SendError>>;

    fn call(&'c self, req: HttpRequest) -> Self::Future {
        Box::pin(async move { self.0.send(req).await.map_err(|e| SendError(format!("{e:#}"))) })
    }
}

/// Asks the bot whether a Discord user may use the UI, and tells its log.
pub(crate) trait Members: Send + Sync {
    fn may_use(&self, user: Snowflake) -> BoxFuture<'_, Result<bool>>;
    /// Post `text` in the bot log (`log.bot`), pinging nobody; failures are only logged.
    fn note(&self, text: String) -> BoxFuture<'_, ()>;
}

/// Sign-in settings, from `APPLICATION_ID`, `DISCORD_CLIENT_SECRET` and `WEB_URL`.
pub(crate) struct Discord {
    pub client_id: String,
    pub client_secret: String,
    /// The public address, without a trailing slash.
    pub base_url: String,
    pub oauth: Box<dyn OAuth>,
}

impl Discord {
    /// Discord sign-in, if `DISCORD_CLIENT_SECRET` and `WEB_URL` are set.
    pub fn from_env() -> Result<Option<Self>> {
        let var = |name| std::env::var(name).ok().map(|v| v.trim().to_owned()).filter(|v| !v.is_empty());
        let Some((secret, url, app)) =
            Self::settings(var("DISCORD_CLIENT_SECRET"), var("WEB_URL"), var("APPLICATION_ID"))?
        else {
            return Ok(None);
        };
        Ok(Some(Self { client_id: app, client_secret: secret, base_url: url, oauth: Box::new(Live::new()?) }))
    }

    /// The checked (secret, base URL, client id), or none when sign-in with Discord is off.
    pub fn settings(
        secret: Option<String>,
        url: Option<String>,
        app: Option<String>,
    ) -> Result<Option<(String, String, String)>> {
        let (secret, url) = match (secret, url) {
            (None, None) => return Ok(None),
            (Some(s), Some(u)) => (s, u),
            (Some(_), None) => {
                bail!("DISCORD_CLIENT_SECRET is set but WEB_URL isn't; set both to sign in with Discord")
            }
            (None, Some(_)) => {
                bail!("WEB_URL is set but DISCORD_CLIENT_SECRET isn't; set both to sign in with Discord")
            }
        };
        let app = app.context("set APPLICATION_ID (the OAuth2 client id) to sign in with Discord")?;
        if app.parse::<u64>().is_err() {
            bail!("APPLICATION_ID should be the application's numeric id");
        }
        let url = url.trim_end_matches('/').to_owned();
        let host = url.strip_prefix("https://").or_else(|| url.strip_prefix("http://"));
        if host.is_none_or(|h| h.is_empty() || h.contains(['?', '#', ' '])) {
            bail!("WEB_URL should be the UI's public address, like https://vps.example.ts.net");
        }
        Ok(Some((secret, url, app)))
    }

    pub fn secure(&self) -> bool {
        self.base_url.starts_with("https://")
    }

    /// Where Discord sends people back to; add it under OAuth2 → Redirects.
    pub fn redirect_uri(&self) -> String {
        format!("{}/auth/discord", self.base_url)
    }

    fn client(&self) -> Result<Client> {
        Ok(BasicClient::new(ClientId::new(self.client_id.clone()))
            .set_client_secret(ClientSecret::new(self.client_secret.clone()))
            .set_auth_uri(AuthUrl::new(AUTHORIZE.to_owned())?)
            .set_token_uri(TokenUrl::new(format!("{API}/oauth2/token"))?)
            .set_redirect_uri(RedirectUrl::new(self.redirect_uri())?)
            // Discord takes the client's id and secret in the form.
            .set_auth_type(AuthType::RequestBody))
    }

    /// Discord's sign-in page, carrying `state` and the PKCE `challenge`.
    pub fn authorize_url(&self, state: &CsrfToken, challenge: PkceCodeChallenge) -> Result<String> {
        let (url, _) = self
            .client()?
            .authorize_url(|| state.clone())
            .add_scope(Scope::new("identify".to_owned()))
            .set_pkce_challenge(challenge)
            .url();
        Ok(url.to_string())
    }

    /// Who the code from Discord's redirect belongs to, traded with the
    /// PKCE `verifier` the sign-in started with.
    pub async fn user(&self, code: String, verifier: PkceCodeVerifier) -> Result<User> {
        let token = self
            .client()?
            .exchange_code(AuthorizationCode::new(code))
            .set_pkce_verifier(verifier)
            .request_async(&Via(&*self.oauth))
            .await
            .map_err(|e| match e {
                // Discord's error, never the request (it holds the secret).
                RequestTokenError::ServerResponse(r) => anyhow::anyhow!(
                    "Discord refused the sign-in code: {}",
                    r.error_description().cloned().unwrap_or_else(|| r.error().to_string())
                ),
                other => anyhow::anyhow!("trading the sign-in code: {other}"),
            })?;
        let req = http::Request::get(format!("{API}/v10/users/@me"))
            .header(http::header::AUTHORIZATION, format!("Bearer {}", token.access_token().secret()))
            .body(Vec::new())?;
        let res = self.oauth.send(req).await.context("asking Discord who signed in")?;
        if !res.status().is_success() {
            bail!("Discord said {} when asked who signed in", res.status());
        }
        let me: Me = serde_json::from_slice(res.body()).context("reading the Discord user")?;
        Ok(User { id: me.id, name: me.username })
    }
}

type Client = oauth2::basic::BasicClient<
    oauth2::EndpointSet,
    oauth2::EndpointNotSet,
    oauth2::EndpointNotSet,
    oauth2::EndpointNotSet,
    oauth2::EndpointSet,
>;

const AUTHORIZE: &str = "https://discord.com/oauth2/authorize";

/// The real Discord, over HTTPS.
struct Live {
    http: reqwest::Client,
}

pub(crate) const API: &str = "https://discord.com/api";

impl Live {
    fn new() -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .user_agent("chipsoc-bot web UI")
            // Never follow a redirect with the client's secret in hand.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .context("setting up HTTPS")?;
        Ok(Self { http })
    }
}

#[derive(Deserialize)]
struct Me {
    id: Snowflake,
    username: String,
}

impl OAuth for Live {
    fn send(&self, req: HttpRequest) -> BoxFuture<'_, Result<HttpResponse>> {
        Box::pin(async move {
            let (parts, body) = req.into_parts();
            let res =
                self.http.request(parts.method, parts.uri.to_string()).headers(parts.headers).body(body).send().await?;
            let mut out = http::Response::builder().status(res.status());
            for (k, v) in res.headers() {
                out = out.header(k, v);
            }
            Ok(out.body(res.bytes().await?.to_vec())?)
        })
    }
}

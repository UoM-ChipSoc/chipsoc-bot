//! Remote mode: the commands that reach Discord, through the bot's web server
//! instead of the bot's token. `login` signs in there with Discord.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use chipsoc_client::remote::{Client, Login, Verifier, server_url, this_device, trade};
use chipsoc_config::authoring::Report;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use crate::{BIN, author};

/// How long `login` waits for the browser.
const LOGIN_WAIT: Duration = Duration::from_secs(5 * 60);

/// Where `login` keeps the sign-in.
pub fn file() -> Result<PathBuf> {
    Login::path().context("set HOME (or XDG_CONFIG_HOME) so the sign-in has somewhere to live")
}

/// Beside the sign-in file: the last server signed in to (no secrets), kept
/// after `logout`, so `login` needs no `--server` next time.
fn last_file(file: &Path) -> PathBuf {
    file.with_file_name("last-server")
}

/// The server to sign in to when none is given: the saved sign-in's, else the last one.
pub fn last_server(file: &Path) -> Option<String> {
    let saved = Login::load(file).ok().flatten().map(|l| l.server);
    saved.or_else(|| {
        std::fs::read_to_string(last_file(file)).ok().map(|s| s.trim().to_owned()).filter(|s| !s.is_empty())
    })
}

/// Note `server` for [`last_server`].
pub fn remember(file: &Path, server: &str) {
    if let Err(e) = std::fs::write(last_file(file), format!("{server}\n")) {
        tracing::debug!("noting the server failed: {e}");
    }
}

/// The saved sign-in, if any; `--server` must match it.
pub fn client(server: Option<&str>) -> Result<Option<Client>> {
    let login = match Login::path() {
        Some(p) => Login::load(&p)?,
        None => None,
    };
    let login = match (server, login) {
        (None, login) => login,
        (Some(s), Some(l)) if server_url(s)? == l.server => Some(l),
        (Some(s), _) => bail!("not signed in to {s}: run `{BIN} login --server {s}`"),
    };
    login.map(Client::new).transpose()
}

/// After a command that only edits local files, while signed in to a server.
pub fn hint() {
    if let Some(l) = Login::path().and_then(|p| Login::load(&p).ok().flatten()) {
        println!("{} plans from its own copy of the config: commit and push this to use it there.", l.server);
    }
}

/// `login`: sign in with Discord in the browser and save the token to
/// `file`; also says which community the server runs. `open` shows the
/// person the link. The browser brings a one-time code back to 127.0.0.1
/// here, worth a token only with the secret kept here: a link someone else
/// started lands on the clicker's machine, and another program here that
/// catches the code can't use it.
pub async fn login(server: &str, paste: bool, file: &Path, open: impl FnOnce(&str)) -> Result<(Login, String)> {
    login_with(server, paste, file, open, || {
        Ok(inquire::Password::new("Code from the page:").without_confirmation().prompt()?)
    })
    .await
}

/// [`login`], asking for the pasted code with `ask`.
async fn login_with(
    server: &str,
    paste: bool,
    file: &Path,
    open: impl FnOnce(&str),
    ask: impl FnOnce() -> Result<String>,
) -> Result<(Login, String)> {
    let server = server_url(server)?;
    let state = random_state();
    let verifier = Verifier::new();
    let challenge = &verifier.challenge;
    // Plain characters only, so it needs no escaping in the link.
    let device: String = this_device()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '_') { c } else { '-' })
        .collect();
    let start = format!("state={state}&challenge={challenge}&device={device}");
    let minutes = LOGIN_WAIT.as_secs() / 60;
    println!("Signing in to {server} with Discord");
    let token = if paste {
        open(&format!("{server}/auth/cli?{start}"));
        println!("Then paste the code the page shows here (it lasts {minutes} min).");
        trade(&server, &ask()?, &verifier).await?
    } else {
        let listener = TcpListener::bind("127.0.0.1:0").await.context("listening on 127.0.0.1")?;
        let port = listener.local_addr()?.port();
        open(&format!("{server}/auth/cli?port={port}&{start}"));
        println!("Waiting… (Ctrl-C to cancel, up to {minutes} min)");
        let wait = tokio::time::timeout(LOGIN_WAIT, callback(&listener, &state));
        let (code, mut browser) =
            wait.await.with_context(|| format!("gave up waiting for the browser; try `{BIN} login --paste`"))??;
        let traded = trade(&server, &code, &verifier).await;
        // The browser lands on the server's own page saying how it went.
        let page = match &traded {
            Ok(_) => format!("{server}/auth/cli/done"),
            Err(e) if e.to_string().contains("expired") => format!("{server}/auth/cli/failed?why=expired"),
            Err(_) => format!("{server}/auth/cli/failed?why=refused"),
        };
        let reply =
            format!("HTTP/1.1 303 See Other\r\nLocation: {page}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        let _ = browser.write_all(reply.as_bytes()).await;
        traded?
    };
    let mut login = Login { server, token, user: String::new() };
    let me = Client::new(login.clone())?.whoami().await?;
    login.user = me.user;
    login.save(file)?;
    Ok((login, me.server))
}

/// Try to open a browser on the link, and print it in case that didn't work.
pub fn open_browser(link: &str) {
    let opener = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
    let quiet = std::process::Stdio::null;
    let opened = std::process::Command::new(opener).arg(link).stdout(quiet()).stderr(quiet()).spawn().is_ok();
    if opened {
        println!("Opening your browser… (if it didn't open: {link})");
    } else {
        println!("Open this in your browser: {link}");
    }
}

/// Wait for the browser to bring the one-time code to `/callback`, with our
/// `state`: the code, and the browser's connection, to answer once it's traded.
async fn callback(listener: &TcpListener, state: &str) -> Result<(String, tokio::net::TcpStream)> {
    loop {
        let (mut conn, _) = listener.accept().await?;
        let mut head = Vec::new();
        let mut chunk = [0u8; 1024];
        while !head.windows(2).any(|w| w == b"\r\n") && head.len() < 8192 {
            match conn.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(n) => head.extend_from_slice(&chunk[..n]),
            }
        }
        let head = String::from_utf8_lossy(&head);
        let target = head.split_whitespace().nth(1).unwrap_or_default();
        let query = target.strip_prefix("/callback?").unwrap_or_default();
        // Both are hex, so there's nothing to decode.
        let fields: HashMap<&str, &str> = query.split('&').filter_map(|kv| kv.split_once('=')).collect();
        let token = fields.get("code").filter(|_| fields.get("state") == Some(&state)).map(|t| t.to_string());
        if let Some(t) = token {
            return Ok((t, conn));
        }
        let text = "This isn't the sign-in the terminal is waiting for.";
        let reply = format!(
            "HTTP/1.1 400 Bad Request\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\n\
             Connection: close\r\n\r\n{text}",
            text.len()
        );
        let _ = conn.write_all(reply.as_bytes()).await;
    }
}

/// `export` while signed in: the server's setup, as the bot sees it, written to `dir`.
pub async fn export(c: &Client, dir: &Path) -> Result<()> {
    if dir.join("server.toml").exists() {
        bail!("{} already has a config; export into an empty directory", dir.display());
    }
    let files = c.export().await?.files;
    for (path, body) in &files {
        // Only paths inside `dir`, whatever the server sent.
        let rel = Path::new(path);
        if rel.is_absolute() || rel.components().any(|p| !matches!(p, std::path::Component::Normal(_))) {
            bail!("the server sent an odd file name: {path}");
        }
        let to = dir.join(rel);
        if let Some(parent) = to.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&to, body).with_context(|| format!("writing {}", to.display()))?;
    }
    println!("Exported {} files from {} to {}.", files.len(), c.login.server, dir.display());
    println!("Add a bot.toml to turn features on (see docs/config.md).");
    Ok(())
}

fn random_state() -> String {
    chipsoc_client::remote::random_hex(16)
}

/// `logout`: revoke the token on the server and forget it here.
pub async fn logout(file: &Path) -> Result<()> {
    let Some(login) = Login::load(file)? else {
        println!("Not signed in.");
        return Ok(());
    };
    if let Err(e) = Client::new(login.clone())?.logout().await {
        println!("Couldn't reach {} to revoke the sign-in, so it works until it expires: {e:#}", login.server);
    }
    remember(file, &login.server);
    std::fs::remove_file(file).with_context(|| format!("removing {}", file.display()))?;
    println!("Signed out of {}.", login.server);
    Ok(())
}

/// `check` with no config here: the server's copy, saying so.
pub async fn check(c: &Client, dir: &Path) -> Result<()> {
    println!("No config in {}, so this checks {}'s copy.", dir.display(), c.login.server);
    let r = c.check().await?;
    author::report(&Report { errors: r.errors, warnings: r.warnings })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chipsoc_web::testing;

    #[cfg(unix)]
    fn mode(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn login_remembers_the_server_after_logout() {
        let home = tempfile::tempdir().unwrap();
        let file = home.path().join("chipsoc-bot/remote.toml");
        assert_eq!(last_server(&file), None, "nothing yet: login asks or says the one fix");
        let login = Login { server: "https://bot.example".into(), token: "secret".into(), user: "ada".into() };
        login.save(&file).unwrap();
        assert_eq!(last_server(&file).as_deref(), Some("https://bot.example"));
        remember(&file, &login.server);
        std::fs::remove_file(&file).unwrap();
        assert_eq!(last_server(&file).as_deref(), Some("https://bot.example"), "still known after logout");
        let noted = std::fs::read_to_string(last_file(&file)).unwrap();
        assert!(!noted.contains("secret"), "no token beside it");
    }

    #[cfg(unix)]
    #[test]
    fn the_token_file_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let home = tempfile::tempdir().unwrap();
        let file = home.path().join("chipsoc-bot/remote.toml");
        let login = Login { server: "https://bot.example".into(), token: "secret".into(), user: "ada".into() };
        login.save(&file).unwrap();
        assert_eq!(mode(&file), 0o600);
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
        login.save(&file).unwrap();
        assert_eq!(mode(&file), 0o600, "tightened when saved again");
        assert_eq!(Login::load(&file).unwrap(), Some(login));
    }

    #[test]
    fn plain_http_only_to_this_machine() {
        assert_eq!(server_url("https://bot.example/").unwrap(), "https://bot.example");
        assert!(server_url("http://127.0.0.1:8080").is_ok() && server_url("http://[::1]:8080").is_ok());
        assert!(server_url("http://bot.example").is_err());
        assert!(server_url("bot.example").is_err());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_pasted_code_needs_this_terminal_s_secret() {
        let config = tempfile::tempdir().unwrap();
        let server = testing::spawn(config.path()).await.unwrap();
        let home = tempfile::tempdir().unwrap();
        let file = home.path().join("chipsoc-bot/remote.toml");
        let page = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let shown = page.clone();
        let open = move |link: &str| {
            let link = link.to_owned();
            *shown.lock().unwrap() = blocking(async move { testing::browse(&link, "code-ada").await });
        };
        let code = || {
            let p = page.lock().unwrap().clone();
            Ok(p.split("<code>").nth(1).and_then(|s| s.split("</code>").next()).unwrap().to_owned())
        };
        let (login, _) = login_with(&server.url, true, &file, open, code).await.unwrap();
        assert_eq!(login.user, "ada");

        // The same kind of code, caught by someone without the secret, is worth nothing.
        let link = format!("{}/auth/cli?state=0123456789abcdef&challenge={}", server.url, Verifier::new().challenge);
        let page = testing::browse(&link, "code-ada").await.unwrap();
        let stolen = page.split("<code>").nth(1).and_then(|s| s.split("</code>").next()).unwrap();
        let err = trade(&server.url, stolen, &Verifier::new()).await.unwrap_err().to_string();
        assert!(err.contains("isn't yours"), "{err}");
    }

    /// Run a future to completion from sync code, on its own thread.
    fn blocking<F: std::future::Future<Output = anyhow::Result<String>> + Send + 'static>(f: F) -> String {
        let run = move || tokio::runtime::Runtime::new().unwrap().block_on(f).unwrap();
        tokio::task::block_in_place(|| std::thread::spawn(run).join().unwrap())
    }

    #[tokio::test]
    async fn signs_in_and_applies_through_the_web_server() {
        let config = tempfile::tempdir().unwrap();
        let server = testing::spawn(config.path()).await.unwrap();
        let home = tempfile::tempdir().unwrap();
        let file = home.path().join("chipsoc-bot/remote.toml");

        let browser = |link: &str| {
            let link = link.to_owned();
            tokio::spawn(async move { testing::browse(&link, "code-ada").await.unwrap() });
        };
        let (login, _) = login(&server.url, false, &file, browser).await.unwrap();
        assert!(!login.token.is_empty());
        assert_eq!((login.server.as_str(), login.user.as_str()), (server.url.as_str(), "ada"));
        assert_eq!(Login::load(&file).unwrap(), Some(login.clone()));
        #[cfg(unix)]
        assert_eq!(mode(&file), 0o600);

        let c = Client::new(login).unwrap();
        let o = crate::Orphans { delete: false, sweep: false };
        let through = chipsoc_client::Backend::Remote(Client::new(c.login.clone()).unwrap());
        crate::apply(&through, true, o).await.unwrap();
        assert!(!server.discord.take_calls().is_empty(), "applied in Discord");
        assert!(c.plan(false, false).await.unwrap().plan.is_empty(), "nothing left");

        let b = chipsoc_client::Backend::<crate::manage::CliRest>::Remote(Client::new(c.login.clone()).unwrap());
        let ctx = crate::manage::Ctx { b: &b, json: false, yes: true, dir: config.path() };
        let open = crate::manage::ProjectCmd::Applications { slug: "golem".into(), state: true };
        crate::manage::project(&ctx, open).await.unwrap();
        let projects = c.projects().await.unwrap().projects;
        assert!(projects[0].application.open);
        let saved = std::fs::read_to_string(config.path().join("projects.toml")).unwrap();
        assert!(saved.contains("open = true"), "{saved}");

        logout(&file).await.unwrap();
        assert!(!file.exists());
        let err = c.me().await.unwrap_err().to_string();
        assert!(err.contains("login --server"), "{err}");
    }
}

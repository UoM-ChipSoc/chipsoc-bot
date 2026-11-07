//! A terminal UI for the committee: what the web dashboard does (roles,
//! channels, projects, positions, the review queue, members, feeds,
//! resources, and planning and applying changes to Discord), with every key
//! on screen and `?` for help.
//! Edits are config ops and admin work goes through the shared backend, so
//! signed in to the bot's web server it works on the server's copy.

mod app;
mod changes;
mod edits;
mod form;
mod keys;
mod lists;
mod ui;
mod worker;

#[cfg(test)]
mod tests;

use std::fs;
use std::io::stdout;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use chipsoc_client::{Backend, Client, Local, editor};
use chipsoc_discord::fake::FakeDiscord;
use chipsoc_discord::{MaybeHttp, Rest};
use crossterm::event::{DisableBracketedPaste, EnableBracketedPaste, Event, EventStream, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{EnterAlternateScreen, enable_raw_mode};
use futures_util::StreamExt;
use ratatui::DefaultTerminal;
use tokio::sync::mpsc;

use crate::app::{App, Command};
use crate::worker::{Msg, Worker};

/// Run against the real Discord, with the token from `DISCORD_TOKEN`.
/// Without a token the files can still be browsed and edited.
pub async fn run(config_dir: &Path) -> Result<()> {
    run_with(MaybeHttp::from_env(NO_TOKEN), config_dir, false, None).await
}

/// Run with Discord reached through the bot's web server, signed in with `login`.
pub async fn run_remote(config_dir: &Path, client: Client) -> Result<()> {
    run_with(MaybeHttp::NoToken { why: NO_TOKEN.into() }, config_dir, false, Some(client)).await
}

/// Run on a copy of the config against a pretend, empty Discord server, with
/// a scratch database for the queue and the rest.
pub async fn demo(config_dir: &Path) -> Result<()> {
    let copy = tempfile::tempdir()?;
    chipsoc_config::ops::copy_dir(config_dir, copy.path())?;
    // The lockfile's ids belong to the real server.
    let _ = fs::remove_file(copy.path().join(chipsoc_config::LOCK_FILE));
    let _ = fs::remove_file(copy.path().join(chipsoc_config::LOCK_BACKUP));
    // Hidden, so config edits leave it alone.
    let db = format!("sqlite:{}?mode=rwc", copy.path().join(".demo.db").display());
    let fake: Fake = Arc::new(FakeDiscord::new());
    let backend = Backend::Local(Local::with_db(copy.path(), fake.clone(), Some(db), "you (demo)"));
    run_on(fake, copy.path(), true, None, Some(backend)).await
}

/// A pretend Discord the worker and its backend share.
pub(crate) type Fake = Arc<FakeDiscord>;

/// Why Discord can't be reached without a token, and what to do.
const NO_TOKEN: &str = "DISCORD_TOKEN isn't set, so Discord can't be reached: add it to .env, or sign in to \
                        the bot's web server with `chipsoc-bot login --server https://…`, and restart. \
                        Or try it on a pretend server: chipsoc-bot tui --demo";

async fn run_with<R: Rest + Clone + 'static>(rest: R, dir: &Path, demo: bool, remote: Option<Client>) -> Result<()> {
    run_on(rest, dir, demo, remote, None).await
}

async fn run_on<R: Rest + Clone + 'static>(
    rest: R,
    dir: &Path,
    demo: bool,
    remote: Option<Client>,
    backend: Option<Backend<R>>,
) -> Result<()> {
    // Signed in, everything is on the server's copy: no local config needed.
    if remote.is_none() && !dir.join("server.toml").exists() {
        bail!(
            "No config in {}: use --config DIR, or sign in to the bot's web server with \
             `chipsoc-bot login --server <url>`. Or try it on a pretend server: chipsoc-bot tui --demo",
            dir.display()
        );
    }
    let (tx, rx) = mpsc::unbounded_channel();
    let mut worker = match backend {
        Some(b) => Worker::with_backend(rest, dir, tx, b),
        None => Worker::new(rest, dir, tx),
    };
    let mut app = App::with(dir.to_owned(), demo, remote.is_some());
    if let Some(c) = remote {
        app.connected = Some(format!("connected to {} as {}", c.login.server, c.login.user));
        worker = worker.through(c)?;
    }
    for job in app.startup() {
        worker.spawn(job);
    }
    let mut terminal = ratatui::try_init()?;
    // Pasted text arrives whole, so a trailing newline isn't an Enter.
    let _ = execute!(stdout(), EnableBracketedPaste);
    let result = event_loop(&mut terminal, &mut app, &worker, rx).await;
    let _ = execute!(stdout(), DisableBracketedPaste);
    ratatui::restore();
    result
}

async fn event_loop<R: Rest + Clone + 'static>(
    terminal: &mut DefaultTerminal,
    app: &mut App,
    worker: &Worker<R>,
    mut rx: mpsc::UnboundedReceiver<Msg>,
) -> Result<()> {
    let mut events = EventStream::new();
    let mut ticker = tokio::time::interval(Duration::from_millis(100));
    while !app.quit {
        terminal.draw(|f| ui::draw(f, app))?;
        let cmd = tokio::select! {
            ev = events.next() => match ev.context("the terminal closed")?? {
                Event::Key(k) if k.kind == KeyEventKind::Press => app.on_key(k),
                Event::Paste(text) => {
                    app.on_paste(&text);
                    None
                }
                _ => None,
            },
            Some(msg) = rx.recv() => app.on_msg(msg),
            _ = ticker.tick() => {
                app.tick();
                None
            }
        };
        match cmd {
            Some(Command::Discord(job)) => worker.spawn(job),
            Some(Command::Edit(path, line)) => {
                // The editor needs the keyboard: stop reading it until we're back.
                drop(events);
                let result = suspended(terminal, || editor::open_editor(&path, line))?;
                events = EventStream::new();
                app.edited(result);
            }
            Some(Command::Text(text, target)) => {
                drop(events);
                let result = suspended(terminal, || editor::edit_text(&text, ".md"))?;
                events = EventStream::new();
                if let Some(Command::Discord(job)) = app.text_edited(target, &text, result) {
                    worker.spawn(job);
                }
            }
            None => {}
        }
    }
    Ok(())
}

/// Hand the terminal to `f`, then take it back.
fn suspended<T>(terminal: &mut DefaultTerminal, f: impl FnOnce() -> T) -> Result<T> {
    ratatui::try_restore()?;
    let out = f();
    enable_raw_mode()?;
    execute!(stdout(), EnterAlternateScreen, EnableBracketedPaste)?;
    // Not `clear()`: it asks the terminal for the cursor, which can time out here.
    let size = terminal.size()?;
    terminal.resize(size.into())?;
    Ok(out)
}

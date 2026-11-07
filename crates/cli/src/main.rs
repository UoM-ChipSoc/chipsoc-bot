//! Manage a Discord server from config files in git, and run its bot.

mod author;
mod edits;
mod manage;
mod remote;
#[cfg(test)]
mod tests;

use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use chipsoc_client::{Backend, Local};
use chipsoc_config::authoring::Kind;
use chipsoc_discord::Http;
use chipsoc_engine::{Options, Pulled};
use chipsoc_fedproto as fedproto;
use chipsoc_model::{Action, Plan, Snowflake};
use clap::{Parser, Subcommand};

/// This program's name, for messages.
pub const BIN: &str = env!("CARGO_BIN_NAME");

#[derive(Parser)]
#[command(version, about = "Manage a Discord server from config files, and run its bot.")]
struct Cli {
    /// Config directory.
    #[arg(
        long,
        short,
        global = true,
        env = "BOT_CONFIG",
        hide_env_values = true,
        default_value = "config",
        help_heading = "Global options"
    )]
    config: PathBuf,
    /// The bot's web server to work through (default: the one you signed in to).
    #[arg(long, global = true, value_name = "URL", help_heading = "Global options")]
    server: Option<String>,
    /// Print JSON, for scripts.
    #[arg(long, global = true, help_heading = "Global options")]
    json: bool,
    /// Don't ask before applying or removing things.
    #[arg(long, short, global = true, help_heading = "Global options")]
    yes: bool,
    /// On the bot's own server: use its token and database, not a sign-in.
    #[arg(
        long,
        global = true,
        env = "BOT_LOCAL",
        hide_env_values = true,
        value_parser = clap::builder::FalseyValueParser::new(),
        help_heading = "Global options"
    )]
    local: bool,
    /// What to do; with none, opens the terminal UI.
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// The terminal UI (also what runs with no command).
    Tui {
        /// Try it on a pretend server, on a copy of the config.
        #[arg(long)]
        demo: bool,
    },
    /// The web UI (Discord sign-in; without it, 127.0.0.1 only).
    ///
    /// Signs in with Discord when DISCORD_CLIENT_SECRET and WEB_URL are set;
    /// else it listens only on 127.0.0.1 (WEB_TOKEN optional).
    Web {
        /// Address to listen on (anything but 127.0.0.1 needs Discord sign-in).
        #[arg(long, default_value = "127.0.0.1:8080")]
        addr: std::net::SocketAddr,
    },
    /// The server at a glance: size, config problems, the review queue.
    Status,
    /// Does Discord match the config? What changed, and who.
    Drift,
    /// Check the config for mistakes (doesn't touch Discord).
    Check,
    /// What `apply` would change in Discord.
    Plan {
        #[command(flatten)]
        orphans: Orphans,
    },
    /// Make Discord match the config. Shows the plan and asks first.
    Apply {
        /// Show the plan only (same as `plan`).
        #[arg(long)]
        dry_run: bool,
        #[command(flatten)]
        orphans: Orphans,
    },
    /// Copy changes made in Discord into the config files.
    #[command(alias = "adopt")]
    Pull {
        /// Also copy role permissions.
        #[arg(long)]
        permissions: bool,
    },
    /// Write the server's current setup as a new config directory.
    Export {
        /// The new directory.
        dir: PathBuf,
        /// Which server, if the bot is in several.
        #[arg(long)]
        guild: Option<u64>,
    },
    /// Start a new config directory from a discord.new template.
    Template {
        /// The template's code (the end of its discord.new link).
        code: String,
        /// The new directory.
        dir: PathBuf,
    },
    /// Open the file that defines something, in $EDITOR.
    Open {
        /// What kind of thing.
        #[arg(value_parser = clap::builder::PossibleValuesParser::new(Kind::ALL.map(|(n, _)| n)))]
        kind: String,
        /// Its key (default: the file for all of that kind).
        key: Option<String>,
    },
    /// Projects: add, change, status, applications, brief, leads.
    #[command(subcommand)]
    Project(manage::ProjectCmd),
    /// Roles: list, new, set, move, remove.
    #[command(subcommand)]
    Role(manage::RoleCmd),
    /// Categories: list, new, rename, move, remove, perms.
    #[command(subcommand)]
    Category(manage::CategoryCmd),
    /// Channels: list, new, set, move, move-to, archive, perms.
    #[command(subcommand)]
    Channel(manage::ChannelCmd),
    /// Committee positions (/volunteer): list, add, change, questions, open, close.
    #[command(subcommand)]
    Position(manage::PositionCmd),
    /// Who holds positions and leads; restore people who came back.
    #[command(subcommand)]
    Member(manage::MemberCmd),
    /// The review queue: applications, ideas, feeds and links.
    #[command(subcommand)]
    Queue(manage::QueueCmd),
    /// Feeds whose new posts the bot shares.
    #[command(subcommand)]
    Feed(manage::FeedCmd),
    /// Links shared in the resources forum.
    #[command(subcommand)]
    Resource(manage::ResourceCmd),
    /// The join flow (join.toml): progress, and the one-off notice to members.
    #[command(subcommand)]
    Join(manage::JoinCmd),
    /// Share an announcement or one of our events with partners (`send` in federation.toml).
    Share {
        /// `announcement` or `event`.
        what: String,
        /// The message's link, or the event's link or id.
        item: String,
        /// Partners' names, comma-separated (default: every partner we send it to).
        #[arg(long)]
        to: Option<String>,
    },
    /// Project ideas: list the newest, close one.
    #[command(subcommand, alias = "ideas")]
    Idea(manage::IdeaCmd),
    /// Posts the bot keeps in channels (rules, welcome, …).
    #[command(subcommand)]
    Post(manage::PostCmd),
    /// Discord's onboarding questions.
    #[command(subcommand)]
    Onboarding(manage::OnboardingCmd),
    /// What admins did, newest first.
    Activity {
        /// How many.
        #[arg(long, default_value_t = 30)]
        limit: usize,
    },
    /// The config's latest changes and who made them; with `--admin`, what they did this week.
    History {
        /// Who: their @username, server name or Discord id.
        #[arg(long)]
        admin: Option<String>,
        /// How many to show.
        #[arg(long, default_value_t = 30)]
        limit: usize,
    },
    /// Undo a config change (its id, from `history`), saved as a new one; then apply. The server owner only.
    Revert {
        /// The change's id, from `history`.
        change: String,
        /// Put the whole config back as it was right after that change instead.
        #[arg(long)]
        rewind: bool,
    },
    /// Any config edit, as JSON (see `schema`).
    Op {
        /// The edit, e.g. '{"op": "set_role", "key": "member", "set": {"hoist": true}}'.
        json: String,
    },
    /// What can be edited, with the current values, as JSON.
    Schema,
    /// Run the bot (on its own server, with --local).
    Run {
        /// More servers' config directories.
        also: Vec<PathBuf>,
    },
    /// Keys for signed sync with partner servers.
    #[command(subcommand)]
    Fed(FedCmd),
    /// Sign in to the bot's web server with Discord (default: bot.toml's [web] url).
    Login {
        /// Show a code to paste here instead of coming back to this machine.
        #[arg(long)]
        paste: bool,
    },
    /// Sign out of the bot's web server.
    Logout,
}

#[derive(clap::Args, Clone, Copy)]
struct Orphans {
    /// Delete objects removed from config (default: move them to the archive).
    #[arg(long)]
    delete: bool,
    /// Also archive channels the config has never managed.
    #[arg(long)]
    sweep: bool,
}

#[derive(Subcommand)]
enum FedCmd {
    /// Make our signing key (saved in .env, or --env) and print the public key for partners.
    Keygen {
        /// Replace an existing key; partners will need the new public key.
        #[arg(long)]
        force: bool,
        /// The env file to save it in: the one the bot reads (with deploy.sh,
        /// the secrets file it uploads, e.g. .env.server).
        #[arg(long, value_name = "PATH")]
        env: Option<PathBuf>,
    },
    /// Show our public key and check each partner's setup.
    Key {
        /// Read the key and webhooks from this env file (e.g. .env.server).
        #[arg(long, value_name = "PATH")]
        env: Option<PathBuf>,
    },
    /// Take back everything a partner gave (e.g. a stolen key). On the bot's
    /// server: `scripts/deploy.sh revoke`.
    Revoke {
        /// Their id or name.
        peer: String,
        /// Only what was given on messages signed by this key id.
        #[arg(long)]
        key: Option<String>,
    },
}

fn main() -> Result<()> {
    tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::from_default_env()).init();
    // .env first, so it can set BOT_CONFIG.
    let mut env_file = dotenvy::dotenv().ok();
    let cli = Cli::parse();
    if let Some(Cmd::Fed(FedCmd::Keygen { env: Some(path), .. } | FedCmd::Key { env: Some(path) })) = &cli.cmd {
        // That file instead of .env: drop what .env set, then read it.
        // Before any other thread exists, so changing the environment is safe.
        for (var, _) in env_file.iter().flat_map(dotenvy::from_path_iter).flatten().flatten() {
            // SAFETY: no other threads are running yet (the runtime starts below).
            unsafe { std::env::remove_var(var) };
        }
        if path.exists() {
            dotenvy::from_path_override(path).with_context(|| format!("reading {}", path.display()))?;
        }
        env_file = Some(path.clone());
    }
    if matches!(cli.cmd, Some(Cmd::Web { .. })) {
        // Before any other thread exists, so changing the environment is safe.
        for var in bot_only_secrets(&cli.config) {
            // SAFETY: no other threads are running yet (the runtime starts below).
            unsafe { std::env::remove_var(var) };
        }
    }
    tokio::runtime::Builder::new_multi_thread().enable_all().build()?.block_on(run(cli, env_file))
}

/// Secrets only the bot needs, which the web UI (and the git it runs) never
/// sees: the partner-sync signing key and webhooks, and the Tailscale API key.
/// The web UI reads `DISCORD_TOKEN`, `APPLICATION_ID`, `DISCORD_CLIENT_SECRET`,
/// `WEB_URL`, `WEB_TOKEN`, `DATABASE_URL`, `CONFIG_AUTOSAVE` and `BOT_CONFIG`.
fn bot_only_secrets(config: &Path) -> Vec<String> {
    let mut vars = vec!["FED_SIGNING_KEY".to_owned(), "TAILSCALE_API_KEY".to_owned()];
    if let Ok(c) = chipsoc_config::Config::load(config) {
        let fed = c.federation;
        vars.push(fed.key_env.clone());
        for p in fed.peers {
            vars.extend([p.webhook_env, p.share_webhook_env].into_iter().filter(|v| !v.is_empty()));
        }
    }
    vars
}

/// What a command needs before it runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Needs {
    /// Only files here, or nothing.
    Nothing,
    /// Discord, the bot's database or its server: a sign-in (`login`), or
    /// `--local` on the bot's own server.
    SignIn,
    /// The bot's own server (`--local`): it runs the bot, or works on its database.
    Local,
}

fn needs(cmd: &Cmd, local_check: bool) -> Needs {
    match cmd {
        Cmd::Check if local_check => Needs::Nothing,
        Cmd::Tui { demo: true }
        | Cmd::Template { .. }
        | Cmd::Open { .. }
        | Cmd::Login { .. }
        | Cmd::Logout
        | Cmd::Fed(FedCmd::Keygen { .. } | FedCmd::Key { .. }) => Needs::Nothing,
        Cmd::Run { .. } | Cmd::Web { .. } | Cmd::Fed(FedCmd::Revoke { .. }) => Needs::Local,
        _ => Needs::SignIn,
    }
}

/// Lists that only read the config files: these work on a checkout here
/// when signed out.
fn reads_files(cmd: &Cmd) -> bool {
    use manage::{CategoryCmd, ChannelCmd, OnboardingCmd, PositionCmd, PostCmd, ProjectCmd, RoleCmd};
    matches!(
        cmd,
        Cmd::Project(ProjectCmd::List | ProjectCmd::Show { .. })
            | Cmd::Position(PositionCmd::List)
            | Cmd::Role(RoleCmd::List)
            | Cmd::Category(CategoryCmd::List)
            | Cmd::Channel(ChannelCmd::List)
            | Cmd::Post(PostCmd::List | PostCmd::Show { .. })
            | Cmd::Onboarding(OnboardingCmd::List)
            | Cmd::Schema
    )
}

/// The dashboard's address from bot.toml's `[web] url`, if the config here has one.
fn web_url(config: &Path) -> Option<String> {
    chipsoc_config::Config::load(config).ok().and_then(|c| c.bot.web).map(|w| w.url)
}

/// Why a command that reaches Discord won't run here without signing in.
fn sign_in_first(config: &Path) -> anyhow::Error {
    let url = web_url(config).unwrap_or_else(|| "<url>".to_owned());
    anyhow::anyhow!(
        "Sign in first: {BIN} login --server {url}\n\
         Commands that reach Discord or the bot's data run through the bot's server, as you; the bot's token \
         stays there. (On the bot's own server, pass --local.)"
    )
}

async fn run(cli: Cli, env_file: Option<PathBuf>) -> Result<()> {
    let c = cli.config.as_path();
    let server = cli.server.as_deref();
    let cmd = cli.cmd.unwrap_or(Cmd::Tui { demo: false });
    // `check` looks at the files here whenever there are some.
    let local_check = matches!(cmd, Cmd::Check) && c.join("server.toml").exists();
    let need = needs(&cmd, local_check);
    let remote = if need == Needs::SignIn && !cli.local { remote::client(server)? } else { None };
    let files_here = c.join("server.toml").exists();
    match need {
        Needs::SignIn if remote.is_none() && !cli.local && reads_files(&cmd) && files_here => {
            eprintln!("Not signed in: reading the files in {}.", c.display());
        }
        Needs::SignIn if remote.is_none() && !cli.local && matches!(cmd, Cmd::Tui { .. }) => {
            bail!("{}\nOr try it on a pretend server: {BIN} tui --demo", sign_in_first(c));
        }
        Needs::SignIn if remote.is_none() && !cli.local => return Err(sign_in_first(c)),
        Needs::Local if !cli.local => bail!(
            "this runs only on the bot's own server, where deploy.sh sets BOT_LOCAL=1 (or pass --local there). \
             From here, sign in instead: {BIN} login"
        ),
        _ => {}
    }
    if let Some(r) = &remote {
        eprintln!("On {} as {}.", r.login.server, r.login.user);
        if std::env::var_os("DISCORD_TOKEN").is_some() {
            eprintln!("Note: DISCORD_TOKEN is set here but not used; remove it from .env.");
        }
    }
    match cmd {
        Cmd::Tui { demo: false } => match remote {
            Some(r) => chipsoc_tui::run_remote(c, r).await,
            None => chipsoc_tui::run(c).await,
        },
        Cmd::Tui { demo: true } => chipsoc_tui::demo(c).await,
        Cmd::Web { addr } => {
            println!("Open http://{addr} in a browser (Ctrl-C to stop).");
            chipsoc_web::serve(cli.config.clone(), addr).await
        }
        Cmd::Check => match remote {
            Some(r) => remote::check(&r, c).await,
            None => author::check(c),
        },
        Cmd::Plan { orphans } | Cmd::Apply { dry_run: true, orphans, .. } => {
            plan(&backend(c, remote)?, orphans).await.map(drop)
        }
        Cmd::Apply { orphans, .. } => apply(&backend(c, remote)?, cli.yes, orphans).await,
        Cmd::Pull { permissions } => pull(&backend(c, remote)?, permissions).await,
        Cmd::Export { dir, guild } => match remote {
            Some(r) => remote::export(&r, &dir).await,
            None => export(&dir, guild).await,
        },
        Cmd::Template { code, dir } => template(&code, &dir).await,
        Cmd::Open { kind, key } => {
            author::open(c, Kind::parse(&kind).context("unknown kind")?, key.as_deref())?;
            remote::hint();
            Ok(())
        }
        Cmd::Status
        | Cmd::Drift
        | Cmd::Project(_)
        | Cmd::Role(_)
        | Cmd::Category(_)
        | Cmd::Channel(_)
        | Cmd::Position(_)
        | Cmd::Member(_)
        | Cmd::Queue(_)
        | Cmd::Feed(_)
        | Cmd::Resource(_)
        | Cmd::Join(_)
        | Cmd::Share { .. }
        | Cmd::Idea(_)
        | Cmd::Post(_)
        | Cmd::Onboarding(_)
        | Cmd::Activity { .. }
        | Cmd::History { .. }
        | Cmd::Revert { .. }
        | Cmd::Op { .. }
        | Cmd::Schema => {
            let b = match remote {
                Some(r) => Backend::Remote(r),
                None => Backend::Local(Local::new(c, manage::cli_rest())),
            };
            let ctx = manage::Ctx { b: &b, json: cli.json, yes: cli.yes, dir: c };
            manage(&ctx, cmd).await
        }
        Cmd::Run { also } => {
            let dirs: Vec<PathBuf> = std::iter::once(cli.config.clone()).chain(also).collect();
            chipsoc_runtime::run(&dirs).await
        }
        Cmd::Fed(FedCmd::Keygen { force, .. }) => fed_keygen(c, env_file.as_deref(), force),
        Cmd::Fed(FedCmd::Key { .. }) => fed_key(c),
        Cmd::Fed(FedCmd::Revoke { peer, key }) => fed_revoke(c, &peer, key.as_deref()).await,
        Cmd::Login { paste } => {
            let file = remote::file()?;
            let server = match server.map(str::to_owned).or_else(|| web_url(c)).or_else(|| remote::last_server(&file)) {
                Some(s) => s,
                None if std::io::stdin().is_terminal() => {
                    inquire::Text::new("The dashboard's address (https://…):").prompt()?
                }
                None => bail!("say which server: {BIN} login --server https://<the dashboard's address>"),
            };
            let (login, name) = remote::login(&server, paste, &file, remote::open_browser).await?;
            remote::remember(&file, &login.server);
            let on = if name.is_empty() { login.server.clone() } else { name };
            println!("✓ Signed in as {} on {on}. Commands now run through the server.", login.user);
            if std::env::var_os("DISCORD_TOKEN").is_some() {
                println!("Remove DISCORD_TOKEN from .env: you don't need it now.");
            }
            Ok(())
        }
        Cmd::Logout => remote::logout(&remote::file()?).await,
    }
}

/// A command for one area, on `ctx`'s backend.
async fn manage(ctx: &manage::Ctx<'_, manage::CliRest>, cmd: Cmd) -> Result<()> {
    match cmd {
        Cmd::Status => manage::status(ctx).await,
        Cmd::Drift => manage::drift(ctx).await,
        Cmd::Project(p) => manage::project(ctx, p).await,
        Cmd::Role(r) => manage::role(ctx, r).await,
        Cmd::Category(x) => manage::category(ctx, x).await,
        Cmd::Channel(x) => manage::channel(ctx, x).await,
        Cmd::Position(x) => manage::position(ctx, x).await,
        Cmd::Member(x) => manage::member(ctx, x).await,
        Cmd::Queue(x) => manage::queue(ctx, x).await,
        Cmd::Feed(x) => manage::feed(ctx, x).await,
        Cmd::Resource(x) => manage::resource(ctx, x).await,
        Cmd::Join(x) => manage::join(ctx, x).await,
        Cmd::Share { what, item, to } => manage::share(ctx, what, item, to).await,
        Cmd::Idea(x) => manage::idea(ctx, x).await,
        Cmd::Post(x) => manage::post(ctx, x).await,
        Cmd::Onboarding(x) => manage::onboarding(ctx, x).await,
        Cmd::Activity { limit } => manage::activity(ctx, limit).await,
        Cmd::History { admin, limit } => manage::history(ctx, admin.as_deref(), limit).await,
        Cmd::Revert { change, rewind } => manage::revert(ctx, &change, rewind).await,
        Cmd::Op { json } => manage::raw_op(ctx, &json).await,
        Cmd::Schema => manage::schema(ctx).await,
        _ => unreachable!("not an area command"),
    }
}

/// The bot's token, for `--local` on the bot's own server.
fn http() -> Result<Http> {
    let token = std::env::var("DISCORD_TOKEN")
        .with_context(|| "DISCORD_TOKEN isn't set: with --local this runs as the bot, on its own server")?;
    Ok(Http::new(token))
}

/// Where plan, apply and pull go: the bot's web server when signed in, else
/// the files here with the bot's token (which must be set).
fn backend(config: &Path, remote: Option<chipsoc_client::Client>) -> Result<Backend<Http>> {
    Ok(match remote {
        Some(r) => Backend::Remote(r),
        None => Backend::Local(Local::new(config, http()?)),
    })
}

/// A plan, as [`chipsoc_engine::plan_lines`] puts it.
fn print_plan(p: &Plan, lines: &[String]) {
    for l in lines {
        println!("  {l}");
    }
    match p.changes() {
        0 => println!("\nNothing to do: Discord matches the config."),
        n => println!("\n{}.", chipsoc_admin::fmt::count(n, "change", "changes")),
    }
}

/// Plan and print it. Here, the config's problems are shown first, and refuse it.
async fn plan(b: &Backend<Http>, o: Orphans) -> Result<chipsoc_client::Planned> {
    if let Backend::Local(l) = b {
        let r = chipsoc_config::authoring::check(l.dir())?;
        author::print_report(&r);
        if !r.errors.is_empty() {
            bail!("fix the problems above first (`{BIN} check`)");
        }
    }
    let p = b.plan(Options { delete: o.delete, sweep: o.sweep, ..Default::default() }).await?;
    print_plan(&p.plan, &p.lines);
    Ok(p)
}

/// Whether to apply `p`: it changes something and the person agrees (or said `--yes`).
fn go_ahead(p: &Plan, yes: bool) -> Result<bool> {
    if p.is_empty() || yes {
        return Ok(!p.is_empty());
    }
    if !std::io::stdin().is_terminal() {
        bail!("not a terminal: re-run with --yes to apply without asking");
    }
    let deletes = p.actions.iter().any(|a| matches!(a, Action::Delete { .. }));
    let q = if deletes { "Apply? This DELETES things for good." } else { "Apply these changes?" };
    let go = inquire::Confirm::new(q).with_default(false).prompt()?;
    if !go {
        println!("Nothing changed.");
    }
    Ok(go)
}

fn print_step(a: &Action, r: &Result<(), String>) {
    let head = a.headline();
    match r {
        Ok(()) => println!("  ✓ {head}"),
        Err(e) => println!("  ✗ {head}\n      {e}"),
    }
}

/// Fails if `n` steps failed.
fn failed(n: usize) -> Result<()> {
    if n > 0 {
        bail!(
            "{} failed; the rest were applied and saved. Fix and run `{BIN} apply` again.",
            chipsoc_admin::fmt::count(n, "step", "steps")
        );
    }
    Ok(())
}

async fn apply(b: &Backend<Http>, yes: bool, o: Orphans) -> Result<()> {
    let p = plan(b, o).await?;
    if !go_ahead(&p.plan, yes)? {
        return Ok(());
    }
    let steps = b.apply(&p, &mut print_step).await?;
    failed(steps.iter().filter(|s| s.error.is_some()).count())?;
    if b.is_remote() {
        println!("\nDone.");
    } else {
        println!("\nDone. Commit the config, including lock.toml.");
    }
    Ok(())
}

async fn pull(b: &Backend<Http>, permissions: bool) -> Result<()> {
    let notes = b.pull(permissions).await?;
    print_pulled(&notes);
    if b.is_remote() && notes.iter().any(|n| matches!(n, Pulled::Adopted(_))) {
        println!("\nSaved in the server's copy of the config; `git pull` to get it here.");
    }
    Ok(())
}

fn print_pulled(notes: &[Pulled]) {
    if notes.is_empty() {
        println!("Nothing to pull: the config already matches Discord.");
    }
    for n in notes {
        println!("  {n}");
    }
    if notes.iter().any(|n| matches!(n, Pulled::ByHand(_))) {
        println!("\n! items: edit the config by hand to keep them, or run `{BIN} apply` to undo them in Discord.");
    }
}

async fn export(dir: &Path, guild: Option<u64>) -> Result<()> {
    let rest = http()?;
    let g = match guild {
        Some(g) => Snowflake(g),
        None => chipsoc_discord::sole_guild(&rest).await?.0,
    };
    let snap = chipsoc_engine::export_to(&rest, g, dir).await?;
    let channels = snap.channels().count();
    println!(
        "Exported {} roles, {} categories, {channels} channels to {}.",
        snap.roles.roles.len(),
        snap.categories.len(),
        dir.display()
    );
    println!("Add a bot.toml to turn features on (see docs/config.md), then `{BIN} plan --config {}`.", dir.display());
    Ok(())
}

async fn template(code: &str, dir: &Path) -> Result<()> {
    // Templates are public: no token needed.
    let snap = chipsoc_discord::template(&Http::anonymous(), code).await?;
    if dir.join("server.toml").exists() {
        bail!("{} already has a config", dir.display());
    }
    chipsoc_config::write_snapshot(dir, &snap)?;
    println!("Wrote the template's roles and channels to {}.", dir.display());
    Ok(())
}

/// `fed keygen`: the secret goes straight into .env and is never printed.
fn fed_keygen(config: &Path, env_file: Option<&Path>, force: bool) -> Result<()> {
    let var =
        chipsoc_config::Config::load(config).map(|c| c.federation.key_env).unwrap_or_else(|_| "FED_SIGNING_KEY".into());
    let path = env_file.map_or_else(|| PathBuf::from(".env"), Path::to_path_buf);
    refuse_symlink(&path)?;
    let existing = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let prefix = format!("{var}=");
    let is_key = |l: &&str| l.trim_start().starts_with(&prefix);
    if existing.lines().any(|l| is_key(&l)) && !force {
        bail!("{} already has {var}; `{BIN} fed key` shows its public key (--force replaces it)", path.display());
    }
    let (secret, public) = fedproto::generate().map_err(anyhow::Error::msg)?;
    let mut lines: Vec<&str> = existing.lines().filter(|l| !is_key(l)).collect();
    let line = format!("{var}={secret}");
    lines.push(&line);
    write_secret(&path, &(lines.join("\n") + "\n"))?;
    let shown = std::fs::canonicalize(&path).unwrap_or(path);
    println!("Saved the signing key in {}.\nPublic key, for partners:\n  {public}", shown.display());
    Ok(())
}

fn refuse_symlink(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_symlink() => {
            bail!("{} is a symlink; refusing to write a secret through it", path.display())
        }
        _ => Ok(()),
    }
}

/// Replace a file holding secrets: written to a new file only we can read
/// (never an existing one or a symlink), flushed to disk, then renamed over.
fn write_secret(path: &Path, body: &str) -> Result<()> {
    use std::io::Write;
    refuse_symlink(path)?;
    let mut tag = [0u8; 8];
    getrandom::fill(&mut tag).map_err(|e| anyhow::anyhow!("no randomness: {e}"))?;
    let tag: String = tag.iter().map(|b| format!("{b:02x}")).collect();
    let name = path.file_name().context("no file name")?.to_string_lossy();
    let tmp = path.with_file_name(format!(".{name}.{tag}.tmp"));
    let mut open = std::fs::OpenOptions::new();
    open.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut open, 0o600);
    let written = (|| {
        let mut f = open.open(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
        f.write_all(body.as_bytes())?;
        f.sync_all()?;
        std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written?;
    if let Some(dir) = path.parent().map(|d| if d.as_os_str().is_empty() { Path::new(".") } else { d })
        && let Ok(d) = std::fs::File::open(dir)
    {
        let _ = d.sync_all();
    }
    Ok(())
}

async fn fed_revoke(config: &Path, peer: &str, key: Option<&str>) -> Result<()> {
    bots_database(std::env::var_os("DATABASE_URL").is_some(), Path::new(DEFAULT_DB))?;
    let cfg = chipsoc_config::Config::load(config)?;
    println!("{}", chipsoc_runtime::revoke_partner(&http()?, &cfg, peer, key).await?);
    if cfg.federation.peers.iter().any(|p| p.id == peer || p.name == peer) {
        let what = if key.is_some() { "that key from their `public_keys`" } else { "their [[peer]]" };
        println!(
            "They're still in federation.toml: remove {what} there too, or their next sync gives the roles back (held for an admin if it's many)."
        );
    }
    Ok(())
}

/// Where the bot's database is when `DATABASE_URL` isn't set (see `Store::from_env`).
const DEFAULT_DB: &str = "data/bot.db";

/// `fed revoke` changes the bot's own state, so it must open the bot's
/// database, never make a new empty one that knows of no partner.
fn bots_database(url_set: bool, default: &Path) -> Result<()> {
    if url_set || default.exists() {
        return Ok(());
    }
    bail!(
        "DATABASE_URL isn't set and there's no {} here, so this isn't where the bot keeps its state. \
         Run it with the bot's DATABASE_URL (on a server set up with deploy.sh: `scripts/deploy.sh revoke <partner>`).",
        default.display()
    )
}

fn fed_key(config: &Path) -> Result<()> {
    let fed = chipsoc_config::Config::load(config)?.federation;
    match std::env::var(&fed.key_env) {
        Ok(k) => {
            let sk = fedproto::signing_key(&k).map_err(anyhow::Error::msg)?;
            println!("We are `{}`. Public key: {}", fed.name, fedproto::public_of(&sk));
        }
        Err(_) => println!("No {} in the env file yet: run `{BIN} fed keygen`.", fed.key_env),
    }
    if fed.inbox.is_none() {
        println!("No `inbox` in federation.toml, so partners have nowhere to send to.");
    }
    for p in &fed.peers {
        let keys: Vec<&str> = p.keys().collect();
        let bad: Vec<String> = keys.iter().filter_map(|k| fedproto::verifying_key(k).err()).collect();
        let key = match (keys.len(), bad.first()) {
            (0, _) => "no public key".to_owned(),
            (_, Some(e)) => format!("bad key: {e}"),
            (n, None) => format!("{} ok", chipsoc_admin::fmt::count(n, "key", "keys")),
        };
        let set = |v: &str| {
            if v.is_empty() {
                "not used"
            } else if std::env::var(v).is_ok() {
                "set"
            } else {
                "MISSING"
            }
        };
        println!(
            "  {} (`{}`): {key}; inbox webhook {}; share webhook {}",
            p.name,
            p.id,
            set(&p.webhook_env),
            set(&p.share_webhook_env)
        );
    }
    Ok(())
}

//! The commands for each area: roles, categories, channels, projects,
//! positions, members, the review queue, feeds, resources, posts, onboarding
//! and the activity log. Each goes through a [`Backend`]: the bot's web
//! server after `login`, else the files and database here. Config edits are
//! [`Op`]s, checked before anything is written.

mod channels;
mod feeds;
mod join;
mod members;
mod positions;
mod posts;
mod projects;
mod queue;
mod roles;

use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use chipsoc_admin::NewResource;
use chipsoc_client::Backend;
use chipsoc_config::ops::{Change, Op, OptionField, PositionField, ProjectField, PromptField, Schema};
use chipsoc_config::schema::{Channel, ChannelKind, OnbOption, Overwrite, ProjectStatus};
use chipsoc_config::{Snowflake, slug};
use chipsoc_discord::Rest;
use clap::{Args, Subcommand};
use inquire::{Confirm, Text};
use serde::Serialize;

use crate::BIN;
use crate::author;
use crate::edits::{self, Perms, parse_bool, parse_kind, parse_move, parse_status};

pub use channels::*;
pub use feeds::*;
pub use join::*;
pub use members::*;
pub use positions::*;
pub use posts::*;
pub use projects::*;
pub use queue::*;
pub use roles::*;

/// Discord for work done here: the bot's token, or a stand-in that says how to set one.
pub type CliRest = chipsoc_discord::MaybeHttp;

/// [`CliRest`] from the environment.
pub fn cli_rest() -> CliRest {
    CliRest::from_env(format!(
        "this needs Discord: sign in to the bot's web server with `{BIN} login --server https://…`, \
         or put the bot's token in .env as DISCORD_TOKEN"
    ))
}

/// What every command here shares.
pub struct Ctx<'a, R> {
    pub b: &'a Backend<R>,
    /// Print JSON instead of words.
    pub json: bool,
    /// Don't ask before destructive things.
    pub yes: bool,
    /// The local config directory, for messages.
    pub dir: &'a Path,
}

fn tty() -> bool {
    std::io::stdin().is_terminal()
}

/// Ask before something destructive, unless `--yes`.
pub fn confirm(question: &str, yes: bool) -> Result<bool> {
    if yes {
        return Ok(true);
    }
    if !tty() {
        bail!("not a terminal: re-run with --yes to go ahead without asking");
    }
    let go = Confirm::new(question).with_default(false).prompt()?;
    if !go {
        println!("Nothing changed.");
    }
    Ok(go)
}

/// A value from a flag, else asked for (when someone's there).
fn ask(given: Option<String>, label: &str, flag: &str) -> Result<String> {
    match given {
        Some(v) => Ok(v),
        None if tty() => Ok(Text::new(label).prompt()?.trim().to_owned()),
        None => bail!("missing --{flag}"),
    }
}

/// Text from a file (`-` for stdin), else `$EDITOR` on `initial`.
fn text_from(file: Option<&Path>, initial: &str, suffix: &str) -> Result<String> {
    if let Some(f) = file {
        if f == Path::new("-") {
            return std::io::read_to_string(std::io::stdin()).context("reading stdin");
        }
        return std::fs::read_to_string(f).with_context(|| format!("reading {}", f.display()));
    }
    if !tty() {
        bail!("not a terminal: give --file (or `--file -` for stdin)");
    }
    chipsoc_client::editor::edit_text(initial, suffix)
}

fn print_json(v: &impl Serialize) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(v)?);
    Ok(())
}

impl<R: Rest + Clone> Ctx<'_, R> {
    /// Make edits as one change (one check, one commit, all or nothing) and
    /// say how it went. A refused edit lists the problems and fails, having
    /// written nothing.
    async fn edit_all(&self, ops: &[Op]) -> Result<Change> {
        let change = match self.b.ops(ops).await {
            Ok(c) => c,
            Err(e) => {
                if let Some(p) = e.downcast_ref::<chipsoc_engine::Problems>() {
                    let what: Vec<String> = ops.iter().map(Op::describe).collect();
                    eprintln!("Refused: {} would leave the config with problems:", what.join("; "));
                    for problem in &p.0 {
                        eprintln!("  ✗ {problem}");
                    }
                    bail!("nothing was changed");
                }
                return Err(e);
            }
        };
        if self.json {
            print_json(&change)?;
        } else if change.files.is_empty() {
            println!("{}", change.summary);
        } else {
            println!("✓ {} ({})", change.summary, change.files.join(", "));
        }
        if !self.json {
            for w in &change.warnings {
                println!("  ! {w}");
            }
        }
        Ok(change)
    }

    /// Several edits, then what's next.
    pub async fn edits(&self, ops: &[Op]) -> Result<()> {
        if ops.is_empty() {
            bail!("say what to change (see --help)");
        }
        if !self.edit_all(ops).await?.files.is_empty() {
            self.next();
        }
        Ok(())
    }

    /// What to do after editing the config.
    pub fn next(&self) {
        if self.json {
            return;
        }
        if self.b.is_remote() {
            println!("\nSaved on the server. Next: `{BIN} plan`, then `{BIN} apply`.");
        } else {
            author::next_steps(self.dir);
            println!("Commit and push it so the bot's server has it too.");
        }
    }

    /// A line of text, or the value as JSON.
    fn say(&self, text: &str, v: &impl Serialize) -> Result<()> {
        if self.json {
            return print_json(v);
        }
        println!("{text}");
        Ok(())
    }

    fn done(&self, text: &str) -> Result<()> {
        self.say(&format!("✓ {}", text.trim()), &serde_json::json!({ "text": text.trim() }))
    }

    async fn schema(&self) -> Result<Schema> {
        self.b.schema().await
    }
}

// ---- status and drift ----

pub async fn status<R: Rest + Clone>(c: &Ctx<'_, R>) -> Result<()> {
    let s = c.b.status().await?;
    if c.json {
        return print_json(&s);
    }
    println!("{}", s.name);
    if !s.linked {
        println!("  Not applied to Discord yet.");
    }
    println!(
        "  {} roles · {} channels · {} projects · {} positions · {} waiting in the review queue",
        s.roles, s.channels, s.projects, s.positions, s.queue
    );
    for e in &s.errors {
        println!("  ✗ {e}");
    }
    for w in &s.warnings {
        println!("  ! {w}");
    }
    if s.errors.is_empty() && s.warnings.is_empty() {
        println!("  The config files have no problems.");
    }
    if s.queue > 0 {
        println!("\nSee them with `{BIN} queue list`.");
    }
    Ok(())
}

pub async fn drift<R: Rest + Clone>(c: &Ctx<'_, R>) -> Result<()> {
    let d = c.b.drift().await?;
    if c.json {
        return print_json(&d);
    }
    if d.is_clean() {
        println!("✓ Discord matches the config.");
    }
    let group = |head: &str, lines: &[chipsoc_admin::Line], hint: &str| {
        if lines.is_empty() {
            return;
        }
        println!("{head}");
        for l in lines {
            println!("  {}", l.text);
        }
        println!("  → {hint}\n");
    };
    group("Changed in the config, waiting to be applied:", &d.waiting, &format!("`{BIN} apply` sends them."));
    group(
        "Changed in Discord:",
        &d.discord,
        &format!("`{BIN} pull` keeps Discord's version; `{BIN} apply` puts the config's back."),
    );
    group(
        "Changed on both sides, or not known:",
        &d.person,
        &format!("make the config say what you want, then `{BIN} apply`."),
    );
    for w in &d.warnings {
        println!("! {w}");
    }
    Ok(())
}

// ---- activity, raw ops, schema ----

pub async fn activity<R: Rest + Clone>(c: &Ctx<'_, R>, limit: usize) -> Result<()> {
    let log = c.b.activity(limit).await?;
    if c.json {
        return print_json(&log);
    }
    if log.is_empty() {
        println!("Nothing logged yet.");
    }
    for e in &log {
        let actor = e.actor.as_deref().unwrap_or("-");
        let target = e.target.as_deref().map(|t| format!(" {t}")).unwrap_or_default();
        let detail = e.detail.as_deref().map(|d| format!(" ({d})")).unwrap_or_default();
        println!("{}  {actor:<20} {}{target}{detail}", when(e.ts), e.action);
    }
    Ok(())
}

/// `history [--admin <id>]`.
pub async fn history<R: Rest + Clone>(c: &Ctx<'_, R>, admin: Option<Snowflake>, limit: usize) -> Result<()> {
    let done = c.b.changes(admin, limit).await?;
    if c.json {
        return print_json(&done);
    }
    if done.is_empty() {
        println!("{}", if admin.is_some() { "Nothing this week." } else { "No config history (not a git checkout?)." });
    }
    for d in &done {
        let id = d.commit.as_deref().unwrap_or("-");
        println!("{}  {id:<9} {:<20} {}", when(d.at), d.who, d.what);
    }
    if admin.is_none() && !done.is_empty() {
        println!("`{BIN} revert <id>` undoes one (the server owner only).");
    }
    Ok(())
}

/// `revert <id> [--rewind]`: a new commit; applying it is the next step.
pub async fn revert<R: Rest + Clone>(c: &Ctx<'_, R>, change: &str, rewind: bool) -> Result<()> {
    use chipsoc_config::git::Undo;
    let (how, what) =
        if rewind { (Undo::Rewind, "Put the config back as it was right after") } else { (Undo::Revert, "Undo") };
    if !confirm(&format!("{what} change {change}? It's saved as a new change; Discord changes when you apply."), c.yes)?
    {
        bail!("cancelled");
    }
    println!("{}", c.b.undo(change, how).await?);
    Ok(())
}

/// `op '<json>'`: any edit the API knows (see `schema` and ops/mod.rs).
pub async fn raw_op<R: Rest + Clone>(c: &Ctx<'_, R>, json: &str) -> Result<()> {
    let op: Op = serde_json::from_str(json).context("that isn't an op (e.g. {\"op\": \"set_role\", …})")?;
    c.edits(&[op]).await
}

pub async fn schema<R: Rest + Clone>(c: &Ctx<'_, R>) -> Result<()> {
    print_json(&c.schema().await?)
}

/// A Unix time as `2026-11-04 13:05` (UTC).
pub fn when(ts: i64) -> String {
    chipsoc_model::utc_minute(ts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times_read_as_dates() {
        assert_eq!(when(0), "1970-01-01 00:00");
        assert_eq!(when(1_793_793_900), "2026-11-04 12:05");
        assert_eq!(when(951_782_400), "2000-02-29 00:00", "leap day");
    }
}

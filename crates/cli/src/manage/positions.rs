//! Positions people apply for, and whether they're open.

use super::*;

#[derive(Subcommand)]
pub enum PositionCmd {
    /// Committee positions and whether they're open.
    List,
    /// Add a position for an existing role.
    New {
        /// The position's role key (e.g. treasurer).
        role: String,
        /// Shown in /volunteer (default: the role's name).
        #[arg(long)]
        label: Option<String>,
        /// Holders also get the admin role (committee access).
        #[arg(long)]
        admin: bool,
        /// Not listed in /volunteer (e.g. filled by election).
        #[arg(long)]
        closed: bool,
    },
    /// Change a position's label, admin or open.
    Set {
        /// The position's role key (e.g. treasurer).
        role: String,
        #[command(flatten)]
        set: PositionSetArgs,
    },
    /// Edit the application form's questions, from a file (text or JSON) or in $EDITOR.
    Questions {
        /// The position's role key (e.g. treasurer).
        role: String,
        /// Read them from here (`-` for stdin).
        #[arg(long)]
        file: Option<PathBuf>,
    },
    /// The text above the form's questions (`--text none` takes it away).
    Intro {
        /// The position's role key (e.g. treasurer).
        role: String,
        /// The text.
        #[arg(long)]
        text: Option<String>,
        /// Read it from here (`-` for stdin).
        #[arg(long, conflicts_with = "text")]
        file: Option<PathBuf>,
    },
    /// List it in /volunteer.
    Open {
        /// The position's role key (e.g. treasurer).
        role: String,
    },
    /// Take it out of /volunteer.
    Close {
        /// The position's role key (e.g. treasurer).
        role: String,
    },
    /// Move it in /volunteer's list.
    Move {
        /// The position's role key (e.g. treasurer).
        role: String,
        /// `up`, `down`, or a place from 1.
        to: String,
    },
    /// Take a position out (the role stays).
    Remove {
        /// The position's role key (e.g. treasurer).
        role: String,
    },
}

#[derive(Args)]
pub struct PositionSetArgs {
    /// Or `none` for the role's name.
    #[arg(long)]
    label: Option<String>,
    /// on/off: holders also get the admin role (committee access).
    #[arg(long, value_parser = parse_bool, action = clap::ArgAction::Set)]
    admin: Option<bool>,
    /// on/off: listed in /volunteer.
    #[arg(long, value_parser = parse_bool, action = clap::ArgAction::Set)]
    open: Option<bool>,
}

pub async fn position<R: Rest + Clone>(c: &Ctx<'_, R>, cmd: PositionCmd) -> Result<()> {
    match cmd {
        PositionCmd::List => {
            let s = c.schema().await?;
            if c.json {
                return print_json(&s.bot.positions);
            }
            if s.bot.positions.is_empty() {
                println!("No positions yet. Add one with `{BIN} position new <role>`.");
            }
            for p in &s.bot.positions {
                let name = s.roles.iter().find(|r| r.key == p.role).map_or(p.role.as_str(), |r| r.name.as_str());
                let label = p.label.as_deref().unwrap_or(name);
                let open = if p.open { "open" } else { "closed" };
                let admin = if p.admin { " · admin" } else { "" };
                let held = s.holders.get(&p.role).map_or(0, Vec::len);
                println!(
                    "{:<20} {label:<24} {open}{admin} · {} · {}",
                    p.role,
                    chipsoc_admin::fmt::count(p.questions.len(), "question", "questions"),
                    chipsoc_admin::fmt::count(held, "holder", "holders"),
                );
            }
            Ok(())
        }
        PositionCmd::New { role, label, admin, closed } => {
            let position = chipsoc_config::bot::Position { role, label, admin, open: !closed, ..Default::default() };
            c.edits(&[Op::AddPosition { position }]).await
        }
        PositionCmd::Set { role, set } => {
            let s = edits::PositionSet { label: set.label, admin: set.admin, open: set.open };
            c.edits(&edits::position_ops(&role, &s)).await
        }
        PositionCmd::Questions { role, file } => {
            let s = c.schema().await?;
            let p = s.bot.positions.iter().find(|p| p.role == role).with_context(|| format!("no position `{role}`"))?;
            let json = file.as_ref().is_some_and(|f| f.extension().is_some_and(|e| e == "json"));
            let text = text_from(file.as_deref(), &chipsoc_config::ops::text::questions_text(&p.questions), ".txt")?;
            let questions = if json {
                serde_json::from_str(&text).context("reading the questions as JSON")?
            } else {
                chipsoc_config::ops::text::parse_questions(&text).map_err(anyhow::Error::msg)?
            };
            if questions == p.questions {
                println!("Nothing changed.");
                return Ok(());
            }
            c.edits(&[Op::SetPosition { role, set: PositionField::Questions(questions) }]).await
        }
        PositionCmd::Intro { role, text, file } => {
            let s = c.schema().await?;
            let p = s.bot.positions.iter().find(|p| p.role == role).with_context(|| format!("no position `{role}`"))?;
            let intro = match text {
                Some(t) => t,
                None => text_from(file.as_deref(), p.intro.as_deref().unwrap_or_default(), ".md")?,
            };
            c.edits(&[Op::SetPosition { role, set: PositionField::Intro(edits::optional(&intro)) }]).await
        }
        PositionCmd::Open { role } => c.edits(&[Op::SetPosition { role, set: PositionField::Open(true) }]).await,
        PositionCmd::Close { role } => c.edits(&[Op::SetPosition { role, set: PositionField::Open(false) }]).await,
        PositionCmd::Move { role, to } => c.edits(&[Op::MovePosition { role, to: parse_move(&to)? }]).await,
        PositionCmd::Remove { role } => {
            if !confirm(&format!("Remove the position for `{role}`? The role itself stays."), c.yes)? {
                return Ok(());
            }
            c.edits(&[Op::RemovePosition { role }]).await
        }
    }
}

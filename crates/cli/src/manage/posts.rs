//! Bot posts and onboarding.

use super::*;

#[derive(Subcommand)]
pub enum PostCmd {
    /// Channels with a post the bot keeps there.
    List,
    /// A channel's post, as written and as Discord gets it.
    Show {
        /// The channel's key.
        channel: String,
    },
    /// Edit a channel's post, from a file or in $EDITOR, after seeing how it reads.
    Set {
        /// The channel's key.
        channel: String,
        /// Read it from here (`-` for stdin).
        #[arg(long)]
        file: Option<PathBuf>,
    },
}

pub async fn post<R: Rest + Clone>(c: &Ctx<'_, R>, cmd: PostCmd) -> Result<()> {
    match cmd {
        PostCmd::List => {
            let s = c.schema().await?;
            if c.json {
                return print_json(&s.posts);
            }
            if s.posts.is_empty() {
                println!("No channel has a post yet. `{BIN} post set <channel>` adds one.");
            }
            for (k, p) in &s.posts {
                let first = p.body.lines().next().unwrap_or_default();
                println!("#{k:<20} {}  {first}", p.file);
            }
            Ok(())
        }
        PostCmd::Show { channel } => {
            let s = c.schema().await?;
            let p = s.posts.get(&channel).with_context(|| format!("#{channel} has no post"))?;
            let shown = c.b.preview(&p.body).await?;
            if c.json {
                return print_json(&serde_json::json!({ "file": p.file, "body": p.body, "rendered": shown }));
            }
            println!("{} ({}):\n{}", channel, p.file, indent(&p.body));
            if !shown.buttons.is_empty() {
                let buttons: Vec<String> =
                    shown.buttons.iter().map(|b| format!("[{}] → {}", b.label, b.action)).collect();
                println!("Buttons: {}", buttons.join(", "));
            }
            Ok(())
        }
        PostCmd::Set { channel, file } => {
            let s = c.schema().await?;
            let current = s.posts.get(&channel).map(|p| p.body.clone()).unwrap_or_default();
            let body = text_from(file.as_deref(), &current, ".md")?;
            post_body(c, body, &current, |body| Op::SetPost { channel, body }).await
        }
    }
}

#[derive(Subcommand)]
pub enum OnboardingCmd {
    /// The onboarding questions and their options (numbered from 1).
    List,
    /// Change a question's title.
    Title {
        /// The question's number in `onboarding list`.
        prompt: usize,
        /// The new title.
        title: String,
    },
    /// Make a question required or not.
    Required {
        /// The question's number in `onboarding list`.
        prompt: usize,
        /// on or off.
        #[arg(value_parser = parse_bool, action = clap::ArgAction::Set)]
        on: bool,
    },
    /// Add an option to a question.
    AddOption {
        /// The question's number in `onboarding list`.
        prompt: usize,
        /// What it says.
        #[arg(long)]
        title: String,
        /// The line under it.
        #[arg(long)]
        description: Option<String>,
        /// An emoji shown with it.
        #[arg(long)]
        emoji: Option<String>,
        /// The role key it gives.
        #[arg(long)]
        role: Option<String>,
        /// Channel keys it shows, comma-separated.
        #[arg(long)]
        channels: Option<String>,
    },
    /// Change which role an option gives (`none` for none).
    OptionRole {
        /// The question's number in `onboarding list`.
        prompt: usize,
        /// The option's number under it.
        option: usize,
        /// The role key, or `none`.
        role: String,
    },
    /// Take an option off a question.
    RemoveOption {
        /// The question's number in `onboarding list`.
        prompt: usize,
        /// The option's number under it.
        option: usize,
    },
}

/// A place counted from 1, as counted from 0.
pub(crate) fn place(n: usize, what: &str) -> Result<usize> {
    n.checked_sub(1).ok_or_else(|| anyhow!("{what}s are numbered from 1"))
}

pub async fn onboarding<R: Rest + Clone>(c: &Ctx<'_, R>, cmd: OnboardingCmd) -> Result<()> {
    let op = match cmd {
        OnboardingCmd::List => {
            let s = c.schema().await?;
            if c.json {
                return print_json(&s.onboarding);
            }
            let Some(o) = s.onboarding else {
                println!("No onboarding.toml: Discord's onboarding questions aren't managed.");
                return Ok(());
            };
            println!("Onboarding {}", if o.enabled { "on" } else { "off" });
            for (i, p) in o.prompts.iter().enumerate() {
                let req = if p.required { " (required)" } else { "" };
                let one = if p.single_select { "pick one" } else { "pick any" };
                println!("{}. {}{req} · {one}", i + 1, p.title);
                for (j, opt) in p.options.iter().enumerate() {
                    let role = if opt.role.is_empty() { String::new() } else { format!(" → @{}", opt.role) };
                    let chans = if opt.channels.is_empty() {
                        String::new()
                    } else {
                        format!(" · #{}", opt.channels.join(", #"))
                    };
                    println!("   {}. {}{role}{chans}", j + 1, opt.title);
                }
            }
            return Ok(());
        }
        OnboardingCmd::Title { prompt, title } => {
            Op::SetPrompt { prompt: place(prompt, "question")?, set: PromptField::Title(title) }
        }
        OnboardingCmd::Required { prompt, on } => {
            Op::SetPrompt { prompt: place(prompt, "question")?, set: PromptField::Required(on) }
        }
        OnboardingCmd::AddOption { prompt, title, description, emoji, role, channels } => Op::AddOption {
            prompt: place(prompt, "question")?,
            option: OnbOption {
                title,
                description,
                emoji,
                role: role.unwrap_or_default(),
                channels: channels.as_deref().map(edits::list).unwrap_or_default(),
            },
        },
        OnboardingCmd::OptionRole { prompt, option, role } => Op::SetOption {
            prompt: place(prompt, "question")?,
            option: place(option, "option")?,
            set: OptionField::Role(edits::optional(&role).unwrap_or_default()),
        },
        OnboardingCmd::RemoveOption { prompt, option } => {
            let op = Op::RemoveOption { prompt: place(prompt, "question")?, option: place(option, "option")? };
            if !confirm(&format!("{}?", op.describe()), c.yes)? {
                return Ok(());
            }
            op
        }
    };
    c.edits(&[op]).await
}

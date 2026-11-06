//! Projects: details, status, applications, briefs and leads.

use super::*;

#[derive(Subcommand)]
pub enum ProjectCmd {
    /// Add a project (asks a few questions; flags skip them).
    New {
        /// Its name.
        #[arg(long)]
        name: Option<String>,
        /// One-line description.
        #[arg(long)]
        blurb: Option<String>,
        /// Its area (bot.toml's [projects] areas).
        #[arg(long)]
        area: Option<String>,
        /// proposed, active or past.
        #[arg(long, value_parser = parse_status)]
        status: Option<ProjectStatus>,
    },
    /// Projects and their status.
    List,
    /// Everything about one project.
    Show {
        /// The project's slug.
        slug: String,
    },
    /// Change one field of a project.
    Set {
        /// The project's slug.
        slug: String,
        /// name, blurb, area, status, stage, github, channels, tags, team-role,
        /// lead-role, applications, lead-question or questions (`|`-separated).
        field: String,
        /// The new value.
        value: String,
    },
    /// Set a project's status (`past` moves its channels to the archive).
    Status {
        /// The project's slug.
        slug: String,
        /// The new status.
        #[arg(value_parser = statuses())]
        status: ProjectStatus,
    },
    /// Give a project a new slug, keeping its roles, channels and post (do it
    /// signed in, not on a laptop's files).
    Rename {
        /// The project's slug.
        slug: String,
        /// The new slug: lower-case letters, digits and dashes.
        to: String,
    },
    /// Open or close applications (an Apply button on its post).
    Applications {
        /// The project's slug.
        slug: String,
        /// on or off.
        #[arg(value_parser = parse_bool, action = clap::ArgAction::Set)]
        state: bool,
    },
    /// Edit its brief (the post in the projects forum), from a file or in $EDITOR.
    Brief {
        /// The project's slug.
        slug: String,
        /// Read it from here (`-` for stdin).
        #[arg(long)]
        file: Option<PathBuf>,
    },
    /// Add or remove a lead.
    #[command(subcommand)]
    Lead(LeadCmd),
}

#[derive(Subcommand)]
pub enum LeadCmd {
    /// Make someone a lead and give them the Lead role.
    Add {
        /// The project's slug.
        slug: String,
        /// Their @username, server name, or Discord user id.
        user: String,
    },
    /// Take someone off the leads; the Lead role is taken away.
    Remove {
        /// The project's slug.
        slug: String,
        /// Their @username, server name, or Discord user id.
        user: String,
    },
}

/// proposed, active or past, listed in the help (`finished` works too).
fn statuses() -> impl clap::builder::TypedValueParser<Value = ProjectStatus> {
    use clap::builder::TypedValueParser;
    let names =
        ["proposed", "active", "past", "finished"].map(|n| clap::builder::PossibleValue::new(n).hide(n == "finished"));
    clap::builder::PossibleValuesParser::new(names).map(|s| parse_status(&s).unwrap_or_default())
}

pub async fn project<R: Rest + Clone>(c: &Ctx<'_, R>, cmd: ProjectCmd) -> Result<()> {
    match cmd {
        ProjectCmd::New { name, blurb, area, status } => {
            let s = c.schema().await?;
            let areas = s.bot.projects.map(|p| p.areas).unwrap_or_default();
            let p = author::ask_project(author::NewProject { name, blurb, area, status }, &areas)?;
            let slug = slug(&p.name);
            c.edits(&[Op::AddProject { name: p.name, blurb: p.blurb, area: p.area, status: p.status }]).await?;
            if !c.json {
                println!("To take applications: `{BIN} project applications {slug} on`.");
            }
            Ok(())
        }
        ProjectCmd::List => {
            let projects = c.schema().await?.projects;
            if c.json {
                return print_json(&projects);
            }
            author::print_projects(&projects);
            Ok(())
        }
        ProjectCmd::Show { slug } => {
            let s = c.schema().await?;
            let p = s.projects.iter().find(|p| p.slug == slug).with_context(|| format!("no project `{slug}`"))?;
            if c.json {
                return print_json(p);
            }
            println!("{} ({}) · {}", p.display(), p.slug, p.status);
            let field = |k: &str, v: &str| {
                if !v.is_empty() {
                    println!("  {k:<13} {v}");
                }
            };
            field("blurb", &p.blurb);
            field("area", &p.kind);
            field("stage", p.stage.as_deref().unwrap_or_default());
            field("github", p.github.as_deref().unwrap_or_default());
            field("channels", &p.channels.join(", "));
            field("tags", &p.tags.join(", "));
            field("leads", &p.leads.iter().map(ToString::to_string).collect::<Vec<_>>().join(", "));
            field("applications", author::applications(p.application.open));
            field("questions", &p.application.questions.join(" | "));
            if let Some(b) = s.briefs.get(&slug) {
                println!("  brief ({}):\n{}", b.file, indent(&b.body));
            }
            Ok(())
        }
        ProjectCmd::Set { slug, field, value } => {
            let op = edits::project_op(&slug, &field, &value)?;
            if let Op::SetProject { set: ProjectField::Status(status), .. } = op
                && !confirm_status(c, &slug, status).await?
            {
                return Ok(());
            }
            c.edits(&[op]).await
        }
        ProjectCmd::Status { slug, status } => {
            if !confirm_status(c, &slug, status).await? {
                return Ok(());
            }
            c.edits(&[Op::SetProject { slug, set: ProjectField::Status(status) }]).await
        }
        ProjectCmd::Rename { slug, to } => {
            if !c.b.is_remote() && !c.json {
                println!(
                    "Note: the bot keeps its own lock.toml, so pushing this makes the project anew. \
                     Rename signed in to the bot's web server (`{BIN} login`), on the dashboard or with \
                     `/server project rename` instead."
                );
            }
            c.edits(&[Op::RenameProject { from: slug, to }]).await
        }
        ProjectCmd::Applications { slug, state } => {
            c.edits(&[Op::SetProject { slug: slug.clone(), set: ProjectField::Applications(state) }]).await?;
            if !c.json {
                println!("{slug}: applications {}", author::applications(state));
            }
            Ok(())
        }
        ProjectCmd::Brief { slug, file } => {
            let s = c.schema().await?;
            let current = s.briefs.get(&slug).map(|b| b.body.clone()).unwrap_or_default();
            let body = text_from(file.as_deref(), &current, ".md")?;
            post_body(c, body, &current, |body| Op::SetBrief { slug, body }).await
        }
        ProjectCmd::Lead(LeadCmd::Add { slug, user }) => c.done(&c.b.add_lead(&slug, c.b.user(&user).await?).await?),
        ProjectCmd::Lead(LeadCmd::Remove { slug, user }) => {
            let user = c.b.user(&user).await?;
            if !confirm(&format!("Take {user} off {slug}'s leads (and their Lead role)?"), c.yes)? {
                return Ok(());
            }
            c.done(&c.b.remove_lead(&slug, user).await?)
        }
    }
}

/// Every change of status does something to roles or channels: ask first.
pub(crate) async fn confirm_status<R: Rest + Clone>(c: &Ctx<'_, R>, slug: &str, to: ProjectStatus) -> Result<bool> {
    let s = c.schema().await?;
    let from = s.projects.iter().find(|p| p.slug == slug).with_context(|| format!("no project `{slug}`"))?.status;
    match chipsoc_config::projects::status_change(from, to) {
        Some(what) => confirm(&format!("Make `{slug}` {to}? {}{}.", what[..1].to_uppercase(), &what[1..]), c.yes),
        None => Ok(true),
    }
}

pub(crate) fn indent(text: &str) -> String {
    text.lines().map(|l| format!("    {l}")).collect::<Vec<_>>().join("\n")
}

/// Show a post as Discord gets it and save it, if it changed and the person agrees.
pub(crate) async fn post_body<R: Rest + Clone>(
    c: &Ctx<'_, R>,
    body: String,
    current: &str,
    op: impl FnOnce(String) -> Op,
) -> Result<()> {
    if body.trim() == current.trim() {
        println!("Nothing changed.");
        return Ok(());
    }
    let shown = c.b.preview(&body).await?;
    if !c.json {
        println!("It will read:\n{}", indent(&shown.content));
        for b in &shown.buttons {
            println!("    [{}] → {}", b.label, b.action);
        }
    }
    if !c.yes && tty() && !Confirm::new("Save it?").with_default(true).prompt()? {
        println!("Nothing changed.");
        return Ok(());
    }
    c.edits(&[op(body)]).await
}

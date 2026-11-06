//! Helpers that write config for you. They never touch Discord: review the
//! change with `git diff`, then `plan` and `apply`.

use std::io::IsTerminal;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};
use chipsoc_config::authoring::{self, Kind, Report};
use chipsoc_config::schema::{Channel, ChannelKind, Project, ProjectStatus, Role};
use chipsoc_config::slug;
use inquire::{Confirm, MultiSelect, Select, Text};

use crate::BIN;

fn interactive() -> bool {
    std::io::stdin().is_terminal()
}

pub fn next_steps(config: &Path) {
    println!("\nNext: check it with `git -C {} diff`, then `{BIN} plan` and `{BIN} apply`.", config.display());
}

pub fn print_report(r: &Report) {
    for w in &r.warnings {
        println!("  ! {w}");
    }
    for e in &r.errors {
        println!("  ✗ {e}");
    }
}

/// `check`: validate offline; fails on errors.
pub fn check(config: &Path) -> Result<()> {
    report(&authoring::check(config)?)
}

/// Show what `check` found; fails on errors.
pub fn report(r: &Report) -> Result<()> {
    print_report(r);
    if !r.errors.is_empty() {
        bail!("{} in the config", chipsoc_admin::fmt::count(r.errors.len(), "problem", "problems"));
    }
    println!("config OK");
    Ok(())
}

pub struct NewProject {
    pub name: Option<String>,
    pub blurb: Option<String>,
    pub area: Option<String>,
    pub status: Option<ProjectStatus>,
}

/// What `project new` wasn't told by flags, asked; `areas` to pick from.
pub fn ask_project(a: NewProject, areas: &[String]) -> Result<chipsoc_client::remote::NewProject> {
    let tty = interactive();
    let ask = |label: &str, help: &str, flag: &str| -> Result<String> {
        if !tty {
            bail!("missing --{flag}");
        }
        Ok(Text::new(label).with_help_message(help).prompt()?.trim().to_owned())
    };
    let name = match a.name {
        Some(n) => n,
        None => ask("Name", "Its codename", "name")?,
    };
    if name.is_empty() {
        bail!("a name is required");
    }
    let blurb = match a.blurb {
        Some(b) => b,
        None => ask("One-line description", "e.g. RISC-V CPU", "blurb")?,
    };
    let area = match a.area {
        Some(k) => k,
        None if tty && !areas.is_empty() => Select::new("Area", areas.to_vec()).prompt()?,
        None => String::new(),
    };
    let status = match a.status {
        Some(s) => s,
        None if tty => Select::new("Status", ProjectStatus::ALL.to_vec())
            .with_help_message("proposed: a forum post only; active: its roles and channels too")
            .prompt()?,
        None => ProjectStatus::Proposed,
    };
    Ok(chipsoc_client::remote::NewProject { name, blurb, area, status })
}

pub fn print_projects(projects: &[Project]) {
    if projects.is_empty() {
        println!("No projects yet. Add one with `{BIN} project new`.");
    }
    for p in projects {
        let open = if p.application.open { " · taking applications" } else { "" };
        let leads = if p.leads.is_empty() {
            String::new()
        } else {
            format!(" · {}", chipsoc_admin::fmt::count(p.leads.len(), "lead", "leads"))
        };
        println!("{:<14} {:<9} {}{open}{leads}", p.slug, p.status, p.display());
    }
}

pub fn applications(open: bool) -> &'static str {
    if open { "open (an Apply button appears on its post)" } else { "closed" }
}

/// What `role new` was told by flags.
#[derive(Default)]
pub struct NewRole {
    pub name: Option<String>,
    pub key: Option<String>,
    pub color: Option<String>,
    pub hoist: bool,
    pub permissions: Option<String>,
}

/// A new role from the flags, asking for the rest when someone's there.
pub fn ask_role(a: NewRole, all_permissions: &[String]) -> Result<Role> {
    let tty = interactive();
    let name = match a.name {
        Some(n) => n,
        None if tty => Text::new("Name").prompt()?,
        None => bail!("missing --name"),
    };
    if name.trim().is_empty() {
        bail!("a name is required");
    }
    let asking = tty && a.color.is_none() && a.permissions.is_none() && !a.hoist;
    let color = match a.color {
        Some(c) => c,
        None if asking => Text::new("Colour (#rrggbb, optional)").prompt()?,
        None => String::new(),
    };
    let hoist = a.hoist || (asking && Confirm::new("List its members separately?").with_default(false).prompt()?);
    let permissions = match a.permissions {
        Some(p) => crate::edits::list(&p).iter().map(|p| crate::edits::permission(p)).collect(),
        None if asking => MultiSelect::new("Extra permissions (most roles need none)", all_permissions.to_vec())
            .with_page_size(15)
            .prompt()?,
        None => Vec::new(),
    };
    Ok(Role {
        key: a.key.unwrap_or_else(|| slug(&name)),
        name: name.trim().to_owned(),
        color: crate::edits::optional(&color),
        hoist,
        permissions,
        ..Default::default()
    })
}

/// What `channel new` was told by flags.
#[derive(Default)]
pub struct NewChannel {
    /// Category key, or `none` for outside any.
    pub category: Option<String>,
    pub kind: Option<ChannelKind>,
    pub name: Option<String>,
    pub key: Option<String>,
    pub topic: Option<String>,
    pub tags: Option<String>,
}

/// A new channel and its category from the flags, asking for the rest when
/// someone's there. `cats` are (key, name).
pub fn ask_channel(a: NewChannel, cats: &[(String, String)]) -> Result<(Option<String>, Channel)> {
    let tty = interactive();
    let category = match a.category {
        Some(c) => crate::edits::optional(&c),
        None if tty => {
            let mut labels: Vec<String> = cats.iter().map(|(k, n)| format!("{n} ({k})")).collect();
            labels.push("(not in a category)".into());
            let picked = Select::new("Category", labels.clone()).prompt()?;
            cats.get(labels.iter().position(|l| *l == picked).unwrap_or(0)).map(|(k, _)| k.clone())
        }
        None => bail!("missing --category (a category key, or `none`)"),
    };
    let kind = match a.kind {
        Some(k) => k,
        None if tty && a.name.is_none() => Select::new("Type", ChannelKind::ALL.to_vec()).prompt()?,
        None => ChannelKind::Text,
    };
    let name = match a.name {
        Some(n) => n,
        None if tty => Text::new("Name").prompt()?,
        None => bail!("missing --name"),
    };
    if name.trim().is_empty() {
        bail!("a name is required");
    }
    let topic = match a.topic {
        Some(t) => t,
        None if tty && a.tags.is_none() => Text::new("Topic (optional)").prompt()?,
        None => String::new(),
    };
    let tags = match a.tags {
        Some(t) => crate::edits::list(&t),
        None if tty && kind.has_tags() => crate::edits::list(&Text::new("Tags, comma-separated (optional)").prompt()?),
        None => Vec::new(),
    };
    let ch = Channel {
        key: a.key.unwrap_or_else(|| slug(&name)),
        kind,
        name: if kind.is_voice() { name.trim().to_owned() } else { slug(&name) },
        topic: crate::edits::optional(&topic),
        tags,
        ..Default::default()
    };
    Ok((category, ch))
}

/// `open <kind> [key]`: $VISUAL/$EDITOR at the definition, then re-check.
pub fn open(config: &Path, kind: Kind, key: Option<&str>) -> Result<()> {
    let (path, line) = authoring::locate(config, kind, key)?;
    let editor = std::env::var("VISUAL").or_else(|_| std::env::var("EDITOR")).unwrap_or_else(|_| "nano".into());
    let mut cmd = Command::new(&editor);
    if editor.ends_with("code") || editor.ends_with("codium") {
        cmd.arg("--goto").arg(format!("{}:{line}", path.display()));
    } else {
        cmd.arg(format!("+{line}")).arg(&path);
    }
    let status = cmd.status().with_context(|| format!("launching {editor}"))?;
    if !status.success() {
        bail!("{editor} exited with {status}");
    }
    let r = authoring::check(config)?;
    print_report(&r);
    if r.errors.is_empty() {
        next_steps(config);
    }
    Ok(())
}

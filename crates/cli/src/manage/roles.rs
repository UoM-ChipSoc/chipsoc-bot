//! Roles: list, show, add, edit, move and remove.

use super::*;

#[derive(Subcommand)]
pub enum RoleCmd {
    /// Roles, top of the hierarchy first.
    List,
    /// Add a role at the bottom of the list (asks when flags are missing).
    New {
        /// Its name in Discord.
        #[arg(long)]
        name: Option<String>,
        /// Its permanent key (default: from the name).
        #[arg(long)]
        key: Option<String>,
        /// `#rrggbb`.
        #[arg(long)]
        color: Option<String>,
        /// List its members separately.
        #[arg(long)]
        hoist: bool,
        /// Comma-separated, e.g. `MANAGE_MESSAGES,MANAGE_THREADS`.
        #[arg(long)]
        permissions: Option<String>,
    },
    /// Change a role.
    Set {
        /// The role's key.
        key: String,
        #[command(flatten)]
        set: RoleSetArgs,
    },
    /// Move a role up or down the hierarchy.
    Move {
        /// The role's key.
        key: String,
        /// `up`, `down`, or a place from 1.
        to: String,
    },
    /// Take a role out of the config (Discord keeps it unless you apply with --delete).
    Remove {
        /// The role's key.
        key: String,
    },
}

#[derive(Args)]
pub struct RoleSetArgs {
    /// Its name in Discord.
    #[arg(long)]
    name: Option<String>,
    /// `#rrggbb`, or `none`.
    #[arg(long)]
    color: Option<String>,
    /// on/off: list its members separately.
    #[arg(long, value_parser = parse_bool, action = clap::ArgAction::Set)]
    hoist: Option<bool>,
    /// on/off: anyone can @mention it.
    #[arg(long, value_parser = parse_bool, action = clap::ArgAction::Set)]
    mentionable: Option<bool>,
    /// Comma-separated; replaces the list (`none` for none).
    #[arg(long)]
    permissions: Option<String>,
}

pub async fn role<R: Rest + Clone>(c: &Ctx<'_, R>, cmd: RoleCmd) -> Result<()> {
    match cmd {
        RoleCmd::List => {
            let roles = c.schema().await?.roles;
            if c.json {
                return print_json(&roles);
            }
            for r in roles {
                let color = r.color.map(|c| format!(" {c}")).unwrap_or_default();
                let managed = if r.managed { " (managed by Discord)" } else { "" };
                let perms =
                    if r.permissions.is_empty() { String::new() } else { format!("  [{}]", r.permissions.join(", ")) };
                println!("{:<24} {}{color}{managed}{perms}", r.key, r.name);
            }
            Ok(())
        }
        RoleCmd::New { name, key, color, hoist, permissions } => {
            let all = c.schema().await?.choices.permissions;
            let role = author::ask_role(author::NewRole { name, key, color, hoist, permissions }, &all)?;
            let key = role.key.clone();
            c.edits(&[Op::AddRole { role }]).await?;
            if !c.json {
                println!("It's at the bottom of the list; `{BIN} role move {key} up` ranks it higher.");
            }
            Ok(())
        }
        RoleCmd::Set { key, set } => {
            let s = edits::RoleSet {
                name: set.name,
                color: set.color,
                hoist: set.hoist,
                mentionable: set.mentionable,
                permissions: set.permissions,
            };
            c.edits(&edits::role_ops(&key, &s)).await
        }
        RoleCmd::Move { key, to } => c.edits(&[Op::MoveRole { key, to: parse_move(&to)? }]).await,
        RoleCmd::Remove { key } => {
            if !confirm(&format!("Take role `{key}` out of the config?"), c.yes)? {
                return Ok(());
            }
            c.edits(&[Op::RemoveRole { key }]).await
        }
    }
}

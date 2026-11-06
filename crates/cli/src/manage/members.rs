//! Who holds what, and people who left.

use super::*;

#[derive(Subcommand)]
pub enum MemberCmd {
    /// Positions and who holds them, each project's leads, and who left holding roles.
    List,
    /// What someone told the bot: their profile and join-flow answers.
    Info {
        /// Their @username, server name, or Discord user id.
        user: String,
    },
    /// Appoint someone to a position or role (recorded in the config, given now).
    #[command(alias = "add")]
    Appoint {
        /// A position or role key from members.toml (e.g. treasurer).
        role: String,
        /// Their @username, server name, or Discord user id.
        user: String,
    },
    /// Take someone out of a position or role; the role is taken away.
    Remove {
        /// A position or role key from members.toml (e.g. treasurer).
        role: String,
        /// Their @username, server name, or Discord user id.
        user: String,
    },
    /// Restore: give someone who came back the roles they held when they left.
    Restore {
        /// Their @username, server name, or Discord user id.
        user: String,
    },
    /// Forget: stop offering someone's old roles back.
    Forget {
        /// Their @username, server name, or Discord user id.
        user: String,
    },
}

pub async fn member<R: Rest + Clone>(c: &Ctx<'_, R>, cmd: MemberCmd) -> Result<()> {
    match cmd {
        MemberCmd::List => {
            let r = c.b.members().await?;
            if c.json {
                return print_json(&r);
            }
            let people = |p: &[chipsoc_admin::Person]| {
                if p.is_empty() {
                    "nobody".to_owned()
                } else {
                    p.iter().map(|x| format!("{} ({})", x.name, x.id)).collect::<Vec<_>>().join(", ")
                }
            };
            if r.holders.is_empty() {
                println!("No roles are managed yet. Add positions in bot.toml, or `role = []` lines in members.toml.");
            }
            for h in &r.holders {
                let position = h.position.as_deref().map(|p| format!(" [position: {p}]")).unwrap_or_default();
                println!("{:<20} {}{position}: {}", h.role, h.name, people(&h.people));
            }
            if !r.leads.is_empty() {
                println!("\nProject leads");
                for l in &r.leads {
                    println!("  {:<18} {}", l.project.as_deref().unwrap_or_default(), people(&l.people));
                }
            }
            if !r.former.is_empty() {
                println!("\nLeft holding roles (`{BIN} member restore <id>` if they're back)");
                for f in &r.former {
                    let roles: Vec<&str> = f.roles.iter().map(|n| n.name.as_str()).collect();
                    println!("  {} ({}): {} · left {}", f.person.name, f.person.id, roles.join(", "), when(f.since));
                }
            }
            Ok(())
        }
        MemberCmd::Appoint { role, user } => c.done(&c.b.appoint(&role, c.b.user(&user).await?).await?),
        MemberCmd::Remove { role, user } => {
            let user = c.b.user(&user).await?;
            if !confirm(&format!("Take {user} out of `{role}`? The role is taken away from them."), c.yes)? {
                return Ok(());
            }
            c.done(&c.b.unappoint(&role, user).await?)
        }
        MemberCmd::Info { user } => {
            let a = c.b.about(c.b.user(&user).await?).await?;
            if c.json {
                return print_json(&a);
            }
            print!("{}", about_text(&a));
            Ok(())
        }
        MemberCmd::Restore { user } => c.done(&c.b.restore(c.b.user(&user).await?).await?),
        MemberCmd::Forget { user } => {
            let user = c.b.user(&user).await?;
            if !confirm(&format!("Forget the roles {user} held? They won't be offered back."), c.yes)? {
                return Ok(());
            }
            c.b.forget(user).await?;
            c.done("Forgotten.")
        }
    }
}

/// What someone told the bot, as `member info` prints it.
pub(crate) fn about_text(a: &chipsoc_admin::join::About) -> String {
    if a.is_empty() {
        return format!("{}: hasn't been through the join flow or set a profile.\n", a.id);
    }
    let shown = if a.shown.is_empty() { "not in the server now".to_owned() } else { a.shown.clone() };
    let mut out = format!("{} ({shown})\n", a.id);
    let name = format!("{} {}", a.name, a.surname).trim().to_owned();
    if !name.is_empty() {
        out.push_str(&format!("  Name:    {name}\n"));
    }
    if !a.company.is_empty() {
        out.push_str(&format!("  Company: {}\n", a.company));
    }
    if !a.answers.is_empty() {
        let finished = a.finished.map(|t| format!(", finished {}", when(t))).unwrap_or_default();
        out.push_str(&format!("  Join flow{finished}:\n"));
        for s in &a.answers {
            let said = if s.answer.is_empty() { "—" } else { &s.answer };
            out.push_str(&format!("    {} {said}\n", s.question));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use chipsoc_admin::join::{About, Said};
    use chipsoc_config::Snowflake;

    #[test]
    fn member_info_lists_the_profile_and_answers() {
        let a = About {
            id: Snowflake(7),
            shown: "Ada L.".into(),
            name: "Ada".into(),
            company: "Arm".into(),
            answers: vec![Said { question: "Placement?".into(), answer: String::new() }],
            ..About::default()
        };
        let text = super::about_text(&a);
        assert_eq!(text, "7 (Ada L.)\n  Name:    Ada\n  Company: Arm\n  Join flow:\n    Placement? —\n");
        assert!(super::about_text(&About { id: Snowflake(8), ..About::default() }).contains("hasn't been through"));
    }
}

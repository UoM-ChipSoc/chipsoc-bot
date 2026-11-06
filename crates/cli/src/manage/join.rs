//! The join flow.

use super::*;

#[derive(Subcommand)]
pub enum JoinCmd {
    /// How many have finished it, and who's been waiting.
    Status,
    /// DM members who haven't been through it (once each, ever); says how many first.
    Notify,
}

pub async fn join<R: Rest + Clone>(c: &Ctx<'_, R>, cmd: JoinCmd) -> Result<()> {
    match cmd {
        JoinCmd::Status => {
            let p = c.b.join().await?;
            if c.json {
                return print_json(&p);
            }
            println!("{} finished · {} sent the notice", p.finished, p.notified);
            if !p.waiting.is_empty() {
                let who: Vec<String> = p.waiting.iter().map(ToString::to_string).collect();
                println!("Waiting to finish (past the reminder): {}", who.join(", "));
            }
            Ok(())
        }
        JoinCmd::Notify => {
            let n = c.b.join_notice().await?;
            if n.members == 0 {
                return c.done("Nobody to tell: everyone has been through the flow or had the DM.");
            }
            if !c.json {
                println!("The DM:\n  {}", n.text);
            }
            let q = format!("DM {} members who haven't been through the join flow (once each, ever)?", n.members);
            if !confirm(&q, c.yes)? {
                return Ok(());
            }
            c.done(&c.b.send_join_notice().await?)
        }
    }
}

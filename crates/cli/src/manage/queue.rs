//! The review queue.

use super::*;
use chipsoc_admin::AppKind;

#[derive(Subcommand)]
pub enum QueueCmd {
    /// What's waiting for a decision.
    List,
    /// One with its answers.
    Show {
        /// Its number in `queue list`.
        id: i64,
    },
    /// Accept: they get the role (or join the team, or it's posted or followed) and a DM;
    /// a partner's is posted, and the partner isn't told.
    Accept {
        /// Its number in `queue list`.
        id: i64,
    },
    /// Reject; they get a DM (a partner isn't told).
    Reject {
        /// Its number in `queue list`.
        id: i64,
        /// Reason (optional): sent to them, or for a partner's, only kept in the history.
        #[arg(long)]
        reason: Option<String>,
    },
    /// Make an accepted project applicant one of its leads.
    Lead {
        /// Its number in `queue list`.
        id: i64,
    },
    /// The latest decisions.
    History {
        /// How many.
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
}

pub async fn queue<R: Rest + Clone>(c: &Ctx<'_, R>, cmd: QueueCmd) -> Result<()> {
    match cmd {
        QueueCmd::List => {
            let q = c.b.queue().await?;
            if c.json {
                return print_json(&q);
            }
            if q.is_empty() {
                println!("Nothing waiting.");
            }
            for a in &q {
                let lead = if a.as_lead { " · would like to lead" } else { "" };
                let kind = AppKind::label_of(&a.kind);
                println!("#{:<5} {kind:<20} {:<24} from {} · {}{lead}", a.id, a.what, from(a), when(a.created_at));
            }
            if !q.is_empty() {
                println!("\n`{BIN} queue show <id>` for the answers; then `queue accept <id>` or `queue reject <id>`.");
            }
            Ok(())
        }
        QueueCmd::Show { id } => {
            let q = c.b.queue().await?;
            let a = q.iter().find(|a| a.id == id).with_context(|| format!("no application #{id} is waiting"))?;
            if c.json {
                return print_json(a);
            }
            let kind = AppKind::label_of(&a.kind);
            println!("#{} {kind}: {} from {} ({})", a.id, a.what, from(a), when(a.created_at));
            for ans in &a.answers {
                println!("\n{}\n{}", ans.question, indent(if ans.answer.is_empty() { "(blank)" } else { &ans.answer }));
            }
            Ok(())
        }
        QueueCmd::Accept { id } => {
            let d = c.b.decide(id, true, None).await?;
            if c.json {
                return print_json(&d);
            }
            println!("✓ Accepted #{id}. {}", d.outcome);
            if d.lead {
                println!("They'd like to lead it: `{BIN} queue lead {id}` if agreed.");
            }
            Ok(())
        }
        QueueCmd::Reject { id, reason } => {
            let kind = c.b.queue().await?.into_iter().find(|a| a.id == id).map(|a| a.kind).unwrap_or_default();
            let (_, rejecting, label) = chipsoc_admin::says_of(&kind);
            let reason = match reason {
                Some(r) => Some(r),
                None if tty() && !c.yes => edits::optional(&Text::new(label).prompt()?),
                None => None,
            };
            if !confirm(&format!("Reject #{id}? {rejecting}"), c.yes)? {
                return Ok(());
            }
            let d = c.b.decide(id, false, reason.as_deref()).await?;
            if c.json {
                return print_json(&d);
            }
            println!("✓ Rejected #{id}.{}", if d.told { " They've been told by DM." } else { "" });
            Ok(())
        }
        QueueCmd::Lead { id } => c.done(&c.b.make_lead(id).await?),
        QueueCmd::History { limit } => {
            let h = c.b.history(limit).await?;
            if c.json {
                return print_json(&h);
            }
            if h.is_empty() {
                println!("No decisions yet.");
            }
            for p in &h {
                let by = p.by.as_deref().or(p.reviewed_by.as_deref()).map(|b| format!(" by {b}")).unwrap_or_default();
                let at = p.reviewed_at.map(|t| format!(" on {}", when(t))).unwrap_or_default();
                let why = p.reason.as_deref().map(|r| format!(" ({r})")).unwrap_or_default();
                let kind = AppKind::label_of(&p.kind);
                let from = if p.from.is_empty() { &p.user } else { &p.from };
                println!("#{:<5} {kind:<20} {:<24} from {from}: {}{by}{at}{why}", p.id, p.what, p.status);
            }
            Ok(())
        }
    }
}

/// Who sent it, by name (their id if the server didn't say).
fn from(a: &chipsoc_admin::Pending) -> &str {
    if a.from.is_empty() { &a.user } else { &a.from }
}

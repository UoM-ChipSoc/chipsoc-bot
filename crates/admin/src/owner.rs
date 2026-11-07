//! What only the server owner may do (see [`chipsoc_engine::owner_only`]),
//! going back on config changes, and who did what.

use anyhow::Result;
use chipsoc_config::Snowflake;
use chipsoc_config::git::{self, Undid, Undo};
use chipsoc_discord::{Method, Rest};
use chipsoc_engine::Ask;
use serde::{Deserialize, Serialize};

use crate::{Actor, Host, Refusal, fmt};

/// The server's owner, as Discord says now (see [`chipsoc_engine::owner_of`]).
/// `Err` when Discord couldn't say: what only the owner may do is then refused.
pub async fn owner_id<H: Host>(h: &H) -> Result<Snowflake> {
    chipsoc_engine::owner_of(h.rest(), h.guild()).await.inspect_err(|e| {
        tracing::warn!("checking the server owner failed: {e:#}");
    })
}

/// Refuse what only the server owner may do (see
/// [`chipsoc_engine::owner_only`]) unless `by` is the owner. When Discord
/// can't say who the owner is, it's refused, saying so.
pub async fn only_owner<H: Host>(h: &H, by: &Actor, ask: &Ask<'_>) -> Result<(), Refusal> {
    // Not knowing the owner makes this as strict as it gets: nothing to ask.
    if chipsoc_engine::owner_only(ask, None).is_none() {
        return Ok(());
    }
    let by = by.by(h.trusted());
    match owner_id(h).await {
        Ok(owner) => chipsoc_engine::owner_check(&by, ask, Some(owner)).map_err(Refusal::OwnerOnly),
        Err(_) => chipsoc_engine::owner_check(&by, ask, None).map_err(|_| Refusal::OwnerUnknown),
    }
}

/// Go back on config commit `commit` (its id, or the start of it): revert
/// just it, or rewind the config to how it was right after it. The server
/// owner only. Saved as a new commit (history is never rewritten) and
/// audited, and told in the bot log; like any config change, it reaches
/// Discord when applied, and what it removes is archived, not deleted.
/// `Ok(None)`: nothing to change. [`undo_said`] words the outcome.
pub async fn undo<H: Host>(h: &H, commit: &str, how: Undo, by: &Actor) -> Result<Option<Undid>, Refusal> {
    only_owner(h, by, &Ask::Undo).await?;
    let made = h.undo(commit, how, &by.by(h.trusted())).await.map_err(|e| {
        tracing::warn!("going back on config commit {commit} failed: {e:#}");
        Refusal::from(fmt::error(&e))
    })?;
    if let Some(m) = &made {
        crate::log_bot(h, &format!("↩️ {} changed the config: {}.", by.mention(), fmt::escape(&m.subject))).await;
    }
    Ok(made)
}

/// What [`undo`] came to, worded the same in every front end.
pub fn undo_said(made: &Result<Option<Undid>, Refusal>) -> crate::Said {
    let said = crate::Said::new();
    match made {
        Ok(Some(m)) => {
            let said = said
                .text("Saved as a new change: ")
                .text(m.subject.trim_end_matches('.'))
                .text(". Apply to make Discord match (anything removed is archived).");
            match &m.warning {
                Some(w) => said.text(format!(" {w}")),
                None => said,
            }
        }
        Ok(None) => said.text("Nothing to change: the config already matches."),
        Err(why) => crate::Said::md(&format!("Not changed: {}.", why.markdown().trim().trim_end_matches('.'))),
    }
}

/// What to ask before [`undo`]: change `short` (with its `subject`, if
/// known), the same in every front end.
pub fn undo_question(short: &str, subject: Option<&str>, how: Undo) -> crate::Said {
    let mut what = crate::Said::new().code(short);
    if let Some(s) = subject {
        what = what.text(format!(" (\"{}\")", s.trim()));
    }
    let after = " It's saved as a new change, and Discord changes only when you apply it.";
    match how {
        Undo::Revert => crate::Said::new()
            .text("Undo change ")
            .then(what)
            .text("? Only that change is undone; later ones stay.")
            .text(after),
        Undo::Rewind => crate::Said::new()
            .text("Put the config back as it was right after ")
            .then(what)
            .text("? ")
            .strong("Every later change is undone.")
            .text(after),
    }
}

/// [`undo_question`] for a commit id (or the start of one) in the config at `dir`.
pub fn undo_question_for(dir: &std::path::Path, commit: &str, how: Undo) -> crate::Said {
    let found = git::log(dir, 1000).into_iter().find(|c| !commit.is_empty() && c.id.starts_with(commit));
    match found {
        Some(c) => undo_question(&c.short, Some(&c.subject), how),
        None => undo_question(commit, None, how),
    }
}

/// One thing done through the bot, for `/server history` and the web's History page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Done {
    /// When (Unix seconds).
    pub at: i64,
    /// Who: a name (an id only if Discord couldn't say theirs).
    pub who: String,
    pub who_id: Option<Snowflake>,
    /// What, in words.
    pub what: String,
    /// The config commit it made, if it's one.
    pub commit: Option<String>,
    /// Whether that commit changed config (so reverting it changes something).
    pub revertible: bool,
}

/// How far back [`history_of`] looks for what one person did.
pub const WEEK: i64 = 7 * 24 * 60 * 60;

/// What happened, newest first, at most `limit`. Without `user`: the
/// config's latest commits, with who made them. With `user`: what they did
/// in the last [`WEEK`], from the audit log and the config's history.
pub async fn history_of<H: Host>(h: &H, user: Option<Snowflake>, limit: usize) -> Result<Vec<Done>> {
    let since = crate::now() - WEEK;
    let dir = h.cfg().dir.clone();
    let commits = git::blocking(move || Ok(git::log(&dir, 200))).await?;
    let from_commit = |c: git::Commit| Done {
        at: c.at,
        who: c.who,
        who_id: c.who_id,
        what: c.subject,
        commit: Some(c.short),
        revertible: c.config,
    };
    let Some(user) = user else {
        return Ok(commits.into_iter().take(limit).map(from_commit).collect());
    };
    let in_git = !commits.is_empty();
    // The audit log keeps ids: their name from a commit they signed, else as Discord shows them.
    let signed = commits.iter().find(|c| c.who_id == Some(user)).map(|c| c.who.clone());
    let name = match signed {
        Some(n) => n,
        None => match h.rest().call(Method::Get, &format!("guilds/{}/members/{user}", h.guild()), None).await {
            Ok(m) => chipsoc_discord::wire::shown_name(&m).unwrap_or_else(|| user.to_string()),
            Err(_) => user.to_string(),
        },
    };
    let mut out: Vec<Done> =
        commits.into_iter().filter(|c| c.who_id == Some(user) && c.config && c.at >= since).map(from_commit).collect();
    // Edits and reverts are commits too: listed once, as those.
    let committed = ["config_edit", "revert", "rewind"];
    for e in h.db().audit_by(&user.to_string(), since, limit as i64).await? {
        if in_git && committed.contains(&e.action.as_str()) {
            continue;
        }
        let mut what = fmt::did(&e.action);
        for part in [&e.target, &e.detail].into_iter().flatten().filter(|p| !p.is_empty()) {
            what.push_str(&format!(" · {part}"));
        }
        out.push(Done { at: e.ts, who: name.clone(), who_id: Some(user), what, commit: None, revertible: false });
    }
    out.sort_by_key(|d| std::cmp::Reverse(d.at));
    out.truncate(limit);
    Ok(out)
}

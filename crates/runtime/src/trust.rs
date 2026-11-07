//! Changes pushed to the config repo that the bot doesn't take on trust (see
//! [`chipsoc_config::trust`]): sensitive settings and lock.toml entries the
//! repo dropped wait for an admin's Approve in the bot log; ids the repo
//! changed are refused and reported.

use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use chipsoc_admin::trust as kept;
use chipsoc_config::Config;
use chipsoc_config::trust::{Entry, Sensitive, unlink};
use chipsoc_engine::Baseline;
use twilight_model::application::interaction::Interaction;
use twilight_model::channel::message::component::ButtonStyle;

use crate::apps::invoker;
use crate::commands::CustomId;
use chipsoc_admin::fmt;

use crate::{Ctx, control, reload, ui};

/// Files whose commits may change sensitive settings.
const FILES: &[&str] = &["bot.toml", "federation.toml", "members.toml", "lock.toml"];

pub fn baseline(ctx: &Ctx) -> Arc<dyn Baseline> {
    chipsoc_admin::trust::StoreBaseline::new(ctx.store.clone(), &ctx.cfg().dir)
}

/// What the files ask for that the bot holds back.
#[derive(Debug, Default)]
struct Pending {
    approved: Sensitive,
    /// The files' sensitive settings, if they differ.
    new: Option<Sensitive>,
    unlinks: Vec<Entry>,
    refused: Vec<String>,
}

impl Pending {
    fn hash(&self) -> String {
        let s = serde_json::json!([self.new.as_ref().map(Sensitive::hash), self.unlinks, self.refused]);
        chipsoc_model::stable_hash(s.to_string())
    }

    fn is_empty(&self) -> bool {
        self.new.is_none() && self.unlinks.is_empty() && self.refused.is_empty()
    }

    fn needs_approval(&self) -> bool {
        self.new.is_some() || !self.unlinks.is_empty()
    }
}

async fn pending(ctx: &Ctx, dir: &Path) -> Result<Pending> {
    let mut cfg = Config::load(dir)?;
    let t = kept::load(&ctx.store, &kept::scope(dir)).await?;
    let v = t.vet(&mut cfg);
    Ok(Pending {
        approved: t.approved.unwrap_or_default(),
        new: v.held,
        unlinks: v.lock.unlinks,
        refused: v.lock.refused,
    })
}

/// Who made the commits changing sensitive files since the last approval,
/// by Discord id, from the bot's own records (see [`kept::made`]): what a
/// commit says about its author is never taken on trust.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct Authors {
    /// Those who made some of them through the bot, the web UI or the CLI on the bot's machine.
    ids: Vec<chipsoc_model::Snowflake>,
    /// Whether some were pushed to the repo another way (nobody knows by whom).
    pushed: bool,
}

async fn authors(ctx: &Ctx, dir: &Path) -> Authors {
    let scope = kept::scope(dir);
    let since = kept::approved_at(&ctx.store, &scope).await.ok().flatten();
    let (d, files) = (dir.to_owned(), FILES);
    let changed = chipsoc_config::git::blocking(move || Ok(chipsoc_config::git::changed(&d, since.as_deref(), files)))
        .await
        .unwrap_or_default();
    let mut who = Authors::default();
    for fp in changed {
        match kept::made_by(&ctx.store, &scope, &fp).await {
            Ok(Some(id)) => who.ids.push(id),
            Ok(None) => who.pushed = true,
            Err(e) => {
                tracing::warn!("reading who made a config commit failed: {e:#}");
                who.pushed = true;
            }
        }
    }
    who.ids.sort();
    who.ids.dedup();
    who
}

fn text(cfg: &Config, p: &Pending, authors: &Authors, owner: Option<chipsoc_model::Snowflake>) -> String {
    let names = fmt::Names::new(cfg);
    let (role, channel) = (|k: &str| names.role(k), |k: &str| names.channel(k));
    let mut out = Vec::new();
    if p.needs_approval() {
        out.push(format!("{} **These config changes wait for an Approve.**", fmt::ACCESS));
        out.push(
            "They change who's an admin or what the bot trusts; until then the bot keeps the current settings."
                .to_owned(),
        );
        let mut lines = Vec::new();
        if let Some(new) = &p.new {
            lines.extend(p.approved.changes_named(new, &role, &channel));
        }
        for e in &p.unlinks {
            lines.push(format!("lock.toml drops `{e}`: the bot would stop managing it"));
        }
        out.extend(fmt::bullets(&lines, 12));
        let owner_only = p
            .new
            .as_ref()
            .and_then(|new| chipsoc_engine::owner_only(&chipsoc_engine::Ask::Approve { old: &p.approved, new }, owner));
        let made: Vec<String> = authors.ids.iter().map(|id| format!("<@{id}>")).collect();
        out.push(match owner_only {
            Some(why) => format!("{why}."),
            None if made.is_empty() => {
                "Pushed to the config repo, not made through the bot, so any admin can approve.".to_owned()
            }
            None if authors.pushed => format!(
                "Made by {} (and partly pushed to the config repo), so another admin approves.",
                fmt::collapse(&made, 5)
            ),
            None => format!("Made by {}, so another admin approves.", fmt::collapse(&made, 5)),
        });
    }
    if !p.refused.is_empty() {
        let keys: Vec<String> = p.refused.iter().map(|r| format!("`{r}`")).collect();
        out.push(format!(
            "{} **Refused:** lock.toml in the repo changes which Discord object {} {}. The bot keeps its own ids \
             (it never takes new ids for existing keys from the repo).",
            fmt::ATTENTION,
            fmt::collapse(&keys, 5),
            if keys.len() == 1 { "is" } else { "are" }
        ));
    }
    ui::truncate(&out.join("\n"), ui::MAX_CONTENT - 1)
}

/// Where the last approval card went: `channel/message`.
const CARD_AT: &str = "prompted_at";

/// After the config loads, and on every sync: say in the bot log what's
/// held back, once per change. A card is remembered only once it's posted,
/// so one that couldn't be (or was deleted since) is posted again.
pub async fn review(ctx: &Ctx, dir: &Path) {
    let p = match pending(ctx, dir).await {
        Ok(p) => p,
        Err(e) => return tracing::warn!("checking the config's sensitive settings failed: {e:#}"),
    };
    let scope = kept::scope(dir);
    let hash = p.hash();
    if p.is_empty() {
        return;
    }
    if kept::prompted(&ctx.store, &scope).await.ok().flatten().as_deref() == Some(hash.as_str())
        && (!p.needs_approval() || card_still_there(ctx, &scope).await)
    {
        return;
    }
    post_card(ctx, dir, &p).await;
}

/// Whether the last card posted is still in the bot log (true if that can't be told).
async fn card_still_there(ctx: &Ctx, scope: &str) -> bool {
    let Ok(Some(at)) = ctx.store.trusted(scope, CARD_AT).await else { return true };
    let Some((channel, message)) = at.split_once('/') else { return true };
    match ctx.call(chipsoc_discord::Method::Get, &format!("channels/{channel}/messages/{message}"), None).await {
        Ok(_) => true,
        Err(e) => chipsoc_discord::ApiError::status_of(&e) != Some(404),
    }
}

/// Post the card for `p` in the bot log; once it's there, remember it.
/// False if it couldn't be posted.
async fn post_card(ctx: &Ctx, dir: &Path, p: &Pending) -> bool {
    let scope = kept::scope(dir);
    let hash = p.hash();
    let Some(ch) = ctx.bot_log() else { return false };
    let owner = if p.needs_approval() { chipsoc_admin::owner_id(ctx).await.ok() } else { None };
    let body = text(&ctx.cfg(), p, &authors(ctx, dir).await, owner);
    let buttons = if p.needs_approval() {
        let ok = CustomId::Trust { approve: true, hash: hash.clone() }.to_string();
        let no = CustomId::Trust { approve: false, hash: hash.clone() }.to_string();
        vec![ui::row(vec![
            ui::button(&ok, "Approve", ButtonStyle::Success),
            ui::button(&no, "Reject", ButtonStyle::Danger),
        ])]
    } else {
        Vec::new()
    };
    let sent = async {
        let message = Ctx::message(&body, Some(&buttons))?;
        ctx.call(chipsoc_discord::Method::Post, &format!("channels/{ch}/messages"), Some(message)).await
    };
    let posted = match sent.await {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!("posting an approval card failed (tried again on the next sync): {e:#}");
            return false;
        }
    };
    ctx.log_err(kept::set_prompted(&ctx.store, &scope, &hash).await, "remembering an approval card");
    if let Some(id) = posted["id"].as_str() {
        ctx.log_err(
            ctx.store.set_trusted(&scope, CARD_AT, &format!("{ch}/{id}")).await,
            "remembering an approval card",
        );
    }
    true
}

/// `/server approvals`: what waits for an Approve, with the card posted
/// again in the bot log (for one lost or buried there).
pub async fn approvals(ctx: &Ctx) -> String {
    let dir = ctx.cfg().dir.clone();
    let p = match pending(ctx, &dir).await {
        Ok(p) => p,
        Err(e) => return format!("{} **Couldn't read the config:** {}", fmt::ATTENTION, fmt::error(&e)),
    };
    if p.is_empty() {
        return "Nothing waits for an Approve: the bot uses the config's settings as they are.".to_owned();
    }
    let Some(ch) = ctx.bot_log() else {
        return format!("{} There's no bot log channel to post the card in (`[log] bot`).", fmt::ATTENTION);
    };
    if !post_card(ctx, &dir, &p).await {
        return format!("{} Couldn't post the card in <#{ch}>; is the bot allowed to post there?", fmt::ATTENTION);
    }
    if p.needs_approval() {
        format!("{} Posted the card again in <#{ch}>: press **Approve** or **Reject** there.", fmt::DONE)
    } else {
        format!("{} Nothing waits for an Approve; the card in <#{ch}> says what the bot refused.", fmt::DONE)
    }
}

/// Approve or Reject under a card. Only admins under the approved settings,
/// and not whoever made the change (unless they own the server).
pub async fn button(ctx: &Ctx, i: &Interaction, approve: bool, hash: &str) {
    if !ctx.is_admin(i.member.as_ref()) {
        return ctx.respond(i, &ui::ephemeral("Only admins can approve or reject config changes.")).await;
    }
    let Some(user) = invoker(i) else { return };
    let dir = ctx.cfg().dir.clone();
    let one = ctx.changing.lock().await;
    let p = match pending(ctx, &dir).await {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!("reading the config for an approval failed: {e:#}");
            let text = format!("{} **Couldn't read the config:** {}", fmt::ATTENTION, fmt::error(&e));
            return ctx.respond(i, &ui::ephemeral(&text)).await;
        }
    };
    if !p.needs_approval() {
        return ctx.respond(i, &ui::update("Nothing in the config waits for approval any more.")).await;
    }
    if p.hash() != hash {
        let why = "The config changed since this was posted; see the newer message in the bot log.";
        return ctx.respond(i, &ui::ephemeral(why)).await;
    }
    let original = i.message.as_ref().map(|m| m.content.clone()).unwrap_or_default();
    let me = format!("<@{}>", user.id);
    if !approve {
        ctx.audit(Some(&user.id.to_string()), "reject_config_change", None, Some(hash)).await;
        let note = format!(
            "\n\n{} **Rejected by {me}.** The bot keeps the approved settings; undo the commit in the config repo, or the owner can `/server revert` it.",
            fmt::FAILED
        );
        let done = fmt::with_note(&original, &note);
        return ctx.respond(i, &ui::update(&done)).await;
    }
    let owner_id = chipsoc_admin::owner_id(ctx).await.ok();
    if let Some(new) = &p.new {
        let ask = chipsoc_engine::Ask::Approve { old: &p.approved, new };
        if let Err(why) = chipsoc_engine::owner_check(&control::by(i), &ask, owner_id) {
            let why = if owner_id.is_some() { format!("{why}.") } else { chipsoc_engine::OWNER_UNKNOWN.to_owned() };
            return ctx.respond(i, &ui::ephemeral(&why)).await;
        }
    }
    // The owner may approve their own change: they could make it in Discord anyway.
    let me_id = chipsoc_model::Snowflake(user.id.get());
    if owner_id != Some(me_id) && authors(ctx, &dir).await.ids.contains(&me_id) {
        let why = "You made this change, so another admin has to approve it.";
        return ctx.respond(i, &ui::ephemeral(why)).await;
    }
    let scope = kept::scope(&dir);
    let saved = async {
        if let Some(new) = &p.new {
            kept::approve(&ctx.store, &scope, new, chipsoc_config::git::head_full(&dir).as_deref()).await?;
        }
        if !p.unlinks.is_empty()
            && let Some(mut lock) = kept::load(&ctx.store, &scope).await?.lock
        {
            unlink(&mut lock, &p.unlinks);
            kept::save_lock(&ctx.store, &scope, &lock).await?;
        }
        anyhow::Ok(())
    }
    .await;
    if let Err(e) = saved {
        tracing::warn!("saving an approval failed: {e:#}");
        let text = format!("{} **Couldn't save the approval:** {}", fmt::ATTENTION, fmt::error(&e));
        return ctx.respond(i, &ui::ephemeral(&text)).await;
    }
    ctx.audit(Some(&user.id.to_string()), "approve_config_change", None, Some(hash)).await;
    ctx.respond(i, &ui::defer_update()).await;
    drop(one);
    reload::reload(ctx, &dir).await;
    // Who holds the admin roles was part of what was approved: give and take them now.
    let changed: Vec<String> = match &p.new {
        Some(new) => new
            .holders
            .keys()
            .chain(p.approved.holders.keys())
            .filter(|k| new.holders.get(*k) != p.approved.holders.get(*k))
            .cloned()
            .collect(),
        None => Vec::new(),
    };
    let mut done = format!("\n\n{} **Approved by {me}.**", fmt::DONE);
    if !changed.is_empty() {
        let holders = |a: &chipsoc_model::Action| matches!(a, chipsoc_model::Action::Edit { kind: chipsoc_model::ObjectKind::Holders, key, .. } if changed.contains(key));
        let applied = match control::begin(ctx).await {
            Ok(mut c) => {
                let r = c.engine.apply_only(holders, &mut control::noop).await;
                c.end().await;
                r
            }
            Err(e) => Err(e),
        };
        match applied {
            Ok(r) if r.failed().next().is_none() => done.push_str(" Admin roles given and taken as listed."),
            Ok(r) => done.push_str(&format!(
                "\n{} {} didn't go through; see `/server plan`.",
                fmt::ATTENTION,
                fmt::count(r.failed().count(), "role change", "role changes")
            )),
            Err(e) => {
                tracing::warn!("applying admin roles after an approval failed: {e:#}");
                done.push_str(&format!(
                    "\n{} Couldn't give and take the admin roles yet: {} See `/server plan`.",
                    fmt::ATTENTION,
                    fmt::error(&e)
                ))
            }
        }
    }
    ctx.edit_response(i, &fmt::with_note(&original, &done), &[]).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_card_lists_what_waits_and_what_was_refused() {
        let approved = Sensitive { admin_role: Some("admin".into()), ..Default::default() };
        let mut new = Sensitive { admin_role: Some("member".into()), ..Default::default() };
        new.holders.insert("admin".into(), vec![chipsoc_model::Snowflake(5)]);
        let p = Pending {
            approved,
            new: Some(new),
            unlinks: vec!["channels.mods".into()],
            refused: vec!["roles.member".into()],
        };
        let mut cfg = Config::default();
        let role =
            |k: &str, n: &str| chipsoc_config::schema::Role { key: k.into(), name: n.into(), ..Default::default() };
        cfg.desired.roles.roles = vec![role("admin", "Committee"), role("member", "Member")];
        cfg.desired.lock.roles.insert("admin".into(), chipsoc_model::Snowflake(70));
        let ada = Authors { ids: vec![chipsoc_model::Snowflake(7)], pushed: false };
        let t = text(&cfg, &p, &ada, Some(chipsoc_model::Snowflake(1)));
        assert_eq!(
            t,
            "🔒 **These config changes wait for an Approve.**
They change who's an admin or what the bot trusts; until then the bot keeps the current settings.
• admin role (`admin_role`): <@&70> → **Member**
• <@&70> (members.toml): give to <@5>
• lock.toml drops `channels.mods`: the bot would stop managing it
Only the server owner can approve changes to who's an admin (`admin_role`, admin positions) or to partner sync's key and partners.
⚠️ **Refused:** lock.toml in the repo changes which Discord object `roles.member` is. The bot keeps its own ids (it never takes new ids for existing keys from the repo)."
        );
        // A new admin in members.toml is any other admin's to approve.
        let mut holders = p.approved.clone();
        holders.holders.insert("admin".into(), vec![chipsoc_model::Snowflake(5)]);
        let p2 = Pending { approved: p.approved.clone(), new: Some(holders), ..Default::default() };
        let pushed = Authors { ids: Vec::new(), pushed: true };
        assert!(
            text(&cfg, &p2, &pushed, None)
                .ends_with("Pushed to the config repo, not made through the bot, so any admin can approve.")
        );
        assert!(text(&cfg, &p2, &ada, None).ends_with("Made by <@7>, so another admin approves."));
        let both = Authors { pushed: true, ..ada.clone() };
        assert!(
            text(&cfg, &p2, &both, None)
                .ends_with("Made by <@7> (and partly pushed to the config repo), so another admin approves.")
        );
        // Taking the admin role from the owner is the owner's to approve, and the card says so.
        let p3 = Pending { approved: p2.new.clone().unwrap(), new: Some(p.approved.clone()), ..Default::default() };
        let owner = Some(chipsoc_model::Snowflake(5));
        let someone_else = Some(chipsoc_model::Snowflake(1));
        assert!(text(&cfg, &p3, &pushed, someone_else).ends_with("so any admin can approve."));
        assert!(
            text(&cfg, &p3, &pushed, owner).contains("Only the server owner"),
            "{}",
            text(&cfg, &p3, &pushed, owner)
        );

        let only_refused = Pending { refused: vec!["roles.x".into()], ..Default::default() };
        assert!(!only_refused.needs_approval() && !only_refused.is_empty());
        assert_ne!(p.hash(), only_refused.hash());
    }
}

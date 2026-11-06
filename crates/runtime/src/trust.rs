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

/// Names of whoever made the commits changing sensitive files since the
/// last approval (the Discord names the bot and web UI sign them with, else
/// git's author), lowercase.
async fn authors(ctx: &Ctx, dir: &Path) -> Vec<String> {
    let since = kept::approved_at(&ctx.store, &kept::scope(dir)).await.ok().flatten();
    let mut who: Vec<String> = chipsoc_config::git::authors(dir, since.as_deref(), FILES)
        .iter()
        .map(|w| w.trim().to_lowercase())
        .filter(|w| !w.is_empty())
        .collect();
    who.sort();
    who.dedup();
    who
}

fn text(cfg: &Config, p: &Pending, authors: &[String], owner: Option<chipsoc_model::Snowflake>) -> String {
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
        out.push(match owner_only {
            Some(why) => format!("{why}."),
            None if authors.is_empty() => "Any admin but whoever made them can approve.".to_owned(),
            None => format!("Made by {}, so another admin approves.", authors.join(", ")),
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

/// After the config loads: say in the bot log what's held back, once per change.
pub async fn review(ctx: &Ctx, dir: &Path) {
    let p = match pending(ctx, dir).await {
        Ok(p) => p,
        Err(e) => return tracing::warn!("checking the config's sensitive settings failed: {e:#}"),
    };
    let scope = kept::scope(dir);
    let hash = p.hash();
    if p.is_empty() || kept::prompted(&ctx.store, &scope).await.ok().flatten().as_deref() == Some(hash.as_str()) {
        return;
    }
    ctx.log_err(kept::set_prompted(&ctx.store, &scope, &hash).await, "remembering an approval card");
    let Some(ch) = ctx.bot_log() else { return };
    let owner = if p.needs_approval() { chipsoc_admin::owner_id(ctx).await.ok() } else { None };
    let body = text(&ctx.cfg(), &p, &authors(ctx, dir).await, owner);
    let buttons = if p.needs_approval() {
        let ok = CustomId::Trust { approve: true, hash: hash.clone() }.to_string();
        let no = CustomId::Trust { approve: false, hash }.to_string();
        vec![ui::row(vec![
            ui::button(&ok, "Approve", ButtonStyle::Success),
            ui::button(&no, "Reject", ButtonStyle::Danger),
        ])]
    } else {
        Vec::new()
    };
    ctx.say_with(ch, &body, &buttons).await;
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
            "\n\n{} **Rejected by {me}.** The bot keeps the approved settings; change the config back.",
            fmt::FAILED
        );
        let done = fmt::with_note(&original, &note);
        return ctx.respond(i, &ui::update(&done)).await;
    }
    let names: Vec<String> =
        [Some(&user.name), user.global_name.as_ref()].into_iter().flatten().map(|n| n.to_lowercase()).collect();
    let owner_id = chipsoc_admin::owner_id(ctx).await.ok();
    if let Some(new) = &p.new {
        let ask = chipsoc_engine::Ask::Approve { old: &p.approved, new };
        if let Err(why) = chipsoc_engine::owner_check(&control::by(i), &ask, owner_id) {
            let why = if owner_id.is_some() { format!("{why}.") } else { chipsoc_engine::OWNER_UNKNOWN.to_owned() };
            return ctx.respond(i, &ui::ephemeral(&why)).await;
        }
    }
    // The owner may approve their own change: they could make it in Discord anyway.
    let owner = owner_id == Some(chipsoc_model::Snowflake(user.id.get()));
    if !owner && authors(ctx, &dir).await.iter().any(|a| names.contains(a)) {
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
        let t = text(&cfg, &p, &["ada".into()], Some(chipsoc_model::Snowflake(1)));
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
        assert!(text(&cfg, &p2, &[], None).ends_with("Any admin but whoever made them can approve."));
        assert!(text(&cfg, &p2, &["ada".into()], None).ends_with("Made by ada, so another admin approves."));
        // Taking the admin role from the owner is the owner's to approve, and the card says so.
        let p3 = Pending { approved: p2.new.clone().unwrap(), new: Some(p.approved.clone()), ..Default::default() };
        let owner = Some(chipsoc_model::Snowflake(5));
        let someone_else = Some(chipsoc_model::Snowflake(1));
        assert!(text(&cfg, &p3, &[], someone_else).ends_with("Any admin but whoever made them can approve."));
        assert!(text(&cfg, &p3, &[], owner).contains("Only the server owner"), "{}", text(&cfg, &p3, &[], owner));

        let only_refused = Pending { refused: vec!["roles.x".into()], ..Default::default() };
        assert!(!only_refused.needs_approval() && !only_refused.is_empty());
        assert_ne!(p.hash(), only_refused.hash());
    }
}

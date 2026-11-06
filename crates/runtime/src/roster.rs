//! Who holds positions and leads projects, kept in the config: accepted
//! applications are written to members.toml and projects.toml, leavers are
//! taken out (and remembered), and admins are asked whether to restore them
//! when they come back.

use anyhow::Result;
use chipsoc_admin::{Actor, Host};
use chipsoc_config::{Snowflake, authoring};
use twilight_model::application::interaction::Interaction;
use twilight_model::channel::message::component::ButtonStyle;
use twilight_model::id::Id;
use twilight_model::id::marker::UserMarker;
use twilight_model::user::User;

use crate::apps::invoker;
use crate::commands::CustomId;
use chipsoc_admin::fmt;

use crate::{Ctx, control, now, ui};

/// Someone left: take them out of the config (so rejoining doesn't hand their
/// roles back) and remember what they held.
pub async fn on_leave(ctx: &Ctx, user: &User) {
    let cfg = ctx.cfg();
    let listed = match authoring::listings(&cfg.dir, user.id.get()) {
        Ok(l) if l.is_empty() => return,
        Ok(l) => l,
        Err(e) => return tracing::warn!("reading who holds what failed: {e:#}"),
    };
    let leads = listed.leads.iter().filter_map(|s| cfg.projects.iter().find(|p| &p.slug == s));
    let roles: Vec<String> = listed.roles.iter().cloned().chain(leads.map(|p| p.lead_role_key())).collect();
    ctx.log_err(ctx.db().remember_roles(&user.id.to_string(), &roles, now()).await, "remembering roles");
    let note = match ctx.unwritable() {
        Some(why) => format!(" They're still listed in the config ({why}); take them out there."),
        None => match unlist(ctx, user).await {
            Ok(()) => " Taken out of the config.".to_owned(),
            Err(e) => {
                tracing::warn!("taking a leaver out of the config failed: {e:#}");
                format!(" ⚠️ Couldn't take them out of the config: {}", fmt::error(&e))
            }
        },
    };
    let names = roles.iter().map(|k| ctx.role_name(k)).collect::<Vec<_>>().join(", ");
    ctx.log_people(&format!("👋 **<@{}> ({}) left.** They were {names}.{note}", user.id, user.name)).await;
}

async fn unlist(ctx: &Ctx, user: &User) -> Result<()> {
    let mut c = control::begin(ctx).await?;
    let (id, why) = (user.id.get(), format!("{} left", user.name));
    let unlist = move |dir: &std::path::Path| {
        authoring::unlist(dir, id)?;
        Ok(((), Some(why)))
    };
    let made = c.engine.edit_with(unlist, &chipsoc_engine::By::bot()).await;
    c.end().await;
    made?.1.map(drop)
}

/// Someone came back: if they held positions, ask admins whether to restore them.
pub async fn on_join(ctx: &Ctx, user: &User) {
    let roles = match ctx.db().former_roles(&user.id.to_string()).await {
        Ok(r) if r.is_empty() => return,
        Ok(r) => r,
        Err(e) => return tracing::warn!(?e, "reading former roles failed"),
    };
    let review = ctx.cfg().bot.review.as_ref().and_then(|r| ctx.channel(&r.channel));
    let Some(ch) = review.or_else(|| ctx.bot_log()) else { return };
    let names: Vec<String> = roles.iter().map(|k| format!("**{}**", ctx.role_name(k))).collect();
    let id = |restore| CustomId::Rejoined { restore, user: user.id.get() }.to_string();
    let buttons = [ui::row(vec![
        ui::button(&id(true), "Restore", ButtonStyle::Success),
        ui::button(&id(false), "Forget", ButtonStyle::Secondary),
    ])];
    let text = format!(
        "🔁 **<@{}> rejoined.** They were {}.\n**Restore** gives those roles back; **Forget** means they won't be offered again.",
        user.id,
        fmt::and_list(&names)
    );
    ctx.say_with(ch, &text, &buttons).await;
}

/// Restore or Forget on a rejoin card.
pub async fn rejoined(ctx: &Ctx, i: &Interaction, restore: bool, user: u64) {
    let Some(admin) = invoker(i).filter(|_| ctx.is_admin(i.member.as_ref())) else {
        return ctx.respond(i, &ui::ephemeral(&crate::manage::admins_only(&ctx.cfg()))).await;
    };
    let by = Actor { id: Some(Snowflake(admin.id.get())), name: admin.name.clone() };
    if restore && admin.id.get() == user {
        return ctx.respond(i, &ui::ephemeral("Another admin has to restore your own roles.")).await;
    }
    let Some(user) = Id::<UserMarker>::new_checked(user) else { return };
    let original = i.message.as_ref().map(|m| m.content.clone()).unwrap_or_default();
    if !restore {
        ctx.log_err(chipsoc_admin::forget_former(ctx, Snowflake(user.get()), &by).await, "forgetting roles");
        return ctx
            .respond(
                i,
                &ui::update(&chipsoc_admin::fmt::with_note(
                    &original,
                    &format!("\n**Forgotten by <@{}>:** their roles won't be offered back.", admin.id),
                )),
            )
            .await;
    }
    ctx.respond(i, &ui::defer_update()).await;
    let text = match chipsoc_admin::restore(ctx, Snowflake(user.get()), &by).await {
        Ok(said) => format!("✅ **Restored by <@{}>.**{}", admin.id, said.spaced()),
        Err(why) => format!("⚠️ {}", why.markdown()),
    };
    ctx.edit_response(i, &chipsoc_admin::fmt::with_note(&original, &format!("\n{text}")), &[]).await;
}

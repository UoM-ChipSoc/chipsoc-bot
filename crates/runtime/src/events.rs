//! `/event`: admins make an event from a form at once; members suggest one,
//! which goes to the review queue and is made when an admin accepts it (see
//! [`chipsoc_admin::events`]: the scheduled event, its announcement and
//! reminders a day and an hour before). Reminders are posted from here.

use std::sync::Arc;
use std::time::Duration;

use chipsoc_admin::events::{self as ev, NewEvent};
use chipsoc_store::AppKind;
use twilight_model::application::interaction::Interaction;
use twilight_model::channel::message::Component;
use twilight_model::channel::message::component::TextInputStyle;
use twilight_model::id::Id;
use twilight_model::id::marker::ChannelMarker;
use twilight_model::user::User;

use crate::review::{Filing, Refusal, Vetted};
use crate::ui::Values;
use crate::{Ctx, now, ui};

/// Admins can always make events; members can suggest them if `[events]`
/// allows it and there's a review queue.
pub fn ready(ctx: &Ctx, i: &Interaction) -> Result<(), String> {
    let cfg = ctx.cfg();
    if cfg.bot.events.is_none() {
        return Err("Events aren't set up here.".into());
    }
    if !ctx.is_admin(i.member.as_ref()) && !cfg.bot.event_suggestions() {
        return Err(crate::manage::admins_only(&cfg));
    }
    ev::tz(&cfg).map(drop)
}

pub fn form(ctx: &Ctx, i: &Interaction) -> (String, Vec<Component>) {
    let fields = vec![
        ui::input("title", "Title", TextInputStyle::Short, true, None),
        ui::input(
            "start",
            &format!("Starts ({})", ctx.cfg().bot.zone_label()),
            TextInputStyle::Short,
            true,
            Some("2026-11-12 18:00, or tomorrow 18:00"),
        ),
        ui::input("duration", "Length in minutes", TextInputStyle::Short, false, Some("60")),
        ui::input("location", "Where", TextInputStyle::Short, true, Some("A room, a link, or a voice channel's name")),
        ui::input("description", "Details", TextInputStyle::Paragraph, false, Some("Who it's for, what to bring")),
    ];
    let title = if ctx.is_admin(i.member.as_ref()) { "New event" } else { "Suggest an event" };
    (title.into(), fields)
}

/// The event form, sent: an admin's is made now; a member's goes for review.
pub async fn vet(ctx: &Ctx, i: &Interaction, user: &User, v: &Values) -> Result<Vetted, Refusal> {
    let cfg = ctx.cfg();
    let tz = ev::tz(&cfg).map_err(Refusal::last)?;
    let today = chrono::Utc::now().with_timezone(&tz).date_naive();
    let e = ev::check(
        tz,
        today,
        &v.text("title"),
        &v.text("start"),
        &v.text("duration"),
        &v.text("location"),
        &v.text("description"),
    )
    .map_err(Refusal::fix)?;
    if ctx.is_admin(i.member.as_ref()) {
        return made(ctx, user, &e).await;
    }
    if let Some(why) = ev::already_suggested(ctx, &e).await {
        return Err(Refusal::fix(why));
    }
    let title = ui::escape(&e.title);
    let thanks = format!(
        "Thanks! **{title}** is with the admins for a look. You'll get a DM once it's on the calendar (or if it isn't)."
    );
    let mut f =
        Filing::new(AppKind::Event, &e.title, format!("{} · {title}", AppKind::Event.label()), e.answers(tz), thanks);
    f.who = true;
    Ok(Vetted::Review(f))
}

/// An admin's event, made at once; partners to share it with are offered
/// under the reply, if any partner takes our events.
async fn made(ctx: &Ctx, user: &User, e: &NewEvent) -> Result<Vetted, Refusal> {
    let made = ev::create(ctx, e, None).await.map_err(|err| {
        tracing::warn!("creating a scheduled event failed: {err:#}");
        Refusal::fix(format!("⚠️ **Discord refused the event:** {}", chipsoc_admin::fmt::error(&err)))
    })?;
    let action = chipsoc_admin::AuditAction::CreateEvent.as_str();
    ctx.audit(Some(&user.id.to_string()), action, Some(&e.title), Some(&e.start.to_string())).await;
    let link = made.link.as_deref().map(|l| format!("\n{l}")).unwrap_or_default();
    let text = format!("{}{link}", made.said(&e.title, e.start).markdown());
    match made.id.map(|id| crate::fedshare::share_menu(ctx, chipsoc_admin::shares::Pick::Event(id))) {
        Some(Some(menu)) => Ok(Vetted::Done(format!("{text}\nShare it with partners? Pick them below."), vec![menu])),
        _ => Ok(Vetted::Done(text, Vec::new())),
    }
}

/// Whether a reminder's event is still on: not deleted, cancelled or over.
/// Unknown (Discord didn't answer) counts as on.
async fn still_on(ctx: &Ctx, event: Option<&str>) -> bool {
    /// Discord's scheduled event statuses that mean it's over.
    const COMPLETED: u64 = 3;
    const CANCELLED: u64 = 4;
    let Some(id) = event.and_then(|e| e.parse::<u64>().ok()) else { return true };
    match ctx.get(&format!("guilds/{}/scheduled-events/{id}", ctx.guild)).await {
        Ok(e) => !matches!(e["status"].as_u64(), Some(COMPLETED | CANCELLED)),
        Err(e) => chipsoc_discord::ApiError::status_of(&e) != Some(404),
    }
}

pub async fn reminder_loop(ctx: Arc<Ctx>) {
    let mut tick = tokio::time::interval(Duration::from_secs(30));
    loop {
        tick.tick().await;
        remind(&ctx).await;
    }
}

/// Post the reminders that are due (for events still on).
pub(crate) async fn remind(ctx: &Ctx) {
    let due = match ctx.db().due_reminders(now()).await {
        Ok(d) => d,
        Err(e) => return tracing::warn!(?e, "reading reminders failed"),
    };
    for r in due {
        let Ok(ch) = r.channel_id.parse::<Id<ChannelMarker>>() else { continue };
        if !still_on(ctx, r.event_id.as_deref()).await {
            ctx.log_err(ctx.db().done_reminder(r.id).await, "dropping a reminder for a cancelled event");
            continue;
        }
        // Kept until sent, so a failed send is retried next tick.
        if ctx.post(ch, &r.text, &[]).await.is_ok() || now() - r.due > 3600 {
            ctx.log_err(ctx.db().done_reminder(r.id).await, "clearing reminder");
        }
    }
}

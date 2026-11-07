//! Application windows. A position with `closes` takes applications quietly
//! until then: each gets one line in the review channel, no buttons. Once the
//! window closes (the date passes, or an admin closes the position), one card
//! lists everyone who applied, each with Accept and Reject, and a "Reject the
//! rest" for whoever is left. Positions without a window keep a card each.

use anyhow::Result;
use chipsoc_config::{Config, Snowflake};
use chipsoc_discord::{Method, Rest};
use chipsoc_store::{AppKind, AppStatus, Application, Decision};
use serde_json::{Value, json};

use crate::fmt::{MAX_CONTENT, escape, truncate};
use crate::{Actor, Answer, Host, now};

/// Applicants on one card: two to a row of Accept/Reject buttons, four rows,
/// and a last row for "Reject the rest" (Discord allows five).
pub const PER_CARD: usize = 8;

/// The custom id of a review button (the bot's `CustomId::Review`).
pub fn review_button(accept: bool, id: i64) -> String {
    format!("rev:{}:{id}", if accept { "accept" } else { "reject" })
}

/// The custom id of Accept on a card showing one version of an entry (see
/// [`crate::shares::version`]): refused once the entry has changed.
pub fn review_seen(id: i64, version: &str) -> String {
    format!("rev:accept:{id}:{version}")
}

/// The custom id of "Reject the rest" for a position (the bot's `CustomId::RejectRest`).
pub fn rest_button(role: &str) -> String {
    format!("rest:{role}")
}

/// A position's name, as people know it.
pub fn label(cfg: &Config, role: &str) -> String {
    cfg.bot
        .positions
        .iter()
        .find(|p| p.role == role)
        .and_then(|p| p.label.clone())
        .unwrap_or_else(|| crate::role_name(cfg, role))
}

/// Whether `role` is a position whose window is open at `at`: its
/// applications wait for one card listing them all.
pub fn in_window(cfg: &Config, role: &str, at: i64) -> bool {
    cfg.bot
        .positions
        .iter()
        .find(|p| p.role == role)
        .is_some_and(|p| p.closes.is_some() && p.taking(&cfg.bot.timezone, at))
}

fn review_channel(cfg: &Config) -> Option<Snowflake> {
    cfg.bot.review.as_ref().and_then(|r| cfg.desired.lock.channel(&r.channel))
}

/// The line in the review channel for an application taken while `role`'s window is open.
pub async fn noted<H: Host>(h: &H, role: &str, user: Snowflake) {
    let cfg = h.cfg();
    let Some(ch) = review_channel(&cfg) else { return };
    let so_far = h.db().waiting_round(AppKind::Position, role).await.map_or(1, |w| w.len());
    let text = format!("📨 <@{user}> applied for **{}** ({so_far} so far)", label(&cfg, role));
    if let Err(e) = crate::say(h, ch, &text).await {
        tracing::warn!("noting an application failed: {e:#}");
    }
}

/// Whether `ops` close a position, and so its window: its applicants are
/// then listed at once (see [`post_closed`]), not when the bot next looks.
pub fn closes_window(ops: &[chipsoc_config::ops::Op]) -> bool {
    use chipsoc_config::ops::{Op, PositionField};
    ops.iter().any(|o| matches!(o, Op::SetPosition { set: PositionField::Open(false), .. }))
}

/// Post a card for each position whose window has closed with applications
/// waiting. Says how many cards it posted. Safe to call from anywhere, any
/// number of times: each application is listed once. One that can't be
/// posted waits for next time without holding up the others.
pub async fn post_closed<H: Host>(h: &H) -> Result<usize> {
    let cfg = h.cfg();
    let mut cards = 0;
    for role in h.db().rounds_waiting().await? {
        if !in_window(&cfg, &role, now()) {
            match post_round(h, &role).await {
                Ok(n) => cards += n,
                Err(e) => tracing::warn!("posting the applicants for {role} failed; trying again next time: {e:#}"),
            }
        }
    }
    Ok(cards)
}

/// Post the cards listing everyone waiting in `role`'s round.
async fn post_round<H: Host>(h: &H, role: &str) -> Result<usize> {
    let cfg = h.cfg();
    // Without a review channel they wait (in the queue) until there is one.
    let Some(ch) = review_channel(&cfg) else { return Ok(0) };
    let apps = h.db().take_round(AppKind::Position, role).await?;
    let parts: Vec<&[Application]> = apps.chunks(PER_CARD).collect();
    for (n, chunk) in parts.iter().enumerate() {
        let mut head = format!(
            "**{} · {}: {}**",
            AppKind::Position.label(),
            escape(&label(&cfg, role)),
            crate::fmt::count(apps.len(), "applicant", "applicants")
        );
        head.push_str(" · applications have closed");
        if parts.len() > 1 {
            head.push_str(&format!(" (part {} of {})", n + 1, parts.len()));
        }
        let rows: Vec<Decision> = chunk
            .iter()
            .map(|a| Decision { app: a.clone(), reviewed_by: None, reviewed_at: None, reason: None })
            .collect();
        let (text, components) = render(h, &head, &rows).await;
        let body = json!({ "content": text, "components": components, "allowed_mentions": { "parse": [] } });
        let posted = h.rest().call(Method::Post, &format!("channels/{ch}/messages"), Some(body)).await;
        let msg = match posted {
            Ok(m) => chipsoc_discord::wire::s(&m["id"]),
            Err(e) => {
                // Put this part and the ones after it back, to post next time.
                let left: Vec<i64> = parts[n..].iter().flat_map(|c| c.iter().map(|a| a.id)).collect();
                if let Err(e) = h.db().untake_round(&left).await {
                    tracing::warn!("putting a round back failed: {e:#}");
                }
                return Err(e.context("posting a round's card"));
            }
        };
        for a in chunk.iter() {
            if let Err(e) = h.db().set_card(a.id, &ch.to_string(), &msg).await {
                tracing::warn!("remembering a round's card failed: {e:#}");
            }
        }
    }
    Ok(parts.len())
}

/// Who decided, for a line on the card.
fn by(key: &str) -> String {
    match key.parse::<u64>() {
        Ok(id) => format!("<@{id}>"),
        Err(_) => escape(key),
    }
}

/// A round card: `head`, then each applicant numbered with who they are,
/// their answers (cut so all fit) and how they were decided; buttons for
/// those still waiting.
async fn render<H: Host>(h: &H, head: &str, rows: &[Decision]) -> (String, Value) {
    let mut blocks = Vec::new();
    for (n, d) in rows.iter().enumerate() {
        let a = &d.app;
        let who = match a.user.parse::<Snowflake>() {
            Ok(u) => crate::join::summary(h, u).await,
            Err(_) => None,
        };
        let mut top = format!("**{}.** <@{}>", n + 1, a.user);
        if let Some(w) = who.filter(|w| !w.trim().is_empty()) {
            top.push_str(&format!(" · {}", escape(&truncate(w.trim(), 150))));
        }
        let status = match a.status {
            AppStatus::Pending => String::new(),
            AppStatus::Accepted => format!("\n✅ Accepted by {}.", by(d.reviewed_by.as_deref().unwrap_or("an admin"))),
            AppStatus::Rejected => {
                let why = d.reason.as_deref().map(|r| format!(" Reason: {}", truncate(r, 100))).unwrap_or_default();
                format!("\n❌ Rejected by {}.{why}", by(d.reviewed_by.as_deref().unwrap_or("an admin")))
            }
        };
        let answers: Vec<String> = Answer::parse(&a.answers)
            .into_iter()
            .filter(|x| !x.answer.trim().is_empty())
            .map(|x| format!("> **{}** {}", escape(&x.question), escape(&x.answer.replace('\n', " "))))
            .collect();
        blocks.push((top, answers.join("\n"), status));
    }
    // Each applicant gets an even share of the message; their answers are what's cut.
    let room = (MAX_CONTENT - 1).saturating_sub(head.chars().count() + 1);
    let share = room / rows.len().max(1);
    let mut text = head.to_owned();
    for (top, answers, status) in &blocks {
        let fixed = top.chars().count() + status.chars().count() + 3;
        let answers = truncate(answers, share.saturating_sub(fixed));
        text.push_str(&format!("\n\n{top}"));
        if !answers.is_empty() && answers != "…" {
            text.push_str(&format!("\n{answers}"));
        }
        text.push_str(status);
    }
    let text = truncate(&text, MAX_CONTENT - 1);

    let button =
        |label: String, id: String, style: u8| json!({ "type": 2, "style": style, "label": label, "custom_id": id });
    let waiting: Vec<(usize, &Application)> = rows
        .iter()
        .enumerate()
        .filter(|(_, d)| d.app.status == AppStatus::Pending)
        .map(|(n, d)| (n + 1, &d.app))
        .collect();
    let mut components: Vec<Value> = waiting
        .chunks(2)
        .take(4)
        .map(|pair| {
            let buttons: Vec<Value> = pair
                .iter()
                .flat_map(|(n, a)| {
                    [
                        button(format!("Accept {n}"), review_button(true, a.id), 3),
                        button(format!("Reject {n}"), review_button(false, a.id), 4),
                    ]
                })
                .collect();
            json!({ "type": 1, "components": buttons })
        })
        .collect();
    if waiting.len() > 1
        && let Some(first) = waiting.first()
    {
        let rest = button("Reject the rest…".into(), rest_button(&first.1.target), 2);
        components.push(json!({ "type": 1, "components": [rest] }));
    }
    (text, Value::Array(components))
}

/// Draw a round card again from what's stored (after a decision on it): its
/// first line kept, each applicant's state shown, buttons only for those waiting.
pub async fn redraw<H: Host>(h: &H, channel: &str, message: &str) -> Result<()> {
    let rows = h.db().on_card(channel, message).await?;
    let path = format!("channels/{channel}/messages/{message}");
    let old = h.rest().call(Method::Get, &path, None).await?;
    let content = chipsoc_discord::wire::s(&old["content"]);
    let head = content.lines().next().unwrap_or_default().to_owned();
    let (text, components) = render(h, &head, &rows).await;
    let body = json!({ "content": text, "components": components, "allowed_mentions": { "parse": [] } });
    h.rest().call(Method::Patch, &path, Some(body)).await.map(drop)
}

/// Reject everyone still waiting on `role`'s round cards, telling each
/// `why`; then draw the cards again. Says how many were rejected.
pub async fn reject_rest<H: Host>(h: &H, role: &str, why: Option<&str>, by: &Actor) -> Result<usize, String> {
    let waiting = h.db().posted_round(AppKind::Position, role).await.map_err(|e| {
        tracing::warn!("reading a round failed: {e:#}");
        "Couldn't read who's waiting; try again.".to_owned()
    })?;
    let mut cards = std::collections::BTreeSet::new();
    let mut done = 0;
    for app in waiting {
        // Someone may have decided one meanwhile: that one's skipped.
        let Ok(app) = crate::claim(h, app.id, false, by).await else { continue };
        crate::settle_with(h, &app, false, why, by).await;
        done += 1;
        if let Some(card) = app.card {
            cards.insert(card);
        }
    }
    for (ch, msg) in cards {
        if let Err(e) = redraw(h, &ch, &msg).await {
            tracing::warn!("drawing a round's card again failed: {e:#}");
        }
    }
    Ok(done)
}

/// Whether an application is one of a posted round (its card lists others too).
pub fn on_round_card(app: &Application) -> bool {
    app.round == chipsoc_store::Round::Posted
}

//! Sharing posts with partner servers.
//!
//! - Follow: posts in the `publish` announcement channels are published, so
//!   partners can Follow those channels in Discord.
//! - Relay: each peer gets new posts from its `share` channels through a
//!   webhook they gave us (the URL lives in the environment, never in git).

use std::collections::HashMap;

use chipsoc_config::Config;
use twilight_model::channel::Message;
use twilight_model::channel::message::AllowedMentions;
use twilight_model::id::Id;
use twilight_model::id::marker::{ChannelMarker, WebhookMarker};

use crate::{Ctx, ui};

/// A peer's resolved webhook.
#[derive(Clone)]
pub struct Relay {
    pub name: String,
    pub id: Id<WebhookMarker>,
    pub token: String,
}

/// Resolved federation config: channel → what to do with its posts.
#[derive(Default, Clone)]
pub struct Routes {
    publish: Vec<Id<ChannelMarker>>,
    relay: HashMap<Id<ChannelMarker>, Vec<usize>>,
    relays: Vec<Relay>,
    names: HashMap<Id<ChannelMarker>, String>,
    /// Shown as the relay's author, e.g. "ChipSoc · #events".
    sender: String,
}

/// `https://discord.com/api/webhooks/<id>/<token>` -> (id, token)
pub fn parse_webhook(url: &str) -> Option<(Id<WebhookMarker>, String)> {
    let rest = url.trim().split("/webhooks/").nth(1)?;
    let mut parts = rest.split('/');
    let id = parts.next()?.parse().ok()?;
    let token = parts.next()?.split('?').next()?.to_owned();
    (!token.is_empty()).then_some((id, token))
}

impl Routes {
    pub fn resolve(cfg: &Config) -> Self {
        let fed = &cfg.federation;
        let channel = |k: &str| cfg.desired.lock.channel(k).and_then(|s| Id::new_checked(s.0));
        let mut r = Routes { sender: cfg.bot.name.clone(), ..Routes::default() };
        for key in &fed.publish {
            match channel(key) {
                Some(c) => {
                    r.publish.push(c);
                    r.names.insert(c, key.clone());
                }
                None => tracing::warn!(channel = key, "federation publish channel not in lockfile"),
            }
        }
        for peer in &fed.peers {
            if peer.share.is_empty() {
                continue;
            }
            let var = &peer.share_webhook_env;
            let Some((id, token)) = std::env::var(var).ok().as_deref().and_then(parse_webhook) else {
                tracing::warn!(peer = peer.name, var, "partner's share webhook not set or invalid; skipping");
                continue;
            };
            let idx = r.relays.len();
            r.relays.push(Relay { name: peer.name.clone(), id, token });
            for key in &peer.share {
                if let Some(c) = channel(key) {
                    r.relay.entry(c).or_default().push(idx);
                    r.names.insert(c, key.clone());
                }
            }
        }
        r
    }

    pub fn is_empty(&self) -> bool {
        self.publish.is_empty() && self.relay.is_empty()
    }
}

/// Handle a new message: publish and/or relay it if its channel is shared.
pub async fn on_message(ctx: &Ctx, m: &Message) {
    let routes = ctx.routes();
    if routes.is_empty() {
        return;
    }
    // Human posts and our own (e.g. event announcements); never other bots/webhooks.
    if m.author.bot && m.author.id != ctx.me {
        return;
    }
    if routes.publish.contains(&m.channel_id)
        && let Err(e) = ctx
            .call(
                chipsoc_discord::Method::Post,
                &format!("channels/{}/messages/{}/crosspost", m.channel_id, m.id),
                None,
            )
            .await
    {
        tracing::warn!(?e, "publishing announcement failed");
    }
    let Some(targets) = routes.relay.get(&m.channel_id) else {
        return;
    };
    let channel = routes.names.get(&m.channel_id).cloned().unwrap_or_default();
    let mut body = m.content.clone();
    for a in &m.attachments {
        body.push_str(&format!("\n{}", a.url));
    }
    if body.trim().is_empty() {
        return;
    }
    let body = ui::truncate(&body, ui::MAX_CONTENT - 1);
    let name = ui::truncate(&format!("{} · #{channel}", routes.sender), 80);
    for &i in targets {
        let relay = &routes.relays[i];
        if let Err(e) = by_webhook(ctx, relay.id, &relay.token, &name, &body).await {
            tracing::warn!(?e, peer = relay.name, "relay to peer failed");
        }
    }
}

/// Post `content` through a partner's webhook as `username`, pinging nobody.
pub(crate) async fn by_webhook(
    ctx: &Ctx,
    id: Id<WebhookMarker>,
    token: &str,
    username: &str,
    content: &str,
) -> anyhow::Result<()> {
    let body = serde_json::json!({
        "username": username,
        "content": content,
        "allowed_mentions": AllowedMentions::default(),
    });
    ctx.call(chipsoc_discord::Method::Post, &format!("webhooks/{id}/{token}"), Some(body)).await.map(drop)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_webhook_urls() {
        let (id, token) = parse_webhook("https://discord.com/api/webhooks/123456/abc-DEF_9?wait=true").unwrap();
        assert_eq!((id.get(), token.as_str()), (123456, "abc-DEF_9"));
        assert!(parse_webhook("https://example.com/nope").is_none());
    }
}

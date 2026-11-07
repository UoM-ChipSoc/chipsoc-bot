//! People by name, not Discord id: names for the ids pages show, Discord's
//! mentions in messages turned into names, and who "@ada" is when someone
//! types it into a form.

use std::collections::HashMap;

use chipsoc_admin::{Host, Member, Unresolved};
use chipsoc_config::{Config, Snowflake};
use dioxus::prelude::*;

/// The names of the people a page shows.
#[derive(Default)]
pub(crate) struct Names(HashMap<Snowflake, String>);

impl Names {
    /// Names for every id among `raw` (anything else is shown as it is).
    pub(crate) async fn of<H: Host>(h: &H, raw: &[&str]) -> Self {
        let ids: Vec<Snowflake> = raw.iter().filter_map(|r| r.trim().parse().ok()).collect();
        Self(chipsoc_admin::members_of(h, ids).await.into_iter().map(|(id, m)| (id, m.name)).collect())
    }

    /// Their name; "unknown member (id …1234)" for an id not in the server;
    /// anything that isn't an id as it is.
    pub(crate) fn name(&self, raw: &str) -> String {
        match raw.trim().parse::<Snowflake>() {
            Ok(id) => self.0.get(&id).filter(|n| !n.is_empty()).cloned().unwrap_or_else(|| unknown(id)),
            Err(_) => raw.to_owned(),
        }
    }

    /// Their name, with the id on hover.
    pub(crate) fn who(&self, raw: &str) -> Element {
        let name = self.name(raw);
        if raw.trim().parse::<Snowflake>().is_ok() {
            rsx! { span { title: "Discord user id {raw}", "{name}" } }
        } else {
            rsx! { "{name}" }
        }
    }
}

/// Someone the member list doesn't have (left, or it couldn't be read).
fn unknown(id: Snowflake) -> String {
    let s = id.to_string();
    let tail = &s[s.len().saturating_sub(4)..];
    format!("unknown member (id …{tail})")
}

/// Discord's `<@id>`, `<@&id>` and `<#id>` in a message as names.
pub(crate) async fn unmention<H: Host>(h: &H, text: &str) -> String {
    let users: Vec<Snowflake> = mentions(text, "<@").into_iter().collect();
    if users.is_empty() && !text.contains("<#") && !text.contains("<@&") {
        return text.to_owned();
    }
    let names = chipsoc_admin::members_of(h, users).await;
    replace(text, &h.cfg(), |id| names.get(&id).map(|m| m.name.clone()))
}

/// What admin work says back, as plain text for the web UI: people by
/// name, not Discord's mentions or markdown.
pub(crate) trait Spoken: Send {
    fn spoken<H: Host>(self, h: &H) -> impl std::future::Future<Output = String> + Send;
}

impl Spoken for String {
    /// Older prose: mentions given back as names.
    async fn spoken<H: Host>(self, h: &H) -> String {
        unmention(h, &self).await
    }
}

impl Spoken for chipsoc_admin::Said {
    async fn spoken<H: Host>(self, h: &H) -> String {
        self.plain_named(h).await
    }
}

/// The user ids mentioned in `text` as `<@id>` or `<@!id>`.
fn mentions(text: &str, open: &str) -> Vec<Snowflake> {
    text.split(open).skip(1).filter_map(|rest| rest.trim_start_matches('!').split('>').next()?.parse().ok()).collect()
}

/// `text` with each mention replaced: people by `person`, roles and
/// channels by the config's names.
fn replace(text: &str, cfg: &Config, person: impl Fn(Snowflake) -> Option<String>) -> String {
    let lock = &cfg.desired.lock;
    let role = |id: Snowflake| {
        let key = lock.roles.iter().find(|(_, v)| **v == id).map(|(k, _)| k.clone())?;
        Some(chipsoc_admin::role_name(cfg, &key))
    };
    let channel = |id: Snowflake| {
        let key = lock.channels.iter().find(|(_, v)| **v == id).map(|(k, _)| k.clone())?;
        Some(format!("#{}", cfg.desired.channel(&key).map_or(key.as_str(), |c| c.name.as_str())))
    };
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('<') {
        out.push_str(&rest[..at]);
        let tail = &rest[at..];
        let Some(end) = tail.find('>') else { break };
        let inner = &tail[1..end];
        let named = if let Some(id) = inner.strip_prefix("@&") {
            id.parse().ok().map(|id| role(id).unwrap_or_else(|| "a role".into()))
        } else if let Some(id) = inner.strip_prefix('#') {
            id.parse().ok().map(|id| channel(id).unwrap_or_else(|| "a channel".into()))
        } else if let Some(id) = inner.strip_prefix('@') {
            id.trim_start_matches('!').parse().ok().map(|id| person(id).unwrap_or_else(|| unknown(id)))
        } else {
            None
        };
        match named {
            Some(n) => {
                out.push_str(&n);
                rest = &tail[end + 1..];
            }
            None => {
                out.push('<');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Who someone typed into a form means: an id, a mention, or an @username
/// or name. `Err`: why not, and the people it could be, to pick from.
pub(crate) async fn resolve<H: Host>(h: &H, typed: &str) -> Result<Snowflake, (String, Vec<Member>)> {
    match chipsoc_admin::resolve_user(h, typed).await {
        Ok(id) => Ok(id),
        Err(Unresolved::Several(found)) => Err(("More than one person matches: pick one.".into(), found)),
        Err(e) => Err((e.to_string(), Vec::new())),
    }
}

/// Where to find someone's id, for when a name won't do.
pub(crate) const ID_HINT: &str = "Their @username or server name, or their Discord user id (Settings → Advanced → \
                                  Developer Mode, then right-click them → Copy User ID).";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mentions_become_names() {
        let mut cfg = Config::default();
        cfg.desired.lock.roles.insert("treasurer".into(), Snowflake(11));
        let person = |id: Snowflake| (id.0 == 7).then(|| "Ada".to_owned());
        let text = replace("<@7> holds <@&11>; <@!8> asked in <#99>. 1 < 2 <b>", &cfg, person);
        assert_eq!(text, "Ada holds treasurer; unknown member (id …8) asked in a channel. 1 < 2 <b>");
        assert_eq!(mentions("<@7> and <@!8>, not <@&9>", "<@"), [Snowflake(7), Snowflake(8)]);
    }
}

//! Feeds members suggested and admins accepted: list, follow, stop following.
//! Fetching is careful: http(s) only, public addresses only, bounded size and
//! time. A website's address will do: its feed is found from the page.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use serde::{Deserialize, Serialize};

use crate::{Actor, AuditAction, Host, Refusal, Said, audit, fmt};

const MAX_BYTES: usize = 2 * 1024 * 1024;
const MAX_REDIRECTS: usize = 3;
const PRIVATE: &str = "that address points inside a private network";
/// Longest one fetch may take, and finding a feed from a website altogether.
const FETCH_TIME: Duration = Duration::from_secs(20);
const FIND_TIME: Duration = Duration::from_secs(45);
/// Where a site's feed usually is, tried when its page names none.
const COMMON_PATHS: [&str; 3] = ["/feed", "/rss", "/atom.xml"];
/// Feeds a page names that are tried, at most.
const MAX_LINKS: usize = 3;
const NOT_A_FEED: &str = "that page isn't an RSS or Atom feed";

/// A feed being followed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeedInfo {
    pub id: i64,
    pub url: String,
    /// Its title, as it last read (none before it was first read).
    #[serde(default)]
    pub title: Option<String>,
    /// Channel key its items are posted in.
    pub channel: String,
    pub added_by: Option<String>,
    /// Who added it, as people know them.
    #[serde(default)]
    pub added_by_name: Option<String>,
    pub added_at: i64,
    /// Not checked for new items while paused.
    pub paused: bool,
}

pub async fn feeds<H: Host>(h: &H) -> Result<Vec<FeedInfo>> {
    let all = h.db().feeds().await?;
    let ids = crate::people::ids(all.iter().filter_map(|f| f.added_by.as_deref()));
    let names = crate::people::names_of(h, &ids).await;
    let cfg = h.cfg();
    let info = |f: chipsoc_store::Feed| FeedInfo {
        added_by_name: f.added_by.as_deref().map(|b| crate::people::named(&cfg, &names, b)),
        id: f.id,
        url: f.url,
        title: f.title,
        channel: f.channel_key,
        added_by: f.added_by,
        added_at: f.ts,
        paused: f.paused,
    };
    Ok(all.into_iter().map(info).collect())
}

/// Stop following a feed. False if there was no such feed.
pub async fn remove_feed<H: Host>(h: &H, id: i64, by: &Actor) -> Result<bool> {
    let Some(feed) = h.db().feed(id).await? else { return Ok(false) };
    let removed = h.db().delete_feed(id).await?;
    if removed {
        audit(h, by, AuditAction::RemoveFeed, Some(&feed.url), None).await;
    }
    Ok(removed)
}

/// Follow a feed an admin gives (no suggestion needed). Returns its title.
pub async fn add_feed<H: Host>(h: &H, url: &str, by: &Actor) -> Result<String, Refusal> {
    let url = url.trim();
    let title = accept_feed(h, url, by).await?;
    audit(h, by, AuditAction::AddFeed, Some(url), Some(&title)).await;
    Ok(title)
}

/// Pause or resume a feed. False if there was no such feed.
pub async fn pause_feed<H: Host>(h: &H, id: i64, paused: bool, by: &Actor) -> Result<bool> {
    let Some(feed) = h.db().feed(id).await? else { return Ok(false) };
    let done = h.db().set_feed_paused(id, paused).await?;
    if done {
        audit(h, by, if paused { AuditAction::PauseFeed } else { AuditAction::ResumeFeed }, Some(&feed.url), None)
            .await;
    }
    Ok(done)
}

/// Start following an approved feed (or the one a website's address leads
/// to). Existing items are marked seen. Returns its title.
pub async fn accept_feed<H: Host>(h: &H, url: &str, by: &Actor) -> Result<String, Refusal> {
    let channel = h.cfg().bot.feeds.as_ref().map(|f| f.channel.clone()).ok_or("feeds aren't set up in bot.toml")?;
    let (url, feed) = find(url).await.map_err(|e| Refusal::from(Said::new().text(e.to_string())))?;
    let id =
        h.db().add_feed(&url, &channel, &by.key(), crate::now()).await.map_err(|e| Refusal::from(fmt::error(&e)))?;
    for entry in &feed.entries {
        if let Err(e) = h.db().mark_item(id, &entry.id).await {
            tracing::warn!("marking feed item failed: {e:#}");
        }
    }
    let title = title(&feed, &url);
    if let Err(e) = h.db().set_feed_title(id, &title).await {
        tracing::warn!("keeping a feed's title failed: {e:#}");
    }
    Ok(title)
}

/// What people know a feed as: its title, else its address.
pub fn shown(f: &FeedInfo) -> &str {
    f.title.as_deref().filter(|t| !t.is_empty()).unwrap_or(&f.url)
}

/// A feed's title, else its address.
pub fn title(feed: &feed_rs::model::Feed, url: &str) -> String {
    feed.title.as_ref().map_or_else(|| url.to_owned(), |t| t.content.trim().to_owned())
}

/// Fetch and parse a feed: http(s) only, public addresses only (checked on
/// every connection, redirects included), bounded size.
pub async fn fetch(url: &str) -> Result<feed_rs::model::Feed> {
    let page = get(&client()?, url).await?;
    feed_rs::parser::parse(&page.body[..]).map_err(|_| anyhow!(NOT_A_FEED))
}

/// The feed an address leads to, and where it was found: the address itself
/// if it's a feed; else, for a web page, a feed its head names
/// (`<link rel="alternate" type="application/rss+xml">`, or Atom); else one
/// at a common path on the same site. Every fetch is as careful as [`fetch`],
/// and finding one takes at most `FIND_TIME`.
pub async fn find(url: &str) -> Result<(String, feed_rs::model::Feed)> {
    match tokio::time::timeout(FIND_TIME, find_in_time(url.trim())).await {
        Ok(found) => found,
        Err(_) => Err(anyhow!("looking for its feed took too long")),
    }
}

async fn find_in_time(url: &str) -> Result<(String, feed_rs::model::Feed)> {
    let client = client()?;
    let page = get(&client, url).await?;
    if let Ok(feed) = feed_rs::parser::parse(&page.body[..]) {
        return Ok((url.to_owned(), feed));
    }
    let html = String::from_utf8_lossy(&page.body);
    if !looks_like_html(&html) {
        return Err(anyhow!(NOT_A_FEED));
    }
    let named = feed_links(&html, &page.url);
    let common = COMMON_PATHS.iter().filter_map(|p| page.url.join(p).ok());
    let mut tried = Vec::new();
    for candidate in named.into_iter().take(MAX_LINKS).chain(common) {
        if tried.contains(&candidate) {
            continue;
        }
        tried.push(candidate.clone());
        let Ok(found) = get(&client, candidate.as_str()).await else { continue };
        if let Ok(feed) = feed_rs::parser::parse(&found.body[..]) {
            return Ok((candidate.to_string(), feed));
        }
    }
    Err(anyhow!("{NOT_A_FEED}, and no feed could be found from it"))
}

/// A page as fetched: where it ended up (after redirects) and its body.
struct Page {
    url: reqwest::Url,
    body: Vec<u8>,
}

/// The careful client: public addresses only, few redirects, each checked.
/// Made once, so its connections and TLS setup are shared.
fn client() -> Result<reqwest::Client> {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    if let Some(c) = CLIENT.get() {
        return Ok(c.clone());
    }
    let c = reqwest::Client::builder()
        .user_agent(concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION")))
        .timeout(FETCH_TIME)
        .no_proxy()
        .dns_resolver(Arc::new(PublicOnly))
        .redirect(reqwest::redirect::Policy::custom(|a| {
            if a.previous().len() > MAX_REDIRECTS {
                return a.error("too many redirects");
            }
            match check_url(a.url()) {
                Ok(()) => a.follow(),
                Err(e) => a.error(e),
            }
        }))
        .build()?;
    Ok(CLIENT.get_or_init(|| c).clone())
}

async fn get(client: &reqwest::Client, url: &str) -> Result<Page> {
    let parsed = reqwest::Url::parse(url.trim()).map_err(|_| anyhow!("that isn't a web address"))?;
    check_url(&parsed)?;
    let mut resp =
        client.get(parsed).send().await.and_then(|r| r.error_for_status()).map_err(|e| anyhow!(cause(&e)))?;
    let url = resp.url().clone();
    let mut body = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(|e| anyhow!(cause(&e)))? {
        body.extend_from_slice(&chunk);
        if body.len() > MAX_BYTES {
            return Err(anyhow!("the feed is too large"));
        }
    }
    Ok(Page { url, body })
}

fn looks_like_html(text: &str) -> bool {
    let start: String = text.chars().take(2048).collect::<String>().to_lowercase();
    start.contains("<html") || start.contains("<head") || start.contains("<!doctype html")
}

/// The feeds a web page's head names, in its order: `<link>` tags with
/// `rel="alternate"` and an RSS or Atom type, their addresses made whole
/// against the page's.
pub(crate) fn feed_links(html: &str, base: &reqwest::Url) -> Vec<reqwest::Url> {
    // Only the head. ASCII lower-casing keeps byte offsets the same.
    let lower = html.to_ascii_lowercase();
    let end = lower.find("</head").unwrap_or(lower.len());
    let (html, lower) = (&html[..end], &lower[..end]);
    let mut out = Vec::new();
    let mut at = 0;
    while let Some(found) = lower[at..].find("<link") {
        let start = at + found + "<link".len();
        let Some(close) = lower[start..].find('>') else { break };
        let attrs = attributes(&html[start..start + close]);
        at = start + close;
        let get = |name: &str| attrs.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str());
        let alternate = get("rel").is_some_and(|r| r.split_whitespace().any(|t| t.eq_ignore_ascii_case("alternate")));
        let feed = get("type").is_some_and(|t| {
            let t = t.trim().to_lowercase();
            t == "application/rss+xml" || t == "application/atom+xml"
        });
        if alternate
            && feed
            && let Some(href) = get("href")
            && let Ok(url) = base.join(&href.trim().replace("&amp;", "&"))
            && !out.contains(&url)
        {
            out.push(url);
        }
    }
    out
}

/// A tag's attributes as (lower-case name, value).
fn attributes(tag: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut rest = tag.trim_start();
    while !rest.is_empty() {
        let name_end = rest.find(|c: char| c == '=' || c.is_whitespace() || c == '/').unwrap_or(rest.len());
        let name = rest[..name_end].to_lowercase();
        rest = rest[name_end..].trim_start();
        let value = if let Some(after) = rest.strip_prefix('=') {
            let after = after.trim_start();
            match after.chars().next() {
                Some(q @ ('"' | '\'')) => {
                    let inner = &after[1..];
                    let end = inner.find(q).unwrap_or(inner.len());
                    rest = inner.get(end + 1..).unwrap_or_default();
                    inner[..end].to_owned()
                }
                _ => {
                    let end = after.find(char::is_whitespace).unwrap_or(after.len());
                    rest = &after[end..];
                    after[..end].to_owned()
                }
            }
        } else {
            if name.is_empty() {
                // A stray character (such as the `/` of `/>`): step over it.
                rest = rest.get(1..).unwrap_or_default();
            }
            String::new()
        };
        if !name.is_empty() {
            out.push((name, value));
        }
        rest = rest.trim_start();
    }
    out
}

/// The innermost error, which says what actually went wrong.
fn cause(e: &reqwest::Error) -> String {
    let mut msg = e.to_string();
    let mut source = std::error::Error::source(e);
    while let Some(s) = source {
        msg = s.to_string();
        source = s.source();
    }
    msg
}

/// Whether `url` may be fetched at all, before fetching it: a web address
/// whose host isn't written as a private one ([`find`] checks every address
/// a name resolves to as well).
pub fn check_address(url: &str) -> Result<()> {
    let parsed = reqwest::Url::parse(url.trim()).map_err(|_| anyhow!("that isn't a web address"))?;
    check_url(&parsed)
}

/// http(s), with a host that isn't a private address. Names are checked when
/// they resolve (see [`PublicOnly`]); addresses written out are checked here.
pub(crate) fn check_url(url: &reqwest::Url) -> Result<()> {
    if !matches!(url.scheme(), "http" | "https") {
        return Err(anyhow!("it should start with https://"));
    }
    let host = url.host_str().context("no host in the address")?;
    match host.trim_start_matches('[').trim_end_matches(']').parse::<IpAddr>() {
        Ok(ip) if !is_public(ip) => Err(anyhow!(PRIVATE)),
        _ => Ok(()),
    }
}

/// Resolves names as the system does, refusing any that point at a private address.
struct PublicOnly;

impl Resolve for PublicOnly {
    fn resolve(&self, name: Name) -> Resolving {
        let host = name.as_str().to_owned();
        Box::pin(async move {
            let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), 0)).await?.collect();
            if addrs.is_empty() || addrs.iter().any(|a| !is_public(a.ip())) {
                return Err(PRIVATE.into());
            }
            Ok(Box::new(addrs.into_iter()) as Addrs)
        })
    }
}

fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v) => is_public_v4(v),
        IpAddr::V6(v) => match v.to_ipv4_mapped() {
            Some(v4) => is_public_v4(v4),
            None => {
                let s = v.segments();
                let o = v.octets();
                let embedded = |at: usize| Ipv4Addr::new(o[at], o[at + 1], o[at + 2], o[at + 3]);
                // Addresses that carry an IPv4 one: NAT64 (64:ff9b::/96 and
                // the local 64:ff9b:1::/48), 6to4 (2002::/16), IPv4-compatible (::a.b.c.d).
                if s[0] == 0x64 && s[1] == 0xff9b && (s[2..6] == [0, 0, 0, 0] || s[2] == 1) {
                    return s[2] == 0 && is_public_v4(embedded(12));
                }
                if s[0] == 0x2002 {
                    return is_public_v4(embedded(2));
                }
                if s[..6] == [0; 6] && !v.is_loopback() && !v.is_unspecified() {
                    return is_public_v4(embedded(12));
                }
                !(v.is_loopback()
                    || v.is_unspecified()
                    || v.is_multicast()
                    || (s[0] & 0xfe00) == 0xfc00 // unique local
                    || (s[0] & 0xffc0) == 0xfe80 // link local
                    || (s[0] == 0x2001 && s[1] == 0x0db8)) // documentation
            }
        },
    }
}

fn is_public_v4(v: Ipv4Addr) -> bool {
    let [a, b, c, _] = v.octets();
    !(v.is_private()
        || v.is_loopback()
        || v.is_link_local()
        || v.is_unspecified()
        || v.is_broadcast()
        || v.is_multicast()
        || v.is_documentation()
        || a == 0 // "this network"
        || a >= 240 // reserved
        || (a == 100 && (b & 0xc0) == 64) // carrier-grade NAT, 100.64/10
        || (a == 192 && b == 0 && c == 0) // IETF protocol assignments
        || (a == 198 && (b & 0xfe) == 18)) // benchmarking, 198.18/15
}

#[cfg(test)]
mod tests {
    use super::{attributes, check_url, feed_links, is_public, looks_like_html};

    #[test]
    fn a_website_names_its_feeds_in_its_head() {
        let base = reqwest::Url::parse("https://blog.example/posts/").unwrap();
        let page = r#"<!DOCTYPE html><HTML><head>
            <link rel="stylesheet" href="/style.css">
            <LINK REL="alternate" TYPE="application/rss+xml" title="RSS" href="/feed.xml">
            <link type='application/atom+xml' rel='alternate home' href='atom?a=1&amp;b=2' />
            <link rel=alternate type=application/json href=/feed.json>
            <link rel="alternate" type="application/rss+xml" href="/feed.xml">
            </head><body><link rel="alternate" type="application/rss+xml" href="/not-in-head"></body></html>"#;
        let found: Vec<String> = feed_links(page, &base).iter().map(ToString::to_string).collect();
        assert_eq!(found, ["https://blog.example/feed.xml", "https://blog.example/posts/atom?a=1&b=2"]);
        assert!(looks_like_html(page) && !looks_like_html("<?xml version=\"1.0\"?><rss>"));
        assert!(feed_links("<html><head><link", &base).is_empty(), "cut off: nothing, and no panic");
        let a = attributes(r#" rel = "a b" href=x data-y  "#);
        assert_eq!(a, [("rel".into(), "a b".into()), ("href".into(), "x".into()), ("data-y".into(), String::new())]);
    }

    #[test]
    fn private_addresses_are_refused() {
        for ip in [
            "127.0.0.1",
            "10.1.2.3",
            "192.168.0.1",
            "169.254.1.1",
            "100.64.0.1",
            "192.0.0.8",
            "224.0.0.1",
            "203.0.113.5",
            "0.1.2.3",
            "::1",
            "64:ff9b::7f00:1",
            "64:ff9b:1::a00:1",
            "2002:a00:1::1",
            "2002:7f00:1::",
            "::10.0.0.1",
            "fd00::1",
            "fe80::1",
            "ff02::1",
            "2001:db8::1",
            "::ffff:127.0.0.1",
            "::ffff:10.0.0.1",
        ] {
            assert!(!is_public(ip.parse().unwrap()), "{ip}");
        }
        for ip in ["1.1.1.1", "100.128.0.1", "2606:4700::1111", "::ffff:1.1.1.1"] {
            assert!(is_public(ip.parse().unwrap()), "{ip}");
        }
    }

    #[test]
    fn written_out_addresses_and_odd_schemes_are_refused() {
        let check = |u: &str| check_url(&reqwest::Url::parse(u).unwrap());
        assert!(check("https://example.org/feed").is_ok());
        assert!(check("http://1.1.1.1/feed").is_ok());
        assert!(check("http://127.0.0.1:8080/").is_err());
        assert!(check("http://[::ffff:192.168.1.1]/").is_err());
        assert!(check("file:///etc/passwd").is_err());
    }
}

//! Read a live server into a [`Snapshot`]. Objects the lockfile knows keep their
//! config keys; new ones get keys made from their names.

use std::collections::{BTreeMap, HashMap, HashSet};

use anyhow::Result;
use chipsoc_config::projects;
use chipsoc_config::schema::*;
use chipsoc_config::slug;
use chipsoc_config::template::{self, Rendered};
use chipsoc_model::{ObjectKind, perms};
use serde_json::Value;

use crate::wire::{self, id, s};
use crate::{ApiError, Method, Rest};

/// Hands out keys: the lockfile's for known ids, else unique slugs.
struct Keys<'a> {
    /// The lockfile's keys by kind and id.
    known: HashMap<(ObjectKind, Snowflake), &'a str>,
    used: HashSet<String>,
}

impl<'a> Keys<'a> {
    fn new(lock: &'a Lock) -> Self {
        let kinds = [ObjectKind::Role, ObjectKind::Category, ObjectKind::Channel];
        let used = kinds.into_iter().flat_map(|kind| Self::ids(lock, kind).keys().map(move |k| format!("{kind}:{k}")));
        let known = kinds
            .into_iter()
            .flat_map(|kind| Self::ids(lock, kind).iter().map(move |(k, id)| ((kind, *id), k.as_str())))
            .collect();
        Self { known, used: used.collect() }
    }

    fn ids(lock: &Lock, kind: ObjectKind) -> &std::collections::BTreeMap<String, Snowflake> {
        match kind {
            ObjectKind::Role => &lock.roles,
            ObjectKind::Category => &lock.categories,
            _ => &lock.channels,
        }
    }

    /// A key for a role, category or channel (any other kind counts as a channel).
    fn key(&mut self, kind: ObjectKind, id: Snowflake, name: &str) -> String {
        let kind = if matches!(kind, ObjectKind::Role | ObjectKind::Category) { kind } else { ObjectKind::Channel };
        if let Some(k) = self.known.get(&(kind, id)) {
            return (*k).to_owned();
        }
        let base = slug(name);
        let mut k = base.clone();
        let mut n = 2;
        while !self.used.insert(format!("{kind}:{k}")) {
            k = format!("{base}-{n}");
            n += 1;
        }
        k
    }
}

fn position(v: &Value) -> i64 {
    v["position"].as_i64().unwrap_or(0)
}

enum Read {
    Found(Value),
    /// 404.
    Absent,
    /// 403: there, perhaps, but this token may not read it.
    Forbidden,
}

async fn get_read(rest: &impl Rest, path: &str) -> Result<Read> {
    match rest.call(Method::Get, path, None).await {
        Ok(v) => Ok(Read::Found(v)),
        Err(e) => match ApiError::status_of(&e) {
            Some(404) => Ok(Read::Absent),
            Some(403) => Ok(Read::Forbidden),
            _ => Err(e),
        },
    }
}

/// Everyone in the server, with their role ids.
pub async fn members(rest: &impl Rest, guild: Snowflake) -> Result<Vec<(Snowflake, Vec<Snowflake>)>> {
    Ok(rest
        .members(guild)
        .await?
        .iter()
        .filter_map(|m| Some((id(&m["user"]["id"])?, wire::ids(&m["roles"]))))
        .collect())
}

/// Everyone in the server with their roles, a page at a time: the one
/// paginator (call it through [`Rest::members`]). Errors abort: a partial
/// list would read as "these people left".
pub async fn fetch_members<R: Rest + ?Sized>(rest: &R, guild: Snowflake) -> Result<Vec<Value>> {
    let mut out = Vec::new();
    let mut after = 0u64;
    loop {
        let path = format!("guilds/{guild}/members?limit=1000&after={after}");
        let page = rest.call(Method::Get, &path, None).await.map_err(|e| {
            if ApiError::status_of(&e) == Some(403) {
                e.context("listing members needs the bot's Server Members intent (Developer Portal → Bot)")
            } else {
                e
            }
        })?;
        let page = into_array(page);
        let done = page.len() < 1000;
        after = page.last().and_then(|m| id(&m["user"]["id"])).map_or(after, |i| i.0);
        out.extend(page);
        if done {
            return Ok(out);
        }
    }
}

/// Like [`export`], also listing who holds `roles` (and everyone in the
/// server). Also returns the guild as Discord sent it.
pub async fn export_with_holders(
    rest: &impl Rest,
    guild: Snowflake,
    known: &Lock,
    roles: &[Snowflake],
) -> Result<(Snapshot, Value)> {
    let (mut snap, g) = export_raw(rest, guild, known).await?;
    if !roles.is_empty() {
        holders_into(&mut snap, &rest.members(guild).await?, roles);
    }
    Ok((snap, g))
}

/// Who holds `roles`, and everyone in the server, into `snap` (which needs its lock).
pub fn holders_into(snap: &mut Snapshot, members: &[Value], roles: &[Snowflake]) {
    for m in members {
        let Some(user) = id(&m["user"]["id"]) else { continue };
        snap.members.insert(user);
        for r in wire::ids(&m["roles"]).into_iter().filter(|r| roles.contains(r)) {
            if let Some(key) = snap.lock.role_key(r) {
                snap.holders.entry(key.to_owned()).or_default().insert(user);
            }
        }
    }
    for r in roles {
        if let Some(key) = snap.lock.role_key(*r) {
            snap.holders.entry(key.to_owned()).or_default();
        }
    }
}

pub async fn export(rest: &impl Rest, guild: Snowflake, known: &Lock) -> Result<Snapshot> {
    Ok(export_raw(rest, guild, known).await?.0)
}

/// A JSON array's items, taken (not copied); none if it isn't one.
fn into_array(v: Value) -> Vec<Value> {
    match v {
        Value::Array(items) => items,
        _ => Vec::new(),
    }
}

async fn export_raw(rest: &impl Rest, guild: Snowflake, known: &Lock) -> Result<(Snapshot, Value)> {
    let g = rest.call(Method::Get, &format!("guilds/{guild}"), None).await?;
    let channels = rest.call(Method::Get, &format!("guilds/{guild}/channels"), None).await?;
    let channels = into_array(channels);
    let mut keys = Keys::new(known);
    let mut lock = Lock { guild: Some(guild), ..Default::default() };

    // Roles, top of the hierarchy first; @everyone last.
    let mut raw_roles: Vec<&Value> = g["roles"].as_array().into_iter().flatten().collect();
    raw_roles.sort_by_key(|r| (id(&r["id"]) == Some(guild), -position(r), id(&r["id"])));
    let mut roles = Vec::new();
    for r in raw_roles {
        let Some(rid) = id(&r["id"]) else { continue };
        let key = if rid == guild { "everyone".to_owned() } else { keys.key(ObjectKind::Role, rid, &s(&r["name"])) };
        lock.roles.insert(key.clone(), rid);
        let color = r["color"].as_u64().unwrap_or(0);
        let bits = r["permissions"].as_str().and_then(|p| p.parse().ok()).unwrap_or(0);
        roles.push(Role {
            key,
            name: s(&r["name"]),
            color: (color != 0).then(|| format!("#{color:06x}")),
            hoist: r["hoist"].as_bool().unwrap_or(false),
            mentionable: r["mentionable"].as_bool().unwrap_or(false),
            permissions: perms::names(bits),
            managed: r["managed"].as_bool().unwrap_or(false),
        });
    }
    let role_keys: HashMap<Snowflake, String> = lock.roles.iter().map(|(k, id)| (*id, k.clone())).collect();
    let role_key = |rid: Snowflake| role_keys.get(&rid).cloned().unwrap_or_else(|| rid.to_string());

    // Categories, then channels into them (text-like first, then voice), in Discord's order.
    let sort_key = |c: &&Value| (position(c), id(&c["id"]));
    let mut cats: Vec<&Value> = channels.iter().filter(|c| c["type"].as_u64() == Some(4)).collect();
    cats.sort_by_key(sort_key);
    let mut categories: Vec<(Snowflake, CategoryFile)> = Vec::new();
    for c in cats {
        let Some(cid) = id(&c["id"]) else { continue };
        let key = keys.key(ObjectKind::Category, cid, &s(&c["name"]));
        lock.categories.insert(key.clone(), cid);
        let overwrites = wire::overwrites_from(&c["permission_overwrites"], &role_key);
        categories.push((
            cid,
            CategoryFile { category: Category { key, name: s(&c["name"]), overwrites }, channels: Vec::new() },
        ));
    }
    let mut chans: Vec<(&Value, ChannelKind)> =
        channels.iter().filter_map(|c| Some((c, ChannelKind::from_discord(c["type"].as_u64()? as u8)?))).collect();
    chans.sort_by_key(|(c, k)| (k.is_voice(), position(c), id(&c["id"])));
    let mut top_level = Vec::new();
    for (c, kind) in chans {
        let Some(cid) = id(&c["id"]) else { continue };
        let key = keys.key(ObjectKind::Channel, cid, &s(&c["name"]));
        lock.channels.insert(key.clone(), cid);
        let ch = Channel {
            key,
            kind,
            name: s(&c["name"]),
            topic: c["topic"].as_str().filter(|t| !t.is_empty()).map(str::to_owned),
            slowmode: c["rate_limit_per_user"].as_u64().unwrap_or(0) as u16,
            nsfw: c["nsfw"].as_bool().unwrap_or(false),
            user_limit: c["user_limit"].as_u64().filter(|n| *n > 0).map(|n| n as u16),
            tags: c["available_tags"].as_array().into_iter().flatten().map(|t| s(&t["name"])).collect(),
            overwrites: wire::overwrites_from(&c["permission_overwrites"], &role_key),
            inherit: false,
            post: None,
            sort: c["default_sort_order"].as_u64().and_then(ForumSort::from_discord).filter(|_| kind.has_tags()),
            threads: Vec::new(),
        };
        match id(&c["parent_id"]).and_then(|p| categories.iter_mut().find(|(i, _)| *i == p)) {
            Some((_, cf)) => cf.channels.push(ch),
            None => top_level.push(ch),
        }
    }

    let chan_key = |field: &str| id(&g[field]).and_then(|i| lock.channel_key(i)).map(str::to_owned);
    let level = |field: &str| g[field].as_u64().map(|n| n as u8);
    let server = Server {
        guild: Guild {
            name: s(&g["name"]),
            description: g["description"].as_str().filter(|d| !d.is_empty()).map(str::to_owned),
            icon: None,
            banner: None,
            verification_level: level("verification_level"),
            explicit_content_filter: level("explicit_content_filter"),
            default_message_notifications: level("default_message_notifications"),
            system_channel: chan_key("system_channel_id"),
            rules_channel: chan_key("rules_channel_id"),
            updates_channel: chan_key("public_updates_channel_id"),
            afk_channel: chan_key("afk_channel_id"),
        },
    };
    let mut assets = BTreeMap::new();
    for name in ["icon", "banner"] {
        if let Some(h) = g[name].as_str() {
            assets.insert(name.to_owned(), h.to_owned());
        }
    }

    let mut unreadable = Unreadable::default();
    let settings = settings(rest, guild, &g, known, &mut lock, &mut unreadable).await?;
    let (posts, project_posts) = posts(rest, known, &mut lock, &channels, &mut unreadable).await?;
    let threads = threads(rest, known, &mut lock, &channels, &mut unreadable).await?;

    let snap = Snapshot {
        server,
        roles: Roles { roles },
        categories: categories.into_iter().map(|(_, cf)| cf).collect(),
        top_level: ChannelsFile { channels: top_level },
        settings,
        posts,
        project_posts,
        threads,
        assets,
        lock,
        unreadable,
        ..Default::default()
    };
    Ok((snap, g))
}

async fn settings(
    rest: &impl Rest,
    guild: Snowflake,
    g: &Value,
    known: &Lock,
    lock: &mut Lock,
    unreadable: &mut Unreadable,
) -> Result<Settings> {
    // Discord documents no permission for reading onboarding, and the
    // screening endpoint isn't documented: if either is refused, say so.
    let onboarding = match get_read(rest, &format!("guilds/{guild}/onboarding")).await? {
        Read::Found(v) => wire::onboarding_from(&v, lock),
        Read::Absent => None,
        Read::Forbidden => {
            unreadable.onboarding = true;
            None
        }
    };
    let gate = g["features"].as_array().into_iter().flatten().any(|f| f == "MEMBER_VERIFICATION_GATE_ENABLED");
    let screening = match get_read(rest, &format!("guilds/{guild}/member-verification")).await? {
        Read::Found(v) => wire::screening_from(&v, gate),
        Read::Absent => None,
        Read::Forbidden => {
            unreadable.screening = true;
            None
        }
    };
    let mut automod = Vec::new();
    // Reading AutoMod rules takes Manage Server.
    let rules = match get_read(rest, &format!("guilds/{guild}/auto-moderation/rules")).await? {
        Read::Found(v) => v,
        Read::Absent => Value::Null,
        Read::Forbidden => {
            unreadable.automod = true;
            Value::Null
        }
    };
    for r in rules.as_array().into_iter().flatten() {
        let Some(rid) = id(&r["id"]) else { continue };
        let key =
            known.automod.iter().find(|(_, v)| **v == rid).map_or_else(|| slug(&s(&r["name"])), |(k, _)| k.clone());
        if let Some(rule) = wire::automod_from(r, key.clone(), lock) {
            lock.automod.insert(key, rid);
            automod.push(rule);
        }
    }
    Ok(Settings { onboarding, screening, automod })
}

type Posts = (BTreeMap<String, String>, BTreeMap<String, String>);

/// The text of every managed post the lockfile knows, as signatures to compare.
async fn posts(
    rest: &impl Rest,
    known: &Lock,
    lock: &mut Lock,
    channels: &[Value],
    unreadable: &mut Unreadable,
) -> Result<Posts> {
    let mut posts = BTreeMap::new();
    let me = if known.posts.is_empty() && known.project_posts.is_empty() { None } else { Some(me(rest).await?) };
    for (key, p) in &known.posts {
        let Some(ch) = known.channel(key) else { continue };
        // A forum's post is a thread whose first message has its id.
        let forum = channels.iter().find(|c| id(&c["id"]) == Some(ch)).is_some_and(|c| {
            c["type"].as_u64().and_then(|t| ChannelKind::from_discord(t as u8)).is_some_and(ChannelKind::has_tags)
        });
        let at = if forum { p.message } else { ch };
        // A forum's post must be a thread in that forum.
        if forum {
            match get_read(rest, &format!("channels/{at}")).await? {
                Read::Found(t) if id(&t["parent_id"]) == Some(ch) => {}
                Read::Found(_) | Read::Absent => continue,
                Read::Forbidden => {
                    unreadable.posts.insert(key.clone());
                    lock.posts.insert(key.clone(), p.clone());
                    continue;
                }
            }
        }
        match get_read(rest, &format!("channels/{at}/messages/{}", p.message)).await? {
            // Only the bot's own message is its post (one it can edit).
            Read::Found(m) if id(&m["author"]["id"]) != me => {}
            Read::Found(m) => match later_parts(rest, at, &p.parts).await? {
                Some(later) => {
                    let all: Vec<Rendered> = std::iter::once(wire::rendered_from(&m)).chain(later).collect();
                    posts.insert(key.clone(), template::signature(&all));
                    lock.posts.insert(key.clone(), p.clone());
                }
                None => {
                    unreadable.posts.insert(key.clone());
                    lock.posts.insert(key.clone(), p.clone());
                }
            },
            Read::Forbidden => {
                unreadable.posts.insert(key.clone());
                lock.posts.insert(key.clone(), p.clone());
            }
            Read::Absent => {}
        }
    }
    let mut project_posts = BTreeMap::new();
    for (slug, t) in &known.project_posts {
        let Some((thread, sig)) = forum_post(rest, t, channels, None, me).await? else {
            continue;
        };
        let sig = match sig {
            Some(sig) => projects::pinned(sig, thread["flags"].as_u64().unwrap_or(0) & PINNED_THREAD != 0),
            None => {
                unreadable.project_posts.insert(slug.clone());
                lock.project_posts.insert(slug.clone(), t.clone());
                continue;
            }
        };
        project_posts.insert(slug.clone(), sig);
        lock.project_posts.insert(slug.clone(), t.clone());
    }
    Ok((posts, project_posts))
}

/// Discord's flag for a forum post pinned at the top.
pub const PINNED_THREAD: u64 = 1 << 1;

/// The `[[channel.thread]]` posts the lockfile knows (`[posts]` entries keyed
/// `forum/key`), as signatures to compare.
async fn threads(
    rest: &impl Rest,
    known: &Lock,
    lock: &mut Lock,
    channels: &[Value],
    unreadable: &mut Unreadable,
) -> Result<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    let mut bot = None;
    for (key, p) in known.posts.iter().filter(|(k, _)| is_thread_key(k)) {
        // Only a post in the forum its key names is one of its threads.
        let Some(forum) = key.split_once('/').and_then(|(f, _)| known.channel(f)) else { continue };
        if bot.is_none() {
            bot = Some(me(rest).await?);
        }
        let t = ThreadRef { thread: p.message, message: p.message, parts: p.parts.clone() };
        match forum_post(rest, &t, channels, Some(forum), bot).await? {
            None => {}
            Some((_, None)) => {
                unreadable.posts.insert(key.clone());
                lock.posts.insert(key.clone(), p.clone());
            }
            Some((_, Some(sig))) => {
                out.insert(key.clone(), sig);
                lock.posts.insert(key.clone(), p.clone());
            }
        }
    }
    Ok(out)
}

/// The bot's own user id.
async fn me(rest: &impl Rest) -> Result<Snowflake> {
    use anyhow::Context;
    id(&rest.call(Method::Get, "users/@me", None).await?["id"]).context("no id for the bot")
}

/// A forum post the bot keeps: its thread, and its signature (title, tags,
/// text), or none if the token can't read it. None at all if it's gone, or
/// isn't the bot's post (in `forum`, when given, with `me`'s first message):
/// a lockfile entry can't make the bot take over someone else's.
async fn forum_post(
    rest: &impl Rest,
    t: &ThreadRef,
    channels: &[Value],
    forum: Option<Snowflake>,
    me: Option<Snowflake>,
) -> Result<Option<(Value, Option<String>)>> {
    let (thread, m) = (
        get_read(rest, &format!("channels/{}", t.thread)).await?,
        get_read(rest, &format!("channels/{}/messages/{}", t.thread, t.message)).await?,
    );
    let (thread, m) = match (thread, m) {
        (Read::Found(thread), Read::Found(m)) => (thread, m),
        (Read::Forbidden, _) | (_, Read::Forbidden) => return Ok(Some((Value::Null, None))),
        _ => return Ok(None),
    };
    if forum.is_some_and(|f| id(&thread["parent_id"]) != Some(f))
        || me.is_some_and(|me| id(&m["author"]["id"]) != Some(me))
    {
        return Ok(None);
    }
    let Some(later) = later_parts(rest, t.thread, &t.parts).await? else {
        return Ok(Some((thread, None)));
    };
    let forum = id(&thread["parent_id"]).and_then(|p| channels.iter().find(|c| id(&c["id"]) == Some(p)));
    let applied = wire::ids(&thread["applied_tags"]);
    let tags: Vec<String> = forum
        .map(|f| {
            f["available_tags"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|tag| id(&tag["id"]).is_some_and(|i| applied.contains(&i)))
                .map(|tag| s(&tag["name"]))
                .collect()
        })
        .unwrap_or_default();
    let all: Vec<Rendered> = std::iter::once(wire::rendered_from(&m)).chain(later).collect();
    let sig = projects::signature(&s(&thread["name"]), &tags, &all);
    Ok(Some((thread, Some(sig))))
}

/// A long post's later parts in `ch`, as Discord has them: one deleted reads
/// as empty, so the plan sends it again. None if the token can't read them.
async fn later_parts(rest: &impl Rest, ch: Snowflake, parts: &[Snowflake]) -> Result<Option<Vec<Rendered>>> {
    let mut out = Vec::new();
    for m in parts {
        match get_read(rest, &format!("channels/{ch}/messages/{m}")).await? {
            Read::Found(m) => out.push(wire::rendered_from(&m)),
            Read::Absent => out.push(Rendered { content: String::new(), buttons: Vec::new() }),
            Read::Forbidden => return Ok(None),
        }
    }
    Ok(Some(out))
}

//! Forum posts: resources members share, and project ideas.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use chipsoc_config::Snowflake;
use chipsoc_config::bot::{IDEA_KEYS, IdeaQuestion};
use chipsoc_discord::wire::{id, s};
use chipsoc_discord::{Method, Rest};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::fmt::escape;
use crate::{Actor, Answer, AuditAction, Host, audit, truncate};

/// Discord caps thread titles at 100 characters.
const MAX_TITLE: usize = 95;

/// A resource to share.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct NewResource {
    pub title: String,
    pub url: String,
    pub description: String,
    /// Forum tag names (any case); each must be one of the forum's.
    pub tags: Vec<String>,
}

/// The questions a shared resource is kept under in the review queue (and the form's labels).
pub const RESOURCE_TITLE: &str = "Title";
pub const RESOURCE_LINK: &str = "Link";
pub const RESOURCE_ABOUT: &str = "What is it, and why is it worth a look?";
pub const RESOURCE_TAGS: &str = "Tags";

impl NewResource {
    /// As kept in the review queue.
    pub fn answers(&self) -> Vec<Answer> {
        let tags = self.tags.iter().map(|t| t.trim().to_owned()).filter(|t| !t.is_empty()).collect();
        vec![
            Answer::new(RESOURCE_TITLE, Some(&self.title)),
            Answer::new(RESOURCE_LINK, Some(&self.url)),
            Answer::new(RESOURCE_ABOUT, Some(&self.description)),
            Answer::list(RESOURCE_TAGS, tags),
        ]
    }

    /// Back from [`NewResource::answers`].
    pub fn from_answers(answers: &[Answer]) -> Self {
        let get = |q: &str| answers.iter().find(|a| a.question == q).map(|a| a.answer.clone()).unwrap_or_default();
        Self {
            title: get(RESOURCE_TITLE),
            url: get(RESOURCE_LINK),
            description: get(RESOURCE_ABOUT),
            tags: answers.iter().find(|a| a.question == RESOURCE_TAGS).map(|a| a.items.clone()).unwrap_or_default(),
        }
    }
}

/// Who'd lead a proposed project.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Lead {
    /// The proposer would.
    Lead,
    /// The proposer would help; someone else should lead.
    Help,
    #[default]
    Idea,
}

impl Lead {
    pub const ALL: [Self; 3] = [Self::Lead, Self::Help, Self::Idea];

    pub fn key(self) -> &'static str {
        match self {
            Self::Lead => "lead",
            Self::Help => "help",
            Self::Idea => "idea",
        }
    }

    /// As the form offers it.
    pub fn label(self) -> &'static str {
        match self {
            Self::Lead => "I'd like to lead it",
            Self::Help => "I'd help, but someone else should lead",
            Self::Idea => "Just an idea",
        }
    }

    pub fn parse(s: &str) -> Self {
        Self::ALL.into_iter().find(|l| l.key() == s || l.label() == s).unwrap_or_default()
    }
}

/// A project idea a member proposes with `/propose-project`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct NewIdea {
    pub name: String,
    pub blurb: String,
    /// Tagged with the forum's tag of this name, if it has one.
    pub area: String,
    pub scope: String,
    pub lead: Lead,
    /// Answers to the questions `[ideas] questions` adds, each with its key.
    pub more: Vec<Answer>,
}

/// The questions an idea is kept under in the review queue (and the form's
/// labels) by default (see [`chipsoc_config::bot::IdeaQuestion::own`]).
pub const IDEA_NAME: &str = "Name";
pub const IDEA_BLURB: &str = "One-line description";
pub const IDEA_AREA: &str = "Area";
pub const IDEA_SCOPE: &str = "What would it do?";
/// The lead question, worded the same on the idea form and the project form.
pub const IDEA_LEAD: &str = "Would you like to lead it?";
/// What it was called before, in ideas still kept under it.
const IDEA_LEAD_BEFORE: &str = "Who'd lead it?";

impl NewIdea {
    /// As kept in the review queue, asked by the default form.
    pub fn answers(&self) -> Vec<Answer> {
        self.answers_for(&chipsoc_config::bot::default_idea_questions())
    }

    /// As kept in the review queue: one answer for each of `questions` (the
    /// form it was sent with), in its order, under its key.
    pub fn answers_for(&self, questions: &[IdeaQuestion]) -> Vec<Answer> {
        questions
            .iter()
            .map(|q| {
                let text = match q.key.as_str() {
                    "lead" => self.lead.label().to_owned(),
                    k => self.answer(k).to_owned(),
                };
                Answer::keyed(&q.key, &q.question.ask, text)
            })
            .collect()
    }

    /// What they answered to the question with `key` ("" if none).
    pub fn answer(&self, key: &str) -> &str {
        match key {
            "name" => &self.name,
            "blurb" => &self.blurb,
            "area" => &self.area,
            "scope" => &self.scope,
            "lead" => self.lead.key(),
            k => self.more.iter().find(|a| a.key.as_deref() == Some(k)).map_or("", |a| a.answer.as_str()),
        }
    }

    /// Back from [`NewIdea::answers_for`]: by key, or (kept before keys) by the default questions.
    pub fn from_answers(answers: &[Answer]) -> Self {
        let get = |key: &str, q: &str| {
            let a = answers.iter().find(|a| a.key.as_deref() == Some(key));
            let a = a.or_else(|| answers.iter().find(|a| a.key.is_none() && a.question == q));
            a.map(|a| a.answer.clone()).unwrap_or_default()
        };
        let own = |a: &&Answer| a.key.as_deref().is_some_and(|k| IDEA_KEYS.contains(&k));
        Self {
            name: get("name", IDEA_NAME),
            blurb: get("blurb", IDEA_BLURB),
            area: get("area", IDEA_AREA),
            scope: get("scope", IDEA_SCOPE),
            lead: Lead::parse(
                &Some(get("lead", IDEA_LEAD))
                    .filter(|l| !l.is_empty())
                    .unwrap_or_else(|| get("lead", IDEA_LEAD_BEFORE)),
            ),
            more: answers.iter().filter(|a| a.key.is_some() && !own(a)).cloned().collect(),
        }
    }
}

/// A post in a forum.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Post {
    pub id: Snowflake,
    pub title: String,
    pub tags: Vec<String>,
    /// The post's first message.
    pub content: String,
    pub author: Option<Snowflake>,
    pub archived: bool,
    pub link: String,
}

/// Discord applies at most this many tags to a post.
const MAX_POST_TAGS: usize = 5;
/// Posts looked through for the same link.
const SEEN_POSTS: usize = 100;
/// How long members' submits reuse one read of the resources forum.
const SEEN_FOR: Duration = Duration::from_secs(5 * 60);

/// The resources forum's recent posts, read once per [`SEEN_FOR`] for
/// members' submits: each read is a request per post, on the bot's token.
/// Posting a resource (an admin's accept) always reads afresh, then forgets
/// what was kept.
#[derive(Debug, Default)]
pub struct SeenPosts(Mutex<Option<(Instant, Snowflake, Vec<Post>)>>);

impl SeenPosts {
    fn get(&self, forum: Snowflake) -> Option<Vec<Post>> {
        let kept = self.0.lock().unwrap_or_else(|e| e.into_inner());
        kept.as_ref().filter(|(at, f, _)| *f == forum && at.elapsed() < SEEN_FOR).map(|(_, _, p)| p.clone())
    }

    fn keep(&self, forum: Snowflake, posts: &[Post]) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = Some((Instant::now(), forum, posts.to_vec()));
    }

    fn forget(&self) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
}

/// Why a post wasn't made, in words for whoever asked (not a Discord failure).
fn refuse<T>(why: impl Into<String>) -> Result<T> {
    Err(crate::Refusal::from(why.into()).into())
}

/// The checks on a resource a member shares that ask nothing of Discord: a
/// title, an https link to a public address, a line on what it is. A
/// refusal here may be fixed and sent again at once; [`vet_resource`]'s
/// other checks read the forum.
pub fn precheck_resource(r: &NewResource) -> Result<()> {
    if !r.url.trim().starts_with("https://") {
        return refuse("Give a link starting https://.");
    }
    if r.description.trim().is_empty() {
        return refuse("Say what it is and why it's worth a look.");
    }
    address(r).map(drop)
}

/// A resource's title and link, as [`checked`] wants them.
fn address(r: &NewResource) -> Result<reqwest::Url> {
    if r.title.trim().is_empty() {
        return refuse("Give the resource a title.");
    }
    let Ok(url) = reqwest::Url::parse(r.url.trim()) else { return refuse("That isn't a web address.") };
    if let Err(why) = crate::feeds::check_url(&url) {
        return refuse(format!("That link won't do: {why}."));
    }
    Ok(url)
}

/// Check a resource a member shares with `/share-resource` before it goes
/// to the review queue: [`precheck_resource`], tags the forum has, and a
/// link nobody has shared (as of the last few minutes, see [`SeenPosts`]) or
/// sent for review already.
pub async fn vet_resource<H: Host>(h: &H, r: &NewResource) -> Result<()> {
    precheck_resource(r)?;
    let checked = checked(h, r, h.seen_posts()).await?;
    let waiting = h.db().pending_applications().await?;
    let queued = |a: &chipsoc_store::Application| {
        a.kind == chipsoc_store::AppKind::Resource
            && reqwest::Url::parse(&a.target).is_ok_and(|u| same_link(&u, &checked.url))
    };
    if waiting.iter().any(queued) {
        return refuse("Someone has already sent that link; it's waiting for an admin to look at it.");
    }
    Ok(())
}

/// A resource that passed [`checked`]: where it goes and what it gets.
struct Checked {
    forum: Snowflake,
    info: Value,
    tags: Vec<Value>,
    url: reqwest::Url,
}

/// What every resource needs: a title, a public web address, tags the forum
/// has, and a link not already posted there (else it says where). The posts
/// are those `seen` keeps, if it has them.
async fn checked<H: Host>(h: &H, r: &NewResource, seen: Option<&SeenPosts>) -> Result<Checked> {
    let cfg = h.cfg();
    let key = cfg.bot.resources.as_ref().map(|r| r.forum.clone()).context("resources aren't set up in bot.toml")?;
    let forum = forum_id(h, &key)?;
    let url = address(r)?;
    let info = h.rest().call(Method::Get, &format!("channels/{forum}"), None).await?;
    let tags = chosen_tags(&info, &r.tags)?;
    let seen = match seen.and_then(|s| s.get(forum)) {
        Some(posts) => posts,
        None => {
            let posts = posts(h, &key, SEEN_POSTS).await?;
            if let Some(s) = seen {
                s.keep(forum, &posts);
            }
            posts
        }
    };
    if let Some(p) = seen.into_iter().find(|p| links(&p.content).any(|l| same_link(&l, &url))) {
        return refuse(format!("That link has already been shared: {}", p.link));
    }
    Ok(Checked { forum, info, tags, url })
}

/// Post a resource in the resources forum, crediting `by`. Refused if the
/// link has been shared there before (saying where), or a tag isn't one of
/// the forum's.
pub async fn post_resource<H: Host>(h: &H, r: &NewResource, by: &Actor) -> Result<Post> {
    let Checked { forum, info, tags, url } = checked(h, r, None).await?;
    let mut text = format!("{url}");
    if !r.description.trim().is_empty() {
        // Their words, shown as written: no masked links or headings in a bot's post.
        text.push_str(&format!("\n\n{}", escape(r.description.trim())));
    }
    text.push_str(&format!("\n\nShared by {}", by.mention()));
    let body = json!({
        "name": truncate(r.title.trim(), MAX_TITLE),
        "applied_tags": tags,
        "message": { "content": truncate(&text, crate::MAX_CONTENT), "allowed_mentions": { "parse": [] } },
    });
    let thread = h.rest().call(Method::Post, &format!("channels/{forum}/threads"), Some(body)).await?;
    if let Some(s) = h.seen_posts() {
        s.forget();
    }
    audit(h, by, AuditAction::PostResource, Some(r.title.trim()), Some(url.as_str())).await;
    let names = tag_names(&info);
    let made = post(h.guild(), &thread, &names, text);
    if !crate::shares::partners_for(&h.cfg(), crate::shares::ShareKind::Resource).is_empty() {
        let name = match by.id {
            Some(u) => crate::shares::display_name(h, u).await,
            None => by.name.clone(),
        };
        crate::shares::send_quietly(h, &crate::shares::resource(&made, r, &name), by.id).await;
    }
    Ok(made)
}

/// An accepted idea's post. With `template` (`[ideas] post`): that, with
/// `{answer:<key>}`, `{by}` and `{lead}` filled in. Else the area, who
/// proposed it, what it would do, the answers to any questions the form
/// adds, and who'd lead it. Their words are shown as written: no masked
/// links or headings in a bot's post.
pub fn idea_text(idea: &NewIdea, by: Snowflake, template: Option<&str>) -> String {
    let who = match idea.lead {
        Lead::Lead => format!("<@{by}> would like to lead it."),
        Lead::Help => "Looking for someone to lead it.".to_owned(),
        Lead::Idea => "Just an idea for now; looking for a lead and a team.".to_owned(),
    };
    if let Some(t) = template {
        return chipsoc_config::template::fill(t, |key| match key {
            "by" => Some(format!("<@{by}>")),
            "lead" => Some(who.clone()),
            _ => {
                let key = key.strip_prefix("answer:")?;
                let text = if key == "lead" { idea.lead.label() } else { idea.answer(key) };
                Some(escape(text.trim()))
            }
        });
    }
    let mut parts = Vec::new();
    if !idea.area.trim().is_empty() {
        parts.push(format!("**Area:** {}\nProposed by <@{by}>", escape(idea.area.trim())));
    } else {
        parts.push(format!("Proposed by <@{by}>"));
    }
    if !idea.scope.trim().is_empty() {
        parts.push(format!("**What it would do**\n{}", escape(idea.scope.trim())));
    }
    for a in idea.more.iter().filter(|a| !a.answer.trim().is_empty()) {
        parts.push(format!("**{}**\n{}", escape(a.question.trim()), escape(a.answer.trim())));
    }
    parts.push(who);
    parts.push("_Reply or react to back it._".to_owned());
    parts.join("\n\n")
}

/// Post an accepted idea in the ideas forum, crediting whoever proposed it:
/// tagged with its area (if the forum has that tag) and, unless they'll lead
/// it, the needs-lead tag. Its text is [`idea_text`].
pub async fn post_idea<H: Host>(h: &H, idea: &NewIdea, by: Snowflake) -> Result<Post> {
    let cfg = h.cfg();
    let conf = cfg.bot.ideas.as_ref().context("the ideas forum isn't set up in bot.toml")?;
    let forum = forum_id(h, &conf.forum)?;
    let info = h.rest().call(Method::Get, &format!("channels/{forum}"), None).await?;
    let needs_lead = idea.lead != Lead::Lead;
    let wanted = |t: &&Value| {
        let name = s(&t["name"]);
        (!idea.area.trim().is_empty() && name.eq_ignore_ascii_case(idea.area.trim()))
            || (needs_lead && conf.needs_lead_tag.as_deref().is_some_and(|n| n.eq_ignore_ascii_case(&name)))
    };
    let tags: Vec<Value> =
        available(&info).iter().filter(wanted).map(|t| t["id"].clone()).take(MAX_POST_TAGS).collect();
    let template = match &conf.post {
        Some(path) => match chipsoc_config::inside(&cfg.dir, path).and_then(|p| Ok(std::fs::read_to_string(p)?)) {
            Ok(t) => Some(t),
            Err(e) => {
                tracing::warn!("reading [ideas] post {path} failed, so the idea is posted as by default: {e:#}");
                None
            }
        },
        None => None,
    };
    let text = idea_text(idea, by, template.as_deref());
    let title = match (idea.name.trim(), idea.blurb.trim()) {
        (name, "") => name.to_owned(),
        (name, blurb) => format!("{name} — {blurb}"),
    };
    let title = truncate(&title, MAX_TITLE);
    let body = json!({
        "name": title,
        "applied_tags": tags,
        "message": { "content": truncate(&text, crate::MAX_CONTENT), "allowed_mentions": { "parse": [] } },
    });
    let thread = h.rest().call(Method::Post, &format!("channels/{forum}/threads"), Some(body)).await?;
    Ok(post(h.guild(), &thread, &tag_names(&info), text))
}

/// The ids of the forum's tags named in `wanted` (any case). Refused if one
/// isn't the forum's, or there are too many.
fn chosen_tags(forum: &Value, wanted: &[String]) -> Result<Vec<Value>> {
    let have = available(forum);
    let mut ids = Vec::new();
    for w in wanted.iter().map(|w| w.trim()).filter(|w| !w.is_empty()) {
        match have.iter().find(|t| w.eq_ignore_ascii_case(&s(&t["name"]))) {
            Some(t) if !ids.contains(&t["id"]) => ids.push(t["id"].clone()),
            Some(_) => {}
            None if have.is_empty() => return refuse("The resources forum has no tags; leave them out."),
            None => {
                let names: Vec<String> = have.iter().map(|t| s(&t["name"])).collect();
                return refuse(format!("There's no tag `{w}`. Pick from: {}.", names.join(", ")));
            }
        }
    }
    if ids.len() > MAX_POST_TAGS {
        return refuse(format!("Pick at most {MAX_POST_TAGS} tags."));
    }
    Ok(ids)
}

/// The web addresses in a post's text.
fn links(text: &str) -> impl Iterator<Item = reqwest::Url> + '_ {
    text.split(|c: char| c.is_whitespace() || matches!(c, '<' | '>' | '(' | ')'))
        .filter(|w| w.starts_with("https://") || w.starts_with("http://"))
        .filter_map(|w| reqwest::Url::parse(w).ok())
}

/// Whether two addresses are the same page: scheme, `www.`, a trailing
/// slash and the `#fragment` aside.
fn same_link(a: &reqwest::Url, b: &reqwest::Url) -> bool {
    let key = |u: &reqwest::Url| {
        let host = u.host_str().unwrap_or_default().trim_start_matches("www.").to_lowercase();
        let path = u.path().trim_end_matches('/').to_owned();
        (host, u.port_or_known_default().filter(|p| ![80, 443].contains(p)), path, u.query().map(str::to_owned))
    };
    key(a) == key(b)
}

/// Take a resource down: its post is closed and locked, never deleted.
/// Only posts in the resources forum. False if there's no such post there.
pub async fn archive_resource<H: Host>(h: &H, post: Snowflake, by: &Actor) -> Result<bool> {
    let key = h.cfg().bot.resources.as_ref().map(|r| r.forum.clone()).context("resources aren't set up in bot.toml")?;
    let forum = forum_id(h, &key)?;
    let thread = match h.rest().call(Method::Get, &format!("channels/{post}"), None).await {
        Ok(t) => t,
        Err(e) if chipsoc_discord::ApiError::status_of(&e) == Some(404) => return Ok(false),
        Err(e) => return Err(e),
    };
    if id(&thread["parent_id"]) != Some(forum) {
        return Ok(false);
    }
    let body = json!({ "archived": true, "locked": true });
    h.rest().call(Method::Patch, &format!("channels/{post}"), Some(body)).await?;
    audit(h, by, AuditAction::ArchiveResource, Some(&s(&thread["name"])), Some(&post.to_string())).await;
    if let Err(e) = crate::shares::remove(h, crate::shares::ShareKind::Resource, &post.to_string()).await {
        tracing::warn!("telling partners a resource was archived failed: {e:#}");
    }
    Ok(true)
}

/// What the bot posts in an idea it closes: the reason, else a plain line,
/// so members never just find a locked post.
pub fn closing_line(reason: Option<&str>) -> String {
    match reason.map(str::trim).filter(|r| !r.is_empty()) {
        Some(why) => truncate(&format!("🔒 **Closed:** {why}\nIt stays here to read."), crate::MAX_CONTENT),
        None => "🔒 **Closed.** It stays here to read.".to_owned(),
    }
}

/// An idea [`close_idea`] closed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Closed {
    /// The post's title.
    pub title: String,
    /// The tag it got (`[ideas] closed_tag`), if one is set.
    pub tag: Option<String>,
}

impl Closed {
    /// What happened, for whoever closed it (Discord markdown).
    pub fn said(&self, post: Snowflake) -> String {
        match &self.tag {
            Some(t) => {
                format!("Closed <#{post}>: tagged **{}**, locked and archived; still readable.", crate::fmt::escape(t))
            }
            None => format!("Closed <#{post}>: locked and archived; still readable."),
        }
    }
}

/// Close an idea: its post gets `[ideas] closed_tag` if set (and loses the
/// needs-lead tag), the [`closing_line`] as the bot's reply, then is archived
/// and locked, never deleted. Only posts in the ideas forum: `None` if
/// there's no such post there.
pub async fn close_idea<H: Host>(h: &H, post: Snowflake, reason: Option<&str>, by: &Actor) -> Result<Option<Closed>> {
    let cfg = h.cfg();
    let conf = cfg.bot.ideas.as_ref().context("the ideas forum isn't set up in bot.toml")?;
    let forum = forum_id(h, &conf.forum)?;
    let rest = h.rest();
    let thread = match rest.call(Method::Get, &format!("channels/{post}"), None).await {
        Ok(t) => t,
        Err(e) if chipsoc_discord::ApiError::status_of(&e) == Some(404) => return Ok(None),
        Err(e) => return Err(e),
    };
    if id(&thread["parent_id"]) != Some(forum) {
        return Ok(None);
    }
    let info = rest.call(Method::Get, &format!("channels/{forum}"), None).await?;
    let tag = |name: &Option<String>| -> Option<Value> {
        let name = name.as_deref()?;
        available(&info).into_iter().find(|t| s(&t["name"]).eq_ignore_ascii_case(name)).map(|t| t["id"].clone())
    };
    let closed = match (&conf.closed_tag, tag(&conf.closed_tag)) {
        (Some(name), None) => {
            anyhow::bail!("the ideas forum has no tag `{name}` (`[ideas] closed_tag`); add it and apply")
        }
        (_, t) => t,
    };
    let needs_lead = tag(&conf.needs_lead_tag);
    let mut tags: Vec<Value> = thread["applied_tags"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|t| Some(*t) != needs_lead.as_ref() && Some(*t) != closed.as_ref())
        .cloned()
        .collect();
    let tagged = closed.is_some().then(|| conf.closed_tag.clone()).flatten();
    if let Some(c) = closed {
        tags.truncate(MAX_POST_TAGS - 1);
        tags.push(c);
    }
    // An archived post takes no replies until it's opened again. The reply
    // goes first and the rest in one change, so a failure leaves the post as
    // it was: archived again if it was.
    let path = format!("channels/{post}");
    let was_archived = thread["thread_metadata"]["archived"] == true;
    if was_archived {
        rest.call(Method::Patch, &path, Some(json!({ "archived": false }))).await?;
    }
    let body = json!({ "content": closing_line(reason), "allowed_mentions": { "parse": [] } });
    if let Err(e) = rest.call(Method::Post, &format!("channels/{post}/messages"), Some(body)).await {
        if was_archived && let Err(e) = rest.call(Method::Patch, &path, Some(json!({ "archived": true }))).await {
            tracing::warn!("archiving idea {post} again failed: {e:#}");
        }
        return Err(e.context("posting why the idea is closed"));
    }
    let close = json!({ "applied_tags": tags, "archived": true, "locked": true });
    rest.call(Method::Patch, &path, Some(close)).await.context("closing the idea")?;
    let title = s(&thread["name"]);
    audit(h, by, AuditAction::CloseIdea, Some(&title), Some(&post.to_string())).await;
    Ok(Some(Closed { title, tag: tagged }))
}

/// The newest posts in the resources forum.
pub async fn resources<H: Host>(h: &H, limit: usize) -> Result<Vec<Post>> {
    let key = h.cfg().bot.resources.as_ref().map(|r| r.forum.clone()).context("resources aren't set up in bot.toml")?;
    posts(h, &key, limit).await
}

/// The newest posts in the ideas forum.
pub async fn ideas<H: Host>(h: &H, limit: usize) -> Result<Vec<Post>> {
    let key =
        h.cfg().bot.ideas.as_ref().map(|i| i.forum.clone()).context("the ideas forum isn't set up in bot.toml")?;
    posts(h, &key, limit).await
}

fn forum_id<H: Host>(h: &H, key: &str) -> Result<Snowflake> {
    h.cfg().desired.lock.channel(key).with_context(|| format!("#{key} isn't in Discord yet; apply the config first"))
}

/// Open and archived posts in a forum, newest first, with their first messages.
async fn posts<H: Host>(h: &H, key: &str, limit: usize) -> Result<Vec<Post>> {
    let forum = forum_id(h, key)?;
    let rest = h.rest();
    let active = rest.call(Method::Get, &format!("guilds/{}/threads/active", h.guild()), None).await?;
    let archived = format!("channels/{forum}/threads/archived/public?limit={limit}");
    let archived = rest.call(Method::Get, &archived, None).await?;
    let mut threads: Vec<Value> = [&active, &archived]
        .into_iter()
        .flat_map(|v| v["threads"].as_array().cloned().unwrap_or_default())
        .filter(|t| id(&t["parent_id"]) == Some(forum))
        .collect();
    threads.sort_by_key(|t| std::cmp::Reverse(id(&t["id"]).map_or(0, |i| i.0)));
    threads.truncate(limit);
    let names = tag_names(&rest.call(Method::Get, &format!("channels/{forum}"), None).await?);
    let mut out = Vec::new();
    for t in threads {
        // A forum post's first message has the thread's id.
        let tid = s(&t["id"]);
        let first = rest.call(Method::Get, &format!("channels/{tid}/messages/{tid}"), None).await;
        let content = first.map(|m| s(&m["content"])).unwrap_or_default();
        out.push(post(h.guild(), &t, &names, content));
    }
    Ok(out)
}

fn available(forum: &Value) -> Vec<Value> {
    forum["available_tags"].as_array().cloned().unwrap_or_default()
}

/// Tag id → name.
fn tag_names(forum: &Value) -> Vec<(Value, String)> {
    available(forum).into_iter().map(|t| (t["id"].clone(), s(&t["name"]))).collect()
}

fn post(guild: Snowflake, t: &Value, names: &[(Value, String)], content: String) -> Post {
    let applied = t["applied_tags"].as_array().cloned().unwrap_or_default();
    let tags = names.iter().filter(|(i, _)| applied.contains(i)).map(|(_, n)| n.clone()).collect();
    let tid = id(&t["id"]).unwrap_or_default();
    Post {
        id: tid,
        title: s(&t["name"]),
        tags,
        content,
        author: id(&t["owner_id"]),
        archived: t["thread_metadata"]["archived"] == true,
        link: format!("https://discord.com/channels/{guild}/{tid}"),
    }
}

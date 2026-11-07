//! An in-memory Discord for tests: just the REST endpoints the engine uses,
//! behaving like the real ones where it matters (ids, positions, tags, 404s,
//! role hierarchy). Inspect or tamper with `state` to simulate UI changes.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use anyhow::Result;
use chipsoc_config::Snowflake;
use serde_json::{Map, Value, json};

use crate::{ApiError, Method, Rest};

#[derive(Debug, Default)]
pub struct State {
    pub guild: Map<String, Value>,
    pub roles: Vec<Value>,
    pub channels: Vec<Value>,
    pub messages: Vec<Value>,
    pub onboarding: Value,
    pub verification: Value,
    pub automod: Vec<Value>,
    /// Server members: `{ "user": { "id": … }, "roles": [ids] }`.
    pub members: Vec<Value>,
    /// DM channels opened: `{ "id": …, "recipient": user id }`.
    pub dms: Vec<Value>,
    /// Every mutating call made, as "METHOD path".
    pub calls: Vec<String>,
    /// Every body sent, with its "METHOD path", as Discord would get it.
    pub bodies: Vec<(String, Value)>,
    /// Interaction replies, in order: `{ "token": …, "reply": the callback body }`
    /// or `{ "token": …, "edit": the new original message }`.
    pub replies: Vec<Value>,
    /// Scheduled events, as created.
    pub events: Vec<Value>,
    /// The guild's slash commands, as last set.
    pub commands: Value,
    /// Audit-log entries, newest first, as Discord lists them.
    pub audit_log: Vec<Value>,
}

pub struct FakeDiscord {
    pub guild_id: Snowflake,
    pub bot_role: Snowflake,
    pub state: Mutex<State>,
    /// Failures to inject, used once each in order.
    pub faults: Mutex<Vec<Fault>>,
    next: AtomicU64,
    /// Every call made, reads too.
    requests: AtomicUsize,
}

/// Make the next call matching `call` ("POST guilds/1000/channels") fail.
#[derive(Debug, Clone)]
pub struct Fault {
    pub call: String,
    pub status: u16,
    /// Fail after doing the work, like a timeout after Discord acted.
    pub after: bool,
    /// Discord's own error code to give (50007: can't DM this user).
    pub code: Option<u64>,
}

fn err(status: u16, message: &str) -> anyhow::Error {
    ApiError { status, message: message.into(), code: None }.into()
}

/// The bot's application id, as webhooks for its interactions carry it.
pub const APP: &str = "1";

impl FakeDiscord {
    /// Who owns the pretend server, unless a test says otherwise.
    pub const OWNER: u64 = 999_999;

    /// A server with @everyone and the bot's own (managed, top) role, owned
    /// by someone who takes no part (user [`FakeDiscord::OWNER`]).
    pub fn new() -> Self {
        let guild_id = Snowflake(1000);
        let bot_role = Snowflake(1001);
        let guild = json!({ "id": "1000", "name": "Test", "features": [], "owner_id": Self::OWNER.to_string() });
        let state = State {
            guild: guild.as_object().cloned().unwrap_or_default(),
            roles: vec![
                json!({ "id": "1000", "name": "@everyone", "position": 0, "permissions": "1024", "color": 0, "managed": false }),
                json!({ "id": "1001", "name": "Bot", "position": 1, "permissions": "8", "color": 0, "managed": true, "tags": { "bot_id": "42" } }),
            ],
            onboarding: json!({ "enabled": false, "mode": 0, "default_channel_ids": [], "prompts": [] }),
            verification: json!({ "description": "", "form_fields": [] }),
            ..Default::default()
        };
        Self {
            guild_id,
            bot_role,
            state: Mutex::new(state),
            faults: Mutex::new(Vec::new()),
            next: AtomicU64::new(2000),
            requests: AtomicUsize::new(0),
        }
    }

    /// How many calls have been made, reads included (what a member's action
    /// costs the bot's token).
    pub fn requests(&self) -> usize {
        self.requests.load(Ordering::Relaxed)
    }

    fn new_id(&self) -> String {
        self.next.fetch_add(1, Ordering::Relaxed).to_string()
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn fail(&self, call: &str, status: u16, after: bool) {
        let fault = Fault { call: call.to_owned(), status, after, code: None };
        self.faults.lock().unwrap_or_else(|e| e.into_inner()).push(fault);
    }

    /// Make the next `call` fail with Discord's error `code` too.
    pub fn fail_with_code(&self, call: &str, status: u16, code: u64) {
        let fault = Fault { call: call.to_owned(), status, after: false, code: Some(code) };
        self.faults.lock().unwrap_or_else(|e| e.into_inner()).push(fault);
    }

    /// Mutating calls made since the last `take_calls`.
    pub fn take_calls(&self) -> Vec<String> {
        std::mem::take(&mut self.state().calls)
    }

    /// Add someone to the server.
    pub fn join(&self, user: u64) {
        self.state().members.push(json!({ "user": { "id": user.to_string() }, "roles": [] }));
    }

    /// The role ids a member holds.
    pub fn roles_of(&self, user: u64) -> Vec<String> {
        let st = self.state();
        let m = st.members.iter().find(|m| m["user"]["id"].as_str() == Some(user.to_string().as_str()));
        m.and_then(|m| m["roles"].as_array())
            .map(|r| r.iter().filter_map(|x| x.as_str().map(str::to_owned)).collect())
            .unwrap_or_default()
    }

    /// The replies to the interaction with this token (see [`State::replies`]).
    pub fn replies_to(&self, token: &str) -> Vec<Value> {
        self.state().replies.iter().filter(|r| r["token"] == token).cloned().collect()
    }

    /// The texts of messages sent to `user` by DM.
    pub fn dms_to(&self, user: u64) -> Vec<String> {
        let st = self.state();
        let user = user.to_string();
        let chans: Vec<&Value> = st.dms.iter().filter(|d| d["recipient"] == user.as_str()).map(|d| &d["id"]).collect();
        st.messages
            .iter()
            .filter(|m| chans.contains(&&m["channel_id"]))
            .map(|m| crate::wire::s(&m["content"]))
            .collect()
    }

    /// Change a channel as if someone used Discord's UI.
    pub fn edit_channel(&self, name: &str, f: impl FnOnce(&mut Value)) {
        let mut st = self.state();
        if let Some(c) = st.channels.iter_mut().find(|c| c["name"] == name) {
            f(c);
        }
    }

    fn bot_position(&self, st: &State) -> i64 {
        let bot = st.roles.iter().find(|r| r["id"] == self.bot_role.to_string());
        bot.and_then(|r| r["position"].as_i64()).unwrap_or(i64::MAX)
    }

    /// Whether a role is at or above the bot's own (or is it), so Discord
    /// won't let the bot change it, move it, or give it to anyone.
    fn above_bot(&self, st: &State, role: &str) -> bool {
        let pos = st.roles.iter().find(|r| r["id"] == role).and_then(|r| r["position"].as_i64());
        role == self.bot_role.to_string() || pos.is_some_and(|p| p >= self.bot_position(st))
    }

    /// Move a role, as an owner dragging it in Server Settings would.
    pub fn set_role_position(&self, name: &str, position: i64) {
        let mut st = self.state();
        if let Some(r) = st.roles.iter_mut().find(|r| r["name"] == name) {
            r["position"] = json!(position);
        }
    }

    fn route(&self, method: Method, path: &str, body: Option<Value>) -> Result<Value> {
        let g = self.guild_id.to_string();
        let parts: Vec<&str> = path.split('?').next().unwrap_or_default().split('/').collect();
        let body = body.unwrap_or(Value::Null);
        let mut st = self.state();
        if !matches!(method, Method::Get) {
            st.calls.push(format!("{method:?} {path}"));
        }
        if !body.is_null() {
            st.bodies.push((format!("{method:?} {path}"), body.clone()));
        }
        let find = |list: &[Value], i: &str| list.iter().position(|x| x["id"] == i);
        match (method, parts.as_slice()) {
            (Method::Get, ["users", "@me"]) => Ok(json!({ "id": "42", "username": "bot" })),
            (Method::Get, ["users", "@me", "guilds"]) => Ok(json!([{ "id": g, "name": st.guild["name"] }])),
            (Method::Get, ["guilds", id]) if *id == g => {
                let mut v = Value::Object(st.guild.clone());
                v["roles"] = Value::Array(st.roles.clone());
                Ok(v)
            }
            (Method::Patch, ["guilds", id]) if *id == g => {
                for (k, v) in body.as_object().cloned().unwrap_or_default() {
                    let (k, v) = match k.as_str() {
                        "icon" | "banner" => (k.clone(), json!(chipsoc_model::stable_hash(v.to_string()))),
                        _ => (k, v),
                    };
                    st.guild.insert(k, v);
                }
                Ok(Value::Object(st.guild.clone()))
            }
            (Method::Get, ["guilds", _, "channels"]) => Ok(Value::Array(st.channels.clone())),
            (Method::Get, ["guilds", _, "roles"]) => Ok(Value::Array(st.roles.clone())),
            (Method::Get, ["guilds", _, "members"]) => {
                let after: u64 =
                    path.split("after=").nth(1).and_then(|a| a.split('&').next()?.parse().ok()).unwrap_or(0);
                let page = st
                    .members
                    .iter()
                    .filter(|m| m["user"]["id"].as_str().and_then(|i| i.parse::<u64>().ok()).unwrap_or(0) > after);
                Ok(Value::Array(page.take(1000).cloned().collect()))
            }
            (Method::Get, ["guilds", _, "members", uid]) => {
                st.members.iter().find(|m| m["user"]["id"] == *uid).cloned().ok_or_else(|| err(404, "Unknown Member"))
            }
            (Method::Patch, ["guilds", _, "members", uid]) => {
                let m = st
                    .members
                    .iter_mut()
                    .find(|m| m["user"]["id"] == *uid)
                    .ok_or_else(|| err(404, "Unknown Member"))?;
                for (k, v) in body.as_object().cloned().unwrap_or_default() {
                    m[k] = v;
                }
                Ok(m.clone())
            }
            (Method::Put | Method::Delete, ["guilds", _, "members", _, "roles", rid]) if self.above_bot(&st, rid) => {
                Err(err(403, "Missing Permissions"))
            }
            (Method::Put | Method::Delete, ["guilds", _, "members", uid, "roles", rid]) => {
                let m = st
                    .members
                    .iter_mut()
                    .find(|m| m["user"]["id"] == *uid)
                    .ok_or_else(|| err(404, "Unknown Member"))?;
                let roles = m["roles"].as_array_mut().ok_or_else(|| err(500, "bad member"))?;
                roles.retain(|r| r != rid);
                if matches!(method, Method::Put) {
                    roles.push(json!(rid));
                }
                Ok(Value::Null)
            }
            (Method::Post, ["guilds", _, "channels"]) => {
                let mut c = body;
                c["id"] = json!(self.new_id());
                let parent = c["parent_id"].clone();
                let max = st
                    .channels
                    .iter()
                    .filter(|x| x["parent_id"] == parent)
                    .filter_map(|x| x["position"].as_i64())
                    .max();
                c["position"] = json!(max.map_or(0, |m| m + 1));
                with_tag_ids(&mut c, &self.next);
                st.channels.push(c.clone());
                Ok(c)
            }
            (Method::Patch, ["guilds", _, "channels"]) => {
                for p in body.as_array().into_iter().flatten() {
                    if let Some(i) = p["id"].as_str().and_then(|i| find(&st.channels, i)) {
                        st.channels[i]["position"] = p["position"].clone();
                        if !p["parent_id"].is_null() {
                            st.channels[i]["parent_id"] = p["parent_id"].clone();
                        }
                    }
                }
                Ok(Value::Null)
            }
            (Method::Get, ["channels", cid]) => {
                st.channels.iter().find(|c| c["id"] == *cid).cloned().ok_or_else(|| err(404, "Unknown Channel"))
            }
            (Method::Patch, ["channels", cid]) => {
                let i = find(&st.channels, cid).ok_or_else(|| err(404, "Unknown Channel"))?;
                let mut patch = body;
                with_tag_ids(&mut patch, &self.next);
                if st.channels[i]["thread"] == true {
                    // Like Discord: one pinned post per forum.
                    let pins = patch["flags"].as_u64().is_some_and(|f| f & 2 != 0);
                    let parent = st.channels[i]["parent_id"].clone();
                    let other = st.channels.iter().any(|c| {
                        c["thread"] == true
                            && c["parent_id"] == parent
                            && c["id"] != *cid
                            && c["flags"].as_u64().is_some_and(|f| f & 2 != 0)
                    });
                    if pins && other {
                        return Err(err(400, "Maximum number of pinned threads in a forum channel has been reached"));
                    }
                    // Discord keeps these in the thread's metadata.
                    for k in ["archived", "locked"] {
                        if let Some(v) = patch.get(k) {
                            st.channels[i]["thread_metadata"][k] = v.clone();
                        }
                    }
                    // And says a rename in the thread, as by whoever made it (here the bot).
                    if let Some(name) = patch.get("name").filter(|n| **n != st.channels[i]["name"]) {
                        let notice = json!({
                            "id": self.new_id(), "channel_id": cid, "type": 4, "content": name, "author": { "id": "42" },
                        });
                        st.messages.push(notice);
                    }
                }
                for (k, v) in patch.as_object().cloned().unwrap_or_default() {
                    st.channels[i][k] = v;
                }
                Ok(st.channels[i].clone())
            }
            (Method::Delete, ["channels", cid]) => {
                let i = find(&st.channels, cid).ok_or_else(|| err(404, "Unknown Channel"))?;
                Ok(st.channels.remove(i))
            }
            (Method::Post, ["guilds", _, "roles"]) => {
                let mut r = body;
                r["id"] = json!(self.new_id());
                r["position"] = json!(1);
                r["managed"] = json!(false);
                // New roles go just above @everyone, pushing the rest up.
                for x in st.roles.iter_mut().filter(|x| x["position"].as_i64().unwrap_or(0) >= 1) {
                    x["position"] = json!(x["position"].as_i64().unwrap_or(0) + 1);
                }
                st.roles.push(r.clone());
                Ok(r)
            }
            (Method::Patch, ["guilds", _, "roles"]) => {
                // Like Discord: the whole request is refused if any role in it
                // is, or would be, at or above the bot's own.
                let bot_pos = self.bot_position(&st);
                let list: Vec<Value> = body.as_array().cloned().unwrap_or_default();
                for p in &list {
                    let pos = p["position"].as_i64().unwrap_or(0);
                    let now = p["id"]
                        .as_str()
                        .and_then(|i| find(&st.roles, i))
                        .and_then(|i| st.roles[i]["position"].as_i64());
                    if p["id"] == self.bot_role.to_string()
                        || [Some(pos), now].into_iter().flatten().any(|x| x >= bot_pos)
                    {
                        return Err(err(403, "Missing Permissions"));
                    }
                }
                for p in &list {
                    if let Some(i) = p["id"].as_str().and_then(|i| find(&st.roles, i)) {
                        st.roles[i]["position"] = p["position"].clone();
                    }
                }
                Ok(Value::Array(st.roles.clone()))
            }
            (Method::Patch | Method::Delete, ["guilds", _, "roles", rid]) if self.above_bot(&st, rid) => {
                Err(err(403, "Missing Permissions"))
            }
            (Method::Patch, ["guilds", _, "roles", rid]) => {
                let i = find(&st.roles, rid).ok_or_else(|| err(404, "Unknown Role"))?;
                for (k, v) in body.as_object().cloned().unwrap_or_default() {
                    st.roles[i][k] = v;
                }
                Ok(st.roles[i].clone())
            }
            (Method::Delete, ["guilds", _, "roles", rid]) => {
                let i = find(&st.roles, rid).ok_or_else(|| err(404, "Unknown Role"))?;
                st.roles.remove(i);
                Ok(Value::Null)
            }
            (Method::Get, ["guilds", _, "onboarding"]) => Ok(st.onboarding.clone()),
            (Method::Put, ["guilds", _, "onboarding"]) => {
                st.onboarding = body;
                Ok(st.onboarding.clone())
            }
            (Method::Get, ["guilds", _, "member-verification"]) => Ok(st.verification.clone()),
            (Method::Patch, ["guilds", _, "member-verification"]) => {
                let on = body["enabled"].as_bool().unwrap_or(false);
                st.verification = body;
                let features = st.guild.entry("features").or_insert(json!([]));
                if let Some(f) = features.as_array_mut() {
                    f.retain(|x| x != "MEMBER_VERIFICATION_GATE_ENABLED");
                    if on {
                        f.push(json!("MEMBER_VERIFICATION_GATE_ENABLED"));
                    }
                }
                Ok(st.verification.clone())
            }
            (Method::Get, ["guilds", _, "auto-moderation", "rules"]) => Ok(Value::Array(st.automod.clone())),
            (Method::Post, ["guilds", _, "auto-moderation", "rules"]) => {
                let mut r = body;
                r["id"] = json!(self.new_id());
                st.automod.push(r.clone());
                Ok(r)
            }
            (Method::Patch, ["guilds", _, "auto-moderation", "rules", rid]) => {
                let i = find(&st.automod, rid).ok_or_else(|| err(404, "Unknown Rule"))?;
                for (k, v) in body.as_object().cloned().unwrap_or_default() {
                    st.automod[i][k] = v;
                }
                Ok(st.automod[i].clone())
            }
            (Method::Delete, ["guilds", _, "auto-moderation", "rules", rid]) => {
                let i = find(&st.automod, rid).ok_or_else(|| err(404, "Unknown Rule"))?;
                st.automod.remove(i);
                Ok(Value::Null)
            }
            (Method::Put, ["channels", _, "voice-status"]) => Ok(Value::Null),
            (Method::Post, ["channels", cid, "messages"]) => {
                let mut m = body;
                m["id"] = json!(self.new_id());
                m["channel_id"] = json!(cid);
                m["author"] = json!({ "id": "42" });
                st.messages.push(m.clone());
                Ok(m)
            }
            (Method::Get, ["channels", cid, "messages"]) => {
                let after: u64 = query(path, "after").unwrap_or(0);
                let newer = |m: &&Value| m["id"].as_str().and_then(|i| i.parse::<u64>().ok()).unwrap_or(0) > after;
                let newest_first = st.messages.iter().rev().filter(|m| m["channel_id"] == *cid).filter(newer);
                Ok(Value::Array(newest_first.take(100).cloned().collect()))
            }
            (Method::Post, ["channels", cid, "messages", mid, "crosspost"]) => st
                .messages
                .iter()
                .find(|m| m["id"] == *mid && m["channel_id"] == *cid)
                .cloned()
                .ok_or_else(|| err(404, "Unknown Message")),
            (Method::Get, ["channels", cid, "messages", mid]) => st
                .messages
                .iter()
                .find(|m| m["id"] == *mid && m["channel_id"] == *cid)
                .cloned()
                .ok_or_else(|| err(404, "Unknown Message")),
            (Method::Patch, ["channels", cid, "messages", mid]) => {
                let m = st
                    .messages
                    .iter_mut()
                    .find(|m| m["id"] == *mid && m["channel_id"] == *cid)
                    .ok_or_else(|| err(404, "Unknown Message"))?;
                for (k, v) in body.as_object().cloned().unwrap_or_default() {
                    m[k] = v;
                }
                Ok(m.clone())
            }
            (Method::Delete, ["channels", cid, "messages", mid]) => {
                let at = st
                    .messages
                    .iter()
                    .position(|m| m["id"] == *mid && m["channel_id"] == *cid)
                    .ok_or_else(|| err(404, "Unknown Message"))?;
                st.messages.remove(at);
                Ok(Value::Null)
            }
            (Method::Post, ["interactions", _, token, "callback"]) => {
                st.replies.push(json!({ "token": token, "reply": body }));
                Ok(Value::Null)
            }
            (Method::Patch, ["webhooks", app, token, "messages", "@original"]) => {
                if *app != APP {
                    return Err(err(404, "Unknown Webhook"));
                }
                st.replies.push(json!({ "token": token, "edit": body }));
                Ok(body)
            }
            (Method::Post, ["webhooks", hook, token]) => {
                // An interaction's follow-up (the bot's application id), or a post through a webhook.
                st.replies.push(json!({ "token": token, "webhook": hook, "followup": body }));
                Ok(Value::Null)
            }
            (Method::Put, ["applications", _, "guilds", _, "commands"]) => {
                st.commands = body;
                Ok(st.commands.clone())
            }
            (Method::Get, ["guilds", _, "audit-logs"]) => {
                Ok(json!({ "audit_log_entries": st.audit_log.clone(), "users": [], "webhooks": [] }))
            }
            (Method::Post, ["users", "@me", "channels"]) => {
                let dm = json!({ "id": self.new_id(), "type": 1, "recipient": body["recipient_id"] });
                st.dms.push(dm.clone());
                Ok(dm)
            }
            (Method::Get, ["guilds", _, "threads", "active"]) => {
                let active = st.channels.iter().filter(|c| c["thread"] == true && !archived(c)).cloned();
                Ok(json!({ "threads": active.collect::<Vec<_>>(), "members": [] }))
            }
            (Method::Get, ["channels", cid, "threads", "archived", "public"]) => {
                let old = st.channels.iter().filter(|c| c["thread"] == true && c["parent_id"] == *cid && archived(c));
                Ok(json!({ "threads": old.cloned().collect::<Vec<_>>(), "has_more": false }))
            }
            (Method::Post, ["channels", forum, "threads"]) => {
                let tid = self.new_id();
                let mut msg = body["message"].clone();
                msg["id"] = json!(tid);
                msg["channel_id"] = json!(tid);
                msg["author"] = json!({ "id": "42" });
                let thread = json!({
                    "id": tid, "type": 11, "parent_id": forum, "name": body["name"], "owner_id": "42",
                    "applied_tags": body["applied_tags"].clone(), "message": msg.clone(),
                    "thread_metadata": { "archived": false },
                });
                st.messages.push(msg);
                // Threads aren't listed among guild channels, but GET channels/{id} finds them.
                let mut stored = thread.clone();
                stored["thread"] = json!(true);
                st.channels.push(stored);
                Ok(thread)
            }
            (Method::Post, ["guilds", _, "scheduled-events"]) => {
                let mut e = body;
                e["id"] = json!(self.new_id());
                e["status"] = json!(1);
                st.events.push(e.clone());
                Ok(e)
            }
            (Method::Patch, ["guilds", _, "scheduled-events", eid]) => {
                let e = st.events.iter_mut().find(|e| e["id"] == *eid).ok_or_else(|| err(404, "Unknown Event"))?;
                for (k, v) in body.as_object().cloned().unwrap_or_default() {
                    e[k] = v;
                }
                Ok(e.clone())
            }
            (Method::Get, ["guilds", _, "scheduled-events", eid]) => st
                .events
                .iter()
                .find(|e| e["id"] == *eid)
                .cloned()
                .ok_or_else(|| err(404, "Unknown Guild Scheduled Event")),
            (m, p) => Err(err(404, &format!("fake has no route for {m:?} {}", p.join("/")))),
        }
    }
}

impl Default for FakeDiscord {
    fn default() -> Self {
        Self::new()
    }
}

/// A number in a path's query string.
fn query(path: &str, key: &str) -> Option<u64> {
    let q = path.split_once('?')?.1;
    q.split('&').find_map(|kv| kv.strip_prefix(key)?.strip_prefix('=')?.parse().ok())
}

fn archived(thread: &Value) -> bool {
    thread["thread_metadata"]["archived"] == true
}

/// Give forum tags without an id a fresh one, as Discord does.
fn with_tag_ids(c: &mut Value, next: &AtomicU64) {
    if let Some(tags) = c.get_mut("available_tags").and_then(Value::as_array_mut) {
        for t in tags {
            if t["id"].is_null() {
                t["id"] = json!(next.fetch_add(1, Ordering::Relaxed).to_string());
            }
        }
    }
}

impl Rest for FakeDiscord {
    async fn call(&self, method: Method, path: &str, mut body: Option<Value>) -> Result<Value> {
        self.requests.fetch_add(1, Ordering::Relaxed);
        // As `Http` sends it, so what's kept is what Discord would get.
        if let Some(b) = &mut body {
            crate::no_pings_unless_asked(b);
        }
        let call = format!("{method:?} {path}").to_uppercase();
        let fault = {
            let mut faults = self.faults.lock().unwrap_or_else(|e| e.into_inner());
            faults.iter().position(|f| call.starts_with(&f.call.to_uppercase())).map(|i| faults.remove(i))
        };
        if let Some(f) = &fault
            && !f.after
        {
            return Err(ApiError { status: f.status, message: "injected failure".into(), code: f.code }.into());
        }
        let r = self.route(method, path, body);
        if let Some(f) = fault {
            return Err(err(f.status, "injected failure after the change"));
        }
        // Threads live in `channels` for lookup, but Discord doesn't list them with the guild's.
        if let (Method::Get, Ok(Value::Array(list))) = (method, &r)
            && path.ends_with("/channels")
        {
            return Ok(Value::Array(list.iter().filter(|c| c["thread"].is_null()).cloned().collect()));
        }
        r
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn one_member_by_id() {
        let d = std::sync::Arc::new(FakeDiscord::new());
        d.join(7);
        let m = d.call(Method::Get, "guilds/1000/members/7", None).await.unwrap();
        assert_eq!(m["user"]["id"], "7");
        let e = d.call(Method::Get, "guilds/1000/members/8", None).await.unwrap_err();
        assert_eq!(ApiError::status_of(&e), Some(404));
    }
}

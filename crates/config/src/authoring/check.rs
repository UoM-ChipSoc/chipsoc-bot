//! `check`: validate a config directory without touching Discord.

use std::collections::BTreeSet;
use std::path::Path;

use anyhow::Result;
use chipsoc_model::{MAX_CONTENT, perms};

use crate::schema::{ChannelKind, ModAction, Overwrite, Peer, Snapshot};
use crate::{Config, public_channels, template};

/// Now, in Unix seconds (for dates that have passed).
fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

/// Discord's limits that matter here (and [`MAX_CONTENT`]).
const MAX_TAGS: usize = 20;
/// Tags on one forum post, and characters in its title.
const MAX_POST_TAGS: usize = 5;
const MAX_THREAD_TITLE: usize = 100;
const MIN_ONBOARDING_DEFAULTS: usize = 7;
const MAX_APPLICATION_QUESTIONS: usize = 3;
/// Boxes on one Discord form.
pub const MAX_FORM_FIELDS: usize = 5;

/// Button actions the bot handles (`apply:<slug>` too).
pub const BUTTON_ACTIONS: &[&str] = template::ButtonAction::NAMES;
/// Buttons the bot puts under a post (one row).
const MAX_BUTTONS: usize = 5;
/// Options in one Discord menu.
const MAX_MENU: usize = 25;
/// Characters in a menu option's label and description, and a form box's label.
const MAX_OPTION_TEXT: usize = 100;
const MAX_BOX_LABEL: usize = 45;
/// Join-flow keys go in Discord's custom ids, which hold 100 characters.
const MAX_JOIN_KEY: usize = 32;

#[derive(Debug, Default)]
pub struct Report {
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
}

impl Report {
    fn err(&mut self, s: impl Into<String>) {
        self.errors.push(s.into());
    }
    fn warn(&mut self, s: impl Into<String>) {
        self.warnings.push(s.into());
    }
}

/// Load and validate. A file that doesn't parse is reported, not returned as an error.
pub fn check(dir: &Path) -> Result<Report> {
    match Config::load(dir) {
        Ok(c) => Ok(check_config(&c)),
        Err(e) => Ok(Report { errors: vec![format!("{e:#}")], warnings: Vec::new() }),
    }
}

pub fn check_config(c: &Config) -> Report {
    let mut r = Report { warnings: c.notes.clone(), ..Default::default() };
    let keys = Keys::of(&c.desired);
    roles(&mut r, &c.desired);
    members(&mut r, c);
    structure(&mut r, &c.desired, &keys);
    for (key, from, to) in &c.renamed {
        r.warn(format!("channel `{key}`: Discord writes the name {from:?} as {to:?}, so that's the name it gets"));
    }
    server(&mut r, c, &keys);
    posts(&mut r, c, &keys);
    onboarding(&mut r, c, &keys);
    join(&mut r, c, &keys);
    automod(&mut r, c, &keys);
    projects(&mut r, c);
    bot(&mut r, c, &keys);
    forums(&mut r, c);
    federation(&mut r, c, &keys);
    r
}

struct Keys {
    roles: BTreeSet<String>,
    channels: BTreeSet<String>,
    categories: BTreeSet<String>,
}

impl Keys {
    fn of(s: &Snapshot) -> Self {
        Self {
            roles: s.roles.roles.iter().map(|r| r.key.clone()).collect(),
            channels: s.channels().map(|(c, _)| c.key.clone()).collect(),
            categories: s.categories.iter().map(|c| c.category.key.clone()).collect(),
        }
    }
    fn role(&self, r: &mut Report, owner: &str, k: &str) {
        if !self.roles.contains(k) {
            r.err(format!("{owner}: unknown role `{k}`"));
        }
    }
    fn channel(&self, r: &mut Report, owner: &str, k: &str) {
        if !self.channels.contains(k) {
            r.err(format!("{owner}: unknown channel `{k}`"));
        }
    }
}

fn duplicates<'a>(r: &mut Report, what: &str, keys: impl Iterator<Item = &'a str>) {
    let mut seen = BTreeSet::new();
    for k in keys {
        if !seen.insert(k) {
            r.err(format!("duplicate {what} `{k}`"));
        }
    }
}

/// Keys go in URLs and Discord custom ids: only [`crate::KEY_RULE`].
fn well_formed<'a>(r: &mut Report, what: &str, keys: impl Iterator<Item = &'a str>) {
    for k in keys.filter(|k| !crate::is_key(k)) {
        r.err(format!("{what} `{k}`: a key must be {}", crate::KEY_RULE));
    }
}

fn roles(r: &mut Report, s: &Snapshot) {
    duplicates(r, "role key", s.roles.roles.iter().map(|x| x.key.as_str()));
    well_formed(r, "role", s.roles.roles.iter().map(|x| x.key.as_str()));
    for k in s.holders.keys() {
        match s.role(k) {
            None => r.err(format!("members.toml: unknown role `{k}`")),
            Some(role) if role.managed => {
                r.err(format!("members.toml: `{k}` is Discord's own role; it can't be given"))
            }
            Some(_) => {}
        }
    }
    for role in &s.roles.roles {
        if let Err(e) = perms::bits(&role.permissions) {
            r.err(format!("role `{}`: {e}", role.key));
        }
        if let Some(c) = &role.color
            && u32::from_str_radix(c.trim_start_matches('#'), 16).is_err()
        {
            r.err(format!("role `{}`: bad colour {c:?} (use #rrggbb)", role.key));
        }
    }
}

/// Someone listed twice for the same role is almost always a copy-paste slip.
fn members(r: &mut Report, c: &Config) {
    for (role, ids) in &c.members {
        let mut seen = BTreeSet::new();
        for id in ids.iter().filter(|id| !seen.insert(**id)) {
            r.err(format!("members.toml: {id} is listed twice for `{role}`"));
        }
    }
    for p in &c.projects {
        let mut seen = BTreeSet::new();
        for id in p.leads.iter().filter(|id| !seen.insert(**id)) {
            r.err(format!("project `{}`: lead {id} is listed twice", p.slug));
        }
    }
}

fn overwrites(r: &mut Report, keys: &Keys, owner: &str, ows: &[Overwrite]) {
    for o in ows {
        match (&o.role, &o.member) {
            (Some(k), None) => keys.role(r, owner, k),
            (None, Some(_)) => {}
            _ => r.err(format!("{owner}: an overwrite needs exactly one of `role` or `member`")),
        }
        for list in [&o.allow, &o.deny] {
            if let Err(e) = perms::bits(list) {
                r.err(format!("{owner}: {e}"));
            }
        }
    }
}

fn structure(r: &mut Report, s: &Snapshot, keys: &Keys) {
    duplicates(r, "category key", s.categories.iter().map(|c| c.category.key.as_str()));
    duplicates(r, "channel key", s.channels().map(|(c, _)| c.key.as_str()));
    well_formed(r, "category", s.categories.iter().map(|c| c.category.key.as_str()));
    well_formed(r, "channel", s.channels().map(|(c, _)| c.key.as_str()));
    for cf in &s.categories {
        overwrites(r, keys, &format!("category `{}`", cf.category.key), &cf.category.overwrites);
    }
    for (ch, _) in s.channels() {
        let owner = format!("channel `{}`", ch.key);
        overwrites(r, keys, &owner, &ch.overwrites);
        if ch.tags.len() > MAX_TAGS {
            r.err(format!("{owner}: forums allow at most {MAX_TAGS} tags"));
        }
        if !ch.tags.is_empty() && !ch.kind.has_tags() {
            r.warn(format!("{owner}: tags only apply to forum and media channels"));
        }
        if ch.sort.is_some() && !ch.kind.has_tags() {
            r.warn(format!("{owner}: sort only applies to forum and media channels"));
        }
        if ch.user_limit.is_some() && !ch.kind.is_voice() {
            r.warn(format!("{owner}: user_limit only applies to voice channels"));
        }
    }
}

fn server(r: &mut Report, c: &Config, keys: &Keys) {
    let g = &c.desired.server.guild;
    for (label, key) in [
        ("system_channel", &g.system_channel),
        ("rules_channel", &g.rules_channel),
        ("updates_channel", &g.updates_channel),
        ("afk_channel", &g.afk_channel),
    ] {
        if let Some(k) = key {
            keys.channel(r, &format!("server.toml {label}"), k);
        }
    }
    for (label, file) in [("icon", &g.icon), ("banner", &g.banner)] {
        if let Some(f) = file
            && !crate::inside(&c.dir, f).is_ok_and(|p| p.exists())
        {
            r.err(format!("server.toml {label}: file `{f}` not found"));
        }
    }
}

fn posts(r: &mut Report, c: &Config, keys: &Keys) {
    let open: BTreeSet<String> =
        c.projects.iter().filter(|p| p.application.open && p.is_live()).map(|p| format!("apply:{}", p.slug)).collect();
    for (channel, body) in &c.desired.posts {
        let file = c.desired.channel(channel).and_then(|ch| ch.post.clone()).unwrap_or_default();
        post_body(r, c, keys, &open, &file, body);
    }
    threads(r, c, keys, &open);
}

/// Forums' `[[channel.thread]]` posts: a forum's, keyed and titled as
/// Discord allows, with tags the forum has, and text like any post's.
fn threads(r: &mut Report, c: &Config, keys: &Keys, open: &BTreeSet<String>) {
    for (ch, _) in c.desired.channels().filter(|(ch, _)| !ch.threads.is_empty()) {
        let owner = format!("channel `{}`", ch.key);
        if !ch.kind.has_tags() {
            r.err(format!("{owner}: `[[channel.thread]]` posts go in forum and media channels only"));
            continue;
        }
        duplicates(r, &format!("{owner} thread key"), ch.threads.iter().map(|t| t.key.as_str()));
        well_formed(r, &format!("{owner} thread"), ch.threads.iter().map(|t| t.key.as_str()));
        duplicates(r, &format!("{owner} thread title"), ch.threads.iter().map(|t| t.title.as_str()));
        for t in &ch.threads {
            let at = format!("{owner} thread `{}`", t.key);
            match t.title.trim().chars().count() {
                0 => r.err(format!("{at}: give it a `title`")),
                n if n > MAX_THREAD_TITLE => {
                    r.err(format!("{at}: the title is {n} characters; Discord allows {MAX_THREAD_TITLE}"))
                }
                _ => {}
            }
            if t.tags.len() > MAX_POST_TAGS {
                r.err(format!("{at}: a forum post takes at most {MAX_POST_TAGS} tags"));
            }
            let unknown: Vec<&str> = t
                .tags
                .iter()
                .filter(|n| !ch.tags.iter().any(|f| f.eq_ignore_ascii_case(n)))
                .map(String::as_str)
                .collect();
            if !unknown.is_empty() {
                r.err(format!("{at}: #{} has no tag {}; add it to the forum's `tags`", ch.key, unknown.join(", ")));
            }
            match c.desired.threads.get(&crate::schema::thread_key(&ch.key, &t.key)) {
                Some(body) => post_body(r, c, keys, open, &t.post, body),
                None => r.err(format!("{at}: post file `{}` not found", t.post)),
            }
        }
    }
}

/// One post's text: what it mentions exists, its buttons work, its parts fit.
fn post_body(r: &mut Report, c: &Config, keys: &Keys, open: &BTreeSet<String>, file: &str, body: &str) {
    for k in template::refs(body, '#') {
        keys.channel(r, file, &k);
    }
    for k in template::refs(body, '@') {
        keys.role(r, file, &k);
    }
    let actions = template::button_actions(body);
    if actions.len() > MAX_BUTTONS {
        r.err(format!("{file}: at most {MAX_BUTTONS} buttons fit under a post (it has {})", actions.len()));
    }
    let mut seen = BTreeSet::new();
    for a in actions.iter().filter(|a| !seen.insert(a.as_str())) {
        r.err(format!(
            "{file}: two `{a}` buttons; Discord needs each button on a post to differ (e.g. `join` and `join:update`)"
        ));
    }
    for a in actions {
        if a.split(':').next() == Some("join") && c.join.is_none() {
            r.err(format!("{file}: the `join` button needs a join.toml"));
        }
        use template::ButtonAction as B;
        let parsed = B::parse(&a);
        let form = matches!(parsed, Some(B::ProposeProject | B::ShareResource | B::SuggestFeed));
        let off = match parsed {
            Some(B::ProposeProject) => c.bot.ideas.is_none().then_some("[ideas]"),
            Some(B::ShareResource) => c.bot.resources.is_none().then_some("[resources]"),
            Some(B::SuggestFeed) => c.bot.feeds.is_none().then_some("[feeds]"),
            Some(B::Event) => c.bot.events.is_none().then_some("[events]"),
            _ => None,
        };
        if parsed == Some(B::Event) && c.bot.events.is_some() && !c.bot.event_suggestions() {
            r.warn(format!(
                "{file}: the `event` button works only for admins: members can suggest events with [review] in bot.toml and `suggest` left on"
            ));
        }
        let needs = off.or((form && c.bot.review.is_none()).then_some("[review]"));
        if let Some(section) = needs {
            r.warn(format!("{file}: the `{a}` button does nothing without {section} in bot.toml"));
        }
        let known = match parsed {
            Some(B::Apply(_)) => open.contains(&a),
            Some(_) => true,
            None => false,
        };
        if !known {
            r.err(format!(
                "{file}: unknown button action `{a}` (use {}, or apply:<project>)",
                BUTTON_ACTIONS.join(", ")
            ));
        }
    }
    parts(r, file, body, &template::split(body));
    for a in template::early_buttons(body) {
        r.err(format!(
            "{file}: the `{a}` button is in a part before the last; buttons go under the last part, so write it after the last `{}` line",
            template::PART_BREAK
        ));
    }
}

/// A long post's parts: not too many, and each fits in a message.
fn parts(r: &mut Report, file: &str, body: &str, parts: &[String]) {
    if parts.len() > template::MAX_PARTS {
        r.err(format!("{file}: {} parts, but a post takes at most {}", parts.len(), template::MAX_PARTS));
    }
    let breaks = body.lines().any(|l| l.trim() == template::PART_BREAK);
    for n in template::too_long(parts) {
        let which = if parts.len() > 1 || breaks { format!("part {n} is") } else { "it's".to_owned() };
        r.err(format!(
            "{file}: {which} longer than Discord's {MAX_CONTENT}-character limit (counting each mention at its longest) \
             with no blank line or heading to split it at; add one, or a `{}` line",
            template::PART_BREAK
        ));
    }
}

fn onboarding(r: &mut Report, c: &Config, keys: &Keys) {
    let Some(onb) = c.desired.settings.onboarding.as_ref().filter(|o| o.enabled) else { return };
    for k in &onb.default_channels {
        keys.channel(r, "onboarding.toml default_channels", k);
    }
    let defaults =
        if onb.default_channels.is_empty() { public_channels(&c.desired).len() } else { onb.default_channels.len() };
    if defaults < MIN_ONBOARDING_DEFAULTS {
        r.warn(format!("onboarding.toml: Discord needs at least {MIN_ONBOARDING_DEFAULTS} default channels"));
    }
    let leads: BTreeSet<String> = c.projects.iter().filter(|p| p.lead_role).map(|p| p.lead_role_key()).collect();
    for p in &onb.prompts {
        for o in &p.options {
            let owner = format!("onboarding option `{}`", o.title);
            if c.desired.holders.contains_key(&o.role) || leads.contains(&o.role) {
                r.err(format!(
                    "{owner}: `{}` is given by members.toml or project leads, so onboarding can't give it too (Discord and the config would fight over it)",
                    o.role
                ));
            }
            if !o.role.is_empty() && c.bot.admin_role.as_deref() == Some(o.role.as_str()) {
                r.err(format!("{owner}: `{}` is the admin role; anyone could pick it", o.role));
            } else if !o.role.is_empty() && c.bot.positions.iter().any(|p| p.role == o.role) {
                r.err(format!("{owner}: `{}` is a position admins appoint; anyone could pick it", o.role));
            } else if !o.role.is_empty() && c.desired.dangerous_role(&o.role) {
                r.err(format!("{owner}: `{}` has moderator or admin permissions; anyone could pick it", o.role));
            }
            if o.role.is_empty() && o.channels.is_empty() {
                r.err(format!("{owner}: give it a role or channels"));
            }
            if !o.role.is_empty() {
                keys.role(r, &owner, &o.role);
            }
            for k in &o.channels {
                keys.channel(r, &owner, k);
            }
        }
    }
}

/// The join flow: steps that lead somewhere and end, menus and forms that fit
/// Discord, and roles safe to hand to anyone who says so.
fn join(r: &mut Report, c: &Config, keys: &Keys) {
    use crate::join::{self as j, END};
    let Some(flow) = &c.join else { return };
    let file = "join.toml";
    if flow.steps.is_empty() {
        return r.err(format!("{file}: add at least one [[step]]"));
    }
    duplicates(r, "join step", flow.steps.iter().map(|s| s.key.as_str()));
    if let Some(done) = &flow.done_message {
        for k in template::refs(done, '#') {
            keys.channel(r, "join.toml done_message", &k);
        }
        for k in template::refs(done, '@') {
            keys.role(r, "join.toml done_message", &k);
        }
    }
    let key_ok = |k: &str| {
        !k.is_empty()
            && k.len() <= MAX_JOIN_KEY
            && k != END
            && k.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
    };
    let bad_key = |what: &str, k: &str| {
        format!(
            "{file}: {what} key `{k}` must be lower-case letters, digits, `-` or `_`, at most {MAX_JOIN_KEY} long, and not `{END}`"
        )
    };
    if let Some(s) = &flow.start
        && flow.step(s).is_none()
    {
        r.err(format!("{file}: start: no step `{s}`"));
    }
    let to = |r: &mut Report, owner: &str, next: &Option<String>| {
        if let Some(n) = next
            && n != END
            && flow.step(n).is_none()
        {
            r.err(format!("{owner}: next: no step `{n}` (use `{END}` to finish)"));
        }
    };
    let boxes = |r: &mut Report, owner: &str, fields: &[&crate::join::Field]| {
        if fields.len() > MAX_FORM_FIELDS {
            r.err(format!("{owner}: Discord forms hold {MAX_FORM_FIELDS} boxes, and this asks {}", fields.len()));
        }
        duplicates(r, &format!("box in {owner}:"), fields.iter().map(|f| f.key.as_str()));
        for f in fields {
            if !key_ok(&f.key) {
                r.err(bad_key("box", &f.key));
            }
            if f.label.chars().count() > MAX_BOX_LABEL {
                r.err(format!("{owner}: box `{}`: labels fit {MAX_BOX_LABEL} characters", f.key));
            }
            for (what, t) in [("help", &f.help), ("hint", &f.hint)] {
                if t.as_ref().is_some_and(|t| t.chars().count() > MAX_OPTION_TEXT) {
                    r.err(format!("{owner}: box `{}`: {what} fits {MAX_OPTION_TEXT} characters", f.key));
                }
            }
        }
    };
    for step in &flow.steps {
        let owner = format!("{file} step `{}`", step.key);
        if !key_ok(&step.key) {
            r.err(bad_key("step", &step.key));
        }
        if step.question.trim().is_empty() {
            r.err(format!("{owner}: give it a question"));
        }
        to(r, &owner, &step.next);
        match (step.options.is_empty(), step.fields.is_empty()) {
            (true, true) => r.err(format!("{owner}: give it options or fields")),
            (false, false) => r.err(format!("{owner}: options or fields, not both")),
            (true, false) => {
                if step.multi || step.optional {
                    r.err(format!("{owner}: `multi` and `optional` are for steps with options"));
                }
                boxes(r, &owner, &step.fields.iter().collect::<Vec<_>>());
            }
            (false, true) => {}
        }
        if step.options.len() > MAX_MENU {
            r.err(format!("{owner}: Discord menus hold {MAX_MENU} options"));
        }
        if step.optional && !step.multi && !step.options.is_empty() {
            r.err(format!("{owner}: only `multi` steps can be `optional`; add a \"None\" option instead"));
        }
        let option_keys: Vec<String> = step.options.iter().map(|o| o.key()).collect();
        duplicates(r, &format!("option in {owner}:"), option_keys.iter().map(String::as_str));
        if step.multi {
            boxes(r, &owner, &step.options.iter().flat_map(|o| &o.ask).collect::<Vec<_>>());
        }
        for o in &step.options {
            let owner = format!("{owner} option {:?}", o.label);
            if !key_ok(&o.key()) {
                r.err(bad_key("option", &o.key()));
            }
            if o.label.chars().count() > MAX_OPTION_TEXT
                || o.description.as_ref().is_some_and(|d| d.chars().count() > MAX_OPTION_TEXT)
            {
                r.err(format!("{owner}: labels and descriptions fit {MAX_OPTION_TEXT} characters"));
            }
            if step.multi && o.next.is_some() {
                r.err(format!("{owner}: options of a `multi` step can't choose the next step; set `next` on the step"));
            }
            to(r, &owner, &o.next);
            if !step.multi {
                boxes(r, &owner, &o.ask.iter().collect::<Vec<_>>());
            }
            for role in &o.roles {
                keys.role(r, &owner, role);
                if keys.roles.contains(role) {
                    join_role(r, c, &owner, role, true);
                }
            }
        }
    }
    if let Some(cy) = j::cycle(flow) {
        r.err(format!("{file}: the steps go round in a loop: {}", cy.join(" → ")));
    }
    for k in j::unreachable(flow) {
        r.err(format!("{file}: step `{k}` can't be reached from the first step"));
    }
    if let Some(a) = &flow.access_role {
        let owner = format!("{file} access_role");
        keys.role(r, &owner, a);
        if keys.roles.contains(a) {
            join_role(r, c, &owner, a, false);
        }
    }
    let with_button: Vec<&String> = c
        .desired
        .channels()
        .filter_map(|(ch, _)| ch.post.as_ref().map(|_| &ch.key))
        .filter(|k| c.desired.posts.get(*k).is_some_and(|b| template::button_actions(b).iter().any(|a| a == "join")))
        .collect();
    if flow.gate {
        if flow.access_role.is_none() {
            r.err(format!("{file}: `gate = true` needs an access_role to let people in"));
        }
        if c.desired.role("everyone").is_none() {
            r.err(format!(
                "{file}: `gate = true` takes View Channel from @everyone, so list the `everyone` role in roles.toml"
            ));
        }
        if c.desired.role("everyone").is_some_and(|e| e.permissions.iter().any(|p| p == "ADMINISTRATOR")) {
            r.err(format!("{file}: @everyone has Administrator, so the gate can't hide anything"));
        }
        if with_button.is_empty() {
            r.warn(format!("{file}: no post has a `{{button:join|…}}`, so new members can't start the flow"));
        }
        for k in with_button.into_iter().filter(|k| !flow.open_channels.contains(k)) {
            r.err(format!("{file}: #{k} has the join button, so add it to open_channels (else nobody new can see it)"));
        }
        if c.desired.settings.onboarding.as_ref().is_some_and(|o| o.enabled) {
            r.err("onboarding.toml: with the join gate on, set `enabled = false`: Discord's onboarding needs 7 channels everyone can see");
        }
    } else {
        if with_button.is_empty() {
            r.warn(format!(
                "{file}: no post has a `{{button:join|…}}`, so members can only find the flow through `/profile`"
            ));
        }
        if flow.grandfather {
            r.warn(format!("{file}: grandfather only matters with `gate = true`"));
        }
    }
    for k in &flow.open_channels {
        keys.channel(r, &format!("{file} open_channels"), k);
    }
    let yearly = flow.steps.iter().any(|s| s.yearly);
    match &flow.yearly_refresh {
        Some(d) if j::month_day(d).is_none() => {
            r.err(format!("{file}: yearly_refresh {d:?} isn't a date like \"09-15\""))
        }
        Some(_) if !yearly => r.warn(format!("{file}: yearly_refresh is set but no step is `yearly = true`")),
        None if yearly => {
            r.warn(format!("{file}: steps are `yearly` but yearly_refresh (a date like \"09-15\") isn't set"))
        }
        _ => {}
    }
    let fields: BTreeSet<&str> = flow
        .steps
        .iter()
        .flat_map(|s| s.fields.iter().chain(s.options.iter().flat_map(|o| &o.ask)))
        .map(|f| f.key.as_str())
        .collect();
    if fields.contains(j::SURNAME) && !fields.contains(j::NAME) {
        r.warn(format!("{file}: a `surname` box only sets the nickname together with a `name` box"));
    }
    if c.desired.settings.onboarding.as_ref().is_some_and(|o| !o.prompts.is_empty()) {
        r.err("onboarding.toml: join.toml replaces Discord's onboarding questions (which can't branch); remove the [[prompt]] entries, keep the rest");
    }
    if c.bot.profile.as_ref().is_some_and(|p| !p.company_roles.is_empty()) {
        r.warn("bot.toml profile.company_roles: with join.toml, ask where people work in a join step instead; it's ignored");
    }
}

/// A role the join flow hands to anyone who picks it (`option`), or gives at the end.
fn join_role(r: &mut Report, c: &Config, owner: &str, role: &str, option: bool) {
    let leads = c.projects.iter().any(|p| p.lead_role && p.lead_role_key() == role);
    let partner = c.federation.peers.iter().flat_map(|p| &p.role_map).any(|m| m.ours == role);
    if c.desired.role(role).is_some_and(|x| x.managed) || role == "everyone" {
        r.err(format!("{owner}: `{role}` can't be given (it's Discord's own)"));
    } else if c.bot.admin_role.as_deref() == Some(role) {
        r.err(format!("{owner}: `{role}` is the admin role"));
    } else if c.desired.holders.contains_key(role) || leads {
        r.err(format!(
            "{owner}: members.toml or project leads decide who has `{role}`; the join flow can't give it too"
        ));
    } else if c.admins_decide(role) {
        r.err(format!("{owner}: `{role}` is a position admins appoint; the join flow can't give it"));
    } else if c.desired.dangerous_role(role) {
        r.err(format!("{owner}: `{role}` has moderator or admin permissions"));
    } else if partner {
        r.err(format!(
            "{owner}: partner sync (federation.toml role_map) gives `{role}`; people can't just say they have it"
        ));
    } else if option && c.desired.sees_private(role) {
        r.err(format!("{owner}: `{role}` sees private channels; anyone could pick it"));
    }
}

fn automod(r: &mut Report, c: &Config, keys: &Keys) {
    duplicates(r, "automod rule key", c.desired.settings.automod.iter().map(|x| x.key.as_str()));
    for rule in &c.desired.settings.automod {
        let owner = format!("automod `{}`", rule.key);
        if let Some(ch) = &rule.alert_channel {
            keys.channel(r, &owner, ch);
        }
        if rule.actions.contains(&ModAction::Alert) && rule.alert_channel.is_none() {
            r.err(format!("{owner}: `alert` needs an alert_channel"));
        }
        if rule.actions.contains(&ModAction::Timeout) && rule.timeout_seconds.is_none() {
            r.err(format!("{owner}: `timeout` needs timeout_seconds"));
        }
        for k in &rule.exempt_roles {
            keys.role(r, &owner, k);
        }
        for k in &rule.exempt_channels {
            keys.channel(r, &owner, k);
        }
    }
}

fn projects(r: &mut Report, c: &Config) {
    duplicates(r, "project slug", c.projects.iter().map(|p| p.slug.as_str()));
    for p in &c.projects {
        if p.slug != crate::slug(&p.slug) {
            r.err(format!("project `{}`: slug must be lower-case letters, digits and dashes", p.slug));
        }
        if p.application.open && !p.team_role {
            r.warn(format!("project `{}` takes applications but has no team role to grant", p.slug));
        }
        if p.application.open && p.status != crate::ProjectStatus::Active {
            r.warn(format!(
                "project `{}` has applications open but is {}, so `/apply` doesn't offer it until it's active; close them or make it active",
                p.slug, p.status
            ));
        }
        if p.application.questions.len() > MAX_APPLICATION_QUESTIONS {
            r.err(format!("project `{}`: at most {MAX_APPLICATION_QUESTIONS} application questions", p.slug));
        }
        if let (Some(file), Some(brief)) = (&p.brief, c.desired.project_posts.get(&p.slug)) {
            // Channel mentions in the post's header at their longest too.
            let mut worst = c.desired.lock.clone();
            for ch in &p.channels {
                worst.channels.insert(p.channel_key(ch), crate::Snowflake(u64::MAX));
            }
            parts(r, file, brief, &crate::projects::parts(p, &worst, Some(brief)));
        }
    }
    if !c.projects.is_empty() && c.bot.projects.is_none() {
        r.warn("projects.toml has projects but bot.toml has no [projects] section, so they get no forum posts");
    }
    let Some(conf) = &c.bot.projects else { return };
    let forum = c.desired.channel(&conf.forum);
    // A project's tags go on its post only where the forum has them.
    if let Some(forum) = forum.filter(|f| f.kind.has_tags()) {
        for p in &c.projects {
            let missing: Vec<&str> = p
                .tags
                .iter()
                .filter(|t| !forum.tags.iter().any(|f| f.eq_ignore_ascii_case(t)))
                .map(String::as_str)
                .collect();
            if !missing.is_empty() {
                r.warn(format!(
                    "project `{}`: #{} has no tag {}, so its post goes without; add it to the forum's `tags`",
                    p.slug,
                    conf.forum,
                    missing.join(", ")
                ));
            }
        }
    }
    if let Some(pinned) = &conf.pinned {
        if !c.projects.iter().any(|p| &p.slug == pinned) {
            r.err(format!("bot.toml [projects] pinned: no project `{pinned}` in projects.toml"));
        }
        // Discord pins one post per forum.
        if forum.is_some_and(|f| f.post.is_some()) {
            r.err(format!(
                "bot.toml [projects] pinned: #{} has its own pinned post (`post = …`), and Discord pins only one post per forum; drop one",
                conf.forum
            ));
        }
    }
}

fn bot(r: &mut Report, c: &Config, keys: &Keys) {
    let b = &c.bot;
    for (setting, k) in b.channel_refs() {
        keys.channel(r, &format!("bot.toml {setting}"), k);
    }
    let kind_of = |k: &str| c.desired.channel(k).map(|ch| ch.kind);
    for (setting, k) in [
        ("help.forum", b.help.as_ref().map(|h| h.forum.as_str())),
        ("ideas.forum", b.ideas.as_ref().map(|h| h.forum.as_str())),
        ("projects.forum", b.projects.as_ref().map(|h| h.forum.as_str())),
        ("resources.forum", b.resources.as_ref().map(|h| h.forum.as_str())),
    ] {
        if let Some(k) = k
            && kind_of(k).is_some_and(|kind| kind != ChannelKind::Forum)
        {
            r.err(format!("bot.toml {setting}: `{k}` must be a forum channel"));
        }
    }
    if let Some(a) = &b.archive
        && !keys.categories.contains(a)
    {
        r.err(format!("bot.toml archive: unknown category `{a}`"));
    }
    if let Some(a) = &b.admin_role {
        keys.role(r, "bot.toml admin_role", a);
        let projects = c.projects.iter().flat_map(|p| [p.team_role_key(), p.lead_role_key()]);
        if projects.into_iter().any(|k| k == *a) {
            r.err(format!("bot.toml admin_role: `{a}` is a project role, which applications hand out"));
        }
    }
    duplicates(r, "position", b.positions.iter().map(|p| p.role.as_str()));
    for p in &b.positions {
        keys.role(r, "bot.toml position", &p.role);
        if b.admin_role.as_deref() == Some(p.role.as_str()) {
            r.err(format!(
                "bot.toml position `{}`: that's the admin role; make a committee role the position and set `admin = true`",
                p.role
            ));
        }
        if p.admin && b.admin_role.is_none() {
            r.err(format!("bot.toml position `{}`: `admin = true` needs admin_role set", p.role));
        }
        let room = MAX_FORM_FIELDS - usize::from(p.intro.is_some());
        if p.questions.is_empty() {
            r.err(format!("bot.toml position `{}`: give it at least one question", p.role));
        } else if p.questions.len() > room {
            r.warn(format!(
                "bot.toml position `{}`: Discord forms have {MAX_FORM_FIELDS} boxes, so only the first {room} questions are asked",
                p.role
            ));
        }
        match p.closes_at(&b.timezone) {
            Err(e) => r.err(format!("bot.toml position `{}`: {e}", p.role)),
            Ok(Some(at)) if p.open && at <= now() => r.warn(format!(
                "bot.toml position `{}`: applications closed on {}; move `closes` on to take more, or set `open = false`",
                p.role,
                p.closes_label().unwrap_or_default()
            )),
            Ok(_) => {}
        }
    }
    // `/volunteer` and `/apply` list what's open in one Discord menu; more wouldn't show.
    let open = b.positions.iter().filter(|p| p.open).count();
    if open > MAX_MENU {
        r.err(format!("bot.toml: {open} positions are open, but `/volunteer` can list only {MAX_MENU}; close some"));
    }
    let taking = c.projects.iter().filter(|p| p.application.open && p.is_live()).count();
    if taking > MAX_MENU {
        r.err(format!(
            "projects.toml: {taking} projects take applications, but `/apply` can list only {MAX_MENU}; close some"
        ));
    }
    if let Some(rec) = &b.recording {
        for k in &rec.who {
            keys.role(r, "bot.toml recording.who", k);
        }
        for p in rec.problems(&c.dir) {
            r.err(p);
        }
    }
    if let Some(p) = &b.profile {
        for k in &p.company_roles {
            keys.role(r, "bot.toml profile.company_roles", k);
        }
    }
    // An idea's area becomes a tag on its post; one that isn't a tag is dropped.
    if let (Some(i), Some(p)) = (&b.ideas, &b.projects)
        && let Some(forum) = c.desired.channel(&i.forum).filter(|f| !f.tags.is_empty())
    {
        let untagged: Vec<&str> = p
            .areas
            .iter()
            .filter(|a| !forum.tags.iter().any(|t| t.eq_ignore_ascii_case(a)))
            .map(String::as_str)
            .collect();
        if !untagged.is_empty() {
            let what = if untagged.len() == 1 { "isn't a tag" } else { "aren't tags" };
            r.warn(format!(
                "bot.toml [projects] areas: {} {what} on #{}, so ideas with them get no tag; make the two lists match",
                untagged.join(", "),
                i.forum
            ));
        }
    }
    if let Some(i) = &b.ideas {
        idea_form(r, c, i);
        if let Some(tag) = &i.closed_tag
            && let Some(forum) = c.desired.channel(&i.forum)
            && !forum.tags.iter().any(|t| t.eq_ignore_ascii_case(tag))
        {
            r.err(format!("bot.toml [ideas] closed_tag: `{tag}` isn't a tag on #{}; add it to its `tags`", i.forum));
        }
    }
    if b.name.is_empty() {
        r.warn("bot.toml: set `name` (used in the bot's messages)");
    }
    if b.review.is_none() {
        let forms = [
            (b.ideas.is_some(), "[ideas]", "/propose-project"),
            (b.resources.is_some(), "[resources]", "/share-resource"),
        ];
        for (_, section, cmd) in forms.into_iter().filter(|f| f.0) {
            r.warn(format!(
                "bot.toml {section}: what members send is reviewed before it's posted, so {cmd} needs [review]; until then it isn't offered"
            ));
        }
    }
}

/// `[ideas] questions` and `post`: a form Discord can show, that asks the
/// name, and a template that fills only what the form asks.
fn idea_form(r: &mut Report, c: &Config, i: &crate::bot::Ideas) {
    let at = "bot.toml [ideas] questions";
    if i.questions.len() > MAX_FORM_FIELDS {
        r.err(format!("{at}: Discord forms hold {MAX_FORM_FIELDS} boxes, and this asks {}", i.questions.len()));
    }
    duplicates(r, "[ideas] question key", i.questions.iter().map(|q| q.key.as_str()));
    for q in &i.questions {
        let ok = !q.key.is_empty() && q.key.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
        if !ok {
            r.err(format!(
                "{at}: `{}` needs a `key` of lowercase letters, digits and _ (it names the answer in `{{answer:<key>}}`)",
                q.question.ask
            ));
        }
        if q.question.ask.trim().is_empty() {
            r.err(format!("{at}: `{}` has no question (`ask`)", q.key));
        }
    }
    match i.questions.iter().find(|q| q.key == "name") {
        None => r.err(format!("{at}: ask `name` (the idea's name, its post's title)")),
        Some(q) if q.question.optional => r.err(format!("{at}: `name` can't be optional (it's the post's title)")),
        Some(_) => {}
    }
    let Some(path) = &i.post else { return };
    let text = crate::inside(&c.dir, path).ok().and_then(|p| std::fs::read_to_string(p).ok());
    let Some(text) = text else {
        return r.err(format!("bot.toml [ideas] post: file `{path}` not found"));
    };
    let keys: Vec<&str> = i.questions.iter().map(|q| q.key.as_str()).collect();
    let unknown = template::unknown_idea_placeholders(&text, &keys);
    if !unknown.is_empty() {
        r.err(format!(
            "{path}: nothing fills {}; use {{by}}, {{lead}} or {{answer:<key>}} with one of: {}",
            unknown.join(", "),
            keys.join(", ")
        ));
    }
}

/// Forums where @everyone can't start posts, so members post only through
/// a bot form: each needs that form and a pinned post with its button (else
/// a warning), and the bot must be able to post there (else an error).
fn forums(r: &mut Report, c: &Config) {
    let b = &c.bot;
    let reviewed = b.review.is_some();
    // Forum → the form members post there with, and its button actions.
    let mut forms: Vec<(&str, &str, [&str; 2])> = Vec::new();
    if let (Some(i), true) = (&b.ideas, reviewed) {
        forms.push((&i.forum, "/propose-project", ["idea", "propose-project"]));
    }
    if let (Some(x), true) = (&b.resources, reviewed) {
        forms.push((&x.forum, "/share-resource", ["resource", "share-resource"]));
    }
    if let (Some(f), true) = (&b.feeds, reviewed) {
        forms.push((&f.channel, "/suggest-feed", ["feed", "suggest-feed"]));
    }
    // Forums the bot fills by itself: project posts, feed items.
    let own: Vec<&str> = [b.projects.as_ref().map(|p| p.forum.as_str()), b.feeds.as_ref().map(|f| f.channel.as_str())]
        .into_iter()
        .flatten()
        .collect();
    for (ch, cat) in c.desired.channels().filter(|(ch, _)| ch.kind.has_tags()) {
        let ows = crate::schema::effective_overwrites(cat, ch);
        if !says(&ows, "everyone", false, "SEND_MESSAGES") {
            continue;
        }
        let key = ch.key.as_str();
        let form = forms.iter().find(|(k, ..)| *k == key);
        let post = c.desired.posts.get(key);
        match form {
            None if own.contains(&key) || !ch.threads.is_empty() => {}
            None => r.warn(format!(
                "#{key}: @everyone can't start posts here and no bot form posts into it, so members can only reply"
            )),
            Some((_, cmd, actions)) => {
                let buttons = post.map(|p| template::button_actions(p)).unwrap_or_default();
                if !buttons.iter().any(|a| actions.contains(&a.as_str())) {
                    r.warn(format!(
                        "#{key}: members post here only through {cmd}; give it a pinned post (`post = \"content/….md\"`) with a `{{button:{}|…}}` so they find it",
                        actions[0]
                    ));
                }
            }
        }
        let bot_posts = form.is_some() || own.contains(&key) || post.is_some() || !ch.threads.is_empty();
        if bot_posts && !bot_can_post(c, &ows) {
            r.err(format!(
                "#{key}: @everyone is denied Send Messages here and the bot's role isn't allowed it back, so the bot can't post; allow it on the bot's role (or give that role Administrator)"
            ));
        }
    }
}

/// Whether an overwrite for `role` allows (or denies) `perm`.
fn says(ows: &[Overwrite], role: &str, allow: bool, perm: &str) -> bool {
    ows.iter()
        .filter(|o| o.role.as_deref() == Some(role))
        .any(|o| if allow { &o.allow } else { &o.deny }.iter().any(|p| p == perm))
}

/// Whether the bot can see and post in a channel with these overwrites, as
/// Discord works it out: its role's permissions and @everyone's, then the
/// @everyone overwrite, then its role's. The bot's role is the first
/// Discord-managed role listed; without one (or @everyone) it can't be told.
fn bot_can_post(c: &Config, ows: &[Overwrite]) -> bool {
    let s = &c.desired;
    let (Some(bot), Some(everyone)) = (s.roles.roles.iter().find(|r| r.managed), s.role("everyone")) else {
        return true;
    };
    let bits = perms::bits_or_all;
    let mut have = bits(&bot.permissions) | bits(&everyone.permissions);
    if have & (1 << perms::ADMINISTRATOR) != 0 {
        return true;
    }
    for key in ["everyone", bot.key.as_str()] {
        for o in ows.iter().filter(|o| o.role.as_deref() == Some(key)) {
            have &= !bits(&o.deny);
            have |= bits(&o.allow);
        }
    }
    let need = (1 << perms::VIEW_CHANNEL) | bits(&["SEND_MESSAGES".to_owned()]);
    have & need == need
}

fn federation(r: &mut Report, c: &Config, keys: &Keys) {
    let fed = &c.federation;
    for k in fed.publish.iter().chain(fed.inbox.iter()) {
        keys.channel(r, "federation.toml", k);
    }
    let signed = fed.peers.iter().any(|p| {
        p.catalog || !p.send_roles.is_empty() || !p.role_map.is_empty() || !p.send.is_empty() || !p.accept.is_empty()
    });
    if signed && fed.name.is_empty() {
        r.err("federation.toml: set `name` (our id, as partners know us) to use signed sync");
    }
    if fed.peers.iter().any(|p| !p.role_map.is_empty() || !p.accept.is_empty() || p.keys().next().is_some())
        && fed.inbox.is_none()
    {
        r.warn("federation.toml: partners can't send to us without an `inbox` channel");
    }
    duplicates(r, "peer id", fed.peers.iter().filter(|p| !p.id.is_empty()).map(|p| p.id.as_str()));
    for p in &fed.peers {
        peer(r, c, keys, p);
    }
}

fn peer(r: &mut Report, c: &Config, keys: &Keys, p: &Peer) {
    let owner = format!("peer `{}`", p.name);
    for (field, var) in [("webhook_env", &p.webhook_env), ("share_webhook_env", &p.share_webhook_env)] {
        if var.contains("://") {
            r.err(format!("{owner}: {field} must name an environment variable, not hold the URL"));
        }
    }
    let signed = p.catalog || !p.send_roles.is_empty() || !p.send.is_empty();
    if signed && p.webhook_env.is_empty() {
        r.err(format!("{owner}: set webhook_env (their inbox) to send them signed sync"));
    }
    if !p.share.is_empty() && p.share_webhook_env.is_empty() {
        r.err(format!("{owner}: set share_webhook_env for the channels in `share`"));
    }
    for k in &p.share {
        keys.channel(r, &owner, k);
    }
    for k in p.send_roles.iter().chain(p.role_map.iter().map(|m| &m.ours)) {
        keys.role(r, &owner, k);
    }
    let admin = c.bot.admin_role.as_deref();
    for m in &p.role_map {
        // Unknown roles are reported above.
        if keys.roles.contains(&m.ours) && m.too_powerful(&c.desired, admin) {
            r.err(format!("{owner}: `{}` is too powerful to hand out on a partner's word", m.ours));
        }
    }
    if !p.role_map.is_empty() && (p.id.is_empty() || p.keys().next().is_none()) {
        r.err(format!("{owner}: role_map needs the peer's `id` and `public_key` (or `public_keys`)"));
    }
    shares(r, c, keys, p, &owner);
}

/// A peer's `send` and `accept`: kinds this version knows, landing in
/// channels that exist and suit them, with a review queue when they wait.
fn shares(r: &mut Report, c: &Config, keys: &Keys, p: &Peer, owner: &str) {
    for k in &p.send {
        if !crate::SEND_KINDS.contains(&k.as_str()) {
            r.err(format!("{owner}: `send` has `{k}`; it can list {}", crate::SEND_KINDS.join(", ")));
        }
    }
    if (!p.send.is_empty() || !p.accept.is_empty()) && (p.id.is_empty() || p.keys().next().is_none()) {
        r.err(format!("{owner}: `send` and `accept` need the peer's `id` and `public_key` (or `public_keys`)"));
    }
    for (kind, key) in p.accept.channels() {
        let Some(ch) = c.desired.channel(key) else {
            keys.channel(r, owner, key);
            continue;
        };
        let forum = ch.kind.has_tags();
        if matches!(kind, "resources" | "questions") && !forum {
            r.err(format!("{owner}: accept.{kind} must be a forum, and `{key}` isn't one"));
        }
        if kind == "announcements" && forum {
            r.err(format!("{owner}: accept.announcements must be a text or announcement channel, not a forum"));
        }
    }
    let reviewed = ["resources", "questions", "feeds", "announcements", "events"]
        .into_iter()
        .any(|k| p.accept.landing(k).is_some_and(|(_, review)| review || p.accept.edits_reviewed(k)));
    if reviewed && c.bot.review.is_none() {
        r.err(format!(
            "{owner}: what `accept` takes (or their edits of it) waits for review, but there's no review queue ([review] in bot.toml); set one up, or `review = false`"
        ));
    }
    if p.accept.landing("events").is_some() && c.bot.events.is_none() {
        r.warn(format!(
            "{owner}: accept.events takes their events, but events aren't set up here ([events] in bot.toml)"
        ));
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn dir() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        let p = d.path();
        fs::create_dir_all(p.join("categories")).unwrap();
        fs::write(p.join("server.toml"), "[guild]\nname = \"T\"\n").unwrap();
        fs::write(p.join("bot.toml"), "name = \"T\"\n").unwrap();
        fs::write(
            p.join("roles.toml"),
            "[[role]]\nkey = \"everyone\"\nname = \"@everyone\"\n\n[[role]]\nkey = \"member\"\nname = \"Member\"\n",
        )
        .unwrap();
        fs::write(
            p.join("categories/10-a.toml"),
            "[category]\nkey = \"a\"\nname = \"A\"\n\n[[channel]]\nkey = \"general\"\nname = \"general\"\n",
        )
        .unwrap();
        d
    }

    fn errors(d: &tempfile::TempDir) -> String {
        check(d.path()).unwrap().errors.join("\n")
    }

    fn warnings(d: &tempfile::TempDir) -> String {
        check(d.path()).unwrap().warnings.join("\n")
    }

    /// A forum only the bot starts posts in, and a bot role with `bot_perms`.
    fn bot_only(d: &tempfile::TempDir, bot_toml: &str, post: Option<&str>, bot_perms: &str) {
        let p = d.path();
        fs::write(p.join("bot.toml"), format!("name = \"T\"\n{bot_toml}")).unwrap();
        fs::write(
            p.join("roles.toml"),
            format!("[[role]]\nkey = \"bot\"\nname = \"Bot\"\nmanaged = true\npermissions = [{bot_perms}]\n\n[[role]]\nkey = \"everyone\"\nname = \"@everyone\"\npermissions = [\"VIEW_CHANNEL\", \"SEND_MESSAGES\"]\n"),
        )
        .unwrap();
        let post_line = if post.is_some() { "post = \"content/how.md\"\n" } else { "" };
        fs::write(
            p.join("categories/10-a.toml"),
            format!("[category]\nkey = \"a\"\nname = \"A\"\n\n[[channel]]\nkey = \"general\"\nname = \"general\"\n\n[[channel]]\nkey = \"ideas\"\ntype = \"forum\"\nname = \"ideas\"\n{post_line}[[channel.overwrites]]\nrole = \"everyone\"\ndeny = [\"SEND_MESSAGES\"]\nallow = [\"SEND_MESSAGES_IN_THREADS\"]\n"),
        )
        .unwrap();
        if let Some(body) = post {
            fs::create_dir_all(p.join("content")).unwrap();
            fs::write(p.join("content/how.md"), body).unwrap();
        }
    }

    #[test]
    fn an_idea_form_must_fit_ask_the_name_and_fill_its_template() {
        let d = dir();
        let set = |ideas: &str| {
            let toml = format!("name = \"T\"\n[review]\nchannel = \"general\"\n[ideas]\nforum = \"general\"\n{ideas}");
            fs::write(d.path().join("bot.toml"), toml).unwrap();
        };
        set("");
        assert!(!errors(&d).contains("[ideas]"), "{}", errors(&d));
        set("questions = [\"blurb\", \"a\", \"b\", \"c\", \"d\", \"e\"]");
        let e = errors(&d);
        assert!(e.contains("forms hold 5 boxes, and this asks 6") && e.contains("ask `name`"), "{e}");
        set(
            "questions = [{ key = \"name\", optional = true }, \"Budget?\", { key = \"budget\", ask = \"Again?\" }, \"Тема?\"]",
        );
        let e = errors(&d);
        assert!(e.contains("`name` can't be optional"), "{e}");
        assert!(e.contains("duplicate [ideas] question key `budget`"), "{e}");
        assert!(e.contains("`Тема?` needs a `key`"), "{e}");
        set("post = \"content/idea.md\"");
        assert!(errors(&d).contains("[ideas] post: file `content/idea.md` not found"), "{}", errors(&d));
        fs::create_dir_all(d.path().join("content")).unwrap();
        fs::write(d.path().join("content/idea.md"), "{by}: {answer:scope} {answer:budget} {lead}").unwrap();
        let e = errors(&d);
        assert!(e.contains("content/idea.md: nothing fills {answer:budget}"), "{e}");
        fs::write(d.path().join("content/idea.md"), "{by}: {answer:scope} {lead}").unwrap();
        assert!(!errors(&d).contains("idea.md"), "{}", errors(&d));
    }

    #[test]
    fn bot_only_forums_need_a_form_a_pinned_post_and_a_bot_that_can_post() {
        let d = dir();
        let ideas = "[review]\nchannel = \"general\"\n[ideas]\nforum = \"ideas\"\n";
        bot_only(&d, "", None, "\"ADMINISTRATOR\"");
        assert!(warnings(&d).contains("#ideas: @everyone can't start posts here and no bot form posts into it"));
        bot_only(&d, ideas, None, "\"ADMINISTRATOR\"");
        let w = warnings(&d);
        assert!(w.contains("#ideas: members post here only through /propose-project") && w.contains("{button:idea|…}"));
        let how = "Ideas go here, reviewed first.\n{button:idea|Propose an idea}\n";
        bot_only(&d, ideas, Some(how), "\"ADMINISTRATOR\"");
        assert!(!warnings(&d).contains("#ideas"), "{}", warnings(&d));
        assert_eq!(errors(&d), "");
        // The older action name does too.
        bot_only(&d, ideas, Some("{button:propose-project|Propose}\n"), "\"ADMINISTRATOR\"");
        assert!(!warnings(&d).contains("#ideas"), "{}", warnings(&d));

        // Without Administrator, @everyone's deny takes Send Messages from the bot too.
        bot_only(&d, ideas, Some(how), "\"SEND_MESSAGES\", \"VIEW_CHANNEL\"");
        assert!(
            errors(&d)
                .contains("#ideas: @everyone is denied Send Messages here and the bot's role isn't allowed it back")
        );
        let p = d.path();
        let cat = fs::read_to_string(p.join("categories/10-a.toml")).unwrap();
        fs::write(
            p.join("categories/10-a.toml"),
            format!("{cat}[[channel.overwrites]]\nrole = \"bot\"\nallow = [\"SEND_MESSAGES\"]\n"),
        )
        .unwrap();
        assert_eq!(errors(&d), "", "allowed back on the bot's role");
    }

    #[test]
    fn member_forms_need_review() {
        let d = dir();
        fs::write(d.path().join("bot.toml"), "name = \"T\"\n[resources]\nforum = \"general\"\n").unwrap();
        assert!(warnings(&d).contains(
            "bot.toml [resources]: what members send is reviewed before it's posted, so /share-resource needs [review]"
        ));
    }

    #[test]
    fn more_open_positions_than_a_menu_holds_is_an_error() {
        let d = dir();
        let p = d.path();
        let roles: String = (0..26).map(|n| format!("[[role]]\nkey = \"r{n}\"\nname = \"R{n}\"\n\n")).collect();
        let old = fs::read_to_string(p.join("roles.toml")).unwrap();
        fs::write(p.join("roles.toml"), format!("{old}\n{roles}")).unwrap();
        let positions: String =
            (0..26).map(|n| format!("[[position]]\nrole = \"r{n}\"\nquestions = [{{ ask = \"Why?\" }}]\n\n")).collect();
        let bot = fs::read_to_string(p.join("bot.toml")).unwrap();
        fs::write(p.join("bot.toml"), format!("{bot}\n[review]\nchannel = \"general\"\n\n{positions}")).unwrap();
        assert!(errors(&d).contains("26 positions are open, but `/volunteer` can list only 25"), "{}", errors(&d));
    }

    #[test]
    fn clean_config_passes() {
        let d = dir();
        assert_eq!(errors(&d), "");
    }

    #[test]
    fn parse_errors_and_typos_are_reported() {
        let d = dir();
        fs::write(d.path().join("roles.toml"), "[[role]]\nkey = \"m\"\nnaem = \"M\"\n").unwrap();
        assert!(errors(&d).contains("naem"), "{}", errors(&d));
    }

    #[test]
    fn federation_references_and_secrets() {
        let d = dir();
        fs::write(
            d.path().join("federation.toml"),
            "inbox = \"nope\"\n[[peer]]\nname = \"S\"\nwebhook_env = \"https://discord.com/api/webhooks/1/x\"\ncatalog = true\n[[peer.role_map]]\ntheirs = \"c\"\nours = \"ghost\"\n",
        )
        .unwrap();
        let e = errors(&d);
        for want in [
            "unknown channel `nope`",
            "set `name`",
            "not hold the URL",
            "unknown role `ghost`",
            "`id` and `public_key`",
        ] {
            assert!(e.contains(want), "missing {want:?} in {e}");
        }
        fs::write(
            d.path().join("federation.toml"),
            "name = \"us\"\ninbox = \"general\"\n[[peer]]\nname = \"S\"\nwebhook_env = \"PEER_S\"\nid = \"s\"\npublic_key = \"k\"\n[[peer.role_map]]\ntheirs = \"c\"\nours = \"member\"\n",
        )
        .unwrap();
        assert_eq!(errors(&d), "");
    }

    #[test]
    fn shared_content_kinds_and_landings_are_checked() {
        let d = dir();
        let p = d.path();
        fs::write(
            p.join("categories/10-a.toml"),
            "[category]\nkey = \"a\"\nname = \"A\"\n\n[[channel]]\nkey = \"general\"\nname = \"general\"\n\n[[channel]]\nkey = \"theirs\"\ntype = \"forum\"\nname = \"theirs\"\n",
        )
        .unwrap();
        let peer = "name = \"us\"\ninbox = \"general\"\n[[peer]]\nname = \"S\"\nid = \"s\"\npublic_key = \"k\"\nwebhook_env = \"PEER_S\"\n";
        let fed = |extra: &str| fs::write(p.join("federation.toml"), format!("{peer}{extra}")).unwrap();
        fed(
            "send = [\"resources\", \"polls\"]\naccept = { resources = \"general\", announcements = \"theirs\", questions = \"ghost\", events = true }\n",
        );
        let e = errors(&d);
        for want in [
            "`send` has `polls`",
            "accept.resources must be a forum",
            "accept.announcements must be a text",
            "unknown channel `ghost`",
            "no review queue",
        ] {
            assert!(e.contains(want), "missing {want:?} in {e}");
        }
        assert!(warnings(&d).contains("events aren't set up"), "{}", warnings(&d));
        fed(
            "send = [\"resources\", \"announcements\"]\n[peer.accept]\nresources = { to = \"theirs\", review = false }\nannouncements = { to = \"general\", review = false }\n",
        );
        assert_eq!(errors(&d), "");
        fed("[peer.accept]\nresources = { to = \"theirs\", review = false, edits = \"review\" }\n");
        assert!(errors(&d).contains("no review queue"), "their edits wait for review: {}", errors(&d));
        fed("[peer.accept]\nresources = { to = \"theirs\", edits = \"sometimes\" }\n");
        assert!(!errors(&d).is_empty(), "edits is review or apply");
        fed("accept = { resources = { to = \"theirs\", reveiw = false } }\n");
        assert!(!errors(&d).is_empty(), "a typo in a landing is refused");
    }

    #[test]
    fn partners_cannot_be_given_powerful_roles() {
        let d = dir();
        let p = d.path();
        fs::write(
            p.join("roles.toml"),
            "[[role]]\nkey = \"everyone\"\nname = \"@everyone\"\n\n[[role]]\nkey = \"mod\"\nname = \"Mod\"\npermissions = [\"KICK_MEMBERS\"]\n\n[[role]]\nkey = \"lead\"\nname = \"Lead\"\n\n[[role]]\nkey = \"friend\"\nname = \"Friend\"\n",
        )
        .unwrap();
        fs::write(
            p.join("categories/10-a.toml"),
            "[category]\nkey = \"a\"\nname = \"A\"\n\n[[channel]]\nkey = \"general\"\nname = \"general\"\n[[channel.overwrites]]\nrole = \"lead\"\nallow = [\"MANAGE_MESSAGES\"]\n[[channel.overwrites]]\nrole = \"friend\"\nallow = [\"SEND_MESSAGES\"]\ndeny = [\"MANAGE_MESSAGES\"]\n",
        )
        .unwrap();
        let peer = "name = \"us\"\ninbox = \"general\"\n[[peer]]\nname = \"S\"\nid = \"s\"\npublic_keys = [\"old\", \"new\"]\n";
        let map = |ours: &str| format!("{peer}[[peer.role_map]]\ntheirs = \"c\"\nours = \"{ours}\"\n");
        for (ours, powerful) in [("mod", true), ("lead", true), ("everyone", true), ("friend", false)] {
            fs::write(p.join("federation.toml"), map(ours)).unwrap();
            assert_eq!(errors(&d).contains("too powerful"), powerful, "{ours}: {}", errors(&d));
        }
        assert_eq!(errors(&d), "", "public_keys stands in for public_key");
    }

    #[test]
    fn onboarding_bot_and_buttons() {
        let d = dir();
        let p = d.path();
        fs::write(
            p.join("onboarding.toml"),
            "enabled = true\n[[prompt]]\ntitle = \"Q\"\n[[prompt.option]]\ntitle = \"x\"\nrole = \"ghost\"\n",
        )
        .unwrap();
        fs::write(p.join("bot.toml"), "name = \"T\"\nadmin_role = \"nobody\"\n[help]\nforum = \"general\"\n").unwrap();
        fs::create_dir_all(p.join("content")).unwrap();
        fs::write(p.join("content/hi.md"), "Hi {#ghost}\n{button:launch|Go}\n").unwrap();
        fs::write(
            p.join("categories/10-a.toml"),
            "[category]\nkey = \"a\"\nname = \"A\"\n\n[[channel]]\nkey = \"general\"\nname = \"general\"\npost = \"content/hi.md\"\n",
        )
        .unwrap();
        let r = check(p).unwrap();
        let e = r.errors.join("\n");
        for want in [
            "unknown role `ghost`",
            "unknown role `nobody`",
            "must be a forum",
            "unknown channel `ghost`",
            "unknown button action `launch`",
        ] {
            assert!(e.contains(want), "missing {want:?} in {e}");
        }
        assert!(r.warnings.iter().any(|w| w.contains("at least 7")));
    }

    /// Closing a project's applications mustn't break every plan.
    #[test]
    fn apply_button_for_a_closed_project_is_a_warning() {
        let d = dir();
        let p = d.path();
        fs::create_dir_all(p.join("content")).unwrap();
        fs::write(p.join("content/hi.md"), "Join us\n{button:apply:golem|Apply}\n{button:volunteer|Help}\n").unwrap();
        fs::write(
            p.join("categories/10-a.toml"),
            "[category]\nkey = \"a\"\nname = \"A\"\n\n[[channel]]\nkey = \"general\"\nname = \"general\"\npost = \"content/hi.md\"\n",
        )
        .unwrap();
        let project = |open: bool| {
            format!(
                "[[project]]\nslug = \"golem\"\nname = \"Golem\"\nstatus = \"active\"\n[project.application]\nopen = {open}\n"
            )
        };
        fs::write(p.join("projects.toml"), project(true)).unwrap();
        let c = Config::load(p).unwrap();
        assert!(c.desired.posts["general"].contains("apply:golem"));
        assert_eq!(errors(&d), "");

        fs::write(p.join("projects.toml"), project(false)).unwrap();
        let r = check(p).unwrap();
        assert!(r.errors.is_empty(), "{:?}", r.errors);
        assert!(r.warnings.iter().any(|w| w.contains("`apply:golem` button is left off")), "{:?}", r.warnings);
        let c = Config::load(p).unwrap();
        assert_eq!(c.desired.posts["general"], "Join us\n{button:volunteer|Help}\n", "sent without it");
        let schema = crate::ops::schema(p).unwrap();
        assert!(schema.posts["general"].body.contains("apply:golem"), "the editor still shows it");

        fs::write(p.join("content/hi.md"), "{button:apply:nope|Apply}\n").unwrap();
        assert!(errors(&d).contains("unknown button action `apply:nope`"));
    }

    #[test]
    fn position_forms_fit_discord() {
        let d = dir();
        let p = d.path();
        let bot = |extra: &str| format!("name = \"T\"\n[[position]]\nrole = \"member\"\n{extra}");
        fs::write(p.join("bot.toml"), bot("intro = \"Hi\"\nquestions = [\"a\", \"b\", \"c\", \"d\", \"e\"]")).unwrap();
        let r = check(p).unwrap();
        assert!(r.errors.is_empty() && r.warnings.iter().any(|w| w.contains("first 4")), "{r:?}");
        fs::write(p.join("bot.toml"), bot("questions = []")).unwrap();
        assert!(errors(&d).contains("at least one question"));
    }

    #[test]
    fn a_position_s_closing_date_is_checked() {
        let d = dir();
        let p = d.path();
        let bot = |closes: &str| format!("name = \"T\"\n[[position]]\nrole = \"member\"\ncloses = \"{closes}\"\n");
        fs::write(p.join("bot.toml"), bot("20/11/2026")).unwrap();
        assert!(errors(&d).contains("isn't a date"), "{}", errors(&d));
        fs::write(p.join("bot.toml"), bot("2020-01-31")).unwrap();
        let r = check(p).unwrap();
        assert!(r.errors.is_empty(), "{:?}", r.errors);
        assert!(r.warnings.iter().any(|w| w.contains("closed on Friday 31 January 2020")), "{:?}", r.warnings);
        fs::write(p.join("bot.toml"), bot("2999-01-31 18:00")).unwrap();
        let r = check(p).unwrap();
        assert!(r.errors.is_empty() && !r.warnings.iter().any(|w| w.contains("closed on")), "{r:?}");
        let c = Config::load(p).unwrap();
        let one = c.fill_lists("{positions}");
        assert!(one.contains(": applications close Thursday 31 January 2999, 18:00"), "{one}");
        // Several closing at once: said once, under the list.
        let two =
            format!("{}[[position]]\nrole = \"helper\"\ncloses = \"2999-01-31 18:00\"\n", bot("2999-01-31 18:00"));
        fs::write(p.join("bot.toml"), two).unwrap();
        let list = Config::load(p).unwrap().fill_lists("{positions}");
        assert_eq!(list.matches("close").count(), 1, "{list}");
        assert!(list.ends_with("\nApplications close Thursday 31 January 2999, 18:00."), "{list}");
        let apart = format!("{}[[position]]\nrole = \"helper\"\ncloses = \"2999-02-01\"\n", bot("2999-01-31 18:00"));
        fs::write(p.join("bot.toml"), apart).unwrap();
        let list = Config::load(p).unwrap().fill_lists("{positions}");
        assert_eq!(list.matches("applications close").count(), 2, "{list}");
    }

    #[test]
    fn onboarding_cannot_give_managed_roles() {
        let d = dir();
        let p = d.path();
        let option = |role: &str| {
            format!("enabled = true\n[[prompt]]\ntitle = \"Q\"\n[[prompt.option]]\ntitle = \"x\"\nrole = \"{role}\"\n")
        };
        fs::write(p.join("onboarding.toml"), option("member")).unwrap();
        assert!(!errors(&d).contains("fight"), "{}", errors(&d));
        fs::write(p.join("members.toml"), "member = [\"7\"]\n").unwrap();
        assert!(errors(&d).contains("fight"), "{}", errors(&d));
        fs::remove_file(p.join("members.toml")).unwrap();
        fs::write(p.join("projects.toml"), "[[project]]\nslug = \"golem\"\nname = \"Golem\"\nstatus = \"active\"\n")
            .unwrap();
        fs::write(p.join("onboarding.toml"), option("proj-golem-lead")).unwrap();
        assert!(errors(&d).contains("fight"), "{}", errors(&d));
    }

    #[test]
    fn partners_cannot_be_given_roles_powerful_by_bit_or_by_access() {
        let d = dir();
        let p = d.path();
        fs::write(
            p.join("roles.toml"),
            "[[role]]\nkey = \"everyone\"\nname = \"@everyone\"\n\n[[role]]\nkey = \"bits\"\nname = \"B\"\npermissions = [\"BIT_3\"]\n\n[[role]]\nkey = \"dj\"\nname = \"DJ\"\npermissions = [\"MOVE_MEMBERS\"]\n\n[[role]]\nkey = \"insider\"\nname = \"I\"\n\n[[role]]\nkey = \"chair\"\nname = \"C\"\n\n[[role]]\nkey = \"friend\"\nname = \"F\"\n",
        )
        .unwrap();
        fs::write(
            p.join("categories/20-private.toml"),
            "[category]\nkey = \"private\"\nname = \"P\"\n[[category.overwrites]]\nrole = \"everyone\"\ndeny = [\"VIEW_CHANNEL\"]\n[[category.overwrites]]\nrole = \"insider\"\nallow = [\"VIEW_CHANNEL\"]\n\n[[channel]]\nkey = \"committee\"\nname = \"committee\"\n",
        )
        .unwrap();
        fs::write(p.join("members.toml"), "chair = [\"7\"]\n").unwrap();
        let peer = "name = \"us\"\ninbox = \"general\"\n[[peer]]\nname = \"S\"\nid = \"s\"\npublic_key = \"k\"\n";
        let map = |ours: &str| format!("{peer}[[peer.role_map]]\ntheirs = \"c\"\nours = \"{ours}\"\n");
        for (ours, powerful) in [("bits", true), ("dj", true), ("insider", true), ("chair", true), ("friend", false)] {
            fs::write(p.join("federation.toml"), map(ours)).unwrap();
            assert_eq!(errors(&d).contains("too powerful"), powerful, "{ours}: {}", errors(&d));
        }
    }

    #[test]
    fn members_are_listed_once() {
        let d = dir();
        fs::write(d.path().join("members.toml"), "member = [\"7\", \"8\", \"7\"]\n").unwrap();
        assert!(errors(&d).contains("7 is listed twice for `member`"), "{}", errors(&d));
    }

    #[test]
    fn posts_fit_with_the_longest_mentions() {
        let d = dir();
        let p = d.path();
        fs::create_dir_all(p.join("content")).unwrap();
        // 1950 characters, plus a mention that's 10 written but 23 at its longest (`<@&` + 20 digits + `>`).
        fs::write(p.join("content/hi.md"), format!("{}{{@member}}", "x".repeat(1980))).unwrap();
        fs::write(
            p.join("categories/10-a.toml"),
            "[category]\nkey = \"a\"\nname = \"A\"\n\n[[channel]]\nkey = \"general\"\nname = \"general\"\npost = \"content/hi.md\"\n",
        )
        .unwrap();
        assert!(errors(&d).contains("2000-character limit"), "{}", errors(&d));
    }

    #[test]
    fn long_posts_and_briefs_split_into_parts_that_fit() {
        let d = dir();
        let p = d.path();
        fs::create_dir_all(p.join("content")).unwrap();
        fs::write(
            p.join("categories/10-a.toml"),
            "[category]\nkey = \"a\"\nname = \"A\"\n\n[[channel]]\nkey = \"general\"\nname = \"general\"\npost = \"content/hi.md\"\n",
        )
        .unwrap();
        let para = "word ".repeat(300); // 1500 characters
        // Paragraphs split at blank lines: fine.
        fs::write(p.join("content/hi.md"), format!("{para}\n\n{para}\n\n{para}\n")).unwrap();
        assert_eq!(errors(&d), "");
        // One part with nowhere to split is named.
        fs::write(p.join("content/hi.md"), format!("{para}\n<!-- part -->\n{para}{para}\n")).unwrap();
        assert!(
            errors(&d).contains("content/hi.md: part 2 is longer than Discord's 2000-character limit"),
            "{}",
            errors(&d)
        );
        // Too many parts.
        fs::write(p.join("content/hi.md"), "x\n<!-- part -->\n".repeat(11)).unwrap();
        assert!(errors(&d).contains("11 parts, but a post takes at most 10"), "{}", errors(&d));
        // Buttons only under the last part.
        fs::write(p.join("content/hi.md"), "One\n{button:profile|Profile}\n<!-- part -->\nTwo\n").unwrap();
        assert!(errors(&d).contains("the `profile` button is in a part before the last"), "{}", errors(&d));
        fs::write(p.join("content/hi.md"), "One\n<!-- part -->\nTwo\n{button:profile|Profile}\n").unwrap();
        assert_eq!(errors(&d), "");

        // A project's brief: the same, with the post's header counted in part 1.
        fs::write(p.join("bot.toml"), "name = \"T\"\n[projects]\nforum = \"general\"\n").unwrap();
        fs::write(p.join("content/hi.md"), "Hi\n").unwrap();
        fs::write(
            p.join("projects.toml"),
            "[[project]]\nslug = \"golem\"\nname = \"Golem\"\nstatus = \"proposed\"\nbrief = \"content/golem.md\"\n",
        )
        .unwrap();
        fs::write(p.join("content/golem.md"), format!("{para}\n\n{para}\n")).unwrap();
        assert!(!errors(&d).contains("golem.md"), "{}", errors(&d));
        fs::write(p.join("content/golem.md"), format!("{para}{para}\n")).unwrap();
        assert!(errors(&d).contains("content/golem.md: part 2 is longer than"), "{}", errors(&d));
    }

    #[test]
    fn the_admin_role_is_never_handed_out_by_forms() {
        let d = dir();
        let p = d.path();
        fs::write(p.join("bot.toml"), "name = \"T\"\nadmin_role = \"member\"\n[[position]]\nrole = \"member\"\n")
            .unwrap();
        assert!(errors(&d).contains("that's the admin role"), "{}", errors(&d));
        fs::write(p.join("bot.toml"), "name = \"T\"\nadmin_role = \"member\"\n").unwrap();
        fs::write(
            p.join("onboarding.toml"),
            "enabled = true\n[[prompt]]\ntitle = \"Q\"\n[[prompt.option]]\ntitle = \"x\"\nrole = \"member\"\n",
        )
        .unwrap();
        assert!(errors(&d).contains("is the admin role; anyone could pick it"), "{}", errors(&d));
    }

    #[test]
    fn onboarding_cannot_give_moderator_roles() {
        let d = dir();
        let p = d.path();
        fs::write(
            p.join("roles.toml"),
            "[[role]]\nkey = \"everyone\"\nname = \"@everyone\"\n\n[[role]]\nkey = \"mod\"\nname = \"Mod\"\npermissions = [\"BIT_40\"]\n",
        )
        .unwrap();
        fs::write(
            p.join("onboarding.toml"),
            "enabled = true\n[[prompt]]\ntitle = \"Q\"\n[[prompt.option]]\ntitle = \"x\"\nrole = \"mod\"\n",
        )
        .unwrap();
        assert!(errors(&d).contains("moderator or admin permissions"), "{}", errors(&d));
    }

    fn join_dir() -> tempfile::TempDir {
        let d = dir();
        let p = d.path();
        fs::write(
            p.join("roles.toml"),
            "[[role]]\nkey = \"everyone\"\nname = \"@everyone\"\npermissions = [\"VIEW_CHANNEL\"]\n\n\
             [[role]]\nkey = \"member\"\nname = \"Member\"\n\n[[role]]\nkey = \"access\"\nname = \"Access\"\n\n\
             [[role]]\nkey = \"mod\"\nname = \"Mod\"\npermissions = [\"BAN_MEMBERS\"]\n\n\
             [[role]]\nkey = \"chair\"\nname = \"Chair\"\n\n[[role]]\nkey = \"partner\"\nname = \"Partner\"\n",
        )
        .unwrap();
        fs::create_dir_all(p.join("content")).unwrap();
        fs::write(p.join("content/welcome.md"), "Hi\n{button:join|Get started}\n").unwrap();
        fs::write(
            p.join("categories/10-a.toml"),
            "[category]\nkey = \"a\"\nname = \"A\"\n\n[[channel]]\nkey = \"welcome\"\nname = \"welcome\"\npost = \"content/welcome.md\"\n\n[[channel]]\nkey = \"general\"\nname = \"general\"\n",
        )
        .unwrap();
        d
    }

    const FLOW: &str = "access_role = \"access\"\n\
        [[step]]\nkey = \"uni\"\nquestion = \"Where?\"\n\
        [[step.option]]\nlabel = \"Here\"\nroles = [\"member\"]\nnext = \"name\"\n\
        [[step.option]]\nlabel = \"Elsewhere\"\n[[step.option.ask]]\nkey = \"uni\"\nlabel = \"Which?\"\n\
        [[step]]\nkey = \"name\"\nquestion = \"Name?\"\n[[step.field]]\nkey = \"name\"\nlabel = \"Name\"\n";

    #[test]
    fn a_join_flow_passes() {
        let d = join_dir();
        let p = d.path();
        fs::write(p.join("join.toml"), FLOW).unwrap();
        let r = check(p).unwrap();
        assert!(r.errors.is_empty(), "{:?}", r.errors);
        let c = Config::load(p).unwrap();
        let onb = c.desired.settings.onboarding.as_ref().unwrap();
        assert!(!onb.enabled && onb.prompts.is_empty(), "Discord's questions are turned off");
    }

    #[test]
    fn join_steps_must_lead_somewhere_and_end() {
        let d = join_dir();
        let p = d.path();
        let flow = FLOW.replace("next = \"name\"", "next = \"nowhere\"");
        fs::write(p.join("join.toml"), flow).unwrap();
        let e = errors(&d);
        assert!(e.contains("no step `nowhere`") && e.contains("`name` can't be reached"), "{e}");
        let looped = FLOW.replace("question = \"Name?\"", "question = \"Name?\"\nnext = \"uni\"");
        fs::write(p.join("join.toml"), looped).unwrap();
        assert!(errors(&d).contains("loop: uni → name → uni"), "{}", errors(&d));
        let both = format!("{FLOW}[[step.option]]\nlabel = \"x\"\n");
        fs::write(p.join("join.toml"), both).unwrap();
        assert!(errors(&d).contains("options or fields, not both"), "{}", errors(&d));
        let many: String = (0..26).map(|n| format!("[[step.option]]\nlabel = \"o{n}\"\n")).collect();
        fs::write(p.join("join.toml"), format!("[[step]]\nkey = \"s\"\nquestion = \"Q\"\n{many}")).unwrap();
        assert!(errors(&d).contains("hold 25 options"), "{}", errors(&d));
        let boxes: String = (0..6).map(|n| format!("[[step.field]]\nkey = \"f{n}\"\nlabel = \"F\"\n")).collect();
        fs::write(p.join("join.toml"), format!("[[step]]\nkey = \"s\"\nquestion = \"Q\"\n{boxes}")).unwrap();
        assert!(errors(&d).contains("hold 5 boxes"), "{}", errors(&d));
    }

    #[test]
    fn join_options_cannot_give_powerful_or_managed_roles() {
        let d = join_dir();
        let p = d.path();
        fs::write(p.join("members.toml"), "chair = [\"7\"]\n").unwrap();
        fs::write(p.join("bot.toml"), "name = \"T\"\nadmin_role = \"chair\"\n").unwrap();
        fs::write(
            p.join("federation.toml"),
            "name = \"us\"\ninbox = \"general\"\n[[peer]]\nname = \"S\"\nid = \"s\"\npublic_key = \"k\"\n[[peer.role_map]]\ntheirs = \"c\"\nours = \"partner\"\n",
        )
        .unwrap();
        for (role, want) in [
            ("ghost", "unknown role `ghost`"),
            ("mod", "moderator or admin permissions"),
            ("chair", "admin role"),
            ("partner", "partner sync"),
            ("everyone", "Discord's own"),
        ] {
            fs::write(p.join("join.toml"), FLOW.replace("roles = [\"member\"]", &format!("roles = [\"{role}\"]")))
                .unwrap();
            assert!(errors(&d).contains(want), "{role}: {}", errors(&d));
        }
        fs::write(p.join("bot.toml"), "name = \"T\"\n").unwrap();
        fs::write(p.join("join.toml"), FLOW.replace("roles = [\"member\"]", "roles = [\"chair\"]")).unwrap();
        assert!(errors(&d).contains("members.toml or project leads"), "{}", errors(&d));
    }

    /// A position's role is the admins' to give, listed in members.toml or not
    /// (nor as the access role everyone who finishes gets).
    #[test]
    fn join_and_onboarding_cannot_give_positions() {
        let d = join_dir();
        let p = d.path();
        fs::write(p.join("bot.toml"), "name = \"T\"\n[[position]]\nrole = \"chair\"\nquestions = [\"Why?\"]\n")
            .unwrap();
        fs::write(p.join("join.toml"), FLOW.replace("roles = [\"member\"]", "roles = [\"chair\"]")).unwrap();
        assert!(errors(&d).contains("a position admins appoint"), "{}", errors(&d));
        fs::write(p.join("join.toml"), FLOW.replace("access_role = \"access\"", "access_role = \"chair\"")).unwrap();
        assert!(errors(&d).contains("a position admins appoint"), "{}", errors(&d));
        fs::remove_file(p.join("join.toml")).unwrap();
        fs::write(
            p.join("onboarding.toml"),
            "enabled = true\n[[prompt]]\ntitle = \"Q\"\n[[prompt.option]]\ntitle = \"x\"\nrole = \"chair\"\n",
        )
        .unwrap();
        assert!(errors(&d).contains("a position admins appoint; anyone could pick it"), "{}", errors(&d));
    }

    /// An unknown permission name is an error, and everything that reads one
    /// anyway treats it as every permission: too powerful to hand out.
    #[test]
    fn unknown_permissions_fail_closed() {
        let d = join_dir();
        let p = d.path();
        let roles = fs::read_to_string(p.join("roles.toml")).unwrap();
        let typo = "key = \"member\"\nname = \"Member\"\npermissions = [\"SEND_MESAGES\"]\n";
        fs::write(p.join("roles.toml"), roles.replace("key = \"member\"\nname = \"Member\"\n", typo)).unwrap();
        fs::write(p.join("join.toml"), FLOW).unwrap();
        let e = errors(&d);
        assert!(e.contains("role `member`: unknown permission \"SEND_MESAGES\""), "{e}");
        assert!(e.contains("`member` has moderator or admin permissions"), "{e}");
        let c = Config::load(p).unwrap();
        assert!(c.desired.dangerous_role("member"));
        assert_eq!(c.desired.power_bits("member"), u64::MAX);
    }

    #[test]
    fn a_flow_without_a_join_button_is_said_whatever_the_gate() {
        let d = join_dir();
        let p = d.path();
        fs::write(p.join("join.toml"), FLOW).unwrap();
        fs::write(p.join("content/welcome.md"), "Hi\n").unwrap();
        assert!(warnings(&d).contains("only find the flow through `/profile`"), "{}", warnings(&d));
    }

    /// A `projects` forum with tags (and `extra` on it), and a project `golem` with `tags`.
    fn projects_forum(d: &tempfile::TempDir, extra: &str, tags: &str) {
        let p = d.path();
        fs::write(
            p.join("categories/20-p.toml"),
            format!(
                "[category]\nkey = \"p\"\nname = \"P\"\n\n[[channel]]\nkey = \"projects\"\nname = \"projects\"\ntype = \"forum\"\ntags = [\"CPU\", \"tooling\"]\n{extra}"
            ),
        )
        .unwrap();
        fs::write(
            p.join("projects.toml"),
            format!("[[project]]\nslug = \"golem\"\nname = \"Golem\"\nblurb = \"a CPU\"\ntags = {tags}\n"),
        )
        .unwrap();
    }

    #[test]
    fn the_pinned_project_must_exist_and_have_the_forums_pin_to_itself() {
        let d = dir();
        let p = d.path();
        projects_forum(&d, "", "[\"cpu\"]");
        let bot = "name = \"T\"\n[projects]\nforum = \"projects\"\n";
        fs::write(p.join("bot.toml"), format!("{bot}pinned = \"golem\"\n")).unwrap();
        assert_eq!(errors(&d), "");
        fs::write(p.join("bot.toml"), format!("{bot}pinned = \"robot\"\n")).unwrap();
        assert!(errors(&d).contains("pinned: no project `robot`"), "{}", errors(&d));
        fs::write(p.join("content.md"), "How to post.\n").unwrap();
        projects_forum(&d, "post = \"content.md\"\n", "[\"cpu\"]");
        fs::write(p.join("bot.toml"), format!("{bot}pinned = \"golem\"\n")).unwrap();
        assert!(errors(&d).contains("Discord pins only one post per forum"), "{}", errors(&d));
    }

    #[test]
    fn project_tags_the_forum_lacks_are_said() {
        let d = dir();
        projects_forum(&d, "", "[\"cpu\", \"fpga\"]");
        fs::write(d.path().join("bot.toml"), "name = \"T\"\n[projects]\nforum = \"projects\"\n").unwrap();
        assert!(warnings(&d).contains("project `golem`: #projects has no tag fpga"), "{}", warnings(&d));
        assert!(!warnings(&d).contains("no tag cpu"), "matched whatever the case: {}", warnings(&d));
    }

    #[test]
    fn forum_threads_are_checked_like_posts() {
        let d = dir();
        let p = d.path();
        fs::write(p.join("toolchain.md"), "Tools in {#nowhere}.\n").unwrap();
        let thread = |title: &str, tags: &str| {
            format!(
                "[[channel.thread]]\nkey = \"toolchain\"\ntitle = \"{title}\"\npost = \"toolchain.md\"\ntags = {tags}\n"
            )
        };
        projects_forum(&d, &thread("Toolchain", "[\"tooling\"]"), "[]");
        assert_eq!(errors(&d), "toolchain.md: unknown channel `nowhere`");
        fs::write(p.join("toolchain.md"), "Tools.\n").unwrap();
        assert_eq!(errors(&d), "");
        projects_forum(&d, &thread(&"x".repeat(101), "[\"tooling\", \"asic\"]"), "[]");
        let e = errors(&d);
        assert!(e.contains("the title is 101 characters; Discord allows 100"), "{e}");
        assert!(e.contains("#projects has no tag asic"), "{e}");
        let cat = fs::read_to_string(p.join("categories/10-a.toml")).unwrap();
        fs::write(
            p.join("categories/10-a.toml"),
            format!("{cat}\n[[channel]]\nkey = \"chat\"\nname = \"chat\"\n{}", thread("T", "[]")),
        )
        .unwrap();
        assert!(
            errors(&d).contains("channel `chat`: `[[channel.thread]]` posts go in forum and media channels only"),
            "{}",
            errors(&d)
        );
    }

    #[test]
    fn idea_areas_that_arent_forum_tags_are_said() {
        let d = dir();
        let p = d.path();
        let cat = fs::read_to_string(p.join("categories/10-a.toml")).unwrap();
        fs::write(
            p.join("categories/10-a.toml"),
            format!("{cat}\n[[channel]]\nkey = \"ideas\"\nname = \"ideas\"\ntype = \"forum\"\ntags = [\"CPU\", \"tooling\"]\n"),
        )
        .unwrap();
        let bot = "name = \"T\"\n[review]\nchannel = \"general\"\n[ideas]\nforum = \"ideas\"\n[projects]\nforum = \"ideas\"\n";
        fs::write(p.join("bot.toml"), format!("{bot}areas = [\"cpu\", \"ai\"]\n")).unwrap();
        assert!(warnings(&d).contains("areas: ai isn't a tag on #ideas"), "{}", warnings(&d));
        fs::write(p.join("bot.toml"), format!("{bot}areas = [\"cpu\", \"tooling\"]\n")).unwrap();
        assert!(!warnings(&d).contains("a tag on"), "{}", warnings(&d));
    }

    #[test]
    fn an_ideas_closed_tag_must_be_the_forums() {
        let d = dir();
        let p = d.path();
        let cat = fs::read_to_string(p.join("categories/10-a.toml")).unwrap();
        fs::write(
            p.join("categories/10-a.toml"),
            format!("{cat}\n[[channel]]\nkey = \"ideas\"\nname = \"ideas\"\ntype = \"forum\"\ntags = [\"Closed\"]\n"),
        )
        .unwrap();
        let bot = "name = \"T\"\n[review]\nchannel = \"general\"\n[ideas]\nforum = \"ideas\"\n";
        fs::write(p.join("bot.toml"), format!("{bot}closed_tag = \"done\"\n")).unwrap();
        assert!(errors(&d).contains("closed_tag: `done` isn't a tag on #ideas"), "{}", errors(&d));
        fs::write(p.join("bot.toml"), format!("{bot}closed_tag = \"closed\"\n")).unwrap();
        assert_eq!(errors(&d), "");
    }

    #[test]
    fn the_join_flow_replaces_discords_questions() {
        let d = join_dir();
        let p = d.path();
        fs::write(p.join("join.toml"), FLOW).unwrap();
        fs::write(
            p.join("onboarding.toml"),
            "enabled = true\n[[prompt]]\ntitle = \"Q\"\n[[prompt.option]]\ntitle = \"x\"\nrole = \"member\"\n",
        )
        .unwrap();
        assert!(errors(&d).contains("remove the [[prompt]] entries"), "{}", errors(&d));
        fs::write(p.join("onboarding.toml"), "enabled = true\n").unwrap();
        assert_eq!(errors(&d), "");
        fs::write(p.join("join.toml"), format!("gate = true\nopen_channels = [\"welcome\"]\n{FLOW}")).unwrap();
        assert!(errors(&d).contains("set `enabled = false`"), "{}", errors(&d));
        fs::write(p.join("onboarding.toml"), "enabled = false\n").unwrap();
        assert_eq!(errors(&d), "");
        fs::write(p.join("join.toml"), format!("gate = true\n{FLOW}")).unwrap();
        assert!(errors(&d).contains("#welcome has the join button"), "{}", errors(&d));
        fs::remove_file(p.join("join.toml")).unwrap();
        assert!(errors(&d).contains("the `join` button needs a join.toml"), "{}", errors(&d));
        fs::write(p.join("join.toml"), FLOW).unwrap();
        fs::write(p.join("content/welcome.md"), "Hi\n{button:join|Get started}\n{button:join|Update}\n").unwrap();
        assert!(errors(&d).contains("two `join` buttons"), "{}", errors(&d));
        fs::write(p.join("content/welcome.md"), "Hi\n{button:join|Get started}\n{button:join:update|Update}\n")
            .unwrap();
        assert_eq!(errors(&d), "");
    }

    #[test]
    fn the_gate_hides_everything_but_the_open_channels() {
        let d = join_dir();
        let p = d.path();
        fs::write(p.join("onboarding.toml"), "enabled = false\n").unwrap();
        fs::write(p.join("join.toml"), format!("gate = true\nopen_channels = [\"welcome\"]\n{FLOW}")).unwrap();
        let c = Config::load(p).unwrap();
        assert_eq!(public_channels(&c.desired), ["welcome"]);
        assert!(c.desired.role("access").unwrap().permissions.contains(&"VIEW_CHANNEL".to_string()));
        assert_eq!(errors(&d), "");
    }

    #[test]
    fn yearly_refresh_is_a_date() {
        use crate::join::month_day;
        assert_eq!(month_day("09-15"), Some((9, 15)));
        assert_eq!(month_day("02-29"), Some((2, 29)));
        for bad in ["13-01", "09-31", "9/15", ""] {
            assert_eq!(month_day(bad), None, "{bad}");
        }
    }

    #[test]
    fn names_discord_would_change_are_warned_about() {
        let d = dir();
        fs::write(
            d.path().join("categories/10-a.toml"),
            "[category]\nkey = \"a\"\nname = \"A\"\n\n[[channel]]\nkey = \"general\"\nname = \"General Chat\"\n",
        )
        .unwrap();
        let r = check(d.path()).unwrap();
        assert!(r.errors.is_empty(), "{r:?}");
        assert!(r.warnings.iter().any(|w| w.contains("\"general-chat\"")), "{r:?}");
        let c = Config::load(d.path()).unwrap();
        assert_eq!(c.desired.channel("general").unwrap().name, "general-chat", "planned as Discord has it");
    }
}

//! How the bot words what it says to people, in one place: a short bold
//! line saying what happened, then a few bullets; names and mentions rather
//! than ids and keys; plain sentences (with the fix) rather than error chains;
//! long lists cut short. Every message the bot posts or replies with goes
//! through these, so they read the same everywhere.
//!
//! Emoji, used sparingly and always for the same thing: [`DONE`],
//! [`ATTENTION`], [`FAILED`], [`NEW_CONFIG`], [`ACCESS`].

use std::collections::BTreeMap;

use chipsoc_config::Config;
use chipsoc_model::{Action, Cause, Field, Move, ObjectKind, OrderScope, Plan, Snowflake, Warning};

/// It's done.
pub const DONE: &str = "✅";
/// Someone should look at this.
pub const ATTENTION: &str = "⚠️";
/// It didn't work.
pub const FAILED: &str = "❌";
/// The config changed.
pub const NEW_CONFIG: &str = "📥";
/// Who can get in: access given or taken.
pub const ACCESS: &str = "🔒";

/// Discord's limit for a message's text.
pub const MAX_CONTENT: usize = 2000;
/// Bullets shown per group before "+N more".
pub const SHOWN: usize = 6;

/// "a", "a and b", "a, b and c".
pub fn and_list(items: &[String]) -> String {
    match items {
        [] => String::new(),
        [one] => one.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

/// Up to `max` items joined with commas, then "+N more".
pub fn collapse(items: &[String], max: usize) -> String {
    let mut out = items.iter().take(max).cloned().collect::<Vec<_>>().join(", ");
    if items.len() > max {
        out.push_str(&format!(" +{} more", items.len() - max));
    }
    out
}

/// "1 change", "3 changes".
pub fn count(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// Lines as bullets, at most `max`, then "+N more".
pub fn bullets(lines: &[String], max: usize) -> Vec<String> {
    let mut out: Vec<String> = lines.iter().take(max).map(|l| format!("• {l}")).collect();
    if lines.len() > max {
        out.push(format!("• +{} more", lines.len() - max));
    }
    out
}

/// At most `max` characters, the last one `…` if it had to be cut.
pub fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_owned();
    }
    let keep: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{keep}…")
}

/// A message with `note` added at the end (a decision under a card, say),
/// within what the bot sends (a character under Discord's limit): if both
/// don't fit, `original` is shortened, never the note.
pub fn with_note(original: &str, note: &str) -> String {
    let note = truncate(note, MAX_CONTENT - 1);
    let room = MAX_CONTENT - 1 - note.chars().count();
    let original = if room == 0 { String::new() } else { truncate(original, room) };
    format!("{original}{note}")
}

/// A wait in words: "a few seconds", "about 40 seconds", "about a minute",
/// "about 5 minutes", "about 2 hours". Seconds and minutes round up, hours
/// to the nearest.
pub fn about(secs: u64) -> String {
    match secs {
        0..=9 => "a few seconds".into(),
        10..=50 => format!("about {} seconds", secs.div_ceil(10) * 10),
        51..=89 => "about a minute".into(),
        90..=3599 => format!("about {} minutes", secs.div_ceil(60)),
        3600..=5399 => "about an hour".into(),
        _ => format!("about {} hours", (secs + 1800) / 3600),
    }
}

/// Text someone else wrote (an applicant, a feed), shown as written: markdown
/// is escaped, so it can't make masked links, mentions, headings or bold
/// lines that pass for the bot's own.
pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut url = false;
    for (i, c) in s.char_indices() {
        if c.is_whitespace() {
            url = false;
        } else if !url {
            let rest = &s[i..];
            url = rest.starts_with("https://") || rest.starts_with("http://");
        }
        // A bare address stays clickable (it can't hide where it goes), but
        // nothing in it may start a masked link or a mention.
        let special = if url { "\\[]<>`" } else { "\\*_~`|>#[]()<@" };
        if special.contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Partner-supplied text, safe to show: on one line, with markdown, masked
/// links and mentions escaped, and invisible characters (which can reorder or
/// hide what's shown) taken out.
pub fn plain(s: &str, max: usize) -> String {
    let line = s.split_whitespace().collect::<Vec<_>>().join(" ");
    let line: String = line.chars().filter(|c| !c.is_control() && !invisible(*c)).collect();
    let mut out = String::new();
    for c in truncate(&line, max).chars() {
        if matches!(c, '\\' | '*' | '_' | '~' | '`' | '|' | '>' | '<' | '#' | '[' | ']' | '(' | ')' | '@' | ':') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Partner-supplied text over several lines, safe to post: [`escape`]d,
/// invisible and control characters taken out, at most two blank lines in a
/// row, cut to `max` characters.
pub fn partner_text(s: &str, max: usize) -> String {
    let clean: String = s.chars().filter(|c| (*c == '\n' || !c.is_control()) && !invisible(*c)).collect();
    let mut lines: Vec<&str> = Vec::new();
    for line in clean.lines().map(str::trim_end) {
        if line.is_empty() && lines.len() >= 2 && lines[lines.len() - 2..].iter().all(|l| l.is_empty()) {
            continue;
        }
        lines.push(line);
    }
    escape(&truncate(lines.join("\n").trim(), max))
}

/// Partner-supplied text where Discord shows no markdown (a post's or an
/// event's name): one line, invisible characters out, cut to `max`.
pub fn partner_name(s: &str, max: usize) -> String {
    let line = s.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate(&line.chars().filter(|c| !c.is_control() && !invisible(*c)).collect::<String>(), max)
}

/// Zero-width, direction-changing and other invisible formatting characters.
pub fn invisible(c: char) -> bool {
    matches!(c,
        '\u{00AD}' | '\u{061C}' | '\u{180E}' | '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}'
        | '\u{2060}'..='\u{206F}' | '\u{FEFF}' | '\u{FFF9}'..='\u{FFFB}')
}

/// A partner's link, if it's a plain https URL: an ASCII host with no user
/// name (`https://github.com@evil.example`), nothing that could end the
/// `<…>` it's shown in or start markdown. Shown whole, never masked.
pub fn safe_link(url: &str) -> Option<String> {
    let rest = url.strip_prefix("https://")?;
    let host = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let ok = url.len() <= 200
        && !host.is_empty()
        && host.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':'))
        && rest.chars().all(|c| c.is_ascii_graphic() && !matches!(c, '<' | '>' | '`' | '"' | '\\' | '|' | '*'));
    ok.then(|| format!("<{url}>"))
}

/// A head line, then as many `lines` as fit in one message, then how many didn't.
pub fn fit(head: &str, lines: &[String]) -> String {
    let mut out = head.to_owned();
    for (n, line) in lines.iter().enumerate() {
        let more = format!("\n…and {} more.", lines.len() - n);
        if out.chars().count() + 1 + line.chars().count() + more.chars().count() >= MAX_CONTENT {
            out.push_str(&more);
            return out;
        }
        out.push('\n');
        out.push_str(line);
    }
    out
}

/// An error, as a sentence a person can act on. The full chain is for the
/// process log (`tracing`), never for Discord.
pub fn error(e: &anyhow::Error) -> String {
    if let Some(p) = e.downcast_ref::<chipsoc_config::authoring::Problems>() {
        let list = p.0.iter().take(3).cloned().collect::<Vec<_>>().join("; ");
        let more = if p.0.len() > 3 { format!(" (+{} more)", p.0.len() - 3) } else { String::new() };
        return format!("the config would have problems: {list}{more}.");
    }
    if let Some(a) = e.downcast_ref::<chipsoc_discord::ApiError>() {
        return discord(a.status, &a.message);
    }
    // What was being done, and the root cause: never the whole chain.
    let said = match e.chain().last().map(ToString::to_string) {
        Some(root) if e.chain().count() > 1 => format!("{e}: {root}"),
        _ => e.to_string(),
    };
    error_text(&format!("{e:#}")).unwrap_or_else(|| sentence(&truncate(&said, 300)))
}

/// An audit-log action as a sentence, the same in every front end:
/// `accepted` as "Accepted an application or suggestion". One this doesn't
/// know reads as written ("some_new_thing" as "Some new thing").
pub fn did(action: &str) -> String {
    let said = match action {
        "accepted" => "Accepted an application or suggestion",
        "rejected" => "Rejected an application or suggestion",
        "accept_failed" => "Pressed Accept, but it didn't go through",
        "made_lead" => "Made someone a project lead",
        "appoint" => "Appointed someone",
        "unappoint" => "Removed someone from a position or role",
        "remove_lead" => "Took someone off a project's leads",
        "restore" => "Gave someone who came back their roles",
        "forget_former" => "Stopped offering someone their old roles",
        "add_feed" => "Followed a feed",
        "remove_feed" => "Stopped following a feed",
        "pause_feed" => "Paused a feed",
        "resume_feed" => "Resumed a feed",
        "post_resource" => "Shared a resource",
        "archive_resource" => "Archived a resource",
        "close_idea" => "Closed an idea",
        "join_notice" => "Sent the join flow reminder",
        "join_flow" => "Finished the join flow",
        "approve_config_change" => "Approved a config change",
        "reject_config_change" => "Rejected a config change",
        "create_event" => "Created an event",
        "share_partners" => "Shared with partners",
        "config_edit" => "Changed the config",
        "apply" => "Applied the config",
        "revert" => "Reverted a config change",
        "rewind" => "Rewound the config",
        "solved" => "Marked a help thread solved",
        other => return capital(&other.replace('_', " ")),
    };
    said.to_owned()
}

/// `s` with its first letter capitalised, to start a line.
pub fn capital(s: &str) -> String {
    let mut c = s.chars();
    c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default()
}

/// [`error`] for one already turned into text (as plan reports keep them).
pub fn error_str(s: &str) -> String {
    error_text(s).unwrap_or_else(|| sentence(&truncate(s, 300)))
}

/// What Discord's refusal means, and what to do.
fn discord(status: u16, message: &str) -> String {
    let m = message.to_lowercase();
    match status {
        403 if m.contains("cannot send messages to this user") => "their DMs are closed.".into(),
        403 => "Discord refused (Missing Permissions): the bot's role needs the permission for this, and has to \
                be above any role it changes. Check the bot's role in Server Settings → Roles."
            .into(),
        404 if m.contains("member") => "they aren't in the server any more.".into(),
        404 if m.contains("channel") => "that channel doesn't exist in Discord any more.".into(),
        404 if m.contains("role") => "that role doesn't exist in Discord any more.".into(),
        404 if m.contains("message") => "that message doesn't exist any more.".into(),
        404 => "Discord doesn't have that any more (it was probably deleted).".into(),
        429 => "Discord is rate-limiting the bot; it tries again shortly.".into(),
        500..=599 => format!("Discord had a problem on its side ({status}); try again in a minute."),
        _ => sentence(&format!("Discord refused it: {}", truncate(message, 200))),
    }
}

/// Known problems in an error's text, in plain words.
fn error_text(chain: &str) -> Option<String> {
    let c = chain.to_lowercase();
    let has = |s: &str| c.contains(s);
    if has("discord said ") {
        let rest = &chain[c.find("discord said ")? + "discord said ".len()..];
        let (status, message) = rest.split_once(": ").unwrap_or((rest, ""));
        if let Ok(status) = status.trim().parse() {
            return Some(discord(status, message));
        }
    }
    if has("missing permissions") || has("50013") {
        return Some(discord(403, "Missing Permissions"));
    }
    if has("unknown member") {
        return Some(discord(404, "Unknown Member"));
    }
    if has("rate limit") || has("ratelimit") || has("you are being rate limited") {
        return Some(discord(429, ""));
    }
    if has("permission denied (publickey)") || has("could not read from remote repository") {
        return Some(
            "GitHub turned the bot's deploy key away (Permission denied (publickey)). This is often a short \
             hiccup on GitHub's side; if it keeps happening, check the deploy key in the config repo's settings."
                .into(),
        );
    }
    if has("could not resolve host")
        || has("connection timed out")
        || has("network is unreachable")
        || has("connection reset")
        || has("connection refused")
        || has("operation timed out")
    {
        return Some("the bot couldn't reach the network (GitHub or Discord); it tries again shortly.".into());
    }
    if has("conflict") && (has("rebase") || has("merge")) {
        return Some(
            "the bot's own commits clash with the repo's, so it can't push them. Someone has to sort it out on the \
             bot's server (the bot's docs/deploy.md: Recovering from failures)."
                .into(),
        );
    }
    None
}

/// With one full stop at the end.
fn sentence(s: &str) -> String {
    format!("{}.", s.trim().trim_end_matches('.'))
}

/// How config keys read in messages: as Discord names and mentions.
pub struct Names<'a> {
    pub cfg: &'a Config,
}

impl<'a> Names<'a> {
    pub fn new(cfg: &'a Config) -> Self {
        Self { cfg }
    }

    /// A role: its mention if it exists (mentions in the bot's posts never
    /// ping), else its name in bold, else its key.
    pub fn role(&self, key: &str) -> String {
        if key == "everyone" {
            return "@everyone".into();
        }
        match (self.cfg.desired.lock.role(key), self.cfg.desired.role(key)) {
            (Some(id), _) => format!("<@&{id}>"),
            (None, Some(r)) => format!("**{}**", r.name),
            (None, None) => format!("`{key}`"),
        }
    }

    /// A role's name, plain (for places mentions don't render, like button labels).
    pub fn role_name(&self, key: &str) -> String {
        self.cfg.desired.role(key).map_or_else(|| key.to_owned(), |r| r.name.clone())
    }

    /// A channel: its mention if it exists, else `#name`.
    pub fn channel(&self, key: &str) -> String {
        match (self.cfg.desired.lock.channel(key), self.cfg.desired.channel(key)) {
            (Some(id), _) => format!("<#{id}>"),
            (None, Some(c)) => format!("**#{}**", c.name),
            (None, None) => format!("`{key}`"),
        }
    }

    pub fn category(&self, key: &str) -> String {
        match self.cfg.desired.category(key) {
            Some(c) => format!("**{}**", c.category.name),
            None => format!("`{key}`"),
        }
    }

    fn project(&self, slug: &str) -> String {
        self.cfg
            .projects
            .iter()
            .find(|p| p.slug == slug)
            .map_or_else(|| format!("`{slug}`"), |p| format!("**{}**", p.name))
    }

    /// Whatever a plan step is about. `id` (from the step) names things
    /// the config no longer has.
    pub fn object(&self, kind: ObjectKind, key: &str, id: Option<Snowflake>) -> String {
        let gone = |mention: String| if id.is_some() { mention } else { format!("`{key}`") };
        match kind {
            ObjectKind::Role | ObjectKind::Holders if self.cfg.desired.role(key).is_none() => {
                gone(format!("<@&{}>", id.unwrap_or_default()))
            }
            ObjectKind::Role | ObjectKind::Holders => self.role(key),
            ObjectKind::Channel if self.cfg.desired.channel(key).is_none() => {
                gone(format!("<#{}>", id.unwrap_or_default()))
            }
            ObjectKind::Channel => self.channel(key),
            ObjectKind::Category if self.cfg.desired.category(key).is_none() => {
                gone(format!("<#{}>", id.unwrap_or_default()))
            }
            ObjectKind::Category => self.category(key),
            ObjectKind::Post => format!("the post in {}", self.channel(key)),
            ObjectKind::ProjectPost => format!("{}'s project post", self.project(key)),
            ObjectKind::Thread => {
                let (forum, own) = key.split_once('/').unwrap_or(("", key));
                let configured = self.cfg.desired.channel(forum).and_then(|c| c.threads.iter().find(|t| t.key == own));
                // One the config no longer has is named by Discord (its real title), not by its key.
                let title = match (configured, id) {
                    (Some(t), _) => format!("**{}**", t.title),
                    (None, Some(id)) => format!("<#{id}> (`{own}` in lock.toml)"),
                    (None, None) => format!("`{own}`"),
                };
                format!("the post {title} in {}", self.channel(forum))
            }
            ObjectKind::Server => "server settings".into(),
            ObjectKind::Onboarding => "onboarding".into(),
            ObjectKind::Screening => "membership screening".into(),
            ObjectKind::AutoMod => format!("AutoMod rule `{key}`"),
        }
    }

    /// A key in a reorder, by what's being sorted.
    fn ordered(&self, scope: &OrderScope, key: &str) -> String {
        match scope {
            OrderScope::Roles => self.role(key),
            OrderScope::Categories => self.category(key),
            OrderScope::Channels(_) => self.channel(key),
        }
    }
}

/// The groups a plan is shown in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Group {
    Roles,
    Channels,
    Posts,
    Members,
    Server,
}

impl Group {
    fn of(a: &Action) -> Self {
        let kind = match a {
            Action::Reorder { scope: OrderScope::Roles, .. } => return Self::Roles,
            Action::Reorder { .. } => return Self::Channels,
            Action::Create { kind, .. }
            | Action::Edit { kind, .. }
            | Action::Archive { kind, .. }
            | Action::Delete { kind, .. }
            | Action::Skip { kind, .. } => *kind,
        };
        match kind {
            ObjectKind::Role => Self::Roles,
            ObjectKind::Category | ObjectKind::Channel => Self::Channels,
            ObjectKind::Post | ObjectKind::ProjectPost | ObjectKind::Thread => Self::Posts,
            ObjectKind::Holders => Self::Members,
            ObjectKind::Server | ObjectKind::Onboarding | ObjectKind::Screening | ObjectKind::AutoMod => Self::Server,
        }
    }

    fn title(self) -> &'static str {
        match self {
            Self::Roles => "Roles",
            Self::Channels => "Channels",
            Self::Posts => "Posts",
            Self::Members => "Members",
            Self::Server => "Server settings",
        }
    }
}

/// People in a holders list as the diff writes it ("<@1> <@2>", "(nobody)").
fn people(s: &str) -> Vec<&str> {
    s.split_whitespace().filter(|p| p.starts_with("<@")).collect()
}

fn short(v: &str) -> String {
    let v = v.replace('\n', " ");
    if v.is_empty() { "(none)".into() } else { truncate(&v, 40) }
}

/// One plan step as a short line.
pub fn step(a: &Action, n: &Names) -> String {
    let why = |cause: &Cause| match cause {
        Cause::Discord => " (changed in Discord; Apply puts it back)",
        Cause::Both => " (changed on both sides: Apply keeps the config's, `/server pull` keeps Discord's)",
        Cause::Config | Cause::Unknown => "",
    };
    match a {
        Action::Create { kind, key } => format!("Create {}", n.object(*kind, key, None)),
        Action::Edit { kind: ObjectKind::Holders, key, id, changes, cause } => {
            let (mut gain, mut lose) = (Vec::new(), Vec::new());
            for c in changes.iter().filter(|c| c.field == Field::Holders) {
                let (from, to) = (people(&c.from), people(&c.to));
                gain.extend(to.iter().filter(|p| !from.contains(p)).map(|p| (*p).to_owned()));
                lose.extend(from.iter().filter(|p| !to.contains(p)).map(|p| (*p).to_owned()));
            }
            let mut what = Vec::new();
            if !gain.is_empty() {
                what.push(format!("give to {}", collapse(&gain, 5)));
            }
            if !lose.is_empty() {
                what.push(format!("take from {}", collapse(&lose, 5)));
            }
            format!("{}: {}{}", n.object(ObjectKind::Holders, key, Some(*id)), what.join("; "), why(cause))
        }
        Action::Edit { kind, key, id, changes, cause } => {
            let what = match changes.as_slice() {
                [] => String::new(),
                [c] if c.field != Field::Content && c.field != Field::Overwrites && c.field != Field::Permissions => {
                    format!("{} {} → {}", c.field, short(&c.from), short(&c.to))
                }
                many => many.iter().map(|c| c.field.to_string()).collect::<Vec<_>>().join(", "),
            };
            let what = if what.is_empty() { what } else { format!(": {what}") };
            format!("Edit {}{what}{}", n.object(*kind, key, Some(*id)), why(cause))
        }
        Action::Reorder { scope, from, to } => {
            let moves: Vec<Move> = chipsoc_model::moves(from, to);
            let what = match scope {
                OrderScope::Roles => "roles".to_owned(),
                OrderScope::Categories => "categories".to_owned(),
                OrderScope::Channels(Some(c)) => format!("channels in {}", n.category(c)),
                OrderScope::Channels(None) => "channels outside categories".to_owned(),
            };
            if moves.is_empty() {
                return format!("Sort {what}: put the new ones in place");
            }
            let said: Vec<String> = moves
                .iter()
                .map(|m| match (&m.above, &m.below) {
                    (Some(a), _) => format!("{} above {}", n.ordered(scope, &m.key), n.ordered(scope, a)),
                    (None, Some(b)) => format!("{} below {}", n.ordered(scope, &m.key), n.ordered(scope, b)),
                    (None, None) => format!("{} to the top", n.ordered(scope, &m.key)),
                })
                .collect();
            format!("Sort {what}: {}", collapse(&said, 3))
        }
        Action::Archive { kind, key, id } => {
            format!("Archive {} (kept, read-only)", n.object(*kind, key, Some(*id)))
        }
        Action::Delete { kind, key, id } => format!("Delete {}", n.object(*kind, key, Some(*id))),
        Action::Skip { kind, key, reason } => format!("Leave {} as is ({reason})", n.object(*kind, key, None)),
    }
}

/// A plan's warnings as lines; many "not in the config" ones become one.
pub fn warnings(ws: &[Warning]) -> Vec<String> {
    let mut out = Vec::new();
    let unmanaged: Vec<String> = ws
        .iter()
        .filter_map(|w| match w {
            Warning::Unmanaged { name, .. } => Some(name.clone()),
            _ => None,
        })
        .collect();
    for w in ws {
        match w {
            Warning::Unmanaged { .. } => {}
            Warning::BotRoleTooLow { bot_role, above } => out.push(format!(
                "**The bot's role ({bot_role}) is below {}**, so the bot can't edit, move or give {}. Discord only \
                 lets a bot manage roles below its own. The server owner (or an admin above it) can drag the bot's \
                 role to the top in Server Settings → Roles.",
                if above.is_empty() { "some roles".to_owned() } else { and_list(above) },
                if above.len() == 1 { "it" } else { "them" }
            )),
            Warning::Note { text } => out.push(text.trim_start_matches(ATTENTION).trim().to_owned()),
            other => out.push(capital(&sentence(&other.to_string()))),
        }
    }
    if !unmanaged.is_empty() {
        out.push(format!(
            "{} in Discord {} in the config, so {} left as {}: {}.",
            count(unmanaged.len(), "thing", "things"),
            if unmanaged.len() == 1 { "isn't" } else { "aren't" },
            if unmanaged.len() == 1 { "it's" } else { "they're" },
            if unmanaged.len() == 1 { "it is" } else { "they are" },
            collapse(&unmanaged, 5)
        ));
    }
    out
}

/// A plan's steps, grouped (Roles, Channels, Posts, Members, Server
/// settings) with counts, each group cut at [`SHOWN`]. Steps that change
/// nothing are left out.
pub fn steps(actions: &[&Action], n: &Names) -> Vec<String> {
    let mut groups: BTreeMap<Group, Vec<String>> = BTreeMap::new();
    for a in actions.iter().filter(|a| a.mutates()) {
        groups.entry(Group::of(a)).or_default().push(step(a, n));
    }
    let mut out = Vec::new();
    for (g, lines) in groups {
        out.push(format!("**{}** ({})", g.title(), lines.len()));
        out.extend(bullets(&lines, SHOWN));
    }
    out
}

/// A plan as a message: `head` (made bold), then its steps, then warnings.
pub fn plan(head: &str, p: &Plan, n: &Names) -> String {
    let actions: Vec<&Action> = p.actions.iter().collect();
    let mut lines = steps(&actions, n);
    lines.extend(warnings(&p.warnings).into_iter().map(|w| format!("{ATTENTION} {w}")));
    fit(&format!("**{head}**"), &lines)
}

/// How applying went: who did it, a count, failures first (in plain words),
/// then what was done, grouped.
pub fn outcome(who: &str, report: &chipsoc_discord::exec::Report, n: &Names) -> String {
    let failed: Vec<&chipsoc_discord::exec::Outcome> = report.failed().collect();
    let done: Vec<&Action> = report.outcomes.iter().filter(|o| o.result.is_ok()).map(|o| &o.action).collect();
    if report.outcomes.is_empty() {
        return format!("{DONE} **{who} applied the plan:** nothing needed changing.");
    }
    let head = if failed.is_empty() {
        format!("{DONE} **Applied by {who}:** {} done.", count(done.len(), "change", "changes"))
    } else {
        format!("{ATTENTION} **Applied by {who}:** {} done, {} failed.", done.len(), failed.len())
    };
    let mut lines = Vec::new();
    for o in failed.iter().take(SHOWN) {
        let why = o.result.as_ref().err().map(|e| error_str(e)).unwrap_or_default();
        lines.push(format!("{FAILED} {}: {why}", step(&o.action, n)));
    }
    if failed.len() > SHOWN {
        lines.push(format!("{FAILED} +{} more failed", failed.len() - SHOWN));
    }
    lines.extend(steps(&done, n));
    fit(&head, &lines)
}

#[cfg(test)]
mod tests {
    use chipsoc_discord::exec::{Outcome, Report};
    use chipsoc_model::{Change, Snowflake};

    use super::*;

    #[test]
    fn a_note_always_fits_and_the_original_is_what_is_cut() {
        let card = "x".repeat(1990);
        let note = "\n✅ **Accepted by <@123456789012345678>.**";
        let out = with_note(&card, note);
        assert!(out.ends_with(note), "{out}");
        assert_eq!(out.chars().count(), MAX_CONTENT - 1);
        assert!(out.contains("x…\n✅"), "the card is shortened");
        assert_eq!(with_note("short", " note"), "short note");
        assert_eq!(with_note("a", &"n".repeat(3000)).chars().count(), MAX_CONTENT - 1);
    }

    #[test]
    fn waits_read_in_words() {
        assert_eq!(about(3), "a few seconds");
        assert_eq!(about(38), "about 40 seconds");
        assert_eq!(about(60), "about a minute");
        assert_eq!(about(55), "about a minute", "not \"about 60 seconds\"");
        assert_eq!(about(287), "about 5 minutes");
        assert_eq!(about(300), "about 5 minutes");
        assert_eq!(about(3600), "about an hour");
        assert_eq!(about(7300), "about 2 hours");
    }

    fn cfg() -> Config {
        let mut c = Config::default();
        let role =
            |k: &str, n: &str| chipsoc_config::schema::Role { key: k.into(), name: n.into(), ..Default::default() };
        c.desired.roles.roles = vec![role("chair", "Chair"), role("treasurer", "Treasurer"), role("member", "Member")];
        c.desired.lock.roles = [("chair".to_owned(), Snowflake(11)), ("member".to_owned(), Snowflake(13))].into();
        c.desired.categories.push(chipsoc_config::schema::CategoryFile {
            category: chipsoc_config::schema::Category { key: "main".into(), name: "Main".into(), overwrites: vec![] },
            channels: vec![chipsoc_config::schema::Channel {
                key: "general".into(),
                name: "general".into(),
                ..Default::default()
            }],
        });
        c.desired.lock.channels.insert("general".into(), Snowflake(21));
        c
    }

    fn keys(s: &str) -> Vec<String> {
        s.split(' ').map(str::to_owned).collect()
    }

    fn sample() -> Plan {
        let topic = Change { field: Field::Topic, from: "Chat".into(), to: "Talk here".into() };
        let holders = Change { field: Field::Holders, from: "<@1> <@2>".into(), to: "<@2> <@3>".into() };
        let mut actions = vec![
            Action::Create { kind: ObjectKind::Role, key: "treasurer".into() },
            Action::Reorder {
                scope: OrderScope::Roles,
                from: keys("member chair"),
                to: keys("chair treasurer member"),
            },
            Action::Edit {
                kind: ObjectKind::Channel,
                key: "general".into(),
                id: Snowflake(21),
                changes: vec![topic],
                cause: Cause::Discord,
            },
            Action::Archive { kind: ObjectKind::Channel, key: "old".into(), id: Snowflake(99) },
            Action::Edit {
                kind: ObjectKind::Holders,
                key: "chair".into(),
                id: Snowflake(11),
                changes: vec![holders],
                cause: Cause::Config,
            },
            Action::Create { kind: ObjectKind::Post, key: "general".into() },
            Action::Skip { kind: ObjectKind::Role, key: "admin".into(), reason: "above the bot".into() },
        ];
        for n in 0..8 {
            actions.push(Action::Create { kind: ObjectKind::Channel, key: format!("new-{n}") });
        }
        let warnings = vec![
            Warning::BotRoleTooLow { bot_role: "Bot".into(), above: vec!["Admin".into()] },
            Warning::Unmanaged { kind: ObjectKind::Channel, name: "random".into(), id: Snowflake(5) },
            Warning::Unmanaged { kind: ObjectKind::Role, name: "Muted".into(), id: Snowflake(6) },
        ];
        Plan { actions, warnings, ..Plan::default() }
    }

    #[test]
    fn a_plan_post_is_grouped_named_and_short() {
        let c = cfg();
        let text = plan("📥 New config (abc1234): 14 changes to apply", &sample(), &Names::new(&c));
        assert_eq!(
            text,
            "**📥 New config (abc1234): 14 changes to apply**
**Roles** (2)
• Create **Treasurer**
• Sort roles: <@&13> below <@&11>
**Channels** (10)
• Edit <#21>: topic Chat → Talk here (changed in Discord; Apply puts it back)
• Archive <#99> (kept, read-only)
• Create `new-0`
• Create `new-1`
• Create `new-2`
• Create `new-3`
• +4 more
**Posts** (1)
• Create the post in <#21>
**Members** (1)
• <@&11>: give to <@3>; take from <@1>
⚠️ **The bot's role (Bot) is below Admin**, so the bot can't edit, move or give it. Discord only lets a bot manage roles below its own. The server owner (or an admin above it) can drag the bot's role to the top in Server Settings → Roles.
⚠️ 2 things in Discord aren't in the config, so they're left as they are: random, Muted."
        );
    }

    #[test]
    fn an_apply_result_leads_with_failures_in_plain_words() {
        let c = cfg();
        let mut report = Report::default();
        report.outcomes.push(Outcome {
            action: Action::Create { kind: ObjectKind::Role, key: "treasurer".into() },
            result: Err("Discord said 403: Missing Permissions".into()),
        });
        report.outcomes.push(Outcome {
            action: Action::Edit {
                kind: ObjectKind::Channel,
                key: "general".into(),
                id: Snowflake(21),
                changes: vec![],
                cause: Cause::Config,
            },
            result: Ok(()),
        });
        let text = outcome("<@7>", &report, &Names::new(&c));
        assert_eq!(
            text,
            "⚠️ **Applied by <@7>:** 1 done, 1 failed.
❌ Create **Treasurer**: Discord refused (Missing Permissions): the bot's role needs the permission for this, and has to be above any role it changes. Check the bot's role in Server Settings → Roles.
**Channels** (1)
• Edit <#21>"
        );
        let ok = Report { outcomes: report.outcomes.into_iter().skip(1).collect(), ..Default::default() };
        assert!(outcome("<@7>", &ok, &Names::new(&c)).starts_with("✅ **Applied by <@7>:** 1 change done."));
        assert!(outcome("<@7>", &Report::default(), &Names::new(&c)).contains("nothing needed changing"));
    }

    #[test]
    fn long_reports_stay_inside_discords_limit() {
        let c = cfg();
        let mut report = Report::default();
        for n in 0..500 {
            let result = if n % 2 == 0 { Err("Discord said 500: oops".into()) } else { Ok(()) };
            report
                .outcomes
                .push(Outcome { action: Action::Create { kind: ObjectKind::Channel, key: format!("c{n}") }, result });
        }
        let text = outcome("x", &report, &Names::new(&c));
        assert!(text.chars().count() < MAX_CONTENT, "{}", text.len());
        assert!(text.contains("+244 more failed"), "{text}");
    }

    #[test]
    fn errors_read_as_sentences_with_the_fix() {
        let e = anyhow::Error::from(chipsoc_discord::ApiError {
            status: 404,
            message: "Unknown Member".into(),
            code: None,
        })
        .context("giving the role");
        assert_eq!(error(&e), "they aren't in the server any more.");
        let git = anyhow::anyhow!(
            "git fetch: git@github.com: Permission denied (publickey).\nfatal: Could not read from remote repository."
        );
        assert!(error(&git).starts_with("GitHub turned the bot's deploy key away"), "{}", error(&git));
        assert!(error_str("Discord said 429: You are being rate limited.").contains("rate-limiting"));
        assert!(error_str("Discord said 403: Missing Permissions").contains("Server Settings → Roles"));
        let plain = anyhow::anyhow!("no such feed").context("pausing feed 3");
        assert_eq!(error(&plain), "pausing feed 3: no such feed.", "what and why, never the whole chain");
        let p = anyhow::Error::from(chipsoc_config::authoring::Problems(vec!["a".into(), "b".into()]));
        assert_eq!(error(&p), "the config would have problems: a; b.");
    }

    #[test]
    fn lists_collapse() {
        let items: Vec<String> = (1..=15).map(|n| n.to_string()).collect();
        assert_eq!(collapse(&items, 3), "1, 2, 3 +12 more");
        assert_eq!(and_list(&items[..3]), "1, 2 and 3");
        assert_eq!(bullets(&items[..2], 1), ["• 1", "• +1 more"]);
        assert_eq!(count(1, "change", "changes"), "1 change");
    }
}

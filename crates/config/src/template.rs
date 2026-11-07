//! Managed-post templates: `{#channel}` and `{@role}` become mentions, and
//! `{button:action|Label}` lines become buttons under the message.

use crate::Lock;

/// The title of a forum's managed post: a pinned thread at the top.
pub const FORUM_POST_TITLE: &str = "How to post here";

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Rendered {
    pub content: String,
    pub buttons: Vec<Button>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Button {
    /// What pressing it does, e.g. `volunteer` or `apply:golem` (see the runtime).
    pub action: String,
    pub label: String,
}

impl Rendered {
    /// One string covering text and buttons, for comparing with Discord.
    pub fn signature(&self) -> String {
        let mut s = self.content.clone();
        for b in &self.buttons {
            s.push_str(&format!("\n[{}|{}]", b.action, b.label));
        }
        s
    }
}

/// Render a template against the lockfile. Unknown keys are left as written.
/// What a `{button:action|Label}` does: the one list `check` and the bot share.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ButtonAction {
    Volunteer,
    /// `idea` (or `propose-project`).
    ProposeProject,
    /// `feed` (or `suggest-feed`).
    SuggestFeed,
    /// `resource` (or `share-resource`).
    ShareResource,
    /// `event`: the `/event` form (members suggest, admins create).
    Event,
    Profile,
    /// `join`, or `join:<anything>` (so two join buttons on a post can differ).
    Join(Option<String>),
    /// `apply:<project slug>`.
    Apply(String),
}

impl ButtonAction {
    /// Every action without an argument, as written, plus the second join button.
    pub const NAMES: &[&str] = &[
        "volunteer",
        "idea",
        "resource",
        "feed",
        "propose-project",
        "share-resource",
        "suggest-feed",
        "event",
        "profile",
        "join",
        "join:update",
    ];

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.split_once(':') {
            Some(("apply", slug)) if !slug.is_empty() => Self::Apply(slug.to_owned()),
            Some(("join", rest)) => Self::Join(Some(rest.to_owned())),
            Some(_) => return None,
            None => match s {
                "volunteer" => Self::Volunteer,
                "idea" | "propose-project" => Self::ProposeProject,
                "feed" | "suggest-feed" => Self::SuggestFeed,
                "resource" | "share-resource" => Self::ShareResource,
                "event" => Self::Event,
                "profile" => Self::Profile,
                "join" => Self::Join(None),
                _ => return None,
            },
        })
    }
}

pub fn render(src: &str, lock: &Lock) -> Rendered {
    let mut buttons = Vec::new();
    let mut lines = Vec::new();
    for line in src.lines() {
        match parse_button(line.trim()) {
            Some(b) => buttons.push(b),
            None => lines.push(line),
        }
    }
    let mut content = lines.join("\n").trim().to_owned();
    for (sigil, mention) in [('#', "<#"), ('@', "<@&")] {
        for key in refs(&content, sigil) {
            let id = if sigil == '#' { lock.channel(&key) } else { lock.role(&key) };
            if let Some(id) = id {
                content = content.replace(&format!("{{{sigil}{key}}}"), &format!("{mention}{id}>"));
            }
        }
    }
    Rendered { content, buttons }
}

/// A line holding only this starts the next part of a long post: each part is
/// its own message, as Discord takes at most [`MAX_CONTENT`] characters in one.
pub const PART_BREAK: &str = "<!-- part -->";
pub use chipsoc_model::MAX_CONTENT;
/// Most messages one post may take.
pub const MAX_PARTS: usize = 10;
/// Between parts' signatures in one post's signature. No part holds it: a
/// part-break line always splits.
const SIGNATURE_BREAK: &str = "\n<!-- part -->\n";

/// A post's source as one source per message: split at each [`PART_BREAK`]
/// line, and where a part is still too long, at the last heading or blank line
/// that keeps it within the limit (each mention counted at its longest). Its
/// buttons go under the last part. A part with nowhere to split stays too long
/// ([`too_long`] says which).
pub fn split(src: &str) -> Vec<String> {
    let mut buttons = Vec::new();
    let mut explicit: Vec<Vec<&str>> = vec![Vec::new()];
    for line in src.lines() {
        if line.trim() == PART_BREAK {
            explicit.push(Vec::new());
        } else if parse_button(line.trim()).is_some() {
            buttons.push(line);
        } else if let Some(part) = explicit.last_mut() {
            part.push(line);
        }
    }
    let mut parts: Vec<String> =
        explicit.iter().flat_map(|lines| fit(lines)).filter(|p| !p.trim().is_empty()).collect();
    if parts.is_empty() {
        parts.push(String::new());
    }
    if let Some(last) = parts.last_mut() {
        for b in buttons {
            last.push('\n');
            last.push_str(b);
        }
    }
    parts
}

/// [`render`] each of a post's parts.
pub fn render_parts(src: &str, lock: &Lock) -> Vec<Rendered> {
    split(src).iter().map(|p| render(p, lock)).collect()
}

/// Which parts (from 1) of [`split`]'s are longer than a message can be.
pub fn too_long(parts: &[String]) -> Vec<usize> {
    parts.iter().enumerate().filter(|(_, p)| longest(p) > MAX_CONTENT).map(|(i, _)| i + 1).collect()
}

/// Button actions written in a part other than the last (they go under the last).
pub fn early_buttons(src: &str) -> Vec<String> {
    // Each explicit part: its buttons, and whether it has any text.
    let mut parts: Vec<(Vec<String>, bool)> = vec![(Vec::new(), false)];
    for line in src.lines().map(str::trim) {
        let Some(part) = parts.last_mut() else { continue };
        match parse_button(line) {
            _ if line == PART_BREAK => parts.push((Vec::new(), false)),
            Some(b) => part.0.push(b.action),
            None => part.1 |= !line.is_empty(),
        }
    }
    let Some(last_text) = parts.iter().rposition(|(_, text)| *text) else { return Vec::new() };
    parts.into_iter().take(last_text).flat_map(|(buttons, _)| buttons).collect()
}

/// One string for a post of several parts, to compare with Discord.
pub fn signature(parts: &[Rendered]) -> String {
    parts.iter().map(Rendered::signature).collect::<Vec<_>>().join(SIGNATURE_BREAK)
}

/// A [`signature`]'s parts.
pub fn signature_parts(sig: &str) -> Vec<&str> {
    sig.split(SIGNATURE_BREAK).collect()
}

/// A rendered text's length with every mention as long as a Discord id can
/// make it (`<#id>`, `<@&id>` with a 20-digit id), so it fits whatever ids
/// things get. Button lines don't count.
pub fn longest(src: &str) -> usize {
    let text: Vec<&str> = src.lines().filter(|l| parse_button(l.trim()).is_none()).collect();
    let text = text.join("\n");
    let text = text.trim();
    let mut n = text.chars().count() as isize;
    for (sigil, mention) in [('#', 23), ('@', 24)] {
        for k in refs(text, sigil) {
            n += mention - (k.chars().count() as isize + 3);
        }
    }
    n.max(0) as usize
}

/// [`longest`] of lines joined, a line at a time: what [`fit`] measures
/// each piece with, so a long post isn't measured again for every cut.
#[derive(Default)]
struct Measure {
    /// Characters from the first one that isn't space to the last.
    text: usize,
    /// Space after the last such character (it counts once more text comes).
    space: usize,
    /// What mentions add (or take away) to the length.
    mentions: isize,
    /// Whether any line was added (the next one comes after a newline).
    lines: bool,
    /// Whether any line, buttons too, isn't blank.
    any: bool,
}

impl Measure {
    fn add(&mut self, line: &str) {
        self.any |= !line.trim().is_empty();
        if parse_button(line.trim()).is_some() {
            return;
        }
        let newline = usize::from(self.lines);
        self.lines = true;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            if self.text > 0 {
                self.space += newline + line.chars().count();
            }
            return;
        }
        let lead = line.chars().count() - line.trim_start().chars().count();
        let trail = line.trim_start().chars().count() - trimmed.chars().count();
        if self.text > 0 {
            self.text += self.space + newline + lead;
        }
        self.text += trimmed.chars().count();
        self.space = trail;
        for (sigil, mention) in [('#', 23), ('@', 24)] {
            for k in refs(trimmed, sigil) {
                self.mentions += mention - (k.chars().count() as isize + 3);
            }
        }
    }

    fn longest(&self) -> usize {
        (self.text as isize + self.mentions).max(0) as usize
    }
}

/// One explicit part as messages that fit, cut before headings or at blank
/// lines (never inside a ``` code block): each as long as it can be.
fn fit(lines: &[&str]) -> Vec<String> {
    let mut fence = false;
    let in_code: Vec<bool> = lines
        .iter()
        .map(|l| {
            let inside = fence;
            if l.trim_start().starts_with("```") {
                fence = !fence;
            }
            inside
        })
        .collect();
    let heading = |l: &str| {
        let t = l.trim_start();
        let rest = t.trim_start_matches('#');
        (1..=6).contains(&(t.len() - rest.len())) && rest.starts_with(' ')
    };
    let mut out = Vec::new();
    let mut start = 0;
    loop {
        let rest = &lines[start..];
        let at = |i: usize| {
            let l = rest[i];
            !in_code[start + i] && (l.trim().is_empty() || heading(l) || rest[i - 1].trim().is_empty())
        };
        // A piece only grows as lines are added, so the last cut found
        // before it's too long is the one to make.
        let mut piece = Measure::default();
        let mut cut = None;
        let mut fits = true;
        for (i, l) in rest.iter().enumerate() {
            if i > 0 && at(i) && piece.any {
                cut = Some(i);
            }
            piece.add(l);
            if piece.longest() > MAX_CONTENT {
                fits = false;
                break;
            }
        }
        match cut {
            _ if fits => {
                out.push(rest.join("\n"));
                return out;
            }
            Some(i) => {
                out.push(rest[..i].join("\n"));
                start += i;
            }
            None => {
                out.push(rest.join("\n"));
                return out;
            }
        }
    }
}

fn parse_button(line: &str) -> Option<Button> {
    let inner = line.strip_prefix("{button:")?.strip_suffix('}')?;
    let (action, label) = inner.split_once('|')?;
    Some(Button { action: action.trim().to_owned(), label: label.trim().to_owned() })
}

/// Keys referenced with `{#key}` (channels) or `{@key}` (roles).
pub fn refs(body: &str, sigil: char) -> Vec<String> {
    let open = format!("{{{sigil}");
    body.match_indices(&open)
        .filter_map(|(i, _)| {
            let rest = &body[i + open.len()..];
            rest.find('}').map(|end| rest[..end].to_owned())
        })
        .filter(|k| !k.is_empty() && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'))
        .collect()
}

/// `{positions}`: the positions open in `/volunteer`.
pub const POSITIONS: &str = "{positions}";
/// `{projects:open}`: the projects taking applications.
pub const OPEN_PROJECTS: &str = "{projects:open}";

/// One entry of a [`lists`] list: a name and a short line about it.
pub type Item = (String, String);

/// What [`lists`] fills in.
#[derive(Debug, Clone, Default)]
pub struct Open {
    pub positions: Vec<Item>,
    /// A line under the positions (when they all close at once, say).
    pub positions_note: Option<String>,
    pub projects: Vec<Item>,
}

/// Fill in `{positions}` and `{projects:open}` with a bullet per item
/// ("- **Name**: about"), or a line saying nothing is open.
pub fn lists(src: &str, open: &Open) -> String {
    let (positions, projects) = (&open.positions, &open.projects);
    let list = |items: &[Item]| {
        if items.is_empty() {
            return "Nothing open right now.".to_owned();
        }
        let line = |(name, about): &Item| {
            if about.is_empty() { format!("- **{name}**") } else { format!("- **{name}**: {about}") }
        };
        items.iter().map(line).collect::<Vec<_>>().join("\n")
    };
    let mut out = src.to_owned();
    if out.contains(POSITIONS) {
        let mut l = list(positions);
        if let Some(note) = open.positions_note.as_ref().filter(|_| !positions.is_empty()) {
            l.push_str(&format!("\n{note}"));
        }
        out = out.replace(POSITIONS, &l);
    }
    if out.contains(OPEN_PROJECTS) {
        out = out.replace(OPEN_PROJECTS, &list(projects));
    }
    out
}

/// `template` with each `{name}` that `value` knows replaced; the rest left as written.
pub fn fill(template: &str, mut value: impl FnMut(&str) -> Option<String>) -> String {
    let mut out = String::new();
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        match after.find('}').and_then(|end| Some((value(&after[..end])?, end))) {
            Some((v, end)) => {
                out.push_str(&v);
                rest = &after[end + 1..];
            }
            None => {
                out.push('{');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// The placeholders an `[ideas] post` template uses that an idea's post
/// doesn't fill: not `{by}`, `{lead}`, or `{answer:<key>}` for one of `keys`.
pub fn unknown_idea_placeholders(template: &str, keys: &[&str]) -> Vec<String> {
    let mut unknown = Vec::new();
    fill(template, |p| {
        let known = matches!(p, "by" | "lead") || p.strip_prefix("answer:").is_some_and(|k| keys.contains(&k));
        let looks =
            p.starts_with("answer:") || (!p.is_empty() && p.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'));
        if !known && looks {
            unknown.push(format!("{{{p}}}"));
        }
        None
    });
    unknown
}

/// The first line of `text`, cut to about a tweet, for a list entry.
pub fn short_line(text: &str) -> String {
    let line = text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or_default();
    if line.chars().count() <= 120 { line.to_owned() } else { line.chars().take(119).chain(['…']).collect() }
}

/// The template without the button lines whose action `drop` picks.
pub fn without_buttons(src: &str, drop: impl Fn(&str) -> bool) -> String {
    let keep = |l: &&str| !parse_button(l.trim()).is_some_and(|b| drop(&b.action));
    let mut out = src.lines().filter(keep).collect::<Vec<_>>().join("\n");
    if src.ends_with('\n') {
        out.push('\n');
    }
    out
}

/// Button actions used in a template.
pub fn button_actions(body: &str) -> Vec<String> {
    body.lines().filter_map(|l| parse_button(l.trim())).map(|b| b.action).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Snowflake;

    #[test]
    fn lists_of_what_is_open() {
        let positions =
            vec![("Treasurer".to_owned(), "Keeps the books".to_owned()), ("Helper".to_owned(), String::new())];
        let mut open = Open { positions, ..Default::default() };
        let src = "Open roles:\n{positions}\n\nProjects:\n{projects:open}\n";
        assert_eq!(
            lists(src, &open),
            "Open roles:\n- **Treasurer**: Keeps the books\n- **Helper**\n\nProjects:\nNothing open right now.\n"
        );
        assert_eq!(lists("no lists", &open), "no lists");
        open.positions_note = Some("Applications close Friday.".into());
        assert_eq!(
            lists("{positions}", &open),
            "- **Treasurer**: Keeps the books\n- **Helper**\nApplications close Friday."
        );
        assert_eq!(short_line("\n  First line.\nSecond."), "First line.");
        assert_eq!(short_line(&"x".repeat(200)).chars().count(), 120);
    }

    #[test]
    fn mentions_and_buttons() {
        let mut lock = Lock::default();
        lock.channels.insert("general".into(), Snowflake(1));
        lock.roles.insert("committee".into(), Snowflake(2));
        let r = render("Say hi in {#general}, ask {@committee}, see {#nope}.\n\n{button:volunteer|Apply}\n", &lock);
        assert_eq!(r.content, "Say hi in <#1>, ask <@&2>, see {#nope}.");
        assert_eq!(r.buttons, [Button { action: "volunteer".into(), label: "Apply".into() }]);
        assert_eq!(refs("{#a} {@b} {#c}", '#'), ["a", "c"]);
    }

    #[test]
    fn drops_chosen_buttons() {
        let src = "Hi\n{button:apply:a|Join A}\n{button:volunteer|Help}\n";
        assert_eq!(without_buttons(src, |a| a == "apply:a"), "Hi\n{button:volunteer|Help}\n");
    }

    #[test]
    fn measuring_a_line_at_a_time_is_measuring_the_whole() {
        let lines = [
            "",
            "  ",
            "  Hello {#general} and {@chair}",
            "",
            "{button:x|Go}",
            "\t",
            "## Heading  ",
            "text {#a}",
            " ",
            "{#a-very-long-channel-key-that-is-longer-than-an-id}",
            "",
            "",
        ];
        for from in 0..lines.len() {
            let mut m = Measure::default();
            for to in from..lines.len() {
                m.add(lines[to]);
                let joined = lines[from..=to].join("\n");
                assert_eq!(m.longest(), longest(&joined), "{joined:?}");
                assert_eq!(m.any, !joined.trim().is_empty());
            }
        }
    }

    #[test]
    fn long_posts_split_into_parts() {
        let src = "One.\n{button:volunteer|Help}\n<!-- part -->\nTwo.\n  <!-- part -->  \n\nThree.\n<!-- part -->\n";
        assert_eq!(
            split(src),
            ["One.", "Two.", "\nThree.\n{button:volunteer|Help}"],
            "buttons go last; empty parts dropped"
        );
        assert_eq!(early_buttons(src), ["volunteer"]);
        assert_eq!(early_buttons("One.\n<!-- part -->\nTwo.\n{button:volunteer|Help}\n"), Vec::<String>::new());
        let parts = render_parts(src, &Lock::default());
        assert!(parts[..2].iter().all(|p| p.buttons.is_empty()) && parts[2].buttons.len() == 1);
        assert_eq!(signature_parts(&signature(&parts)).len(), 3);
        assert_eq!(signature(&parts[..1]), parts[0].signature(), "one part signs as before");
        assert_eq!(split("Short.\n"), ["Short."]);
        assert_eq!(split(""), [""]);

        // Too long: cut at the last heading or blank line that fits.
        let para = "word ".repeat(150); // 750 characters
        let src = format!("# Top\n{para}\n\n{para}\n## Next\n{para}\n{para}\n");
        let parts = split(&src);
        assert_eq!(parts.len(), 2, "{parts:?}");
        assert!(parts[1].starts_with("## Next"), "{:?}", parts[1]);
        assert!(too_long(&parts).is_empty());
        // Mentions count at their longest.
        let mentions = "{#a} ".repeat(400); // 2000 written, 9600 at worst
        assert!(longest(&mentions) > MAX_CONTENT && mentions.trim().chars().count() <= MAX_CONTENT);
        assert_eq!(too_long(&split(&mentions)), [1], "nowhere to split");
        // Never inside a code block.
        let code = format!("Intro\n\n```\n{para}\n\n{para}\n\n{para}\n```\n");
        assert_eq!(too_long(&split(&code)), [2], "{:?}", split(&code));
    }
}

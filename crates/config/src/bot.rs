//! `bot.toml`: how the bot behaves on this server. Each section turns a
//! feature on and says where it posts; leave a section out to turn it off.

use serde::{Deserialize, Serialize};

fn london() -> String {
    "Europe/London".into()
}

/// `bot.toml`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BotConfig {
    /// How the bot refers to the community in messages.
    pub name: String,
    /// IANA time zone for event times and yearly dates, e.g. `"Europe/Paris"` (default `Europe/London`).
    #[serde(default = "london")]
    pub timezone: String,
    /// Role key whose members review applications and run committee commands
    /// (server admins always can).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub admin_role: Option<String>,
    /// `[log]`: where the bot reports.
    #[serde(default)]
    pub log: Log,
    /// Category key removed channels move to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub archive: Option<String>,
    /// `[[position]]` entries: positions people can apply for with `/volunteer`.
    #[serde(default, rename = "position", skip_serializing_if = "Vec::is_empty")]
    pub positions: Vec<Position>,
    /// `[review]`: where applications and suggestions wait for approval.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub review: Option<Review>,
    /// `[events]`: `/event` and its announcements.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub events: Option<Events>,
    /// `[feeds]`: RSS/Atom feeds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub feeds: Option<Feeds>,
    /// `[ideas]`: `/propose-project`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ideas: Option<Ideas>,
    /// `[help]`: a help forum with `/solved`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub help: Option<Help>,
    /// `[projects]`: forum posts for `projects.toml`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub projects: Option<ProjectsConf>,
    /// `[profile]`: names as nicknames.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile: Option<Profile>,
    /// `[resources]`: shared links.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resources: Option<Resources>,
    /// `[web]`: the web UI, and giving admins access to it over Tailscale.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub web: Option<Web>,
    /// `[drift]`: what drift watch does about changes made in Discord.
    #[serde(default, skip_serializing_if = "Drift::is_default")]
    pub drift: Drift,
    /// `[recording]`: `/record` for meetings in voice channels.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recording: Option<crate::recording::Recording>,
}

/// Where the web UI lives. With `share_device` (and TAILSCALE_API_KEY set), new
/// admins get a DM with a one-time Tailscale link to that machine, revoked when
/// they stop being admins.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Web {
    /// Its address, as shown to admins and given to `login --server`.
    pub url: String,
    /// Tailscale machine name of the server running the web UI.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub share_device: Option<String>,
}

impl Default for BotConfig {
    fn default() -> Self {
        Self {
            name: String::new(),
            timezone: london(),
            admin_role: None,
            log: Log::default(),
            archive: None,
            positions: Vec::new(),
            review: None,
            events: None,
            feeds: None,
            ideas: None,
            help: None,
            projects: None,
            profile: None,
            resources: None,
            web: None,
            drift: Drift::default(),
            recording: None,
        }
    }
}

/// `[drift]`: how the bot treats changes made in Discord instead of the config.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Drift {
    /// `undo` (default) or `report`.
    #[serde(default)]
    pub mode: DriftMode,
}

impl Drift {
    fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// What to do about drift.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DriftMode {
    /// Put back what the config says, and say so in the bot log.
    #[default]
    Undo,
    /// Only report the difference.
    Report,
}

/// `[log]`: channels the bot reports in; leave one out for no log.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Log {
    /// Channel key for people: edits, deletes, joins, leaves, review decisions.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub people: Option<String>,
    /// Channel key for the bot itself: plans to apply, approvals, drift, partner sync, errors.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bot: Option<String>,
    /// Days to keep message text for the edit/delete log (default 90; 0 = keep none).
    #[serde(default = "keep_days")]
    pub keep_days: u32,
}

fn keep_days() -> u32 {
    90
}

impl Default for Log {
    fn default() -> Self {
        Self { people: None, bot: None, keep_days: keep_days() }
    }
}

/// A position people apply for with `/volunteer`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Position {
    /// Role key granted when accepted.
    pub role: String,
    /// Shown in the `/volunteer` menu; defaults to the role's name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Also grant the admin role.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub admin: bool,
    /// Listed in `/volunteer` (default true; false for positions filled by election, say).
    #[serde(default = "yes", skip_serializing_if = "Clone::clone")]
    pub open: bool,
    /// Shown at the top of the application form.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub intro: Option<String>,
    /// The form's questions (default: [`default_questions`]). Discord shows 5
    /// boxes at most, the intro taking one.
    #[serde(default = "default_questions", skip_serializing_if = "is_default_questions")]
    pub questions: Vec<Question>,
    /// When applications close: `"YYYY-MM-DD"` (the end of that day) or
    /// `"YYYY-MM-DD HH:MM"`, in [`BotConfig::timezone`]. Until then each
    /// application gets only a line in the review channel; once it closes, one
    /// card lists everyone who applied. Without it each gets its own card.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub closes: Option<String>,
}

impl Default for Position {
    fn default() -> Self {
        Self {
            role: String::new(),
            label: None,
            admin: false,
            open: true,
            intro: None,
            questions: default_questions(),
            closes: None,
        }
    }
}

impl Position {
    /// When applications close (see [`Position::closes`]) as a Unix time, if
    /// they do. `Err` says what's wrong with the date or the time zone.
    pub fn closes_at(&self, tz: &str) -> Result<Option<i64>, String> {
        let Some(text) = self.closes.as_deref() else { return Ok(None) };
        let tz: chrono_tz::Tz = tz.parse().map_err(|_| format!("`timezone = \"{tz}\"` isn't a time zone"))?;
        let local = closing(text).ok_or_else(|| {
            format!("`closes = \"{text}\"` isn't a date: write it as \"2026-11-20\" or \"2026-11-20 18:00\"")
        })?;
        use chrono::TimeZone as _;
        let at = tz.from_local_datetime(&local).earliest();
        at.map(|t| Some(t.timestamp())).ok_or_else(|| format!("`closes = \"{text}\"` doesn't happen in {tz}"))
    }

    /// Whether it takes applications at `now`: open, and its window (if any) not closed yet.
    pub fn taking(&self, tz: &str, now: i64) -> bool {
        self.open && self.closes_at(tz).ok().flatten().is_none_or(|at| now < at)
    }

    /// When applications close, for people: "20 November 2026", or with the time if one was given.
    pub fn closes_label(&self) -> Option<String> {
        let text = self.closes.as_deref()?.trim();
        let (date, time) = text.split_once(' ').map_or((text, None), |(d, t)| (d, Some(t.trim())));
        let day = chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d").ok()?;
        let day = day.format("%-d %B %Y").to_string();
        Some(match time {
            Some(t) => format!("{day}, {t}"),
            None => day,
        })
    }
}

/// The local time `closes` means: the end of a day given alone, or the time given.
fn closing(text: &str) -> Option<chrono::NaiveDateTime> {
    let text = text.trim();
    if let Ok(t) = chrono::NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M") {
        return Some(t);
    }
    let day = chrono::NaiveDate::parse_from_str(text, "%Y-%m-%d").ok()?;
    day.succ_opt()?.and_hms_opt(0, 0, 0)
}

fn yes() -> bool {
    true
}

/// A question on an application form: just the text, or a table
/// `{ ask = "…", long = true, optional = true, hint = "…" }`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "QuestionToml", into = "QuestionToml")]
pub struct Question {
    /// The question, as the box's label.
    pub ask: String,
    /// A paragraph box instead of one line.
    pub long: bool,
    /// May be left blank.
    pub optional: bool,
    /// Placeholder text in the box.
    pub hint: Option<String>,
}

impl Question {
    pub fn new(ask: &str) -> Self {
        Self { ask: ask.into(), long: false, optional: false, hint: None }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum QuestionToml {
    Text(String),
    Full(FullQuestion),
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FullQuestion {
    ask: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    long: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    optional: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    hint: Option<String>,
}

impl From<QuestionToml> for Question {
    fn from(q: QuestionToml) -> Self {
        match q {
            QuestionToml::Text(ask) => Self::new(&ask),
            QuestionToml::Full(f) => Self { ask: f.ask, long: f.long, optional: f.optional, hint: f.hint },
        }
    }
}

impl From<Question> for QuestionToml {
    fn from(q: Question) -> Self {
        if !q.long && !q.optional && q.hint.is_none() {
            return Self::Text(q.ask);
        }
        Self::Full(FullQuestion { ask: q.ask, long: q.long, optional: q.optional, hint: q.hint })
    }
}

/// What `/volunteer` asks a position that doesn't set its own questions.
pub fn default_questions() -> Vec<Question> {
    // Course and year aren't asked: the join flow knows them, and the card shows its answers.
    // Running things and technical know-how are asked apart, so the card shows each.
    vec![
        Question { hint: Some("e.g. 2–4".into()), ..Question::new("Hours per week you can give") },
        Question {
            long: true,
            optional: true,
            hint: Some("Committees, societies, events you've run".into()),
            ..Question::new("Committee or organising experience")
        },
        Question {
            long: true,
            optional: true,
            hint: Some("Projects, courses, work, anything you've built".into()),
            ..Question::new("Electronics / chip design experience")
        },
        Question { long: true, ..Question::new("Why this position?") },
    ]
}

fn is_default_questions(q: &[Question]) -> bool {
    q == default_questions().as_slice()
}

/// Applications (`/volunteer`, `/apply`) and suggestions land here for approval.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Review {
    /// Channel key.
    pub channel: String,
}

/// `/event`: scheduled events, announced here, with reminders.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Events {
    /// Channel key.
    pub channel: String,
    /// Members may suggest events with `/event` (default true); each is
    /// reviewed first, so it needs `[review]`. Admins' events are made at once.
    #[serde(default = "yes", skip_serializing_if = "Clone::clone")]
    pub suggest: bool,
}

impl Default for Events {
    fn default() -> Self {
        Self { channel: String::new(), suggest: true }
    }
}

impl BotConfig {
    /// Whether members can suggest events: `[events]` with `suggest`, and `[review]`.
    pub fn event_suggestions(&self) -> bool {
        self.events.as_ref().is_some_and(|e| e.suggest) && self.review.is_some()
    }
}

/// RSS/Atom feeds, suggested with `/suggest-feed`, posted here.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Feeds {
    /// Channel key.
    pub channel: String,
}

/// Accepted `/propose-project` ideas are posted in this forum.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ideas {
    /// Forum channel key.
    pub forum: String,
    /// Tag name for ideas that still need someone to lead them.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub needs_lead_tag: Option<String>,
    /// The `/propose-project` form, in order (default:
    /// [`default_idea_questions`]). Discord shows 5 boxes at most.
    #[serde(default = "default_idea_questions", skip_serializing_if = "is_default_idea_questions")]
    pub questions: Vec<IdeaQuestion>,
    /// A Markdown file the accepted idea's post is written from, with
    /// `{answer:<key>}`, `{by}` and `{lead}` filled in (default: a post the
    /// bot lays out).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub post: Option<String>,
}

impl Default for Ideas {
    fn default() -> Self {
        Self { forum: String::new(), needs_lead_tag: None, questions: default_idea_questions(), post: None }
    }
}

/// A question on the `/propose-project` form: a [`Question`] (just the text,
/// or `{ ask, long, optional, hint }`) with a `key` naming its answer, or
/// one of the form's own by key alone (see [`IDEA_KEYS`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "IdeaQuestionToml", into = "IdeaQuestionToml")]
pub struct IdeaQuestion {
    /// Names the answer: in `{answer:<key>}`, and for the form's own questions which one it is.
    pub key: String,
    pub question: Question,
}

/// The form's own questions, by key: `name` and `blurb` make the post's
/// title, `area` tags it (a menu of the forum's tags, if it has any),
/// `scope` is what it would do, `lead` asks whether they'd lead it (and so
/// whether it gets the needs-lead tag).
pub const IDEA_KEYS: [&str; 5] = ["name", "blurb", "area", "scope", "lead"];

impl IdeaQuestion {
    /// One of the form's own questions as it's asked by default (`None`: `key` isn't one).
    pub fn own(key: &str) -> Option<Self> {
        let q = match key {
            "name" => Question::new("Name"),
            "blurb" => Question::new("One-line description"),
            "area" => Question { optional: true, ..Question::new("Area") },
            "scope" => Question {
                long: true,
                hint: Some("Goals, scope, what people would learn".into()),
                ..Question::new("What would it do?")
            },
            "lead" => Question::new("Would you like to lead it?"),
            _ => return None,
        };
        Some(Self { key: key.to_owned(), question: q })
    }

    /// Whether it's one of the form's own (see [`IDEA_KEYS`]).
    pub fn is_own(&self) -> bool {
        IDEA_KEYS.contains(&self.key.as_str())
    }
}

/// What `/propose-project` asks unless `[ideas] questions` says otherwise.
pub fn default_idea_questions() -> Vec<IdeaQuestion> {
    IDEA_KEYS.iter().filter_map(|k| IdeaQuestion::own(k)).collect()
}

fn is_default_idea_questions(q: &[IdeaQuestion]) -> bool {
    q == default_idea_questions().as_slice()
}

/// A key from a question's text: "What's the budget?" → `whats_the_budget`.
fn key_of(ask: &str) -> String {
    let mut key = String::new();
    for c in ask.chars() {
        if c.is_ascii_alphanumeric() {
            key.push(c.to_ascii_lowercase());
        } else if (c.is_whitespace() || c == '-' || c == '_') && !key.is_empty() && !key.ends_with('_') {
            key.push('_');
        }
    }
    key.trim_end_matches('_').chars().take(40).collect()
}

#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum IdeaQuestionToml {
    Text(String),
    Full(FullIdeaQuestion),
}

/// Left out, `long`, `optional` and `hint` are as the question asks them by default.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FullIdeaQuestion {
    #[serde(skip_serializing_if = "Option::is_none")]
    key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ask: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    long: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    optional: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    hint: Option<String>,
}

impl TryFrom<IdeaQuestionToml> for IdeaQuestion {
    type Error = String;

    fn try_from(q: IdeaQuestionToml) -> Result<Self, String> {
        let f = match q {
            // One of the form's own keys asks it as by default; other text is the question.
            IdeaQuestionToml::Text(t) => {
                return Ok(Self::own(t.trim()).unwrap_or_else(|| Self { key: key_of(&t), question: Question::new(&t) }));
            }
            IdeaQuestionToml::Full(f) => f,
        };
        let own = f.key.as_deref().and_then(Self::own);
        let ask = match (f.ask, &own) {
            (Some(a), _) => a,
            (None, Some(o)) => o.question.ask.clone(),
            (None, None) => {
                return Err("an idea question needs `ask` (its text), unless its `key` is one of the form's own \
                            (name, blurb, area, scope, lead)"
                    .into());
            }
        };
        let base = own.map_or_else(|| Question::new(&ask), |o| o.question);
        let question = Question {
            long: f.long.unwrap_or(base.long),
            optional: f.optional.unwrap_or(base.optional),
            hint: f.hint.or(base.hint),
            ask,
        };
        Ok(Self { key: f.key.unwrap_or_else(|| key_of(&question.ask)), question })
    }
}

impl From<IdeaQuestion> for IdeaQuestionToml {
    fn from(q: IdeaQuestion) -> Self {
        let own = IdeaQuestion::own(&q.key);
        if own.as_ref() == Some(&q) {
            return Self::Text(q.key);
        }
        let base = own.map_or_else(|| Question::new(&q.question.ask), |o| o.question);
        if own_key(&q) && base == q.question {
            return Self::Text(q.question.ask);
        }
        let differs = |a: bool, b: bool| (a != b).then_some(a);
        let (own, key) = (q.is_own(), (key_of(&q.question.ask) != q.key || q.is_own()).then(|| q.key.clone()));
        let Question { ask, long, optional, hint } = q.question;
        Self::Full(FullIdeaQuestion {
            key,
            long: differs(long, base.long),
            optional: differs(optional, base.optional),
            hint: hint.filter(|h| Some(h) != base.hint.as_ref()),
            ask: (ask != base.ask || !own).then_some(ask),
        })
    }
}

/// Whether a question's key is the one its text gives (and so needn't be written).
fn own_key(q: &IdeaQuestion) -> bool {
    !q.is_own() && key_of(&q.question.ask) == q.key
}

/// `/solved` closes posts in this forum.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Help {
    /// Forum channel key.
    pub forum: String,
    /// Tag name `/solved` adds (default `solved`, any case).
    #[serde(default = "solved")]
    pub solved_tag: String,
}

fn solved() -> String {
    "solved".into()
}

/// Projects (projects.toml): each gets roles, channels and a post in this forum.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectsConf {
    /// Forum channel key.
    pub forum: String,
    /// Suggested areas in `project new`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub areas: Vec<String>,
    /// Channels a new project starts with.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub default_channels: Vec<String>,
}

/// `[resources]`: links members share (title, address, tags), one post each in this forum.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Resources {
    /// Forum channel key.
    pub forum: String,
}

/// Ask members their name (and company, for some roles) and set their nickname
/// to it, adding the start of their surname only when someone else has the same
/// name. Members get a DM after onboarding; put `{button:profile|Set up your
/// profile}` in a post so they can change it later.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    /// Members with any of these role keys are also asked where they work…
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub company_roles: Vec<String>,
    /// …with this question (default "Where do you work?").
    #[serde(default = "company_question")]
    pub company_question: String,
}

fn company_question() -> String {
    COMPANY_QUESTION.into()
}

/// How the bot asks where someone works (the join flow's `company` box should say the same).
pub const COMPANY_QUESTION: &str = "Where do you work?";
/// The line under it.
pub const COMPANY_HELP: &str = "Leave it blank if you aren't working.";

impl BotConfig {
    /// Every channel key this file refers to, with the setting that names it.
    pub fn channel_refs(&self) -> Vec<(&'static str, &str)> {
        [
            ("log.people", self.log.people.as_deref()),
            ("log.bot", self.log.bot.as_deref()),
            ("review.channel", self.review.as_ref().map(|r| r.channel.as_str())),
            ("events.channel", self.events.as_ref().map(|r| r.channel.as_str())),
            ("feeds.channel", self.feeds.as_ref().map(|r| r.channel.as_str())),
            ("ideas.forum", self.ideas.as_ref().map(|r| r.forum.as_str())),
            ("help.forum", self.help.as_ref().map(|r| r.forum.as_str())),
            ("projects.forum", self.projects.as_ref().map(|r| r.forum.as_str())),
            ("resources.forum", self.resources.as_ref().map(|r| r.forum.as_str())),
            ("recording.channel", self.recording.as_ref().map(|r| r.channel.as_str())),
        ]
        .into_iter()
        .filter_map(|(what, k)| k.map(|k| (what, k)))
        .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drift_mode_defaults_to_undo() {
        let parse = |s: &str| toml::from_str::<BotConfig>(&format!("name = \"T\"\n{s}"));
        assert_eq!(parse("").unwrap().drift.mode, DriftMode::Undo);
        assert_eq!(parse("[drift]\nmode = \"report\"").unwrap().drift.mode, DriftMode::Report);
        assert!(parse("[drift]\nmode = \"ignore\"").is_err());
        assert!(!toml::to_string(&BotConfig::default()).unwrap().contains("drift"), "default isn't written");
    }

    #[test]
    fn position_questions_are_text_or_tables() {
        let parse = |s: &str| toml::from_str::<BotConfig>(&format!("name = \"T\"\n[[position]]\nrole = \"r\"\n{s}"));
        assert_eq!(parse("").unwrap().positions[0].questions, default_questions());
        let p = parse("questions = [\"Why?\", { ask = \"More\", long = true }]").unwrap();
        assert_eq!(p.positions[0].questions[1], Question { long: true, ..Question::new("More") });
        assert!(parse("questions = [{ ask = \"x\", lnog = true }]").is_err(), "typos are errors");
        let written = toml::to_string(&p).unwrap();
        assert!(written.contains("questions = [\"Why?\", { ask = \"More\", long = true }]"), "{written}");
        assert!(!toml::to_string(&parse("").unwrap()).unwrap().contains("questions"), "defaults aren't written");
    }

    #[test]
    fn idea_questions_are_the_forms_own_by_key_or_questions_of_their_own() {
        let parse = |s: &str| toml::from_str::<BotConfig>(&format!("name = \"T\"\n[ideas]\nforum = \"f\"\n{s}"));
        let keys =
            |b: &BotConfig| b.ideas.as_ref().unwrap().questions.iter().map(|q| q.key.clone()).collect::<Vec<_>>();
        assert_eq!(keys(&parse("").unwrap()), IDEA_KEYS, "today's form by default");
        assert!(!toml::to_string(&parse("").unwrap()).unwrap().contains("questions"), "defaults aren't written");

        let b = parse(
            "questions = [\"name\", \"What's the budget?\", { key = \"scope\", ask = \"What would it build?\" }, \
             { key = \"kit\", ask = \"Kit needed\", long = true, optional = true, hint = \"FPGA, …\" }, \"lead\"]",
        )
        .unwrap();
        assert_eq!(keys(&b), ["name", "whats_the_budget", "scope", "kit", "lead"]);
        let q = &b.ideas.as_ref().unwrap().questions;
        assert_eq!(q[1].question, Question::new("What's the budget?"));
        assert!(q[2].question.long && q[2].question.hint.is_some(), "the form's own keep what isn't set: {:?}", q[2]);
        assert_eq!(
            q[3].question,
            Question { long: true, optional: true, hint: Some("FPGA, …".into()), ..Question::new("Kit needed") }
        );
        let written = toml::to_string(&b).unwrap();
        assert_eq!(toml::from_str::<BotConfig>(&written).unwrap(), b, "written as read: {written}");
        assert!(
            written.contains("\"name\", \"What's the budget?\", { key = \"scope\", ask = \"What would it build?\" }"),
            "{written}"
        );

        let err = parse("questions = [{ key = \"budget\" }]").unwrap_err().to_string();
        assert!(err.contains("needs `ask`"), "{err}");
        assert!(parse("questions = [{ ask = \"x\", lnog = true }]").is_err(), "typos are errors");
    }

    #[test]
    fn default_questions_ask_organising_and_technical_experience_apart() {
        let q = default_questions();
        let asked: Vec<&str> = q.iter().map(|q| q.ask.as_str()).collect();
        assert_eq!(
            asked,
            [
                "Hours per week you can give",
                "Committee or organising experience",
                "Electronics / chip design experience",
                "Why this position?"
            ]
        );
        assert!(q[1].optional && q[1].long && q[2].optional && q[2].long);
        assert!(!q[3].optional && q[3].long, "why is required");
        assert!(q.iter().all(|q| q.ask.chars().count() <= 45), "labels fit");
    }

    #[test]
    fn a_window_closes_at_the_end_of_its_day_in_the_server_s_time_zone() {
        let p = |closes: &str| Position { closes: Some(closes.into()), ..Position::default() };
        // 21 Nov 2026 00:00 in London (GMT) is 1795219200.
        assert_eq!(p("2026-11-20").closes_at("Europe/London"), Ok(Some(1_795_219_200)));
        assert_eq!(p("2026-11-20 18:00").closes_at("Europe/London"), Ok(Some(1_795_219_200 - 6 * 3600)));
        assert_eq!(p("2026-11-20").closes_at("Europe/Paris"), Ok(Some(1_795_219_200 - 3600)));
        assert!(p("20/11/2026").closes_at("Europe/London").unwrap_err().contains("isn't a date"));
        assert_eq!(Position::default().closes_at("Europe/London"), Ok(None));
        assert!(p("2026-11-20").taking("Europe/London", 1_795_219_199));
        assert!(!p("2026-11-20").taking("Europe/London", 1_795_219_200), "closed");
        assert!(!Position { open: false, ..Position::default() }.taking("Europe/London", 0));
        assert_eq!(p("2026-11-20").closes_label().as_deref(), Some("20 November 2026"));
        assert_eq!(p("2026-11-20 18:00").closes_label().as_deref(), Some("20 November 2026, 18:00"));
        let parsed: BotConfig =
            toml::from_str("name = \"T\"\n[[position]]\nrole = \"r\"\ncloses = \"2026-11-20\"\n").unwrap();
        assert_eq!(parsed.positions[0].closes.as_deref(), Some("2026-11-20"));
    }

    #[test]
    fn event_suggestions_are_on_with_review_unless_turned_off() {
        let parse = |s: &str| toml::from_str::<BotConfig>(&format!("name = \"T\"\n{s}")).unwrap();
        assert!(parse("[events]\nchannel = \"e\"\n[review]\nchannel = \"r\"").event_suggestions());
        assert!(!parse("[events]\nchannel = \"e\"").event_suggestions(), "no review queue");
        assert!(!parse("[events]\nchannel = \"e\"\nsuggest = false\n[review]\nchannel = \"r\"").event_suggestions());
        let written = toml::to_string(&parse("[events]\nchannel = \"e\"")).unwrap();
        assert!(!written.contains("suggest"), "the default isn't written: {written}");
    }
}

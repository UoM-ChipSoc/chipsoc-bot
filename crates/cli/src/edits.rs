//! Command-line words turned into config [`Op`]s, and back into lines. Pure,
//! so each mapping is tested without a config or a server.

use anyhow::{Context, Result, bail};
use chipsoc_config::bot::Question;
use chipsoc_config::ops::{ChannelField, Move, Op, PositionField, ProjectField, RoleField};
use chipsoc_config::schema::{ChannelKind, Overwrite, ProjectStatus};

/// `up`, `down`, or a place counting from 1.
pub fn parse_move(s: &str) -> Result<Move> {
    match s.trim().to_lowercase().as_str() {
        "up" => Ok(Move::Up),
        "down" => Ok(Move::Down),
        "top" | "first" => Ok(Move::To(0)),
        n => match n.parse::<usize>() {
            Ok(0) | Err(_) => bail!("say `up`, `down` or a place counting from 1, not `{s}`"),
            Ok(n) => Ok(Move::To(n - 1)),
        },
    }
}

pub fn parse_status(s: &str) -> Result<ProjectStatus> {
    let s = s.trim().to_lowercase();
    let s = if s == "finished" { "past".to_owned() } else { s };
    s.parse().map_err(|_| anyhow::anyhow!("say proposed, active or past"))
}

pub fn parse_kind(s: &str) -> Result<ChannelKind> {
    s.trim().to_lowercase().parse().map_err(anyhow::Error::msg)
}

/// `on`/`off` and the like.
pub fn parse_bool(s: &str) -> Result<bool> {
    match s.trim().to_lowercase().as_str() {
        "on" | "yes" | "true" | "open" | "1" => Ok(true),
        "off" | "no" | "false" | "closed" | "close" | "0" => Ok(false),
        _ => bail!("say on or off, not `{s}`"),
    }
}

/// `a, b ,c` as a list; empty items dropped.
pub fn list(s: &str) -> Vec<String> {
    s.split(',').map(|x| x.trim().to_owned()).filter(|x| !x.is_empty()).collect()
}

/// `MANAGE_MESSAGES`, from `manage messages` or `manage-messages`.
pub fn permission(s: &str) -> String {
    s.trim().to_uppercase().replace([' ', '-'], "_")
}

/// Text, or none when blank (or `none`).
pub fn optional(s: &str) -> Option<String> {
    let t = s.trim();
    (!t.is_empty() && !t.eq_ignore_ascii_case("none")).then(|| t.to_owned())
}

/// What `role set` was told.
#[derive(Default, Debug, Clone)]
pub struct RoleSet {
    pub name: Option<String>,
    /// `none` takes the colour away.
    pub color: Option<String>,
    pub hoist: Option<bool>,
    pub mentionable: Option<bool>,
    /// Comma-separated; replaces the list (`none` for no permissions).
    pub permissions: Option<String>,
}

pub fn role_ops(key: &str, s: &RoleSet) -> Vec<Op> {
    let mut fields = Vec::new();
    fields.extend(s.name.clone().map(RoleField::Name));
    fields.extend(s.color.as_deref().map(|c| RoleField::Color(optional(c))));
    fields.extend(s.hoist.map(RoleField::Hoist));
    fields.extend(s.mentionable.map(RoleField::Mentionable));
    fields.extend(s.permissions.as_deref().map(|p| {
        let perms = if p.trim().eq_ignore_ascii_case("none") { Vec::new() } else { list(p) };
        RoleField::Permissions(perms.iter().map(|p| permission(p)).collect())
    }));
    fields.into_iter().map(|set| Op::SetRole { key: key.to_owned(), set }).collect()
}

/// What `channel set` was told.
#[derive(Default, Debug, Clone)]
pub struct ChannelSet {
    pub name: Option<String>,
    /// `none` takes the topic away.
    pub topic: Option<String>,
    pub kind: Option<ChannelKind>,
    pub slowmode: Option<u16>,
    pub nsfw: Option<bool>,
    /// Voice only; 0 for no limit.
    pub user_limit: Option<u16>,
    /// Forum tags, comma-separated; replaces the list.
    pub tags: Option<String>,
}

pub fn channel_ops(key: &str, s: &ChannelSet) -> Vec<Op> {
    let mut fields = Vec::new();
    fields.extend(s.name.clone().map(ChannelField::Name));
    fields.extend(s.topic.as_deref().map(|t| ChannelField::Topic(optional(t))));
    fields.extend(s.kind.map(ChannelField::Kind));
    fields.extend(s.slowmode.map(ChannelField::Slowmode));
    fields.extend(s.nsfw.map(ChannelField::Nsfw));
    fields.extend(s.user_limit.map(|n| ChannelField::UserLimit((n > 0).then_some(n))));
    fields.extend(s.tags.as_deref().map(|t| ChannelField::Tags(list(t))));
    fields.into_iter().map(|set| Op::SetChannel { key: key.to_owned(), set }).collect()
}

/// What `position set` was told.
#[derive(Default, Debug, Clone)]
pub struct PositionSet {
    /// `none` goes back to the role's name.
    pub label: Option<String>,
    pub admin: Option<bool>,
    pub open: Option<bool>,
}

pub fn position_ops(role: &str, s: &PositionSet) -> Vec<Op> {
    let mut fields = Vec::new();
    fields.extend(s.label.as_deref().map(|l| PositionField::Label(optional(l))));
    fields.extend(s.admin.map(PositionField::Admin));
    fields.extend(s.open.map(PositionField::Open));
    fields.into_iter().map(|set| Op::SetPosition { role: role.to_owned(), set }).collect()
}

/// The fields `project set` knows, for help and errors.
pub const PROJECT_FIELDS: &[&str] = &[
    "name",
    "blurb",
    "area",
    "status",
    "stage",
    "github",
    "channels",
    "tags",
    "team-role",
    "lead-role",
    "applications",
    "lead-question",
    "questions",
];

/// `project set <slug> <field> <value>`.
pub fn project_op(slug: &str, field: &str, value: &str) -> Result<Op> {
    let set = match field.trim().to_lowercase().replace('_', "-").as_str() {
        "name" => ProjectField::Name(value.trim().to_owned()),
        "blurb" | "description" => ProjectField::Blurb(value.trim().to_owned()),
        "area" | "kind" => ProjectField::Area(value.trim().to_owned()),
        "status" => ProjectField::Status(parse_status(value)?),
        "stage" => ProjectField::Stage(optional(value)),
        "github" | "link" | "repo" => ProjectField::Github(optional(value)),
        "channels" => ProjectField::Channels(list(value)),
        "tags" => ProjectField::Tags(list(value)),
        "team-role" => ProjectField::TeamRole(parse_bool(value)?),
        "lead-role" => ProjectField::LeadRole(parse_bool(value)?),
        "applications" => ProjectField::Applications(parse_bool(value)?),
        "lead-question" => ProjectField::LeadQuestion(parse_bool(value)?),
        // Separated by `|`, since questions have commas in them.
        "questions" => {
            ProjectField::Questions(value.split('|').map(|q| q.trim().to_owned()).filter(|q| !q.is_empty()).collect())
        }
        other => bail!("projects have no field `{other}`; the fields are {}", PROJECT_FIELDS.join(", ")),
    };
    Ok(Op::SetProject { slug: slug.to_owned(), set })
}

/// Questions as text: one per line; `* ` first makes a paragraph box,
/// ` (optional)` last lets it be left blank, ` -- hint` adds a placeholder.
/// Lines starting with `#` are notes and left out.
pub fn questions_from_text(text: &str) -> Vec<Question> {
    let mut out = Vec::new();
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')) {
        let (long, line) = match line.strip_prefix("* ") {
            Some(rest) => (true, rest.trim()),
            None => (false, line),
        };
        let (line, hint) = match line.split_once(" -- ") {
            Some((q, h)) => (q.trim(), optional(h)),
            None => (line, None),
        };
        let (ask, optional) = match line.strip_suffix("(optional)") {
            Some(q) => (q.trim(), true),
            None => (line, false),
        };
        out.push(Question { ask: ask.to_owned(), long, optional, hint });
    }
    out
}

/// The other way: what [`questions_from_text`] reads.
pub fn questions_to_text(qs: &[Question]) -> String {
    let mut out = String::from(
        "# One question per line. `* ` first: a paragraph box. ` (optional)` last: may be left blank.\n\
         # ` -- some text` adds a placeholder. Discord shows 5 boxes at most, the intro taking one.\n",
    );
    for q in qs {
        let long = if q.long { "* " } else { "" };
        let opt = if q.optional { " (optional)" } else { "" };
        let hint = q.hint.as_deref().map(|h| format!(" -- {h}")).unwrap_or_default();
        out.push_str(&format!("{long}{}{opt}{hint}\n", q.ask));
    }
    out
}

/// What `perms` was told about one list of overwrites.
#[derive(Default, Debug, Clone)]
pub struct Perms {
    /// `role:PERM,PERM`, each replacing that role's allowed list.
    pub allow: Vec<String>,
    /// `role:PERM,PERM`, each replacing that role's denied list.
    pub deny: Vec<String>,
    /// Roles whose entry is taken out.
    pub clear: Vec<String>,
}

impl Perms {
    pub fn is_empty(&self) -> bool {
        self.allow.is_empty() && self.deny.is_empty() && self.clear.is_empty()
    }
}

/// `role:PERM,PERM` as the role and its permissions.
fn role_perms(s: &str) -> Result<(String, Vec<String>)> {
    let (role, perms) = s.split_once(':').with_context(|| format!("say `role:PERMISSION,…`, not `{s}`"))?;
    let role = role.trim().trim_start_matches('@').to_owned();
    if role.is_empty() {
        bail!("say which role in `{s}` (`everyone` for everybody)");
    }
    Ok((role, list(perms).iter().map(|p| permission(p)).collect()))
}

/// `current` with `p`'s changes: a role named in `allow` or `deny` gets that
/// list replaced (others kept); `clear` takes a role's entry out; entries
/// left with nothing in them go.
pub fn merge_overwrites(current: &[Overwrite], p: &Perms) -> Result<Vec<Overwrite>> {
    let mut out = current.to_vec();
    let entry = |out: &mut Vec<Overwrite>, role: &str| -> usize {
        match out.iter().position(|o| o.role.as_deref() == Some(role)) {
            Some(i) => i,
            None => {
                out.push(Overwrite { role: Some(role.to_owned()), ..Default::default() });
                out.len() - 1
            }
        }
    };
    for a in &p.allow {
        let (role, perms) = role_perms(a)?;
        let i = entry(&mut out, &role);
        out[i].allow = perms;
    }
    for d in &p.deny {
        let (role, perms) = role_perms(d)?;
        let i = entry(&mut out, &role);
        out[i].deny = perms;
    }
    for c in &p.clear {
        let role = c.trim().trim_start_matches('@');
        out.retain(|o| o.role.as_deref() != Some(role));
    }
    out.retain(|o| !o.allow.is_empty() || !o.deny.is_empty());
    Ok(out)
}

/// `member: allow SEND_MESSAGES; deny VIEW_CHANNEL`.
pub fn overwrite_line(o: &Overwrite) -> String {
    let who = match (&o.role, o.member) {
        (Some(r), _) => r.clone(),
        (None, Some(m)) => format!("member {m}"),
        (None, None) => "?".into(),
    };
    let mut parts = Vec::new();
    if !o.allow.is_empty() {
        parts.push(format!("allow {}", o.allow.join(", ")));
    }
    if !o.deny.is_empty() {
        parts.push(format!("deny {}", o.deny.join(", ")));
    }
    format!("{who}: {}", parts.join("; "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn moves_users_and_switches() {
        assert_eq!(parse_move("Up").unwrap(), Move::Up);
        assert_eq!(parse_move("3").unwrap(), Move::To(2), "places count from 1");
        assert!(parse_move("0").is_err() && parse_move("sideways").is_err());
        assert!(parse_bool("on").unwrap() && !parse_bool("Closed").unwrap() && parse_bool("maybe").is_err());
        assert_eq!(parse_status("finished").unwrap(), ProjectStatus::Past);
        assert_eq!(parse_kind("Forum").unwrap(), ChannelKind::Forum);
        assert!(parse_kind("chat").unwrap_err().to_string().contains("text, voice"));
    }

    #[test]
    fn role_flags_become_one_op_each() {
        let s = RoleSet {
            name: Some("Members".into()),
            color: Some("none".into()),
            hoist: Some(true),
            permissions: Some("send messages, manage-threads".into()),
            ..Default::default()
        };
        let ops = role_ops("member", &s);
        let k = |set| Op::SetRole { key: "member".into(), set };
        assert_eq!(
            ops,
            [
                k(RoleField::Name("Members".into())),
                k(RoleField::Color(None)),
                k(RoleField::Hoist(true)),
                k(RoleField::Permissions(vec!["SEND_MESSAGES".into(), "MANAGE_THREADS".into()])),
            ]
        );
        assert!(role_ops("member", &RoleSet::default()).is_empty());
        let none = RoleSet { permissions: Some("none".into()), ..Default::default() };
        assert_eq!(role_ops("m", &none), [Op::SetRole { key: "m".into(), set: RoleField::Permissions(vec![]) }]);
    }

    #[test]
    fn channel_and_position_flags() {
        let s = ChannelSet {
            topic: Some(" Say hi ".into()),
            user_limit: Some(0),
            tags: Some("a, b".into()),
            ..Default::default()
        };
        let k = |set| Op::SetChannel { key: "g".into(), set };
        assert_eq!(
            channel_ops("g", &s),
            [
                k(ChannelField::Topic(Some("Say hi".into()))),
                k(ChannelField::UserLimit(None)),
                k(ChannelField::Tags(vec!["a".into(), "b".into()])),
            ]
        );
        let p = PositionSet { label: Some("none".into()), open: Some(false), ..Default::default() };
        let k = |set| Op::SetPosition { role: "chair".into(), set };
        assert_eq!(position_ops("chair", &p), [k(PositionField::Label(None)), k(PositionField::Open(false))]);
    }

    #[test]
    fn project_fields_by_name() {
        let k = |set| Op::SetProject { slug: "golem".into(), set };
        assert_eq!(project_op("golem", "status", "active").unwrap(), k(ProjectField::Status(ProjectStatus::Active)));
        assert_eq!(project_op("golem", "github", "none").unwrap(), k(ProjectField::Github(None)));
        assert_eq!(project_op("golem", "applications", "on").unwrap(), k(ProjectField::Applications(true)));
        assert_eq!(
            project_op("golem", "questions", "Why, really? | What do you know?").unwrap(),
            k(ProjectField::Questions(vec!["Why, really?".into(), "What do you know?".into()]))
        );
        assert_eq!(project_op("golem", "team_role", "off").unwrap(), k(ProjectField::TeamRole(false)));
        assert!(project_op("golem", "colour", "red").unwrap_err().to_string().contains("the fields are name"));
    }

    #[test]
    fn questions_round_trip_through_text() {
        let qs = vec![
            Question::new("Course and year?"),
            Question { long: true, optional: true, hint: Some("A sentence or two".into()), ..Question::new("Why?") },
        ];
        let text = questions_to_text(&qs);
        assert_eq!(questions_from_text(&text), qs);
        assert_eq!(questions_from_text("# note\n\n  Hours a week?  \n"), [Question::new("Hours a week?")]);
    }

    #[test]
    fn overwrites_merge_by_role() {
        let current = vec![
            Overwrite { role: Some("everyone".into()), deny: vec!["VIEW_CHANNEL".into()], ..Default::default() },
            Overwrite { role: Some("member".into()), allow: vec!["VIEW_CHANNEL".into()], ..Default::default() },
        ];
        let p = Perms {
            allow: vec!["member:view channel, send messages".into(), "@mods:manage_messages".into()],
            deny: vec![],
            clear: vec!["everyone".into()],
        };
        let out = merge_overwrites(&current, &p).unwrap();
        let lines: Vec<String> = out.iter().map(overwrite_line).collect();
        assert_eq!(lines, ["member: allow VIEW_CHANNEL, SEND_MESSAGES", "mods: allow MANAGE_MESSAGES"]);
        let emptied = Perms { allow: vec!["member:".into()], ..Default::default() };
        assert_eq!(merge_overwrites(&current, &emptied).unwrap().len(), 1, "an empty entry goes");
        assert!(merge_overwrites(&current, &Perms { allow: vec!["SEND".into()], ..Default::default() }).is_err());
    }
}

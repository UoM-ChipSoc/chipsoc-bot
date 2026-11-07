//! Forms: a list of fields to type in, pick from or switch on and off, and
//! the status picker. Also the plain-text layouts used when a list is edited
//! in $EDITOR (questions, permissions).

use chipsoc_config::bot::Question;
use chipsoc_config::schema::{Overwrite, ProjectStatus};

/// What each status means, in a few words.
pub fn status_help(s: ProjectStatus) -> &'static str {
    match s {
        ProjectStatus::Proposed => "an idea; a forum post only",
        ProjectStatus::Active => "running, with its own channels",
        ProjectStatus::Past => "finished; channels move to the archive, history kept",
    }
}

/// The item `by` steps from `current`, wrapping around.
fn cycle<T: PartialEq + Clone>(items: &[T], current: &T, by: isize) -> T {
    let i = items.iter().position(|x| x == current).unwrap_or(0) as isize;
    items[(i + by).rem_euclid(items.len() as isize) as usize].clone()
}

pub struct StatusPicker {
    pub slug: String,
    pub name: String,
    pub status: ProjectStatus,
}

impl StatusPicker {
    pub fn cycle(&mut self, by: isize) {
        self.status = cycle(&ProjectStatus::ALL, &self.status, by);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    Text(String),
    /// (value, label) pairs and which is picked.
    Choice(Vec<(String, String)>, usize),
    Toggle(bool),
}

/// Which field of a form an input is: what its parser reads it by. The
/// words shown are [`Key::label`], so changing them can't lose a field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Address,
    AdminPosition,
    AgeRestricted,
    Area,
    Blurb,
    Category,
    Channels,
    CodeLink,
    Colour,
    Description,
    Label,
    Lead,
    ListedApart,
    Mentionable,
    Name,
    PositionOrRole,
    Reason,
    Role,
    SlowMode,
    Stage,
    Status,
    Tags,
    TakingApplications,
    Title,
    Topic,
    Type,
    UserId,
    UserLimit,
}

impl Key {
    /// The field's name, as shown in the form.
    pub fn label(self) -> &'static str {
        match self {
            Self::Address => "Address",
            Self::AdminPosition => "Admin position",
            Self::AgeRestricted => "Age-restricted",
            Self::Area => "Area",
            Self::Blurb => "One-line description",
            Self::Category => "Category",
            Self::Channels => "Channels",
            Self::CodeLink => "Code link",
            Self::Colour => "Colour",
            Self::Description => "Description",
            Self::Label => "Label",
            Self::Lead => "Lead",
            Self::ListedApart => "Listed apart",
            Self::Mentionable => "@mentionable",
            Self::Name => "Name",
            Self::PositionOrRole => "Position or role",
            Self::Reason => "Reason (optional, sent to them)",
            Self::Role => "Role",
            Self::SlowMode => "Slow mode",
            Self::Stage => "Stage",
            Self::Status => "Status",
            Self::Tags => "Tags",
            Self::TakingApplications => "Taking applications",
            Self::Title => "Title",
            Self::Topic => "Topic",
            Self::Type => "Type",
            Self::UserId => "Discord user id",
            Self::UserLimit => "User limit",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Input {
    pub key: Key,
    pub label: &'static str,
    pub value: Value,
    /// A line of help under the form while this field is picked.
    pub hint: &'static str,
}

impl Input {
    pub fn text(key: Key, value: impl Into<String>) -> Self {
        Self { key, label: key.label(), value: Value::Text(value.into()), hint: "" }
    }

    pub fn toggle(key: Key, on: bool) -> Self {
        Self { key, label: key.label(), value: Value::Toggle(on), hint: "" }
    }

    /// Pick one of `options` (value, label), starting at `current` (else the first).
    pub fn choice(key: Key, options: Vec<(String, String)>, current: &str) -> Self {
        let at = options.iter().position(|(v, _)| v == current).unwrap_or(0);
        Self { key, label: key.label(), value: Value::Choice(options, at), hint: "" }
    }

    pub fn hint(self, hint: &'static str) -> Self {
        Self { hint, ..self }
    }

    pub fn labelled(self, label: &'static str) -> Self {
        Self { label, ..self }
    }

    pub fn is_choice(&self) -> bool {
        !matches!(self.value, Value::Text(_))
    }

    /// The value as shown.
    pub fn shown(&self) -> String {
        match &self.value {
            Value::Text(s) => s.clone(),
            Value::Choice(o, i) => o.get(*i).map(|(_, l)| l.clone()).unwrap_or_default(),
            Value::Toggle(b) => if *b { "yes" } else { "no" }.into(),
        }
    }
}

/// A form; `then` says what saving it does.
pub struct Form<T> {
    pub title: String,
    pub fields: Vec<Input>,
    pub at: usize,
    /// Where typing goes in the current text field, in characters.
    pub cursor: usize,
    pub error: Option<String>,
    pub then: T,
}

impl<T> Form<T> {
    pub fn new(title: impl Into<String>, fields: Vec<Input>, then: T) -> Self {
        let mut f = Self { title: title.into(), fields, at: 0, cursor: 0, error: None, then };
        f.cursor = f.text_len();
        f
    }

    pub fn on_choice(&self) -> bool {
        self.fields.get(self.at).is_some_and(Input::is_choice)
    }

    pub fn on_last(&self) -> bool {
        self.at + 1 >= self.fields.len()
    }

    fn text_mut(&mut self) -> Option<&mut String> {
        match self.fields.get_mut(self.at).map(|f| &mut f.value) {
            Some(Value::Text(s)) => Some(s),
            _ => None,
        }
    }

    fn text_len(&self) -> usize {
        match self.fields.get(self.at).map(|f| &f.value) {
            Some(Value::Text(s)) => s.chars().count(),
            _ => 0,
        }
    }

    /// The byte offset of the cursor in `s`.
    fn byte_at(s: &str, cursor: usize) -> usize {
        s.char_indices().nth(cursor).map_or(s.len(), |(i, _)| i)
    }

    pub fn move_field(&mut self, by: isize) {
        let n = self.fields.len().max(1) as isize;
        self.at = (self.at as isize + by).rem_euclid(n) as usize;
        self.cursor = self.text_len();
    }

    /// ←→: pick another choice, flip a toggle, or move the cursor.
    pub fn cycle(&mut self, by: isize) {
        match self.fields.get_mut(self.at).map(|f| &mut f.value) {
            Some(Value::Choice(o, i)) if !o.is_empty() => {
                *i = (*i as isize + by).rem_euclid(o.len() as isize) as usize;
            }
            Some(Value::Toggle(b)) => *b = !*b,
            Some(Value::Text(_)) => self.cursor = self.cursor.saturating_add_signed(by).min(self.text_len()),
            _ => {}
        }
    }

    /// Home (false) or End (true) of the text.
    pub fn cursor_to(&mut self, end: bool) {
        self.cursor = if end { self.text_len() } else { 0 };
    }

    pub fn type_char(&mut self, c: char) {
        let at = self.cursor;
        match self.text_mut() {
            Some(s) => {
                let i = Self::byte_at(s, at);
                s.insert(i, c);
                self.cursor += 1;
            }
            // Space flips a toggle or steps a choice.
            None if c == ' ' => self.cycle(1),
            None => {}
        }
    }

    /// Pasted text, on one line.
    pub fn paste(&mut self, text: &str) {
        let line = text.trim_end_matches(['\r', '\n']).replace(['\r', '\n'], " ");
        for c in line.chars() {
            self.type_char(c);
        }
    }

    pub fn backspace(&mut self) {
        let at = self.cursor;
        if at == 0 {
            return;
        }
        if let Some(s) = self.text_mut() {
            let i = Self::byte_at(s, at - 1);
            s.remove(i);
            self.cursor -= 1;
        }
    }

    pub fn clear(&mut self) {
        if let Some(s) = self.text_mut() {
            s.clear();
            self.cursor = 0;
        }
    }

    /// The current text field split at the cursor.
    pub fn split_at_cursor(&self) -> (String, String) {
        match self.fields.get(self.at).map(|f| &f.value) {
            Some(Value::Text(s)) => {
                let i = Self::byte_at(s, self.cursor);
                (s[..i].to_owned(), s[i..].to_owned())
            }
            _ => Default::default(),
        }
    }

    fn field(&self, key: Key) -> Option<&Value> {
        self.fields.iter().find(|f| f.key == key).map(|f| &f.value)
    }

    /// A text field, trimmed.
    pub fn text(&self, key: Key) -> String {
        match self.field(key) {
            Some(Value::Text(s)) => s.trim().to_owned(),
            _ => String::new(),
        }
    }

    /// A text field, or none if blank.
    pub fn opt(&self, key: Key) -> Option<String> {
        Some(self.text(key)).filter(|s| !s.is_empty())
    }

    /// The picked choice's value.
    pub fn picked(&self, key: Key) -> String {
        match self.field(key) {
            Some(Value::Choice(o, i)) => o.get(*i).map(|(v, _)| v.clone()).unwrap_or_default(),
            _ => String::new(),
        }
    }

    pub fn on(&self, key: Key) -> bool {
        matches!(self.field(key), Some(Value::Toggle(true)))
    }

    /// A comma-separated text field as a list.
    pub fn list(&self, key: Key) -> Vec<String> {
        split_list(&self.text(key))
    }
}

/// "a, b ,c" as ["a", "b", "c"].
pub fn split_list(s: &str) -> Vec<String> {
    s.split(',').map(str::trim).filter(|x| !x.is_empty()).map(str::to_owned).collect()
}

/// Lines that aren't blank or `#` comments.
fn content(text: &str) -> impl Iterator<Item = &str> {
    text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#'))
}

const QUESTIONS_HELP: &str = "# One question per line. Add [long] for a big box, [optional] if it may be left\n\
# blank, and a hint for the box after \" // \". Discord shows 5 boxes at most\n\
# (the intro, if there is one, takes one).\n";

/// Questions as text to edit.
pub fn questions_text(qs: &[Question]) -> String {
    let mut out = QUESTIONS_HELP.to_owned();
    for q in qs {
        out.push_str(&q.ask);
        if q.long {
            out.push_str(" [long]");
        }
        if q.optional {
            out.push_str(" [optional]");
        }
        if let Some(h) = &q.hint {
            out.push_str(&format!(" // {h}"));
        }
        out.push('\n');
    }
    out
}

pub fn parse_questions(text: &str) -> Result<Vec<Question>, String> {
    let qs: Vec<Question> = content(text)
        .map(|line| {
            let (rest, hint) = match line.split_once(" // ") {
                Some((r, h)) => (r, Some(h.trim().to_owned()).filter(|h| !h.is_empty())),
                None => (line, None),
            };
            let long = rest.contains("[long]");
            let optional = rest.contains("[optional]");
            let ask = rest.replace("[long]", "").replace("[optional]", "").trim().to_owned();
            Question { ask, long, optional, hint }
        })
        .collect();
    if qs.is_empty() {
        return Err("Give at least one question.".into());
    }
    Ok(qs)
}

/// Permission names as text to edit, one per line, with the others listed.
pub fn permissions_text(have: &[String], all: &[String]) -> String {
    let mut out = "# One permission per line. Delete a line to take it away.\n".to_owned();
    for p in have {
        out.push_str(p);
        out.push('\n');
    }
    out.push_str("#\n# All permissions:\n");
    for chunk in all.chunks(4) {
        out.push_str(&format!("#   {}\n", chunk.join(" ")));
    }
    out
}

pub fn parse_permissions(text: &str) -> Vec<String> {
    content(text).flat_map(|l| l.split([' ', ','])).filter(|w| !w.is_empty()).map(str::to_uppercase).collect()
}

/// Overwrites as text to edit: `<role> allow PERM … deny PERM …` per line.
pub fn overwrites_text(ows: &[Overwrite], roles: &[String]) -> String {
    let mut out = "# Who can see and do what here, one line per role:\n\
                   #   <role key> allow PERMISSION … deny PERMISSION …\n\
                   # e.g.  everyone deny VIEW_CHANNEL\n\
                   #       committee allow VIEW_CHANNEL SEND_MESSAGES\n"
        .to_owned();
    out.push_str(&format!("# Roles: {}\n", roles.join(", ")));
    for o in ows {
        let mut line = match (&o.role, o.member) {
            (Some(r), _) => r.clone(),
            (None, Some(m)) => format!("member:{m}"),
            (None, None) => continue,
        };
        if !o.allow.is_empty() {
            line.push_str(&format!(" allow {}", o.allow.join(" ")));
        }
        if !o.deny.is_empty() {
            line.push_str(&format!(" deny {}", o.deny.join(" ")));
        }
        out.push_str(&line);
        out.push('\n');
    }
    out
}

pub fn parse_overwrites(text: &str) -> Result<Vec<Overwrite>, String> {
    let mut out = Vec::new();
    for line in content(text) {
        let mut words = line.split_whitespace();
        let who = words.next().unwrap_or_default();
        let mut o = Overwrite::default();
        match who.strip_prefix("member:") {
            Some(id) => o.member = Some(id.parse().map_err(|_| format!("`{who}`: a member is member:<their id>"))?),
            None => o.role = Some(who.to_owned()),
        }
        let mut allow = None;
        for w in words {
            match w {
                "allow" => allow = Some(true),
                "deny" => allow = Some(false),
                p => match allow {
                    Some(true) => o.allow.push(p.to_uppercase()),
                    Some(false) => o.deny.push(p.to_uppercase()),
                    None => return Err(format!("`{line}`: say allow or deny before {p}")),
                },
            }
        }
        out.push(o);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typing_goes_where_the_cursor_is() {
        let mut f = Form::new("t", vec![Input::text(Key::Name, "Ada"), Input::toggle(Key::ListedApart, false)], ());
        assert_eq!(f.cursor, 3);
        f.cycle(-1);
        f.type_char('x');
        assert_eq!(f.text(Key::Name), "Adxa");
        f.cursor_to(false);
        f.backspace();
        f.type_char('é');
        f.cursor_to(true);
        f.backspace();
        assert_eq!(f.text(Key::Name), "éAdx");
        f.paste("  y\n");
        assert_eq!(f.split_at_cursor(), ("éAdx  y".to_owned(), String::new()), "a pasted newline isn't Enter");
        f.clear();
        assert_eq!(f.text(Key::Name), "");
        f.move_field(1);
        f.type_char(' ');
        assert!(f.on(Key::ListedApart), "space flips a toggle");
        assert!(!f.on(Key::Mentionable) && f.text(Key::Topic).is_empty(), "a field the form hasn't is empty");
        assert_eq!(f.fields[0].label, "Name", "shown by its key's label");
    }

    #[test]
    fn questions_round_trip_through_text() {
        let qs = vec![
            Question::new("Course?"),
            Question { long: true, optional: true, hint: Some("a line or two".into()), ..Question::new("Why?") },
        ];
        assert_eq!(parse_questions(&questions_text(&qs)).unwrap(), qs);
        assert!(parse_questions("# nothing\n").is_err());
    }

    #[test]
    fn overwrites_and_permissions_round_trip_through_text() {
        let ows = vec![
            Overwrite { role: Some("everyone".into()), deny: vec!["VIEW_CHANNEL".into()], ..Default::default() },
            Overwrite {
                member: Some(chipsoc_config::Snowflake(7)),
                allow: vec!["SEND_MESSAGES".into()],
                ..Default::default()
            },
        ];
        let text = overwrites_text(&ows, &["everyone".into()]);
        assert_eq!(parse_overwrites(&text).unwrap(), ows);
        assert!(parse_overwrites("admin VIEW_CHANNEL").unwrap_err().contains("allow or deny"));
        let perms = parse_permissions("# x\nsend_messages\nVIEW_CHANNEL, KICK_MEMBERS");
        assert_eq!(perms, ["SEND_MESSAGES", "VIEW_CHANNEL", "KICK_MEMBERS"]);
        let all = vec!["A".to_owned(), "B".to_owned()];
        assert_eq!(parse_permissions(&permissions_text(&perms, &all)), perms, "the list of all is a comment");
    }
}

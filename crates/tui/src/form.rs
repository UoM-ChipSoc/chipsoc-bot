//! Forms: a list of fields to type in, pick from or switch on and off, and
//! the status picker. Lists edited in $EDITOR (questions, permissions) are
//! laid out by [`chipsoc_config::ops::text`].

use chipsoc_config::ops::text;
use chipsoc_config::schema::ProjectStatus;

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
        text::list(&self.text(key))
    }
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
}

//! Key bindings. The footer is drawn from the same table that handles keys,
//! so it always lists exactly what works.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::app::Tab;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Act {
    Quit,
    Help,
    Go(Tab),
    NextTab,
    PrevTab,
    Up,
    Down,
    First,
    Last,
    PageUp,
    PageDown,
    Left,
    Right,
    Home,
    End,
    Backspace,
    Clear,
    Save,
    Cancel,
    /// Open the file that defines the selection in $EDITOR.
    Edit,
    Reload,
    /// Edit the selection in a form.
    Open,
    New,
    NewCategory,
    Remove,
    MoveUp,
    MoveDown,
    MoveTo,
    Permissions,
    Post,
    SetStatus,
    ToggleApplications,
    Brief,
    AddLead,
    RemoveLead,
    ToggleOpen,
    Intro,
    Questions,
    Accept,
    Reject,
    MakeLead,
    History,
    Restore,
    Forget,
    TogglePause,
    Fetch,
    Apply,
    Pull,
    Drift,
}

/// A key, with Shift or Ctrl held if they matter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Key {
    pub code: KeyCode,
    pub mods: KeyModifiers,
}

const fn plain(code: KeyCode) -> Key {
    Key { code, mods: KeyModifiers::NONE }
}

const fn shift(code: KeyCode) -> Key {
    Key { code, mods: KeyModifiers::SHIFT }
}

const fn ctrl(c: char) -> Key {
    Key { code: KeyCode::Char(c), mods: KeyModifiers::CONTROL }
}

const fn ch(c: char) -> Key {
    plain(KeyCode::Char(c))
}

impl Key {
    /// Letters carry their case, so Shift is only looked at for other keys.
    pub fn matches(&self, ev: &KeyEvent) -> bool {
        let mut mods = ev.modifiers & (KeyModifiers::SHIFT | KeyModifiers::CONTROL);
        if matches!(ev.code, KeyCode::Char(_) | KeyCode::BackTab) {
            mods.remove(KeyModifiers::SHIFT);
        }
        self.code == ev.code && self.mods == mods
    }
}

/// Keys shown together in the footer, e.g. "↑↓ move".
pub struct Binding {
    pub hint: &'static str,
    pub what: &'static str,
    pub keys: &'static [(Key, Act)],
}

impl Binding {
    pub fn act_for(&self, ev: &KeyEvent) -> Option<Act> {
        self.keys.iter().find(|(k, _)| k.matches(ev)).map(|(_, a)| *a)
    }

    /// The same keys, saying what they do here.
    pub const fn doing(self, what: &'static str) -> Self {
        Self { what, ..self }
    }
}

const fn bind(hint: &'static str, what: &'static str, keys: &'static [(Key, Act)]) -> Binding {
    Binding { hint, what, keys }
}

/// One letter for one act.
macro_rules! key {
    ($hint:literal, $c:literal, $what:literal, $act:ident) => {
        bind($hint, $what, &[(ch($c), Act::$act)])
    };
}

const UP_DOWN: &[(Key, Act)] = &[
    (plain(KeyCode::Up), Act::Up),
    (plain(KeyCode::Down), Act::Down),
    (ch('k'), Act::Up),
    (ch('j'), Act::Down),
    (plain(KeyCode::Home), Act::First),
    (plain(KeyCode::End), Act::Last),
];
pub const MOVE: Binding = bind("↑↓/jk", "move", UP_DOWN);
pub const SCROLL: Binding = bind("↑↓/jk", "scroll", UP_DOWN);
pub const PAGE: Binding =
    bind("PgUp/PgDn", "page", &[(plain(KeyCode::PageUp), Act::PageUp), (plain(KeyCode::PageDown), Act::PageDown)]);
pub const DETAILS: Binding = PAGE.doing("scroll details");
pub const EDIT: Binding = key!("e", 'e', "open file", Edit);
pub const EDIT_SERVER: Binding = key!("e", 'e', "edit server settings", Edit);
pub const RELOAD: Binding = key!("r", 'r', "reload", Reload);
pub const OPEN: Binding = bind("Enter", "edit", &[(plain(KeyCode::Enter), Act::Open)]);

pub const NEW_ROLE: Binding = key!("n", 'n', "new role", New);
pub const NEW_CHANNEL: Binding = key!("n", 'n', "new channel", New);
pub const NEW_CATEGORY: Binding = key!("g", 'g', "new category", NewCategory);
pub const NEW_PROJECT: Binding = key!("n", 'n', "new project", New);
pub const NEW_POSITION: Binding = key!("n", 'n', "new position", New);
pub const NEW_FEED: Binding = key!("n", 'n', "follow a feed", New);
pub const NEW_RESOURCE: Binding = key!("n", 'n', "share a link", New);
pub const APPOINT: Binding = key!("n", 'n', "appoint", New);
pub const REORDER: Binding = bind(
    "Shift+↑↓/JK",
    "reorder",
    &[
        (shift(KeyCode::Up), Act::MoveUp),
        (shift(KeyCode::Down), Act::MoveDown),
        (ch('K'), Act::MoveUp),
        (ch('J'), Act::MoveDown),
    ],
);
pub const MOVE_TO: Binding = key!("m", 'm', "move to category", MoveTo);
pub const REMOVE: Binding = key!("x", 'x', "remove", Remove);
pub const ARCHIVE: Binding = key!("x", 'x', "archive", Remove);
pub const REMOVE_POSITION: Binding = key!("x", 'x', "remove position", Remove);
pub const REMOVE_FEED: Binding = key!("x", 'x', "remove feed", Remove);
pub const REMOVE_CATEGORY: Binding = key!("x", 'x', "remove category", Remove);
pub const ARCHIVE_POST: Binding = key!("x", 'x', "archive post", Remove);
pub const CLOSE_IDEA: Binding = key!("x", 'x', "close idea", Remove);
pub const PERMISSIONS: Binding = key!("p", 'p', "permissions", Permissions);
pub const POST: Binding = key!("b", 'b', "bot post", Post);
pub const STATUS: Binding = key!("s", 's', "status", SetStatus);
pub const APPLICATIONS: Binding = key!("o", 'o', "open/close applications", ToggleApplications);
pub const BRIEF: Binding = key!("b", 'b', "brief", Brief);
pub const ADD_LEAD: Binding = key!("l", 'l', "add lead", AddLead);
pub const REMOVE_LEAD: Binding = key!("L", 'L', "remove lead", RemoveLead);
pub const OPEN_POSITION: Binding = key!("o", 'o', "open/close applications", ToggleOpen);
pub const INTRO: Binding = key!("i", 'i', "intro", Intro);
pub const QUESTIONS: Binding = key!("Q", 'Q', "questions", Questions);
pub const ACCEPT: Binding = key!("a", 'a', "accept", Accept);
pub const REJECT: Binding = key!("x", 'x', "reject", Reject);
pub const MAKE_LEAD: Binding = key!("l", 'l', "make lead", MakeLead);
pub const HISTORY: Binding = key!("h", 'h', "history", History);
pub const BACK_TO_QUEUE: Binding =
    bind("h/Esc", "back to the queue", &[(ch('h'), Act::History), (plain(KeyCode::Esc), Act::History)]);
pub const RESTORE: Binding = key!("g", 'g', "restore", Restore);
pub const FORGET: Binding = key!("f", 'f', "forget", Forget);
pub const PAUSE: Binding = key!("p", 'p', "pause/resume", TogglePause);
pub const FETCH: Binding = key!("f", 'f', "plan", Fetch);
pub const APPLY: Binding = key!("a", 'a', "apply", Apply);
pub const PULL: Binding = key!("p", 'p', "pull edits made in Discord", Pull);
pub const DRIFT: Binding = key!("d", 'd', "who changed what (drift)", Drift);
pub const TABS: Binding = bind(
    "Tab/0-9",
    "screens",
    &[
        (plain(KeyCode::Tab), Act::NextTab),
        (plain(KeyCode::BackTab), Act::PrevTab),
        (ch('1'), Act::Go(Tab::Overview)),
        (ch('2'), Act::Go(Tab::Queue)),
        (ch('3'), Act::Go(Tab::Members)),
        (ch('4'), Act::Go(Tab::Projects)),
        (ch('5'), Act::Go(Tab::Positions)),
        (ch('6'), Act::Go(Tab::Feeds)),
        (ch('7'), Act::Go(Tab::Resources)),
        (ch('8'), Act::Go(Tab::Channels)),
        (ch('9'), Act::Go(Tab::Roles)),
        (ch('0'), Act::Go(Tab::Changes)),
    ],
);
pub const HELP: Binding = key!("?", '?', "help", Help);
pub const QUIT: Binding = key!("q", 'q', "quit", Quit);

pub const YES: Binding = key!("y", 'y', "yes", Save);
pub const NO: Binding = bind("n/Esc", "back", &[(ch('n'), Act::Cancel), (plain(KeyCode::Esc), Act::Cancel)]);
pub const CLOSE: Binding =
    bind("Esc/?", "close", &[(plain(KeyCode::Esc), Act::Cancel), (ch('?'), Act::Cancel), (ch('q'), Act::Cancel)]);
/// Pick from a list shown top to bottom.
pub const PICK: Binding = bind(
    "↑↓",
    "choose",
    &[
        (plain(KeyCode::Up), Act::Left),
        (plain(KeyCode::Down), Act::Right),
        (plain(KeyCode::Left), Act::Left),
        (plain(KeyCode::Right), Act::Right),
    ],
);
pub const CHOOSE: Binding = bind(
    "←→/Space",
    "choose",
    &[(plain(KeyCode::Left), Act::Left), (plain(KeyCode::Right), Act::Right), (ch(' '), Act::Right)],
);
pub const FIELDS: Binding = bind(
    "Tab/↑↓",
    "field",
    &[
        (plain(KeyCode::Tab), Act::Down),
        (plain(KeyCode::Down), Act::Down),
        (plain(KeyCode::BackTab), Act::Up),
        (plain(KeyCode::Up), Act::Up),
    ],
);
pub const CURSOR: Binding = bind(
    "←→",
    "cursor",
    &[
        (plain(KeyCode::Left), Act::Left),
        (plain(KeyCode::Right), Act::Right),
        (plain(KeyCode::Home), Act::Home),
        (plain(KeyCode::End), Act::End),
    ],
);
pub const ERASE: Binding = bind("⌫", "erase", &[(plain(KeyCode::Backspace), Act::Backspace)]);
pub const CLEAR: Binding = bind("Ctrl+U", "clear", &[(ctrl('u'), Act::Clear)]);
pub const NEXT: Binding = bind("Enter", "next field", &[(plain(KeyCode::Enter), Act::Down)]);
/// Enter on a form's last field, or the status picker: `doing` says what it does.
pub const ENTER: Binding = bind("Enter", "save", &[(plain(KeyCode::Enter), Act::Save)]);
/// Save a form from any field.
pub const SAVE_NOW: Binding = bind("Ctrl+S", "save", &[(ctrl('s'), Act::Save)]);
pub const CANCEL: Binding = bind("Esc", "cancel", &[(plain(KeyCode::Esc), Act::Cancel)]);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letters_match_by_case_and_arrows_by_shift() {
        let up = KeyEvent::from(KeyCode::Up);
        let shift_up = KeyEvent::new(KeyCode::Up, KeyModifiers::SHIFT);
        assert_eq!(MOVE.act_for(&up), Some(Act::Up));
        assert_eq!(MOVE.act_for(&shift_up), None);
        assert_eq!(REORDER.act_for(&shift_up), Some(Act::MoveUp));
        // Terminals send Shift with capitals.
        assert_eq!(REORDER.act_for(&KeyEvent::new(KeyCode::Char('K'), KeyModifiers::SHIFT)), Some(Act::MoveUp));
        assert_eq!(MOVE.act_for(&KeyEvent::from(KeyCode::Char('j'))), Some(Act::Down));
        assert_eq!(CLEAR.act_for(&KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL)), Some(Act::Clear));
        assert_eq!(CLEAR.act_for(&KeyEvent::from(KeyCode::Char('u'))), None);
    }
}

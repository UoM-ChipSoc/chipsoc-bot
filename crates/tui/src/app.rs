//! All TUI state and how keys and job results change it. No drawing here.
//! What each tab's keys do is in [`crate::edits`].

use std::path::PathBuf;

use chipsoc_client::remote::{Entry, FeedInfo, Past, Pending, Post, Roster, Status};
use chipsoc_config::authoring::{self, Kind, Report};
use chipsoc_config::ops::{Change, Op, Schema};
use chipsoc_config::schema::Project;
use chipsoc_config::{BotConfig, Config, Snapshot};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::changes::Changes;
use crate::edits::{Target, Then};
use crate::form::{Form, StatusPicker};
use crate::keys::{self, Act, Binding};
use crate::worker::{Area, Data, Job, Msg};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Overview,
    Roles,
    Channels,
    Projects,
    Positions,
    Queue,
    Members,
    Feeds,
    Resources,
    Changes,
}

impl Tab {
    pub const ALL: [Self; 10] = [
        Self::Overview,
        Self::Roles,
        Self::Channels,
        Self::Projects,
        Self::Positions,
        Self::Queue,
        Self::Members,
        Self::Feeds,
        Self::Resources,
        Self::Changes,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Self::Overview => "Overview",
            Self::Roles => "Roles",
            Self::Channels => "Channels",
            Self::Projects => "Projects",
            Self::Positions => "Positions",
            Self::Queue => "Queue",
            Self::Members => "Members",
            Self::Feeds => "Feeds",
            Self::Resources => "Resources",
            Self::Changes => "Apply",
        }
    }

    /// The number key that shows it.
    pub fn key(self) -> char {
        let i = self.index();
        if i == 9 { '0' } else { char::from(b'1' + i as u8) }
    }

    pub fn index(self) -> usize {
        Self::ALL.iter().position(|t| *t == self).unwrap_or(0)
    }

    fn step(self, by: isize) -> Self {
        Self::ALL[(self.index() as isize + by).rem_euclid(Self::ALL.len() as isize) as usize]
    }
}

/// Something the event loop must do for the app.
pub enum Command {
    Discord(Job),
    /// Open a config file at a line in $EDITOR.
    Edit(PathBuf, usize),
    /// Edit this text in $EDITOR, then hand it to [`App::text_edited`].
    Text(String, Target),
}

pub enum Confirm {
    Apply,
    Pull,
    /// Ctrl+C while something runs.
    Quit,
    /// Anything else that changes things: what it does, and the job that does it.
    Do {
        title: String,
        lines: Vec<String>,
        job: Job,
    },
}

pub enum Overlay {
    Help,
    Confirm(Confirm),
    Status(StatusPicker),
    Form(Form<Then>),
}

pub struct Notice {
    pub text: String,
    pub error: bool,
}

/// Admin data: not asked for yet, couldn't be had, or here.
pub enum Loaded<T> {
    No,
    Failed(String),
    Got(T),
}

impl<T> Loaded<T> {
    pub fn get(&self) -> Option<&T> {
        match self {
            Self::Got(t) => Some(t),
            _ => None,
        }
    }
}

/// Resources, and ideas (which may not be set up).
pub type Forums = (Vec<Post>, Result<Vec<Post>, String>);

/// The audit log, and the config's latest changes (if it has a history).
pub type Activity = (Vec<Entry>, Result<Vec<chipsoc_client::remote::Done>, String>);

pub struct App {
    pub dir: PathBuf,
    pub demo: bool,
    /// Edits go to the bot's web server, and the tabs show its copy of the config.
    pub remote: bool,
    /// The local files; None when they don't load (`report` says why).
    pub cfg: Option<Config>,
    pub report: Report,
    /// The config as its files say, for the editing tabs.
    pub schema: Option<Schema>,
    pub schema_error: Option<String>,
    pub status: Loaded<Status>,
    pub tab: Tab,
    pub overview_scroll: usize,
    /// How far the details pane is scrolled; back to the top on a new selection.
    pub detail_scroll: usize,
    /// The furthest it can scroll, as last drawn.
    pub detail_max: std::cell::Cell<usize>,
    /// The selected row on each tab.
    pub sel: [usize; 10],
    pub changes: Changes,
    pub queue: Loaded<Vec<Pending>>,
    pub history: Loaded<Vec<Past>>,
    pub show_history: bool,
    pub roster: Loaded<Roster>,
    pub feeds: Loaded<Vec<FeedInfo>>,
    pub resources: Loaded<Forums>,
    pub activity: Loaded<Activity>,
    pub overlay: Option<Overlay>,
    /// A form whose save is running: shown again with the problem if it's refused.
    pub(crate) waiting: Option<Form<Then>>,
    /// What's running, for the spinner.
    pub busy: Option<&'static str>,
    pub ticks: usize,
    pub notice: Option<Notice>,
    /// `connected to <server> as <user>`, when jobs go through the bot's web server.
    pub connected: Option<String>,
    pub quit: bool,
}

/// What a plan is made from, besides Discord.
fn plan_inputs(c: &Config) -> (&Snapshot, &[Project], &BotConfig) {
    (&c.desired, &c.projects, &c.bot)
}

impl App {
    #[cfg(test)]
    pub fn new(dir: PathBuf, demo: bool) -> Self {
        Self::with(dir, demo, false)
    }

    /// `remote`: the config tabs wait for the server's copy (see [`App::startup`]).
    pub fn with(dir: PathBuf, demo: bool, remote: bool) -> Self {
        let mut app = Self {
            dir,
            demo,
            remote,
            cfg: None,
            report: Report::default(),
            schema: None,
            schema_error: None,
            status: Loaded::No,
            tab: Tab::Overview,
            overview_scroll: 0,
            detail_scroll: 0,
            detail_max: Default::default(),
            sel: [0; 10],
            changes: Changes::default(),
            queue: Loaded::No,
            history: Loaded::No,
            show_history: false,
            roster: Loaded::No,
            feeds: Loaded::No,
            resources: Loaded::No,
            activity: Loaded::No,
            overlay: None,
            waiting: None,
            busy: None,
            ticks: 0,
            notice: None,
            connected: None,
            quit: false,
        };
        app.refresh();
        app
    }

    /// Jobs to run when the TUI starts: the status, and the server's config when remote.
    pub fn startup(&self) -> Vec<Job> {
        let mut v = vec![Job::Status, Job::Load(Area::Activity)];
        if self.remote {
            v.push(Job::Schema);
        }
        v
    }

    /// Reload the local files and re-run check. True if what a plan is made from changed.
    pub fn refresh(&mut self) -> bool {
        let (cfg, report) = match Config::load(&self.dir) {
            Ok(c) => {
                let r = authoring::check_config(&c);
                (Some(c), r)
            }
            Err(e) => (None, Report { errors: vec![format!("{e:#}")], warnings: Vec::new() }),
        };
        let changed = self.cfg.as_ref().map(plan_inputs) != cfg.as_ref().map(plan_inputs);
        if changed && !self.remote {
            self.changes.files_changed();
        }
        self.cfg = cfg;
        self.report = report;
        if !self.remote {
            match chipsoc_config::ops::schema(&self.dir) {
                Ok(s) => self.set_schema(Some(s), None),
                Err(e) => self.set_schema(None, Some(format!("{e:#}"))),
            }
        }
        changed
    }

    fn set_schema(&mut self, schema: Option<Schema>, error: Option<String>) {
        self.schema = schema;
        self.schema_error = error;
        for t in Tab::ALL {
            let n = self.len(t);
            self.sel[t.index()] = self.sel[t.index()].min(n.saturating_sub(1));
        }
    }

    /// Config problems that stop an apply: the server's when remote.
    pub fn problems(&self) -> &[String] {
        match (&self.status, self.remote) {
            (Loaded::Got(s), true) => &s.errors,
            _ => &self.report.errors,
        }
    }

    pub fn sel(&self) -> usize {
        self.sel[self.tab.index()]
    }

    /// Rows on a list tab.
    pub fn len(&self, tab: Tab) -> usize {
        let s = self.schema.as_ref();
        match tab {
            Tab::Overview | Tab::Changes => 0,
            Tab::Roles => s.map_or(0, |s| s.roles.len()),
            Tab::Channels => s.map_or(0, |s| crate::lists::spots(s).len()),
            Tab::Projects => s.map_or(0, |s| s.projects.len()),
            Tab::Positions => s.map_or(0, |s| s.bot.positions.len()),
            Tab::Queue if self.show_history => self.history.get().map_or(0, Vec::len),
            Tab::Queue => self.queue.get().map_or(0, Vec::len),
            Tab::Members => self.roster.get().map_or(0, |r| crate::lists::people(r).len()),
            Tab::Feeds => self.feeds.get().map_or(0, Vec::len),
            Tab::Resources => self.resources.get().map_or(0, |(r, i)| r.len() + i.as_ref().map_or(0, Vec::len)),
        }
    }

    /// The keys that work right now, in footer order.
    pub fn bindings(&self) -> Vec<Binding> {
        use keys::*;
        match &self.overlay {
            Some(Overlay::Help) => vec![CLOSE],
            Some(Overlay::Confirm(_)) => vec![YES, NO],
            Some(Overlay::Status(_)) => vec![PICK, ENTER.doing("set"), CANCEL],
            Some(Overlay::Form(f)) => {
                let mut v = vec![FIELDS];
                if f.on_choice() {
                    v.push(CHOOSE);
                } else {
                    v.extend([CURSOR, ERASE, CLEAR]);
                }
                if f.on_last() {
                    v.push(ENTER.doing(f.then.verb()));
                } else {
                    v.extend([NEXT, SAVE_NOW.doing(f.then.verb())]);
                }
                v.push(CANCEL);
                v
            }
            None => {
                let idle = self.busy.is_none();
                let mut v = if idle { self.tab_keys() } else { self.nav_keys() };
                if idle {
                    v.push(RELOAD);
                }
                v.extend([TABS, HELP]);
                // Quitting mid-job would cut an apply short.
                v.extend(idle.then_some(QUIT));
                v
            }
        }
    }

    /// Just moving about, while a job runs.
    fn nav_keys(&self) -> Vec<Binding> {
        match self.tab {
            Tab::Overview | Tab::Changes => vec![keys::SCROLL, keys::PAGE],
            t if self.len(t) > 0 => vec![keys::MOVE],
            _ => Vec::new(),
        }
    }

    pub fn on_key(&mut self, key: KeyEvent) -> Option<Command> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && key.code == KeyCode::Char('c') {
            // Mid-job, ask first; a second Ctrl+C quits anyway.
            if self.busy.is_some() && !matches!(self.overlay, Some(Overlay::Confirm(Confirm::Quit))) {
                self.overlay = Some(Overlay::Confirm(Confirm::Quit));
            } else {
                self.quit = true;
            }
            return None;
        }
        if let Some(act) = self.bindings().iter().find_map(|b| b.act_for(&key)) {
            return self.act(act);
        }
        if let (Some(Overlay::Form(f)), KeyCode::Char(c), false) = (&mut self.overlay, key.code, ctrl) {
            f.type_char(c);
        }
        None
    }

    /// Pasted text goes into the form being filled in, on one line.
    pub fn on_paste(&mut self, text: &str) {
        if let Some(Overlay::Form(f)) = &mut self.overlay {
            f.paste(text);
        }
    }

    fn act(&mut self, act: Act) -> Option<Command> {
        if self.overlay.is_none() {
            self.notice = None;
        }
        match act {
            Act::Quit => self.quit = true,
            Act::Help => self.overlay = Some(Overlay::Help),
            Act::Go(t) => return self.go(t),
            Act::NextTab => return self.go(self.tab.step(1)),
            Act::PrevTab => return self.go(self.tab.step(-1)),
            Act::Up => self.move_by(-1),
            Act::Down => self.move_by(1),
            Act::First => self.move_by(-10_000),
            Act::Last => self.move_by(10_000),
            Act::PageUp => self.page(-1),
            Act::PageDown => self.page(1),
            Act::Left => self.cycle(-1),
            Act::Right => self.cycle(1),
            Act::Home | Act::End | Act::Backspace | Act::Clear => {
                if let Some(Overlay::Form(f)) = &mut self.overlay {
                    match act {
                        Act::Home => f.cursor_to(false),
                        Act::End => f.cursor_to(true),
                        Act::Backspace => f.backspace(),
                        _ => f.clear(),
                    }
                }
            }
            Act::Edit => return self.edit_file(),
            Act::Reload => return self.reload(),
            Act::Fetch => return Some(self.start(Job::Plan)),
            Act::Drift => return Some(self.start(Job::Drift)),
            Act::Apply => self.overlay = Some(Overlay::Confirm(Confirm::Apply)),
            Act::Pull => self.overlay = Some(Overlay::Confirm(Confirm::Pull)),
            Act::Save => return self.save(),
            Act::Cancel => self.overlay = None,
            other => return self.tab_act(other),
        }
        None
    }

    /// Show a tab, loading what it shows the first time.
    fn go(&mut self, tab: Tab) -> Option<Command> {
        self.tab = tab;
        self.detail_scroll = 0;
        let area = self.area()?;
        // Not here yet, or it failed last time (perhaps it's set up now).
        let unloaded = match area {
            Area::Queue => !matches!(self.queue, Loaded::Got(_)),
            Area::History => !matches!(self.history, Loaded::Got(_)),
            Area::Members => !matches!(self.roster, Loaded::Got(_)),
            Area::Feeds => !matches!(self.feeds, Loaded::Got(_)),
            Area::Resources => !matches!(self.resources, Loaded::Got(_)),
            Area::Activity => !matches!(self.activity, Loaded::Got(_)),
        };
        (unloaded && self.busy.is_none()).then(|| self.start(Job::Load(area)))
    }

    /// The admin data the current tab shows.
    fn area(&self) -> Option<Area> {
        Some(match self.tab {
            Tab::Overview => Area::Activity,
            Tab::Queue if self.show_history => Area::History,
            Tab::Queue => Area::Queue,
            Tab::Members => Area::Members,
            Tab::Feeds => Area::Feeds,
            Tab::Resources => Area::Resources,
            _ => return None,
        })
    }

    fn reload(&mut self) -> Option<Command> {
        self.refresh();
        if let Some(area) = self.area() {
            return Some(self.start(Job::Load(area)));
        }
        if self.remote {
            return Some(self.start(Job::Schema));
        }
        self.info("Reloaded the files.");
        None
    }

    fn move_by(&mut self, by: isize) {
        match &mut self.overlay {
            Some(Overlay::Form(f)) => return f.move_field(by),
            Some(Overlay::Help) => return,
            _ => {}
        }
        let step = |i: usize, len: usize| i.saturating_add_signed(by).min(len.saturating_sub(1));
        match self.tab {
            Tab::Overview => self.overview_scroll = step(self.overview_scroll, 200),
            Tab::Changes => self.changes.scroll_by(by),
            t => {
                self.sel[t.index()] = step(self.sel[t.index()], self.len(t));
                self.detail_scroll = 0;
            }
        }
    }

    /// PgUp/PgDn: a page of the overview or plan, else of the details pane.
    fn page(&mut self, by: isize) {
        let by = by * PAGE;
        match self.tab {
            Tab::Overview => self.overview_scroll = self.overview_scroll.saturating_add_signed(by).min(200),
            Tab::Changes => self.changes.scroll_by(by),
            _ => self.detail_scroll = self.detail_scroll.saturating_add_signed(by).min(self.detail_max.get()),
        }
    }

    fn cycle(&mut self, by: isize) {
        match &mut self.overlay {
            Some(Overlay::Form(f)) => f.cycle(by),
            Some(Overlay::Status(p)) => p.cycle(by),
            _ => {}
        }
    }

    pub(crate) fn info(&mut self, text: impl Into<String>) {
        self.notice = Some(Notice { text: text.into(), error: false });
    }

    pub(crate) fn error(&mut self, text: impl Into<String>) {
        self.notice = Some(Notice { text: text.into(), error: true });
    }

    /// A note for after a config edit: what's next (before any notes), or what's wrong.
    fn next_step(&mut self, done: &str, notes: &[String]) {
        let done = done.trim();
        let notes = notes.iter().map(|n| format!(" Note: {n}")).collect::<String>();
        let next = "To send it: 0, then f (plan), then a (apply).";
        if self.remote {
            return self.info(format!("{done} Saved on the server. {next}{notes}"));
        }
        match self.report.errors.len() {
            0 => self.info(format!("{done} {next}{notes}")),
            n => self.error(format!(
                "{done} But the files now have {}; see Overview.{notes}",
                crate::ui::count(n, "problem", "problems")
            )),
        }
    }

    /// The file and key `e` opens on this tab.
    fn edit_target(&self) -> Option<(Kind, Option<String>)> {
        let s = self.schema.as_ref();
        Some(match self.tab {
            Tab::Overview => (Kind::Server, None),
            Tab::Roles => match s?.roles.get(self.sel()) {
                Some(r) => (Kind::Role, Some(r.key.clone())),
                None => (Kind::Roles, None),
            },
            Tab::Channels => match crate::lists::spots(s?).get(self.sel())? {
                crate::lists::Spot::Category(k) => (Kind::Category, Some(k.clone())),
                crate::lists::Spot::Channel(k, _) => (Kind::Channel, Some(k.clone())),
            },
            Tab::Projects => match s?.projects.get(self.sel()) {
                Some(p) => (Kind::Project, Some(p.slug.clone())),
                None => (Kind::Projects, None),
            },
            Tab::Positions => (Kind::Bot, None),
            _ => return None,
        })
    }

    fn edit_file(&mut self) -> Option<Command> {
        let (kind, key) = self.edit_target()?;
        match authoring::locate(&self.dir, kind, key.as_deref()) {
            Ok((path, line)) => Some(Command::Edit(path, line)),
            Err(e) => {
                self.error(format!("Can't find where that's defined: {e:#}"));
                None
            }
        }
    }

    /// After the editor closes on a config file: re-check and say how it went.
    pub fn edited(&mut self, result: anyhow::Result<()>) {
        let changed = self.refresh();
        match result {
            Ok(()) if changed && self.remote => {
                self.info("Saved here. Commit and push it: the bot's server plans from its own copy.");
            }
            Ok(()) if changed => self.next_step("Saved.", &[]),
            Ok(()) => self.info("Nothing changed."),
            Err(e) => self.error(format!("The editor didn't run: {e:#}")),
        }
    }

    fn save(&mut self) -> Option<Command> {
        match self.overlay.take()? {
            Overlay::Confirm(Confirm::Apply) => {
                let plan = self.changes.plan()?.clone();
                self.changes.applying(plan.changes());
                Some(self.start(Job::Apply(Box::new(plan))))
            }
            Overlay::Confirm(Confirm::Pull) => Some(self.start(Job::Pull)),
            Overlay::Confirm(Confirm::Quit) => {
                self.quit = true;
                None
            }
            Overlay::Help => None,
            Overlay::Confirm(Confirm::Do { job, .. }) => Some(self.start(job)),
            Overlay::Status(p) => self.status_picked(p),
            Overlay::Form(f) => self.submit(f),
        }
    }

    /// Run config edits; nothing to run is said so.
    pub(crate) fn ops(&mut self, ops: Vec<Op>) -> Option<Command> {
        if ops.is_empty() {
            self.info("Nothing changed.");
            return None;
        }
        Some(self.start(Job::Ops(ops)))
    }

    pub(crate) fn confirm(&mut self, title: impl Into<String>, lines: Vec<String>, job: Job) {
        self.overlay = Some(Overlay::Confirm(Confirm::Do { title: title.into(), lines, job }));
    }

    pub(crate) fn start(&mut self, job: Job) -> Command {
        self.busy = Some(match &job {
            Job::Plan => "Planning: checking Discord for differences…",
            Job::Apply(_) => "Sending changes to Discord…",
            Job::Pull => "Copying Discord's edits into your files…",
            Job::Drift => "Sorting out who changed what…",
            Job::Status | Job::Schema => "Reading the config…",
            Job::Ops(_) => "Saving…",
            Job::Load(_) => "Loading…",
            Job::Admin(_) => "Working…",
        });
        Command::Discord(job)
    }

    /// A job's result. Returns a load the current tab still needs, if the
    /// tab was switched to while something else ran.
    pub fn on_msg(&mut self, msg: Msg) -> Option<Command> {
        if !matches!(msg, Msg::Step(..) | Msg::Schema(_, false)) {
            self.busy = None;
        }
        self.take(msg);
        self.load_shown()
    }

    /// Load what the current tab shows if it was never asked for.
    fn load_shown(&mut self) -> Option<Command> {
        let area = self.area().filter(|_| self.busy.is_none())?;
        let never = match area {
            Area::Queue => matches!(self.queue, Loaded::No),
            Area::History => matches!(self.history, Loaded::No),
            Area::Members => matches!(self.roster, Loaded::No),
            Area::Feeds => matches!(self.feeds, Loaded::No),
            Area::Resources => matches!(self.resources, Loaded::No),
            Area::Activity => matches!(self.activity, Loaded::No),
        };
        never.then(|| self.start(Job::Load(area)))
    }

    fn take(&mut self, msg: Msg) {
        match msg {
            Msg::Planned(Ok(plan)) => self.changes.planned(plan),
            Msg::Step(action, result) => self.changes.step(&action, result),
            Msg::Applied(Ok(())) => {
                self.refresh();
                self.info("Applied. Press f to plan again and check that nothing is left.");
            }
            Msg::Pulled(Ok(notes)) => {
                self.refresh();
                self.info(match self.connected {
                    Some(_) => "Done, in the server's copy of the config; git pull to get it here.",
                    None => "Done. Review the file changes with git before committing.",
                });
                self.changes.pulled(notes);
            }
            Msg::Applied(Err(e)) => {
                self.changes = Changes::default();
                self.error(e);
            }
            Msg::Planned(Err(e)) | Msg::Pulled(Err(e)) | Msg::Drifted(Err(e)) => self.error(e),
            Msg::Drifted(Ok(d)) => self.changes.drifted(d),
            Msg::Status(Ok(s)) => self.status = Loaded::Got(s),
            Msg::Status(Err(e)) => self.status = Loaded::Failed(e),
            Msg::Schema(Ok(s), _) => self.set_schema(Some(*s), None),
            Msg::Schema(Err(e), _) => {
                let keep = self.schema.take();
                self.set_schema(keep, Some(e));
            }
            Msg::Edited(r) => self.edited_config(r),
            Msg::Loaded(area, r) => self.loaded(area, r),
            Msg::Did(Ok(text)) => {
                self.waiting = None;
                self.refresh();
                self.info(text.trim());
            }
            Msg::Did(Err(e)) => self.refused(e),
        }
    }

    fn edited_config(&mut self, r: Result<Vec<Change>, String>) {
        let changes = match r {
            Ok(c) => c,
            Err(e) => return self.refused(e),
        };
        let form = self.waiting.take();
        self.refresh();
        if changes.iter().any(|c| !c.files.is_empty()) {
            self.changes.files_changed();
        }
        // Show what was just added.
        if form.is_some_and(|f| matches!(f.then, Then::NewProject | Then::NewPosition)) {
            let i = self.tab.index();
            self.sel[i] = self.len(self.tab).saturating_sub(1);
        }
        let text: Vec<String> =
            changes.iter().map(|c| format!("{}.", past_tense(c.summary.trim_end_matches('.')))).collect();
        let notes: Vec<String> = changes.iter().flat_map(|c| c.warnings.iter().cloned()).collect();
        if changes.iter().all(|c| c.files.is_empty()) {
            return self.info("Nothing to change.");
        }
        self.next_step(&text.join(" "), &notes);
    }

    /// A save was refused: back to its form with why, else say so.
    fn refused(&mut self, e: String) {
        match self.waiting.take() {
            Some(mut f) => {
                f.error = Some(e);
                self.overlay = Some(Overlay::Form(f));
            }
            None => self.error(format!("Not done: {e}")),
        }
    }

    fn loaded(&mut self, area: Area, r: Result<Data, String>) {
        let failed = |e: &String| e.clone();
        match (area, r) {
            (_, Ok(Data::Queue(q))) => self.queue = Loaded::Got(q),
            (_, Ok(Data::History(h))) => self.history = Loaded::Got(h),
            (_, Ok(Data::Members(m))) => self.roster = Loaded::Got(m),
            (_, Ok(Data::Feeds(f))) => self.feeds = Loaded::Got(f),
            (_, Ok(Data::Resources(r, i))) => self.resources = Loaded::Got((r, i)),
            (_, Ok(Data::Activity(a, c))) => self.activity = Loaded::Got((a, c)),
            (Area::Queue, Err(e)) => self.queue = Loaded::Failed(failed(&e)),
            (Area::History, Err(e)) => self.history = Loaded::Failed(e),
            (Area::Members, Err(e)) => self.roster = Loaded::Failed(e),
            (Area::Feeds, Err(e)) => self.feeds = Loaded::Failed(e),
            (Area::Resources, Err(e)) => self.resources = Loaded::Failed(e),
            (Area::Activity, Err(e)) => self.activity = Loaded::Failed(e),
        }
        for t in Tab::ALL {
            let n = self.len(t);
            self.sel[t.index()] = self.sel[t.index()].min(n.saturating_sub(1));
        }
    }

    pub fn tick(&mut self) {
        self.ticks = self.ticks.wrapping_add(1);
    }

    pub(crate) fn toggle_history(&mut self) -> Option<Command> {
        self.detail_scroll = 0;
        self.show_history = !self.show_history;
        self.sel[Tab::Queue.index()] = 0;
        self.go(Tab::Queue)
    }
}

/// Rows PgUp/PgDn move.
const PAGE: isize = 10;

/// "Add project Golem" as "Added project Golem": what was done, not what to do.
fn past_tense(summary: &str) -> String {
    const VERBS: [(&str, &str); 6] = [
        ("Add ", "Added "),
        ("Remove ", "Removed "),
        ("Move ", "Moved "),
        ("Rename ", "Renamed "),
        ("Set ", "Set "),
        ("Make ", "Made "),
    ];
    VERBS
        .iter()
        .find_map(|(now, then)| summary.strip_prefix(now).map(|rest| format!("{then}{rest}")))
        .unwrap_or_else(|| summary.to_owned())
}

//! The Apply tab: a plan, an apply in progress, pulled edits, or
//! the drift report.

use chipsoc_client::remote::Drift;
use chipsoc_model::{Action, Plan};

pub enum View {
    /// Nothing fetched yet, or it went stale; says what to do.
    Prompt(&'static str),
    Plan(Plan),
    Applying {
        total: usize,
        steps: Vec<(String, Result<(), String>)>,
    },
    Pulled(Vec<chipsoc_engine::Pulled>),
    /// The plan's changes by who made them.
    Drift(Drift),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mark {
    Create,
    Edit,
    Move,
    Remove,
    Skip,
    Detail,
    Warn,
    Ok,
    Fail,
    Note,
}

pub struct Row {
    pub mark: Mark,
    pub text: String,
}

pub const START: &str = "Press f to plan: see what would change in Discord. d shows who changed what (drift).";
pub const STALE: &str = "The config changed since the last plan. Press f to plan again.";

pub struct Changes {
    pub view: View,
    pub scroll: usize,
}

impl Default for Changes {
    fn default() -> Self {
        Self { view: View::Prompt(START), scroll: 0 }
    }
}

fn row(mark: Mark, text: impl Into<String>) -> Row {
    Row { mark, text: text.into() }
}

fn mark(a: &Action) -> Mark {
    match a {
        Action::Create { .. } => Mark::Create,
        Action::Edit { .. } => Mark::Edit,
        Action::Reorder { .. } | Action::Archive { .. } => Mark::Move,
        Action::Delete { .. } => Mark::Remove,
        Action::Skip { .. } => Mark::Skip,
    }
}

impl Changes {
    fn show(&mut self, view: View) {
        self.view = view;
        self.scroll = 0;
    }

    pub fn planned(&mut self, plan: Plan) {
        self.show(View::Plan(plan));
    }

    pub fn applying(&mut self, total: usize) {
        self.show(View::Applying { total, steps: Vec::new() });
    }

    pub fn step(&mut self, action: &Action, result: Result<(), String>) {
        if let View::Applying { steps, .. } = &mut self.view {
            steps.push((action.headline(), result));
        }
    }

    pub fn pulled(&mut self, notes: Vec<chipsoc_engine::Pulled>) {
        self.show(View::Pulled(notes));
    }

    pub fn drifted(&mut self, d: Drift) {
        self.show(View::Drift(d));
    }

    /// Drop a plan made from files that have since changed.
    pub fn files_changed(&mut self) {
        if matches!(self.view, View::Plan(_)) {
            self.show(View::Prompt(STALE));
        }
    }

    pub fn plan(&self) -> Option<&Plan> {
        match &self.view {
            View::Plan(p) => Some(p),
            _ => None,
        }
    }

    pub fn scroll_by(&mut self, by: isize) {
        let max = self.rows().len().saturating_sub(1);
        self.scroll = self.scroll.saturating_add_signed(by).min(max);
    }

    pub fn rows(&self) -> Vec<Row> {
        match &self.view {
            View::Prompt(_) => Vec::new(),
            View::Plan(p) => plan_rows(p),
            View::Applying { total, steps } => {
                let mut rows = Vec::new();
                for (line, result) in steps {
                    match result {
                        Ok(()) => rows.push(row(Mark::Ok, format!("✓ {line}"))),
                        Err(e) => {
                            rows.push(row(Mark::Fail, format!("✗ {line}")));
                            rows.push(row(Mark::Detail, format!("    {e}")));
                        }
                    }
                }
                if let Some(left) = total.checked_sub(steps.len()).filter(|n| *n > 0) {
                    rows.push(row(Mark::Note, format!("… {left} more to go")));
                }
                rows
            }
            View::Pulled(notes) => notes
                .iter()
                .map(|n| {
                    row(
                        if matches!(n, chipsoc_engine::Pulled::Adopted(_)) { Mark::Ok } else { Mark::Note },
                        n.to_string(),
                    )
                })
                .collect(),
            View::Drift(d) => drift_rows(d),
        }
    }
}

fn drift_rows(d: &Drift) -> Vec<Row> {
    let mut rows: Vec<Row> = d.warnings.iter().map(|w| row(Mark::Warn, format!("! {w}"))).collect();
    let mut section = |head: &str, hint: &str, mark: Mark, lines: Vec<&str>| {
        if lines.is_empty() {
            return;
        }
        rows.push(row(Mark::Note, ""));
        rows.push(row(mark, head));
        rows.extend(lines.iter().map(|l| row(Mark::Detail, format!("  {l}"))));
        rows.push(row(Mark::Note, hint));
    };
    section(
        "Changed in the config, waiting to be applied:",
        "  Press f (plan), then a (apply).",
        Mark::Create,
        d.waiting.iter().map(|l| l.text.as_str()).collect(),
    );
    section(
        "Changed in Discord:",
        "  p copies Discord's version into the config; applying puts the config's back.",
        Mark::Edit,
        d.discord.iter().map(|l| l.text.as_str()).collect(),
    );
    section(
        "Needs a person (changed on both sides, or not known):",
        "  Make the config say what you want, then plan and apply.",
        Mark::Remove,
        d.person.iter().map(|l| l.text.as_str()).collect(),
    );
    rows
}

fn plan_rows(p: &Plan) -> Vec<Row> {
    let mut rows: Vec<Row> = p.warnings.iter().map(|w| row(Mark::Warn, format!("! {w}"))).collect();
    for a in &p.actions {
        let text = a.to_string();
        let mut lines = text.lines();
        rows.push(row(mark(a), lines.next().unwrap_or_default()));
        rows.extend(lines.map(|l| row(Mark::Detail, l)));
    }
    rows
}

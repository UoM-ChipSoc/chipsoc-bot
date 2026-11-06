//! How Discord and the config differ, sorted by who changed what: the drift
//! report every front end shows.

use chipsoc_model::{Action, Cause, Plan};
use serde::{Deserialize, Serialize};

/// One step of a plan, as a line, with its place in the plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Line {
    /// Index into the plan's actions.
    pub step: usize,
    pub text: String,
}

/// A plan's steps by who changed what since the last apply.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Drift {
    /// The config changed: `apply` sends these.
    pub waiting: Vec<Line>,
    /// Changed in Discord only: `pull` keeps them, `apply` undoes them.
    pub discord: Vec<Line>,
    /// Changed on both sides, or not known: someone has to choose.
    pub person: Vec<Line>,
    pub warnings: Vec<String>,
}

impl Drift {
    /// Discord matches the config.
    pub fn is_clean(&self) -> bool {
        self.waiting.is_empty() && self.discord.is_empty() && self.person.is_empty()
    }
}

/// Sort `plan`'s changes the way drift watch does.
pub fn drift(plan: &Plan) -> Drift {
    let mut d = Drift { warnings: plan.warnings.iter().map(ToString::to_string).collect(), ..Drift::default() };
    for (step, a) in plan.actions.iter().enumerate() {
        let line = Line { step, text: a.headline() };
        match a {
            Action::Skip { .. } => {}
            Action::Create { .. }
            | Action::Archive { .. }
            | Action::Delete { .. }
            | Action::Edit { cause: Cause::Config, .. } => d.waiting.push(line),
            Action::Edit { cause: Cause::Discord, .. } => d.discord.push(line),
            _ => d.person.push(line),
        }
    }
    d
}

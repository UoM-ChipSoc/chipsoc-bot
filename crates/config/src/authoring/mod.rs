//! Validation and comment-preserving edits, shared by every front end.

mod check;
mod edit;

pub use check::{BUTTON_ACTIONS, MAX_FORM_FIELDS, Report, check, check_config};
pub use edit::*;

/// Mistakes in the config that stop plan, apply and edits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problems(pub Vec<String>);

impl std::fmt::Display for Problems {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the config has {} problem(s): {}", self.0.len(), self.0.join("; "))
    }
}

impl std::error::Error for Problems {}

//! What the editing pages share: the fields a form sent, a form shown again
//! with what was wrong next to each field, the inputs that do that, confirm
//! boxes for destructive steps, and running config edits with a flash after.

use std::sync::Arc;

use anyhow::Result;
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use chipsoc_config::ops::{Change, Op};
use chipsoc_discord::Rest;
use chipsoc_engine::Problems;
use dioxus::prelude::*;

use crate::App;
use crate::auth::Session;
use crate::view::csrf;

/// The fields a form sent, in order (a name can repeat, as checkboxes do).
#[derive(Clone, Default)]
pub(crate) struct Fields(pub Vec<(String, String)>);

impl Fields {
    /// The first value given for `name`, trimmed; empty if none.
    pub fn get(&self, name: &str) -> &str {
        self.0.iter().find(|(k, _)| k == name).map_or("", |(_, v)| v.trim())
    }

    /// `name`'s value as given, untrimmed (for markdown).
    pub fn raw(&self, name: &str) -> &str {
        self.0.iter().find(|(k, _)| k == name).map_or("", |(_, v)| v.as_str())
    }

    /// Every value given for `name`.
    pub fn all(&self, name: &str) -> Vec<&str> {
        self.0.iter().filter(|(k, _)| k == name).map(|(_, v)| v.as_str()).collect()
    }

    /// A ticked checkbox, or any value at all.
    pub fn has(&self, name: &str) -> bool {
        self.0.iter().any(|(k, _)| k == name)
    }

    /// `None` when blank.
    pub fn opt(&self, name: &str) -> Option<String> {
        Some(self.get(name)).filter(|v| !v.is_empty()).map(str::to_owned)
    }

    /// The confirm box was ticked or its "Yes" pressed.
    pub fn confirmed(&self) -> bool {
        self.get("confirm") == "yes"
    }

    /// A comma- or line-separated list.
    pub fn list(&self, name: &str) -> Vec<String> {
        split_list(self.raw(name))
    }
}

/// "a, b\nc" as `[a, b, c]`.
pub(crate) fn split_list(s: &str) -> Vec<String> {
    s.split([',', '\n']).map(str::trim).filter(|v| !v.is_empty()).map(str::to_owned).collect()
}

/// One form shown again: what was sent, what was wrong with which field,
/// and the config's own problems with it. Empty for a fresh page.
#[derive(Clone, Default)]
pub(crate) struct Shown {
    /// Which form on the page it was (its id).
    pub form: String,
    pub fields: Fields,
    pub errors: Vec<(String, String)>,
    pub problems: Vec<String>,
    /// The problems are `check`'s, with the config as the form would leave it.
    pub config: bool,
    /// People the form's name could mean, to pick from: (id, label).
    pub choices: Box<[(String, String)]>,
}

impl Shown {
    pub fn none() -> Self {
        Self::default()
    }

    /// `form` sent `fields`, with `errors` (field name, what's wrong).
    pub fn invalid(form: &str, fields: Fields, errors: Vec<(String, String)>) -> Self {
        Self { form: form.into(), fields, errors, ..Self::default() }
    }

    /// `form` was refused: the config's problems with it, or why not.
    pub fn refused(form: &str, fields: Fields, e: &anyhow::Error) -> Self {
        let (problems, config) = match e.downcast_ref::<Problems>() {
            Some(p) => (p.0.clone(), true),
            None => (vec![format!("{e:#}")], false),
        };
        Self { form: form.into(), fields, problems, config, ..Self::default() }
    }

    fn mine(&self, form: &str) -> bool {
        !self.form.is_empty() && self.form == form
    }

    /// What to show in a field: what was sent, else `current`.
    pub fn val(&self, form: &str, name: &str, current: &str) -> String {
        if self.mine(form) { self.fields.raw(name).to_owned() } else { current.to_owned() }
    }

    pub fn checked(&self, form: &str, name: &str, current: bool) -> bool {
        if self.mine(form) { self.fields.has(name) } else { current }
    }

    pub fn err(&self, form: &str, name: &str) -> Option<&str> {
        if !self.mine(form) {
            return None;
        }
        self.errors.iter().find(|(f, _)| f == name).map(|(_, e)| e.as_str())
    }

    /// Whether this form should start open (it was just sent back).
    pub fn open(&self, form: &str) -> bool {
        self.mine(form)
    }

    /// A form shown again as sent, with nothing wrong (e.g. to preview it).
    pub fn again(form: &str, fields: Fields) -> Self {
        Self { form: form.into(), fields, ..Self::default() }
    }

    /// 422 when a form came back with something wrong.
    pub fn status(&self) -> StatusCode {
        if self.errors.is_empty() && self.problems.is_empty() {
            StatusCode::OK
        } else {
            StatusCode::UNPROCESSABLE_ENTITY
        }
    }

    /// The config's problems with this form, shown at its top.
    pub fn problems(&self, form: &str) -> Element {
        if !self.mine(form) || (self.problems.is_empty() && self.errors.is_empty()) {
            return rsx! {};
        }
        let head = if self.problems.is_empty() {
            "Some fields need fixing; see below."
        } else if self.config {
            "That wasn't saved: the config would have these problems."
        } else {
            "That wasn't done:"
        };
        rsx! {
            div { class: "note bad", role: "alert",
                p { "{head}" }
                if !self.problems.is_empty() {
                    ul {
                        for p in &self.problems {
                            li { {crate::view::plain(p)} }
                        }
                    }
                }
            }
        }
    }
}

/// A page with its form shown again (or fresh), at the right status.
pub(crate) fn shown(sh: &Shown, page: Html<String>) -> Response {
    (sh.status(), page).into_response()
}

fn field_id(form: &str, name: &str) -> String {
    format!("{form}-{name}")
}

/// The error under a field, if it has one.
fn error_line(id: &str, err: Option<&str>) -> Element {
    match err {
        Some(e) => rsx! { p { class: "field-error", id: "{id}-error", "{e}" } },
        None => rsx! {},
    }
}

/// The ids of what describes field `id`: its error and its hint.
fn described_by(id: &str, error: bool, hint: bool) -> String {
    let mut ids = Vec::new();
    if error {
        ids.push(format!("{id}-error"));
    }
    if hint {
        ids.push(format!("{id}-hint"));
    }
    ids.join(" ")
}

/// A labelled one-line text box.
pub(crate) fn text(sh: &Shown, form: &str, name: &str, label: &str, current: &str, hint: &str) -> Element {
    input_of(sh, form, name, label, current, hint, "text")
}

/// A labelled box of some `kind` (`text`, `url`, `number`).
pub(crate) fn input_of(
    sh: &Shown,
    form: &str,
    name: &str,
    label: &str,
    current: &str,
    hint: &str,
    kind: &str,
) -> Element {
    let id = field_id(form, name);
    let err = sh.err(form, name);
    let value = sh.val(form, name, current);
    let described = described_by(&id, err.is_some(), !hint.is_empty());
    rsx! {
        label { r#for: "{id}", "{label}" }
        input {
            id: "{id}",
            name: "{name}",
            r#type: "{kind}",
            value: "{value}",
            "aria-invalid": if err.is_some() { "true" },
            "aria-describedby": if !described.is_empty() { "{described}" },
        }
        {error_line(&id, err)}
        if !hint.is_empty() {
            p { class: "hint", id: "{id}-hint", "{hint}" }
        }
    }
}

/// A labelled multi-line box.
pub(crate) fn area(sh: &Shown, form: &str, name: &str, label: &str, current: &str, rows: u32, hint: &str) -> Element {
    let id = field_id(form, name);
    let err = sh.err(form, name);
    let value = sh.val(form, name, current);
    let described = described_by(&id, err.is_some(), !hint.is_empty());
    rsx! {
        label { r#for: "{id}", "{label}" }
        textarea {
            id: "{id}",
            name: "{name}",
            rows: "{rows}",
            "aria-invalid": if err.is_some() { "true" },
            "aria-describedby": if !described.is_empty() { "{described}" },
            "{value}"
        }
        {error_line(&id, err)}
        if !hint.is_empty() {
            p { class: "hint", id: "{id}-hint", "{hint}" }
        }
    }
}

/// A labelled checkbox.
pub(crate) fn check(sh: &Shown, form: &str, name: &str, label: &str, current: bool) -> Element {
    let on = sh.checked(form, name, current);
    rsx! {
        label { class: "check",
            input { r#type: "checkbox", name: "{name}", value: "yes", checked: on }
            "{label}"
        }
    }
}

/// A labelled drop-down of (value, label).
pub(crate) fn select(
    sh: &Shown,
    form: &str,
    name: &str,
    label: &str,
    options: &[(String, String)],
    current: &str,
) -> Element {
    let id = field_id(form, name);
    let err = sh.err(form, name);
    let chosen = sh.val(form, name, current);
    rsx! {
        label { r#for: "{id}", "{label}" }
        select {
            id: "{id}",
            name: "{name}",
            "aria-invalid": if err.is_some() { "true" },
            "aria-describedby": if err.is_some() { "{id}-error" },
            for (v, l) in options {
                option { value: "{v}", selected: *v == chosen, "{l}" }
            }
        }
        {error_line(&id, err)}
    }
}

/// Hidden fields.
pub(crate) fn hidden(fields: &[(&str, &str)]) -> Element {
    rsx! {
        for (k, v) in fields {
            input { r#type: "hidden", name: "{k}", value: "{v}" }
        }
    }
}

/// A one-button form, e.g. "Move up".
pub(crate) fn button(s: &Session, action: &str, fields: &[(&str, &str)], label: &str, class: &str) -> Element {
    rsx! {
        form { method: "post", action: "{action}", class: "inline",
            {csrf(s)}
            {hidden(fields)}
            button { class: "{class}", "{label}" }
        }
    }
}

/// A ↑ or ↓ button that moves `name` (`fields` say which) up or down a list.
pub(crate) fn mover(s: &Session, action: &str, fields: &[(&str, &str)], up: bool, name: &str) -> Element {
    let (to, arrow, said) = if up { ("up", "↑", "up") } else { ("down", "↓", "down") };
    let mut fields = fields.to_vec();
    fields.push(("to", to));
    let label = format!("Move {name} {said}");
    rsx! {
        form { method: "post", action: "{action}", class: "inline",
            {csrf(s)}
            {hidden(&fields)}
            button { class: "quiet small", "aria-label": "{label}", title: "{label}", "{arrow}" }
        }
    }
}

/// A destructive step behind a second press: `label` opens it, `yes` does it.
pub(crate) fn confirm(
    s: &Session,
    action: &str,
    fields: &[(&str, &str)],
    label: &str,
    yes: &str,
    warning: &str,
) -> Element {
    confirm_box(s, action, fields, label, yes, warning, true)
}

/// [`confirm`] for a big step that isn't destructive (e.g. a change of status).
pub(crate) fn confirm_calm(
    s: &Session,
    action: &str,
    fields: &[(&str, &str)],
    label: &str,
    yes: &str,
    warning: &str,
) -> Element {
    confirm_box(s, action, fields, label, yes, warning, false)
}

fn confirm_box(
    s: &Session,
    action: &str,
    fields: &[(&str, &str)],
    label: &str,
    yes: &str,
    warning: &str,
    danger: bool,
) -> Element {
    let label = summary_label(label);
    let (box_class, summary_class, yes_class) = if danger {
        ("confirm-box", "button quiet danger", "danger")
    } else {
        ("confirm-box calm", "button quiet", "")
    };
    rsx! {
        details { class: "{box_class}",
            summary { class: "{summary_class}",
                span { class: "closed", "{label}" }
                span { class: "opened", "Cancel" }
            }
            form { method: "post", action: "{action}",
                {csrf(s)}
                {hidden(fields)}
                input { r#type: "hidden", name: "confirm", value: "yes" }
                if !warning.is_empty() {
                    p { class: "hint", "{warning}" }
                }
                button { class: "{yes_class}", "{yes}" }
            }
        }
    }
}

/// A button that opens a second step ends in "…".
pub(crate) fn summary_label(label: &str) -> String {
    if label.ends_with('…') { label.to_owned() } else { format!("{label}…") }
}

/// Run config edits as `s`, as one change: one check and one commit, all
/// or nothing (a refused one leaves the config as it was). What was done,
/// as one line for a flash.
pub(crate) async fn run_ops<R: Rest + 'static>(app: &Arc<App<R>>, s: &Session, ops: &[Op]) -> Result<String> {
    Ok(said(&app.edit_ops(s, ops).await?))
}

/// "Role member: hoist on; role member: name to "X"." and new warnings.
fn said(c: &Change) -> String {
    if c.files.is_empty() {
        return "Nothing to change.".into();
    }
    let mut out = vec![format!("{}.", c.summary)];
    out.extend(c.warnings.iter().map(|w| format!("Worth a look: {w}")));
    out.join(" ")
}

/// After a config edit: what's next.
pub(crate) const APPLY_HINT: &str = "Not in Discord yet: apply it on the Apply page.";

/// Run admin work on the request's desk to the end even if the browser goes
/// away; `$work` is a `Result` (what happened, or why not) of
/// [`Said`](chipsoc_admin::Said) or text, given back as plain text with
/// people's names (see [`crate::people::Spoken`]).
macro_rules! spawned {
    ($app:expr, $s:expr, |$d:ident| $work:expr) => {{
        let (app, s) = ($app.clone(), $s.clone());
        let run = async move {
            use crate::people::Spoken as _;
            let desk = app.desk(&s).await?;
            let $d = &desk;
            anyhow::Ok(match $work {
                Ok(t) => Ok(t.spoken(&desk).await),
                Err(t) => Err(t.spoken(&desk).await),
            })
        };
        crate::to_end(run).await.unwrap_or_else(|e| Err(format!("{e:#}")))
    }};
}
pub(crate) use spawned;

/// `ts` (seconds) as "2026-11-04 13:05 UTC".
pub(crate) fn when(ts: i64) -> String {
    format!("{} UTC", chipsoc_model::utc_minute(ts))
}

/// A role or channel key from what someone typed: given, else made from the name.
pub(crate) fn key_from(given: &str, name: &str) -> String {
    let k = if given.is_empty() { name } else { given };
    chipsoc_config::slug(k)
}

pub(crate) use chipsoc_config::is_key;

/// `#rrggbb`.
pub(crate) fn is_colour(s: &str) -> bool {
    s.len() == 7 && s.starts_with('#') && s[1..].chars().all(|c| c.is_ascii_hexdigit())
}

/// A user id, or a mention of one.
pub(crate) fn user_id(s: &str) -> Option<chipsoc_config::Snowflake> {
    chipsoc_config::Snowflake::parse_mention(s)
}

/// Handle a config-editing form: `built` is the form's id and the edits it
/// asks for (or the form back with what's wrong). Done: back to `here` with a
/// flash. `Err`: the form to show again, with what's wrong.
pub(crate) async fn edit<R: Rest + 'static>(
    app: &Arc<App<R>>,
    s: &Session,
    here: &str,
    f: Fields,
    built: Result<(String, Vec<Op>), Shown>,
) -> Result<Response, Shown> {
    let (form, ops) = built?;
    match run_ops(app, s, &ops).await {
        Ok(done) if done == "Nothing to change." => Ok(crate::flash::redirect(here, &done, true)),
        Ok(done) => Ok(crate::flash::redirect(here, &format!("{done} {APPLY_HINT}"), true)),
        Err(e) => Err(Shown::refused(&form, f, &e)),
    }
}

/// The form back with one field's problem.
pub(crate) fn wrong(form: &str, f: &Fields, field: &str, why: &str) -> Shown {
    Shown::invalid(form, f.clone(), vec![(field.to_owned(), why.to_owned())])
}

/// The form back with a problem that isn't one field's.
pub(crate) fn refuse(form: &str, f: &Fields, why: &str) -> Shown {
    Shown { form: form.into(), fields: f.clone(), problems: vec![why.to_owned()], ..Shown::default() }
}

/// The people a name could mean, as a choice in the form (field `pick`).
pub(crate) fn pick(sh: &Shown, form: &str) -> Element {
    if !sh.mine(form) || sh.choices.is_empty() {
        return rsx! {};
    }
    rsx! {
        fieldset {
            legend { "Which one?" }
            for (i, (id, label)) in sh.choices.iter().enumerate() {
                label { class: "check",
                    input { r#type: "radio", name: "pick", value: "{id}", checked: i == 0 }
                    "{label}"
                }
            }
        }
    }
}

/// What a destructive step says until its "Yes" is pressed.
pub(crate) const CONFIRM_FIRST: &str = "Press the “Yes” button to confirm.";

/// Destructive steps need their "Yes" pressed.
pub(crate) fn need_confirm(form: &str, f: &Fields) -> Result<(), Shown> {
    if f.confirmed() { Ok(()) } else { Err(refuse(form, f, CONFIRM_FIRST)) }
}

/// `up`/`down` from a form.
pub(crate) fn direction(f: &Fields) -> chipsoc_config::ops::Move {
    if f.get("to") == "up" { chipsoc_config::ops::Move::Up } else { chipsoc_config::ops::Move::Down }
}

impl<R: Rest> App<R> {
    /// The config as its files say it (what `Op`s edit).
    pub(crate) async fn schema(&self) -> Result<chipsoc_config::ops::Schema> {
        let dir = self.dir.clone();
        chipsoc_config::blocking(move || chipsoc_config::ops::schema(&dir)).await
    }
}

/// Back to `here`, saying how admin work went.
pub(crate) fn said_back(here: &str, r: Result<String, String>) -> Response {
    match r {
        Ok(text) => crate::flash::redirect(here, text.trim(), true),
        Err(why) => crate::flash::redirect(here, why.trim(), false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_and_ids_read_right() {
        assert_eq!(when(0), "1970-01-01 00:00 UTC");
        assert_eq!(when(1_793_793_900), "2026-11-04 12:05 UTC");
        assert_eq!(when(951_782_400), "2000-02-29 00:00 UTC");
        assert_eq!(user_id("<@123>").map(|s| s.0), Some(123));
        assert!(user_id("ada").is_none() && user_id("").is_none());
        assert_eq!(split_list("a, b\nc,,"), ["a", "b", "c"]);
        assert!(is_colour("#00ffAA") && !is_colour("red"));
    }
}

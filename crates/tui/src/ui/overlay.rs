//! Pop-ups: help, confirmations, the status picker and forms. The keys for
//! each are in the footer, so they aren't repeated here.

use chipsoc_config::schema::ProjectStatus;
use ratatui::Frame;
use ratatui::layout::{Constraint, Rect};
use ratatui::style::Stylize;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph, Wrap};

use crate::app::{App, Confirm, Overlay};
use crate::edits::Then;
use crate::form::{Form, Key, StatusPicker, status_help};

pub fn draw(f: &mut Frame, area: Rect, app: &App, o: &Overlay) {
    let (title, lines) = match o {
        Overlay::Help => ("How it works".to_owned(), help_text()),
        Overlay::Confirm(Confirm::Apply) => ("Apply to Discord?".to_owned(), apply_text(app)),
        Overlay::Confirm(Confirm::Pull) => ("Pull from Discord?".to_owned(), pull_text(app)),
        Overlay::Confirm(Confirm::Quit) => ("Quit now?".to_owned(), quit_text(app)),
        Overlay::Confirm(Confirm::Do { title, lines, .. }) => (title.clone(), confirm_text(lines)),
        Overlay::Status(p) => ("Change status".to_owned(), status_text(p)),
        Overlay::Form(form) => (form.title.clone(), form_text(form)),
    };
    let block = Block::bordered().title(Line::from(format!(" {title} ")).bold());
    let text = Paragraph::new(lines).block(block).wrap(Wrap { trim: false });
    let width = area.width.min(76);
    let height = text.line_count(width.saturating_sub(2)) as u16;
    let popup = area.centered(Constraint::Length(width), Constraint::Length(height));
    f.render_widget(Clear, popup);
    f.render_widget(text, popup);
}

/// Prose lines wrap to the box; the key tables fit it.
const HELP: &[&str] = &[
    "Edits here change the config: your files, or the server's copy when signed in. Nothing reaches \
     Discord until you apply it: 0, then f (plan) shows what would change, then a (apply) sends it. \
     p there copies edits made in Discord's own settings back into the config; d shows who changed what.",
    "",
    "The queue, members, feeds and resources act in Discord straight away.",
    "",
    "Keys",
    "  ↑↓ or j k      move           Home End     first, last",
    "  PgUp PgDn      scroll details Shift+↑↓     reorder (or J K)",
    "  Tab, 1-9, 0    screens        r            reload",
    "  e              open the file that defines it, in $EDITOR",
    "",
    "In a form",
    "  Tab ↑↓         field          ←→ Home End  cursor",
    "  ←→ Space       choose, flip   Ctrl+U       clear",
    "  Enter          next field, then do it (Ctrl+S from any field)",
    "  Esc            cancel",
];

fn help_text() -> Vec<Line<'static>> {
    HELP.iter()
        .map(|l| if *l == "Keys" || *l == "In a form" { Line::from(*l).bold() } else { Line::from(*l) })
        .collect()
}

fn confirm_text(lines: &[String]) -> Vec<Line<'static>> {
    lines.iter().map(|l| Line::from(l.clone())).collect()
}

fn apply_text(app: &App) -> Vec<Line<'static>> {
    let n = app.changes.plan().map_or(0, |p| p.changes());
    let server = app.cfg.as_ref().map_or(String::new(), |c| c.desired.server.guild.name.clone());
    vec![
        Line::from(format!("Send {} to the Discord server \"{server}\"?", super::count(n, "change", "changes"))),
        Line::default(),
        Line::from("Members see them straight away. Removed channels move to the archive; nothing is deleted."),
    ]
}

fn pull_text(app: &App) -> Vec<Line<'static>> {
    let whose = if app.remote { "the server's copy of the config" } else { "your files" };
    vec![
        Line::from("Copy edits made in Discord's own settings into the config?"),
        Line::default(),
        Line::from(format!("This changes {whose} (comments are kept).")),
    ]
}

fn quit_text(app: &App) -> Vec<Line<'static>> {
    let what = match app.busy {
        Some(_) if matches!(app.changes.view, crate::changes::View::Applying { .. }) => {
            "Stop mid-apply? Some changes may be half-sent; plan again afterwards to see what's left."
        }
        _ => "Something is still running; quitting may cut it short.",
    };
    vec![Line::from(what), Line::from("Ctrl+C again quits too.").italic()]
}

fn status_text(p: &StatusPicker) -> Vec<Line<'_>> {
    let mut v = vec![Line::from(format!("What stage is {} at?", p.name)), Line::default()];
    for s in ProjectStatus::ALL {
        let row = Line::from(format!("{:<9} {}", s.as_str(), status_help(s)));
        v.push(if s == p.status { row.reversed() } else { row });
    }
    v
}

fn form_text(form: &Form<Then>) -> Vec<Line<'_>> {
    let mut v = Vec::new();
    let width = form.fields.iter().map(|f| f.label.chars().count()).max().unwrap_or(0) + 2;
    for (i, field) in form.fields.iter().enumerate() {
        let current = i == form.at;
        let value = match (field.is_choice(), current) {
            (true, _) => format!("‹ {} ›", field.shown()),
            (false, true) => {
                let (before, after) = form.split_at_cursor();
                format!("{before}▏{after}")
            }
            (false, false) => field.shown(),
        };
        let label = Span::from(format!("{:<width$}", field.label));
        let value = if current { Span::from(value).reversed() } else { Span::from(value) };
        v.push(Line::from(vec![if current { label.bold() } else { label }, value]));
    }
    if let Some(field) = form.fields.get(form.at).filter(|f| !f.hint.is_empty()) {
        v.push(Line::default());
        v.push(Line::from(field.hint).italic());
    }
    if form.then == Then::NewProject {
        let status = form.fields.iter().find(|f| f.key == Key::Status).map(|f| f.shown()).unwrap_or_default();
        if let Ok(s) = status.parse::<ProjectStatus>() {
            v.push(Line::from(format!("{s}: {}", status_help(s))).italic());
        }
    }
    v.extend(form.error.as_ref().map(|e| Line::from(format!("✗ {e}")).red().bold()));
    v
}

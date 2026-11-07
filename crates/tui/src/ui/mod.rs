//! Drawing. Reads [`App`]; never changes it.

mod admin;
mod overlay;
mod tabs;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

use crate::app::{App, Tab};
use crate::changes::View;

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

pub fn draw(f: &mut Frame, app: &App) {
    let (width, height) = (f.area().width.max(1), f.area().height);
    let footer = footer(app, width as usize);
    // Every key fits, unless that would leave little room for the screen itself.
    let footer_height = footer.len().clamp(1, (height as usize / 3).max(1)) as u16;
    let status = status_line(app);
    let status_height = status.line_count(width).clamp(1, 3) as u16;
    let [head, body, status_area, foot] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Fill(1),
        Constraint::Length(status_height),
        Constraint::Length(footer_height),
    ])
    .areas(f.area());
    header(f, head, app);
    match app.tab {
        Tab::Overview => tabs::overview(f, body, app),
        Tab::Roles => tabs::roles(f, body, app),
        Tab::Channels => tabs::channels(f, body, app),
        Tab::Projects => tabs::projects(f, body, app),
        Tab::Positions => tabs::positions(f, body, app),
        Tab::Queue => admin::queue(f, body, app),
        Tab::Members => admin::members(f, body, app),
        Tab::Feeds => admin::feeds(f, body, app),
        Tab::Resources => admin::resources(f, body, app),
        Tab::Changes => tabs::changes(f, body, app),
    }
    f.render_widget(status, status_area);
    f.render_widget(Paragraph::new(footer), foot);
    if let Some(o) = &app.overlay {
        overlay::draw(f, body, app, o);
    }
}

/// The tabs, with the demo badge first so it never gets cut off. When they
/// don't all fit, only the current one keeps its name: `1 2 [6 Queue] 7`.
fn header(f: &mut Frame, area: Rect, app: &App) {
    f.render_widget(Line::from(header_spans(app, area.width as usize)), area);
}

fn header_spans(app: &App, width: usize) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    if app.demo {
        spans.push(Span::from(" DEMO ").black().on_yellow().bold());
        spans.push(Span::raw(" "));
    }
    let used: usize = spans.iter().map(Span::width).sum();
    let full: usize = Tab::ALL.iter().map(|t| t.title().len() + 3).sum();
    let narrow = used + full > width;
    for t in Tab::ALL {
        let label = if narrow && t != app.tab { t.key().to_string() } else { format!("{} {}", t.key(), t.title()) };
        let label = Span::from(label);
        spans.push(if t == app.tab { label.reversed().bold() } else { label });
        spans.push(Span::raw(" "));
    }
    if app.demo && !narrow {
        spans.push(Span::raw(" pretend Discord").italic());
    }
    spans
}

/// What's running, else the last notice, wrapped onto up to 3 rows.
fn status_line(app: &App) -> Paragraph<'_> {
    let line = if let Some(what) = app.busy {
        let mut line = format!("{} {what}", SPINNER[app.ticks % SPINNER.len()]);
        if let View::Applying { total, steps } = &app.changes.view {
            line.push_str(&format!(" {}/{total}", steps.len()));
        }
        Line::from(line).cyan()
    } else {
        match &app.notice {
            Some(n) if n.error => Line::from(format!("✗ {}", n.text)).red().bold(),
            Some(n) => Line::from(format!("✓ {}", n.text)).green(),
            None => app.connected.as_deref().map_or_else(Line::default, |c| Line::from(c).italic()),
        }
    };
    Paragraph::new(line).wrap(Wrap { trim: true })
}

/// The keys that work, packed into lines `width` wide without splitting one.
fn footer(app: &App, width: usize) -> Vec<Line<'static>> {
    let key = Style::new().fg(Color::Black).bg(Color::Gray).add_modifier(Modifier::BOLD);
    let mut items: Vec<Vec<Span<'static>>> = app
        .bindings()
        .iter()
        .map(|b| vec![Span::styled(format!(" {} ", b.hint), key), Span::raw(format!(" {}   ", b.what))])
        .collect();
    if matches!(app.overlay, Some(crate::app::Overlay::Form(ref form)) if !form.on_choice()) {
        items.push(vec![Span::raw("type to fill in").italic()]);
    }
    let mut lines: Vec<Vec<Span<'static>>> = vec![Vec::new()];
    let mut used = 0;
    for item in items {
        let w: usize = item.iter().map(Span::width).sum();
        if used > 0 && used + w > width {
            lines.push(Vec::new());
            used = 0;
        }
        used += w;
        lines.last_mut().unwrap_or(&mut Vec::new()).extend(item);
    }
    lines.into_iter().map(Line::from).collect()
}

/// "Label        value", for detail panes; a long label still gets a space.
pub(super) fn field<'a>(label: &'a str, value: impl Into<String>) -> Line<'a> {
    let width = (label.chars().count() + 1).max(13);
    Line::from(vec![Span::from(format!("{label:<width$}")).italic(), Span::raw(value.into())])
}

/// "1 change", "3 changes".
pub fn count(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// A unix time as `2026-11-04 13:05 UK time`, in the config's time zone
/// (UTC until the config is loaded).
fn when(app: &App, ts: i64) -> String {
    let tz = app.schema.as_ref().map_or("UTC", |s| s.bot.timezone.as_str());
    chipsoc_config::bot::local_minute(tz, ts)
}

#[cfg(test)]
mod tests {
    #[test]
    fn plurals_read_right() {
        assert_eq!(super::count(1, "change", "changes"), "1 change");
        assert_eq!(super::count(16, "change", "changes"), "16 changes");
    }

    #[test]
    fn times_read_in_the_config_s_zone() {
        let mut app = crate::app::App::with(".".into(), false, false);
        assert_eq!(super::when(&app, 1_782_907_200), "2026-07-01 12:00 UTC", "before the config is loaded");
        let dir = crate::tests::fixture();
        app.schema = Some(chipsoc_config::ops::schema(dir.path()).unwrap());
        assert_eq!(super::when(&app, 1_782_907_200), "2026-07-01 13:00 UK time");
    }
}

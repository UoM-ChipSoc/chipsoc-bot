//! The config screens: overview, roles, channels, projects, positions, changes.

use chipsoc_config::ops::Schema;
use chipsoc_config::schema::{ChannelKind, Project, ProjectStatus};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, List, ListState, Paragraph, Wrap};

use super::{count, field};
use crate::app::{App, Loaded, Tab};
use crate::changes::{Mark, View};
use crate::edits::find_channel;
use crate::form::status_help;
use crate::lists::{self, Spot};

pub(super) fn pane(title: &str) -> Block<'_> {
    Block::bordered().title(format!(" {title} "))
}

pub(super) fn text(f: &mut Frame, area: Rect, title: &str, lines: Vec<Line>, scroll: usize) {
    let p = Paragraph::new(lines).block(pane(title)).wrap(Wrap { trim: false }).scroll((scroll as u16, 0));
    f.render_widget(p, area);
}

/// The config isn't here: why, or that it's on its way.
fn not_loaded(f: &mut Frame, area: Rect, app: &App, title: &str) {
    let msg = match &app.schema_error {
        Some(e) if app.remote => Line::from(format!("Couldn't read the server's config: {e}")).red(),
        Some(_) => Line::from("Your files have a problem, so this can't be shown. See Overview (press 1).").red(),
        None => Line::from("Loading the server's copy of the config…").italic(),
    };
    text(f, area, title, vec![msg], 0);
}

/// A list, and the selection's details beside it (scrolled by PgUp/PgDn).
pub(super) fn list_and_details(
    f: &mut Frame,
    area: Rect,
    app: &App,
    list: (&str, Vec<Line>, usize),
    details: (&str, Vec<Line>),
) {
    let [left, right] = Layout::horizontal([Constraint::Percentage(40), Constraint::Fill(1)]).areas(area);
    let (title, items, selected) = list;
    let widget = List::new(items).block(pane(title)).highlight_style(Style::new().reversed()).highlight_symbol("▶ ");
    f.render_stateful_widget(widget, left, &mut ListState::default().with_selected(Some(selected)));
    let (title, lines) = details;
    let p = Paragraph::new(lines).wrap(Wrap { trim: false });
    let inner = right.width.saturating_sub(2);
    let max = p.line_count(inner).saturating_sub(right.height.saturating_sub(2) as usize);
    app.detail_max.set(max);
    let scroll = app.detail_scroll.min(max);
    let title = if max > 0 { format!("{title} ({}/{})", scroll + 1, max + 1) } else { title.to_owned() };
    f.render_widget(p.block(pane(&title)).scroll((scroll as u16, 0)), right);
}

pub fn overview(f: &mut Frame, area: Rect, app: &App) {
    let mut lines = Vec::new();
    let (errors, warnings) = match (&app.status, app.remote) {
        (Loaded::Got(s), true) => (s.errors.clone(), s.warnings.clone()),
        _ => (app.report.errors.clone(), app.report.warnings.clone()),
    };
    lines.extend(summary(app));
    lines.push(Line::default());
    if !errors.is_empty() {
        let n = count(errors.len(), "problem", "problems");
        lines.push(Line::from(format!("✗ {n} to fix before anything can be sent:")).red().bold());
        lines.extend(errors.iter().map(|e| Line::from(format!("  ✗ {e}")).red()));
    }
    if !warnings.is_empty() {
        let n = count(warnings.len(), "thing", "things");
        lines.push(Line::from(format!("! {n} worth a look:")).bold());
        lines.extend(warnings.iter().map(|w| Line::from(format!("  ! {w}"))));
    }
    if errors.is_empty() {
        if warnings.is_empty() {
            lines.push(Line::from("✓ No problems found in the config.").green());
        }
        lines.push(Line::default());
        lines.push(Line::from("Next: press 0, then f (plan), to see what would change in Discord."));
    }
    lines.push(Line::default());
    lines.push(Line::from("Recent activity").bold());
    match &app.activity {
        Loaded::No => lines.push(Line::from("  …").italic()),
        Loaded::Failed(e) => lines.push(Line::from(format!("  {e}")).italic()),
        Loaded::Got((a, _)) if a.is_empty() => lines.push(Line::from("  Nothing yet.").italic()),
        Loaded::Got((a, _)) => lines.extend(a.iter().take(15).map(|e| {
            let what = [Some(e.action.as_str()), e.target.as_deref(), e.detail.as_deref()];
            let what: Vec<&str> = what.into_iter().flatten().collect();
            Line::from(format!("  {}  {}  {}", super::when(e.ts), e.actor.as_deref().unwrap_or("-"), what.join(" ")))
        })),
    }
    if let Loaded::Got((_, Ok(changes))) = &app.activity
        && !changes.is_empty()
    {
        lines.push(Line::default());
        lines.push(Line::from("Latest config changes").bold());
        lines.extend(changes.iter().map(|d| {
            let id = d.commit.as_deref().unwrap_or("-");
            Line::from(format!("  {}  {id:<9} {}  {}", super::when(d.at), d.who, d.what))
        }));
        lines.push(Line::from("  The server owner can undo one: chipsoc-bot revert <id>.").italic());
    }
    text(f, area, "Overview", lines, app.overview_scroll);
}

fn summary(app: &App) -> Vec<Line<'_>> {
    // The count is 0 when the bot's data can't be read; the activity log says which.
    let unreadable = matches!(app.activity, Loaded::Failed(_)) || matches!(app.queue, Loaded::Failed(_));
    let queue = match &app.status {
        Loaded::Got(s) if s.queue > 0 => format!("{} waiting for Accept or Reject (press 6)", s.queue),
        Loaded::Got(_) if unreadable => "unknown (can't reach the bot's data)".into(),
        Loaded::Got(_) => "none waiting".into(),
        Loaded::Failed(_) | Loaded::No => "unknown".into(),
    };
    let mut v = Vec::new();
    match (&app.status, &app.cfg) {
        (Loaded::Got(s), _) if app.remote => {
            v.push(field("Server", s.name.clone()).bold());
            v.push(field("Discord", if s.linked { "linked" } else { "not linked yet" }));
            v.push(field(
                "Contents",
                format!("{} roles · {} channels · {} positions", s.roles, s.channels, s.positions),
            ));
            v.push(field("Projects", s.projects.to_string()));
        }
        (_, Some(c)) => {
            let s = &c.desired;
            let linked = match c.guild() {
                Some(id) => format!("linked (server {id})"),
                None => "not linked yet; the first apply sets it up".into(),
            };
            let count = |st: ProjectStatus| c.projects.iter().filter(|p| p.status == st).count();
            v.push(field("Server", s.server.guild.name.clone()).bold());
            v.push(field("Discord", linked));
            v.push(field(
                "Contents",
                format!(
                    "{} roles · {} categories · {} channels",
                    s.roles.roles.len(),
                    s.categories.len(),
                    s.channels().count()
                ),
            ));
            v.push(field(
                "Projects",
                format!(
                    "{} active · {} proposed · {} past",
                    count(ProjectStatus::Active),
                    count(ProjectStatus::Proposed),
                    count(ProjectStatus::Past)
                ),
            ));
        }
        _ => {}
    }
    v.push(field("Review queue", queue));
    v
}

fn rgb(hex: &str) -> Option<Color> {
    u32::from_str_radix(hex.trim_start_matches('#'), 16).ok().map(Color::from_u32)
}

fn yes_no(b: bool) -> &'static str {
    if b { "yes" } else { "no" }
}

pub fn roles(f: &mut Frame, area: Rect, app: &App) {
    let Some(s) = &app.schema else { return not_loaded(f, area, app, "Roles") };
    if s.roles.is_empty() {
        return text(f, area, "Roles", vec![Line::from("No roles yet. Press n to add one.")], 0);
    }
    let labels = s
        .roles
        .iter()
        .map(|r| {
            let dot = Span::from("● ").fg(r.color.as_deref().and_then(rgb).unwrap_or(Color::Reset));
            Line::from(vec![dot, Span::raw(r.name.clone())])
        })
        .collect();
    let mut details = Vec::new();
    if let Some(r) = s.roles.get(app.sel()) {
        let perms = if r.permissions.is_empty() {
            "none extra".into()
        } else {
            r.permissions.iter().map(|p| lists::permission(p)).collect::<Vec<_>>().join(", ")
        };
        details = vec![
            Line::from(r.name.clone()).bold(),
            field("Colour", r.color.clone().unwrap_or_else(|| "none".into())),
            field("Listed apart", yes_no(r.hoist)),
            field("@mentionable", yes_no(r.mentionable)),
            field("Permissions", perms),
            field("Key", r.key.clone()).italic(),
        ];
        if let Some(people) = s.holders.get(&r.key) {
            details.push(field("Held by", format!("{} (see tab 7)", people.len())));
        }
        if r.managed {
            details.push(Line::from("Made by Discord for a bot; it can't be changed here.").italic());
        }
    }
    details.push(Line::default());
    details.push(
        Line::from("Higher in the list ranks higher in Discord. Project roles are on the Projects tab.").italic(),
    );
    list_and_details(f, area, app, ("Roles (top ranks highest)", labels, app.sel()), ("Details", details));
}

pub fn channels(f: &mut Frame, area: Rect, app: &App) {
    let Some(s) = &app.schema else { return not_loaded(f, area, app, "Channels") };
    let spots = lists::spots(s);
    if spots.is_empty() {
        return text(
            f,
            area,
            "Channels",
            vec![Line::from("No channels yet. Press g for a category, n for a channel.")],
            0,
        );
    }
    let labels = spots.iter().map(|spot| spot_label(s, spot)).collect();
    let details = spots.get(app.sel()).map(|spot| spot_details(s, spot)).unwrap_or_default();
    list_and_details(f, area, app, ("Categories and channels", labels, app.sel()), ("Details", details));
}

fn spot_label<'a>(s: &'a Schema, spot: &'a Spot) -> Line<'a> {
    match spot {
        Spot::Category(k) => {
            let name = s.categories.iter().find(|c| &c.category.key == k).map_or(k.as_str(), |c| &c.category.name);
            Line::from(format!("▾ {name}")).bold()
        }
        Spot::Channel(k, cat) => {
            let pad = if cat.is_some() { "  " } else { "" };
            match find_channel(s, k) {
                Some(c) if c.kind == ChannelKind::Text => Line::from(format!("{pad}# {}", c.name)),
                Some(c) => Line::from(vec![
                    Span::raw(format!("{pad}  {} ", c.name)),
                    Span::from(format!("({})", c.kind)).italic(),
                ]),
                None => Line::from(k.clone()),
            }
        }
    }
}

fn spot_details<'a>(s: &'a Schema, spot: &Spot) -> Vec<Line<'a>> {
    match spot {
        Spot::Category(k) => match s.categories.iter().find(|c| &c.category.key == k) {
            Some(cf) => vec![
                Line::from(cf.category.name.clone()).bold(),
                field("Channels", cf.channels.len().to_string()),
                field("Who sees it", lists::audience(s, &cf.category.overwrites)),
                field("Key", k.clone()).italic(),
            ],
            None => Vec::new(),
        },
        Spot::Channel(k, cat) => match find_channel(s, k) {
            Some(c) => {
                let inherited = cat
                    .as_ref()
                    .and_then(|ck| s.categories.iter().find(|x| &x.category.key == ck))
                    .filter(|_| c.inherit)
                    .map(|cf| cf.category.overwrites.clone())
                    .unwrap_or_default();
                let mut ows = inherited;
                ows.retain(|o| !c.overwrites.iter().any(|x| x.target() == o.target()));
                ows.extend(c.overwrites.iter().cloned());
                let mut v = vec![Line::from(c.name.clone()).bold(), field("Type", c.kind.as_str())];
                v.extend(c.topic.clone().map(|t| field("Topic", t)));
                v.push(field("Who sees it", lists::audience(s, &ows)));
                v.extend((c.slowmode > 0).then(|| field("Slow mode", format!("{}s between messages", c.slowmode))));
                v.extend(c.user_limit.map(|n| field("User limit", n.to_string())));
                v.extend((!c.tags.is_empty()).then(|| field("Tags", c.tags.join(", "))));
                v.extend(c.post.clone().map(|p| field("Bot post", format!("{p} (b to edit)"))));
                v.push(field("Key", k.clone()).italic());
                v
            }
            None => Vec::new(),
        },
    }
}

pub fn projects(f: &mut Frame, area: Rect, app: &App) {
    let Some(s) = &app.schema else { return not_loaded(f, area, app, "Projects") };
    if s.projects.is_empty() {
        return text(f, area, "Projects", vec![Line::from("No projects yet. Press n to add one.")], 0);
    }
    let labels = s.projects.iter().map(project_label).collect();
    let details = s.projects.get(app.sel()).map(|p| project_details(app, p)).unwrap_or_default();
    list_and_details(f, area, app, ("Projects", labels, app.sel()), ("Details", details));
}

fn project_label(p: &Project) -> Line<'_> {
    let status = Span::from(format!("{:<9}", p.status.as_str()));
    let status = match p.status {
        ProjectStatus::Active => status.green(),
        ProjectStatus::Proposed => status.italic(),
        ProjectStatus::Past => status.dim(),
    };
    let apps = if p.application.open { "recruiting" } else { "" };
    Line::from(vec![Span::raw(format!("{:<14} ", p.name)), status, Span::from(apps).cyan()])
}

fn project_details<'a>(app: &App, p: &'a Project) -> Vec<Line<'a>> {
    let apps = if p.application.open { "open; an Apply button shows on its post" } else { "closed" };
    let leads: Vec<String> =
        p.leads.iter().map(|u| app.roster.get().map_or_else(|| u.to_string(), |r| lists::name_of(r, *u))).collect();
    let mut v = vec![
        Line::from(p.display()).bold(),
        field("Status", format!("{}: {}", p.status, status_help(p.status))),
        field("Area", if p.kind.is_empty() { "none" } else { &p.kind }.to_owned()),
        field("Applications", apps),
        field("Channels", p.channels.join(", ")),
        field("Leads", if leads.is_empty() { "none yet".into() } else { leads.join(", ") }),
    ];
    v.extend(p.stage.clone().map(|g| field("Stage", g)));
    v.extend(p.github.clone().map(|g| field("Code", g)));
    v.extend((!p.tags.is_empty()).then(|| field("Tags", p.tags.join(", "))));
    v.push(field("Brief", if p.brief.is_some() { "written (b to edit)" } else { "none yet (b to write one)" }));
    v.push(field("Short name", p.slug.clone()).italic());
    v
}

pub fn positions(f: &mut Frame, area: Rect, app: &App) {
    let Some(s) = &app.schema else { return not_loaded(f, area, app, "Positions") };
    let ps = &s.bot.positions;
    if ps.is_empty() {
        let hint = "No positions yet. Press n to make a role something people can apply for with /volunteer.";
        return text(f, area, "Positions", vec![Line::from(hint)], 0);
    }
    let label_of =
        |p: &chipsoc_config::bot::Position| p.label.clone().unwrap_or_else(|| lists::role_name(s, &p.role).to_owned());
    let labels = ps
        .iter()
        .map(|p| {
            let open = if p.open { Span::from("  open").cyan() } else { Span::from("  closed").italic() };
            Line::from(vec![Span::raw(label_of(p)), open])
        })
        .collect();
    let admin = s.bot.admin_role.as_deref().map_or("the admin role", |k| lists::role_name(s, k));
    let mut details = Vec::new();
    if let Some(p) = ps.get(app.sel()) {
        details = vec![
            Line::from(label_of(p)).bold(),
            field("Role", lists::role_name(s, &p.role).to_owned()),
            field("Admin", if p.admin { format!("yes: holders also get {admin}") } else { "no".into() }),
            field("Applications", if p.open { "open in /volunteer" } else { "closed" }),
            field("Intro", p.intro.clone().unwrap_or_else(|| "none".into())),
            Line::from("Questions").italic(),
        ];
        details.extend(p.questions.iter().enumerate().map(|(i, q)| {
            let mut extra = Vec::new();
            if q.long {
                extra.push("long");
            }
            if q.optional {
                extra.push("optional");
            }
            let extra = if extra.is_empty() { String::new() } else { format!(" ({})", extra.join(", ")) };
            Line::from(format!("  {}. {}{extra}", i + 1, q.ask))
        }));
        let held = s.holders.get(&p.role).map_or(0, Vec::len);
        details.push(field("Held by", format!("{held} (see tab 7)")));
    }
    list_and_details(f, area, app, ("Positions", labels, app.sel()), ("Details", details));
}

pub fn changes(f: &mut Frame, area: Rect, app: &App) {
    let mut lines = vec![changes_summary(app), Line::default()];
    let style = |m: Mark| match m {
        Mark::Create | Mark::Ok => Style::new().green(),
        Mark::Edit | Mark::Warn => Style::new().bold(),
        Mark::Move => Style::new().cyan(),
        Mark::Remove | Mark::Fail => Style::new().red(),
        Mark::Skip | Mark::Detail | Mark::Note => Style::new().italic(),
    };
    lines.extend(app.changes.rows().into_iter().skip(app.changes.scroll).map(|r| Line::styled(r.text, style(r.mark))));
    text(f, area, Tab::Changes.title(), lines, 0);
}

fn changes_summary(app: &App) -> Line<'_> {
    match &app.changes.view {
        View::Prompt(msg) => Line::from(*msg),
        View::Plan(p) if p.is_empty() => Line::from("✓ Discord matches the config. Nothing to send.").green(),
        View::Plan(_) if !app.problems().is_empty() => {
            Line::from("Fix the problems on Overview before applying these.").red().bold()
        }
        View::Plan(p) => {
            Line::from(format!("{} ready. Press a to apply.", count(p.changes(), "change", "changes"))).bold()
        }
        View::Applying { total, .. } if app.busy.is_some() => {
            Line::from(format!("Applying {}…", count(*total, "change", "changes"))).bold()
        }
        View::Applying { steps, .. } => {
            let failed = steps.iter().filter(|(_, r)| r.is_err()).count();
            let line = Line::from(format!("Finished: {} done, {failed} failed.", steps.len() - failed)).bold();
            if failed > 0 { line.red() } else { line.green() }
        }
        View::Pulled(notes) if notes.is_empty() => Line::from("Discord had no edits to copy.").green(),
        View::Pulled(_) => Line::from("Copied from Discord into the config:").bold(),
        View::Drift(d) if d.is_clean() => Line::from("✓ Discord matches the config.").green(),
        View::Drift(_) => Line::from("Who changed what since the last apply:").bold(),
    }
}

use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
};
use serde_json::Value;

use super::{
    model::{App, Focus, Item, Mode, enabled_of, source_of},
    schema::Field,
};

const DIM: Style = Style::new().fg(Color::DarkGray);
const ACCENT: Color = Color::Cyan;

pub fn draw(frame: &mut Frame, app: &App) {
    let [main, footer] = Layout::vertical([Constraint::Min(5), Constraint::Length(2)]).areas(frame.area());
    let [left, right] = Layout::horizontal([Constraint::Percentage(36), Constraint::Percentage(64)]).areas(main);
    draw_list(frame, app, left);
    draw_details(frame, app, right);
    draw_footer(frame, app, footer);
    match &app.mode {
        Mode::Edit(text) => {
            let field = app.fields().get(app.field_sel).cloned();
            let title = match field {
                Some(field) => format!("{} ({}): Enter set, Esc cancel, empty = unset", field.label(), field.kind.hint()),
                None => "Value: Enter set, Esc cancel, empty = unset".into(),
            };
            popup(frame, &title, text, &app.notice);
        }
        Mode::Add(text) => popup(frame, "Add an extension: github:owner/repo, a .wasm URL, a file path, or builtin:docs", text, &app.notice),
        Mode::ConfirmDelete => {
            let name = match app.item() {
                Item::Ext(i) => app.name_of(i),
                _ => "this extension".into(),
            };
            popup_text(frame, &format!("Remove {name} from the config?  y = yes, any other key = no"));
        }
        Mode::ConfirmQuit => popup_text(frame, "Save changes before quitting?  y = save, n = discard, Ctrl+C = quit anyway, other keys = keep editing"),
        Mode::Normal => {}
    }
}

fn draw_list(frame: &mut Frame, app: &App, area: Rect) {
    let mut rows: Vec<ListItem> = Vec::new();
    for item in app.items() {
        let line = match item {
            Item::Server => Line::from("Server"),
            Item::Tunnel => {
                let on = app.tunnel_on();
                Line::from(vec![Span::styled(if on { "[x] " } else { "[ ] " }, switch_style(on)), Span::raw("Cloudflare tunnel")])
            }
            Item::Ext(i) => {
                let on = enabled_of(&app.entries()[i]);
                let name = app.name_of(i);
                let mut spans = vec![Span::styled(if on { "[x] " } else { "[ ] " }, switch_style(on)), Span::styled(name, if on { Style::default() } else { DIM })];
                if app.problems.contains_key(source_of(&app.entries()[i])) {
                    spans.push(Span::styled("  !", Style::default().fg(Color::Red)));
                }
                Line::from(spans)
            }
            Item::Add => Line::from(Span::styled("+ Add an extension", Style::default().fg(ACCENT))),
        };
        rows.push(ListItem::new(line));
    }
    let mut state = ListState::default().with_selected(Some(app.sel));
    let highlight = if app.focus == Focus::List { Style::default().add_modifier(Modifier::REVERSED) } else { Style::default().add_modifier(Modifier::BOLD) };
    let list = List::new(rows).block(Block::default().borders(Borders::ALL).title(" samishisa ")).highlight_style(highlight);
    frame.render_stateful_widget(list, area, &mut state);
}

fn switch_style(on: bool) -> Style {
    Style::default().fg(if on { Color::Green } else { Color::DarkGray })
}

fn draw_details(frame: &mut Frame, app: &App, area: Rect) {
    let mut lines: Vec<Line> = Vec::new();
    let title;
    match app.item() {
        Item::Server => title = " Server ".to_owned(),
        Item::Tunnel => {
            title = " Cloudflare tunnel ".to_owned();
            lines.push(Line::from("Makes this server reachable from the internet through cloudflared."));
            lines.push(Line::from(Span::styled("No token = a quick tunnel with a random address, shown in the log. A token = your own named tunnel.", DIM)));
            lines.push(Line::default());
        }
        Item::Ext(i) => {
            let entry = &app.entries()[i];
            let source = source_of(entry);
            title = format!(" {} ", app.name_of(i));
            if let Some(info) = app.infos.get(source) {
                let version = info.version.as_deref().map(|v| format!(" {v}")).unwrap_or_default();
                lines.push(Line::from(Span::styled(format!("{}{version}, {} route(s)", info.name, info.routes), Style::default().add_modifier(Modifier::BOLD))));
                if let Some(description) = &info.description {
                    lines.push(Line::from(description.clone()));
                }
            }
            lines.push(Line::from(Span::styled(source.to_owned(), DIM)));
            if !enabled_of(entry) {
                lines.push(Line::from(Span::styled("Switched off: not served.", Style::default().fg(Color::Yellow))));
            }
            if let Some(problem) = app.problems.get(source) {
                lines.push(Line::from(Span::styled(format!("Could not load: {problem}"), Style::default().fg(Color::Red))));
            }
            lines.push(Line::default());
        }
        Item::Add => {
            title = " Add ".to_owned();
            lines.push(Line::from("Press Enter to add an extension."));
            lines.push(Line::from(Span::styled("github:owner/repo   https://example.com/ext.wasm   C:\\path\\ext.wasm", DIM)));
            lines.push(Line::from(Span::styled("builtin:docs = the interactive API docs page at /docs", DIM)));
        }
    }
    let fields = app.fields();
    let editing = app.focus == Focus::Fields;
    for (n, field) in fields.iter().enumerate() {
        let selected = editing && n == app.field_sel;
        let label = format!("{}{}", field.label(), if field.required { " *" } else { "" });
        let (value, set) = display_value(app, field);
        let marker = if selected { "> " } else { "  " };
        let value_style = if set { Style::default().fg(ACCENT) } else { DIM };
        let mut line = Line::from(vec![Span::raw(marker), Span::styled(format!("{label:<27} "), Style::default()), Span::styled(value, value_style)]);
        if selected {
            line = line.style(Style::default().add_modifier(Modifier::REVERSED));
        }
        lines.push(line);
    }
    let described = match app.item() {
        Item::Ext(i) => app.infos.contains_key(source_of(&app.entries()[i])),
        _ => true,
    };
    if fields.is_empty() && matches!(app.item(), Item::Ext(_)) {
        let why = if described { "This extension has no settings." } else { "Its settings are unknown until it can be loaded." };
        lines.push(Line::from(Span::styled(why, DIM)));
    } else if !described && !fields.is_empty() {
        lines.push(Line::default());
        lines.push(Line::from(Span::styled("Settings shown as written; the extension couldn't be loaded to describe them.", DIM)));
    }
    if fields.iter().any(|f| f.required) {
        lines.push(Line::default());
        lines.push(Line::from(Span::styled("* required", DIM)));
    }
    if editing && let Some(description) = fields.get(app.field_sel).and_then(|f| f.description.as_ref()) {
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(description.clone(), Style::default().fg(Color::Yellow))));
    }
    let border = if editing { Style::default().fg(ACCENT) } else { Style::default() };
    let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false }).block(Block::default().borders(Borders::ALL).border_style(border).title(title));
    frame.render_widget(paragraph, area);
}

fn display_value(app: &App, field: &Field) -> (String, bool) {
    match app.value(field) {
        None => match &field.default {
            Some(Value::Bool(b)) => (format!("{} (default)", if *b { "on" } else { "off" }), false),
            Some(Value::String(s)) => (format!("{s} (default)"), false),
            Some(other) => (format!("{other} (default)"), false),
            None => ("(not set)".into(), false),
        },
        Some(Value::String(s)) if field.secret() && !s.starts_with("${") && !s.is_empty() => ("********".into(), true),
        Some(Value::String(s)) => (s.clone(), true),
        Some(Value::Bool(b)) => (if *b { "on".into() } else { "off".into() }, true),
        Some(other) => (other.to_string(), true),
    }
}

fn draw_footer(frame: &mut Frame, app: &App, area: Rect) {
    let keys = match (&app.mode, app.focus) {
        (Mode::Normal, Focus::List) => "↑↓ move   Space on/off   Enter settings   a add   d remove   s save   q quit",
        (Mode::Normal, Focus::Fields) => "↑↓ move   Enter edit   x clear   Esc back   s save   q quit",
        _ => "",
    };
    let dirty = if app.dirty { "  (unsaved changes)" } else { "" };
    let second = if app.notice.is_empty() || !matches!(app.mode, Mode::Normal) {
        Span::styled(app.path.display().to_string(), DIM)
    } else {
        Span::styled(app.notice.clone(), Style::default().fg(if app.notice_is_error { Color::Red } else { Color::Green }))
    };
    let lines = vec![Line::from(vec![Span::styled(keys, DIM), Span::styled(dirty, Style::default().fg(Color::Yellow))]), Line::from(second)];
    frame.render_widget(Paragraph::new(lines), area);
}

fn centered(area: Rect, height: u16) -> Rect {
    let width = area.width.saturating_sub(8).clamp(20, 90);
    Rect { x: area.x + (area.width.saturating_sub(width)) / 2, y: area.y + area.height.saturating_sub(height) / 2, width: width.min(area.width), height: height.min(area.height) }
}

fn popup(frame: &mut Frame, title: &str, text: &str, notice: &str) {
    let area = centered(frame.area(), 5);
    frame.render_widget(Clear, area);
    let mut lines = vec![Line::from(vec![Span::raw(text.to_owned()), Span::styled("▏", Style::default().fg(ACCENT))])];
    if !notice.is_empty() {
        lines.push(Line::from(Span::styled(notice.to_owned(), Style::default().fg(Color::Red))));
    }
    frame.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }).block(Block::default().borders(Borders::ALL).border_style(Style::default().fg(ACCENT)).title(format!(" {title} "))),
        area,
    );
}

fn popup_text(frame: &mut Frame, text: &str) {
    let area = centered(frame.area(), 3);
    frame.render_widget(Clear, area);
    frame.render_widget(Paragraph::new(text.to_owned()).block(Block::default().borders(Borders::ALL).border_style(Style::default().fg(Color::Yellow))), area);
}

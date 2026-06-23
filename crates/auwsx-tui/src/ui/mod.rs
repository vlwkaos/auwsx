//! Rendering. [`draw`] lays out one operator console: tree on the left,
//! contextual detail on the right, and a wsx-style status/footer line.

mod backlog;
mod config;
mod issue;
mod logs;
mod overview;
pub(crate) mod theme;

use crate::app::{App, FormField, View};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;

/// Accent for the focused/selected element across all views. Re-exported from
/// [`theme`] so existing `ACCENT` references resolve to the themed color.
pub(crate) use theme::ACCENT;

pub fn draw(frame: &mut Frame, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(0),    // body
            Constraint::Length(1), // footer
        ])
        .split(frame.area());

    match app.view {
        View::Overview => overview::render(frame, app, chunks[0]),
        View::Issue => issue::render(frame, app, chunks[0]),
        View::Backlog => backlog::render(frame, app, chunks[0]),
        View::Logs => logs::render(frame, app, chunks[0]),
        View::Config => config::render(frame, app, chunks[0]),
    }
    draw_footer(frame, app, chunks[1]);
    if app.form.is_some() {
        draw_form(frame, app);
    }
}

fn draw_footer(frame: &mut Frame, app: &App, area: Rect) {
    // Version pinned to the right; hint/status fills the remaining left space.
    let version = format!(" v{} ", env!("CARGO_PKG_VERSION"));
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(0), Constraint::Length(version.len() as u16)])
        .split(area);

    // A status/error message preempts the hint line when present.
    let (text, style) = if !app.status.is_empty() {
        (app.status.clone(), Style::default().fg(theme::WARN))
    } else {
        let conn = if app.connected { "live" } else { "offline" };
        (
            format!(
                "{conn} · j/k move · ⏎ open · a project · c config · n backlog · i issue · f steering · A approve · x dismiss · T triage · Space toggle · E run · r refresh · q quit"
            ),
            theme::hint(),
        )
    };
    frame.render_widget(Paragraph::new(Span::styled(text, style)), cols[0]);
    frame.render_widget(Paragraph::new(Span::styled(version, theme::dim())), cols[1]);
}

fn draw_form(frame: &mut Frame, app: &App) {
    let Some(form) = app.form.as_ref() else {
        return;
    };
    // Git-repo completions, shown only when the cursor is on the repository field.
    let suggestions = app.repo_suggestions();
    let extra = suggestions.len() as u16 + if suggestions.is_empty() { 0 } else { 1 };
    let area = centered_rect(
        84,
        form_height(form.fields.len() as u16, extra),
        frame.area(),
    );
    frame.render_widget(Clear, area);

    let mut lines = Vec::new();
    for (idx, field) in form.fields.iter().enumerate() {
        let marker = if idx == form.current { ">" } else { " " };
        let optional = if field.optional { " optional" } else { "" };
        let value = if idx == form.current {
            active_form_value(field)
        } else if field.value.is_empty() {
            String::new()
        } else {
            field.value.clone()
        };
        lines.push(Line::from(vec![
            Span::styled(
                format!("{marker} {:>18}{optional}: ", field.label),
                theme::dim(),
            ),
            Span::styled(
                value,
                if idx == form.current {
                    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(theme::TEXT)
                },
            ),
        ]));

        if idx == form.current && field.key == "repo_path" && !suggestions.is_empty() {
            push_repo_suggestions(&mut lines, &suggestions);
        }
    }

    lines.push(Line::raw(""));
    lines.push(Line::from(Span::styled(
        "Enter next/submit · Tab field/complete · Backspace edit · Esc cancel",
        theme::hint(),
    )));

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme::border(true))
        .title(Span::styled(format!(" {} ", form.title), theme::title()));
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(block),
        area,
    );
}

fn active_form_value(field: &FormField) -> String {
    let mut value = field.value.clone();
    value.insert(field.cursor_byte_index(), '_');
    value
}

fn push_repo_suggestions(lines: &mut Vec<Line<'static>>, suggestions: &[String]) {
    lines.push(Line::from(Span::styled(
        "    found repos (Tab fills top):",
        theme::dim(),
    )));
    for repo in suggestions {
        lines.push(Line::from(vec![
            Span::styled("      • ", Style::default().fg(theme::TREE_CONNECTOR)),
            Span::styled(repo.clone(), Style::default().fg(theme::TEXT)),
        ]));
    }
}

fn form_height(field_count: u16, extra: u16) -> u16 {
    field_count
        .saturating_add(4)
        .saturating_add(extra)
        .clamp(6, 28)
}

fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    let w = width.min(area.width.saturating_sub(2)).max(1);
    let h = height.min(area.height.saturating_sub(2)).max(1);
    Rect {
        x: area.x + area.width.saturating_sub(w) / 2,
        y: area.y + area.height.saturating_sub(h) / 2,
        width: w,
        height: h,
    }
}

// --- shared helpers used by the view modules --------------------------------

/// Build a bordered, selection-highlighting list and render it statefully.
/// `focused` accents the border + highlight for the active pane.
pub(crate) fn render_list(
    frame: &mut Frame,
    area: Rect,
    title: &str,
    items: Vec<ListItem>,
    selected: usize,
    focused: bool,
) {
    let is_empty = items.is_empty();
    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(theme::border(focused))
                .title(Span::styled(format!(" {title} "), theme::title())),
        )
        .highlight_style(theme::highlight(focused))
        .highlight_symbol("▌");
    let mut state = ListState::default();
    // No phantom highlight on an empty list.
    state.select((!is_empty).then_some(selected));
    frame.render_stateful_widget(list, area, &mut state);
}

#[cfg(test)]
mod tests {
    use crate::app::{App, Form, FormField, FormKind, View};
    use crate::ui::draw;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn draw_view(view: View, w: u16, h: u16) {
        let mut app = App::new(std::path::PathBuf::from("/tmp/nonexistent.sock"));
        app.view = view;
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| draw(f, &app)).unwrap();
    }

    fn rendered_empty_app() -> String {
        let app = App::new(std::path::PathBuf::from("/tmp/nonexistent.sock"));
        let mut term = Terminal::new(TestBackend::new(100, 30)).unwrap();
        term.draw(|f| draw(f, &app)).unwrap();
        term.backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect()
    }

    fn rendered_app(app: &App) -> String {
        let mut term = Terminal::new(TestBackend::new(100, 30)).unwrap();
        term.draw(|f| draw(f, app)).unwrap();
        term.backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect()
    }

    fn test_field(key: &'static str, label: &'static str, value: &str) -> FormField {
        FormField {
            key,
            label,
            value: value.into(),
            cursor: value.chars().count(),
            optional: false,
        }
    }

    fn appears_in_order(rendered: &str, needles: &[&str]) -> bool {
        let mut offset = 0;
        for needle in needles {
            let Some(found) = rendered[offset..].find(needle) else {
                return false;
            };
            offset += found + needle.len();
        }
        true
    }

    #[test]
    fn draw_overview_normal_no_panic() {
        draw_view(View::Overview, 100, 30);
    }

    #[test]
    fn draw_issue_normal_no_panic() {
        draw_view(View::Issue, 100, 30);
    }

    #[test]
    fn draw_backlog_normal_no_panic() {
        draw_view(View::Backlog, 100, 30);
    }

    #[test]
    fn draw_logs_normal_no_panic() {
        draw_view(View::Logs, 100, 30);
    }

    #[test]
    fn draw_config_normal_no_panic() {
        draw_view(View::Config, 100, 30);
    }

    #[test]
    fn draw_overview_tiny_no_panic() {
        draw_view(View::Overview, 8, 4);
    }

    #[test]
    fn draw_issue_tiny_no_panic() {
        draw_view(View::Issue, 8, 4);
    }

    #[test]
    fn draw_backlog_tiny_no_panic() {
        draw_view(View::Backlog, 8, 4);
    }

    #[test]
    fn draw_logs_tiny_no_panic() {
        draw_view(View::Logs, 8, 4);
    }

    #[test]
    fn draw_config_tiny_no_panic() {
        draw_view(View::Config, 8, 4);
    }

    #[test]
    fn draw_empty_app_renders_tree_title() {
        assert!(rendered_empty_app().contains("auwsx"));
    }

    #[test]
    fn draw_empty_app_renders_project_row() {
        assert!(rendered_empty_app().contains("Project"));
    }

    #[test]
    fn active_form_value_renders_cursor_at_stored_char_position() {
        let field = FormField {
            key: "text",
            label: "text",
            value: "abé".to_string(),
            cursor: 2,
            optional: false,
        };

        assert_eq!(super::active_form_value(&field), "ab_é");
    }

    #[test]
    fn given_focused_repo_field_with_suggestions_when_drawn_then_suggestion_precedes_next_field() {
        let mut app = App::new(std::path::PathBuf::from("/tmp/nonexistent.sock"));
        app.scanned_repos = vec!["~/foo".to_string()];
        app.form = Some(Form {
            kind: FormKind::Project,
            title: "t",
            fields: vec![
                test_field("repo_path", "Repository", "foo"),
                test_field("branch", "Default branch", "main"),
            ],
            current: 0,
        });
        assert!(appears_in_order(
            &rendered_app(&app),
            &["Repository", "found repos", "~/foo", "Default branch"],
        ));
    }
}

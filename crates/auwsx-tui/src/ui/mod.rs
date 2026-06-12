//! Rendering. [`draw`] lays out one operator console: tree on the left,
//! contextual detail on the right, and a wsx-style status/footer line.

mod backlog;
mod config;
mod issue;
mod logs;
mod overview;

use crate::app::{App, View};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;

/// Accent for the focused/selected element across all views.
pub(crate) const ACCENT: Color = Color::Cyan;

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
    // A status/error message preempts the hint line when present.
    let text = if !app.status.is_empty() {
        app.status.clone()
    } else {
        let conn = if app.connected { "live" } else { "offline" };
        format!(
            "{conn} · j/k move · a project · c config · n backlog · i issue · f steering · A approve · x dismiss · T triage · Space toggle · E run · r refresh · q quit"
        )
    };
    let style = if app.status.is_empty() {
        Style::default().fg(Color::DarkGray)
    } else {
        Style::default().fg(Color::Yellow)
    };
    frame.render_widget(Paragraph::new(Span::styled(text, style)), area);
}

fn draw_form(frame: &mut Frame, app: &App) {
    let Some(form) = app.form.as_ref() else {
        return;
    };
    let area = centered_rect(74, form_height(form.fields.len() as u16), frame.area());
    frame.render_widget(Clear, area);

    let mut lines = Vec::new();
    for (idx, field) in form.fields.iter().enumerate() {
        let marker = if idx == form.current { ">" } else { " " };
        let optional = if field.optional { " optional" } else { "" };
        let value = if idx == form.current {
            format!("{}_", field.value)
        } else if field.value.is_empty() {
            String::new()
        } else {
            field.value.clone()
        };
        lines.push(Line::from(vec![
            Span::styled(
                format!("{marker} {:>12}{optional}: ", field.label),
                Style::default().fg(Color::DarkGray),
            ),
            Span::styled(
                value,
                if idx == form.current {
                    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                },
            ),
        ]));
    }
    lines.push(Line::raw(""));
    lines.push(Line::from(Span::styled(
        "Enter next/submit · Tab field · Backspace edit · Esc cancel",
        Style::default().fg(Color::DarkGray),
    )));

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(ACCENT))
        .title(Span::styled(
            format!(" {} ", form.title),
            Style::default().add_modifier(Modifier::BOLD),
        ));
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(block),
        area,
    );
}

fn form_height(field_count: u16) -> u16 {
    field_count.saturating_add(4).clamp(6, 22)
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
    let border_style = if focused {
        Style::default().fg(ACCENT)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let is_empty = items.is_empty();
    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(border_style)
                .title(Span::styled(
                    format!(" {title} "),
                    Style::default().add_modifier(Modifier::BOLD),
                )),
        )
        .highlight_style(
            Style::default()
                .add_modifier(Modifier::BOLD)
                .bg(if focused { ACCENT } else { Color::DarkGray })
                .fg(Color::Black),
        )
        .highlight_symbol("▌");
    let mut state = ListState::default();
    // No phantom highlight on an empty list.
    state.select((!is_empty).then_some(selected));
    frame.render_stateful_widget(list, area, &mut state);
}

#[cfg(test)]
mod tests {
    use crate::app::{App, View};
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
}

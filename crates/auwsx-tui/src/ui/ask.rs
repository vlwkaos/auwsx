//! Ask-mode history: project-level Q&A answers, newest first.

use super::theme;
use crate::app::App;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::Frame;

pub(super) fn render(frame: &mut Frame, app: &App, area: Rect) {
    let mut lines = Vec::new();
    if app.ask_answers.is_empty() {
        lines.push(Line::styled(
            "No answers yet. Press ? to ask this project.",
            theme::dim(),
        ));
    }
    for answer in &app.ask_answers {
        lines.push(Line::from(vec![
            Span::styled(
                format!("#{} ", answer.id),
                Style::default()
                    .fg(theme::TEXT)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(answer.mode.as_str(), theme::dim()),
            Span::styled(
                format!(
                    "  {}",
                    crate::ui::schedule::format_epoch_ms_local(answer.created_at)
                ),
                theme::dim(),
            ),
        ]));
        lines.push(Line::from(vec![
            Span::styled("Q: ", theme::dim()),
            Span::styled(answer.question.clone(), Style::default().fg(theme::TEXT)),
        ]));
        for line in answer.answer.lines().take(12) {
            lines.push(Line::from(vec![
                Span::styled("A: ", theme::dim()),
                Span::styled(line.to_string(), Style::default().fg(theme::TEXT)),
            ]));
        }
        lines.push(Line::raw(""));
    }

    frame.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(theme::border(false))
                .title(Span::styled(" Ask ", theme::title())),
        ),
        area,
    );
}

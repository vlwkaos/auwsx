//! Backlog: intake items for the selected project, with approve/dismiss/triage.

use super::{render_list, theme};
use crate::app::App;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::ListItem;
use ratatui::Frame;

pub(super) fn render(frame: &mut Frame, app: &App, area: Rect) {
    let items: Vec<ListItem> = app
        .backlog()
        .iter()
        .map(|b| {
            ListItem::new(Line::from(vec![
                Span::styled(
                    format!("{:<9} ", b.approval.as_str()),
                    Style::default().fg(theme::approval(b.approval.as_str())),
                ),
                Span::styled(format!("{:<7} ", b.source.as_str()), theme::dim()),
                Span::styled(b.text.clone(), Style::default().fg(theme::TEXT)),
            ]))
        })
        .collect();
    let title = match app.selected_project_id() {
        Some(_) => format!("Backlog ({})", app.backlog().len()),
        None => "Backlog — no project selected".to_string(),
    };
    render_list(frame, area, &title, items, app.backlog_sel, true);
}

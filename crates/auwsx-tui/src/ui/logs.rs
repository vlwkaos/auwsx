//! Logs: a live tail of daemon events (newest at the bottom).

use super::theme;
use crate::app::App;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::Span;
use ratatui::widgets::{Block, Borders, List, ListItem};
use ratatui::Frame;

pub(super) fn render(frame: &mut Frame, app: &App, area: Rect) {
    // Show the most recent events that fit; the list grows downward so the
    // newest line sits just above the footer.
    let rows = area.height.saturating_sub(2) as usize; // minus the border
    let start = app.log.len().saturating_sub(rows.max(1));
    let items: Vec<ListItem> = app
        .log
        .iter()
        .skip(start)
        .map(|l| ListItem::new(l.clone()))
        .collect();

    let title = if app.connected {
        format!(" Event log ({}) — live ", app.log.len())
    } else {
        format!(" Event log ({}) — offline (r to retry) ", app.log.len())
    };
    let list = List::new(items).block(
        Block::default()
            .borders(Borders::ALL)
            .title(Span::styled(title, theme::title()))
            .border_style(Style::default().fg(if app.connected {
                theme::OK
            } else {
                theme::BORDER
            })),
    );
    frame.render_widget(list, area);
}

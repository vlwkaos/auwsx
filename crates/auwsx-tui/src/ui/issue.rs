//! Issue detail: header fields (left) + subtasks / findings / steering (right).

use crate::app::App;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph, Wrap};
use ratatui::Frame;

pub(super) fn render(frame: &mut Frame, app: &App, area: Rect) {
    let Some(issue) = app.detail.issue.as_ref() else {
        frame.render_widget(
            Paragraph::new("No issue selected — pick one in Overview (⏎).")
                .block(Block::default().borders(Borders::ALL).title(" Issue ")),
            area,
        );
        return;
    };

    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
        .split(area);

    // --- left: header fields ---
    let mut lines = vec![
        kv("id", &issue.id.to_string()),
        kv("status", issue.status.as_str()),
        kv("title", &issue.title),
    ];
    if let Some(d) = &issue.description {
        lines.push(kv("desc", d));
    }
    lines.push(kv("branch", issue.branch.as_deref().unwrap_or("(none)")));
    lines.push(kv(
        "worktree",
        issue.worktree_path.as_deref().unwrap_or("(none)"),
    ));
    lines.push(kv("review_round", &issue.review_round.to_string()));
    lines.push(kv(
        "conflict_attempts",
        &issue.conflict_attempts.to_string(),
    ));
    lines.push(kv(
        "pending_steering",
        if issue.has_pending_steering {
            "yes"
        } else {
            "no"
        },
    ));
    frame.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: true }).block(
            Block::default().borders(Borders::ALL).title(Span::styled(
                format!(" Issue #{} ", issue.id),
                Style::default().add_modifier(Modifier::BOLD),
            )),
        ),
        cols[0],
    );

    // --- right: subtasks / findings / steering stacked ---
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage(40),
            Constraint::Percentage(35),
            Constraint::Percentage(25),
        ])
        .split(cols[1]);

    let subtasks: Vec<ListItem> = app
        .detail
        .subtasks
        .iter()
        .map(|s| {
            ListItem::new(format!(
                "[{}] {}. {}",
                if s.done { 'x' } else { ' ' },
                s.ord,
                s.text
            ))
        })
        .collect();
    list_block(
        frame,
        rows[0],
        &format!("Subtasks ({})", subtasks.len()),
        subtasks,
    );

    let findings: Vec<ListItem> = app
        .detail
        .findings
        .iter()
        .map(|f| {
            ListItem::new(Line::from(vec![
                Span::styled(
                    format!("[{}] ", f.severity.as_str()),
                    Style::default().fg(severity_color(f.severity.as_str())),
                ),
                Span::raw(format!("{} — {}", f.status.as_str(), f.title)),
            ]))
        })
        .collect();
    list_block(
        frame,
        rows[1],
        &format!("Findings ({})", findings.len()),
        findings,
    );

    let steering: Vec<ListItem> = app
        .detail
        .steering
        .iter()
        .map(|s| ListItem::new(format!("[{}] {}", s.source.as_str(), s.note)))
        .collect();
    list_block(
        frame,
        rows[2],
        &format!("Pending steering ({})", steering.len()),
        steering,
    );
}

fn kv<'a>(key: &'a str, val: &str) -> Line<'a> {
    Line::from(vec![
        Span::styled(format!("{key:>17}: "), Style::default().fg(Color::DarkGray)),
        Span::raw(val.to_string()),
    ])
}

fn list_block(frame: &mut Frame, area: Rect, title: &str, items: Vec<ListItem>) {
    let list = List::new(items).block(Block::default().borders(Borders::ALL).title(Span::styled(
        format!(" {title} "),
        Style::default().add_modifier(Modifier::BOLD),
    )));
    frame.render_widget(list, area);
}

fn severity_color(sev: &str) -> Color {
    match sev {
        "blocker" => Color::Red,
        "major" => Color::LightRed,
        "minor" => Color::Yellow,
        _ => Color::DarkGray, // nit
    }
}

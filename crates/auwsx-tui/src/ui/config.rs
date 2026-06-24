//! Config: the selected project's policy + agent commands. Edits go through
//! daemon IPC; this view reflects the `projects` row.

use super::theme;
use crate::app::App;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::Frame;

pub(super) fn render(frame: &mut Frame, app: &App, area: Rect) {
    let Some(p) = app.projects.get(app.proj_sel) else {
        frame.render_widget(
            Paragraph::new("No project selected.")
                .block(Block::default().borders(Borders::ALL).title(" Config ")),
            area,
        );
        return;
    };

    let lines = vec![
        kv("name", p.name.clone()),
        kv("repo", p.repo_path.clone()),
        kv("default_branch", p.default_branch.clone()),
        sep(),
        kv("main_agent", p.main_agent_cmd.clone()),
        kv("plan_agent", p.plan_agent_cmd.clone()),
        kv("work_agent", p.work_agent_cmd.clone()),
        kv(
            "review_agent",
            p.review_agent_cmd
                .clone()
                .unwrap_or_else(|| "(falls back to work)".into()),
        ),
        sep(),
        kv(
            "completion_policy",
            p.completion_policy.as_str().to_string(),
        ),
        kv(
            "plan_gate_timeout",
            format!("{} min", p.plan_gate_timeout_min),
        ),
        kv(
            "completion_soft_timeout",
            format!("{} min", p.completion_soft_timeout_min),
        ),
        kv(
            "iteration_timeout",
            format!("{} min", p.iteration_timeout_min),
        ),
        kv("review_max_rounds", p.review_max_rounds.to_string()),
        kv("conflict_max_attempts", p.conflict_max_attempts.to_string()),
        kv("max_concurrency", p.max_concurrency.to_string()),
        kv("merge_mode", p.merge_mode.as_str().to_string()),
    ];

    frame.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: true }).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(theme::border(false))
                .title(Span::styled(
                    format!(" Config — {} ", p.name),
                    theme::title(),
                )),
        ),
        area,
    );
}

fn kv(key: &str, val: String) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{key:>24}: "), theme::dim()),
        Span::styled(val, Style::default().fg(theme::TEXT)),
    ])
}

fn sep() -> Line<'static> {
    Line::from(Span::styled("  ─", Style::default().fg(theme::BORDER)))
}

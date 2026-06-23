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
        kv("Name", p.name.clone()),
        kv("Repository", p.repo_path.clone()),
        kv("Default branch", p.default_branch.clone()),
        sep(),
        kv("Main command", p.main_agent_cmd.clone()),
        kv("Plan command", p.plan_agent_cmd.clone()),
        kv("Work command", p.work_agent_cmd.clone()),
        kv(
            "Review command",
            p.review_agent_cmd
                .clone()
                .unwrap_or_else(|| "(falls back to work)".into()),
        ),
        sep(),
        kv(
            "Completion policy",
            p.completion_policy.as_str().to_string(),
        ),
        kv(
            "Plan gate timeout",
            format!("{} min", p.plan_gate_timeout_min),
        ),
        kv(
            "Completion timeout",
            format!("{} min", p.completion_soft_timeout_min),
        ),
        kv(
            "Iteration timeout",
            format!("{} min", p.iteration_timeout_min),
        ),
        kv(
            "Main job timeout",
            format!("{} min", p.main_job_timeout_min),
        ),
        kv("Review rounds", p.review_max_rounds.to_string()),
        kv("Conflict attempts", p.conflict_max_attempts.to_string()),
        kv("Concurrency", p.max_concurrency.to_string()),
        kv(
            "Schedule interval",
            p.schedule_interval_min
                .map(|v| v.to_string())
                .unwrap_or_else(|| "(manual)".into()),
        ),
        kv("Merge mode", p.merge_mode.as_str().to_string()),
        kv(
            "Skills path",
            p.skill_path.clone().unwrap_or_else(|| "(none)".into()),
        ),
        kv("Deepsleep interval", p.deepsleep_interval_days.to_string()),
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
        Span::styled(format!("{key:>20}: "), theme::dim()),
        Span::styled(val, Style::default().fg(theme::TEXT)),
    ])
}

fn sep() -> Line<'static> {
    Line::from(Span::styled("  ─", Style::default().fg(theme::BORDER)))
}

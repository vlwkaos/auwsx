//! Operator console: one tree on the left, contextual detail on the right.

use super::{render_list, ACCENT};
use crate::app::{App, TreeItem};
use auwsx_core::backlog::Approval;
use auwsx_core::db::scheduler_runs::{SchedulerRunDecision, SchedulerRunPicked};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, ListItem, Paragraph, Wrap};
use ratatui::Frame;

pub(super) fn render(frame: &mut Frame, app: &App, area: Rect) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(36), Constraint::Percentage(64)])
        .split(area);

    let rows = app.tree_rows();
    let items: Vec<ListItem> = rows
        .iter()
        .map(|r| {
            let prefix = match r.depth {
                0 => "",
                1 => "  ",
                _ => "    ",
            };
            ListItem::new(format!("{prefix}{}", r.label))
        })
        .collect();
    render_list(frame, cols[0], "auwsx", items, app.tree_sel, true);

    match app.selected_tree_item().unwrap_or(TreeItem::Project) {
        TreeItem::Project => render_project(frame, app, cols[1]),
        TreeItem::RoutinesRoot => render_routines(frame, app, cols[1]),
        TreeItem::Routine(_) => render_routine(frame, app, cols[1]),
        TreeItem::BacklogRoot => render_backlog_summary(frame, app, cols[1]),
        TreeItem::Backlog(_) => render_backlog(frame, app, cols[1]),
        TreeItem::IssuesRoot => render_issue_summary(frame, app, cols[1]),
        TreeItem::Issue(_) => render_issue(frame, app, cols[1]),
    }
}

fn render_project(frame: &mut Frame, app: &App, area: Rect) {
    let Some(p) = app.projects.get(app.proj_sel) else {
        panel(
            frame,
            area,
            "Project",
            vec![Line::raw("No project registered.")],
        );
        return;
    };
    let running = app
        .issues
        .iter()
        .filter(|i| i.status.is_actionable())
        .count();
    let last_auto_tick = app
        .recent_scheduler_runs
        .iter()
        .find(|run| run.source == auwsx_core::db::scheduler_runs::SchedulerRunSource::Auto);
    let mut lines = vec![
        kv("name", &p.name),
        kv("repo", &p.repo_path),
        kv("branch", &p.default_branch),
        kv("policy", p.completion_policy.as_str()),
        kv("schedule", &schedule_label(p.schedule_interval_min)),
        kv(
            "last tick",
            &app.recent_scheduler_runs
                .first()
                .map(|run| format!("{} {}", run.fired_at, run.source.as_str()))
                .unwrap_or_else(|| "(none)".to_string()),
        ),
        kv(
            "next auto",
            &next_due_label(p.schedule_interval_min, last_auto_tick),
        ),
        kv("plan gate", &format!("{} min", p.plan_gate_timeout_min)),
        kv(
            "completion gate",
            &format!("{} min", p.completion_soft_timeout_min),
        ),
        kv("issues", &app.issues.len().to_string()),
        kv("actionable", &running.to_string()),
        kv("backlog", &app.backlog.len().to_string()),
        kv("routines", &app.routines.len().to_string()),
        sep(),
        Line::styled(
            "Recent scheduler ticks",
            Style::default().add_modifier(Modifier::BOLD),
        ),
    ];
    if app.recent_scheduler_runs.is_empty() {
        lines.push(Line::styled(
            "no ticks recorded; daemon may be stopped or this project has not been ticked yet",
            Style::default().fg(Color::Yellow),
        ));
    }
    for run in app.recent_scheduler_runs.iter().take(5) {
        lines.push(Line::raw(format!(
            "#{:<3} {:<6} fired_at={} {}",
            run.id,
            run.source.as_str(),
            run.fired_at,
            summarize_picked(run.picked.as_deref())
        )));
    }
    lines.push(sep());
    lines.push(Line::styled(
        "Recent agent runs",
        Style::default().add_modifier(Modifier::BOLD),
    ));
    for run in app.recent_agent_runs.iter().take(6) {
        lines.push(Line::raw(format!(
            "#{:<3} {:<6} {:<13} {}",
            run.id,
            run.role.as_str(),
            run.phase,
            run.log_path.as_deref().unwrap_or("")
        )));
    }
    lines.push(sep());
    lines.push(Line::styled(
        "Recent main jobs",
        Style::default().add_modifier(Modifier::BOLD),
    ));
    for job in app.recent_main_jobs.iter().take(6) {
        lines.push(Line::raw(format!(
            "#{:<3} {:?} {:<10} {}",
            job.id,
            job.status,
            job.kind,
            job.outcome.as_deref().unwrap_or("")
        )));
    }
    panel(frame, area, &format!("Project {}", p.name), lines);
}

fn render_routines(frame: &mut Frame, app: &App, area: Rect) {
    let mut lines = vec![Line::raw("Configured recurring main-workspace prompts.")];
    for r in &app.routines {
        lines.push(Line::raw(format!(
            "#{:<3} {:<3} {:<9} {}",
            r.id,
            if r.enabled { "on" } else { "off" },
            r.routine_type.as_str(),
            r.name
        )));
    }
    panel(frame, area, "Routines", lines);
}

fn render_routine(frame: &mut Frame, app: &App, area: Rect) {
    let Some(r) = app.selected_routine() else {
        panel(
            frame,
            area,
            "Routine",
            vec![Line::raw("No routine selected.")],
        );
        return;
    };
    panel(
        frame,
        area,
        &format!("Routine #{}", r.id),
        vec![
            kv("name", &r.name),
            kv("enabled", if r.enabled { "yes" } else { "no" }),
            kv("origin", r.origin.as_str()),
            kv("type", r.routine_type.as_str()),
            kv("cron", &r.cron),
            kv("last run", &fmt_opt_ms(r.last_run_at)),
            kv("next run", &fmt_opt_ms(r.next_run_at)),
            kv("writable", r.writable_paths.as_deref().unwrap_or("(none)")),
            sep(),
            Line::raw(r.prompt.clone()),
        ],
    );
}

fn render_backlog_summary(frame: &mut Frame, app: &App, area: Rect) {
    let pending = app
        .backlog
        .iter()
        .filter(|b| b.approval == Approval::Pending)
        .count();
    let approved = app
        .backlog
        .iter()
        .filter(|b| b.approval == Approval::Approved)
        .count();
    panel(
        frame,
        area,
        "Backlog",
        vec![
            kv("items", &app.backlog.len().to_string()),
            kv("pending", &pending.to_string()),
            kv("approved", &approved.to_string()),
            Line::raw("n adds a backlog item. T promotes approved items. E promotes and runs the selected item."),
        ],
    );
}

fn render_backlog(frame: &mut Frame, app: &App, area: Rect) {
    let Some(b) = app.selected_backlog() else {
        panel(
            frame,
            area,
            "Backlog",
            vec![Line::raw("No backlog item selected.")],
        );
        return;
    };
    let mut lines = vec![
        kv("id", &b.id.to_string()),
        kv("approval", b.approval.as_str()),
        kv("source", b.source.as_str()),
        kv(
            "consumed",
            &b.consumed_issue_id
                .map(|id| format!("#{id}"))
                .unwrap_or_else(|| "no".into()),
        ),
        kv("created", &b.created_at.to_string()),
        sep(),
        Line::raw(b.text.clone()),
        sep(),
    ];
    let action = if b.consumed_issue_id.is_some() {
        "Already promoted; select linked issue to continue."
    } else if b.approval == Approval::Dismissed {
        "Dismissed backlog item will not run."
    } else if b.approval == Approval::Pending {
        "Waiting for approval. Automatic scheduler will ignore it; a approves or E approves and runs now."
    } else {
        "Suggested next action: E promotes this item and runs the first issue phase."
    };
    lines.push(Line::styled(action, Style::default().fg(Color::Yellow)));
    panel(frame, area, &format!("Backlog #{}", b.id), lines);
}

fn render_issue_summary(frame: &mut Frame, app: &App, area: Rect) {
    let mut lines = vec![];
    for i in &app.issues {
        lines.push(Line::raw(format!(
            "#{:<3} {:<13} {}",
            i.id,
            i.status.as_str(),
            i.title
        )));
    }
    panel(frame, area, "Issues", lines);
}

fn render_issue(frame: &mut Frame, app: &App, area: Rect) {
    let Some(issue) = app.detail.issue.as_ref().or_else(|| app.selected_issue()) else {
        panel(frame, area, "Issue", vec![Line::raw("No issue selected.")]);
        return;
    };
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage(46),
            Constraint::Percentage(28),
            Constraint::Percentage(26),
        ])
        .split(area);
    let mut lines = vec![
        kv("status", issue.status.as_str()),
        kv("title", &issue.title),
        kv("branch", issue.branch.as_deref().unwrap_or("(none)")),
        kv(
            "worktree",
            issue.worktree_path.as_deref().unwrap_or("(none)"),
        ),
        kv("review round", &issue.review_round.to_string()),
        kv(
            "pending steering",
            if issue.has_pending_steering {
                "yes"
            } else {
                "no"
            },
        ),
    ];
    if let Some(desc) = &issue.description {
        lines.push(kv("description", desc));
    }
    panel(frame, rows[0], &format!("Issue #{}", issue.id), lines);

    let mut work = vec![Line::styled(
        "Subtasks",
        Style::default().add_modifier(Modifier::BOLD),
    )];
    for s in &app.detail.subtasks {
        work.push(Line::raw(format!(
            "[{}] {}. {}",
            if s.done { 'x' } else { ' ' },
            s.ord,
            s.text
        )));
    }
    work.push(sep());
    work.push(Line::styled(
        "Pending steering",
        Style::default().add_modifier(Modifier::BOLD),
    ));
    for s in &app.detail.steering {
        work.push(Line::raw(format!("[{}] {}", s.source.as_str(), s.note)));
    }
    panel(frame, rows[1], "Phase Detail", work);

    let title = app
        .log_tail_path
        .as_deref()
        .map(|p| format!("Agent Log {}", p))
        .unwrap_or_else(|| "Agent Log".to_string());
    panel(
        frame,
        rows[2],
        &title,
        vec![Line::raw(app.log_tail.clone())],
    );
}

fn panel(frame: &mut Frame, area: Rect, title: &str, lines: Vec<Line<'static>>) {
    frame.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(ACCENT))
                .title(Span::styled(
                    format!(" {title} "),
                    Style::default().add_modifier(Modifier::BOLD),
                )),
        ),
        area,
    );
}

fn kv(key: &str, val: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{key:>16}: "), Style::default().fg(Color::DarkGray)),
        Span::raw(val.to_string()),
    ])
}

fn sep() -> Line<'static> {
    Line::from(Span::styled("", Style::default().fg(Color::DarkGray)))
}

fn next_due_label(
    schedule_interval_min: Option<i64>,
    last: Option<&auwsx_core::db::scheduler_runs::SchedulerRun>,
) -> String {
    match (schedule_interval_min, last) {
        (Some(min), Some(run)) if min > 0 => (run.fired_at + min * 60_000).to_string(),
        (Some(min), _) if min > 0 => "now".to_string(),
        (Some(_), _) => "global tick".to_string(),
        (None, _) => "manual only".to_string(),
    }
}

fn schedule_label(schedule_interval_min: Option<i64>) -> String {
    match schedule_interval_min {
        None => "manual only".to_string(),
        Some(min) if min <= 0 => "global daemon tick".to_string(),
        Some(min) => format!("{min} min"),
    }
}

fn fmt_opt_ms(v: Option<i64>) -> String {
    v.map(|n| n.to_string())
        .unwrap_or_else(|| "(none)".to_string())
}

fn summarize_picked(raw: Option<&str>) -> String {
    let Some(raw) = raw else {
        return "no decision data".to_string();
    };
    let Ok(picked) = serde_json::from_str::<SchedulerRunPicked>(raw) else {
        return raw.to_string();
    };
    let triaged = picked
        .triaged_issue_ids
        .iter()
        .map(|id| format!("#{id}"))
        .collect::<Vec<_>>();
    let mut parts = Vec::new();
    if !triaged.is_empty() {
        parts.push(format!("triaged {}", triaged.join(", ")));
    }
    let decisions = picked
        .decisions
        .iter()
        .map(|decision| match decision {
            SchedulerRunDecision::Spawn { issue_id } => format!("spawn #{issue_id}"),
            SchedulerRunDecision::SoftGate { issue_id } => format!("soft_gate #{issue_id}"),
            SchedulerRunDecision::Teardown { issue_id } => format!("teardown #{issue_id}"),
        })
        .collect::<Vec<_>>();
    parts.extend(decisions);
    if parts.is_empty() {
        if picked.pending_backlog > 0 {
            format!("{} backlog pending approval", picked.pending_backlog)
        } else if picked.ready_backlog > 0 {
            format!("{} backlog ready", picked.ready_backlog)
        } else if picked.max_concurrency > 0 && picked.running_issues >= picked.max_concurrency {
            format!(
                "capacity full ({}/{})",
                picked.running_issues, picked.max_concurrency
            )
        } else {
            "nothing eligible".to_string()
        }
    } else {
        parts.join(", ")
    }
}

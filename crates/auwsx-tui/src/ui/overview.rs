//! Operator console: one tree on the left, contextual detail on the right.

use super::{render_list, theme, ACCENT};
use crate::app::{App, TreeItem};
use auwsx_core::backlog::Approval;
use auwsx_core::db::scheduler_runs::SchedulerRunSource;
use auwsx_core::state::{IssueStatus, ProgressLane};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, ListItem, Paragraph, Wrap};
use ratatui::Frame;

pub(super) fn render(frame: &mut Frame, app: &App, area: Rect) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(30), Constraint::Percentage(70)])
        .split(area);

    let rows = app.tree_rows();
    let items: Vec<ListItem> = rows
        .iter()
        .enumerate()
        .map(|(i, r)| match r.depth {
            // depth 0 — project header. ▾/▸ reflects expanded state.
            0 => {
                let expanded = rows.get(i + 1).map(|n| n.depth > 0).unwrap_or(false);
                let glyph = if expanded { "▾ " } else { "▸ " };
                ListItem::new(Line::from(vec![
                    Span::raw(glyph),
                    Span::styled(
                        r.label.clone(),
                        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                    ),
                ]))
            }
            // depth 1 — category header (Routines/Backlog/Issues).
            1 => ListItem::new(Line::from(vec![
                Span::raw("  "),
                Span::styled(
                    r.label.clone(),
                    Style::default()
                        .fg(theme::TEXT_DIM)
                        .add_modifier(Modifier::BOLD),
                ),
            ])),
            // depth 2 — leaf. └ for the last item in its category, ├ otherwise.
            _ => {
                let is_last = rows.get(i + 1).map(|n| n.depth < 2).unwrap_or(true);
                let connector = if is_last { "    └ " } else { "    ├ " };
                ListItem::new(Line::from(vec![
                    Span::styled(connector, Style::default().fg(theme::TREE_CONNECTOR)),
                    Span::styled(r.label.clone(), Style::default().fg(theme::TEXT)),
                ]))
            }
        })
        .collect();
    render_list(
        frame,
        cols[0],
        &format!("auwsx [{}]", app.active_profile_name()),
        items,
        app.tree_sel,
        app.focus == crate::app::Focus::Left,
    );

    match app.selected_tree_item() {
        None => render_project(frame, app, cols[1]),
        Some(TreeItem::Project(_)) => render_project(frame, app, cols[1]),
        Some(TreeItem::RoutinesRoot(_)) => render_routines(frame, app, cols[1]),
        Some(TreeItem::Routine { .. }) => render_routine(frame, app, cols[1]),
        Some(TreeItem::BacklogRoot(_)) => render_backlog_summary(frame, app, cols[1]),
        Some(TreeItem::Backlog { .. }) => render_backlog(frame, app, cols[1]),
        Some(TreeItem::IssuesRoot(_)) => render_issue_summary(frame, app, cols[1]),
        Some(TreeItem::ArchiveRoot(_)) => render_archive_summary(frame, app, cols[1]),
        Some(TreeItem::Issue { .. } | TreeItem::ArchivedIssue { .. }) => {
            render_issue(frame, app, cols[1])
        }
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
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(8),
            Constraint::Min(8),
            Constraint::Length(7),
            Constraint::Length(5),
        ])
        .split(area);
    let running = app
        .issues()
        .iter()
        .filter(|i| i.status.is_actionable())
        .count();
    use std::time::{SystemTime, UNIX_EPOCH};
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let last_auto_ms = app
        .recent_scheduler_runs
        .iter()
        .find(|run| run.source == SchedulerRunSource::Auto)
        .map(|run| run.fired_at);
    let lines = vec![
        kv("name", &p.name),
        kv("repo", &p.repo_path),
        kv(
            "branch",
            &format!("{} / {}", p.default_branch, p.completion_policy.as_str()),
        ),
        kv(
            "schedule",
            &format!(
                "{} / next {}",
                crate::ui::schedule::interval_label(
                    p.schedule_cron.as_deref(),
                    p.schedule_interval_min,
                    app.daemon_tick_secs,
                ),
                crate::ui::schedule::next_due_label(
                    p.schedule_cron.as_deref(),
                    p.schedule_interval_min,
                    last_auto_ms,
                    p.created_at,
                    now_ms,
                    app.daemon_tick_secs,
                )
            ),
        ),
        kv(
            "counts",
            &format!(
                "{} routines  {} backlog  {} active  {} archive  {} actionable",
                app.routines().len(),
                app.backlog().len(),
                app.issues().len(),
                app.archived_issues().len(),
                running
            ),
        ),
        kv(
            "last tick",
            &app.recent_scheduler_runs
                .first()
                .map(|run| {
                    format!(
                        "{} {}",
                        crate::ui::schedule::format_epoch_ms_local(run.fired_at),
                        run.source.as_str()
                    )
                })
                .unwrap_or_else(|| "(none)".to_string()),
        ),
    ];
    panel(frame, rows[0], &format!("Project {}", p.name), lines);
    render_kanban(frame, app, rows[1]);
    render_kanban_preview(frame, app, rows[2]);
    render_archive_summary(frame, app, rows[3]);
}

fn render_routines(frame: &mut Frame, app: &App, area: Rect) {
    let mut lines = vec![Line::raw("Configured recurring main-workspace prompts.")];
    for r in app.routines() {
        lines.push(Line::raw(format!(
            "#{:<3} {:<3} {:<9} {}",
            r.id,
            if r.enabled { "on" } else { "off" },
            r.output_route.as_str(),
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
            kv("output", r.output_route.as_str()),
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
        .backlog()
        .iter()
        .filter(|b| b.approval == Approval::Pending)
        .count();
    let approved = app
        .backlog()
        .iter()
        .filter(|b| b.approval == Approval::Approved)
        .count();
    panel(
        frame,
        area,
        "Backlog",
        vec![
            kv("items", &app.backlog().len().to_string()),
            kv("pending", &pending.to_string()),
            kv("approved", &approved.to_string()),
            Line::raw("n adds a backlog item. E runs scheduler now or runs the selected item."),
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
    lines.push(Line::styled(action, Style::default().fg(theme::WARN)));
    panel(frame, area, &format!("Backlog #{}", b.id), lines);
}

fn render_issue_summary(frame: &mut Frame, app: &App, area: Rect) {
    render_kanban(frame, app, area);
}

fn render_archive_summary(frame: &mut Frame, app: &App, area: Rect) {
    let done = app
        .archived_issues()
        .iter()
        .filter(|issue| issue.status == IssueStatus::Done)
        .count();
    let failed = app
        .archived_issues()
        .iter()
        .filter(|issue| issue.status == IssueStatus::Failed)
        .count();
    let abandoned = app
        .archived_issues()
        .iter()
        .filter(|issue| issue.status == IssueStatus::Abandoned)
        .count();
    let mut lines = vec![
        kv("archived", &app.archived_issues().len().to_string()),
        kv(
            "outcomes",
            &format!("{done} done  {failed} failed  {abandoned} abandoned"),
        ),
        Line::styled(
            "low-frequency terminal history; latest items only",
            theme::dim(),
        ),
        sep(),
    ];
    for issue in app.archived_issues().iter().take(3) {
        lines.push(Line::raw(super::vm::issue_tree_label(issue)));
    }
    if app.archived_issues().len() > 3 {
        lines.push(Line::styled(
            format!("... {} more", app.archived_issues().len() - 3),
            theme::dim(),
        ));
    }
    panel(frame, area, "Archive", lines);
}

fn render_issue(frame: &mut Frame, app: &App, area: Rect) {
    let Some(issue) = app.detail.issue.as_ref().or_else(|| app.selected_issue()) else {
        panel(frame, area, "Issue", vec![Line::raw("No issue selected.")]);
        return;
    };
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints(issue_section_constraints(app.issue_section_sel))
        .split(area);
    let mut lines = vec![
        kv("title", &issue.title),
        kv("branch", issue.branch.as_deref().unwrap_or("(none)")),
        kv(
            "worktree",
            issue.worktree_path.as_deref().unwrap_or("(none)"),
        ),
        kv("review round", &issue.review_round.to_string()),
        kv(
            "queue message",
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
    append_issue_summary_lines(&mut lines, app, issue);
    panel_with_focus(
        frame,
        rows[0],
        issue_section_title(app, 0, &format!("Issue #{}", issue.id)),
        app.focus == crate::app::Focus::IssueDetail && app.issue_section_sel == 0,
        lines,
    );

    let findings = if app.detail.findings.is_empty() {
        vec![Line::styled("empty", theme::dim())]
    } else {
        app.detail
            .findings
            .iter()
            .map(|f| {
                Line::from(vec![
                    Span::styled(
                        format!("[{}] ", f.severity.as_str()),
                        Style::default().fg(theme::severity(f.severity.as_str())),
                    ),
                    Span::raw(format!("{} {}", f.status.as_str(), f.title)),
                ])
            })
            .collect()
    };
    panel_with_focus(
        frame,
        rows[1],
        issue_section_title(app, 1, "Findings"),
        app.focus == crate::app::Focus::IssueDetail && app.issue_section_sel == 1,
        findings,
    );

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
        "Queue messages",
        Style::default().add_modifier(Modifier::BOLD),
    ));
    for s in &app.detail.steering {
        work.push(Line::raw(format!("[{}] {}", s.source.as_str(), s.note)));
    }
    panel_with_focus(
        frame,
        rows[2],
        issue_section_title(app, 2, "Subtasks / Queue"),
        app.focus == crate::app::Focus::IssueDetail && app.issue_section_sel == 2,
        work,
    );

    super::issue::log_block_with_title_focused(
        frame,
        rows[3],
        app,
        issue_section_title(app, 3, "Log"),
        app.focus == crate::app::Focus::IssueDetail && app.issue_section_sel == 3,
    );
}

fn issue_section_constraints(active: usize) -> [Constraint; 4] {
    match active {
        0 => [
            Constraint::Length(10),
            Constraint::Length(4),
            Constraint::Length(5),
            Constraint::Min(3),
        ],
        1 => [
            Constraint::Length(5),
            Constraint::Length(10),
            Constraint::Length(5),
            Constraint::Min(3),
        ],
        2 => [
            Constraint::Length(5),
            Constraint::Length(4),
            Constraint::Length(10),
            Constraint::Min(3),
        ],
        _ => [
            Constraint::Length(5),
            Constraint::Length(4),
            Constraint::Length(5),
            Constraint::Min(8),
        ],
    }
}

fn issue_section_title<'a>(app: &App, idx: usize, title: &'a str) -> &'a str {
    if app.focus == crate::app::Focus::IssueDetail && app.issue_section_sel == idx {
        match idx {
            0 => "Issue Detail *",
            1 => "Findings *",
            2 => "Subtasks / Queue *",
            _ => "Log *",
        }
    } else {
        title
    }
}

fn render_kanban(frame: &mut Frame, app: &App, area: Rect) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(25),
            Constraint::Percentage(25),
            Constraint::Percentage(25),
            Constraint::Percentage(25),
        ])
        .split(area);
    let cards = super::vm::kanban_cards(app.backlog(), app.issues());
    for (idx, lane) in super::vm::KanbanLane::ALL.iter().copied().enumerate() {
        let title = if app.focus == crate::app::Focus::ProjectKanban && app.kanban_lane_sel == idx {
            format!("{} *", lane.title())
        } else {
            lane.title().to_string()
        };
        panel_with_focus(
            frame,
            cols[idx],
            &title,
            app.focus == crate::app::Focus::ProjectKanban && app.kanban_lane_sel == idx,
            kanban_lines(app, &cards, lane),
        );
    }
}

fn kanban_lines(
    app: &App,
    cards: &[super::vm::KanbanCard],
    lane: super::vm::KanbanLane,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for card in cards.iter().filter(|card| card.belongs_to(lane)) {
        match card {
            super::vm::KanbanCard::Backlog {
                id,
                approval,
                title,
            } => {
                let selected = app.is_kanban_item_selected(card.item());
                lines.push(Line::from(vec![
                    Span::styled(
                        if selected { "> " } else { "\u{25a1} " },
                        if selected {
                            Style::default()
                                .fg(theme::ACCENT)
                                .add_modifier(Modifier::BOLD)
                        } else {
                            Style::default().fg(theme::TREE_CONNECTOR)
                        },
                    ),
                    Span::styled(format!("B#{} ", id), theme::dim()),
                    Span::styled(
                        format!("[{}] ", approval.as_str()),
                        Style::default().fg(theme::approval(approval.as_str())),
                    ),
                    Span::styled(title.clone(), Style::default().fg(theme::TEXT)),
                ]));
            }
            super::vm::KanbanCard::Issue {
                id,
                status,
                title,
                needs_attention,
            } => {
                let selected = app.is_kanban_item_selected(card.item());
                let chip = super::vm::issue_status_chip(*status);
                let status_style = Style::default().fg(status_color(*status));
                let chip_style = if *needs_attention {
                    Style::default()
                        .fg(theme::WARN)
                        .add_modifier(Modifier::BOLD)
                } else {
                    status_style
                };
                lines.push(Line::from(vec![
                    Span::styled(
                        if selected {
                            format!(">{chip} ")
                        } else {
                            format!(" {chip} ")
                        },
                        if selected {
                            chip_style.add_modifier(Modifier::BOLD)
                        } else {
                            chip_style
                        },
                    ),
                    Span::styled(format!("#{} ", id), theme::dim()),
                    Span::styled(title.clone(), Style::default().fg(theme::TEXT)),
                ]));
            }
        }
    }
    if lines.is_empty() {
        lines.push(Line::styled("empty", theme::dim()));
    }
    lines
}

fn render_kanban_preview(frame: &mut Frame, app: &App, area: Rect) {
    let Some(preview) = app.selected_kanban_item_preview() else {
        panel(
            frame,
            area,
            "Preview",
            vec![Line::styled("empty", theme::dim())],
        );
        return;
    };
    match preview {
        super::vm::KanbanPreview::Backlog(item) => panel(
            frame,
            area,
            &format!("Preview Backlog #{}", item.id),
            vec![
                kv("approval", item.approval.as_str()),
                kv("source", item.source.as_str()),
                kv(
                    "consumed",
                    &item
                        .consumed_issue_id
                        .map(|id| format!("#{id}"))
                        .unwrap_or_else(|| "no".to_string()),
                ),
                sep(),
                Line::raw(item.text.clone()),
            ],
        ),
        super::vm::KanbanPreview::Issue(issue) => {
            panel(frame, area, &format!("Preview Issue #{}", issue.id), {
                let mut lines = vec![
                    kv("title", &issue.title),
                    kv("branch", issue.branch.as_deref().unwrap_or("(none)")),
                    kv(
                        "worktree",
                        issue.worktree_path.as_deref().unwrap_or("(none)"),
                    ),
                ];
                append_issue_summary_lines(&mut lines, app, issue);
                lines
            })
        }
    }
}

fn status_color(status: IssueStatus) -> ratatui::style::Color {
    match status.progress_lane() {
        ProgressLane::Plan => theme::TEXT_DIM,
        ProgressLane::InProgress => theme::ACCENT,
        ProgressLane::Finalizing => theme::WARN,
        ProgressLane::Done => theme::OK,
    }
}

fn panel(frame: &mut Frame, area: Rect, title: &str, lines: Vec<Line<'static>>) {
    panel_with_focus(frame, area, title, false, lines);
}

fn panel_with_focus(
    frame: &mut Frame,
    area: Rect,
    title: &str,
    focused: bool,
    mut lines: Vec<Line<'static>>,
) {
    let visible = area.height.saturating_sub(2) as usize;
    if visible > 0 && lines.len() > visible {
        let hidden = lines.len().saturating_sub(visible);
        lines.truncate(visible.saturating_sub(1));
        lines.push(Line::styled(
            format!("... {hidden} more; focus/enter to inspect"),
            theme::dim(),
        ));
    }
    frame.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(theme::border(focused))
                .title(Span::styled(format!(" {title} "), theme::title())),
        ),
        area,
    );
}

fn kv(key: &str, val: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{key:>16}: "), theme::dim()),
        Span::styled(val.to_string(), Style::default().fg(theme::TEXT)),
    ])
}

fn append_issue_summary_lines(
    lines: &mut Vec<Line<'static>>,
    app: &App,
    issue: &auwsx_core::db::issues::Issue,
) {
    let (subtasks, findings, steering, runs) =
        if app.detail.issue.as_ref().map(|i| i.id) == Some(issue.id) {
            (
                app.detail.subtasks.as_slice(),
                app.detail.findings.as_slice(),
                app.detail.steering.as_slice(),
                app.detail.runs.as_slice(),
            )
        } else {
            (&[][..], &[][..], &[][..], &[][..])
        };
    for row in super::vm::issue_summary_rows(issue, subtasks, findings, steering, runs) {
        lines.push(kv(row.label, &truncate_text(&row.value, 180)));
    }
}

fn truncate_text(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max.saturating_sub(3)).collect();
    out.push_str("...");
    out
}

fn sep() -> Line<'static> {
    Line::from(Span::styled("", theme::dim()))
}

fn fmt_opt_ms(v: Option<i64>) -> String {
    v.map(|n| n.to_string())
        .unwrap_or_else(|| "(none)".to_string())
}

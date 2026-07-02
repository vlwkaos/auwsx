//! Operator console: one tree on the left, contextual detail on the right.

use super::{render_list, theme, ACCENT};
use crate::app::{App, ProjectDetailSection, TreeItem};
use auwsx_core::backlog::Approval;
use auwsx_core::db::agent_runs::AgentRun;
use auwsx_core::db::scheduler_runs::SchedulerRunSource;
use auwsx_core::main_jobs::{MainJob, MainJobStatus};
use auwsx_core::reconcile::{ProjectReconcileReport, ReconcileActionKind};
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
        Some(TreeItem::ArchiveRoot(_)) => render_tree_archive_summary(frame, app, cols[1]),
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
            Constraint::Length(7),
            Constraint::Length(6),
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
        kv("remote", &remote_summary(app, p.id)),
        kv(
            "branch",
            &format!("{} / {}", p.default_branch, p.completion_policy.as_str()),
        ),
        kv(
            "schedule",
            &crate::ui::schedule::schedule_timer_label(
                p.schedule_cron.as_deref(),
                last_auto_ms,
                p.created_at,
                now_ms,
                app.daemon_tick_secs,
            )
            .unwrap_or_else(|| "manual".to_string()),
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
    render_recovery(
        frame,
        app,
        p.id,
        rows[1],
        project_section_selected(app, ProjectDetailSection::Recovery),
    );
    render_remote_sync(
        frame,
        app,
        p.id,
        rows[2],
        project_section_selected(app, ProjectDetailSection::Remote),
    );
    render_kanban(
        frame,
        app,
        rows[3],
        project_section_selected(app, ProjectDetailSection::Kanban),
    );
    render_kanban_preview(
        frame,
        app,
        rows[4],
        project_section_selected(app, ProjectDetailSection::Preview),
    );
    render_project_archive(
        frame,
        app,
        rows[5],
        project_section_selected(app, ProjectDetailSection::Archive),
    );
}

fn project_section_selected(app: &App, section: ProjectDetailSection) -> bool {
    app.focus == crate::app::Focus::ProjectDetail && app.selected_project_section() == section
}

fn remote_summary(app: &App, project_id: i64) -> String {
    let Some(config) = app.remote_config(project_id) else {
        return "not configured".to_string();
    };
    let mut enabled = Vec::new();
    if config.inbound_auwsx_run_enabled {
        enabled.push("/auwsx-run");
    }
    if config.outbound_issue_create_enabled {
        enabled.push("issues");
    }
    if config.remote_pr_merge_enabled {
        enabled.push("PR merge");
    }
    if config.agent_comment_sync_enabled
        || config.subtask_comment_sync_enabled
        || config.finding_comment_sync_enabled
    {
        enabled.push("comments");
    }
    let toggles = if enabled.is_empty() {
        "no sync toggles".to_string()
    } else {
        enabled.join(", ")
    };
    format!(
        "{} {}/{} · {}",
        config.provider.as_str(),
        config.owner,
        config.repo,
        toggles
    )
}

fn render_recovery(frame: &mut Frame, app: &App, project_id: i64, area: Rect, focused: bool) {
    let Some(report) = app.reconcile_reports.get(&project_id) else {
        panel_with_focus(
            frame,
            area,
            "Recovery",
            focused,
            vec![Line::styled("diagnostics unavailable", theme::dim())],
        );
        return;
    };
    panel_with_focus(
        frame,
        area,
        "Recovery",
        focused,
        recovery_lines(report, &app.recent_main_jobs, &app.recent_agent_runs),
    );
}

fn recovery_lines(
    report: &ProjectReconcileReport,
    recent_main_jobs: &[MainJob],
    recent_agent_runs: &[AgentRun],
) -> Vec<Line<'static>> {
    let counts = report.diagnosis_counts();
    let mut lines = vec![kv(
        "counts",
        &format!(
            "safe {}  represented {}  conflict {}  stale {}  unknown {}",
            counts.safe, counts.represented, counts.conflict, counts.stale, counts.unknown
        ),
    )];
    if let Some(job) = active_reconcile_job(report.project_id, recent_main_jobs) {
        lines.push(kv(
            "agent",
            &format!(
                "reconcile #{} {}",
                job.id,
                job.status.as_str().to_lowercase()
            ),
        ));
    }
    let running_issues = running_issue_agents(recent_agent_runs);
    if running_issues > 0 {
        lines.push(kv("issues", &format!("{running_issues} running")));
    }
    for item in report
        .issues
        .iter()
        .filter(|item| item.proposed_action != ReconcileActionKind::None)
        .take(2)
    {
        lines.push(kv(
            &format!("#{}", item.issue_id),
            &format!(
                "{} -> {}",
                item.diagnosis.as_str(),
                item.proposed_action.as_str()
            ),
        ));
    }
    for orphan in report.orphans.iter().take(1) {
        lines.push(kv(
            &format!("orphan #{}", orphan.issue_id),
            orphan.proposed_action.as_str(),
        ));
    }
    if lines.len() == 1 {
        lines.push(Line::styled("no recovery actions pending", theme::dim()));
    }
    lines
}

fn active_reconcile_job(project_id: i64, jobs: &[MainJob]) -> Option<&MainJob> {
    jobs.iter().find(|job| {
        job.project_id == project_id
            && job.kind == "reconcile"
            && matches!(job.status, MainJobStatus::Queued | MainJobStatus::Running)
    })
}

fn running_issue_agents(runs: &[AgentRun]) -> usize {
    runs.iter()
        .filter(|run| run.issue_id.is_some() && run.exited_at.is_none())
        .count()
}

fn render_remote_sync(frame: &mut Frame, app: &App, project_id: i64, area: Rect, focused: bool) {
    panel_with_focus(
        frame,
        area,
        "Remote Sync",
        focused,
        remote_sync_lines(app, project_id),
    );
}

fn remote_sync_lines(app: &App, project_id: i64) -> Vec<Line<'static>> {
    if app.remote_config(project_id).is_none() {
        return vec![Line::styled("remote not configured", theme::dim())];
    }
    let summary = crate::ui::vm::remote_sync_summary(app.remote_sync_runs(project_id), 3);
    let mut lines = vec![kv(
        "state",
        &format!(
            "{} active  {} failed recent",
            summary.active, summary.failures
        ),
    )];
    if summary.rows.is_empty() {
        lines.push(Line::styled("no remote sync runs yet", theme::dim()));
        return lines;
    }
    for row in summary.rows {
        lines.push(kv(row.label, &row.value));
    }
    lines
}

#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod tests {
    use super::*;
    use auwsx_core::agent::ExitKind;
    use auwsx_core::db::agent_runs::Role;
    use auwsx_core::db::remote::{
        ProjectRemoteConfig, RemoteAuthKind, RemoteProvider, RemoteSyncDirection, RemoteSyncKind,
        RemoteSyncRun, RemoteSyncStatus, RequiredChecksPolicy,
    };
    use auwsx_core::main_jobs::MainJobSource;
    use auwsx_core::reconcile::ReconcileDiagnosis;
    use auwsx_core::reconcile::{ReconcileIssueReport, ReconcileOrphanReport};

    fn line_text(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>()
    }

    fn issue(
        issue_id: i64,
        diagnosis: ReconcileDiagnosis,
        action: ReconcileActionKind,
    ) -> ReconcileIssueReport {
        ReconcileIssueReport {
            issue_id,
            status: IssueStatus::ReadyToMerge,
            branch: Some(format!("auwsx/issue-{issue_id}")),
            worktree_path: None,
            diagnosis,
            confidence: 90,
            proposed_action: action,
            blocking_reason: None,
            manual_command: None,
        }
    }

    fn preview_issue(id: i64, description: Option<&str>) -> auwsx_core::db::issues::Issue {
        auwsx_core::db::issues::Issue {
            id,
            project_id: 1,
            title: format!("issue {id}"),
            description: description.map(str::to_string),
            agent_summary: None,
            progress_report: None,
            result_report: None,
            status: IssueStatus::Planning,
            branch: None,
            worktree_path: None,
            review_round: 0,
            conflict_attempts: 0,
            wait_until: None,
            has_pending_steering: false,
            created_at: 0,
            updated_at: 0,
        }
    }

    fn main_job(id: i64, project_id: i64, kind: &str, status: MainJobStatus) -> MainJob {
        MainJob {
            id,
            project_id,
            routine_id: None,
            source: MainJobSource::UserOneoff,
            kind: kind.to_string(),
            prompt: String::new(),
            status,
            worktree_path: None,
            report_path: None,
            scope_violation: None,
            queued_at: 0,
            started_at: None,
            ended_at: None,
            log_path: None,
            outcome: None,
        }
    }

    fn agent_run(id: i64, issue_id: Option<i64>, exited_at: Option<i64>) -> AgentRun {
        AgentRun {
            id,
            issue_id,
            main_job_id: None,
            role: Role::Work,
            phase: "WORKING".to_string(),
            agent_cmd: "agent".to_string(),
            status_before: None,
            status_after: None,
            pid: None,
            exit_code: None,
            exit_kind: exited_at.map(|_| ExitKind::Exited),
            prompt_path: None,
            log_path: None,
            phase_report: None,
            spawned_at: 0,
            exited_at,
            note: None,
        }
    }

    fn remote_config(project_id: i64) -> ProjectRemoteConfig {
        ProjectRemoteConfig {
            project_id,
            provider: RemoteProvider::Github,
            remote_url: "https://github.com/acme/repo".to_string(),
            owner: "acme".to_string(),
            repo: "repo".to_string(),
            api_base_url: "https://api.github.com".to_string(),
            auth_kind: RemoteAuthKind::None,
            auth_ref: None,
            webhook_secret_ref: None,
            inbound_auwsx_run_enabled: true,
            outbound_issue_create_enabled: true,
            remote_pr_merge_enabled: true,
            agent_comment_sync_enabled: true,
            subtask_comment_sync_enabled: true,
            finding_comment_sync_enabled: true,
            draft_pr_enabled: false,
            required_checks_policy: RequiredChecksPolicy::Observe,
            default_labels: None,
            default_assignees: None,
            pr_base_branch: Some("main".to_string()),
            created_at: 1,
            updated_at: 1,
        }
    }

    fn remote_run(id: i64, status: RemoteSyncStatus) -> RemoteSyncRun {
        RemoteSyncRun {
            id,
            project_id: 1,
            issue_id: Some(9),
            backlog_item_id: None,
            remote_issue_link_id: None,
            remote_pr_link_id: None,
            direction: RemoteSyncDirection::Outbound,
            kind: RemoteSyncKind::Pr,
            status,
            summary: Some("opened pull request".to_string()),
            error: None,
            started_at: Some(1),
            ended_at: Some(2),
            created_at: 1,
        }
    }

    #[test]
    fn given_project_archive_panel_when_lines_built_then_tree_toggle_hint_is_absent() {
        let app = App::new(std::path::PathBuf::from(
            "target/nonexistent-auwsx-test.sock",
        ));

        let text = archive_summary_lines(&app, 3, ArchivePanelKind::ProjectDetail)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>();

        assert!(text
            .iter()
            .any(|line| line.contains("Project archive summary")));
        assert!(!text.iter().any(|line| line.contains("h/l")));
    }

    #[test]
    fn given_tree_archive_row_when_lines_built_then_toggle_hint_is_present() {
        let app = App::new(std::path::PathBuf::from(
            "target/nonexistent-auwsx-test.sock",
        ));

        let text = archive_summary_lines(&app, 3, ArchivePanelKind::TreeRow)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>();

        assert!(text
            .iter()
            .any(|line| line.contains("h/l expands or collapses")));
    }

    #[test]
    fn given_recovery_report_when_lines_built_then_counts_actions_and_truncates() {
        let mut report = ProjectReconcileReport::empty(1, true);
        report.issues = vec![
            issue(
                1,
                ReconcileDiagnosis::SafeToMerge,
                ReconcileActionKind::ApplyMerge,
            ),
            issue(
                2,
                ReconcileDiagnosis::RepresentedInMain,
                ReconcileActionKind::MarkDone,
            ),
            issue(
                3,
                ReconcileDiagnosis::MergeConflict,
                ReconcileActionKind::QueueAgenticReconcile,
            ),
            issue(
                4,
                ReconcileDiagnosis::StaleNoDiffBranch,
                ReconcileActionKind::MarkDone,
            ),
            issue(
                5,
                ReconcileDiagnosis::Unknown,
                ReconcileActionKind::ManualRequired,
            ),
        ];
        report.orphans = vec![
            ReconcileOrphanReport {
                issue_id: 10,
                branch: "auwsx/issue-10".to_string(),
                path: "/repo-issue-10".into(),
                diagnosis: ReconcileDiagnosis::OrphanWorktree,
                proposed_action: ReconcileActionKind::PruneOrphanWorktree,
            },
            ReconcileOrphanReport {
                issue_id: 11,
                branch: "auwsx/issue-11".to_string(),
                path: "/repo-issue-11".into(),
                diagnosis: ReconcileDiagnosis::OrphanWorktree,
                proposed_action: ReconcileActionKind::PruneOrphanWorktree,
            },
        ];

        let text = recovery_lines(&report, &[], &[])
            .iter()
            .map(line_text)
            .collect::<Vec<_>>();

        assert!(text[0].contains("safe 1"));
        assert!(text[0].contains("represented 1"));
        assert!(text[0].contains("conflict 1"));
        assert!(text[0].contains("stale 1"));
        assert!(text[0].contains("unknown 1"));
        assert!(text.iter().any(|line| line.contains("#1")));
        assert!(text.iter().any(|line| line.contains("#2")));
        assert!(!text.iter().any(|line| line.contains("#3")));
        assert!(text.iter().any(|line| line.contains("orphan #10")));
        assert!(!text.iter().any(|line| line.contains("orphan #11")));
    }

    #[test]
    fn given_recovery_report_without_actions_when_lines_built_then_empty_message() {
        let mut report = ProjectReconcileReport::empty(1, true);
        report.issues = vec![issue(
            1,
            ReconcileDiagnosis::SafeToMerge,
            ReconcileActionKind::None,
        )];

        let text = recovery_lines(&report, &[], &[])
            .iter()
            .map(line_text)
            .collect::<Vec<_>>();

        assert!(text
            .iter()
            .any(|line| line == "no recovery actions pending"));
    }

    #[test]
    fn given_active_reconcile_job_when_lines_built_then_agent_indicator_is_visible() {
        let report = ProjectReconcileReport::empty(1, true);
        let jobs = vec![
            main_job(12, 1, "dream", MainJobStatus::Running),
            main_job(11, 2, "reconcile", MainJobStatus::Running),
            main_job(10, 1, "reconcile", MainJobStatus::Running),
        ];

        let text = recovery_lines(&report, &jobs, &[])
            .iter()
            .map(line_text)
            .collect::<Vec<_>>();

        assert!(text
            .iter()
            .any(|line| line.contains("agent") && line.contains("reconcile #10 running")));
    }

    #[test]
    fn given_terminal_reconcile_job_when_lines_built_then_agent_indicator_is_hidden() {
        let report = ProjectReconcileReport::empty(1, true);
        let jobs = vec![main_job(10, 1, "reconcile", MainJobStatus::Done)];

        let text = recovery_lines(&report, &jobs, &[])
            .iter()
            .map(line_text)
            .collect::<Vec<_>>();

        assert!(!text.iter().any(|line| line.contains("agent")));
    }

    #[test]
    fn given_running_issue_agents_when_lines_built_then_issue_indicator_is_visible() {
        let report = ProjectReconcileReport::empty(1, true);
        let runs = vec![
            agent_run(1, Some(10), None),
            agent_run(2, Some(11), Some(2)),
            agent_run(3, None, None),
        ];

        let text = recovery_lines(&report, &[], &runs)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>();

        assert!(text
            .iter()
            .any(|line| line.contains("issues") && line.contains("1 running")));
    }

    #[test]
    fn given_remote_sync_runs_when_lines_built_then_status_and_failures_are_visible() {
        let mut app = App::new("socket".into());
        app.remote_configs.insert(1, remote_config(1));
        let mut failed = remote_run(7, RemoteSyncStatus::Failed);
        failed.error = Some("required check failed".to_string());
        app.remote_sync_runs
            .insert(1, vec![remote_run(8, RemoteSyncStatus::Running), failed]);

        let text = remote_sync_lines(&app, 1)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>();

        assert!(text
            .iter()
            .any(|line| line.contains("1 active") && line.contains("1 failed")));
        assert!(text
            .iter()
            .any(|line| line.contains("#8 outbound pr running issue #9")));
        assert!(text
            .iter()
            .any(|line| line.contains("required check failed")));
    }

    #[test]
    fn given_scrolled_text_when_lines_built_then_window_starts_at_offset() {
        let mut app = App::new("socket".into());
        app.selected_text_scroll_key = Some("backlog:9".to_string());
        app.selected_text_scroll_offset = 1;

        let text = scrolling_text_lines("text", "backlog:9", "one\ntwo\nthree", &app, 2)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>();

        assert!(text[0].contains("line 2/3"));
        assert_eq!(text[1], "  two");
        assert_eq!(text[2], "  three");
    }

    #[test]
    fn given_kanban_issue_preview_when_description_scrolls_then_window_starts_at_offset() {
        let mut app = App::new("socket".into());
        app.selected_text_scroll_key = Some("issue:7:description".to_string());
        app.selected_text_scroll_offset = 1;
        let issue = preview_issue(7, Some("alpha\nbeta\ngamma"));

        let text = kanban_issue_preview_lines(&app, &issue, 14)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>();

        assert!(text.iter().any(|line| line.contains("line 2/3")));
        assert!(text.iter().any(|line| line == "  beta"));
        assert!(text.iter().any(|line| line == "  gamma"));
    }

    #[test]
    fn given_issue_log_height_when_capped_then_never_exceeds_sixty_percent() {
        for height in [20, 40, 80] {
            let cap = issue_log_height_cap(height);

            assert!(cap <= ((height as usize * 60).div_ceil(100) as u16));
            assert!(cap <= height.saturating_sub(20).max(3));
        }
    }
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
            Line::raw("a adds a backlog item. E runs scheduler now or runs the selected item."),
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
    let text_lines = scrolling_text_lines(
        "text",
        &format!("backlog:{}", b.id),
        &b.text,
        app,
        area.height.saturating_sub(10).max(3) as usize,
    );
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
    ];
    lines.extend(text_lines);
    lines.extend([sep()]);
    let action = if b.consumed_issue_id.is_some() {
        "Already promoted; select linked issue to continue."
    } else if b.approval == Approval::Dismissed {
        "Dismissed backlog item will not run."
    } else if b.approval == Approval::Pending {
        "Waiting for approval. Automatic scheduler will ignore it; A approves or E approves and runs now."
    } else {
        "Suggested next action: E promotes this item and runs the first issue phase."
    };
    lines.push(Line::styled(action, Style::default().fg(theme::WARN)));
    panel(frame, area, &format!("Backlog #{}", b.id), lines);
}

fn render_issue_summary(frame: &mut Frame, app: &App, area: Rect) {
    render_kanban(frame, app, area, false);
}

#[derive(Clone, Copy)]
enum ArchivePanelKind {
    ProjectDetail,
    TreeRow,
}

fn archive_summary_lines(
    app: &App,
    visible_issue_rows: usize,
    kind: ArchivePanelKind,
) -> Vec<Line<'static>> {
    let limit = app.archive_visible_limit().min(visible_issue_rows.max(1));
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
        archive_panel_help(kind),
        sep(),
    ];
    for issue in app.archived_issues().iter().take(limit) {
        lines.push(Line::raw(super::vm::issue_tree_label(issue)));
    }
    if app.archived_issues().len() > limit {
        lines.push(Line::styled(
            format!(
                "showing latest {limit} of {} archived",
                app.archived_issues().len()
            ),
            theme::dim(),
        ));
    }
    lines
}

fn archive_panel_help(kind: ArchivePanelKind) -> Line<'static> {
    match kind {
        ArchivePanelKind::ProjectDetail => Line::styled(
            "Project archive summary; terminal issues stay out of active kanban.",
            theme::dim(),
        ),
        ArchivePanelKind::TreeRow => Line::styled(
            "h/l expands or collapses archived issue rows in the project list.",
            theme::dim(),
        ),
    }
}

fn render_project_archive(frame: &mut Frame, app: &App, area: Rect, focused: bool) {
    let visible_issue_rows = area.height.saturating_sub(6) as usize;
    panel_with_focus(
        frame,
        area,
        "Archived Issues",
        focused,
        archive_summary_lines(app, visible_issue_rows, ArchivePanelKind::ProjectDetail),
    );
}

fn render_tree_archive_summary(frame: &mut Frame, app: &App, area: Rect) {
    let visible_issue_rows = area.height.saturating_sub(6) as usize;
    panel(
        frame,
        area,
        "Project List Archive",
        archive_summary_lines(app, visible_issue_rows, ArchivePanelKind::TreeRow),
    );
}

fn render_issue(frame: &mut Frame, app: &App, area: Rect) {
    let Some(issue) = app.detail.issue.as_ref().or_else(|| app.selected_issue()) else {
        panel(frame, area, "Issue", vec![Line::raw("No issue selected.")]);
        return;
    };
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints(issue_section_constraints(
            app.selected_issue_section(),
            area.height,
        ))
        .split(area);
    let mut lines = vec![
        kv("summary", &quick_issue_summary(issue)),
        kv("title", &issue.title),
    ];
    if let Some(desc) = &issue.description {
        lines.extend(scrolling_text_lines(
            "description",
            &format!("issue:{}:description", issue.id),
            desc,
            app,
            3,
        ));
    }
    append_issue_summary_lines(&mut lines, app, issue);
    lines.extend([
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
    ]);
    panel_with_focus_scrolled(
        frame,
        rows[0],
        issue_section_title(
            app,
            crate::app::IssueDetailSection::Summary,
            &format!("Summary #{}", issue.id),
        ),
        issue_section_selected(app, crate::app::IssueDetailSection::Summary),
        issue_section_scroll(app, crate::app::IssueDetailSection::Summary),
        "enter section to scroll",
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
    panel_with_focus_scrolled(
        frame,
        rows[1],
        issue_section_title(app, crate::app::IssueDetailSection::Findings, "Findings"),
        issue_section_selected(app, crate::app::IssueDetailSection::Findings),
        issue_section_scroll(app, crate::app::IssueDetailSection::Findings),
        "enter section to scroll",
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
    panel_with_focus_scrolled(
        frame,
        rows[2],
        issue_section_title(
            app,
            crate::app::IssueDetailSection::WorkQueue,
            "Subtasks / Queue",
        ),
        issue_section_selected(app, crate::app::IssueDetailSection::WorkQueue),
        issue_section_scroll(app, crate::app::IssueDetailSection::WorkQueue),
        "enter section to scroll",
        work,
    );

    let verify = issue_verify_lines(issue);
    panel_with_focus_scrolled(
        frame,
        rows[3],
        issue_section_title(app, crate::app::IssueDetailSection::Verify, "Verify"),
        issue_section_selected(app, crate::app::IssueDetailSection::Verify),
        issue_section_scroll(app, crate::app::IssueDetailSection::Verify),
        "enter section to scroll",
        verify,
    );

    super::issue::log_block_with_title_focused(
        frame,
        rows[4],
        app,
        issue_section_title(app, crate::app::IssueDetailSection::Log, "Log"),
        issue_section_selected(app, crate::app::IssueDetailSection::Log),
    );
}

fn issue_section_constraints(
    active: crate::app::IssueDetailSection,
    height: u16,
) -> [Constraint; 5] {
    let log_cap = issue_log_height_cap(height);
    match active {
        crate::app::IssueDetailSection::Summary => [
            Constraint::Length(10),
            Constraint::Length(4),
            Constraint::Length(5),
            Constraint::Length(6),
            Constraint::Length(8.min(log_cap)),
        ],
        crate::app::IssueDetailSection::Findings => [
            Constraint::Length(5),
            Constraint::Length(10),
            Constraint::Length(5),
            Constraint::Length(6),
            Constraint::Length(8.min(log_cap)),
        ],
        crate::app::IssueDetailSection::WorkQueue => [
            Constraint::Length(5),
            Constraint::Length(4),
            Constraint::Length(10),
            Constraint::Length(6),
            Constraint::Length(8.min(log_cap)),
        ],
        crate::app::IssueDetailSection::Verify => [
            Constraint::Length(5),
            Constraint::Length(4),
            Constraint::Length(5),
            Constraint::Length(12),
            Constraint::Length(8.min(log_cap)),
        ],
        crate::app::IssueDetailSection::Log => [
            Constraint::Length(5),
            Constraint::Length(4),
            Constraint::Length(5),
            Constraint::Length(6),
            Constraint::Length(log_cap),
        ],
    }
}

fn issue_log_height_cap(height: u16) -> u16 {
    let viewport_cap = ((height as usize * 60).div_ceil(100) as u16).max(3);
    viewport_cap.min(height.saturating_sub(20).max(3))
}

fn issue_section_selected(app: &App, section: crate::app::IssueDetailSection) -> bool {
    app.focus == crate::app::Focus::IssueDetail && app.selected_issue_section() == section
}

fn issue_section_scroll(app: &App, section: crate::app::IssueDetailSection) -> usize {
    if issue_section_selected(app, section)
        && app.issue_section_is_active()
        && section != crate::app::IssueDetailSection::Log
    {
        app.issue_section_scroll
    } else {
        0
    }
}

fn issue_section_title(app: &App, section: crate::app::IssueDetailSection, title: &str) -> String {
    if issue_section_selected(app, section) {
        match app.issue_section_is_active() {
            true => format!("{} active", section.title()),
            false => format!("{} *", section.title()),
        }
    } else {
        title.to_string()
    }
}

fn render_kanban(frame: &mut Frame, app: &App, area: Rect, focused: bool) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(25),
            Constraint::Percentage(25),
            Constraint::Percentage(25),
            Constraint::Percentage(25),
        ])
        .split(area);
    let cards = app.kanban_cards();
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
            focused || app.focus == crate::app::Focus::ProjectKanban && app.kanban_lane_sel == idx,
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
                activity,
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
                let mut row = vec![
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
                ];
                if let Some(activity) = activity {
                    row.extend([
                        Span::styled("  ", theme::dim()),
                        Span::styled(format!("({activity})"), theme::hint()),
                    ]);
                }
                lines.push(Line::from(row));
            }
        }
    }
    if lines.is_empty() {
        lines.push(Line::styled("empty", theme::dim()));
    }
    lines
}

fn render_kanban_preview(frame: &mut Frame, app: &App, area: Rect, focused: bool) {
    let Some(preview) = app.selected_kanban_item_preview() else {
        panel_with_focus(
            frame,
            area,
            "Preview",
            focused,
            vec![Line::styled("empty", theme::dim())],
        );
        return;
    };
    match preview {
        super::vm::KanbanPreview::Backlog(item) => {
            let mut lines = vec![
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
            ];
            lines.extend(scrolling_text_lines(
                "text",
                &format!("backlog:{}", item.id),
                &item.text,
                app,
                area.height.saturating_sub(8).max(3) as usize,
            ));
            panel_with_focus(
                frame,
                area,
                &format!("Preview Backlog #{}", item.id),
                focused,
                lines,
            )
        }
        super::vm::KanbanPreview::Issue(issue) => panel_with_focus(
            frame,
            area,
            &format!("Preview Issue #{}", issue.id),
            focused,
            kanban_issue_preview_lines(app, issue, area.height as usize),
        ),
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
    title: impl AsRef<str>,
    focused: bool,
    lines: Vec<Line<'static>>,
) {
    panel_with_focus_scrolled(
        frame,
        area,
        title,
        focused,
        0,
        "select section to inspect",
        lines,
    );
}

fn panel_with_focus_scrolled(
    frame: &mut Frame,
    area: Rect,
    title: impl AsRef<str>,
    focused: bool,
    scroll: usize,
    overflow_hint: &'static str,
    mut lines: Vec<Line<'static>>,
) {
    let title = title.as_ref();
    let visible = area.height.saturating_sub(2) as usize;
    if visible > 0 && lines.len() > visible {
        let total = lines.len();
        let start = scroll.min(total.saturating_sub(visible));
        lines = lines.into_iter().skip(start).take(visible).collect();
        if start > 0 && !lines.is_empty() {
            lines[0] = Line::styled(format!("... {start} above"), theme::dim());
        }
        let bottom = start.saturating_add(visible);
        if bottom < total && !lines.is_empty() {
            let hidden = total.saturating_sub(bottom).saturating_add(1);
            let last = lines.len().saturating_sub(1);
            lines[last] = Line::styled(format!("... {hidden} more; {overflow_hint}"), theme::dim());
        }
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

fn scrolling_text_lines(
    label: &str,
    key: &str,
    text: &str,
    app: &App,
    max_lines: usize,
) -> Vec<Line<'static>> {
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() <= 1 {
        return vec![kv(label, text)];
    }
    let max_lines = max_lines.max(1).min(lines.len());
    let offset = app.selected_text_scroll_offset(key, lines.len());
    let mut out = vec![Line::from(vec![
        Span::styled(format!("{label:>16}: "), theme::dim()),
        Span::styled(
            format!("line {}/{}", offset + 1, lines.len()),
            Style::default().fg(theme::TEXT_DIM),
        ),
    ])];
    for idx in 0..max_lines {
        out.push(Line::raw(format!(
            "  {}",
            lines[(offset + idx) % lines.len()]
        )));
    }
    out
}

fn kanban_issue_preview_lines(
    app: &App,
    issue: &auwsx_core::db::issues::Issue,
    area_height: usize,
) -> Vec<Line<'static>> {
    let mut lines = vec![
        kv("title", &issue.title),
        kv("branch", issue.branch.as_deref().unwrap_or("(none)")),
        kv(
            "worktree",
            issue.worktree_path.as_deref().unwrap_or("(none)"),
        ),
    ];
    if let Some(description) = issue.description.as_deref() {
        lines.extend(scrolling_text_lines(
            "description",
            &format!("issue:{}:description", issue.id),
            description,
            app,
            area_height.saturating_sub(10).max(3),
        ));
    }
    append_issue_summary_lines(&mut lines, app, issue);
    lines
}

fn quick_issue_summary(issue: &auwsx_core::db::issues::Issue) -> String {
    issue
        .agent_summary
        .as_deref()
        .or(issue.progress_report.as_deref())
        .or(issue.description.as_deref())
        .map(first_nonempty_line)
        .filter(|line| !line.is_empty())
        .unwrap_or(&issue.title)
        .to_string()
}

fn first_nonempty_line(text: &str) -> &str {
    text.lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("")
}

fn issue_verify_lines(issue: &auwsx_core::db::issues::Issue) -> Vec<Line<'static>> {
    let mut lines = vec![
        kv(
            "worktree",
            issue.worktree_path.as_deref().unwrap_or("(none yet)"),
        ),
        kv(
            "shell",
            &auwsx_core::worktree::issue_shell_attach_command(issue.id),
        ),
    ];
    if let Some(path) = issue.worktree_path.as_deref() {
        lines.push(kv("cwd", &format!("cd {path}")));
    }
    lines.push(kv(
        "verify file",
        ".auwsx/human-verify.md in the issue worktree",
    ));
    lines.push(sep());
    match issue
        .result_report
        .as_deref()
        .filter(|text| !text.trim().is_empty())
    {
        Some(report) => {
            lines.push(Line::styled(
                "Human verify",
                Style::default().add_modifier(Modifier::BOLD),
            ));
            lines.extend(report.lines().map(|line| Line::raw(line.to_string())));
        }
        None => {
            lines.push(Line::styled(
                "No human verification report has been written yet.",
                theme::dim(),
            ));
        }
    }
    lines
}

fn append_issue_summary_lines(
    lines: &mut Vec<Line<'static>>,
    app: &App,
    issue: &auwsx_core::db::issues::Issue,
) {
    let (subtasks, findings, steering, runs, remote_pr_link) =
        if app.detail.issue.as_ref().map(|i| i.id) == Some(issue.id) {
            (
                app.detail.subtasks.as_slice(),
                app.detail.findings.as_slice(),
                app.detail.steering.as_slice(),
                app.detail.runs.as_slice(),
                app.detail
                    .remote_links
                    .as_ref()
                    .and_then(|links| links.pr_link.as_ref()),
            )
        } else {
            (&[][..], &[][..], &[][..], &[][..], None)
        };
    for row in
        super::vm::issue_summary_rows(issue, subtasks, findings, steering, runs, remote_pr_link)
    {
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

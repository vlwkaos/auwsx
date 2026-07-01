//! View-model projections for TUI renderers.
//!
//! Render modules should consume these structs instead of re-deriving domain
//! concepts such as progress lanes, attention, or archive membership.

use auwsx_core::backlog::{Approval, BacklogItem};
use auwsx_core::db::agent_runs::AgentRun;
use auwsx_core::db::findings::Finding;
use auwsx_core::db::issues::Issue;
use auwsx_core::db::remote::{RemotePrLink, RemoteSyncRun, RemoteSyncStatus};
use auwsx_core::db::subtasks::Subtask;
use auwsx_core::state::{IssueStatus, ProgressLane};
use auwsx_core::steering::Steering;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum KanbanLane {
    Plan,
    InProgress,
    Finalizing,
    Done,
}

impl KanbanLane {
    pub(crate) const ALL: [Self; 4] = [Self::Plan, Self::InProgress, Self::Finalizing, Self::Done];

    pub(crate) fn title(self) -> &'static str {
        match self {
            Self::Plan => "PLAN",
            Self::InProgress => "IN PROGRESS",
            Self::Finalizing => "FINALIZING",
            Self::Done => "DONE",
        }
    }

    fn from_progress(lane: ProgressLane) -> Self {
        match lane {
            ProgressLane::Plan => Self::Plan,
            ProgressLane::InProgress => Self::InProgress,
            ProgressLane::Finalizing => Self::Finalizing,
            ProgressLane::Done => Self::Done,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum KanbanCard {
    Backlog {
        id: i64,
        approval: Approval,
        title: String,
    },
    Issue {
        id: i64,
        status: IssueStatus,
        title: String,
        needs_attention: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct IssueSortKey {
    attention: u8,
    lane: u8,
    status: u8,
    id: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IssueStatusPresentation {
    pub(crate) marker: &'static str,
    pub(crate) code: String,
    pub(crate) label: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SummaryRow {
    pub(crate) label: &'static str,
    pub(crate) value: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RemoteSyncSummary {
    pub(crate) active: usize,
    pub(crate) failures: usize,
    pub(crate) rows: Vec<SummaryRow>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum KanbanItem {
    Backlog(i64),
    Issue(i64),
}

pub(crate) enum KanbanPreview<'a> {
    Backlog(&'a BacklogItem),
    Issue(&'a Issue),
}

impl KanbanCard {
    pub(crate) fn belongs_to(&self, lane: KanbanLane) -> bool {
        match self {
            Self::Backlog { .. } => lane == KanbanLane::Plan,
            Self::Issue { status, .. } => KanbanLane::from_progress(status.progress_lane()) == lane,
        }
    }

    pub(crate) fn item(&self) -> KanbanItem {
        match self {
            Self::Backlog { id, .. } => KanbanItem::Backlog(*id),
            Self::Issue { id, .. } => KanbanItem::Issue(*id),
        }
    }
}

pub(crate) fn kanban_cards(backlog: &[BacklogItem], issues: &[Issue]) -> Vec<KanbanCard> {
    let mut cards = Vec::new();
    for item in backlog {
        if item.approval == Approval::Dismissed {
            continue;
        }
        cards.push(KanbanCard::Backlog {
            id: item.id,
            approval: item.approval,
            title: first_line(&item.text),
        });
    }
    let mut archived = 0usize;
    for issue in issues {
        if issue.status.is_archive_status() {
            if archived >= 10 {
                continue;
            }
            archived += 1;
        }
        cards.push(KanbanCard::Issue {
            id: issue.id,
            status: issue.status,
            title: first_line(&issue.title),
            needs_attention: issue.status.needs_attention(),
        });
    }
    cards.sort_by_key(card_sort_key);
    cards
}

pub(crate) fn sort_issues_for_status_view(issues: &mut [Issue]) {
    issues.sort_by_key(issue_sort_key);
}

pub(crate) fn issue_indicator(status: IssueStatus) -> &'static str {
    if status.needs_attention() {
        "!"
    } else if status.is_archive_status() {
        "●"
    } else if status.is_actionable() {
        "◉"
    } else {
        "○"
    }
}

pub(crate) fn issue_status_presentation(status: IssueStatus) -> IssueStatusPresentation {
    let marker = issue_indicator(status);
    // ^ Keep issue chips derived from IssueStatus semantics; do not add a per-status label table.
    let code = format!("{:<4}", status_code(status));
    let label = format!("{marker} {code}");
    IssueStatusPresentation {
        marker,
        code,
        label,
    }
}

pub(crate) fn issue_status_text(status: IssueStatus) -> String {
    format!("{} {}", issue_indicator(status), status.as_str())
}

pub(crate) fn issue_status_chip(status: IssueStatus) -> String {
    issue_status_presentation(status).label
}

pub(crate) fn issue_tree_label(issue: &Issue) -> String {
    let title = first_line(&issue.title);
    let summary = issue
        .description
        .as_deref()
        .map(first_nonempty_line)
        .filter(|desc| !desc.is_empty() && desc != &title)
        .map(|desc| format!("{title} - {desc}"))
        .unwrap_or(title);
    format!(
        "{} #{:<3} {}",
        issue_status_chip(issue.status),
        issue.id,
        summary
    )
}

pub(crate) fn issue_summary_rows(
    issue: &Issue,
    subtasks: &[Subtask],
    findings: &[Finding],
    steering: &[Steering],
    runs: &[AgentRun],
    remote_pr_link: Option<&RemotePrLink>,
) -> Vec<SummaryRow> {
    let mut rows = vec![
        summary_row("status", issue_status_text(issue.status)),
        summary_row(
            "intent",
            first_nonempty_line(issue.description.as_deref().unwrap_or(&issue.title)),
        ),
    ];
    if let Some(link) = remote_pr_link {
        rows.push(summary_row("remote pr", remote_pr_summary(link)));
    }
    push_optional_report(&mut rows, "agent summary", issue.agent_summary.as_deref());
    push_optional_report(&mut rows, "progress", issue.progress_report.as_deref());
    push_optional_report(&mut rows, "result", issue.result_report.as_deref());
    for run in runs
        .iter()
        .filter(|run| run.phase_report.is_some())
        .rev()
        .take(5)
    {
        if let Some(report) = run.phase_report.as_deref() {
            let line = first_nonempty_line(report);
            if !line.is_empty() {
                rows.push(summary_row(
                    "phase report",
                    format!("#{} {} {} - {}", run.id, run.role.as_str(), run.phase, line),
                ));
            }
        }
    }

    if !subtasks.is_empty() {
        let done = subtasks.iter().filter(|task| task.done).count();
        rows.push(summary_row(
            "plan",
            format!("{done}/{} subtasks done", subtasks.len()),
        ));
    }
    if !findings.is_empty() {
        let open = findings
            .iter()
            .filter(|finding| finding.status.as_str() == "open")
            .count();
        rows.push(summary_row(
            "review",
            format!("{open}/{} findings open", findings.len()),
        ));
    }
    if !steering.is_empty() {
        rows.push(summary_row(
            "queue",
            format!("{} pending messages", steering.len()),
        ));
    }
    if let Some(run) = runs.last() {
        let state = match (run.exit_kind, run.exit_code, run.status_after.as_deref()) {
            (Some(kind), Some(code), Some(after)) => {
                format!("{}:{} -> {}", kind.as_str(), code, after)
            }
            (Some(kind), Some(code), None) => format!("{}:{}", kind.as_str(), code),
            (Some(kind), None, Some(after)) => format!("{} -> {}", kind.as_str(), after),
            (Some(kind), None, None) => kind.as_str().to_string(),
            (None, _, _) => "running".to_string(),
        };
        rows.push(summary_row(
            "latest run",
            format!("#{} {} {} {state}", run.id, run.role.as_str(), run.phase),
        ));
    }
    rows
}

fn remote_pr_summary(link: &RemotePrLink) -> String {
    let mut parts = vec![
        format!("#{}", link.remote_pr_number),
        link.state.as_str().to_string(),
        format!("checks {}", link.check_status.as_str()),
    ];
    if let Some(summary) = link
        .check_summary
        .as_deref()
        .map(first_nonempty_line)
        .filter(|line| !line.is_empty())
    {
        parts.push(summary);
    }
    if let Some(merge) = link.merge_state_status.as_deref() {
        parts.push(format!("merge {merge}"));
    }
    if let Some(review) = link.review_decision.as_deref() {
        parts.push(format!("review {review}"));
    }
    parts.push(link.remote_url.clone());
    parts.join(" · ")
}

pub(crate) fn remote_sync_summary(runs: &[RemoteSyncRun], limit: usize) -> RemoteSyncSummary {
    let active = runs
        .iter()
        .filter(|run| {
            matches!(
                run.status,
                RemoteSyncStatus::Queued | RemoteSyncStatus::Running
            )
        })
        .count();
    let failures = runs
        .iter()
        .filter(|run| run.status == RemoteSyncStatus::Failed)
        .count();
    let rows = runs
        .iter()
        .take(limit)
        .map(remote_sync_row)
        .collect::<Vec<_>>();
    RemoteSyncSummary {
        active,
        failures,
        rows,
    }
}

fn remote_sync_row(run: &RemoteSyncRun) -> SummaryRow {
    let label = match run.status {
        RemoteSyncStatus::Queued | RemoteSyncStatus::Running => "active",
        RemoteSyncStatus::Failed => "failed",
        RemoteSyncStatus::Skipped => "skipped",
        RemoteSyncStatus::Done => "latest",
    };
    let mut parts = vec![
        format!("#{}", run.id),
        run.direction.as_str().to_string(),
        run.kind.as_str().to_string(),
        run.status.as_str().to_string(),
    ];
    if let Some(target) = remote_sync_target(run) {
        parts.push(target);
    }
    if let Some(message) = remote_sync_message(run) {
        parts.push(format!("- {message}"));
    }
    summary_row(label, parts.join(" "))
}

fn remote_sync_target(run: &RemoteSyncRun) -> Option<String> {
    if let Some(issue_id) = run.issue_id {
        return Some(format!("issue #{issue_id}"));
    }
    if let Some(backlog_id) = run.backlog_item_id {
        return Some(format!("backlog #{backlog_id}"));
    }
    if let Some(pr_id) = run.remote_pr_link_id {
        return Some(format!("pr-link #{pr_id}"));
    }
    if let Some(remote_issue_id) = run.remote_issue_link_id {
        return Some(format!("remote-issue-link #{remote_issue_id}"));
    }
    None
}

fn remote_sync_message(run: &RemoteSyncRun) -> Option<String> {
    run.error
        .as_deref()
        .filter(|text| !text.trim().is_empty())
        .or_else(|| {
            run.summary
                .as_deref()
                .filter(|text| !text.trim().is_empty())
        })
        .map(first_nonempty_line)
}

fn card_sort_key(card: &KanbanCard) -> (u8, IssueSortKey, i64) {
    match card {
        KanbanCard::Backlog { id, .. } => (
            0,
            IssueSortKey {
                attention: 1,
                lane: 0,
                status: 0,
                id: *id,
            },
            *id,
        ),
        KanbanCard::Issue { id, status, .. } => (1, issue_status_sort_key(*status, *id), *id),
    }
}

fn summary_row(label: &'static str, value: String) -> SummaryRow {
    SummaryRow { label, value }
}

fn push_optional_report(rows: &mut Vec<SummaryRow>, label: &'static str, value: Option<&str>) {
    let Some(value) = value else {
        return;
    };
    let line = first_nonempty_line(value);
    if !line.is_empty() {
        rows.push(summary_row(label, line));
    }
}

fn issue_sort_key(issue: &Issue) -> IssueSortKey {
    issue_status_sort_key(issue.status, issue.id)
}

fn issue_status_sort_key(status: IssueStatus, id: i64) -> IssueSortKey {
    IssueSortKey {
        attention: u8::from(!status.needs_attention()),
        lane: progress_rank(status),
        status: status_rank(status),
        id,
    }
}

fn progress_rank(status: IssueStatus) -> u8 {
    match KanbanLane::from_progress(status.progress_lane()) {
        KanbanLane::Plan => 0,
        KanbanLane::InProgress => 1,
        KanbanLane::Finalizing => 2,
        KanbanLane::Done => 3,
    }
}

fn status_rank(status: IssueStatus) -> u8 {
    const ORDER: [IssueStatus; 16] = [
        IssueStatus::PlanBlocked,
        IssueStatus::ReviewBlocked,
        IssueStatus::ReadyToMerge,
        IssueStatus::ConflictBlocked,
        IssueStatus::Failed,
        IssueStatus::New,
        IssueStatus::Planning,
        IssueStatus::PlanReady,
        IssueStatus::Working,
        IssueStatus::Reviewing,
        IssueStatus::Fixing,
        IssueStatus::Auditing,
        IssueStatus::Merging,
        IssueStatus::ResolvingConflict,
        IssueStatus::Done,
        IssueStatus::Abandoned,
    ];
    ORDER
        .iter()
        .position(|candidate| *candidate == status)
        .unwrap_or(ORDER.len()) as u8
}

fn status_code(status: IssueStatus) -> &'static str {
    let id = status.as_str();
    let words: Vec<&str> = id
        .split('_')
        .filter(|word| !matches!(*word, "TO" | "READY" | "BLOCKED" | "RESOLVING"))
        .collect();
    let salient = if status.needs_attention() {
        words.last().copied()
    } else if id.contains("CONFLICT") {
        Some("CONFLICT")
    } else {
        words.first().copied()
    }
    .unwrap_or(id);
    word_code(salient)
}

fn word_code(word: &str) -> &'static str {
    match word {
        "NEW" => "NEW",
        "PLAN" | "PLANNING" => "PLAN",
        "WORK" | "WORKING" => "WORK",
        "REVIEW" | "REVIEWING" => "REVW",
        "FIX" | "FIXING" => "FIX",
        "AUDIT" | "AUDITING" => "AUDT",
        "MERGE" | "MERGING" => "MERG",
        "CONFLICT" => "CNFL",
        "DONE" => "DONE",
        "FAILED" => "FAIL",
        "ABANDONED" => "ABND",
        _ => "????",
    }
}

fn first_line(text: &str) -> String {
    text.lines().next().unwrap_or("").to_string()
}

fn first_nonempty_line(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use auwsx_core::agent::ExitKind;
    use auwsx_core::backlog::Source;
    use auwsx_core::db::agent_runs::Role;
    use auwsx_core::db::remote::{
        RemotePrCheckStatus, RemotePrState, RemoteProvider, RemoteSyncDirection, RemoteSyncKind,
    };

    const TS: i64 = 1_000_000;

    fn backlog(id: i64, approval: Approval) -> BacklogItem {
        BacklogItem {
            id,
            project_id: 1,
            text: format!("item {id}\nextra"),
            source: Source::Human,
            approval,
            origin_routine_id: None,
            consumed_issue_id: None,
            created_at: TS,
            resolved_at: None,
        }
    }

    fn issue(id: i64, status: IssueStatus) -> Issue {
        Issue {
            id,
            project_id: 1,
            title: format!("issue {id}\nextra"),
            description: None,
            agent_summary: None,
            progress_report: None,
            result_report: None,
            status,
            branch: None,
            worktree_path: None,
            review_round: 0,
            conflict_attempts: 0,
            wait_until: None,
            absorbed_into_id: None,
            has_pending_steering: false,
            created_at: TS,
            updated_at: TS,
        }
    }

    fn remote_run(id: i64, status: RemoteSyncStatus) -> RemoteSyncRun {
        RemoteSyncRun {
            id,
            project_id: 1,
            issue_id: Some(7),
            backlog_item_id: None,
            remote_issue_link_id: None,
            remote_pr_link_id: None,
            direction: RemoteSyncDirection::Outbound,
            kind: RemoteSyncKind::Pr,
            status,
            summary: Some("created pull request".to_string()),
            error: None,
            started_at: Some(TS),
            ended_at: Some(TS + 1),
            created_at: TS,
        }
    }

    fn remote_pr_link(check_status: RemotePrCheckStatus) -> RemotePrLink {
        RemotePrLink {
            id: 1,
            project_id: 1,
            issue_id: 7,
            provider: RemoteProvider::Github,
            remote_owner: "acme".to_string(),
            remote_repo: "app".to_string(),
            remote_pr_number: 42,
            remote_node_id: None,
            remote_url: "https://github.com/acme/app/pull/42".to_string(),
            head_branch: "auwsx/issue-7".to_string(),
            head_sha: Some("head".to_string()),
            base_branch: "main".to_string(),
            base_sha: Some("base".to_string()),
            state: RemotePrState::Open,
            check_status,
            check_summary: Some("1 success, 0 pending, 1 failure, 0 unknown".to_string()),
            merge_state_status: Some("BLOCKED".to_string()),
            review_decision: Some("REVIEW_REQUIRED".to_string()),
            last_synced_at: Some(TS),
            created_at: TS,
            updated_at: TS,
        }
    }

    #[test]
    fn given_dismissed_backlog_when_projected_then_excluded() {
        let cards = kanban_cards(
            &[
                backlog(1, Approval::Approved),
                backlog(2, Approval::Dismissed),
            ],
            &[],
        );
        assert_eq!(cards.len(), 1);
    }

    #[test]
    fn given_many_archived_issues_when_projected_then_archive_cards_are_capped_at_ten() {
        let issues: Vec<Issue> = (0..12).map(|i| issue(i, IssueStatus::Done)).collect();
        let cards = kanban_cards(&[], &issues);
        assert_eq!(cards.len(), 10);
    }

    #[test]
    fn given_statuses_when_projected_then_cards_belong_to_expected_lanes() {
        let cards = kanban_cards(
            &[],
            &[
                issue(1, IssueStatus::Planning),
                issue(2, IssueStatus::Working),
                issue(3, IssueStatus::ReadyToMerge),
                issue(4, IssueStatus::Done),
            ],
        );
        for (id, lane) in [
            (1, KanbanLane::Plan),
            (2, KanbanLane::InProgress),
            (3, KanbanLane::Finalizing),
            (4, KanbanLane::Done),
        ] {
            let card = cards
                .iter()
                .find(|card| card.item() == KanbanItem::Issue(id))
                .expect("card exists");
            assert!(card.belongs_to(lane), "card {id} should belong to {lane:?}");
        }
    }

    #[test]
    fn given_attention_status_when_projected_then_issue_card_marks_attention() {
        let cards = kanban_cards(&[], &[issue(1, IssueStatus::PlanBlocked)]);
        match &cards[0] {
            KanbanCard::Issue {
                needs_attention, ..
            } => assert!(*needs_attention),
            other => panic!("expected issue card, got {other:?}"),
        }
    }

    #[test]
    fn given_attention_status_when_indicator_requested_then_bang() {
        assert_eq!(issue_indicator(IssueStatus::PlanBlocked), "!");
    }

    #[test]
    fn given_status_when_status_text_requested_then_uses_icon_and_exact_status() {
        assert_eq!(
            issue_status_text(IssueStatus::PlanBlocked),
            "! PLAN_BLOCKED"
        );
    }

    #[test]
    fn given_status_when_status_chip_requested_then_attention_and_four_char_code() {
        for status in [
            IssueStatus::New,
            IssueStatus::Planning,
            IssueStatus::PlanReady,
            IssueStatus::PlanBlocked,
            IssueStatus::Working,
            IssueStatus::Reviewing,
            IssueStatus::Fixing,
            IssueStatus::ReviewBlocked,
            IssueStatus::Auditing,
            IssueStatus::ReadyToMerge,
            IssueStatus::Merging,
            IssueStatus::ResolvingConflict,
            IssueStatus::ConflictBlocked,
            IssueStatus::Done,
            IssueStatus::Failed,
            IssueStatus::Abandoned,
        ] {
            let presentation = issue_status_presentation(status);
            assert_eq!(presentation.marker, issue_indicator(status));
            assert_eq!(presentation.code.chars().count(), 4);
            assert_eq!(presentation.label.chars().count(), 6);
        }
        assert_eq!(issue_status_chip(IssueStatus::ReadyToMerge), "! MERG");
        assert_eq!(issue_status_chip(IssueStatus::ResolvingConflict), "◉ CNFL");
    }

    #[test]
    fn given_issue_with_description_when_tree_label_requested_then_includes_description() {
        let mut issue = issue(7, IssueStatus::ReadyToMerge);
        issue.title = "input cursor".to_string();
        issue.description = Some("\nleft/right movement is missing\nmore".to_string());

        assert_eq!(
            issue_tree_label(&issue),
            "! MERG #7   input cursor - left/right movement is missing"
        );
    }

    #[test]
    fn given_mixed_issues_when_sorted_then_attention_statuses_come_first_by_pipeline_order() {
        let mut issues = vec![
            issue(4, IssueStatus::Working),
            issue(3, IssueStatus::ReadyToMerge),
            issue(2, IssueStatus::PlanBlocked),
            issue(1, IssueStatus::New),
        ];

        sort_issues_for_status_view(&mut issues);

        assert_eq!(
            issues.iter().map(|issue| issue.id).collect::<Vec<_>>(),
            vec![2, 3, 1, 4]
        );
    }

    #[test]
    fn given_issue_context_when_summary_rows_requested_then_progress_and_latest_run_are_visible() {
        let mut issue = issue(7, IssueStatus::Working);
        issue.description = Some("implement archive UX".to_string());
        issue.agent_summary = Some("Created plan for archive access.".to_string());
        issue.progress_report = Some("Two subtasks are complete.".to_string());
        let subtasks = vec![
            Subtask {
                id: 1,
                issue_id: 7,
                ord: 1,
                text: "plan".to_string(),
                done: true,
                created_at: TS,
                done_at: Some(TS),
            },
            Subtask {
                id: 2,
                issue_id: 7,
                ord: 2,
                text: "implement".to_string(),
                done: false,
                created_at: TS,
                done_at: None,
            },
        ];
        let runs = vec![AgentRun {
            id: 9,
            issue_id: Some(7),
            main_job_id: None,
            role: Role::Work,
            phase: "WORKING".to_string(),
            agent_cmd: "codex".to_string(),
            status_before: Some("WORKING".to_string()),
            status_after: Some("REVIEWING".to_string()),
            pid: Some(123),
            exit_code: Some(0),
            exit_kind: Some(ExitKind::Exited),
            prompt_path: None,
            log_path: None,
            phase_report: Some("Implemented archive view and verified tests.".to_string()),
            spawned_at: TS,
            exited_at: Some(TS + 1),
            note: None,
        }];

        let rows = issue_summary_rows(&issue, &subtasks, &[], &[], &runs, None);

        assert!(rows
            .iter()
            .any(|row| row.label == "intent" && row.value == "implement archive UX"));
        assert!(rows
            .iter()
            .any(|row| row.label == "plan" && row.value == "1/2 subtasks done"));
        assert!(rows.iter().any(|row| {
            row.label == "phase report"
                && row.value == "#9 work WORKING - Implemented archive view and verified tests."
        }));
        assert!(rows.iter().any(|row| {
            row.label == "latest run" && row.value == "#9 work WORKING exited:0 -> REVIEWING"
        }));
    }

    #[test]
    fn given_remote_pr_link_when_issue_rows_requested_then_check_status_is_visible() {
        let issue = issue(7, IssueStatus::ReadyToMerge);
        let link = remote_pr_link(RemotePrCheckStatus::Failure);

        let rows = issue_summary_rows(&issue, &[], &[], &[], &[], Some(&link));

        assert!(rows.iter().any(|row| {
            row.label == "remote pr"
                && row.value.contains("#42")
                && row.value.contains("checks failure")
                && row.value.contains("merge BLOCKED")
                && row.value.contains("review REVIEW_REQUIRED")
                && row.value.contains("https://github.com/acme/app/pull/42")
        }));
    }

    #[test]
    fn given_remote_sync_runs_when_projected_then_active_failures_and_messages_are_visible() {
        let mut failed = remote_run(2, RemoteSyncStatus::Failed);
        failed.error = Some("check suite is red".to_string());
        let rows = remote_sync_summary(
            &[
                remote_run(3, RemoteSyncStatus::Running),
                failed,
                remote_run(1, RemoteSyncStatus::Done),
            ],
            2,
        );

        assert_eq!(rows.active, 1);
        assert_eq!(rows.failures, 1);
        assert_eq!(rows.rows.len(), 2);
        assert_eq!(rows.rows[0].label, "active");
        assert!(rows.rows[0]
            .value
            .contains("#3 outbound pr running issue #7"));
        assert_eq!(rows.rows[1].label, "failed");
        assert!(rows.rows[1].value.contains("check suite is red"));
    }
}

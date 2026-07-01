//! Pure remote repository workflow planning.
//!
//! This module deliberately does not talk to GitHub, git, or SQLite. Runtime
//! adapters execute the returned actions and record the resulting links/runs.

use crate::db::remote::{ProjectRemoteConfig, RemoteIssueLink, RemotePrLink, RemotePrState};
use crate::db::Issue;
use crate::state::IssueStatus;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemotePlanBlocker {
    MissingConfig,
    OutboundIssueCreateDisabled,
    RemotePrMergeDisabled,
    MissingBranch,
    MissingRemoteIssueLink,
    MissingRemotePrLink,
    PullRequestAlreadyMerged,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteCommentTarget {
    Issue,
    PullRequest,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "action")]
pub enum RemotePlannedAction {
    CreateIssue {
        issue_id: i64,
        title: String,
        body: String,
        labels: Vec<String>,
        assignees: Vec<String>,
    },
    CreateOrUpdatePullRequest {
        issue_id: i64,
        title: String,
        body: String,
        head_branch: String,
        base_branch: String,
        draft: bool,
        require_green_checks: bool,
    },
    PostProgressComment {
        issue_id: i64,
        target: RemoteCommentTarget,
        remote_link_id: i64,
        marker: String,
        body: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct RemoteWorkflowPlan {
    pub actions: Vec<RemotePlannedAction>,
    pub blockers: Vec<RemotePlanBlocker>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct RemoteNotesPresence {
    pub agent_summary: bool,
    pub subtasks: bool,
    pub findings: bool,
    pub subtask_lines: Vec<String>,
    pub finding_lines: Vec<String>,
}

impl RemoteNotesPresence {
    pub fn from_issue(issue: &Issue) -> Self {
        Self {
            agent_summary: issue.agent_summary.as_deref().is_some_and(non_blank),
            subtasks: false,
            findings: false,
            subtask_lines: Vec::new(),
            finding_lines: Vec::new(),
        }
    }

    fn any_enabled(&self, config: &ProjectRemoteConfig) -> bool {
        (self.agent_summary && config.agent_comment_sync_enabled)
            || (self.subtasks && config.subtask_comment_sync_enabled)
            || (self.findings && config.finding_comment_sync_enabled)
    }
}

#[derive(Debug, Clone)]
pub struct RemoteWorkflowInput<'a> {
    pub config: Option<&'a ProjectRemoteConfig>,
    pub issue: &'a Issue,
    pub issue_link: Option<&'a RemoteIssueLink>,
    pub pr_link: Option<&'a RemotePrLink>,
    pub notes: RemoteNotesPresence,
}

pub fn plan_issue_remote_workflow(input: RemoteWorkflowInput<'_>) -> RemoteWorkflowPlan {
    let mut plan = RemoteWorkflowPlan::default();
    let Some(config) = input.config else {
        plan.blockers.push(RemotePlanBlocker::MissingConfig);
        return plan;
    };

    plan_remote_issue(config, input.issue, input.issue_link, &mut plan);
    plan_progress_comment(
        config,
        input.issue,
        input.issue_link,
        input.pr_link,
        &input.notes,
        &mut plan,
    );
    plan_pull_request(config, input.issue, input.pr_link, &mut plan);

    plan
}

fn plan_remote_issue(
    config: &ProjectRemoteConfig,
    issue: &Issue,
    issue_link: Option<&RemoteIssueLink>,
    plan: &mut RemoteWorkflowPlan,
) {
    if issue_link.is_some() {
        return;
    }
    if !config.outbound_issue_create_enabled {
        plan.blockers
            .push(RemotePlanBlocker::OutboundIssueCreateDisabled);
        return;
    }
    plan.actions.push(RemotePlannedAction::CreateIssue {
        issue_id: issue.id,
        title: issue.title.clone(),
        body: issue_body(issue),
        labels: split_csv(config.default_labels.as_deref()),
        assignees: split_csv(config.default_assignees.as_deref()),
    });
}

fn plan_progress_comment(
    config: &ProjectRemoteConfig,
    issue: &Issue,
    issue_link: Option<&RemoteIssueLink>,
    pr_link: Option<&RemotePrLink>,
    notes: &RemoteNotesPresence,
    plan: &mut RemoteWorkflowPlan,
) {
    if !notes.any_enabled(config) {
        return;
    }
    let body = progress_comment_body(issue, notes);
    if let Some(pr) = pr_link {
        plan.actions.push(RemotePlannedAction::PostProgressComment {
            issue_id: issue.id,
            target: RemoteCommentTarget::PullRequest,
            remote_link_id: pr.id,
            marker: progress_marker(issue.id),
            body,
        });
        return;
    }
    if let Some(link) = issue_link {
        plan.actions.push(RemotePlannedAction::PostProgressComment {
            issue_id: issue.id,
            target: RemoteCommentTarget::Issue,
            remote_link_id: link.id,
            marker: progress_marker(issue.id),
            body,
        });
        return;
    }
    plan.blockers
        .push(RemotePlanBlocker::MissingRemoteIssueLink);
}

fn plan_pull_request(
    config: &ProjectRemoteConfig,
    issue: &Issue,
    pr_link: Option<&RemotePrLink>,
    plan: &mut RemoteWorkflowPlan,
) {
    if issue.status != IssueStatus::ReadyToMerge {
        return;
    }
    if !config.remote_pr_merge_enabled {
        plan.blockers.push(RemotePlanBlocker::RemotePrMergeDisabled);
        return;
    }
    if pr_link.is_some_and(|link| link.state == RemotePrState::Merged) {
        plan.blockers
            .push(RemotePlanBlocker::PullRequestAlreadyMerged);
        return;
    }
    let Some(head_branch) = issue.branch.as_deref().filter(|s| non_blank(s)) else {
        plan.blockers.push(RemotePlanBlocker::MissingBranch);
        return;
    };
    plan.actions
        .push(RemotePlannedAction::CreateOrUpdatePullRequest {
            issue_id: issue.id,
            title: format!("Issue {}: {}", issue.id, issue.title),
            body: pull_request_body(issue),
            head_branch: head_branch.to_string(),
            base_branch: config
                .pr_base_branch
                .as_deref()
                .filter(|s| non_blank(s))
                .unwrap_or("main")
                .to_string(),
            draft: config.draft_pr_enabled,
            require_green_checks: matches!(
                config.required_checks_policy,
                crate::db::remote::RequiredChecksPolicy::RequireGreen
            ),
        });
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "decision")]
pub enum InboundAuwsxRunDecision {
    Accept { title: String, description: String },
    Ignore { reason: InboundAuwsxRunIgnoreReason },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InboundAuwsxRunIgnoreReason {
    MissingConfig,
    Disabled,
    CommandMissing,
}

pub fn plan_inbound_auwsx_run(
    config: Option<&ProjectRemoteConfig>,
    remote_issue_title: &str,
    remote_issue_url: &str,
    comment_body: &str,
) -> InboundAuwsxRunDecision {
    let Some(config) = config else {
        return InboundAuwsxRunDecision::Ignore {
            reason: InboundAuwsxRunIgnoreReason::MissingConfig,
        };
    };
    if !config.inbound_auwsx_run_enabled {
        return InboundAuwsxRunDecision::Ignore {
            reason: InboundAuwsxRunIgnoreReason::Disabled,
        };
    }
    let Some(command) = extract_auwsx_run(comment_body) else {
        return InboundAuwsxRunDecision::Ignore {
            reason: InboundAuwsxRunIgnoreReason::CommandMissing,
        };
    };

    let title = if command.is_empty() {
        format!("Remote /auwsx-run: {}", remote_issue_title.trim())
    } else {
        command.to_string()
    };
    InboundAuwsxRunDecision::Accept {
        title,
        description: format!(
            "Remote issue: {}\n\nCommand:\n{}\n\nComment:\n{}",
            remote_issue_url.trim(),
            if command.is_empty() {
                "/auwsx-run"
            } else {
                command
            },
            comment_body.trim()
        ),
    }
}

fn extract_auwsx_run(comment_body: &str) -> Option<&str> {
    comment_body.lines().find_map(|line| {
        let line = line.trim();
        if line == "/auwsx-run" {
            Some("")
        } else {
            line.strip_prefix("/auwsx-run ").map(str::trim)
        }
    })
}

fn issue_body(issue: &Issue) -> String {
    let mut body = format!(
        "Local auwsx issue #{}\n\nStatus: {}",
        issue.id,
        issue.status.as_str()
    );
    if let Some(description) = issue.description.as_deref().filter(|s| non_blank(s)) {
        body.push_str("\n\n");
        body.push_str(description.trim());
    }
    body
}

fn pull_request_body(issue: &Issue) -> String {
    let mut body = format!(
        "## Summary\n{}\n\n## Safety\n- auwsx status: {}\n- branch: {}",
        issue
            .result_report
            .as_deref()
            .or(issue.agent_summary.as_deref())
            .unwrap_or("Ready for remote merge."),
        issue.status.as_str(),
        issue.branch.as_deref().unwrap_or("(missing)")
    );
    if let Some(progress) = issue.progress_report.as_deref().filter(|s| non_blank(s)) {
        body.push_str("\n\n## Progress\n");
        body.push_str(progress.trim());
    }
    body
}

fn progress_comment_body(issue: &Issue, notes: &RemoteNotesPresence) -> String {
    let mut lines = vec![
        format!("auwsx issue #{}", issue.id),
        format!("status: {}", issue.status.as_str()),
    ];
    if notes.agent_summary {
        if let Some(summary) = issue.agent_summary.as_deref().filter(|s| non_blank(s)) {
            lines.push(format!("summary: {}", summary.trim()));
        }
    }
    if notes.subtasks {
        lines.push("subtasks:".to_string());
        if notes.subtask_lines.is_empty() {
            lines.push("- changed".to_string());
        } else {
            lines.extend(notes.subtask_lines.iter().map(|line| format!("- {line}")));
        }
    }
    if notes.findings {
        lines.push("findings:".to_string());
        if notes.finding_lines.is_empty() {
            lines.push("- changed".to_string());
        } else {
            lines.extend(notes.finding_lines.iter().map(|line| format!("- {line}")));
        }
    }
    lines.join("\n")
}

fn progress_marker(issue_id: i64) -> String {
    format!("<!-- auwsx:issue-progress:{issue_id} -->")
}

fn split_csv(value: Option<&str>) -> Vec<String> {
    value
        .unwrap_or("")
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn non_blank(value: &str) -> bool {
    !value.trim().is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::remote::{RemoteAuthKind, RemoteProvider, RequiredChecksPolicy};

    fn config() -> ProjectRemoteConfig {
        ProjectRemoteConfig {
            project_id: 7,
            provider: RemoteProvider::Github,
            remote_url: "https://github.com/acme/app".to_string(),
            owner: "acme".to_string(),
            repo: "app".to_string(),
            api_base_url: "https://api.github.com".to_string(),
            auth_kind: RemoteAuthKind::TokenEnv,
            auth_ref: Some("GITHUB_TOKEN".to_string()),
            webhook_secret_ref: Some("GITHUB_WEBHOOK_SECRET".to_string()),
            inbound_auwsx_run_enabled: true,
            outbound_issue_create_enabled: true,
            remote_pr_merge_enabled: true,
            agent_comment_sync_enabled: true,
            subtask_comment_sync_enabled: false,
            finding_comment_sync_enabled: false,
            draft_pr_enabled: true,
            required_checks_policy: RequiredChecksPolicy::RequireGreen,
            default_labels: Some("auwsx, automation".to_string()),
            default_assignees: Some("alice".to_string()),
            pr_base_branch: Some("main".to_string()),
            created_at: 1,
            updated_at: 1,
        }
    }

    fn issue(status: IssueStatus) -> Issue {
        Issue {
            id: 42,
            project_id: 7,
            title: "Add remote sync".to_string(),
            description: Some("Implement GitHub bridge.".to_string()),
            agent_summary: Some("Implemented model boundary.".to_string()),
            progress_report: Some("Tests pass.".to_string()),
            result_report: Some("Remote sync ready.".to_string()),
            status,
            branch: Some("auwsx/issue-42".to_string()),
            worktree_path: None,
            review_round: 0,
            conflict_attempts: 0,
            wait_until: None,
            absorbed_into_id: None,
            has_pending_steering: false,
            created_at: 1,
            updated_at: 1,
        }
    }

    fn issue_link() -> RemoteIssueLink {
        RemoteIssueLink {
            id: 5,
            project_id: 7,
            issue_id: Some(42),
            backlog_item_id: None,
            provider: RemoteProvider::Github,
            remote_owner: "acme".to_string(),
            remote_repo: "app".to_string(),
            remote_issue_number: 99,
            remote_node_id: None,
            remote_url: "https://github.com/acme/app/issues/99".to_string(),
            last_synced_at: None,
            created_at: 1,
            updated_at: 1,
        }
    }

    #[test]
    fn given_missing_config_when_planning_issue_remote_workflow_then_blocks() {
        let issue = issue(IssueStatus::Working);

        let plan = plan_issue_remote_workflow(RemoteWorkflowInput {
            config: None,
            issue: &issue,
            issue_link: None,
            pr_link: None,
            notes: RemoteNotesPresence::from_issue(&issue),
        });

        assert_eq!(plan.blockers, vec![RemotePlanBlocker::MissingConfig]);
    }

    #[test]
    fn given_unlinked_issue_with_outbound_enabled_when_planning_then_create_issue_action() {
        let config = config();
        let issue = issue(IssueStatus::Working);

        let plan = plan_issue_remote_workflow(RemoteWorkflowInput {
            config: Some(&config),
            issue: &issue,
            issue_link: None,
            pr_link: None,
            notes: RemoteNotesPresence::from_issue(&issue),
        });

        assert!(matches!(
            plan.actions.first(),
            Some(RemotePlannedAction::CreateIssue { issue_id: 42, .. })
        ));
    }

    #[test]
    fn given_ready_issue_with_pr_merge_enabled_when_planning_then_pr_action() {
        let config = config();
        let issue = issue(IssueStatus::ReadyToMerge);
        let link = issue_link();

        let plan = plan_issue_remote_workflow(RemoteWorkflowInput {
            config: Some(&config),
            issue: &issue,
            issue_link: Some(&link),
            pr_link: None,
            notes: RemoteNotesPresence::from_issue(&issue),
        });

        assert!(plan.actions.iter().any(|action| matches!(
            action,
            RemotePlannedAction::CreateOrUpdatePullRequest {
                issue_id: 42,
                head_branch,
                base_branch,
                draft: true,
                require_green_checks: true,
                ..
            } if head_branch == "auwsx/issue-42" && base_branch == "main"
        )));
    }

    #[test]
    fn given_ready_issue_without_branch_when_planning_pr_then_missing_branch_blocker() {
        let config = config();
        let mut issue = issue(IssueStatus::ReadyToMerge);
        issue.branch = Some(" ".to_string());

        let plan = plan_issue_remote_workflow(RemoteWorkflowInput {
            config: Some(&config),
            issue: &issue,
            issue_link: None,
            pr_link: None,
            notes: RemoteNotesPresence::from_issue(&issue),
        });

        assert!(plan.blockers.contains(&RemotePlanBlocker::MissingBranch));
    }

    #[test]
    fn given_enabled_inbound_run_comment_when_planning_then_accepts_backlog() {
        let config = config();

        let decision = plan_inbound_auwsx_run(
            Some(&config),
            "Remote task",
            "https://github.com/acme/app/issues/99",
            "please handle\n/auwsx-run add retry telemetry",
        );

        assert!(matches!(
            decision,
            InboundAuwsxRunDecision::Accept { ref title, .. }
                if title == "add retry telemetry"
        ));
    }

    #[test]
    fn given_note_sync_enabled_when_planning_comment_then_body_contains_subtasks_and_findings() {
        let config = config();
        let issue = issue(IssueStatus::Working);
        let link = issue_link();
        let notes = RemoteNotesPresence {
            agent_summary: true,
            subtasks: true,
            findings: true,
            subtask_lines: vec!["[x] write executor".to_string()],
            finding_lines: vec!["[open/major] add stale guard (src/lib.rs)".to_string()],
        };

        let plan = plan_issue_remote_workflow(RemoteWorkflowInput {
            config: Some(&config),
            issue: &issue,
            issue_link: Some(&link),
            pr_link: None,
            notes,
        });

        let body = plan
            .actions
            .iter()
            .find_map(|action| match action {
                RemotePlannedAction::PostProgressComment { body, .. } => Some(body),
                _ => None,
            })
            .expect("comment action exists");
        assert!(body.contains("- [x] write executor"));
        assert!(body.contains("- [open/major] add stale guard (src/lib.rs)"));
    }

    #[test]
    fn given_disabled_inbound_run_when_planning_then_ignores() {
        let mut config = config();
        config.inbound_auwsx_run_enabled = false;

        let decision = plan_inbound_auwsx_run(
            Some(&config),
            "Remote task",
            "https://github.com/acme/app/issues/99",
            "/auwsx-run add retry telemetry",
        );

        assert_eq!(
            decision,
            InboundAuwsxRunDecision::Ignore {
                reason: InboundAuwsxRunIgnoreReason::Disabled
            }
        );
    }
}

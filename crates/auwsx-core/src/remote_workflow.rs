//! Daemon-owned remote workflow queueing.
//!
//! This layer converts pure [`remote_plan`] actions into durable
//! `remote_sync_runs`. It still performs no provider network I/O.

use crate::db::remote::{
    self, NewRemoteSyncRun, RemoteSyncDirection, RemoteSyncKind, RemoteSyncStatus,
};
use crate::db::{findings, issues, subtasks, Db, Issue};
use crate::remote_plan::{
    self, RemoteNotesPresence, RemotePlannedAction, RemoteWorkflowInput, RemoteWorkflowPlan,
};
use crate::Result;
use anyhow::anyhow;
use sqlx::SqlitePool;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueuedRemoteWorkflow {
    pub issue_id: i64,
    pub plan: RemoteWorkflowPlan,
    pub queued_run_ids: Vec<i64>,
}

pub async fn queue_issue_remote_workflow(
    pool: &SqlitePool,
    issue_id: i64,
    now: i64,
) -> Result<QueuedRemoteWorkflow> {
    let issue = issues::get(pool, issue_id)
        .await?
        .ok_or_else(|| anyhow!("issue {issue_id} not found"))?;
    let config = remote::get_config(pool, issue.project_id).await?;
    let issue_link = remote::issue_link_by_issue(pool, issue_id).await?;
    let pr_link = remote::pr_link_by_issue(pool, issue_id).await?;
    let notes = notes_presence(pool, &issue).await?;
    let plan = remote_plan::plan_issue_remote_workflow(RemoteWorkflowInput {
        config: config.as_ref(),
        issue: &issue,
        issue_link: issue_link.as_ref(),
        pr_link: pr_link.as_ref(),
        notes,
    });

    let mut queued_run_ids = Vec::new();
    for action in &plan.actions {
        let kind = action_kind(action);
        if remote::has_active_sync_run(pool, issue.project_id, Some(issue_id), kind).await? {
            continue;
        }
        let summary = action_summary(action);
        queued_run_ids.push(
            remote::create_sync_run(
                pool,
                NewRemoteSyncRun {
                    project_id: issue.project_id,
                    issue_id: Some(issue_id),
                    backlog_item_id: None,
                    remote_issue_link_id: remote_issue_link_id(action),
                    remote_pr_link_id: remote_pr_link_id(action),
                    direction: RemoteSyncDirection::Outbound,
                    kind,
                    status: RemoteSyncStatus::Queued,
                    summary: Some(&summary),
                    error: None,
                    started_at: None,
                    ended_at: None,
                },
                now,
            )
            .await?,
        );
    }

    Ok(QueuedRemoteWorkflow {
        issue_id,
        plan,
        queued_run_ids,
    })
}

pub async fn queue_issues_remote_workflow(
    db: &Db,
    issue_ids: impl IntoIterator<Item = i64>,
    now: i64,
) -> Result<Vec<QueuedRemoteWorkflow>> {
    let mut out = Vec::new();
    for issue_id in issue_ids {
        out.push(queue_issue_remote_workflow(db.pool(), issue_id, now).await?);
    }
    Ok(out)
}

pub async fn notes_presence(pool: &SqlitePool, issue: &Issue) -> Result<RemoteNotesPresence> {
    let subtasks = subtasks::list_by_issue(pool, issue.id).await?;
    let findings = findings::list_by_issue(pool, issue.id).await?;
    Ok(RemoteNotesPresence {
        agent_summary: issue.agent_summary.as_deref().is_some_and(non_blank),
        subtasks: !subtasks.is_empty(),
        findings: !findings.is_empty(),
        subtask_lines: subtasks
            .iter()
            .map(|item| {
                format!(
                    "[{}] {}",
                    if item.done { "x" } else { " " },
                    item.text.trim()
                )
            })
            .collect(),
        finding_lines: findings
            .iter()
            .map(|item| {
                let status = item.status.as_str();
                let severity = item.severity.as_str();
                match item.file_ref.as_deref().filter(|s| !s.trim().is_empty()) {
                    Some(file_ref) => {
                        format!("[{status}/{severity}] {} ({file_ref})", item.title.trim())
                    }
                    None => format!("[{status}/{severity}] {}", item.title.trim()),
                }
            })
            .collect(),
    })
}

fn non_blank(value: &str) -> bool {
    !value.trim().is_empty()
}

fn action_kind(action: &RemotePlannedAction) -> RemoteSyncKind {
    match action {
        RemotePlannedAction::CreateIssue { .. } => RemoteSyncKind::Issue,
        RemotePlannedAction::CreateOrUpdatePullRequest { .. } => RemoteSyncKind::Pr,
        RemotePlannedAction::PostProgressComment { .. } => RemoteSyncKind::Comment,
    }
}

fn remote_issue_link_id(action: &RemotePlannedAction) -> Option<i64> {
    match action {
        RemotePlannedAction::PostProgressComment {
            target: remote_plan::RemoteCommentTarget::Issue,
            remote_link_id,
            ..
        } => Some(*remote_link_id),
        _ => None,
    }
}

fn remote_pr_link_id(action: &RemotePlannedAction) -> Option<i64> {
    match action {
        RemotePlannedAction::PostProgressComment {
            target: remote_plan::RemoteCommentTarget::PullRequest,
            remote_link_id,
            ..
        } => Some(*remote_link_id),
        _ => None,
    }
}

fn action_summary(action: &RemotePlannedAction) -> String {
    match action {
        RemotePlannedAction::CreateIssue {
            issue_id, title, ..
        } => {
            format!("create remote issue for local issue #{issue_id}: {title}")
        }
        RemotePlannedAction::CreateOrUpdatePullRequest {
            issue_id,
            head_branch,
            base_branch,
            ..
        } => format!(
            "create or update remote PR for local issue #{issue_id}: {head_branch} -> {base_branch}"
        ),
        RemotePlannedAction::PostProgressComment {
            issue_id, target, ..
        } => format!("sync auwsx progress comment for local issue #{issue_id} to {target:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::projects::{self, NewProject};
    use crate::db::remote::{
        RemoteAuthKind, RemoteProvider, RequiredChecksPolicy, UpsertProjectRemoteConfig,
    };
    use crate::state::IssueStatus;

    const TS: i64 = 1_000;

    async fn project(db: &Db) -> i64 {
        projects::create(
            db.pool(),
            NewProject {
                name: "p",
                repo_path: ".",
                default_branch: "main",
                arsenal_preset_name: None,
                main_agent_cmd: "cat",
                route_agent_cmd: "cat",
                plan_agent_cmd: "cat",
                work_agent_cmd: "cat",
                review_agent_cmd: None,
                completion_policy: Some(projects::CompletionPolicy::Manual),
                plan_gate_timeout_min: Some(10),
                completion_soft_timeout_min: Some(60),
                schedule_interval_min: None,
                schedule_cron: None,
            },
            TS,
        )
        .await
        .unwrap()
    }

    async fn config(db: &Db, project_id: i64) {
        remote::upsert_config(
            db.pool(),
            UpsertProjectRemoteConfig {
                project_id,
                provider: RemoteProvider::Github,
                remote_url: "https://github.com/acme/app",
                owner: "acme",
                repo: "app",
                api_base_url: "https://api.github.com",
                auth_kind: RemoteAuthKind::None,
                auth_ref: None,
                webhook_secret_ref: None,
                inbound_auwsx_run_enabled: false,
                outbound_issue_create_enabled: true,
                remote_pr_merge_enabled: true,
                agent_comment_sync_enabled: false,
                subtask_comment_sync_enabled: false,
                finding_comment_sync_enabled: false,
                draft_pr_enabled: false,
                required_checks_policy: RequiredChecksPolicy::Observe,
                default_labels: None,
                default_assignees: None,
                pr_base_branch: Some("main"),
            },
            TS,
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn given_unlinked_issue_when_queued_then_remote_issue_run_created() {
        let db = Db::open_memory().await.unwrap();
        let project_id = project(&db).await;
        config(&db, project_id).await;
        let issue_id = issues::create(db.pool(), project_id, "remote", None, TS)
            .await
            .unwrap();

        let queued = queue_issue_remote_workflow(db.pool(), issue_id, TS + 1)
            .await
            .unwrap();

        assert_eq!(queued.queued_run_ids.len(), 1);
    }

    #[tokio::test]
    async fn given_active_issue_sync_when_queued_again_then_no_duplicate_run_created() {
        let db = Db::open_memory().await.unwrap();
        let project_id = project(&db).await;
        config(&db, project_id).await;
        let issue_id = issues::create(db.pool(), project_id, "remote", None, TS)
            .await
            .unwrap();
        queue_issue_remote_workflow(db.pool(), issue_id, TS + 1)
            .await
            .unwrap();

        let queued = queue_issue_remote_workflow(db.pool(), issue_id, TS + 2)
            .await
            .unwrap();

        assert!(queued.queued_run_ids.is_empty());
    }

    #[tokio::test]
    async fn given_ready_issue_when_queued_then_pr_run_created() {
        let db = Db::open_memory().await.unwrap();
        let project_id = project(&db).await;
        config(&db, project_id).await;
        let issue_id = issues::create(db.pool(), project_id, "remote", None, TS)
            .await
            .unwrap();
        issues::set_worktree(
            db.pool(),
            issue_id,
            Some("auwsx/issue-1"),
            Some("/worktree"),
            None,
            TS + 1,
        )
        .await
        .unwrap();
        issues::force_status(db.pool(), issue_id, IssueStatus::ReadyToMerge, TS + 2)
            .await
            .unwrap();

        let queued = queue_issue_remote_workflow(db.pool(), issue_id, TS + 3)
            .await
            .unwrap();

        assert!(queued.plan.actions.iter().any(|action| matches!(
            action,
            RemotePlannedAction::CreateOrUpdatePullRequest { .. }
        )));
    }
}

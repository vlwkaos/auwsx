//! Remote sync execution boundary.
//!
//! The scheduler owns row lifecycle. This module re-plans queued work against
//! current DB state before provider mutation so stale queued rows are skipped
//! instead of applying old assumptions.

use crate::db::remote::{
    self, ProjectRemoteConfig, RemoteIssueLink, RemotePrLink, RemotePrState, RemoteProvider,
    RemoteSyncKind, RemoteSyncRun, RemoteSyncStatus, UpsertRemoteIssueLink, UpsertRemotePrLink,
};
use crate::db::{issues, Issue};
use crate::remote_plan::{self, RemotePlannedAction, RemoteWorkflowInput, RemoteWorkflowPlan};
use crate::Result;
use anyhow::{anyhow, bail, Context};
use async_trait::async_trait;
use serde_json::Value;
use sqlx::SqlitePool;
use std::process::Stdio;
use tokio::process::Command;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteSyncExecution {
    pub run_id: i64,
    pub status: RemoteSyncStatus,
    pub remote_issue_link_id: Option<i64>,
    pub remote_pr_link_id: Option<i64>,
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct RemoteSyncRequest {
    pub run: RemoteSyncRun,
    pub config: ProjectRemoteConfig,
    pub issue: Issue,
    pub issue_link: Option<RemoteIssueLink>,
    pub pr_link: Option<RemotePrLink>,
    pub action: RemotePlannedAction,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreatedRemoteIssue {
    pub number: i64,
    pub node_id: Option<String>,
    pub url: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreatedRemotePr {
    pub number: i64,
    pub node_id: Option<String>,
    pub url: String,
    pub head_branch: String,
    pub head_sha: Option<String>,
    pub base_branch: String,
    pub base_sha: Option<String>,
    pub state: RemotePrState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteProviderEffect {
    Issue(CreatedRemoteIssue),
    PullRequest(CreatedRemotePr),
    Comment,
}

#[async_trait]
pub trait RemoteProviderExecutor: Send + Sync {
    async fn execute(&self, request: RemoteSyncRequest) -> Result<RemoteProviderEffect>;
}

#[derive(Debug, Default)]
pub struct GithubCliRemoteExecutor;

#[async_trait]
impl RemoteProviderExecutor for GithubCliRemoteExecutor {
    async fn execute(&self, request: RemoteSyncRequest) -> Result<RemoteProviderEffect> {
        if request.config.provider != RemoteProvider::Github {
            bail!("unsupported remote provider {:?}", request.config.provider);
        }
        match &request.action {
            RemotePlannedAction::CreateIssue {
                title,
                body,
                labels,
                assignees,
                ..
            } => {
                let url = run_gh(
                    &request.config,
                    gh_issue_create_args(&request.config, title, body, labels, assignees),
                )
                .await?;
                Ok(RemoteProviderEffect::Issue(CreatedRemoteIssue {
                    number: parse_github_number(&url, "issues")?,
                    node_id: None,
                    url: url.trim().to_string(),
                }))
            }
            RemotePlannedAction::CreateOrUpdatePullRequest {
                title,
                body,
                head_branch,
                base_branch,
                draft,
                ..
            } => {
                let create = run_gh(
                    &request.config,
                    gh_pr_create_args(
                        &request.config,
                        title,
                        body,
                        head_branch,
                        base_branch,
                        *draft,
                    ),
                )
                .await;
                let url = match create {
                    Ok(url) => url,
                    Err(create_err) => {
                        let viewed = run_gh(
                            &request.config,
                            vec![
                                "pr".to_string(),
                                "view".to_string(),
                                head_branch.to_string(),
                                "--repo".to_string(),
                                repo_slug(&request.config),
                                "--json".to_string(),
                                "number,url,id,state,headRefName,baseRefName,headRefOid,baseRefOid"
                                    .to_string(),
                            ],
                        )
                        .await
                        .with_context(|| {
                            format!("creating PR failed and no existing PR was found: {create_err}")
                        })?;
                        return parse_pr_view_json(&viewed).map(RemoteProviderEffect::PullRequest);
                    }
                };
                Ok(RemoteProviderEffect::PullRequest(CreatedRemotePr {
                    number: parse_github_number(&url, "pull")?,
                    node_id: None,
                    url: url.trim().to_string(),
                    head_branch: head_branch.clone(),
                    head_sha: None,
                    base_branch: base_branch.clone(),
                    base_sha: None,
                    state: RemotePrState::Open,
                }))
            }
            RemotePlannedAction::PostProgressComment { target, body, .. } => {
                let number = match target {
                    remote_plan::RemoteCommentTarget::Issue => request
                        .issue_link
                        .as_ref()
                        .map(|link| link.remote_issue_number)
                        .ok_or_else(|| anyhow!("comment sync missing remote issue link"))?,
                    remote_plan::RemoteCommentTarget::PullRequest => request
                        .pr_link
                        .as_ref()
                        .map(|link| link.remote_pr_number)
                        .ok_or_else(|| anyhow!("comment sync missing remote PR link"))?,
                };
                let target_arg = match target {
                    remote_plan::RemoteCommentTarget::Issue => "issue",
                    remote_plan::RemoteCommentTarget::PullRequest => "pr",
                };
                run_gh(
                    &request.config,
                    vec![
                        target_arg.to_string(),
                        "comment".to_string(),
                        number.to_string(),
                        "--repo".to_string(),
                        repo_slug(&request.config),
                        "--body".to_string(),
                        body.clone(),
                    ],
                )
                .await?;
                Ok(RemoteProviderEffect::Comment)
            }
        }
    }
}

pub async fn execute_queued_project_syncs(
    pool: &SqlitePool,
    executor: &dyn RemoteProviderExecutor,
    project_id: i64,
    now: i64,
    limit: i64,
) -> Result<Vec<RemoteSyncExecution>> {
    let runs = remote::queued_sync_runs(pool, project_id, limit).await?;
    let mut out = Vec::with_capacity(runs.len());
    for run in runs {
        out.push(execute_sync_run(pool, executor, run.id, now).await?);
    }
    Ok(out)
}

pub async fn execute_sync_run(
    pool: &SqlitePool,
    executor: &dyn RemoteProviderExecutor,
    run_id: i64,
    now: i64,
) -> Result<RemoteSyncExecution> {
    if !remote::mark_sync_run_running(pool, run_id, now).await? {
        let run = remote::sync_run(pool, run_id)
            .await?
            .ok_or_else(|| anyhow!("remote sync run {run_id} not found"))?;
        return Ok(RemoteSyncExecution {
            run_id,
            status: run.status,
            remote_issue_link_id: run.remote_issue_link_id,
            remote_pr_link_id: run.remote_pr_link_id,
            error: run.error,
        });
    }

    let running = remote::sync_run(pool, run_id)
        .await?
        .ok_or_else(|| anyhow!("remote sync run {run_id} not found after claim"))?;
    let result = execute_claimed_sync_run(pool, executor, running.clone(), now).await;
    let terminal = match result {
        Ok(done) => done,
        Err(e) => RemoteSyncExecution {
            run_id,
            status: RemoteSyncStatus::Failed,
            remote_issue_link_id: None,
            remote_pr_link_id: None,
            error: Some(format!("{e:#}")),
        },
    };
    remote::finish_sync_run(
        pool,
        run_id,
        terminal.status,
        terminal.error.as_deref(),
        terminal.remote_issue_link_id,
        terminal.remote_pr_link_id,
        now,
    )
    .await?;
    Ok(terminal)
}

async fn execute_claimed_sync_run(
    pool: &SqlitePool,
    executor: &dyn RemoteProviderExecutor,
    run: RemoteSyncRun,
    now: i64,
) -> Result<RemoteSyncExecution> {
    if run.kind == RemoteSyncKind::Webhook {
        return Ok(skipped(run.id, "webhook runs are inbound"));
    }
    let Some(issue_id) = run.issue_id else {
        return Ok(skipped(run.id, "outbound sync run has no issue_id"));
    };
    let issue = issues::get(pool, issue_id)
        .await?
        .ok_or_else(|| anyhow!("issue {issue_id} not found"))?;
    let config = remote::get_config(pool, issue.project_id)
        .await?
        .ok_or_else(|| anyhow!("project {} has no remote config", issue.project_id))?;
    let issue_link = remote::issue_link_by_issue(pool, issue_id).await?;
    let pr_link = remote::pr_link_by_issue(pool, issue_id).await?;
    let notes = crate::remote_workflow::notes_presence(pool, &issue).await?;
    let plan = remote_plan::plan_issue_remote_workflow(RemoteWorkflowInput {
        config: Some(&config),
        issue: &issue,
        issue_link: issue_link.as_ref(),
        pr_link: pr_link.as_ref(),
        notes,
    });
    let Some(action) = matching_action(run.kind, &plan) else {
        return Ok(skipped(
            run.id,
            format!("queued action is stale or blocked: {:?}", plan.blockers),
        ));
    };

    let request = RemoteSyncRequest {
        run: run.clone(),
        config: config.clone(),
        issue: issue.clone(),
        issue_link: issue_link.clone(),
        pr_link: pr_link.clone(),
        action: action.clone(),
    };
    match executor.execute(request).await? {
        RemoteProviderEffect::Issue(created) => {
            let link_id = remote::upsert_issue_link(
                pool,
                UpsertRemoteIssueLink {
                    project_id: issue.project_id,
                    issue_id: Some(issue.id),
                    backlog_item_id: None,
                    provider: config.provider,
                    remote_owner: &config.owner,
                    remote_repo: &config.repo,
                    remote_issue_number: created.number,
                    remote_node_id: created.node_id.as_deref(),
                    remote_url: &created.url,
                    last_synced_at: Some(now),
                },
                now,
            )
            .await?;
            Ok(done(run.id, Some(link_id), None))
        }
        RemoteProviderEffect::PullRequest(created) => {
            let link_id = remote::upsert_pr_link(
                pool,
                UpsertRemotePrLink {
                    project_id: issue.project_id,
                    issue_id: issue.id,
                    provider: config.provider,
                    remote_owner: &config.owner,
                    remote_repo: &config.repo,
                    remote_pr_number: created.number,
                    remote_node_id: created.node_id.as_deref(),
                    remote_url: &created.url,
                    head_branch: &created.head_branch,
                    head_sha: created.head_sha.as_deref(),
                    base_branch: &created.base_branch,
                    base_sha: created.base_sha.as_deref(),
                    state: created.state,
                    last_synced_at: Some(now),
                },
                now,
            )
            .await?;
            Ok(done(run.id, None, Some(link_id)))
        }
        RemoteProviderEffect::Comment => Ok(done(run.id, None, None)),
    }
}

fn matching_action(
    kind: RemoteSyncKind,
    plan: &RemoteWorkflowPlan,
) -> Option<&RemotePlannedAction> {
    plan.actions.iter().find(|action| {
        matches!(
            (kind, *action),
            (
                RemoteSyncKind::Issue,
                RemotePlannedAction::CreateIssue { .. }
            ) | (
                RemoteSyncKind::Pr,
                RemotePlannedAction::CreateOrUpdatePullRequest { .. }
            ) | (
                RemoteSyncKind::Comment,
                RemotePlannedAction::PostProgressComment { .. }
            )
        )
    })
}

fn done(
    run_id: i64,
    remote_issue_link_id: Option<i64>,
    remote_pr_link_id: Option<i64>,
) -> RemoteSyncExecution {
    RemoteSyncExecution {
        run_id,
        status: RemoteSyncStatus::Done,
        remote_issue_link_id,
        remote_pr_link_id,
        error: None,
    }
}

fn skipped(run_id: i64, reason: impl Into<String>) -> RemoteSyncExecution {
    RemoteSyncExecution {
        run_id,
        status: RemoteSyncStatus::Skipped,
        remote_issue_link_id: None,
        remote_pr_link_id: None,
        error: Some(reason.into()),
    }
}

fn repo_slug(config: &ProjectRemoteConfig) -> String {
    format!("{}/{}", config.owner, config.repo)
}

fn gh_issue_create_args(
    config: &ProjectRemoteConfig,
    title: &str,
    body: &str,
    labels: &[String],
    assignees: &[String],
) -> Vec<String> {
    let mut args = vec![
        "issue".to_string(),
        "create".to_string(),
        "--repo".to_string(),
        repo_slug(config),
        "--title".to_string(),
        title.to_string(),
        "--body".to_string(),
        body.to_string(),
    ];
    for label in labels {
        args.extend(["--label".to_string(), label.to_string()]);
    }
    for assignee in assignees {
        args.extend(["--assignee".to_string(), assignee.to_string()]);
    }
    args
}

fn gh_pr_create_args(
    config: &ProjectRemoteConfig,
    title: &str,
    body: &str,
    head_branch: &str,
    base_branch: &str,
    draft: bool,
) -> Vec<String> {
    let mut args = vec![
        "pr".to_string(),
        "create".to_string(),
        "--repo".to_string(),
        repo_slug(config),
        "--title".to_string(),
        title.to_string(),
        "--body".to_string(),
        body.to_string(),
        "--head".to_string(),
        head_branch.to_string(),
        "--base".to_string(),
        base_branch.to_string(),
    ];
    if draft {
        args.push("--draft".to_string());
    }
    args
}

async fn run_gh(config: &ProjectRemoteConfig, args: Vec<String>) -> Result<String> {
    let mut command = Command::new("gh");
    command.args(args);
    if let Some((name, value)) = token_env(config)? {
        command.env(name, value);
    }
    let output = command
        .stdin(Stdio::null())
        .output()
        .await
        .context("running gh")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("gh failed: {}", stderr.trim());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn token_env(config: &ProjectRemoteConfig) -> Result<Option<(&'static str, String)>> {
    if config.auth_kind != remote::RemoteAuthKind::TokenEnv {
        return Ok(None);
    }
    let Some(name) = config.auth_ref.as_deref() else {
        return Ok(None);
    };
    let value = std::env::var(name).with_context(|| format!("reading token env {name}"))?;
    Ok(Some(("GH_TOKEN", value)))
}

fn parse_pr_view_json(text: &str) -> Result<CreatedRemotePr> {
    let value: Value = serde_json::from_str(text).context("parsing gh pr view JSON")?;
    let state = match value
        .get("state")
        .and_then(|v| v.as_str())
        .unwrap_or("OPEN")
    {
        "MERGED" => RemotePrState::Merged,
        "CLOSED" => RemotePrState::Closed,
        _ => RemotePrState::Open,
    };
    Ok(CreatedRemotePr {
        number: value
            .get("number")
            .and_then(|v| v.as_i64())
            .ok_or_else(|| anyhow!("gh pr view JSON missing number"))?,
        node_id: value
            .get("id")
            .and_then(|v| v.as_str())
            .map(ToOwned::to_owned),
        url: value
            .get("url")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("gh pr view JSON missing url"))?
            .to_string(),
        head_branch: value
            .get("headRefName")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        head_sha: value
            .get("headRefOid")
            .and_then(|v| v.as_str())
            .map(ToOwned::to_owned),
        base_branch: value
            .get("baseRefName")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        base_sha: value
            .get("baseRefOid")
            .and_then(|v| v.as_str())
            .map(ToOwned::to_owned),
        state,
    })
}

fn parse_github_number(url: &str, segment: &str) -> Result<i64> {
    let marker = format!("/{segment}/");
    let Some(after) = url.trim().rsplit_once(&marker).map(|(_, tail)| tail) else {
        bail!("could not parse GitHub {segment} URL: {url}");
    };
    let number = after
        .split(['/', '?', '#'])
        .next()
        .unwrap_or("")
        .parse::<i64>()
        .with_context(|| format!("parsing GitHub {segment} number from {url}"))?;
    Ok(number)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::projects::{self, NewProject};
    use crate::db::remote::{
        NewRemoteSyncRun, RemoteAuthKind, RemoteSyncDirection, RequiredChecksPolicy,
        UpsertProjectRemoteConfig,
    };
    use crate::state::IssueStatus;
    use std::sync::{Arc, Mutex};

    const TS: i64 = 1_000;

    #[derive(Default)]
    struct FakeRemoteExecutor {
        seen: Arc<Mutex<Vec<RemoteSyncKind>>>,
    }

    #[async_trait]
    impl RemoteProviderExecutor for FakeRemoteExecutor {
        async fn execute(&self, request: RemoteSyncRequest) -> Result<RemoteProviderEffect> {
            self.seen.lock().unwrap().push(request.run.kind);
            match request.action {
                RemotePlannedAction::CreateIssue { .. } => {
                    Ok(RemoteProviderEffect::Issue(CreatedRemoteIssue {
                        number: 77,
                        node_id: Some("node-77".to_string()),
                        url: "https://github.com/acme/app/issues/77".to_string(),
                    }))
                }
                RemotePlannedAction::CreateOrUpdatePullRequest {
                    head_branch,
                    base_branch,
                    ..
                } => Ok(RemoteProviderEffect::PullRequest(CreatedRemotePr {
                    number: 12,
                    node_id: None,
                    url: "https://github.com/acme/app/pull/12".to_string(),
                    head_branch,
                    head_sha: None,
                    base_branch,
                    base_sha: None,
                    state: RemotePrState::Open,
                })),
                RemotePlannedAction::PostProgressComment { .. } => {
                    Ok(RemoteProviderEffect::Comment)
                }
            }
        }
    }

    async fn project(db: &crate::db::Db) -> i64 {
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

    async fn config(db: &crate::db::Db, project_id: i64) {
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
                inbound_auwsx_run_enabled: true,
                outbound_issue_create_enabled: true,
                remote_pr_merge_enabled: true,
                agent_comment_sync_enabled: true,
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

    async fn queued_issue_run(db: &crate::db::Db, project_id: i64, issue_id: i64) -> i64 {
        remote::create_sync_run(
            db.pool(),
            NewRemoteSyncRun {
                project_id,
                issue_id: Some(issue_id),
                backlog_item_id: None,
                remote_issue_link_id: None,
                remote_pr_link_id: None,
                direction: RemoteSyncDirection::Outbound,
                kind: RemoteSyncKind::Issue,
                status: RemoteSyncStatus::Queued,
                summary: Some("issue"),
                error: None,
                started_at: None,
                ended_at: None,
            },
            TS,
        )
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn given_queued_issue_sync_when_executed_then_link_is_recorded_and_run_done() {
        let db = crate::db::Db::open_memory().await.unwrap();
        let project_id = project(&db).await;
        config(&db, project_id).await;
        let issue_id = issues::create(db.pool(), project_id, "remote issue", None, TS)
            .await
            .unwrap();
        let run_id = queued_issue_run(&db, project_id, issue_id).await;
        let executor = FakeRemoteExecutor::default();

        let execution = execute_sync_run(db.pool(), &executor, run_id, TS + 1)
            .await
            .unwrap();

        let run = remote::sync_run(db.pool(), run_id)
            .await
            .unwrap()
            .expect("run exists");
        let link = remote::issue_link_by_issue(db.pool(), issue_id)
            .await
            .unwrap()
            .expect("link exists");
        assert_eq!(execution.status, RemoteSyncStatus::Done);
        assert_eq!(run.status, RemoteSyncStatus::Done);
        assert_eq!(run.remote_issue_link_id, Some(link.id));
        assert_eq!(link.remote_issue_number, 77);
    }

    #[tokio::test]
    async fn given_stale_queued_issue_sync_when_link_exists_then_run_is_skipped() {
        let db = crate::db::Db::open_memory().await.unwrap();
        let project_id = project(&db).await;
        config(&db, project_id).await;
        let issue_id = issues::create(db.pool(), project_id, "remote issue", None, TS)
            .await
            .unwrap();
        let run_id = queued_issue_run(&db, project_id, issue_id).await;
        remote::upsert_issue_link(
            db.pool(),
            UpsertRemoteIssueLink {
                project_id,
                issue_id: Some(issue_id),
                backlog_item_id: None,
                provider: RemoteProvider::Github,
                remote_owner: "acme",
                remote_repo: "app",
                remote_issue_number: 9,
                remote_node_id: None,
                remote_url: "https://github.com/acme/app/issues/9",
                last_synced_at: Some(TS),
            },
            TS,
        )
        .await
        .unwrap();
        let executor = FakeRemoteExecutor::default();

        let execution = execute_sync_run(db.pool(), &executor, run_id, TS + 1)
            .await
            .unwrap();

        assert_eq!(execution.status, RemoteSyncStatus::Skipped);
        assert!(executor.seen.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn given_queued_pr_sync_when_executed_then_pr_link_is_recorded() {
        let db = crate::db::Db::open_memory().await.unwrap();
        let project_id = project(&db).await;
        config(&db, project_id).await;
        let issue_id = issues::create(db.pool(), project_id, "remote pr", None, TS)
            .await
            .unwrap();
        issues::set_worktree(
            db.pool(),
            issue_id,
            Some("auwsx/issue-1"),
            Some("/worktree"),
            None,
            TS,
        )
        .await
        .unwrap();
        issues::force_status(db.pool(), issue_id, IssueStatus::ReadyToMerge, TS)
            .await
            .unwrap();
        let run_id = remote::create_sync_run(
            db.pool(),
            NewRemoteSyncRun {
                project_id,
                issue_id: Some(issue_id),
                backlog_item_id: None,
                remote_issue_link_id: None,
                remote_pr_link_id: None,
                direction: RemoteSyncDirection::Outbound,
                kind: RemoteSyncKind::Pr,
                status: RemoteSyncStatus::Queued,
                summary: Some("pr"),
                error: None,
                started_at: None,
                ended_at: None,
            },
            TS,
        )
        .await
        .unwrap();
        let executor = FakeRemoteExecutor::default();

        let execution = execute_sync_run(db.pool(), &executor, run_id, TS + 1)
            .await
            .unwrap();

        let link = remote::pr_link_by_issue(db.pool(), issue_id)
            .await
            .unwrap()
            .expect("pr link exists");
        assert_eq!(execution.status, RemoteSyncStatus::Done);
        assert_eq!(link.remote_pr_number, 12);
        assert_eq!(link.head_branch, "auwsx/issue-1");
        assert_eq!(link.base_branch, "main");
    }

    #[test]
    fn given_github_url_when_parse_number_then_extracts_issue_or_pr_number() {
        assert_eq!(
            parse_github_number("https://github.com/acme/app/issues/77", "issues").unwrap(),
            77
        );
        assert_eq!(
            parse_github_number("https://github.com/acme/app/pull/12", "pull").unwrap(),
            12
        );
    }
}

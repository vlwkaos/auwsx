//! Remote repository typed rows + CRUD. Schema: src/db/migrations/0020_remote_git.sql.

use crate::Result;
use anyhow::{anyhow, bail};
use serde::{Deserialize, Serialize};
use sqlx::sqlite::SqliteRow;
use sqlx::{Row, SqlitePool};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteProvider {
    Github,
}

impl RemoteProvider {
    pub fn as_str(self) -> &'static str {
        match self {
            RemoteProvider::Github => "github",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        Some(match s {
            "github" => RemoteProvider::Github,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteAuthKind {
    None,
    TokenEnv,
    GithubApp,
}

impl RemoteAuthKind {
    pub fn as_str(self) -> &'static str {
        match self {
            RemoteAuthKind::None => "none",
            RemoteAuthKind::TokenEnv => "token_env",
            RemoteAuthKind::GithubApp => "github_app",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        Some(match s {
            "none" => RemoteAuthKind::None,
            "token_env" => RemoteAuthKind::TokenEnv,
            "github_app" => RemoteAuthKind::GithubApp,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequiredChecksPolicy {
    Observe,
    RequireGreen,
}

impl RequiredChecksPolicy {
    pub fn as_str(self) -> &'static str {
        match self {
            RequiredChecksPolicy::Observe => "observe",
            RequiredChecksPolicy::RequireGreen => "require_green",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        Some(match s {
            "observe" => RequiredChecksPolicy::Observe,
            "require_green" => RequiredChecksPolicy::RequireGreen,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemotePrState {
    Open,
    Closed,
    Merged,
}

impl RemotePrState {
    pub fn as_str(self) -> &'static str {
        match self {
            RemotePrState::Open => "open",
            RemotePrState::Closed => "closed",
            RemotePrState::Merged => "merged",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        Some(match s {
            "open" => RemotePrState::Open,
            "closed" => RemotePrState::Closed,
            "merged" => RemotePrState::Merged,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteEventStatus {
    Received,
    Processed,
    Ignored,
    Failed,
}

impl RemoteEventStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            RemoteEventStatus::Received => "received",
            RemoteEventStatus::Processed => "processed",
            RemoteEventStatus::Ignored => "ignored",
            RemoteEventStatus::Failed => "failed",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        Some(match s {
            "received" => RemoteEventStatus::Received,
            "processed" => RemoteEventStatus::Processed,
            "ignored" => RemoteEventStatus::Ignored,
            "failed" => RemoteEventStatus::Failed,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteSyncDirection {
    Inbound,
    Outbound,
}

impl RemoteSyncDirection {
    pub fn as_str(self) -> &'static str {
        match self {
            RemoteSyncDirection::Inbound => "inbound",
            RemoteSyncDirection::Outbound => "outbound",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        Some(match s {
            "inbound" => RemoteSyncDirection::Inbound,
            "outbound" => RemoteSyncDirection::Outbound,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteSyncKind {
    Webhook,
    Issue,
    Comment,
    Pr,
}

impl RemoteSyncKind {
    pub fn as_str(self) -> &'static str {
        match self {
            RemoteSyncKind::Webhook => "webhook",
            RemoteSyncKind::Issue => "issue",
            RemoteSyncKind::Comment => "comment",
            RemoteSyncKind::Pr => "pr",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        Some(match s {
            "webhook" => RemoteSyncKind::Webhook,
            "issue" => RemoteSyncKind::Issue,
            "comment" => RemoteSyncKind::Comment,
            "pr" => RemoteSyncKind::Pr,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteSyncStatus {
    Queued,
    Running,
    Done,
    Failed,
    Skipped,
}

impl RemoteSyncStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            RemoteSyncStatus::Queued => "queued",
            RemoteSyncStatus::Running => "running",
            RemoteSyncStatus::Done => "done",
            RemoteSyncStatus::Failed => "failed",
            RemoteSyncStatus::Skipped => "skipped",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        Some(match s {
            "queued" => RemoteSyncStatus::Queued,
            "running" => RemoteSyncStatus::Running,
            "done" => RemoteSyncStatus::Done,
            "failed" => RemoteSyncStatus::Failed,
            "skipped" => RemoteSyncStatus::Skipped,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectRemoteConfig {
    pub project_id: i64,
    pub provider: RemoteProvider,
    pub remote_url: String,
    pub owner: String,
    pub repo: String,
    pub api_base_url: String,
    pub auth_kind: RemoteAuthKind,
    pub auth_ref: Option<String>,
    pub webhook_secret_ref: Option<String>,
    pub inbound_auwsx_run_enabled: bool,
    pub outbound_issue_create_enabled: bool,
    pub remote_pr_merge_enabled: bool,
    pub agent_comment_sync_enabled: bool,
    pub subtask_comment_sync_enabled: bool,
    pub finding_comment_sync_enabled: bool,
    pub draft_pr_enabled: bool,
    pub required_checks_policy: RequiredChecksPolicy,
    pub default_labels: Option<String>,
    pub default_assignees: Option<String>,
    pub pr_base_branch: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone)]
pub struct UpsertProjectRemoteConfig<'a> {
    pub project_id: i64,
    pub provider: RemoteProvider,
    pub remote_url: &'a str,
    pub owner: &'a str,
    pub repo: &'a str,
    pub api_base_url: &'a str,
    pub auth_kind: RemoteAuthKind,
    pub auth_ref: Option<&'a str>,
    pub webhook_secret_ref: Option<&'a str>,
    pub inbound_auwsx_run_enabled: bool,
    pub outbound_issue_create_enabled: bool,
    pub remote_pr_merge_enabled: bool,
    pub agent_comment_sync_enabled: bool,
    pub subtask_comment_sync_enabled: bool,
    pub finding_comment_sync_enabled: bool,
    pub draft_pr_enabled: bool,
    pub required_checks_policy: RequiredChecksPolicy,
    pub default_labels: Option<&'a str>,
    pub default_assignees: Option<&'a str>,
    pub pr_base_branch: Option<&'a str>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteIssueLink {
    pub id: i64,
    pub project_id: i64,
    pub issue_id: Option<i64>,
    pub backlog_item_id: Option<i64>,
    pub provider: RemoteProvider,
    pub remote_owner: String,
    pub remote_repo: String,
    pub remote_issue_number: i64,
    pub remote_node_id: Option<String>,
    pub remote_url: String,
    pub last_synced_at: Option<i64>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone)]
pub struct UpsertRemoteIssueLink<'a> {
    pub project_id: i64,
    pub issue_id: Option<i64>,
    pub backlog_item_id: Option<i64>,
    pub provider: RemoteProvider,
    pub remote_owner: &'a str,
    pub remote_repo: &'a str,
    pub remote_issue_number: i64,
    pub remote_node_id: Option<&'a str>,
    pub remote_url: &'a str,
    pub last_synced_at: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemotePrLink {
    pub id: i64,
    pub project_id: i64,
    pub issue_id: i64,
    pub provider: RemoteProvider,
    pub remote_owner: String,
    pub remote_repo: String,
    pub remote_pr_number: i64,
    pub remote_node_id: Option<String>,
    pub remote_url: String,
    pub head_branch: String,
    pub head_sha: Option<String>,
    pub base_branch: String,
    pub base_sha: Option<String>,
    pub state: RemotePrState,
    pub last_synced_at: Option<i64>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone)]
pub struct UpsertRemotePrLink<'a> {
    pub project_id: i64,
    pub issue_id: i64,
    pub provider: RemoteProvider,
    pub remote_owner: &'a str,
    pub remote_repo: &'a str,
    pub remote_pr_number: i64,
    pub remote_node_id: Option<&'a str>,
    pub remote_url: &'a str,
    pub head_branch: &'a str,
    pub head_sha: Option<&'a str>,
    pub base_branch: &'a str,
    pub base_sha: Option<&'a str>,
    pub state: RemotePrState,
    pub last_synced_at: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteEvent {
    pub id: i64,
    pub project_id: Option<i64>,
    pub provider: RemoteProvider,
    pub delivery_id: String,
    pub event_kind: String,
    pub action: Option<String>,
    pub payload_hash: String,
    pub status: RemoteEventStatus,
    pub error: Option<String>,
    pub received_at: i64,
    pub processed_at: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct RecordRemoteEvent<'a> {
    pub project_id: Option<i64>,
    pub provider: RemoteProvider,
    pub delivery_id: &'a str,
    pub event_kind: &'a str,
    pub action: Option<&'a str>,
    pub payload_hash: &'a str,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteSyncRun {
    pub id: i64,
    pub project_id: i64,
    pub issue_id: Option<i64>,
    pub backlog_item_id: Option<i64>,
    pub remote_issue_link_id: Option<i64>,
    pub remote_pr_link_id: Option<i64>,
    pub direction: RemoteSyncDirection,
    pub kind: RemoteSyncKind,
    pub status: RemoteSyncStatus,
    pub summary: Option<String>,
    pub error: Option<String>,
    pub started_at: Option<i64>,
    pub ended_at: Option<i64>,
    pub created_at: i64,
}

#[derive(Debug, Clone)]
pub struct NewRemoteSyncRun<'a> {
    pub project_id: i64,
    pub issue_id: Option<i64>,
    pub backlog_item_id: Option<i64>,
    pub remote_issue_link_id: Option<i64>,
    pub remote_pr_link_id: Option<i64>,
    pub direction: RemoteSyncDirection,
    pub kind: RemoteSyncKind,
    pub status: RemoteSyncStatus,
    pub summary: Option<&'a str>,
    pub error: Option<&'a str>,
    pub started_at: Option<i64>,
    pub ended_at: Option<i64>,
}

pub async fn get_config(pool: &SqlitePool, project_id: i64) -> Result<Option<ProjectRemoteConfig>> {
    let row = sqlx::query("SELECT * FROM project_remote_configs WHERE project_id = ?")
        .bind(project_id)
        .fetch_optional(pool)
        .await?;
    row.map(|row| config_from_row(&row)).transpose()
}

pub async fn upsert_config(
    pool: &SqlitePool,
    input: UpsertProjectRemoteConfig<'_>,
    now: i64,
) -> Result<()> {
    validate_config(&input)?;
    sqlx::query(
        "INSERT INTO project_remote_configs
            (project_id, provider, remote_url, owner, repo, api_base_url, auth_kind, auth_ref,
             webhook_secret_ref, inbound_auwsx_run_enabled, outbound_issue_create_enabled,
             remote_pr_merge_enabled, agent_comment_sync_enabled, subtask_comment_sync_enabled,
             finding_comment_sync_enabled, draft_pr_enabled, required_checks_policy,
             default_labels, default_assignees, pr_base_branch, created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT(project_id) DO UPDATE SET
            provider = excluded.provider,
            remote_url = excluded.remote_url,
            owner = excluded.owner,
            repo = excluded.repo,
            api_base_url = excluded.api_base_url,
            auth_kind = excluded.auth_kind,
            auth_ref = excluded.auth_ref,
            webhook_secret_ref = excluded.webhook_secret_ref,
            inbound_auwsx_run_enabled = excluded.inbound_auwsx_run_enabled,
            outbound_issue_create_enabled = excluded.outbound_issue_create_enabled,
            remote_pr_merge_enabled = excluded.remote_pr_merge_enabled,
            agent_comment_sync_enabled = excluded.agent_comment_sync_enabled,
            subtask_comment_sync_enabled = excluded.subtask_comment_sync_enabled,
            finding_comment_sync_enabled = excluded.finding_comment_sync_enabled,
            draft_pr_enabled = excluded.draft_pr_enabled,
            required_checks_policy = excluded.required_checks_policy,
            default_labels = excluded.default_labels,
            default_assignees = excluded.default_assignees,
            pr_base_branch = excluded.pr_base_branch,
            updated_at = excluded.updated_at",
    )
    .bind(input.project_id)
    .bind(input.provider.as_str())
    .bind(input.remote_url.trim())
    .bind(input.owner.trim())
    .bind(input.repo.trim())
    .bind(input.api_base_url.trim())
    .bind(input.auth_kind.as_str())
    .bind(trimmed_opt(input.auth_ref))
    .bind(trimmed_opt(input.webhook_secret_ref))
    .bind(bool_int(input.inbound_auwsx_run_enabled))
    .bind(bool_int(input.outbound_issue_create_enabled))
    .bind(bool_int(input.remote_pr_merge_enabled))
    .bind(bool_int(input.agent_comment_sync_enabled))
    .bind(bool_int(input.subtask_comment_sync_enabled))
    .bind(bool_int(input.finding_comment_sync_enabled))
    .bind(bool_int(input.draft_pr_enabled))
    .bind(input.required_checks_policy.as_str())
    .bind(trimmed_opt(input.default_labels))
    .bind(trimmed_opt(input.default_assignees))
    .bind(trimmed_opt(input.pr_base_branch))
    .bind(now)
    .bind(now)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn delete_config(pool: &SqlitePool, project_id: i64) -> Result<bool> {
    let changed = sqlx::query("DELETE FROM project_remote_configs WHERE project_id = ?")
        .bind(project_id)
        .execute(pool)
        .await?
        .rows_affected();
    Ok(changed > 0)
}

pub async fn upsert_issue_link(
    pool: &SqlitePool,
    input: UpsertRemoteIssueLink<'_>,
    now: i64,
) -> Result<i64> {
    if input.issue_id.is_none() && input.backlog_item_id.is_none() {
        bail!("remote issue link requires issue_id or backlog_item_id");
    }
    let id: i64 = sqlx::query(
        "INSERT INTO remote_issue_links
            (project_id, issue_id, backlog_item_id, provider, remote_owner, remote_repo,
             remote_issue_number, remote_node_id, remote_url, last_synced_at, created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT(provider, remote_owner, remote_repo, remote_issue_number) DO UPDATE SET
            project_id = excluded.project_id,
            issue_id = excluded.issue_id,
            backlog_item_id = excluded.backlog_item_id,
            remote_node_id = excluded.remote_node_id,
            remote_url = excluded.remote_url,
            last_synced_at = excluded.last_synced_at,
            updated_at = excluded.updated_at
         RETURNING id",
    )
    .bind(input.project_id)
    .bind(input.issue_id)
    .bind(input.backlog_item_id)
    .bind(input.provider.as_str())
    .bind(input.remote_owner)
    .bind(input.remote_repo)
    .bind(input.remote_issue_number)
    .bind(input.remote_node_id)
    .bind(input.remote_url)
    .bind(input.last_synced_at)
    .bind(now)
    .bind(now)
    .fetch_one(pool)
    .await?
    .get("id");
    Ok(id)
}

pub async fn issue_link_by_issue(
    pool: &SqlitePool,
    issue_id: i64,
) -> Result<Option<RemoteIssueLink>> {
    let row = sqlx::query("SELECT * FROM remote_issue_links WHERE issue_id = ?")
        .bind(issue_id)
        .fetch_optional(pool)
        .await?;
    row.map(|row| issue_link_from_row(&row)).transpose()
}

pub async fn upsert_pr_link(
    pool: &SqlitePool,
    input: UpsertRemotePrLink<'_>,
    now: i64,
) -> Result<i64> {
    let id: i64 = sqlx::query(
        "INSERT INTO remote_pr_links
            (project_id, issue_id, provider, remote_owner, remote_repo, remote_pr_number,
             remote_node_id, remote_url, head_branch, head_sha, base_branch, base_sha, state,
             last_synced_at, created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT(issue_id) DO UPDATE SET
            provider = excluded.provider,
            remote_owner = excluded.remote_owner,
            remote_repo = excluded.remote_repo,
            remote_pr_number = excluded.remote_pr_number,
            remote_node_id = excluded.remote_node_id,
            remote_url = excluded.remote_url,
            head_branch = excluded.head_branch,
            head_sha = excluded.head_sha,
            base_branch = excluded.base_branch,
            base_sha = excluded.base_sha,
            state = excluded.state,
            last_synced_at = excluded.last_synced_at,
            updated_at = excluded.updated_at
         RETURNING id",
    )
    .bind(input.project_id)
    .bind(input.issue_id)
    .bind(input.provider.as_str())
    .bind(input.remote_owner)
    .bind(input.remote_repo)
    .bind(input.remote_pr_number)
    .bind(input.remote_node_id)
    .bind(input.remote_url)
    .bind(input.head_branch)
    .bind(input.head_sha)
    .bind(input.base_branch)
    .bind(input.base_sha)
    .bind(input.state.as_str())
    .bind(input.last_synced_at)
    .bind(now)
    .bind(now)
    .fetch_one(pool)
    .await?
    .get("id");
    Ok(id)
}

pub async fn pr_link_by_issue(pool: &SqlitePool, issue_id: i64) -> Result<Option<RemotePrLink>> {
    let row = sqlx::query("SELECT * FROM remote_pr_links WHERE issue_id = ?")
        .bind(issue_id)
        .fetch_optional(pool)
        .await?;
    row.map(|row| pr_link_from_row(&row)).transpose()
}

pub async fn record_event(
    pool: &SqlitePool,
    input: RecordRemoteEvent<'_>,
    now: i64,
) -> Result<Option<i64>> {
    let result = sqlx::query(
        "INSERT OR IGNORE INTO remote_events
            (project_id, provider, delivery_id, event_kind, action, payload_hash, status, received_at)
         VALUES (?, ?, ?, ?, ?, ?, 'received', ?)",
    )
    .bind(input.project_id)
    .bind(input.provider.as_str())
    .bind(input.delivery_id)
    .bind(input.event_kind)
    .bind(input.action)
    .bind(input.payload_hash)
    .bind(now)
    .execute(pool)
    .await?;
    if result.rows_affected() == 0 {
        return Ok(None);
    }
    let id: i64 = sqlx::query("SELECT last_insert_rowid() AS id")
        .fetch_one(pool)
        .await?
        .get("id");
    Ok(Some(id))
}

pub async fn update_event_status(
    pool: &SqlitePool,
    event_id: i64,
    status: RemoteEventStatus,
    error: Option<&str>,
    now: i64,
) -> Result<()> {
    sqlx::query(
        "UPDATE remote_events
         SET status = ?, error = ?, processed_at = ?
         WHERE id = ?",
    )
    .bind(status.as_str())
    .bind(trimmed_opt(error))
    .bind(now)
    .bind(event_id)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn event_by_delivery(
    pool: &SqlitePool,
    provider: RemoteProvider,
    delivery_id: &str,
) -> Result<Option<RemoteEvent>> {
    let row = sqlx::query("SELECT * FROM remote_events WHERE provider = ? AND delivery_id = ?")
        .bind(provider.as_str())
        .bind(delivery_id)
        .fetch_optional(pool)
        .await?;
    row.map(|row| event_from_row(&row)).transpose()
}

pub async fn create_sync_run(
    pool: &SqlitePool,
    input: NewRemoteSyncRun<'_>,
    now: i64,
) -> Result<i64> {
    let id: i64 = sqlx::query(
        "INSERT INTO remote_sync_runs
            (project_id, issue_id, backlog_item_id, remote_issue_link_id, remote_pr_link_id,
             direction, kind, status, summary, error, started_at, ended_at, created_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
         RETURNING id",
    )
    .bind(input.project_id)
    .bind(input.issue_id)
    .bind(input.backlog_item_id)
    .bind(input.remote_issue_link_id)
    .bind(input.remote_pr_link_id)
    .bind(input.direction.as_str())
    .bind(input.kind.as_str())
    .bind(input.status.as_str())
    .bind(trimmed_opt(input.summary))
    .bind(trimmed_opt(input.error))
    .bind(input.started_at)
    .bind(input.ended_at)
    .bind(now)
    .fetch_one(pool)
    .await?
    .get("id");
    Ok(id)
}

pub async fn recent_sync_runs(
    pool: &SqlitePool,
    project_id: i64,
    limit: i64,
) -> Result<Vec<RemoteSyncRun>> {
    let rows = sqlx::query(
        "SELECT * FROM remote_sync_runs
         WHERE project_id = ?
         ORDER BY id DESC
         LIMIT ?",
    )
    .bind(project_id)
    .bind(limit.clamp(1, 200))
    .fetch_all(pool)
    .await?;
    rows.iter().map(sync_run_from_row).collect()
}

fn validate_config(input: &UpsertProjectRemoteConfig<'_>) -> Result<()> {
    if input.remote_url.trim().is_empty() {
        bail!("remote_url is required");
    }
    if input.owner.trim().is_empty() {
        bail!("remote owner is required");
    }
    if input.repo.trim().is_empty() {
        bail!("remote repo is required");
    }
    if input.api_base_url.trim().is_empty() {
        bail!("api_base_url is required");
    }
    if input.auth_kind != RemoteAuthKind::None && trimmed_opt(input.auth_ref).is_none() {
        bail!("auth_ref is required unless auth_kind is none");
    }
    Ok(())
}

fn config_from_row(row: &SqliteRow) -> Result<ProjectRemoteConfig> {
    let provider: String = row.try_get("provider")?;
    let auth_kind: String = row.try_get("auth_kind")?;
    let checks: String = row.try_get("required_checks_policy")?;
    Ok(ProjectRemoteConfig {
        project_id: row.try_get("project_id")?,
        provider: RemoteProvider::from_str(&provider)
            .ok_or_else(|| anyhow!("unknown remote provider {provider:?}"))?,
        remote_url: row.try_get("remote_url")?,
        owner: row.try_get("owner")?,
        repo: row.try_get("repo")?,
        api_base_url: row.try_get("api_base_url")?,
        auth_kind: RemoteAuthKind::from_str(&auth_kind)
            .ok_or_else(|| anyhow!("unknown remote auth kind {auth_kind:?}"))?,
        auth_ref: row.try_get("auth_ref")?,
        webhook_secret_ref: row.try_get("webhook_secret_ref")?,
        inbound_auwsx_run_enabled: int_bool(row.try_get("inbound_auwsx_run_enabled")?)?,
        outbound_issue_create_enabled: int_bool(row.try_get("outbound_issue_create_enabled")?)?,
        remote_pr_merge_enabled: int_bool(row.try_get("remote_pr_merge_enabled")?)?,
        agent_comment_sync_enabled: int_bool(row.try_get("agent_comment_sync_enabled")?)?,
        subtask_comment_sync_enabled: int_bool(row.try_get("subtask_comment_sync_enabled")?)?,
        finding_comment_sync_enabled: int_bool(row.try_get("finding_comment_sync_enabled")?)?,
        draft_pr_enabled: int_bool(row.try_get("draft_pr_enabled")?)?,
        required_checks_policy: RequiredChecksPolicy::from_str(&checks)
            .ok_or_else(|| anyhow!("unknown required checks policy {checks:?}"))?,
        default_labels: row.try_get("default_labels")?,
        default_assignees: row.try_get("default_assignees")?,
        pr_base_branch: row.try_get("pr_base_branch")?,
        created_at: row.try_get("created_at")?,
        updated_at: row.try_get("updated_at")?,
    })
}

fn issue_link_from_row(row: &SqliteRow) -> Result<RemoteIssueLink> {
    let provider: String = row.try_get("provider")?;
    Ok(RemoteIssueLink {
        id: row.try_get("id")?,
        project_id: row.try_get("project_id")?,
        issue_id: row.try_get("issue_id")?,
        backlog_item_id: row.try_get("backlog_item_id")?,
        provider: RemoteProvider::from_str(&provider)
            .ok_or_else(|| anyhow!("unknown remote provider {provider:?}"))?,
        remote_owner: row.try_get("remote_owner")?,
        remote_repo: row.try_get("remote_repo")?,
        remote_issue_number: row.try_get("remote_issue_number")?,
        remote_node_id: row.try_get("remote_node_id")?,
        remote_url: row.try_get("remote_url")?,
        last_synced_at: row.try_get("last_synced_at")?,
        created_at: row.try_get("created_at")?,
        updated_at: row.try_get("updated_at")?,
    })
}

fn pr_link_from_row(row: &SqliteRow) -> Result<RemotePrLink> {
    let provider: String = row.try_get("provider")?;
    let state: String = row.try_get("state")?;
    Ok(RemotePrLink {
        id: row.try_get("id")?,
        project_id: row.try_get("project_id")?,
        issue_id: row.try_get("issue_id")?,
        provider: RemoteProvider::from_str(&provider)
            .ok_or_else(|| anyhow!("unknown remote provider {provider:?}"))?,
        remote_owner: row.try_get("remote_owner")?,
        remote_repo: row.try_get("remote_repo")?,
        remote_pr_number: row.try_get("remote_pr_number")?,
        remote_node_id: row.try_get("remote_node_id")?,
        remote_url: row.try_get("remote_url")?,
        head_branch: row.try_get("head_branch")?,
        head_sha: row.try_get("head_sha")?,
        base_branch: row.try_get("base_branch")?,
        base_sha: row.try_get("base_sha")?,
        state: RemotePrState::from_str(&state)
            .ok_or_else(|| anyhow!("unknown remote PR state {state:?}"))?,
        last_synced_at: row.try_get("last_synced_at")?,
        created_at: row.try_get("created_at")?,
        updated_at: row.try_get("updated_at")?,
    })
}

fn event_from_row(row: &SqliteRow) -> Result<RemoteEvent> {
    let provider: String = row.try_get("provider")?;
    let status: String = row.try_get("status")?;
    Ok(RemoteEvent {
        id: row.try_get("id")?,
        project_id: row.try_get("project_id")?,
        provider: RemoteProvider::from_str(&provider)
            .ok_or_else(|| anyhow!("unknown remote provider {provider:?}"))?,
        delivery_id: row.try_get("delivery_id")?,
        event_kind: row.try_get("event_kind")?,
        action: row.try_get("action")?,
        payload_hash: row.try_get("payload_hash")?,
        status: RemoteEventStatus::from_str(&status)
            .ok_or_else(|| anyhow!("unknown remote event status {status:?}"))?,
        error: row.try_get("error")?,
        received_at: row.try_get("received_at")?,
        processed_at: row.try_get("processed_at")?,
    })
}

fn sync_run_from_row(row: &SqliteRow) -> Result<RemoteSyncRun> {
    let direction: String = row.try_get("direction")?;
    let kind: String = row.try_get("kind")?;
    let status: String = row.try_get("status")?;
    Ok(RemoteSyncRun {
        id: row.try_get("id")?,
        project_id: row.try_get("project_id")?,
        issue_id: row.try_get("issue_id")?,
        backlog_item_id: row.try_get("backlog_item_id")?,
        remote_issue_link_id: row.try_get("remote_issue_link_id")?,
        remote_pr_link_id: row.try_get("remote_pr_link_id")?,
        direction: RemoteSyncDirection::from_str(&direction)
            .ok_or_else(|| anyhow!("unknown remote sync direction {direction:?}"))?,
        kind: RemoteSyncKind::from_str(&kind)
            .ok_or_else(|| anyhow!("unknown remote sync kind {kind:?}"))?,
        status: RemoteSyncStatus::from_str(&status)
            .ok_or_else(|| anyhow!("unknown remote sync status {status:?}"))?,
        summary: row.try_get("summary")?,
        error: row.try_get("error")?,
        started_at: row.try_get("started_at")?,
        ended_at: row.try_get("ended_at")?,
        created_at: row.try_get("created_at")?,
    })
}

fn bool_int(value: bool) -> i64 {
    if value {
        1
    } else {
        0
    }
}

fn int_bool(value: i64) -> Result<bool> {
    match value {
        0 => Ok(false),
        1 => Ok(true),
        other => Err(anyhow!("expected sqlite boolean 0 or 1, got {other}")),
    }
}

fn trimmed_opt(value: Option<&str>) -> Option<&str> {
    value.and_then(|s| {
        let trimmed = s.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed)
        }
    })
}

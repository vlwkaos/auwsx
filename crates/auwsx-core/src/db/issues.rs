//! `issues` typed row + CRUD. Schema: src/db/migrations/0001_init.sql.
//!
//! The issue is the pipeline unit and its `status` is the scheduler's sync
//! marker (see `state::IssueStatus`). Autonomous status changes go through
//! [`transition`] (legality enforced by `state::check_transition`); a human
//! override uses [`force_status`], which deliberately bypasses the matrix and is
//! expected to be logged to `agent_runs` by the caller.
//!
//! Every mutating op stamps `updated_at = now` (caller-supplied clock).

use crate::state::{self, IssueStatus};
use crate::Result;
use anyhow::anyhow;
use serde::{Deserialize, Serialize};
use sqlx::sqlite::SqliteRow;
use sqlx::{Row, SqlitePool};

/// Status a freshly-created standalone issue enters.
pub const INITIAL_STATUS: IssueStatus = IssueStatus::New;

/// Full `issues` row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Issue {
    pub id: i64,
    pub project_id: i64,
    pub title: String,
    pub description: Option<String>,
    pub agent_summary: Option<String>,
    pub progress_report: Option<String>,
    pub result_report: Option<String>,
    pub status: IssueStatus,
    pub branch: Option<String>,
    pub worktree_path: Option<String>,
    pub review_round: i64,
    pub conflict_attempts: i64,
    pub wait_until: Option<i64>,
    pub absorbed_into_id: Option<i64>,
    pub has_pending_steering: bool,
    pub created_at: i64,
    pub updated_at: i64,
}

impl Issue {
    fn from_row(row: &SqliteRow) -> Result<Self> {
        let status_raw: String = row.try_get("status")?;
        let pending: i64 = row.try_get("has_pending_steering")?;
        Ok(Issue {
            id: row.try_get("id")?,
            project_id: row.try_get("project_id")?,
            title: row.try_get("title")?,
            description: row.try_get("description")?,
            agent_summary: row.try_get("agent_summary")?,
            progress_report: row.try_get("progress_report")?,
            result_report: row.try_get("result_report")?,
            status: IssueStatus::from_str(&status_raw)
                .ok_or_else(|| anyhow!("unknown issue status {status_raw:?} in db"))?,
            branch: row.try_get("branch")?,
            worktree_path: row.try_get("worktree_path")?,
            review_round: row.try_get("review_round")?,
            conflict_attempts: row.try_get("conflict_attempts")?,
            wait_until: row.try_get("wait_until")?,
            absorbed_into_id: row.try_get("absorbed_into_id")?,
            has_pending_steering: pending != 0,
            created_at: row.try_get("created_at")?,
            updated_at: row.try_get("updated_at")?,
        })
    }
}

/// Create an issue at [`INITIAL_STATUS`]. Returns the new id.
pub async fn create(
    pool: &SqlitePool,
    project_id: i64,
    title: &str,
    description: Option<&str>,
    now: i64,
) -> Result<i64> {
    let id: i64 = sqlx::query(
        "INSERT INTO issues (project_id, title, description, status, created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, ?)
         RETURNING id",
    )
    .bind(project_id)
    .bind(title)
    .bind(description)
    .bind(INITIAL_STATUS.as_str())
    .bind(now)
    .bind(now)
    .fetch_one(pool)
    .await?
    .get("id");
    Ok(id)
}

pub async fn get(pool: &SqlitePool, id: i64) -> Result<Option<Issue>> {
    let row = sqlx::query("SELECT * FROM issues WHERE id = ?")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    row.as_ref().map(Issue::from_row).transpose()
}

pub async fn update_reports(
    pool: &SqlitePool,
    id: i64,
    agent_summary: Option<&str>,
    progress_report: Option<&str>,
    result_report: Option<&str>,
    now: i64,
) -> Result<()> {
    let n = sqlx::query(
        "UPDATE issues
         SET agent_summary = COALESCE(?, agent_summary),
             progress_report = COALESCE(?, progress_report),
             result_report = COALESCE(?, result_report),
             updated_at = ?
         WHERE id = ?",
    )
    .bind(agent_summary)
    .bind(progress_report)
    .bind(result_report)
    .bind(now)
    .bind(id)
    .execute(pool)
    .await?
    .rows_affected();
    ensure_found(n, id)
}

/// All issues for a project, newest first.
pub async fn list_by_project(pool: &SqlitePool, project_id: i64) -> Result<Vec<Issue>> {
    let rows = sqlx::query("SELECT * FROM issues WHERE project_id = ? ORDER BY id DESC")
        .bind(project_id)
        .fetch_all(pool)
        .await?;
    rows.iter().map(Issue::from_row).collect()
}

/// Issues for a project in a specific status (the scheduler's primary query).
pub async fn list_by_status(
    pool: &SqlitePool,
    project_id: i64,
    status: IssueStatus,
) -> Result<Vec<Issue>> {
    let rows = sqlx::query("SELECT * FROM issues WHERE project_id = ? AND status = ? ORDER BY id")
        .bind(project_id)
        .bind(status.as_str())
        .fetch_all(pool)
        .await?;
    rows.iter().map(Issue::from_row).collect()
}

/// Load the current status of one issue (cheap; for transition checks).
async fn current_status(pool: &SqlitePool, id: i64) -> Result<IssueStatus> {
    let raw: Option<String> = sqlx::query_scalar("SELECT status FROM issues WHERE id = ?")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    let raw = raw.ok_or_else(|| anyhow!("issue {id} not found"))?;
    IssueStatus::from_str(&raw).ok_or_else(|| anyhow!("unknown issue status {raw:?} in db"))
}

/// Autonomous status change. Errors if `current -> to` is not a legal
/// transition (see `state::is_legal_transition`).
pub async fn transition(pool: &SqlitePool, id: i64, to: IssueStatus, now: i64) -> Result<()> {
    let from = current_status(pool, id).await?;
    state::check_transition(from, to)?;
    write_status(pool, id, to, now).await
}

/// Human override: set status WITHOUT the legality check. The matrix only
/// encodes autonomous moves; an operator may force any jump (and is expected to
/// record it). Use [`transition`] for system-driven changes.
pub async fn force_status(pool: &SqlitePool, id: i64, to: IssueStatus, now: i64) -> Result<()> {
    write_status(pool, id, to, now).await
}

pub async fn transition_if_current_project(
    pool: &SqlitePool,
    id: i64,
    project_id: i64,
    from: IssueStatus,
    to: IssueStatus,
    now: i64,
) -> Result<()> {
    state::check_transition(from, to)?;
    write_status_if_current_project(pool, id, project_id, from, to, now).await
}

pub async fn force_status_if_current_project(
    pool: &SqlitePool,
    id: i64,
    project_id: i64,
    from: IssueStatus,
    to: IssueStatus,
    now: i64,
) -> Result<()> {
    write_status_if_current_project(pool, id, project_id, from, to, now).await
}

async fn write_status_if_current_project(
    pool: &SqlitePool,
    id: i64,
    project_id: i64,
    from: IssueStatus,
    to: IssueStatus,
    now: i64,
) -> Result<()> {
    let n = sqlx::query(
        "UPDATE issues SET status = ?, updated_at = ? WHERE id = ? AND project_id = ? AND status = ?",
    )
    .bind(to.as_str())
    .bind(now)
    .bind(id)
    .bind(project_id)
    .bind(from.as_str())
    .execute(pool)
    .await?
    .rows_affected();
    if n == 0 {
        return Err(anyhow!(
            "stale_proposal: issue {id} is no longer {} in project {project_id}",
            from.as_str()
        ));
    }
    Ok(())
}

async fn write_status(pool: &SqlitePool, id: i64, to: IssueStatus, now: i64) -> Result<()> {
    let n = sqlx::query("UPDATE issues SET status = ?, updated_at = ? WHERE id = ?")
        .bind(to.as_str())
        .bind(now)
        .bind(id)
        .execute(pool)
        .await?
        .rows_affected();
    if n == 0 {
        return Err(anyhow!("issue {id} not found"));
    }
    Ok(())
}

/// Deprecated compatibility shim. Backlog attachment now records the target on
/// the backlog/message path instead of self-closing a donor issue.
pub async fn mark_absorbed(_pool: &SqlitePool, id: i64, _into_id: i64, _now: i64) -> Result<()> {
    Err(anyhow!(
        "issue {id} absorption was replaced by backlog routing to queue messages"
    ))
}

/// Record the worktree/branch a standalone issue acquires at PLANNING.
/// Any field may be `None` to leave it NULL.
pub async fn set_worktree(
    pool: &SqlitePool,
    id: i64,
    branch: Option<&str>,
    worktree_path: Option<&str>,
    now: i64,
) -> Result<()> {
    let n = sqlx::query(
        "UPDATE issues SET branch = ?, worktree_path = ?, updated_at = ?
         WHERE id = ?",
    )
    .bind(branch)
    .bind(worktree_path)
    .bind(now)
    .bind(id)
    .execute(pool)
    .await?
    .rows_affected();
    ensure_found(n, id)
}

/// Delete an issue after the caller has handled runtime cleanup. Child tables
/// cascade via schema FKs; backlog links use ON DELETE SET NULL.
pub async fn remove(pool: &SqlitePool, id: i64) -> Result<()> {
    let n = sqlx::query("DELETE FROM issues WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?
        .rows_affected();
    ensure_found(n, id)
}

/// Flip the re-trigger flag (set when new steering arrives for a working issue;
/// cleared once the work agent consumes it).
pub async fn set_pending_steering(
    pool: &SqlitePool,
    id: i64,
    pending: bool,
    now: i64,
) -> Result<()> {
    let n = sqlx::query("UPDATE issues SET has_pending_steering = ?, updated_at = ? WHERE id = ?")
        .bind(pending as i64)
        .bind(now)
        .bind(id)
        .execute(pool)
        .await?
        .rows_affected();
    ensure_found(n, id)
}

/// Set (or clear with `None`) the soft-gate deadline.
pub async fn set_wait_until(
    pool: &SqlitePool,
    id: i64,
    wait_until: Option<i64>,
    now: i64,
) -> Result<()> {
    let n = sqlx::query("UPDATE issues SET wait_until = ?, updated_at = ? WHERE id = ?")
        .bind(wait_until)
        .bind(now)
        .bind(id)
        .execute(pool)
        .await?
        .rows_affected();
    ensure_found(n, id)
}

/// `review_round += 1`, returning the new value (cap checks live in the caller).
pub async fn bump_review_round(pool: &SqlitePool, id: i64, now: i64) -> Result<i64> {
    let new: Option<i64> = sqlx::query_scalar(
        "UPDATE issues SET review_round = review_round + 1, updated_at = ?
         WHERE id = ? RETURNING review_round",
    )
    .bind(now)
    .bind(id)
    .fetch_optional(pool)
    .await?;
    new.ok_or_else(|| anyhow!("issue {id} not found"))
}

/// `conflict_attempts += 1`, returning the new value.
pub async fn bump_conflict_attempts(pool: &SqlitePool, id: i64, now: i64) -> Result<i64> {
    let new: Option<i64> = sqlx::query_scalar(
        "UPDATE issues SET conflict_attempts = conflict_attempts + 1, updated_at = ?
         WHERE id = ? RETURNING conflict_attempts",
    )
    .bind(now)
    .bind(id)
    .fetch_optional(pool)
    .await?;
    new.ok_or_else(|| anyhow!("issue {id} not found"))
}

/// Turn "no row updated" into a not-found error.
fn ensure_found(rows_affected: u64, id: i64) -> Result<()> {
    if rows_affected == 0 {
        return Err(anyhow!("issue {id} not found"));
    }
    Ok(())
}

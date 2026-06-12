//! Main-workspace lifecycle. Plan Step 3.5.
//!
//! Each registered project gets two persistent tmux sessions on the repo root:
//!   - `auwsx-{proj}-main-agent` — receives canonical/maintenance prompts
//!   - `auwsx-{proj}-main-shell` — empty bash, never touched by auwsx
//!
//! `ensure_main_sessions(project)` is idempotent: creates both if absent,
//! re-uses if present. Called on daemon start AND on every scheduler tick
//! (cheap; just shells `tmux has-session` first).
//!
//! MainJob queue: serialized through the `-main-agent` session, so routines and
//! the pipeline's own main-branch ops never race (one main writer per project).
//! Sources:
//!   - post_merge — after an issue hits DONE
//!   - routine — fired by cron/triage scheduler
//!   - user_oneoff — explicit UI click ([/dream], [/release], custom)
//!
//! State: QUEUED → RUNNING → DONE | FAILED | REJECTED. REJECTED is a `knowledge`
//! routine whose diff escaped its configured writable_paths (auwsx owns the
//! commit + path-scope check, so it refuses and flags). Logs to
//! `<repo>/.auwsx/main/log-{ts}.md`.

use crate::Result;
use anyhow::anyhow;
use serde::{Deserialize, Serialize};
use sqlx::sqlite::SqliteRow;
use sqlx::{Row, SqlitePool};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MainJobSource {
    PostMerge,
    Routine,
    UserOneoff,
}

impl MainJobSource {
    pub fn as_str(self) -> &'static str {
        match self {
            MainJobSource::PostMerge => "post_merge",
            MainJobSource::Routine => "routine",
            MainJobSource::UserOneoff => "user_oneoff",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        Some(match s {
            "post_merge" => MainJobSource::PostMerge,
            "routine" => MainJobSource::Routine,
            "user_oneoff" => MainJobSource::UserOneoff,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum MainJobStatus {
    Queued,
    Running,
    Done,
    Failed,
    Rejected,
}

impl MainJobStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            MainJobStatus::Queued => "QUEUED",
            MainJobStatus::Running => "RUNNING",
            MainJobStatus::Done => "DONE",
            MainJobStatus::Failed => "FAILED",
            MainJobStatus::Rejected => "REJECTED",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        Some(match s {
            "QUEUED" => MainJobStatus::Queued,
            "RUNNING" => MainJobStatus::Running,
            "DONE" => MainJobStatus::Done,
            "FAILED" => MainJobStatus::Failed,
            "REJECTED" => MainJobStatus::Rejected,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MainJob {
    pub id: i64,
    pub project_id: i64,
    pub routine_id: Option<i64>,
    pub source: MainJobSource,
    pub kind: String,
    pub prompt: String,
    pub status: MainJobStatus,
    pub worktree_path: Option<String>,
    pub report_path: Option<String>,
    pub scope_violation: Option<String>,
    pub queued_at: i64,
    pub started_at: Option<i64>,
    pub ended_at: Option<i64>,
    pub log_path: Option<String>,
    pub outcome: Option<String>,
}

impl MainJob {
    fn from_row(row: &SqliteRow) -> Result<Self> {
        let source_raw: String = row.try_get("source")?;
        let status_raw: String = row.try_get("status")?;
        Ok(MainJob {
            id: row.try_get("id")?,
            project_id: row.try_get("project_id")?,
            routine_id: row.try_get("routine_id")?,
            source: MainJobSource::from_str(&source_raw)
                .ok_or_else(|| anyhow!("unknown main_jobs.source {source_raw:?} in db"))?,
            kind: row.try_get("kind")?,
            prompt: row.try_get("prompt")?,
            status: MainJobStatus::from_str(&status_raw)
                .ok_or_else(|| anyhow!("unknown main_jobs.status {status_raw:?} in db"))?,
            worktree_path: row.try_get("worktree_path")?,
            report_path: row.try_get("report_path")?,
            scope_violation: row.try_get("scope_violation")?,
            queued_at: row.try_get("queued_at")?,
            started_at: row.try_get("started_at")?,
            ended_at: row.try_get("ended_at")?,
            log_path: row.try_get("log_path")?,
            outcome: row.try_get("outcome")?,
        })
    }
}

pub async fn recent_by_project(
    pool: &SqlitePool,
    project_id: i64,
    limit: i64,
) -> Result<Vec<MainJob>> {
    let rows = sqlx::query("SELECT * FROM main_jobs WHERE project_id = ? ORDER BY id DESC LIMIT ?")
        .bind(project_id)
        .bind(limit.max(0))
        .fetch_all(pool)
        .await?;
    rows.iter().map(MainJob::from_row).collect()
}

pub async fn get(pool: &SqlitePool, id: i64) -> Result<Option<MainJob>> {
    let row = sqlx::query("SELECT * FROM main_jobs WHERE id = ?")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    row.as_ref().map(MainJob::from_row).transpose()
}

pub async fn enqueue_routine(
    pool: &SqlitePool,
    project_id: i64,
    routine_id: i64,
    kind: &str,
    prompt: &str,
    queued_at: i64,
) -> Result<i64> {
    let id: i64 = sqlx::query(
        "INSERT INTO main_jobs
            (project_id, routine_id, source, kind, prompt, status, queued_at)
         VALUES (?, ?, ?, ?, ?, ?, ?)
         RETURNING id",
    )
    .bind(project_id)
    .bind(routine_id)
    .bind(MainJobSource::Routine.as_str())
    .bind(kind)
    .bind(prompt)
    .bind(MainJobStatus::Queued.as_str())
    .bind(queued_at)
    .fetch_one(pool)
    .await?
    .get("id");
    Ok(id)
}

pub async fn mark_running(
    pool: &SqlitePool,
    id: i64,
    started_at: i64,
    log_path: &str,
) -> Result<()> {
    let n = sqlx::query(
        "UPDATE main_jobs
            SET status = ?, started_at = ?, log_path = ?
         WHERE id = ? AND status = ?",
    )
    .bind(MainJobStatus::Running.as_str())
    .bind(started_at)
    .bind(log_path)
    .bind(id)
    .bind(MainJobStatus::Queued.as_str())
    .execute(pool)
    .await?
    .rows_affected();
    if n == 0 {
        return Err(anyhow!("main_job {id} is not queued"));
    }
    Ok(())
}

pub async fn finish(
    pool: &SqlitePool,
    id: i64,
    status: MainJobStatus,
    ended_at: i64,
    outcome: Option<&str>,
) -> Result<()> {
    match status {
        MainJobStatus::Done | MainJobStatus::Failed | MainJobStatus::Rejected => {}
        _ => return Err(anyhow!("main_job finish status must be terminal")),
    }
    let n = sqlx::query(
        "UPDATE main_jobs
            SET status = ?, ended_at = ?, outcome = ?
         WHERE id = ?",
    )
    .bind(status.as_str())
    .bind(ended_at)
    .bind(outcome)
    .bind(id)
    .execute(pool)
    .await?
    .rows_affected();
    if n == 0 {
        return Err(anyhow!("main_job {id} not found"));
    }
    Ok(())
}

pub async fn recent_by_routine(
    pool: &SqlitePool,
    routine_id: i64,
    limit: i64,
) -> Result<Vec<MainJob>> {
    let rows = sqlx::query("SELECT * FROM main_jobs WHERE routine_id = ? ORDER BY id DESC LIMIT ?")
        .bind(routine_id)
        .bind(limit.max(0))
        .fetch_all(pool)
        .await?;
    rows.iter().map(MainJob::from_row).collect()
}

// TODO: ensure_main_sessions(project) — idempotent tmux create
// TODO: generalize enqueue beyond routine source as post-merge/user one-off land
// TODO: serial worker draining the queue per project for automatic cron runs

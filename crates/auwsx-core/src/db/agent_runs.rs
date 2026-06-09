//! `agent_runs` typed row + CRUD. Schema: src/db/migrations/0001_init.sql.
//!
//! Append-only action log: one row per agent spawn, written in two steps —
//! [`start`] when the process is launched, [`finish`] when it ends — so a crash
//! mid-run still leaves a record (with no `exited_at`). Exactly one of
//! `issue_id` / `main_job_id` is set (enforced here, mirroring the schema note).
//! This is both the transparency trail and the self-eval dataset.

use crate::agent::ExitKind;
use crate::Result;
use anyhow::{anyhow, bail};
use serde::{Deserialize, Serialize};
use sqlx::sqlite::SqliteRow;
use sqlx::{Row, SqlitePool};

/// Which per-role agent CLI ran. Matches the `agent_runs.role` CHECK domain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Main,
    Plan,
    Work,
    Review,
}

impl Role {
    pub fn as_str(&self) -> &'static str {
        match self {
            Role::Main => "main",
            Role::Plan => "plan",
            Role::Work => "work",
            Role::Review => "review",
        }
    }
    pub fn from_str(s: &str) -> Option<Self> {
        Some(match s {
            "main" => Role::Main,
            "plan" => Role::Plan,
            "work" => Role::Work,
            "review" => Role::Review,
            _ => return None,
        })
    }
}

/// Full `agent_runs` row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentRun {
    pub id: i64,
    pub issue_id: Option<i64>,
    pub main_job_id: Option<i64>,
    pub role: Role,
    pub phase: String,
    pub agent_cmd: String,
    pub status_before: Option<String>,
    pub status_after: Option<String>,
    pub pid: Option<i64>,
    pub exit_code: Option<i64>,
    pub exit_kind: Option<ExitKind>,
    pub prompt_path: Option<String>,
    pub log_path: Option<String>,
    pub spawned_at: i64,
    pub exited_at: Option<i64>,
    pub note: Option<String>,
}

impl AgentRun {
    fn from_row(row: &SqliteRow) -> Result<Self> {
        let role_raw: String = row.try_get("role")?;
        let exit_kind_raw: Option<String> = row.try_get("exit_kind")?;
        let exit_kind = match exit_kind_raw {
            Some(s) => Some(
                ExitKind::from_str(&s)
                    .ok_or_else(|| anyhow!("unknown agent_runs.exit_kind {s:?} in db"))?,
            ),
            None => None,
        };
        Ok(AgentRun {
            id: row.try_get("id")?,
            issue_id: row.try_get("issue_id")?,
            main_job_id: row.try_get("main_job_id")?,
            role: Role::from_str(&role_raw)
                .ok_or_else(|| anyhow!("unknown agent_runs.role {role_raw:?} in db"))?,
            phase: row.try_get("phase")?,
            agent_cmd: row.try_get("agent_cmd")?,
            status_before: row.try_get("status_before")?,
            status_after: row.try_get("status_after")?,
            pid: row.try_get("pid")?,
            exit_code: row.try_get("exit_code")?,
            exit_kind,
            prompt_path: row.try_get("prompt_path")?,
            log_path: row.try_get("log_path")?,
            spawned_at: row.try_get("spawned_at")?,
            exited_at: row.try_get("exited_at")?,
            note: row.try_get("note")?,
        })
    }
}

/// Inputs recorded when an agent is launched (before it ends).
#[derive(Debug, Clone)]
pub struct StartRun<'a> {
    /// Exactly one of `issue_id` / `main_job_id` must be `Some`.
    pub issue_id: Option<i64>,
    pub main_job_id: Option<i64>,
    pub role: Role,
    pub phase: &'a str,
    pub agent_cmd: &'a str,
    pub status_before: Option<&'a str>,
    pub pid: Option<i64>,
    pub prompt_path: Option<&'a str>,
    pub log_path: Option<&'a str>,
}

/// Open a run record at spawn time. Returns the new id (pass it to [`finish`]).
pub async fn start(pool: &SqlitePool, run: StartRun<'_>, spawned_at: i64) -> Result<i64> {
    match (run.issue_id, run.main_job_id) {
        (Some(_), None) | (None, Some(_)) => {}
        _ => bail!("agent_run must reference exactly one of issue_id / main_job_id"),
    }
    let id: i64 = sqlx::query(
        "INSERT INTO agent_runs
            (issue_id, main_job_id, role, phase, agent_cmd, status_before,
             pid, prompt_path, log_path, spawned_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
         RETURNING id",
    )
    .bind(run.issue_id)
    .bind(run.main_job_id)
    .bind(run.role.as_str())
    .bind(run.phase)
    .bind(run.agent_cmd)
    .bind(run.status_before)
    .bind(run.pid)
    .bind(run.prompt_path)
    .bind(run.log_path)
    .bind(spawned_at)
    .fetch_one(pool)
    .await?
    .get("id");
    Ok(id)
}

/// Close a run record when the agent ends.
pub async fn finish(
    pool: &SqlitePool,
    run_id: i64,
    status_after: Option<&str>,
    exit_code: Option<i64>,
    exit_kind: ExitKind,
    exited_at: i64,
    note: Option<&str>,
) -> Result<()> {
    let n = sqlx::query(
        "UPDATE agent_runs
            SET status_after = ?, exit_code = ?, exit_kind = ?, exited_at = ?, note = ?
         WHERE id = ?",
    )
    .bind(status_after)
    .bind(exit_code)
    .bind(exit_kind.as_str())
    .bind(exited_at)
    .bind(note)
    .bind(run_id)
    .execute(pool)
    .await?
    .rows_affected();
    if n == 0 {
        return Err(anyhow!("agent_run {run_id} not found"));
    }
    Ok(())
}

pub async fn get(pool: &SqlitePool, id: i64) -> Result<Option<AgentRun>> {
    let row = sqlx::query("SELECT * FROM agent_runs WHERE id = ?")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    row.as_ref().map(AgentRun::from_row).transpose()
}

/// All runs for an issue, oldest first (the issue's execution history).
pub async fn list_by_issue(pool: &SqlitePool, issue_id: i64) -> Result<Vec<AgentRun>> {
    let rows = sqlx::query("SELECT * FROM agent_runs WHERE issue_id = ? ORDER BY id")
        .bind(issue_id)
        .fetch_all(pool)
        .await?;
    rows.iter().map(AgentRun::from_row).collect()
}

//! Route-agent runs for semantic backlog triage.

use crate::agent::ExitKind;
use crate::Result;
use anyhow::anyhow;
use serde::{Deserialize, Serialize};
use sqlx::sqlite::SqliteRow;
use sqlx::{Row, SqlitePool};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoutingRun {
    pub id: i64,
    pub project_id: i64,
    pub backlog_item_id: i64,
    pub candidate_issue_ids: String,
    pub agent_cmd: String,
    pub prompt_path: Option<String>,
    pub log_path: Option<String>,
    pub raw_decision: Option<String>,
    pub parsed_decision: Option<String>,
    pub fallback_reason: Option<String>,
    pub exit_code: Option<i64>,
    pub exit_kind: Option<ExitKind>,
    pub spawned_at: i64,
    pub exited_at: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct StartRoutingRun<'a> {
    pub project_id: i64,
    pub backlog_item_id: i64,
    pub candidate_issue_ids: &'a str,
    pub agent_cmd: &'a str,
    pub prompt_path: Option<&'a str>,
    pub log_path: Option<&'a str>,
}

#[derive(Debug, Clone)]
pub struct FinishRoutingRun<'a> {
    pub raw_decision: Option<&'a str>,
    pub parsed_decision: Option<&'a str>,
    pub fallback_reason: Option<&'a str>,
    pub exit_code: Option<i64>,
    pub exit_kind: Option<ExitKind>,
    pub exited_at: i64,
}

fn from_row(row: &SqliteRow) -> Result<RoutingRun> {
    let exit_kind_raw: Option<String> = row.try_get("exit_kind")?;
    let exit_kind = match exit_kind_raw {
        Some(s) => Some(
            ExitKind::from_str(&s)
                .ok_or_else(|| anyhow!("unknown routing_runs.exit_kind {s:?} in db"))?,
        ),
        None => None,
    };
    Ok(RoutingRun {
        id: row.try_get("id")?,
        project_id: row.try_get("project_id")?,
        backlog_item_id: row.try_get("backlog_item_id")?,
        candidate_issue_ids: row.try_get("candidate_issue_ids")?,
        agent_cmd: row.try_get("agent_cmd")?,
        prompt_path: row.try_get("prompt_path")?,
        log_path: row.try_get("log_path")?,
        raw_decision: row.try_get("raw_decision")?,
        parsed_decision: row.try_get("parsed_decision")?,
        fallback_reason: row.try_get("fallback_reason")?,
        exit_code: row.try_get("exit_code")?,
        exit_kind,
        spawned_at: row.try_get("spawned_at")?,
        exited_at: row.try_get("exited_at")?,
    })
}

pub async fn start(pool: &SqlitePool, run: StartRoutingRun<'_>, spawned_at: i64) -> Result<i64> {
    let id: i64 = sqlx::query(
        "INSERT INTO routing_runs
            (project_id, backlog_item_id, candidate_issue_ids, agent_cmd,
             prompt_path, log_path, spawned_at)
         VALUES (?, ?, ?, ?, ?, ?, ?)
         RETURNING id",
    )
    .bind(run.project_id)
    .bind(run.backlog_item_id)
    .bind(run.candidate_issue_ids)
    .bind(run.agent_cmd)
    .bind(run.prompt_path)
    .bind(run.log_path)
    .bind(spawned_at)
    .fetch_one(pool)
    .await?
    .get("id");
    Ok(id)
}

pub async fn finish(pool: &SqlitePool, id: i64, finish: FinishRoutingRun<'_>) -> Result<()> {
    let n = sqlx::query(
        "UPDATE routing_runs
            SET raw_decision = ?,
                parsed_decision = ?,
                fallback_reason = ?,
                exit_code = ?,
                exit_kind = ?,
                exited_at = ?
         WHERE id = ?",
    )
    .bind(finish.raw_decision)
    .bind(finish.parsed_decision)
    .bind(finish.fallback_reason)
    .bind(finish.exit_code)
    .bind(finish.exit_kind.map(|kind| kind.as_str()))
    .bind(finish.exited_at)
    .bind(id)
    .execute(pool)
    .await?
    .rows_affected();
    if n == 0 {
        return Err(anyhow!("routing_run {id} not found"));
    }
    Ok(())
}

pub async fn recent_by_project(
    pool: &SqlitePool,
    project_id: i64,
    limit: i64,
) -> Result<Vec<RoutingRun>> {
    let rows = sqlx::query(
        "SELECT * FROM routing_runs
         WHERE project_id = ?
         ORDER BY id DESC
         LIMIT ?",
    )
    .bind(project_id)
    .bind(limit.max(0))
    .fetch_all(pool)
    .await?;
    rows.iter().map(from_row).collect()
}

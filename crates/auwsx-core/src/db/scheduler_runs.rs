//! `scheduler_runs` typed row + CRUD. Observability for daemon ticks.

use crate::Result;
use serde::{Deserialize, Serialize};
use sqlx::sqlite::SqliteRow;
use sqlx::{Row, SqlitePool};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SchedulerRunSource {
    Auto,
    Manual,
}

impl SchedulerRunSource {
    pub fn as_str(self) -> &'static str {
        match self {
            SchedulerRunSource::Auto => "auto",
            SchedulerRunSource::Manual => "manual",
        }
    }

    fn from_str(raw: &str) -> Result<Self> {
        Ok(match raw {
            "auto" => SchedulerRunSource::Auto,
            "manual" => SchedulerRunSource::Manual,
            _ => anyhow::bail!("unknown scheduler_runs.source {raw:?}"),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SchedulerRun {
    pub id: i64,
    pub project_id: i64,
    pub fired_at: i64,
    pub source: SchedulerRunSource,
    pub picked: Option<String>,
}

impl SchedulerRun {
    fn from_row(row: &SqliteRow) -> Result<Self> {
        let source_raw: String = row.try_get("source")?;
        Ok(SchedulerRun {
            id: row.try_get("id")?,
            project_id: row.try_get("project_id")?,
            fired_at: row.try_get("fired_at")?,
            source: SchedulerRunSource::from_str(&source_raw)?,
            picked: row.try_get("picked")?,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum SchedulerRunDecision {
    Spawn { issue_id: i64 },
    SoftGate { issue_id: i64 },
    Teardown { issue_id: i64 },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SchedulerRunPicked {
    pub triaged_issue_ids: Vec<i64>,
    #[serde(default)]
    pub triaged_routes: Vec<SchedulerRunRoute>,
    pub decisions: Vec<SchedulerRunDecision>,
    pub pending_backlog: usize,
    pub ready_backlog: usize,
    pub running_issues: usize,
    pub max_concurrency: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SchedulerRunRoute {
    pub backlog_item_id: i64,
    pub issue_id: i64,
    pub kind: String,
    pub fallback_reason: Option<String>,
}

impl SchedulerRunPicked {
    pub fn to_json_string(&self) -> Result<String> {
        Ok(serde_json::to_string(self)?)
    }
}

pub async fn record(
    pool: &SqlitePool,
    project_id: i64,
    fired_at: i64,
    source: SchedulerRunSource,
    picked: Option<&str>,
) -> Result<i64> {
    let id: i64 = sqlx::query(
        "INSERT INTO scheduler_runs (project_id, fired_at, source, picked)
         VALUES (?, ?, ?, ?)
         RETURNING id",
    )
    .bind(project_id)
    .bind(fired_at)
    .bind(source.as_str())
    .bind(picked)
    .fetch_one(pool)
    .await?
    .get("id");
    Ok(id)
}

pub async fn recent_by_project(
    pool: &SqlitePool,
    project_id: i64,
    limit: i64,
) -> Result<Vec<SchedulerRun>> {
    let rows = sqlx::query(
        "SELECT * FROM scheduler_runs WHERE project_id = ? ORDER BY fired_at DESC LIMIT ?",
    )
    .bind(project_id)
    .bind(limit.max(0))
    .fetch_all(pool)
    .await?;
    rows.iter().map(SchedulerRun::from_row).collect()
}

pub async fn latest_auto_by_project(
    pool: &SqlitePool,
    project_id: i64,
) -> Result<Option<SchedulerRun>> {
    let row = sqlx::query(
        "SELECT * FROM scheduler_runs
         WHERE project_id = ? AND source = 'auto'
         ORDER BY fired_at DESC
         LIMIT 1",
    )
    .bind(project_id)
    .fetch_optional(pool)
    .await?;
    row.as_ref().map(SchedulerRun::from_row).transpose()
}

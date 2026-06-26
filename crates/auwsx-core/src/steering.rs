//! Queue messages: append-only guidance into in-flight issues. Plan Step 3.8.
//!
//! The table/module keeps the old `steering` storage name for compatibility.
//! A queue message is appended to an issue that accepts messages
//! (`IssueStatus::accepts_queue_message`) and never edits `plan.md`.
//! The work agent consumes pending queue messages on its next spawn.
//!
//! Two sources:
//! - `human`         — the user nudges an in-flight issue.
//! - `consolidation` — routing folds a relevant approved backlog task into
//!   an existing issue instead of opening a new one.
//!
//! Adding steering flips `issues.has_pending_steering = 1`, which re-activates
//! the issue on the next scheduler tick even if it had gone quiet.

use crate::db::issues;
use crate::Result;
use anyhow::{anyhow, bail};
use serde::{Deserialize, Serialize};
use sqlx::sqlite::SqliteRow;
use sqlx::{Row, SqlitePool};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SteeringSource {
    Human,
    Consolidation,
}

impl SteeringSource {
    /// DB/wire id; matches the `steering.source` CHECK domain.
    pub fn as_str(&self) -> &'static str {
        match self {
            SteeringSource::Human => "human",
            SteeringSource::Consolidation => "consolidation",
        }
    }
    pub fn from_str(s: &str) -> Option<Self> {
        Some(match s {
            "human" => SteeringSource::Human,
            "consolidation" => SteeringSource::Consolidation,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Steering {
    pub id: i64,
    pub issue_id: i64,
    pub source: SteeringSource,
    pub note: String,
    pub consumed: bool,
    pub created_at: i64,
    pub consumed_at: Option<i64>,
}

impl Steering {
    fn from_row(row: &SqliteRow) -> Result<Self> {
        let source_raw: String = row.try_get("source")?;
        let consumed: i64 = row.try_get("consumed")?;
        Ok(Steering {
            id: row.try_get("id")?,
            issue_id: row.try_get("issue_id")?,
            source: SteeringSource::from_str(&source_raw)
                .ok_or_else(|| anyhow!("unknown steering source {source_raw:?} in db"))?,
            note: row.try_get("note")?,
            consumed: consumed != 0,
            created_at: row.try_get("created_at")?,
            consumed_at: row.try_get("consumed_at")?,
        })
    }
}

/// Append steering to an issue and flip its re-trigger flag, in one transaction.
///
/// Guarded by `IssueStatus::accepts_queue_message`: the issue must be in a
/// queue-eligible phase (a locked plan + worktree exist, or READY_TO_MERGE is
/// waiting for human verification), otherwise this errors. Steering never
/// touches `plan.md`, so it is meaningless before the plan is locked.
/// Returns the new steering id.
pub async fn add(
    pool: &SqlitePool,
    issue_id: i64,
    source: SteeringSource,
    note: &str,
    now: i64,
) -> Result<i64> {
    let issue = issues::get(pool, issue_id)
        .await?
        .ok_or_else(|| anyhow!("issue {issue_id} not found"))?;
    if !issue.status.accepts_queue_message() {
        bail!(
            "issue {issue_id} is {:?}, which does not accept steering (needs a queue-eligible phase)",
            issue.status
        );
    }

    // Append + set the flag together so a tick can never see one without the
    // other. SQLite serializes writers, so a transaction is sufficient.
    let mut tx = pool.begin().await?;
    let id: i64 = sqlx::query(
        "INSERT INTO steering (issue_id, source, note, created_at)
         VALUES (?, ?, ?, ?)
         RETURNING id",
    )
    .bind(issue_id)
    .bind(source.as_str())
    .bind(note)
    .bind(now)
    .fetch_one(&mut *tx)
    .await?
    .get("id");

    sqlx::query("UPDATE issues SET has_pending_steering = 1, updated_at = ? WHERE id = ?")
        .bind(now)
        .bind(issue_id)
        .execute(&mut *tx)
        .await?;

    tx.commit().await?;
    Ok(id)
}

pub async fn get(pool: &SqlitePool, id: i64) -> Result<Option<Steering>> {
    let row = sqlx::query("SELECT * FROM steering WHERE id = ?")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    row.as_ref().map(Steering::from_row).transpose()
}

/// Unconsumed steering for an issue, oldest first (the work agent's inbox).
pub async fn list_pending(pool: &SqlitePool, issue_id: i64) -> Result<Vec<Steering>> {
    let rows =
        sqlx::query("SELECT * FROM steering WHERE issue_id = ? AND consumed = 0 ORDER BY id")
            .bind(issue_id)
            .fetch_all(pool)
            .await?;
    rows.iter().map(Steering::from_row).collect()
}

/// Mark every pending note for an issue consumed and clear the re-trigger flag,
/// in one transaction. Called when the work agent has ingested the steering.
pub async fn consume_all(pool: &SqlitePool, issue_id: i64, now: i64) -> Result<()> {
    let mut tx = pool.begin().await?;
    sqlx::query(
        "UPDATE steering SET consumed = 1, consumed_at = ?
         WHERE issue_id = ? AND consumed = 0",
    )
    .bind(now)
    .bind(issue_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query("UPDATE issues SET has_pending_steering = 0, updated_at = ? WHERE id = ?")
        .bind(now)
        .bind(issue_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

/// Remove a single still-pending note (human retracts guidance). Errors if the
/// note is missing or already consumed.
pub async fn remove_pending(pool: &SqlitePool, id: i64) -> Result<()> {
    let n = sqlx::query("DELETE FROM steering WHERE id = ? AND consumed = 0")
        .bind(id)
        .execute(pool)
        .await?
        .rows_affected();
    if n == 0 {
        return Err(anyhow!("pending steering {id} not found"));
    }
    Ok(())
}

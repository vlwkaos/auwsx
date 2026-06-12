//! `subtasks` typed row + CRUD. Schema: src/db/migrations/0001_init.sql.
//!
//! Subtasks are the plan agent's output: the ordered IMPLEMENTING checklist for
//! one issue. `ord` is the display/execution order; `done` flips as the work
//! agent completes each item.

use crate::Result;
use anyhow::anyhow;
use serde::{Deserialize, Serialize};
use sqlx::sqlite::SqliteRow;
use sqlx::{Row, SqlitePool};

/// Full `subtasks` row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Subtask {
    pub id: i64,
    pub issue_id: i64,
    pub ord: i64,
    pub text: String,
    pub done: bool,
    pub created_at: i64,
    pub done_at: Option<i64>,
}

impl Subtask {
    fn from_row(row: &SqliteRow) -> Result<Self> {
        let done: i64 = row.try_get("done")?;
        Ok(Subtask {
            id: row.try_get("id")?,
            issue_id: row.try_get("issue_id")?,
            ord: row.try_get("ord")?,
            text: row.try_get("text")?,
            done: done != 0,
            created_at: row.try_get("created_at")?,
            done_at: row.try_get("done_at")?,
        })
    }
}

/// Append one subtask (`done = 0`). Returns the new id.
pub async fn add(pool: &SqlitePool, issue_id: i64, ord: i64, text: &str, now: i64) -> Result<i64> {
    let id: i64 = sqlx::query(
        "INSERT INTO subtasks (issue_id, ord, text, created_at)
         VALUES (?, ?, ?, ?)
         RETURNING id",
    )
    .bind(issue_id)
    .bind(ord)
    .bind(text)
    .bind(now)
    .fetch_one(pool)
    .await?
    .get("id");
    Ok(id)
}

/// The issue's checklist, in execution order.
pub async fn list_by_issue(pool: &SqlitePool, issue_id: i64) -> Result<Vec<Subtask>> {
    let rows = sqlx::query("SELECT * FROM subtasks WHERE issue_id = ? ORDER BY ord, id")
        .bind(issue_id)
        .fetch_all(pool)
        .await?;
    rows.iter().map(Subtask::from_row).collect()
}

/// Mark a subtask done (stamps `done_at = now`).
pub async fn mark_done(pool: &SqlitePool, id: i64, now: i64) -> Result<()> {
    let n = sqlx::query("UPDATE subtasks SET done = 1, done_at = ? WHERE id = ?")
        .bind(now)
        .bind(id)
        .execute(pool)
        .await?
        .rows_affected();
    ensure_found(n, id)
}

/// Reopen a subtask (clears `done_at`).
pub async fn mark_undone(pool: &SqlitePool, id: i64) -> Result<()> {
    let n = sqlx::query("UPDATE subtasks SET done = 0, done_at = NULL WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?
        .rows_affected();
    ensure_found(n, id)
}

fn ensure_found(rows_affected: u64, id: i64) -> Result<()> {
    if rows_affected == 0 {
        return Err(anyhow!("subtask {id} not found"));
    }
    Ok(())
}

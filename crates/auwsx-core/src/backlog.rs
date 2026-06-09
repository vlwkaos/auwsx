//! Backlog items + triage/consolidation. Plan Step 3.7.
//!
//! A backlog item is a lightweight intent: `(project_id, text, source)`. It
//! carries an admission gate:
//!
//!   * `source` = human | agent | routine | inbox
//!   * `approval` = pending | approved | dismissed
//!
//! Human/inbox items are inserted `approved`; agent/routine items are inserted
//! `pending` and wait for a human approve/dismiss in the overview. Only
//! `approved` items flow into triage.
//!
//! Triage (a built-in main-job) auto-groups approved items into issues and
//! promotes them — no human grouping gate (the gate is admission, above). Each
//! provisional issue then runs the CONSOLIDATING phase (see `pipeline`), which
//! decides delegate-as-steering vs. standalone before any worktree is created.

use crate::db::issues;
use crate::Result;
use anyhow::anyhow;
use serde::{Deserialize, Serialize};
use sqlx::sqlite::SqliteRow;
use sqlx::{Row, SqlitePool};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    Human,
    Agent,
    Routine,
    Inbox,
}

impl Source {
    /// Items a human authored (directly or via inbox) are pre-approved; agent
    /// and routine output must clear the admission gate.
    pub fn default_approval(&self) -> Approval {
        match self {
            Source::Human | Source::Inbox => Approval::Approved,
            Source::Agent | Source::Routine => Approval::Pending,
        }
    }

    /// DB/wire id; matches the `backlog_items.source` CHECK domain.
    pub fn as_str(&self) -> &'static str {
        match self {
            Source::Human => "human",
            Source::Agent => "agent",
            Source::Routine => "routine",
            Source::Inbox => "inbox",
        }
    }
    pub fn from_str(s: &str) -> Option<Self> {
        Some(match s {
            "human" => Source::Human,
            "agent" => Source::Agent,
            "routine" => Source::Routine,
            "inbox" => Source::Inbox,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Approval {
    Pending,
    Approved,
    Dismissed,
}

impl Approval {
    /// DB/wire id; matches the `backlog_items.approval` CHECK domain.
    pub fn as_str(&self) -> &'static str {
        match self {
            Approval::Pending => "pending",
            Approval::Approved => "approved",
            Approval::Dismissed => "dismissed",
        }
    }
    pub fn from_str(s: &str) -> Option<Self> {
        Some(match s {
            "pending" => Approval::Pending,
            "approved" => Approval::Approved,
            "dismissed" => Approval::Dismissed,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BacklogItem {
    pub id: i64,
    pub project_id: i64,
    pub text: String,
    pub source: Source,
    pub approval: Approval,
    pub origin_routine_id: Option<i64>,
    pub consumed_issue_id: Option<i64>,
    pub created_at: i64,
    pub resolved_at: Option<i64>,
}

impl BacklogItem {
    fn from_row(row: &SqliteRow) -> Result<Self> {
        let source_raw: String = row.try_get("source")?;
        let approval_raw: String = row.try_get("approval")?;
        Ok(BacklogItem {
            id: row.try_get("id")?,
            project_id: row.try_get("project_id")?,
            text: row.try_get("text")?,
            source: Source::from_str(&source_raw)
                .ok_or_else(|| anyhow!("unknown backlog source {source_raw:?} in db"))?,
            approval: Approval::from_str(&approval_raw)
                .ok_or_else(|| anyhow!("unknown backlog approval {approval_raw:?} in db"))?,
            origin_routine_id: row.try_get("origin_routine_id")?,
            consumed_issue_id: row.try_get("consumed_issue_id")?,
            created_at: row.try_get("created_at")?,
            resolved_at: row.try_get("resolved_at")?,
        })
    }
}

/// Add a backlog item. The admission gate is applied here: `approval` is derived
/// from `source` via [`Source::default_approval`] (human/inbox -> approved,
/// agent/routine -> pending), so callers can't accidentally smuggle agent
/// output straight to approved. Returns the new id.
pub async fn add(
    pool: &SqlitePool,
    project_id: i64,
    text: &str,
    source: Source,
    origin_routine_id: Option<i64>,
    now: i64,
) -> Result<i64> {
    let id: i64 = sqlx::query(
        "INSERT INTO backlog_items
            (project_id, text, source, approval, origin_routine_id, created_at)
         VALUES (?, ?, ?, ?, ?, ?)
         RETURNING id",
    )
    .bind(project_id)
    .bind(text)
    .bind(source.as_str())
    .bind(source.default_approval().as_str())
    .bind(origin_routine_id)
    .bind(now)
    .fetch_one(pool)
    .await?
    .get("id");
    Ok(id)
}

pub async fn get(pool: &SqlitePool, id: i64) -> Result<Option<BacklogItem>> {
    let row = sqlx::query("SELECT * FROM backlog_items WHERE id = ?")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    row.as_ref().map(BacklogItem::from_row).transpose()
}

/// All items for a project, newest first.
pub async fn list_by_project(pool: &SqlitePool, project_id: i64) -> Result<Vec<BacklogItem>> {
    let rows =
        sqlx::query("SELECT * FROM backlog_items WHERE project_id = ? ORDER BY id DESC")
            .bind(project_id)
            .fetch_all(pool)
            .await?;
    rows.iter().map(BacklogItem::from_row).collect()
}

/// Items in a given approval state (e.g. `Approved` is what flows to triage).
pub async fn list_by_approval(
    pool: &SqlitePool,
    project_id: i64,
    approval: Approval,
) -> Result<Vec<BacklogItem>> {
    let rows = sqlx::query(
        "SELECT * FROM backlog_items WHERE project_id = ? AND approval = ? ORDER BY id",
    )
    .bind(project_id)
    .bind(approval.as_str())
    .fetch_all(pool)
    .await?;
    rows.iter().map(BacklogItem::from_row).collect()
}

/// Human approves a pending item (admits it to triage). Stamps `resolved_at`.
pub async fn approve(pool: &SqlitePool, id: i64, now: i64) -> Result<()> {
    set_approval(pool, id, Approval::Approved, now).await
}

/// Human dismisses an item (it never reaches triage). Stamps `resolved_at`.
pub async fn dismiss(pool: &SqlitePool, id: i64, now: i64) -> Result<()> {
    set_approval(pool, id, Approval::Dismissed, now).await
}

async fn set_approval(
    pool: &SqlitePool,
    id: i64,
    approval: Approval,
    now: i64,
) -> Result<()> {
    let n = sqlx::query("UPDATE backlog_items SET approval = ?, resolved_at = ? WHERE id = ?")
        .bind(approval.as_str())
        .bind(now)
        .bind(id)
        .execute(pool)
        .await?
        .rows_affected();
    ensure_found(n, id)
}

/// Edit an item's text (only meaningful while still pending/approved).
pub async fn edit_text(pool: &SqlitePool, id: i64, text: &str) -> Result<()> {
    let n = sqlx::query("UPDATE backlog_items SET text = ? WHERE id = ?")
        .bind(text)
        .bind(id)
        .execute(pool)
        .await?
        .rows_affected();
    ensure_found(n, id)
}

/// Link an item to the issue triage grouped it into (sets `consumed_issue_id`).
pub async fn mark_consumed(pool: &SqlitePool, id: i64, issue_id: i64, now: i64) -> Result<()> {
    let n = sqlx::query(
        "UPDATE backlog_items SET consumed_issue_id = ?, resolved_at = ? WHERE id = ?",
    )
    .bind(issue_id)
    .bind(now)
    .bind(id)
    .execute(pool)
    .await?
    .rows_affected();
    ensure_found(n, id)
}

/// Delete an item outright.
pub async fn remove(pool: &SqlitePool, id: i64) -> Result<()> {
    let n = sqlx::query("DELETE FROM backlog_items WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?
        .rows_affected();
    ensure_found(n, id)
}

fn ensure_found(rows_affected: u64, id: i64) -> Result<()> {
    if rows_affected == 0 {
        return Err(anyhow!("backlog item {id} not found"));
    }
    Ok(())
}

/// Promote every approved, not-yet-grouped backlog item in a project into its
/// own issue (which enters `CONSOLIDATING`, where the pipeline later decides
/// standalone-vs-delegate). Each promoted item is linked to its issue via
/// `consumed_issue_id`. Returns the created issue ids, in item order.
///
/// v1 has no grouping heuristic — one item, one issue. Consolidation across
/// similar issues happens later, per-issue, in the CONSOLIDATING phase.
pub async fn run_triage(pool: &SqlitePool, project_id: i64, now: i64) -> Result<Vec<i64>> {
    let approved = list_by_approval(pool, project_id, Approval::Approved).await?;
    let mut created = Vec::new();
    for item in approved {
        if item.consumed_issue_id.is_some() {
            continue; // already grouped into an issue
        }
        let issue_id = issues::create(pool, project_id, &item.text, None, now).await?;
        mark_consumed(pool, item.id, issue_id, now).await?;
        created.push(issue_id);
    }
    Ok(created)
}

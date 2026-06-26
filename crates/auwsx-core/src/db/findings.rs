//! `findings` typed row + CRUD. Schema: src/db/migrations/0001_init.sql.
//!
//! A finding is one reviewer observation. The REVIEWING agent emits findings
//! (`open`); the re-spawned implementer adjudicates each (`accepted`/`rejected`
//! with a rationale on the record); a human may `dismissed` one. Open findings
//! are what drive the `REVIEWING <-> FIXING` loop, so [`list_open`] is the
//! loop's read.

use crate::Result;
use anyhow::anyhow;
use serde::{Deserialize, Serialize};
use sqlx::sqlite::SqliteRow;
use sqlx::{Row, SqlitePool};

/// Reviewer-assigned severity. Matches the `findings.severity` CHECK domain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Blocker,
    Major,
    Minor,
    Nit,
}

impl Severity {
    pub fn as_str(&self) -> &'static str {
        match self {
            Severity::Blocker => "blocker",
            Severity::Major => "major",
            Severity::Minor => "minor",
            Severity::Nit => "nit",
        }
    }
    pub fn from_str(s: &str) -> Option<Self> {
        Some(match s {
            "blocker" => Severity::Blocker,
            "major" => Severity::Major,
            "minor" => Severity::Minor,
            "nit" => Severity::Nit,
            _ => return None,
        })
    }
}

/// Adjudication state. Matches the `findings.status` CHECK domain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FindingStatus {
    Open,
    Accepted,
    Rejected,
    Dismissed,
}

impl FindingStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            FindingStatus::Open => "open",
            FindingStatus::Accepted => "accepted",
            FindingStatus::Rejected => "rejected",
            FindingStatus::Dismissed => "dismissed",
        }
    }
    pub fn from_str(s: &str) -> Option<Self> {
        Some(match s {
            "open" => FindingStatus::Open,
            "accepted" => FindingStatus::Accepted,
            "rejected" => FindingStatus::Rejected,
            "dismissed" => FindingStatus::Dismissed,
            _ => return None,
        })
    }
}

/// Full `findings` row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    pub id: i64,
    pub issue_id: i64,
    pub review_round: i64,
    pub severity: Severity,
    pub lens: Option<String>,
    pub title: String,
    pub detail: Option<String>,
    pub file_ref: Option<String>,
    pub status: FindingStatus,
    pub adjudication: Option<String>,
    pub created_at: i64,
    pub resolved_at: Option<i64>,
}

impl Finding {
    fn from_row(row: &SqliteRow) -> Result<Self> {
        let severity_raw: String = row.try_get("severity")?;
        let status_raw: String = row.try_get("status")?;
        Ok(Finding {
            id: row.try_get("id")?,
            issue_id: row.try_get("issue_id")?,
            review_round: row.try_get("review_round")?,
            severity: Severity::from_str(&severity_raw)
                .ok_or_else(|| anyhow!("unknown finding severity {severity_raw:?} in db"))?,
            lens: row.try_get("lens")?,
            title: row.try_get("title")?,
            detail: row.try_get("detail")?,
            file_ref: row.try_get("file_ref")?,
            status: FindingStatus::from_str(&status_raw)
                .ok_or_else(|| anyhow!("unknown finding status {status_raw:?} in db"))?,
            adjudication: row.try_get("adjudication")?,
            created_at: row.try_get("created_at")?,
            resolved_at: row.try_get("resolved_at")?,
        })
    }
}

/// New finding inputs. `status` defaults to `open` at the DB.
#[derive(Debug, Clone)]
pub struct NewFinding<'a> {
    pub issue_id: i64,
    pub review_round: i64,
    pub severity: Severity,
    pub lens: Option<&'a str>,
    pub title: &'a str,
    pub detail: Option<&'a str>,
    pub file_ref: Option<&'a str>,
}

/// Append a finding (`status = open`). Returns the new id.
pub async fn add(pool: &SqlitePool, new: NewFinding<'_>, now: i64) -> Result<i64> {
    let id: i64 = sqlx::query(
        "INSERT INTO findings
            (issue_id, review_round, severity, lens, title, detail, file_ref, created_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)
         RETURNING id",
    )
    .bind(new.issue_id)
    .bind(new.review_round)
    .bind(new.severity.as_str())
    .bind(new.lens)
    .bind(new.title)
    .bind(new.detail)
    .bind(new.file_ref)
    .bind(now)
    .fetch_one(pool)
    .await?
    .get("id");
    Ok(id)
}

pub async fn get(pool: &SqlitePool, id: i64) -> Result<Option<Finding>> {
    let row = sqlx::query("SELECT * FROM findings WHERE id = ?")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    row.as_ref().map(Finding::from_row).transpose()
}

/// All findings for an issue, oldest first.
pub async fn list_by_issue(pool: &SqlitePool, issue_id: i64) -> Result<Vec<Finding>> {
    let rows = sqlx::query("SELECT * FROM findings WHERE issue_id = ? ORDER BY id")
        .bind(issue_id)
        .fetch_all(pool)
        .await?;
    rows.iter().map(Finding::from_row).collect()
}

/// Findings still `open` for an issue — the unresolved set the loop acts on.
pub async fn list_open(pool: &SqlitePool, issue_id: i64) -> Result<Vec<Finding>> {
    let rows =
        sqlx::query("SELECT * FROM findings WHERE issue_id = ? AND status = 'open' ORDER BY id")
            .bind(issue_id)
            .fetch_all(pool)
            .await?;
    rows.iter().map(Finding::from_row).collect()
}

/// Implementer accepts a finding (will fix), recording the rationale.
pub async fn accept(pool: &SqlitePool, id: i64, rationale: &str, now: i64) -> Result<()> {
    resolve(pool, id, FindingStatus::Accepted, Some(rationale), now).await
}

/// Implementer rejects a finding (won't fix), recording the rationale.
pub async fn reject(pool: &SqlitePool, id: i64, rationale: &str, now: i64) -> Result<()> {
    resolve(pool, id, FindingStatus::Rejected, Some(rationale), now).await
}

/// Human dismisses a finding (taken off the board, no implementer action).
pub async fn dismiss(pool: &SqlitePool, id: i64, now: i64) -> Result<()> {
    resolve(pool, id, FindingStatus::Dismissed, None, now).await
}

async fn resolve(
    pool: &SqlitePool,
    id: i64,
    status: FindingStatus,
    adjudication: Option<&str>,
    now: i64,
) -> Result<()> {
    let n = sqlx::query(
        "UPDATE findings SET status = ?, adjudication = ?, resolved_at = ? WHERE id = ?",
    )
    .bind(status.as_str())
    .bind(adjudication)
    .bind(now)
    .bind(id)
    .execute(pool)
    .await?
    .rows_affected();
    if n == 0 {
        return Err(anyhow!("finding {id} not found"));
    }
    Ok(())
}

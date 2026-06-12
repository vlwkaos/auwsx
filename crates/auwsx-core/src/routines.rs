//! Cron-driven routines. Plan Step 3.6.
//!
//! Routines are recurring prompts on the main-agent session. Distinct from
//! tasks (no worktree, no iteration, no feedback, no completion).
//!
//! Built-ins seeded per project (cron editable, prompt locked):
//!   - triage          — runs on every scheduler tick (Plan Step 3.7)
//!   - deepsleep       — weekly Sun 04:00
//!   - dream           — disabled by default
//!   - morning-summary — disabled by default
//!
//! User-defined routines: any cron + prompt template. Examples in plan.

use crate::Result;
use anyhow::anyhow;
use serde::{Deserialize, Serialize};
use sqlx::sqlite::SqliteRow;
use sqlx::{Row, SqlitePool};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoutineOrigin {
    Builtin,
    User,
}

impl RoutineOrigin {
    pub fn as_str(&self) -> &'static str {
        match self {
            RoutineOrigin::Builtin => "builtin",
            RoutineOrigin::User => "user",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        Some(match s {
            "builtin" => RoutineOrigin::Builtin,
            "user" => RoutineOrigin::User,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoutineType {
    Report,
    Idea,
    Knowledge,
}

impl RoutineType {
    pub fn as_str(&self) -> &'static str {
        match self {
            RoutineType::Report => "report",
            RoutineType::Idea => "idea",
            RoutineType::Knowledge => "knowledge",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        Some(match s {
            "report" => RoutineType::Report,
            "idea" => RoutineType::Idea,
            "knowledge" => RoutineType::Knowledge,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Routine {
    pub id: i64,
    pub project_id: i64,
    pub name: String,
    pub origin: RoutineOrigin,
    pub routine_type: RoutineType,
    pub prompt: String,
    pub cron: String,
    pub writable_paths: Option<String>,
    pub enabled: bool,
    pub last_run_at: Option<i64>,
    pub next_run_at: Option<i64>,
    pub created_at: i64,
}

#[derive(Debug, Clone)]
pub struct NewRoutine<'a> {
    pub project_id: i64,
    pub name: &'a str,
    pub routine_type: RoutineType,
    pub prompt: &'a str,
    pub cron: &'a str,
    pub writable_paths: Option<&'a str>,
    pub enabled: bool,
}

#[derive(Debug, Clone)]
pub struct UpdateRoutine<'a> {
    pub name: &'a str,
    pub routine_type: RoutineType,
    pub prompt: &'a str,
    pub cron: &'a str,
    pub writable_paths: Option<&'a str>,
    pub enabled: bool,
}

impl Routine {
    fn from_row(row: &SqliteRow) -> Result<Self> {
        let origin_raw: String = row.try_get("origin")?;
        let type_raw: String = row.try_get("type")?;
        Ok(Routine {
            id: row.try_get("id")?,
            project_id: row.try_get("project_id")?,
            name: row.try_get("name")?,
            origin: RoutineOrigin::from_str(&origin_raw)
                .ok_or_else(|| anyhow!("unknown routine origin {origin_raw:?} in db"))?,
            routine_type: RoutineType::from_str(&type_raw)
                .ok_or_else(|| anyhow!("unknown routine type {type_raw:?} in db"))?,
            prompt: row.try_get("prompt")?,
            cron: row.try_get("cron")?,
            writable_paths: row.try_get("writable_paths")?,
            enabled: row.try_get::<i64, _>("enabled")? != 0,
            last_run_at: row.try_get("last_run_at")?,
            next_run_at: row.try_get("next_run_at")?,
            created_at: row.try_get("created_at")?,
        })
    }
}

pub async fn create(pool: &SqlitePool, new: NewRoutine<'_>, now: i64) -> Result<i64> {
    let id: i64 = sqlx::query(
        "INSERT INTO routines
            (project_id, name, origin, type, prompt, cron, writable_paths, enabled, created_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
         RETURNING id",
    )
    .bind(new.project_id)
    .bind(new.name)
    .bind(RoutineOrigin::User.as_str())
    .bind(new.routine_type.as_str())
    .bind(new.prompt)
    .bind(new.cron)
    .bind(new.writable_paths)
    .bind(if new.enabled { 1 } else { 0 })
    .bind(now)
    .fetch_one(pool)
    .await?
    .get("id");
    Ok(id)
}

pub async fn list_by_project(pool: &SqlitePool, project_id: i64) -> Result<Vec<Routine>> {
    let rows = sqlx::query("SELECT * FROM routines WHERE project_id = ? ORDER BY id")
        .bind(project_id)
        .fetch_all(pool)
        .await?;
    rows.iter().map(Routine::from_row).collect()
}

pub async fn get(pool: &SqlitePool, id: i64) -> Result<Option<Routine>> {
    let row = sqlx::query("SELECT * FROM routines WHERE id = ?")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    row.as_ref().map(Routine::from_row).transpose()
}

pub async fn set_enabled(pool: &SqlitePool, id: i64, enabled: bool) -> Result<()> {
    let n = sqlx::query("UPDATE routines SET enabled = ? WHERE id = ?")
        .bind(if enabled { 1 } else { 0 })
        .bind(id)
        .execute(pool)
        .await?
        .rows_affected();
    if n == 0 {
        return Err(anyhow!("routine {id} not found"));
    }
    Ok(())
}

pub async fn update(pool: &SqlitePool, id: i64, update: UpdateRoutine<'_>) -> Result<()> {
    let n = sqlx::query(
        "UPDATE routines SET
            name = ?,
            type = ?,
            prompt = ?,
            cron = ?,
            writable_paths = ?,
            enabled = ?
         WHERE id = ?",
    )
    .bind(update.name)
    .bind(update.routine_type.as_str())
    .bind(update.prompt)
    .bind(update.cron)
    .bind(update.writable_paths)
    .bind(if update.enabled { 1 } else { 0 })
    .bind(id)
    .execute(pool)
    .await?
    .rows_affected();
    if n == 0 {
        return Err(anyhow!("routine {id} not found"));
    }
    Ok(())
}

pub async fn mark_ran(pool: &SqlitePool, id: i64, ran_at: i64) -> Result<()> {
    let n = sqlx::query("UPDATE routines SET last_run_at = ? WHERE id = ?")
        .bind(ran_at)
        .bind(id)
        .execute(pool)
        .await?
        .rows_affected();
    if n == 0 {
        return Err(anyhow!("routine {id} not found"));
    }
    Ok(())
}

// TODO: built-in routine definitions table (triage / deepsleep / dream / morning-summary)
// TODO: next_run calculation via `cron` crate
// TODO: output_target templating ({date}, {datetime})
// TODO: seed_builtins(project_id) — non-overwriting INSERT IGNORE

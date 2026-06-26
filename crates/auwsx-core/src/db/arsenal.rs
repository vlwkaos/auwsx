//! Global Arsenal presets for per-role agent command templates.
//!
//! Projects may link to a preset and store only per-role command overrides.
//! Project queries resolve effective commands from overrides first, then Arsenal.

use crate::Result;
use anyhow::anyhow;
use serde::{Deserialize, Serialize};
use sqlx::sqlite::SqliteRow;
use sqlx::{Row, SqlitePool};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArsenalPreset {
    pub id: i64,
    pub name: String,
    pub main_agent_cmd: String,
    pub plan_agent_cmd: String,
    pub work_agent_cmd: String,
    pub review_agent_cmd: Option<String>,
    pub builtin: bool,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone)]
pub struct NewArsenalPreset<'a> {
    pub name: &'a str,
    pub main_agent_cmd: &'a str,
    pub plan_agent_cmd: &'a str,
    pub work_agent_cmd: &'a str,
    pub review_agent_cmd: Option<&'a str>,
}

impl ArsenalPreset {
    fn from_row(row: &SqliteRow) -> Result<Self> {
        Ok(Self {
            id: row.try_get("id")?,
            name: row.try_get("name")?,
            main_agent_cmd: row.try_get("main_agent_cmd")?,
            plan_agent_cmd: row.try_get("plan_agent_cmd")?,
            work_agent_cmd: row.try_get("work_agent_cmd")?,
            review_agent_cmd: row.try_get("review_agent_cmd")?,
            builtin: row.try_get::<i64, _>("builtin")? != 0,
            created_at: row.try_get("created_at")?,
            updated_at: row.try_get("updated_at")?,
        })
    }
}

pub async fn list(pool: &SqlitePool) -> Result<Vec<ArsenalPreset>> {
    let rows = sqlx::query("SELECT * FROM arsenal_agent_presets ORDER BY builtin DESC, name")
        .fetch_all(pool)
        .await?;
    rows.iter().map(ArsenalPreset::from_row).collect()
}

pub async fn get_by_name(pool: &SqlitePool, name: &str) -> Result<Option<ArsenalPreset>> {
    let row = sqlx::query("SELECT * FROM arsenal_agent_presets WHERE name = ?")
        .bind(name)
        .fetch_optional(pool)
        .await?;
    row.as_ref().map(ArsenalPreset::from_row).transpose()
}

pub async fn upsert(pool: &SqlitePool, preset: NewArsenalPreset<'_>, now: i64) -> Result<i64> {
    if preset.name.trim().is_empty() {
        return Err(anyhow!("arsenal preset name is required"));
    }
    if preset.main_agent_cmd.trim().is_empty()
        || preset.plan_agent_cmd.trim().is_empty()
        || preset.work_agent_cmd.trim().is_empty()
    {
        return Err(anyhow!("main, plan, and work agent commands are required"));
    }

    let id: i64 = sqlx::query(
        "INSERT INTO arsenal_agent_presets
            (name, main_agent_cmd, plan_agent_cmd, work_agent_cmd, review_agent_cmd,
             builtin, created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, 0, ?, ?)
         ON CONFLICT(name) DO UPDATE SET
            main_agent_cmd = excluded.main_agent_cmd,
            plan_agent_cmd = excluded.plan_agent_cmd,
            work_agent_cmd = excluded.work_agent_cmd,
            review_agent_cmd = excluded.review_agent_cmd,
            builtin = 0,
            updated_at = excluded.updated_at
         RETURNING id",
    )
    .bind(preset.name.trim())
    .bind(preset.main_agent_cmd)
    .bind(preset.plan_agent_cmd)
    .bind(preset.work_agent_cmd)
    .bind(preset.review_agent_cmd)
    .bind(now)
    .bind(now)
    .fetch_one(pool)
    .await?
    .get("id");
    Ok(id)
}

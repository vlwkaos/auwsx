//! Global Memory presets for durable context operations.
//!
//! Memory presets mirror Arsenal's shape: a project/runtime selects a named
//! global preset, and each memory interface resolves to either auwsx's portable
//! internal store, an external command, or the packaged auwsx skill workflow.

use crate::Result;
use anyhow::{anyhow, ensure};
use serde::{Deserialize, Serialize};
use sqlx::sqlite::SqliteRow;
use sqlx::{Row, SqlitePool};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryPreset {
    pub id: i64,
    pub name: String,
    pub retrieve_kind: String,
    pub retrieve_cmd: Option<String>,
    pub save_kind: String,
    pub save_cmd: Option<String>,
    pub dream_kind: String,
    pub dream_cmd: Option<String>,
    pub deepsleep_kind: String,
    pub deepsleep_cmd: Option<String>,
    pub builtin: bool,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone)]
pub struct NewMemoryPreset<'a> {
    pub name: &'a str,
    pub retrieve_kind: &'a str,
    pub retrieve_cmd: Option<&'a str>,
    pub save_kind: &'a str,
    pub save_cmd: Option<&'a str>,
    pub dream_kind: &'a str,
    pub dream_cmd: Option<&'a str>,
    pub deepsleep_kind: &'a str,
    pub deepsleep_cmd: Option<&'a str>,
}

impl MemoryPreset {
    fn from_row(row: &SqliteRow) -> Result<Self> {
        Ok(Self {
            id: row.try_get("id")?,
            name: row.try_get("name")?,
            retrieve_kind: row.try_get("retrieve_kind")?,
            retrieve_cmd: row.try_get("retrieve_cmd")?,
            save_kind: row.try_get("save_kind")?,
            save_cmd: row.try_get("save_cmd")?,
            dream_kind: row.try_get("dream_kind")?,
            dream_cmd: row.try_get("dream_cmd")?,
            deepsleep_kind: row.try_get("deepsleep_kind")?,
            deepsleep_cmd: row.try_get("deepsleep_cmd")?,
            builtin: row.try_get::<i64, _>("builtin")? != 0,
            created_at: row.try_get("created_at")?,
            updated_at: row.try_get("updated_at")?,
        })
    }
}

pub async fn list(pool: &SqlitePool) -> Result<Vec<MemoryPreset>> {
    let rows = sqlx::query("SELECT * FROM memory_presets ORDER BY builtin DESC, name")
        .fetch_all(pool)
        .await?;
    rows.iter().map(MemoryPreset::from_row).collect()
}

pub async fn get_by_name(pool: &SqlitePool, name: &str) -> Result<Option<MemoryPreset>> {
    let row = sqlx::query("SELECT * FROM memory_presets WHERE name = ?")
        .bind(name)
        .fetch_optional(pool)
        .await?;
    row.as_ref().map(MemoryPreset::from_row).transpose()
}

pub async fn upsert(pool: &SqlitePool, preset: NewMemoryPreset<'_>, now: i64) -> Result<i64> {
    validate(&preset)?;
    let id: i64 = sqlx::query(
        "INSERT INTO memory_presets
            (name, retrieve_kind, retrieve_cmd, save_kind, save_cmd, dream_kind, dream_cmd,
             deepsleep_kind, deepsleep_cmd, builtin, created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, 0, ?, ?)
         ON CONFLICT(name) DO UPDATE SET
            retrieve_kind = excluded.retrieve_kind,
            retrieve_cmd = excluded.retrieve_cmd,
            save_kind = excluded.save_kind,
            save_cmd = excluded.save_cmd,
            dream_kind = excluded.dream_kind,
            dream_cmd = excluded.dream_cmd,
            deepsleep_kind = excluded.deepsleep_kind,
            deepsleep_cmd = excluded.deepsleep_cmd,
            builtin = 0,
            updated_at = excluded.updated_at
         RETURNING id",
    )
    .bind(preset.name.trim())
    .bind(preset.retrieve_kind)
    .bind(preset.retrieve_cmd)
    .bind(preset.save_kind)
    .bind(preset.save_cmd)
    .bind(preset.dream_kind)
    .bind(preset.dream_cmd)
    .bind(preset.deepsleep_kind)
    .bind(preset.deepsleep_cmd)
    .bind(now)
    .bind(now)
    .fetch_one(pool)
    .await?
    .get("id");
    Ok(id)
}

fn validate(preset: &NewMemoryPreset<'_>) -> Result<()> {
    if preset.name.trim().is_empty() {
        return Err(anyhow!("memory preset name is required"));
    }
    for (label, kind, cmd) in [
        ("retrieve", preset.retrieve_kind, preset.retrieve_cmd),
        ("save", preset.save_kind, preset.save_cmd),
        ("dream", preset.dream_kind, preset.dream_cmd),
        ("deepsleep", preset.deepsleep_kind, preset.deepsleep_cmd),
    ] {
        ensure!(
            matches!(kind, "portable" | "command" | "auwsx_skill"),
            "{label} kind must be portable, command, or auwsx_skill"
        );
        if kind == "command" {
            ensure!(
                cmd.map(|value| !value.trim().is_empty()).unwrap_or(false),
                "{label} command is required when kind is command"
            );
        }
    }
    Ok(())
}

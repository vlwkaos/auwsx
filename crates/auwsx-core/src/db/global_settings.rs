//! Global auwsx settings that are not project execution policy.

use crate::Result;
use anyhow::{anyhow, ensure};
use serde::{Deserialize, Serialize};
use sqlx::sqlite::SqliteRow;
use sqlx::{Row, SqlitePool};

pub const PIPELINE_UX_GUIDANCE_MAX_CHARS: usize = 16 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GlobalSettings {
    pub memory_preset_name: String,
    /// Legacy alias kept for older callers while the UI moves to Memory presets.
    pub memory_provider: String,
    pub pipeline_ux_guidance: String,
    pub updated_at: i64,
}

impl GlobalSettings {
    fn from_row(row: &SqliteRow) -> Result<Self> {
        let memory_preset_name: String = row.try_get("memory_preset_name")?;
        Ok(Self {
            memory_preset_name: memory_preset_name.clone(),
            memory_provider: row.try_get("memory_provider")?,
            pipeline_ux_guidance: row.try_get("pipeline_ux_guidance")?,
            updated_at: row.try_get("updated_at")?,
        })
    }
}

pub async fn get(pool: &SqlitePool) -> Result<GlobalSettings> {
    let row = sqlx::query(
        "SELECT memory_preset_name, memory_provider, pipeline_ux_guidance, updated_at
         FROM global_settings WHERE id = 1",
    )
    .fetch_one(pool)
    .await?;
    GlobalSettings::from_row(&row)
}

pub async fn update(
    pool: &SqlitePool,
    memory_preset_name: &str,
    pipeline_ux_guidance: &str,
    now: i64,
) -> Result<()> {
    let memory_preset_name = memory_preset_name.trim();
    ensure!(
        !memory_preset_name.is_empty(),
        "memory preset name is required"
    );
    let preset_exists: Option<i64> =
        sqlx::query_scalar("SELECT id FROM memory_presets WHERE name = ?")
            .bind(memory_preset_name)
            .fetch_optional(pool)
            .await?;
    ensure!(
        preset_exists.is_some(),
        "unknown memory preset {memory_preset_name}"
    );
    let trimmed = pipeline_ux_guidance.trim();
    ensure!(
        trimmed.chars().count() <= PIPELINE_UX_GUIDANCE_MAX_CHARS,
        "pipeline UX guidance must be at most {} characters",
        PIPELINE_UX_GUIDANCE_MAX_CHARS
    );
    let result = sqlx::query(
        "UPDATE global_settings
         SET memory_preset_name = ?, pipeline_ux_guidance = ?, updated_at = ?
         WHERE id = 1",
    )
    .bind(memory_preset_name)
    .bind(trimmed)
    .bind(now)
    .execute(pool)
    .await?;
    if result.rows_affected() != 1 {
        return Err(anyhow!(
            "global settings singleton missing; expected one row to update"
        ));
    }
    Ok(())
}

pub async fn update_pipeline_ux_guidance(
    pool: &SqlitePool,
    pipeline_ux_guidance: &str,
    now: i64,
) -> Result<()> {
    let trimmed = pipeline_ux_guidance.trim();
    ensure!(
        trimmed.chars().count() <= PIPELINE_UX_GUIDANCE_MAX_CHARS,
        "pipeline UX guidance must be at most {} characters",
        PIPELINE_UX_GUIDANCE_MAX_CHARS
    );
    let result = sqlx::query(
        "UPDATE global_settings
         SET pipeline_ux_guidance = ?, updated_at = ?
         WHERE id = 1",
    )
    .bind(trimmed)
    .bind(now)
    .execute(pool)
    .await?;
    if result.rows_affected() != 1 {
        return Err(anyhow!(
            "global settings singleton missing; expected one row to update"
        ));
    }
    Ok(())
}

//! `projects` typed row + CRUD. Schema: src/db/migrations/0001_init.sql.
//!
//! A project is the top-level unit the scheduler enumerates. Most of its
//! columns are policy knobs with SQL-side DEFAULTs; [`create`] supplies only the
//! NOT-NULL-without-default columns and lets the DB fill the rest, so the
//! default policy lives in exactly one place (the migration).

use crate::Result;
use anyhow::anyhow;
use serde::{Deserialize, Serialize};
use sqlx::sqlite::SqliteRow;
use sqlx::{Row, SqlitePool};

/// Integration method for a finished issue's branch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MergeMode {
    /// Rebase onto current default branch + single `--no-ff` merge commit.
    Local,
    /// Open a PR instead of merging locally.
    Pr,
}

impl MergeMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            MergeMode::Local => "local",
            MergeMode::Pr => "pr",
        }
    }
    pub fn from_str(s: &str) -> Option<Self> {
        Some(match s {
            "local" => MergeMode::Local,
            "pr" => MergeMode::Pr,
            _ => return None,
        })
    }
}

/// Gate policy for `READY_TO_MERGE -> MERGING`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompletionPolicy {
    /// Hard gate: a human must release the issue (default).
    Manual,
    /// Soft gate: auto-release after `completion_soft_timeout_min`.
    Soft,
    /// No gate: advance immediately.
    Auto,
}

impl CompletionPolicy {
    pub fn as_str(&self) -> &'static str {
        match self {
            CompletionPolicy::Manual => "manual",
            CompletionPolicy::Soft => "soft",
            CompletionPolicy::Auto => "auto",
        }
    }
    pub fn from_str(s: &str) -> Option<Self> {
        Some(match s {
            "manual" => CompletionPolicy::Manual,
            "soft" => CompletionPolicy::Soft,
            "auto" => CompletionPolicy::Auto,
            _ => return None,
        })
    }
}

/// Full `projects` row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Project {
    pub id: i64,
    pub profile_id: i64,
    pub profile_order: i64,
    pub name: String,
    pub repo_path: String,
    pub default_branch: String,

    pub arsenal_preset_name: Option<String>,
    pub main_agent_cmd: String,
    #[serde(default)]
    pub route_agent_cmd: String,
    pub plan_agent_cmd: String,
    pub work_agent_cmd: String,
    pub review_agent_cmd: Option<String>,
    pub main_agent_cmd_override: Option<String>,
    #[serde(default)]
    pub route_agent_cmd_override: Option<String>,
    pub plan_agent_cmd_override: Option<String>,
    pub work_agent_cmd_override: Option<String>,
    pub review_agent_cmd_override: Option<String>,

    pub completion_policy: CompletionPolicy,
    pub completion_soft_timeout_min: i64,
    pub plan_gate_timeout_min: i64,
    pub iteration_timeout_min: i64,
    pub main_job_timeout_min: i64,
    pub review_max_rounds: i64,
    pub conflict_max_attempts: i64,
    pub max_concurrency: i64,
    pub schedule_interval_min: Option<i64>,
    pub schedule_cron: Option<String>,
    pub merge_mode: MergeMode,
    pub skill_path: Option<String>,
    pub deepsleep_interval_days: i64,
    pub deepsleep_cron: Option<String>,
    pub last_deepsleep_at: Option<i64>,
    pub created_at: i64,
}

/// Required inputs for [`create`]; every other column takes its SQL DEFAULT.
///
/// The three `Option` policy fields are overrides: `None` leaves the migration's
/// DEFAULT in place (so the default lives in exactly one place), `Some` replaces
/// it. They are the knobs an autonomous run needs to clear its soft gates
/// (`completion_policy=auto` + `plan_gate_timeout_min=0` ⇒ no human stop).
#[derive(Debug, Clone)]
pub struct NewProject<'a> {
    pub name: &'a str,
    pub repo_path: &'a str,
    pub default_branch: &'a str,
    /// Linked Arsenal preset. Commands below are per-project overrides when set.
    pub arsenal_preset_name: Option<&'a str>,
    pub main_agent_cmd: &'a str,
    pub route_agent_cmd: &'a str,
    pub plan_agent_cmd: &'a str,
    pub work_agent_cmd: &'a str,
    /// NULL falls back to `work_agent_cmd` at spawn (still a fresh third-eye).
    pub review_agent_cmd: Option<&'a str>,
    /// Gate policy for `READY_TO_MERGE -> MERGING`. None ⇒ DB default (`manual`).
    pub completion_policy: Option<CompletionPolicy>,
    /// Soft-release delay for `PLAN_READY -> WORKING`. None ⇒ DB default.
    pub plan_gate_timeout_min: Option<i64>,
    /// Soft-release delay for `READY_TO_MERGE -> MERGING` under `soft`. None ⇒ DB default.
    pub completion_soft_timeout_min: Option<i64>,
    /// Legacy autonomous cadence in minutes. `schedule_cron` is canonical.
    pub schedule_interval_min: Option<i64>,
    /// User-facing autonomous cadence. None ⇒ DB default/manual-only.
    pub schedule_cron: Option<&'a str>,
}

#[derive(Debug, Clone)]
pub struct UpdateProject<'a> {
    pub name: &'a str,
    pub repo_path: &'a str,
    pub default_branch: &'a str,
    /// Linked Arsenal preset. Commands below are per-project overrides when set.
    pub arsenal_preset_name: Option<&'a str>,
    pub main_agent_cmd: &'a str,
    pub route_agent_cmd: &'a str,
    pub plan_agent_cmd: &'a str,
    pub work_agent_cmd: &'a str,
    pub review_agent_cmd: Option<&'a str>,
    pub completion_policy: CompletionPolicy,
    pub plan_gate_timeout_min: i64,
    pub completion_soft_timeout_min: i64,
    pub iteration_timeout_min: i64,
    pub main_job_timeout_min: i64,
    pub review_max_rounds: i64,
    pub conflict_max_attempts: i64,
    pub max_concurrency: i64,
    pub schedule_interval_min: Option<i64>,
    pub schedule_cron: Option<&'a str>,
    pub merge_mode: MergeMode,
    pub skill_path: Option<&'a str>,
    pub deepsleep_interval_days: i64,
    pub deepsleep_cron: Option<&'a str>,
}

impl Project {
    /// The agent command template for a phase role. `Review` falls back to the
    /// work command when `review_agent_cmd` is unset (a fresh work-agent session
    /// is still a valid third eye).
    pub fn agent_cmd_for(&self, role: crate::db::agent_runs::Role) -> &str {
        use crate::db::agent_runs::Role;
        match role {
            Role::Main => &self.main_agent_cmd,
            Role::Plan => &self.plan_agent_cmd,
            Role::Work => &self.work_agent_cmd,
            Role::Review => self
                .review_agent_cmd
                .as_deref()
                .unwrap_or(&self.work_agent_cmd),
        }
    }

    pub fn route_agent_cmd(&self) -> &str {
        &self.route_agent_cmd
    }

    fn from_row(row: &SqliteRow) -> Result<Self> {
        let merge_raw: String = row.try_get("merge_mode")?;
        let policy_raw: String = row.try_get("completion_policy")?;
        Ok(Project {
            id: row.try_get("id")?,
            profile_id: row.try_get("profile_id")?,
            profile_order: row.try_get("profile_order")?,
            name: row.try_get("name")?,
            repo_path: row.try_get("repo_path")?,
            default_branch: row.try_get("default_branch")?,
            arsenal_preset_name: row.try_get("arsenal_preset_name")?,
            main_agent_cmd: row.try_get("main_agent_cmd")?,
            route_agent_cmd: row.try_get("route_agent_cmd")?,
            plan_agent_cmd: row.try_get("plan_agent_cmd")?,
            work_agent_cmd: row.try_get("work_agent_cmd")?,
            review_agent_cmd: row.try_get("review_agent_cmd")?,
            main_agent_cmd_override: row.try_get("main_agent_cmd_override")?,
            route_agent_cmd_override: row.try_get("route_agent_cmd_override")?,
            plan_agent_cmd_override: row.try_get("plan_agent_cmd_override")?,
            work_agent_cmd_override: row.try_get("work_agent_cmd_override")?,
            review_agent_cmd_override: row.try_get("review_agent_cmd_override")?,
            completion_policy: CompletionPolicy::from_str(&policy_raw)
                .ok_or_else(|| anyhow!("unknown completion_policy {policy_raw:?} in db"))?,
            completion_soft_timeout_min: row.try_get("completion_soft_timeout_min")?,
            plan_gate_timeout_min: row.try_get("plan_gate_timeout_min")?,
            iteration_timeout_min: row.try_get("iteration_timeout_min")?,
            main_job_timeout_min: row.try_get("main_job_timeout_min")?,
            review_max_rounds: row.try_get("review_max_rounds")?,
            conflict_max_attempts: row.try_get("conflict_max_attempts")?,
            max_concurrency: row.try_get("max_concurrency")?,
            schedule_interval_min: row.try_get("schedule_interval_min")?,
            schedule_cron: row.try_get("schedule_cron")?,
            merge_mode: MergeMode::from_str(&merge_raw)
                .ok_or_else(|| anyhow!("unknown merge_mode {merge_raw:?} in db"))?,
            skill_path: row.try_get("skill_path")?,
            deepsleep_interval_days: row.try_get("deepsleep_interval_days")?,
            deepsleep_cron: row.try_get("deepsleep_cron")?,
            last_deepsleep_at: row.try_get("last_deepsleep_at")?,
            created_at: row.try_get("created_at")?,
        })
    }
}

const DEFAULT_MAX_CONCURRENCY: i64 = 3;

const PROJECT_SELECT: &str = "
    SELECT
        p.id,
        p.profile_id,
        p.profile_order,
        p.name,
        p.repo_path,
        p.default_branch,
        p.arsenal_preset_name,
        COALESCE(NULLIF(p.main_agent_cmd, ''), a.main_agent_cmd) AS main_agent_cmd,
        COALESCE(NULLIF(p.route_agent_cmd, ''), a.route_agent_cmd, NULLIF(p.work_agent_cmd, ''), a.work_agent_cmd) AS route_agent_cmd,
        COALESCE(NULLIF(p.plan_agent_cmd, ''), a.plan_agent_cmd) AS plan_agent_cmd,
        COALESCE(NULLIF(p.work_agent_cmd, ''), a.work_agent_cmd) AS work_agent_cmd,
        COALESCE(NULLIF(p.review_agent_cmd, ''), a.review_agent_cmd) AS review_agent_cmd,
        NULLIF(p.main_agent_cmd, '') AS main_agent_cmd_override,
        NULLIF(p.route_agent_cmd, '') AS route_agent_cmd_override,
        NULLIF(p.plan_agent_cmd, '') AS plan_agent_cmd_override,
        NULLIF(p.work_agent_cmd, '') AS work_agent_cmd_override,
        NULLIF(p.review_agent_cmd, '') AS review_agent_cmd_override,
        p.completion_policy,
        p.completion_soft_timeout_min,
        p.plan_gate_timeout_min,
        p.iteration_timeout_min,
        p.main_job_timeout_min,
        p.review_max_rounds,
        p.conflict_max_attempts,
        p.max_concurrency,
        p.schedule_interval_min,
        p.schedule_cron,
        p.merge_mode,
        p.skill_path,
        p.deepsleep_interval_days,
        p.deepsleep_cron,
        p.last_deepsleep_at,
        p.created_at
    FROM projects p
    LEFT JOIN arsenal_agent_presets a ON a.name = p.arsenal_preset_name";

fn normalize_optional(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn normalize_cadence(value: Option<&str>) -> Result<Option<String>> {
    match value {
        Some(raw) => crate::schedule::normalize_cadence_input(raw),
        None => Ok(None),
    }
}

async fn validate_agent_source(
    pool: &SqlitePool,
    arsenal_preset_name: Option<&str>,
    main_agent_cmd: &str,
    route_agent_cmd: &str,
    plan_agent_cmd: &str,
    work_agent_cmd: &str,
) -> Result<Option<String>> {
    let arsenal_preset_name = normalize_optional(arsenal_preset_name);
    if let Some(name) = &arsenal_preset_name {
        let exists: Option<i64> =
            sqlx::query_scalar("SELECT id FROM arsenal_agent_presets WHERE name = ?")
                .bind(name)
                .fetch_optional(pool)
                .await?;
        if exists.is_none() {
            return Err(anyhow!("unknown Arsenal preset {name:?}"));
        }
    } else if main_agent_cmd.trim().is_empty()
        || route_agent_cmd.trim().is_empty()
        || plan_agent_cmd.trim().is_empty()
        || work_agent_cmd.trim().is_empty()
    {
        return Err(anyhow!(
            "main, route, plan, and work commands are required without an Arsenal preset"
        ));
    }
    Ok(arsenal_preset_name)
}

/// Insert a project. Required columns are supplied; policy columns take their
/// SQL DEFAULT unless the matching `NewProject` override is `Some`. Operational
/// defaults that may change after `0001_init.sql` is applied are set here and by
/// follow-up migrations, so existing installs do not need a checksum-breaking
/// edit to the initial migration. Returns the new id.
pub async fn create(pool: &SqlitePool, new: NewProject<'_>, now: i64) -> Result<i64> {
    let arsenal_preset_name = validate_agent_source(
        pool,
        new.arsenal_preset_name,
        new.main_agent_cmd,
        new.route_agent_cmd,
        new.plan_agent_cmd,
        new.work_agent_cmd,
    )
    .await?;
    let schedule_cron = normalize_cadence(new.schedule_cron)?;
    let id: i64 = sqlx::query(
        "INSERT INTO projects
            (profile_id, profile_order, name, repo_path, default_branch,
             arsenal_preset_name, main_agent_cmd, route_agent_cmd, plan_agent_cmd, work_agent_cmd, review_agent_cmd,
             max_concurrency, created_at)
         VALUES (1, (SELECT COALESCE(MAX(profile_order), 0) + 1 FROM projects WHERE profile_id = 1), ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
         RETURNING id",
    )
    .bind(new.name)
    .bind(new.repo_path)
    .bind(new.default_branch)
    .bind(arsenal_preset_name)
    .bind(new.main_agent_cmd)
    .bind(new.route_agent_cmd)
    .bind(new.plan_agent_cmd)
    .bind(new.work_agent_cmd)
    .bind(new.review_agent_cmd)
    .bind(DEFAULT_MAX_CONCURRENCY)
    .bind(now)
    .fetch_one(pool)
    .await?
    .get("id");

    // Apply policy overrides only when at least one is set — COALESCE leaves
    // unspecified columns at their DEFAULT.
    if new.completion_policy.is_some()
        || new.plan_gate_timeout_min.is_some()
        || new.completion_soft_timeout_min.is_some()
        || new.schedule_interval_min.is_some()
        || schedule_cron.is_some()
    {
        sqlx::query(
            "UPDATE projects SET
                completion_policy = COALESCE(?, completion_policy),
                plan_gate_timeout_min = COALESCE(?, plan_gate_timeout_min),
                completion_soft_timeout_min = COALESCE(?, completion_soft_timeout_min),
                schedule_interval_min = COALESCE(?, schedule_interval_min),
                schedule_cron = COALESCE(?, schedule_cron)
             WHERE id = ?",
        )
        .bind(new.completion_policy.map(|p| p.as_str()))
        .bind(new.plan_gate_timeout_min)
        .bind(new.completion_soft_timeout_min)
        .bind(new.schedule_interval_min)
        .bind(schedule_cron)
        .bind(id)
        .execute(pool)
        .await?;
    }
    Ok(id)
}

pub async fn get(pool: &SqlitePool, id: i64) -> Result<Option<Project>> {
    let sql = format!("{PROJECT_SELECT} WHERE p.id = ?");
    let row = sqlx::query(&sql).bind(id).fetch_optional(pool).await?;
    row.as_ref().map(Project::from_row).transpose()
}

pub async fn get_by_name(pool: &SqlitePool, name: &str) -> Result<Option<Project>> {
    let sql = format!("{PROJECT_SELECT} WHERE p.name = ?");
    let row = sqlx::query(&sql).bind(name).fetch_optional(pool).await?;
    row.as_ref().map(Project::from_row).transpose()
}

pub async fn update(pool: &SqlitePool, id: i64, update: UpdateProject<'_>) -> Result<()> {
    let arsenal_preset_name = validate_agent_source(
        pool,
        update.arsenal_preset_name,
        update.main_agent_cmd,
        update.route_agent_cmd,
        update.plan_agent_cmd,
        update.work_agent_cmd,
    )
    .await?;
    let schedule_cron = normalize_cadence(update.schedule_cron)?;
    let deepsleep_cron = normalize_cadence(update.deepsleep_cron)?;
    let n = sqlx::query(
        "UPDATE projects SET
            name = ?,
            repo_path = ?,
            default_branch = ?,
            arsenal_preset_name = ?,
            main_agent_cmd = ?,
            route_agent_cmd = ?,
            plan_agent_cmd = ?,
            work_agent_cmd = ?,
            review_agent_cmd = ?,
            completion_policy = ?,
            plan_gate_timeout_min = ?,
            completion_soft_timeout_min = ?,
            iteration_timeout_min = ?,
            main_job_timeout_min = ?,
            review_max_rounds = ?,
            conflict_max_attempts = ?,
            max_concurrency = ?,
            schedule_interval_min = ?,
            schedule_cron = ?,
            merge_mode = ?,
            skill_path = ?,
            deepsleep_interval_days = ?,
            deepsleep_cron = ?
         WHERE id = ?",
    )
    .bind(update.name)
    .bind(update.repo_path)
    .bind(update.default_branch)
    .bind(arsenal_preset_name)
    .bind(update.main_agent_cmd)
    .bind(update.route_agent_cmd)
    .bind(update.plan_agent_cmd)
    .bind(update.work_agent_cmd)
    .bind(update.review_agent_cmd)
    .bind(update.completion_policy.as_str())
    .bind(update.plan_gate_timeout_min)
    .bind(update.completion_soft_timeout_min)
    .bind(update.iteration_timeout_min)
    .bind(update.main_job_timeout_min)
    .bind(update.review_max_rounds)
    .bind(update.conflict_max_attempts)
    .bind(update.max_concurrency)
    .bind(update.schedule_interval_min)
    .bind(schedule_cron)
    .bind(update.merge_mode.as_str())
    .bind(update.skill_path)
    .bind(update.deepsleep_interval_days)
    .bind(deepsleep_cron)
    .bind(id)
    .execute(pool)
    .await?
    .rows_affected();
    if n == 0 {
        return Err(anyhow!("project {id} not found"));
    }
    Ok(())
}

pub async fn mark_deepsleep_ran(pool: &SqlitePool, id: i64, ran_at: i64) -> Result<()> {
    let n = sqlx::query("UPDATE projects SET last_deepsleep_at = ? WHERE id = ?")
        .bind(ran_at)
        .bind(id)
        .execute(pool)
        .await?
        .rows_affected();
    if n == 0 {
        return Err(anyhow!("project {id} not found"));
    }
    Ok(())
}

/// All projects, oldest first.
pub async fn list(pool: &SqlitePool) -> Result<Vec<Project>> {
    let sql = format!("{PROJECT_SELECT} ORDER BY p.profile_id, p.profile_order, p.id");
    let rows = sqlx::query(&sql).fetch_all(pool).await?;
    rows.iter().map(Project::from_row).collect()
}

pub async fn move_to_profile(pool: &SqlitePool, project_id: i64, profile_id: i64) -> Result<()> {
    let next_order: i64 = sqlx::query_scalar(
        "SELECT COALESCE(MAX(profile_order), 0) + 1 FROM projects WHERE profile_id = ?",
    )
    .bind(profile_id)
    .fetch_one(pool)
    .await?;
    let n = sqlx::query("UPDATE projects SET profile_id = ?, profile_order = ? WHERE id = ?")
        .bind(profile_id)
        .bind(next_order)
        .bind(project_id)
        .execute(pool)
        .await?
        .rows_affected();
    if n == 0 {
        return Err(anyhow!("project {project_id} not found"));
    }
    Ok(())
}

pub async fn move_within_profile(pool: &SqlitePool, project_id: i64, delta: isize) -> Result<()> {
    if delta == 0 {
        return Ok(());
    }
    let project = get(pool, project_id)
        .await?
        .ok_or_else(|| anyhow!("project {project_id} not found"))?;
    let rows = sqlx::query(
        "SELECT id, profile_order FROM projects
         WHERE profile_id = ?
         ORDER BY profile_order, id",
    )
    .bind(project.profile_id)
    .fetch_all(pool)
    .await?;
    let items: Vec<(i64, i64)> = rows
        .iter()
        .map(|row| Ok((row.try_get("id")?, row.try_get("profile_order")?)))
        .collect::<Result<Vec<_>>>()?;
    let Some(current) = items.iter().position(|(id, _)| *id == project_id) else {
        return Err(anyhow!("project {project_id} not found"));
    };
    let next = (current as isize + delta).clamp(0, items.len().saturating_sub(1) as isize) as usize;
    if next == current {
        return Ok(());
    }
    let (other_id, other_order) = items[next];
    let (_, current_order) = items[current];
    let mut tx = pool.begin().await?;
    sqlx::query("UPDATE projects SET profile_order = ? WHERE id = ?")
        .bind(other_order)
        .bind(project_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE projects SET profile_order = ? WHERE id = ?")
        .bind(current_order)
        .bind(other_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

pub async fn remove(pool: &SqlitePool, project_id: i64) -> Result<()> {
    let n = sqlx::query("DELETE FROM projects WHERE id = ?")
        .bind(project_id)
        .execute(pool)
        .await?
        .rows_affected();
    if n == 0 {
        return Err(anyhow!("project {project_id} not found"));
    }
    Ok(())
}

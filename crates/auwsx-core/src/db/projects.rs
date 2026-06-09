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

/// Gate policy for `ENDED -> COMPLETING`.
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
    pub name: String,
    pub repo_path: String,
    pub default_branch: String,

    pub main_agent_cmd: String,
    pub plan_agent_cmd: String,
    pub work_agent_cmd: String,
    pub review_agent_cmd: Option<String>,

    pub completion_policy: CompletionPolicy,
    pub completion_soft_timeout_min: i64,
    pub plan_gate_timeout_min: i64,
    pub iteration_timeout_min: i64,
    pub main_job_timeout_min: i64,
    pub review_max_rounds: i64,
    pub conflict_max_attempts: i64,
    pub max_concurrency: i64,
    pub schedule_interval_min: Option<i64>,
    pub merge_mode: MergeMode,
    pub skill_path: Option<String>,
    pub deepsleep_interval_days: i64,
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
    pub main_agent_cmd: &'a str,
    pub plan_agent_cmd: &'a str,
    pub work_agent_cmd: &'a str,
    /// NULL falls back to `work_agent_cmd` at spawn (still a fresh third-eye).
    pub review_agent_cmd: Option<&'a str>,
    /// Gate policy for `ENDED -> COMPLETING`. None ⇒ DB default (`manual`).
    pub completion_policy: Option<CompletionPolicy>,
    /// Soft-release delay for `PLANNED -> IMPLEMENTING`. None ⇒ DB default.
    pub plan_gate_timeout_min: Option<i64>,
    /// Soft-release delay for `ENDED -> COMPLETING` under `soft`. None ⇒ DB default.
    pub completion_soft_timeout_min: Option<i64>,
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
            Role::Review => self.review_agent_cmd.as_deref().unwrap_or(&self.work_agent_cmd),
        }
    }

    fn from_row(row: &SqliteRow) -> Result<Self> {
        let merge_raw: String = row.try_get("merge_mode")?;
        let policy_raw: String = row.try_get("completion_policy")?;
        Ok(Project {
            id: row.try_get("id")?,
            name: row.try_get("name")?,
            repo_path: row.try_get("repo_path")?,
            default_branch: row.try_get("default_branch")?,
            main_agent_cmd: row.try_get("main_agent_cmd")?,
            plan_agent_cmd: row.try_get("plan_agent_cmd")?,
            work_agent_cmd: row.try_get("work_agent_cmd")?,
            review_agent_cmd: row.try_get("review_agent_cmd")?,
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
            merge_mode: MergeMode::from_str(&merge_raw)
                .ok_or_else(|| anyhow!("unknown merge_mode {merge_raw:?} in db"))?,
            skill_path: row.try_get("skill_path")?,
            deepsleep_interval_days: row.try_get("deepsleep_interval_days")?,
            last_deepsleep_at: row.try_get("last_deepsleep_at")?,
            created_at: row.try_get("created_at")?,
        })
    }
}

/// Insert a project. Required columns are supplied; all policy columns take
/// their SQL DEFAULT unless the matching `NewProject` override is `Some`. Any
/// override is applied in the same call via a `COALESCE` UPDATE (NULL keeps the
/// just-defaulted value), so the migration remains the single source of truth
/// for defaults. Returns the new id.
pub async fn create(pool: &SqlitePool, new: NewProject<'_>, now: i64) -> Result<i64> {
    let id: i64 = sqlx::query(
        "INSERT INTO projects
            (name, repo_path, default_branch,
             main_agent_cmd, plan_agent_cmd, work_agent_cmd, review_agent_cmd,
             created_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)
         RETURNING id",
    )
    .bind(new.name)
    .bind(new.repo_path)
    .bind(new.default_branch)
    .bind(new.main_agent_cmd)
    .bind(new.plan_agent_cmd)
    .bind(new.work_agent_cmd)
    .bind(new.review_agent_cmd)
    .bind(now)
    .fetch_one(pool)
    .await?
    .get("id");

    // Apply policy overrides only when at least one is set — COALESCE leaves
    // unspecified columns at their DEFAULT.
    if new.completion_policy.is_some()
        || new.plan_gate_timeout_min.is_some()
        || new.completion_soft_timeout_min.is_some()
    {
        sqlx::query(
            "UPDATE projects SET
                completion_policy = COALESCE(?, completion_policy),
                plan_gate_timeout_min = COALESCE(?, plan_gate_timeout_min),
                completion_soft_timeout_min = COALESCE(?, completion_soft_timeout_min)
             WHERE id = ?",
        )
        .bind(new.completion_policy.map(|p| p.as_str()))
        .bind(new.plan_gate_timeout_min)
        .bind(new.completion_soft_timeout_min)
        .bind(id)
        .execute(pool)
        .await?;
    }
    Ok(id)
}

pub async fn get(pool: &SqlitePool, id: i64) -> Result<Option<Project>> {
    let row = sqlx::query("SELECT * FROM projects WHERE id = ?")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    row.as_ref().map(Project::from_row).transpose()
}

pub async fn get_by_name(pool: &SqlitePool, name: &str) -> Result<Option<Project>> {
    let row = sqlx::query("SELECT * FROM projects WHERE name = ?")
        .bind(name)
        .fetch_optional(pool)
        .await?;
    row.as_ref().map(Project::from_row).transpose()
}

/// All projects, oldest first.
pub async fn list(pool: &SqlitePool) -> Result<Vec<Project>> {
    let rows = sqlx::query("SELECT * FROM projects ORDER BY id")
        .fetch_all(pool)
        .await?;
    rows.iter().map(Project::from_row).collect()
}

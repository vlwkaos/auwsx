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
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoutineOrigin {
    Builtin,
    User,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Routine {
    pub id: i64,
    pub project_id: i64,
    pub name: String,
    pub origin: RoutineOrigin,
    pub prompt_template: String,
    pub cron: String,
    pub enabled: bool,
    pub output_target: Option<String>,
    pub last_run_at: Option<i64>,
    pub next_run_at: Option<i64>,
}

// TODO: built-in routine definitions table (triage / deepsleep / dream / morning-summary)
// TODO: next_run calculation via `cron` crate
// TODO: output_target templating ({date}, {datetime})
// TODO: seed_builtins(project_id) — non-overwriting INSERT IGNORE

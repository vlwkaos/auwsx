//! Global + per-project configuration. Plan Step 9.
//!
//! Global config: `~/.config/auwsx/config.toml` (TOML)
//! Per-project config: SQLite rows on `projects` table (no scattered TOML)
//!
//! Per-repo `.gtrconfig` (wsx convention) is read at worktree creation time
//! for `hooks.postCreate` + `copy.include/exclude` via `wsx_core::hooks`.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GlobalConfig {
    pub server: ServerConfig,
    pub defaults: DefaultsConfig,
    pub notifications: NotificationsConfig,
    pub inbox: InboxConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    #[serde(default = "default_host")]
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
}
fn default_host() -> String { "127.0.0.1".into() }
fn default_port() -> u16 { 7777 }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DefaultsConfig {
    pub agent: String,
    pub schedule_interval_min: u32,
    pub max_concurrency: u32,
    pub merge_mode: String, // auto | pr | local
    pub deepsleep_interval_days: u32,
    pub dream_interval_days: u32,
    pub iteration_timeout_min: u32,
    pub main_job_timeout_min: u32,
    pub triage_max_interval_min: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotificationsConfig {
    pub task_pending_feedback: bool,
    pub task_done: bool,
    pub task_failed: bool,
    pub routine_failed: bool,
    pub triage_summary: bool,
    pub daemon_lifecycle: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InboxConfig {
    pub watch: bool,
    pub path: String,
}

// TODO: load() / save() helpers; defaults bootstrap on first run

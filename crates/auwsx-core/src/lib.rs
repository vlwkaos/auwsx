//! auwsx-core — pipeline state machine, agent runners, scheduler, DB.
//!
//! Plan: ~/.claude/plans/current-wsx-is-agent-cosmic-gadget.md
//!
//! Module map (each file references the plan section that drives its behaviour):
//!
//! - [`state`]      — `TaskStatus` enum + transition matrix (Plan Step 3)
//! - [`pipeline`]   — async fn per state transition; orchestrator (Plan Step 3, 3.8)
//! - [`scheduler`]  — per-project tokio ticker (tasks + routines) (Plan Step 3, 3.6)
//! - [`main_jobs`]  — main-workspace lifecycle, queued ops (Plan Step 3.5)
//! - [`routines`]   — cron routines (incl. built-ins) (Plan Step 3.6)
//! - [`drafts`]     — drafts CRUD + triage execution (Plan Step 3.7)
//! - [`followups`]  — followups CRUD + decide_next_step (Plan Step 3.8)
//! - [`inbox`]      — file-watch async input channel (Plan Step 3.65)
//! - [`notify`]     — system notifications (Plan Step 7 / north star §5)
//! - [`launchd`]    — daemon install/uninstall (Plan Step 7 / north star §1)
//! - [`agent`]      — `AgentRunner` trait + 3 impls (Plan Step 4)
//! - [`skills`]     — bundled skills install helper (Plan Step 5)
//! - [`db`]         — sqlx pool + typed queries (Plan Step 6)
//! - [`events`]     — broadcast channel + Event enum
//! - [`ipc`]        — Unix-socket Command/Event protocol
//! - [`config`]     — global + per-project TOML

pub mod agent;
pub mod config;
pub mod db;
pub mod drafts;
pub mod events;
pub mod followups;
pub mod inbox;
pub mod ipc;
pub mod launchd;
pub mod main_jobs;
pub mod notify;
pub mod pipeline;
pub mod routines;
pub mod scheduler;
pub mod skills;
pub mod state;

pub use anyhow::{Error, Result};

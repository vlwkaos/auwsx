//! auwsx-core — pipeline state machine, agent runners, scheduler, DB.
//!
//! Plan: ~/.claude/plans/current-wsx-is-agent-cosmic-gadget.md
//!
//! Module map (each file references the plan section that drives its behaviour):
//!
//! - [`state`]      — `IssueStatus` enum + transition matrix (Plan Step 3)
//! - [`pipeline`]   — async fn per state transition; orchestrator (Plan Step 3, 3.8)
//! - [`scheduler`]  — per-project tokio ticker (issues + routines) (Plan Step 3, 3.6)
//! - [`main_jobs`]  — main-workspace lifecycle, queued ops (Plan Step 3.5)
//! - [`main_job_runner`] — main-job agent execution + artifact recording
//! - [`remote_plan`] — pure remote issue/PR/comment workflow decisions
//! - [`remote_workflow`] — daemon-owned remote workflow queueing
//! - [`routines`]   — cron routines (incl. built-ins) (Plan Step 3.6)
//! - [`backlog`]    — backlog_items CRUD + admission gate (Plan Step 3.7)
//! - [`routing`]    — backlog routing into issues / queue messages
//! - [`steering`]   — append-only steering into in-flight issues (Plan Step 3.8)
//! - [`inbox`]      — file-watch async input channel (Plan Step 3.65)
//! - [`issue_control`] — pure operator lifecycle policy for issues/projects
//! - [`notify`]     — system notifications (Plan Step 7 / north star §5)
//! - [`launchd`]    — daemon install/uninstall (Plan Step 7 / north star §1)
//! - [`agent`]      — `AgentRunner` trait + 3 impls (Plan Step 4)
//! - [`skills`]     — bundled skills install helper (Plan Step 5)
//! - [`db`]         — sqlx pool + typed queries (Plan Step 6)
//! - [`events`]     — broadcast channel + Event enum
//! - [`ipc`]        — Unix-socket Command/Event protocol
//! - [`config`]     — global + per-project TOML

#![allow(clippy::should_implement_trait)]

pub mod agent;
pub mod artifacts;
pub mod backlog;
pub mod clock;
pub mod config;
pub mod control_outbox;
pub mod db;
pub mod events;
pub mod inbox;
pub mod ipc;
pub mod issue_control;
pub mod launchd;
pub mod local_merge;
pub mod main_job_runner;
pub mod main_jobs;
pub mod memory;
pub mod notify;
pub mod pipeline;
pub mod project_setup;
pub mod prompt;
pub mod reconcile;
pub mod remote_plan;
pub mod remote_workflow;
pub mod routines;
pub mod routing;
pub mod schedule;
pub mod scheduler;
pub mod skills;
pub mod state;
pub mod steering;
pub mod worktree;

pub use anyhow::{Error, Result};

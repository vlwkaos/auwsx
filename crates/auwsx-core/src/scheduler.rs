//! Per-project scheduler. Plan Step 3 (task schedule) + Step 3.6 (routines) + Step 3.7 (triage).
//!
//! For each enabled project, spawn one tokio task that:
//!   1. Sleeps to `schedule_interval_min` boundary (default 30m, configurable).
//!   2. On wake: fire triage built-in routine first (Plan Step 3.7) if drafts pending
//!      OR triage hasn't run within `triage_max_interval_min`.
//!   3. Then fire due cron routines (Plan Step 3.6).
//!   4. Then pick up tasks where status ∈ {Queued, Ready} up to
//!      `max_concurrency`. Spawn pipeline future per task.
//!
//! Stop signals: shutdown via tokio cancellation_token; per-project enable toggle
//! reloads scheduler for that project only.

use crate::Result;

// TODO: define SchedulerHandle, ProjectTicker, spawn loop.

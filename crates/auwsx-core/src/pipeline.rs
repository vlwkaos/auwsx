//! Pipeline orchestrator. Plan Step 3, 3.5, 3.8.
//!
//! Each state transition is either a deterministic app action OR a single
//! focused agent invocation. The agent NEVER sees the whole pipeline — it
//! gets a tight per-step prompt. The app glues steps together by writing
//! deterministic artifacts (`.auwsx/plan.md`, `progress.md`, `summary.md`,
//! `feedback-{n}.md`, `signal-done`) and reading them back.
//!
//! Core API (to be implemented):
//!
//! ```ignore
//! pub async fn prepare(task: &Task) -> Result<()>;            // pure: create wt, copy env, post-create hooks
//! pub async fn iterate(task: &Task, n: u32) -> Result<()>;    // agent call (impl/recall/plan/progress)
//! pub async fn qa(task: &Task, n: u32) -> Result<()>;         // agent call (/backpressure post)
//! pub async fn decide_next_step(task: &Task, n: u32) -> NextStep;   // explicit fb > followups > PEND_FB
//! pub async fn complete_commit(task: &Task) -> Result<()>;    // /commit
//! pub async fn complete_merge(task: &Task) -> Result<()>;     // /gh-pr or local merge
//! pub async fn propagate_knowledge(task: &Task) -> Result<()>; // /memo + /dream on main
//! pub async fn cleanup(task: &Task) -> Result<()>;            // kill sessions, delete wt
//! ```
//!
//! Per-iteration prompts are minimal (Plan Step 3 — "Agent prompts" subsection).
//! Followup-handoff rules live in `followups::decide_next_step`.

use crate::Result;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum NextStep {
    /// Followups concat → feedback-{n}.md, auto-advance to READY.
    AutoAdvanceFromFollowups { followup_ids: Vec<i64> },
    /// Explicit feedback file present, auto-advance to READY.
    AutoAdvanceFromExplicit,
    /// Nothing to do — wait for user.
    AwaitFeedback,
}

// TODO: implement transitions per plan. Each fn takes a `&Db`, a `&Task`,
// and an `&dyn AgentRunner`; emits Events; returns Result<()>.

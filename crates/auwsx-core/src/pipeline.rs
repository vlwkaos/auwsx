//! Pipeline orchestrator. Plan Step 3.
//!
//! The pipeline is a state machine, NOT one mega-prompt. auwsx owns transitions,
//! invocation, and artifact I/O (deterministic); the agent owns the cognitive
//! work within a phase. The contract between them is the control CLI (`auwsx
//! ...` over IPC) plus durable `.auwsx/` artifacts — auwsx never parses prose.
//!
//! Each `IssueStatus` that is [`crate::state::IssueStatus::is_actionable`] maps
//! to one phase function here. The scheduler spawns the phase's agent; the agent
//! sets the next status via the control CLI before it exits; the next tick
//! advances or halts based purely on that status.
//!
//! Phase → role (see `projects.*_agent_cmd`):
//!
//! ```text
//!   CONSOLIDATING  main   delegate-as-steering vs. standalone (no worktree yet)
//!   PLANNING       plan   write plan.md + subtasks            (worktree created)
//!   IMPLEMENTING   work   code + progress.md
//!   REVIEW         review fresh session: findings (3rd eye + devil's advocate)
//!   NEEDS_FIX      work   adjudicate findings on record, fix
//!   AUDIT          work   /good-to-go
//!   COMPLETING     work   rebase + --no-ff merge (+ /memo, post-merge /dream)
//! ```
//!
//! Per-phase prompts are minimal and inline the phase-relevant bundled skill
//! (see `skills`). Loop caps (`review_max_rounds`, `conflict_max_attempts`) and
//! the soft-gate deadline live on the `issues` row.

// TODO: one async fn per actionable phase, each taking `&Db`, the issue row,
//       and an `&dyn AgentRunner`; writing the per-phase prompt + context,
//       spawning the agent, logging to `agent_runs`, and emitting Events.
// TODO: worktree lifecycle — create at CONSOLIDATING->PLANNING (standalone),
//       tear down at DONE.

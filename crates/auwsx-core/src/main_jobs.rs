//! Main-workspace lifecycle. Plan Step 3.5.
//!
//! Each registered project gets two persistent tmux sessions on the repo root:
//!   - `auwsx-{proj}-main-agent` — receives canonical/maintenance prompts
//!   - `auwsx-{proj}-main-shell` — empty bash, never touched by auwsx
//!
//! `ensure_main_sessions(project)` is idempotent: creates both if absent,
//! re-uses if present. Called on daemon start AND on every scheduler tick
//! (cheap; just shells `tmux has-session` first).
//!
//! MainJob queue: serialized through the `-main-agent` session, so routines and
//! the pipeline's own main-branch ops never race (one main writer per project).
//! Sources:
//!   - post_merge — after an issue hits DONE
//!   - routine — fired by cron/triage scheduler
//!   - user_oneoff — explicit UI click ([/dream], [/release], custom)
//!
//! State: QUEUED → RUNNING → DONE | FAILED | REJECTED. REJECTED is a `knowledge`
//! routine whose diff escaped its configured writable_paths (auwsx owns the
//! commit + path-scope check, so it refuses and flags). Logs to
//! `<repo>/.auwsx/main/log-{ts}.md`.

use crate::Result;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MainJobSource {
    PostMerge,
    Routine,
    UserOneoff,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum MainJobStatus {
    Queued,
    Running,
    Done,
    Failed,
    Rejected,
}

// TODO: ensure_main_sessions(project) — idempotent tmux create
// TODO: enqueue(source, kind, prompt) -> MainJobId
// TODO: serial worker draining the queue per project

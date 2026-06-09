//! Backlog items + triage/consolidation. Plan Step 3.7.
//!
//! A backlog item is a lightweight intent: `(project_id, text, source)`. It
//! carries an admission gate:
//!
//!   * `source` = human | agent | routine | inbox
//!   * `approval` = pending | approved | dismissed
//!
//! Human/inbox items are inserted `approved`; agent/routine items are inserted
//! `pending` and wait for a human approve/dismiss in the overview. Only
//! `approved` items flow into triage.
//!
//! Triage (a built-in main-job) auto-groups approved items into issues and
//! promotes them — no human grouping gate (the gate is admission, above). Each
//! provisional issue then runs the CONSOLIDATING phase (see `pipeline`), which
//! decides delegate-as-steering vs. standalone before any worktree is created.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    Human,
    Agent,
    Routine,
    Inbox,
}

impl Source {
    /// Items a human authored (directly or via inbox) are pre-approved; agent
    /// and routine output must clear the admission gate.
    pub fn default_approval(&self) -> Approval {
        match self {
            Source::Human | Source::Inbox => Approval::Approved,
            Source::Agent | Source::Routine => Approval::Pending,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Approval {
    Pending,
    Approved,
    Dismissed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BacklogItem {
    pub id: i64,
    pub project_id: i64,
    pub text: String,
    pub source: Source,
    pub approval: Approval,
    pub origin_routine_id: Option<i64>,
    pub consumed_issue_id: Option<i64>,
    pub created_at: i64,
    pub resolved_at: Option<i64>,
}

// TODO: CRUD over the IPC command surface — add (source-aware default_approval),
//       list (filter by approval), approve / dismiss / edit / remove.
// TODO: run_triage(project_id) — group approved items into provisional issues.

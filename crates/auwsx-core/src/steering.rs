//! Steering: append-only guidance into in-flight issues. Plan Step 3.8.
//!
//! Steering replaces the old followups mechanism. A steering note is appended
//! to an issue that is in a working phase (`IssueStatus::accepts_steering`) —
//! it NEVER edits `plan.md` (that would risk conflicting with the locked plan).
//! The work agent consumes pending steering on its next spawn.
//!
//! Two sources:
//!   * `human`         — the user nudges an in-flight issue.
//!   * `consolidation` — the CONSOLIDATING phase folds a similar approved
//!                       backlog task into an existing working issue instead of
//!                       opening a new one; the donor issue then self-closes to
//!                       `ABSORBED`.
//!
//! Adding steering flips `issues.has_pending_steering = 1`, which re-activates
//! the issue on the next scheduler tick even if it had gone quiet.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SteeringSource {
    Human,
    Consolidation,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Steering {
    pub id: i64,
    pub issue_id: i64,
    pub source: SteeringSource,
    pub note: String,
    pub consumed: bool,
    pub created_at: i64,
    pub consumed_at: Option<i64>,
}

// TODO: CRUD over the IPC command surface — add (guarded by accepts_steering,
//       sets has_pending_steering), list pending, mark consumed, remove pending.

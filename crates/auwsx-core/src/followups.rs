//! Followups + decide_next_step. Plan Step 3.8.
//!
//! Followups are ad-hoc instructions attached to a task while it's in flight.
//! Valid only when parent ∈ {Iterating, Qa, PendingFeedback, Ready}.
//!
//! End-of-iteration rule (the only auto-advance in the system):
//!
//! ```text
//! match (explicit_feedback_file?, pending_followups?) {
//!     (true,  _)     => READY,                    // user wrote feedback themselves
//!     (false, true)  => write_concat → READY,     // followups become the implicit feedback
//!     (false, false) => PENDING_FEEDBACK,         // halt and wait for user
//! }
//! ```
//!
//! If user deletes all followups before iteration ends → falls through to (false, false) → PENDING_FEEDBACK.

use crate::pipeline::NextStep;
use crate::Result;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Followup {
    pub id: i64,
    pub task_id: i64,
    pub body: String,
    pub created_at: i64,
    pub consumed_at: Option<i64>,
    pub consumed_into_iter: Option<u32>,
}

// TODO: create/list/update/delete CRUD with status-guard (only if parent accepts_followups())
// TODO: decide_next_step(task_id, iteration_n) -> NextStep
// TODO: concat_into_feedback_file(task, iteration_n, followup_ids) -> Result<PathBuf>

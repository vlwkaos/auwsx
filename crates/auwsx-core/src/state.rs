//! Task state machine. Plan Step 3.
//!
//! Lifecycle:
//!   BACKLOG → QUEUED → PREPARING → ITERATING(n) → QA(n) →
//!     PENDING_FEEDBACK(n) → READY(n+1) → ITERATING(n+1) → ... →
//!   COMPLETING → KNOWLEDGE_PROPAGATING → DONE
//!
//! The READY → ITERATING auto-advance is driven by either explicit user
//! feedback (`feedback-{n}.md`) OR by `followups` rows being concatenated
//! into the same file (Plan Step 3.8 — `decide_next_step`).
//!
//! Iteration count `n` is stored on the `tasks` row separately; this enum
//! is dataless to keep transitions small.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum TaskStatus {
    Backlog,
    Queued,
    Preparing,
    Iterating,
    Qa,
    PendingFeedback,
    Ready,
    Completing,
    KnowledgePropagating,
    Done,
    Failed,
}

impl TaskStatus {
    /// Whether this status accepts followup attachment.
    /// Plan Step 3.8: followups valid only while parent ∈ {ITER, QA, PEND_FB, READY}.
    pub fn accepts_followups(&self) -> bool {
        matches!(
            self,
            Self::Iterating | Self::Qa | Self::PendingFeedback | Self::Ready
        )
    }

    /// Whether the scheduler will pick this task on its tick.
    pub fn is_schedulable(&self) -> bool {
        matches!(self, Self::Queued | Self::Ready)
    }

    /// Whether this is a terminal status (no more transitions).
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Done | Self::Failed)
    }
}

/// Returns true if `from -> to` is a legal transition. Used to gate DB updates.
/// TODO: implement full matrix per Plan Step 3 diagram.
pub fn is_legal_transition(from: TaskStatus, to: TaskStatus) -> bool {
    use TaskStatus::*;
    matches!(
        (from, to),
        (Backlog, Queued)
            | (Queued, Preparing)
            | (Preparing, Iterating)
            | (Preparing, Failed)
            | (Iterating, Qa)
            | (Iterating, Failed)
            | (Qa, PendingFeedback)
            | (Qa, Ready)             // followup auto-advance
            | (PendingFeedback, Ready)
            | (PendingFeedback, Completing)
            | (Ready, Iterating)
            | (Completing, KnowledgePropagating)
            | (Completing, Failed)
            | (KnowledgePropagating, Done)
            | (KnowledgePropagating, Failed)
    )
}

#[cfg(test)]
mod tests {
    // NOTE: do not write tests inline. Use /write-test once this module
    // has real logic to cover (transition matrix, accepts_followups guards).
}

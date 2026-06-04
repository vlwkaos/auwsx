//! Task state machine. Plan Step 3.
//!
//! Lifecycle:
//!   BACKLOG → QUEUED → PREPARING → ITERATING(n) → QA(n) →
//!     PENDING_FEEDBACK(n) → READY(n+1) → ITERATING(n+1) → ... →
//!   COMPLETING → KNOWLEDGE_PROPAGATING → DONE
//!
//! Failures land in FAILED from any non-terminal state. User-initiated cancel
//! is handled at the DB layer (row delete + worktree/session cleanup), not
//! through a state transition — Failed is reserved for genuine errors.
//!
//! Iteration number `n` is stored on the `tasks` row separately; this enum
//! is dataless so transitions are cheap to reason about.
//!
//! This module is intentionally DB-agnostic: serialization to/from SQLite
//! goes through `as_str` / `from_str`, called by `db`.

use serde::{Deserialize, Serialize};
use thiserror::Error;

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
    /// Stable string id used as the SQLite `status` column. Must match the
    /// CHECK domain in `0001_init.sql` exactly.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Backlog => "BACKLOG",
            Self::Queued => "QUEUED",
            Self::Preparing => "PREPARING",
            Self::Iterating => "ITERATING",
            Self::Qa => "QA",
            Self::PendingFeedback => "PENDING_FEEDBACK",
            Self::Ready => "READY",
            Self::Completing => "COMPLETING",
            Self::KnowledgePropagating => "KNOWLEDGE_PROPAGATING",
            Self::Done => "DONE",
            Self::Failed => "FAILED",
        }
    }

    /// Inverse of [`as_str`]. Used by `db` when loading a row.
    pub fn from_str(s: &str) -> Option<Self> {
        Some(match s {
            "BACKLOG" => Self::Backlog,
            "QUEUED" => Self::Queued,
            "PREPARING" => Self::Preparing,
            "ITERATING" => Self::Iterating,
            "QA" => Self::Qa,
            "PENDING_FEEDBACK" => Self::PendingFeedback,
            "READY" => Self::Ready,
            "COMPLETING" => Self::Completing,
            "KNOWLEDGE_PROPAGATING" => Self::KnowledgePropagating,
            "DONE" => Self::Done,
            "FAILED" => Self::Failed,
            _ => return None,
        })
    }

    /// Whether this status accepts followup attachment.
    /// Plan Step 3.8: followups valid only while parent ∈ {ITER, QA, PEND_FB, READY}.
    pub fn accepts_followups(&self) -> bool {
        matches!(
            self,
            Self::Iterating | Self::Qa | Self::PendingFeedback | Self::Ready
        )
    }

    /// Whether the scheduler will pick this task on its tick.
    /// Plan Step 3: only Queued (fresh) and Ready (post-feedback) are pickable.
    pub fn is_schedulable(&self) -> bool {
        matches!(self, Self::Queued | Self::Ready)
    }

    /// Whether this is a terminal status (no more transitions).
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Done | Self::Failed)
    }
}

#[derive(Debug, Error)]
pub enum TransitionError {
    #[error("illegal transition: {from:?} -> {to:?}")]
    Illegal {
        from: TaskStatus,
        to: TaskStatus,
    },
}

/// Returns true if `from -> to` is a legal transition.
///
/// Most transitions are linear; the two non-linear cases:
///   - `Qa -> Ready`         — followup auto-advance (Plan Step 3.8)
///   - `PendingFeedback -> Completing` — user marks task COMPLETE
///
/// Failure transitions are allowed from every active phase that has actual
/// agent or pipeline work (Preparing through KnowledgePropagating). The
/// Backlog, Queued, Ready, Done states have no in-flight work to fail; user
/// cancellation in those cases is handled at the DB level (delete the row),
/// not through Failed.
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
            | (Qa, Ready)
            | (Qa, Failed)
            | (PendingFeedback, Ready)
            | (PendingFeedback, Completing)
            | (Ready, Iterating)
            | (Completing, KnowledgePropagating)
            | (Completing, Failed)
            | (KnowledgePropagating, Done)
            | (KnowledgePropagating, Failed)
    )
}

/// Convenience wrapper: typed error version of `is_legal_transition`.
pub fn check_transition(from: TaskStatus, to: TaskStatus) -> Result<(), TransitionError> {
    if is_legal_transition(from, to) {
        Ok(())
    } else {
        Err(TransitionError::Illegal { from, to })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::hash_map::DefaultHasher;
    use std::collections::HashSet;
    use std::hash::{Hash, Hasher};

    const ALL_VARIANTS: &[TaskStatus] = &[
        TaskStatus::Backlog,
        TaskStatus::Queued,
        TaskStatus::Preparing,
        TaskStatus::Iterating,
        TaskStatus::Qa,
        TaskStatus::PendingFeedback,
        TaskStatus::Ready,
        TaskStatus::Completing,
        TaskStatus::KnowledgePropagating,
        TaskStatus::Done,
        TaskStatus::Failed,
    ];

    /// Single source of truth for the stable SCREAMING_SNAKE_CASE ids.
    /// Any change here is a wire/SQLite-format break and must be intentional.
    const ID_TABLE: &[(TaskStatus, &str)] = &[
        (TaskStatus::Backlog, "BACKLOG"),
        (TaskStatus::Queued, "QUEUED"),
        (TaskStatus::Preparing, "PREPARING"),
        (TaskStatus::Iterating, "ITERATING"),
        (TaskStatus::Qa, "QA"),
        (TaskStatus::PendingFeedback, "PENDING_FEEDBACK"),
        (TaskStatus::Ready, "READY"),
        (TaskStatus::Completing, "COMPLETING"),
        (TaskStatus::KnowledgePropagating, "KNOWLEDGE_PROPAGATING"),
        (TaskStatus::Done, "DONE"),
        (TaskStatus::Failed, "FAILED"),
    ];

    const LEGAL_TRANSITIONS: &[(TaskStatus, TaskStatus)] = &[
        (TaskStatus::Backlog, TaskStatus::Queued),
        (TaskStatus::Queued, TaskStatus::Preparing),
        (TaskStatus::Preparing, TaskStatus::Iterating),
        (TaskStatus::Preparing, TaskStatus::Failed),
        (TaskStatus::Iterating, TaskStatus::Qa),
        (TaskStatus::Iterating, TaskStatus::Failed),
        (TaskStatus::Qa, TaskStatus::PendingFeedback),
        (TaskStatus::Qa, TaskStatus::Ready),
        (TaskStatus::Qa, TaskStatus::Failed),
        (TaskStatus::PendingFeedback, TaskStatus::Ready),
        (TaskStatus::PendingFeedback, TaskStatus::Completing),
        (TaskStatus::Ready, TaskStatus::Iterating),
        (TaskStatus::Completing, TaskStatus::KnowledgePropagating),
        (TaskStatus::Completing, TaskStatus::Failed),
        (TaskStatus::KnowledgePropagating, TaskStatus::Done),
        (TaskStatus::KnowledgePropagating, TaskStatus::Failed),
    ];

    // ---------- as_str / from_str ----------

    #[test]
    fn as_str_exact_literals_for_every_variant() {
        for (v, want) in ID_TABLE {
            assert_eq!(v.as_str(), *want, "as_str mismatch for {v:?}");
        }
        assert_eq!(ID_TABLE.len(), ALL_VARIANTS.len(), "ID_TABLE must cover all variants");
    }

    #[test]
    fn from_str_screaming_snake_literals_match_table() {
        for (v, s) in ID_TABLE {
            let parsed = TaskStatus::from_str(s)
                .unwrap_or_else(|| panic!("from_str({s:?}) returned None"));
            assert_eq!(parsed, *v, "from_str mismatch for {s:?}");
        }
    }

    #[test]
    fn as_str_and_from_str_roundtrip_every_variant() {
        for v in ALL_VARIANTS {
            let s = v.as_str();
            let parsed = TaskStatus::from_str(s)
                .unwrap_or_else(|| panic!("from_str({s:?}) returned None for variant {v:?}"));
            assert_eq!(parsed, *v, "roundtrip mismatch: {v:?} -> {s:?} -> {parsed:?}");
        }
    }

    #[test]
    fn from_str_empty_string_returns_none() {
        assert_eq!(TaskStatus::from_str(""), None);
    }

    #[test]
    fn from_str_lowercase_returns_none() {
        assert_eq!(TaskStatus::from_str("iterating"), None);
    }

    #[test]
    fn from_str_unknown_word_returns_none() {
        assert_eq!(TaskStatus::from_str("FOO"), None);
    }

    #[test]
    fn from_str_trailing_whitespace_returns_none() {
        assert_eq!(TaskStatus::from_str("READY "), None);
    }

    #[test]
    fn from_str_leading_whitespace_returns_none() {
        assert_eq!(TaskStatus::from_str(" READY"), None);
    }

    #[test]
    fn from_str_mixed_case_returns_none() {
        for bad in ["Ready", "pendingFeedback", "Pending_Feedback", "Knowledge_Propagating"] {
            assert_eq!(TaskStatus::from_str(bad), None, "from_str must reject {bad:?}");
        }
    }

    #[test]
    fn from_str_partial_prefix_returns_none() {
        for bad in ["READ", "PENDING", "KNOWLEDGE", "ITERAT"] {
            assert_eq!(TaskStatus::from_str(bad), None, "from_str must reject prefix {bad:?}");
        }
    }

    #[test]
    fn from_str_extra_underscore_returns_none() {
        assert_eq!(TaskStatus::from_str("PENDING__FEEDBACK"), None);
        assert_eq!(TaskStatus::from_str("_READY"), None);
        assert_eq!(TaskStatus::from_str("READY_"), None);
    }

    // ---------- predicate sets ----------

    #[test]
    fn accepts_followups_exhaustive() {
        use TaskStatus::*;
        let expected = |v: TaskStatus| matches!(v, Iterating | Qa | PendingFeedback | Ready);
        for v in ALL_VARIANTS {
            assert_eq!(v.accepts_followups(), expected(*v), "accepts_followups mismatch for {v:?}");
        }
    }

    #[test]
    fn is_schedulable_exhaustive() {
        use TaskStatus::*;
        let expected = |v: TaskStatus| matches!(v, Queued | Ready);
        for v in ALL_VARIANTS {
            assert_eq!(v.is_schedulable(), expected(*v), "is_schedulable mismatch for {v:?}");
        }
    }

    #[test]
    fn is_terminal_exhaustive() {
        use TaskStatus::*;
        let expected = |v: TaskStatus| matches!(v, Done | Failed);
        for v in ALL_VARIANTS {
            assert_eq!(v.is_terminal(), expected(*v), "is_terminal mismatch for {v:?}");
        }
    }

    #[test]
    fn terminal_implies_not_schedulable_and_not_accepts_followups() {
        for v in ALL_VARIANTS {
            if v.is_terminal() {
                assert!(!v.accepts_followups(), "{v:?} is terminal but accepts followups");
                assert!(!v.is_schedulable(), "{v:?} is terminal but is schedulable");
            }
        }
    }

    #[test]
    fn every_variant_either_terminal_schedulable_or_intermediate_no_overlap() {
        let mut terminal = 0;
        let mut schedulable = 0;
        let mut other = 0;
        for v in ALL_VARIANTS {
            assert!(
                !(v.is_terminal() && v.is_schedulable()),
                "{v:?} is both terminal and schedulable"
            );
            if v.is_terminal() {
                terminal += 1;
            } else if v.is_schedulable() {
                schedulable += 1;
            } else {
                other += 1;
            }
        }
        assert_eq!(terminal, 2, "expected 2 terminal variants");
        assert_eq!(schedulable, 2, "expected 2 schedulable variants");
        assert_eq!(terminal + schedulable + other, ALL_VARIANTS.len());
    }

    // ---------- transitions ----------

    #[test]
    fn every_legal_transition_is_accepted() {
        assert_eq!(LEGAL_TRANSITIONS.len(), 16, "expected 16 legal transitions");
        for (from, to) in LEGAL_TRANSITIONS {
            assert!(is_legal_transition(*from, *to), "expected legal transition: {from:?} -> {to:?}");
        }
    }

    #[test]
    fn illegal_transitions_complement_is_exhaustive() {
        let legal_set: HashSet<(TaskStatus, TaskStatus)> =
            LEGAL_TRANSITIONS.iter().copied().collect();
        for from in ALL_VARIANTS {
            for to in ALL_VARIANTS {
                let expected_legal = legal_set.contains(&(*from, *to));
                assert_eq!(
                    is_legal_transition(*from, *to),
                    expected_legal,
                    "is_legal_transition({from:?}, {to:?}) mismatch",
                );
            }
        }
    }

    #[test]
    fn legal_transitions_never_self_loop() {
        for v in ALL_VARIANTS {
            assert!(
                !is_legal_transition(*v, *v),
                "self-loop {v:?} -> {v:?} must be illegal",
            );
        }
    }

    #[test]
    fn terminal_has_no_outgoing_legal_transitions() {
        for from in ALL_VARIANTS.iter().filter(|v| v.is_terminal()) {
            for to in ALL_VARIANTS {
                assert!(
                    !is_legal_transition(*from, *to),
                    "terminal {from:?} must not transition to {to:?}",
                );
            }
        }
    }

    #[test]
    fn backlog_to_iterating_is_illegal() {
        assert!(!is_legal_transition(TaskStatus::Backlog, TaskStatus::Iterating));
    }

    #[test]
    fn done_to_failed_is_illegal_because_done_is_terminal() {
        assert!(!is_legal_transition(TaskStatus::Done, TaskStatus::Failed));
    }

    #[test]
    fn failed_to_ready_is_illegal_because_failed_is_terminal() {
        assert!(!is_legal_transition(TaskStatus::Failed, TaskStatus::Ready));
    }

    #[test]
    fn ready_to_qa_is_illegal_must_iterate_first() {
        assert!(!is_legal_transition(TaskStatus::Ready, TaskStatus::Qa));
    }

    #[test]
    fn knowledge_propagating_to_iterating_is_illegal_one_way() {
        assert!(!is_legal_transition(TaskStatus::KnowledgePropagating, TaskStatus::Iterating));
    }

    #[test]
    fn backlog_to_failed_is_illegal_no_in_flight_work() {
        assert!(!is_legal_transition(TaskStatus::Backlog, TaskStatus::Failed));
    }

    #[test]
    fn queued_to_failed_is_illegal_no_in_flight_work() {
        assert!(!is_legal_transition(TaskStatus::Queued, TaskStatus::Failed));
    }

    // ---------- check_transition ----------

    #[test]
    fn check_transition_agrees_with_is_legal_transition_for_all_pairs() {
        for from in ALL_VARIANTS {
            for to in ALL_VARIANTS {
                let legal = is_legal_transition(*from, *to);
                let checked = check_transition(*from, *to);
                match (legal, checked) {
                    (true, Ok(())) => {}
                    (false, Err(TransitionError::Illegal { from: f, to: t })) => {
                        assert_eq!(f, *from, "error.from for {from:?} -> {to:?}");
                        assert_eq!(t, *to, "error.to for {from:?} -> {to:?}");
                    }
                    (true, Err(e)) => panic!("legal {from:?} -> {to:?} returned Err({e:?})"),
                    (false, Ok(())) => panic!("illegal {from:?} -> {to:?} returned Ok"),
                }
            }
        }
    }

    #[test]
    fn check_transition_illegal_returns_err_with_matching_fields() {
        let from = TaskStatus::Backlog;
        let to = TaskStatus::Iterating;
        match check_transition(from, to) {
            Err(TransitionError::Illegal { from: f, to: t }) => {
                assert_eq!(f, from, "error.from should equal arg");
                assert_eq!(t, to, "error.to should equal arg");
            }
            Ok(()) => panic!("expected Err for illegal transition {from:?} -> {to:?}"),
        }
    }

    #[test]
    fn check_transition_error_display_format() {
        let err = check_transition(TaskStatus::Backlog, TaskStatus::Iterating)
            .expect_err("should be illegal");
        let rendered = format!("{err}");
        assert_eq!(
            rendered, "illegal transition: Backlog -> Iterating",
            "thiserror Display must match documented format",
        );
    }

    // ---------- serde ----------

    #[test]
    fn serde_roundtrip_every_variant_matches_as_str() {
        for v in ALL_VARIANTS {
            let json = serde_json::to_string(v)
                .unwrap_or_else(|e| panic!("serialize failed for {v:?}: {e}"));
            let inner = json
                .strip_prefix('"')
                .and_then(|s| s.strip_suffix('"'))
                .unwrap_or_else(|| panic!("expected quoted JSON string, got {json}"));
            assert_eq!(inner, v.as_str(), "serde JSON body should equal as_str() for {v:?}");
            let back: TaskStatus = serde_json::from_str(&json)
                .unwrap_or_else(|e| panic!("deserialize failed for {json}: {e}"));
            assert_eq!(back, *v, "serde roundtrip mismatch for {v:?}");
        }
    }

    #[test]
    fn serde_pending_feedback_serializes_to_screaming_snake_literal() {
        let json = serde_json::to_string(&TaskStatus::PendingFeedback).unwrap();
        assert_eq!(json, "\"PENDING_FEEDBACK\"");
    }

    #[test]
    fn serde_deserialize_rejects_lowercase_and_unknown() {
        for bad in ["\"ready\"", "\"Ready\"", "\"FOO\"", "\"\"", "\"PENDING_FEEDBACK \""] {
            let r: Result<TaskStatus, _> = serde_json::from_str(bad);
            assert!(r.is_err(), "deserialize must reject {bad}");
        }
    }

    #[test]
    fn serde_deserialize_rejects_non_string_json() {
        for bad in ["1", "null", "true", "{}", "[]", "{\"Ready\":null}"] {
            let r: Result<TaskStatus, _> = serde_json::from_str(bad);
            assert!(r.is_err(), "deserialize must reject non-string JSON: {bad}");
        }
    }

    // ---------- derives: Hash / Eq / Copy ----------

    fn hash_of<T: Hash>(t: &T) -> u64 {
        let mut h = DefaultHasher::new();
        t.hash(&mut h);
        h.finish()
    }

    #[test]
    fn hash_eq_consistency_for_all_variants() {
        for v in ALL_VARIANTS {
            let copy = *v;
            assert_eq!(v, &copy);
            assert_eq!(hash_of(v), hash_of(&copy), "hash differs for equal {v:?}");
        }
        let set: HashSet<TaskStatus> = ALL_VARIANTS.iter().copied().collect();
        assert_eq!(set.len(), ALL_VARIANTS.len(), "HashSet must hold every variant uniquely");
    }

    #[test]
    fn copy_semantics_does_not_move() {
        let v = TaskStatus::Ready;
        let _a = is_legal_transition(v, TaskStatus::Iterating);
        let _b = is_legal_transition(v, TaskStatus::Qa);
        let _c = v.is_schedulable();
        let _d: TaskStatus = v;
    }
}


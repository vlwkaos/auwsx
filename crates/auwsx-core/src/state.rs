//! Issue state machine. Design: ~/.claude/plans/current-wsx-is-agent-cosmic-gadget.md
//!
//! **Status is the synchronization marker.** The scheduler does not track
//! processes; on each tick it reads an issue's status and acts by its class:
//!
//! ```text
//!   Actionable  -> spawn the phase's agent
//!   HumanGated  -> wait (a soft-gated one auto-releases when wait_until passes)
//!   Terminal    -> archive
//! ```
//!
//! An agent exits whenever it wants; whatever status it set via the control CLI
//! before exiting decides whether the scheduler continues or halts. Crash-resume
//! is free: a died agent leaves status untouched, so the next tick respawns it.
//!
//! Lifecycle (autonomous transitions only — human override can force any jump,
//! handled out-of-band and logged, NOT encoded here):
//!
//! ```text
//!   CONSOLIDATING ─┬─> PLANNING ─> PLANNED ─> IMPLEMENTING ─> REVIEW ⇄ NEEDS_FIX
//!                  │                                            │  │
//!                  └─> ABSORBED (delegated as steering)         │  └─> AUDIT ─> ENDED
//!                                                               │              │
//!                                       REVIEW_BLOCKED <────────┘              v
//!                                                                          COMPLETING ─> DONE
//!                                                                              │
//!                                                                          CONFLICTED ⇄ CONFLICT_BLOCKED
//! ```
//!
//! Loop counters (`review_round`, `conflict_attempts`) and the soft-gate
//! deadline (`wait_until`) live on the `issues` row, not in this enum, so the
//! enum stays dataless and transitions are cheap to reason about.
//!
//! DB-agnostic: serialization goes through `as_str` / `from_str`, and the set
//! of ids MUST match the `issues.status` CHECK domain in `0001_init.sql`.

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum IssueStatus {
    // --- consolidation (pre-worktree) ---
    Consolidating,
    // --- planning ---
    Planning,
    Planned,
    PlanBlocked,
    // --- implementation + quality loop ---
    Implementing,
    Review,
    NeedsFix,
    ReviewBlocked,
    Audit,
    // --- completion ---
    Ended,
    Completing,
    Conflicted,
    ConflictBlocked,
    // --- terminal ---
    Done,
    Absorbed,
    Failed,
}

/// How the scheduler treats a status on its tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchedulerClass {
    /// Spawn the phase's agent.
    Actionable,
    /// Wait for a human (a soft-gated one also auto-releases on `wait_until`).
    HumanGated,
    /// No further transitions.
    Terminal,
}

impl IssueStatus {
    /// Stable string id used as the SQLite `status` column and the IPC wire
    /// form. Must match the `issues.status` CHECK domain in `0001_init.sql`.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Consolidating => "CONSOLIDATING",
            Self::Planning => "PLANNING",
            Self::Planned => "PLANNED",
            Self::PlanBlocked => "PLAN_BLOCKED",
            Self::Implementing => "IMPLEMENTING",
            Self::Review => "REVIEW",
            Self::NeedsFix => "NEEDS_FIX",
            Self::ReviewBlocked => "REVIEW_BLOCKED",
            Self::Audit => "AUDIT",
            Self::Ended => "ENDED",
            Self::Completing => "COMPLETING",
            Self::Conflicted => "CONFLICTED",
            Self::ConflictBlocked => "CONFLICT_BLOCKED",
            Self::Done => "DONE",
            Self::Absorbed => "ABSORBED",
            Self::Failed => "FAILED",
        }
    }

    /// Inverse of [`as_str`]. Used by `db` / IPC when loading a value.
    pub fn from_str(s: &str) -> Option<Self> {
        Some(match s {
            "CONSOLIDATING" => Self::Consolidating,
            "PLANNING" => Self::Planning,
            "PLANNED" => Self::Planned,
            "PLAN_BLOCKED" => Self::PlanBlocked,
            "IMPLEMENTING" => Self::Implementing,
            "REVIEW" => Self::Review,
            "NEEDS_FIX" => Self::NeedsFix,
            "REVIEW_BLOCKED" => Self::ReviewBlocked,
            "AUDIT" => Self::Audit,
            "ENDED" => Self::Ended,
            "COMPLETING" => Self::Completing,
            "CONFLICTED" => Self::Conflicted,
            "CONFLICT_BLOCKED" => Self::ConflictBlocked,
            "DONE" => Self::Done,
            "ABSORBED" => Self::Absorbed,
            "FAILED" => Self::Failed,
            _ => return None,
        })
    }

    /// Scheduler treatment for this status.
    pub fn scheduler_class(&self) -> SchedulerClass {
        use IssueStatus::*;
        match self {
            Consolidating | Planning | Implementing | Review | NeedsFix | Audit | Conflicted
            | Completing => SchedulerClass::Actionable,
            Planned | PlanBlocked | ReviewBlocked | ConflictBlocked | Ended => {
                SchedulerClass::HumanGated
            }
            Done | Absorbed | Failed => SchedulerClass::Terminal,
        }
    }

    /// The scheduler spawns an agent for this status on its tick.
    pub fn is_actionable(&self) -> bool {
        self.scheduler_class() == SchedulerClass::Actionable
    }

    /// The scheduler waits for a human at this status.
    pub fn is_human_gated(&self) -> bool {
        self.scheduler_class() == SchedulerClass::HumanGated
    }

    /// Terminal: no further transitions.
    pub fn is_terminal(&self) -> bool {
        self.scheduler_class() == SchedulerClass::Terminal
    }

    /// A human-gated status the scheduler auto-releases once `wait_until`
    /// expires (vs. one that waits indefinitely for an explicit human action).
    ///
    /// `PLANNED` is always soft. `ENDED` is soft only when the project's
    /// `completion_policy = 'soft'`, which this dataless enum can't know — the
    /// scheduler ORs that policy in.
    pub fn is_soft_gated(&self) -> bool {
        matches!(self, Self::Planned)
    }

    /// Has a worktree and a locked plan, so it can host delegated work. The
    /// only statuses into which consolidation may fold a similar backlog task
    /// as steering, and the only ones a human may steer.
    pub fn is_working_phase(&self) -> bool {
        matches!(
            self,
            Self::Implementing | Self::Review | Self::NeedsFix | Self::Audit
        )
    }

    /// Whether append-only steering may be attached. Same set as
    /// [`is_working_phase`] — steering never touches `plan.md`, so it is only
    /// meaningful once the plan is locked and a worktree exists.
    pub fn accepts_steering(&self) -> bool {
        self.is_working_phase()
    }
}

#[derive(Debug, Error)]
pub enum TransitionError {
    #[error("illegal transition: {from:?} -> {to:?}")]
    Illegal { from: IssueStatus, to: IssueStatus },
}

/// Returns true if `from -> to` is a legal autonomous transition.
///
/// This is the contract the pipeline + IPC obey. Human override (`issue status
/// set --force`) deliberately bypasses it (and is logged to `agent_runs`), so
/// the matrix only encodes what the system does on its own.
///
/// `FAILED` is reachable from every non-terminal phase (abort / error).
/// `ABSORBED` is reachable only from `CONSOLIDATING` (delegation self-close).
pub fn is_legal_transition(from: IssueStatus, to: IssueStatus) -> bool {
    use IssueStatus::*;
    matches!(
        (from, to),
        // consolidation
        (Consolidating, Planning)
            | (Consolidating, Absorbed)
            | (Consolidating, Failed)
            // planning
            | (Planning, Planned)
            | (Planning, PlanBlocked)
            | (Planning, Failed)
            | (Planned, Implementing)
            | (Planned, Planning)        // human rejects plan -> replan
            | (Planned, Failed)
            | (PlanBlocked, Planning)
            | (PlanBlocked, Failed)
            // implementation + quality loop
            | (Implementing, Review)
            | (Implementing, Failed)
            | (Review, NeedsFix)
            | (Review, Audit)
            | (Review, ReviewBlocked)
            | (Review, Failed)
            | (NeedsFix, Review)
            | (NeedsFix, ReviewBlocked)
            | (NeedsFix, Failed)
            | (ReviewBlocked, NeedsFix)
            | (ReviewBlocked, Audit)
            | (ReviewBlocked, Failed)
            | (Audit, Ended)
            | (Audit, NeedsFix)          // good-to-go found issues
            | (Audit, Failed)
            // completion
            | (Ended, Completing)
            | (Ended, Failed)
            | (Completing, Done)
            | (Completing, Conflicted)
            | (Completing, Failed)
            | (Conflicted, Completing)
            | (Conflicted, ConflictBlocked)
            | (Conflicted, Failed)
            | (ConflictBlocked, Completing)
            | (ConflictBlocked, Conflicted)
            | (ConflictBlocked, Failed)
    )
}

/// Typed-error wrapper over [`is_legal_transition`].
pub fn check_transition(from: IssueStatus, to: IssueStatus) -> Result<(), TransitionError> {
    if is_legal_transition(from, to) {
        Ok(())
    } else {
        Err(TransitionError::Illegal { from, to })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    // --- exhaustive ground truth -------------------------------------------

    /// All 16 variants. Used to prove every table-driven loop covers the whole
    /// domain; a compile error here forces this list to track the enum.
    const ALL: [IssueStatus; 16] = [
        IssueStatus::Consolidating,
        IssueStatus::Planning,
        IssueStatus::Planned,
        IssueStatus::PlanBlocked,
        IssueStatus::Implementing,
        IssueStatus::Review,
        IssueStatus::NeedsFix,
        IssueStatus::ReviewBlocked,
        IssueStatus::Audit,
        IssueStatus::Ended,
        IssueStatus::Completing,
        IssueStatus::Conflicted,
        IssueStatus::ConflictBlocked,
        IssueStatus::Done,
        IssueStatus::Absorbed,
        IssueStatus::Failed,
    ];

    /// Spec section A: variant -> canonical id. Independent restatement of the
    /// contract (NOT read from the impl), so a wrong id is caught here.
    const STR_IDS: [(IssueStatus, &str); 16] = [
        (IssueStatus::Consolidating, "CONSOLIDATING"),
        (IssueStatus::Planning, "PLANNING"),
        (IssueStatus::Planned, "PLANNED"),
        (IssueStatus::PlanBlocked, "PLAN_BLOCKED"),
        (IssueStatus::Implementing, "IMPLEMENTING"),
        (IssueStatus::Review, "REVIEW"),
        (IssueStatus::NeedsFix, "NEEDS_FIX"),
        (IssueStatus::ReviewBlocked, "REVIEW_BLOCKED"),
        (IssueStatus::Audit, "AUDIT"),
        (IssueStatus::Ended, "ENDED"),
        (IssueStatus::Completing, "COMPLETING"),
        (IssueStatus::Conflicted, "CONFLICTED"),
        (IssueStatus::ConflictBlocked, "CONFLICT_BLOCKED"),
        (IssueStatus::Done, "DONE"),
        (IssueStatus::Absorbed, "ABSORBED"),
        (IssueStatus::Failed, "FAILED"),
    ];

    /// Spec section B: the scheduler-class partition, stated independently.
    const ACTIONABLE: [IssueStatus; 8] = [
        IssueStatus::Consolidating,
        IssueStatus::Planning,
        IssueStatus::Implementing,
        IssueStatus::Review,
        IssueStatus::NeedsFix,
        IssueStatus::Audit,
        IssueStatus::Conflicted,
        IssueStatus::Completing,
    ];
    const HUMAN_GATED: [IssueStatus; 5] = [
        IssueStatus::Planned,
        IssueStatus::PlanBlocked,
        IssueStatus::ReviewBlocked,
        IssueStatus::ConflictBlocked,
        IssueStatus::Ended,
    ];
    const TERMINAL: [IssueStatus; 3] = [
        IssueStatus::Done,
        IssueStatus::Absorbed,
        IssueStatus::Failed,
    ];

    /// Spec section C: the working-phase / steering set.
    const WORKING_PHASE: [IssueStatus; 4] = [
        IssueStatus::Implementing,
        IssueStatus::Review,
        IssueStatus::NeedsFix,
        IssueStatus::Audit,
    ];

    /// Spec section D: the COMPLETE legal-transition set, restated by hand.
    const LEGAL: [(IssueStatus, IssueStatus); 37] = [
        (IssueStatus::Consolidating, IssueStatus::Planning),
        (IssueStatus::Consolidating, IssueStatus::Absorbed),
        (IssueStatus::Consolidating, IssueStatus::Failed),
        (IssueStatus::Planning, IssueStatus::Planned),
        (IssueStatus::Planning, IssueStatus::PlanBlocked),
        (IssueStatus::Planning, IssueStatus::Failed),
        (IssueStatus::Planned, IssueStatus::Implementing),
        (IssueStatus::Planned, IssueStatus::Planning),
        (IssueStatus::Planned, IssueStatus::Failed),
        (IssueStatus::PlanBlocked, IssueStatus::Planning),
        (IssueStatus::PlanBlocked, IssueStatus::Failed),
        (IssueStatus::Implementing, IssueStatus::Review),
        (IssueStatus::Implementing, IssueStatus::Failed),
        (IssueStatus::Review, IssueStatus::NeedsFix),
        (IssueStatus::Review, IssueStatus::Audit),
        (IssueStatus::Review, IssueStatus::ReviewBlocked),
        (IssueStatus::Review, IssueStatus::Failed),
        (IssueStatus::NeedsFix, IssueStatus::Review),
        (IssueStatus::NeedsFix, IssueStatus::ReviewBlocked),
        (IssueStatus::NeedsFix, IssueStatus::Failed),
        (IssueStatus::ReviewBlocked, IssueStatus::NeedsFix),
        (IssueStatus::ReviewBlocked, IssueStatus::Audit),
        (IssueStatus::ReviewBlocked, IssueStatus::Failed),
        (IssueStatus::Audit, IssueStatus::Ended),
        (IssueStatus::Audit, IssueStatus::NeedsFix),
        (IssueStatus::Audit, IssueStatus::Failed),
        (IssueStatus::Ended, IssueStatus::Completing),
        (IssueStatus::Ended, IssueStatus::Failed),
        (IssueStatus::Completing, IssueStatus::Done),
        (IssueStatus::Completing, IssueStatus::Conflicted),
        (IssueStatus::Completing, IssueStatus::Failed),
        (IssueStatus::Conflicted, IssueStatus::Completing),
        (IssueStatus::Conflicted, IssueStatus::ConflictBlocked),
        (IssueStatus::Conflicted, IssueStatus::Failed),
        (IssueStatus::ConflictBlocked, IssueStatus::Completing),
        (IssueStatus::ConflictBlocked, IssueStatus::Conflicted),
        (IssueStatus::ConflictBlocked, IssueStatus::Failed),
    ];

    fn legal_set() -> HashSet<(IssueStatus, IssueStatus)> {
        LEGAL.iter().copied().collect()
    }

    // --- enum/table drift guards -------------------------------------------
    //
    // The `[IssueStatus; 16]` annotation on ALL only fails to compile if a
    // variant is *removed* (list too long) or a duplicate pads it back to 16.
    // It does NOT catch a 17th variant being *added*: ALL would silently stay
    // length-16 and every table-driven loop below would skip the new variant.
    // The exhaustive match here is the real guard: adding a variant makes this
    // test fail to compile (non-exhaustive match), forcing ALL/STR_IDS/the
    // partition tables to be updated.

    #[test]
    fn given_the_enum_when_exhaustively_matched_then_all_table_lists_it() {
        // Map every variant to its position in ALL. A new variant => compile
        // error here (missing arm); a variant missing from ALL => assertion.
        fn assert_in_all(v: IssueStatus) {
            match v {
                IssueStatus::Consolidating
                | IssueStatus::Planning
                | IssueStatus::Planned
                | IssueStatus::PlanBlocked
                | IssueStatus::Implementing
                | IssueStatus::Review
                | IssueStatus::NeedsFix
                | IssueStatus::ReviewBlocked
                | IssueStatus::Audit
                | IssueStatus::Ended
                | IssueStatus::Completing
                | IssueStatus::Conflicted
                | IssueStatus::ConflictBlocked
                | IssueStatus::Done
                | IssueStatus::Absorbed
                | IssueStatus::Failed => {}
            }
            assert!(ALL.contains(&v), "{v:?} missing from ALL");
        }
        for v in ALL {
            assert_in_all(v);
        }
    }

    #[test]
    fn given_all_table_when_collected_then_16_distinct_variants() {
        // Guards against a duplicate entry masking a missing variant inside the
        // fixed-length [_; 16] literal.
        let set: HashSet<IssueStatus> = ALL.into_iter().collect();
        assert_eq!(set.len(), 16, "ALL must list 16 distinct variants");
    }

    #[test]
    fn given_str_ids_table_when_collected_then_covers_every_variant_once() {
        let keys: HashSet<IssueStatus> = STR_IDS.iter().map(|(v, _)| *v).collect();
        assert_eq!(
            keys.len(),
            16,
            "STR_IDS must map every variant exactly once"
        );
        for v in ALL {
            assert!(keys.contains(&v), "STR_IDS missing {v:?}");
        }
    }

    // --- A. string ids ------------------------------------------------------

    #[test]
    fn given_each_variant_when_as_str_then_matches_spec_id() {
        for (v, id) in STR_IDS {
            assert_eq!(v.as_str(), id, "as_str mismatch for {v:?}");
        }
    }

    #[test]
    fn given_canonical_id_when_from_str_then_returns_that_variant() {
        for (v, id) in STR_IDS {
            assert_eq!(IssueStatus::from_str(id), Some(v), "from_str({id:?})");
        }
    }

    #[test]
    fn given_every_variant_when_round_tripped_through_as_str_then_unchanged() {
        for v in ALL {
            assert_eq!(
                IssueStatus::from_str(v.as_str()),
                Some(v),
                "round-trip {v:?}"
            );
        }
    }

    #[test]
    fn given_distinct_variants_when_as_str_then_ids_are_unique() {
        let ids: HashSet<&str> = ALL.iter().map(|v| v.as_str()).collect();
        assert_eq!(ids.len(), ALL.len(), "as_str ids must be unique");
    }

    #[test]
    fn given_empty_string_when_from_str_then_none() {
        assert_eq!(IssueStatus::from_str(""), None);
    }

    #[test]
    fn given_lowercase_id_when_from_str_then_none() {
        assert_eq!(IssueStatus::from_str("planning"), None);
    }

    #[test]
    fn given_mixed_case_id_when_from_str_then_none() {
        assert_eq!(IssueStatus::from_str("Planning"), None);
    }

    #[test]
    fn given_leading_whitespace_id_when_from_str_then_none() {
        assert_eq!(IssueStatus::from_str(" PLANNING"), None);
    }

    #[test]
    fn given_trailing_whitespace_id_when_from_str_then_none() {
        assert_eq!(IssueStatus::from_str("PLANNING "), None);
    }

    #[test]
    fn given_partial_prefix_when_from_str_then_none() {
        // "READ" is a prefix of nothing valid; "PENDING" is a foreign token.
        assert_eq!(IssueStatus::from_str("READ"), None);
        assert_eq!(IssueStatus::from_str("PENDING"), None);
        assert_eq!(IssueStatus::from_str("REVIEW_BLO"), None);
    }

    #[test]
    fn given_unknown_token_when_from_str_then_none() {
        assert_eq!(IssueStatus::from_str("FOO"), None);
    }

    #[test]
    fn given_extra_underscores_when_from_str_then_none() {
        assert_eq!(IssueStatus::from_str("PLAN__BLOCKED"), None);
        assert_eq!(IssueStatus::from_str("_PLANNING"), None);
        assert_eq!(IssueStatus::from_str("PLANNING_"), None);
    }

    // --- B. scheduler-class partition --------------------------------------

    #[test]
    fn given_actionable_set_when_scheduler_class_then_actionable() {
        for v in ACTIONABLE {
            assert_eq!(v.scheduler_class(), SchedulerClass::Actionable, "{v:?}");
        }
    }

    #[test]
    fn given_human_gated_set_when_scheduler_class_then_human_gated() {
        for v in HUMAN_GATED {
            assert_eq!(v.scheduler_class(), SchedulerClass::HumanGated, "{v:?}");
        }
    }

    #[test]
    fn given_terminal_set_when_scheduler_class_then_terminal() {
        for v in TERMINAL {
            assert_eq!(v.scheduler_class(), SchedulerClass::Terminal, "{v:?}");
        }
    }

    #[test]
    fn given_all_variants_when_partitioned_by_class_then_each_in_exactly_one() {
        let a: HashSet<IssueStatus> = ACTIONABLE.into_iter().collect();
        let h: HashSet<IssueStatus> = HUMAN_GATED.into_iter().collect();
        let t: HashSet<IssueStatus> = TERMINAL.into_iter().collect();
        // non-overlapping
        assert!(a.is_disjoint(&h));
        assert!(a.is_disjoint(&t));
        assert!(h.is_disjoint(&t));
        // exhaustive cover
        assert_eq!(a.len() + h.len() + t.len(), ALL.len());
        for v in ALL {
            let count = a.contains(&v) as u8 + h.contains(&v) as u8 + t.contains(&v) as u8;
            assert_eq!(count, 1, "{v:?} must be in exactly one class");
        }
    }

    #[test]
    fn given_each_variant_when_is_actionable_then_agrees_with_class() {
        for v in ALL {
            assert_eq!(
                v.is_actionable(),
                v.scheduler_class() == SchedulerClass::Actionable,
                "{v:?}"
            );
        }
    }

    #[test]
    fn given_each_variant_when_is_human_gated_then_agrees_with_class() {
        for v in ALL {
            assert_eq!(
                v.is_human_gated(),
                v.scheduler_class() == SchedulerClass::HumanGated,
                "{v:?}"
            );
        }
    }

    #[test]
    fn given_each_variant_when_is_terminal_then_agrees_with_class() {
        for v in ALL {
            assert_eq!(
                v.is_terminal(),
                v.scheduler_class() == SchedulerClass::Terminal,
                "{v:?}"
            );
        }
    }

    #[test]
    fn given_each_variant_when_three_predicates_checked_then_exactly_one_true() {
        for v in ALL {
            let count = v.is_actionable() as u8 + v.is_human_gated() as u8 + v.is_terminal() as u8;
            assert_eq!(count, 1, "{v:?} must satisfy exactly one predicate");
        }
    }

    // --- C. soft-gate / working-phase / steering ---------------------------

    #[test]
    fn given_planned_when_is_soft_gated_then_true() {
        assert!(IssueStatus::Planned.is_soft_gated());
    }

    #[test]
    fn given_any_non_planned_when_is_soft_gated_then_false() {
        for v in ALL {
            if v != IssueStatus::Planned {
                assert!(!v.is_soft_gated(), "{v:?} must not be soft-gated");
            }
        }
    }

    #[test]
    fn given_working_phase_set_when_is_working_phase_then_true() {
        for v in WORKING_PHASE {
            assert!(v.is_working_phase(), "{v:?}");
        }
    }

    #[test]
    fn given_non_working_phase_when_is_working_phase_then_false() {
        let working: HashSet<IssueStatus> = WORKING_PHASE.into_iter().collect();
        for v in ALL {
            if !working.contains(&v) {
                assert!(!v.is_working_phase(), "{v:?} must not be a working phase");
            }
        }
    }

    #[test]
    fn given_each_variant_when_accepts_steering_then_equals_is_working_phase() {
        for v in ALL {
            assert_eq!(v.accepts_steering(), v.is_working_phase(), "{v:?}");
        }
    }

    #[test]
    fn given_terminal_variants_when_predicates_checked_then_inert() {
        for v in TERMINAL {
            assert!(!v.is_actionable(), "{v:?} actionable");
            assert!(!v.is_soft_gated(), "{v:?} soft-gated");
            assert!(!v.is_working_phase(), "{v:?} working-phase");
            assert!(!v.accepts_steering(), "{v:?} accepts-steering");
        }
    }

    // --- D. transition matrix ----------------------------------------------

    #[test]
    fn given_full_pair_space_when_is_legal_transition_then_matches_legal_set_exactly() {
        let legal = legal_set();
        for from in ALL {
            for to in ALL {
                let expected = legal.contains(&(from, to));
                assert_eq!(
                    is_legal_transition(from, to),
                    expected,
                    "{from:?} -> {to:?}: expected {expected}"
                );
            }
        }
    }

    #[test]
    fn given_legal_set_when_counted_then_has_no_duplicates() {
        // Guards the hand-written LEGAL table itself against accidental dupes.
        assert_eq!(legal_set().len(), LEGAL.len(), "LEGAL table has duplicates");
    }

    #[test]
    fn given_any_variant_when_self_transition_then_illegal() {
        for v in ALL {
            assert!(!is_legal_transition(v, v), "self-loop {v:?} -> {v:?}");
        }
    }

    #[test]
    fn given_terminal_states_when_any_outgoing_transition_then_illegal() {
        for from in TERMINAL {
            for to in ALL {
                assert!(
                    !is_legal_transition(from, to),
                    "terminal {from:?} must have no outgoing edge, got {from:?} -> {to:?}"
                );
            }
        }
    }

    #[test]
    fn given_absorbed_target_when_source_not_consolidating_then_illegal() {
        for from in ALL {
            if from != IssueStatus::Consolidating {
                assert!(
                    !is_legal_transition(from, IssueStatus::Absorbed),
                    "only CONSOLIDATING may reach ABSORBED, got {from:?}"
                );
            }
        }
    }

    #[test]
    fn given_every_non_terminal_when_to_failed_then_legal() {
        let terminal: HashSet<IssueStatus> = TERMINAL.into_iter().collect();
        for from in ALL {
            if !terminal.contains(&from) {
                assert!(
                    is_legal_transition(from, IssueStatus::Failed),
                    "FAILED must be reachable from {from:?}"
                );
            }
        }
    }

    // --- check_transition typed wrapper ------------------------------------

    #[test]
    fn given_full_pair_space_when_check_transition_then_ok_iff_legal() {
        let legal = legal_set();
        for from in ALL {
            for to in ALL {
                let is_ok = check_transition(from, to).is_ok();
                assert_eq!(
                    is_ok,
                    legal.contains(&(from, to)),
                    "check_transition {from:?} -> {to:?}"
                );
            }
        }
    }

    #[test]
    fn given_illegal_pair_when_check_transition_then_err_carries_same_from_and_to() {
        let from = IssueStatus::Done;
        let to = IssueStatus::Failed; // terminal source -> always illegal
        match check_transition(from, to) {
            Err(TransitionError::Illegal { from: f, to: t }) => {
                assert_eq!(f, from);
                assert_eq!(t, to);
            }
            Ok(()) => panic!("expected Illegal error for {from:?} -> {to:?}"),
        }
    }

    #[test]
    fn given_illegal_transition_error_when_displayed_then_matches_spec_format() {
        let err = TransitionError::Illegal {
            from: IssueStatus::Done,
            to: IssueStatus::Failed,
        };
        assert_eq!(err.to_string(), "illegal transition: Done -> Failed");
    }

    #[test]
    fn given_every_illegal_pair_when_displayed_then_uses_debug_variant_names() {
        // Independently derive the expected message from Debug formatting and
        // confirm check_transition's error renders identically across the space.
        let legal = legal_set();
        for from in ALL {
            for to in ALL {
                if legal.contains(&(from, to)) {
                    continue;
                }
                let err = check_transition(from, to).expect_err("must be illegal");
                let expected = format!("illegal transition: {from:?} -> {to:?}");
                assert_eq!(err.to_string(), expected);
            }
        }
    }

    // --- E. serde -----------------------------------------------------------

    #[test]
    fn given_each_variant_when_serialized_then_json_is_quoted_canonical_id() {
        for (v, id) in STR_IDS {
            let json = serde_json::to_string(&v).expect("serialize");
            assert_eq!(json, format!("\"{id}\""), "serialize {v:?}");
        }
    }

    #[test]
    fn given_quoted_canonical_id_when_deserialized_then_returns_variant() {
        for (v, id) in STR_IDS {
            let parsed: IssueStatus =
                serde_json::from_str(&format!("\"{id}\"")).expect("deserialize");
            assert_eq!(parsed, v, "deserialize {id:?}");
        }
    }

    #[test]
    fn given_each_variant_when_round_tripped_through_json_then_unchanged() {
        for v in ALL {
            let json = serde_json::to_string(&v).expect("serialize");
            let back: IssueStatus = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(back, v, "json round-trip {v:?}");
        }
    }

    #[test]
    fn given_lowercase_json_string_when_deserialized_then_err() {
        assert!(serde_json::from_str::<IssueStatus>("\"planning\"").is_err());
    }

    #[test]
    fn given_unknown_json_string_when_deserialized_then_err() {
        assert!(serde_json::from_str::<IssueStatus>("\"FOO\"").is_err());
    }

    #[test]
    fn given_whitespace_padded_json_string_when_deserialized_then_err() {
        assert!(serde_json::from_str::<IssueStatus>("\" PLANNING\"").is_err());
        assert!(serde_json::from_str::<IssueStatus>("\"PLANNING \"").is_err());
    }

    #[test]
    fn given_non_string_json_when_deserialized_then_err() {
        for json in ["1", "null", "true", "{}", "[]"] {
            assert!(
                serde_json::from_str::<IssueStatus>(json).is_err(),
                "non-string JSON {json} must fail"
            );
        }
    }
}

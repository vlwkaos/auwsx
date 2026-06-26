//! Issue state machine.
//!
//! Status is the scheduler synchronization marker. The UI derives two separate
//! operator-facing axes from it:
//! - progress lane: where the issue is in the lifecycle
//! - attention marker: whether the operator must act
//!
//! Backlog routing is project-level work and is deliberately not an issue
//! status. A routed backlog item either creates an issue or becomes a queue
//! message on an existing attachable issue.

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum IssueStatus {
    New,
    Planning,
    PlanReady,
    PlanBlocked,
    Working,
    Reviewing,
    Fixing,
    ReviewBlocked,
    Auditing,
    ReadyToMerge,
    Merging,
    ResolvingConflict,
    ConflictBlocked,
    Done,
    Failed,
    Abandoned,
}

/// How the scheduler treats a status on its tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchedulerClass {
    /// Spawn the phase's agent.
    Actionable,
    /// Wait for human action or a soft gate.
    HumanGated,
    /// No further autonomous transitions.
    Terminal,
}

/// Operator-facing lifecycle lane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ProgressLane {
    Plan,
    InProgress,
    Finalizing,
    Done,
}

impl ProgressLane {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Plan => "PLAN",
            Self::InProgress => "IN_PROGRESS",
            Self::Finalizing => "FINALIZING",
            Self::Done => "DONE",
        }
    }
}

impl IssueStatus {
    /// Stable string id used as the SQLite `status` column and IPC wire form.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::New => "NEW",
            Self::Planning => "PLANNING",
            Self::PlanReady => "PLAN_READY",
            Self::PlanBlocked => "PLAN_BLOCKED",
            Self::Working => "WORKING",
            Self::Reviewing => "REVIEWING",
            Self::Fixing => "FIXING",
            Self::ReviewBlocked => "REVIEW_BLOCKED",
            Self::Auditing => "AUDITING",
            Self::ReadyToMerge => "READY_TO_MERGE",
            Self::Merging => "MERGING",
            Self::ResolvingConflict => "RESOLVING_CONFLICT",
            Self::ConflictBlocked => "CONFLICT_BLOCKED",
            Self::Done => "DONE",
            Self::Failed => "FAILED",
            Self::Abandoned => "ABANDONED",
        }
    }

    /// Inverse of [`as_str`](Self::as_str).
    pub fn from_str(s: &str) -> Option<Self> {
        Some(match s {
            "NEW" => Self::New,
            "PLANNING" => Self::Planning,
            "PLAN_READY" => Self::PlanReady,
            "PLAN_BLOCKED" => Self::PlanBlocked,
            "WORKING" => Self::Working,
            "REVIEWING" => Self::Reviewing,
            "FIXING" => Self::Fixing,
            "REVIEW_BLOCKED" => Self::ReviewBlocked,
            "AUDITING" => Self::Auditing,
            "READY_TO_MERGE" => Self::ReadyToMerge,
            "MERGING" => Self::Merging,
            "RESOLVING_CONFLICT" => Self::ResolvingConflict,
            "CONFLICT_BLOCKED" => Self::ConflictBlocked,
            "DONE" => Self::Done,
            "FAILED" => Self::Failed,
            "ABANDONED" => Self::Abandoned,
            _ => return None,
        })
    }

    pub fn scheduler_class(self) -> SchedulerClass {
        use IssueStatus::*;
        match self {
            New | Planning | Working | Reviewing | Fixing | Auditing | Merging
            | ResolvingConflict => SchedulerClass::Actionable,
            PlanReady | PlanBlocked | ReviewBlocked | ReadyToMerge | ConflictBlocked => {
                SchedulerClass::HumanGated
            }
            Done | Failed | Abandoned => SchedulerClass::Terminal,
        }
    }

    pub fn is_actionable(self) -> bool {
        self.scheduler_class() == SchedulerClass::Actionable
    }

    pub fn is_human_gated(self) -> bool {
        self.scheduler_class() == SchedulerClass::HumanGated
    }

    pub fn is_terminal(self) -> bool {
        self.scheduler_class() == SchedulerClass::Terminal
    }

    /// Human-gated statuses the scheduler may auto-release by policy.
    ///
    /// `PLAN_READY` is always soft. `READY_TO_MERGE` is soft only under project
    /// completion policy, which the scheduler checks separately.
    pub fn is_soft_gated(self) -> bool {
        matches!(self, Self::PlanReady)
    }

    /// The issue can accept a queue message from a user or backlog router.
    pub fn accepts_queue_message(self) -> bool {
        matches!(
            self,
            Self::Planning
                | Self::Working
                | Self::Reviewing
                | Self::Fixing
                | Self::Auditing
                | Self::ReadyToMerge
        )
    }

    pub fn progress_lane(self) -> ProgressLane {
        use IssueStatus::*;
        match self {
            New | Planning | PlanReady | PlanBlocked => ProgressLane::Plan,
            Working | Reviewing | Fixing | ReviewBlocked | Auditing => ProgressLane::InProgress,
            ReadyToMerge | Merging | ResolvingConflict | ConflictBlocked => {
                ProgressLane::Finalizing
            }
            Done | Failed | Abandoned => ProgressLane::Done,
        }
    }

    /// Compatibility label used by existing board renderers.
    pub fn stage_label(self) -> &'static str {
        self.progress_lane().as_str()
    }

    pub fn needs_attention(self) -> bool {
        matches!(
            self,
            Self::PlanReady
                | Self::PlanBlocked
                | Self::ReviewBlocked
                | Self::ReadyToMerge
                | Self::ConflictBlocked
                | Self::Failed
        )
    }

    pub fn is_archive_status(self) -> bool {
        self.is_terminal()
    }
}

#[derive(Debug, Error)]
pub enum TransitionError {
    #[error("illegal transition: {from:?} -> {to:?}")]
    Illegal { from: IssueStatus, to: IssueStatus },
}

pub fn is_legal_transition(from: IssueStatus, to: IssueStatus) -> bool {
    use IssueStatus::*;
    matches!(
        (from, to),
        // planning
        (New, Planning)
            | (New, Failed)
            | (New, Abandoned)
            | (Planning, PlanReady)
            | (Planning, PlanBlocked)
            | (Planning, Failed)
            | (Planning, Abandoned)
            | (PlanReady, Working)
            | (PlanReady, Planning)
            | (PlanReady, Failed)
            | (PlanReady, Abandoned)
            | (PlanBlocked, Planning)
            | (PlanBlocked, Failed)
            | (PlanBlocked, Abandoned)
            // implementation + quality loop
            | (Working, Reviewing)
            | (Working, Failed)
            | (Working, Abandoned)
            | (Reviewing, Fixing)
            | (Reviewing, Auditing)
            | (Reviewing, ReviewBlocked)
            | (Reviewing, Failed)
            | (Reviewing, Abandoned)
            | (Fixing, Reviewing)
            | (Fixing, ReviewBlocked)
            | (Fixing, Failed)
            | (Fixing, Abandoned)
            | (ReviewBlocked, Fixing)
            | (ReviewBlocked, Auditing)
            | (ReviewBlocked, Failed)
            | (ReviewBlocked, Abandoned)
            | (Auditing, ReadyToMerge)
            | (Auditing, Fixing)
            | (Auditing, Failed)
            | (Auditing, Abandoned)
            // finalizing
            | (ReadyToMerge, Working)
            | (ReadyToMerge, Merging)
            | (ReadyToMerge, Failed)
            | (ReadyToMerge, Abandoned)
            | (Merging, Done)
            | (Merging, ResolvingConflict)
            | (Merging, Failed)
            | (Merging, Abandoned)
            | (ResolvingConflict, Merging)
            | (ResolvingConflict, ConflictBlocked)
            | (ResolvingConflict, Failed)
            | (ResolvingConflict, Abandoned)
            | (ConflictBlocked, Merging)
            | (ConflictBlocked, ResolvingConflict)
            | (ConflictBlocked, Failed)
            | (ConflictBlocked, Abandoned)
    )
}

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

    const ALL: [IssueStatus; 16] = [
        IssueStatus::New,
        IssueStatus::Planning,
        IssueStatus::PlanReady,
        IssueStatus::PlanBlocked,
        IssueStatus::Working,
        IssueStatus::Reviewing,
        IssueStatus::Fixing,
        IssueStatus::ReviewBlocked,
        IssueStatus::Auditing,
        IssueStatus::ReadyToMerge,
        IssueStatus::Merging,
        IssueStatus::ResolvingConflict,
        IssueStatus::ConflictBlocked,
        IssueStatus::Done,
        IssueStatus::Failed,
        IssueStatus::Abandoned,
    ];

    const IDS: [(IssueStatus, &str); 16] = [
        (IssueStatus::New, "NEW"),
        (IssueStatus::Planning, "PLANNING"),
        (IssueStatus::PlanReady, "PLAN_READY"),
        (IssueStatus::PlanBlocked, "PLAN_BLOCKED"),
        (IssueStatus::Working, "WORKING"),
        (IssueStatus::Reviewing, "REVIEWING"),
        (IssueStatus::Fixing, "FIXING"),
        (IssueStatus::ReviewBlocked, "REVIEW_BLOCKED"),
        (IssueStatus::Auditing, "AUDITING"),
        (IssueStatus::ReadyToMerge, "READY_TO_MERGE"),
        (IssueStatus::Merging, "MERGING"),
        (IssueStatus::ResolvingConflict, "RESOLVING_CONFLICT"),
        (IssueStatus::ConflictBlocked, "CONFLICT_BLOCKED"),
        (IssueStatus::Done, "DONE"),
        (IssueStatus::Failed, "FAILED"),
        (IssueStatus::Abandoned, "ABANDONED"),
    ];

    #[test]
    fn given_each_status_when_roundtripped_then_same_status() {
        for (status, id) in IDS {
            assert_eq!(status.as_str(), id);
            assert_eq!(IssueStatus::from_str(id), Some(status));
            let json = serde_json::to_string(&status).unwrap();
            assert_eq!(json, format!("\"{id}\""));
            assert_eq!(serde_json::from_str::<IssueStatus>(&json).unwrap(), status);
        }
    }

    #[test]
    fn given_unknown_status_string_when_parsed_then_none() {
        for id in ["", "planning", "CONSOLIDATING", "ABSORBED", " PLAN_READY"] {
            assert_eq!(IssueStatus::from_str(id), None);
        }
    }

    #[test]
    fn given_each_status_when_classified_then_exactly_one_scheduler_class() {
        for status in ALL {
            let count = status.is_actionable() as u8
                + status.is_human_gated() as u8
                + status.is_terminal() as u8;
            assert_eq!(count, 1, "{status:?}");
        }
    }

    #[test]
    fn given_statuses_when_laned_then_match_operator_progress() {
        use IssueStatus::*;
        let cases = [
            (New, ProgressLane::Plan),
            (Planning, ProgressLane::Plan),
            (PlanReady, ProgressLane::Plan),
            (PlanBlocked, ProgressLane::Plan),
            (Working, ProgressLane::InProgress),
            (Reviewing, ProgressLane::InProgress),
            (Fixing, ProgressLane::InProgress),
            (ReviewBlocked, ProgressLane::InProgress),
            (Auditing, ProgressLane::InProgress),
            (ReadyToMerge, ProgressLane::Finalizing),
            (Merging, ProgressLane::Finalizing),
            (ResolvingConflict, ProgressLane::Finalizing),
            (ConflictBlocked, ProgressLane::Finalizing),
            (Done, ProgressLane::Done),
            (Failed, ProgressLane::Done),
            (Abandoned, ProgressLane::Done),
        ];
        for (status, lane) in cases {
            assert_eq!(status.progress_lane(), lane);
            assert_eq!(status.stage_label(), lane.as_str());
        }
    }

    #[test]
    fn given_attachable_statuses_when_checked_then_only_expected_accept_messages() {
        let attachable: HashSet<IssueStatus> = [
            IssueStatus::Planning,
            IssueStatus::Working,
            IssueStatus::Reviewing,
            IssueStatus::Fixing,
            IssueStatus::Auditing,
            IssueStatus::ReadyToMerge,
        ]
        .into_iter()
        .collect();
        for status in ALL {
            assert_eq!(
                status.accepts_queue_message(),
                attachable.contains(&status),
                "{status:?}"
            );
        }
    }

    #[test]
    fn given_attention_statuses_when_checked_then_only_expected_need_attention() {
        let attention: HashSet<IssueStatus> = [
            IssueStatus::PlanReady,
            IssueStatus::PlanBlocked,
            IssueStatus::ReviewBlocked,
            IssueStatus::ReadyToMerge,
            IssueStatus::ConflictBlocked,
            IssueStatus::Failed,
        ]
        .into_iter()
        .collect();
        for status in ALL {
            assert_eq!(
                status.needs_attention(),
                attention.contains(&status),
                "{status:?}"
            );
        }
    }

    #[test]
    fn given_terminal_statuses_when_checked_then_archive_status() {
        for status in ALL {
            assert_eq!(
                status.is_archive_status(),
                status.is_terminal(),
                "{status:?}"
            );
        }
    }

    #[test]
    fn given_legal_transition_when_checked_then_ok() {
        use IssueStatus::*;
        for (from, to) in [
            (New, Planning),
            (Planning, PlanReady),
            (PlanReady, Working),
            (Working, Reviewing),
            (Reviewing, Fixing),
            (Fixing, Reviewing),
            (Reviewing, Auditing),
            (Auditing, ReadyToMerge),
            (ReadyToMerge, Working),
            (ReadyToMerge, Merging),
            (Merging, Done),
            (Merging, ResolvingConflict),
            (ResolvingConflict, ConflictBlocked),
            (ConflictBlocked, Merging),
            (Working, Abandoned),
        ] {
            assert!(check_transition(from, to).is_ok(), "{from:?} -> {to:?}");
        }
    }

    #[test]
    fn given_terminal_source_when_transition_checked_then_illegal() {
        for from in [
            IssueStatus::Done,
            IssueStatus::Failed,
            IssueStatus::Abandoned,
        ] {
            for to in ALL {
                assert!(!is_legal_transition(from, to), "{from:?} -> {to:?}");
            }
        }
    }
}

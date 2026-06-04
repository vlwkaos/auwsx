//! Event bus. tokio::sync::broadcast channel multiplexed to all UI subscribers
//! (TUI, web SSE, future channels). All state transitions and log appends emit Events.
//!
//! Events MUST be cheap to clone (they fan out to N subscribers). Hold IDs and
//! short fields; let UIs query DB for full rows when rendering.

use crate::main_jobs::MainJobStatus;
use crate::state::TaskStatus;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    TaskStatus {
        task_id: i64,
        status: TaskStatus,
        iteration: u32,
    },
    TaskLog {
        task_id: i64,
        iteration: u32,
        chunk: String,
    },
    DraftCreated {
        draft_id: i64,
        project_id: i64,
    },
    DraftResolved {
        draft_id: i64,
        state: String, // "consumed" | "discarded"
    },
    FollowupCreated {
        followup_id: i64,
        task_id: i64,
    },
    FollowupDeleted {
        followup_id: i64,
    },
    MainJobStatus {
        main_job_id: i64,
        status: MainJobStatus,
    },
    RoutineFired {
        routine_id: i64,
        main_job_id: i64,
    },
    DaemonLifecycle {
        kind: String, // "started" | "recovered"
    },
}

// TODO: pub fn channel() -> (broadcast::Sender<Event>, broadcast::Receiver<Event>)

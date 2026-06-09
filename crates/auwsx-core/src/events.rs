//! Event bus. tokio::sync::broadcast channel multiplexed to all UI subscribers
//! (TUI, web SSE, future channels). All state transitions and log appends emit Events.
//!
//! Events MUST be cheap to clone (they fan out to N subscribers). Hold IDs and
//! short fields; let UIs query DB for full rows when rendering.

use crate::main_jobs::MainJobStatus;
use crate::state::IssueStatus;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    IssueStatus {
        issue_id: i64,
        status: IssueStatus,
    },
    IssueLog {
        issue_id: i64,
        phase: String,
        chunk: String,
    },
    BacklogChanged {
        item_id: i64,
        project_id: i64,
        approval: String, // "pending" | "approved" | "dismissed"
    },
    FindingAdded {
        finding_id: i64,
        issue_id: i64,
    },
    SteeringAdded {
        steering_id: i64,
        issue_id: i64,
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

/// Capacity of the broadcast buffer. A slow subscriber that lags past this many
/// events gets a `RecvError::Lagged` (and should resync from the DB) rather than
/// blocking the daemon — events are advisory, the DB is the source of truth.
pub const CHANNEL_CAPACITY: usize = 1024;

/// Create the daemon's event bus. The daemon holds the [`Sender`]; each IPC
/// `Subscribe` connection gets a [`Receiver`] via `sender.subscribe()`.
///
/// [`Sender`]: tokio::sync::broadcast::Sender
/// [`Receiver`]: tokio::sync::broadcast::Receiver
pub fn channel() -> tokio::sync::broadcast::Sender<Event> {
    tokio::sync::broadcast::Sender::new(CHANNEL_CAPACITY)
}

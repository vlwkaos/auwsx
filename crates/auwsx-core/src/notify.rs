//! System notifications. Plan Step 7 / north star §5.
//!
//! Pull-toward-user model: when a state change might warrant attention,
//! fire a macOS notification. User reads when they want. No blocking.
//!
//! Backends:
//!   1. `terminal-notifier` if on PATH (preferred: supports click-actions).
//!   2. fallback to `osascript -e 'display notification ...'`.
//!
//! Toggles live in `~/.config/auwsx/config.toml` `[notifications]` section.
//!
//! Events: task_pending_feedback, task_done, task_failed, routine_failed,
//! triage_summary, daemon_lifecycle.

#[derive(Debug, Clone)]
pub enum NotifyEvent {
    TaskPendingFeedback {
        project: String,
        title: String,
        iteration: u32,
    },
    TaskDone {
        project: String,
        title: String,
        target: String,
    },
    TaskFailed {
        project: String,
        title: String,
        phase: String,
    },
    RoutineFailed {
        project: String,
        routine: String,
    },
    TriageSummary {
        project: String,
        created: u32,
        discarded: u32,
    },
    DaemonStarted,
    DaemonRecovered,
}

// TODO: notify(event: NotifyEvent) -> Result<()>
// TODO: backend detection: terminal-notifier vs osascript
// TODO: click-action URI: auwsx://task/{id}

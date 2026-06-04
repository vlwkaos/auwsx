//! Unix-socket Command/Event protocol. Plan Step 7.
//!
//! Socket path: `$XDG_RUNTIME_DIR/auwsx.sock` (fallback `~/.cache/auwsx/sock`).
//! Wire format: JSON-lines (one Command or Event per line, terminated by `\n`).
//!
//! Front-ends (TUI v0.1, web v0.2) connect, send Commands, subscribe to Events.
//! The daemon owns the SQLite write path. Multiple clients can connect simultaneously.

use crate::events::Event;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Command {
    Ping,

    // Project CRUD
    ListProjects,
    AddProject { repo_path: String, agent: String, schedule_min: u32 },
    PatchProject { project_id: i64, /* TODO field set */ },
    DeleteProject { project_id: i64 },

    // Task CRUD
    ListTasks { project_id: i64 },
    NewTaskDirect { project_id: i64, title: String, description: Option<String> },
    PatchTaskBody { task_id: i64, title: Option<String>, description: Option<String> },
    RunTaskNow { task_id: i64 },
    CompleteTask { task_id: i64 },
    DeleteTask { task_id: i64 },

    // Drafts
    ListDrafts { project_id: i64 },
    AddDraft { project_id: i64, body: String },
    EditDraft { draft_id: i64, body: String },
    DeleteDraft { draft_id: i64 },
    TriageNow { project_id: i64 },

    // Followups
    ListFollowups { task_id: i64 },
    AddFollowup { task_id: i64, body: String },
    EditFollowup { followup_id: i64, body: String },
    DeleteFollowup { followup_id: i64 },

    // Feedback
    SubmitFeedback { task_id: i64, body: String },

    // Routines
    ListRoutines { project_id: i64 },
    AddRoutine { project_id: i64, name: String, cron: String, prompt: String, output_target: Option<String> },
    PatchRoutine { routine_id: i64, /* TODO field set */ },
    DeleteRoutine { routine_id: i64 },
    RunRoutineNow { routine_id: i64 },
    ToggleRoutine { routine_id: i64, enabled: bool },

    // Main jobs
    ListMainJobs { project_id: i64 },
    EnqueueOneoff { project_id: i64, kind: String, prompt: Option<String> },

    // Subscription
    Subscribe,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Response {
    Ok,
    Err { message: String },
    Projects(serde_json::Value),
    Tasks(serde_json::Value),
    Drafts(serde_json::Value),
    Followups(serde_json::Value),
    Routines(serde_json::Value),
    MainJobs(serde_json::Value),
    Event(Event),
}

// TODO: server: listen on socket, accept connections, spawn per-client tasks
// TODO: client: connect, send Command, await Response stream

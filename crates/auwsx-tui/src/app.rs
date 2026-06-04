//! Top-level TUI state, view router, event loop. Plan Step 7.
//!
//! Four views (Plan Step 7 — View router table):
//!   1. Workspace    — projects / drafts+tasks / artifacts (main loop)
//!   2. Main/Routines — per-project main jobs + routines
//!   3. Config       — per-project schedule/concurrency/agent/merge_mode
//!   4. History      — consumed drafts, finished iterations, retention log
//!
//! Event loop: tokio::select! over { crossterm events, IPC Event stream, redraw tick }.

use anyhow::Result;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    Workspace,
    MainJobs,
    Config,
    History,
}

pub struct App {
    pub view: View,
    pub selected_project: Option<i64>,
    pub selected_task: Option<i64>,
    // TODO: drafts, tasks, routines, main_jobs, artifact_tab, scroll positions
    // TODO: IPC client handle + Event receiver
}

impl App {
    pub fn new() -> Self {
        Self {
            view: View::Workspace,
            selected_project: None,
            selected_task: None,
        }
    }

    pub async fn run(self) -> Result<()> {
        // TODO: ratatui init, terminal setup, event loop, restore on Drop.
        Ok(())
    }
}

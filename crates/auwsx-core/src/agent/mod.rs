//! AgentRunner trait + impls. Plan Step 4.
//!
//! All agents run headlessly inside a per-task tmux session
//! (`auwsx-{proj}-{task}-agent`) so the user can spectate via wsx attach
//! if desired, and so output is captured to a log file via `tmux pipe-pane`.
//!
//! Headless invocations (verified by exploration, plan Step 4):
//!   claude    → `claude --print --permission-mode bypassPermissions --output-format stream-json "<prompt>"`
//!   codex     → `codex exec --sandbox workspace-write -C <cwd> --json "<prompt>"`
//!   opencode  → `echo "<prompt>" | opencode run --dangerously-skip-permissions -q --format json`
//!
//! For non-Claude agents that don't understand slash-skills, the impl inlines
//! the skill prompt text via `skills::inline_for_agent(skill, agent_name)`.

pub mod claude;
pub mod codex;
pub mod opencode;

use crate::Result;
use async_trait::async_trait;
use std::path::Path;

pub struct AgentHandle {
    pub session: String,
    pub log_path: std::path::PathBuf,
    // TODO: tokio::task::JoinHandle<Result<ExitOutcome>>
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitOutcome {
    SignalDone,
    ProcessExit,
    Timeout,
    Error,
}

#[async_trait]
pub trait AgentRunner: Send + Sync {
    fn name(&self) -> &'static str;

    /// Spawn the agent inside `tmux_session` at `cwd`, send `prompt`, capture
    /// to `log_path`. Returns a handle; await it for natural exit.
    async fn run(
        &self,
        cwd: &Path,
        tmux_session: &str,
        prompt: &str,
        log_path: &Path,
    ) -> Result<AgentHandle>;
}

// TODO: factory: pick_runner(name: &str) -> Box<dyn AgentRunner>

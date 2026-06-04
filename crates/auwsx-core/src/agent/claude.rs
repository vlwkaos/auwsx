//! Claude Code runner. Plan Step 4.
//!
//! Headless invocation:
//!   claude --print --permission-mode bypassPermissions --output-format stream-json "<prompt>"
//!
//! Skill calls in the prompt (e.g. `/recall`, `/backpressure`) work natively
//! because Claude Code resolves them from `~/.claude/skills/`. auwsx ensures
//! they're installed there via `skills::install_skills_if_missing()`.

use super::{AgentHandle, AgentRunner, ExitOutcome};
use crate::Result;
use async_trait::async_trait;
use std::path::Path;

pub struct Claude;

#[async_trait]
impl AgentRunner for Claude {
    fn name(&self) -> &'static str {
        "claude"
    }

    async fn run(
        &self,
        _cwd: &Path,
        _tmux_session: &str,
        _prompt: &str,
        _log_path: &Path,
    ) -> Result<AgentHandle> {
        // TODO:
        //   1. Ensure tmux session exists at cwd (via wsx_core::tmux::session::create_session).
        //   2. `tmux pipe-pane -t <session> "cat >> <log_path>"`.
        //   3. send-keys with the `claude --print ...` command, terminated with Enter.
        //   4. Spawn task that polls process group + watches for .auwsx/signal-done.
        //   5. Return AgentHandle.
        todo!("claude::run")
    }
}

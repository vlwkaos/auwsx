//! opencode CLI runner. Plan Step 4.
//!
//! Headless invocation:
//!   echo "<prompt>" | opencode run --dangerously-skip-permissions -q --format json
//!
//! Like Codex, no `/skill` resolution. Inline-substitute via skills::inline_for_agent.

use super::{AgentHandle, AgentRunner};
use crate::Result;
use async_trait::async_trait;
use std::path::Path;

pub struct OpenCode;

#[async_trait]
impl AgentRunner for OpenCode {
    fn name(&self) -> &'static str {
        "opencode"
    }

    async fn run(
        &self,
        _cwd: &Path,
        _tmux_session: &str,
        _prompt: &str,
        _log_path: &Path,
    ) -> Result<AgentHandle> {
        // TODO: inline-substitute, pipe prompt via stdin to opencode run in tmux session.
        todo!("opencode::run")
    }
}

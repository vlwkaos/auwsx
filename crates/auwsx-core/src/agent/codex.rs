//! OpenAI Codex CLI runner. Plan Step 4.
//!
//! Headless invocation:
//!   codex exec --sandbox workspace-write -C <cwd> --json "<prompt>"
//!
//! Codex doesn't resolve `/skill` calls. The runner must substitute any
//! skill mentions in the prompt with inline text via `skills::inline_for_agent(...)`.

use super::{AgentHandle, AgentRunner};
use crate::Result;
use async_trait::async_trait;
use std::path::Path;

pub struct Codex;

#[async_trait]
impl AgentRunner for Codex {
    fn name(&self) -> &'static str {
        "codex"
    }

    async fn run(
        &self,
        _cwd: &Path,
        _tmux_session: &str,
        _prompt: &str,
        _log_path: &Path,
    ) -> Result<AgentHandle> {
        // TODO: inline-substitute skill calls, then spawn `codex exec` in tmux session.
        todo!("codex::run")
    }
}

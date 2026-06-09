//! OpenAI Codex CLI: default headless command template.
//!
//! Codex does not resolve `/skill` calls — the pipeline inlines skill text into
//! the prompt before spawning (see `skills`). `{prompt}` is the substitution
//! point used by `super::run`.

pub const NAME: &str = "codex";

/// Recommended default command template for `projects.*_agent_cmd`.
pub const DEFAULT_CMD: &str = "codex exec --sandbox workspace-write --json {prompt}";

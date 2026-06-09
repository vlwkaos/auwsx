//! opencode CLI: default headless command template.
//!
//! Like Codex, no `/skill` resolution — skills are inlined into the prompt. This
//! template has NO `{prompt}` token, so `super::run` feeds the prompt on the
//! child's stdin (the `echo "<prompt>" | opencode run …` shape without a shell).

pub const NAME: &str = "opencode";

/// Recommended default command template for `projects.*_agent_cmd`.
pub const DEFAULT_CMD: &str = "opencode run --dangerously-skip-permissions -q --format json";

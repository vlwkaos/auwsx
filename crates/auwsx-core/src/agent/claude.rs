//! Claude Code: default headless command template.
//!
//! Skill calls in the prompt (e.g. `/recall`, `/backpressure`) resolve natively
//! from the agent's skill path. The `{prompt}` token is the substitution point
//! used by `super::run`.

/// Agent id (matches `agent_runs.role` only indirectly — role is the phase
/// role; this is the binary name for defaults/UX).
pub const NAME: &str = "claude";

/// Recommended default command template for `projects.*_agent_cmd`.
pub const DEFAULT_CMD: &str =
    "claude --print --permission-mode bypassPermissions --output-format stream-json {prompt}";
